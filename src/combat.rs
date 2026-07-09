//! The combat loop: an ATB (active-time-battle) driver that fills action bars
//! over discrete ticks, asks each ready entity's gambit what to do via
//! [`decide`], and resolves the chosen action (damage, healing, statuses,
//! cooldowns). Engine-agnostic — no Macroquad — so the whole fight is testable.

use std::collections::HashMap;

use crate::battle::{BattleState, DamageType, EntityId, Effect, SkillId, Status, StatusKind, Team};
use crate::eval::{decide, Action};
use crate::gambit::Node;

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
    Died(EntityId),
    Victory(Team),
}

/// Owns the mutable battle plus each entity's gambit tree, and advances time.
pub struct Combat {
    pub state: BattleState,
    /// Each entity's ruleset, keyed by id. An entity with no gambit never acts.
    pub gambits: HashMap<EntityId, Node>,
    pub time: u32,
    over: bool,
}

impl Combat {
    pub fn new(state: BattleState, gambits: HashMap<EntityId, Node>) -> Self {
        Combat {
            state,
            gambits,
            time: 0,
            over: false,
        }
    }

    pub fn is_over(&self) -> bool {
        self.over
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

        // Fill bars, capped at READY so a waiting entity doesn't accumulate.
        for e in &mut self.state.entities {
            if e.is_alive() {
                e.action_bar = (e.action_bar + e.speed).min(READY);
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
                    // Spend the turn: reset the bar, then resolve.
                    self.state.entities[actor.0].action_bar = 0.0;
                    events.push(Event::Acted {
                        actor,
                        skill: action.skill,
                        targets: action.targets.clone(),
                    });
                    self.resolve(actor, &action, &mut events);
                    self.check_over(&mut events);
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

    // --- resolution ------------------------------------------------------

    fn resolve(&mut self, actor: EntityId, action: &Action, events: &mut Vec<Event>) {
        let skill = self.state.skill(action.skill).clone();

        // Pay the cost and start the cooldown.
        let a = &mut self.state.entities[actor.0];
        a.mp = a.mp.saturating_sub(skill.cost);
        if skill.cooldown > 0 {
            a.cooldowns.insert(action.skill, skill.cooldown);
        }

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
    }

    impl Arena {
        fn new() -> Self {
            Arena {
                state: BattleState {
                    entities: Vec::new(),
                    skills: Vec::new(),
                },
                gambits: HashMap::new(),
            }
        }

        fn skill(&mut self, s: Skill) -> SkillId {
            let id = SkillId(self.state.skills.len());
            self.state.skills.push(s);
            id
        }

        fn add(&mut self, name: &str, team: Team, hp: f32, speed: f32) -> EntityId {
            let id = EntityId(self.state.entities.len());
            self.state.entities.push(Entity {
                id,
                name: name.into(),
                team,
                hp,
                max_hp: 100.0,
                mp: 100,
                pos: Pos { x: 0.0, y: 0.0 },
                statuses: Vec::new(),
                weaknesses: Vec::new(),
                skills: Vec::new(),
                cooldowns: HashMap::new(),
                speed,
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

        fn into_combat(self) -> Combat {
            Combat::new(self.state, self.gambits)
        }
    }

    fn damage_skill(name: &str, amount: f32, dt: Option<DamageType>, cooldown: u32) -> Skill {
        Skill {
            name: name.into(),
            cost: 0,
            range: 1000.0,
            cooldown,
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
