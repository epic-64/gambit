//! Evaluation: given a gambit tree and an actor, decide what the actor does
//! when its action bar fills.
//!
//! The walk is depth-first. Each node's guard is checked; on success an `Act`
//! leaf tries to produce a feasible action and a `Group` recurses into its
//! children. Feasibility (cooldown / cost / range / has-a-valid-target) is
//! checked here implicitly — it is never hand-authored in a rule.

use std::cmp::Ordering;

use crate::battle::{BattleState, EntityId, Pos, SkillId, ENTITY_RADIUS};
use crate::gambit::{
    Body, Condition, Filter, GroupMode, MoveGambit, Node, Order, Pick, Pool, SortKey,
    TargetQuery, Term,
};
use crate::nav;
use crate::terrain::Tile;

/// The action a character commits to for this turn.
#[derive(Debug, Clone, PartialEq)]
pub struct Action {
    pub skill: SkillId,
    /// One or more targets (more than one for AoE picks).
    pub targets: Vec<EntityId>,
}

/// Result of evaluating a subtree.
enum Outcome {
    /// A feasible action was found; stop and use it.
    Act(Action),
    /// Nothing here — continue to siblings / outer rules.
    Fall,
    /// A `Commit` context deliberately chose to do nothing this tick.
    Wait,
}

/// Walk `root` for `actor` and decide on an action.
///
/// Returns `None` when the tree yields no action — either because nothing
/// matched (fallthrough ran off the end) or because a `Commit` context chose
/// to wait. Both mean "the actor takes no action this tick".
pub fn decide(root: &Node, actor: EntityId, state: &BattleState) -> Option<Action> {
    match eval_node(root, actor, state) {
        Outcome::Act(a) => Some(a),
        Outcome::Fall | Outcome::Wait => None,
    }
}

// --- movement scoring knobs -------------------------------------------------

/// Candidate-search radius (A\* path cost ≈ tiles) around the actor. Bounds the
/// per-tick neighbourhood a mover considers standing in.
const SEEK_RADIUS: f32 = 8.0;
/// World units of |distance − ideal| per point of [`Term::Near`] score. Larger
/// = gentler distance gradient relative to the other terms.
const DIST_NORM: f32 = 4.0;
/// Distance at which [`Term::AwayFrom`] saturates: beyond it, farther isn't
/// better — the bounded flee that replaces the old run-to-the-corner kite.
const AWAY_RANGE: f32 = 8.0;
/// Score per elevation step for [`Term::HighGround`].
const ELEV_SCORE: f32 = 0.25;
/// Bonus for the actor's *current* position: a move must beat standing still
/// by this margin. The damping that kills dithering between near-equal spots —
/// the stateless replacement for hysteresis.
const STICKINESS: f32 = 0.25;
/// Score penalty per world-unit of distance to a candidate: moving isn't free,
/// so near-equal spots resolve to the *nearest* one. Without it, ties on (say)
/// an ideal-range ring resolve by scan order and the mover orbits its target
/// instead of backing straight out. Kept well below the terms' gradients so it
/// breaks ties rather than fighting real preferences.
const TRAVEL_COST: f32 = 0.02;

/// Whether a movement term pulls the mover *toward* its reference point or
/// pushes it *away* — the distinction the viewer paints intent lines by. A
/// negative weight flips a term's natural pull (e.g. negative `SightOf` =
/// hide from it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    Toward,
    Away,
}

/// One movement decision with the *why* kept: where the mover is ultimately
/// heading and which reference points it is steering relative to. The sim
/// only consumes `step` (via [`decide_move`]); the rest exists for the
/// viewer's intent lines.
#[derive(Debug, Clone, PartialEq)]
pub struct MoveIntent {
    /// The chosen stand point (the scoring argmax) — where the mover is heading.
    pub goal: Pos,
    /// This tick's one-`move_speed` step toward the goal (A\*-routed).
    pub step: Pos,
    /// Each active term's reference point, tagged by pull and deduplicated
    /// (e.g. `Near` + `SightOf` of the same target is one entry). Empty when
    /// only reference-free terms (`HighGround`) are active.
    pub refs: Vec<(Pos, Pull)>,
}

/// Decide where `actor` drifts this tick from its movement gambit, independent
/// of the action gambit. Scores every reachable candidate stand point as the
/// gambit's weighted term sum and steers one `move_speed` step toward the
/// argmax (A\*-routed around terrain).
///
/// Returns the actor's *new* position, or `None` to hold — when standing still
/// (plus [`STICKINESS`]) already beats every alternative, or when no term's
/// query matched anything to position against. Pure — mutation is the
/// caller's job.
pub fn decide_move(gambit: &MoveGambit, actor: EntityId, state: &BattleState) -> Option<Pos> {
    move_intent(gambit, actor, state, 1.0).map(|i| i.step)
}

