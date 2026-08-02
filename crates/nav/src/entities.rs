//! The BSP entity lump — what the map *is*, not just what shape it is.
//!
//! Lump 0 is a plain text block of `{ "key" "value" ... }` records, one per
//! entity, exactly as the engine hands it to `ED_LoadFromFile`. Everything the
//! navigation layer needs in order to have a goal rather than just a floor is
//! in here: where the bomb sites are, where each team spawns, where the
//! hostages start, and which brushes are ladders.
//!
//! **Why the entity lump and not the `Scenario` user message.** The server does
//! send a `Scenario` message, but it is gated on Condition Zero
//! (`multiplay_gamerules.cpp`, `UTIL_ScenarioMessage`) and is simply absent on
//! a stock 1.6 server. `CHalfLifeMultiplay::CheckMapConditions`
//! (`regamedll/dlls/multiplay_gamerules.cpp:1640-1671`) decides the game mode
//! by looking for these same classnames, so reading them here gives the same
//! answer the server itself reached, with no round trip and no guessing.

use crate::bsp::{Bsp, Vec3};

/// An axis-aligned box in world space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub mins: Vec3,
    pub maxs: Vec3,
}

impl Aabb {
    pub fn new(mins: Vec3, maxs: Vec3) -> Self {
        Self { mins, maxs }
    }

    /// The box a point entity stands in for.
    ///
    /// The legacy point forms of the zone entities have no brush, so the game
    /// tests a radius around them instead: 256 units for `info_bomb_target`
    /// (`regamedll/dlls/player.cpp:7099-7112`, `OLD_CheckBombTarget`) and
    /// `MAX_HOSTAGES_RESCUE_RADIUS`, also 256, for `info_hostage_rescue`
    /// (`regamedll/dlls/hostage/hostage.h:36`). A cube of that half-extent is
    /// the AABB that matches.
    pub fn around(p: Vec3, radius: f32) -> Self {
        Self {
            mins: [p[0] - radius, p[1] - radius, p[2] - radius],
            maxs: [p[0] + radius, p[1] + radius, p[2] + radius],
        }
    }

    pub fn centre(&self) -> Vec3 {
        [
            (self.mins[0] + self.maxs[0]) * 0.5,
            (self.mins[1] + self.maxs[1]) * 0.5,
            (self.mins[2] + self.maxs[2]) * 0.5,
        ]
    }

    pub fn contains(&self, p: Vec3) -> bool {
        (0..3).all(|i| p[i] >= self.mins[i] && p[i] <= self.maxs[i])
    }

    /// Contains `p` ignoring z. Useful because a nav node's origin is 36 above
    /// the floor while a trigger brush often starts at the floor.
    pub fn contains_xy(&self, p: Vec3) -> bool {
        (0..2).all(|i| p[i] >= self.mins[i] && p[i] <= self.maxs[i])
    }

    /// Do the two boxes overlap at all? This is the engine's own "is the
    /// player touching this trigger" test: `SV_LinkEdict` gives every entity an
    /// `absmin`/`absmax` and the touch check is a plain AABB overlap of the
    /// player's box with the brush's.
    pub fn intersects(&self, other: &Aabb) -> bool {
        (0..3).all(|i| self.mins[i] <= other.maxs[i] && self.maxs[i] >= other.mins[i])
    }

    pub fn size(&self) -> Vec3 {
        [
            self.maxs[0] - self.mins[0],
            self.maxs[1] - self.mins[1],
            self.maxs[2] - self.mins[2],
        ]
    }
}

/// The radius the game uses for the point forms of the zone entities.
pub const LEGACY_ZONE_RADIUS: f32 = 256.0;

/// What the round is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// `de_` — plant or defuse.
    Bomb,
    /// `cs_` — rescue the hostages.
    Hostage,
    /// `as_` — escort the VIP to a safety zone.
    Assassination,
    /// `es_` — get the terrorists to an escape zone.
    Escape,
    /// None of the above: no objective entities at all.
    Deathmatch,
}

/// One `{ ... }` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Entity {
    pairs: Vec<(String, String)>,
}

