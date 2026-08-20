//! GoldSrc bit-level reader/writer.
//!
//! Port of `internal/proto/bitbuf.go`. Bit order and overflow behaviour were
//! read out of the disassembly rather than assumed:
//!
//! * bits are packed **LSB-first** within each byte
//!   (`ReadBit` @ `0x1406E2DC0`: `shr dl, cl` with `cl` = bit position)
//! * reading past the limit sets an overflow flag and yields **1**, not 0
//!   (`0x1406E2E0E`: `mov byte [rax+0x30], 1` / `mov eax, 1`)
//! * signed values are **sign-magnitude**, not two's complement
//!   (`ReadSBits` @ `0x1406E2EC0`: one sign bit, then `n-1` magnitude bits,
//!   then `neg esi`)

/// Reader over a packed bit stream.
///
/// Mirrors the Go `BitReader` field layout: data slice, a `limit` in bytes that
/// may be shorter than the slice, a byte cursor, a bit cursor and a sticky
/// overflow flag.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    data: &'a [u8],
    limit: usize,
    byte_pos: usize,
    bit_pos: u32,
    overflow: bool,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, limit: data.len(), byte_pos: 0, bit_pos: 0, overflow: false }
    }

    /// Reader that refuses to read beyond `limit` bytes even if `data` is longer.
    pub fn with_limit(data: &'a [u8], limit: usize) -> Self {
        Self { data, limit: limit.min(data.len()), byte_pos: 0, bit_pos: 0, overflow: false }
    }

    /// Current byte cursor (`BytePos`).
    pub fn byte_pos(&self) -> usize {
        self.byte_pos
    }

    /// Bits consumed within the current byte, 0..7.
    ///
    /// Needed to realign after a packed region: GoldSrc bit sections end on a
    /// byte boundary, so a non-zero offset means one more byte was touched.
    pub fn bit_offset(&self) -> u32 {
        self.bit_pos
    }

    /// Sticky overflow flag — set once a read ran past `limit` (`End`).
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Whole bytes left before the limit (`Remaining`).
    pub fn remaining(&self) -> usize {
        self.limit.saturating_sub(self.byte_pos)
    }

    /// Discard the rest of the current byte (`Align`).
    pub fn align(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
    }

    pub fn read_bit(&mut self) -> u32 {
        if self.byte_pos >= self.limit {
            self.overflow = true;
            return 1;
        }
        let v = (self.data[self.byte_pos] >> self.bit_pos) & 1;
        if self.bit_pos == 7 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        } else {
            self.bit_pos += 1;
        }
        u32::from(v)
    }

    /// Read `n` bits LSB-first. Bits at index >= 32 are consumed but discarded,
    /// matching the `cmp rcx,0x20 / sbb / and` guard in the original.
    pub fn read_bits(&mut self, n: u32) -> u32 {
        let mut out = 0u32;
        for i in 0..n {
            let bit = self.read_bit();
            if i < 32 {
                out |= bit << i;
            }
        }
        out
    }

    /// Sign-magnitude read: one sign bit, then `n - 1` magnitude bits.
    pub fn read_sbits(&mut self, n: u32) -> i32 {
        let sign = self.read_bit();
        let v = self.read_bits(n.saturating_sub(1)) as i32;
        if sign != 0 {
            -v
        } else {
            v
        }
    }

    /// Read `n` bits without advancing.
    pub fn peek_bits(&mut self, n: u32) -> u32 {
        let save = (self.byte_pos, self.bit_pos, self.overflow);
        let v = self.read_bits(n);
        self.byte_pos = save.0;
        self.bit_pos = save.1;
        self.overflow = save.2;
        v
    }

    pub fn skip(&mut self, bits: u32) {
        for _ in 0..bits {
            self.read_bit();
        }
    }

    pub fn read_byte(&mut self) -> u8 {
        self.read_bits(8) as u8
    }
    pub fn read_char(&mut self) -> i8 {
        self.read_bits(8) as u8 as i8
    }
    pub fn read_short(&mut self) -> i16 {
        self.read_bits(16) as u16 as i16
    }
    pub fn read_word(&mut self) -> u16 {
        self.read_bits(16) as u16
    }
    pub fn read_long(&mut self) -> i32 {
        self.read_bits(32) as i32
    }
    pub fn read_ulong(&mut self) -> u32 {
        self.read_bits(32)
    }
    pub fn read_float(&mut self) -> f32 {
        f32::from_bits(self.read_bits(32))
    }

    /// NUL-terminated string. Stops at the terminator, at overflow, or at
    /// `max` bytes when supplied.
    pub fn read_string_limit(&mut self, max: Option<usize>) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            if let Some(m) = max {
                if out.len() >= m {
                    break;
                }
            }
            let c = self.read_byte();
            if c == 0 || self.overflow {
                break;
            }
            out.push(c);
        }
        out
    }

    pub fn read_string(&mut self) -> String {
        String::from_utf8_lossy(&self.read_string_limit(None)).into_owned()
    }

    pub fn read_buf(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.read_byte()).collect()
    }

    /// GoldSrc coordinate: integer-present bit, fraction-present bit, sign bit,
    /// then 12 integer bits and 3 fractional eighths.
    ///
    /// Constants verified: the fractional scale at `0x141148BE0` is `0.125f32`,
    /// and the sign flip is an `xorps` against `-0.0f32` at `0x141148B74`.
    pub fn read_bit_coord(&mut self) -> f32 {
        let int_present = self.read_bit();
        let frac_present = self.read_bit();
        if int_present == 0 && frac_present == 0 {
            return 0.0;
        }
        let sign = self.read_bit();
        let mut v = 0.0f32;
        if int_present != 0 {
            v += self.read_bits(12) as f32;
        }
        if frac_present != 0 {
            v += self.read_bits(3) as f32 * 0.125;
        }
        if sign != 0 {
            -v
        } else {
            v
        }
    }

    /// Three coordinates behind three presence bits.
    ///
    /// NOTE: this is the standard GoldSrc `MSG_ReadBitVec3Coord` shape. Unlike
    /// the rest of this module it was **not** verified instruction by
    /// instruction against `ReadBitVec3Coord` (`0x1406E38C0`, 512 bytes) —
    /// confirm before trusting it on the wire.
    pub fn read_bit_vec3_coord(&mut self) -> [f32; 3] {
        let xf = self.read_bit();
        let yf = self.read_bit();
        let zf = self.read_bit();
        let mut v = [0.0f32; 3];
        if xf != 0 {
            v[0] = self.read_bit_coord();
        }
        if yf != 0 {
            v[1] = self.read_bit_coord();
        }
        if zf != 0 {
            v[2] = self.read_bit_coord();
        }
        v
    }
}

