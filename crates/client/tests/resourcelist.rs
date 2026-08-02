//! Parse a real `svc_resourcelist` captured from HLDS.
//!
//! The layout was recovered from this exact message, so the test is what
//! keeps it honest: every one of the 793 names must decode, and the sizes
//! must be real file sizes.

use proto::bitbuf::BitReader;
use proto::resources::{parse_resource_list_full, ConsistencyList, ResourceType};

/// Captured 2026-08-01; starts at the `svc_resourcelist` opcode byte.
const RESLIST: &[u8] = include_bytes!("fixtures/svc_resourcelist.bin");

/// The spawncount of the session this fixture was captured in.
///
/// Recovered rather than recorded: the reserved blocks are munged with it, and
/// 5 is the only key in 0..200000 that decodes all 52 of them to a valid
/// FORCE_TYPE with sane bounds.
const SPAWNCOUNT: u32 = 5;

fn parse() -> Vec<proto::resources::Resource> {
    parse_full().0
}

fn parse_full() -> (Vec<proto::resources::Resource>, ConsistencyList, usize) {
    assert_eq!(RESLIST[0], 43, "fixture must start at svc_resourcelist");
    let mut r = BitReader::new(&RESLIST[1..]);
    let (res, cons) = parse_resource_list_full(&mut r);
    assert!(!r.overflowed(), "the parse ran off the end of the message");
    // Bit blocks are byte-framed: MSG_EndBitReading advances by ceil(bits/8).
    let consumed = r.byte_pos() + usize::from(r.bit_offset() > 0);
    (res, cons, consumed)
}

#[test]
fn every_entry_in_the_live_list_decodes() {
    let res = parse();
    assert_eq!(res.len(), 793, "the capture holds 793 resources");
    for (i, r) in res.iter().enumerate() {
        assert!(
            !r.name.is_empty() && r.name.chars().all(|c| c.is_ascii_graphic() || c == ' '),
            "entry {i} has an unreadable name {:?}",
            r.name
        );
    }
}

#[test]
fn decoded_sizes_are_real_file_sizes() {
    // A wrong bit width shows up here first: sizes come out as clean
    // powers-of-two multiples of the truth.
    let res = parse();
    let find = |n: &str| res.iter().find(|r| r.name == n).unwrap_or_else(|| panic!("missing {n}"));

    assert_eq!(find("models/player.mdl").size, 2_329_328);
    assert_eq!(find("sprites/scope_arc.tga").size, 262_188);
    assert_eq!(find("models/player/leet/leet.mdl").size, 2_401_992);
}

#[test]
fn indices_are_dense_and_ordered_within_a_type() {
    let res = parse();
    // player.mdl is 149 and leet.mdl 150 -- consecutive, which is how the
    // off-by-four-bits index bug was caught.
    let a = res.iter().find(|r| r.name == "models/player.mdl").unwrap();
    let b = res.iter().find(|r| r.name == "models/player/leet/leet.mdl").unwrap();
    assert_eq!(a.index, 149);
    assert_eq!(b.index, 150);
}

#[test]
fn the_type_mix_looks_like_a_counter_strike_map() {
    let res = parse();
    let count = |t: ResourceType| res.iter().filter(|r| r.res_type == Some(t)).count();
    assert_eq!(count(ResourceType::Sound), 313);
    assert_eq!(count(ResourceType::Model), 221);
    assert_eq!(count(ResourceType::Decal), 225);
    assert_eq!(count(ResourceType::EventScript), 29);
    assert_eq!(count(ResourceType::Generic), 5);
}

#[test]
fn bounds_checked_entries_carry_their_reserved_block() {
    let res = parse();
    let with = res.iter().filter(|r| r.reserved.is_some()).count();
    assert_eq!(with, 52, "52 entries carry a reserved (bounds) block");

    // The blob is `rguc_reserved`, NOT an MD5: it is COM_Munge'd model bounds
    // plus a check_type byte (sv_user.cpp:287-312). Answering consistency with
    // its first four bytes as a hash was always wrong.
    assert!(res
        .iter()
        .any(|r| r.name == "models/player.mdl" && r.reserved.is_some()));

    // RES_CUSTOM is masked off before transmission (sv_main.cpp:1240), so the
    // 16-byte MD5 branch is unreachable and must never fire on real data.
    assert!(
        res.iter().all(|r| r.md5.is_none()),
        "RES_CUSTOM cannot survive the 0x03 flags mask, so no MD5 can be on the wire"
    );
}

/// The assertion that catches *any* width error anywhere in the message.
///
/// The resource entries and the consistency list share one bit block, so a
/// single wrong field width leaves the cursor short or long. Landing exactly on
/// the last byte of a 19,500-byte capture is not something a wrong layout does
/// by luck.
#[test]
fn the_parse_consumes_the_message_exactly() {
    let (_res, _cons, consumed) = parse_full();
    assert_eq!(
        consumed,
        RESLIST.len() - 1,
        "bit cursor must land on the end of the message (opcode byte excluded)"
    );
}

