//! The terrain layer: a grid of tiles the battle plays out on. Units keep
//! *continuous* positions ([`battle::Pos`]) and flowy movement — the grid is the
//! *terrain*, not the unit positions (RTS-style: grid navigation underneath,
//! smooth units on top). See CLAUDE.md "Terrain, height & navigation".
//!
//! Engine-agnostic, like the rest of the core. Two concerns live here:
//! - **walkability** — which tiles a unit may stand on / step between (used by
//!   the A\* navigator in `nav.rs`). A tile is walkable-adjacent if both tiles
//!   are passable and their elevation delta is within [`STEP_HEIGHT`]; a bigger
//!   delta is a **cliff** (impassable to walking).
//! - **line-of-sight** — whether one point can *see* another across the terrain.
//!   Purely elevation-driven: a tile blocks sight if it rises above the straight
//!   line between the two eye points. High ground therefore sees over low walls
//!   and shoots across pits; a low unit behind a tall wall is blocked. LoS is a
//!   new implicit feasibility check for skills (see `eval.rs`).

use crate::battle::Pos;

/// Max elevation delta between two adjacent tiles that a unit can still walk
/// across. A larger rise/drop is a cliff — impassable to walking, but you can
/// still see and shoot across it (that's the point of height).
pub const STEP_HEIGHT: i32 = 1;

/// Height of a unit's eyes above the ground it stands on. Sight lines run
/// between *eye* points, not ground points — without this, a unit on a hill
/// crown is blinded to an adjacent lower unit by the crown's own edge (its
/// ground-level ray clips its own hill). Side effect, accepted as correct:
/// elevation-1 bumps no longer block sight between two ground units —
/// knee-high cover shouldn't blind. Walls (elevation ≥ 3) still block.
pub const EYE_HEIGHT: f32 = 1.0;

/// Column/row index of a tile. Signed so out-of-range neighbours are expressible
/// without underflow during navigation.
pub type Tile = (i32, i32);

/// One terrain cell. `elevation` drives both walkability (cliffs) and
/// line-of-sight (occlusion); `passable` marks walls/pits you can never stand on
/// regardless of height. A wall is modelled as a *tall, impassable* tile (so it
/// blocks LoS); a pit as a *low, impassable* tile (so you see across it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile3 {
    pub elevation: i32,
    pub passable: bool,
}

impl Default for Tile3 {
    fn default() -> Self {
        Tile3 {
            elevation: 0,
            passable: true,
        }
    }
}

/// A tile grid covering the world rectangle `[0, cols*tile_size] × [0,
/// rows*tile_size]`. That rectangle is exactly the playable arena, so units
/// clamped to bounds can never leave the grid.
#[derive(Debug, Clone)]
pub struct Terrain {
    pub cols: i32,
    pub rows: i32,
    /// World units per tile edge (tiles are square).
    pub tile_size: f32,
    /// Row-major, `rows * cols` entries.
    tiles: Vec<Tile3>,
}

impl Terrain {
    /// A featureless arena: every tile passable, elevation 0. Behaves exactly
    /// like the pre-terrain flat field.
    pub fn flat(cols: i32, rows: i32, tile_size: f32) -> Terrain {
        Terrain {
            cols,
            rows,
            tile_size,
            tiles: vec![Tile3::default(); (cols * rows) as usize],
        }
    }

    fn idx(&self, col: i32, row: i32) -> Option<usize> {
        if col < 0 || row < 0 || col >= self.cols || row >= self.rows {
            None
        } else {
            Some((row * self.cols + col) as usize)
        }
    }

    pub fn in_bounds(&self, col: i32, row: i32) -> bool {
        self.idx(col, row).is_some()
    }

    pub fn tile(&self, col: i32, row: i32) -> Option<Tile3> {
        self.idx(col, row).map(|i| self.tiles[i])
    }

    /// Overwrite a tile (map authoring). No-op if out of range.
    pub fn set(&mut self, col: i32, row: i32, tile: Tile3) {
        if let Some(i) = self.idx(col, row) {
            self.tiles[i] = tile;
        }
    }

