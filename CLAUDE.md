# gambit

A 2D semi-turn-based RPG built around a modular **gambit system**: every character has an
action bar that fills over time (ATB-style), and when it's full the character selects an
action by walking a player-authored ruleset. Inspired by Final Fantasy XII gambits and
Dragon Age: Origins tactics, but deliberately more modular.

## Tech

- **Language:** Rust (edition 2024)
- **Engine:** [Macroquad](https://macroquad.rs/) — 2D game framework. Not yet added to
  `Cargo.toml`; add `macroquad` as a dependency when we start on rendering/input.

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
