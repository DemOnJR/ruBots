//! GoldSrc BSP v30 — line of sight and ground height.
//!
//! Port of `internal/nav/bsp.go` (`LoadBSP`, `Visible`, `TraceFraction`,
//! `GroundHeight`, `trace`). This is what gates every shot: a bot must not
//! engage an enemy it cannot actually see.
//!
//! Verified from `LoadBSP` (`0x1406F62A0`): it rejects the file with
//! "unsupported BSP version" and strides one structure array by `0x1C` (28),
//! which is `dleaf_t`. Cross-checked against a real `de_dust2.bsp` shipped
//! with HLDS: version 30, and every lump length divides exactly by the
//! structure sizes below (9582 planes, 2766 nodes, 1455 leaves, 8321
//! clipnodes, 43 models).
//!
//! The trace is the classic Quake/GoldSrc recursive hull check, restricted to
//! point traces, which is all line-of-sight needs.

pub type Vec3 = [f32; 3];

/// The only version GoldSrc maps use.
pub const BSP_VERSION: i32 = 30;

/// Lump indices.
pub mod lump {
    /// Plain text: `{ "key" "value" ... }` blocks. See `crate::entities`.
    pub const ENTITIES: usize = 0;
    pub const PLANES: usize = 1;
    pub const NODES: usize = 5;
    pub const CLIPNODES: usize = 9;
    pub const LEAVES: usize = 10;
    pub const MODELS: usize = 14;
    pub const COUNT: usize = 15;
}

pub const PLANE_LEN: usize = 20;
pub const NODE_LEN: usize = 24;
pub const LEAF_LEN: usize = 28;
pub const CLIPNODE_LEN: usize = 8;
pub const MODEL_LEN: usize = 64;

/// Leaf contents. Only `SOLID` and `SKY` block sight.
pub mod contents {
    pub const EMPTY: i32 = -1;
    pub const SOLID: i32 = -2;
    pub const WATER: i32 = -3;
    pub const SLIME: i32 = -4;
    pub const LAVA: i32 = -5;
    pub const SKY: i32 = -6;
}

const DIST_EPSILON: f32 = 0.03125;

/// The flattest a surface may be and still count as ground: `normal[2] >= 0.7`
/// (`regamedll/pm_shared/pm_shared.cpp:1220`).
pub const WALKABLE_NORMAL_Z: f32 = 0.7;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Plane {
    pub normal: Vec3,
    pub dist: f32,
    pub kind: i32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node {
    pub plane: u32,
    /// Positive: node index. Negative: `-(leaf + 1)`.
    pub children: [i16; 2],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leaf {
    pub contents: i32,
}

/// One node of a *collision* hull.
///
/// `dclipnode_t`: `{ int32 planenum; int16 children[2] }`. The difference from
/// [`Node`] is what a negative child means. A visual node's negative child is
/// `-(leaf + 1)` and you must look the contents up in the leaf array; a
/// clipnode's negative child **is** the `CONTENTS_*` value (`-1` empty, `-2`
/// solid). See `PM_HullPointContents` (`rehlds/engine/pmovetst.cpp:104`), which
/// simply returns `num` once it goes negative.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClipNode {
    pub plane: u32,
    /// Positive: clipnode index. Negative: a `CONTENTS_*` value.
    pub children: [i16; 2],
}

/// Which collision hull to trace through.
///
/// The map compiler pre-expands the world into one clipnode tree per hull, so
/// a **point** trace through hull 1 already answers "does a standing player fit
/// here" — there is no box to sweep. The player origin is the point.
///
/// The mapping from the engine's `usehull` to the BSP hull index is
/// `PM_HullForBsp` (`rehlds/engine/pmovetst.cpp:232-252`):
/// `usehull 0` (standing) -> hull 1, `usehull 1` (ducking) -> hull 3,
/// `usehull 2` (point) -> hull 0, `usehull 3` (large) -> hull 2. The sizes are
/// `player_mins`/`player_maxs` (`rehlds/engine/pmove.cpp:36-47`), and they are
/// equal to the hulls' `clip_mins`/`clip_maxs`
/// (`rehlds/engine/model.cpp:1107-1133`) — which is why `PM_HullForBsp`'s
/// `offset` reduces to the entity origin and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hull {
    /// Hull 0 — a dimensionless point, the visual BSP tree.
    Point,
    /// Hull 1 — standing player, 32x32x72.
    Stand,
    /// Hull 2 — the large hull, 64x64x64.
    Large,
    /// Hull 3 — ducking player, 32x32x36.
    Duck,
}

impl Hull {
    /// Index into [`Model::headnode`].
    pub const fn index(self) -> usize {
        match self {
            Self::Point => 0,
            Self::Stand => 1,
            Self::Large => 2,
            Self::Duck => 3,
        }
    }

    pub const fn mins(self) -> Vec3 {
        match self {
            Self::Point => [0.0, 0.0, 0.0],
            Self::Stand => [-16.0, -16.0, -36.0],
            Self::Large => [-32.0, -32.0, -32.0],
            Self::Duck => [-16.0, -16.0, -18.0],
        }
    }

