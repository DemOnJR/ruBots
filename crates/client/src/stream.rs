//! Walking a running-phase `svc_*` stream.
//!
//! [`crate::signon`] walks the signon burst; this walks what arrives
//! afterwards, where the interesting content is **user messages** — `TeamInfo`,
//! `ScoreInfo`, `VGUIMenu` and the rest of the game DLL's traffic.
//!
//! User messages are self-describing only if you have the registration table
//! the server sent during the signon (`svc_newusermsg`: id, size, 16-byte
//! name). A registered size of **255** means the message was registered with
//! `-1`, i.e. variable length with a leading length byte — that is how
//! `TeamInfo`, `VGUIMenu`, `SayText` and `TextMsg` are declared in
//! ReGameDLL_CS, while e.g. `ScoreInfo` is a fixed 9 bytes.
//!
//! Several engine messages are **bit-packed** (`svc_clientdata`,
//! `svc_packetentities`, …) and cannot be skipped byte-wise without the delta
//! tables. The walk stops cleanly at those and reports where, exactly as the
//! signon walker does — a mis-sized message desynchronises everything after it,
//! so halting beats guessing.

use std::collections::HashMap;

use crate::messages::Reader;
use crate::svc;

/// A user message the server registered during the signon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserMsgDef {
    pub name: String,
    /// Wire size; **255** means variable length with a leading length byte.
    pub size: u8,
}

impl UserMsgDef {
    pub fn is_variable(&self) -> bool {
        self.size == 255
    }
}

/// id → definition, built from every `svc_newusermsg` seen.
pub type UserMsgTable = HashMap<u8, UserMsgDef>;

/// Scan assembled messages for `svc_newusermsg` registrations.
///
/// Layout: opcode, `u8` id, `u8` size, then a fixed 16-byte name field.
pub fn collect_user_messages(streams: &[Vec<u8>]) -> UserMsgTable {
    let mut out = UserMsgTable::new();
    for msg in streams {
        let mut i = 0usize;
        while i + 19 <= msg.len() {
            if msg[i] == svc::SVC_NEWUSERMSG {
                let id = msg[i + 1];
                let size = msg[i + 2];
                let name: String = msg[i + 3..i + 19]
                    .iter()
                    .take_while(|&&c| c != 0)
                    .map(|&c| c as char)
                    .collect();
                if !name.is_empty() && name.chars().all(|c| c.is_ascii_graphic()) {
                    out.insert(id, UserMsgDef { name, size });
                    i += 19;
                    continue;
                }
            }
            i += 1;
        }
    }
    out
}

/// One decoded item from the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// An engine message we walked over; payload excludes the opcode.
    Engine { id: u8, payload: Vec<u8> },
    /// A registered user message.
    User { id: u8, name: String, payload: Vec<u8> },
}

/// Result of walking one stream.
#[derive(Debug, Default, Clone)]
pub struct Walk {
    pub items: Vec<Item>,
    /// Byte offset the walk stopped at.
    pub stopped_at: usize,
    /// The message id that halted the walk, if any.
    pub stopped_on: Option<u8>,
}

impl Walk {
    /// Every user message of the given name, in order.
    pub fn user<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Item> + 'a {
        self.items.iter().filter(move |it| match it {
            Item::User { name: n, .. } => n == name,
            _ => false,
        })
    }

    /// How many user messages of this name appeared.
    pub fn count(&self, name: &str) -> usize {
        self.user(name).count()
    }
}

/// Payload length of `svc_temp_entity` by TE type, **excluding** the type byte.
///
/// Transcribed from the reference client's `TE_LENGTH[]`
/// (`rehlds/HLTV/Core/src/Server.cpp:1148-1158`). `-1` means the length is
/// dynamic (only `TE_BSPDECAL` and `TE_TEXTMESSAGE`); `-2` means the type does
/// not exist and the message cannot be stepped over at all.
const TE_LENGTH: [i8; 128] = [
    24, 20, 6, 11, 6, 10, 12, 17, 16, 6, 6, 6, 8, -1, 9, 19, //
    -2, 10, 16, 24, 24, 24, 10, 11, 16, 19, -2, 12, 16, -1, 19, 17, //
    -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, //
    -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, //
    -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, //
    -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, -2, //
    -2, -2, -2, 2, 10, 14, 12, 14, 9, 5, 17, 13, 24, 9, 17, 7, //
    10, 19, 19, 12, 7, 7, 9, 16, 18, 6, 10, 13, 7, 1, 18, 15, //
];

