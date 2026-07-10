//! Hand-built demo battles for the renderer / headless runs. Temporary — this
//! goes away once real encounters (equipment + gambits + terrain) exist.

use std::collections::HashMap;

use crate::battle::*;
use crate::combat::Combat;
use crate::gambit::*;
use crate::terrain::{Terrain, Tile3};

/// Global multiplier applied to every entity's spawn HP. Bumps battle length
/// (skills deal fixed damage) without touching per-entity balance.
const HP_SCALE: f32 = 3.0;

fn push_skill(skills: &mut Vec<Skill>, s: Skill) -> SkillId {
    let id = SkillId(skills.len());
    skills.push(s);
    id
}

/// The demo battles the viewer can pick between, as `(label, builder)` pairs.
/// The viewer lists these on its title screen; each builder produces a fresh
/// `Combat` so "restart" is just calling it again.
pub fn scenarios() -> Vec<(&'static str, fn() -> Combat)> {
    vec![
        ("Duel — Hero & Mage vs Goblin & Ogre (hill + wall)", demo as fn() -> Combat),
        ("Skirmish — 4v4 party battle (plateau + cover)", skirmish as fn() -> Combat),
    ]
}

/// A 2v2: Hero + Mage (players) vs Goblin + Ogre (enemies). Positions are
/// spread across the arena purely for display — nothing moves yet.
pub fn demo() -> Combat {
    let mut skills = Vec::new();
    // Short-range melee: the actor must close the distance before it can hit,
    // so movement actually matters.
    let attack = push_skill(
        &mut skills,
        Skill {
            name: "Attack".into(),
            cost: 0,
            range: 2.5,
            cooldown: 0,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(12.0)],
        },
    );
    // Long range but a 3-tick cast: the mage roots to fire it, which is the
    // window a chaser exploits (and a kited-away target can dodge by fizzle).
    let fireball = push_skill(
        &mut skills,
        Skill {
            name: "Fireball".into(),
            cost: 12,
            range: 100.0,
            cooldown: 3,
            cast_time: 3,
            damage_type: Some(DamageType::Fire),
            effects: vec![Effect::Damage(18.0)],
        },
    );
    let heal = push_skill(
        &mut skills,
        Skill {
            name: "Heal".into(),
            cost: 10,
            range: 100.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(40.0)],
        },
    );

    let mk = |id: usize,
              name: &str,
              team: Team,
              hp: f32,
              atb_speed: f32,
              move_speed: f32,
              x: f32,
              y: f32,
              weak: &[DamageType]| Entity {
        id: EntityId(id),
        name: name.into(),
        team,
        hp: hp * HP_SCALE,
        max_hp: hp * HP_SCALE,
        mp: 100,
        pos: Pos { x, y },
        statuses: Vec::new(),
        weaknesses: weak.to_vec(),
        skills: vec![attack, fireball, heal],
        cooldowns: HashMap::new(),
        atb_speed,
        move_speed,
        action_bar: 0.0,
    };

    let hero = EntityId(0);
    let mage = EntityId(1);
    let goblin = EntityId(2);
    let ogre = EntityId(3);
    //         id  name      team          hp     atb   move  x     y     weak
    let entities = vec![
        // Players start on the west side; enemies on the east. A wall splits the
        // field, so both must funnel through the southern gap — except the mage,
        // who can climb the hill and shoot over the wall.
        mk(0, "Hero", Team::Player, 80.0, 0.30, 0.40, 3.0, 9.5, &[]),
        mk(1, "Mage", Team::Player, 50.0, 0.22, 0.30, 2.5, 4.5, &[]),
        mk(2, "Goblin", Team::Enemy, 40.0, 0.28, 0.45, 17.0, 9.5, &[DamageType::Fire]),
        mk(3, "Ogre", Team::Enemy, 120.0, 0.18, 0.25, 17.0, 4.5, &[]),
    ];
    let terrain = demo_terrain();
    // The grid *is* the playable field — take bounds straight from its extent so
    // drift can't wander off the drawn map.
    let state = BattleState {
        bounds: terrain.world_extent(),
        entities,
        skills,
        terrain: Some(terrain),
    };

    let mut gambits = HashMap::new();

    // Hero: self-preserve first (Commit), else bash the nearest enemy.
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

    // Mage: fireball the highest-HP enemy, else plink.
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

    // --- movement gambits: run every tick, independent of the action bar ---
    let nearest_enemy = || TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc);

    let mut move_gambits = HashMap::new();
    // Melee closes on the nearest foe (A* routes it through the gap). The mage
    // seeks the high ground to shoot over the wall, kiting only if it can't climb.
    move_gambits.insert(hero, vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))]);
    // Mage: hold the high ground and shoot over the wall while it can see a foe;
    // only advance (toward the gap) when it has *no* line-of-sight to anyone —
    // otherwise it stays put and fires instead of wandering off its perch.
    let sees_a_foe =
        || Condition::Exists(TargetQuery::new(Pool::Enemies).filter(Filter::HasLineOfSight));
    move_gambits.insert(
        mage,
        vec![
            MoveRule::new(MoveIntent::SeekHighGround(nearest_enemy())),
            MoveRule::new(MoveIntent::Toward(nearest_enemy())).when(Condition::Not(Box::new(
                sees_a_foe(),
            ))),
        ],
    );
    move_gambits.insert(goblin, vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))]);
    move_gambits.insert(ogre, vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))]);

    Combat::new(state, gambits).with_movement(move_gambits)
}