impl Entity {
    pub fn pairs(&self) -> &[(String, String)] {
        &self.pairs
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn classname(&self) -> &str {
        self.get("classname").unwrap_or("")
    }

    /// The `"origin" "x y z"` key, if present and well formed.
    pub fn origin(&self) -> Option<Vec3> {
        parse_vec3(self.get("origin")?)
    }

    /// The submodel index behind `"model" "*12"`, if this is a brush entity.
    /// A `"model" "models/foo.mdl"` key is not a brush model and yields `None`.
    pub fn brush_model(&self) -> Option<usize> {
        self.get("model")?.strip_prefix('*')?.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityError {
    /// A `func_*` entity that must have a brush does not.
    MissingBrushModel(String),
    /// `"model" "*n"` with an `n` past the end of the model lump.
    BrushModelOutOfRange { classname: String, index: usize },
    /// An `info_*` entity that must have a position does not, or its `origin`
    /// key does not parse as three numbers.
    BadOrigin(String),
}

impl std::fmt::Display for EntityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBrushModel(c) => write!(f, "{c} has no \"model\" \"*n\" key"),
            Self::BrushModelOutOfRange { classname, index } => {
                write!(f, "{classname} references submodel *{index}, which does not exist")
            }
            Self::BadOrigin(c) => write!(f, "{c} has no usable \"origin\" key"),
        }
    }
}

impl std::error::Error for EntityError {}

fn parse_vec3(s: &str) -> Option<Vec3> {
    let mut it = s.split_whitespace();
    let v = [
        it.next()?.parse::<f32>().ok()?,
        it.next()?.parse::<f32>().ok()?,
        it.next()?.parse::<f32>().ok()?,
    ];
    if v.iter().all(|f| f.is_finite()) {
        Some(v)
    } else {
        None
    }
}

/// Split the entity lump into records.
///
/// Deliberately total: a malformed lump yields the records it could read rather
/// than an error, because the alternative is refusing to navigate a map over a
/// stray brace in a `wad` path. What is *not* tolerated is a record that claims
/// a brush or a position it does not have — see [`MapInfo::from_bsp`].
pub fn parse(text: &str) -> Vec<Entity> {
    let b = text.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();

    // COM_Parse's whitespace-and-`//`-comment skip.
    let skip = |i: &mut usize| loop {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
        if *i + 1 < b.len() && b[*i] == b'/' && b[*i + 1] == b'/' {
            while *i < b.len() && b[*i] != b'\n' {
                *i += 1;
            }
        } else {
            return;
        }
    };

    // A `"..."` token. Returns None at end of input or on an unterminated quote.
    let quoted = |i: &mut usize| -> Option<String> {
        if *i >= b.len() || b[*i] != b'"' {
            return None;
        }
        *i += 1;
        let start = *i;
        while *i < b.len() && b[*i] != b'"' {
            *i += 1;
        }
        if *i >= b.len() {
            return None; // unterminated
        }
        let s = text[start..*i].to_string();
        *i += 1;
        Some(s)
    };

    loop {
        skip(&mut i);
        if i >= b.len() {
            break;
        }
        if b[i] != b'{' {
            i += 1;
            continue;
        }
        i += 1;
        let mut pairs = Vec::new();
        loop {
            skip(&mut i);
            if i >= b.len() || b[i] == b'}' {
                i += 1;
                break;
            }
            let Some(key) = quoted(&mut i) else {
                // Not a key; skip the byte and try again rather than losing the
                // rest of the lump.
                i += 1;
                continue;
            };
            skip(&mut i);
            let Some(value) = quoted(&mut i) else {
                break;
            };
            pairs.push((key, value));
        }
        out.push(Entity { pairs });
    }
    out
}

/// Everything the navigator needs to know about the map's objectives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MapInfo {
    pub scenario: Scenario,
    pub bomb_sites: Vec<Aabb>,
    pub rescue_zones: Vec<Aabb>,
    pub buy_zones: Vec<Aabb>,
    /// `info_player_deathmatch` — the terrorist spawns.
    pub t_spawns: Vec<Vec3>,
    /// `info_player_start` — the counter-terrorist spawns.
    pub ct_spawns: Vec<Vec3>,
    pub hostage_spawns: Vec<Vec3>,
    pub ladders: Vec<Aabb>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self::Deathmatch
    }
}

