//! Grid navigation: global routing over the terrain tiles. Movement gambits only
//! ever express *intent* (`MoveToward`/`MoveAway`/seek high ground …); this layer
//! answers *where to actually step*. Steering (following the route smoothly,
//! local avoidance) lives in `eval.rs`/`combat.rs` — A\* here is the "where",
//! steering there is the "how".
//!
//! Steering alone can't escape a concave obstacle (it walks into the pocket and
//! stops), so global routing is genuinely needed. Both entry points are pure
//! functions of the terrain — no world state, fully unit-testable.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use crate::terrain::{Terrain, Tile};

/// √2, the cost of a diagonal step (orthogonal steps cost 1).
const SQRT2: f32 = std::f32::consts::SQRT_2;

/// The eight tile neighbours, orthogonal first (keeps ties biased toward
/// straight moves, which look tidier than a diagonal jitter).
const DIRS: [(i32, i32); 8] = [
    (1, 0), (-1, 0), (0, 1), (0, -1),
    (1, 1), (1, -1), (-1, 1), (-1, -1),
];

/// A frontier entry ordered by *estimated total cost* so [`BinaryHeap`] (a
/// max-heap) pops the cheapest first — hence the reversed comparison.
#[derive(Clone, Copy)]
struct Frontier {
    est: f32,
    tile: Tile,
}
impl PartialEq for Frontier {
    fn eq(&self, o: &Self) -> bool {
        self.est == o.est
    }
}
impl Eq for Frontier {}
impl Ord for Frontier {
    fn cmp(&self, o: &Self) -> Ordering {
        o.est.total_cmp(&self.est) // reversed: smaller est is "greater"
    }
}
impl PartialOrd for Frontier {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

/// Can a unit move from `from` to the *diagonally* adjacent `to`? Beyond the
/// usual walkability, both flanking tiles must be passable — otherwise the step
/// would cut through the corner where two walls meet.
fn diagonal_ok(t: &Terrain, from: Tile, to: Tile) -> bool {
    t.walkable(from, to)
        && t.passable(to.0, from.1)
        && t.passable(from.0, to.1)
}

fn step_ok(t: &Terrain, from: Tile, to: Tile) -> bool {
    let diagonal = from.0 != to.0 && from.1 != to.1;
    if diagonal {
        diagonal_ok(t, from, to)
    } else {
        t.walkable(from, to)
    }
}

/// Octile heuristic: the exact cost of an obstacle-free diagonal+straight run.
/// Admissible (never overestimates), so A\* stays optimal.
fn heuristic(a: Tile, b: Tile) -> f32 {
    let dx = (a.0 - b.0).abs() as f32;
    let dy = (a.1 - b.1).abs() as f32;
    let (lo, hi) = if dx < dy { (dx, dy) } else { (dy, dx) };
    (hi - lo) + SQRT2 * lo
}

/// A\* from `start` to `goal` over walkable tiles. Returns the waypoint tiles
/// from `start` to `goal` inclusive, or `None` if the goal is unreachable (or
/// itself impassable). `start == goal` yields a single-tile path.
pub fn find_path(t: &Terrain, start: Tile, goal: Tile) -> Option<Vec<Tile>> {
    if !t.passable(goal.0, goal.1) || !t.passable(start.0, start.1) {
        return None;
    }
    if start == goal {
        return Some(vec![start]);
    }

    let mut g: HashMap<Tile, f32> = HashMap::new();
    let mut came: HashMap<Tile, Tile> = HashMap::new();
    let mut open = BinaryHeap::new();
    g.insert(start, 0.0);
    open.push(Frontier { est: heuristic(start, goal), tile: start });

    while let Some(Frontier { tile: cur, .. }) = open.pop() {
        if cur == goal {
            return Some(reconstruct(&came, goal));
        }
        let cur_g = g[&cur];
        for (dc, dr) in DIRS {
            let next = (cur.0 + dc, cur.1 + dr);
            if !step_ok(t, cur, next) {
                continue;
            }
            let step = if dc != 0 && dr != 0 { SQRT2 } else { 1.0 };
            let tentative = cur_g + step;
            if tentative < *g.get(&next).unwrap_or(&f32::INFINITY) {
                came.insert(next, cur);
                g.insert(next, tentative);
                open.push(Frontier { est: tentative + heuristic(next, goal), tile: next });
            }
        }
    }
    None
}

fn reconstruct(came: &HashMap<Tile, Tile>, goal: Tile) -> Vec<Tile> {
    let mut path = vec![goal];
    let mut cur = goal;
    while let Some(&prev) = came.get(&cur) {
        path.push(prev);
        cur = prev;
    }
    path.reverse();
    path
}

/// Flood outward from `start` over walkable tiles, returning every tile reachable
/// within `max_cost` mapped to its shortest-path cost (Dijkstra, no goal). Used
/// by tile-seeking movement intents (seek high ground / break line-of-sight) to
/// score a bounded neighbourhood of stand points without an A\* per candidate.
pub fn reachable(t: &Terrain, start: Tile, max_cost: f32) -> HashMap<Tile, f32> {
    let mut dist: HashMap<Tile, f32> = HashMap::new();
    let mut open = BinaryHeap::new();
    if !t.passable(start.0, start.1) {
        return dist;
    }
    dist.insert(start, 0.0);
    open.push(Frontier { est: 0.0, tile: start });

    while let Some(Frontier { tile: cur, est }) = open.pop() {
        // Stale entry (a cheaper path was already settled).
        if est > *dist.get(&cur).unwrap_or(&f32::INFINITY) {
            continue;
        }
        let cur_d = dist[&cur];
        for (dc, dr) in DIRS {
            let next = (cur.0 + dc, cur.1 + dr);
            if !step_ok(t, cur, next) {
                continue;
            }
            let step = if dc != 0 && dr != 0 { SQRT2 } else { 1.0 };
            let nd = cur_d + step;
            if nd <= max_cost && nd < *dist.get(&next).unwrap_or(&f32::INFINITY) {
                dist.insert(next, nd);
                open.push(Frontier { est: nd, tile: next });
            }
        }
    }
    dist
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terrain::Tile3;

    fn wall() -> Tile3 {
        Tile3 { elevation: 4, passable: false }
    }

    #[test]
    fn straight_path_on_open_ground() {
        let t = Terrain::flat(5, 1, 1.0);
        let path = find_path(&t, (0, 0), (4, 0)).unwrap();
        assert_eq!(path, vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]);
    }