const TE_BSPDECAL: u8 = 13;
const TE_TEXTMESSAGE: u8 = 29;

/// Fixed byte length of the engine messages we can safely step over.
///
/// `None` means "not a fixed-length message" — it is bit-packed, or its length
/// depends on its own contents, and [`walk`] handles it separately or stops.
fn engine_len(id: u8) -> Option<usize> {
    Some(match id {
        svc::SVC_NOP => 0,
        svc::SVC_VERSION => 4,
        svc::SVC_SETVIEW => 2,
        svc::SVC_TIME => 4,
        svc::SVC_SETANGLE => 6,
        svc::SVC_STOPSOUND => 2,
        svc::SVC_PARTICLE => 11,
        svc::SVC_SETPAUSE => 1,
        svc::SVC_SIGNONNUM => 1,
        svc::SVC_SPAWNSTATICSOUND => 14,
        svc::SVC_INTERMISSION => 0,
        svc::SVC_CDTRACK => 2,
        svc::SVC_WEAPONANIM => 2,
        svc::SVC_ROOMTYPE => 2,
        svc::SVC_ADDANGLE => 2,
        svc::SVC_NEWUSERMSG => 18,
        svc::SVC_CHOKE => 0,
        svc::SVC_RESOURCEREQUEST => 8,
        svc::SVC_CROSSHAIRANGLE => 3,
        svc::SVC_SOUNDFADE => 4,
        svc::SVC_TIMESCALE => 4,
        // svc_serverinfo, svc_newmovevars, svc_updateuserinfo, svc_lightstyle,
        // svc_decalname, svc_customization, svc_temp_entity, svc_hltv,
        // svc_director, svc_voiceinit, svc_voicedata, svc_sendextrainfo,
        // svc_sendcvarvalue2 and the string messages are composite -- see walk.
        _ => return None,
    })
}

/// Length of a composite (self-describing but byte-aligned) message, given a
/// reader positioned just after its opcode. Returns the payload length, having
/// left the reader where it started.
///
/// Kept separate from [`engine_len`] because these need to *read* to know how
/// far to skip, which is exactly what makes them easy to get wrong and easy to
/// silently desynchronise on.
fn composite_len(id: u8, r: &mut Reader<'_>) -> Option<usize> {
    let start = r.pos();
    let len = (|| -> Option<usize> {
        match id {
            // byte + string
            svc::SVC_LIGHTSTYLE | svc::SVC_DECALNAME => {
                r.u8()?;
                r.cstr()?;
            }
            // string + byte
            svc::SVC_VOICEINIT | svc::SVC_SENDEXTRAINFO => {
                r.cstr()?;
                r.u8()?;
            }
            // long + string
            svc::SVC_SENDCVARVALUE2 => {
                r.bytes(4)?;
                r.cstr()?;
            }
            // byte index, long id, string info, 16-byte hash
            svc::SVC_UPDATEUSERINFO => {
                r.u8()?;
                r.bytes(4)?;
                r.cstr()?;
                r.bytes(16)?;
            }
            // 16 floats, byte, 8 floats, string (sv_main.cpp:1150-1180)
            svc::SVC_NEWMOVEVARS => {
                r.bytes(16 * 4)?;
                r.u8()?;
                r.bytes(8 * 4)?;
                r.cstr()?;
            }
            // byte slot, byte type, string, short index, long size, byte flags,
            // and an MD5 only when RES_CUSTOM is set (Server.cpp:1369).
            svc::SVC_CUSTOMIZATION => {
                r.u8()?;
                r.u8()?;
                r.cstr()?;
                r.bytes(2)?;
                r.bytes(4)?;
                let flags = r.u8()?;
                if flags & 0x04 != 0 {
                    r.bytes(16)?;
                }
            }
            // byte length, then that many bytes.
            svc::SVC_DIRECTOR => {
                let n = r.u8()?;
                r.bytes(usize::from(n))?;
            }
            // byte slot, short length, then that many bytes.
            svc::SVC_VOICEDATA => {
                r.u8()?;
                let n = i16::from_le_bytes(r.bytes(2)?.try_into().ok()?);
                r.bytes(usize::try_from(n).ok()?)?;
            }
            // byte cmd; only HLTV_STATUS (1) carries a payload
            // (Server.cpp:1802-1830). The old table treated this as always
            // 1 byte, which desynchronises on a status message.
            svc::SVC_HLTV => {
                let cmd = r.u8()?;
                if cmd == 1 {
                    r.bytes(10)?;
                }
            }
            // byte type, then TE_LENGTH[type], with two dynamic types.
            svc::SVC_TEMP_ENTITY => {
                let ty = r.u8()?;
                let n = *TE_LENGTH.get(usize::from(ty))?;
                match n {
                    -2 => return None,
                    -1 if ty == TE_BSPDECAL => {
                        r.bytes(8)?;
                        let w = u16::from_le_bytes(r.bytes(2)?.try_into().ok()?);
                        if w != 0 {
                            r.bytes(2)?;
                        }
                    }
                    -1 if ty == TE_TEXTMESSAGE => {
                        r.bytes(5)?;
                        let channel = r.u8()?;
                        if channel == 2 {
                            r.bytes(2)?;
                        }
                        r.bytes(14)?;
                        r.cstr()?;
                    }
                    -1 => return None,
                    n => {
                        r.bytes(usize::try_from(n).ok()?)?;
                    }
                }
            }
            _ => return None,
        }
        Some(r.pos() - start)
    })();
    r.seek(start);
    len
}

