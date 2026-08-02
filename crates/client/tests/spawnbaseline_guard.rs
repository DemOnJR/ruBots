//! The real bytes that cost a live run every other player it could see.
//!
//! `captures/swarm/Bot02.bin`, record 165: an 894-byte round-restart burst of
//! `TeamInfo` / `ScoreInfo` / `Money` user messages. Byte 344 of it happens to
//! be `0x16` — 22, `svc_spawnbaseline`. `client::world::Decoder::absorb_baselines`
//! finds `svc_spawnbaseline` by scanning for that byte, so it handed everything
//! after it to `proto::entity::parse_spawn_baseline`, which **accepted it**:
//! nine baselines and sixty-three *instanced* baselines.
//!
//! Nothing failed at that moment. But the instanced count gates a conditional
//! bit in every entity header (`SV_WriteDeltaHeader`, `sv_main.cpp:4450` —
//! ReGameDLL creates no instanced baselines, `regamedll/dlls/client.cpp:5247`,
//! so the real server never writes that bit). From the next datagram on, every
//! `svc_packetentities` header was read one bit off and every frame was
//! refused. The live log:
//!
//! ```text
//! t+ 2s  world: 11 entities, 0 players | ok=284  ents=284 errs=0    stop=None
//! t+ 4s  world: 19 entities, 0 players | ok=394  ents=339 errs=55   stop=Some(40)
//! ...
//! end    world: 19 entities, 0 players | ok=7602 ents=339 errs=7263 stop=Some(40)
//! ```
//!
//! `ents` never moves past 339 again. The fixture here is that record's tail
//! from just after the byte 22, and the test is that it is now refused.

use proto::bitbuf::BitReader;
use proto::entity::parse_spawn_baseline;

/// Record 165 of `captures/swarm/Bot02.bin`, from the byte after the `0x16`.
const FALSE_POSITIVE: &[u8] = include_bytes!("fixtures/false_spawnbaseline.bin");

const SIGNON: &[u8] = include_bytes!("fixtures/signon.bin");

#[test]
fn a_user_message_burst_is_not_mistaken_for_a_spawnbaseline() {
    let signon = client::walk_signon(SIGNON);
    let reg = &signon.registry;
    assert!(reg.get("entity_state_t").is_some(), "signon fixture loaded");

    let mut r = BitReader::new(FALSE_POSITIVE);
    let got = parse_spawn_baseline(&mut r, reg, 12);

    // Read as baselines, this stream is (number, entityType) =
    // (768,0) (3,0) (528,0) (0,0) (816,0) (25,0) (0,1) (680,2) (0,0) ...
    //
    // Two of `SV_CreateBaseline`'s invariants are broken inside the first two
    // entries, and the earlier one wins: `entityType` is only ever
    // `ENTITY_NORMAL` or `ENTITY_BEAM` (`sv_main.cpp:5848-5851`, sent at
    // `:5897`), and 0 is neither -- so this comes back as
    // `EntityError::BadEntityType(0)`. The numbers then also fall, 768 to 3,
    // where the writer's loop counter can only rise (`:5891-5896`), which is
    // the backstop if a mod ever does emit a type outside the pair.
    //
    // Asserted as `is_err` rather than by variant deliberately: the variants
    // did not exist before the fix, and this test has to be able to compile
    // against the parser it is a regression test for.
    assert!(
        got.is_err(),
        "a user-message burst decoded as a baseline block: {:?}",
        got.map(|b| (b.by_number.len(), b.instanced.len()))
    );
}

/// The specific consequence, spelled out: whatever a mis-identified stream
/// decodes to must never reach `PacketCtx::instanced`, because its *length* is
/// what decides whether an entity header carries the instanced-baseline bit.
#[test]
fn the_false_positive_never_yields_an_instanced_baseline_count() {
    let signon = client::walk_signon(SIGNON);
    let mut r = BitReader::new(FALSE_POSITIVE);
    let instanced = parse_spawn_baseline(&mut r, &signon.registry, 12)
        .map(|b| b.instanced.len())
        .unwrap_or(0);
    assert_eq!(
        instanced, 0,
        "63 bogus instanced baselines shifted every later entity header by a bit"
    );
}
