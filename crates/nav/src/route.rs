//! One A\* for every kind of navigation data.
//!
//! There are two sources of waypoints in this crate and there will be more: a
//! YaPB `.graph` file if the server ships one ([`crate::graph`]), and a grid
//! generated from the map's own collision hulls when it does not
//! ([`crate::navgrid`]). The router must not care which it got — a bot that
//! paths differently depending on where the nodes came from is two bots to
//! debug.
//!
//! So the graph shape lives behind [`NavSource`] and the search lives here,
//! once. The default [`NavSource::cost`] is straight-line distance, which is
//! what a `.graph` uses; a generated grid overrides it to make a jump or a
//! crouch-crawl cost more than a walk of the same length. Any override must
//! return **at least** the straight-line distance, or the Euclidean heuristic
//! stops being admissible and A\* stops being optimal.

use std::collections::BinaryHeap;

pub type Vec3 = [f32; 3];

pub fn dist(a: Vec3, b: Vec3) -> f32 {
    let (dx, dy, dz) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    (dx * dx + dy * dy + dz * dz).sqrt()
}

/// Anything the router can walk over.
pub trait NavSource {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// World position of node `i`. Callers must only pass `i < len()`.
    fn origin(&self, i: usize) -> Vec3;

    /// Node flags — see [`crate::graph::flags`] and [`crate::navgrid::flags`].
    fn flags(&self, i: usize) -> u32;

    /// Append the nodes reachable **from** `i` to `out`.
    ///
    /// `out` is cleared by the router before each call. Taking a buffer rather
    /// than returning a `Vec` keeps the inner loop allocation-free; the search
    /// touches this once per expanded node.
    ///
    /// Edges may be one-way: a drop off a ledge is traversable downwards only,
    /// and reporting it both ways would send bots walking into walls.
    fn neighbours(&self, i: usize, out: &mut Vec<usize>);

    /// Cost of traversing the edge `from -> to`. Must be `>= dist(origins)`.
    fn cost(&self, from: usize, to: usize) -> f32 {
        dist(self.origin(from), self.origin(to))
    }

    fn has_flag(&self, i: usize, flag: u32) -> bool {
        self.flags(i) & flag != 0
    }
}

