# gambit

A 2D semi-turn-based RPG built around a modular **gambit system**: every character has an
action bar that fills over time (ATB-style), and when it's full the character selects an
action by walking a player-authored ruleset. Inspired by Final Fantasy XII gambits and
Dragon Age: Origins tactics, but deliberately more modular.

## Tech

- **Language:** Rust (edition 2024)
- **Engine:** [Macroquad](https://macroquad.rs/) — 2D game framework (`macroquad = "0.4"`).
  The combat *core* stays engine-agnostic; only `main.rs` (the viewer) touches Macroquad.

## Core principle: entities = equipment + rules

**An entity's identity is fully determined by its equipment and its gambit rules — there are
no classes or archetypes.** "Mage", "archer", "tank" are emergent labels for a bundle of gear
+ rules, never a type in the code. All stats (HP, ATB speed, move speed, ranges, cast times)
come from equipment; all behaviour comes from the gambit tree.

This is load-bearing and constrains every future feature: **do not** add systems that tune
behaviour per "entity type", and be suspicious of any balance mechanism that implicitly
reintroduces classes. Balance problems are solved with more *tools* (skills, equipment,
counter-rules), not with hardcoded, type-specific throttles. (This is exactly why the
per-archetype move budget below was rejected.)

## Code layout

The whole game core is currently **engine-agnostic** (no Macroquad types), so it's unit
testable in isolation. When rendering lands, keep engine types at the boundary and convert
(e.g. `battle::Pos` <-> `macroquad::Vec2`).

```
src/
  battle.rs   World state: Entity, Skill, Effect, Status, BattleState (+ arena bounds + optional
              Terrain, with flat-arena fallbacks for elevation/LoS/passability). No engine/AI deps.
  gambit.rs   The rule model: Node / Body / Condition / TargetQuery (a behaviour tree),
              plus MoveGambit / Term (positional-scoring movement — see "Movement").
  terrain.rs  Tile grid: passability, elevation, cliffs (walkability), and line-of-sight
              (eye-height rays). (unit tests)
  nav.rs      A* pathfinding + a reachability flood over the tile grid. Pure, engine-agnostic. (unit tests)
  eval.rs     decide(root, actor, state) -> Option<Action>: walk the action tree (LoS is an implicit
              feasibility check); decide_move(gambit, actor, state) -> Option<Pos>: score reachable
              stand points by the movement gambit's weighted terms and step toward the argmax,
              A*-routed around terrain. (unit tests)
  combat.rs   Combat: the ATB loop + movement + cast-time state machine + action resolution. (unit tests)
  scenario.rs Hand-built demo battle + demo map (temporary, until real encounters exist).
  main.rs     Macroquad viewer: feeds real frame time to Combat::step and draws combat.state
              verbatim — terrain (elevation/walls/pits), HP/action bars, movement + casting,
              intent lines, log. No render-side interpolation.
```

Run `cargo test` for the behaviour specs and `cargo run` for the live viewer
(Space = pause, R = restart). Only `main.rs` depends on Macroquad; everything else is
engine-agnostic and headless-testable.

## Combat loop (`combat.rs`)

`Combat { state, gambits, move_gambits, casts, time }` drives an ATB battle in **continuous
time measured in ticks**. `step(dt)` advances by a fractional tick; `tick()` = `step(1.0)` is
the exact-stepping form tests use. The viewer feeds real frame time
(`step(frame_seconds / TICK_INTERVAL)`) and draws `combat.state` verbatim — **the sim is the
single source of truth for rendering; there is no render-side interpolation, prediction, or
smoothing** (that layer existed once and caused sync bugs — don't reintroduce it; if something
looks choppy, make the *sim* quantity continuous instead). Only transient event vfx
(projectiles/pierce-beams) animate outside the sim.

**Continuous quantities** integrate over every `step` slice:

- **Move:** every alive, non-casting entity drifts `move_speed·dt` along its `move_gambit`
  (`decide_move`) — concurrent with the ATB, never move-*or*-act. Movement checks casting per
  slice, so a caster roots the instant its cast starts and stays rooted through the boundary
  the cast resolves on.
- **Projectiles in flight:** effects land on *impact*, never at fire. A hit on a target
  beyond `MELEE_RANGE` (3.0, per shot — a long-range skill fired point-blank connects
  instantly) spawns a `Flight` that homes on its target at `PROJECTILE_SPEED`; reaching the
  body applies the skill's effects there and then. Target dies first → the flight fizzles.
- **Dash lunges:** `Effect::Dash` gap-closers are continuous, **not teleports**: the actor
  rushes its mark at `DASH_SPEED` (committed — no gambit movement, ATB frozen), and the
  skill's damage/status land at contact (or when the travel budget runs out). Target dies
  mid-lunge → fizzle. `Acted` is emitted at commit; `Damage`/`Inflicted` arrive at landing.
- **Fill bars:** `action_bar += atb_speed·dt`, capped at `READY` (1.0). **Casting entities are
  frozen** (bar stays put until the cast resolves), stunned and dashing ones too.
- **MP regen:** `mp += mp_regen·dt`, capped at `max_mp`.

Crossing a **whole-tick boundary** fires the discrete phases, in order:

1. **Statuses:** apply DoT/regen pulses (Poison/Burn damage, Regen heal — per stack, per tick),
   then age every status and drop expired ones.
2. **Cooldowns:** decrement each entity's per-skill cooldowns.
3. **Resolve casts:** tick down each in-flight cast; a completed one resolves — re-validating
   its committed targets against the *current* world (alive + still in range) and **fizzling**
   if none survive. A caster that died mid-cast simply loses the cast.
4. **Act:** every non-casting entity with a full bar acts, fullest-bar-first (ties by id). For
   each, `decide()` walks its gambit; on an `Action` the bar resets to 0 and MP + cooldown are
   committed immediately — an **instant** skill (`cast_time == 0`) resolves now, a **cast-time**
   skill instead roots the actor into the `casts` map to resolve at a later boundary (step 3).
   On `None` the entity **waits** with its bar still full, re-evaluating next boundary.

All durations (cast times, cooldowns, status lifetimes, DoT cadence) stay authored in whole
ticks — the boundary is the game's heartbeat; continuity is only for what the eye tracks.

Battle ends when one whole team is dead; `run(max_ticks)` loops until then (the cap guards
against stalemates). Every tick returns a `Vec<Event>` log (Acted / Waited / Damage / Heal /
Inflicted / Died / Victory) for tests and, later, the UI.

**Resolution rules** (tunable constants at the top of `combat.rs`): weakness multiplier
`1.5×` when the target's `weaknesses` contains the skill's `damage_type`; per-stack DoT/regen
amounts. Resolution also pays MP cost and starts the skill's cooldown. Feasibility
(cooldown / MP / range / has-a-valid-target) is checked in `eval`, never hand-authored.

## The gambit system — design decisions

This is the heart of the game. The design below is settled; implement against it.

### Core insight: separate Condition / Target / Skill

FF12 rules have two parts: a fused `<target+condition>` and a `<skill>`
(e.g. `Foe: HP < 50% -> Fireball`). That fusion means the thing you *test* is forced to be
the thing you *hit*. We split it into **three** independent parts so the trigger and the
target can differ:

1. **Condition** — a boolean guard: should this rule fire at all?
2. **Target** — if it fires, what does it act on? (its own query)
3. **Skill** — what to do.

This unlocks rules FF12 can't express, e.g. "if *any* enemy is below 50% HP, fireball the
*highest-HP* enemy." Condition and target are different queries.

**Coupling:** support both modes — target = "the entity the condition matched" (the FF12
ergonomic common case) *and* target = a fresh independent query. Don't force the filter to
be written twice, but allow them to diverge.

### Target selection is itself modular

```
Target = Pool -> [Filter…] -> SortKey (+order) -> Pick
```

- **Pool:** enemies / allies / self / summons / everyone. (`self` is just another pool, so
  DA:O-style self-preservation rules fall out for free.)
- **Filter:** 0..n predicates, AND-ed (`hp% < 50`, `weak_to(Poison)`, `in_range`, `!is_self`).
- **SortKey (+ order):** hp, hp%, distance, threat, status stacks, action-bar fill, …
- **Pick:** First (= highest/lowest by sort) / Random-of-matching / All (AoE) / Nearest(n).

### Conditions: a Condition is a Target query wrapped in a quantifier

Don't build two DSLs. A condition is "run a target query and assert something about the
result set": `Exists(q)`, `Count(q) cmp n`, plus `Not` / `All` (AND) / `Any` (OR).

**Two distinct kinds of "multiple conditions" — keep them at different layers:**

1. **Multiple predicates on the *same* target** → these are **filters** on one query,
   AND-ed. "enemy below 50% HP **and** weak to poison" is one query with two filters —
   *not* two conditions. (Two separate conditions would wrongly match when one enemy is
   <50% and a *different* one is weak to poison; filters guarantee the same entity satisfies
   both, and the picked target provably satisfies all of them.)
2. **Conditions about *different* subjects / global state** → the rule-level `All`/`Any`
   combinators. "I'm below 30% HP **and** there are 3+ enemies."

Cap combinator nesting shallow (~1 level) — deep boolean trees become a programming language
nobody wants to configure. The escape valve is writing another rule.

### Contexts / nesting = a behavior tree

Rules live in a **tree**, not a flat list. An outer node is a *guard* (context) that scopes
child rules; children are only evaluated when the parent condition holds. This is a
behavior/decision tree.

```
Context: I am below 30% HP
├─ Context: 3+ enemies   → AoE heal / defensive stance
└─ Context: 1 enemy      → focus heal + counterattack
```

A tree is **not more expressive** than a flat list (any tree flattens by AND-ing ancestor
conditions into each leaf). It buys: **DRY** (write the guard once), **organization** (huge
once lists grow past ~30 rules), and one genuinely-new capability — **scoped fallthrough**.

**Key semantics decision — what happens when a context is entered but no child fires**
(e.g. condition true but the only skill is on cooldown):

- **Fallthrough** (default): keep searching siblings/outer rules — a gambit system should
  almost always produce *some* action.
- **Commit** (opt-in per context): stay in this context and *wait* rather than falling out.
  Lets you say "if below 30% HP, only ever consider defensive skills; if none available,
  do nothing" — impossible in a flat list. This is the payoff of the tree.

### Evaluation model

Depth-first walk of the tree when a character's action bar fills. At each node:
check `condition`; if false → skip to sibling. If true:
- **Act (leaf):** try to produce an action; **auto feasibility check** (cooldown / MP-cost /
  range / has-valid-target) — if feasible, done; else fall through.
- **Group (context):** recurse children in order; if a child yields an action, done; if none
  do → fall through (or, in `Commit` mode, stop and wait).

**Feasibility is implicit, never a hand-authored condition.** The engine skips rules whose
skill is on cooldown / unaffordable / has no valid target in range. Players must not have to
encode this plumbing.

A flat FF12-style list is just the root `Group` with all-`Act` children — the model
subsumes it, so a player who wants simplicity never nests.

### Proposed types (starting point)

```rust
struct Node {
    condition: Condition,      // guard; Always if empty
    body: Body,
}

enum Body {
    Act { target: TargetQuery, skill: SkillId },        // leaf
    Group { mode: GroupMode, children: Vec<Node> },     // context
}

enum GroupMode { Fallthrough, Commit }

enum Condition {
    Always,
    Exists(TargetQuery),
    Count { q: TargetQuery, cmp: Cmp, n: u32 },
    Not(Box<Condition>),
    All(Vec<Condition>),   // AND
    Any(Vec<Condition>),   // OR
}

struct TargetQuery {
    pool: Pool,
    filters: Vec<Filter>,
    sort: Option<(SortKey, Order)>,
    pick: Pick,            // First | Random | All | Nearest(n)
    // + a way to say "reuse the entity the condition matched"
}
```

### UX principle

The **model** is the powerful one; the **UI** decides how much to expose. Ship FF12-simple
presets ("nearest foe", "weakest foe", "lowest-HP ally") that expand into full
pool→filter→sort→pick queries, and hide the individual knobs behind an "advanced" mode. Cap
*displayed* nesting to ~2–3 levels even if the data model allows arbitrary depth.

## Movement & positioning (BUILT; movement gambit REVISED to positional scoring)

**Status:** implemented. Entities *move*: `move_speed` + a per-entity `MoveGambit` (weighted
positional-scoring terms — see the revised bullet below) drive continuous drift each tick, and
`cast_time` skills root the caster via a Casting state. `Entity.speed` was renamed `atb_speed`;
`BattleState.bounds` clamps drift to the arena. See the combat-loop steps above and
`combat.rs` / `eval.rs::decide_move`. The design that was built:

- **Flowy, continuous movement — decoupled from the action.** Movement is NOT a turn/action
  choice competing with skills (that makes it a dead option). Instead a unit *drifts* toward a
  desired position every tick at its `move_speed`, driven by its own lightweight **movement
  gambit**, while the ATB bar independently fills and fires skills. Move **and** pick skills,
  never move-*or*-pick. This decoupling is what makes movement worth having — and it's the
  reason we don't need a movement budget (see below).
- **Movement is positional *scoring*, not directional rules (REVISED).** The first build used
  a priority list of directional intents (`MoveToward`/`MoveAway` + threshold guards). That's
  bang-bang control over continuous space: a foe parked on a threshold flips which rule wins
  every tick and the unit oscillates (this livelocked ranged units mid-fight). Movement is a
  continuous optimization, so it now uses the matching primitive: a `MoveGambit` is a
  **weighted sum of scoring terms** (`Near(query, ideal_range)` / `AwayFrom(query)` /
  `HighGround` / `SightOf(query)`), evaluated over the reachable stand points
  (`nav::reachable` tiles; a sampled lattice on flat arenas). The evaluator adds a
  **stickiness** bonus to the current spot (a move must beat standing still — the stateless
  replacement for hysteresis) and a small **travel cost** (near-equal spots resolve to the
  nearest, killing orbit artifacts), then steps toward the argmax, A\*-routed. Conflicting
  pulls *blend into one best spot* instead of alternating rule wins — no wobble by
  construction. `Near(enemy, 6.5)` alone is approach + standoff + retreat (the whole kite
  band); a *negative* `SightOf` weight is "hide from the target" for free. Target queries
  survive unchanged as the reference-picker inside terms — the same pool→filter→sort→pick
  machinery picks what to position *relative to*. Knobs live in `eval.rs`
  (`STICKINESS`/`TRAVEL_COST`/`DIST_NORM`/`AWAY_RANGE`/`SEEK_RADIUS`).
- **Hitboxes (BUILT).** Every entity is a circle of one shared radius (`battle::ENTITY_RADIUS`,
  uniform for now — *not* a per-entity field; if size ever varies it should come from equipment,
  not a spawn literal). Units can't overlap and can't hang over the arena edge. After
  `decide_move` picks a destination, `combat.rs::resolve_collisions` clamps the *whole circle*
  inside `bounds` (`clamp_within`) and pushes the mover out of any other unit's circle to
  just-touching (only the mover is displaced; movers resolve one at a time in id order, so it's
  deterministic and terminating). This is the "don't obliviously stack up / walk off the map"
  spatial sanity — the always-on, never hand-authored kind. **True steering / local avoidance**
  (sliding around obstacles rather than just stopping at contact) is deliberately deferred to the
  terrain layer, which brings A* + steering; today a blocked mover simply halts at the contact
  point.
- **Cast-time / rooting.** Some skills have `cast_time > 0` (ticks). Selecting one puts the
  unit in a **Casting** state: rooted (movement suppressed — "stand still"), ATB not filling,
  not re-deciding, until it resolves. Idle drifts, Casting stands still — one small state
  machine shared with movement. Cast-time is the key counter to kiting: committing to a big
  skill = a vulnerability window. Decisions: **commit MP + cooldown at cast start**, **resolve
  at completion** (re-validate targets; fizzle if none still valid — this is the interrupt/
  counterplay engine); **not interruptible by plain damage**, but a future hard-CC status
  cancels a cast. Open: whether a unit may *abandon* a cast to flee (ship locked-in first).
- **Rejected: a movement budget / stamina.** Considered to stop infinite kiting, but dropped:
  (a) it throttles the *chaser* as much as the fleer, so it can gut melee; (b) tuning it
  per-archetype smuggles back a class system, violating the equipment+rules identity
  principle. "Never commits to an attack" is an authoring problem (a bad gambit), not a system
  one; solve real kiting with more *tools* (gap-closers, roots), not a systemic throttle.
- **`speed` naming:** DONE — `Entity.speed` was renamed `atb_speed` (the ATB fill rate) and a
  separate `move_speed` (world units drifted per tick) added.

## Terrain, height & navigation (BUILT — grid + A* + LoS; steering-smoothing & authoring deferred)

**Status:** implemented. `terrain.rs` holds the tile grid (passability, elevation, cliffs,
line-of-sight); `nav.rs` does A\* + a reachability flood; `eval.rs` routes movement through it and
treats LoS as an implicit feasibility check; the viewer shades terrain by elevation. `BattleState`
carries an `Option<Terrain>` — `None` is the old flat arena (everything passable, elevation 0,
unobstructed sight), so the pre-terrain behaviour and tests are unchanged. What's built vs. what was
deliberately left for later is called out inline below.

This makes the game a **tactics RPG** (Final Fantasy Tactics / Tactics Ogre lineage): fights
happen on terrain with obstacles and elevation, not a flat plane. It's the largest subsystem —
pulls in pathfinding, line-of-sight, terrain data, and terrain rendering.

Implementation notes / decisions made while building:
- **Line-of-sight is elevation-only, with eye height.** A tile blocks sight iff it rises above
  the straight line drawn between the two eye points — each eye sits `EYE_HEIGHT` (1.0) *above*
  its tile's elevation. Eye height matters: with ground-level rays, a unit on a hill crown was
  blinded to an adjacent lower unit by its own crown's edge (this livelocked a ranged unit being
  meleed at its hill's base). Consequences: walls (elev ≥ 3) still block ground units; elev-1
  bumps no longer block sight between ground units (knee-high cover shouldn't blind). Passability
  doesn't affect sight — you shoot *across* pits and *over* lower cover. Model a wall as a
  *tall, impassable* tile and a pit as a *low, impassable* one.
- **LoS is enforced as feasibility, not a hand-authored filter.** A skill's target set is gated by
  range **and** LoS together in `eval::candidates` (only when a skill range is supplied — conditions
  and movement queries are sightless). There is also a `Filter::HasLineOfSight` for use inside
  *conditions* (e.g. "flee if a foe that can see me exists").
- **New gambit material shipped:** filters `HasLineOfSight` / `OnHigherGround` (negate for
  lower-or-equal) and sort key `Elevation` (`Desc` = prefer high-ground targets). The original
  tile-seeking movement *intents* (`SeekHighGround`/`BreakLoS`) were superseded by the scoring
  terms: "seek high ground with a shot" = `HighGround` + `SightOf(q)` weights; "duck behind
  cover" = a *negative* `SightOf(threat)` weight. `InCover` was **not** built — it was
  under-specified; revisit with real cover authoring.
- **Steering is "follow the A\* waypoints" + the existing collision pass.** The mover steps toward
  the next waypoint centre of an A\* route to its chosen stand point. A terrain backstop in
  `combat::resolve_collisions` reverts a mover to its (valid) start if entity separation ever
  shoves it onto a wall/cliff. **True smooth steering / string-pulling** (cutting corners between
  waypoints rather than threading tile centres) is still deferred — movement threads centres for
  now, which is slightly robotic.
- **Terrain authoring** is still just data literals (`scenario::demo_terrain`); no editor/format yet.

- **Representation — tile grid, continuous units.** Terrain is a grid of tiles; each tile has
  an `elevation` and a `passable` flag (walls/pits impassable). Units keep **continuous** `Pos`
  and flowy movement — the grid is the *terrain*, not the unit positions (RTS-style: grid
  navigation underneath, smooth units on top). The playable arena bounds are the grid extent,
  so units can't drift off-screen. (Chosen over a continuous heightmap+navmesh, which is far
  more engineering, and over a pure tile-step grid, which would drop flowy movement.)
- **Navigation — A\* + steering.** Global routing is **A\*** over the tile grid (steering alone
  can't route around concave obstacles). A* yields waypoints; the **steering** layer follows
  them smoothly and does local avoidance (walls, other units). A* = *where* to go, steering =
  *how* to move there. Movement gambits still express only *preference* (the weighted scoring
  terms); the evaluator picks the stand point and navigation resolves the route to it.
- **Spatial sanity is implicit** — like feasibility. Pathfinding around obstacles, staying in
  bounds, and not overlapping allies are systemic and always-on; the player never hand-authors
  "don't run into walls". Getting cornered is still *possible* (a real tactical outcome when
  outplayed), just not obliviously self-inflicted.
- **Height — discrete tile elevations.** Adjacent tiles are walkable if their elevation delta
  is within a step threshold; a larger drop/rise is a **cliff** (impassable to walking, but you
  can see/shoot across it). Traversability is a property of the terrain + threshold, not of an
  "entity type" — consistent with the equipment+rules identity principle.
- **Height in combat = line-of-sight & range only (no stat bonuses).** High ground sees over
  lower obstacles and can shoot across gaps; a low unit behind cover may have no LoS.
  **LoS is a new implicit feasibility check** for ranged skills, alongside range/cooldown/cost:
  you can't target what you can't see. No flat accuracy/damage bonus for height — kept emergent
  from geometry; revisit only if positioning feels weak.
- **Gambit payoff — new spatial material** (the reason terrain is worth the cost):
  - Filters: `HasLineOfSight`, `OnHigherGround`, `InCover`, elevation comparisons.
  - Sort key: `Elevation` (e.g. "prefer the high-ground target", "seek the highest reachable tile").
  - Movement terms: `HighGround` + `SightOf(target)` ("perch where you can shoot"),
    negative `SightOf(threat)` ("break LoS from ranged threats").
- **Not doing (yet):** navmesh / continuous heightmap (the grid is enough); flight/teleport
  traversal; height stat bonuses; interior-cover authoring tools.

## Open questions / not yet built

- **`Pick::Random` is deterministic** (hashes actor + candidate set) to keep `decide()` pure.
  Swap for a seeded RNG threaded through `BattleState` when real randomness is wanted.
- **Rendering:** the viewer (`main.rs`) draws terrain in a **fake-depth oblique projection**
  (`View`): the ground plane stays top-down, but elevation lifts everything up-screen by
  `ELEV_LIFT` tiles per level — tiles show lifted top faces, exposed south faces (striated per
  level, sunlit lip, darkening toward the ground), rim lines on drop-off edges, and contact
  shadows under taller neighbours. Units/bars/vfx project through the same `View` (bilinear
  elevation smoothing, cliff-clamped, so units walk *up* steps instead of popping) and always
  draw after terrain, so nothing gameplay-relevant hides behind a wall — tall terrain only
  overlaps dead ground to its north. A full isometric/2.5D (FFT-style) projection remains a
  possible future upgrade if height still reads too weakly.
- **Terrain authoring is undecided** — how maps are defined (hand-authored data files, an
  in-engine editor, procedural). Not needed until we build the terrain layer.