/// [`decide_move`] with the intent kept — goal, step and term references —
/// and the step scaled to the tick-fraction `dt` that is elapsing (so the
/// combat loop can integrate movement continuously). Returns `None` exactly
/// when `decide_move` does: holding is "no intent".
pub fn move_intent(
    gambit: &MoveGambit,
    actor: EntityId,
    state: &BattleState,
    dt: f32,
) -> Option<MoveIntent> {
    // Resolve each term's reference point once — references are actor-relative
    // (the query engine), not candidate-relative. Movement stays rangeless &
    // sightless: the whole point is to *reach* a position, so references are
    // never pre-filtered by range or LoS. Terms whose query matches nothing
    // drop out; if everything drops, there is nothing to position against.
    let active: Vec<(&Term, f32, Option<Pos>)> = gambit
        .terms
        .iter()
        .filter_map(|(term, weight)| match term.query() {
            Some(q) => {
                let refs = select(q, actor, state, &[], None);
                centroid(&refs, state).map(|point| (term, *weight, Some(point)))
            }
            None => Some((term, *weight, None)),
        })
        .collect();
    if active.is_empty() {
        return None;
    }

    let score = |p: Pos| -> f32 {
        let mut total = 0.0;
        for (term, w, reference) in &active {
            let s = match term {
                Term::Near(_, ideal) => {
                    let Some(r) = reference else { continue };
                    -((p.dist(*r) - ideal).abs() / DIST_NORM)
                }
                Term::AwayFrom(_) => {
                    let Some(r) = reference else { continue };
                    (p.dist(*r) / AWAY_RANGE).min(1.0)
                }
                Term::HighGround => state.elevation_at(p) as f32 * ELEV_SCORE,
                Term::SightOf(_) => {
                    let Some(r) = reference else { continue };
                    if state.line_of_sight(p, *r) { 1.0 } else { 0.0 }
                }
            };
            total += w * s;
        }
        total
    };

    // Argmax over the candidates, with standing still as the incumbent (its
    // stickiness bonus means a move must clearly beat holding). Candidates pay
    // travel cost, so near-equal spots resolve to the nearest; residual ties
    // keep the earlier candidate — the walk is deterministic.
    let from = state.entity(actor).pos;
    let mut best_p = from;
    let mut best = score(from) + STICKINESS;
    for p in candidate_points(from, state) {
        let s = score(p) - from.dist(p) * TRAVEL_COST;
        if s > best {
            best = s;
            best_p = p;
        }
    }
    if best_p == from {
        return None; // standing still is (still) the best option
    }

    let mut refs: Vec<(Pos, Pull)> = Vec::new();
    for (term, w, reference) in &active {
        let Some(r) = reference else { continue };
        let toward = match term {
            Term::AwayFrom(_) => *w < 0.0,
            _ => *w >= 0.0,
        };
        let entry = (*r, if toward { Pull::Toward } else { Pull::Away });
        if !refs.contains(&entry) {
            refs.push(entry);
        }
    }
    Some(MoveIntent {
        goal: best_p,
        step: nav_toward(actor, best_p, state, dt),
        refs,
    })
}

/// The stand points a mover considers this tick. With terrain: the reachable
/// tiles' centres (a Dijkstra flood, so walls/cliffs are respected), sorted —
/// the flood map's iteration order is arbitrary and the argmax must be
/// deterministic. On a flat arena: a unit lattice around the actor, clamped so
/// the body stays in bounds.
fn candidate_points(from: Pos, state: &BattleState) -> Vec<Pos> {
    match state.terrain.as_ref() {
        Some(t) => {
            let mut tiles: Vec<Tile> = nav::reachable(t, t.tile_of(from), SEEK_RADIUS)
                .into_keys()
                .collect();
            tiles.sort_unstable();
            tiles.into_iter().map(|tl| t.tile_center(tl)).collect()
        }
        None => {
            let r = SEEK_RADIUS as i32;
            let mut pts = Vec::with_capacity(((2 * r + 1) * (2 * r + 1)) as usize);
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    pts.push(state.clamp_within(
                        Pos {
                            x: from.x + dx as f32,
                            y: from.y + dy as f32,
                        },
                        ENTITY_RADIUS,
                    ));
                }
            }
            pts
        }
    }
}

/// The midpoint of a set of entities — the reference point a movement intent
/// steers relative to (handles both single picks and AoE-style `All`).
fn centroid(ids: &[EntityId], state: &BattleState) -> Option<Pos> {
    if ids.is_empty() {
        return None;
    }
    let (mut x, mut y) = (0.0f32, 0.0f32);
    for &id in ids {
        let p = state.entity(id).pos;
        x += p.x;
        y += p.y;
    }
    let n = ids.len() as f32;
    Some(Pos { x: x / n, y: y / n })
}

