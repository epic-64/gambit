// The gambit/battle API surface is defined ahead of its consumers (many enum
// variants, filters, and builders aren't exercised until the game is built on
// top), so allow dead code crate-wide for now.
#![allow(dead_code)]

//! gambit — a 2D semi-turn-based RPG built around a modular gambit system.
//!
//! This binary exercises the gambit *core* (no Macroquad yet): it builds a
//! small battle, gives each character a gambit tree, and runs the ATB combat
//! loop to completion, printing the event log. See CLAUDE.md for the design
//! and `cargo test` for the behaviour specs.

mod battle;
mod combat;
mod eval;
mod gambit;

use battle::*;
use combat::{Combat, Event};
use gambit::*;
use std::collections::HashMap;

fn main() {
    let mut skills = Vec::new();
    let mut skill = |s: Skill| {
        let id = SkillId(skills.len());
        skills.push(s);
        id
    };
    let attack = skill(Skill {
        name: "Attack".into(),
        cost: 0,
        range: 100.0,
        cooldown: 0,
        damage_type: Some(DamageType::Physical),
        effects: vec![Effect::Damage(12.0)],
    });
    let fireball = skill(Skill {
        name: "Fireball".into(),
        cost: 12,
        range: 100.0,
        cooldown: 3,
        damage_type: Some(DamageType::Fire),
        effects: vec![Effect::Damage(18.0)],
    });
    let heal = skill(Skill {
        name: "Heal".into(),
        cost: 10,
        range: 100.0,
        cooldown: 0,
        damage_type: None,
        effects: vec![Effect::Heal(40.0)],
    });

    let mk = |id: usize, name: &str, team: Team, hp: f32, speed: f32, weak: &[DamageType]| Entity {
        id: EntityId(id),
        name: name.into(),
        team,
        hp,
        max_hp: hp,
        mp: 100,
        pos: Pos { x: 0.0, y: 0.0 },
        statuses: Vec::new(),
        weaknesses: weak.to_vec(),
        skills: vec![attack, fireball, heal],
        cooldowns: HashMap::new(),
        speed,
        action_bar: 0.0,
    };

    let hero = EntityId(0);
    let mage = EntityId(1);
    let goblin = EntityId(2);
    let ogre = EntityId(3);
    let entities = vec![
        mk(0, "Hero", Team::Player, 80.0, 0.30, &[]),
        mk(1, "Mage", Team::Player, 50.0, 0.22, &[]),
        mk(2, "Goblin", Team::Enemy, 40.0, 0.28, &[DamageType::Fire]),
        mk(3, "Ogre", Team::Enemy, 120.0, 0.18, &[]),
    ];
    let state = BattleState { entities, skills };

    let mut gambits = HashMap::new();

    // Hero: self-preserve first (Commit — no valid heal? then wait), else bash
    // the nearest enemy.
    gambits.insert(
        hero,
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::context(
                    Condition::Exists(TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.3))),
                    GroupMode::Commit,
                    vec![Node::act(TargetQuery::new(Pool::Myself), heal)],
                ),
                Node::act(
                    TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
                    attack,
                ),
            ],
        ),
    );

    // Mage: fireball the highest-HP enemy while it has any MP, else plink.
    gambits.insert(
        mage,
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(
                    TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Desc),
                    fireball,
                ),
                Node::act(TargetQuery::new(Pool::Enemies), attack),
            ],
        ),
    );

    // Enemies: focus-fire the lowest-HP player.
    let focus_weakest = || {
        Node::act(
            TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc),
            attack,
        )
    };
    gambits.insert(goblin, focus_weakest());
    gambits.insert(ogre, focus_weakest());

    let mut combat = Combat::new(state, gambits);
    let log = combat.run(500);

    let name = |id: EntityId| combat.state.entity(id).name.clone();
    let skill_name = |id: SkillId| combat.state.skill(id).name.clone();

    println!("=== Battle start ===");
    for ev in &log {
        match ev {
            Event::Acted { actor, skill, targets } => {
                let tnames: Vec<String> = targets.iter().map(|&t| name(t)).collect();
                println!("{} casts {} at {}", name(*actor), skill_name(*skill), tnames.join(", "));
            }
            Event::Waited(actor) => println!("{} waits.", name(*actor)),
            Event::Damage { target, amount, weakness } => {
                let tag = if *weakness { " (weakness!)" } else { "" };
                println!("  {} takes {amount:.0} damage{tag}", name(*target));
            }
            Event::Heal { target, amount } => {
                println!("  {} heals {amount:.0} HP", name(*target));
            }
            Event::Inflicted { target, kind, stacks } => {
                println!("  {} is afflicted with {kind:?} x{stacks}", name(*target));
            }
            Event::Died(target) => println!("  *** {} is defeated!", name(*target)),
            Event::Victory(team) => println!("=== {team:?} wins in {} ticks ===", combat.time),
        }
    }
}
