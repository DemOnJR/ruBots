//! Server-to-client message identifiers.
//!
//! Verified from `handleMsg` (`0x1406F12A0`): the dispatch tests
//! `cmp rcx, 0x40 / jl` before anything else, so IDs **below 64** are engine
//! messages handled by the switch, and IDs **64 and above** are user messages
//! resolved through a registration map (which is how `#Bomb_Planted`,
//! `#Team_Select` and friends arrive). `0xFF` is treated specially.
//!
//! The individual engine IDs below are the standard GoldSrc set. The
//! functions the original dispatches to line up one-for-one with them —
//! `parseServerinfo`, `onDeltaDescription`, `onResourceList`, `onStufftext`,
//! `onSignonNum`, `respondCvar`, `parseClientData`, `parsePacketEntities`,
//! `parseSpawnBaseline`, `parseEvent`, `parseEventReliable`, `parseSound`,
//! `parseTempEntity`, `parsePings`.

/// First user-message id. Verified: `cmp rcx, 0x40`.
pub const USER_MESSAGE_BASE: u8 = 64;

/// Reserved marker the dispatch special-cases.
pub const SVC_SENTINEL: u8 = 0xFF;

pub const SVC_BAD: u8 = 0;
pub const SVC_NOP: u8 = 1;
pub const SVC_DISCONNECT: u8 = 2;
pub const SVC_EVENT: u8 = 3;
pub const SVC_VERSION: u8 = 4;
pub const SVC_SETVIEW: u8 = 5;
pub const SVC_SOUND: u8 = 6;
pub const SVC_TIME: u8 = 7;
pub const SVC_PRINT: u8 = 8;
pub const SVC_STUFFTEXT: u8 = 9;
pub const SVC_SETANGLE: u8 = 10;
pub const SVC_SERVERINFO: u8 = 11;
pub const SVC_LIGHTSTYLE: u8 = 12;
pub const SVC_UPDATEUSERINFO: u8 = 13;
pub const SVC_DELTADESCRIPTION: u8 = 14;
pub const SVC_CLIENTDATA: u8 = 15;
pub const SVC_STOPSOUND: u8 = 16;
pub const SVC_PINGS: u8 = 17;
pub const SVC_PARTICLE: u8 = 18;
pub const SVC_DAMAGE: u8 = 19;
pub const SVC_SPAWNSTATIC: u8 = 20;
pub const SVC_EVENT_RELIABLE: u8 = 21;
pub const SVC_SPAWNBASELINE: u8 = 22;
pub const SVC_TEMP_ENTITY: u8 = 23;
pub const SVC_SETPAUSE: u8 = 24;
pub const SVC_SIGNONNUM: u8 = 25;
pub const SVC_CENTERPRINT: u8 = 26;
pub const SVC_SPAWNSTATICSOUND: u8 = 29;
pub const SVC_INTERMISSION: u8 = 30;
pub const SVC_CDTRACK: u8 = 32;
pub const SVC_WEAPONANIM: u8 = 35;
pub const SVC_DECALNAME: u8 = 36;
pub const SVC_ROOMTYPE: u8 = 37;
pub const SVC_ADDANGLE: u8 = 38;
pub const SVC_NEWUSERMSG: u8 = 39;
pub const SVC_PACKETENTITIES: u8 = 40;
pub const SVC_DELTAPACKETENTITIES: u8 = 41;
pub const SVC_CHOKE: u8 = 42;
pub const SVC_RESOURCELIST: u8 = 43;
pub const SVC_NEWMOVEVARS: u8 = 44;
pub const SVC_RESOURCEREQUEST: u8 = 45;
pub const SVC_CUSTOMIZATION: u8 = 46;
pub const SVC_CROSSHAIRANGLE: u8 = 47;
pub const SVC_SOUNDFADE: u8 = 48;
pub const SVC_FILETXFERFAILED: u8 = 49;
pub const SVC_HLTV: u8 = 50;
pub const SVC_DIRECTOR: u8 = 51;
pub const SVC_VOICEINIT: u8 = 52;
pub const SVC_VOICEDATA: u8 = 53;
pub const SVC_SENDEXTRAINFO: u8 = 54;
pub const SVC_TIMESCALE: u8 = 55;
pub const SVC_RESOURCELOCATION: u8 = 56;
pub const SVC_SENDCVARVALUE: u8 = 57;
pub const SVC_SENDCVARVALUE2: u8 = 58;

/// Is this a registered user message rather than an engine one?
pub fn is_user_message(id: u8) -> bool {
    id >= USER_MESSAGE_BASE && id != SVC_SENTINEL
}

/// Human-readable name, for tracing (the original logs `[post] svc=%d @%d`).
pub fn name(id: u8) -> &'static str {
    match id {
        SVC_BAD => "svc_bad",
        SVC_NOP => "svc_nop",
        SVC_DISCONNECT => "svc_disconnect",
        SVC_EVENT => "svc_event",
        SVC_VERSION => "svc_version",
        SVC_SETVIEW => "svc_setview",
        SVC_SOUND => "svc_sound",
        SVC_TIME => "svc_time",
        SVC_PRINT => "svc_print",
        SVC_STUFFTEXT => "svc_stufftext",
        SVC_SETANGLE => "svc_setangle",
        SVC_SERVERINFO => "svc_serverinfo",
        SVC_LIGHTSTYLE => "svc_lightstyle",
        SVC_UPDATEUSERINFO => "svc_updateuserinfo",
        SVC_DELTADESCRIPTION => "svc_deltadescription",
        SVC_CLIENTDATA => "svc_clientdata",
        SVC_PINGS => "svc_pings",
        SVC_EVENT_RELIABLE => "svc_event_reliable",
        SVC_SPAWNBASELINE => "svc_spawnbaseline",
        SVC_TEMP_ENTITY => "svc_temp_entity",
        SVC_SIGNONNUM => "svc_signonnum",
        SVC_NEWUSERMSG => "svc_newusermsg",
        SVC_PACKETENTITIES => "svc_packetentities",
        SVC_DELTAPACKETENTITIES => "svc_deltapacketentities",
        SVC_RESOURCELIST => "svc_resourcelist",
        SVC_SENDCVARVALUE => "svc_sendcvarvalue",
        SVC_SENDCVARVALUE2 => "svc_sendcvarvalue2",
        id if is_user_message(id) => "user_message",
        _ => "svc_unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_message_boundary_is_sixty_four() {
        assert_eq!(USER_MESSAGE_BASE, 0x40);
        assert!(!is_user_message(63));
        assert!(is_user_message(64));
        assert!(is_user_message(200));
    }

    #[test]
    fn the_sentinel_is_not_a_user_message() {
        // `cmp rcx, 0xff / je` routes 255 away from the user-message map.
        assert!(!is_user_message(SVC_SENTINEL));
    }

    #[test]
    fn engine_ids_all_sit_below_the_boundary() {
        for id in [
            SVC_SERVERINFO,
            SVC_DELTADESCRIPTION,
            SVC_CLIENTDATA,
            SVC_PACKETENTITIES,
            SVC_DELTAPACKETENTITIES,
            SVC_SPAWNBASELINE,
            SVC_RESOURCELIST,
            SVC_SIGNONNUM,
            SVC_STUFFTEXT,
            SVC_SENDCVARVALUE2,
        ] {
            assert!(id < USER_MESSAGE_BASE, "{} is not an engine id", name(id));
        }
    }

    #[test]
    fn names_are_available_for_tracing() {
        assert_eq!(name(SVC_SERVERINFO), "svc_serverinfo");
        assert_eq!(name(70), "user_message");
        assert_eq!(name(200), "user_message");
    }
}