/// One `move_speed` step from `from` toward `aim`, never overshooting it, then
/// bounds-clamped. Entity-vs-entity separation and the terrain backstop are the
/// caller's job (`combat::resolve_collisions`); here we only clamp the point.
fn step_point(from: Pos, aim: Pos, speed: f32, state: &BattleState) -> Pos {
    let (dx, dy) = (aim.x - from.x, aim.y - from.y);
    let dist = (dx * dx + dy * dy).sqrt();
    let dest = if dist <= speed || dist <= f32::EPSILON {
        aim
    } else {
        Pos { x: from.x + dx / dist * speed, y: from.y + dy / dist * speed }
    };
    state.clamp_pos(dest)
}

/// Close on `point`, routing around terrain obstacles. With terrain we A\* from
/// the actor's tile to the goal's and steer toward the next waypoint's centre;
/// on a flat arena (or once in the goal tile) we aim straight at the precise
/// point so melee can close exactly. Steering here is "follow the route"; the
/// caller resolves fine contact and separation. The step covers `dt` ticks'
/// worth of `move_speed`.
fn nav_toward(actor: EntityId, point: Pos, state: &BattleState, dt: f32) -> Pos {
    let a = state.entity(actor);
    let from = a.pos;
    let speed = a.effective_move_speed() * dt;

    if let Some(t) = state.terrain.as_ref() {
        let start = t.tile_of(from);
        let goal = t.tile_of(point);
        // Cap the step at the next waypoint's centre so we stay on the routed
        // tiles instead of cutting the corner into a wall. If we're already in the
        // goal tile or there's no route (walled off), fall through to a straight
        // nudge — the collision backstop keeps the mover out of the wall.
        if start != goal
            && let Some(path) = nav::find_path(t, start, goal)
            && path.len() >= 2
        {
            return step_point(from, t.tile_center(path[1]), speed, state);
        }
    }
    step_point(from, point, speed, state)
}

fn eval_node(node: &Node, actor: EntityId, state: &BattleState) -> Outcome {
    let (passed, matched) = eval_condition(&node.condition, actor, state);
    if !passed {
        return Outcome::Fall;
    }

    match &node.body {
        Body::Act { target, skill } => try_act(target, *skill, actor, state, &matched),
        Body::Group { mode, children } => {
            for child in children {
                match eval_node(child, actor, state) {
                    Outcome::Fall => continue,
                    found => return found, // Act or Wait both stop the walk
                }
            }
            // No child produced an action.
            match mode {
                GroupMode::Fallthrough => Outcome::Fall,
                GroupMode::Commit => Outcome::Wait,
            }
        }
    }
}

/// Try to turn an `Act` leaf into a feasible [`Action`].
fn try_act(
    target: &TargetQuery,
    skill_id: SkillId,
    actor: EntityId,
    state: &BattleState,
    matched: &[EntityId],
) -> Outcome {
    let actor_ent = state.entity(actor);
    let skill = state.skill(skill_id);

    // Feasibility: cooldown and resource cost are independent of the target.
    if actor_ent.cooldown_remaining(skill_id) > 0 || actor_ent.mp < skill.cost as f32 {
        return Outcome::Fall;
    }

    // The skill's range acts as an implicit filter on the candidate set, so
    // "no valid target in range" naturally becomes "not feasible".
    let targets = select(target, actor, state, matched, Some(skill.range));
    if targets.is_empty() {
        Outcome::Fall
    } else {
        Outcome::Act(Action {
            skill: skill_id,
            targets,
        })
    }
}

// ---------------------------------------------------------------------------
// Condition evaluation
// ---------------------------------------------------------------------------

/// Returns whether the condition holds, plus the union of entities its queries
/// matched (used by `Pool::Matched` to reuse the condition's result).
fn eval_condition(cond: &Condition, actor: EntityId, state: &BattleState) -> (bool, Vec<EntityId>) {
    match cond {
        Condition::Always => (true, Vec::new()),
        Condition::Exists(q) => {
            let c = candidates(q, actor, state, &[], None);
            (!c.is_empty(), c)
        }
        Condition::Count { q, cmp, n } => {
            let c = candidates(q, actor, state, &[], None);
            (cmp.test(c.len() as u32, *n), c)
        }
        // Negation does not contribute a positive match set to reuse.
        Condition::Not(inner) => {
            let (ok, _) = eval_condition(inner, actor, state);
            (!ok, Vec::new())
        }
        Condition::All(conds) => {
            let mut matched = Vec::new();
            for c in conds {
                let (ok, mut m) = eval_condition(c, actor, state);
                if !ok {
                    return (false, Vec::new());
                }
                matched.append(&mut m);
            }
            (true, matched)
        }
        Condition::Any(conds) => {
            for c in conds {
                let (ok, m) = eval_condition(c, actor, state);
                if ok {
                    return (true, m);
                }
            }
            (false, Vec::new())
        }
    }
}

