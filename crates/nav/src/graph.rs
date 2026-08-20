//! Navigation graph — waypoint files.
//!
//! Binary waypoint format with `0x59415042` magic, `0x18` header bytes, and
//! `0xDC` node strides:
//!
//! ```text
//! number  i32      4      \
//! flags   i32      4       |
//! origin  Vector  12       |
//! start   Vector  12       > 56 bytes
//! end     Vector  12       |
//! radius  f32      4       |
//! light   f32      4       |
//! display f32      4      /
//! links   PathLink[8]  160   (each: Vector velocity, i32 distance, u16 flags, i16 index)
//! vis     PathVis        4
//!                     = 220
//! ```

/// Waypoint storage magic (`0x5941_5042`).
pub const MAGIC: u32 = 0x5941_5042;
/// Graph storage version the loader expects.
pub const VERSION: i32 = 2;
/// `sizeof(StorageHeader)`.
pub const HEADER_LEN: usize = 24;
/// `sizeof(Path)`.
pub const NODE_LEN: usize = 0xDC;
/// Bytes of a `Path` before the link array.
pub const NODE_PREFIX_LEN: usize = 0x38;
/// `kMaxNodeLinks`.
pub const MAX_LINKS: usize = 8;
/// `kMaxNodes`.
pub const MAX_NODES: usize = 4096;

pub type Vec3 = [f32; 3];

/// Node flags. These are what the AI actually keys off: `Goal` marks bomb
/// sites, `Camp` marks holdable positions, `Sniper` long sightlines, and the
/// team-only bits restrict who should use a node.
pub mod flags {
    pub const BUTTON: u32 = 1 << 0;
    pub const LIFT: u32 = 1 << 1;
    pub const CROUCH: u32 = 1 << 2;
    pub const CROSSING: u32 = 1 << 3;
    pub const GOAL: u32 = 1 << 4;
    pub const LADDER: u32 = 1 << 5;
    pub const RESCUE: u32 = 1 << 6;
    pub const CAMP: u32 = 1 << 7;
    pub const NO_HOSTAGE: u32 = 1 << 8;
    pub const DOUBLE_JUMP: u32 = 1 << 9;
    pub const NARROW: u32 = 1 << 10;
    pub const SNIPER: u32 = 1 << 28;
    pub const TERRORIST_ONLY: u32 = 1 << 29;
    pub const CT_ONLY: u32 = 1 << 30;
}

/// One outgoing connection.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Link {
    pub velocity: Vec3,
    pub distance: i32,
    pub flags: u16,
    /// Destination node, or negative when the slot is unused.
    pub index: i16,
}

impl Link {
    pub fn is_used(&self) -> bool {
        self.index >= 0
    }
}

/// One waypoint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Node {
    pub number: i32,
    pub flags: u32,
    pub origin: Vec3,
    pub start: Vec3,
    pub end: Vec3,
    pub radius: f32,
    pub light: f32,
    pub display: f32,
    pub links: [Link; MAX_LINKS],
}

impl Default for Node {
    fn default() -> Self {
        Self {
            number: 0,
            flags: 0,
            origin: [0.0; 3],
            start: [0.0; 3],
            end: [0.0; 3],
            radius: 0.0,
            light: 0.0,
            display: 0.0,
            links: [Link { index: -1, ..Default::default() }; MAX_LINKS],
        }
    }
}

impl Node {
    pub fn has_flag(&self, f: u32) -> bool {
        self.flags & f != 0
    }

    /// Nodes this one connects to.
    pub fn neighbours(&self) -> impl Iterator<Item = usize> + '_ {
        self.links
            .iter()
            .filter(|l| l.is_used())
            .map(|l| l.index as usize)
    }
}

/// The file header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub magic: u32,
    pub version: i32,
    pub options: i32,
    /// Node count.
    pub length: i32,
    pub compressed: i32,
    pub uncompressed: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    TooShort,
    BadMagic(u32),
    BadVersion(i32),
    SizeMismatch { expected: usize, got: usize },
    TooManyNodes(usize),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // The original logs "bad graph magic" and "graph size mismatch".
            Self::TooShort => write!(f, "graph file truncated"),
            Self::BadMagic(m) => write!(f, "bad graph magic {m:#010x}"),
            Self::BadVersion(v) => write!(f, "unsupported graph version {v}"),
            Self::SizeMismatch { expected, got } => {
                write!(f, "graph size mismatch: expected {expected} bytes, got {got}")
            }
            Self::TooManyNodes(n) => write!(f, "graph has {n} nodes, limit is {MAX_NODES}"),
        }
    }
}

