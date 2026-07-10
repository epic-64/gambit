//! Engine-agnostic support for the gambit editor UI: human-readable
//! descriptions of gambit data, the preset catalogs the editor cycles
//! through, and structural edit operations on the rule tree (nodes addressed
//! by child-index paths from the root).
//!
//! Per the UX principle in CLAUDE.md, the editor never exposes the raw
//! pool/filter/sort/pick knobs: modification happens by cycling *presets*
//! (each expands to a full query/condition/term), while inspection renders
//! any hand-authored value faithfully via the describe functions. Rendering
//! and input live in `main.rs`; everything here is pure data-in/data-out and
//! headless-testable.

use crate::battle::{BattleState, DamageType, Effect, SkillId, StatusKind};
use crate::gambit::*;

// ---------------------------------------------------------------------------
// Descriptions: render any gambit value as compact, readable text
// ---------------------------------------------------------------------------

/// Format a number without a trailing `.0` when it's whole.
pub fn num(x: f32) -> String {
    if (x - x.round()).abs() < 1e-4 {
        format!("{x:.0}")
    } else {
        format!("{x:.1}")
    }
}

pub fn describe_pool(p: Pool) -> &'static str {
    match p {
        Pool::Enemies => "foes",
        Pool::Allies => "allies",
        Pool::Myself => "me",
        Pool::Everyone => "anyone",
        // The passthrough pool: act on whatever the node's condition matched.
        Pool::Matched => "= condition",
    }
}

pub fn describe_filter(f: &Filter) -> String {
    match f {
        Filter::HpPctBelow(x) => format!("hp < {}%", num(x * 100.0)),
        Filter::HpPctAbove(x) => format!("hp > {}%", num(x * 100.0)),
        Filter::HpBelow(x) => format!("hp < {}", num(*x)),
        Filter::HasStatus(k) => format!("has {k:?}"),
        Filter::StatusStacksAtLeast(k, n) => format!("{k:?} x{n}+"),
        Filter::WeakTo(dt) => format!("weak to {dt:?}"),
        Filter::IsSelf => "is me".into(),
        Filter::NotSelf => "not me".into(),
        Filter::HasLineOfSight => "in my sight".into(),
        Filter::OnHigherGround => "above me".into(),
        Filter::WithinDistance(d) => format!("within {} of me", num(*d)),
        Filter::WithinDistanceOf(q, d) => {
            format!("within {} of ({})", num(*d), describe_query(q))
        }
        Filter::Not(inner) => format!("not {}", describe_filter(inner)),
    }
}

pub fn describe_sort(key: SortKey, order: Order) -> String {
    let asc = order == Order::Asc;
    match key {
        SortKey::Distance => if asc { "nearest first" } else { "farthest first" }.into(),
        SortKey::Hp => if asc { "lowest hp first" } else { "highest hp first" }.into(),
        SortKey::HpPct => if asc { "most hurt first" } else { "healthiest first" }.into(),
        SortKey::MaxHp => if asc { "frailest first" } else { "toughest first" }.into(),
        SortKey::Mp => if asc { "driest mp first" } else { "fullest mp first" }.into(),
        SortKey::Elevation => {
            if asc { "lowest ground first" } else { "highest ground first" }.into()
        }
        SortKey::StatusStacks(k) => {
            if asc {
                format!("fewest {k:?} first")
            } else {
                format!("most {k:?} first")
            }
        }
    }
}

/// `Pick::First` is the implied default and renders as nothing.
pub fn describe_pick(p: Pick) -> Option<String> {
    match p {
        Pick::First => None,
        Pick::Take(n) => Some(format!("take {n}")),
        Pick::All => Some("all".into()),
        Pick::Random => Some("random".into()),
    }
}

pub fn describe_query(q: &TargetQuery) -> String {
    let mut parts: Vec<String> = vec![describe_pool(q.pool).into()];
    parts.extend(q.filters.iter().map(describe_filter));
    if let Some((k, o)) = q.sort {
        parts.push(describe_sort(k, o));
    }
    if let Some(p) = describe_pick(q.pick) {
        parts.push(p);
    }
    parts.join(", ")
}

fn cmp_sym(c: Cmp) -> &'static str {
    match c {
        Cmp::Lt => "<",
        Cmp::Le => "<=",
        Cmp::Eq => "=",
        Cmp::Ge => ">=",
        Cmp::Gt => ">",
    }
}