    pub const fn maxs(self) -> Vec3 {
        match self {
            Self::Point => [0.0, 0.0, 0.0],
            Self::Stand => [16.0, 16.0, 36.0],
            Self::Large => [32.0, 32.0, 32.0],
            Self::Duck => [16.0, 16.0, 18.0],
        }
    }

    /// How far the origin sits above the feet: `-mins[2]`.
    ///
    /// This is the number that makes ducking work. A standing origin is 36
    /// above the floor, a ducking one only 18 — so to duck *in place* you drop
    /// the origin by 18 and the feet stay put.
    pub const fn eye_to_feet(self) -> f32 {
        -self.mins()[2]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    pub mins: Vec3,
    pub maxs: Vec3,
    pub origin: Vec3,
    pub headnode: [i32; 4],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BspError {
    TooShort,
    BadVersion(i32),
    LumpOutOfRange(usize),
    NoModels,
}

impl std::fmt::Display for BspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "bsp file truncated"),
            // The original logs "unsupported BSP version".
            Self::BadVersion(v) => write!(f, "unsupported BSP version {v}"),
            Self::LumpOutOfRange(i) => write!(f, "bsp lump {i} out of range"),
            Self::NoModels => write!(f, "bsp has no models"),
        }
    }
}

impl std::error::Error for BspError {}

fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_i16(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}
fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn rd_vec(b: &[u8], o: usize) -> Vec3 {
    [rd_f32(b, o), rd_f32(b, o + 4), rd_f32(b, o + 8)]
}

fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// A loaded map, reduced to what tracing needs.
#[derive(Debug, Clone, Default)]
pub struct Bsp {
    pub planes: Vec<Plane>,
    pub nodes: Vec<Node>,
    pub leaves: Vec<Leaf>,
    /// The collision hulls, one tree per hull rooted at `models[n].headnode[h]`.
    pub clipnodes: Vec<ClipNode>,
    pub models: Vec<Model>,
    /// Lump 0 verbatim, decoded byte-for-byte as Latin-1 so no map can fail to
    /// load over a stray high byte in a wad path.
    pub entities: String,
}

/// Result of a point trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trace {
    /// 0..1 along the segment where it stopped. 1.0 means it reached the end.
    pub fraction: f32,
    /// The trace began inside solid space.
    pub start_solid: bool,
    pub end: Vec3,
    /// Normal of the surface that stopped it, facing back along the segment.
    /// All zeroes when nothing was hit.
    ///
    /// This is what tells a floor from a wall. `PM_CatagorizePosition` treats a
    /// surface with `normal[2] < 0.7` as not ground at all
    /// (`regamedll/pm_shared/pm_shared.cpp:1220`, `:1661`), which is a slope of
    /// about 45 degrees — steeper than that and the player slides off.
    pub plane: Vec3,
}

impl Trace {
    /// A trace that reached `end` without touching anything.
    pub fn miss(end: Vec3) -> Self {
        Self { fraction: 1.0, start_solid: false, end, plane: [0.0; 3] }
    }

    /// The whole segment is traversable: it neither started in solid nor hit
    /// anything. Both halves matter — see [`Bsp::visible`].
    pub fn is_clear(&self) -> bool {
        !self.start_solid && self.fraction >= 1.0
    }

    /// Is the surface it stopped on flat enough to stand on?
    ///
    /// `PM_CatagorizePosition` (`pm_shared.cpp:1220`) drops the player off the
    /// ground when `normal[2] < 0.7`; a nav node on such a face would be a spot
    /// the bot immediately slides out of.
    pub fn is_walkable_floor(&self) -> bool {
        self.fraction < 1.0 && self.plane[2] >= WALKABLE_NORMAL_Z
    }
}

/// The two shapes of tree the walker descends.
///
/// Hull 0 is the visual node tree, where a negative child is `-(leaf + 1)` and
/// the contents live in the leaf array. Hulls 1..3 are clipnode trees, where a
/// negative child *is* the contents. Everything else about the descent — the
/// plane test, the split, `DIST_EPSILON` — is identical, which is why there is
/// one walker and not two.
trait Tree {
    /// `(plane index, children)` for an interior node, or `None` if the index
    /// is out of range.
    fn interior(&self, n: usize) -> Option<(usize, [i16; 2])>;
    /// Contents for a negative child value.
    fn terminal(&self, num: i32) -> i32;
    /// Number of interior nodes, used to spot a hull the map has no tree for.
    fn len(&self) -> usize;
}

struct VisualTree<'a>(&'a Bsp);
struct ClipTree<'a>(&'a Bsp);

impl Tree for VisualTree<'_> {
    fn interior(&self, n: usize) -> Option<(usize, [i16; 2])> {
        self.0.nodes.get(n).map(|nd| (nd.plane as usize, nd.children))
    }
    fn terminal(&self, num: i32) -> i32 {
        let leaf = (-num - 1) as usize;
        self.0.leaves.get(leaf).map_or(contents::SOLID, |l| l.contents)
    }
    fn len(&self) -> usize {
        self.0.nodes.len()
    }
}