    /// Paint a rectangular block of tiles (inclusive ranges), clamped to grid.
    pub fn fill(&mut self, cols: std::ops::RangeInclusive<i32>, rows: std::ops::RangeInclusive<i32>, tile: Tile3) {
        for r in rows {
            for c in cols.clone() {
                self.set(c, r, tile);
            }
        }
    }

    /// The world-space size of the whole grid, `(width, height)`.
    pub fn world_extent(&self) -> (f32, f32) {
        (self.cols as f32 * self.tile_size, self.rows as f32 * self.tile_size)
    }

    /// The tile a world point falls in. May be out of range (negative or past
    /// the far edge) — callers standing inside the arena always get a valid one.
    pub fn tile_of(&self, p: Pos) -> Tile {
        (
            (p.x / self.tile_size).floor() as i32,
            (p.y / self.tile_size).floor() as i32,
        )
    }

    /// The world position of a tile's centre.
    pub fn tile_center(&self, (col, row): Tile) -> Pos {
        Pos {
            x: (col as f32 + 0.5) * self.tile_size,
            y: (row as f32 + 0.5) * self.tile_size,
        }
    }

    pub fn passable(&self, col: i32, row: i32) -> bool {
        self.tile(col, row).is_some_and(|t| t.passable)
    }

    /// Whether a world point sits on a passable tile.
    pub fn passable_at(&self, p: Pos) -> bool {
        let (c, r) = self.tile_of(p);
        self.passable(c, r)
    }

    /// Elevation of the tile a world point falls in. Out-of-range points clamp to
    /// the nearest edge tile (so a body brushing the boundary still reads a real
    /// height rather than a hole).
    pub fn elevation_at(&self, p: Pos) -> i32 {
        let (c, r) = self.tile_of(p);
        let c = c.clamp(0, self.cols - 1);
        let r = r.clamp(0, self.rows - 1);
        self.tile(c, r).map_or(0, |t| t.elevation)
    }

    pub fn elevation(&self, (col, row): Tile) -> i32 {
        self.tile(col, row).map_or(0, |t| t.elevation)
    }

    /// Can a unit step directly between two (assumed adjacent) tiles? Both must
    /// be passable and within one [`STEP_HEIGHT`] of each other. A tile is always
    /// "walkable to itself". Diagonal corner-cutting is handled by the navigator,
    /// which has both flanking tiles.
    pub fn walkable(&self, from: Tile, to: Tile) -> bool {
        if from == to {
            return self.passable(from.0, from.1);
        }
        let (Some(a), Some(b)) = (self.tile(from.0, from.1), self.tile(to.0, to.1)) else {
            return false;
        };
        a.passable && b.passable && (a.elevation - b.elevation).abs() <= STEP_HEIGHT
    }