/// The tail is the only authority on what to answer.
#[test]
fn the_consistency_tail_decodes() {
    let (res, cons, _) = parse_full();
    assert!(
        cons.should_send,
        "this capture was taken against a server with consistency enabled"
    );
    // 62, not 52. The demand list is NOT the same set as the resources that
    // carry a reserved block: 52 of them do (those are answered with bounds),
    // and the other 10 are `force_exactfile` entries with a zero reserved
    // block, which need a real MD5. Confirming that split is the whole reason
    // a bot can pass consistency at all.
    assert_eq!(cons.indices.len(), 62, "the server demands 62 answers");

    // The demand index is the ARRAY POSITION in the server's resource list,
    // not `Resource::index` (which is the precache slot within a type, and so
    // restarts from zero for models, sounds, decals...). Matching on the wrong
    // one resolves 4 of 62.
    let demanded: Vec<&proto::resources::Resource> = cons
        .indices
        .iter()
        .filter_map(|i| res.get(*i as usize))
        .collect();
    assert_eq!(demanded.len(), 62, "every demand must name a real resource");

    let bounds = demanded.iter().filter(|r| r.reserved.is_some()).count();
    assert_eq!(bounds, 52, "52 demands are answerable from the wire alone");
    assert_eq!(
        demanded.len() - bounds,
        10,
        "10 demands are force_exactfile and need local content"
    );

    // Every index must name a real resource. A wrong delta/absolute split
    // shows up here as an out-of-range index long before it shows up on a wire.
    for &i in &cons.indices {
        assert!(
            (i as usize) < res.len(),
            "consistency index {i} is outside the {}-entry list",
            res.len()
        );
    }

    // The whole reason a bot with no game files can pass: bounds demands are
    // answered by echoing back the server's own numbers.
    //
    // The reserved blocks are COM_Munge'd with the spawncount, which this
    // fixture does not carry (it starts at svc_resourcelist, and the spawncount
    // is in the svc_resourcerequest that precedes it). SPAWNCOUNT is the unique
    // value in 0..200000 for which all 52 blocks decode to a valid FORCE_TYPE
    // with finite, sane bounds -- which is itself strong evidence that both the
    // munge and the block layout are right.
    let demands = proto::consistency::demands(&res, &cons, SPAWNCOUNT);
    assert_eq!(demands.len(), 62);

    let exact: Vec<&str> = demands
        .iter()
        .filter(|d| matches!(d, proto::consistency::Demand::ExactFile { .. }))
        .map(|d| d.path())
        .collect();
    assert_eq!(
        exact.len(),
        10,
        "only these need real content on disk: {exact:?}"
    );
    assert!(
        exact.iter().all(|p| p.starts_with("sprites/")),
        "the exact-file demands are stock, map-independent sprites: {exact:?}"
    );

    // And the bounds we would echo back must be real model bounds.
    let player = demands
        .iter()
        .find(|d| d.path() == "models/player.mdl")
        .expect("player.mdl is consistency-checked");
    match player {
        proto::consistency::Demand::Bounds { mins, maxs, check, .. } => {
            assert_eq!(*check, proto::consistency::ForceType::ModelSameBounds);
            for i in 0..3 {
                assert!(mins[i] < maxs[i], "degenerate bounds on axis {i}");
                assert!(mins[i].is_finite() && maxs[i].is_finite());
                // Studio model bounds are whole sixteenths; garbage is not.
                assert_eq!(mins[i] * 16.0, (mins[i] * 16.0).round());
            }
        }
        other => panic!("player.mdl should be a bounds demand, got {other:?}"),
    }

    // Strictly ascending: the encoding is a running delta from `lastcheck`.
    assert!(
        cons.indices.windows(2).all(|w| w[0] < w[1]),
        "indices must ascend: {:?}",
        &cons.indices[..cons.indices.len().min(8)]
    );

    // Do NOT let anyone "simplify" this to 0..N. MoveCheckedResourcesToFirstPositions
    // did not run on the server that produced this capture, so the demanded
    // indices are scattered through the list, not packed at the front.
    assert_ne!(
        cons.indices,
        (0..cons.indices.len() as u32).collect::<Vec<_>>(),
        "indices are not necessarily the first N -- read them from the tail"
    );
}

#[test]
fn the_list_ends_on_event_scripts() {
    let res = parse();
    let tail: Vec<&str> = res.iter().rev().take(3).map(|r| r.name.as_str()).collect();
    assert_eq!(
        tail,
        vec!["events/decal_reset.sc", "events/famas.sc", "events/galil.sc"]
    );
}

#[test]
fn sound_entries_get_the_sound_prefix_for_hashing() {
    let res = parse();
    let snd = res
        .iter()
        .find(|r| r.res_type == Some(ResourceType::Sound))
        .expect("at least one sound");
    assert!(snd.hash_path().starts_with("sound/"));
}
