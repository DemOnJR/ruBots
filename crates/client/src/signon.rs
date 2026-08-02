//! Walking the signon stream to build the delta registry.
//!
//! After `clc_stringcmd "new"`, the server sends a bzip2-compressed burst of
//! `svc_*` messages. The important ones are the `svc_deltadescription`s: they
//! teach the client how to decode every later entity update, which is where
//! player positions come from.
//!
//! Message layouts here were derived by walking a real capture (see
//! `tests/fixtures/signon.bin`), which is the only reliable way to get them —
//! the binary inlines its `BitReader` calls, so the widths are not readable
//! from the disassembly.
//!
//! One layout detail worth recording: `svc_serverinfo` is followed by a
//! **trailing flag byte** after the map-cycle string. Missing it shifts
//! everything after by one and the walk collapses immediately.

use proto::bitbuf::BitReader;
use proto::delta::{self, DeltaRegistry};

use crate::messages::{Reader, ServerInfo};
use crate::svc;

/// What a signon walk recovered.
#[derive(Debug, Default)]
pub struct Signon {
    pub server_info: Option<ServerInfo>,
    pub registry: DeltaRegistry,
    /// Byte offset the walk stopped at.
    pub stopped_at: usize,
    /// Message id that halted the walk, if any.
    pub stopped_on: Option<u8>,
    /// Signon stage, when one was seen.
    pub signon_num: Option<u8>,
    /// Resources the server advertised, if a `svc_resourcelist` was present.
    pub resources: Vec<proto::resources::Resource>,
}