impl Tree for ClipTree<'_> {
    fn interior(&self, n: usize) -> Option<(usize, [i16; 2])> {
        self.0.clipnodes.get(n).map(|cn| (cn.plane as usize, cn.children))
    }
    fn terminal(&self, num: i32) -> i32 {
        num
    }
    fn len(&self) -> usize {
        self.0.clipnodes.len()
    }
}

impl Bsp {
    pub fn parse(data: &[u8]) -> Result<Self, BspError> {
        // 4-byte version + 15 lumps of (offset, length).
        if data.len() < 4 + lump::COUNT * 8 {
            return Err(BspError::TooShort);
        }
        let version = rd_i32(data, 0);
        if version != BSP_VERSION {
            return Err(BspError::BadVersion(version));
        }

        let lump_slice = |i: usize| -> Result<&[u8], BspError> {
            let off = rd_i32(data, 4 + i * 8) as usize;
            let len = rd_i32(data, 8 + i * 8) as usize;
            data.get(off..off + len).ok_or(BspError::LumpOutOfRange(i))
        };

        let pl = lump_slice(lump::PLANES)?;
        let planes = (0..pl.len() / PLANE_LEN)
            .map(|i| {
                let o = i * PLANE_LEN;
                Plane { normal: rd_vec(pl, o), dist: rd_f32(pl, o + 12), kind: rd_i32(pl, o + 16) }
            })
            .collect();

        let nd = lump_slice(lump::NODES)?;
        let nodes = (0..nd.len() / NODE_LEN)
            .map(|i| {
                let o = i * NODE_LEN;
                Node {
                    plane: rd_i32(nd, o) as u32,
                    children: [rd_i16(nd, o + 4), rd_i16(nd, o + 6)],
                }
            })
            .collect();

        let lf = lump_slice(lump::LEAVES)?;
        let leaves = (0..lf.len() / LEAF_LEN)
            .map(|i| Leaf { contents: rd_i32(lf, i * LEAF_LEN) })
            .collect();

        let cn = lump_slice(lump::CLIPNODES)?;
        let clipnodes = (0..cn.len() / CLIPNODE_LEN)
            .map(|i| {
                let o = i * CLIPNODE_LEN;
                ClipNode {
                    plane: rd_i32(cn, o) as u32,
                    children: [rd_i16(cn, o + 4), rd_i16(cn, o + 6)],
                }
            })
            .collect();

        let en = lump_slice(lump::ENTITIES)?;
        // The lump is a C string padded to a 4-byte boundary; stop at the NUL.
        let text = en.split(|&b| b == 0).next().unwrap_or(&[]);
        let entities = text.iter().map(|&b| b as char).collect();

        let md = lump_slice(lump::MODELS)?;
        let models: Vec<Model> = (0..md.len() / MODEL_LEN)
            .map(|i| {
                let o = i * MODEL_LEN;
                Model {
                    mins: rd_vec(md, o),
                    maxs: rd_vec(md, o + 12),
                    origin: rd_vec(md, o + 24),
                    headnode: [
                        rd_i32(md, o + 36),
                        rd_i32(md, o + 40),
                        rd_i32(md, o + 44),
                        rd_i32(md, o + 48),
                    ],
                }
            })
            .collect();

        if models.is_empty() {
            return Err(BspError::NoModels);
        }

        Ok(Self { planes, nodes, leaves, clipnodes, models, entities })
    }

    /// Contents of the leaf containing `p`, walking the world tree.
    pub fn point_contents(&self, p: Vec3) -> i32 {
        self.contents_in(&VisualTree(self), self.models[0].headnode[0], p)
    }

    /// Trace a point from `start` to `end`.
    pub fn trace(&self, start: Vec3, end: Vec3) -> Trace {
        self.trace_in(&VisualTree(self), self.models[0].headnode[0], start, end)
    }

    /// Trace a *player hull* through the world.
    ///
    /// The hull is already baked into the map, so this is still a point trace —
    /// see [`Hull`]. `start`/`end` are player **origins**, not feet: a standing
    /// origin sits 36 above the floor it rests on.
    pub fn hull_trace(&self, hull: Hull, start: Vec3, end: Vec3) -> Trace {
        self.hull_trace_model(0, hull, start, end)
    }

    /// [`Bsp::hull_trace`] against one brush entity's submodel — doors, lifts,
    /// breakables, and the trigger volumes (`func_buyzone`, `func_ladder`, ...)
    /// that carry a `"model" "*n"` key.
    ///
    /// The submodel is traced in its own coordinate space, so the segment is
    /// shifted by `-models[n].origin` on the way in and the hit point shifted
    /// back on the way out. That is `PM_HullForBsp`'s `offset`
    /// (`rehlds/engine/pmovetst.cpp:246-251`) with the `clip_mins -
    /// player_mins` term cancelled, which it always does for BSP hulls.
    pub fn hull_trace_model(&self, model: usize, hull: Hull, start: Vec3, end: Vec3) -> Trace {
        let Some(m) = self.models.get(model) else {
            return Trace::miss(end);
        };
        let off = m.origin;
        let s = [start[0] - off[0], start[1] - off[1], start[2] - off[2]];
        let e = [end[0] - off[0], end[1] - off[1], end[2] - off[2]];
        let head = m.headnode[hull.index()];

        let mut t = if hull == Hull::Point {
            if self.degenerate(&VisualTree(self), head) {
                return Trace::miss(end);
            }
            self.trace_in(&VisualTree(self), head, s, e)
        } else {
            if self.degenerate(&ClipTree(self), head) {
                return Trace::miss(end);
            }
            self.trace_in(&ClipTree(self), head, s, e)
        };
        t.end = [t.end[0] + off[0], t.end[1] + off[1], t.end[2] + off[2]];
        t
    }

