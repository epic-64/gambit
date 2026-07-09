//! Evaluation: given a gambit tree and an actor, decide what the actor does
//! when its action bar fills.
//!
//! The walk is depth-first. Each node's guard is checked; on success an `Act`
//! leaf tries to produce a feasible action and a `Group` recurses into its
//! children. Feasibility (cooldown / cost / range / has-a-valid-target) is
//! checked here implicitly — it is never hand-authored in a rule.

use std::cmp::Ordering;

use crate::battle::{BattleState, EntityId, SkillId};
use crate::gambit::{
    Body, Condition, Filter, GroupMode, Node, Order, Pick, Pool, SortKey, TargetQuery,
};

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
        .filter(|&id| match range {
            Some(r) => state.entity(actor).pos.dist(state.entity(id).pos) <= r,
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
                speed: 0.25,
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