/// The demo map: a 20×12 tile arena split by a north wall with a southern gap,
/// plus a stepped hill on the players' side whose crest rises *above* the wall —
/// so the high ground can see (and fire) over it while everyone else funnels
/// through the gap. Showcases pathfinding (routing around the wall), cliffs
/// (the hill's climbable steps vs. the impassable wall), and line-of-sight.
fn demo_terrain() -> Terrain {
    let mut t = Terrain::flat(20, 12, 1.0);
    let ground = |elevation| Tile3 { elevation, passable: true };

    // Dividing wall at column 10, rows 0..=7 — impassable, elevation 3. Rows
    // 8..=11 are left open as the gap.
    for r in 0..=7 {
        t.set(10, r, Tile3 { elevation: 3, passable: false });
    }

    // A stepped hill west of the wall, climbing 1→2→3→4 toward it. Each step is a
    // single elevation up (walkable); the crest (elevation 4) overtops the wall
    // (elevation 3), so a unit on top has line-of-sight across it.
    for r in 3..=6 {
        t.set(6, r, ground(1));
        t.set(7, r, ground(2));
        t.set(8, r, ground(3));
        t.set(9, r, ground(4));
    }

    t
}

/// A 4v4 party skirmish. Players field a **tanky brawler**, an **archer**, a
/// **mage** and a **healer**; the enemy fields a **heavy tank**, a squishy-diving
/// **assassin**, an **archer** and a **healer**. There are no classes in the code —
/// each of those labels is just a bundle of stats + a skill kit + gambit rules
/// (see CLAUDE.md). Everyone knows only their own kit, so feasibility (range /
/// cost / cooldown / line-of-sight) does all the routing; the gambits only state
/// intent ("focus the weakest foe", "heal the most-hurt ally", "seek high ground").
pub fn skirmish() -> Combat {
    let mut skills = Vec::new();
    // Melee: brawler / tank swing. Short range, so they must close in.
    let bash = push_skill(
        &mut skills,
        Skill {
            name: "Bash".into(),
            cost: 0,
            range: 2.5,
            cooldown: 0,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(16.0)],
        },
    );
    // Ranged physical: archers plink from afar, a short 1-tick cast (a small
    // draw window) but no MP or cooldown.
    let shot = push_skill(
        &mut skills,
        Skill {
            name: "Shot".into(),
            cost: 0,
            range: 9.0,
            cooldown: 0,
            cast_time: 1,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(11.0)],
        },
    );
    // Mage nuke: long range, big hit, but a 3-tick cast + cooldown + MP — the
    // classic commit/vulnerability window.
    let fireball = push_skill(
        &mut skills,
        Skill {
            name: "Fireball".into(),
            cost: 12,
            range: 100.0,
            cooldown: 3,
            cast_time: 3,
            damage_type: Some(DamageType::Fire),
            effects: vec![Effect::Damage(20.0)],
        },
    );
    // Healer's mend: map-wide range, instant. Healers also carry a Shot so they
    // still contribute (and can't stalemate) when nobody needs mending.
    let heal = push_skill(
        &mut skills,
        Skill {
            name: "Heal".into(),
            cost: 10,
            range: 100.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(38.0)],
        },
    );
    // Assassin's strike: melee, hits and leaves Poison ticking — punishes the
    // squishy it dives.
    let backstab = push_skill(
        &mut skills,
        Skill {
            name: "Backstab".into(),
            cost: 0,
            range: 2.5,
            cooldown: 2,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![
                Effect::Damage(13.0),
                Effect::Inflict {
                    kind: StatusKind::Poison,
                    stacks: 1,
                    duration: 4,
                },
            ],
        },
    );

    let mk = |id: usize,
              name: &str,
              team: Team,
              hp: f32,
              atb_speed: f32,
              move_speed: f32,
              x: f32,
              y: f32,
              known: Vec<SkillId>,
              weak: &[DamageType]| Entity {
        id: EntityId(id),
        name: name.into(),
        team,
        hp: hp * HP_SCALE,
        max_hp: hp * HP_SCALE,
        mp: 100,
        pos: Pos { x, y },
        statuses: Vec::new(),
        weaknesses: weak.to_vec(),
        skills: known,
        cooldowns: HashMap::new(),
        atb_speed,
        move_speed,
        action_bar: 0.0,
    };

    //          id  name        team          hp     atb   move  x     y     kit                weak
    let entities = vec![
        // Players muster on the west edge; the enemy on the east.
        mk(0, "Brawler", Team::Player, 150.0, 0.26, 0.42, 3.5, 7.0, vec![bash], &[]),
        mk(1, "Archer", Team::Player, 65.0, 0.30, 0.36, 2.0, 2.5, vec![shot], &[]),
        mk(2, "Mage", Team::Player, 50.0, 0.22, 0.30, 2.0, 11.0, vec![fireball, shot], &[]),
        mk(3, "Cleric", Team::Player, 60.0, 0.24, 0.34, 2.0, 7.0, vec![heal, shot], &[]),
        mk(4, "Ogre", Team::Enemy, 160.0, 0.20, 0.30, 20.5, 7.0, vec![bash], &[DamageType::Fire]),
        mk(5, "Assassin", Team::Enemy, 55.0, 0.34, 0.50, 22.0, 2.5, vec![backstab], &[]),
        mk(6, "Raider", Team::Enemy, 62.0, 0.30, 0.36, 22.0, 11.0, vec![shot], &[]),
        mk(7, "Shaman", Team::Enemy, 60.0, 0.24, 0.34, 22.0, 7.0, vec![heal, shot], &[DamageType::Holy]),
    ];
    let terrain = skirmish_terrain();
    let state = BattleState {
        bounds: terrain.world_extent(),
        entities,
        skills,
        terrain: Some(terrain),
    };

    // --- shared target-query building blocks ---
    let nearest_enemy = || TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc);
    let weakest_enemy = || TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc);
    let toughest_enemy = || TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Desc);
    // The most-hurt ally (self included) that is actually below ~70% — the heal's
    // "has a valid target" feasibility check makes the guard implicit.
    let hurt_ally = || {
        TargetQuery::new(Pool::Allies)
            .filter(Filter::HpPctBelow(0.7))
            .sort(SortKey::HpPct, Order::Asc)
    };
    // Heal the worst-off ally; if none needs it, fall through to plinking.
    let healer_gambit = |heal: SkillId, shot: SkillId| {
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(hurt_ally(), heal),
                Node::act(nearest_enemy(), shot),
            ],
        )
    };

    let mut gambits = HashMap::new();
    // Brawler / Ogre: wade in and bash whoever is closest.
    gambits.insert(EntityId(0), Node::act(nearest_enemy(), bash));
    gambits.insert(EntityId(4), Node::act(nearest_enemy(), bash));
    // Archers focus-fire the weakest foe to secure kills.
    gambits.insert(EntityId(1), Node::act(weakest_enemy(), shot));
    gambits.insert(EntityId(6), Node::act(weakest_enemy(), shot));
    // Mage: nuke the toughest foe (the fire-weak Ogre) if it can, else plink.
    gambits.insert(
        EntityId(2),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(toughest_enemy(), fireball),
                Node::act(nearest_enemy(), shot),
            ],
        ),
    );
    // Assassin dives the weakest player and poisons it.
    gambits.insert(EntityId(5), Node::act(weakest_enemy(), backstab));
    // Both healers mend-first.
    gambits.insert(EntityId(3), healer_gambit(heal, shot));
    gambits.insert(EntityId(7), healer_gambit(heal, shot));

    // --- movement gambits (run every tick, independent of the action bar) ---
    let sees_a_foe =
        || Condition::Exists(TargetQuery::new(Pool::Enemies).filter(Filter::HasLineOfSight));
    // Ranged units hold the high ground and shoot over cover; they only advance
    // toward the gap when they have no line-of-sight to anyone.
    let ranged_move = || {
        vec![
            MoveRule::new(MoveIntent::SeekHighGround(nearest_enemy())),
            MoveRule::new(MoveIntent::Toward(nearest_enemy()))
                .when(Condition::Not(Box::new(sees_a_foe()))),
        ]
    };

    let mut move_gambits = HashMap::new();
    // Melee closes on the nearest foe.
    move_gambits.insert(EntityId(0), vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))]);
    move_gambits.insert(EntityId(4), vec![MoveRule::new(MoveIntent::Toward(nearest_enemy()))]);
    // Ranged attackers (archers, mage) and healers alike seek high ground and
    // shoot/mend from range, advancing only when they've lost line-of-sight to
    // everyone. (A pure `Away` kite has no distance guard to express with the
    // current filters, so it just flees to a corner and stalemates — the
    // high-ground seek keeps ranged units engaged instead.)
    move_gambits.insert(EntityId(1), ranged_move());
    move_gambits.insert(EntityId(2), ranged_move());
    move_gambits.insert(EntityId(6), ranged_move());
    move_gambits.insert(EntityId(3), ranged_move());
    move_gambits.insert(EntityId(7), ranged_move());
    // The assassin dives the squishiest target directly.
    move_gambits.insert(EntityId(5), vec![MoveRule::new(MoveIntent::Toward(weakest_enemy()))]);

    Combat::new(state, gambits).with_movement(move_gambits)
}

