# PLAN — de-messing combat: simple primitives for movement & feasibility

**Trigger:** the skirmish livelock. Mid-fight, the Raider and Shaman sat *ready-but-idle*
for 40+ consecutive ticks while oscillating ~0.3 units between two points, and red
collapsed. Diagnosis (traced, not guessed):

1. **Bang-bang movement.** Movement gambits are priority rules over hard distance
   thresholds (`Away` when a foe is within `KITE_RANGE 3.5`, `SeekHighGround` otherwise).
   A foe parked *on* the threshold flips which rule wins every tick → oscillation. This is
   structural: discrete rules over continuous space; every patch (dead-bands, guards,
   hysteresis) adds constants and state without removing the cliff edge.
2. **Ready but toothless.** The Shaman had a full action bar the whole time but no feasible
   action: its only attack needed line-of-sight, and…
3. **Blind at its own feet.** LoS rays start at ground elevation, so the Shaman's own
   hill-crown occluded the Brawler bashing it at the hill's base.

Fix = three ideas, ordered smallest-first, each independently shippable.

---

## Idea 3 — Eye height in line-of-sight  `[status: ✅ done]`

**What:** sight lines run between eye points at `tile elevation + EYE_HEIGHT` (1.0)
instead of at ground level.

**Why:** kills the whole "my own high ground blinds me to an adjacent lower unit" class of
degenerate blindness with one constant. Walls (elev ≥ 3) still block ground units; a unit
on an elev-2 crown now sees the melee diver at its base. Side effect (accepted, arguably
correct): elevation-1 bumps no longer block sight between ground units — knee-high cover
shouldn't blind.

**Changes:** `terrain.rs` — `EYE_HEIGHT` const, add it to both endpoints in
`line_of_sight`; doc update; regression test (unit on a crown sees a unit at the base —
the Shaman/Brawler geometry).

---

## Idea 2 — Everyone always has a swing  `[status: ✅ done]`

**What:** every kit contains a basic attack that is feasible at melee range (weapon-derived
once equipment exists; authored into the demo kits for now), and every gambit tree ends in
a use-it fallback. Plus a **systemic regression test**: no unit may stay ready-but-idle
(`Waited`) for a long stretch without net displacement — the livelock signature.

**Why:** "full bar, no feasible action, forever" becomes impossible by construction;
waiting becomes a deliberate `Commit` choice only. (FF12's quiet load-bearing feature:
everybody auto-attacks.)

**Changes:** `scenario.rs` — audit both scenarios' kits: the only dead-end was the
Assassin (Dash cd 12 + Backstab cd 2 → could idle with both on cooldown); give it a basic
`Strike` + fallback leaf. Livelock invariant test over the skirmish.

---

## Idea 1 — Positional scoring replaces directional move rules  `[status: ✅ done]`

**What:** the movement gambit becomes a **weighted sum of scoring terms** evaluated over
candidate stand-points (a small utility field), instead of a priority list of directional
intents:

```rust
struct MoveGambit { terms: Vec<(Term, f32)> }   // replaces Vec<MoveRule>

enum Term {
    Near(TargetQuery, f32), // peak score at ideal range; approach+standoff+retreat in ONE term
    AwayFrom(TargetQuery),  // farther is better, saturating (bounded flee, no corner-camping)
    HighGround,             // elevation scores
    SightOf(TargetQuery),   // stand where you can see the target (negative weight = hide!)
}
```

Evaluator (`eval::decide_move`): resolve each term's reference once → collect candidate
points (reachable tiles via `nav::reachable`, sorted for determinism; a sampled lattice on
flat arenas) → score every candidate as the weighted sum → **stickiness bonus** on the
current position (moving must *beat* standing still) → A\* toward the argmax
(`nav_toward`, unchanged).

**Why it kills the mess structurally:**
- Conflicting pulls **blend** into one argmax spot instead of alternating rule wins → no
  wobble, no hysteresis state; `decide_move` stays pure.
- `Near(enemy, 6.5)` *is* the kite band: pushes in when too far, backs off when dived,
  holds between. Deletes `KITE_RANGE`/`SHOT_RANGE` and their apologetic comments.
- `BreakLoS` falls out for free as `(SightOf(threat), -w)` — one primitive fewer.
- Player UX unchanged in spirit: presets ("Skirmisher", "Hold high ground") are weight
  bundles; target queries survive as the reference-picker inside terms.

**Changes:** `gambit.rs` (delete `MoveIntent`/`MoveRule`, add `Term`/`MoveGambit`),
`eval.rs` (rewrite `decide_move` + delete `flee`/`seek_high_ground`/`break_los`),
`combat.rs` (map type), `scenario.rs` (gambits as term bundles), test rewrites
(behavioural: pursuit, standoff-hold, retreat-when-dived, bounded flee, hill-climb),
CLAUDE.md movement/terrain sections.

---

## Acceptance (whole plan)

- [x] `cargo test` green.
- [x] Livelock invariant test passes: no skirmish unit waits 40+ consecutive ticks with
      < 1 unit of net movement.
- [x] `demo_mage_takes_actions` still passes (mage climbs the hill and fires over the wall
      under the new movement).
- [x] Both scenarios resolve; headless sanity-run shows red no longer folding to livelock.
- [x] CLAUDE.md updated to match (movement section, LoS note, gambit payoff list).

## Outcome notes (post-implementation)

- Eye height **alone** already defused the original livelock (the Shaman could suddenly see
  and shoot its diver), before Idea 1 even landed — confirming the compound diagnosis.
- One addition discovered during Idea 1: a small **travel-cost** penalty per world-unit to a
  candidate (`eval::TRAVEL_COST`). Without it, ties on an ideal-range ring resolved by scan
  order and the mover *orbited* its target instead of backing straight out. With it,
  near-equal spots resolve to the nearest; the standoff test walks retreat to convergence
  (settles in the band, then holds — provably no wobble).
- Headless skirmish after all three ideas: resolves in ~117 ticks (was ~210), red *wins*
  with all four alive — the formerly-livelocked Raider/Shaman fight the whole battle. Blue
  vs red balance is now a scenario-tuning question, not a systems bug.
- Deleted: `MoveIntent`, `MoveRule`, `eval::flee/seek_high_ground/break_los`,
  `KITE_RANGE`/`SHOT_RANGE` and their tuning comments. Added: `Term`, `MoveGambit`, one
  scoring loop, five knobs in `eval.rs`, `EYE_HEIGHT`, a universal `Strike`.