pub fn describe_condition(c: &Condition) -> String {
    match c {
        Condition::Always => "always".into(),
        Condition::Exists(q) => format!("exists: {}", describe_query(q)),
        Condition::Count { q, cmp, n } => {
            format!("#({}) {} {}", describe_query(q), cmp_sym(*cmp), n)
        }
        Condition::Not(inner) => format!("not [{}]", describe_condition(inner)),
        Condition::All(v) if v.is_empty() => "always".into(),
        Condition::All(v) => v
            .iter()
            .map(describe_condition)
            .collect::<Vec<_>>()
            .join(" AND "),
        Condition::Any(v) if v.is_empty() => "never".into(),
        Condition::Any(v) => v
            .iter()
            .map(describe_condition)
            .collect::<Vec<_>>()
            .join(" OR "),
    }
}

pub fn describe_term(t: &Term) -> String {
    match t {
        Term::Near(q, ideal) if *ideal <= 0.0 => format!("chase [{}]", describe_query(q)),
        Term::Near(q, ideal) => format!("hold {} from [{}]", num(*ideal), describe_query(q)),
        Term::AwayFrom(q) => format!("away from [{}]", describe_query(q)),
        Term::HighGround => "high ground".into(),
        Term::SightOf(q) => format!("sight of [{}]", describe_query(q)),
        Term::Crowd(q, r) => format!("amid [{}] (reach {})", describe_query(q), num(*r)),
    }
}

/// One editor row's text for a node: leaves read `skill -> target` (prefixed
/// by their guard when it isn't `Always`), groups read as a bracketed context.
pub fn describe_node(n: &Node, state: &BattleState) -> String {
    match row_parts(n, state) {
        RowParts::Leaf { condition, target, skill } => {
            let base = format!("{skill} -> {target}");
            if condition == "always" {
                base
            } else {
                format!("if {condition}:   {base}")
            }
        }
        RowParts::Group { condition, commit, children } => {
            let m = if commit { "commit" } else { "fallthrough" };
            let head = format!("group ({m}, {children} rules)");
            if condition == "always" {
                head
            } else {
                format!("if {condition}:   {head}")
            }
        }
    }
}

/// One line per thing a skill does when it resolves — the detail card's
/// "on use" section.
pub fn describe_effect(e: &Effect) -> String {
    match e {
        Effect::Damage(x) => format!("deal {} damage", num(*x)),
        Effect::ExecuteDamage(x) => {
            format!("execute: {} dmg, +1% per 1% missing hp", num(*x))
        }
        Effect::Drain(x) => format!("drain {} hp (half heals self)", num(*x)),
        Effect::Heal(x) => format!("heal {}", num(*x)),
        Effect::Cleanse => "cleanse all harmful statuses".into(),
        Effect::DrainMp(x) => format!("steal up to {} mp", num(*x)),
        Effect::ChainDamage { base, jumps, falloff, jump_range } => format!(
            "chain: {} dmg, {jumps} arcs, x{falloff} each, {} reach",
            num(*base),
            num(*jump_range)
        ),
        Effect::Inflict { kind, stacks, duration } => {
            format!("inflict {kind:?} x{stacks} for {duration} ticks")
        }
        Effect::Dash { max } => format!("dash to contact (max {})", num(*max)),
    }
}

/// The skill a leaf acts with; `None` on a group.
pub fn leaf_skill_id(n: &Node) -> Option<SkillId> {
    match &n.body {
        Body::Act { skill, .. } => Some(*skill),
        Body::Group { .. } => None,
    }
}

/// One rule row split into the editor's columns: every row has a condition
/// cell; a leaf adds target + skill cells, a group a mode/children cell.
pub enum RowParts {
    Leaf {
        condition: String,
        target: String,
        skill: String,
    },
    Group {
        condition: String,
        commit: bool,
        children: usize,
    },
}

pub fn row_parts(n: &Node, state: &BattleState) -> RowParts {
    let condition = describe_condition(&n.condition);
    match &n.body {
        Body::Act { target, skill } => RowParts::Leaf {
            condition,
            target: describe_query(target),
            skill: state.skill(*skill).name.clone(),
        },
        Body::Group { mode, children } => RowParts::Group {
            condition,
            commit: *mode == GroupMode::Commit,
            children: children.len(),
        },
    }
}

// ---------------------------------------------------------------------------
// Preset catalogs, structured as dropdown menus: top-level entries are either
// a directly pickable value or a labelled submenu of values. The flat
// `*_presets()` lists (used by the cycle functions) are derived from these,
// so the menus are the single source of truth for the editor's vocabulary.
// ---------------------------------------------------------------------------

