//! The combat loop: an ATB (active-time-battle) driver that fills action bars
//! over discrete ticks, asks each ready entity's gambit what to do via
//! [`decide`], and resolves the chosen action (damage, healing, statuses,
//! cooldowns). Engine-agnostic — no Macroquad — so the whole fight is testable.

use std::collections::HashMap;

use crate::battle::{
    BattleState, DamageType, EntityId, Effect, Pos, Skill, SkillId, Status, StatusKind, Team,
    ENTITY_RADIUS,
};
use crate::eval::{decide, decide_move, Action};
use crate::gambit::{MoveRule, Node};

/// Action-bar value at which an entity gets to act.
const READY: f32 = 1.0;
/// Damage multiplier applied when a target is weak to the skill's element.
const WEAKNESS_MULT: f32 = 1.5;
/// Per-stack, per-tick amounts for damage-/heal-over-time statuses.
const POISON_PER_STACK: f32 = 3.0;
const BURN_PER_STACK: f32 = 5.0;
const REGEN_PER_STACK: f32 = 4.0;

/// Something that happened during a tick — a log for tests and (later) the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Acted {
        actor: EntityId,
        skill: SkillId,
        targets: Vec<EntityId>,
    },
    /// A ready entity's gambit produced no action (fallthrough exhausted or a
    /// Commit context chose to wait).
    Waited(EntityId),
    Damage {
        target: EntityId,
        amount: f32,
        weakness: bool,
    },
    Heal {
        target: EntityId,
        amount: f32,
    },
    Inflicted {
        target: EntityId,
        kind: StatusKind,
        stacks: u32,
    },
    /// A cast-time skill was begun; the actor is now rooted until it resolves.
    /// MP and cooldown are already committed at this point.
    StartedCast {
        actor: EntityId,
        skill: SkillId,
        targets: Vec<EntityId>,
    },
    /// A cast completed but no committed target was still valid (dead or moved
    /// out of range) — it produces no effect. The counterplay to a big cast.
    Fizzled {
        actor: EntityId,
        skill: SkillId,
    },
    Died(EntityId),
    Victory(Team),
}

/// A cast-time skill in progress: the chosen action, frozen at cast start, plus
/// the ticks left before it resolves. While present the caster is rooted and its
/// ATB is frozen.
struct Cast {
    action: Action,
    remaining: u32,
}

/// Owns the mutable battle plus each entity's gambit tree, and advances time.
pub struct Combat {
    pub state: BattleState,
    /// Each entity's action ruleset, keyed by id. An entity with no gambit never acts.
    pub gambits: HashMap<EntityId, Node>,
    /// Each entity's movement ruleset, keyed by id. An entity with no movement
    /// gambit holds position (the pre-movement behaviour).
    pub move_gambits: HashMap<EntityId, Vec<MoveRule>>,
    /// Casts currently in flight, keyed by caster. Presence == "is casting".
    casts: HashMap<EntityId, Cast>,
    pub time: u32,
    over: bool,
}

impl Combat {
    pub fn new(state: BattleState, gambits: HashMap<EntityId, Node>) -> Self {
        Combat {
            state,
            gambits,
            move_gambits: HashMap::new(),
            casts: HashMap::new(),
            time: 0,
            over: false,
        }
    }

    /// Attach movement gambits (builder-style).
    pub fn with_movement(mut self, move_gambits: HashMap<EntityId, Vec<MoveRule>>) -> Self {
        self.move_gambits = move_gambits;
        self
    }

    pub fn is_over(&self) -> bool {
        self.over
    }

    /// Whether `id` is mid-cast (rooted, ATB frozen).
    pub fn is_casting(&self, id: EntityId) -> bool {
        self.casts.contains_key(&id)
    }

    /// Ticks remaining on `id`'s cast, if any (for UI cast bars).
    pub fn cast_remaining(&self, id: EntityId) -> Option<u32> {
        self.casts.get(&id).map(|c| c.remaining)
    }

