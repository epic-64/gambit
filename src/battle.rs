//! The battle world state that gambit rules are evaluated against.
//!
//! This is intentionally engine-agnostic (no Macroquad types) so the gambit
//! core can be unit-tested in isolation before rendering is wired up.

use std::collections::HashMap;

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
    pub speed: f32,
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
}

/// The whole battlefield: every entity plus the shared skill registry.
#[derive(Debug, Clone)]
pub struct BattleState {
    pub entities: Vec<Entity>,
    pub skills: Vec<Skill>,
}

impl BattleState {
    pub fn entity(&self, id: EntityId) -> &Entity {
        &self.entities[id.0]
    }

    pub fn skill(&self, id: SkillId) -> &Skill {
        &self.skills[id.0]
    }

    /// All *living* entity ids, in stable order.
    pub fn living(&self) -> impl Iterator<Item = EntityId> + '_ {
        self.entities
            .iter()
            .filter(|e| e.is_alive())
            .map(|e| e.id)
    }
}