/// One entry of a dropdown menu: a pickable value, or a one-level-deep
/// submenu of pickable values.
pub enum MenuEntry<T> {
    Item(String, T),
    Sub(String, Vec<(String, T)>),
}

impl<T> MenuEntry<T> {
    pub fn label(&self) -> &str {
        match self {
            MenuEntry::Item(l, _) | MenuEntry::Sub(l, _) => l,
        }
    }
}

fn item<T>(label: &str, v: T) -> MenuEntry<T> {
    MenuEntry::Item(label.into(), v)
}

fn sub<T>(label: &str, items: Vec<(&str, T)>) -> MenuEntry<T> {
    MenuEntry::Sub(
        label.into(),
        items.into_iter().map(|(l, v)| (l.to_string(), v)).collect(),
    )
}

/// Every pickable value of a menu, in display order (submenus inlined).
pub fn flatten_menu<T: Clone>(menu: &[MenuEntry<T>]) -> Vec<(String, T)> {
    let mut out = Vec::new();
    for e in menu {
        match e {
            MenuEntry::Item(l, v) => out.push((l.clone(), v.clone())),
            MenuEntry::Sub(_, items) => out.extend(items.iter().cloned()),
        }
    }
    out
}

fn nearest_foe() -> TargetQuery {
    TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)
}

fn nearest_other_ally() -> TargetQuery {
    TargetQuery::new(Pool::Allies)
        .filter(Filter::NotSelf)
        .sort(SortKey::Distance, Order::Asc)
}

/// A foe engaging a teammate: an enemy within melee reach of an ally other
/// than the actor (the protect/peel reference used across the scenarios).
fn foe_on_ally() -> TargetQuery {
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

pub fn target_menu() -> Vec<MenuEntry<TargetQuery>> {
    vec![
        // The passthrough: hit exactly what the condition matched (the FF12
        // ergonomic case), so the query isn't written twice.
        item(
            "same as condition",
            TargetQuery::new(Pool::Matched),
        ),
        sub(
            "foes",
            vec![
                ("nearest foe", nearest_foe()),
                (
                    "weakest foe",
                    TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc),
                ),
                (
                    "toughest foe",
                    TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Desc),
                ),
                (
                    "frailest foe (max hp)",
                    TargetQuery::new(Pool::Enemies).sort(SortKey::MaxHp, Order::Asc),
                ),
                (
                    "most hurt foe",
                    TargetQuery::new(Pool::Enemies).sort(SortKey::HpPct, Order::Asc),
                ),
                (
                    "fullest-mp foe",
                    TargetQuery::new(Pool::Enemies).sort(SortKey::Mp, Order::Desc),
                ),
                (
                    "foe weak to fire",
                    TargetQuery::new(Pool::Enemies).filter(Filter::WeakTo(DamageType::Fire)),
                ),
                ("random foe", TargetQuery::new(Pool::Enemies).pick(Pick::Random)),
                ("all foes", TargetQuery::new(Pool::Enemies).pick(Pick::All)),
                (
                    "clustered foe",
                    TargetQuery::new(Pool::Enemies)
                        .filter(Filter::WithinDistanceOf(
                            Box::new(TargetQuery::new(Pool::Enemies).pick(Pick::All)),
                            5.0,
                        ))
                        .sort(SortKey::MaxHp, Order::Asc),
                ),
                ("foe on an ally", foe_on_ally()),
            ],
        ),
        sub(
            "allies",
            vec![
                (
                    "most hurt ally",
                    TargetQuery::new(Pool::Allies).sort(SortKey::HpPct, Order::Asc),
                ),
                (
                    "hurt ally (<70%)",
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::HpPctBelow(0.7))
                        .sort(SortKey::HpPct, Order::Asc),
                ),
                (
                    "hurt allies (all <80%)",
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::HpPctBelow(0.8))
                        .pick(Pick::All),
                ),
                ("nearest other ally", nearest_other_ally()),
                (
                    "poisoned ally",
                    TargetQuery::new(Pool::Allies)
                        .filter(Filter::HasStatus(StatusKind::Poison))
                        .sort(SortKey::HpPct, Order::Asc),
                ),
            ],
        ),
        item("myself", TargetQuery::new(Pool::Myself)),
    ]
}

pub fn target_presets() -> Vec<(String, TargetQuery)> {
    flatten_menu(&target_menu())
}

