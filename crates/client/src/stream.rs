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

/// Fixed byte length of the engine messages we can safely step over.
///
/// `None` means "not walkable here" — either bit-packed or variable in a way
/// that needs its own parser — and the walk stops.
fn engine_len(id: u8) -> Option<usize> {
    Some(match id {
        svc::SVC_NOP => 0,
        svc::SVC_TIME => 4,
        svc::SVC_SETVIEW => 2,
        svc::SVC_SETANGLE => 6,
        svc::SVC_ADDANGLE => 2,
        svc::SVC_SIGNONNUM => 1,
        svc::SVC_CDTRACK => 2,
        svc::SVC_WEAPONANIM => 2,
        svc::SVC_ROOMTYPE => 2,
        svc::SVC_INTERMISSION => 0,
        svc::SVC_CHOKE => 0,
        svc::SVC_NEWUSERMSG => 18,
        svc::SVC_RESOURCEREQUEST => 8,
        svc::SVC_CROSSHAIRANGLE => 3,
        svc::SVC_SOUNDFADE => 4,
        svc::SVC_TIMESCALE => 4,
        svc::SVC_HLTV => 1,
        _ => return None,
    })
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

        match engine_len(id) {
            Some(n) => match r.bytes(n) {
                Some(p) => out.items.push(Item::Engine { id, payload: p.to_vec() }),
                None => {
                    out.stopped_at = start;
                    out.stopped_on = Some(id);
                    return out;
                }
            },
            None => {
                // Bit-packed or unknown: stop rather than desynchronise.
                out.stopped_at = start;
                out.stopped_on = Some(id);
                return out;
            }
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