/// Walk a running-phase stream, decoding user messages against `table`.
pub fn walk(data: &[u8], table: &UserMsgTable) -> Walk {
    let mut out = Walk::default();
    let mut r = Reader::new(data);

    loop {
        let start = r.pos();
        let Some(id) = r.u8() else {
            out.stopped_at = start;
            return out;
        };

        // User messages first: their ids (>= 64) never collide with engine ones.
        if let Some(def) = table.get(&id) {
            let len = if def.is_variable() {
                match r.u8() {
                    Some(n) => usize::from(n),
                    None => {
                        out.stopped_at = start;
                        out.stopped_on = Some(id);
                        return out;
                    }
                }
            } else {
                usize::from(def.size)
            };
            match r.bytes(len) {
                Some(p) => out.items.push(Item::User {
                    id,
                    name: def.name.clone(),
                    payload: p.to_vec(),
                }),
                None => {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
            }
            continue;
        }

        // Nul-terminated string messages.
        if matches!(
            id,
            svc::SVC_PRINT
                | svc::SVC_STUFFTEXT
                | svc::SVC_CENTERPRINT
                | svc::SVC_DISCONNECT
                | svc::SVC_FILETXFERFAILED
                | svc::SVC_RESOURCELOCATION
                | svc::SVC_SENDCVARVALUE
        ) {
            match r.cstr() {
                Some(s) => out.items.push(Item::Engine {
                    id,
                    payload: s.into_bytes(),
                }),
                None => {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
            }
            continue;
        }

        // Fixed-length, then self-describing-but-byte-aligned.
        let n = engine_len(id).or_else(|| composite_len(id, &mut r));
        match n {
            Some(n) => match r.bytes(n) {
                Some(p) => out.items.push(Item::Engine { id, payload: p.to_vec() }),
                None => {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
            },
            None => {
                // Bit-packed, or a message whose length we genuinely cannot
                // determine. Stop rather than desynchronise: a wrong length
                // here does not fail loudly, it silently reinterprets the rest
                // of the stream as garbage.
                out.stopped_at = start;
                out.stopped_on = Some(id);
                return out;
            }
        }
    }
}

#[cfg(test)]
mod composite_tests {
    use super::*;

    /// Every composite message must be stepped over exactly, leaving the walk
    /// on the next opcode. A wrong length here is silent: the stream is simply
    /// reinterpreted as garbage from that point on, so the assertion that
    /// matters is "the message AFTER it still decodes".
    fn walks_over(payload: Vec<u8>) {
        let table = UserMsgTable::new();
        // Sandwich the message between two unmistakable markers.
        let mut data = payload;
        data.push(svc::SVC_SIGNONNUM);
        data.push(0x42);
        let w = walk(&data, &table);
        assert_eq!(w.stopped_on, None, "walk halted: {:?}", w.stopped_on);
        assert_eq!(w.stopped_at, data.len(), "did not consume the whole stream");
        match w.items.last() {
            Some(Item::Engine { id, payload }) => {
                assert_eq!(*id, svc::SVC_SIGNONNUM);
                assert_eq!(payload, &[0x42]);
            }
            other => panic!("trailing marker was not decoded: {other:?}"),
        }
    }

    fn cstr(s: &str) -> Vec<u8> {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        v
    }

    #[test]
    fn byte_then_string_messages() {
        for id in [svc::SVC_LIGHTSTYLE, svc::SVC_DECALNAME] {
            let mut m = vec![id, 3];
            m.extend(cstr("mmnmmnmmnmmn"));
            walks_over(m);
        }
    }

    #[test]
    fn string_then_byte_messages() {
        for id in [svc::SVC_VOICEINIT, svc::SVC_SENDEXTRAINFO] {
            let mut m = vec![id];
            m.extend(cstr("voice_speex"));
            m.push(5);
            walks_over(m);
        }
    }

    #[test]
    fn updateuserinfo_carries_a_trailing_md5() {
        let mut m = vec![svc::SVC_UPDATEUSERINFO, 2];
        m.extend_from_slice(&7u32.to_le_bytes());
        m.extend(cstr("\\name\\Daemon\\rate\\100000"));
        m.extend_from_slice(&[0xAB; 16]);
        walks_over(m);
    }

    #[test]
    fn newmovevars_is_sixteen_floats_a_byte_eight_floats_and_a_string() {
        let mut m = vec![svc::SVC_NEWMOVEVARS];
        m.extend_from_slice(&[0u8; 16 * 4]);
        m.push(1);
        m.extend_from_slice(&[0u8; 8 * 4]);
        m.extend(cstr("desert"));
        walks_over(m);
    }

    #[test]
    fn customization_only_carries_a_hash_when_res_custom_is_set() {
        for (flags, extra) in [(0u8, 0usize), (0x04, 16)] {
            let mut m = vec![svc::SVC_CUSTOMIZATION, 1, 3];
            m.extend(cstr("tempdecal.wad"));
            m.extend_from_slice(&9i16.to_le_bytes());
            m.extend_from_slice(&512i32.to_le_bytes());
            m.push(flags);
            m.extend(std::iter::repeat(0xCDu8).take(extra));
            walks_over(m);
        }
    }

    #[test]
    fn length_prefixed_messages() {
        let mut m = vec![svc::SVC_DIRECTOR, 5, 1, 2, 3, 4, 5];
        walks_over(std::mem::take(&mut m));

        let mut m = vec![svc::SVC_VOICEDATA, 2];
        m.extend_from_slice(&4i16.to_le_bytes());
        m.extend_from_slice(&[9, 9, 9, 9]);
        walks_over(m);
    }

    /// Only HLTV_STATUS carries a payload. Treating svc_hltv as always one
    /// byte -- which the previous table did -- desynchronises on a status
    /// message and loses the rest of the stream.
    #[test]
    fn svc_hltv_length_depends_on_its_command_byte() {
        walks_over(vec![svc::SVC_HLTV, 0]);

        let mut m = vec![svc::SVC_HLTV, 1];
        m.extend_from_slice(&[0u8; 10]);
        walks_over(m);
    }

    #[test]
    fn temp_entities_use_the_length_table() {
        // TE_GUNSHOT (2) is 6 bytes of payload after the type byte.
        let mut m = vec![svc::SVC_TEMP_ENTITY, 2];
        m.extend_from_slice(&[0u8; 6]);
        walks_over(m);

        // TE_EXPLOSION (3) is 11.
        let mut m = vec![svc::SVC_TEMP_ENTITY, 3];
        m.extend_from_slice(&[0u8; 11]);
        walks_over(m);
    }

    #[test]
    fn te_bspdecal_is_two_bytes_longer_when_its_entity_word_is_set() {
        for (word, tail) in [(0u16, 0usize), (7, 2)] {
            let mut m = vec![svc::SVC_TEMP_ENTITY, TE_BSPDECAL];
            m.extend_from_slice(&[0u8; 8]);
            m.extend_from_slice(&word.to_le_bytes());
            m.extend(std::iter::repeat(0u8).take(tail));
            walks_over(m);
        }
    }

    #[test]
    fn te_textmessage_has_a_variable_channel_block_and_a_trailing_string() {
        for channel in [1u8, 2] {
            let mut m = vec![svc::SVC_TEMP_ENTITY, TE_TEXTMESSAGE];
            m.extend_from_slice(&[0u8; 5]);
            m.push(channel);
            if channel == 2 {
                m.extend_from_slice(&[0u8; 2]);
            }
            m.extend_from_slice(&[0u8; 14]);
            m.extend(cstr("Bomb has been planted"));
            walks_over(m);
        }
    }

    /// A type the table marks as nonexistent must halt the walk rather than
    /// invent a length.
    #[test]
    fn an_impossible_temp_entity_type_halts_instead_of_guessing() {
        let table = UserMsgTable::new();
        let data = vec![svc::SVC_TEMP_ENTITY, 40, 0, 0, 0];
        let w = walk(&data, &table);
        assert_eq!(w.stopped_on, Some(svc::SVC_TEMP_ENTITY));
        assert_eq!(w.stopped_at, 0);
    }

    /// A truncated composite must halt, not read past the end.
    #[test]
    fn truncation_halts_cleanly() {
        let table = UserMsgTable::new();
        for data in [
            vec![svc::SVC_DIRECTOR, 200, 1, 2],
            vec![svc::SVC_LIGHTSTYLE, 3, b'a', b'b'],
            vec![svc::SVC_NEWMOVEVARS, 0, 0],
        ] {
            let w = walk(&data, &table);
            assert!(w.stopped_on.is_some(), "should have halted on {data:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> UserMsgTable {
        let mut t = UserMsgTable::new();
        t.insert(86, UserMsgDef { name: "TeamInfo".into(), size: 255 });
        t.insert(85, UserMsgDef { name: "ScoreInfo".into(), size: 9 });
        t
    }

    #[test]
    fn registrations_are_collected_from_a_stream() {
        // svc_newusermsg, id 86, size 255 (-1), then a 16-byte name field.
        let mut msg = vec![svc::SVC_NEWUSERMSG, 86, 255];
        let mut name = [0u8; 16];
        name[..8].copy_from_slice(b"TeamInfo");
        msg.extend_from_slice(&name);
        let t = collect_user_messages(&[msg]);
        assert_eq!(t[&86], UserMsgDef { name: "TeamInfo".into(), size: 255 });
        assert!(t[&86].is_variable());
    }

    #[test]
    fn a_variable_user_message_reads_its_length_byte() {
        // TeamInfo: id, length, payload.
        let stream = vec![86u8, 5, b'C', b'T', 0, 1, 2];
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, None, "should consume the whole stream");
        assert_eq!(w.count("TeamInfo"), 1);
        match &w.items[0] {
            Item::User { payload, .. } => assert_eq!(payload, &[b'C', b'T', 0, 1, 2]),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_fixed_size_user_message_uses_its_registered_size() {
        let stream = vec![85u8, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, None);
        assert_eq!(w.count("ScoreInfo"), 1);
    }

    #[test]
    fn engine_and_user_messages_interleave() {
        let mut stream = vec![svc::SVC_TIME, 0, 0, 0, 0, svc::SVC_NOP];
        stream.extend_from_slice(&[86, 3, b'T', b'T', 0]);
        stream.push(svc::SVC_CHOKE);
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, None, "stopped at {}", w.stopped_at);
        assert_eq!(w.count("TeamInfo"), 1);
        assert_eq!(w.items.len(), 4);
    }

    #[test]
    fn a_bit_packed_message_halts_the_walk_rather_than_desyncing() {
        // svc_packetentities cannot be stepped over byte-wise.
        let stream = vec![svc::SVC_NOP, svc::SVC_PACKETENTITIES, 0xAA, 0xBB];
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, Some(svc::SVC_PACKETENTITIES));
        assert_eq!(w.stopped_at, 1);
    }

    #[test]
    fn a_truncated_user_message_halts_cleanly() {
        let stream = vec![86u8, 40, b'x'];
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, Some(86));
        assert_eq!(w.stopped_at, 0);
    }

    #[test]
    fn string_messages_are_consumed_to_their_terminator() {
        let mut stream = vec![svc::SVC_STUFFTEXT];
        stream.extend_from_slice(b"allow_shaders 0\n\0");
        stream.push(svc::SVC_CHOKE);
        let w = walk(&stream, &table());
        assert_eq!(w.stopped_on, None);
        assert_eq!(w.items.len(), 2);
    }
}
