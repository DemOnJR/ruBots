//! GoldSrc delta compression.
//!
//! Port of `internal/proto/delta.go`. The delta system is *data driven*: the
//! server sends `svc_deltadescription` messages describing each struct's
//! fields, and everything afterwards is decoded against those descriptions.
//! That means the important thing to get right here is the framing and the
//! type dispatch, not a hardcoded field list.
//!
//! Verified from `ParseDelta` (`0x1406E3CA0`):
//!
//! * a delta starts with a 3-bit count of mask bytes, then that many mask
//!   bytes; field *i* is present iff bit `i % 8` of mask byte `i / 8` is set
//!   (`cmp rcx, 3` on the leading read, then the per-byte `shr r8b, cl` test)
//! * the field type carries a sign flag in bit 31, stripped with
//!   `and r10d, 0x7fffffff` before the type dispatch
//! * the dispatch compares against 1, 2, 4, 8 (and the higher powers), i.e.
//!   the standard `DT_*` bit constants
//! * signed fields route to `ReadSBits` (`call 0x1406E2EC0`)
//!
//! The meta-description field names in [`DELTA_DESCRIPTION_FIELDS`] were read
//! from `map.init.0` (`0x1406E2680`).

use crate::bitbuf::{BitReader, BitWriter};
use std::collections::HashMap;

pub const DT_BYTE: u32 = 1;
pub const DT_SHORT: u32 = 2;
pub const DT_FLOAT: u32 = 4;
pub const DT_INTEGER: u32 = 8;
pub const DT_ANGLE: u32 = 16;
pub const DT_TIMEWINDOW_8: u32 = 32;
pub const DT_TIMEWINDOW_BIG: u32 = 64;
pub const DT_STRING: u32 = 128;
/// Sign flag, stripped before dispatch.
pub const DT_SIGNED: u32 = 1 << 31;

/// The seven fields of `delta_description_t`, in wire order.
///
/// Names and order are verified; this is the bootstrap table used to decode
/// the very first `svc_deltadescription`, which then describes everything else.
pub const DELTA_DESCRIPTION_FIELDS: [&str; 7] = [
    "fieldType",
    "fieldName",
    "fieldOffset",
    "fieldSize",
    "significant_bits",
    "premultiply",
    "postmultiply",
];

/// The bootstrap table itself â€” the description of a description.
///
/// Everything else in the delta system is learned from the wire, but this one
/// table has to be known in advance to decode the first
/// `svc_deltadescription`.
///
/// **Validated against a live server.** Decoding the real `event_t`
/// description from a captured signon with this table yields all fourteen
/// GoldSrc field names in order â€” `entindex`, `bparam1`, `bparam2`,
/// `origin[0..2]`, `fparam1`, `fparam2`, `iparam1`, `iparam2`,
/// `angles[0..2]`, `ducking` â€” with sensible types (`entindex` as an 11-bit
/// integer, positions as 26-bit signed floats). Fourteen readable identifiers
/// do not fall out of a wrong layout.
pub fn delta_description_table() -> DeltaTable {
    let f = |name: &str, field_type: u32, bits: u32| FieldDesc {
        name: name.to_string(),
        field_type,
        bits,
        premultiply: 1.0,
        postmultiply: 1.0,
    };
    // The two scale fields are themselves transmitted as fixed-point integers
    // scaled by DESCRIPTION_SCALE, so they must be divided by it on decode.
    // Verified against the live `usercmd_t` table: with this divisor,
    // `forwardmove`/`msec` read premultiply 1.0 and `impact_position` reads
    // 8.0 — the true GoldSrc values — instead of 2000 and 16000. Without it,
    // encoding `forwardmove = 250` overflows the 12-bit field (saturates to
    // 2047) because it is multiplied by the inflated scale.
    let scaled = |name: &str| FieldDesc {
        name: name.to_string(),
        field_type: DT_FLOAT | DT_SIGNED,
        bits: 32,
        premultiply: DESCRIPTION_SCALE,
        postmultiply: 1.0,
    };
    vec![
        f("fieldType", DT_INTEGER, 32),
        f("fieldName", DT_STRING, 1),
        f("fieldOffset", DT_INTEGER, 16),
        f("fieldSize", DT_INTEGER, 8),
        f("significant_bits", DT_INTEGER, 8),
        scaled("premultiply"),
        scaled("postmultiply"),
    ]
}