/// Walk `data`, learning delta descriptions until an unknown message is hit.
///
/// Stops rather than guessing: a mis-sized message desynchronises everything
/// after it, so halting with `stopped_on` set is far more useful than
/// producing plausible garbage.
pub fn walk(data: &[u8]) -> Signon {
    let mut out = Signon::default();
    let mut r = Reader::new(data);

    loop {
        let start = r.pos();
        let Some(id) = r.u8() else {
            out.stopped_at = start;
            return out;
        };

        match id {
            svc::SVC_NOP => {}
            svc::SVC_PRINT | svc::SVC_STUFFTEXT => {
                if r.cstr().is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_SERVERINFO => {
                let Some(si) = ServerInfo::parse(&mut r) else {
                    out.stopped_at = start;
                    return out;
                };
                // map cycle list, then the trailing flag byte.
                if r.cstr().is_none() || r.u8().is_none() {
                    out.stopped_at = start;
                    return out;
                }
                out.server_info = Some(si);
            }
            svc::SVC_SENDEXTRAINFO => {
                if r.cstr().is_none() || r.u8().is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_LIGHTSTYLE => {
                if r.u8().is_none() || r.cstr().is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_CDTRACK => {
                if r.bytes(2).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_TIME => {
                if r.bytes(4).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_SETVIEW => {
                if r.bytes(2).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_NEWUSERMSG => {
                if r.bytes(18).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_RESOURCEREQUEST => {
                if r.bytes(8).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_NEWMOVEVARS => {
                // 16 floats, then a `footsteps` byte, then 8 more floats,
                // then the sky name. Decoded from a live capture: the first
                // sixteen are gravity 800, stopspeed 75, maxspeed 900,
                // spectatormaxspeed 500, accelerate 5, airaccelerate 10,
                // wateraccelerate 10, friction 4, edgefriction 2,
                // waterfriction 1, entgravity 1, bounce 1, stepsize 18,
                // maxvelocity 2000, zmax 8000, waveHeight 0; the last eight
                // are rollangle, rollspeed, skycolor rgb and the sky vector.
                if r.bytes(16 * 4).is_none()
                    || r.u8().is_none()
                    || r.bytes(8 * 4).is_none()
                    || r.cstr().is_none()
                {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_UPDATEUSERINFO => {
                // slot, userid, the info string, then a 16-byte CD-key hash.
                if r.u8().is_none()
                    || r.bytes(4).is_none()
                    || r.cstr().is_none()
                    || r.bytes(16).is_none()
                {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_SETANGLE => {
                if r.bytes(6).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_ROOMTYPE => {
                if r.bytes(2).is_none() {
                    out.stopped_at = start;
                    return out;
                }
            }
            svc::SVC_SIGNONNUM => {
                match r.u8() {
                    Some(n) => out.signon_num = Some(n),
                    None => {
                        out.stopped_at = start;
                        return out;
                    }
                }
            }
            svc::SVC_RESOURCELIST => {
                // Bit-packed; see proto::resources for the layout. The region
                // ends byte-aligned, like every other packed section.
                let bit_base = r.pos();
                let mut br = BitReader::new(&data[bit_base..]);
                let list = proto::resources::parse_resource_list(&mut br);
                if list.is_empty() {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
                r.seek(bit_base + br.byte_pos() + usize::from(br.bit_offset() > 0));
                out.resources = list;
            }
            svc::SVC_DELTADESCRIPTION => {
                let Some(name) = r.cstr() else {
                    out.stopped_at = start;
                    return out;
                };
                let Some(lo) = r.u8() else {
                    out.stopped_at = start;
                    return out;
                };
                let Some(hi) = r.u8() else {
                    out.stopped_at = start;
                    return out;
                };
                let count = u16::from_le_bytes([lo, hi]) as usize;

                // The packed region runs to the end of the buffer; the reader
                // tracks how far it actually consumed.
                let bit_base = r.pos();
                let mut br = BitReader::new(&data[bit_base..]);
                let fields = delta::parse_description(&mut br, count);
                if fields.len() != count {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
                out.registry.register(name, fields);
                // Bit regions end byte-aligned.
                r.seek(bit_base + br.byte_pos() + usize::from(br.bit_offset() > 0));
            }
            other => {
                out.stopped_at = start;
                out.stopped_on = Some(other);
                return out;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNON: &[u8] = include_bytes!("../tests/fixtures/signon.bin");

    #[test]
    fn the_live_signon_yields_every_delta_table() {
        let s = walk(SIGNON);

        // The seven struct names the original binary references by literal.
        for name in [
            "event_t",
            "weapon_data_t",
            "usercmd_t",
            "custom_entity_state_t",
            "entity_state_player_t",
            "entity_state_t",
            "clientdata_t",
        ] {
            assert!(
                s.registry.get(name).is_some(),
                "missing delta table {name}; walk stopped at {} on {:?}",
                s.stopped_at,
                s.stopped_on
            );
        }
        assert_eq!(s.registry.len(), 7);
    }

    #[test]
    fn the_walk_recovers_the_server_info() {
        let s = walk(SIGNON);
        let si = s.server_info.expect("serverinfo");
        assert_eq!(si.protocol, 48);
        assert_eq!(si.map_name(), "de_aztec");
    }

    #[test]
    fn player_state_carries_position_fields() {
        // This is the table the AI reads enemy positions out of.
        let s = walk(SIGNON);
        let t = s.registry.get("entity_state_player_t").expect("player table");
        for f in ["origin[0]", "origin[1]", "origin[2]", "angles[0]"] {
            assert!(t.iter().any(|d| d.name == f), "missing field {f}");
        }
        assert_eq!(t.len(), 48, "player state has 48 delta fields");
    }

    #[test]
    fn table_field_counts_match_the_capture() {
        let s = walk(SIGNON);
        for (name, n) in [
            ("event_t", 14),
            ("weapon_data_t", 18),
            ("usercmd_t", 15),
            ("custom_entity_state_t", 19),
            ("entity_state_player_t", 48),
            ("entity_state_t", 52),
            ("clientdata_t", 47),
        ] {
            assert_eq!(s.registry.get(name).unwrap().len(), n, "{name}");
        }
    }

    #[test]
    fn the_entire_signon_stream_parses() {
        // Every message in a real signon is understood -- nothing unknown,
        // nothing left over. Any layout error desynchronises the stream and
        // this drops to a partial byte count immediately.
        let s = walk(SIGNON);
        assert_eq!(
            s.stopped_on, None,
            "halted on an unknown message at byte {}",
            s.stopped_at
        );
        assert_eq!(
            s.stopped_at,
            SIGNON.len(),
            "consumed {} of {} bytes",
            s.stopped_at,
            SIGNON.len()
        );
        eprintln!(
            "signon fully parsed: {} bytes, {} delta tables, signon_num={:?}",
            s.stopped_at,
            s.registry.len(),
            s.signon_num
        );
    }

    #[test]
    fn walking_truncated_data_stops_cleanly() {
        for cut in [1usize, 50, 300, 700, 3000] {
            let s = walk(&SIGNON[..cut.min(SIGNON.len())]);
            assert!(s.stopped_at <= cut, "must not read past the end");
        }
    }

    #[test]
    fn an_unknown_message_halts_rather_than_guessing() {
        let s = walk(&[svc::SVC_NOP, 0xEE, 0x01, 0x02]);
        assert_eq!(s.stopped_on, Some(0xEE));
        assert_eq!(s.stopped_at, 1);
    }
}