pub fn condition_menu() -> Vec<MenuEntry<Condition>> {
    let my_hp_below = |x: f32| {
        Condition::Exists(TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(x)))
    };
    let foe_close = Condition::Exists(
        TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistance(4.0)),
    );
    vec![
        item("always", Condition::Always),
        sub(
            "my hp below",
            vec![
                ("my hp < 30%", my_hp_below(0.3)),
                ("my hp < 50%", my_hp_below(0.5)),
                ("my hp < 70%", my_hp_below(0.7)),
            ],
        ),
        sub(
            "threats near me",
            vec![
                ("a foe within 4 of me", foe_close.clone()),
                ("no foe within 4 of me", Condition::Not(Box::new(foe_close))),
                (
                    "2+ foes within 4 of me",
                    Condition::Count {
                        q: TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistance(4.0)),
                        cmp: Cmp::Ge,
                        n: 2,
                    },
                ),
            ],
        ),
        sub(
            "team state",
            vec![
                (
                    "2+ allies hurt (<80%)",
                    Condition::Count {
                        q: TargetQuery::new(Pool::Allies).filter(Filter::HpPctBelow(0.8)),
                        cmp: Cmp::Ge,
                        n: 2,
                    },
                ),
                (
                    "a foe is on an ally",
                    Condition::Exists(TargetQuery::new(Pool::Enemies).filter(
                        Filter::WithinDistanceOf(
                            Box::new(
                                TargetQuery::new(Pool::Allies)
                                    .filter(Filter::NotSelf)
                                    .pick(Pick::All),
                            ),
                            3.0,
                        ),
                    )),
                ),
                (
                    "3+ foes alive",
                    Condition::Count {
                        q: TargetQuery::new(Pool::Enemies),
                        cmp: Cmp::Ge,
                        n: 3,
                    },
                ),
            ],
        ),
        sub(
            "status on me",
            vec![
                (
                    "i am sneaking",
                    Condition::Exists(
                        TargetQuery::new(Pool::Myself)
                            .filter(Filter::HasStatus(StatusKind::Sneak)),
                    ),
                ),
                (
                    "i am not shielded",
                    Condition::Exists(TargetQuery::new(Pool::Myself).filter(Filter::Not(
                        Box::new(Filter::HasStatus(StatusKind::Shield)),
                    ))),
                ),
                (
                    "i am poisoned",
                    Condition::Exists(
                        TargetQuery::new(Pool::Myself)
                            .filter(Filter::HasStatus(StatusKind::Poison)),
                    ),
                ),
            ],
        ),
    ]
}

pub fn condition_presets() -> Vec<(String, Condition)> {
    flatten_menu(&condition_menu())
}

pub fn term_menu() -> Vec<MenuEntry<Term>> {
    vec![
        sub(
            "relative to foes",
            vec![
                ("chase nearest foe", Term::Near(nearest_foe(), 0.0)),
                ("standoff from nearest foe", Term::Near(nearest_foe(), 6.5)),
                ("away from nearest foe", Term::AwayFrom(nearest_foe())),
                ("sight of nearest foe", Term::SightOf(nearest_foe())),
                (
                    "amid the foes",
                    Term::Crowd(TargetQuery::new(Pool::Enemies).pick(Pick::All), 5.0),
                ),
            ],
        ),
        sub(
            "relative to allies",
            vec![
                (
                    "guard most hurt ally",
                    Term::Near(
                        TargetQuery::new(Pool::Allies)
                            .filter(Filter::NotSelf)
                            .sort(SortKey::HpPct, Order::Asc),
                        1.0,
                    ),
                ),
                ("peel: chase foe on an ally", Term::Near(foe_on_ally(), 0.0)),
                ("escort nearest ally", Term::Near(nearest_other_ally(), 1.5)),
            ],
        ),
        item("high ground", Term::HighGround),
    ]
}

pub fn term_presets() -> Vec<(String, Term)> {
    flatten_menu(&term_menu())
}

/// The skill dropdown for one actor: its known skills, in kit order (flat —
/// kits are small).
pub fn skill_menu(known: &[SkillId], state: &BattleState) -> Vec<MenuEntry<SkillId>> {
    known
        .iter()
        .map(|&s| MenuEntry::Item(state.skill(s).name.clone(), s))
        .collect()
}

// ---------------------------------------------------------------------------
// Cycling: step a value through its preset catalog. The current value is
// located by *description* equality (a value hand-authored outside the
// catalog matches nothing and cycles to the first preset).
// ---------------------------------------------------------------------------