    /// Contents at `p` for a player hull. `CONTENTS_SOLID` means a player of
    /// that size cannot stand with its origin there.
    pub fn hull_point_contents(&self, hull: Hull, p: Vec3) -> i32 {
        self.hull_point_contents_model(0, hull, p)
    }

    /// [`Bsp::hull_point_contents`] against one brush entity's submodel.
    pub fn hull_point_contents_model(&self, model: usize, hull: Hull, p: Vec3) -> i32 {
        let Some(m) = self.models.get(model) else {
            return contents::EMPTY;
        };
        let off = m.origin;
        let q = [p[0] - off[0], p[1] - off[1], p[2] - off[2]];
        let head = m.headnode[hull.index()];
        if hull == Hull::Point {
            if self.degenerate(&VisualTree(self), head) {
                return contents::EMPTY;
            }
            self.contents_in(&VisualTree(self), head, q)
        } else {
            if self.degenerate(&ClipTree(self), head) {
                return contents::EMPTY;
            }
            self.contents_in(&ClipTree(self), head, q)
        }
    }

    /// A hull the map carries no tree for. The engine's equivalent is
    /// `hull->firstclipnode >= hull->lastclipnode`, which it treats as
    /// `CONTENTS_EMPTY` (`PM_HullPointContents`, `pmovetst.cpp:104-107`) — a
    /// point entity such as `info_hostage_rescue` has no collision at all, and
    /// reporting it solid would wall the map off.
    fn degenerate<T: Tree>(&self, tree: &T, head: i32) -> bool {
        head >= 0 && head as usize >= tree.len()
    }

    fn trace_in<T: Tree>(&self, tree: &T, head: i32, start: Vec3, end: Vec3) -> Trace {
        let mut t = Trace::miss(end);
        self.recurse(tree, head, 0.0, 1.0, start, end, &mut t);
        if t.fraction < 1.0 {
            t.end = [
                start[0] + (end[0] - start[0]) * t.fraction,
                start[1] + (end[1] - start[1]) * t.fraction,
                start[2] + (end[2] - start[2]) * t.fraction,
            ];
        }
        t
    }

    /// How far along the segment the trace got, 0..1.
    pub fn trace_fraction(&self, start: Vec3, end: Vec3) -> f32 {
        self.trace(start, end).fraction
    }

    /// Is there an unobstructed line between the two points?
    ///
    /// Note the `start_solid` check. Following Quake, a trace that *begins*
    /// inside solid space leaves `fraction` at 1.0 and reports `start_solid`
    /// instead — so testing the fraction alone would call a bot buried in a
    /// wall "able to see everything".
    pub fn visible(&self, a: Vec3, b: Vec3) -> bool {
        let t = self.trace(a, b);
        !t.start_solid && t.fraction >= 1.0
    }

    /// Drop a ray downward to find the floor under `p`, searching at most
    /// `max_drop` units. Returns the z of the first solid surface.
    pub fn ground_height(&self, p: Vec3, max_drop: f32) -> Option<f32> {
        let down = [p[0], p[1], p[2] - max_drop];
        let t = self.trace(p, down);
        if t.fraction >= 1.0 {
            None
        } else {
            Some(t.end[2])
        }
    }

    fn recurse<T: Tree>(
        &self,
        tree: &T,
        num: i32,
        p1f: f32,
        p2f: f32,
        p1: Vec3,
        p2: Vec3,
        trace: &mut Trace,
    ) -> bool {
        // Leaf (visual tree) or contents value (clipnode tree).
        if num < 0 {
            let c = tree.terminal(num);
            if c == contents::SOLID {
                if p1f == 0.0 {
                    trace.start_solid = true;
                }
                return false;
            }
            return true;
        }

        let Some((plane_index, children)) = tree.interior(num as usize) else {
            return true;
        };
        let Some(plane) = self.planes.get(plane_index) else {
            return true;
        };

        let t1 = dot(plane.normal, p1) - plane.dist;
        let t2 = dot(plane.normal, p2) - plane.dist;

        if t1 >= 0.0 && t2 >= 0.0 {
            return self.recurse(tree, i32::from(children[0]), p1f, p2f, p1, p2, trace);
        }
        if t1 < 0.0 && t2 < 0.0 {
            return self.recurse(tree, i32::from(children[1]), p1f, p2f, p1, p2, trace);
        }

        // The segment crosses the plane; split it.
        let denom = t1 - t2;
        let frac = if denom.abs() < f32::EPSILON {
            0.0
        } else if t1 < 0.0 {
            ((t1 + DIST_EPSILON) / denom).clamp(0.0, 1.0)
        } else {
            ((t1 - DIST_EPSILON) / denom).clamp(0.0, 1.0)
        };

        let midf = p1f + (p2f - p1f) * frac;
        let mid = [
            p1[0] + (p2[0] - p1[0]) * frac,
            p1[1] + (p2[1] - p1[1]) * frac,
            p1[2] + (p2[2] - p1[2]) * frac,
        ];

        let side = usize::from(t1 < 0.0);

        if !self.recurse(tree, i32::from(children[side]), p1f, midf, p1, mid, trace) {
            return false;
        }

        // Near half is open; if the far half is not solid at the split point,
        // keep going.
        if self.contents_in(tree, i32::from(children[side ^ 1]), mid) != contents::SOLID {
            return self.recurse(tree, i32::from(children[side ^ 1]), midf, p2f, mid, p2, trace);
        }

        // We hit a solid surface here. The normal faces back along the
        // segment, as PM_RecursiveHullCheck flips it for the far side.
        trace.plane = if side == 1 {
            [-plane.normal[0], -plane.normal[1], -plane.normal[2]]
        } else {
            plane.normal
        };
        trace.fraction = midf;
        false
    }

