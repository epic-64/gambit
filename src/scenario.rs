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

/// Every spawn's MP pool and per-tick regen (uniform for the demos — a real
/// game sources these from equipment/stats, per entity). Deliberately does NOT
/// cover ability costs at their cooldown cadence: the pool is a burst budget
/// that drains over a long fight, forcing units back onto their free basics
/// between windows. Abilities being rationed (not spammed at will) is the point.
const SPAWN_MP: f32 = 100.0;
const MP_REGEN: f32 = 2.0;

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
    // Expensive + a long cooldown: a nuke it commits to, not a rotation filler.
    let fireball = push_skill(
        &mut skills,
        Skill {
            name: "Fireball".into(),
            cost: 30,
            range: 100.0,
            cooldown: 8,
            cast_time: 3,
            damage_type: Some(DamageType::Fire),
            effects: vec![Effect::Damage(18.0)],
        },
    );
    // Costs more than regen returns per cooldown, so sustained healing drains
    // the pool — mending is triage, not a faucet.
    let heal = push_skill(
        &mut skills,
        Skill {
            name: "Heal".into(),
            cost: 25,
            range: 100.0,
            cooldown: 6,
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
        mp: SPAWN_MP,
        max_mp: SPAWN_MP,
        mp_regen: MP_REGEN,
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
    // Melee closes on the nearest foe (A* routes it through the gap).
    move_gambits.insert(hero, MoveGambit::toward(nearest_enemy()));
    // Mage: a standoff blend — hold ~7 units out, preferring high ground with a
    // sightline. The three pulls sum into one best perch (the hill crest, which
    // sees over the wall); no dedicated "seek high ground" rule needed.
    move_gambits.insert(
        mage,
        MoveGambit::new(vec![
            (Term::Near(nearest_enemy(), 7.0), 0.4),
            (Term::HighGround, 0.6),
            (Term::SightOf(nearest_enemy()), 1.0),
        ]),
    );
    move_gambits.insert(goblin, MoveGambit::toward(nearest_enemy()));
    move_gambits.insert(ogre, MoveGambit::toward(nearest_enemy()));

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
    // Universal basic attack: the swing every weapon grants (authored per-kit
    // until equipment exists). Free, no cooldown, melee reach — the guarantee
    // that a cornered unit is never ready-but-toothless. Kits whose own basics
    // are already always-feasible (Bash, Shot) don't need it.
    let strike = push_skill(
        &mut skills,
        Skill {
            name: "Strike".into(),
            cost: 0,
            range: 2.5,
            cooldown: 0,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(8.0)],
        },
    );
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
    // Mage nuke: long range, big hit, but a 3-tick cast + a long cooldown + a
    // hefty MP bite — the classic commit/vulnerability window, rationed to a
    // few casts before the pool runs dry and it falls back to plinking.
    let fireball = push_skill(
        &mut skills,
        Skill {
            name: "Fireball".into(),
            cost: 30,
            range: 100.0,
            cooldown: 8,
            cast_time: 3,
            damage_type: Some(DamageType::Fire),
            effects: vec![Effect::Damage(20.0)],
        },
    );
    // Mage's heavy nuke: a single-target ice lance — the biggest hit in the
    // game, bought with the longest commit (a 5-tick rooted cast, past even
    // Fireball's 3) and a deep MP bite. Five ticks is long enough for the fight
    // to move: the mark can die to focus fire (fizzle) or a diver can be on the
    // mage before it releases — the risk that prices the payload.
    let ice_lance = push_skill(
        &mut skills,
        Skill {
            name: "Ice Lance".into(),
            cost: 35,
            range: 100.0,
            cooldown: 14,
            cast_time: 5,
            damage_type: Some(DamageType::Ice),
            effects: vec![Effect::Damage(34.0)],
        },
    );
    // Archer's aimed shot: a 4-tick draw for roughly 2.5× a plink Shot. Range
    // 12 (not map-wide) so a mark that walks away mid-aim fizzles it — and the
    // gambit only takes the shot when no foe is within melee threat range,
    // because rooting for a full second with an assassin in your face is how
    // archers die.
    let snipe = push_skill(
        &mut skills,
        Skill {
            name: "Snipe".into(),
            cost: 20,
            range: 12.0,
            cooldown: 16,
            cast_time: 4,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(26.0)],
        },
    );
    // Healer's mend: map-wide range, instant. Healers also carry a Shot so they
    // still contribute (and can't stalemate) when nobody needs mending. The cost
    // outruns regen at the cooldown cadence, so sustained mending drains the
    // pool — a healer can no longer out-faucet steady focus fire forever.
    let heal = push_skill(
        &mut skills,
        Skill {
            name: "Heal".into(),
            cost: 25,
            range: 100.0,
            cooldown: 6,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(38.0)],
        },
    );
    // Assassin's strike: melee, hits and leaves Poison ticking — punishes the
    // squishy it dives. Cooldown + cost make it a rhythm hit woven between
    // Strikes, not the every-action default.
    let backstab = push_skill(
        &mut skills,
        Skill {
            name: "Backstab".into(),
            cost: 10,
            range: 2.5,
            cooldown: 6,
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
    // Brawler's charge: rush a foe up to 6m off, hit for moderate damage and stun
    // it for 1s (4 ticks). A gap-closer + hard CC — the tool that punishes kiting
    // by locking a fleeing target down. Long cooldown (3s / 12 ticks) + a real
    // MP cost so it's a signature opener, not a spammable stun-lock.
    let charge = push_skill(
        &mut skills,
        Skill {
            name: "Charge".into(),
            cost: 15,
            range: 6.0,
            cooldown: 12,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![
                Effect::Dash { max: 6.0 },
                Effect::Damage(14.0),
                Effect::Inflict {
                    kind: StatusKind::Stun,
                    stacks: 1,
                    duration: 4,
                },
            ],
        },
    );
    // Assassin's dash: a fast 4m gap-closer, moderate damage, and a 2s (8-tick)
    // snare (-60% move speed) so the squishy it dives can't simply kite back out.
    // Long cooldown (5s / 20 ticks) + cost so it's a periodic engage — dive, then
    // fight with backstab/strike — not a spammed re-dash every couple of actions.
    let dash = push_skill(
        &mut skills,
        Skill {
            name: "Dash".into(),
            cost: 20,
            range: 5.0,
            cooldown: 20,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![
                Effect::Dash { max: 4.0 },
                Effect::Damage(14.0),
                Effect::Inflict {
                    kind: StatusKind::Snare,
                    stacks: 1,
                    duration: 8,
                },
            ],
        },
    );
    // Assassin's finisher: +1% damage per 1% of the target's missing HP — the
    // base of 10 grows toward 20 as the mark bleeds out. Cheap and quick so
    // it's the kill-securing rhythm hit once a dive has done its work; the
    // gambit gates it to already-hurt targets where the scaling actually pays.
    let reap = push_skill(
        &mut skills,
        Skill {
            name: "Reap".into(),
            cost: 5,
            range: 2.5,
            cooldown: 4,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::ExecuteDamage(10.0)],
        },
    );
    // Cleric's group heal: mends every hurt ally within 8m at once, but costs a
    // 2-tick rooted cast and a deep MP bite — the tank-up button when the whole
    // party is bleeding, not a better single-target Heal (which stays instant
    // and cheaper for triage).
    let prayer = push_skill(
        &mut skills,
        Skill {
            name: "Prayer".into(),
            cost: 40,
            range: 8.0,
            cooldown: 14,
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Heal(24.0)],
        },
    );
    // Cleric's barrier: a 3s (12-tick) shield that halves incoming damage —
    // pre-mitigation to Heal's after-the-fact triage. Cooldown outlasts the
    // buff so it's a window, not a permanent state.
    let barrier = push_skill(
        &mut skills,
        Skill {
            name: "Barrier".into(),
            cost: 20,
            range: 100.0,
            cooldown: 16,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict {
                kind: StatusKind::Shield,
                stacks: 1,
                duration: 12,
            }],
        },
    );
    // Cleric's cleanse: strips every harmful status (the assassin's poison and
    // snare, a landed stun) off an ally. Cheap and fast — the counter-tool that
    // makes DoT/CC attrition answerable instead of inevitable.
    let purify = push_skill(
        &mut skills,
        Skill {
            name: "Purify".into(),
            cost: 10,
            range: 100.0,
            cooldown: 4,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Cleanse],
        },
    );
    // Shaman's drain: damage that returns half of what lands as self-healing —
    // the enemy healer sustains itself by hurting you. A 1-tick channel and a
    // real cooldown keep it a woven rhythm hit, not a free-sustain faucet.
    let siphon = push_skill(
        &mut skills,
        Skill {
            name: "Siphon".into(),
            cost: 15,
            range: 9.0,
            cooldown: 8,
            cast_time: 1,
            damage_type: None,
            effects: vec![Effect::Drain(12.0)],
        },
    );
    // Ogre's war cry: self-enrage (+50% outgoing damage for 3s). Fired only
    // once a foe is in reach so the window isn't wasted on the approach march.
    let war_cry = push_skill(
        &mut skills,
        Skill {
            name: "War Cry".into(),
            cost: 10,
            range: 100.0,
            cooldown: 24,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict {
                kind: StatusKind::Enrage,
                stacks: 1,
                duration: 12,
            }],
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
        mp: SPAWN_MP,
        max_mp: SPAWN_MP,
        mp_regen: MP_REGEN,
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
        mk(0, "Brawler", Team::Player, 150.0, 0.26, 0.42, 3.5, 7.0, vec![charge, bash], &[]),
        mk(1, "Archer", Team::Player, 65.0, 0.30, 0.36, 2.0, 2.5, vec![snipe, shot], &[]),
        mk(2, "Mage", Team::Player, 50.0, 0.22, 0.30, 2.0, 11.0, vec![ice_lance, fireball, shot], &[]),
        mk(3, "Cleric", Team::Player, 60.0, 0.24, 0.34, 2.0, 7.0, vec![prayer, heal, barrier, purify, shot], &[]),
        mk(4, "Ogre", Team::Enemy, 160.0, 0.20, 0.30, 20.5, 7.0, vec![war_cry, bash], &[DamageType::Fire]),
        mk(5, "Assassin", Team::Enemy, 55.0, 0.34, 0.50, 22.0, 2.5, vec![dash, reap, backstab, strike], &[]),
        mk(6, "Raider", Team::Enemy, 62.0, 0.30, 0.36, 22.0, 11.0, vec![snipe, shot], &[]),
        mk(7, "Shaman", Team::Enemy, 60.0, 0.24, 0.34, 22.0, 7.0, vec![heal, siphon, shot], &[DamageType::Holy]),
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
    // A foe engaging a teammate: any enemy standing in melee reach of an ally
    // other than the actor itself. Sorted by HP ascending so the peel goes for
    // the squishiest attacker first (the diving assassin, not the ogre) — and,
    // unlike a distance sort, the reference doesn't flip as the peeler runs.
    let ally_attacker = || {
        TargetQuery::new(Pool::Enemies)
            .filter(Filter::WithinDistanceOf(
                Box::new(
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::NotSelf)
                        .pick(Pick::All),
                ),
                3.0,
            ))
            .sort(SortKey::Hp, Order::Asc)
    };
    // The most-hurt ally (self included) that is actually below ~70% — the heal's
    // "has a valid target" feasibility check makes the guard implicit.
    let hurt_ally = || {
        TargetQuery::new(Pool::Allies)
            .filter(Filter::HpPctBelow(0.7))
            .sort(SortKey::HpPct, Order::Asc)
    };

    let mut gambits = HashMap::new();
    // Brawler: protect first — if a foe is on a teammate, charge it (the stun is
    // the peel) or bash it; only then fall through to the generic engage rules
    // (charge/bash the nearest foe). Feasibility (the charge's 6m range + its
    // cooldown, bash's melee reach) picks between all four implicitly, so while
    // the brawler is still marching toward the diver it keeps fighting whatever
    // is at hand instead of idling.
    gambits.insert(
        EntityId(0),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(ally_attacker(), charge),
                Node::act(ally_attacker(), bash),
                Node::act(nearest_enemy(), charge),
                Node::act(nearest_enemy(), bash),
            ],
        ),
    );
    // Ogre: roar once a foe is in reach (the enrage window opens exactly when
    // there's something to swing at), then wade in and bash whoever is closest.
    // War Cry's cooldown (24) outlasts the buff (12), so feasibility alone
    // paces the re-roar; no "am I already enraged?" plumbing needed.
    gambits.insert(
        EntityId(4),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(TargetQuery::new(Pool::Myself), war_cry).when(Condition::Exists(
                    TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistanceOf(
                        Box::new(TargetQuery::new(Pool::Myself)),
                        4.0,
                    )),
                )),
                Node::act(nearest_enemy(), bash),
            ],
        ),
    );
    // Archers: take the long aimed shot only while nobody threatens them up
    // close (rooting for the 4-tick draw with a foe in melee reach is how
    // archers die — the guard is the player-authored counterpart of the
    // engine's implicit feasibility), otherwise focus-fire plinks at the
    // weakest foe to secure kills.
    let no_foe_in_my_face = || {
        Condition::Not(Box::new(Condition::Exists(
            TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistanceOf(
                Box::new(TargetQuery::new(Pool::Myself)),
                4.0,
            )),
        )))
    };
    let archer_gambit = || {
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(weakest_enemy(), snipe).when(no_foe_in_my_face()),
                Node::act(weakest_enemy(), shot),
            ],
        )
    };
    gambits.insert(EntityId(1), archer_gambit());
    gambits.insert(EntityId(6), archer_gambit());
    // Mage: open the biggest commit on the biggest target — lance the toughest
    // foe, fall back to fireball (which the fire-weak Ogre dreads) while the
    // lance recharges, and plink when the MP pool runs dry.
    gambits.insert(
        EntityId(2),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(toughest_enemy(), ice_lance),
                Node::act(toughest_enemy(), fireball),
                Node::act(nearest_enemy(), shot),
            ],
        ),
    );
    // Assassin dives the weakest player: dash in (gap-close + snare so it can't
    // kite away) when off cooldown, then finish or wear down. Reap outranks
    // backstab but only against marks already under 65% HP — that's where its
    // missing-HP scaling beats backstab's flat hit + poison; on healthier
    // targets it falls through to backstab (stack the DoT first). Strike is the
    // always-feasible floor: with everything on cooldown it still swings
    // instead of idling with a full bar.
    gambits.insert(
        EntityId(5),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(weakest_enemy(), dash),
                Node::act(
                    TargetQuery::new(Pool::Enemies)
                        .filter(Filter::HpPctBelow(0.65))
                        .sort(SortKey::HpPct, Order::Asc),
                    reap,
                ),
                Node::act(weakest_enemy(), backstab),
                Node::act(nearest_enemy(), strike),
            ],
        ),
    );
    // Cleric: group-heal when the party (2+ allies) is bleeding, else triage the
    // worst-off ally; shield whoever is deepest in trouble before topping them
    // up is affordable again; strip the assassin's poison/snare off anyone
    // carrying it; and plink when nobody needs anything.
    let poisoned_ally = |kind: StatusKind| {
        TargetQuery::new(Pool::Allies)
            .filter(Filter::HasStatus(kind))
            .sort(SortKey::HpPct, Order::Asc)
    };
    gambits.insert(
        EntityId(3),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::HpPctBelow(0.7))
                        .pick(Pick::All),
                    prayer,
                )
                .when(Condition::Count {
                    q: TargetQuery::new(Pool::Allies)
                        .filter(Filter::HpPctBelow(0.7))
                        .pick(Pick::All),
                    cmp: Cmp::Ge,
                    n: 2,
                }),
                Node::act(hurt_ally(), heal),
                Node::act(
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::HpPctBelow(0.5))
                        .filter(Filter::Not(Box::new(Filter::HasStatus(StatusKind::Shield))))
                        .sort(SortKey::HpPct, Order::Asc),
                    barrier,
                ),
                Node::act(poisoned_ally(StatusKind::Poison), purify),
                Node::act(poisoned_ally(StatusKind::Snare), purify),
                Node::act(nearest_enemy(), shot),
            ],
        ),
    );
    // Shaman: mend-first like any healer, but its filler is the drain — hurting
    // the nearest player is also how it keeps itself topped up. Plain Shot
    // remains the floor while Siphon recharges.
    gambits.insert(
        EntityId(7),
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                Node::act(hurt_ally(), heal),
                Node::act(nearest_enemy(), siphon),
                Node::act(nearest_enemy(), shot),
            ],
        ),
    );

    // --- movement gambits (run every tick, independent of the action bar) ---
    // Ranged units hold a standoff band: `Near(ideal 6.5)` pushes in when out
    // of shot range (9) and backs off when dived — approach, standoff and
    // retreat in one smooth term, so there is no kite threshold to wobble on.
    // High ground and a sightline tip the choice of perch when the distance
    // term is near its peak.
    let ranged_move = || {
        MoveGambit::new(vec![
            (Term::Near(nearest_enemy(), 6.5), 1.0),
            (Term::HighGround, 0.5),
            (Term::SightOf(nearest_enemy()), 0.8),
        ])
    };

    let mut move_gambits = HashMap::new();
    // Brawler: the bodyguard pull. When a foe is on a teammate the heavier
    // `Near(attacker)` term dominates the blend and the brawler runs to the
    // peel (on the line between the two pulls, all of the weight difference
    // favours the attacker, so the argmax sits *at* the attacker); with nobody
    // diving, the term's query matches nothing and drops out, leaving plain
    // nearest-foe pursuit.
    move_gambits.insert(
        EntityId(0),
        MoveGambit::new(vec![
            (Term::Near(ally_attacker(), 0.0), 1.5),
            (Term::Near(nearest_enemy(), 0.0), 1.0),
        ]),
    );
    // Melee closes on the nearest foe.
    move_gambits.insert(EntityId(4), MoveGambit::toward(nearest_enemy()));
    // Ranged attackers (archers, mage) and healers alike hold the standoff band.
    move_gambits.insert(EntityId(1), ranged_move());
    move_gambits.insert(EntityId(2), ranged_move());
    move_gambits.insert(EntityId(6), ranged_move());
    move_gambits.insert(EntityId(3), ranged_move());
    move_gambits.insert(EntityId(7), ranged_move());
    // The assassin dives the squishiest target directly.
    move_gambits.insert(EntityId(5), MoveGambit::toward(weakest_enemy()));

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

    /// Protect regression: the skirmish brawler peels for dived teammates —
    /// over a full battle it attacks the enemy assassin (the backline diver)
    /// at least once instead of tunnelling on the frontline ogre forever.
    #[test]
    fn skirmish_brawler_peels_the_diver() {
        let mut combat = skirmish();
        let log = combat.run(4000);
        let (brawler, assassin) = (EntityId(0), EntityId(5));
        let peeled = log.iter().any(|e| matches!(
            e,
            Event::Acted { actor, targets, .. } if *actor == brawler && targets.contains(&assassin)
        ));
        assert!(peeled, "the brawler should attack the diving assassin at least once");
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

    /// The skirmish's new gap-closers actually get used: over a full battle the
    /// brawler charges (landing a Stun) and the assassin dashes (landing a Snare).
    #[test]
    fn skirmish_uses_charge_and_dash_with_their_cc() {
        let mut combat = skirmish();
        let log = combat.run(4000);

        let acted_skill = |name: &str| {
            log.iter().any(|e| matches!(
                e, Event::Acted { skill, .. } if combat.state.skill(*skill).name == name
            ))
        };
        let inflicted = |kind: StatusKind| {
            log.iter()
                .any(|e| matches!(e, Event::Inflicted { kind: k, .. } if *k == kind))
        };

        assert!(acted_skill("Charge"), "the brawler should charge at least once");
        assert!(acted_skill("Dash"), "the assassin should dash at least once");
        assert!(inflicted(StatusKind::Stun), "a charge should land a stun");
        assert!(inflicted(StatusKind::Snare), "a dash should land a snare");
    }

    /// The expanded kits actually see use over a full skirmish: the ogre roars,
    /// the shaman drains, the assassin executes, and the cleric group-heals.
    #[test]
    fn skirmish_expanded_kits_see_use() {
        let mut combat = skirmish();
        let log = combat.run(4000);

        let used = |name: &str| {
            log.iter().any(|e| matches!(
                e,
                Event::Acted { skill, .. } | Event::StartedCast { skill, .. }
                    if combat.state.skill(*skill).name == name
            ))
        };

        assert!(used("War Cry"), "the ogre should enrage once a foe closes in");
        assert!(used("Siphon"), "the shaman should weave its drain");
        assert!(used("Reap"), "the assassin should execute a hurt target");
        assert!(used("Prayer"), "the cleric should group-heal a bleeding party");
        assert!(used("Snipe"), "an archer should take the aimed shot while unthreatened");
        assert!(used("Ice Lance"), "the mage should commit to its heavy nuke");
    }

    /// Livelock invariant: no unit may sit *ready-but-idle* (`Waited`) for a
    /// long stretch while going nowhere. Waiting is fine while marching toward
    /// range (net displacement grows) or as a deliberate Commit choice — but
    /// "full bar + no action + no net movement, forever" is the wobble-livelock
    /// signature (the Raider/Shaman bug) and must never come back.
    #[test]
    fn no_unit_livelocks_ready_but_idle() {
        for (label, build) in scenarios() {
            let mut combat = build();
            let n = combat.state.entities.len();
            let mut streak = vec![0u32; n];
            // Position at the start of each unit's current wait-streak.
            let mut anchor: Vec<Pos> = combat.state.entities.iter().map(|e| e.pos).collect();

            for _ in 0..2000 {
                if combat.is_over() {
                    break;
                }
                let events = combat.tick();
                let mut waited = vec![false; n];
                for ev in &events {
                    if let Event::Waited(id) = ev {
                        waited[id.0] = true;
                    }
                }
                for i in 0..n {
                    if waited[i] {
                        streak[i] += 1;
                        let moved = combat.state.entities[i].pos.dist(anchor[i]);
                        assert!(
                            streak[i] < 40 || moved > 1.0,
                            "{label}: {} livelocked — waited {} consecutive ticks \
                             with only {moved:.2} units of net movement",
                            combat.state.entities[i].name,
                            streak[i],
                        );
                    } else {
                        streak[i] = 0;
                        anchor[i] = combat.state.entities[i].pos;
                    }
                }
            }
        }
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