    /// Line-of-sight between two world points. Blocked when an intervening tile
    /// rises above the straight sight line drawn between the two eye heights
    /// (each eye sits [`EYE_HEIGHT`] above its tile's elevation). Occlusion is
    /// elevation-only — passability is about walking, not seeing — so you shoot
    /// across pits and over lower cover, but a tall wall between two low units
    /// blocks the shot.
    pub fn line_of_sight(&self, a: Pos, b: Pos) -> bool {
        let dist = a.dist(b);
        if dist <= f32::EPSILON {
            return true;
        }
        let ea = self.elevation_at(a) as f32 + EYE_HEIGHT;
        let eb = self.elevation_at(b) as f32 + EYE_HEIGHT;
        let a_tile = self.tile_of(a);
        let b_tile = self.tile_of(b);

        // Sample finely enough to catch every tile the segment crosses.
        let steps = (dist / (self.tile_size * 0.25)).ceil().max(1.0) as i32;
        for i in 1..steps {
            let t = i as f32 / steps as f32;
            let p = Pos {
                x: a.x + (b.x - a.x) * t,
                y: a.y + (b.y - a.y) * t,
            };
            let tile = self.tile_of(p);
            // The endpoints' own tiles never occlude (you stand on them).
            if tile == a_tile || tile == b_tile {
                continue;
            }
            let sight = ea + (eb - ea) * t; // interpolated height of the sight line
            if self.elevation(tile) as f32 > sight {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wall() -> Tile3 {
        Tile3 { elevation: 4, passable: false }
    }

    #[test]
    fn tile_of_and_center_roundtrip() {
        let t = Terrain::flat(10, 6, 2.0);
        assert_eq!(t.tile_of(Pos { x: 5.0, y: 3.0 }), (2, 1)); // 5/2=2.5→2, 3/2=1.5→1
        // Centre of tile (2,1) is at (2.5, 1.5)*2 = (5,3).
        let c = t.tile_center((2, 1));
        assert_eq!((c.x, c.y), (5.0, 3.0));
        assert_eq!(t.world_extent(), (20.0, 12.0));
    }

    #[test]
    fn out_of_range_is_impassable_but_readable() {
        let t = Terrain::flat(4, 4, 1.0);
        assert!(!t.passable(-1, 0));
        assert!(!t.passable(4, 0));
        // Elevation of an off-grid point clamps to the nearest edge tile.
        assert_eq!(t.elevation_at(Pos { x: 99.0, y: 99.0 }), 0);
    }

    #[test]
    fn cliff_blocks_walking_between_tiles() {
        let mut t = Terrain::flat(4, 1, 1.0);
        t.set(1, 0, Tile3 { elevation: 0, passable: true });
        t.set(2, 0, Tile3 { elevation: 3, passable: true }); // 3-high step = cliff
        assert!(t.walkable((0, 0), (1, 0))); // flat, fine
        assert!(!t.walkable((1, 0), (2, 0))); // delta 3 > STEP_HEIGHT
        // A gentle 1-high step is still walkable.
        t.set(2, 0, Tile3 { elevation: 1, passable: true });
        assert!(t.walkable((1, 0), (2, 0)));
    }

    #[test]
    fn wall_is_never_walkable() {
        let mut t = Terrain::flat(4, 1, 1.0);
        t.set(2, 0, wall());
        assert!(!t.walkable((1, 0), (2, 0)));
        assert!(!t.passable(2, 0));
    }

    #[test]
    fn tall_wall_blocks_line_of_sight_between_low_units() {
        let mut t = Terrain::flat(5, 1, 1.0);
        t.set(2, 0, wall()); // elevation 4 between two elevation-0 units
        let a = Pos { x: 0.5, y: 0.5 };
        let b = Pos { x: 4.5, y: 0.5 };
        assert!(!t.line_of_sight(a, b));
    }

    #[test]
    fn high_ground_sees_over_a_low_wall() {
        let mut t = Terrain::flat(5, 1, 1.0);
        // A short wall (elevation 2) between two units standing on elevation-3
        // hills: the sight line runs at height 3, clearing the wall.
        t.set(0, 0, Tile3 { elevation: 3, passable: true });
        t.set(4, 0, Tile3 { elevation: 3, passable: true });
        t.set(2, 0, Tile3 { elevation: 2, passable: true });
        let a = Pos { x: 0.5, y: 0.5 };
        let b = Pos { x: 4.5, y: 0.5 };
        assert!(t.line_of_sight(a, b));
    }

    /// Regression (the Shaman/Brawler livelock): a unit standing on a hill crown
    /// must see a unit at the hill's base — its *own* high ground must not blind
    /// it. Eye height lifts the sight line clear of the crown's edge.
    #[test]
    fn crown_does_not_blind_you_to_a_unit_at_its_base() {
        let mut t = Terrain::flat(5, 1, 1.0);
        // Crown: elevation-2 tiles at cols 0..=1; flat ground beyond.
        t.set(0, 0, Tile3 { elevation: 2, passable: true });
        t.set(1, 0, Tile3 { elevation: 2, passable: true });
        let on_crown = Pos { x: 0.5, y: 0.5 };
        let at_base = Pos { x: 3.5, y: 0.5 };
        assert!(t.line_of_sight(on_crown, at_base), "crown unit should see the base");
        assert!(t.line_of_sight(at_base, on_crown), "and vice versa");
    }

    #[test]
    fn sight_crosses_a_pit_freely() {
        let mut t = Terrain::flat(5, 1, 1.0);
        // A pit: low and impassable. You can't walk it but you can shoot across.
        t.set(2, 0, Tile3 { elevation: -2, passable: false });
        let a = Pos { x: 0.5, y: 0.5 };
        let b = Pos { x: 4.5, y: 0.5 };
        assert!(t.line_of_sight(a, b));
    }
}
