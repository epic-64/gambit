//! Hand-built demo battles for the renderer / headless runs. Temporary — this
//! goes away once real encounters (equipment + gambits + terrain) exist.

use std::collections::HashMap;

use crate::battle::*;
use crate::combat::Combat;
use crate::gambit::*;
use crate::terrain::{Terrain, Tile3};

fn push_skill(skills: &mut Vec<Skill>, s: Skill) -> SkillId {
    let id = SkillId(skills.len());
    skills.push(s);
    id
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
        hp,
        max_hp: hp,
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
    move_gambits.insert(
        mage,
        vec![
            MoveRule::new(MoveIntent::SeekHighGround(nearest_enemy())),
            MoveRule::new(MoveIntent::Away(nearest_enemy())),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