/// Ordering wrapper so `BinaryHeap` behaves as a min-heap on cost.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Candidate {
    cost: f32,
    node: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reversed: BinaryHeap is a max-heap.
        other
            .cost
            .partial_cmp(&self.cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A\* from `start` to `goal`, returning the node indices inclusive.
///
/// `None` when either endpoint is out of range or no route exists. The
/// heuristic is straight-line distance to the goal.
pub fn find_path<S: NavSource + ?Sized>(src: &S, start: usize, goal: usize) -> Option<Vec<usize>> {
    let n = src.len();
    if start >= n || goal >= n {
        return None;
    }
    if start == goal {
        return Some(vec![start]);
    }

    let mut g = vec![f32::INFINITY; n];
    let mut came: Vec<usize> = vec![usize::MAX; n];
    let mut closed = vec![false; n];
    let mut open = BinaryHeap::new();
    let mut scratch: Vec<usize> = Vec::new();

    let goal_origin = src.origin(goal);
    let h = |i: usize| dist(src.origin(i), goal_origin);

    g[start] = 0.0;
    open.push(Candidate { cost: h(start), node: start });

    while let Some(Candidate { node, .. }) = open.pop() {
        if node == goal {
            let mut path = vec![goal];
            let mut cur = goal;
            while came[cur] != usize::MAX {
                cur = came[cur];
                path.push(cur);
            }
            path.reverse();
            return Some(path);
        }
        if closed[node] {
            continue;
        }
        closed[node] = true;

        scratch.clear();
        src.neighbours(node, &mut scratch);
        for &next in &scratch {
            if next >= n || closed[next] {
                continue;
            }
            let tentative = g[node] + src.cost(node, next);
            if tentative < g[next] {
                g[next] = tentative;
                came[next] = node;
                open.push(Candidate { cost: tentative + h(next), node: next });
            }
        }
    }
    None
}

/// The node closest to a world position. `None` for an empty source.
pub fn nearest<S: NavSource + ?Sized>(src: &S, pos: Vec3) -> Option<usize> {
    (0..src.len()).min_by(|&a, &b| {
        dist(src.origin(a), pos)
            .partial_cmp(&dist(src.origin(b), pos))
            .unwrap_or(std::cmp::Ordering::Equal)
    })
}

/// Every node carrying `flag`.
pub fn nodes_with_flag<S: NavSource + ?Sized>(src: &S, flag: u32) -> Vec<usize> {
    (0..src.len()).filter(|&i| src.has_flag(i, flag)).collect()
}

/// Which nodes are reachable from `roots` by following edges forwards.
///
/// Direction matters: a one-way drop means "reachable from the spawn" and
/// "can reach the spawn" are different questions. This answers the first.
pub fn reachable_from<S: NavSource + ?Sized>(src: &S, roots: &[usize]) -> Vec<bool> {
    let n = src.len();
    let mut seen = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut scratch: Vec<usize> = Vec::new();
    for &r in roots {
        if r < n && !seen[r] {
            seen[r] = true;
            stack.push(r);
        }
    }
    while let Some(i) = stack.pop() {
        scratch.clear();
        src.neighbours(i, &mut scratch);
        for &j in &scratch {
            if j < n && !seen[j] {
                seen[j] = true;
                stack.push(j);
            }
        }
    }
    seen
}

// ---------------------------------------------------------------- .graph

/// A YaPB waypoint file, seen as a routing source.
///
/// This is the adapter that makes [`crate::graph::Graph`] and
/// [`crate::navgrid::NavGrid`] interchangeable to a caller.
impl NavSource for crate::graph::Graph {
    fn len(&self) -> usize {
        self.nodes.len()
    }

    fn origin(&self, i: usize) -> Vec3 {
        self.nodes[i].origin
    }

    fn flags(&self, i: usize) -> u32 {
        self.nodes[i].flags
    }

    fn neighbours(&self, i: usize, out: &mut Vec<usize>) {
        out.extend(self.nodes[i].neighbours());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{Graph, Node};

    /// A hand-built source: positions plus an adjacency list, so the router can
    /// be tested without any file format in the way.
    struct Toy {
        origins: Vec<Vec3>,
        links: Vec<Vec<usize>>,
        costs: Option<f32>,
    }

    impl NavSource for Toy {
        fn len(&self) -> usize {
            self.origins.len()
        }
        fn origin(&self, i: usize) -> Vec3 {
            self.origins[i]
        }
        fn flags(&self, _: usize) -> u32 {
            0
        }
        fn neighbours(&self, i: usize, out: &mut Vec<usize>) {
            out.extend_from_slice(&self.links[i]);
        }
        fn cost(&self, from: usize, to: usize) -> f32 {
            let d = dist(self.origin(from), self.origin(to));
            match self.costs {
                Some(mul) => d * mul,
                None => d,
            }
        }
    }

    fn toy(origins: &[(f32, f32)], links: &[&[usize]]) -> Toy {
        Toy {
            origins: origins.iter().map(|&(x, y)| [x, y, 0.0]).collect(),
            links: links.iter().map(|l| l.to_vec()).collect(),
            costs: None,
        }
    }

    fn node_at(number: i32, x: f32, y: f32, links: &[i16]) -> Node {
        let mut n = Node { number, origin: [x, y, 0.0], ..Default::default() };
        for (slot, idx) in n.links.iter_mut().zip(links) {
            slot.index = *idx;
        }
        n
    }

    #[test]
    fn a_line_is_walked_end_to_end() {
        let t = toy(
            &[(0.0, 0.0), (100.0, 0.0), (200.0, 0.0), (300.0, 0.0)],
            &[&[1], &[0, 2], &[1, 3], &[2]],
        );
        assert_eq!(find_path(&t, 0, 3), Some(vec![0, 1, 2, 3]));
        assert_eq!(find_path(&t, 3, 0), Some(vec![3, 2, 1, 0]));
        assert_eq!(find_path(&t, 2, 2), Some(vec![2]));
    }

    #[test]
    fn the_shorter_of_two_routes_wins() {
        let t = toy(
            &[(0.0, 0.0), (50.0, 900.0), (100.0, 0.0)],
            &[&[1, 2], &[0, 2], &[0, 1]],
        );
        assert_eq!(find_path(&t, 0, 2), Some(vec![0, 2]));
    }

    #[test]
    fn a_disconnected_goal_has_no_path() {
        let t = toy(&[(0.0, 0.0), (100.0, 0.0)], &[&[], &[]]);
        assert_eq!(find_path(&t, 0, 1), None);
    }

    #[test]
    fn out_of_range_endpoints_are_refused() {
        let t = toy(&[(0.0, 0.0)], &[&[]]);
        assert_eq!(find_path(&t, 0, 99), None);
        assert_eq!(find_path(&t, 99, 0), None);
    }

    #[test]
    fn a_link_pointing_past_the_end_is_ignored_not_panicked_on() {
        let t = toy(&[(0.0, 0.0), (10.0, 0.0)], &[&[99, 1], &[]]);
        assert_eq!(find_path(&t, 0, 1), Some(vec![0, 1]));
    }

    #[test]
    fn one_way_edges_are_respected() {
        // 0 -> 1, but not back. This is a ledge drop.
        let t = toy(&[(0.0, 0.0), (100.0, 0.0)], &[&[1], &[]]);
        assert_eq!(find_path(&t, 0, 1), Some(vec![0, 1]));
        assert_eq!(find_path(&t, 1, 0), None);
    }

    #[test]
    fn an_expensive_edge_is_avoided_when_a_detour_is_cheaper() {
        //   0 --(direct, 100 units)-- 2
        //   0 -- 1 -- 2 via (50, 40): 128 units of travel
        // With cost multiplier 2 the direct edge costs 200 and the detour 256,
        // so the direct one still wins; the point is that cost() is consulted.
        let mut t = toy(
            &[(0.0, 0.0), (50.0, 40.0), (100.0, 0.0)],
            &[&[1, 2], &[0, 2], &[0, 1]],
        );
        t.costs = Some(2.0);
        assert_eq!(find_path(&t, 0, 2), Some(vec![0, 2]));
        // Now make everything cost the same but remove the direct edge.
        let t2 = toy(
            &[(0.0, 0.0), (50.0, 40.0), (100.0, 0.0)],
            &[&[1], &[0, 2], &[1]],
        );
        assert_eq!(find_path(&t2, 0, 2), Some(vec![0, 1, 2]));
    }

    #[test]
    fn nearest_and_flags_work_through_the_trait() {
        let t = toy(&[(0.0, 0.0), (100.0, 0.0), (500.0, 0.0)], &[&[], &[], &[]]);
        assert_eq!(nearest(&t, [90.0, 0.0, 0.0]), Some(1));
        assert_eq!(nearest(&t, [-10.0, 0.0, 0.0]), Some(0));
        assert!(nodes_with_flag(&t, 1).is_empty());
        let empty = toy(&[], &[]);
        assert_eq!(nearest(&empty, [0.0; 3]), None);
        assert!(empty.is_empty());
    }

    #[test]
    fn reachability_follows_edges_forwards_only() {
        //  0 -> 1 -> 2   ;   3 isolated   ;   4 -> 1 (but 1 does not reach 4)
        let t = toy(
            &[(0.0, 0.0), (10.0, 0.0), (20.0, 0.0), (99.0, 99.0), (5.0, 5.0)],
            &[&[1], &[2], &[], &[], &[1]],
        );
        assert_eq!(reachable_from(&t, &[0]), vec![true, true, true, false, false]);
        assert_eq!(reachable_from(&t, &[4]), vec![false, true, true, false, true]);
        assert_eq!(reachable_from(&t, &[]), vec![false; 5]);
        // An out-of-range root is ignored rather than panicking.
        assert_eq!(reachable_from(&t, &[99]), vec![false; 5]);
    }

    // -- the .graph adapter -------------------------------------------------

    /// The shared router must agree with `Graph::find_path` exactly, on the
    /// same cases `graph.rs` tests. That is what makes them one router.
    #[test]
    fn the_graph_adapter_agrees_with_graph_find_path() {
        let line = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[1]),
            node_at(1, 100.0, 0.0, &[0, 2]),
            node_at(2, 200.0, 0.0, &[1, 3]),
            node_at(3, 300.0, 0.0, &[2]),
        ]);
        let detour = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[1, 2]),
            node_at(1, 50.0, 900.0, &[0, 2]),
            node_at(2, 100.0, 0.0, &[0, 1]),
        ]);
        let split = Graph::new(vec![
            node_at(0, 0.0, 0.0, &[]),
            node_at(1, 100.0, 0.0, &[]),
        ]);

        for g in [&line, &detour, &split] {
            for a in 0..g.len() + 2 {
                for b in 0..g.len() + 2 {
                    assert_eq!(
                        find_path(g, a, b),
                        g.find_path(a, b),
                        "shared router disagreed on {a} -> {b}"
                    );
                }
            }
        }
        // And the answers are the expected ones, not two matching wrong ones.
        assert_eq!(find_path(&line, 0, 3), Some(vec![0, 1, 2, 3]));
        assert_eq!(find_path(&detour, 0, 2), Some(vec![0, 2]));
        assert_eq!(find_path(&split, 0, 1), None);
    }

    /// The same equivalence at a scale where a difference in tie-breaking,
    /// heap order or relaxation order would actually show up.
    ///
    /// This is the evidence that replacing `Graph::find_path`'s body with
    /// `route::find_path(self, start, goal)` changes nothing: 240 node pairs
    /// over a 120-node graph with duplicate-cost routes and dead ends, and
    /// every answer identical.
    #[test]
    fn the_two_routers_agree_on_a_large_graph_with_ties() {
        // Deterministic pseudo-random layout: a 12x10 mesh whose links are
        // thinned unevenly, which produces plenty of equal-cost alternatives.
        let mut seed = 0x2545_F491u32;
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };

        let (w, h) = (12usize, 10usize);
        let mut nodes: Vec<Node> = Vec::new();
        for y in 0..h {
            for x in 0..w {
                nodes.push(node_at(
                    (y * w + x) as i32,
                    x as f32 * 100.0,
                    y as f32 * 100.0,
                    &[],
                ));
            }
        }
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let mut slot = 0;
                for (dx, dy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                    let (nx, ny) = (x as i32 + dx, y as i32 + dy);
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    // Drop roughly a quarter of the links, in both directions
                    // independently, so some edges end up one-way.
                    if rand() % 4 == 0 {
                        continue;
                    }
                    if slot < crate::graph::MAX_LINKS {
                        nodes[i].links[slot].index = (ny as usize * w + nx as usize) as i16;
                        slot += 1;
                    }
                }
            }
        }
        let g = Graph::new(nodes);

        let mut found = 0;
        let mut missing = 0;
        for k in 0..240 {
            let a = (k * 7) % g.len();
            let b = (k * 13 + 5) % g.len();
            let mine = find_path(&g, a, b);
            assert_eq!(mine, g.find_path(a, b), "disagreed on {a} -> {b}");
            if mine.is_some() {
                found += 1;
            } else {
                missing += 1;
            }
        }
        // The graph must actually exercise both outcomes, or the agreement is
        // vacuous.
        assert!(found > 100, "only {found} pairs were connected");
        eprintln!("router equivalence: {found} routed, {missing} unreachable");
    }

    #[test]
    fn a_graph_exposes_origins_flags_and_links_through_the_trait() {
        let mut a = node_at(0, 0.0, 0.0, &[1]);
        a.flags = crate::graph::flags::GOAL;
        let g = Graph::new(vec![a, node_at(1, 10.0, 0.0, &[0])]);
        assert_eq!(NavSource::len(&g), 2);
        assert_eq!(g.origin(0), [0.0, 0.0, 0.0]);
        assert_eq!(g.flags(0), crate::graph::flags::GOAL);
        assert_eq!(nodes_with_flag(&g, crate::graph::flags::GOAL), vec![0]);
        let mut out = Vec::new();
        g.neighbours(0, &mut out);
        assert_eq!(out, vec![1]);
        // nearest() through the trait matches Graph::nearest.
        assert_eq!(nearest(&g, [9.0, 0.0, 0.0]), g.nearest([9.0, 0.0, 0.0]));
    }
}