// ---------------------------------------------------------------------------
// Target resolution: Pool -> Filters -> (range) -> Sort -> Pick
// ---------------------------------------------------------------------------

/// Full selection including sort and pick — used to choose actual targets.
fn select(
    query: &TargetQuery,
    actor: EntityId,
    state: &BattleState,
    matched: &[EntityId],
    range: Option<f32>,
) -> Vec<EntityId> {
    let mut cands = candidates(query, actor, state, matched, range);
    sort_candidates(&mut cands, query.sort, actor, state);
    apply_pick(cands, query.pick, actor)
}

/// Pool + filters + optional range, with no sort/pick. Used both for the
/// filtered candidate set of an action and for condition matching.
fn candidates(
    query: &TargetQuery,
    actor: EntityId,
    state: &BattleState,
    matched: &[EntityId],
    range: Option<f32>,
) -> Vec<EntityId> {
    let actor_ent = state.entity(actor);
    let base: Vec<EntityId> = match query.pool {
        Pool::Enemies => state
            .living()
            .filter(|&id| state.entity(id).team != actor_ent.team)
            .collect(),
        Pool::Allies => state
            .living()
            .filter(|&id| state.entity(id).team == actor_ent.team)
            .collect(),
        Pool::Myself => vec![actor],
        Pool::Everyone => state.living().collect(),
        // Reuse the condition's matches; keep only ones still living.
        Pool::Matched => matched
            .iter()
            .copied()
            .filter(|&id| state.entity(id).is_alive())
            .collect(),
    };

    base.into_iter()
        .filter(|&id| query.filters.iter().all(|f| pass_filter(f, id, actor, state)))
        // A skill's `range` is supplied only for action feasibility (never for
        // condition/movement queries). When it is, both range *and* line-of-sight
        // gate the target: you can't hit what you can't reach or can't see. LoS is
        // the implicit spatial-sanity check terrain adds, alongside range/cost/cd.
        .filter(|&id| match range {
            Some(r) => {
                let (ap, tp) = (state.entity(actor).pos, state.entity(id).pos);
                ap.dist(tp) <= r && state.line_of_sight(ap, tp)
            }
            None => true,
        })
        .collect()
}

fn pass_filter(filter: &Filter, id: EntityId, actor: EntityId, state: &BattleState) -> bool {
    let e = state.entity(id);
    match filter {
        Filter::HpPctBelow(x) => e.hp_pct() < *x,
        Filter::HpPctAbove(x) => e.hp_pct() > *x,
        Filter::HpBelow(x) => e.hp < *x,
        Filter::HasStatus(k) => e.status(*k).is_some(),
        Filter::StatusStacksAtLeast(k, n) => e.status_stacks(*k) >= *n,
        Filter::WeakTo(dt) => e.weaknesses.contains(dt),
        Filter::IsSelf => id == actor,
        Filter::NotSelf => id != actor,
        Filter::HasLineOfSight => state.line_of_sight(state.entity(actor).pos, e.pos),
        Filter::OnHigherGround => {
            state.elevation_at(e.pos) > state.elevation_at(state.entity(actor).pos)
        }
        Filter::WithinDistance(d) => state.entity(actor).pos.dist(e.pos) <= *d,
        // Resolve the nested query (actor-relative, no range/LoS gate — it's a
        // reference lookup, not an attack) and pass if the candidate stands
        // within `d` of any selected entity other than itself.
        Filter::WithinDistanceOf(q, d) => select(q, actor, state, &[], None)
            .iter()
            .any(|&r| r != id && state.entity(r).pos.dist(e.pos) <= *d),
        Filter::Not(inner) => !pass_filter(inner, id, actor, state),
    }
}

fn sort_candidates(
    cands: &mut [EntityId],
    sort: Option<(SortKey, Order)>,
    actor: EntityId,
    state: &BattleState,
) {
    let Some((key, order)) = sort else { return };
    let actor_pos = state.entity(actor).pos;
    cands.sort_by(|&a, &b| {
        let va = sort_value(key, a, actor_pos, state);
        let vb = sort_value(key, b, actor_pos, state);
        let ord = va.partial_cmp(&vb).unwrap_or(Ordering::Equal);
        match order {
            Order::Asc => ord,
            Order::Desc => ord.reverse(),
        }
    });
}

