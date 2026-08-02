//! Parse a real `svc_resourcelist` captured from HLDS.
//!
//! The layout was recovered from this exact message, so the test is what
//! keeps it honest: every one of the 793 names must decode, and the sizes
//! must be real file sizes.

use proto::bitbuf::BitReader;
use proto::resources::{parse_resource_list, ResourceType, FLAG_CHECKSUM};

/// Captured 2026-08-01; starts at the `svc_resourcelist` opcode byte.
const RESLIST: &[u8] = include_bytes!("fixtures/svc_resourcelist.bin");

fn parse() -> Vec<proto::resources::Resource> {
    assert_eq!(RESLIST[0], 43, "fixture must start at svc_resourcelist");
    let mut r = BitReader::new(&RESLIST[1..]);
    parse_resource_list(&mut r)
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
fn checksummed_entries_carry_their_blob() {
    let res = parse();
    let with = res.iter().filter(|r| r.checksum.is_some()).count();
    assert_eq!(with, 52, "52 entries are consistency-checked");
    for r in res.iter().filter(|r| r.checksum.is_some()) {
        assert!(
            u32::from(r.flags) & FLAG_CHECKSUM != 0,
            "{} has a checksum but not the flag",
            r.name
        );
    }
    // The player models are the checked ones.
    assert!(res
        .iter()
        .any(|r| r.name == "models/player.mdl" && r.checksum.is_some()));
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