/// Fixed-point scale the `premultiply`/`postmultiply` meta-fields are sent
/// with in a `svc_deltadescription`. Recovered empirically from the live
/// `usercmd_t` table (see [`delta_description_table`]).
pub const DESCRIPTION_SCALE: f32 = 2000.0;

/// Decode a `svc_deltadescription` body: `count` field descriptions read with
/// the bootstrap table.
///
/// The caller supplies the reader positioned at the first bit of the packed
/// region (i.e. after the name string and the `u16` count).
pub fn parse_description(r: &mut BitReader, count: usize) -> Vec<FieldDesc> {
    let table = delta_description_table();
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let f = parse_delta(r, &table);
        let name = match f.get("fieldName") {
            Some(Value::Str(s)) => s.clone(),
            _ => break,
        };
        out.push(FieldDesc {
            name,
            field_type: f.get("fieldType").and_then(Value::as_i64).unwrap_or(0) as u32,
            bits: f
                .get("significant_bits")
                .and_then(Value::as_i64)
                .unwrap_or(0) as u32,
            premultiply: f
                .get("premultiply")
                .and_then(Value::as_f32)
                .filter(|v| *v != 0.0)
                .unwrap_or(1.0),
            postmultiply: f
                .get("postmultiply")
                .and_then(Value::as_f32)
                .filter(|v| *v != 0.0)
                .unwrap_or(1.0),
        });
    }
    out
}

/// One field of a delta-encoded struct.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDesc {
    pub name: String,
    pub field_type: u32,
    pub bits: u32,
    pub premultiply: f32,
    pub postmultiply: f32,
}

impl FieldDesc {
    pub fn is_signed(&self) -> bool {
        self.field_type & DT_SIGNED != 0
    }

    /// Field type with the sign flag stripped.
    pub fn base_type(&self) -> u32 {
        self.field_type & !DT_SIGNED
    }
}

/// A decoded field value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f32),
    Str(String),
}

impl Value {
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Value::Int(i) => Some(*i as f32),
            Value::Float(f) => Some(*f),
            Value::Str(_) => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Str(_) => None,
        }
    }
}

/// A named table of field descriptions, e.g. `entity_state_t`.
pub type DeltaTable = Vec<FieldDesc>;

/// Registry of every struct description the server has sent us.
#[derive(Debug, Default, Clone)]
pub struct DeltaRegistry {
    tables: HashMap<String, DeltaTable>,
}

impl DeltaRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, name: impl Into<String>, table: DeltaTable) {
        self.tables.insert(name.into(), table);
    }

    pub fn get(&self, name: &str) -> Option<&DeltaTable> {
        self.tables.get(name)
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

/// Which fields a delta actually carries.
///
/// Split out from value decoding because the framing is verified while the
/// per-type numeric conversions are not (see [`parse_delta`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldMask {
    bytes: [u8; 8],
    count: usize,
}

impl FieldMask {
    /// Read the leading 3-bit byte count and the mask bytes themselves.
    pub fn read(r: &mut BitReader) -> Self {
        let count = r.read_bits(3) as usize;
        let mut bytes = [0u8; 8];
        for b in bytes.iter_mut().take(count) {
            *b = r.read_bits(8) as u8;
        }
        Self { bytes, count }
    }

    pub fn write(&self, w: &mut BitWriter) {
        w.write_bits(self.count as u32, 3);
        for b in self.bytes.iter().take(self.count) {
            w.write_bits(u32::from(*b), 8);
        }
    }