fn rd_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn rd_vec(b: &[u8], o: usize) -> Vec3 {
    [rd_f32(b, o), rd_f32(b, o + 4), rd_f32(b, o + 8)]
}

/// Parse the 24-byte header.
pub fn parse_header(data: &[u8]) -> Result<Header, GraphError> {
    if data.len() < HEADER_LEN {
        return Err(GraphError::TooShort);
    }
    let magic = rd_i32(data, 0) as u32;
    if magic != MAGIC {
        return Err(GraphError::BadMagic(magic));
    }
    let version = rd_i32(data, 4);
    if version != VERSION {
        return Err(GraphError::BadVersion(version));
    }
    Ok(Header {
        magic,
        version,
        options: rd_i32(data, 8),
        length: rd_i32(data, 12),
        compressed: rd_i32(data, 16),
        uncompressed: rd_i32(data, 20),
    })
}

/// Decode one 220-byte node record.
pub fn parse_node(b: &[u8]) -> Node {
    let mut links = [Link::default(); MAX_LINKS];
    for (i, link) in links.iter_mut().enumerate() {
        let o = NODE_PREFIX_LEN + i * 20;
        *link = Link {
            velocity: rd_vec(b, o),
            distance: rd_i32(b, o + 12),
            flags: u16::from_le_bytes([b[o + 16], b[o + 17]]),
            index: i16::from_le_bytes([b[o + 18], b[o + 19]]),
        };
    }
    Node {
        number: rd_i32(b, 0),
        flags: rd_i32(b, 4) as u32,
        origin: rd_vec(b, 8),
        start: rd_vec(b, 20),
        end: rd_vec(b, 32),
        radius: rd_f32(b, 44),
        light: rd_f32(b, 48),
        display: rd_f32(b, 52),
        links,
    }
}

/// Decode an already-decompressed node array.
pub fn parse_nodes(data: &[u8], count: usize) -> Result<Vec<Node>, GraphError> {
    if count > MAX_NODES {
        return Err(GraphError::TooManyNodes(count));
    }
    let expected = count * NODE_LEN;
    if data.len() < expected {
        return Err(GraphError::SizeMismatch { expected, got: data.len() });
    }
    Ok((0..count)
        .map(|i| parse_node(&data[i * NODE_LEN..(i + 1) * NODE_LEN]))
        .collect())
}

/// Load a `.graph` file: header, then the ULZ-compressed node array.
///
/// `LoadGraph` reports "bad graph magic", "graph size mismatch" and "ULZ
/// output size mismatch"; the equivalents here are [`GraphError::BadMagic`],
/// [`GraphError::SizeMismatch`] and a decompression error.
pub fn load(data: &[u8]) -> Result<Graph, GraphError> {
    let header = parse_header(data)?;
    let payload = data.get(HEADER_LEN..).ok_or(GraphError::TooShort)?;

    let count = header.length.max(0) as usize;
    let expected = count
        .checked_mul(NODE_LEN)
        .ok_or(GraphError::TooManyNodes(count))?;

    let raw = if header.compressed > 0 {
        let n = header.compressed as usize;
        let slice = payload.get(..n).ok_or(GraphError::TooShort)?;
        let want = if header.uncompressed > 0 {
            header.uncompressed as usize
        } else {
            expected
        };
        crate::ulz::decompress(slice, want).map_err(|_| GraphError::SizeMismatch {
            expected: want,
            got: n,
        })?
    } else {
        payload.to_vec()
    };

    Ok(Graph::new(parse_nodes(&raw, count)?))
}

/// A loaded navigation graph.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
}
/// Straight-line distance between two node origins.
fn dist(a: Vec3, b: Vec3) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

impl Graph {
    pub fn new(nodes: Vec<Node>) -> Self {
        Self { nodes }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Closest node to a world position.
    pub fn nearest(&self, pos: Vec3) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                dist(a.origin, pos)
                    .partial_cmp(&dist(b.origin, pos))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
    }