    #[test]
    fn same_tile_is_a_singleton_path() {
        let t = Terrain::flat(5, 5, 1.0);
        assert_eq!(find_path(&t, (2, 2), (2, 2)).unwrap(), vec![(2, 2)]);
    }

    #[test]
    fn routes_around_a_wall() {
        // A vertical wall at col 2 spanning rows 0..2, leaving a gap at row 3.
        let mut t = Terrain::flat(5, 4, 1.0);
        for r in 0..3 {
            t.set(2, r, wall());
        }
        let path = find_path(&t, (0, 0), (4, 0)).expect("a path around the wall exists");
        // It must detour through the open row and never step on the wall.
        assert!(path.iter().all(|&(c, r)| !(c == 2 && r < 3)));
        assert_eq!(*path.first().unwrap(), (0, 0));
        assert_eq!(*path.last().unwrap(), (4, 0));
    }

    #[test]
    fn no_path_when_fully_walled_off() {
        // A wall across the whole height seals the right half off.
        let mut t = Terrain::flat(5, 3, 1.0);
        for r in 0..3 {
            t.set(2, r, wall());
        }
        assert_eq!(find_path(&t, (0, 0), (4, 0)), None);
    }

    #[test]
    fn diagonal_cannot_cut_through_a_wall_corner() {
        // Walls at (1,0) and (0,1) form a corner; a diagonal (0,0)->(1,1) would
        // squeeze between them and must be rejected.
        let mut t = Terrain::flat(2, 2, 1.0);
        t.set(1, 0, wall());
        t.set(0, 1, wall());
        // (1,1) is only reachable diagonally, which is blocked -> no path.
        assert_eq!(find_path(&t, (0, 0), (1, 1)), None);
    }

    #[test]
    fn reachable_flood_respects_walls_and_radius() {
        let mut t = Terrain::flat(6, 1, 1.0);
        t.set(3, 0, wall());
        let reach = reachable(&t, (0, 0), 10.0);
        assert!(reach.contains_key(&(2, 0)));
        assert!(!reach.contains_key(&(3, 0))); // the wall
        assert!(!reach.contains_key(&(4, 0))); // sealed off behind it
    }
}