/// Writer producing the same packing `BitReader` consumes.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    buf: Vec<u8>,
    bit_pos: u32,
}

impl BitWriter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }

    pub fn len_bytes(&self) -> usize {
        self.buf.len()
    }

    pub fn write_bit(&mut self, bit: u32) {
        if self.bit_pos == 0 {
            self.buf.push(0);
        }
        if bit & 1 != 0 {
            let last = self.buf.len() - 1;
            self.buf[last] |= 1 << self.bit_pos;
        }
        self.bit_pos = (self.bit_pos + 1) & 7;
    }

    pub fn write_bits(&mut self, value: u32, n: u32) {
        for i in 0..n {
            let bit = if i < 32 { (value >> i) & 1 } else { 0 };
            self.write_bit(bit);
        }
    }

    /// Sign-magnitude write, clamped to +/-((1 << (n-1)) - 1).
    ///
    /// The clamp is verified (`WriteSBits` @ `0x1406E2AE0`); the sign-then-
    /// magnitude emission is the inverse of the verified `ReadSBits` and is
    /// covered by the round-trip test below.
    pub fn write_sbits(&mut self, value: i32, n: u32) {
        let mut v = value;
        if n < 32 && n >= 1 {
            let max = (1i32 << (n - 1)) - 1;
            if v > max {
                v = max;
            } else if v < -max {
                v = -max;
            }
        }
        let sign = u32::from(v < 0);
        self.write_bit(sign);
        self.write_bits(v.unsigned_abs(), n.saturating_sub(1));
    }

    pub fn align(&mut self) {
        if self.bit_pos != 0 {
            self.bit_pos = 0;
        }
    }

    pub fn write_byte(&mut self, v: u8) {
        self.write_bits(u32::from(v), 8);
    }
    pub fn write_short(&mut self, v: i16) {
        self.write_bits(v as u16 as u32, 16);
    }
    pub fn write_long(&mut self, v: i32) {
        self.write_bits(v as u32, 32);
    }
    pub fn write_float(&mut self, v: f32) {
        self.write_bits(v.to_bits(), 32);
    }

    pub fn write_string(&mut self, s: &str) {
        for b in s.as_bytes() {
            self.write_byte(*b);
        }
        self.write_byte(0);
    }

    pub fn write_buf(&mut self, b: &[u8]) {
        for x in b {
            self.write_byte(*x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_are_packed_lsb_first() {
        // 1,0,1 written LSB-first into one byte is 0b0000_0101.
        let mut w = BitWriter::new();
        w.write_bit(1);
        w.write_bit(0);
        w.write_bit(1);
        assert_eq!(w.as_bytes()[0] & 0b111, 0b101);
    }

    #[test]
    fn read_bits_round_trips() {
        let mut w = BitWriter::new();
        w.write_bits(0x2A, 7);
        w.write_bits(0x1FF, 9);
        w.write_bits(0xDEAD_BEEF, 32);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(7), 0x2A);
        assert_eq!(r.read_bits(9), 0x1FF);
        assert_eq!(r.read_bits(32), 0xDEAD_BEEF);
        assert!(!r.overflowed());
    }

    #[test]
    fn sbits_are_sign_magnitude_not_twos_complement() {
        let mut w = BitWriter::new();
        w.write_sbits(-5, 8);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_sbits(8), -5);

        // The distinguishing property: -1 in 8 sign-magnitude bits is the sign
        // bit plus magnitude 1, i.e. two set bits total -- a two's complement
        // encoding would set all eight.
        let mut w = BitWriter::new();
        w.write_sbits(-1, 8);
        assert_eq!(w.as_bytes()[0].count_ones(), 2);
    }

    #[test]
    fn sbits_clamp_to_magnitude_range() {
        let mut w = BitWriter::new();
        w.write_sbits(9999, 8); // max magnitude for n=8 is (1<<7)-1 = 127
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_sbits(8), 127);
    }

    #[test]
    fn overflow_reads_as_one_and_is_sticky() {
        let data = [0u8; 1];
        let mut r = BitReader::new(&data);
        r.skip(8);
        assert!(!r.overflowed());
        assert_eq!(r.read_bit(), 1, "past-the-end reads yield 1, not 0");
        assert!(r.overflowed());
    }

    #[test]
    fn coord_encodes_eighths() {
        // integer present, fraction present, positive, 3 + 5/8
        let mut w = BitWriter::new();
        w.write_bit(1);
        w.write_bit(1);
        w.write_bit(0);
        w.write_bits(3, 12);
        w.write_bits(5, 3);
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bit_coord(), 3.625);
    }

    #[test]
    fn coord_zero_costs_two_bits() {
        let bytes = [0u8; 4];
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bit_coord(), 0.0);
        assert_eq!(r.byte_pos(), 0);
    }

    #[test]
    fn strings_round_trip() {
        let mut w = BitWriter::new();
        w.write_string("de_dust2");
        let bytes = w.into_bytes();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_string(), "de_dust2");
    }
}