fn cycle_by_desc<T: Clone>(presets: Vec<(String, T)>, cur: &str, describe: fn(&T) -> String) -> T {
    let idx = presets.iter().position(|(_, v)| describe(v) == cur);
    let next = idx.map_or(0, |i| (i + 1) % presets.len());
    presets[next].1.clone()
}

pub fn cycle_condition(cur: &Condition) -> Condition {
    cycle_by_desc(condition_presets(), &describe_condition(cur), |c| {
        describe_condition(c)
    })
}

pub fn cycle_target(cur: &TargetQuery) -> TargetQuery {
    cycle_by_desc(target_presets(), &describe_query(cur), |q| describe_query(q))
}

pub fn cycle_term(cur: &Term) -> Term {
    cycle_by_desc(term_presets(), &describe_term(cur), |t| describe_term(t))
}

pub fn cycle_skill(known: &[SkillId], cur: SkillId) -> SkillId {
    match known.iter().position(|&s| s == cur) {
        Some(i) => known[(i + 1) % known.len()],
        None => known.first().copied().unwrap_or(cur),
    }
}

// ---------------------------------------------------------------------------
// Node-level edit helpers (so the viewer never matches on Body itself)
// ---------------------------------------------------------------------------

pub fn is_group(n: &Node) -> bool {
    matches!(n.body, Body::Group { .. })
}

/// Flip a group between Fallthrough and Commit; false on a leaf.
pub fn toggle_mode(n: &mut Node) -> bool {
    let Body::Group { mode, .. } = &mut n.body else {
        return false;
    };
    *mode = match *mode {
        GroupMode::Fallthrough => GroupMode::Commit,
        GroupMode::Commit => GroupMode::Fallthrough,
    };
    true
}

/// Cycle a leaf's target query through the presets; false on a group.
pub fn cycle_leaf_target(n: &mut Node) -> bool {
    let Body::Act { target, .. } = &mut n.body else {
        return false;
    };
    *target = cycle_target(target);
    true
}

/// Cycle a leaf's skill through the actor's known skills; false on a group.
pub fn cycle_leaf_skill(n: &mut Node, known: &[SkillId]) -> bool {
    let Body::Act { skill, .. } = &mut n.body else {
        return false;
    };
    *skill = cycle_skill(known, *skill);
    true
}

/// Set a leaf's target query outright (a dropdown pick); false on a group.
pub fn set_leaf_target(n: &mut Node, q: TargetQuery) -> bool {
    let Body::Act { target, .. } = &mut n.body else {
        return false;
    };
    *target = q;
    true
}

/// Set a leaf's skill outright (a dropdown pick); false on a group.
pub fn set_leaf_skill(n: &mut Node, s: SkillId) -> bool {
    let Body::Act { skill, .. } = &mut n.body else {
        return false;
    };
    *skill = s;
    true
}

/// A fresh unguarded fallthrough group — the canonical editable root and the
/// node the "+ group" button inserts.
pub fn empty_root() -> Node {
    Node::context(Condition::Always, GroupMode::Fallthrough, Vec::new())
}

/// The leaf the "+ rule" button inserts: first known skill at the nearest foe.
pub fn new_leaf(known: &[SkillId]) -> Option<Node> {
    Some(Node::act(nearest_foe(), *known.first()?))
}

/// Ensure the root is a group so every edit op has a child list to work on
/// (a bare-leaf root — like the demo goblin's — gets wrapped).
pub fn normalize_root(root: &mut Node) {
    if !is_group(root) {
        let leaf = std::mem::replace(root, empty_root());
        if let Body::Group { children, .. } = &mut root.body {
            children.push(leaf);
        }
    }
}

// ---------------------------------------------------------------------------
// Tree structure ops: nodes addressed by child-index paths from the root
// (the root itself is not a row; `[1, 0]` = root's 2nd child's 1st child).
// ---------------------------------------------------------------------------

/// Depth-first list of `(path, depth)` for every node under the root.
pub fn rows(root: &Node) -> Vec<(Vec<usize>, usize)> {
    fn walk(n: &Node, path: &mut Vec<usize>, depth: usize, out: &mut Vec<(Vec<usize>, usize)>) {
        if let Body::Group { children, .. } = &n.body {
            for (i, c) in children.iter().enumerate() {
                path.push(i);
                out.push((path.clone(), depth));
                walk(c, path, depth + 1, out);
                path.pop();
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut Vec::new(), 0, &mut out);
    out
}

pub fn node_at<'a>(root: &'a Node, path: &[usize]) -> Option<&'a Node> {
    let mut n = root;
    for &i in path {
        let Body::Group { children, .. } = &n.body else {
            return None;
        };
        n = children.get(i)?;
    }
    Some(n)
}