/// The skirmish map: a 24×14 arena with **two hills** — one flanking each side of
/// the open central lane — plus a few **small rocks** at their bases. Each hill is
/// contested high ground (ranged units climb it to shoot across the lane); the
/// rocks are tall + impassable, so they break the sightlines of low units and
/// force paths around them without walling off the lanes.
fn skirmish_terrain() -> Terrain {
    let mut t = Terrain::flat(24, 14, 1.0);
    let rock = Tile3 { elevation: 3, passable: false };

    // A hill on each side of centre (players' west, enemies' east).
    add_hill(&mut t, 7);
    add_hill(&mut t, 16);

    // Small rocks scattered near each hill's base — cover and routing, not walls.
    t.set(7, 3, rock); // north of the west hill
    t.fill(5..=5, 10..=11, rock); // south-west of the west hill
    t.set(16, 3, rock); // north of the east hill
    t.fill(18..=18, 10..=11, rock); // south-east of the east hill

    t
}

/// Paint a climbable hill centred on column `cx`, rows 5..=9: an elevation-2
/// crown ringed by an elevation-1 skirt one tile wider on every side, so each
/// edge is a single (walkable) step up from the flat ground.
fn add_hill(t: &mut Terrain, cx: i32) {
    let ground = |elevation| Tile3 { elevation, passable: true };
    t.fill(cx - 1..=cx + 1, 6..=8, ground(2)); // crown
    t.fill(cx - 2..=cx + 2, 5..=5, ground(1)); // north skirt
    t.fill(cx - 2..=cx + 2, 9..=9, ground(1)); // south skirt
    t.fill(cx - 2..=cx - 2, 6..=8, ground(1)); // west skirt
    t.fill(cx + 2..=cx + 2, 6..=8, ground(1)); // east skirt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combat::Event;

    /// The wired-up demo (movement + cast-time skills together) runs to a
    /// decisive end, and units actually move off their start positions.
    #[test]
    fn demo_runs_to_completion_with_movement() {
        let mut combat = demo();
        let start: Vec<Pos> = combat.state.entities.iter().map(|e| e.pos).collect();

        combat.run(2000);

        assert!(combat.is_over(), "the demo battle should resolve");
        let moved = combat
            .state
            .entities
            .iter()
            .zip(&start)
            .any(|(e, s)| e.pos.x != s.x || e.pos.y != s.y);
        assert!(moved, "at least one unit should have drifted from its start");
    }

    /// Regression: the demo mage must actually *do something* — climb the hill
    /// and fire — not just shuffle on its perch forever. (It once stranded itself
    /// in a corner because a fleeing fallback undid every step it gained.)
    #[test]
    fn demo_mage_takes_actions() {
        let mut combat = demo();
        let mage = EntityId(1);
        let log = combat.run(2000);
        let mage_acted = log.iter().any(|e| {
            matches!(
                e,
                Event::StartedCast { actor, .. } | Event::Acted { actor, .. } if *actor == mage
            )
        });
        assert!(mage_acted, "the mage should take at least one action, not idle forever");
    }

    /// The 4v4 skirmish resolves and units leave their start positions.
    #[test]
    fn skirmish_runs_to_completion_with_movement() {
        let mut combat = skirmish();
        assert_eq!(combat.state.entities.len(), 8, "skirmish is a 4v4");
        let start: Vec<Pos> = combat.state.entities.iter().map(|e| e.pos).collect();

        combat.run(4000);

        assert!(combat.is_over(), "the skirmish should resolve");
        let moved = combat
            .state
            .entities
            .iter()
            .zip(&start)
            .any(|(e, s)| e.pos.x != s.x || e.pos.y != s.y);
        assert!(moved, "at least one unit should have drifted from its start");
    }

    /// Every scenario in the registry builds and its every entity has a gambit
    /// (an entity with no action gambit would just stand idle forever).
    #[test]
    fn all_scenarios_build_and_wire_every_entity() {
        for (label, build) in scenarios() {
            let combat = build();
            assert!(
                !combat.state.entities.is_empty(),
                "scenario '{label}' has no entities"
            );
            for e in &combat.state.entities {
                assert!(
                    combat.gambits.contains_key(&e.id),
                    "scenario '{label}': {} has no gambit",
                    e.name
                );
            }
        }
    }
}
