//! The battle world state that gambit rules are evaluated against.
//!
//! This is intentionally engine-agnostic (no Macroquad types) so the gambit
//! core can be unit-tested in isolation before rendering is wired up.

use std::collections::HashMap;

use crate::terrain::Terrain;

/// Every entity is a circle of this radius (world units). Uniform for now — a
/// single knob keeps movement/collision simple; if size ever needs to vary it
/// should come from equipment, not a per-entity field (see CLAUDE.md).
pub const ENTITY_RADIUS: f32 = 0.5;

/// Fractional movement-speed reduction inflicted by a `Snare` status (0.6 == a
/// 60% slow). A single knob — snare magnitude isn't stored per-status, so all
/// snares slow by the same amount regardless of stacks (see
/// [`Entity::effective_move_speed`]).
pub const SNARE_SLOW: f32 = 0.6;

/// Index of an entity within [`BattleState::entities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntityId(pub usize);

/// Index of a skill within [`BattleState::skills`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SkillId(pub usize);

/// Which side an entity fights on. Pools are resolved relative to the *actor*,
/// so "enemies" means "entities on the other team".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Team {
    Player,
    Enemy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageType {
    Physical,
    Fire,
    Ice,
    Lightning,
    Poison,
    Holy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Poison,
    Burn,
    Regen,
    Shield,
    Enrage,
    Silence,
    /// Hard crowd-control: the victim can neither act nor move while it lasts
    /// (its action bar is frozen too). The counter that charges/gap-closers
    /// buy with a hit.
    Stun,
    /// A movement slow: cuts the victim's `move_speed` by [`SNARE_SLOW`] while it
    /// lasts. It can still act — only its drift is throttled (the anti-kite tool).
    Snare,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Status {
    pub kind: StatusKind,
    pub stacks: u32,
    /// Remaining ticks before the status expires.
    pub duration: u32,
}

/// What a skill does when it resolves against each of its targets.
#[derive(Debug, Clone)]
pub enum Effect {
    /// Raw damage; multiplied if the target is weak to the skill's `damage_type`.
    Damage(f32),
    /// Restore HP (capped at `max_hp`).
    Heal(f32),
    /// Apply (or stack) a status on the target.
    Inflict {
        kind: StatusKind,
        stacks: u32,
        duration: u32,
    },
    /// Charge / gap-closer: the *actor* dashes toward its primary target, closing
    /// to melee contact but travelling at most `max` world units. Unlike the other
    /// effects (which act on each target), this repositions the actor and resolves
    /// once, before the per-target effects land — so the damage/status that share
    /// the skill are dealt from contact.
    Dash { max: f32 },
}

/// A simple 2D position. We avoid `macroquad::Vec2` here so this module has no
/// engine dependency; a conversion can live at the rendering boundary later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pos {
    pub x: f32,
    pub y: f32,
}

impl Pos {
    pub fn dist(self, other: Pos) -> f32 {
        let dx = self.x - other.x;
        let dy = self.y - other.y;
        (dx * dx + dy * dy).sqrt()
    }
}

/// A usable skill. Feasibility (cooldown / cost / range / valid target) is
/// checked by the evaluator using these fields — players never hand-author it.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    /// Resource cost, checked against the actor's `mp`.
    pub cost: u32,
    /// Max distance from actor to a valid target. Acts as an *implicit* filter
    /// on the target set when this skill is considered.
    pub range: f32,
    /// Ticks of cooldown incurred after use.
    pub cooldown: u32,
    /// Ticks the actor is rooted (Casting) before this skill resolves. `0` means
    /// it resolves the instant it's chosen; `> 0` opens a vulnerability window —
    /// the actor stands still and its ATB stops filling until the cast completes.
    pub cast_time: u32,
    /// Element used for weakness checks, if any.
    pub damage_type: Option<DamageType>,
    /// What resolving this skill does to each target.
    pub effects: Vec<Effect>,
}

#[derive(Debug, Clone)]
pub struct Entity {
    pub id: EntityId,
    pub name: String,
    pub team: Team,
    pub hp: f32,
    pub max_hp: f32,
    pub mp: u32,
    pub pos: Pos,
    pub statuses: Vec<Status>,
    pub weaknesses: Vec<DamageType>,
    /// Skills this entity knows.
    pub skills: Vec<SkillId>,
    /// Remaining cooldown per skill; absent == ready.
    pub cooldowns: HashMap<SkillId, u32>,
    /// How fast the action bar fills per tick (ATB rate).
    pub atb_speed: f32,
    /// World units the entity can drift per tick when moving. Independent of
    /// `atb_speed`: a unit moves *and* fills its bar every tick — never one or
    /// the other. `0.0` == stationary (the pre-movement behaviour).
    pub move_speed: f32,
    /// Action bar in `0.0..=1.0`; the entity acts when it reaches 1.0.
    pub action_bar: f32,
}