    /// Run ticks until the battle ends or `max_ticks` is reached, returning the
    /// full event log. The cap is a safety net against never-ending stalemates.
    pub fn run(&mut self, max_ticks: u32) -> Vec<Event> {
        let mut log = Vec::new();
        for _ in 0..max_ticks {
            if self.over {
                break;
            }
            log.extend(self.tick());
        }
        log
    }

    /// Advance the simulation by one tick: apply status effects, tick down
    /// cooldowns, fill action bars, and let every newly-ready entity act.
    pub fn tick(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        if self.over {
            return events;
        }
        self.time += 1;

        self.tick_statuses(&mut events);
        if self.check_over(&mut events) {
            return events;
        }
        self.tick_cooldowns();

        // Advance in-flight casts; any that complete resolve (or fizzle) now.
        self.advance_casts(&mut events);
        if self.check_over(&mut events) {
            return events;
        }

        // Continuous movement: alive, non-casting units drift per their
        // movement gambit — independent of and concurrent with the ATB.
        self.tick_movement();

        // Fill bars, capped at READY so a waiting entity doesn't accumulate.
        // Casting units are frozen (bar stays at 0 until the cast resolves).
        for e in &mut self.state.entities {
            if e.is_alive() && !self.casts.contains_key(&e.id) {
                e.action_bar = (e.action_bar + e.atb_speed).min(READY);
            }
        }

        // Everyone at/over the threshold acts this tick, fullest bar first
        // (ties broken by id for determinism).
        let mut ready: Vec<EntityId> = self
            .state
            .entities
            .iter()
            .filter(|e| e.is_alive() && e.action_bar >= READY)
            .map(|e| e.id)
            .collect();
        ready.sort_by(|&a, &b| {
            let ba = self.state.entity(a).action_bar;
            let bb = self.state.entity(b).action_bar;
            bb.partial_cmp(&ba)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        for actor in ready {
            if self.over {
                break;
            }
            // May have died to a same-tick effect, or lost readiness.
            if !self.state.entity(actor).is_alive() || self.state.entity(actor).action_bar < READY {
                continue;
            }
            let decision = self
                .gambits
                .get(&actor)
                .and_then(|tree| decide(tree, actor, &self.state));

            match decision {
                Some(action) => {
                    let skill = self.state.skill(action.skill).clone();
                    // Spend the turn and commit MP + cooldown up front — this is
                    // "commit at cast start" for cast-time skills.
                    self.commit_cost(actor, action.skill, &skill);
                    self.state.entities[actor.0].action_bar = 0.0;

                    if skill.cast_time > 0 {
                        // Root the caster; the action resolves later, re-validated.
                        events.push(Event::StartedCast {
                            actor,
                            skill: action.skill,
                            targets: action.targets.clone(),
                        });
                        self.casts.insert(
                            actor,
                            Cast {
                                remaining: skill.cast_time,
                                action,
                            },
                        );
                    } else {
                        // Instant: resolve this tick.
                        events.push(Event::Acted {
                            actor,
                            skill: action.skill,
                            targets: action.targets.clone(),
                        });
                        self.apply_effects(actor, &action, &skill, &mut events);
                        self.check_over(&mut events);
                    }
                }
                None => {
                    // Keep the bar full and re-evaluate next tick (e.g. once a
                    // cooldown expires the entity can finally act).
                    events.push(Event::Waited(actor));
                }
            }
        }

        events
    }

    /// Move every alive, non-casting entity one step along its movement gambit.
    fn tick_movement(&mut self) {
        let movers: Vec<EntityId> = self
            .state
            .entities
            .iter()
            .filter(|e| {
                e.is_alive()
                    && !self.casts.contains_key(&e.id)
                    && self.move_gambits.contains_key(&e.id)
            })
            .map(|e| e.id)
            .collect();

        for id in movers {
            if let Some(gambit) = self.move_gambits.get(&id) {
                if let Some(dest) = decide_move(gambit, id, &self.state) {
                    let resolved = self.resolve_collisions(id, dest);
                    self.state.entities[id.0].pos = resolved;
                }
            }
        }
    }

    /// Radius-aware separation for a moving entity: keep its circle inside the
    /// arena and out of every other living entity's circle. Only the mover is
    /// displaced (movers are resolved one at a time, in id order), so this is
    /// order-stable and always terminates. A few relaxation passes settle the
    /// common case of touching several neighbours at once. This is the "don't
    /// obliviously stack on top of each other" sanity — true steering/avoidance
    /// arrives with the terrain layer.
    fn resolve_collisions(&self, mover: EntityId, dest: Pos) -> Pos {
        let r = ENTITY_RADIUS;
        let from = self.state.entity(mover).pos; // where the mover started this tick
        let min_dist = r + ENTITY_RADIUS; // uniform radius: same for every pair
        let mut p = self.state.clamp_within(dest, r);

        for _ in 0..4 {
            let mut adjusted = false;
            for other in self.state.living() {
                if other == mover {
                    continue;
                }
                let o = self.state.entity(other);
                let dx = p.x - o.pos.x;
                let dy = p.y - o.pos.y;
                let d2 = dx * dx + dy * dy;
                if d2 < min_dist * min_dist {
                    let d = d2.sqrt();
                    // Push the mover out to just-touching along the contact
                    // normal. If the centres coincide (a full overshoot onto the
                    // target), separate back toward where the mover came from so
                    // it stops on the near side, not teleporting past.
                    let (nx, ny) = if d > f32::EPSILON {
                        (dx / d, dy / d)
                    } else {
                        let (bx, by) = (from.x - o.pos.x, from.y - o.pos.y);
                        let bd = (bx * bx + by * by).sqrt();
                        if bd > f32::EPSILON {
                            (bx / bd, by / bd)
                        } else {
                            (1.0, 0.0)
                        }
                    };
                    p = self.state.clamp_within(
                        Pos {
                            x: o.pos.x + nx * min_dist,
                            y: o.pos.y + ny * min_dist,
                        },
                        r,
                    );
                    adjusted = true;
                }
            }
            if !adjusted {
                break;
            }
        }
        p
    }

    /// Tick down every in-flight cast; resolve or fizzle the ones that complete.
    fn advance_casts(&mut self, events: &mut Vec<Event>) {
        let casting: Vec<EntityId> = self.casts.keys().copied().collect();
        let mut completed: Vec<(EntityId, Action)> = Vec::new();
        for id in casting {
            if !self.state.entity(id).is_alive() {
                self.casts.remove(&id); // caster died mid-cast — the cast is lost
                continue;
            }
            let cast = self.casts.get_mut(&id).unwrap();
            cast.remaining -= 1;
            if cast.remaining == 0 {
                completed.push((id, self.casts.remove(&id).unwrap().action));
            }
        }

        for (actor, action) in completed {
            if self.over {
                break;
            }
            self.resolve_cast(actor, action, events);
        }
    }

    /// Resolve a completed cast, re-validating its committed targets against the
    /// *current* world: a target that has died or drifted out of range is
    /// dropped, and a cast with no valid target left fizzles.
    fn resolve_cast(&mut self, actor: EntityId, mut action: Action, events: &mut Vec<Event>) {
        let skill = self.state.skill(action.skill).clone();
        let actor_pos = self.state.entity(actor).pos;
        action.targets.retain(|&t| {
            let e = self.state.entity(t);
            e.is_alive() && actor_pos.dist(e.pos) <= skill.range
        });

        if action.targets.is_empty() {
            events.push(Event::Fizzled {
                actor,
                skill: action.skill,
            });
            return;
        }

        events.push(Event::Acted {
            actor,
            skill: action.skill,
            targets: action.targets.clone(),
        });
        self.apply_effects(actor, &action, &skill, events);
        self.check_over(events);
    }

    // --- resolution ------------------------------------------------------

    /// Pay a skill's MP cost and start its cooldown. Done at action time —
    /// which for a cast-time skill is *cast start*, not resolution.
    fn commit_cost(&mut self, actor: EntityId, skill_id: SkillId, skill: &Skill) {
        let a = &mut self.state.entities[actor.0];
        a.mp = a.mp.saturating_sub(skill.cost);
        if skill.cooldown > 0 {
            a.cooldowns.insert(skill_id, skill.cooldown);
        }
    }

    /// Apply a skill's effects to each target. Cost/cooldown are paid separately
    /// (see [`commit_cost`]) so this can run at cast completion without paying twice.
    fn apply_effects(
        &mut self,
        _actor: EntityId,
        action: &Action,
        skill: &Skill,
        events: &mut Vec<Event>,
    ) {
        for &tgt in &action.targets {
            for effect in &skill.effects {
                match effect {
                    Effect::Damage(base) => {
                        self.apply_damage(tgt, *base, skill.damage_type, events)
                    }
                    Effect::Heal(amount) => self.apply_heal(tgt, *amount, events),
                    Effect::Inflict {
                        kind,
                        stacks,
                        duration,
                    } => self.apply_status(tgt, *kind, *stacks, *duration, events),
                }
            }
        }
    }

    fn apply_damage(
        &mut self,
        target: EntityId,
        base: f32,
        dmg_type: Option<DamageType>,
        events: &mut Vec<Event>,
    ) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        let weak = matches!(dmg_type, Some(dt) if e.weaknesses.contains(&dt));
        let amount = base * if weak { WEAKNESS_MULT } else { 1.0 };
        e.hp = (e.hp - amount).max(0.0);
        events.push(Event::Damage {
            target,
            amount,
            weakness: weak,
        });
        if !e.is_alive() {
            events.push(Event::Died(target));
        }
    }

    fn apply_heal(&mut self, target: EntityId, amount: f32, events: &mut Vec<Event>) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        e.hp = (e.hp + amount).min(e.max_hp);
        events.push(Event::Heal { target, amount });
    }