    /// Contents reached from `num` for point `p`.
    fn contents_in<T: Tree>(&self, tree: &T, num: i32, p: Vec3) -> i32 {
        let mut n = num;
        let mut guard = 0;
        while n >= 0 {
            guard += 1;
            if guard > 4096 {
                return contents::SOLID;
            }
            let Some((plane_index, children)) = tree.interior(n as usize) else {
                return contents::SOLID;
            };
            let Some(plane) = self.planes.get(plane_index) else {
                return contents::SOLID;
            };
            let d = dot(plane.normal, p) - plane.dist;
            n = i32::from(children[usize::from(d < 0.0)]);
        }
        tree.terminal(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structure_sizes_are_the_goldsrc_ones() {
        // LoadGraph strides leaves by 0x1C; the rest follow from BSP v30.
        assert_eq!(LEAF_LEN, 0x1C);
        assert_eq!(PLANE_LEN, 20);
        assert_eq!(NODE_LEN, 24);
        assert_eq!(CLIPNODE_LEN, 8);
        assert_eq!(MODEL_LEN, 64);
    }

    #[test]
    fn a_wrong_version_is_rejected() {
        let mut data = vec![0u8; 4 + lump::COUNT * 8];
        data[0..4].copy_from_slice(&29i32.to_le_bytes());
        assert!(matches!(Bsp::parse(&data), Err(BspError::BadVersion(29))));
    }

    #[test]
    fn a_truncated_file_is_rejected() {
        assert!(matches!(Bsp::parse(&[0u8; 8]), Err(BspError::TooShort)));
    }

    #[test]
    fn lumps_pointing_past_the_end_are_rejected() {
        let mut data = vec![0u8; 4 + lump::COUNT * 8];
        data[0..4].copy_from_slice(&BSP_VERSION.to_le_bytes());
        // Planes lump claims a huge length.
        let base = 4 + lump::PLANES * 8;
        data[base..base + 4].copy_from_slice(&100i32.to_le_bytes());
        data[base + 4..base + 8].copy_from_slice(&999_999i32.to_le_bytes());
        assert!(matches!(Bsp::parse(&data), Err(BspError::LumpOutOfRange(_))));
    }

    /// A tiny hand-built map: one plane at x=0, solid on the negative side.
    fn wall_map() -> Bsp {
        Bsp {
            planes: vec![Plane { normal: [1.0, 0.0, 0.0], dist: 0.0, kind: 0 }],
            nodes: vec![Node { plane: 0, children: [-1, -2] }],
            // child -1 -> leaf 0 (empty), child -2 -> leaf 1 (solid)
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            models: vec![Model {
                mins: [-1000.0; 3],
                maxs: [1000.0; 3],
                origin: [0.0; 3],
                headnode: [0, 0, 0, 0],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn point_contents_reads_both_sides_of_a_plane() {
        let m = wall_map();
        assert_eq!(m.point_contents([10.0, 0.0, 0.0]), contents::EMPTY);
        assert_eq!(m.point_contents([-10.0, 0.0, 0.0]), contents::SOLID);
    }

    #[test]
    fn a_trace_entirely_in_open_space_is_unobstructed() {
        let m = wall_map();
        assert_eq!(m.trace_fraction([10.0, 0.0, 0.0], [50.0, 0.0, 0.0]), 1.0);
        assert!(m.visible([10.0, 0.0, 0.0], [50.0, 0.0, 0.0]));
    }

    #[test]
    fn a_trace_into_solid_stops_partway() {
        let m = wall_map();
        let t = m.trace([10.0, 0.0, 0.0], [-10.0, 0.0, 0.0]);
        assert!(t.fraction < 1.0, "should have been blocked");
        assert!(t.fraction > 0.0, "should not stop at the very start");
        assert!(!m.visible([10.0, 0.0, 0.0], [-10.0, 0.0, 0.0]));
        // It should stop near the plane at x = 0.
        assert!(t.end[0].abs() < 1.0, "stopped at x={}", t.end[0]);
    }

    #[test]
    fn a_zero_length_trace_is_visible() {
        let m = wall_map();
        let p = [5.0, 5.0, 5.0];
        assert_eq!(m.trace_fraction(p, p), 1.0);
        assert!(m.visible(p, p));
    }

    #[test]
    fn visibility_is_symmetric_in_open_space() {
        let m = wall_map();
        let a = [10.0, 0.0, 0.0];
        let b = [200.0, 50.0, 20.0];
        assert_eq!(m.visible(a, b), m.visible(b, a));
    }

    #[test]
    fn ground_height_finds_the_floor() {
        // Floor plane at z = 0, solid below.
        let m = Bsp {
            planes: vec![Plane { normal: [0.0, 0.0, 1.0], dist: 0.0, kind: 2 }],
            nodes: vec![Node { plane: 0, children: [-1, -2] }],
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            models: vec![Model {
                mins: [-1000.0; 3],
                maxs: [1000.0; 3],
                origin: [0.0; 3],
                headnode: [0, 0, 0, 0],
            }],
            ..Default::default()
        };
        let g = m.ground_height([0.0, 0.0, 100.0], 500.0).expect("floor found");
        assert!(g.abs() < 1.0, "floor should be near z=0, got {g}");
    }

    #[test]
    fn ground_height_returns_none_over_a_pit() {
        let m = wall_map(); // vertical plane only; nothing below
        assert_eq!(m.ground_height([10.0, 0.0, 0.0], 50.0), None);
    }

    // ---------------------------------------------------------------- hulls

    #[test]
    fn hull_indices_are_the_pm_hullforbsp_mapping() {
        // PM_HullForBsp (rehlds/engine/pmovetst.cpp:232-252).
        assert_eq!(Hull::Point.index(), 0);
        assert_eq!(Hull::Stand.index(), 1);
        assert_eq!(Hull::Large.index(), 2);
        assert_eq!(Hull::Duck.index(), 3);
        // player_mins/player_maxs (rehlds/engine/pmove.cpp:36-47).
        assert_eq!(Hull::Stand.mins(), [-16.0, -16.0, -36.0]);
        assert_eq!(Hull::Stand.maxs(), [16.0, 16.0, 36.0]);
        assert_eq!(Hull::Duck.mins(), [-16.0, -16.0, -18.0]);
        assert_eq!(Hull::Duck.maxs(), [16.0, 16.0, 18.0]);
        assert_eq!(Hull::Large.mins(), [-32.0, -32.0, -32.0]);
        assert_eq!(Hull::Large.maxs(), [32.0, 32.0, 32.0]);
        assert_eq!(Hull::Point.mins(), [0.0; 3]);
        // The 18 that makes ducking work.
        assert_eq!(Hull::Stand.eye_to_feet() - Hull::Duck.eye_to_feet(), 18.0);
    }

    /// Build a minimal but *valid* file: version, 15 lump directory entries,
    /// then whatever lumps the caller wants, plus the mandatory model lump.
    fn file_with(lumps: &[(usize, Vec<u8>)]) -> Vec<u8> {
        let mut data = vec![0u8; 4 + lump::COUNT * 8];
        data[0..4].copy_from_slice(&BSP_VERSION.to_le_bytes());
        let put = |data: &mut Vec<u8>, index: usize, bytes: &[u8]| {
            let off = data.len();
            data.extend_from_slice(bytes);
            let base = 4 + index * 8;
            data[base..base + 4].copy_from_slice(&(off as i32).to_le_bytes());
            data[base + 4..base + 8].copy_from_slice(&(bytes.len() as i32).to_le_bytes());
        };
        for (index, bytes) in lumps {
            put(&mut data, *index, bytes);
        }
        put(&mut data, lump::MODELS, &[0u8; MODEL_LEN]);
        data
    }

    #[test]
    fn clipnodes_decode_from_the_lump() {
        let mut cn = Vec::new();
        cn.extend_from_slice(&7i32.to_le_bytes());
        cn.extend_from_slice(&3i16.to_le_bytes());
        cn.extend_from_slice(&(-2i16).to_le_bytes());
        assert_eq!(cn.len(), CLIPNODE_LEN);

        let m = Bsp::parse(&file_with(&[(lump::CLIPNODES, cn)])).expect("should parse");
        assert_eq!(m.clipnodes.len(), 1);
        assert_eq!(m.clipnodes[0], ClipNode { plane: 7, children: [3, -2] });
    }

    #[test]
    fn the_entity_lump_stops_at_the_nul() {
        let text = b"{ \"classname\" \"worldspawn\" }\0\0\0".to_vec();
        let m = Bsp::parse(&file_with(&[(lump::ENTITIES, text)])).expect("should parse");
        assert_eq!(m.entities, "{ \"classname\" \"worldspawn\" }");
    }

    /// A corridor along x with a low lintel spanning `0 <= x < 32`.
    ///
    /// Floor top at z = 0, lintel underside at z = 48. Hand-built the way the
    /// compiler would: each hull gets its own tree with the surfaces already
    /// pushed out by that hull's half-extents.
    ///
    /// * hull 1 (stand, +-36): floor solid below z = 36, lintel solid from
    ///   z = 12 up -> the strip is solid for every legal standing origin.
    /// * hull 3 (duck, +-18): floor solid below z = 18, lintel solid from
    ///   z = 30 up -> the band 18 <= z < 30 is open.
    fn doorway_map() -> Bsp {
        Bsp {
            planes: vec![
                Plane { normal: [0.0, 0.0, 1.0], dist: 36.0, kind: 2 }, // 0
                Plane { normal: [1.0, 0.0, 0.0], dist: 0.0, kind: 0 },  // 1
                Plane { normal: [1.0, 0.0, 0.0], dist: 32.0, kind: 0 }, // 2
                Plane { normal: [0.0, 0.0, 1.0], dist: 18.0, kind: 2 }, // 3
                Plane { normal: [0.0, 0.0, 1.0], dist: 30.0, kind: 2 }, // 4
                Plane { normal: [0.0, 0.0, 1.0], dist: 0.0, kind: 2 },  // 5
            ],
            // Visual tree (hull 0): the real floor at z = 0.
            nodes: vec![Node { plane: 5, children: [-1, -2] }],
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            clipnodes: vec![
                // hull 1, root 0
                ClipNode { plane: 0, children: [1, -2] },  // z >= 36 ? .. : floor
                ClipNode { plane: 1, children: [2, -1] },  // x >= 0  ? .. : open
                ClipNode { plane: 2, children: [-1, -2] }, // x >= 32 ? open : lintel
                // hull 3, root 3
                ClipNode { plane: 3, children: [4, -2] },  // z >= 18 ? .. : floor
                ClipNode { plane: 1, children: [5, -1] },  // x >= 0  ? .. : open
                ClipNode { plane: 2, children: [-1, 6] },  // x >= 32 ? open : ..
                ClipNode { plane: 4, children: [-2, -1] }, // z >= 30 ? lintel : open
            ],
            models: vec![Model {
                mins: [-1000.0; 3],
                maxs: [1000.0; 3],
                origin: [0.0; 3],
                // hull 2 is unused here; point it at the standing tree.
                headnode: [0, 0, 0, 3],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_narrow_doorway_admits_a_duck_but_not_a_stand() {
        let m = doorway_map();
        // Standing origin: 36 above the floor. Ducking: 18. Same feet.
        let s = m.hull_trace(Hull::Stand, [-64.0, 0.0, 36.0], [96.0, 0.0, 36.0]);
        assert!(!s.start_solid, "the standing start is in the open corridor");
        assert!(s.fraction < 1.0, "a standing player must not fit under the lintel");
        assert!(!s.is_clear());
        // It should stop just before the doorway at x = 0.
        assert!(s.end[0] < 1.0 && s.end[0] > -1.0, "stopped at x={}", s.end[0]);

        let d = m.hull_trace(Hull::Duck, [-64.0, 0.0, 18.0], [96.0, 0.0, 18.0]);
        assert!(d.is_clear(), "a ducking player fits: {d:?}");

        // Ducking at the *standing* origin height is still blocked -- this is
        // exactly the "lower by 18 to keep the feet fixed" rule.
        let wrong = m.hull_trace(Hull::Duck, [-64.0, 0.0, 36.0], [96.0, 0.0, 36.0]);
        assert!(wrong.fraction < 1.0, "z=36 is above the ducking headroom");
    }

    #[test]
    fn hull_point_contents_reads_the_right_tree() {
        let m = doorway_map();
        // Inside the doorway strip.
        assert_eq!(m.hull_point_contents(Hull::Stand, [16.0, 0.0, 36.0]), contents::SOLID);
        assert_eq!(m.hull_point_contents(Hull::Duck, [16.0, 0.0, 18.0]), contents::EMPTY);
        // Outside it, both fit.
        assert_eq!(m.hull_point_contents(Hull::Stand, [-64.0, 0.0, 36.0]), contents::EMPTY);
        assert_eq!(m.hull_point_contents(Hull::Duck, [-64.0, 0.0, 18.0]), contents::EMPTY);
        // Below the expanded floor, nothing fits.
        assert_eq!(m.hull_point_contents(Hull::Stand, [-64.0, 0.0, 0.0]), contents::SOLID);
        assert_eq!(m.hull_point_contents(Hull::Duck, [-64.0, 0.0, 0.0]), contents::SOLID);
    }

    #[test]
    fn hull_point_still_walks_the_visual_tree() {
        let m = doorway_map();
        // Hull 0 knows only the real floor at z = 0, not the expanded one.
        assert_eq!(m.hull_point_contents(Hull::Point, [-64.0, 0.0, 10.0]), contents::EMPTY);
        assert_eq!(m.hull_point_contents(Hull::Point, [-64.0, 0.0, -10.0]), contents::SOLID);
        // And it agrees with the pre-existing point API, byte for byte.
        for z in [-50.0f32, -1.0, 0.0, 1.0, 50.0] {
            let p = [7.0, 3.0, z];
            assert_eq!(m.hull_point_contents(Hull::Point, p), m.point_contents(p));
            assert_eq!(
                m.hull_trace(Hull::Point, [7.0, 3.0, 100.0], p),
                m.trace([7.0, 3.0, 100.0], p)
            );
        }
    }

    #[test]
    fn a_hull_trace_starting_in_solid_reports_start_solid() {
        let m = doorway_map();
        let t = m.hull_trace(Hull::Stand, [16.0, 0.0, 40.0], [96.0, 0.0, 40.0]);
        assert!(t.start_solid);
        assert!(!t.is_clear(), "start_solid must never read as clear");
    }

    /// A submodel is traced in its own space, shifted by `models[n].origin`.
    #[test]
    fn a_submodel_trace_is_translated_by_the_model_origin() {
        let mut m = doorway_map();
        m.models.push(Model {
            mins: [-1000.0; 3],
            maxs: [1000.0; 3],
            origin: [500.0, 0.0, 0.0],
            headnode: [0, 0, 0, 3],
        });
        // In model space the lintel is at 0..32; shifted, it is at 500..532.
        let blocked = m.hull_trace_model(1, Hull::Stand, [436.0, 0.0, 36.0], [596.0, 0.0, 36.0]);
        assert!(blocked.fraction < 1.0);
        assert!(
            (blocked.end[0] - 500.0).abs() < 1.0,
            "should stop at the shifted lintel, got x={}",
            blocked.end[0]
        );
        // The same segment against the *world* model is nowhere near the lintel.
        assert!(m
            .hull_trace(Hull::Stand, [436.0, 0.0, 36.0], [596.0, 0.0, 36.0])
            .is_clear());
    }

    #[test]
    fn a_hull_the_map_has_no_tree_for_is_empty_not_solid() {
        let mut m = doorway_map();
        // Point every collision hull head past the end of the clipnode array.
        m.models[0].headnode = [0, 9999, 9999, 9999];
        assert_eq!(m.hull_point_contents(Hull::Stand, [16.0, 0.0, 36.0]), contents::EMPTY);
        assert!(m
            .hull_trace(Hull::Stand, [-64.0, 0.0, 36.0], [96.0, 0.0, 36.0])
            .is_clear());
    }

    /// A map with one plane, solid on whichever side the caller asks for.
    fn plane_map(normal: Vec3, solid_front: bool) -> Bsp {
        let children = if solid_front { [-2, -1] } else { [-1, -2] };
        Bsp {
            planes: vec![Plane { normal, dist: 0.0, kind: 3 }],
            nodes: vec![Node { plane: 0, children }],
            leaves: vec![
                Leaf { contents: contents::EMPTY },
                Leaf { contents: contents::SOLID },
            ],
            clipnodes: vec![ClipNode { plane: 0, children }],
            models: vec![Model {
                mins: [-1000.0; 3],
                maxs: [1000.0; 3],
                origin: [0.0; 3],
                headnode: [0, 0, 0, 0],
            }],
            ..Default::default()
        }
    }

    #[test]
    fn a_trace_reports_the_surface_it_stopped_on() {
        // Wall at x = 0, solid behind: approaching from +x, the normal points
        // back at us.
        let m = plane_map([1.0, 0.0, 0.0], false);
        let t = m.trace([10.0, 0.0, 0.0], [-10.0, 0.0, 0.0]);
        assert!(t.fraction < 1.0);
        assert_eq!(t.plane, [1.0, 0.0, 0.0]);
        assert!(!t.is_walkable_floor(), "a vertical wall is not a floor");

        // Solid in *front* of the same plane: approaching from -x now, so the
        // normal is flipped, exactly as PM_RecursiveHullCheck flips it.
        let m = plane_map([1.0, 0.0, 0.0], true);
        let t = m.trace([-10.0, 0.0, 0.0], [10.0, 0.0, 0.0]);
        assert!(t.fraction < 1.0);
        assert_eq!(t.plane, [-1.0, 0.0, 0.0]);

        // Nothing hit, no plane.
        let miss = m.trace([-50.0, 0.0, 0.0], [-10.0, 0.0, 0.0]);
        assert!(miss.is_clear());
        assert_eq!(miss.plane, [0.0; 3]);
        assert!(!miss.is_walkable_floor());
    }

    #[test]
    fn the_walkable_floor_test_is_the_engines_threshold() {
        assert_eq!(WALKABLE_NORMAL_Z, 0.7);
        // A face at 0.8 up is ground; the same face at 0.6 is a slope the
        // engine slides the player off (pm_shared.cpp:1220).
        for (nz, walkable) in [(1.0f32, true), (0.8, true), (0.7, true), (0.6, false)] {
            let nx = (1.0 - nz * nz).sqrt();
            let m = plane_map([nx, 0.0, nz], false);
            let t = m.trace([0.0, 0.0, 100.0], [0.0, 0.0, -100.0]);
            assert!(t.fraction < 1.0, "nz={nz} should have hit");
            assert_eq!(
                t.is_walkable_floor(),
                walkable,
                "nz={nz} normal={:?}",
                t.plane
            );
        }
    }

    #[test]
    fn tracing_a_model_that_does_not_exist_is_a_clear_trace() {
        let m = doorway_map();
        let t = m.hull_trace_model(99, Hull::Stand, [0.0, 0.0, 0.0], [10.0, 0.0, 0.0]);
        assert!(t.is_clear());
        assert_eq!(t.end, [10.0, 0.0, 0.0]);
    }
}
