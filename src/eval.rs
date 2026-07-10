//! Evaluation: given a gambit tree and an actor, decide what the actor does
//! when its action bar fills.
//!
//! The walk is depth-first. Each node's guard is checked; on success an `Act`
//! leaf tries to produce a feasible action and a `Group` recurses into its
//! children. Feasibility (cooldown / cost / range / has-a-valid-target) is
//! checked here implicitly — it is never hand-authored in a rule.

use std::cmp::Ordering;

use crate::battle::{BattleState, EntityId, Pos, SkillId};
use crate::gambit::{
    Body, Condition, Filter, GroupMode, MoveIntent, MoveRule, Node, Order, Pick, Pool, SortKey,
    TargetQuery,
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

/// Decide where `actor` drifts this tick from its movement gambit, independent
/// of the action gambit. Walks the rules in order; the first whose guard holds
/// and whose intent resolves to a reference target yields a destination.
///
/// Returns the actor's *new* position (already bounds-clamped and limited to
/// one `move_speed` step), or `None` to hold position. Pure — mutation is the
/// caller's job.
pub fn decide_move(gambit: &[MoveRule], actor: EntityId, state: &BattleState) -> Option<Pos> {
    for rule in gambit {
        let (passed, matched) = eval_condition(&rule.condition, actor, state);
        if !passed {
            continue;
        }
        // Movement is rangeless & sightless: the whole point is to *reach* a
        // position, so we don't pre-filter the reference set by range or LoS.
        let refs = select(rule.intent.query(), actor, state, &matched, None);
        let Some(point) = centroid(&refs, state) else {
            continue; // nothing to move relative to — try the next rule
        };
        let dest = match &rule.intent {
            MoveIntent::Toward(_) => Some(nav_toward(actor, point, state)),
            MoveIntent::Away(_) => Some(flee(actor, point, state)),
            // Tile-seeking intents can decline (no better tile / flat arena);
            // then we fall through to the next rule rather than forcing a hold.
            MoveIntent::SeekHighGround(_) => seek_high_ground(actor, point, state),
            MoveIntent::BreakLoS(_) => break_los(actor, point, state),
        };
        if let Some(dest) = dest {
            return Some(dest);
        }
    }
    None
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

/// Reachable-tile search radius (A\* cost units ≈ tiles) for the tile-seeking
/// movement intents. Bounds the per-tick neighbourhood scan.
const TILE_SEEK_RADIUS: f32 = 8.0;

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
/// caller resolves fine contact and separation.
fn nav_toward(actor: EntityId, point: Pos, state: &BattleState) -> Pos {
    let a = state.entity(actor);
    let from = a.pos;
    let speed = a.move_speed;

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

/// Retreat from `threat`. Prefer a straight-away step; when terrain blocks it
/// (wall or cliff) rotate the heading progressively to *slide along* the
/// obstacle instead of stopping dead. On a flat arena the straight-away branch
/// always wins, reproducing the pre-terrain behaviour exactly.
fn flee(actor: EntityId, threat: Pos, state: &BattleState) -> Pos {
    let a = state.entity(actor);
    let from = a.pos;
    let speed = a.move_speed;

    // Base "directly away" heading; a deterministic fallback when we're on top
    // of the threat lets overlapping units still separate.
    let (ax, ay) = (from.x - threat.x, from.y - threat.y);
    let mag = (ax * ax + ay * ay).sqrt();
    let (bx, by) = if mag > f32::EPSILON { (ax / mag, ay / mag) } else { (1.0, 0.0) };

    // Straight away first, then widening rotations to either side (never past
    // perpendicular, so a rotated step never heads back toward the threat).
    const HEADINGS: [f32; 9] = [0.0, 30.0, -30.0, 45.0, -45.0, 60.0, -60.0, 90.0, -90.0];
    for deg in HEADINGS {
        let (nx, ny) = rotate(bx, by, deg);
        let dest = state.clamp_pos(Pos { x: from.x + nx * speed, y: from.y + ny * speed });
        if terrain_step_ok(state, from, dest) {
            return dest;
        }
    }
    from // hemmed in on every heading — hold
}

fn rotate(x: f32, y: f32, degrees: f32) -> (f32, f32) {
    let (s, c) = degrees.to_radians().sin_cos();
    (x * c - y * s, x * s + y * c)
}

/// Whether a small step from `from` to `dest` is allowed by the terrain
/// (passable destination, no cliff between the tiles). Vacuously true when flat.
/// Assumes a step shorter than a tile — true for `move_speed` drift.
fn terrain_step_ok(state: &BattleState, from: Pos, dest: Pos) -> bool {
    match state.terrain.as_ref() {
        None => true,
        Some(t) => t.walkable(t.tile_of(from), t.tile_of(dest)),
    }
}

/// Move toward the highest reachable tile that still has line-of-sight to
/// `target` — take the high ground while keeping the shot. Returns `None` (hold,
/// try the next rule) on a flat arena or when no reachable tile improves on the
/// current elevation.
fn seek_high_ground(actor: EntityId, target: Pos, state: &BattleState) -> Option<Pos> {
    let t = state.terrain.as_ref()?;
    let from = state.entity(actor).pos;
    let start = t.tile_of(from);
    let cur_elev = t.elevation(start);
    let reach = nav::reachable(t, start, TILE_SEEK_RADIUS);

    // Highest reachable tile that can see the target; ties → the nearer tile.
    let mut best: Option<(i32, f32, Tile)> = None;
    for (&tile, &cost) in &reach {
        let center = t.tile_center(tile);
        if !t.line_of_sight(center, target) {
            continue;
        }
        let elev = t.elevation(tile);
        let better = match best {
            None => true,
            Some((be, bc, _)) => elev > be || (elev == be && cost < bc),
        };
        if better {
            best = Some((elev, cost, tile));
        }
    }

    let (best_elev, _, goal) = best?;
    if goal == start || best_elev <= cur_elev {
        return None; // already as high as we can usefully get
    }
    Some(nav_toward(actor, t.tile_center(goal), state))
}

/// Move toward the nearest reachable tile from which `threat` can no longer see
/// us — duck behind cover. Returns `None` (hold) when flat, already hidden, or no
/// reachable tile breaks line-of-sight.
fn break_los(actor: EntityId, threat: Pos, state: &BattleState) -> Option<Pos> {
    let t = state.terrain.as_ref()?;
    let from = state.entity(actor).pos;
    let start = t.tile_of(from);
    if !t.line_of_sight(t.tile_center(start), threat) {
        return None; // already out of sight
    }
    let reach = nav::reachable(t, start, TILE_SEEK_RADIUS);

    let mut best: Option<(f32, Tile)> = None;
    for (&tile, &cost) in &reach {
        if tile == start || t.line_of_sight(t.tile_center(tile), threat) {
            continue;
        }
        if best.is_none_or(|(bc, _)| cost < bc) {
            best = Some((cost, tile));
        }
    }
    let (_, goal) = best?;
    Some(nav_toward(actor, t.tile_center(goal), state))
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
    if actor_ent.cooldown_remaining(skill_id) > 0 || actor_ent.mp < skill.cost {
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
                mp: 100,
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

    /// `MoveToward` steps one `move_speed` along the line to the target and
    /// never overshoots it.
    #[test]
    fn move_toward_steps_then_stops_on_arrival() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 10.0);
        w.ent(hero).move_speed = 3.0;

        let gambit = vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))];
        let dest = decide_move(&gambit, hero, &w.state).unwrap();
        assert_eq!(dest.x, 3.0); // one 3-unit step from 0 toward 10

        // Close enough that a full step would overshoot -> land exactly on it.
        w.ent(hero).pos.x = 8.5;
        let dest = decide_move(&gambit, hero, &w.state).unwrap();
        assert_eq!(dest.x, 10.0);
    }

    /// `MoveAway` steps directly away and is clamped to the arena bounds.
    #[test]
    fn move_away_steps_and_clamps_to_bounds() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 5.0);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 10.0);
        w.ent(hero).move_speed = 2.0;

        let gambit = vec![MoveRule::new(MoveIntent::Away(nearest_enemy()))];
        let dest = decide_move(&gambit, hero, &w.state).unwrap();
        assert_eq!(dest.x, 3.0); // fled 2 units away from the enemy at 10

        // Backed against the wall: a step past 0 clamps to the boundary.
        w.ent(hero).pos.x = 1.0;
        let dest = decide_move(&gambit, hero, &w.state).unwrap();
        assert_eq!(dest.x, 0.0);
    }

    /// A `WithinDistance` filter on a flee query makes the kite *bounded*: the
    /// actor backs away only while the threat is inside the guard distance, and
    /// holds once the gap is open (so it doesn't flee to a corner and stalemate).
    #[test]
    fn within_distance_bounds_the_kite() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 5.0);
        let enemy = w.add("enemy", Team::Enemy, 100.0, 9.0); // 4 units away
        w.ent(hero).move_speed = 1.0;

        // Kite only a foe that has closed inside 6 units.
        let gambit = vec![MoveRule::new(MoveIntent::Away(
            TargetQuery::new(Pool::Enemies)
                .filter(Filter::WithinDistance(6.0))
                .sort(SortKey::Distance, Order::Asc),
        ))];

        // Threat is within 6 -> flee away from it (westward, x decreases).
        let dest = decide_move(&gambit, hero, &w.state).expect("should kite a close threat");
        assert_eq!(dest.x, 4.0);

        // Push the threat out past 6 units -> query is empty -> hold, don't flee.
        w.ent(enemy).pos.x = 15.0; // now 10 units away
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
    }

    /// A movement rule whose target query is empty holds position (falls to the
    /// next rule / returns None), rather than moving toward nothing.
    #[test]
    fn move_holds_when_no_target() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.0);
        w.ent(hero).move_speed = 1.0;
        // No enemies exist.
        let gambit = vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))];
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

    /// `SeekHighGround` walks the actor toward the highest reachable tile that
    /// still sees the target, gaining elevation.
    #[test]
    fn seek_high_ground_climbs_toward_a_hill() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.5);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 4.5);
        w.ent(hero).move_speed = 1.0;

        // A two-step hill just east of the actor; the far side is a cliff, so the
        // crest (col 2) is the highest reachable tile — and it sees the target.
        let mut terrain = Terrain::flat(5, 1, 1.0);
        terrain.set(1, 0, Tile3 { elevation: 1, passable: true });
        terrain.set(2, 0, Tile3 { elevation: 2, passable: true });
        w.state.terrain = Some(terrain);

        let gambit = vec![MoveRule::new(MoveIntent::SeekHighGround(nearest_enemy()))];
        let dest = decide_move(&gambit, hero, &w.state).expect("should head for the hill");
        assert!(dest.x > 0.5, "should move east toward the high ground, got {}", dest.x);
    }

    /// On a flat, terrain-free arena there is no high ground, so `SeekHighGround`
    /// declines (holds / falls to the next rule).
    #[test]
    fn seek_high_ground_holds_when_flat() {
        let mut w = World::new();
        let hero = w.add("hero", Team::Player, 100.0, 0.5);
        let _enemy = w.add("enemy", Team::Enemy, 100.0, 4.5);
        w.ent(hero).move_speed = 1.0;

        let gambit = vec![MoveRule::new(MoveIntent::SeekHighGround(nearest_enemy()))];
        assert_eq!(decide_move(&gambit, hero, &w.state), None);
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