pub fn node_at_mut<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Node> {
    let mut n = root;
    for &i in path {
        let Body::Group { children, .. } = &mut n.body else {
            return None;
        };
        n = children.get_mut(i)?;
    }
    Some(n)
}

/// The child list containing the node at `path` (i.e. its parent's children).
fn siblings_mut<'a>(root: &'a mut Node, path: &[usize]) -> Option<&'a mut Vec<Node>> {
    let (_, parent) = path.split_last()?;
    let p = node_at_mut(root, parent)?;
    match &mut p.body {
        Body::Group { children, .. } => Some(children),
        _ => None,
    }
}

pub fn remove_at(root: &mut Node, path: &[usize]) -> Option<Node> {
    let last = *path.last()?;
    let siblings = siblings_mut(root, path)?;
    if last < siblings.len() {
        Some(siblings.remove(last))
    } else {
        None
    }
}

/// Swap the node at `path` with its previous (`up`) or next sibling. False
/// when already at the boundary.
pub fn shift(root: &mut Node, path: &[usize], up: bool) -> bool {
    let Some(&last) = path.last() else { return false };
    let Some(siblings) = siblings_mut(root, path) else {
        return false;
    };
    if up {
        if last == 0 || last >= siblings.len() {
            return false;
        }
        siblings.swap(last - 1, last);
    } else {
        if last + 1 >= siblings.len() {
            return false;
        }
        siblings.swap(last, last + 1);
    }
    true
}

pub fn insert_after(root: &mut Node, path: &[usize], node: Node) -> bool {
    let Some(&last) = path.last() else { return false };
    let Some(siblings) = siblings_mut(root, path) else {
        return false;
    };
    if last >= siblings.len() {
        return false;
    }
    siblings.insert(last + 1, node);
    true
}

pub fn append_child(root: &mut Node, path: &[usize], node: Node) -> bool {
    let Some(p) = node_at_mut(root, path) else {
        return false;
    };
    let Body::Group { children, .. } = &mut p.body else {
        return false;
    };
    children.push(node);
    true
}

/// Insertion relative to the current selection: into a selected group, after
/// a selected leaf, or appended to the root with nothing selected.
pub fn insert_at_selection(root: &mut Node, sel: Option<&[usize]>, node: Node) {
    let done = match sel {
        Some(p) if node_at(root, p).is_some_and(is_group) => append_child(root, p, node.clone()),
        Some(p) => insert_after(root, p, node.clone()),
        None => false,
    };
    if !done {
        append_child(root, &[], node);
    }
}

// ---------------------------------------------------------------------------
// Movement-term helpers
// ---------------------------------------------------------------------------

/// The term the "+ term" button inserts.
pub fn default_term() -> (Term, f32) {
    (Term::Near(nearest_foe(), 0.0), 1.0)
}