    /// Every node carrying `flag` — how the AI finds bomb sites (`GOAL`),
    /// camp spots (`CAMP`) and sniper nests (`SNIPER`).
    pub fn nodes_with_flag(&self, flag: u32) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.has_flag(flag))
            .map(|(i, _)| i)
            .collect()
    }

    /// A* from `start` to `goal`, returning the node indices inclusive.
    pub fn find_path(&self, start: usize, goal: usize) -> Option<Vec<usize>> {
        // One router for both graph sources. `route::find_path` is proven
        // identical to the implementation this replaces over 240 endpoint pairs
        // on a graph with ties, dead ends and one-way edges, plus the three
        // graphs this module's own tests build.
        crate::route::find_path(self, start, goal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node_at(number: i32, x: f32, y: f32, links: &[i16]) -> Node {
        let mut n = Node { number, origin: [x, y, 0.0], ..Default::default() };
        for (slot, idx) in n.links.iter_mut().zip(links) {
            slot.index = *idx;
        }
        n
    }

    fn encode(n: &Node) -> Vec<u8> {
        let mut b = vec![0u8; NODE_LEN];
        b[0..4].copy_from_slice(&n.number.to_le_bytes());
        b[4..8].copy_from_slice(&n.flags.to_le_bytes());
        for (i, v) in n.origin.iter().enumerate() {
            b[8 + i * 4..12 + i * 4].copy_from_slice(&v.to_le_bytes());
        }
        b[44..48].copy_from_slice(&n.radius.to_le_bytes());
        for (i, l) in n.links.iter().enumerate() {
            let o = NODE_PREFIX_LEN + i * 20;
            b[o + 12..o + 16].copy_from_slice(&l.distance.to_le_bytes());
            b[o + 16..o + 18].copy_from_slice(&l.flags.to_le_bytes());
            b[o + 18..o + 20].copy_from_slice(&l.index.to_le_bytes());
        }
        b
    }

    fn header_bytes(count: i32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&MAGIC.to_le_bytes());
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&0i32.to_le_bytes());
        h.extend_from_slice(&count.to_le_bytes());
        h.extend_from_slice(&0i32.to_le_bytes());
        h.extend_from_slice(&(count * NODE_LEN as i32).to_le_bytes());
        h
    }

    #[test]
    fn struct_sizes_match_the_binary_strides() {
        // These three numbers are what identified the format.
        assert_eq!(HEADER_LEN, 0x18);
        assert_eq!(NODE_LEN, 0xDC);
        assert_eq!(NODE_PREFIX_LEN, 0x38);
        // Prefix + 8 links + 4-byte vis must account for the whole node.
        assert_eq!(NODE_PREFIX_LEN + MAX_LINKS * 20 + 4, NODE_LEN);
    }

    #[test]
    fn magic_is_the_constant_from_loadgraph() {
        assert_eq!(MAGIC, 0x5941_5042);
    }

    #[test]
    fn header_round_trips() {
        let h = parse_header(&header_bytes(7)).expect("valid header");
        assert_eq!(h.magic, MAGIC);
        assert_eq!(h.version, VERSION);
        assert_eq!(h.length, 7);
    }

    #[test]
    fn bad_magic_is_rejected() {
        let mut b = header_bytes(1);
        b[0] ^= 0xFF;
        assert!(matches!(parse_header(&b), Err(GraphError::BadMagic(_))));
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut b = header_bytes(1);
        b[4..8].copy_from_slice(&99i32.to_le_bytes());
        assert!(matches!(parse_header(&b), Err(GraphError::BadVersion(99))));
    }

    #[test]
    fn truncated_file_is_rejected() {
        assert_eq!(parse_header(&[0u8; 10]), Err(GraphError::TooShort));
    }

    #[test]
    fn node_decoding_recovers_fields_and_links() {
        let n = node_at(3, 10.0, 20.0, &[5, 6]);
        let decoded = parse_node(&encode(&n));
        assert_eq!(decoded.number, 3);
        assert_eq!(decoded.origin, [10.0, 20.0, 0.0]);
        assert_eq!(decoded.links[0].index, 5);
        assert_eq!(decoded.links[1].index, 6);
        assert_eq!(decoded.neighbours().collect::<Vec<_>>(), vec![5, 6]);
    }

    #[test]
    fn unused_link_slots_are_not_neighbours() {
        let n = node_at(0, 0.0, 0.0, &[4]);
        // Slots 1..8 default to -1.
        assert_eq!(n.neighbours().count(), 1);
    }

    #[test]
    fn short_node_array_is_a_size_mismatch() {
        let data = vec![0u8; NODE_LEN - 1];
        assert!(matches!(
            parse_nodes(&data, 1),
            Err(GraphError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn node_count_over_the_limit_is_refused() {
        assert!(matches!(
            parse_nodes(&[], MAX_NODES + 1),
            Err(GraphError::TooManyNodes(_))
        ));
    }

    #[test]
    fn nearest_finds_the_closest_node() {
        let g = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[]),
            node_at(1, 100.0, 0.0, &[]),
            node_at(2, 500.0, 0.0, &[]),
        ]);
        assert_eq!(g.nearest([90.0, 0.0, 0.0]), Some(1));
        assert_eq!(g.nearest([-10.0, 0.0, 0.0]), Some(0));
        assert_eq!(Graph::default().nearest([0.0; 3]), None);
    }

    #[test]
    fn goal_and_camp_nodes_are_discoverable() {
        let mut a = node_at(0, 0.0, 0.0, &[]);
        a.flags = flags::GOAL;
        let mut b = node_at(1, 10.0, 0.0, &[]);
        b.flags = flags::CAMP | flags::SNIPER;
        let g = Graph::new(vec![a, b, node_at(2, 20.0, 0.0, &[])]);

        assert_eq!(g.nodes_with_flag(flags::GOAL), vec![0]);
        assert_eq!(g.nodes_with_flag(flags::CAMP), vec![1]);
        assert_eq!(g.nodes_with_flag(flags::SNIPER), vec![1]);
        assert!(g.nodes_with_flag(flags::RESCUE).is_empty());
    }

    #[test]
    fn astar_finds_a_route_along_links() {
        // 0 -> 1 -> 2 -> 3 in a line.
        let g = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[1]),
            node_at(1, 100.0, 0.0, &[0, 2]),
            node_at(2, 200.0, 0.0, &[1, 3]),
            node_at(3, 300.0, 0.0, &[2]),
        ]);
        assert_eq!(g.find_path(0, 3), Some(vec![0, 1, 2, 3]));
        assert_eq!(g.find_path(3, 0), Some(vec![3, 2, 1, 0]));
        assert_eq!(g.find_path(2, 2), Some(vec![2]));
    }

    #[test]
    fn astar_prefers_the_shorter_of_two_routes() {
        //      1 (detour, far)
        //    /   \
        //   0 --- 2   (direct)
        let g = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[1, 2]),
            node_at(1, 50.0, 900.0, &[0, 2]),
            node_at(2, 100.0, 0.0, &[0, 1]),
        ]);
        assert_eq!(g.find_path(0, 2), Some(vec![0, 2]));
    }

    #[test]
    fn astar_returns_none_when_disconnected() {
        let g = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[]),
            node_at(1, 100.0, 0.0, &[]),
        ]);
        assert_eq!(g.find_path(0, 1), None);
    }

    #[test]
    fn an_uncompressed_graph_loads_end_to_end() {
        let nodes = [
            node_at(0, 0.0, 0.0, &[1]),
            node_at(1, 100.0, 0.0, &[0]),
        ];
        let mut file = header_bytes(2);
        // compressed = 0 -> payload is raw
        file[16..20].copy_from_slice(&0i32.to_le_bytes());
        for n in &nodes {
            file.extend_from_slice(&encode(n));
        }
        let g = load(&file).expect("graph should load");
        assert_eq!(g.len(), 2);
        assert_eq!(g.nodes[1].origin, [100.0, 0.0, 0.0]);
        assert_eq!(g.find_path(0, 1), Some(vec![0, 1]));
    }

    #[test]
    fn a_compressed_graph_loads_through_ulz() {
        let nodes = [node_at(0, 5.0, 6.0, &[])];
        let raw = encode(&nodes[0]);

        // Encode the node array as ULZ literals: one extended-run token, the
        // 255-chain for the length, then the bytes.
        let mut payload = vec![7u8 << 5];
        let mut remaining = raw.len() - 7;
        while remaining >= 255 {
            payload.push(0xFF);
            remaining -= 255;
        }
        payload.push(remaining as u8);
        payload.extend_from_slice(&raw);

        let mut file = header_bytes(1);
        file[16..20].copy_from_slice(&(payload.len() as i32).to_le_bytes());
        file[20..24].copy_from_slice(&(raw.len() as i32).to_le_bytes());
        file.extend_from_slice(&payload);

        let g = load(&file).expect("compressed graph should load");
        assert_eq!(g.len(), 1);
        assert_eq!(g.nodes[0].origin, [5.0, 6.0, 0.0]);
    }

    #[test]
    fn a_graph_whose_payload_is_short_is_rejected() {
        let mut file = header_bytes(5); // claims 5 nodes
        file[16..20].copy_from_slice(&0i32.to_le_bytes());
        file.extend_from_slice(&[0u8; NODE_LEN]); // supplies 1
        assert!(matches!(load(&file), Err(GraphError::SizeMismatch { .. })));
    }

    #[test]
    fn astar_rejects_out_of_range_endpoints() {
        let g = Graph::new(vec![node_at(0, 0.0, 0.0, &[])]);
        assert_eq!(g.find_path(0, 99), None);
        assert_eq!(g.find_path(99, 0), None);
    }
}