    /// Build a mask covering `indices`, sized to the highest one set.
    pub fn from_indices(indices: &[usize]) -> Self {
        let mut bytes = [0u8; 8];
        let mut count = 0usize;
        for &i in indices {
            let (byte, bit) = (i / 8, i % 8);
            assert!(byte < 8, "delta field index {i} exceeds the 64-field mask");
            bytes[byte] |= 1 << bit;
            count = count.max(byte + 1);
        }
        Self { bytes, count }
    }

    pub fn is_set(&self, index: usize) -> bool {
        let (byte, bit) = (index / 8, index % 8);
        byte < self.count && self.bytes[byte] & (1 << bit) != 0
    }

    /// Number of mask bytes on the wire.
    pub fn byte_count(&self) -> usize {
        self.count
    }
}

/// Decode a delta against `table`, returning only the fields present.
///
/// **Validation status:** the framing (mask, field selection, sign handling)
/// is verified against the disassembly. The per-type numeric conversions
/// below â€” in particular the `premultiply`/`postmultiply` scaling and the
/// time-window and angle encodings â€” are the standard GoldSrc behaviour but
/// were *not* confirmed instruction by instruction. They are the first thing
/// to check against a live server capture.
pub fn parse_delta(r: &mut BitReader, table: &[FieldDesc]) -> HashMap<String, Value> {
    let mask = FieldMask::read(r);
    let mut out = HashMap::new();

    for (i, f) in table.iter().enumerate() {
        if !mask.is_set(i) {
            continue;
        }
        let signed = f.is_signed();
        let bits = f.bits;
        let value = match f.base_type() {
            DT_BYTE | DT_SHORT | DT_INTEGER => {
                let raw = if signed {
                    i64::from(r.read_sbits(bits))
                } else {
                    i64::from(r.read_bits(bits))
                };
                if f.premultiply != 0.0 && f.premultiply != 1.0 {
                    Value::Int((raw as f32 / f.premultiply) as i64)
                } else {
                    Value::Int(raw)
                }
            }
            DT_FLOAT | DT_TIMEWINDOW_8 | DT_TIMEWINDOW_BIG => {
                let raw = if signed {
                    r.read_sbits(bits) as f32
                } else {
                    r.read_bits(bits) as f32
                };
                let mut v = raw;
                if f.premultiply != 0.0 {
                    v /= f.premultiply;
                }
                if f.postmultiply != 0.0 {
                    v *= f.postmultiply;
                }
                Value::Float(v)
            }
            DT_ANGLE => {
                let raw = r.read_bits(bits) as f32;
                Value::Float(raw * (360.0 / (1u32 << bits) as f32))
            }
            DT_STRING => Value::Str(r.read_string()),
            other => {
                // Unknown type: we cannot know the width, so the stream is
                // now unusable. Surface it rather than silently desyncing.
                out.insert(
                    format!("__unknown_type_{other}__"),
                    Value::Str(f.name.clone()),
                );
                break;
            }
        };
        out.insert(f.name.clone(), value);
    }
    out
}

/// Write one field's value the way the engine's `DELTA_WriteField` does.
///
/// Inverse of the per-type reads in [`parse_delta`]. **Engine (ReHLDS)
/// semantics:** numeric fields are scaled by `premultiply` *only* on the way
/// out — `postmultiply` is a read-side scale and is deliberately ignored here
/// (confirmed from `sv_user.cpp`; see [[aiplayers-rust-port]]). Signed fields
/// use sign-magnitude via [`BitWriter::write_sbits`].
fn write_field(w: &mut BitWriter, f: &FieldDesc, v: &Value) {
    let bits = f.bits;
    let signed = f.is_signed();
    match f.base_type() {
        DT_BYTE | DT_SHORT | DT_INTEGER => {
            let raw = if f.premultiply != 0.0 && f.premultiply != 1.0 {
                (v.as_f32().unwrap_or(0.0) * f.premultiply).round() as i64
            } else {
                v.as_i64().unwrap_or(0)
            };
            if signed {
                w.write_sbits(raw as i32, bits);
            } else {
                w.write_bits(raw as u32, bits);
            }
        }
        DT_FLOAT | DT_TIMEWINDOW_8 | DT_TIMEWINDOW_BIG => {
            let mut val = v.as_f32().unwrap_or(0.0);
            if f.premultiply != 0.0 {
                val *= f.premultiply;
            }
            let raw = val.round() as i64;
            if signed {
                w.write_sbits(raw as i32, bits);
            } else {
                w.write_bits(raw as u32, bits);
            }
        }
        DT_ANGLE => {
            // Inverse of `raw * 360/2^bits`, wrapped into the field width.
            let scale = (1u64 << bits) as f32 / 360.0;
            let raw = (v.as_f32().unwrap_or(0.0) * scale).round() as i64;
            let mask = (1u64 << bits) - 1;
            w.write_bits((raw as u64 & mask) as u32, bits);
        }
        DT_STRING => match v {
            Value::Str(s) => w.write_string(s),
            _ => w.write_string(""),
        },
        _ => {}
    }
}