/// Nudge a term's tunable distance (Near's ideal range / Crowd's reach).
/// False for terms with no distance knob.
pub fn adjust_ideal(t: &mut Term, delta: f32) -> bool {
    match t {
        Term::Near(_, ideal) | Term::Crowd(_, ideal) => {
            *ideal = (*ideal + delta).max(0.0);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(skill: usize) -> Node {
        Node::act(TargetQuery::new(Pool::Enemies), SkillId(skill))
    }

    fn leaf_skill(n: &Node) -> usize {
        match &n.body {
            Body::Act { skill, .. } => skill.0,
            _ => panic!("not a leaf"),
        }
    }

    /// root ── leaf0, group(leaf1, leaf2), leaf3
    fn sample_tree() -> Node {
        Node::context(
            Condition::Always,
            GroupMode::Fallthrough,
            vec![
                leaf(0),
                Node::context(Condition::Always, GroupMode::Commit, vec![leaf(1), leaf(2)]),
                leaf(3),
            ],
        )
    }

    #[test]
    fn rows_flatten_depth_first_with_paths() {
        let r = rows(&sample_tree());
        assert_eq!(
            r,
            vec![
                (vec![0], 0),
                (vec![1], 0),
                (vec![1, 0], 1),
                (vec![1, 1], 1),
                (vec![2], 0),
            ]
        );
    }

    #[test]
    fn normalize_wraps_a_bare_leaf_root() {
        let mut root = leaf(7);
        normalize_root(&mut root);
        assert!(is_group(&root));
        let r = rows(&root);
        assert_eq!(r.len(), 1);
        assert_eq!(leaf_skill(node_at(&root, &[0]).unwrap()), 7);
        // Idempotent on an already-group root.
        normalize_root(&mut root);
        assert_eq!(rows(&root).len(), 1);
    }

    #[test]
    fn remove_then_insert_moves_a_node() {
        let mut t = sample_tree();
        let taken = remove_at(&mut t, &[1, 0]).unwrap();
        assert_eq!(leaf_skill(&taken), 1);
        assert_eq!(rows(&t).len(), 4);

        assert!(insert_after(&mut t, &[0], taken));
        // Now: leaf0, leaf1, group(leaf2), leaf3.
        assert_eq!(leaf_skill(node_at(&t, &[1]).unwrap()), 1);
        assert_eq!(leaf_skill(node_at(&t, &[2, 0]).unwrap()), 2);
    }

    #[test]
    fn shift_swaps_siblings_and_respects_bounds() {
        let mut t = sample_tree();
        assert!(!shift(&mut t, &[0], true), "top row can't move up");
        assert!(shift(&mut t, &[2], true));
        // leaf3 swapped with the group.
        assert_eq!(leaf_skill(node_at(&t, &[1]).unwrap()), 3);
        assert!(shift(&mut t, &[1], false));
        assert_eq!(leaf_skill(node_at(&t, &[2]).unwrap()), 3);
        assert!(!shift(&mut t, &[2], false), "bottom row can't move down");
    }

    #[test]
    fn insert_at_selection_targets_groups_leaves_and_root() {
        let mut t = sample_tree();
        // Into a selected group.
        insert_at_selection(&mut t, Some(&[1]), leaf(9));
        assert_eq!(leaf_skill(node_at(&t, &[1, 2]).unwrap()), 9);
        // After a selected leaf.
        insert_at_selection(&mut t, Some(&[0]), leaf(8));
        assert_eq!(leaf_skill(node_at(&t, &[1]).unwrap()), 8);
        // Appended to the root with no selection.
        insert_at_selection(&mut t, None, leaf(7));
        let r = rows(&t);
        let (last_path, _) = r.last().unwrap();
        assert_eq!(leaf_skill(node_at(&t, last_path).unwrap()), 7);
        assert_eq!(last_path.len(), 1, "root append lands at depth 0");
    }

    #[test]
    fn cycling_steps_through_presets_and_wraps() {
        // From a preset: advance to the next one.
        let c = cycle_condition(&Condition::Always);
        assert_ne!(describe_condition(&c), "always");
        // From a value outside the catalog: land on the first preset.
        let odd = Condition::Count {
            q: TargetQuery::new(Pool::Everyone),
            cmp: Cmp::Lt,
            n: 99,
        };
        let first = &condition_presets()[0].1;
        assert_eq!(
            describe_condition(&cycle_condition(&odd)),
            describe_condition(first)
        );
        // A full lap round the target presets returns to the start.
        let presets = target_presets();
        let mut q = presets[0].1.clone();
        for _ in 0..presets.len() {
            q = cycle_target(&q);
        }
        assert_eq!(describe_query(&q), describe_query(&presets[0].1));
    }

    #[test]
    fn cycle_skill_wraps_and_survives_unknown_current() {
        let known = [SkillId(3), SkillId(5), SkillId(9)];
        assert_eq!(cycle_skill(&known, SkillId(3)), SkillId(5));
        assert_eq!(cycle_skill(&known, SkillId(9)), SkillId(3));
        assert_eq!(cycle_skill(&known, SkillId(42)), SkillId(3));
        assert_eq!(cycle_skill(&[], SkillId(42)), SkillId(42));
    }

    #[test]
    fn leaf_and_group_edit_helpers() {
        let mut g = empty_root();
        assert!(toggle_mode(&mut g));
        assert!(matches!(g.body, Body::Group { mode: GroupMode::Commit, .. }));
        assert!(!cycle_leaf_target(&mut g));
        assert!(!cycle_leaf_skill(&mut g, &[SkillId(0)]));

        let mut l = leaf(0);
        assert!(!toggle_mode(&mut l));
        assert!(cycle_leaf_target(&mut l));
        assert!(cycle_leaf_skill(&mut l, &[SkillId(0), SkillId(4)]));
        assert_eq!(leaf_skill(&l), 4);
    }

    #[test]
    fn descriptions_read_naturally() {
        assert_eq!(
            describe_query(&TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)),
            "foes, nearest first"
        );
        assert_eq!(
            describe_query(
                &TargetQuery::new(Pool::Allies)
                    .filter(Filter::HpPctBelow(0.8))
                    .pick(Pick::All)
            ),
            "allies, hp < 80%, all"
        );
        assert_eq!(describe_condition(&Condition::Always), "always");
        assert_eq!(
            describe_condition(&Condition::Count {
                q: TargetQuery::new(Pool::Enemies).filter(Filter::WithinDistance(4.0)),
                cmp: Cmp::Ge,
                n: 2,
            }),
            "#(foes, within 4 of me) >= 2"
        );
        assert_eq!(
            describe_term(&Term::Near(nearest_foe(), 6.5)),
            "hold 6.5 from [foes, nearest first]"
        );
        assert_eq!(describe_term(&Term::HighGround), "high ground");
    }

    /// Every menu value must describe uniquely: the cycle functions locate
    /// the current value by description, and a duplicate would also make two
    /// dropdown picks indistinguishable in the rule list.
    #[test]
    fn menu_descriptions_are_unique() {
        fn assert_unique<T>(flat: Vec<(String, T)>, describe: fn(&T) -> String) {
            let mut seen = std::collections::HashSet::new();
            for (label, v) in &flat {
                assert!(seen.insert(describe(v)), "duplicate description under '{label}'");
            }
        }
        assert_unique(condition_presets(), describe_condition);
        assert_unique(target_presets(), describe_query);
        assert_unique(term_presets(), describe_term);
    }

    /// The UI's whole data path against the *real* scenarios: every entity's
    /// gambit normalizes, flattens, and describes without panicking; a
    /// UI-shaped edit (insert a rule, cycle its parts) leaves a battle that
    /// still runs.
    #[test]
    fn real_scenarios_describe_and_edit_cleanly() {
        for (label, build) in crate::scenario::scenarios() {
            let mut combat = build();
            let ids: Vec<_> = combat.state.entities.iter().map(|e| e.id).collect();
            for id in &ids {
                let root = combat.gambits.entry(*id).or_insert_with(empty_root);
                normalize_root(root);
                for (path, _) in rows(root) {
                    let n = node_at(root, &path).expect("row path resolves");
                    assert!(
                        !describe_node(n, &combat.state).is_empty(),
                        "{label}: describable node"
                    );
                }
                for (t, _) in &combat.move_gambits.entry(*id).or_insert_with(|| MoveGambit::new(Vec::new())).terms {
                    assert!(!describe_term(t).is_empty());
                }
            }
            // Edit the first entity the way the editor buttons do, then let the
            // battle run — the edited tree must still evaluate.
            let id = ids[0];
            let known = combat.state.entities[id.0].skills.clone();
            let leaf = new_leaf(&known).expect("entities know at least one skill");
            let root = combat.gambits.get_mut(&id).unwrap();
            insert_at_selection(root, Some(&[0]), leaf);
            let node = node_at_mut(root, &[1]).unwrap();
            node.condition = cycle_condition(&node.condition);
            cycle_leaf_target(node);
            cycle_leaf_skill(node, &known);
            combat.run(200);
        }
    }

    #[test]
    fn effect_and_leaf_skill_accessors() {
        assert_eq!(describe_effect(&Effect::Damage(12.0)), "deal 12 damage");
        assert_eq!(
            describe_effect(&Effect::Inflict {
                kind: StatusKind::Poison,
                stacks: 2,
                duration: 4
            }),
            "inflict Poison x2 for 4 ticks"
        );
        assert_eq!(describe_effect(&Effect::Dash { max: 6.0 }), "dash to contact (max 6)");
        assert_eq!(leaf_skill_id(&leaf(3)), Some(SkillId(3)));
        assert_eq!(leaf_skill_id(&empty_root()), None);
    }

    #[test]
    fn adjust_ideal_only_touches_distance_terms() {
        let (mut t, _) = default_term();
        assert!(adjust_ideal(&mut t, 0.5));
        assert!(matches!(t, Term::Near(_, i) if (i - 0.5).abs() < 1e-6));
        assert!(adjust_ideal(&mut t, -2.0));
        assert!(matches!(t, Term::Near(_, i) if i == 0.0), "clamped at zero");
        let mut hg = Term::HighGround;
        assert!(!adjust_ideal(&mut hg, 0.5));
    }
}
