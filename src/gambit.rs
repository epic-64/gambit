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
// Movement rules: a separate, lightweight gambit that runs every tick,
// decoupled from the action gambit above. It reuses the same target engine —
// a movement intent is just "pick something, then move relative to it".
// ---------------------------------------------------------------------------

/// Where a movement rule wants the actor to go, expressed relative to a target
/// the same `TargetQuery` machinery selects. "Kite the nearest enemy" is
/// `MoveAway(nearest enemy)`; "close on the weakest foe" is `MoveToward(...)`.
#[derive(Debug, Clone)]
pub enum MoveIntent {
    /// Drift toward the selected target (stops on arrival — never overshoots).
    /// Routes around terrain obstacles via A\*.
    Toward(TargetQuery),
    /// Drift directly away from the selected target, sliding along walls rather
    /// than jamming into them.
    Away(TargetQuery),
    /// Seek the highest reachable tile that still has line-of-sight to the
    /// selected target — "get to the high ground and keep the shot". A terrain
    /// intent: on a flat arena there is no high ground, so it holds. (See
    /// CLAUDE.md — the payoff that makes terrain worth its cost.)
    SeekHighGround(TargetQuery),
    /// Move to the nearest reachable tile that *breaks* line-of-sight to the
    /// selected threat — duck behind cover. Holds on a flat arena.
    BreakLoS(TargetQuery),
}

impl MoveIntent {
    /// The query this intent selects its reference entity from.
    pub fn query(&self) -> &TargetQuery {
        match self {
            MoveIntent::Toward(q)
            | MoveIntent::Away(q)
            | MoveIntent::SeekHighGround(q)
            | MoveIntent::BreakLoS(q) => q,
        }
    }

    /// True for intents that close on their reference (`Toward`), false for the
    /// ones that retreat from it (`Away`). The tile-seeking intents resolve to a
    /// tile goal rather than a straight toward/away drift, so they are handled
    /// explicitly by the movement evaluator and don't use this.
    pub fn is_toward(&self) -> bool {
        matches!(self, MoveIntent::Toward(_))
    }
}

/// One movement rule: a guard plus an intent. A movement gambit is an ordered
/// list of these; the first rule whose condition holds *and* whose intent
/// resolves to a reference target decides the drift for that tick. If none do,
/// the actor holds position.
#[derive(Debug, Clone)]
pub struct MoveRule {
    pub condition: Condition,
    pub intent: MoveIntent,
}

impl MoveRule {
    /// An unconditional movement rule.
    pub fn new(intent: MoveIntent) -> MoveRule {
        MoveRule {
            condition: Condition::Always,
            intent,
        }
    }

    /// Attach a guard condition (builder-style).
    pub fn when(mut self, condition: Condition) -> MoveRule {
        self.condition = condition;
        self
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