/// Encode a delta against `table`: the field mask, then each present field's
/// value. Inverse of [`parse_delta`].
///
/// `fields` holds the values to send, keyed by field name; a field is written
/// iff its name is both in `table` and in `fields`, in table order. An empty
/// `fields` writes a single zero mask-count — the valid "nothing changed"
/// delta that GoldSrc encodes as one `0x00` byte.
///
/// For tables whose `premultiply`/`postmultiply` are 1.0 (as the usercmd
/// tables a bot sends are), this is an exact inverse of `parse_delta`, so
/// `parse_delta(write_delta(x)) == x`.
pub fn write_delta(w: &mut BitWriter, table: &[FieldDesc], fields: &HashMap<String, Value>) {
    let indices: Vec<usize> = table
        .iter()
        .enumerate()
        .filter(|(_, f)| fields.contains_key(&f.name))
        .map(|(i, _)| i)
        .collect();
    FieldMask::from_indices(&indices).write(w);
    for &i in &indices {
        let f = &table[i];
        write_field(w, f, &fields[&f.name]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, ty: u32, bits: u32) -> FieldDesc {
        FieldDesc {
            name: name.into(),
            field_type: ty,
            bits,
            premultiply: 1.0,
            postmultiply: 1.0,
        }
    }

    #[test]
    fn type_constants_are_disjoint_bits() {
        let all = [
            DT_BYTE,
            DT_SHORT,
            DT_FLOAT,
            DT_INTEGER,
            DT_ANGLE,
            DT_TIMEWINDOW_8,
            DT_TIMEWINDOW_BIG,
            DT_STRING,
        ];
        for (i, a) in all.iter().enumerate() {
            assert_eq!(a.count_ones(), 1, "DT_* constants are single bits");
            for b in &all[i + 1..] {
                assert_eq!(a & b, 0);
            }
        }
        assert_eq!(DT_SIGNED, 0x8000_0000);
    }

    #[test]
    fn sign_flag_is_stripped_from_the_type() {
        let f = field("x", DT_SHORT | DT_SIGNED, 16);
        assert!(f.is_signed());
        assert_eq!(f.base_type(), DT_SHORT);
    }

    #[test]
    fn mask_round_trips() {
        let m = FieldMask::from_indices(&[0, 3, 9]);
        assert_eq!(m.byte_count(), 2);
        let mut w = BitWriter::new();
        m.write(&mut w);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let back = FieldMask::read(&mut r);
        assert_eq!(back, m);
        assert!(back.is_set(0) && back.is_set(3) && back.is_set(9));
        assert!(!back.is_set(1) && !back.is_set(8));
    }

    #[test]
    fn only_masked_fields_are_decoded() {
        let table = vec![
            field("a", DT_BYTE, 8),
            field("b", DT_BYTE, 8),
            field("c", DT_BYTE, 8),
        ];
        // Only fields 0 and 2 present.
        let mut w = BitWriter::new();
        FieldMask::from_indices(&[0, 2]).write(&mut w);
        w.write_bits(11, 8);
        w.write_bits(33, 8);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let got = parse_delta(&mut r, &table);
        assert_eq!(got.len(), 2);
        assert_eq!(got["a"], Value::Int(11));
        assert_eq!(got["c"], Value::Int(33));
        assert!(!got.contains_key("b"));
    }

    #[test]
    fn signed_fields_use_sign_magnitude() {
        let table = vec![field("v", DT_SHORT | DT_SIGNED, 16)];
        let mut w = BitWriter::new();
        FieldMask::from_indices(&[0]).write(&mut w);
        w.write_sbits(-1234, 16);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = parse_delta(&mut r, &table);
        assert_eq!(got["v"], Value::Int(-1234));
    }

    /// A mixed table exercised end to end: `write_delta` then `parse_delta`
    /// must return exactly the fields written. This is the airtight,
    /// server-free proof that the encoder is the inverse of the decoder.
    #[test]
    fn write_delta_round_trips_through_parse_delta() {
        let table = vec![
            field("msec", DT_BYTE, 8),
            field("buttons", DT_SHORT, 16),
            field("forwardmove", DT_FLOAT | DT_SIGNED, 16),
            field("sidemove", DT_FLOAT | DT_SIGNED, 16),
            field("impulse", DT_BYTE, 8),
            field("lightlevel", DT_BYTE, 8),
        ];
        let mut want = HashMap::new();
        want.insert("msec".to_string(), Value::Int(21));
        want.insert("buttons".to_string(), Value::Int(0b1001));
        want.insert("forwardmove".to_string(), Value::Float(250.0));
        want.insert("sidemove".to_string(), Value::Float(-160.0));

        let mut w = BitWriter::new();
        write_delta(&mut w, &table, &want);
        let bytes = w.into_bytes();

        let mut r = BitReader::new(&bytes);
        let got = parse_delta(&mut r, &table);
        assert_eq!(got.len(), want.len(), "only the written fields come back");
        assert_eq!(got["msec"], Value::Int(21));
        assert_eq!(got["buttons"], Value::Int(0b1001));
        assert_eq!(got["forwardmove"], Value::Float(250.0));
        assert_eq!(got["sidemove"], Value::Float(-160.0));
        assert!(!got.contains_key("impulse"));
    }

    #[test]
    fn an_empty_delta_is_a_single_zero_byte() {
        // GoldSrc encodes "nothing changed" as a 3-bit zero mask count, which
        // occupies one padded byte. A lone 0x00 usercmd is fully valid.
        let table = vec![field("msec", DT_BYTE, 8)];
        let mut w = BitWriter::new();
        write_delta(&mut w, &table, &HashMap::new());
        let bytes = w.into_bytes();
        assert_eq!(bytes, vec![0x00]);

        let mut r = BitReader::new(&bytes);
        assert!(parse_delta(&mut r, &table).is_empty());
    }

    #[test]
    fn angle_fields_round_trip_within_quantisation() {
        // 90 degrees at 8-bit angle precision quantises exactly (256/360*90 =
        // 64), so the round-trip is exact.
        let table = vec![field("yaw", DT_ANGLE, 8)];
        let mut fields = HashMap::new();
        fields.insert("yaw".to_string(), Value::Float(90.0));
        let mut w = BitWriter::new();
        write_delta(&mut w, &table, &fields);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        let got = parse_delta(&mut r, &table);
        assert_eq!(got["yaw"], Value::Float(90.0));
    }

    #[test]
    fn registry_stores_and_returns_tables() {
        let mut reg = DeltaRegistry::new();
        reg.register("entity_state_t", vec![field("origin[0]", DT_FLOAT, 32)]);
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.get("entity_state_t").unwrap()[0].name, "origin[0]");
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn bootstrap_description_has_seven_named_fields() {
        assert_eq!(DELTA_DESCRIPTION_FIELDS.len(), 7);
        assert_eq!(DELTA_DESCRIPTION_FIELDS[0], "fieldType");
        assert_eq!(DELTA_DESCRIPTION_FIELDS[6], "postmultiply");
        let t = delta_description_table();
        assert_eq!(t.len(), 7);
        for (i, name) in DELTA_DESCRIPTION_FIELDS.iter().enumerate() {
            assert_eq!(&t[i].name, name, "bootstrap table order must match");
        }
    }

    /// The bit-packed region of a real `svc_deltadescription` for `event_t`,
    /// captured from HLDS 1.1.2.7/Stdio on 2026-08-01 (signon offset 305).
    const LIVE_EVENT_T_BITS: &[u8] = &[
        0xF9, 0x43, 0x00, 0x00, 0x00, 0x28, 0x73, 0xA3, 0x4B, 0x73, 0x23, 0x2B,
        0xC3, 0x03, 0x20, 0x00, 0x08, 0x58, 0x00, 0x7D, 0x00, 0x00, 0x00, 0x7D,
        0x00, 0x00, 0xC8, 0x1F, 0x02, 0x00, 0x00, 0x80, 0x18, 0x5C, 0x98, 0x5C,
        0x58, 0x5B, 0x0C, 0x00, 0x10, 0x40, 0x40, 0x00, 0xE8, 0x03, 0x00, 0x00,
        0xE8, 0x03, 0x00, 0x40, 0xFE, 0x10, 0x00, 0x00, 0x00, 0xC4, 0xE0, 0xC2,
        0xE4, 0xC2, 0xDA, 0x64, 0x00, 0x88, 0x00, 0x02, 0x02, 0x40, 0x1F, 0x00,
        0x00, 0x40, 0x1F, 0x00, 0x00, 0xF2, 0x47, 0x00, 0x00, 0x00, 0xF8, 0x26,
        0x97, 0x76, 0x96, 0xE6, 0xB6, 0x05, 0xD3, 0x05, 0x80, 0x00, 0x10, 0xA0,
        0x01, 0x00, 0x40, 0x1F, 0x00, 0xFA, 0x00, 0x00, 0x90, 0x3F, 0x02, 0x00,
        0x00, 0xC0, 0x37, 0xB9, 0xB4, 0xB3, 0x34, 0xB7, 0xAD, 0x98, 0x2E, 0x00,
        0x06, 0x80, 0x00, 0x0D, 0x00, 0x00, 0xFA, 0x00, 0xD0, 0x07, 0x00, 0x80,
        0xFC, 0x11, 0x00, 0x00, 0x00, 0xBE, 0xC9, 0xA5, 0x9D, 0xA5, 0xB9, 0x6D,
        0xC9, 0x74, 0x01, 0x40, 0x00, 0x04, 0x68, 0x00, 0x00, 0xD0, 0x07, 0x80,
        0x3E, 0x00, 0x00, 0xE4, 0x8F, 0x00, 0x00, 0x00, 0xD0, 0x0C, 0x2E, 0x4C,
        0x2E, 0xAC, 0x2D, 0x06, 0x00, 0x06, 0x20, 0x80, 0x02, 0x50, 0xC3, 0x00,
        0x00, 0xF4, 0x01, 0x00, 0x20, 0x7F, 0x04, 0x00, 0x00, 0x80, 0x66, 0x70,
        0x61, 0x72, 0x61, 0x6D, 0x32, 0x00, 0x34, 0x00, 0x01, 0x14, 0x80, 0x1A,
        0x06, 0x00, 0xA0, 0x0F, 0x00, 0x00, 0xF9, 0x43, 0x00, 0x00, 0x00, 0x4C,
        0x83, 0x0B, 0x93, 0x0B, 0x6B, 0x8B, 0x01, 0xC0, 0x01, 0x08, 0x90, 0x00,
        0x7D, 0x00, 0x00, 0x00, 0x7D, 0x00, 0x00, 0xC8, 0x1F, 0x02, 0x00, 0x00,
        0x60, 0x1A, 0x5C, 0x98, 0x5C, 0x58, 0x9B, 0x0C, 0x00, 0x0F, 0x40, 0x80,
        0x04, 0xE8, 0x03, 0x00, 0x00, 0xE8, 0x03, 0x00, 0x40, 0xFE, 0x08, 0x00,
        0x00, 0x00, 0xC3, 0xDC, 0xCE, 0xD8, 0xCA, 0xE6, 0xB6, 0x60, 0xBA, 0x00,
        0x28, 0x00, 0x02, 0x34, 0x00, 0x00, 0xE8, 0x03, 0x40, 0x1F, 0x00, 0x00,
        0xF2, 0x47, 0x00, 0x00, 0x00, 0x18, 0xE6, 0x76, 0xC6, 0x56, 0x36, 0xB7,
        0x15, 0xD3, 0x05, 0x80, 0x01, 0x10, 0xA0, 0x01, 0x00, 0x40, 0x1F, 0x00,
        0xFA, 0x00, 0x00, 0x90, 0x3F, 0x02, 0x00, 0x00, 0xC0, 0x30, 0xB7, 0x33,
        0xB6, 0xB2, 0xB9, 0x2D, 0x99, 0x2E, 0x00, 0x0E, 0x80, 0x00, 0x0D, 0x00,
        0x00, 0xFA, 0x00, 0xD0, 0x07, 0x00, 0x80, 0xFC, 0x21, 0x00, 0x00, 0x00,
        0x90, 0xD5, 0x8D, 0xAD, 0xA5, 0xB9, 0x9D, 0x01, 0xB0, 0x00, 0x04, 0x04,
        0x80, 0x3E, 0x00, 0x00, 0x80, 0x3E, 0x00, 0x00, 0x00, 0x0E, 0x77, 0x65,
        0x61, 0x70, 0x6F, 0x6E, 0x5F, 0x64, 0x61, 0x74, 0x61, 0x5F, 0x74, 0x00,
        0x12, 0x00, 0xF9, 0x23, 0x00, 0x00, 0x00, 0x6C, 0xFB, 0x32, 0x63, 0xA3,
        0x4A, 0x6B, 0x2B, 0xBB, 0x2A, 0x0B, 0x83, 0x7B, 0x73, 0x4B, 0x22, 0x63,
        0x2B, 0x03, 0x80, 0x00, 0x08, 0xB0, 0x00, 0x48, 0xE8, 0x01, 0x00, 0x7D,
        0x00, 0x00, 0xC8, 0x1F, 0x01, 0x00, 0x00, 0x60, 0xDB, 0x97,
    ];

    #[test]
    fn the_live_event_t_description_decodes_to_real_field_names() {
        let mut r = BitReader::new(LIVE_EVENT_T_BITS);
        let fields = parse_description(&mut r, 14);
        let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "entindex", "bparam1", "bparam2", "origin[0]", "origin[1]", "origin[2]",
                "fparam1", "fparam2", "iparam1", "iparam2", "angles[0]", "angles[1]",
                "angles[2]", "ducking",
            ]
        );
    }

    #[test]
    fn the_live_event_t_field_types_are_sensible() {
        let mut r = BitReader::new(LIVE_EVENT_T_BITS);
        let fields = parse_description(&mut r, 14);

        let by = |n: &str| fields.iter().find(|f| f.name == n).unwrap().clone();

        // entindex indexes entities, so it is a small unsigned integer.
        let ent = by("entindex");
        assert_eq!(ent.base_type(), DT_INTEGER);
        assert!(!ent.is_signed());
        assert_eq!(ent.bits, 11, "11 bits covers the 2048 entity slots");

        // Positions are signed floats.
        let ox = by("origin[0]");
        assert_eq!(ox.base_type(), DT_FLOAT);
        assert!(ox.is_signed());
        assert_eq!(ox.bits, 26);

        // iparam is a signed integer, not a float.
        let ip = by("iparam1");
        assert_eq!(ip.base_type(), DT_INTEGER);
        assert!(ip.is_signed());

        // Booleans are single bits.
        assert_eq!(by("ducking").bits, 1);
        assert_eq!(by("bparam1").bits, 1);
    }
}