impl Entity {
    pub fn is_alive(&self) -> bool {
        self.hp > 0.0
    }

    /// Health as a fraction in `0.0..=1.0`.
    pub fn hp_pct(&self) -> f32 {
        if self.max_hp <= 0.0 {
            0.0
        } else {
            self.hp / self.max_hp
        }
    }

    pub fn status(&self, kind: StatusKind) -> Option<&Status> {
        self.statuses.iter().find(|s| s.kind == kind)
    }

    pub fn status_stacks(&self, kind: StatusKind) -> u32 {
        self.status(kind).map_or(0, |s| s.stacks)
    }

    pub fn cooldown_remaining(&self, skill: SkillId) -> u32 {
        self.cooldowns.get(&skill).copied().unwrap_or(0)
    }

    /// Whether the entity is stunned (can't act or move; ATB frozen).
    pub fn is_stunned(&self) -> bool {
        self.status(StatusKind::Stun).is_some()
    }

    /// Whether the entity is snared (drift slowed by [`SNARE_SLOW`]).
    pub fn is_snared(&self) -> bool {
        self.status(StatusKind::Snare).is_some()
    }

    /// Move speed after status modifiers — the distance the entity may actually
    /// drift this tick. A `Snare` cuts it by [`SNARE_SLOW`]; otherwise it's the
    /// raw `move_speed`.
    pub fn effective_move_speed(&self) -> f32 {
        if self.is_snared() {
            self.move_speed * (1.0 - SNARE_SLOW)
        } else {
            self.move_speed
        }
    }
}

/// The whole battlefield: every entity plus the shared skill registry.
#[derive(Debug, Clone)]
pub struct BattleState {
    pub entities: Vec<Entity>,
    pub skills: Vec<Skill>,
    /// Playable arena size in world units, `(width, height)`. Movement is
    /// clamped to `0..=width` × `0..=height` so drifting units can't leave the
    /// field. When `terrain` is present this equals its `world_extent`.
    pub bounds: (f32, f32),
    /// The tile grid the fight plays out on: obstacles, elevation, cliffs, and
    /// the line-of-sight/pathfinding they imply. `None` == a featureless flat
    /// arena (the pre-terrain behaviour): everything passable, elevation 0, and
    /// unobstructed sight everywhere. The queries below hide that fallback so the
    /// rest of the engine never branches on it.
    pub terrain: Option<Terrain>,
}

impl BattleState {
    pub fn entity(&self, id: EntityId) -> &Entity {
        &self.entities[id.0]
    }

    /// Clamp a position to the arena bounds.
    pub fn clamp_pos(&self, p: Pos) -> Pos {
        self.clamp_within(p, 0.0)
    }

    /// Clamp a *circle's* centre so the whole circle of the given `radius` stays
    /// inside the arena — the body can't hang over the edge. Degrades gracefully
    /// (to the centre line) if the arena is narrower than the circle.
    pub fn clamp_within(&self, p: Pos, radius: f32) -> Pos {
        Pos {
            x: p.x.clamp(radius, (self.bounds.0 - radius).max(radius)),
            y: p.y.clamp(radius, (self.bounds.1 - radius).max(radius)),
        }
    }

    pub fn skill(&self, id: SkillId) -> &Skill {
        &self.skills[id.0]
    }

    // --- terrain queries (flat-arena fallbacks when there is no terrain) ---

    /// Ground height at a world point (0 on a flat arena).
    pub fn elevation_at(&self, p: Pos) -> i32 {
        self.terrain.as_ref().map_or(0, |t| t.elevation_at(p))
    }

    /// Whether `a` can see `b` across the terrain (always true when flat). This
    /// is the implicit line-of-sight feasibility check for skills.
    pub fn line_of_sight(&self, a: Pos, b: Pos) -> bool {
        self.terrain.as_ref().is_none_or(|t| t.line_of_sight(a, b))
    }

    /// Whether a unit may stand on the tile under a world point (always true when
    /// flat).
    pub fn passable_at(&self, p: Pos) -> bool {
        self.terrain.as_ref().is_none_or(|t| t.passable_at(p))
    }

    /// All *living* entity ids, in stable order.
    pub fn living(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.entities
            .iter()
            .filter(|e| e.is_alive())
            .map(|e| e.id)
    }
}