/// Classnames, kept as constants so the mirror of `CheckMapConditions` is
/// checkable by eye.
pub mod classname {
    pub const FUNC_BOMB_TARGET: &str = "func_bomb_target";
    pub const INFO_BOMB_TARGET: &str = "info_bomb_target";
    pub const FUNC_HOSTAGE_RESCUE: &str = "func_hostage_rescue";
    pub const INFO_HOSTAGE_RESCUE: &str = "info_hostage_rescue";
    pub const FUNC_BUYZONE: &str = "func_buyzone";
    pub const FUNC_LADDER: &str = "func_ladder";
    pub const FUNC_ESCAPEZONE: &str = "func_escapezone";
    pub const FUNC_VIP_SAFETYZONE: &str = "func_vip_safetyzone";
    pub const T_SPAWN: &str = "info_player_deathmatch";
    pub const CT_SPAWN: &str = "info_player_start";
    pub const HOSTAGE: &str = "hostage_entity";
    pub const SCIENTIST: &str = "monster_scientist";
}

impl MapInfo {
    /// Read the map's objectives straight out of a parsed BSP.
    pub fn from_bsp(bsp: &Bsp) -> Result<Self, EntityError> {
        Self::from_entities(&parse(&bsp.entities), bsp)
    }

    /// As [`MapInfo::from_bsp`], but on already-parsed records. `bsp` is still
    /// needed: a brush entity's box lives in the model lump, not in the text.
    pub fn from_entities(ents: &[Entity], bsp: &Bsp) -> Result<Self, EntityError> {
        // `absmin = origin + model.mins` (SV_LinkEdict for SOLID_BSP). The
        // origin key is absent on almost every trigger brush, but honouring it
        // costs nothing and a map that uses an origin brush would otherwise
        // land its bomb site in the wrong place.
        let brush = |e: &Entity| -> Result<Aabb, EntityError> {
            let idx = e
                .brush_model()
                .ok_or_else(|| EntityError::MissingBrushModel(e.classname().to_string()))?;
            let m = bsp
                .models
                .get(idx)
                .ok_or_else(|| EntityError::BrushModelOutOfRange {
                    classname: e.classname().to_string(),
                    index: idx,
                })?;
            let o = e.origin().unwrap_or([0.0; 3]);
            Ok(Aabb::new(
                [m.mins[0] + o[0], m.mins[1] + o[1], m.mins[2] + o[2]],
                [m.maxs[0] + o[0], m.maxs[1] + o[1], m.maxs[2] + o[2]],
            ))
        };
        let point = |e: &Entity| -> Result<Vec3, EntityError> {
            e.origin()
                .ok_or_else(|| EntityError::BadOrigin(e.classname().to_string()))
        };

        fn of<'a>(ents: &'a [Entity], name: &'static str) -> impl Iterator<Item = &'a Entity> {
            ents.iter().filter(move |e| e.classname() == name)
        }
        let boxes = |name: &'static str| -> Result<Vec<Aabb>, EntityError> {
            of(ents, name).map(brush).collect()
        };
        let points = |name: &'static str| -> Result<Vec<Vec3>, EntityError> {
            of(ents, name).map(point).collect()
        };

        let t_spawns = points(classname::T_SPAWN)?;
        let ct_spawns = points(classname::CT_SPAWN)?;

        let mut hostage_spawns = points(classname::HOSTAGE)?;
        hostage_spawns.extend(points(classname::SCIENTIST)?);

        let ladders = boxes(classname::FUNC_LADDER)?;
        let buy_zones = boxes(classname::FUNC_BUYZONE)?;

        // --- bomb sites: brush form first, then the legacy point form.
        // CheckMapConditions:1642-1656 tests them in exactly this order.
        let mut bomb_sites = boxes(classname::FUNC_BOMB_TARGET)?;
        if bomb_sites.is_empty() {
            bomb_sites = of(ents, classname::INFO_BOMB_TARGET)
                .map(|e| point(e).map(|p| Aabb::around(p, LEGACY_ZONE_RADIUS)))
                .collect::<Result<_, _>>()?;
        }

        // --- rescue zones: three levels, and the third is not obvious.
        //
        // CheckMapConditions:1660 sets m_bMapHasRescueZone from
        // func_hostage_rescue alone. When it is false the hostage falls back to
        // info_hostage_rescue within 256 units, and when *that* does not exist
        // either it falls back to info_player_start within 256 units --
        // hostage.cpp:417-447. cs_assault is exactly this third case: it has
        // four hostages and no rescue entity of any kind, so its rescue zones
        // really are the CT spawns.
        let mut rescue_zones = boxes(classname::FUNC_HOSTAGE_RESCUE)?;
        if rescue_zones.is_empty() {
            rescue_zones = of(ents, classname::INFO_HOSTAGE_RESCUE)
                .map(|e| point(e).map(|p| Aabb::around(p, LEGACY_ZONE_RADIUS)))
                .collect::<Result<_, _>>()?;
        }
        let rescue_is_ct_spawn = rescue_zones.is_empty() && !hostage_spawns.is_empty();
        if rescue_is_ct_spawn {
            rescue_zones = ct_spawns
                .iter()
                .map(|&p| Aabb::around(p, LEGACY_ZONE_RADIUS))
                .collect();
        }

        let has = |name: &'static str| of(ents, name).next().is_some();
        let scenario = if !bomb_sites.is_empty() {
            Scenario::Bomb
        } else if !rescue_zones.is_empty() && !rescue_is_ct_spawn {
            Scenario::Hostage
        } else if has(classname::FUNC_VIP_SAFETYZONE) {
            Scenario::Assassination
        } else if has(classname::FUNC_ESCAPEZONE) {
            Scenario::Escape
        } else if !hostage_spawns.is_empty() {
            // Hostages but no rescue entity: the CT spawns are the zone.
            Scenario::Hostage
        } else {
            Scenario::Deathmatch
        };

        Ok(Self {
            scenario,
            bomb_sites,
            rescue_zones,
            buy_zones,
            t_spawns,
            ct_spawns,
            hostage_spawns,
            ladders,
        })
    }

    /// Every position worth starting a navigation flood fill from: the spawns,
    /// and the centre of every objective volume.
    pub fn seeds(&self) -> Vec<Vec3> {
        let mut v = Vec::new();
        v.extend_from_slice(&self.t_spawns);
        v.extend_from_slice(&self.ct_spawns);
        v.extend_from_slice(&self.hostage_spawns);
        for z in self
            .bomb_sites
            .iter()
            .chain(&self.rescue_zones)
            .chain(&self.buy_zones)
        {
            v.push(z.centre());
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bsp::Model;

    fn bsp_with_models(models: Vec<Model>) -> Bsp {
        Bsp { models, ..Default::default() }
    }

    fn model(mins: Vec3, maxs: Vec3) -> Model {
        Model { mins, maxs, origin: [0.0; 3], headnode: [0; 4] }
    }

    #[test]
    fn a_simple_lump_splits_into_records() {
        let text = "{\n\"classname\" \"worldspawn\"\n\"wad\" \"a.wad;b.wad\"\n}\n\
                    {\n\"origin\" \"1 2 3\"\n\"classname\" \"info_player_start\"\n}\n";
        let e = parse(text);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].classname(), "worldspawn");
        assert_eq!(e[0].get("wad"), Some("a.wad;b.wad"));
        assert_eq!(e[1].classname(), "info_player_start");
        assert_eq!(e[1].origin(), Some([1.0, 2.0, 3.0]));
    }

    #[test]
    fn everything_on_one_line_still_parses() {
        let e = parse("{\"classname\" \"a\"}{\"classname\" \"b\"}");
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].classname(), "b");
    }

    #[test]
    fn comments_and_blank_records_are_tolerated() {
        let e = parse("// leading comment\n{ }\n{ \"classname\" \"x\" } // trailing\n");
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].classname(), "");
        assert_eq!(e[1].classname(), "x");
    }

    #[test]
    fn an_unterminated_record_does_not_hang_or_panic() {
        for bad in ["{", "{\"classname\"", "{\"a\" \"b\"", "\"orphan\"", "}{}{", "{\"a"] {
            let _ = parse(bad); // must simply return
        }
    }

    #[test]
    fn braces_and_quotes_inside_a_value_do_not_derail_the_scan() {
        let e = parse("{ \"message\" \"a { brace\" \"classname\" \"info_target\" }");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].get("message"), Some("a { brace"));
        assert_eq!(e[0].classname(), "info_target");
    }

    #[test]
    fn a_brush_key_is_only_a_brush_when_it_starts_with_a_star() {
        let e = parse("{ \"model\" \"*12\" }{ \"model\" \"models/w_c4.mdl\" }{ \"model\" \"*x\" }");
        assert_eq!(e[0].brush_model(), Some(12));
        assert_eq!(e[1].brush_model(), None);
        assert_eq!(e[2].brush_model(), None);
    }

    #[test]
    fn a_malformed_origin_is_not_silently_zero() {
        let e = parse("{ \"origin\" \"1 2\" }{ \"origin\" \"a b c\" }{ \"origin\" \"1 2 3 4\" }");
        assert_eq!(e[0].origin(), None);
        assert_eq!(e[1].origin(), None);
        // A fourth field is ignored, as the engine's UTIL_StringToVector does.
        assert_eq!(e[2].origin(), Some([1.0, 2.0, 3.0]));
    }

    #[test]
    fn a_brush_entity_takes_its_box_from_the_model_lump() {
        let bsp = bsp_with_models(vec![
            model([-1.0; 3], [1.0; 3]),
            model([10.0, 20.0, 30.0], [40.0, 50.0, 60.0]),
        ]);
        let ents = parse(
            "{ \"classname\" \"func_bomb_target\" \"model\" \"*1\" }\
             { \"classname\" \"info_player_deathmatch\" \"origin\" \"0 0 0\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Bomb);
        assert_eq!(
            info.bomb_sites,
            vec![Aabb::new([10.0, 20.0, 30.0], [40.0, 50.0, 60.0])]
        );
        assert_eq!(info.bomb_sites[0].centre(), [25.0, 35.0, 45.0]);
    }

    #[test]
    fn a_brush_entity_with_an_origin_key_is_shifted_by_it() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3]), model([0.0; 3], [10.0; 3])]);
        let ents = parse("{ \"classname\" \"func_buyzone\" \"model\" \"*1\" \"origin\" \"5 0 -5\" }");
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(
            info.buy_zones,
            vec![Aabb::new([5.0, 0.0, -5.0], [15.0, 10.0, 5.0])]
        );
    }

    #[test]
    fn a_brush_entity_pointing_at_a_model_that_is_not_there_is_an_error() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse("{ \"classname\" \"func_bomb_target\" \"model\" \"*7\" }");
        assert_eq!(
            MapInfo::from_entities(&ents, &bsp),
            Err(EntityError::BrushModelOutOfRange {
                classname: "func_bomb_target".into(),
                index: 7
            })
        );
    }

    #[test]
    fn a_brush_entity_with_no_model_at_all_is_an_error() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse("{ \"classname\" \"func_ladder\" }");
        assert!(matches!(
            MapInfo::from_entities(&ents, &bsp),
            Err(EntityError::MissingBrushModel(_))
        ));
    }

    #[test]
    fn a_spawn_without_an_origin_is_an_error_not_the_world_centre() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse("{ \"classname\" \"info_player_start\" }");
        assert_eq!(
            MapInfo::from_entities(&ents, &bsp),
            Err(EntityError::BadOrigin("info_player_start".into()))
        );
    }

    #[test]
    fn the_legacy_point_bomb_target_becomes_a_256_unit_box() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse("{ \"classname\" \"info_bomb_target\" \"origin\" \"100 200 300\" }");
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Bomb);
        assert_eq!(
            info.bomb_sites,
            vec![Aabb::new([-156.0, -56.0, 44.0], [356.0, 456.0, 556.0])]
        );
    }

    #[test]
    fn the_brush_bomb_target_wins_over_the_legacy_point_one() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3]), model([0.0; 3], [8.0; 3])]);
        let ents = parse(
            "{ \"classname\" \"info_bomb_target\" \"origin\" \"999 0 0\" }\
             { \"classname\" \"func_bomb_target\" \"model\" \"*1\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.bomb_sites, vec![Aabb::new([0.0; 3], [8.0; 3])]);
    }

    #[test]
    fn a_rescue_zone_makes_it_a_hostage_map() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3]), model([0.0; 3], [8.0; 3])]);
        let ents = parse("{ \"classname\" \"func_hostage_rescue\" \"model\" \"*1\" }");
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Hostage);
        assert_eq!(info.rescue_zones.len(), 1);
    }

    #[test]
    fn hostages_with_no_rescue_entity_fall_back_to_the_ct_spawns() {
        // This is cs_assault. hostage.cpp:438-446.
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse(
            "{ \"classname\" \"hostage_entity\" \"origin\" \"0 0 0\" }\
             { \"classname\" \"info_player_start\" \"origin\" \"1000 0 0\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.scenario, Scenario::Hostage);
        assert_eq!(
            info.rescue_zones,
            vec![Aabb::around([1000.0, 0.0, 0.0], LEGACY_ZONE_RADIUS)]
        );
    }

    #[test]
    fn ct_spawns_alone_are_not_a_rescue_zone() {
        // No hostages -> no rescue, and certainly not a hostage map.
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse("{ \"classname\" \"info_player_start\" \"origin\" \"0 0 0\" }");
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert!(info.rescue_zones.is_empty());
        assert_eq!(info.scenario, Scenario::Deathmatch);
    }

    #[test]
    fn the_vip_safety_zone_and_the_escape_zone_are_recognised() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3]), model([0.0; 3], [8.0; 3])]);
        let vip = parse("{ \"classname\" \"func_vip_safetyzone\" \"model\" \"*1\" }");
        assert_eq!(
            MapInfo::from_entities(&vip, &bsp).unwrap().scenario,
            Scenario::Assassination
        );
        let esc = parse("{ \"classname\" \"func_escapezone\" \"model\" \"*1\" }");
        assert_eq!(
            MapInfo::from_entities(&esc, &bsp).unwrap().scenario,
            Scenario::Escape
        );
    }

    #[test]
    fn an_empty_map_is_deathmatch_with_nothing_in_it() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let info = MapInfo::from_entities(&[], &bsp).expect("should derive");
        assert_eq!(info, MapInfo::default());
        assert!(info.seeds().is_empty());
    }

    #[test]
    fn the_two_spawn_classnames_are_not_swapped() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse(
            "{ \"classname\" \"info_player_deathmatch\" \"origin\" \"1 0 0\" }\
             { \"classname\" \"info_player_start\" \"origin\" \"2 0 0\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.t_spawns, vec![[1.0, 0.0, 0.0]]);
        assert_eq!(info.ct_spawns, vec![[2.0, 0.0, 0.0]]);
    }

    #[test]
    fn scientists_count_as_hostages() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3])]);
        let ents = parse(
            "{ \"classname\" \"hostage_entity\" \"origin\" \"1 0 0\" }\
             { \"classname\" \"monster_scientist\" \"origin\" \"2 0 0\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        assert_eq!(info.hostage_spawns, vec![[1.0, 0.0, 0.0], [2.0, 0.0, 0.0]]);
    }

    #[test]
    fn aabb_geometry_is_what_it_says() {
        let b = Aabb::new([-10.0, -20.0, -30.0], [10.0, 20.0, 30.0]);
        assert_eq!(b.centre(), [0.0; 3]);
        assert_eq!(b.size(), [20.0, 40.0, 60.0]);
        assert!(b.contains([0.0; 3]));
        assert!(b.contains([10.0, 20.0, 30.0])); // inclusive
        assert!(!b.contains([11.0, 0.0, 0.0]));
        assert!(b.contains_xy([0.0, 0.0, 9999.0]));
        assert!(!b.contains_xy([9999.0, 0.0, 0.0]));
    }

    #[test]
    fn seeds_cover_spawns_and_objective_centres() {
        let bsp = bsp_with_models(vec![model([0.0; 3], [1.0; 3]), model([0.0; 3], [8.0; 3])]);
        let ents = parse(
            "{ \"classname\" \"info_player_deathmatch\" \"origin\" \"1 0 0\" }\
             { \"classname\" \"func_bomb_target\" \"model\" \"*1\" }",
        );
        let info = MapInfo::from_entities(&ents, &bsp).expect("should derive");
        let seeds = info.seeds();
        assert!(seeds.contains(&[1.0, 0.0, 0.0]));
        assert!(seeds.contains(&[4.0, 4.0, 4.0]));
    }
}
