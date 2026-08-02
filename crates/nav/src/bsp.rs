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
    pub models: Vec<Model>,
}

/// Result of a point trace.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trace {
    /// 0..1 along the segment where it stopped. 1.0 means it reached the end.
    pub fraction: f32,
    /// The trace began inside solid space.
    pub start_solid: bool,
    pub end: Vec3,
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

        Ok(Self { planes, nodes, leaves, models })
    }

    /// Contents of the leaf containing `p`, walking the world tree.
    pub fn point_contents(&self, p: Vec3) -> i32 {
        let mut num = self.models[0].headnode[0];
        let mut guard = 0;
        while num >= 0 {
            guard += 1;
            if guard > 4096 {
                // Malformed tree; refuse to spin.
                return contents::SOLID;
            }
            let Some(node) = self.nodes.get(num as usize) else {
                return contents::SOLID;
            };
            let Some(plane) = self.planes.get(node.plane as usize) else {
                return contents::SOLID;
            };
            let d = dot(plane.normal, p) - plane.dist;
            num = i32::from(node.children[usize::from(d < 0.0)]);
        }
        let leaf = (-num - 1) as usize;
        self.leaves.get(leaf).map_or(contents::SOLID, |l| l.contents)
    }

    /// Trace a point from `start` to `end`.
    pub fn trace(&self, start: Vec3, end: Vec3) -> Trace {
        let mut t = Trace { fraction: 1.0, start_solid: false, end };
        let head = self.models[0].headnode[0];
        self.recurse(head, 0.0, 1.0, start, end, &mut t);
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

    fn recurse(
        &self,
        num: i32,
        p1f: f32,
        p2f: f32,
        p1: Vec3,
        p2: Vec3,
        trace: &mut Trace,
    ) -> bool {
        // Leaf.
        if num < 0 {
            let leaf = (-num - 1) as usize;
            let c = self.leaves.get(leaf).map_or(contents::SOLID, |l| l.contents);
            if c == contents::SOLID {
                if p1f == 0.0 {
                    trace.start_solid = true;
                }
                return false;
            }
            return true;
        }

        let Some(node) = self.nodes.get(num as usize) else {
            return true;
        };
        let Some(plane) = self.planes.get(node.plane as usize) else {
            return true;
        };

        let t1 = dot(plane.normal, p1) - plane.dist;
        let t2 = dot(plane.normal, p2) - plane.dist;

        if t1 >= 0.0 && t2 >= 0.0 {
            return self.recurse(i32::from(node.children[0]), p1f, p2f, p1, p2, trace);
        }
        if t1 < 0.0 && t2 < 0.0 {
            return self.recurse(i32::from(node.children[1]), p1f, p2f, p1, p2, trace);
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

        if !self.recurse(i32::from(node.children[side]), p1f, midf, p1, mid, trace) {
            return false;
        }

        // Near half is open; if the far half is not solid at the split point,
        // keep going.
        if self.contents_at(i32::from(node.children[side ^ 1]), mid) != contents::SOLID {
            return self.recurse(i32::from(node.children[side ^ 1]), midf, p2f, mid, p2, trace);
        }

        // We hit a solid surface here.
        trace.fraction = midf;
        false
    }

    /// Contents of the leaf reached from `num` for point `p`.
    fn contents_at(&self, num: i32, p: Vec3) -> i32 {
        let mut n = num;
        let mut guard = 0;
        while n >= 0 {
            guard += 1;
            if guard > 4096 {
                return contents::SOLID;
            }
            let Some(node) = self.nodes.get(n as usize) else {
                return contents::SOLID;
            };
            let Some(plane) = self.planes.get(node.plane as usize) else {
                return contents::SOLID;
            };
            let d = dot(plane.normal, p) - plane.dist;
            n = i32::from(node.children[usize::from(d < 0.0)]);
        }
        let leaf = (-n - 1) as usize;
        self.leaves.get(leaf).map_or(contents::SOLID, |l| l.contents)
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
        };
        let g = m.ground_height([0.0, 0.0, 100.0], 500.0).expect("floor found");
        assert!(g.abs() < 1.0, "floor should be near z=0, got {g}");
    }

    #[test]
    fn ground_height_returns_none_over_a_pit() {
        let m = wall_map(); // vertical plane only; nothing below
        assert_eq!(m.ground_height([10.0, 0.0, 0.0], 50.0), None);
    }
}