fn sort_value(key: SortKey, id: EntityId, actor_pos: crate::battle::Pos, state: &BattleState) -> f32 {
    let e = state.entity(id);
    match key {
        SortKey::Hp => e.hp,
        SortKey::HpPct => e.hp_pct(),
        SortKey::MaxHp => e.max_hp,
        SortKey::Mp => e.mp,
        SortKey::Distance => actor_pos.dist(e.pos),
        SortKey::Elevation => state.elevation_at(e.pos) as f32,
        SortKey::StatusStacks(k) => e.status_stacks(k) as f32,
    }
}

fn apply_pick(cands: Vec<EntityId>, pick: Pick, actor: EntityId) -> Vec<EntityId> {
    match pick {
        Pick::First => cands.into_iter().take(1).collect(),
        Pick::Take(n) => cands.into_iter().take(n as usize).collect(),
        Pick::All => cands,
        Pick::Random => {
            if cands.is_empty() {
                return cands;
            }
            // Deterministic, state-free "random": hash the actor together with
            // the candidate set. Avoids an rng dependency and keeps `decide`
            // pure, so tests are reproducible.
            let mut h = actor.0 as u64 ^ 0x9E37_79B9_7F4A_7C15;
            for c in &cands {
                h = h.wrapping_mul(0x1000_0000_01b3).wrapping_add(c.0 as u64);
            }
            let idx = (h % cands.len() as u64) as usize;
            vec![cands[idx]]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::*;
    use crate::gambit::*;
    use std::collections::HashMap;

    // --- scenario builder ------------------------------------------------

    struct World {
        state: BattleState,
    }

    impl World {
        fn new() -> Self {
            World {
                state: BattleState {
                    entities: Vec::new(),
                    skills: Vec::new(),
                    bounds: (1000.0, 1000.0),
                    terrain: None,
                },
            }
        }

        fn add_skill(&mut self, name: &str, cost: u32, range: f32, dt: Option<DamageType>) -> SkillId {
            let id = SkillId(self.state.skills.len());
            self.state.skills.push(Skill {
                name: name.into(),
                cost,
                range,
                cooldown: 0,
                cast_time: 0,
                damage_type: dt,
                effects: Vec::new(),
            });
            id
        }

        fn add(&mut self, name: &str, team: Team, hp: f32, x: f32) -> EntityId {
            let id = EntityId(self.state.entities.len());
            self.state.entities.push(Entity {
                id,
                name: name.into(),
                team,
                hp,
                max_hp: 100.0,
                mp: 100.0,
                max_mp: 100.0,
                mp_regen: 0.0,
                pos: Pos { x, y: 0.0 },
                statuses: Vec::new(),
                weaknesses: Vec::new(),
                skills: Vec::new(),
                cooldowns: HashMap::new(),
                atb_speed: 0.25,
                move_speed: 0.0,
                action_bar: 0.0,
            });
            id
        }

        fn ent(&mut self, id: EntityId) -> &mut Entity {
            &mut self.state.entities[id.0]
        }
    }

    // --- tests -----------------------------------------------------------

    /// The headline design goal: fire on "an enemy is below 50%" but target
    /// the *highest-HP* enemy, not the one that tripped the condition.
    #[test]
    fn condition_and_target_can_diverge() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let _weak = w.add("weak", Team::Enemy, 20.0, 1.0); // trips the condition
        let tank = w.add("tank", Team::Enemy, 90.0, 2.0); // but we want to hit this one
        let fireball = w.add_skill("Fireball", 10, 100.0, Some(DamageType::Fire));

        let rule = Node::act(
            TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Desc),
            fireball,
        )
        .when(Condition::Exists(
            TargetQuery::new(Pool::Enemies).filter(Filter::HpPctBelow(0.5)),
        ));

        let action = decide(&rule, hero, &w.state).expect("should act");
        assert_eq!(action.skill, fireball);
        assert_eq!(action.targets, vec![tank]); // highest HP, not `weak`
    }

    /// Two filters on one query must be satisfied by the *same* entity.
    #[test]
    fn filters_and_on_the_same_entity() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        // Below 50% but NOT weak to poison.
        let _bruiser = w.add("bruiser", Team::Enemy, 20.0, 1.0);
        // Weak to poison but healthy.
        let healthy = w.add("healthy", Team::Enemy, 90.0, 2.0);
        w.ent(healthy).weaknesses.push(DamageType::Poison);
        // Both below 50% AND weak to poison — the only real match.
        let both = w.add("both", Team::Enemy, 30.0, 3.0);
        w.ent(both).weaknesses.push(DamageType::Poison);
        let poison = w.add_skill("Poison Nova", 5, 100.0, Some(DamageType::Poison));

        let rule = Node::act(
            TargetQuery::new(Pool::Enemies)
                .filter(Filter::HpPctBelow(0.5))
                .filter(Filter::WeakTo(DamageType::Poison)),
            poison,
        );

        let action = decide(&rule, hero, &w.state).unwrap();
        assert_eq!(action.targets, vec![both]);
    }

    /// `Pool::Matched` reuses the condition's result as the target set.
    #[test]
    fn matched_reuses_condition_result() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let _full = w.add("full", Team::Enemy, 100.0, 1.0);
        let hurt = w.add("hurt", Team::Enemy, 10.0, 2.0);
        let strike = w.add_skill("Strike", 0, 100.0, None);

        let rule = Node::act(TargetQuery::new(Pool::Matched), strike).when(Condition::Exists(
            TargetQuery::new(Pool::Enemies).filter(Filter::HpPctBelow(0.5)),
        ));

        let action = decide(&rule, hero, &w.state).unwrap();
        assert_eq!(action.targets, vec![hurt]); // only the matched low-HP enemy
    }

    /// Feasibility is implicit: an out-of-range target makes the rule fall
    /// through to the next one.
    #[test]
    fn out_of_range_falls_through() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let far = w.add("far", Team::Enemy, 50.0, 100.0);
        let melee = w.add_skill("Slash", 0, 5.0, None); // short range
        let bow = w.add_skill("Shoot", 0, 999.0, None); // long range

        let rules = Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(TargetQuery::new(Pool::Enemies), melee), // too far -> skip
                Node::act(TargetQuery::new(Pool::Enemies), bow),   // reaches
            ],
        );

        let action = decide(&rules, hero, &w.state).unwrap();
        assert_eq!(action.skill, bow);
        assert_eq!(action.targets, vec![far]);
    }

    /// A `Commit` context that can't act makes the actor wait instead of
    /// falling out to more aggressive rules below it.
    #[test]
    fn commit_context_waits_instead_of_falling_out() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 25.0, 0.0); // hurt
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 1.0);
        let heal = w.add_skill("Heal", 0, 100.0, None);
        let attack = w.add_skill("Attack", 0, 100.0, None);

        // Put Heal on cooldown so the defensive branch can't fire.
        w.ent(hero).cooldowns.insert(heal, 3);

        let tree = Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                // "If I'm below 30%, ONLY consider defensive skills."
                Node::context(
                    Condition::Exists(
                        TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.3)),
                    ),
                    GroupMode::Commit,
                    vec![Node::act(TargetQuery::new(Pool::Myself), heal)],
                ),
                // Aggressive fallback — must NOT be reached while hurt.
                Node::act(TargetQuery::new(Pool::Enemies), attack),
            ],
        );

        assert_eq!(decide(&tree, hero, &w.state), None); // waits, does not attack

        // Once healthy, the commit context's guard is false and we fall
        // through to the attack.
        w.ent(hero).hp = 100.0;
        let action = decide(&tree, hero, &w.state).unwrap();
        assert_eq!(action.skill, attack);
    }

    /// Cross-entity condition: "I'm below 30% HP AND there are 3+ enemies".
    #[test]
    fn cross_entity_all_condition() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 25.0, 0.0);
        w.add("e1", Team::Enemy, 100.0, 1.0);
        w.add("e2", Team::Enemy, 100.0, 2.0);
        let panic_skill = w.add_skill("Panic Heal", 0, 100.0, None);

        let rule = Node::act(TargetQuery::new(Pool::Myself), panic_skill).when(Condition::All(
            vec![
                Condition::Exists(TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.3))),
                Condition::Count {
                    q: TargetQuery::new(Pool::Enemies),
                    cmp: Cmp::Ge,
                    n: 3,
                },
            ],
        ));

        // Only 2 enemies -> condition false.
        assert_eq!(decide(&rule, hero, &w.state), None);

        // Add a third enemy -> condition true.
        w.add("e3", Team::Enemy, 100.0, 3.0);
        let action = decide(&rule, hero, &w.state).unwrap();
        assert_eq!(action.skill, panic_skill);
        assert_eq!(action.targets, vec![hero]);
    }

    // --- movement --------------------------------------------------------

    fn nearest_enemy() -> TargetQuery {
        TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)
    }

    /// `Near(q, 0.0)` is pure pursuit: each step closes the gap by one
    /// `move_speed`, and stickiness stops the walk once it's effectively there
    /// (no orbiting / overshooting).
    #[test]
    fn near_zero_pursues_then_holds_on_arrival() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 50.0);
        let enemy = w.add("enemy", Team::Enemy, 100.0, 60.0);
        w.ent(hero).pos.y = 50.0; // interior — away from bounds clamping
        w.ent(enemy).pos.y = 50.0;
        w.ent(hero).move_speed = 3.0;

        let gambit = MoveGambit::toward(nearest_enemy());
        let from = w.state.entity(hero).pos;
        let target = w.state.entity(enemy).pos;
        let dest = decide_move(&gambit, hero, &w.state).expect("should pursue");
        assert!(dest.dist(target) < from.dist(target), "each step closes the gap");
        assert!(dest.dist(from) <= 3.0 + 1e-3, "never exceeds one move_speed step");

        // Practically arrived (inside melee reach): standing still wins.
        w.ent(hero).pos = Pos { x: 58.8, y: 50.0 }; // 1.2 from the target
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
    }

    /// The headline of the scoring model: `Near(q, ideal)` is approach,
    /// standoff *and* retreat in one term — the old three-rule kite band
    /// (with its threshold wobble) collapses into a single peak.
    #[test]
    fn near_ideal_is_a_standoff_band() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 50.0);
        let enemy = w.add("enemy", Team::Enemy, 100.0, 57.0);
        w.ent(hero).pos.y = 50.0;
        w.ent(enemy).pos.y = 50.0;
        w.ent(hero).move_speed = 1.0;
        let gambit = MoveGambit::new(vec![(Term::Near(nearest_enemy(), 7.0), 1.0)]);

        // At the ideal range: hold (stickiness beats any marginal improvement).
        assert_eq!(decide_move(&gambit, hero, &w.state), None, "holds at ideal range");

        // Dived on (too close): walk the drift to convergence — it must settle
        // back near the ideal range and *stop* (a stable standoff, not a
        // wobble). Individual steps may flank rather than back straight out.
        w.ent(enemy).pos.x = 53.0; // 3 away
        let mut steps = 0;
        while let Some(dest) = decide_move(&gambit, hero, &w.state) {
            w.ent(hero).pos = dest;
            steps += 1;
            assert!(steps < 40, "drift must converge, not oscillate forever");
        }
        let settled = w.state.entity(hero).pos.dist(w.state.entity(enemy).pos);
        assert!(
            (5.5..=8.5).contains(&settled),
            "should settle near the ideal range 7, got {settled}"
        );

        // Too far: advance — the destination closes the gap.
        w.ent(hero).pos = Pos { x: 50.0, y: 50.0 };
        w.ent(enemy).pos = Pos { x: 65.0, y: 50.0 }; // 15 away
        let d0 = w.state.entity(hero).pos.dist(w.state.entity(enemy).pos);
        let dest = decide_move(&gambit, hero, &w.state).expect("should advance");
        assert!(
            dest.dist(w.state.entity(enemy).pos) < d0,
            "advances when the target is beyond the ideal range"
        );
    }

    /// `AwayFrom` is a *bounded* flee: it opens the gap while the threat is
    /// close, and saturates once the gap is comfortable — no fleeing to a
    /// corner and stalemating.
    #[test]
    fn away_from_flees_close_threats_but_saturates() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 50.0);
        let enemy = w.add("enemy", Team::Enemy, 100.0, 53.0); // 3 away
        w.ent(hero).pos.y = 50.0;
        w.ent(enemy).pos.y = 50.0;
        w.ent(hero).move_speed = 1.0;
        let gambit = MoveGambit::new(vec![(Term::AwayFrom(nearest_enemy()), 1.0)]);

        let dest = decide_move(&gambit, hero, &w.state).expect("should flee a close threat");
        assert!(dest.dist(w.state.entity(enemy).pos) > 3.0, "opens the gap");

        // Gap already comfortable (>= saturation range): hold, don't corner-camp.
        w.ent(enemy).pos.x = 59.5; // 9.5 away
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
    }

    /// The intent surface the viewer draws from: pursuit exposes its target
    /// as a `Toward` reference and a goal that closes the gap; a flee exposes
    /// the threat as an `Away` reference and a goal that opens it.
    #[test]
    fn move_intent_exposes_goal_and_refs() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 50.0);
        let enemy = w.add("enemy", Team::Enemy, 100.0, 60.0);
        w.ent(hero).pos.y = 50.0;
        w.ent(enemy).pos.y = 50.0;
        w.ent(hero).move_speed = 1.0;
        let target = w.state.entity(enemy).pos;
        let from = w.state.entity(hero).pos;

        let pursue = MoveGambit::toward(nearest_enemy());
        let intent = move_intent(&pursue, hero, &w.state, 1.0).expect("should pursue");
        assert_eq!(intent.refs, vec![(target, Pull::Toward)]);
        assert!(intent.goal.dist(target) < from.dist(target), "goal closes the gap");
        assert_eq!(Some(intent.step), decide_move(&pursue, hero, &w.state));

        w.ent(enemy).pos.x = 53.0; // close threat
        let flee = MoveGambit::new(vec![(Term::AwayFrom(nearest_enemy()), 1.0)]);
        let intent = move_intent(&flee, hero, &w.state, 1.0).expect("should flee");
        assert_eq!(intent.refs, vec![(w.state.entity(enemy).pos, Pull::Away)]);
    }

    /// A gambit whose every query matches nothing holds position rather than
    /// moving relative to nothing.
    #[test]
    fn move_holds_when_no_target() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        w.ent(hero).move_speed = 1.0;
        // No enemies exist.
        let gambit = MoveGambit::toward(nearest_enemy());
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
    }

    // --- terrain: line-of-sight & high ground ---------------------------

    use crate::terrain::{Terrain, Tile3};

    /// A tall wall between actor and target blocks the shot: line-of-sight is an
    /// implicit feasibility check, so the rule falls through to no action. Clear
    /// the terrain and the same rule fires.
    #[test]
    fn line_of_sight_gates_a_ranged_skill() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.5);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 4.5);
        let bolt = w.add_skill("Bolt", 0, 100.0, None); // plenty of range

        let mut terrain = Terrain::flat(5, 1, 1.0);
        terrain.set(2, 0, Tile3 { elevation: 4, passable: false }); // wall between them
        w.state.terrain = Some(terrain);

        let rule = Node::act(TargetQuery::new(Pool::Enemies), bolt);
        assert_eq!(decide(&rule, hero, &w.state), None, "wall should block the shot");

        // Same geometry, no terrain -> in sight -> fires.
        w.state.terrain = None;
        assert!(decide(&rule, hero, &w.state).is_some());
    }

    /// A `HighGround`+`SightOf` blend walks the actor up a hill it can shoot
    /// from — the old dedicated "seek high ground" intent, recovered from two
    /// generic terms.
    #[test]
    fn high_ground_terms_climb_toward_a_hill() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.5);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 4.5);
        w.ent(hero).pos.y = 0.5;
        w.ent(hero).move_speed = 1.0;

        // A two-step hill just east of the actor; the far side is a cliff, so the
        // crest (col 2) is the highest reachable tile — and it sees the target.
        let mut terrain = Terrain::flat(5, 1, 1.0);
        terrain.set(1, 0, Tile3 { elevation: 1, passable: true });
        terrain.set(2, 0, Tile3 { elevation: 2, passable: true });
        w.state.terrain = Some(terrain);

        let gambit = MoveGambit::new(vec![
            (Term::HighGround, 1.0),
            (Term::SightOf(nearest_enemy()), 1.0),
        ]);
        let dest = decide_move(&gambit, hero, &w.state).expect("should head for the hill");
        assert!(dest.x > 0.5, "should move east toward the high ground, got {}", dest.x);
    }

    /// On a flat, terrain-free arena the same terms score uniformly, so the
    /// actor holds instead of shuffling.
    #[test]
    fn high_ground_terms_hold_when_flat() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.5);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 4.5);
        w.ent(hero).move_speed = 1.0;

        let gambit = MoveGambit::new(vec![
            (Term::HighGround, 1.0),
            (Term::SightOf(nearest_enemy()), 1.0),
        ]);
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
    }

    /// The relational filter: "an enemy engaging one of my teammates" picks
    /// the foe standing on the ally, not the one standing on the actor.
    #[test]
    fn within_distance_of_targets_the_allys_attacker() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let _ally = w.add("ally", Team::Player, 100.0, 10.0);
        let attacker = w.add("attacker", Team::Enemy, 100.0, 11.0); // on the ally
        let _brute = w.add("brute", Team::Enemy, 100.0, 2.0); // on the hero — not a match
        let strike = w.add_skill("Strike", 0, 100.0, None);

        let peel = Node::act(
            TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistanceOf(
                Box::new(
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::NotSelf)
                        .pick(Pick::All),
                ),
                3.0,
            )),
            strike,
        );

        let action = decide(&peel, hero, &w.state).unwrap();
        assert_eq!(action.targets, vec![attacker]);
    }

    /// A candidate never satisfies `WithinDistanceOf` through *itself*: a lone
    /// enemy is not "near an enemy" just because it is one.
    #[test]
    fn within_distance_of_never_matches_via_itself() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let _lone = w.add("lone", Team::Enemy, 100.0, 20.0);
        let strike = w.add_skill("Strike", 0, 100.0, None);

        let rule = Node::act(
            TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistanceOf(
                Box::new(TargetQuery::new(Pool::Enemies).pick(Pick::All)),
                3.0,
            )),
            strike,
        );

        assert_eq!(decide(&rule, hero, &w.state), None);
    }

    /// AoE pick returns every matching target.
    #[test]
    fn aoe_pick_all() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let a = w.add("a", Team::Enemy, 100.0, 1.0);
        let b = w.add("b", Team::Enemy, 100.0, 2.0);
        let nova = w.add_skill("Nova", 20, 100.0, Some(DamageType::Fire));

        let rule = Node::act(TargetQuery::new(Pool::Enemies).pick(Pick::All), nova);
        let action = decide(&rule, hero, &w.state).unwrap();
        assert_eq!(action.targets, vec![a, b]);
    }
}
