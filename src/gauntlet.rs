//! Gauntlet: a party-based game mode — your customized party faces an unbroken
//! sequence of enemy waves that grow in number and strength without end.
//!
//! Per wave the run offers a **choice of two encounters**, each rolled from a
//! seeded RNG: its own randomized arena (hills, walls, pits, boulders — always
//! validated connected) and its own enemy roster bought from a wave budget.
//! Between fights the party is prepared on a muster screen: every member either
//! runs a **behavior preset** (a named action-gambit + movement-gambit bundle,
//! combinations proven out in the demo scenarios) or a fully **custom** rule
//! tree authored in the gambit editor. No classes anywhere: heroes and foes
//! alike are stat blocks + kits + gambits (see CLAUDE.md).

use std::collections::HashMap;

use crate::battle::*;
use crate::combat::Combat;
use crate::gambit::*;
use crate::nav;
use crate::scenario::{push_skill, HP_SCALE, MP_REGEN, SPAWN_MP};
use crate::terrain::{Terrain, Tile3};

/// Title-screen label for the gauntlet (the viewer lists it after the scenarios).
pub const GAUNTLET_LABEL: &str = "Gauntlet — party run vs. endless escalating waves";

/// Each cleared wave adds this much base HP to every party member — the run's
/// only source of growth, so a seasoned party can weather deeper waves a fresh
/// one couldn't.
const GAUNTLET_HP_GROWTH: f32 = 8.0;
/// Extra enemy HP per wave beyond the first, as a fraction — so even the same
/// archetype grinds harder deep into a run (the budget adds *bodies*; this adds
/// *bulk*).
const GAUNTLET_ENEMY_HP_PER_WAVE: f32 = 0.06;
/// Never spawn more than this many foes at once — past it the arena just packs
/// up and the party dies to positioning, not difficulty. Deeper waves escalate
/// through per-wave HP growth instead.
const GAUNTLET_MAX_FOES: usize = 7;
/// Points a wave may spend on its roster (see [`Foe::cost`]).
fn wave_budget(wave: u32) -> i32 {
    wave as i32 + 4
}

// ---------------------------------------------------------------------------
// Deterministic RNG — the run owns one and every roll (arena, roster) draws
// from it, so a run is a pure function of its seed (same spirit as
// `Pick::Random`'s hashing: no hidden global randomness in the core).
// ---------------------------------------------------------------------------

