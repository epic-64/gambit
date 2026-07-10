//! The gambit rule model: a behaviour tree of `Node`s that a character walks
//! when its action bar fills.
//!
//! Structure (see CLAUDE.md for the full design):
//! - A **rule** is split into Condition / Target / Skill (not FF12's fused
//!   condition+target), so the thing you *test* can differ from what you *hit*.
//! - **Target** selection is modular: Pool -> Filters -> Sort -> Pick.
//! - A **Condition** is just a Target query wrapped in a quantifier
//!   (Exists / Count), plus And/Or/Not combinators.
//! - Rules live in a **tree**: a `Group` node is a context/guard that scopes
//!   its children.

use crate::battle::{DamageType, SkillId, StatusKind};

// ---------------------------------------------------------------------------
// Target selection: Pool -> Filters -> Sort -> Pick
// ---------------------------------------------------------------------------

/// The candidate set a query starts from, resolved *relative to the actor*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pool {
    /// Entities on the opposing team.
    Enemies,
    /// Entities on the actor's own team (includes the actor).
    Allies,
    /// The acting entity only.
    Myself,
    /// Every entity, regardless of team.
    Everyone,
    /// Reuse the entities matched by *this node's condition*. This is the
    /// FF12 ergonomic case ("the enemy the condition found is the one I hit"),
    /// expressed without rewriting the filter.
    Matched,
}

/// A predicate on a single candidate entity. Multiple filters on one query are
/// AND-ed and all apply to the *same* entity — this is the correct home for
/// "below 50% HP *and* weak to poison".
#[derive(Debug, Clone)]
pub enum Filter {
    HpPctBelow(f32),
    HpPctAbove(f32),
    HpBelow(f32),
    HasStatus(StatusKind),
    StatusStacksAtLeast(StatusKind, u32),
    WeakTo(DamageType),
    IsSelf,
    NotSelf,
    /// The candidate is visible to the actor across the terrain. Redundant for a
    /// skill's own targets (line-of-sight is already an implicit feasibility
    /// check) but useful in *conditions* — e.g. "flee if an enemy that can see me
    /// exists". Always true on a flat, terrain-free arena.
    HasLineOfSight,
    /// The candidate stands on higher ground than the actor. Never true when
    /// flat. Its negation (`Not(OnHigherGround)`) covers "same-or-lower ground".
    OnHigherGround,
    /// The candidate is within `d` world-units of the actor. The distance guard
    /// that makes a *bounded* kite expressible: `MoveAway` a foe filtered by
    /// `WithinDistance` retreats only while the threat is close, then stops once
    /// the gap is open (instead of fleeing to a corner). `Not(WithinDistance)`
    /// covers "at least this far away".
    WithinDistance(f32),
    /// The candidate is within `d` world-units of some *other* entity the
    /// nested query selects (the query is actor-relative, like every query;
    /// the candidate itself never counts as its own reference). This is the
    /// relational filter the actor-relative `WithinDistance` can't express:
    /// "an enemy engaging one of my teammates" is
    /// `Pool::Enemies` + `WithinDistanceOf(allies-not-me, melee reach)` — the
    /// protect/peel trigger. Give the nested query `Pick::All` to mean "near
    /// *any* of them"; a narrower pick ("near the weakest ally") also works.
    WithinDistanceOf(Box<TargetQuery>, f32),
    Not(Box<Filter>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Hp,
    HpPct,
    MaxHp,
    /// Distance from the actor.
    Distance,
    /// Ground elevation the candidate stands on. `Order::Desc` prefers the
    /// high-ground target. Flat everywhere on a terrain-free arena.
    Elevation,
    StatusStacks(StatusKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    Asc,
    Desc,
}

/// How many of the (filtered, sorted) candidates to actually act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// The single best candidate after sorting.
    First,
    /// Up to `n` best candidates.
    Take(u32),
    /// Every candidate (AoE).
    All,
    /// One candidate chosen deterministically from the set.
    Random,
}

/// A full target query: where to look, how to narrow, how to order, how many.
#[derive(Debug, Clone)]
pub struct TargetQuery {
    pub pool: Pool,
    pub filters: Vec<Filter>,
    pub sort: Option<(SortKey, Order)>,
    pub pick: Pick,
}

impl TargetQuery {
    /// A bare query over a pool with no filtering, taking the first entity.
    pub fn new(pool: Pool) -> Self {
        TargetQuery {
            pool,
            filters: Vec::new(),
            sort: None,
            pick: Pick::First,
        }
    }

    pub fn filter(mut self, f: Filter) -> Self {
        self.filters.push(f);
        self
    }

    pub fn sort(mut self, key: SortKey, order: Order) -> Self {
        self.sort = Some((key, order));
        self
    }