    fn apply_status(
        &mut self,
        target: EntityId,
        kind: StatusKind,
        stacks: u32,
        duration: u32,
        events: &mut Vec<Event>,
    ) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        // Stack onto an existing status of the same kind, refreshing duration.
        if let Some(s) = e.statuses.iter_mut().find(|s| s.kind == kind) {
            s.stacks += stacks;
            s.duration = s.duration.max(duration);
        } else {
            e.statuses.push(Status {
                kind,
                stacks,
                duration,
            });
        }
        events.push(Event::Inflicted {
            target,
            kind,
            stacks,
        });
    }

    // --- per-tick upkeep -------------------------------------------------

    fn tick_statuses(&mut self, events: &mut Vec<Event>) {
        for i in 0..self.state.entities.len() {
            if !self.state.entities[i].is_alive() {
                continue;
            }
            let (mut dmg, mut heal) = (0.0f32, 0.0f32);
            for s in &self.state.entities[i].statuses {
                match s.kind {
                    StatusKind::Poison => dmg += POISON_PER_STACK * s.stacks as f32,
                    StatusKind::Burn => dmg += BURN_PER_STACK * s.stacks as f32,
                    StatusKind::Regen => heal += REGEN_PER_STACK * s.stacks as f32,
                    _ => {}
                }
            }
            let id = self.state.entities[i].id;
            if dmg > 0.0 {
                self.apply_damage(id, dmg, None, events);
            }
            if heal > 0.0 && self.state.entities[i].is_alive() {
                self.apply_heal(id, heal, events);
            }

            // Age statuses and drop the expired ones.
            let e = &mut self.state.entities[i];
            for s in &mut e.statuses {
                s.duration = s.duration.saturating_sub(1);
            }
            e.statuses.retain(|s| s.duration > 0);
        }
    }

    fn tick_cooldowns(&mut self) {
        for e in &mut self.state.entities {
            e.cooldowns.retain(|_, remaining| {
                *remaining = remaining.saturating_sub(1);
                *remaining > 0
            });
        }
    }

    /// End the battle once one whole team is down. Returns true if over.
    fn check_over(&mut self, events: &mut Vec<Event>) -> bool {
        if self.over {
            return true;
        }
        let players = self
            .state
            .entities
            .iter()
            .any(|e| e.is_alive() && e.team == Team::Player);
        let enemies = self
            .state
            .entities
            .iter()
            .any(|e| e.is_alive() && e.team == Team::Enemy);
        if players && enemies {
            return false;
        }
        self.over = true;
        let winner = if players { Team::Player } else { Team::Enemy };
        events.push(Event::Victory(winner));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::*;
    use crate::gambit::*;

    /// Fluent scenario builder shared by the combat tests.
    struct Arena {
        state: BattleState,
        gambits: HashMap<EntityId, Node>,
        move_gambits: HashMap<EntityId, Vec<MoveRule>>,
    }

    impl Arena {
        fn new() -> Self {
            Arena {
                state: BattleState {
                    entities: Vec::new(),
                    skills: Vec::new(),
                    bounds: (100.0, 100.0),
                },
                gambits: HashMap::new(),
                move_gambits: HashMap::new(),
            }
        }

        fn skill(&mut self, s: Skill) -> SkillId {
            let id = SkillId(self.state.skills.len());
            self.state.skills.push(s);
            id
        }

        fn add(&mut self, name: &str, team: Team, hp: f32, speed: f32) -> EntityId {
            self.add_at(name, team, hp, speed, 0.0, 0.0)
        }

        fn add_at(
            &mut self,
            name: &str,
            team: Team,
            hp: f32,
            speed: f32,
            x: f32,
            move_speed: f32,
        ) -> EntityId {
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
                atb_speed: speed,
                move_speed,
                action_bar: 0.0,
            });
            id
        }

        fn ent(&mut self, id: EntityId) -> &mut Entity {
            &mut self.state.entities[id.0]
        }

        fn gambit(&mut self, id: EntityId, tree: Node) {
            self.gambits.insert(id, tree);
        }

        fn move_gambit(&mut self, id: EntityId, rules: Vec<MoveRule>) {
            self.move_gambits.insert(id, rules);
        }

        fn into_combat(self) -> Combat {
            Combat::new(self.state, self.gambits).with_movement(self.move_gambits)
        }
    }

    fn damage_skill(name: &str, amount: f32, dt: Option<DamageType>, cooldown: u32) -> Skill {
        Skill {
            name: name.into(),
            cost: 0,
            range: 1000.0,
            cooldown,
            cast_time: 0,
            damage_type: dt,
            effects: vec![Effect::Damage(amount)],
        }
    }

    #[test]
    fn battle_ends_with_a_victor() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 0.5);
        let dummy = a.add("dummy", Team::Enemy, 30.0, 0.0); // never acts
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), hit));

        let log = a.into_combat().run(100);

        assert!(log.contains(&Event::Died(dummy)));
        assert!(log.contains(&Event::Victory(Team::Player)));
    }

    #[test]
    fn weakness_multiplier_applies() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        a.ent(enemy).weaknesses.push(DamageType::Fire);
        let fireball = a.skill(damage_skill("Fireball", 20.0, Some(DamageType::Fire), 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), fireball));

        let log = a.into_combat().tick(); // hero acts once

        let dmg = log
            .iter()
            .find_map(|e| match e {
                Event::Damage { amount, weakness, .. } => Some((*amount, *weakness)),
                _ => None,
            })
            .expect("a damage event");
        assert!(dmg.1, "should be flagged as a weakness hit");
        assert_eq!(dmg.0, 30.0); // 20 * 1.5
    }

    #[test]
    fn cooldown_forces_fallback_in_the_loop() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0); // acts every tick
        let _enemy = a.add("enemy", Team::Enemy, 500.0, 0.0); // soaks hits
        let strong = a.skill(damage_skill("Strong", 50.0, None, 5)); // 5-tick cooldown
        let weak = a.skill(damage_skill("Weak", 5.0, None, 0));

        // Prefer Strong; fall back to Weak while it recharges.
        a.gambit(
            hero,
            Node::context(
                Condition::Always,
                GroupMode::Fallthrough,
                vec![
                    Node::act(TargetQuery::new(Pool::Enemies), strong),
                    Node::act(TargetQuery::new(Pool::Enemies), weak),
                ],
            ),
        );

        let log = a.into_combat().run(3);
        let skills_used: Vec<SkillId> = log
            .iter()
            .filter_map(|e| match e {
                Event::Acted { skill, .. } => Some(*skill),
                _ => None,
            })
            .collect();

        // Tick 1 Strong, then Strong is on cooldown so ticks 2 & 3 use Weak.
        assert_eq!(skills_used, vec![strong, weak, weak]);
    }

    #[test]
    fn damage_over_time_ticks_down_and_expires() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        // One-shot: inflict 2 stacks of poison for 3 ticks, then never again.
        let venom = a.skill(Skill {
            name: "Venom".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 99,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict {
                kind: StatusKind::Poison,
                stacks: 2,
                duration: 3,
            }],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), venom));

        let mut combat = a.into_combat();
        let log = combat.run(10);

        // 2 stacks * 3.0 per stack * 3 ticks = 18 total poison damage.
        let poison_dmg: f32 = log
            .iter()
            .filter_map(|e| match e {
                Event::Damage { target, amount, .. } if *target == enemy => Some(*amount),
                _ => None,
            })
            .sum();
        assert_eq!(poison_dmg, 18.0);
        assert_eq!(combat.state.entity(enemy).hp, 82.0);
        // Status expired, not lingering.
        assert!(combat.state.entity(enemy).statuses.is_empty());
    }

    /// A unit with a movement gambit but too short a range drifts into melee
    /// over several ticks and only then lands a hit — movement and the action
    /// bar advance concurrently, never one instead of the other.
    #[test]
    fn movement_closes_into_melee_range() {
        let mut a = Arena::new();
        //             name    team          hp     atb   x     move
        let hero = a.add_at("hero", Team::Player, 100.0, 1.0, 0.0, 1.0);
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 5.0, 0.0);
        let jab = a.skill(Skill {
            name: "Jab".into(),
            cost: 0,
            range: 2.0, // can't reach the enemy 5 units away at the start
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Damage(10.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), jab));
        a.move_gambit(
            hero,
            vec![MoveRule::new(MoveIntent::Toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ))],
        );

        let mut combat = a.into_combat();

        // First couple of ticks: out of range, so the hero waits but still moves.
        combat.tick();
        assert!(combat.state.entity(hero).pos.x > 0.0, "hero should have moved");
        assert_eq!(combat.state.entity(enemy).hp, 100.0, "no hit while out of range");

        // Given enough ticks it closes the gap and starts landing hits.
        combat.run(10);
        assert!(
            combat.state.entity(enemy).hp < 100.0,
            "hero should have closed in and hit"
        );
    }

    /// Two entities can't occupy the same space: a mover charging a stationary
    /// blocker stops when their circles touch, never on top of it.
    #[test]
    fn movement_stops_at_hitbox_contact() {
        let mut a = Arena::new();
        //             name       team          hp    atb  x    move
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 0.0, 10.0);
        let blocker = a.add_at("blocker", Team::Enemy, 100.0, 0.0, 5.0, 0.0);
        // Put both on the same interior row so the geometry is purely along x
        // (away from the y-edge, which would otherwise lift a y=0 centre).
        a.ent(mover).pos.y = 5.0;
        a.ent(blocker).pos.y = 5.0;
        a.move_gambit(
            mover,
            vec![MoveRule::new(MoveIntent::Toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ))],
        );

        let mut combat = a.into_combat();
        combat.run(20); // plenty of ticks to try (and fail) to overlap

        let m = combat.state.entity(mover).pos;
        let b = combat.state.entity(blocker).pos;
        let sep = m.dist(b);
        // Stops just-touching (sum of the two equal radii), on the near side.
        let contact = 2.0 * ENTITY_RADIUS;
        assert!(
            (sep - contact).abs() < 1e-3,
            "expected contact at {contact}, got {sep}"
        );
        assert!(m.x < b.x, "mover should stop before the blocker, not pass it");
    }

    /// A unit fleeing into a wall keeps its whole body in the arena — its centre
    /// stops a radius short of the edge, not on it.
    #[test]
    fn movement_keeps_body_inside_bounds() {
        let mut a = Arena::new();
        a.state.bounds = (10.0, 10.0);
        let runner = a.add_at("runner", Team::Player, 100.0, 0.0, 1.0, 5.0);
        let _chaser = a.add_at("chaser", Team::Enemy, 100.0, 0.0, 8.0, 0.0);
        a.move_gambit(
            runner,
            vec![MoveRule::new(MoveIntent::Away(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ))],
        );

        let mut combat = a.into_combat();
        combat.run(20);

        // Fled toward x=0 but the radius keeps the whole body in — centre stops
        // a radius short of the wall.
        let x = combat.state.entity(runner).pos.x;
        assert!(
            (x - ENTITY_RADIUS).abs() < 1e-3,
            "expected centre at radius {ENTITY_RADIUS}, got {x}"
        );
    }

    /// A cast-time skill roots the caster, freezes its ATB, pays its cost once
    /// up front, and resolves only after `cast_time` ticks.
    #[test]
    fn cast_time_roots_then_resolves() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0); // ready every tick
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 10,
            range: 1000.0,
            cooldown: 5, // can't immediately recast after it lands
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(30.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), nuke));

        let mut combat = a.into_combat();

        combat.tick(); // tick 1: cast begins
        assert!(combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 100.0);
        assert_eq!(combat.state.entity(hero).mp, 90); // paid at cast start

        combat.tick(); // tick 2: still casting
        assert!(combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 100.0);

        combat.tick(); // tick 3: resolves
        assert!(!combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 70.0);
        assert_eq!(combat.state.entity(hero).mp, 90); // not charged twice
    }

    /// If every committed target becomes invalid mid-cast (here: killed by an
    /// ally), the cast fizzles instead of resolving — the interrupt/counterplay.
    #[test]
    fn cast_fizzles_when_target_dies_midcast() {
        let mut a = Arena::new();
        let caster = a.add("caster", Team::Player, 100.0, 1.0);
        // The nuke's victim: lowest-HP enemy, killed mid-cast. A second, healthy
        // enemy keeps the battle going past its death so the cast can resolve.
        let victim = a.add("victim", Team::Enemy, 25.0, 0.0);
        let _bystander = a.add("bystander", Team::Enemy, 100.0, 0.0);
        let ally = a.add("ally", Team::Player, 100.0, 1.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(50.0)],
        });
        let jab = a.skill(damage_skill("Jab", 20.0, None, 0));
        // Both caster and ally focus the lowest-HP enemy (the victim).
        let focus_weakest =
            || TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc);
        a.gambit(caster, Node::act(focus_weakest(), nuke));
        a.gambit(ally, Node::act(focus_weakest(), jab));

        let mut combat = a.into_combat();
        let log = combat.run(5);

        // The ally's two 20-damage jabs (tick 1 and 2) kill the 25-HP victim
        // before the caster's tick-3 resolution, so the nuke fizzles on it.
        assert!(log.contains(&Event::Died(victim)));
        assert!(log.iter().any(|e| matches!(
            e,
            Event::Fizzled { actor, skill } if *actor == caster && *skill == nuke
        )));
        // The dead victim never absorbed the nuke's 50 damage — only the 20s.
        assert!(!log.iter().any(|e| matches!(
            e,
            Event::Damage { target, amount, .. } if *target == victim && *amount == 50.0
        )));
    }

    #[test]
    fn gambit_heals_self_when_hurt() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 25.0, 1.0); // below 30%
        let _enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        let heal = a.skill(Skill {
            name: "Heal".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(50.0)],
        });
        a.gambit(
            hero,
            Node::act(TargetQuery::new(Pool::Myself), heal).when(Condition::Exists(
                TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.3)),
            )),
        );

        let mut combat = a.into_combat();
        let log = combat.tick();

        assert!(log
            .iter()
            .any(|e| matches!(e, Event::Heal { target, .. } if *target == hero)));
        assert_eq!(combat.state.entity(hero).hp, 75.0);
    }
}