/// xorshift64* — tiny, seedable, good enough for map rolls.
#[derive(Clone)]
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Rng {
        // splitmix64 scramble: adjacent seeds diverge fully, and no seed can
        // produce the all-zero state xorshift would stick at.
        let mut z = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((z ^ (z >> 31)) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..n` (0 when `n == 0`).
    fn range(&mut self, n: u32) -> u32 {
        if n == 0 { 0 } else { (self.next() % n as u64) as u32 }
    }

    /// True with probability `p`.
    fn chance(&mut self, p: f32) -> bool {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32 <= p
    }
}

// ---------------------------------------------------------------------------
// Enemy archetypes
// ---------------------------------------------------------------------------

/// The enemy archetypes the wave budget draws from, cheapest to deadliest. Each
/// is a stat block + kit + a fixed gambit shape (melee close-in or ranged
/// standoff) — never a class, just a recurring bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Foe {
    Grunt,
    Archer,
    Brute,
    Mage,
    Ogre,
}

impl Foe {
    /// Point cost against the wave budget.
    fn cost(self) -> i32 {
        match self {
            Foe::Grunt => 1,
            Foe::Archer => 2,
            Foe::Brute | Foe::Mage => 3,
            Foe::Ogre => 5,
        }
    }

    /// The wave this archetype starts appearing on — the escalation ladder:
    /// early waves are grunts, then archers, brutes, mages and finally ogres
    /// join the pool as the run goes deep.
    fn unlock_wave(self) -> u32 {
        match self {
            Foe::Grunt => 1,
            Foe::Archer => 2,
            Foe::Brute => 3,
            Foe::Mage => 4,
            Foe::Ogre => 6,
        }
    }

    fn is_ranged(self) -> bool {
        matches!(self, Foe::Archer | Foe::Mage)
    }

    pub fn name(self) -> &'static str {
        match self {
            Foe::Grunt => "Grunt",
            Foe::Archer => "Archer",
            Foe::Brute => "Brute",
            Foe::Mage => "Mage",
            Foe::Ogre => "Ogre",
        }
    }

    /// Spawn HP before the wave multiplier and [`HP_SCALE`].
    fn base_hp(self) -> f32 {
        match self {
            Foe::Grunt => 34.0,
            Foe::Archer => 42.0,
            Foe::Brute => 95.0,
            Foe::Mage => 46.0,
            Foe::Ogre => 150.0,
        }
    }
}

/// All archetypes, deadliest first — the order the budget prefers to spend in.
const FOE_LADDER: [Foe; 5] = [Foe::Ogre, Foe::Mage, Foe::Brute, Foe::Archer, Foe::Grunt];

/// Roll a wave's roster: spend the wave budget on the archetypes unlocked by
/// now. Mostly buys the deadliest affordable (monotone menace), with a random
/// swerve so the two offers of a wave — and reruns of a seed — field different
/// mixes. Capped at [`GAUNTLET_MAX_FOES`] bodies.
fn wave_foes(wave: u32, rng: &mut Rng) -> Vec<Foe> {
    let unlocked: Vec<Foe> = FOE_LADDER
        .into_iter()
        .filter(|f| wave >= f.unlock_wave())
        .collect();
    let mut budget = wave_budget(wave);
    let mut foes = Vec::new();
    while foes.len() < GAUNTLET_MAX_FOES {
        let affordable: Vec<Foe> =
            unlocked.iter().copied().filter(|f| f.cost() <= budget).collect();
        if affordable.is_empty() {
            break;
        }
        let pick = if rng.chance(0.55) {
            affordable[0] // ladder order: the deadliest affordable
        } else {
            affordable[rng.range(affordable.len() as u32) as usize]
        };
        foes.push(pick);
        budget -= pick.cost();
    }
    foes
}

// ---------------------------------------------------------------------------
// Random arenas
// ---------------------------------------------------------------------------

/// Columns kept feature-free on each side — the flat muster bands both teams
/// spawn in, so no roll can wall a team into its own corner.
const SPAWN_BAND: i32 = 5;

/// Where the party stands at wave start: the west muster band, spread around
/// mid-field. Shared with the viewer so encounter previews mark the true spots.
pub fn party_spawns(t: &Terrain, n: usize) -> Vec<Pos> {
    let mid = t.rows as f32 / 2.0;
    (0..n)
        .map(|i| {
            // 0, -3, +3, -6, … alternating offsets around the middle row.
            let off = ((i + 1) / 2) as f32 * 3.0 * if i % 2 == 0 { 1.0 } else { -1.0 };
            let y = (mid + off).clamp(1.5, t.rows as f32 - 1.5);
            Pos { x: 2.5 + (i % 2) as f32 * 0.7, y }
        })
        .collect()
}

/// Where a wave's foes muster: fanned out along the east band.
pub fn foe_spawns(t: &Terrain, n: usize) -> Vec<Pos> {
    let mid = t.rows as f32 / 2.0;
    (0..n)
        .map(|i| {
            let y = if n <= 1 {
                mid
            } else {
                1.5 + (t.rows as f32 - 3.0) * i as f32 / (n - 1) as f32
            };
            Pos { x: t.cols as f32 - 2.5 - (i % 2) as f32 * 1.5, y }
        })
        .collect()
}

/// Both muster bands can reach each other (and no roll may seal the field).
/// The bands themselves are untouched flat ground, so each is internally
/// connected — one probe tile per band suffices, but the fords a wall leaves
/// are also walked to catch a gap that opens onto a pit.
fn arena_connected(t: &Terrain) -> bool {
    let start = (2, t.rows / 2);
    let reach = nav::reachable(t, start, (t.cols * t.rows) as f32);
    reach.contains_key(&(t.cols - 3, t.rows / 2))
}

/// Roll a battlefield: terraced hills, an optional gapped wall, pit patches and
/// scattered boulders — all confined between the muster bands — retried until
/// the two bands connect (a fresh roll each attempt; flat fallback if the dice
/// stay hostile).
fn random_arena(rng: &mut Rng) -> Terrain {
    let rock = Tile3 { elevation: 3, passable: false };
    let pit = Tile3 { elevation: -2, passable: false };
    let ground = |elevation| Tile3 { elevation, passable: true };

    for _ in 0..16 {
        let cols = 20 + rng.range(7) as i32; // 20..=26
        let rows = 12 + rng.range(4) as i32; // 12..=15
        let mut t = Terrain::flat(cols, rows, 1.0);
        let (lo, hi) = (SPAWN_BAND, cols - SPAWN_BAND - 1); // feature columns

        // 0..=2 terraced hills: concentric single-step rings, so every tier is
        // walkable and the crown overlooks the field.
        for _ in 0..rng.range(3) {
            let tiers = 2 + rng.range(2) as i32; // 2..=3
            let ext = tiers - 1; // footprint half-extent
            if hi - lo < 2 * ext || rows - 4 < 2 * ext {
                continue;
            }
            let cx = lo + ext + rng.range((hi - lo - 2 * ext + 1) as u32) as i32;
            let cy = 1 + ext + rng.range((rows - 2 - 2 * ext) as u32) as i32;
            for k in 1..=tiers {
                let e = ext - (k - 1);
                t.fill(cx - e..=cx + e, cy - e..=cy + e, ground(k));
            }
        }

        // 40%: a dividing wall with a walkable gap — the funnel maps.
        if rng.chance(0.40) {
            let col = lo + 1 + rng.range((hi - lo - 1).max(1) as u32) as i32;
            let gap_len = 3 + rng.range(2) as i32;
            let gap_start = rng.range((rows - gap_len).max(1) as u32) as i32;
            for r in 0..rows {
                if r < gap_start || r >= gap_start + gap_len {
                    t.set(col, r, rock);
                }
            }
        }

        // Up to 2 pit patches — you shoot across them but walk around.
        for _ in 0..2 {
            if !rng.chance(0.45) {
                continue;
            }
            let w = 2 + rng.range(2) as i32;
            let h = 2 + rng.range(2) as i32;
            if hi - lo < w || rows - 2 <= h {
                continue;
            }
            let c0 = lo + rng.range((hi - lo - w + 2) as u32) as i32;
            let r0 = 1 + rng.range((rows - 1 - h) as u32) as i32;
            t.fill(c0..=c0 + w - 1, r0..=r0 + h - 1, pit);
        }

        // 2..=5 boulders: low-sightline cover, never a corridor by themselves.
        for _ in 0..(2 + rng.range(4)) {
            let c = lo + rng.range((hi - lo + 1) as u32) as i32;
            let r = 1 + rng.range((rows - 2) as u32) as i32;
            t.set(c, r, rock);
        }

        if arena_connected(&t) {
            return t;
        }
    }
    Terrain::flat(22, 12, 1.0)
}

// ---------------------------------------------------------------------------
// The party and its behavior presets
// ---------------------------------------------------------------------------

/// A hero's kit indexed by role, so one preset builder fits every member: a
/// preset only wires the rules whose role the kit actually carries, and always
/// ends on a guaranteed-feasible floor.
struct HeroKit {
    melee: Option<SkillId>,
    ranged: Option<SkillId>,
    heal: Option<SkillId>,
    /// The big committed hit (cast time and/or cooldown).
    nuke: Option<SkillId>,
}

impl HeroKit {
    fn known(&self) -> Vec<SkillId> {
        [self.nuke, self.heal, self.melee, self.ranged]
            .into_iter()
            .flatten()
            .collect()
    }
}

/// The fixed hero templates the party fields. Stats + kit only — behavior is
/// the player's to program, which is the whole game.
struct HeroTemplate {
    name: &'static str,
    hp: f32,
    atb: f32,
    mv: f32,
    kit_names: &'static [&'static str],
    default_preset: usize,
}

const HEROES: [HeroTemplate; 3] = [
    HeroTemplate {
        name: "Champion",
        hp: 110.0,
        atb: 0.30,
        mv: 0.42,
        kit_names: &["Strike", "Bolt", "Mend"],
        default_preset: PRESET_BRUISER,
    },
    HeroTemplate {
        name: "Ranger",
        hp: 70.0,
        atb: 0.30,
        mv: 0.38,
        kit_names: &["Snipe", "Shot"],
        default_preset: PRESET_SKIRMISHER,
    },
    HeroTemplate {
        name: "Sage",
        hp: 55.0,
        atb: 0.22,
        mv: 0.30,
        kit_names: &["Fireball", "Heal", "Shot"],
        default_preset: PRESET_MEDIC,
    },
];

pub const PRESET_BRUISER: usize = 0;
pub const PRESET_SKIRMISHER: usize = 1;
pub const PRESET_EXECUTIONER: usize = 2;
pub const PRESET_GUARDIAN: usize = 3;
pub const PRESET_MEDIC: usize = 4;

/// Display card of one behavior preset.
pub struct PresetInfo {
    pub name: &'static str,
    pub blurb: &'static str,
}

/// The preset catalog: named action + movement gambit bundles, each a pattern
/// the demo scenarios proved out (the brawler's peel, the archer's kite band,
/// the assassin's squishiest-frame hunt, the cleric's triage). The simplified
/// alternative to programming a member by hand.
pub const PRESETS: [PresetInfo; 5] = [
    PresetInfo {
        name: "Bruiser",
        blurb: "wade in and smash the nearest foe; mend when badly hurt",
    },
    PresetInfo {
        name: "Skirmisher",
        blurb: "hold a kite band, take the big shot when unthreatened, focus the weakest",
    },
    PresetInfo {
        name: "Executioner",
        blurb: "hunt the frailest frame on the field and strike from behind",
    },
    PresetInfo {
        name: "Guardian",
        blurb: "peel attackers off embattled teammates before picking own fights",
    },
    PresetInfo {
        name: "Medic",
        blurb: "keep the party healed from the backline; plink between mends",
    },
];

// --- shared target-query building blocks (used by presets and addons alike) ---

fn nearest_enemy() -> TargetQuery {
    TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)
}

fn weakest_enemy() -> TargetQuery {
    TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc)
}

fn squishiest_enemy() -> TargetQuery {
    TargetQuery::new(Pool::Enemies).sort(SortKey::MaxHp, Order::Asc)
}

/// A foe engaging a teammate — the peel trigger (see the skirmish brawler).
fn ally_attacker() -> TargetQuery {
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
}

/// The foe a nearby teammate is already hitting — converged fire kills.
fn allys_target() -> TargetQuery {
    TargetQuery::new(Pool::Enemies)
        .filter(Filter::TargetedBy(Box::new(
            TargetQuery::new(Pool::Allies)
                .filter(Filter::NotSelf)
                .filter(Filter::WithinDistance(8.0))
                .pick(Pick::All),
        )))
        .sort(SortKey::Hp, Order::Asc)
}

fn hurt_ally(pct: f32) -> TargetQuery {
    TargetQuery::new(Pool::Allies)
        .filter(Filter::HpPctBelow(pct))
        .sort(SortKey::HpPct, Order::Asc)
}

/// An enemy standing near *another* enemy — the clump a chain lightning pays on.
fn clustered_enemy() -> TargetQuery {
    TargetQuery::new(Pool::Enemies)
        .filter(Filter::WithinDistanceOf(
            Box::new(TargetQuery::new(Pool::Enemies).pick(Pick::All)),
            5.0,
        ))
        .sort(SortKey::MaxHp, Order::Asc)
}

/// The most-hurt ally carrying a given status — a cleanse's mark.
fn ally_with(status: StatusKind) -> TargetQuery {
    TargetQuery::new(Pool::Allies)
        .filter(Filter::HasStatus(status))
        .sort(SortKey::HpPct, Order::Asc)
}

/// Rooting into a long cast with a foe in your face is how ranged units die.
fn no_foe_in_my_face() -> Condition {
    Condition::Not(Box::new(Condition::Exists(
        TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistanceOf(
            Box::new(TargetQuery::new(Pool::Myself)),
            4.0,
        )),
    )))
}

/// Build a preset's gambits against a concrete kit. Rules whose role the kit
/// lacks drop out; every preset ends on whatever always-feasible floor the kit
/// has, so a full bar never idles.
fn preset_behavior(preset: usize, kit: &HeroKit) -> (Node, MoveGambit) {
    // Push a rule only when the kit carries the role.
    fn act(rules: &mut Vec<Node>, skill: Option<SkillId>, q: TargetQuery) {
        if let Some(s) = skill {
            rules.push(Node::act(q, s));
        }
    }

    let mut rules: Vec<Node> = Vec::new();
    let movement = match preset {
        PRESET_SKIRMISHER => {
            if let Some(s) = kit.nuke {
                rules.push(Node::act(weakest_enemy(), s).when(no_foe_in_my_face()));
            }
            act(&mut rules, kit.ranged, allys_target());
            act(&mut rules, kit.ranged, weakest_enemy());
            act(&mut rules, kit.melee, nearest_enemy());
            MoveGambit::new(vec![
                (Term::Near(nearest_enemy(), 6.5), 1.0),
                (Term::HighGround, 0.5),
                (Term::SightOf(nearest_enemy()), 0.8),
            ])
        }
        PRESET_EXECUTIONER => {
            act(&mut rules, kit.nuke, squishiest_enemy());
            act(&mut rules, kit.melee, squishiest_enemy());
            act(&mut rules, kit.ranged, squishiest_enemy());
            let ideal = if kit.melee.is_some() { 0.0 } else { 6.0 };
            MoveGambit::new(vec![
                (Term::Near(squishiest_enemy(), ideal), 1.2),
                (Term::Behind(squishiest_enemy()), 0.5),
            ])
        }
        PRESET_GUARDIAN => {
            act(&mut rules, kit.heal, hurt_ally(0.5));
            act(&mut rules, kit.melee, ally_attacker());
            act(&mut rules, kit.ranged, ally_attacker());
            act(&mut rules, kit.melee, nearest_enemy());
            act(&mut rules, kit.ranged, nearest_enemy());
            MoveGambit::new(vec![
                (Term::Near(ally_attacker(), 0.0), 1.5),
                (Term::Near(nearest_enemy(), 0.0), 1.0),
            ])
        }
        PRESET_MEDIC => {
            act(&mut rules, kit.heal, hurt_ally(0.7));
            if let Some(s) = kit.nuke {
                rules.push(Node::act(weakest_enemy(), s).when(no_foe_in_my_face()));
            }
            act(&mut rules, kit.ranged, allys_target());
            act(&mut rules, kit.ranged, nearest_enemy());
            act(&mut rules, kit.melee, nearest_enemy());
            MoveGambit::new(vec![
                (Term::Near(nearest_enemy(), 7.5), 1.0),
                (
                    Term::Near(
                        TargetQuery::new(Pool::Allies)
                            .filter(Filter::NotSelf)
                            .filter(Filter::HpPctBelow(0.8))
                            .sort(SortKey::HpPct, Order::Asc),
                        3.0,
                    ),
                    0.9,
                ),
                (Term::SightOf(nearest_enemy()), 0.5),
            ])
        }
        // PRESET_BRUISER and anything out of range.
        _ => {
            act(&mut rules, kit.heal, TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.45)));
            act(&mut rules, kit.melee, nearest_enemy());
            act(&mut rules, kit.nuke, nearest_enemy());
            act(&mut rules, kit.ranged, nearest_enemy());
            MoveGambit::toward(nearest_enemy())
        }
    };
    (
        Node::context(Condition::Always, GroupMode::Fallthrough, rules),
        movement,
    )
}

/// How one party member decides: a preset off the catalog, or a hand-authored
/// rule tree captured from the gambit editor.
#[derive(Clone)]
pub enum Behavior {
    Preset(usize),
    Custom { action: Node, movement: MoveGambit },
}

pub struct PartyMember {
    pub name: &'static str,
    pub behavior: Behavior,
    /// Equipped addon purchases — indices into [`CATALOG`]. Earlier entries
    /// take rule priority (their injected rules sit higher in the tree).
    pub addons: Vec<usize>,
}

// ---------------------------------------------------------------------------
// The upgrade shop: addons bought with points and equipped per member
// ---------------------------------------------------------------------------

/// A purchasable skill — pushed into every battle's skill list so an equip is
/// just "add to the member's kit + inject a sensible use-rule".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShopSkill {
    Charge,
    Barrier,
    Purify,
    ChainLightning,
    WarCry,
    Heal,
}

/// A purchasable tactic: a named rule- or movement-bundle (the strategies the
/// scenarios discovered), injected on top of the member's preset behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tactic {
    /// Attack the foe engaging a teammate first (the brawler's peel).
    PeelAllies,
    /// Join a nearby teammate's target (converged fire kills).
    FocusFire,
    /// Prioritize foes already bleeding out.
    ExecuteWeak,
    /// Movement: prefer high perches with a sightline.
    SeekHighGround,
    /// Movement: hold a standoff band instead of trading toe-to-toe.
    Kite,
    /// Movement: stick to whoever is beating on a teammate.
    Bodyguard,
    /// Movement: curve the approach behind the frailest foe.
    Flank,
}

impl Tactic {
    fn is_movement(self) -> bool {
        matches!(
            self,
            Tactic::SeekHighGround | Tactic::Kite | Tactic::Bodyguard | Tactic::Flank
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddonKind {
    Skill(ShopSkill),
    Tactic(Tactic),
}

/// One shop entry: what it costs and what it does.
pub struct Addon {
    pub name: &'static str,
    pub cost: u32,
    pub blurb: &'static str,
    pub kind: AddonKind,
}

impl Addon {
    /// Category tag for the shop UI.
    pub fn category(&self) -> &'static str {
        match self.kind {
            AddonKind::Skill(_) => "skill",
            AddonKind::Tactic(t) if t.is_movement() => "move",
            AddonKind::Tactic(_) => "tactic",
        }
    }
}

/// Everything money can buy. Skills grant a kit entry plus an auto-rule so the
/// purchase visibly *does something* out of the box; tactics graft proven rule
/// patterns (or movement pulls) onto the member's preset. Injected rules sit
/// *above* the preset's own, in purchase order — equipping is programming.
pub const CATALOG: [Addon; 13] = [
    Addon {
        name: "Charge",
        cost: 6,
        blurb: "gap-close + stun; rushes whoever is beating on a teammate, else the nearest foe",
        kind: AddonKind::Skill(ShopSkill::Charge),
    },
    Addon {
        name: "Barrier",
        cost: 5,
        blurb: "shield the most-hurt ally (halves damage for 3s)",
        kind: AddonKind::Skill(ShopSkill::Barrier),
    },
    Addon {
        name: "Purify",
        cost: 3,
        blurb: "cleanse poison and snares off allies",
        kind: AddonKind::Skill(ShopSkill::Purify),
    },
    Addon {
        name: "Chain Lightning",
        cost: 6,
        blurb: "arc a bolt through clumped foes",
        kind: AddonKind::Skill(ShopSkill::ChainLightning),
    },
    Addon {
        name: "War Cry",
        cost: 4,
        blurb: "self-enrage (+50% damage) once a foe is in reach",
        kind: AddonKind::Skill(ShopSkill::WarCry),
    },
    Addon {
        name: "Heal",
        cost: 5,
        blurb: "learn the triage mend (most-hurt ally first)",
        kind: AddonKind::Skill(ShopSkill::Heal),
    },
    Addon {
        name: "Peel Allies",
        cost: 3,
        blurb: "strike the foe engaging a teammate before anything else",
        kind: AddonKind::Tactic(Tactic::PeelAllies),
    },
    Addon {
        name: "Focus Fire",
        cost: 2,
        blurb: "join a nearby teammate's target (outranks the rules below it!)",
        kind: AddonKind::Tactic(Tactic::FocusFire),
    },
    Addon {
        name: "Execute the Weak",
        cost: 3,
        blurb: "finish foes under 35% HP before opening new wounds",
        kind: AddonKind::Tactic(Tactic::ExecuteWeak),
    },
    Addon {
        name: "Seek High Ground",
        cost: 2,
        blurb: "movement: prefer high perches that keep a sightline",
        kind: AddonKind::Tactic(Tactic::SeekHighGround),
    },
    Addon {
        name: "Kite",
        cost: 3,
        blurb: "movement: hold a 6.5-unit standoff band from the nearest foe",
        kind: AddonKind::Tactic(Tactic::Kite),
    },
    Addon {
        name: "Bodyguard",
        cost: 2,
        blurb: "movement: run to whoever is beating on a teammate",
        kind: AddonKind::Tactic(Tactic::Bodyguard),
    },
    Addon {
        name: "Flank",
        cost: 2,
        blurb: "movement: curve the approach behind the frailest foe",
        kind: AddonKind::Tactic(Tactic::Flank),
    },
];

/// Why a purchase was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuyError {
    NotEnoughPoints,
    AlreadyOwned,
}

/// The battle-local ids of the shop skills, resolved once per build.
struct ShopSkills {
    charge: SkillId,
    barrier: SkillId,
    purify: SkillId,
    chain: SkillId,
    war_cry: SkillId,
    heal: SkillId,
}

/// Insert rules at the *front* of a root's children (keeping their order), so
/// injected addon rules outrank the preset's own. A bare-leaf root is wrapped
/// into a group first.
fn prepend_rules(root: &mut Node, rules: Vec<Node>) {
    if rules.is_empty() {
        return;
    }
    if !matches!(root.body, Body::Group { .. }) {
        let leaf = std::mem::replace(
            root,
            Node::context(Condition::Always, GroupMode::Fallthrough, Vec::new()),
        );
        if let Body::Group { children, .. } = &mut root.body {
            children.push(leaf);
        }
    }
    if let Body::Group { children, .. } = &mut root.body {
        for r in rules.into_iter().rev() {
            children.insert(0, r);
        }
    }
}

/// Apply one equipped addon to a member being built: skills join `known`
/// always; rule/movement injection (`gambits`) only happens for preset-driven
/// members — a Custom tree already *contains* whatever was injected when it
/// was captured, so re-injecting would duplicate rules every wave. Custom
/// members pick up newly bought skills in their kit and wire them by hand.
fn apply_addon(
    addon: &Addon,
    kit: &HeroKit,
    shop: &ShopSkills,
    known: &mut Vec<SkillId>,
    gambits: Option<(&mut Node, &mut MoveGambit)>,
) {
    match addon.kind {
        AddonKind::Skill(s) => {
            let (skill, rules) = match s {
                ShopSkill::Charge => (
                    shop.charge,
                    vec![
                        Node::act(ally_attacker(), shop.charge),
                        Node::act(nearest_enemy(), shop.charge),
                    ],
                ),
                ShopSkill::Barrier => (
                    shop.barrier,
                    vec![Node::act(
                        TargetQuery::new(Pool::Allies)
                            .filter(Filter::HpPctBelow(0.65))
                            .filter(Filter::Not(Box::new(Filter::HasStatus(StatusKind::Shield))))
                            .sort(SortKey::HpPct, Order::Asc),
                        shop.barrier,
                    )],
                ),
                ShopSkill::Purify => (
                    shop.purify,
                    vec![
                        Node::act(ally_with(StatusKind::Poison), shop.purify),
                        Node::act(ally_with(StatusKind::Snare), shop.purify),
                    ],
                ),
                ShopSkill::ChainLightning => (
                    shop.chain,
                    vec![Node::act(clustered_enemy(), shop.chain)],
                ),
                ShopSkill::WarCry => (
                    shop.war_cry,
                    vec![Node::act(TargetQuery::new(Pool::Myself), shop.war_cry).when(
                        Condition::Exists(TargetQuery::new(Pool::Enemies).filter(
                            Filter::WithinDistanceOf(
                                Box::new(TargetQuery::new(Pool::Myself)),
                                4.0,
                            ),
                        )),
                    )],
                ),
                ShopSkill::Heal => (shop.heal, vec![Node::act(hurt_ally(0.6), shop.heal)]),
            };
            if !known.contains(&skill) {
                known.push(skill);
            }
            if let Some((action, _)) = gambits {
                prepend_rules(action, rules);
            }
        }
        AddonKind::Tactic(t) => {
            let Some((action, movement)) = gambits else {
                return;
            };
            // Push a rule per kit role that can serve the tactic (melee first —
            // feasibility picks whichever actually reaches).
            let with_kit = |q: fn() -> TargetQuery, action: &mut Node| {
                let mut rules = Vec::new();
                if let Some(m) = kit.melee {
                    rules.push(Node::act(q(), m));
                }
                if let Some(r) = kit.ranged {
                    rules.push(Node::act(q(), r));
                }
                prepend_rules(action, rules);
            };
            match t {
                Tactic::PeelAllies => with_kit(ally_attacker, action),
                Tactic::FocusFire => with_kit(allys_target, action),
                Tactic::ExecuteWeak => with_kit(
                    || {
                        TargetQuery::new(Pool::Enemies)
                            .filter(Filter::HpPctBelow(0.35))
                            .sort(SortKey::HpPct, Order::Asc)
                    },
                    action,
                ),
                Tactic::SeekHighGround => {
                    movement.terms.push((Term::HighGround, 0.6));
                    movement.terms.push((Term::SightOf(nearest_enemy()), 0.4));
                }
                Tactic::Kite => movement.terms.push((Term::Near(nearest_enemy(), 6.5), 1.2)),
                Tactic::Bodyguard => {
                    movement.terms.push((Term::Near(ally_attacker(), 0.0), 1.5));
                }
                Tactic::Flank => movement.terms.push((Term::Behind(squishiest_enemy()), 0.6)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Encounters and the run
// ---------------------------------------------------------------------------

/// One rolled fight the player may pick: its arena and its roster, fixed at
/// roll time so the preview is exactly the battle you get.
#[derive(Clone)]
pub struct Encounter {
    pub foes: Vec<Foe>,
    pub terrain: Terrain,
    /// The wave's enemy-HP multiplier, baked in so previews show true numbers.
    pub hp_mult: f32,
}

impl Encounter {
    /// Roster grouped for display: `(name, count, effective max HP each)`,
    /// deadliest first.
    pub fn foe_summary(&self) -> Vec<(&'static str, usize, f32)> {
        FOE_LADDER
            .into_iter()
            .filter_map(|kind| {
                let count = self.foes.iter().filter(|&&f| f == kind).count();
                (count > 0).then(|| (kind.name(), count, kind.base_hp() * self.hp_mult * HP_SCALE))
            })
            .collect()
    }

    pub fn total_enemy_hp(&self) -> f32 {
        self.foes
            .iter()
            .map(|f| f.base_hp() * self.hp_mult * HP_SCALE)
            .sum()
    }
}

/// One live gauntlet run, carried by the viewer across battles: the party (and
/// its programmed behaviors), the wave counter, and the current wave's two
/// rolled encounter offers.
pub struct GauntletRun {
    wave: u32,
    /// Waves banked so far — each one grows every member a little.
    cleared: u32,
    rng: Rng,
    pub party: Vec<PartyMember>,
    offers: Vec<Encounter>,
    chosen: usize,
    /// Unspent upgrade points — the run's currency for [`CATALOG`] addons.
    points: u32,
}

/// Points a fresh run starts with (enough for one small addon on wave 1).
const STARTING_POINTS: u32 = 3;

impl GauntletRun {
    /// A fresh run: wave 1, the stock party on its default presets, a small
    /// point stipend, and the first pair of offers rolled from `seed`.
    pub fn new(seed: u64) -> GauntletRun {
        let party = HEROES
            .iter()
            .map(|t| PartyMember {
                name: t.name,
                behavior: Behavior::Preset(t.default_preset),
                addons: Vec::new(),
            })
            .collect();
        let mut run = GauntletRun {
            wave: 1,
            cleared: 0,
            rng: Rng::new(seed),
            party,
            offers: Vec::new(),
            chosen: 0,
            points: STARTING_POINTS,
        };
        run.roll_offers();
        run
    }

    /// The wave the party is currently facing (1-indexed).
    pub fn wave(&self) -> u32 {
        self.wave
    }

    /// Unspent upgrade points.
    pub fn points(&self) -> u32 {
        self.points
    }

    /// Points awarded for clearing a given wave — deeper waves pay better, so
    /// the shop keeps pace with the escalating rosters.
    pub fn wave_award(wave: u32) -> u32 {
        2 + wave
    }

    /// Buy an addon (an index into [`CATALOG`]) and equip it on `member`.
    /// Injected rules take priority in purchase order; a member can own each
    /// addon once.
    pub fn buy(&mut self, member: usize, addon: usize) -> Result<(), BuyError> {
        if self.party[member].addons.contains(&addon) {
            return Err(BuyError::AlreadyOwned);
        }
        let cost = CATALOG[addon].cost;
        if self.points < cost {
            return Err(BuyError::NotEnoughPoints);
        }
        self.points -= cost;
        self.party[member].addons.push(addon);
        Ok(())
    }

    /// The current wave's rolled encounter offers (always two).
    pub fn offers(&self) -> &[Encounter] {
        &self.offers
    }

    /// Commit to one of the offers; [`build`](Self::build) then produces it.
    pub fn choose(&mut self, idx: usize) {
        self.chosen = idx.min(self.offers.len().saturating_sub(1));
    }

    /// The offer the run is committed to (what [`build`](Self::build) fields).
    pub fn chosen_encounter(&self) -> &Encounter {
        &self.offers[self.chosen]
    }

    /// Bank a cleared wave: the party grows a little, the wave's upgrade
    /// points are paid out, and the next, harder wave's offers are rolled.
    pub fn clear_wave(&mut self) {
        self.points += Self::wave_award(self.wave);
        self.cleared += 1;
        self.wave += 1;
        self.roll_offers();
    }

    /// Restart from wave 1 with a fresh set of offers. The party's programmed
    /// behaviors survive — they're the player's work, like gambit edits in
    /// scenario mode — but the growth resets with the waves.
    pub fn restart(&mut self) {
        self.wave = 1;
        self.cleared = 0;
        self.roll_offers();
    }

    fn roll_offers(&mut self) {
        let hp_mult = 1.0 + GAUNTLET_ENEMY_HP_PER_WAVE * (self.wave - 1) as f32;
        self.offers = (0..2)
            .map(|_| Encounter {
                foes: wave_foes(self.wave, &mut self.rng),
                terrain: random_arena(&mut self.rng),
                hp_mult,
            })
            .collect();
        self.chosen = 0;
    }

    /// A member's effective spawn max-HP (growth and [`HP_SCALE`] applied).
    pub fn member_hp(&self, i: usize) -> f32 {
        (HEROES[i].hp + GAUNTLET_HP_GROWTH * self.cleared as f32) * HP_SCALE
    }

    /// The display names of a member's kit.
    pub fn kit_names(i: usize) -> &'static [&'static str] {
        HEROES[i].kit_names
    }

    /// Step a member's behavior through the preset catalog (wrapping). A
    /// custom behavior re-enters the catalog at either end.
    pub fn cycle_preset(&mut self, member: usize, forward: bool) {
        let n = PRESETS.len();
        let next = match self.party[member].behavior {
            Behavior::Preset(p) => {
                if forward { (p + 1) % n } else { (p + n - 1) % n }
            }
            Behavior::Custom { .. } => {
                if forward { 0 } else { n - 1 }
            }
        };
        self.party[member].behavior = Behavior::Preset(next);
    }

    /// Adopt gambits edited in the editor: any party member whose live rules
    /// differ from what its current behavior would build becomes `Custom`,
    /// carrying the edited trees into every later wave. Members the editor
    /// didn't actually change keep their preset label.
    pub fn capture_custom(&mut self, combat: &Combat) {
        let baseline = self.build();
        for i in 0..self.party.len() {
            let id = EntityId(i);
            let (Some(action), Some(movement)) =
                (combat.gambits.get(&id), combat.move_gambits.get(&id))
            else {
                continue;
            };
            let unchanged = baseline
                .gambits
                .get(&id)
                .is_some_and(|b| format!("{b:?}") == format!("{action:?}"))
                && baseline
                    .move_gambits
                    .get(&id)
                    .is_some_and(|b| format!("{b:?}") == format!("{movement:?}"));
            if !unchanged {
                self.party[i].behavior = Behavior::Custom {
                    action: action.clone(),
                    movement: movement.clone(),
                };
            }
        }
    }

    /// Build the chosen encounter as a fresh `Combat`: the party (full HP,
    /// behaviors applied) mustered west, the rolled roster east, on the rolled
    /// arena. Pure — rebuilding (R) replays the exact same battle setup.
    pub fn build(&self) -> Combat {
        let enc = &self.offers[self.chosen];
        let mut skills = Vec::new();

        // --- the party's kits ---
        let strike = push_skill(
            &mut skills,
            Skill {
                name: "Strike".into(),
                cost: 0,
                range: 2.5,
                cooldown: 0,
                cast_time: 0,
                damage_type: Some(DamageType::Physical),
                effects: vec![Effect::Damage(10.0)],
            },
        );
        let bolt = push_skill(
            &mut skills,
            Skill {
                name: "Bolt".into(),
                cost: 0,
                range: 9.0,
                cooldown: 0,
                cast_time: 1,
                damage_type: Some(DamageType::Physical),
                effects: vec![Effect::Damage(11.0)],
            },
        );
        let mend = push_skill(
            &mut skills,
            Skill {
                name: "Mend".into(),
                cost: 25,
                range: 100.0,
                cooldown: 6,
                cast_time: 0,
                damage_type: None,
                effects: vec![Effect::Heal(45.0)],
            },
        );
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
        // Same order as HEROES / their kit_names.
        let kits = [
            HeroKit { melee: Some(strike), ranged: Some(bolt), heal: Some(mend), nuke: None },
            HeroKit { melee: None, ranged: Some(shot), heal: None, nuke: Some(snipe) },
            HeroKit { melee: None, ranged: Some(shot), heal: Some(heal), nuke: Some(fireball) },
        ];

        // --- the shop skills (always in the list; only equipped members know
        // them). Numbers mirror the skirmish versions they were proven in. ---
        let shop = ShopSkills {
            charge: push_skill(
                &mut skills,
                Skill {
                    name: "Charge".into(),
                    cost: 15,
                    range: 6.0,
                    cooldown: 24,
                    cast_time: 0,
                    damage_type: Some(DamageType::Physical),
                    effects: vec![
                        Effect::Dash { max: 6.0 },
                        Effect::Damage(12.0),
                        Effect::Inflict { kind: StatusKind::Stun, stacks: 1, duration: 4 },
                    ],
                },
            ),
            barrier: push_skill(
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
            ),
            purify: push_skill(
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
            ),
            chain: push_skill(
                &mut skills,
                Skill {
                    name: "Chain Lightning".into(),
                    cost: 25,
                    range: 100.0,
                    cooldown: 10,
                    cast_time: 2,
                    damage_type: Some(DamageType::Lightning),
                    effects: vec![Effect::ChainDamage {
                        base: 15.0,
                        jumps: 3,
                        falloff: 0.7,
                        jump_range: 5.0,
                    }],
                },
            ),
            war_cry: push_skill(
                &mut skills,
                Skill {
                    name: "War Cry".into(),
                    cost: 10,
                    range: 100.0,
                    cooldown: 64,
                    cast_time: 0,
                    damage_type: None,
                    effects: vec![Effect::Inflict {
                        kind: StatusKind::Enrage,
                        stacks: 1,
                        duration: 12,
                    }],
                },
            ),
            // The Heal addon teaches the same triage mend the Sage carries.
            heal,
        };

        // --- the enemy kits, shared across the archetypes that field them ---
        let claw = push_skill(
            &mut skills,
            Skill {
                name: "Claw".into(),
                cost: 0,
                range: 2.5,
                cooldown: 0,
                cast_time: 0,
                damage_type: Some(DamageType::Physical),
                effects: vec![Effect::Damage(9.0)],
            },
        );
        let bash = push_skill(
            &mut skills,
            Skill {
                name: "Bash".into(),
                cost: 0,
                range: 2.5,
                cooldown: 0,
                cast_time: 0,
                damage_type: Some(DamageType::Physical),
                effects: vec![Effect::Damage(11.0)],
            },
        );
        let foe_shot = push_skill(
            &mut skills,
            Skill {
                name: "Shot".into(),
                cost: 0,
                range: 9.0,
                cooldown: 0,
                cast_time: 1,
                damage_type: Some(DamageType::Physical),
                effects: vec![Effect::Damage(9.0)],
            },
        );
        let foe_fireball = push_skill(
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

        let mk = |id: usize,
                  name: &str,
                  team: Team,
                  hp: f32,
                  atb_speed: f32,
                  move_speed: f32,
                  pos: Pos,
                  known: Vec<SkillId>,
                  weak: &[DamageType]| Entity {
            id: EntityId(id),
            name: name.into(),
            team,
            hp,
            max_hp: hp,
            mp: SPAWN_MP,
            max_mp: SPAWN_MP,
            mp_regen: MP_REGEN,
            pos,
            statuses: Vec::new(),
            weaknesses: weak.to_vec(),
            skills: known,
            cooldowns: HashMap::new(),
            atb_speed,
            move_speed,
            action_bar: 0.0,
            focus: None,
        };

        let mut entities = Vec::new();
        let mut gambits = HashMap::new();
        let mut move_gambits = HashMap::new();

        // The party musters west: behaviors compiled to live gambits, then the
        // equipped addons grafted on. Addons of Custom members only extend the
        // kit — their captured tree already bakes in whatever was injected
        // when it was authored (re-injecting would duplicate rules per wave).
        for (i, (spot, (tpl, kit))) in party_spawns(&enc.terrain, self.party.len())
            .into_iter()
            .zip(HEROES.iter().zip(&kits))
            .enumerate()
        {
            let (mut action, mut movement, inject) = match &self.party[i].behavior {
                Behavior::Preset(p) => {
                    let (a, m) = preset_behavior(*p, kit);
                    (a, m, true)
                }
                Behavior::Custom { action, movement } => {
                    (action.clone(), movement.clone(), false)
                }
            };
            let mut known = kit.known();
            // Later purchases insert in front last, so the first-bought addon
            // ends up with the top rule priority.
            for &ai in self.party[i].addons.iter().rev() {
                let inj = inject.then(|| (&mut action, &mut movement));
                apply_addon(&CATALOG[ai], kit, &shop, &mut known, inj);
            }
            entities.push(mk(
                i,
                tpl.name,
                Team::Player,
                self.member_hp(i),
                tpl.atb,
                tpl.mv,
                spot,
                known,
                &[],
            ));
            gambits.insert(EntityId(i), action);
            move_gambits.insert(EntityId(i), movement);
        }

        // The rolled roster, fattened by the wave multiplier, fanned out east.
        let nearest_enemy =
            || TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc);
        for (i, (spot, &foe)) in foe_spawns(&enc.terrain, enc.foes.len())
            .into_iter()
            .zip(&enc.foes)
            .enumerate()
        {
            let id = self.party.len() + i;
            let (atb, mv, kit, weak): (f32, f32, Vec<SkillId>, &[DamageType]) = match foe {
                Foe::Grunt => (0.30, 0.46, vec![claw], &[]),
                Foe::Archer => (0.30, 0.34, vec![foe_shot], &[]),
                Foe::Brute => (0.20, 0.26, vec![bash], &[]),
                Foe::Mage => (0.22, 0.30, vec![foe_fireball, foe_shot], &[DamageType::Fire][..]),
                Foe::Ogre => (0.18, 0.24, vec![bash], &[]),
            };
            let hp = foe.base_hp() * enc.hp_mult * HP_SCALE;
            entities.push(mk(id, foe.name(), Team::Enemy, hp, atb, mv, spot, kit.clone(), weak));

            let gambit = match foe {
                Foe::Grunt => Node::act(nearest_enemy(), claw),
                Foe::Archer => Node::act(nearest_enemy(), foe_shot),
                Foe::Brute | Foe::Ogre => Node::act(nearest_enemy(), bash),
                Foe::Mage => Node::context(
                    Condition::Always,
                    GroupMode::Fallthrough,
                    vec![
                        Node::act(nearest_enemy(), foe_fireball),
                        Node::act(nearest_enemy(), foe_shot),
                    ],
                ),
            };
            gambits.insert(EntityId(id), gambit);
            let movement = if foe.is_ranged() {
                MoveGambit::new(vec![
                    (Term::Near(nearest_enemy(), 6.5), 1.0),
                    (Term::SightOf(nearest_enemy()), 0.6),
                ])
            } else {
                MoveGambit::toward(nearest_enemy())
            };
            move_gambits.insert(EntityId(id), movement);
        }

        let state = BattleState {
            bounds: enc.terrain.world_extent(),
            entities,
            skills,
            terrain: Some(enc.terrain.clone()),
        };
        Combat::new(state, gambits).with_movement(move_gambits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combat::Event;
    use crate::terrain::STEP_HEIGHT;

    /// A run advanced to `wave` (banking each earlier one), first offer chosen.
    fn run_at(seed: u64, wave: u32) -> GauntletRun {
        let mut run = GauntletRun::new(seed);
        for _ in 1..wave {
            run.clear_wave();
        }
        assert_eq!(run.wave(), wave);
        run
    }

    /// Every wave offers exactly two encounters, and each choice builds a
    /// battle with the full party (ids 0..3), at least one foe, and a gambit
    /// wired for every entity.
    #[test]
    fn every_wave_offers_two_buildable_encounters() {
        for wave in [1, 2, 3, 5, 8, 13, 20] {
            let mut run = run_at(7, wave);
            assert_eq!(run.offers().len(), 2, "wave {wave}: two offers");
            for choice in 0..2 {
                run.choose(choice);
                let combat = run.build();
                let players: Vec<&Entity> = combat
                    .state
                    .entities
                    .iter()
                    .filter(|e| e.team == Team::Player)
                    .collect();
                assert_eq!(players.len(), 3, "wave {wave}: the party is a trio");
                let foes = combat.state.entities.len() - players.len();
                assert!(foes >= 1, "wave {wave} offer {choice}: at least one foe");
                assert!(
                    foes <= GAUNTLET_MAX_FOES,
                    "wave {wave} offer {choice}: {foes} foes exceeds the crowd cap"
                );
                for e in &combat.state.entities {
                    assert!(
                        combat.gambits.contains_key(&e.id),
                        "wave {wave} offer {choice}: {} has no gambit",
                        e.name
                    );
                    assert!(
                        combat.move_gambits.contains_key(&e.id),
                        "wave {wave} offer {choice}: {} has no movement gambit",
                        e.name
                    );
                }
            }
        }
    }

    /// Runs are pure functions of their seed: two runs on the same seed roll
    /// identical offers (roster and arena tile-for-tile); a different seed
    /// diverges somewhere within a few waves.
    #[test]
    fn offers_are_deterministic_per_seed() {
        let terrain_eq = |a: &Terrain, b: &Terrain| {
            a.cols == b.cols
                && a.rows == b.rows
                && (0..a.rows)
                    .all(|r| (0..a.cols).all(|c| a.tile(c, r) == b.tile(c, r)))
        };
        let mut x = GauntletRun::new(42);
        let mut y = GauntletRun::new(42);
        for _ in 0..4 {
            for (a, b) in x.offers().iter().zip(y.offers()) {
                assert_eq!(a.foes, b.foes, "same seed, same rosters");
                assert!(terrain_eq(&a.terrain, &b.terrain), "same seed, same arenas");
            }
            x.clear_wave();
            y.clear_wave();
        }

        let z = GauntletRun::new(43);
        let x = GauntletRun::new(42);
        let differs = x.offers().iter().zip(z.offers()).any(|(a, b)| {
            a.foes != b.foes || !terrain_eq(&a.terrain, &b.terrain)
        });
        assert!(differs, "different seeds should roll different offers");
    }

    /// Every rolled arena lets both teams reach each other: from the party's
    /// spawn tile there is a walkable path to every foe spawn tile, and every
    /// spawn tile is passable ground.
    #[test]
    fn rolled_arenas_connect_the_muster_bands() {
        for seed in 0..30u64 {
            let mut run = run_at(seed, 3 + (seed % 6) as u32);
            for choice in 0..2 {
                run.choose(choice);
                let combat = run.build();
                let t = combat.state.terrain.as_ref().unwrap();
                let start = t.tile_of(combat.state.entities[0].pos);
                for e in &combat.state.entities {
                    let tile = t.tile_of(e.pos);
                    assert!(
                        t.passable(tile.0, tile.1),
                        "seed {seed}: {} spawns on impassable ground",
                        e.name
                    );
                    assert!(
                        nav::find_path(t, start, tile).is_some(),
                        "seed {seed}: no path from party spawn to {}",
                        e.name
                    );
                }
            }
        }
    }

    /// Rolled hills are climbable: any passable tile adjacent to a lower
    /// passable tile steps up by at most STEP_HEIGHT somewhere — i.e. no
    /// walkable crown is orphaned above sheer sides by construction. (Cliffs
    /// from overlapping features are allowed; this spot-checks the terraced
    /// hill painter on many seeds.)
    #[test]
    fn hill_tiers_step_up_walkably() {
        for seed in 0..10u64 {
            let run = run_at(seed, 5);
            let t = &run.offers()[0].terrain;
            for r in 0..t.rows {
                for c in 0..t.cols {
                    let tile = t.tile(c, r).unwrap();
                    if !tile.passable || tile.elevation <= 0 {
                        continue;
                    }
                    // Some neighbour must be walkable-adjacent (the terraces).
                    let climbable = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                        .iter()
                        .any(|&(dc, dr)| {
                            t.tile(c + dc, r + dr).is_some_and(|n| {
                                n.passable
                                    && (n.elevation - tile.elevation).abs() <= STEP_HEIGHT
                            })
                        });
                    assert!(climbable, "seed {seed}: tile ({c},{r}) is unreachable ground");
                }
            }
        }
    }

    /// The run escalates: total enemy HP of a deep wave's offers clearly
    /// outweighs an early wave's (budget and per-wave bulk both grow).
    #[test]
    fn waves_escalate_in_menace() {
        let early = run_at(11, 2).offers()[0].total_enemy_hp();
        let deep = run_at(11, 15).offers()[0].total_enemy_hp();
        assert!(
            deep > early * 1.5,
            "wave 15 ({deep}) should clearly outweigh wave 2 ({early})"
        );
    }

    /// Cleared waves grow the party: every member's spawn HP rises.
    #[test]
    fn party_grows_between_waves() {
        let fresh = run_at(5, 1);
        let seasoned = run_at(5, 6);
        for i in 0..3 {
            assert!(
                seasoned.member_hp(i) > fresh.member_hp(i),
                "member {i} should gain max HP as waves are cleared"
            );
        }
    }

    /// Every preset builds runnable behavior for every party member: the wave
    /// starts, plays out, and the member takes at least one action. Run on a
    /// flat arena so the spec tests the presets, not the map dice (a rolled
    /// wall can legitimately kill a blind unit before it ever fires).
    #[test]
    fn every_preset_works_on_every_member() {
        for preset in 0..PRESETS.len() {
            let mut run = run_at(9, 1);
            for offer in &mut run.offers {
                offer.terrain = Terrain::flat(22, 12, 1.0);
            }
            for i in 0..run.party.len() {
                run.party[i].behavior = Behavior::Preset(preset);
            }
            let mut combat = run.build();
            let log = combat.run(4000);
            assert!(combat.is_over(), "preset {preset}: the wave should resolve");
            for i in 0..run.party.len() {
                let id = EntityId(i);
                let acted = log.iter().any(|e| {
                    matches!(
                        e,
                        Event::Acted { actor, .. } | Event::StartedCast { actor, .. }
                            if *actor == id
                    )
                });
                assert!(
                    acted,
                    "preset {preset} ({}): member {i} never acted",
                    PRESETS[preset].name
                );
            }
        }
    }

    /// The default party clears wave 1 (the mode has to be beatable at the
    /// start, not an instant loss).
    #[test]
    fn default_party_survives_the_first_wave() {
        let run = run_at(1, 1);
        let mut combat = run.build();
        combat.run(8000);
        assert!(combat.is_over(), "wave 1 should resolve");
        let party_alive = combat
            .state
            .entities
            .iter()
            .any(|e| e.team == Team::Player && e.is_alive());
        assert!(party_alive, "the default party should clear wave 1");
    }

    /// Gambits edited in the editor are captured as Custom and survive into
    /// later waves; untouched members keep their preset label.
    #[test]
    fn editor_edits_are_captured_and_persist() {
        let mut run = run_at(3, 1);
        let mut combat = run.build();

        // "Edit" the Champion: strip its gambit down to a single rule.
        let strike = combat.state.entities[0].skills[0];
        let simple = Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![Node::act(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc),
                strike,
            )],
        );
        combat.gambits.insert(EntityId(0), simple.clone());
        run.capture_custom(&combat);

        assert!(
            matches!(run.party[0].behavior, Behavior::Custom { .. }),
            "the edited member becomes Custom"
        );
        assert!(
            matches!(run.party[1].behavior, Behavior::Preset(_)),
            "an untouched member keeps its preset"
        );

        // The edit survives a wave transition and lands in the next build.
        run.clear_wave();
        let next = run.build();
        assert_eq!(
            format!("{:?}", next.gambits[&EntityId(0)]),
            format!("{simple:?}"),
            "the captured rules should drive the next wave"
        );
    }

    /// Cycling presets wraps in both directions and re-enters from Custom.
    #[test]
    fn preset_cycling_wraps() {
        let mut run = run_at(2, 1);
        run.party[0].behavior = Behavior::Preset(0);
        run.cycle_preset(0, false);
        assert!(matches!(run.party[0].behavior, Behavior::Preset(p) if p == PRESETS.len() - 1));
        run.cycle_preset(0, true);
        assert!(matches!(run.party[0].behavior, Behavior::Preset(0)));

        run.party[0].behavior = Behavior::Custom {
            action: Node::context(Condition::Always, GroupMode::Fallthrough, vec![]),
            movement: MoveGambit::new(vec![]),
        };
        run.cycle_preset(0, true);
        assert!(matches!(run.party[0].behavior, Behavior::Preset(0)));
    }

    /// Clearing waves pays out points; buying deducts them and equips the
    /// addon; duplicates and overdrafts are refused.
    #[test]
    fn shop_economy_works() {
        let mut run = GauntletRun::new(4);
        assert_eq!(run.points(), STARTING_POINTS);
        run.clear_wave();
        assert_eq!(run.points(), STARTING_POINTS + GauntletRun::wave_award(1));

        let purify = CATALOG
            .iter()
            .position(|a| a.name == "Purify")
            .unwrap();
        let before = run.points();
        assert_eq!(run.buy(0, purify), Ok(()));
        assert_eq!(run.points(), before - CATALOG[purify].cost);
        assert!(run.party[0].addons.contains(&purify));
        assert_eq!(run.buy(0, purify), Err(BuyError::AlreadyOwned));

        // Drain the purse buying for another member, then a buy must bounce.
        for i in 0..CATALOG.len() {
            let _ = run.buy(1, i);
        }
        assert!(run.points() < 2, "the catalog should be able to drain the purse");
        let charge = CATALOG.iter().position(|a| a.name == "Charge").unwrap();
        assert_eq!(run.buy(2, charge), Err(BuyError::NotEnoughPoints));
    }

    /// A bought skill addon lands in the member's kit with its use-rule on top
    /// of the preset — and the skill actually fires over a battle.
    #[test]
    fn skill_addons_join_the_kit_and_get_used() {
        let mut run = run_at(8, 1);
        run.points = 100;
        let charge = CATALOG.iter().position(|a| a.name == "Charge").unwrap();
        run.buy(0, charge).unwrap();

        let mut combat = run.build();
        let champion = &combat.state.entities[0];
        let knows_charge = champion
            .skills
            .iter()
            .any(|&s| combat.state.skill(s).name == "Charge");
        assert!(knows_charge, "the Champion should know the bought Charge");

        let log = combat.run(6000);
        let charged = log.iter().any(|e| matches!(
            e,
            Event::Acted { actor, skill, .. }
                if *actor == EntityId(0) && combat.state.skill(*skill).name == "Charge"
        ));
        assert!(charged, "the bought Charge should see use in battle");
    }

    /// A movement tactic addon extends the member's movement gambit; an action
    /// tactic injects rules above the preset's own.
    #[test]
    fn tactic_addons_graft_onto_preset_behavior() {
        let mut run = run_at(8, 1);
        run.points = 100;
        let baseline = run.build();
        let base_terms = baseline.move_gambits[&EntityId(0)].terms.len();
        let base_rules = match &baseline.gambits[&EntityId(0)].body {
            Body::Group { children, .. } => children.len(),
            _ => panic!("preset root should be a group"),
        };

        let kite = CATALOG.iter().position(|a| a.name == "Kite").unwrap();
        let peel = CATALOG.iter().position(|a| a.name == "Peel Allies").unwrap();
        run.buy(0, kite).unwrap();
        run.buy(0, peel).unwrap();

        let built = run.build();
        assert!(
            built.move_gambits[&EntityId(0)].terms.len() > base_terms,
            "Kite should add movement terms"
        );
        match &built.gambits[&EntityId(0)].body {
            Body::Group { children, .. } => {
                assert!(children.len() > base_rules, "Peel Allies should add rules");
                // The peel rule sits above the preset's own rules.
                let first = &children[0];
                assert!(
                    matches!(&first.body, Body::Act { .. }),
                    "the injected rule is an action"
                );
            }
            _ => panic!("root should still be a group"),
        }
    }

    /// Addons never re-inject into a captured Custom tree (that would stack
    /// duplicate rules every wave) — but bought skills still join the kit.
    #[test]
    fn addons_do_not_duplicate_into_custom_behaviors() {
        let mut run = run_at(8, 1);
        run.points = 100;
        let peel = CATALOG.iter().position(|a| a.name == "Peel Allies").unwrap();
        let barrier = CATALOG.iter().position(|a| a.name == "Barrier").unwrap();
        run.buy(0, peel).unwrap();

        // Capture the Champion as Custom (simulate an editor edit).
        let mut combat = run.build();
        let rules_at_capture = match &combat.gambits[&EntityId(0)].body {
            Body::Group { children, .. } => children.len(),
            _ => panic!(),
        };
        let strike = combat.state.entities[0].skills[0];
        if let Body::Group { children, .. } = &mut combat.gambits.get_mut(&EntityId(0)).unwrap().body {
            children.push(Node::act(nearest_enemy(), strike));
        }
        run.capture_custom(&combat);
        assert!(matches!(run.party[0].behavior, Behavior::Custom { .. }));

        // Rebuild twice more: the rule count must stay fixed (edit + no dupes).
        for _ in 0..2 {
            let built = run.build();
            let rules = match &built.gambits[&EntityId(0)].body {
                Body::Group { children, .. } => children.len(),
                _ => panic!(),
            };
            assert_eq!(rules, rules_at_capture + 1, "no rule duplication per wave");
        }

        // A skill bought after the capture still joins the kit...
        run.buy(0, barrier).unwrap();
        let built = run.build();
        let knows_barrier = built.state.entities[0]
            .skills
            .iter()
            .any(|&s| built.state.skill(s).name == "Barrier");
        assert!(knows_barrier, "custom members still learn bought skills");
        // ...without touching the hand-authored tree.
        let rules = match &built.gambits[&EntityId(0)].body {
            Body::Group { children, .. } => children.len(),
            _ => panic!(),
        };
        assert_eq!(rules, rules_at_capture + 1);
    }

    /// A restarted run drops back to wave 1 and re-rolls offers but keeps the
    /// party's programmed behaviors.
    #[test]
    fn restart_keeps_behaviors_but_resets_growth() {
        let mut run = run_at(6, 4);
        run.party[2].behavior = Behavior::Custom {
            action: Node::context(Condition::Always, GroupMode::Fallthrough, vec![]),
            movement: MoveGambit::new(vec![]),
        };
        let grown = run.member_hp(0);
        run.restart();
        assert_eq!(run.wave(), 1);
        assert!(run.member_hp(0) < grown, "growth resets with the waves");
        assert!(
            matches!(run.party[2].behavior, Behavior::Custom { .. }),
            "authored behaviors survive a restart"
        );
        assert_eq!(run.offers().len(), 2);
    }

}