    pub fn pick(mut self, pick: Pick) -> Self {
        self.pick = pick;
        self
    }
}

// ---------------------------------------------------------------------------
// Conditions: a target query wrapped in a quantifier, plus combinators
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Cmp {
    pub fn test(self, lhs: u32, rhs: u32) -> bool {
        match self {
            Cmp::Lt => lhs < rhs,
            Cmp::Le => lhs <= rhs,
            Cmp::Eq => lhs == rhs,
            Cmp::Ge => lhs >= rhs,
            Cmp::Gt => lhs > rhs,
        }
    }
}

/// The boolean guard on a node. Combinators (`All`/`Any`) are for conditions
/// about *different* subjects; predicates on the *same* entity belong in a
/// query's `filters` instead.
#[derive(Debug, Clone)]
pub enum Condition {
    /// Always fires.
    Always,
    /// At least one entity matches the query.
    Exists(TargetQuery),
    /// The match count compares against `n`.
    Count {
        q: TargetQuery,
        cmp: Cmp,
        n: u32,
    },
    Not(Box<Condition>),
    /// Logical AND (empty == true).
    All(Vec<Condition>),
    /// Logical OR (empty == false).
    Any(Vec<Condition>),
}

// ---------------------------------------------------------------------------
// The tree: nodes are either a leaf action or a context grouping children
// ---------------------------------------------------------------------------

/// What a context does when its guard passes but no child yields an action
/// (e.g. the only relevant skill is on cooldown).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupMode {
    /// Keep searching siblings / outer rules. The default — a gambit list
    /// should almost always produce *some* action.
    Fallthrough,
    /// Stay in this context and wait rather than falling out. Lets you say
    /// "if hurt, only ever consider defensive skills; else do nothing".
    Commit,
}

#[derive(Debug, Clone)]
pub enum Body {
    /// A leaf: use `skill` on whatever `target` selects.
    Act { target: TargetQuery, skill: SkillId },
    /// A context: an ordered list of child nodes.
    Group {
        mode: GroupMode,
        children: Vec<Node>,
    },
}

/// One node of the gambit tree: a guard plus a body.
#[derive(Debug, Clone)]
pub struct Node {
    pub condition: Condition,
    pub body: Body,
}

// ---------------------------------------------------------------------------
// Movement: a separate, lightweight gambit that runs every tick, decoupled
// from the action gambit above. Movement is a *continuous* optimization, so —
// unlike actions — it is NOT a priority rule list: threshold rules over
// continuous space flip at the boundary and oscillate (bang-bang control; see
// PLAN.md). Instead a movement gambit is a **weighted sum of scoring terms**
// evaluated over candidate stand points; conflicting pulls blend into one
// best spot instead of alternating rule wins. Terms reuse the same
// `TargetQuery` engine to pick what to position relative to.
// ---------------------------------------------------------------------------

/// One positional scoring term: maps a candidate stand point to a score, given
/// a reference the `TargetQuery` selects. All terms are smooth (or damped by
/// the evaluator's stickiness), so the argmax moves gradually — no wobble.
#[derive(Debug, Clone)]
pub enum Term {
    /// Peak score at exactly `ideal` world-units from the selected target,
    /// degrading linearly with |distance − ideal|. One term expresses
    /// approach ("get in range"), standoff ("hold at range") *and* retreat
    /// ("it dove on me") — `ideal = 0.0` is pure melee pursuit.
    Near(TargetQuery, f32),
    /// Farther from the selected target is better, saturating at the
    /// evaluator's away-range — a *bounded* flee, not a run-to-the-corner.
    AwayFrom(TargetQuery),
    /// Higher ground scores better. Flat everywhere on a terrain-free arena.
    HighGround,
    /// Stand points with line-of-sight to the selected target score full
    /// weight; blind ones score zero. A *negative* weight turns this into
    /// "hide from the target" (break line-of-sight) for free.
    SightOf(TargetQuery),
}

impl Term {
    /// The query this term selects its reference entity from, if it has one.
    pub fn query(&self) -> Option<&TargetQuery> {
        match self {
            Term::Near(q, _) | Term::AwayFrom(q) | Term::SightOf(q) => Some(q),
            Term::HighGround => None,
        }
    }
}

/// A movement gambit: weighted scoring terms summed per candidate stand point.
/// The evaluator (`eval::decide_move`) walks the reachable neighbourhood, adds
/// a stickiness bonus to the current position (moving must *beat* standing
/// still), and steers toward the argmax. An empty gambit — or one whose every
/// query matches nothing — holds position.
#[derive(Debug, Clone)]
pub struct MoveGambit {
    pub terms: Vec<(Term, f32)>,
}

impl MoveGambit {
    pub fn new(terms: Vec<(Term, f32)>) -> MoveGambit {
        MoveGambit { terms }
    }

    /// The common case: pure pursuit of the selected target (melee closing).
    pub fn toward(q: TargetQuery) -> MoveGambit {
        MoveGambit::new(vec![(Term::Near(q, 0.0), 1.0)])
    }
}

impl Node {
    /// A leaf action node with no guard of its own.
    pub fn act(target: TargetQuery, skill: SkillId) -> Node {
        Node {
            condition: Condition::Always,
            body: Body::Act { target, skill },
        }
    }

    /// A context node guarding an ordered list of children.
    pub fn context(condition: Condition, mode: GroupMode, children: Vec<Node>) -> Node {
        Node {
            condition,
            body: Body::Group { mode, children },
        }
    }

    /// Attach/replace this node's guard condition (builder-style).
    pub fn when(mut self, condition: Condition) -> Node {
        self.condition = condition;
        self
    }
}
