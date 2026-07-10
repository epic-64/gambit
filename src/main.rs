// The gambit/battle API surface is defined ahead of its consumers (many enum
// variants, filters, and builders aren't exercised until the game is built on
// top), so allow dead code crate-wide for now.
#![allow(dead_code)]

//! gambit — a 2D semi-turn-based RPG built around a modular gambit system.
//!
//! This binary is the Macroquad viewer for the combat core: it steps `Combat` on
//! a fixed timer and draws the terrain in a fake-depth oblique projection (see
//! [`View`]) plus each entity's HP and action bars, movement, and casting state,
//! and a live event log. See CLAUDE.md for the design and `cargo test` for the
//! behaviour specs.

mod battle;
mod combat;
mod editor;
mod eval;
mod gambit;
mod nav;
mod scenario;
mod terrain;

use std::collections::HashMap;

use macroquad::prelude::*;

use battle::{
    BattleState, DamageType, Effect, Entity, EntityId, Pos, Skill, SkillId, StatusKind, Team,
    ENTITY_RADIUS,
};
use combat::{Combat, Event};
use eval::Pull;
use gambit::MoveGambit;
use terrain::{Terrain, Tile3, STEP_HEIGHT};

/// Seconds of real time per simulation tick.
const TICK_INTERVAL: f32 = 0.25;
/// Width reserved on the right for the event log.
const LOG_W: f32 = 300.0;

/// Screen lift per elevation level, as a fraction of a tile's on-screen size.
/// The single knob of the fake-depth projection: bigger reads more dramatic,
/// but tall terrain overlaps more of the row behind it.
const ELEV_LIFT: f32 = 0.38;

/// Lifetime (real seconds) of a melee pierce-beam before it fades out.
const PIERCE_LIFE: f32 = 0.22;
/// Lifetime of an impact burst (the ring + flash where a hit lands).
const BURST_LIFE: f32 = 0.3;
/// Lifetime of a floating combat-text number before it fades out.
const TEXT_LIFE: f32 = 0.9;
/// Lifetime of the soft glow + motes where a heal lands.
const HEAL_LIFE: f32 = 0.55;
/// Lifetime of the expanding ring where a unit falls.
const DEATH_LIFE: f32 = 0.6;
/// Lifetime of the white hit-flash tint on a struck token.
const FLASH_LIFE: f32 = 0.18;
/// Pixels a floating combat-text number rises over its life.
const TEXT_RISE: f32 = 30.0;
/// Seconds a bar's shadow holds at the pre-hit fill before collapsing.
const SHADOW_HOLD: f32 = 0.2;
/// Seconds the shadow's collapse then takes to sweep down to the live fill.
const SHADOW_DECAY: f32 = 0.2;

// Intent-line palette (toggled with I). Kept faint so the lines read as an
// overlay under the tokens, and off the team colors so a red line means
// "hunting this" rather than "is an enemy".
/// Movement destination: the gambit's chosen stand point.
const INTENT_GOAL: Color = Color::new(0.90, 0.90, 0.95, 0.30);
/// A reference the mover is drawn toward (pursuit, standoff band, watching).
const INTENT_TOWARD: Color = Color::new(1.00, 0.30, 0.30, 0.55);
/// A reference the mover is pushed away from (flee, hide).
const INTENT_AWAY: Color = Color::new(0.35, 0.85, 0.85, 0.55);
/// A rooted caster's committed targets (matches the gold casting ring).
const INTENT_CAST: Color = Color::new(0.95, 0.85, 0.30, 0.60);
/// How far past a fleeing mover its "pushed away" stub extends (world units).
const AWAY_STUB: f32 = 2.0;

/// Arena size in world units. Taken from the running battle's bounds so
/// differently-sized scenario maps all render to fit.
type World = (f32, f32);

/// Linear blend — used only by the event vfx (projectile flight), never to
/// interpolate sim state: entities draw exactly where the sim says they are.
fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// A transient combat visual, animated in real time and dropped once it ages
/// past `life`. All of these *decorate already-resolved sim events* — the
/// beams/bursts/numbers spawn the instant the sim says a hit landed.
/// Projectiles are NOT vfx — they're sim state, drawn live in
/// [`draw_flights`].
enum VfxKind {
    /// Melee pierce-beam: snaps from the attacker through the target and fades.
    Beam { from: Pos, to: Pos },
    /// Impact burst where a hit landed: an expanding ring with a white flash.
    /// `big` marks a weakness hit (larger, louder).
    Burst { at: Pos, big: bool },
    /// Soft glow + rising motes where a heal landed.
    HealGlow { at: Pos },
    /// Floating combat text (damage numbers, heals, statuses), rising and
    /// fading. `lift` is a fixed pixel offset so stacked numbers don't overlap.
    Text { at: Pos, text: String, size: f32, lift: f32 },
    /// Expanding ring where a unit fell.
    Death { at: Pos },
}

struct Vfx {
    kind: VfxKind,
    color: Color,
    /// Seconds elapsed since it spawned.
    age: f32,
    /// Total lifetime in seconds.
    life: f32,
}

/// Trailing "shadow" behind a resource bar: on a loss it stays at the old fill
/// for [`SHADOW_HOLD`], then sweeps down to the live value over
/// [`SHADOW_DECAY`], so the size of the bite just taken stays readable for a
/// beat. Gains snap it up instantly — the shadow only ever shows loss.
struct Shadow {
    /// Displayed fill fraction (>= the live fraction while trailing).
    frac: f32,
    /// Live fraction last frame, to spot fresh drops.
    last_live: f32,
    /// Seconds left holding at the old fill before the collapse starts.
    hold: f32,
    /// Collapse speed (fraction/sec), fixed when the hold expires so the sweep
    /// takes [`SHADOW_DECAY`] seconds regardless of how big the bite was.
    speed: f32,
}

impl Shadow {
    fn new(live: f32) -> Self {
        Shadow { frac: live, last_live: live, hold: 0.0, speed: 0.0 }
    }

    fn tick(&mut self, live: f32, dt: f32) {
        if live < self.last_live {
            // Fresh loss: (re)freeze at the old width.
            self.hold = SHADOW_HOLD;
            self.speed = 0.0;
        }
        self.last_live = live;
        if live >= self.frac {
            self.frac = live;
            return;
        }
        self.hold -= dt;
        if self.hold > 0.0 {
            return;
        }
        if self.speed == 0.0 {
            self.speed = (self.frac - live) / SHADOW_DECAY;
        }
        self.frac = (self.frac - self.speed * dt).max(live);
    }
}

/// Shadow trails for one entity's HP and MP bars.
struct ShadowBars {
    hp: Shadow,
    mp: Shadow,
}

/// The palette of the elements — shared by every attack visual (beams,
/// projectiles, impact bursts, damage numbers) so an element reads the same
/// from fire to landing.
fn element_color(dt: DamageType) -> Color {
    match dt {
        DamageType::Physical => Color::new(0.85, 0.86, 0.92, 1.0),
        DamageType::Fire => Color::new(1.0, 0.55, 0.2, 1.0),
        DamageType::Ice => Color::new(0.5, 0.85, 1.0, 1.0),
        DamageType::Lightning => Color::new(0.95, 0.9, 0.35, 1.0),
        DamageType::Poison => Color::new(0.6, 0.85, 0.35, 1.0),
        DamageType::Holy => Color::new(1.0, 0.96, 0.72, 1.0),
    }
}

/// Tint an attack visual by its element (heals/utility with no damage type get
/// a restorative green).
fn skill_color(skill: &Skill) -> Color {
    match skill.damage_type {
        Some(dt) => element_color(dt),
        None => Color::new(0.4, 0.9, 0.5, 1.0),
    }
}

/// Tint a *landed hit* by its element. Unlike [`skill_color`], typeless damage
/// (DoT pulses, untyped hits) reads neutral — never heal-green.
fn damage_color(dt: Option<DamageType>) -> Color {
    match dt {
        Some(dt) => element_color(dt),
        None => Color::new(0.92, 0.9, 0.88, 1.0),
    }
}

/// Tint a status popup by what the status does.
fn status_color(kind: StatusKind) -> Color {
    match kind {
        StatusKind::Poison => Color::new(0.6, 0.85, 0.35, 1.0),
        StatusKind::Burn => Color::new(1.0, 0.55, 0.2, 1.0),
        StatusKind::Regen => Color::new(0.4, 0.9, 0.5, 1.0),
        StatusKind::Shield => Color::new(0.65, 0.8, 1.0, 1.0),
        StatusKind::Enrage => Color::new(1.0, 0.4, 0.4, 1.0),
        StatusKind::Silence => Color::new(0.75, 0.6, 0.95, 1.0),
        StatusKind::Stun => Color::new(0.95, 0.85, 0.3, 1.0),
        StatusKind::Snare => Color::new(0.5, 0.7, 0.95, 1.0),
        StatusKind::SpellWard => Color::new(0.8, 0.55, 1.0, 1.0),
        StatusKind::Sneak => Color::new(0.55, 0.6, 0.7, 1.0),
        StatusKind::MortalWound => Color::new(0.85, 0.3, 0.45, 1.0),
        StatusKind::RegenAura => Color::new(0.45, 0.9, 0.6, 1.0),
        StatusKind::MightAura => Color::new(1.0, 0.6, 0.3, 1.0),
        StatusKind::StormAura => Color::new(0.55, 0.65, 1.0, 1.0),
        StatusKind::Exposed => Color::new(0.95, 0.8, 0.5, 1.0),
        StatusKind::Lifeleech => Color::new(0.9, 0.3, 0.65, 1.0),
    }
}

/// Draw the aura fields: a faint filled disc + ring of the aura's true radius
/// around each living bearer, so "who is covered" is readable at a glance —
/// the exact circle the sim tests entities against, gently breathing. The
/// storm field additionally crackles (flickering lightning ticks inside the
/// ring), so a *hostile* field never reads as a teammate's buff circle.
fn draw_auras(view: &View, combat: &Combat) {
    let t = get_time() as f32;
    let breathe = 0.75 + 0.25 * (t * 2.0).sin();
    for e in &combat.state.entities {
        if !e.is_alive() {
            continue;
        }
        for (kind, radius) in [
            (StatusKind::RegenAura, combat::AURA_RADIUS),
            (StatusKind::MightAura, combat::AURA_RADIUS),
            (StatusKind::StormAura, combat::STORM_RADIUS),
        ] {
            if e.status(kind).is_none() {
                continue;
            }
            let (sx, sy) = view.pos(e.pos);
            let r = radius * view.scale;
            let color = status_color(kind);
            draw_circle(sx, sy, r, with_alpha(color, 0.05));
            draw_circle_lines(sx, sy, r, 1.5, with_alpha(color, 0.30 * breathe));
            if kind == StatusKind::StormAura {
                // Jagged radial ticks strobing around the disc, each on its
                // own fast cadence — the field is *live*, stand elsewhere.
                let ph = e.id.0 as f32 * 1.73;
                for k in 0..5 {
                    let ang = k as f32 / 5.0 * std::f32::consts::TAU + t * 1.7 + ph;
                    let flick = (t * 9.0 + k as f32 * 2.4 + ph).sin() * 0.5 + 0.5;
                    let (ax, ay) = (sx + ang.cos() * r * 0.45, sy + ang.sin() * r * 0.45);
                    let kink = ang + 0.22;
                    let (mx, my) = (sx + kink.cos() * r * 0.72, sy + kink.sin() * r * 0.72);
                    let (bx, by) = (sx + ang.cos() * r * 0.98, sy + ang.sin() * r * 0.98);
                    draw_line(ax, ay, mx, my, 1.5, with_alpha(color, 0.7 * flick));
                    draw_line(mx, my, bx, by, 1.5, with_alpha(color, 0.7 * flick));
                }
            }
        }
    }
}

/// Spawn hit visuals for an `Acted` event: point-blank targets get a
/// pierce-beam (their hit landed this instant). Farther targets got a sim
/// projectile — drawn live in [`draw_flights`] until it lands — and a
/// gap-closer's visual is the lunge itself.
fn spawn_attack_vfx(combat: &Combat, ev: &Event, vfx: &mut Vec<Vfx>) {
    let Event::Acted { actor, skill, targets } = ev else {
        return;
    };
    let s = combat.state.skill(*skill);
    if s.effects.iter().any(|e| matches!(e, Effect::Dash { .. })) {
        return;
    }
    let from = combat.state.entity(*actor).pos;
    let color = skill_color(s);
    for &t in targets {
        let to = combat.state.entity(t).pos;
        if from.dist(to) > combat::MELEE_RANGE {
            continue; // in flight — the sim's projectile draws it
        }
        vfx.push(Vfx {
            kind: VfxKind::Beam { from, to },
            color,
            age: 0.0,
            life: PIERCE_LIFE,
        });
    }
}

/// Push a floating combat-text popup, lifted above any younger popups already
/// hovering near the same spot so simultaneous numbers (hit + status, DoT
/// pulses on several stacks) stack upward instead of overprinting.
fn push_text(vfx: &mut Vec<Vfx>, at: Pos, text: String, size: f32, color: Color) {
    let stacked = vfx
        .iter()
        .filter(|v| matches!(&v.kind, VfxKind::Text { at: a, .. } if a.dist(at) < 2.0))
        .count();
    vfx.push(Vfx {
        kind: VfxKind::Text { at, text, size, lift: stacked as f32 * 16.0 },
        color,
        age: 0.0,
        life: TEXT_LIFE,
    });
}

/// Spawn landing visuals for the events that mark a resolved outcome: impact
/// bursts + damage numbers where hits land (`Damage` fires at the true landing
/// instant — projectile impact, dash contact, DoT pulse — so these are always
/// in sync with the sim), heal glows, status popups, death rings, and fizzle
/// notes. Also primes the struck token's hit-flash.
fn spawn_impact_vfx(
    combat: &Combat,
    ev: &Event,
    vfx: &mut Vec<Vfx>,
    flash: &mut HashMap<EntityId, (f32, Color)>,
) {
    match ev {
        Event::Damage { target, amount, weakness, dmg_type } => {
            let at = combat.state.entity(*target).pos;
            let color = damage_color(*dmg_type);
            vfx.push(Vfx {
                kind: VfxKind::Burst { at, big: *weakness },
                color,
                age: 0.0,
                life: if *weakness { BURST_LIFE * 1.4 } else { BURST_LIFE },
            });
            let (text, size) = if *weakness {
                (format!("-{amount:.0}!"), 26.0)
            } else {
                (format!("-{amount:.0}"), 19.0)
            };
            push_text(vfx, at, text, size, color);
            flash.insert(*target, (FLASH_LIFE, mix(WHITE, color, 0.35)));
        }
        Event::Reflected { bearer, attacker, dmg_type } => {
            // The parry reads as a popup on the bearer plus the spell's beam
            // snapping *back* toward its caster; the rebound's own Damage
            // event supplies the burst and number on the attacker.
            let a = combat.state.entity(*bearer).pos;
            let b = combat.state.entity(*attacker).pos;
            push_text(vfx, a, "reflected!".into(), 19.0, status_color(StatusKind::SpellWard));
            vfx.push(Vfx {
                kind: VfxKind::Beam { from: a, to: b },
                color: damage_color(*dmg_type),
                age: 0.0,
                life: PIERCE_LIFE,
            });
        }
        Event::Chained { from, to, dmg_type } => {
            // The arc between two chain victims — the same snap-and-fade beam
            // as a melee pierce, tinted by the element it carries.
            let a = combat.state.entity(*from).pos;
            let b = combat.state.entity(*to).pos;
            vfx.push(Vfx {
                kind: VfxKind::Beam { from: a, to: b },
                color: damage_color(*dmg_type),
                age: 0.0,
                life: PIERCE_LIFE,
            });
        }
        Event::Heal { target, amount } => {
            let at = combat.state.entity(*target).pos;
            let color = Color::new(0.4, 0.9, 0.5, 1.0);
            vfx.push(Vfx {
                kind: VfxKind::HealGlow { at },
                color,
                age: 0.0,
                life: HEAL_LIFE,
            });
            push_text(vfx, at, format!("+{amount:.0}"), 19.0, color);
            flash.insert(*target, (FLASH_LIFE, color));
        }
        Event::Inflicted { target, kind, stacks } => {
            let at = combat.state.entity(*target).pos;
            let text = if *stacks > 1 {
                format!("{kind:?} x{stacks}")
            } else {
                format!("{kind:?}")
            };
            push_text(vfx, at, text, 17.0, status_color(*kind));
        }
        Event::Cleansed { target } => {
            let at = combat.state.entity(*target).pos;
            push_text(vfx, at, "cleansed".into(), 17.0, Color::new(1.0, 0.96, 0.72, 1.0));
        }
        Event::MpDrained { target, amount } => {
            let at = combat.state.entity(*target).pos;
            push_text(
                vfx,
                at,
                format!("-{amount:.0} mp"),
                17.0,
                Color::new(0.4, 0.65, 1.0, 1.0),
            );
        }
        Event::Fizzled { actor, .. } => {
            let at = combat.state.entity(*actor).pos;
            push_text(vfx, at, "fizzle".into(), 16.0, Color::new(0.6, 0.62, 0.66, 1.0));
        }
        Event::Died(target) => {
            let at = combat.state.entity(*target).pos;
            vfx.push(Vfx {
                kind: VfxKind::Death { at },
                color: Color::new(0.85, 0.85, 0.9, 1.0),
                age: 0.0,
                life: DEATH_LIFE,
            });
        }
        _ => {}
    }
}

/// Which screen the viewer is showing: the scenario picker, a live battle, or
/// the gambit editor (paused battle underneath, edits apply on resume).
enum Screen {
    Menu,
    Playing,
    Editor,
}

/// UI state of the gambit editor: which character is open, which panel has
/// keyboard focus, and each panel's selection + scroll.
#[derive(Default)]
struct EditorState {
    /// Index of the inspected entity (== its `EntityId`).
    entity: usize,
    /// Focused panel: 0 = action gambit, 1 = movement gambit.
    panel: usize,
    sel_action: usize,
    sel_move: usize,
    scroll_action: f32,
    scroll_move: f32,
    /// The open dropdown, if any. While present it owns every click.
    menu: Option<OpenMenu>,
}

impl EditorState {
    /// Reset per-character selection state (when switching characters).
    fn select_entity(&mut self, entity: usize) {
        *self = EditorState { entity, panel: self.panel, ..EditorState::default() };
    }
}

fn window_conf() -> Conf {
    Conf {
        window_title: "gambit".to_owned(),
        window_width: 1000,
        window_height: 640,
        high_dpi: true,
        ..Default::default()
    }
}

#[macroquad::main(window_conf)]
async fn main() {
    let scenarios = scenario::scenarios();
    let mut screen = Screen::Menu;
    let mut combat: Option<Combat> = None;
    let mut current = 0usize;
    let mut log: Vec<String> = Vec::new();
    let mut paused = false;
    let mut show_intent = true;
    // In-flight attack visuals (pierce-beams, impact bursts, combat text),
    // aged in real time.
    let mut vfx: Vec<Vfx> = Vec::new();
    // Per-entity hit-flash: a brief tint on a token the instant it's struck
    // (white-ish for damage, green for heals), decayed in real time.
    let mut flash: HashMap<EntityId, (f32, Color)> = HashMap::new();
    // Per-entity trailing shadows for the HP/MP bars, animated in real time.
    let mut shadows: HashMap<EntityId, ShadowBars> = HashMap::new();
    // Gambit-editor UI state (selection, focus, scroll).
    let mut editor_state = EditorState::default();

    loop {
        match screen {
            Screen::Menu => {
                // Number keys pick a scenario and drop into the battle.
                for i in 0..scenarios.len() {
                    if digit_key(i).is_some_and(is_key_pressed) {
                        combat = Some(scenarios[i].1());
                        current = i;
                        log.clear();
                        vfx.clear();
                        flash.clear();
                        shadows.clear();
                        paused = false;
                        editor_state = EditorState::default();
                        screen = Screen::Playing;
                    }
                }

                clear_background(Color::new(0.10, 0.11, 0.13, 1.0));
                draw_menu(&scenarios);
            }
            Screen::Playing => {
                // --- input ---
                if is_key_pressed(KeyCode::Space) {
                    paused = !paused;
                }
                if is_key_pressed(KeyCode::R) {
                    // Restart the scenario but keep any gambit edits: the fresh
                    // build's rules are overwritten with the current (possibly
                    // edited) ones. Re-picking from the menu gets a pristine copy.
                    restart_keeping_edits(&scenarios, current, &mut combat);
                    log.clear();
                    vfx.clear();
                    flash.clear();
                    shadows.clear();
                    paused = false;
                }
                if is_key_pressed(KeyCode::M) || is_key_pressed(KeyCode::Escape) {
                    screen = Screen::Menu;
                }
                if is_key_pressed(KeyCode::I) {
                    show_intent = !show_intent;
                }
                if is_key_pressed(KeyCode::G) {
                    paused = true;
                    screen = Screen::Editor;
                }

                let Some(combat) = combat.as_mut() else {
                    continue;
                };

                // --- update: real frame time drives the sim directly ---
                if !paused && !combat.is_over() {
                    // dt in tick units. A hitch (window drag, debugger pause) is
                    // capped at one tick of sim time so the world never leaps.
                    let dt = get_frame_time().min(TICK_INTERVAL) / TICK_INTERVAL;
                    let events = combat.step(dt);
                    for ev in &events {
                        log.push(format_event(combat, ev));
                        spawn_attack_vfx(combat, ev, &mut vfx);
                        spawn_impact_vfx(combat, ev, &mut vfx, &mut flash);
                    }
                    // Keep the log from growing without bound.
                    const MAX_LOG: usize = 500;
                    if log.len() > MAX_LOG {
                        log.drain(0..log.len() - MAX_LOG);
                    }
                }

                // Age attack visuals in real time and drop the finished ones.
                // (Independent of the sim cadence, so they animate smoothly.)
                let dt = get_frame_time();
                for v in &mut vfx {
                    v.age += dt;
                }
                vfx.retain(|v| v.age < v.life);
                flash.retain(|_, (remaining, _)| {
                    *remaining -= dt;
                    *remaining > 0.0
                });

                // Trail each bar's shadow toward the unit's live HP/MP.
                for e in &combat.state.entities {
                    let hp_live = (e.hp / e.max_hp).clamp(0.0, 1.0);
                    let mp_live = if e.max_mp > 0.0 {
                        (e.mp / e.max_mp).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let sb = shadows.entry(e.id).or_insert_with(|| ShadowBars {
                        hp: Shadow::new(hp_live),
                        mp: Shadow::new(mp_live),
                    });
                    sb.hp.tick(hp_live, dt);
                    sb.mp.tick(mp_live, dt);
                }

                // --- draw ---
                let view = View::new(combat.state.bounds, combat.state.terrain.as_ref());
                clear_background(Color::new(0.09, 0.10, 0.12, 1.0));
                draw_arena(&view);
                if let Some(t) = combat.state.terrain.as_ref() {
                    draw_terrain(&view, t);
                }
                // Everything below draws combat.state directly — the sim is the
                // single source of truth; nothing is interpolated or predicted.
                // Aura fields go under everything mobile: who is covered is
                // terrain-like information.
                draw_auras(&view, combat);
                // Intent lines go under the tokens: where each unit is heading
                // and what it's positioning relative to (or casting at).
                if show_intent {
                    for e in &combat.state.entities {
                        if e.is_alive() {
                            draw_intent(&view, e, combat);
                        }
                    }
                }
                // Units paint in depth order against the terrain: south of the
                // screen draws over north, and terrain rising in front of a
                // unit repaints over it (the unit then ghosts through).
                draw_units(&view, combat, &flash, &shadows);
                // Attack visuals ride on top of the tokens: fading pierce-beams
                // (event decoration) and live projectiles (sim state).
                for v in &vfx {
                    draw_vfx(&view, v);
                }
                draw_flights(&view, combat);
                let meter_top = draw_meter(combat);
                draw_log(&log, meter_top);
                draw_hud(combat, paused, scenarios[current].0);
            }
            Screen::Editor => {
                if is_key_pressed(KeyCode::R) {
                    // Apply the edits from the top: fresh battle, edited rules.
                    restart_keeping_edits(&scenarios, current, &mut combat);
                    log.clear();
                    vfx.clear();
                    flash.clear();
                    shadows.clear();
                    paused = false;
                    editor_state.menu = None;
                    screen = Screen::Playing;
                } else if is_key_pressed(KeyCode::G) || is_key_pressed(KeyCode::Escape) {
                    // Esc/G close an open dropdown first, then leave to the
                    // (paused) battle; edits are already live.
                    if editor_state.menu.is_some() {
                        editor_state.menu = None;
                    } else {
                        screen = Screen::Playing;
                    }
                } else if is_key_pressed(KeyCode::M) {
                    editor_state.menu = None;
                    screen = Screen::Menu;
                } else if let Some(combat) = combat.as_mut() {
                    let n = combat.state.entities.len();
                    if is_key_pressed(KeyCode::Tab) && n > 0 {
                        editor_state.select_entity((editor_state.entity + 1) % n);
                    }
                    // Up/Down move the focused panel's selection (clamped when
                    // the panels draw).
                    if is_key_pressed(KeyCode::Down) {
                        if editor_state.panel == 0 {
                            editor_state.sel_action += 1;
                        } else {
                            editor_state.sel_move += 1;
                        }
                    }
                    if is_key_pressed(KeyCode::Up) {
                        if editor_state.panel == 0 {
                            editor_state.sel_action = editor_state.sel_action.saturating_sub(1);
                        } else {
                            editor_state.sel_move = editor_state.sel_move.saturating_sub(1);
                        }
                    }

                    clear_background(Color::new(0.09, 0.10, 0.12, 1.0));
                    draw_editor(&mut editor_state, combat, scenarios[current].0);
                } else {
                    screen = Screen::Menu;
                }
            }
        }

        next_frame().await;
    }
}

/// Map a scenario index to its selection key (1..=9); `None` past the ninth.
fn digit_key(i: usize) -> Option<KeyCode> {
    match i {
        0 => Some(KeyCode::Key1),
        1 => Some(KeyCode::Key2),
        2 => Some(KeyCode::Key3),
        3 => Some(KeyCode::Key4),
        4 => Some(KeyCode::Key5),
        5 => Some(KeyCode::Key6),
        6 => Some(KeyCode::Key7),
        7 => Some(KeyCode::Key8),
        8 => Some(KeyCode::Key9),
        _ => None,
    }
}

fn draw_menu(scenarios: &[(&'static str, fn() -> Combat)]) {
    draw_text("gambit", 44.0, 92.0, 52.0, WHITE);
    draw_text(
        "Select a scenario",
        46.0,
        140.0,
        26.0,
        Color::new(0.78, 0.80, 0.84, 1.0),
    );

    let mut y = 196.0;
    for (i, (name, _)) in scenarios.iter().enumerate() {
        draw_text(&format!("{}.", i + 1), 60.0, y, 28.0, Color::new(0.95, 0.85, 0.3, 1.0));
        draw_text(name, 96.0, y, 26.0, WHITE);
        y += 44.0;
    }

    draw_text(
        "Press a number key to begin.",
        46.0,
        y + 28.0,
        20.0,
        Color::new(0.65, 0.68, 0.72, 1.0),
    );
    draw_text(
        "In battle:  Space pause  ·  G gambit editor  ·  R restart  ·  I intent lines  ·  M / Esc menu",
        46.0,
        y + 54.0,
        20.0,
        Color::new(0.65, 0.68, 0.72, 1.0),
    );
}

// --- world <-> screen ------------------------------------------------------

fn arena_rect() -> (f32, f32, f32, f32) {
    let margin = 20.0;
    let top = 46.0;
    let x = margin;
    let y = top;
    let w = screen_width() - LOG_W - margin * 2.0;
    let h = screen_height() - top - margin;
    (x, y, w, h)
}

/// The battle camera: world → screen with a fake-depth oblique projection.
/// The ground plane maps top-down exactly as before; elevation additionally
/// lifts a point *up the screen* by [`ELEV_LIFT`] tiles per level, so hills
/// rise, walls stand and pits sink — while every tile keeps (most of) its own
/// screen footprint. With no terrain the lift is zero and this degrades to the
/// old flat projection.
struct View<'a> {
    /// Arena size in world units.
    world: World,
    /// Screen position of the arena's top-left corner.
    origin: (f32, f32),
    /// World-units → screen-pixels factor (uniform, so hitboxes read true).
    scale: f32,
    /// Screen pixels of lift per elevation level.
    lift: f32,
    terrain: Option<&'a Terrain>,
}

impl<'a> View<'a> {
    fn new(world: World, terrain: Option<&'a Terrain>) -> View<'a> {
        let (ax, ay, aw, ah) = arena_rect();
        let scale = (aw / world.0).min(ah / world.1);
        let lift = terrain.map_or(0.0, |t| t.tile_size * scale * ELEV_LIFT);
        View { world, origin: (ax, ay), scale, lift, terrain }
    }

    /// Ground-plane projection — no elevation lift.
    fn flat(&self, wx: f32, wy: f32) -> (f32, f32) {
        (self.origin.0 + wx * self.scale, self.origin.1 + wy * self.scale)
    }

    /// Project a world point standing on the terrain, lifted by the smoothed
    /// elevation under it.
    fn pos(&self, p: Pos) -> (f32, f32) {
        let (x, y) = self.flat(p.x, p.y);
        (x, y - self.elevation(p) * self.lift)
    }

    /// Project an airborne point (projectiles): like [`Self::pos`] but never
    /// below the ground plane, so a shot crossing a pit doesn't dive into it.
    fn pos_air(&self, p: Pos) -> (f32, f32) {
        let (x, y) = self.flat(p.x, p.y);
        (x, y - self.elevation(p).max(0.0) * self.lift)
    }

    /// Smoothed visual elevation under a world point. Bilinear across the four
    /// nearest tile centres, so a unit *walks up* a step instead of popping a
    /// level at the tile boundary — but a neighbour more than [`STEP_HEIGHT`]
    /// away (a wall, a cliff face, a pit) doesn't pull: you can't walk there,
    /// so it must not ramp the ground you stand on. This is a pure function of
    /// position (projection, not interpolation of sim state).
    fn elevation(&self, p: Pos) -> f32 {
        let Some(t) = self.terrain else { return 0.0 };
        let (bc, br) = t.tile_of(p);
        let base = t.elevation((bc.clamp(0, t.cols - 1), br.clamp(0, t.rows - 1))) as f32;
        let fx = (p.x / t.tile_size - 0.5).clamp(0.0, (t.cols - 1) as f32);
        let fy = (p.y / t.tile_size - 0.5).clamp(0.0, (t.rows - 1) as f32);
        let (c0, r0) = (fx.floor() as i32, fy.floor() as i32);
        let (tx, ty) = (fx - c0 as f32, fy - r0 as f32);
        let s = |c: i32, r: i32| {
            let e = t.elevation((c.min(t.cols - 1), r.min(t.rows - 1))) as f32;
            if (e - base).abs() <= STEP_HEIGHT as f32 { e } else { base }
        };
        let top = s(c0, r0) * (1.0 - tx) + s(c0 + 1, r0) * tx;
        let bot = s(c0, r0 + 1) * (1.0 - tx) + s(c0 + 1, r0 + 1) * tx;
        top * (1.0 - ty) + bot * ty
    }
}

// --- drawing ---------------------------------------------------------------

fn draw_arena(view: &View) {
    let (sx, sy) = view.flat(0.0, 0.0);
    let (ex, ey) = view.flat(view.world.0, view.world.1);
    draw_rectangle(sx, sy, ex - sx, ey - sy, Color::new(0.14, 0.16, 0.19, 1.0));
    draw_rectangle_lines(sx, sy, ex - sx, ey - sy, 2.0, Color::new(0.3, 0.34, 0.4, 1.0));
}

/// Draw the terrain with fake depth: every tile's top face is lifted by its
/// elevation, and wherever a tile stands above whatever is south of it (a
/// lower step, the rim of a pit, the arena edge) the exposed south face is
/// drawn beneath it. Rows paint north to south so raised terrain correctly
/// overlaps what's behind it. Units are painted into this depth order by
/// [`draw_units`]: terrain that rises above a unit's ground gets repainted
/// over the unit, which then shows through as a ghost outline.
fn draw_terrain(view: &View, t: &Terrain) {
    for r in 0..t.rows {
        for c in 0..t.cols {
            draw_tile(view, t, c, r);
        }
    }
}

/// Draw one tile completely — lifted top face (checker, rims, the contact
/// shadows taller neighbours cast onto it) and the exposed south face beneath
/// it, striated once per level so height stays countable, sunlit along the
/// lip, darkening toward the ground. Factored out of [`draw_terrain`] so
/// [`draw_units`] can repaint a single tile over a unit standing behind it.
fn draw_tile(view: &View, t: &Terrain, c: i32, r: i32) {
    let Some(tile) = t.tile(c, r) else { return };
    let ts = t.tile_size;
    let tp = ts * view.scale; // tile edge in pixels
    let band = (tp * 0.34).min(11.0); // contact-shadow reach
    // Nested translucent strips fake each contact shadow's soft gradient.
    const AO: [(f32, f32); 3] = [(1.0, 0.08), (0.62, 0.09), (0.30, 0.10)];
    let e = tile.elevation;
    let (x, gy) = view.flat(c as f32 * ts, r as f32 * ts);
    let y = gy - e as f32 * view.lift;

    // Top face, with a faint checker so open ground keeps texture.
    let mut top = tile_top_color(tile);
    if (c + r) % 2 == 0 {
        top = mix(top, WHITE, 0.03);
    }
    draw_rectangle(x, y, tp, tp, top);
    draw_rectangle_lines(x, y, tp, tp, 1.0, Color::new(0.0, 0.0, 0.0, 0.12));
    if !tile.passable && e >= 1 {
        // A chiselled inset marks wall/rock caps as solid blocks.
        draw_rectangle_lines(x + 2.0, y + 2.0, tp - 4.0, tp - 4.0, 1.0, with_alpha(WHITE, 0.10));
    }

    // Crisp rim where this tile drops off to lower ground behind or beside
    // it — without it, a raised edge blurs into what it overlaps (the south
    // rim gets its face instead).
    let lower = |dc: i32, dr: i32| t.tile(c + dc, r + dr).is_some_and(|n| n.elevation < e);
    let rim = with_alpha(BLACK, 0.35);
    if lower(0, -1) {
        draw_line(x, y, x + tp, y, 1.5, rim);
    }
    if lower(-1, 0) {
        draw_line(x, y, x, y + tp, 1.5, rim);
    }
    if lower(1, 0) {
        draw_line(x + tp, y, x + tp, y + tp, 1.5, rim);
    }

    // Contact shadows where a higher neighbour looms over this tile.
    let higher = |dc: i32, dr: i32| t.tile(c + dc, r + dr).is_some_and(|n| n.elevation > e);
    if higher(0, -1) {
        for (f, a) in AO {
            draw_rectangle(x, y, tp, band * f, with_alpha(BLACK, a));
        }
    }
    if higher(-1, 0) {
        for (f, a) in AO {
            draw_rectangle(x, y, band * f, tp, with_alpha(BLACK, a));
        }
    }
    if higher(1, 0) {
        for (f, a) in AO {
            draw_rectangle(x + tp - band * f, y, band * f, tp, with_alpha(BLACK, a));
        }
    }

    // South face: the visible drop from this tile's top down to whatever is
    // south of it. Off-map counts as ground level, so a raised rim still
    // shows its face against the backdrop.
    let es = t.tile(c, r + 1).map_or(e.min(0), |n| n.elevation);
    if e > es {
        let fh = (e - es) as f32 * view.lift;
        let fy = y + tp;
        draw_rectangle(x, fy, tp, fh, face_color(tile));
        // Darken toward the ground: a cheap two-step vertical gradient.
        draw_rectangle(x, fy + fh * 0.55, tp, fh * 0.45, with_alpha(BLACK, 0.10));
        draw_rectangle(x, fy + fh * 0.80, tp, fh * 0.20, with_alpha(BLACK, 0.12));
        // One seam per level crossed keeps the drop countable.
        for k in (es + 1)..e {
            let ly = gy + tp - k as f32 * view.lift;
            draw_line(x, ly, x + tp, ly, 1.0, with_alpha(BLACK, 0.22));
        }
        draw_line(x, fy, x, fy + fh, 1.0, with_alpha(BLACK, 0.18));
        draw_line(x + tp, fy, x + tp, fy + fh, 1.0, with_alpha(BLACK, 0.18));
        // Sunlit lip along the top edge of the drop.
        draw_line(x, fy, x + tp, fy, 2.0, with_alpha(WHITE, 0.40));
    }
}

/// Top-face palette: walkable ground warms and lightens as it climbs, wall and
/// rock caps read as cut stone, pit floors as darkness.
fn tile_top_color(t: Tile3) -> Color {
    if !t.passable {
        if t.elevation >= 1 {
            Color::new(0.50, 0.48, 0.54, 1.0) // wall / rock cap
        } else {
            Color::new(0.03, 0.03, 0.05, 1.0) // pit floor
        }
    } else {
        let e = t.elevation as f32;
        let l = (0.15 + e * 0.09).clamp(0.10, 0.62);
        Color::new(l * 0.95 + e * 0.02, l + e * 0.012, l * 0.70, 1.0)
    }
}

/// South-face palette: hewn stone under walls, earthen cliff under ground.
fn face_color(t: Tile3) -> Color {
    if !t.passable && t.elevation >= 1 {
        Color::new(0.24, 0.22, 0.27, 1.0)
    } else {
        mix(tile_top_color(t), Color::new(0.08, 0.06, 0.05, 1.0), 0.62)
    }
}

fn team_color(team: Team) -> Color {
    if team == Team::Player {
        Color::new(0.35, 0.55, 0.95, 1.0)
    } else {
        Color::new(0.9, 0.4, 0.35, 1.0)
    }
}

/// Axis-aligned screen rect: (x, y, w, h).
type Rect4 = (f32, f32, f32, f32);

fn rects_overlap(a: Rect4, b: Rect4) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

fn rect_contains(r: Rect4, px: f32, py: f32) -> bool {
    px >= r.0 && px <= r.0 + r.2 && py >= r.1 && py <= r.1 + r.3
}

/// Screen-space bounding box of everything a tile paints: its lifted top face
/// plus the exposed south face hanging beneath it. `None` off-map.
fn tile_screen_bounds(view: &View, t: &Terrain, c: i32, r: i32) -> Option<Rect4> {
    let tile = t.tile(c, r)?;
    let e = tile.elevation;
    let ts = t.tile_size;
    let tp = ts * view.scale;
    let (x, gy) = view.flat(c as f32 * ts, r as f32 * ts);
    let y = gy - e as f32 * view.lift;
    let es = t.tile(c, r + 1).map_or(e.min(0), |n| n.elevation);
    let face = (e - es).max(0) as f32 * view.lift;
    Some((x, y, tp, tp + face))
}

/// The unit's token circle on screen (the ghost-outline trigger area).
fn token_bounds(view: &View, e: &Entity) -> Rect4 {
    let (sx, sy) = view.pos(e.pos);
    let r = (ENTITY_RADIUS * view.scale).max(6.0);
    (sx - r, sy - r, r * 2.0, r * 2.0)
}

/// Screen box around everything a unit paints — token plus the name above and
/// the bars/status text below. A repainted tile must be tested against this
/// full box, or it would hide the token but leave the bars floating on top.
fn entity_ui_bounds(view: &View, e: &Entity) -> Rect4 {
    let (sx, sy) = view.pos(e.pos);
    let r = (ENTITY_RADIUS * view.scale).max(6.0);
    let half_w = r.max(30.0);
    (sx - half_w, sy - r - 28.0, half_w * 2.0, (r + 28.0) + (r + 30.0))
}

/// Draw the units in painter order against the terrain. Corpses paint first as
/// a ground layer — a body on the floor is a decal a living unit walks *over*,
/// never behind, whatever their rows. Within each layer entities sort north →
/// south, so one nearer the viewer draws over one behind it; then any tile
/// that rises more than a walkable step above an already-drawn unit's ground
/// and reaches it on screen is repainted on top, so a unit standing behind a
/// wall or below a hill edge is genuinely hidden. (A within-[`STEP_HEIGHT`]
/// rise — a stair the unit could walk onto — never repaints: the bilinear
/// smoothing in [`View::elevation`] already ramps the unit up onto it, so
/// covering its feet would read as being stuck inside the stair.) A unit whose
/// token centre ends up covered that way gets a see-through silhouette ring so
/// it stays trackable behind cover.
fn draw_units(
    view: &View,
    combat: &Combat,
    flash: &HashMap<EntityId, (f32, Color)>,
    shadows: &HashMap<EntityId, ShadowBars>,
) {
    let (dead, alive): (Vec<&Entity>, Vec<&Entity>) =
        combat.state.entities.iter().partition(|e| !e.is_alive());
    let mut layers = [dead, alive];
    for layer in &mut layers {
        layer.sort_by(|a, b| a.pos.y.total_cmp(&b.pos.y).then(a.id.0.cmp(&b.id.0)));
    }

    let draw = |e: &Entity| {
        let fl = flash
            .get(&e.id)
            .map(|&(remaining, c)| (remaining / FLASH_LIFE, c));
        draw_entity(view, e, combat.is_casting(e.id), fl, shadows.get(&e.id));
    };

    let Some(t) = view.terrain else {
        for layer in &layers {
            for e in layer {
                draw(e);
            }
        }
        return;
    };

    let row_of = |e: &Entity| t.tile_of(e.pos).1.clamp(0, t.rows - 1);

    for order in &layers {
        // Walk the rows north → south; `order[..i]` are the units already painted.
        let mut i = 0;
        for r in 0..t.rows {
            // Repaint the tiles of this row that loom over an already-drawn
            // unit: the tile rises beyond a walkable step above the unit's
            // ground and its footprint reaches the unit's screen area. (Floor
            // south of a unit within a step of its height never repaints, so
            // a token overhanging a tile edge or climbing a stair isn't
            // clipped.)
            for c in 0..t.cols {
                let Some(tile) = t.tile(c, r) else { continue };
                if tile.elevation <= 0 {
                    continue;
                }
                let Some(rect) = tile_screen_bounds(view, t, c, r) else { continue };
                let looms = order[..i].iter().any(|e| {
                    tile.elevation > t.elevation_at(e.pos) + STEP_HEIGHT
                        && rects_overlap(rect, entity_ui_bounds(view, e))
                });
                if looms {
                    draw_tile(view, t, c, r);
                }
            }
            while i < order.len() && row_of(order[i]) == r {
                draw(order[i]);
                i += 1;
            }
        }
    }

    // Anything the repaint hid gets a see-through outline on top.
    for e in &layers[1] {
        if occluded(view, t, e) {
            let (sx, sy) = view.pos(e.pos);
            let r = (ENTITY_RADIUS * view.scale).max(6.0);
            draw_circle_lines(sx, sy, r, 2.0, with_alpha(team_color(e.team), 0.9));
        }
    }
}

/// Is this unit's token centre covered by terrain painted in front of it —
/// a tile south of the unit's row that rises more than a walkable step above
/// its ground and whose drawn footprint reaches the token? (Centre, not edge:
/// a knee-high step clipping the unit's feet shouldn't ghost it — and a
/// within-[`STEP_HEIGHT`] rise never repaints over the unit at all.)
fn occluded(view: &View, t: &Terrain, e: &Entity) -> bool {
    let row = t.tile_of(e.pos).1.clamp(0, t.rows - 1);
    let ground = t.elevation_at(e.pos);
    let (sx, sy) = view.pos(e.pos);
    for r in (row + 1)..t.rows {
        for c in 0..t.cols {
            let Some(tile) = t.tile(c, r) else { continue };
            if tile.elevation > ground + STEP_HEIGHT
                && tile_screen_bounds(view, t, c, r).is_some_and(|b| rect_contains(b, sx, sy))
            {
                return true;
            }
        }
    }
    false
}

fn draw_entity(
    view: &View,
    e: &Entity,
    casting: bool,
    flash: Option<(f32, Color)>,
    shadow: Option<&ShadowBars>,
) {
    let (sx, sy) = view.pos(e.pos);
    let alive = e.is_alive();
    // Draw the token at the shared collision radius, so overlaps (or the lack of
    // them) are visible. A small floor keeps it legible when zoomed out.
    let r = (ENTITY_RADIUS * view.scale).max(6.0);
    // Ground shadow, pinning the token to the terrain it stands on.
    if alive {
        draw_ellipse(sx, sy + r * 0.55, r * 0.95, r * 0.42, 0.0, with_alpha(BLACK, 0.28));
    }
    let base = team_color(e.team);
    let mut col = if alive {
        base
    } else {
        Color::new(0.3, 0.3, 0.32, 1.0)
    };
    // Hit-flash: a just-struck token blazes toward the impact color and decays.
    if let Some((k, fc)) = flash
        && alive
    {
        col = mix(col, fc, 0.75 * k.clamp(0.0, 1.0));
        draw_circle_lines(sx, sy, r + 2.5, 2.5, with_alpha(fc, 0.8 * k));
    }
    // A sneaking unit ghosts: the omniscient viewer still tracks it, but the
    // faded token reads as "the other team can't see this".
    if alive && e.status(StatusKind::Sneak).is_some() {
        draw_circle(sx, sy, r, with_alpha(col, 0.22));
        draw_circle_lines(sx, sy, r, 2.0, with_alpha(col, 0.6));
    } else {
        draw_circle(sx, sy, r, col);
        draw_circle_lines(sx, sy, r, 2.0, Color::new(0.0, 0.0, 0.0, 0.4));
    }

    // A casting unit is rooted mid-spell — ring it and label it.
    if alive && casting {
        draw_circle_lines(sx, sy, r + 6.0, 2.5, Color::new(0.95, 0.85, 0.3, 0.9));
        draw_text("casting", sx - 24.0, sy + r + 22.0, 16.0, Color::new(0.95, 0.85, 0.3, 1.0));
    }

    // Persistent status overlays (poison bubbles, shield hex, stun stars, …):
    // every effect is readable from the field itself, not just the status text.
    if alive {
        draw_status_fx(view, e);
    }

    // Name (just above the token).
    draw_text(&e.name, sx - 20.0, sy - r - 12.0, 18.0, WHITE);

    if !alive {
        draw_text("x_x", sx - 12.0, sy + 5.0, 18.0, LIGHTGRAY);
        return;
    }

    // HP number in the token.
    draw_text(&format!("{:.0}", e.hp), sx - 9.0, sy + 5.0, 16.0, WHITE);

    let bw = 54.0;
    let bh = 6.0;
    let bx = sx - bw / 2.0;

    // HP bar (above the token). The darker shadow layer trails behind the
    // bright fill on a hit, marking the chunk just lost (see [`Shadow`]).
    let hy = sy - r - 8.0;
    draw_rectangle(bx, hy, bw, bh, Color::new(0.25, 0.05, 0.05, 1.0));
    let hp_frac = (e.hp / e.max_hp).clamp(0.0, 1.0);
    if let Some(sb) = shadow {
        let w = bw * sb.hp.frac.max(hp_frac);
        draw_rectangle(bx, hy, w, bh, Color::new(0.16, 0.40, 0.20, 1.0));
    }
    draw_rectangle(bx, hy, bw * hp_frac, bh, Color::new(0.35, 0.8, 0.4, 1.0));

    // Action bar (below the token).
    let ay = sy + r + 2.0;
    draw_rectangle(bx, ay, bw, bh, Color::new(0.15, 0.15, 0.17, 1.0));
    let ab = e.action_bar.clamp(0.0, 1.0);
    draw_rectangle(bx, ay, bw * ab, bh, Color::new(0.95, 0.85, 0.3, 1.0));

    // MP bar (thin, just under the action bar). Shows the resource skills spend
    // and its regen. Only meaningful for MP-users, but drawn for all so the pool
    // (and any drain/regen on it) is always legible.
    let mut next_y = ay + bh + 1.0;
    if e.max_mp > 0.0 {
        let mh = 4.0;
        draw_rectangle(bx, next_y, bw, mh, Color::new(0.06, 0.08, 0.16, 1.0));
        let mp_frac = (e.mp / e.max_mp).clamp(0.0, 1.0);
        if let Some(sb) = shadow {
            let w = bw * sb.mp.frac.max(mp_frac);
            draw_rectangle(bx, next_y, w, mh, Color::new(0.14, 0.30, 0.52, 1.0));
        }
        draw_rectangle(bx, next_y, bw * mp_frac, mh, Color::new(0.3, 0.6, 0.95, 1.0));
        next_y += mh + 2.0;
    }

    // Compact status readout (e.g. "Poison x2"), below whatever bars are shown.
    if !e.statuses.is_empty() {
        let s: Vec<String> = e
            .statuses
            .iter()
            .map(|st| format!("{:?}x{}", st.kind, st.stacks))
            .collect();
        draw_text(&s.join(" "), bx, next_y + 12.0, 14.0, Color::new(0.8, 0.7, 0.9, 1.0));
    }
}

/// Persistent per-status battlefield overlays, drawn on the token itself.
/// Project rule: **every** effect — shield, snare, elemental ailment, buff,
/// debuff, mark — has its own visual, readable from the field without the
/// GUI. Sneak's ghosting and the aura field circles live elsewhere (they
/// change how the token/field itself draws); everything else is here.
/// Animated on real time with a per-entity phase so a crowd doesn't pulse in
/// lockstep; where a status has meaningful stacks (Poison) or charges
/// (SpellWard) the count shows as more marks.
fn draw_status_fx(view: &View, e: &Entity) {
    use std::f32::consts::TAU;
    let (sx, sy) = view.pos(e.pos);
    let r = (ENTITY_RADIUS * view.scale).max(6.0);
    let t = get_time() as f32 + e.id.0 as f32 * 1.73;

    for st in &e.statuses {
        let c = status_color(st.kind);
        match st.kind {
            // Poison: slow green bubbles rising off the body — one more per
            // stack (capped) so a deep poison reads heavier.
            StatusKind::Poison => {
                let n = (1 + st.stacks).min(4);
                for k in 0..n {
                    let ph = (t * 0.55 + k as f32 / n as f32).fract();
                    let bx = sx + (k as f32 * 2.4 + t * 0.9).sin() * r * 0.55;
                    let by = sy + r * 0.35 - ph * r * 1.9;
                    draw_circle(bx, by, 1.6 + (k % 2) as f32, with_alpha(c, (1.0 - ph) * 0.85));
                }
            }
            // Burn: fast-flickering flame licks off the token's crown.
            StatusKind::Burn => {
                for k in 0..3 {
                    let fx = sx + (k as f32 - 1.0) * r * 0.5;
                    let flick = (t * 9.0 + k as f32 * 2.1).sin() * 0.5 + 0.5;
                    let h = r * (0.45 + 0.5 * flick);
                    let w = r * 0.22;
                    let by = sy - r * 0.35;
                    draw_triangle(
                        vec2(fx - w, by),
                        vec2(fx + w, by),
                        vec2(fx, by - h),
                        with_alpha(c, 0.55 + 0.35 * flick),
                    );
                }
            }
            // Regen: little green crosses drifting upward — restorative,
            // where poison's round bubbles read as sickness.
            StatusKind::Regen => {
                for k in 0..2 {
                    let ph = (t * 0.5 + k as f32 * 0.5).fract();
                    let px = sx + (k as f32 * 2.0 - 1.0) * r * 0.55;
                    let py = sy - ph * r * 1.7;
                    let s = 3.0;
                    let a = (1.0 - ph) * 0.9;
                    draw_line(px - s, py, px + s, py, 1.5, with_alpha(c, a));
                    draw_line(px, py - s, px, py + s, 1.5, with_alpha(c, a));
                }
            }
            // Shield: a slowly turning hex bubble enclosing the token.
            StatusKind::Shield => {
                let pulse = 0.8 + 0.2 * (t * 2.5).sin();
                draw_poly(sx, sy, 6, r + 4.0, t * 18.0, with_alpha(c, 0.10));
                draw_poly_lines(sx, sy, 6, r + 4.0, t * 18.0, 2.0, with_alpha(c, 0.75 * pulse));
            }
            // Enrage: a bristling ring of red spikes, seething outward.
            StatusKind::Enrage => {
                let pulse = 0.5 + 0.5 * (t * 5.0).sin().abs();
                for k in 0..8 {
                    let ang = k as f32 / 8.0 * TAU + t * 0.9;
                    let (ca, sa) = (ang.cos(), ang.sin());
                    let inner = r + 1.5;
                    let outer = inner + 2.5 + 3.5 * pulse;
                    draw_line(
                        sx + ca * inner,
                        sy + sa * inner,
                        sx + ca * outer,
                        sy + sa * outer,
                        2.0,
                        with_alpha(c, 0.85),
                    );
                }
            }
            // Silence: a struck-through speech bubble bobbing at the shoulder.
            StatusKind::Silence => {
                let px = sx + r + 5.0;
                let py = sy - r - 4.0 + (t * 2.2).sin() * 1.5;
                draw_circle_lines(px, py, 4.5, 1.5, with_alpha(c, 0.95));
                draw_line(px - 3.2, py + 3.2, px + 3.2, py - 3.2, 1.5, with_alpha(c, 0.95));
            }
            // Stun: the classic — golden sparks orbiting above the head.
            StatusKind::Stun => {
                for k in 0..3 {
                    let ang = t * 3.2 + k as f32 * TAU / 3.0;
                    let px = sx + ang.cos() * r * 0.9;
                    let py = sy - r - 6.0 + ang.sin() * 3.0;
                    draw_poly(px, py, 4, 3.0, ang.to_degrees(), with_alpha(c, 0.95));
                }
            }
            // Snare: a shackle-ring clamped around the feet, studs crawling.
            StatusKind::Snare => {
                let ey = sy + r * 0.55;
                let (rx, ry) = (r * 1.1, r * 0.5);
                draw_ellipse_lines(sx, ey, rx, ry, 0.0, 2.0, with_alpha(c, 0.8));
                for k in 0..4 {
                    let ang = k as f32 / 4.0 * TAU + t * 0.6;
                    let px = sx + ang.cos() * rx;
                    let py = ey + ang.sin() * ry;
                    draw_circle(px, py, 2.0, with_alpha(c, 0.9));
                }
            }
            // Mortal wound: dark drops bleeding down off the body — the
            // downward mirror of poison's rising bubbles.
            StatusKind::MortalWound => {
                for k in 0..2 {
                    let ph = (t * 0.8 + k as f32 * 0.5).fract();
                    let px = sx + (k as f32 * 2.0 - 1.0) * r * 0.35;
                    let py = sy + r * 0.3 + ph * r * 1.5;
                    draw_circle(px, py, 2.2, with_alpha(c, 1.0 - ph));
                }
            }
            // Spell ward: one orbiting rune per charge on a flattened orbit,
            // so the remaining parries are countable at a glance.
            StatusKind::SpellWard => {
                let n = st.stacks.clamp(1, 4);
                for k in 0..n {
                    let ang = t * 2.1 + k as f32 * TAU / n as f32;
                    let px = sx + ang.cos() * (r + 5.0);
                    let py = sy + ang.sin() * (r + 5.0) * 0.4;
                    draw_poly(px, py, 4, 3.2, 45.0, with_alpha(c, 0.95));
                }
            }
            // Exposed: chevrons stabbing inward — "hit this one".
            StatusKind::Exposed => {
                let slide = ((t * 2.8).sin() * 0.5 + 0.5) * 4.0;
                for k in 0..3 {
                    let ang = k as f32 / 3.0 * TAU - TAU / 4.0;
                    let (ca, sa) = (ang.cos(), ang.sin());
                    let tip = r + 8.0 - slide;
                    let back = tip + 5.0;
                    let spread = 0.38;
                    for s in [-1.0f32, 1.0] {
                        let ba = ang + s * spread;
                        draw_line(
                            sx + ca * tip,
                            sy + sa * tip,
                            sx + ba.cos() * back,
                            sy + ba.sin() * back,
                            2.0,
                            with_alpha(c, 0.9),
                        );
                    }
                }
            }
            // Lifeleech: essence motes streaming up and out of the body —
            // the mark's life feeding whoever strikes it.
            StatusKind::Lifeleech => {
                for k in 0..3 {
                    let ph = (t * 0.7 + k as f32 / 3.0).fract();
                    let ang = k as f32 * 2.1 + t * 0.5;
                    let d = r * (0.25 + ph * 1.5);
                    let px = sx + ang.cos() * d;
                    let py = sy + ang.sin() * d * 0.5 - ph * 5.0;
                    draw_circle(px, py, 2.0, with_alpha(c, (1.0 - ph) * 0.9));
                }
            }
            // Sneak ghosts the token itself (draw_entity); the auras draw
            // their field circles (the storm's with its crackle) under
            // everything (draw_auras).
            StatusKind::Sneak
            | StatusKind::RegenAura
            | StatusKind::MightAura
            | StatusKind::StormAura => {}
        }
    }
}

/// Draw one entity's *intent* — what it is trying to do before anything
/// resolves. Movement intent is a chosen destination, not a target: the dim
/// line + ring marks the stand point the gambit picked, and the colored lines
/// show what it is positioning relative to (red = drawn toward, teal = pushed
/// away — drawn from the threat *through* the mover so the flee reads as a
/// push, not a pursuit). A rooted caster instead gets a gold line to each
/// committed target. No lines at all = holding position by choice.
fn draw_intent(view: &View, e: &Entity, combat: &Combat) {
    let (sx, sy) = view.pos(e.pos);

    // Mid-lunge: the dash is the intent — mark who it's diving on.
    if let Some(t) = combat.dash_target(e.id) {
        let (tx, ty) = view.pos(combat.state.entity(t).pos);
        draw_line(sx, sy, tx, ty, 2.0, INTENT_TOWARD);
        return;
    }

    // Casting: movement is suppressed, the committed targets are the intent.
    if let Some(targets) = combat.cast_targets(e.id) {
        for &t in targets {
            let (tx, ty) = view.pos(combat.state.entity(t).pos);
            draw_line(sx, sy, tx, ty, 1.5, INTENT_CAST);
        }
        return;
    }

    let Some(intent) = combat.move_intent(e.id) else {
        return;
    };
    // Where it's heading — always truthful, even for reference-free intents
    // like seeking high ground.
    let (gx, gy) = view.pos(intent.goal);
    draw_line(sx, sy, gx, gy, 1.5, INTENT_GOAL);
    draw_circle_lines(gx, gy, 4.0, 1.5, INTENT_GOAL);
    // What it's positioning relative to.
    for &(r, pull) in &intent.refs {
        let (rx, ry) = view.pos(r);
        match pull {
            Pull::Toward => draw_line(sx, sy, rx, ry, 1.5, INTENT_TOWARD),
            Pull::Away => {
                let (dx, dy) = (e.pos.x - r.x, e.pos.y - r.y);
                let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
                let (ex, ey) = view.pos(Pos {
                    x: e.pos.x + dx / len * AWAY_STUB,
                    y: e.pos.y + dy / len * AWAY_STUB,
                });
                draw_line(rx, ry, ex, ey, 1.5, INTENT_AWAY);
            }
        }
    }
}

/// Draw one transient combat visual, keyed by kind. Everything eases on the
/// same normalized age `t` and fades out by end of life.
fn draw_vfx(view: &View, v: &Vfx) {
    let t = (v.age / v.life).clamp(0.0, 1.0);
    let scale = view.scale;
    let a = 1.0 - t;
    match &v.kind {
        // Pierce-beam: snaps from the actor through the target and fades.
        VfxKind::Beam { from, to } => {
            let dx = to.x - from.x;
            let dy = to.y - from.y;
            let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
            let (nx, ny) = (dx / len, dy / len);
            // Extend a little past the target so it reads as piercing through it.
            let ext = 2.0 * ENTITY_RADIUS;
            let (sx, sy) = view.pos(*from);
            let (ex, ey) = view.pos(Pos { x: to.x + nx * ext, y: to.y + ny * ext });
            let w = (ENTITY_RADIUS * scale * 0.5).max(3.0);
            draw_line(sx, sy, ex, ey, w * 1.6, with_alpha(v.color, 0.28 * a)); // glow
            draw_line(sx, sy, ex, ey, w * 0.7, with_alpha(WHITE, a)); // bright core
        }
        // Impact burst: a ring that blows outward fast then fades, with a hot
        // white flash at the moment of contact.
        VfxKind::Burst { at, big } => {
            let (sx, sy) = view.pos(*at);
            let max_r = (ENTITY_RADIUS * scale).max(6.0) * if *big { 2.8 } else { 1.9 };
            let r = max_r * (0.35 + 0.65 * t.sqrt()); // fast start, easing out
            draw_circle(sx, sy, r * 0.8, with_alpha(v.color, 0.22 * a));
            draw_circle_lines(sx, sy, r, if *big { 3.5 } else { 2.5 }, with_alpha(v.color, 0.9 * a));
            if t < 0.4 {
                let f = 1.0 - t / 0.4;
                draw_circle(sx, sy, r * 0.45, with_alpha(WHITE, 0.8 * f));
            }
        }
        // Heal glow: a soft swelling halo with a few motes drifting upward.
        VfxKind::HealGlow { at } => {
            let (sx, sy) = view.pos(*at);
            let r0 = (ENTITY_RADIUS * scale).max(6.0);
            let r = r0 * (0.9 + 0.7 * t);
            draw_circle(sx, sy, r, with_alpha(v.color, 0.18 * a));
            draw_circle_lines(sx, sy, r, 2.0, with_alpha(v.color, 0.5 * a));
            for k in 0..3 {
                let ang = k as f32 * 2.1 + 0.7;
                let mx = sx + ang.cos() * r0 * 0.7;
                let my = sy + ang.sin() * r0 * 0.35 - 22.0 * t;
                draw_circle(mx, my, 2.5, with_alpha(WHITE, 0.8 * a));
            }
        }
        // Floating combat text: rises (easing out), holds, then fades.
        VfxKind::Text { at, text, size, lift } => {
            let (sx, sy) = view.pos(*at);
            let rise = TEXT_RISE * (1.0 - (1.0 - t) * (1.0 - t));
            let alpha = if t < 0.6 { 1.0 } else { (1.0 - t) / 0.4 };
            let dims = measure_text(text, None, *size as u16, 1.0);
            let x = sx - dims.width / 2.0;
            let y = sy - (ENTITY_RADIUS * scale).max(6.0) - 14.0 - lift - rise;
            draw_text(text, x + 1.0, y + 1.0, *size, with_alpha(BLACK, 0.7 * alpha));
            draw_text(text, x, y, *size, with_alpha(v.color, alpha));
        }
        // Death ring: one slow shockwave marking where the unit fell.
        VfxKind::Death { at } => {
            let (sx, sy) = view.pos(*at);
            let r0 = (ENTITY_RADIUS * scale).max(6.0);
            let r = r0 * (1.0 + 2.2 * t);
            draw_circle(sx, sy, r0 * a, with_alpha(v.color, 0.3 * a));
            draw_circle_lines(sx, sy, r, 2.5, with_alpha(v.color, 0.7 * a));
        }
    }
}

/// Linear blend of two colors.
fn mix(a: Color, b: Color, t: f32) -> Color {
    Color::new(
        lerp(a.r, b.r, t),
        lerp(a.g, b.g, t),
        lerp(a.b, b.b, t),
        lerp(a.a, b.a, t),
    )
}

/// Draw the sim's in-flight projectiles: a glowing head with a short trail
/// back along the inbound path. Their positions ARE sim state — when a shot
/// lands here, its damage lands in the same instant.
fn draw_flights(view: &View, combat: &Combat) {
    for f in combat.flights() {
        let color = skill_color(combat.state.skill(f.skill));
        let (hx, hy) = view.pos_air(f.pos);
        // Trail points back away from the target it's homing on.
        let tp = combat.state.entity(f.target).pos;
        let (dx, dy) = (f.pos.x - tp.x, f.pos.y - tp.y);
        let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        let tail = 1.2; // world units
        let (tx, ty) = view.pos_air(Pos {
            x: f.pos.x + dx / len * tail,
            y: f.pos.y + dy / len * tail,
        });
        let r = (ENTITY_RADIUS * view.scale * 0.5).max(4.0);
        draw_line(tx, ty, hx, hy, r * 0.9, with_alpha(color, 0.35));
        draw_circle(hx, hy, r * 1.8, with_alpha(color, 0.25)); // glow
        draw_circle(hx, hy, r, color);
        draw_circle(hx, hy, r * 0.5, WHITE); // hot core
    }
}

/// A copy of `c` with its alpha scaled by `a`.
fn with_alpha(c: Color, a: f32) -> Color {
    Color::new(c.r, c.g, c.b, c.a * a)
}

/// Bottom of the right column: a DPS/HPS meter ranking every entity by the
/// damage dealt / healing done credited in `combat.tallies`, averaged over
/// the battle so far (real seconds). Returns its top edge so the log above
/// knows where to stop.
fn draw_meter(combat: &Combat) -> f32 {
    let x = screen_width() - LOG_W + 10.0;
    let w = LOG_W - 20.0;
    let secs = (combat.elapsed_ticks() * TICK_INTERVAL).max(TICK_INTERVAL);

    let mut rows: Vec<(&Entity, f32, f32)> = combat
        .state
        .entities
        .iter()
        .map(|e| {
            let t = combat.tally(e.id);
            (e, t.damage / secs, t.healing / secs)
        })
        .collect();
    // Rank by damage output, healing as the tiebreak — the classic meter view.
    rows.sort_by(|a, b| {
        b.1.total_cmp(&a.1).then(b.2.total_cmp(&a.2))
    });
    let max_dps = rows.iter().map(|r| r.1).fold(0.0f32, f32::max);
    let max_hps = rows.iter().map(|r| r.2).fold(0.0f32, f32::max);

    let line_h = 18.0;
    let header_h = 40.0;
    let top = screen_height() - (header_h + line_h * rows.len() as f32 + 8.0);

    draw_line(x - 4.0, top, x + w + 4.0, top, 1.0, Color::new(0.3, 0.32, 0.36, 1.0));
    draw_text("DPS / HPS", x, top + 20.0, 18.0, WHITE);

    // Column layout: name | damage bar | healing bar.
    let name_w = 88.0;
    let gap = 6.0;
    let dps_x = x + name_w;
    let dps_w = (w - name_w - gap) * 0.55;
    let hps_x = dps_x + dps_w + gap;
    let hps_w = w - name_w - gap - dps_w;
    let dim = Color::new(0.55, 0.57, 0.6, 1.0);
    draw_text("dmg/s", dps_x, top + 34.0, 13.0, dim);
    draw_text("heal/s", hps_x, top + 34.0, 13.0, dim);

    let bar = |bx: f32, by: f32, bw: f32, fill: f32, color: Color, value: f32, alpha: f32| {
        draw_rectangle(bx, by - 11.0, bw, 14.0, with_alpha(Color::new(0.16, 0.17, 0.2, 1.0), alpha));
        if fill > 0.0 {
            draw_rectangle(bx, by - 11.0, bw * fill.clamp(0.0, 1.0), 14.0, with_alpha(color, alpha));
        }
        let label = format!("{value:.1}");
        let tw = measure_text(&label, None, 13, 1.0).width;
        draw_text(&label, bx + bw - tw - 3.0, by, 13.0, with_alpha(WHITE, alpha));
    };

    let mut y = top + header_h + 12.0;
    for (e, dps, hps) in rows {
        // The dead keep their totals on the board, just dimmed.
        let alpha = if e.is_alive() { 1.0 } else { 0.4 };
        let name: String = e.name.chars().take(10).collect();
        draw_text(&name, x, y, 15.0, with_alpha(team_color(e.team), alpha));
        let dps_fill = if max_dps > 0.0 { dps / max_dps } else { 0.0 };
        let hps_fill = if max_hps > 0.0 { hps / max_hps } else { 0.0 };
        bar(dps_x, y, dps_w - gap, dps_fill, Color::new(0.75, 0.38, 0.22, 0.9), dps, alpha);
        bar(hps_x, y, hps_w, hps_fill, Color::new(0.3, 0.65, 0.35, 0.9), hps, alpha);
        y += line_h;
    }
    top
}

fn draw_log(log: &[String], bottom: f32) {
    let x = screen_width() - LOG_W + 10.0;
    draw_text("Combat log", x, 30.0, 22.0, WHITE);

    let line_h = 18.0;
    let top = 52.0;
    let rows = ((bottom - top).max(0.0) / line_h) as usize;
    let start = log.len().saturating_sub(rows);
    let mut y = top;
    for line in &log[start..] {
        // Sub-events (damage/heal/etc.) are indented; dim them.
        let color = if line.starts_with(' ') {
            Color::new(0.7, 0.72, 0.75, 1.0)
        } else if line.starts_with('*') {
            Color::new(0.95, 0.85, 0.3, 1.0)
        } else {
            WHITE
        };
        draw_text(line, x, y, 16.0, color);
        y += line_h;
    }
}

fn draw_hud(combat: &Combat, paused: bool, scenario: &str) {
    let state = if combat.is_over() {
        "OVER"
    } else if paused {
        "PAUSED"
    } else {
        "RUNNING"
    };
    let hud = format!(
        "{scenario}  |  tick {}  [{}]   —   Space: pause · G: gambits · R: restart · I: intent · M: menu",
        combat.time, state
    );
    draw_text(&hud, 20.0, 28.0, 22.0, WHITE);
}

// --- gambit editor ----------------------------------------------------------

/// Button/row height of the editor's hand-drawn widgets.
const BTN_H: f32 = 20.0;
/// Height of one list row in the editor panels.
const EDITOR_ROW_H: f32 = 20.0;

/// Rebuild the current scenario but carry the (possibly edited) gambits over —
/// the "restart with my edits" the editor promises. Entity ids are stable
/// across rebuilds of the same scenario, so the maps transplant directly.
fn restart_keeping_edits(
    scenarios: &[(&'static str, fn() -> Combat)],
    current: usize,
    combat: &mut Option<Combat>,
) {
    let mut fresh = scenarios[current].1();
    if let Some(old) = combat.take() {
        fresh.gambits = old.gambits;
        fresh.move_gambits = old.move_gambits;
    }
    *combat = Some(fresh);
}

/// Immediate-mode input context for the editor's hand-drawn widgets: one
/// snapshot of the mouse per frame.
struct Ui {
    mx: f32,
    my: f32,
    clicked: bool,
}

impl Ui {
    fn frame() -> Ui {
        let (mx, my) = mouse_position();
        Ui { mx, my, clicked: is_mouse_button_pressed(MouseButton::Left) }
    }

    fn hover(&self, r: Rect4) -> bool {
        rect_contains(r, self.mx, self.my)
    }

    /// Draw a small labelled button; true when clicked this frame.
    fn button(&self, x: f32, y: f32, w: f32, label: &str) -> bool {
        let r = (x, y, w, BTN_H);
        let hov = self.hover(r);
        let fill = if hov {
            Color::new(0.30, 0.34, 0.42, 1.0)
        } else {
            Color::new(0.20, 0.23, 0.28, 1.0)
        };
        draw_rectangle(x, y, w, BTN_H, fill);
        draw_rectangle_lines(x, y, w, BTN_H, 1.0, Color::new(0.45, 0.50, 0.58, 1.0));
        let d = measure_text(label, None, 15, 1.0);
        draw_text(label, x + (w - d.width) / 2.0, y + BTN_H - 6.0, 15.0, WHITE);
        hov && self.clicked
    }

    /// Selectable list row: highlight + click detection; true when clicked.
    fn row(&self, r: Rect4, selected: bool) -> bool {
        if selected {
            draw_rectangle(r.0, r.1, r.2, r.3, Color::new(0.22, 0.28, 0.38, 1.0));
        } else if self.hover(r) {
            draw_rectangle(r.0, r.1, r.2, r.3, Color::new(0.15, 0.17, 0.21, 1.0));
        }
        self.hover(r) && self.clicked
    }
}

/// Lays a row of toolbar buttons left to right; `drop` remembers where a
/// dropdown hung under the most recently drawn button should anchor.
struct Toolbar<'a> {
    ui: &'a Ui,
    x: f32,
    y: f32,
    drop: (f32, f32),
}

impl<'a> Toolbar<'a> {
    fn new(ui: &'a Ui, x: f32, y: f32) -> Toolbar<'a> {
        Toolbar { ui, x, y, drop: (x, y + BTN_H + 2.0) }
    }

    fn button(&mut self, label: &str, w: f32) -> bool {
        self.drop = (self.x, self.y + BTN_H + 2.0);
        let hit = self.ui.button(self.x, self.y, w, label);
        self.x += w + 6.0;
        hit
    }
}

// --- dropdown menus ---------------------------------------------------------

/// Height of one dropdown row.
const MENU_ROW_H: f32 = 22.0;

/// Which field an open dropdown edits, with the address of the edited thing
/// captured at open time (paths re-resolve on apply, so a stale one is a
/// harmless no-op).
#[derive(Clone)]
enum MenuKind {
    Condition { path: Vec<usize> },
    Target { path: Vec<usize> },
    Skill { path: Vec<usize> },
    Term { index: usize },
}

/// An open dropdown: what it edits, whose gambit, where it hangs, and which
/// submenu is currently flown out.
struct OpenMenu {
    kind: MenuKind,
    /// Entity the menu was opened for (the roster can't change underneath it —
    /// an open menu owns every click — but keep the id explicit anyway).
    entity: usize,
    /// Screen anchor (top-left; clamped to the screen when laid out).
    anchor: (f32, f32),
    /// Expanded submenu, as an index into the entries.
    sub: Option<usize>,
}

enum MenuOutcome<T> {
    StillOpen,
    Close,
    Pick(T),
}

/// Box size fitting the given labels, one per row.
fn menu_box(labels: &[&str]) -> (f32, f32) {
    let w = labels
        .iter()
        .map(|l| measure_text(l, None, 15, 1.0).width)
        .fold(0.0, f32::max);
    (w + 36.0, labels.len() as f32 * MENU_ROW_H + 8.0)
}

/// The main dropdown column, anchored under its button and clamped on-screen.
fn menu_main_rect<T>(om: &OpenMenu, entries: &[editor::MenuEntry<T>]) -> Rect4 {
    let labels: Vec<&str> = entries.iter().map(|e| e.label()).collect();
    let (w, h) = menu_box(&labels);
    let x = om.anchor.0.min(screen_width() - w - 4.0).max(4.0);
    let y = om.anchor.1.min(screen_height() - h - 4.0).max(4.0);
    (x, y, w, h)
}

/// The flyout column of the submenu at `si`, beside its parent row (flipped
/// to the left edge when it wouldn't fit on the right).
fn menu_sub_rect<T>(main: Rect4, entries: &[editor::MenuEntry<T>], si: usize) -> Option<Rect4> {
    let editor::MenuEntry::Sub(_, items) = &entries[si] else {
        return None;
    };
    let labels: Vec<&str> = items.iter().map(|(l, _)| l.as_str()).collect();
    let (w, h) = menu_box(&labels);
    let mut x = main.0 + main.2 - 2.0;
    if x + w > screen_width() - 4.0 {
        x = (main.0 - w + 2.0).max(4.0);
    }
    let y = (main.1 + 4.0 + si as f32 * MENU_ROW_H)
        .min(screen_height() - h - 4.0)
        .max(4.0);
    Some((x, y, w, h))
}

/// Input side of an open dropdown for one frame: fly submenus out on hover,
/// resolve clicks (pick / expand / close), and consume the click either way —
/// an open menu owns the mouse. Drawing happens separately (after the panels)
/// so the overlay sits on top; both share the same layout functions.
fn drive_menu<T: Clone>(
    ui: &mut Ui,
    om: &mut OpenMenu,
    entries: &[editor::MenuEntry<T>],
) -> MenuOutcome<T> {
    let main = menu_main_rect(om, entries);
    // Hovering a submenu entry flies it out. Hovering a plain item leaves an
    // open flyout alone, so the diagonal mouse path into it never slams it
    // shut; picking or expanding elsewhere is what closes it.
    for (i, e) in entries.iter().enumerate() {
        let r = (main.0, main.1 + 4.0 + i as f32 * MENU_ROW_H, main.2, MENU_ROW_H);
        if ui.hover(r) && matches!(e, editor::MenuEntry::Sub(..)) {
            om.sub = Some(i);
        }
    }
    let sub_rect = om.sub.and_then(|si| menu_sub_rect(main, entries, si));
    if !ui.clicked {
        return MenuOutcome::StillOpen;
    }
    ui.clicked = false;
    if let Some(si) = om.sub
        && let Some(sr) = sub_rect
        && let editor::MenuEntry::Sub(_, items) = &entries[si]
    {
        for (j, (_, v)) in items.iter().enumerate() {
            let r = (sr.0, sr.1 + 4.0 + j as f32 * MENU_ROW_H, sr.2, MENU_ROW_H);
            if rect_contains(r, ui.mx, ui.my) {
                return MenuOutcome::Pick(v.clone());
            }
        }
        if rect_contains(sr, ui.mx, ui.my) {
            return MenuOutcome::StillOpen; // the box's padding
        }
    }
    if rect_contains(main, ui.mx, ui.my) {
        for (i, e) in entries.iter().enumerate() {
            let r = (main.0, main.1 + 4.0 + i as f32 * MENU_ROW_H, main.2, MENU_ROW_H);
            if !rect_contains(r, ui.mx, ui.my) {
                continue;
            }
            match e {
                editor::MenuEntry::Item(_, v) => return MenuOutcome::Pick(v.clone()),
                editor::MenuEntry::Sub(..) => {
                    om.sub = if om.sub == Some(i) { None } else { Some(i) };
                    return MenuOutcome::StillOpen;
                }
            }
        }
        return MenuOutcome::StillOpen;
    }
    MenuOutcome::Close
}

/// One dropdown column: shadowed box, hover highlight, `>` on submenu rows.
fn draw_menu_column<'a>(
    ui: &Ui,
    r: Rect4,
    rows: impl Iterator<Item = (&'a str, bool)>,
    expanded: Option<usize>,
) {
    draw_rectangle(r.0 + 3.0, r.1 + 3.0, r.2, r.3, with_alpha(BLACK, 0.35));
    draw_rectangle(r.0, r.1, r.2, r.3, Color::new(0.16, 0.18, 0.22, 1.0));
    draw_rectangle_lines(r.0, r.1, r.2, r.3, 1.5, Color::new(0.50, 0.56, 0.66, 1.0));
    for (i, (label, is_sub)) in rows.enumerate() {
        let rr = (r.0, r.1 + 4.0 + i as f32 * MENU_ROW_H, r.2, MENU_ROW_H);
        if ui.hover(rr) || expanded == Some(i) {
            draw_rectangle(rr.0 + 2.0, rr.1, rr.2 - 4.0, rr.3, Color::new(0.28, 0.34, 0.44, 1.0));
        }
        draw_text(label, rr.0 + 10.0, rr.1 + 16.0, 15.0, WHITE);
        if is_sub {
            draw_text(">", rr.0 + rr.2 - 14.0, rr.1 + 16.0, 15.0, Color::new(0.70, 0.75, 0.80, 1.0));
        }
    }
}

/// Render an open dropdown (main column + flown-out submenu) on top of
/// everything.
fn draw_menu_overlay<T>(ui: &Ui, om: &OpenMenu, entries: &[editor::MenuEntry<T>]) {
    let main = menu_main_rect(om, entries);
    draw_menu_column(
        ui,
        main,
        entries
            .iter()
            .map(|e| (e.label(), matches!(e, editor::MenuEntry::Sub(..)))),
        om.sub,
    );
    if let Some(si) = om.sub
        && let Some(sr) = menu_sub_rect(main, entries, si)
        && let editor::MenuEntry::Sub(_, items) = &entries[si]
    {
        draw_menu_column(ui, sr, items.iter().map(|(l, _)| (l.as_str(), false)), None);
    }
}

/// Drive the open dropdown's input for this frame: consume the click, apply a
/// pick straight to the combat state, close on click-away. Runs *before* the
/// panels (so they render the applied value and never see the menu's click);
/// [`draw_open_menu`] draws the same menu after them, on top.
fn drive_open_menu(es: &mut EditorState, ui: &mut Ui, combat: &mut Combat) {
    let Some(mut om) = es.menu.take() else { return };
    let id = EntityId(om.entity);
    let keep = match om.kind.clone() {
        MenuKind::Condition { path } => match drive_menu(ui, &mut om, &editor::condition_menu()) {
            MenuOutcome::Pick(c) => {
                if let Some(root) = combat.gambits.get_mut(&id)
                    && let Some(node) = editor::node_at_mut(root, &path)
                {
                    node.condition = c;
                }
                false
            }
            MenuOutcome::Close => false,
            MenuOutcome::StillOpen => true,
        },
        MenuKind::Target { path } => match drive_menu(ui, &mut om, &editor::target_menu()) {
            MenuOutcome::Pick(q) => {
                if let Some(root) = combat.gambits.get_mut(&id)
                    && let Some(node) = editor::node_at_mut(root, &path)
                {
                    editor::set_leaf_target(node, q);
                }
                false
            }
            MenuOutcome::Close => false,
            MenuOutcome::StillOpen => true,
        },
        MenuKind::Skill { path } => {
            let known = combat.state.entities[id.0].skills.clone();
            let entries = editor::skill_menu(&known, &combat.state);
            match drive_menu(ui, &mut om, &entries) {
                MenuOutcome::Pick(s) => {
                    if let Some(root) = combat.gambits.get_mut(&id)
                        && let Some(node) = editor::node_at_mut(root, &path)
                    {
                        editor::set_leaf_skill(node, s);
                    }
                    false
                }
                MenuOutcome::Close => false,
                MenuOutcome::StillOpen => true,
            }
        }
        MenuKind::Term { index } => match drive_menu(ui, &mut om, &editor::term_menu()) {
            MenuOutcome::Pick(t) => {
                if let Some(mg) = combat.move_gambits.get_mut(&id)
                    && let Some(slot) = mg.terms.get_mut(index)
                {
                    slot.0 = t;
                }
                false
            }
            MenuOutcome::Close => false,
            MenuOutcome::StillOpen => true,
        },
    };
    if keep {
        es.menu = Some(om);
    }
}

/// Draw the open dropdown last, over the panels.
fn draw_open_menu(es: &EditorState, ui: &Ui, combat: &Combat) {
    let Some(om) = &es.menu else { return };
    match &om.kind {
        MenuKind::Condition { .. } => draw_menu_overlay(ui, om, &editor::condition_menu()),
        MenuKind::Target { .. } => draw_menu_overlay(ui, om, &editor::target_menu()),
        MenuKind::Skill { .. } => {
            let known = combat.state.entities[om.entity].skills.clone();
            draw_menu_overlay(ui, om, &editor::skill_menu(&known, &combat.state));
        }
        MenuKind::Term { .. } => draw_menu_overlay(ui, om, &editor::term_menu()),
    }
}

/// Panel background + border (brighter when the panel has keyboard focus) +
/// title. Content starts below the title line.
fn panel_chrome(x: f32, y: f32, w: f32, h: f32, title: &str, focused: bool) {
    draw_rectangle(x, y, w, h, Color::new(0.12, 0.13, 0.16, 1.0));
    let border = if focused {
        Color::new(0.50, 0.60, 0.80, 1.0)
    } else {
        Color::new(0.30, 0.34, 0.40, 1.0)
    };
    draw_rectangle_lines(x, y, w, h, 2.0, border);
    draw_text(title, x + 8.0, y + 17.0, 18.0, Color::new(0.85, 0.87, 0.90, 1.0));
}

/// The gambit editor screen: roster of both teams on the left (the editor is
/// always tied to one character at a time), the selected character's action
/// gambit tree and movement gambit on the right. Edits mutate the live
/// `Combat` directly — they apply the moment the battle resumes.
fn draw_editor(es: &mut EditorState, combat: &mut Combat, scenario: &str) {
    let mut ui = Ui::frame();

    draw_text(&format!("Gambit editor — {scenario}"), 20.0, 30.0, 24.0, WHITE);
    draw_text(
        "click/Tab: character  ·  click a cell to edit it in place  ·  G/Esc: back to battle  ·  R: restart with edits  ·  M: menu",
        20.0,
        50.0,
        15.0,
        Color::new(0.65, 0.68, 0.72, 1.0),
    );

    let n = combat.state.entities.len();
    if n == 0 {
        return;
    }
    es.entity = es.entity.min(n - 1);
    let id = EntityId(es.entity);

    // Every entity is editable: a bare-leaf root is wrapped into a group and a
    // missing movement gambit becomes an (equivalent) empty one.
    let root = combat.gambits.entry(id).or_insert_with(editor::empty_root);
    editor::normalize_root(root);
    combat
        .move_gambits
        .entry(id)
        .or_insert_with(|| MoveGambit::new(Vec::new()));

    // An open dropdown eats this frame's click before anything underneath
    // sees it; its pick is applied right here so the panels draw the result.
    drive_open_menu(es, &mut ui, combat);

    draw_roster(es, &ui, &combat.state);

    let px = 225.0;
    let pw = screen_width() - px - 16.0;
    let top = 66.0;
    let bottom = screen_height() - 16.0;
    let action_h = ((bottom - top) * 0.60).floor();
    // The skill-detail card sits beside the action panel, showing the skill
    // of whatever rule is selected.
    let skill_w = 215.0;
    let action_w = pw - skill_w - 10.0;
    draw_action_panel(es, &ui, combat, id, px, top, action_w, action_h);
    draw_skill_panel(es, combat, id, px + action_w + 10.0, top, skill_w, action_h);
    let move_y = top + action_h + 10.0;
    draw_move_panel(es, &ui, combat, id, px, move_y, pw, bottom - move_y);

    // The dropdown draws last, over everything.
    draw_open_menu(es, &ui, combat);
}

/// The character list, grouped by team. Clicking a row opens that character.
fn draw_roster(es: &mut EditorState, ui: &Ui, state: &BattleState) {
    let x = 20.0;
    let w = 195.0;
    let mut y = 66.0;
    for team in [Team::Player, Team::Enemy] {
        let label = if team == Team::Player { "PLAYER TEAM" } else { "ENEMY TEAM" };
        draw_text(label, x, y + 12.0, 14.0, Color::new(0.60, 0.63, 0.68, 1.0));
        y += 20.0;
        for e in state.entities.iter().filter(|e| e.team == team) {
            let r = (x, y, w, EDITOR_ROW_H + 2.0);
            if ui.row(r, e.id.0 == es.entity) {
                es.select_entity(e.id.0);
            }
            draw_circle(x + 10.0, y + 11.0, 5.0, team_color(e.team));
            let col = if e.is_alive() { WHITE } else { Color::new(0.50, 0.50, 0.55, 1.0) };
            draw_text(&e.name, x + 22.0, y + 16.0, 17.0, col);
            y += EDITOR_ROW_H + 4.0;
        }
        y += 10.0;
    }
}

/// Wheel-scroll a panel's list and clamp to its content.
fn panel_scroll(ui: &Ui, panel: Rect4, scroll: &mut f32, rows: usize, list_h: f32) {
    if ui.hover(panel) {
        let wheel = mouse_wheel().1;
        if wheel != 0.0 {
            *scroll -= wheel.signum() * EDITOR_ROW_H * 2.0;
        }
    }
    let max = (rows as f32 * EDITOR_ROW_H - list_h).max(0.0);
    *scroll = scroll.clamp(0.0, max);
}

/// An inline-editable table cell: a faint tint marks it live under the mouse;
/// true when clicked this frame.
fn cell(ui: &Ui, r: Rect4) -> bool {
    if ui.hover(r) {
        draw_rectangle(r.0, r.1, r.2, r.3, with_alpha(WHITE, 0.07));
    }
    ui.hover(r) && ui.clicked
}

/// Truncate `text` (15px font) with an ellipsis so it fits `max_w` pixels.
fn fit_text(text: &str, max_w: f32) -> String {
    if measure_text(text, None, 15, 1.0).width <= max_w {
        return text.into();
    }
    let mut s = String::from(text);
    while !s.is_empty() && measure_text(&s, None, 15, 1.0).width + 12.0 > max_w {
        s.pop();
    }
    format!("{s}...")
}

// Cell text palette: one hue per rule component, so the columns read apart.
const COND_COLOR: Color = Color::new(0.95, 0.82, 0.50, 1.0);
const COND_ALWAYS_COLOR: Color = Color::new(0.50, 0.53, 0.58, 1.0);
const TARGET_COLOR: Color = Color::new(0.62, 0.80, 1.00, 1.0);
const SKILL_COLOR: Color = Color::new(0.70, 0.95, 0.70, 1.0);
const GROUP_COLOR: Color = Color::new(0.95, 0.85, 0.45, 1.0);

/// The action-gambit panel: the rule tree as a three-column table —
/// CONDITION | TARGET | SKILL — every cell editable in place (a click opens
/// the matching dropdown right at the cell; clicking a group's body flips its
/// mode). The toolbar keeps only the structural ops. Rows are snapshotted
/// before drawing so a click's mutation never races the borrow.
fn draw_action_panel(
    es: &mut EditorState,
    ui: &Ui,
    combat: &mut Combat,
    id: EntityId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    panel_chrome(x, y, w, h, "Action gambit", es.panel == 0);

    // Snapshot the tree as owned rows: (path, depth) plus the split cells.
    let root = &combat.gambits[&id];
    let rows = editor::rows(root);
    es.sel_action = es.sel_action.min(rows.len().saturating_sub(1));
    let views: Vec<(usize, editor::RowParts)> = rows
        .iter()
        .map(|(p, d)| (*d, editor::row_parts(editor::node_at(root, p).unwrap(), &combat.state)))
        .collect();
    let sel = rows.get(es.sel_action).map(|(p, _)| p.clone());

    // The actor's known skills — the skill dropdown's set and "+ rule" default.
    let known = combat.state.entities[id.0].skills.clone();
    let skill_names: Vec<String> = known
        .iter()
        .map(|&s| combat.state.skill(s).name.clone())
        .collect();

    // --- column layout: CONDITION | TARGET | SKILL ---
    let inner_x = x + 8.0;
    let inner_w = w - 16.0;
    let cw_cond = inner_w * 0.42;
    let cw_target = inner_w * 0.34;
    let cx_cond = inner_x;
    let cx_target = cx_cond + cw_cond + 10.0;
    let cx_skill = cx_target + cw_target + 10.0;
    let cw_skill = inner_x + inner_w - cx_skill;

    let head_y = y + 24.0 + BTN_H + 8.0;
    let head_col = Color::new(0.55, 0.58, 0.64, 1.0);
    draw_text("CONDITION", cx_cond + 2.0, head_y + 11.0, 13.0, head_col);
    draw_text("TARGET", cx_target + 2.0, head_y + 11.0, 13.0, head_col);
    draw_text("SKILL", cx_skill + 2.0, head_y + 11.0, 13.0, head_col);

    let list_y = head_y + 16.0;
    let list_h = h - (list_y - y) - 22.0;
    // Faint column separators through the list area.
    for cx in [cx_target - 5.0, cx_skill - 5.0] {
        draw_line(cx, list_y, cx, list_y + list_h, 1.0, with_alpha(WHITE, 0.08));
    }

    panel_scroll(ui, (x, y, w, h), &mut es.scroll_action, views.len(), list_h);

    for (i, (depth, parts)) in views.iter().enumerate() {
        let ry = list_y + i as f32 * EDITOR_ROW_H - es.scroll_action;
        if ry < list_y - 1.0 || ry + EDITOR_ROW_H > list_y + list_h + 1.0 {
            continue;
        }
        if ui.row((x + 4.0, ry, w - 8.0, EDITOR_ROW_H), i == es.sel_action) {
            es.sel_action = i;
            es.panel = 0;
        }
        let path = &rows[i].0;
        let open_at = |kind: MenuKind, ax: f32| OpenMenu {
            kind,
            entity: id.0,
            anchor: (ax, ry + EDITOR_ROW_H),
            sub: None,
        };

        // Condition cell (every row has one); tree depth indents it.
        let indent = *depth as f32 * 16.0;
        let cond = match parts {
            editor::RowParts::Leaf { condition, .. }
            | editor::RowParts::Group { condition, .. } => condition,
        };
        if cell(ui, (cx_cond, ry, cw_cond, EDITOR_ROW_H)) {
            es.sel_action = i;
            es.panel = 0;
            es.menu = Some(open_at(MenuKind::Condition { path: path.clone() }, cx_cond + indent));
        }
        let cond_col = if cond == "always" { COND_ALWAYS_COLOR } else { COND_COLOR };
        draw_text(
            &fit_text(cond, cw_cond - indent - 8.0),
            cx_cond + 4.0 + indent,
            ry + 15.0,
            15.0,
            cond_col,
        );

        match parts {
            editor::RowParts::Leaf { target, skill, .. } => {
                // Target cell — "= condition" is the passthrough pick.
                if cell(ui, (cx_target, ry, cw_target, EDITOR_ROW_H)) {
                    es.sel_action = i;
                    es.panel = 0;
                    es.menu = Some(open_at(MenuKind::Target { path: path.clone() }, cx_target));
                }
                draw_text(
                    &fit_text(target, cw_target - 8.0),
                    cx_target + 4.0,
                    ry + 15.0,
                    15.0,
                    TARGET_COLOR,
                );

                // Skill cell.
                if cell(ui, (cx_skill, ry, cw_skill, EDITOR_ROW_H)) {
                    es.sel_action = i;
                    es.panel = 0;
                    es.menu = Some(open_at(MenuKind::Skill { path: path.clone() }, cx_skill));
                }
                draw_text(
                    &fit_text(skill, cw_skill - 8.0),
                    cx_skill + 4.0,
                    ry + 15.0,
                    15.0,
                    SKILL_COLOR,
                );
            }
            editor::RowParts::Group { commit, children, .. } => {
                // A group has no target/skill: its body cell spans both
                // columns and a click flips fallthrough <-> commit in place.
                let gw = inner_x + inner_w - cx_target;
                if cell(ui, (cx_target, ry, gw, EDITOR_ROW_H)) {
                    es.sel_action = i;
                    es.panel = 0;
                    let root = combat.gambits.get_mut(&id).unwrap();
                    if let Some(node) = editor::node_at_mut(root, path) {
                        editor::toggle_mode(node);
                    }
                }
                let mode = if *commit { "commit" } else { "fallthrough" };
                let label = format!("group — {mode} · {children} rules");
                draw_text(&fit_text(&label, gw - 8.0), cx_target + 4.0, ry + 15.0, 15.0, GROUP_COLOR);
            }
        }
    }
    if views.is_empty() {
        draw_text(
            "(no rules — this unit only waits; add one with + rule)",
            x + 12.0,
            list_y + 16.0,
            16.0,
            GRAY,
        );
    }
    draw_text(
        &format!("knows: {}", skill_names.join(", ")),
        x + 8.0,
        y + h - 7.0,
        14.0,
        Color::new(0.60, 0.63, 0.68, 1.0),
    );

    // --- toolbar: structural ops only (cells edit everything else inline) ---
    let ty = y + 24.0;
    let mut tb = Toolbar::new(ui, x + 8.0, ty);

    if tb.button("+ rule", 56.0) {
        if let Some(leaf) = editor::new_leaf(&known) {
            let root = combat.gambits.get_mut(&id).unwrap();
            editor::insert_at_selection(root, sel.as_deref(), leaf);
        }
    }
    if tb.button("+ group", 64.0) {
        let root = combat.gambits.get_mut(&id).unwrap();
        editor::insert_at_selection(root, sel.as_deref(), editor::empty_root());
    }
    let Some(path) = sel else { return };
    for (label, bw, up) in [("up", 36.0, true), ("down", 48.0, false)] {
        if tb.button(label, bw) {
            let root = combat.gambits.get_mut(&id).unwrap();
            if editor::shift(root, &path, up) {
                // Follow the moved node: its new path is the same but for the
                // last step, so find where that lands in the fresh row list.
                let mut moved = path.clone();
                let last = moved.last_mut().unwrap();
                *last = if up { *last - 1 } else { *last + 1 };
                if let Some(i) = editor::rows(root).iter().position(|(p, _)| *p == moved) {
                    es.sel_action = i;
                }
            }
        }
    }
    if tb.button("delete", 56.0) {
        let root = combat.gambits.get_mut(&id).unwrap();
        editor::remove_at(root, &path);
        es.sel_action = es.sel_action.saturating_sub(1);
    }
}

/// The skill-detail card beside the action panel: metadata + effects of the
/// skill in the currently selected rule, so a pick's numbers (cost, range,
/// cast commit, what it actually does) are visible while authoring. Read-only.
fn draw_skill_panel(es: &EditorState, combat: &Combat, id: EntityId, x: f32, y: f32, w: f32, h: f32) {
    panel_chrome(x, y, w, h, "Skill details", false);

    let root = &combat.gambits[&id];
    let rows = editor::rows(root);
    let sid = rows
        .get(es.sel_action.min(rows.len().saturating_sub(1)))
        .and_then(|(p, _)| editor::node_at(root, p))
        .and_then(editor::leaf_skill_id);
    let Some(sid) = sid else {
        draw_text(
            "select a rule to inspect",
            x + 10.0,
            y + 46.0,
            15.0,
            GRAY,
        );
        draw_text("its skill", x + 10.0, y + 64.0, 15.0, GRAY);
        return;
    };
    let s = combat.state.skill(sid);

    // Name, tinted like the skill's combat visuals.
    draw_text(&s.name, x + 10.0, y + 46.0, 22.0, skill_color(s));

    // Metadata: label column + value column.
    let ticks = |t: u32| {
        if t == 0 {
            "—".to_string()
        } else {
            format!("{t} ticks ({:.1}s)", t as f32 * TICK_INTERVAL)
        }
    };
    let stats: [(&str, String, Color); 5] = [
        (
            "element",
            s.damage_type.map_or("—".into(), |dt| format!("{dt:?}")),
            damage_color(s.damage_type),
        ),
        ("mp cost", if s.cost == 0 { "free".into() } else { s.cost.to_string() }, WHITE),
        (
            "range",
            if s.range >= 90.0 { "map-wide".into() } else { editor::num(s.range) },
            WHITE,
        ),
        ("cooldown", ticks(s.cooldown), WHITE),
        (
            "cast time",
            if s.cast_time == 0 { "instant".into() } else { ticks(s.cast_time) },
            WHITE,
        ),
    ];
    let label_col = Color::new(0.60, 0.63, 0.68, 1.0);
    let mut ty = y + 72.0;
    for (label, value, col) in &stats {
        draw_text(label, x + 10.0, ty, 15.0, label_col);
        draw_text(&fit_text(value, w - 96.0), x + 86.0, ty, 15.0, *col);
        ty += 19.0;
    }

    // What resolving it does, one effect per line.
    ty += 8.0;
    draw_text("ON USE", x + 10.0, ty, 13.0, label_col);
    ty += 17.0;
    for effect in &s.effects {
        let line = editor::describe_effect(effect);
        draw_text(&format!("- {}", fit_text(&line, w - 30.0)), x + 10.0, ty, 15.0, WHITE);
        ty += 18.0;
        if ty > y + h - 10.0 {
            break;
        }
    }
}

/// The movement-gambit panel: the weighted scoring terms as a WEIGHT | TERM
/// table — the term cell edits in place via the dropdown, while the toolbar
/// nudges weight / distance and reorders.
fn draw_move_panel(
    es: &mut EditorState,
    ui: &Ui,
    combat: &mut Combat,
    id: EntityId,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) {
    panel_chrome(
        x,
        y,
        w,
        h,
        "Movement gambit  (weighted terms, blended — not priority rules)",
        es.panel == 1,
    );

    let terms_len = combat.move_gambits[&id].terms.len();
    es.sel_move = es.sel_move.min(terms_len.saturating_sub(1));
    let labels: Vec<(String, String)> = combat.move_gambits[&id]
        .terms
        .iter()
        .map(|(t, wt)| (format!("{wt:+.1}"), editor::describe_term(t)))
        .collect();

    // --- column layout: WEIGHT | TERM ---
    let inner_x = x + 8.0;
    let cw_wt = 58.0;
    let cx_term = inner_x + cw_wt + 10.0;
    let cw_term = x + w - 8.0 - cx_term;

    let head_y = y + 24.0 + BTN_H + 8.0;
    let head_col = Color::new(0.55, 0.58, 0.64, 1.0);
    draw_text("WEIGHT", inner_x + 2.0, head_y + 11.0, 13.0, head_col);
    draw_text("TERM", cx_term + 2.0, head_y + 11.0, 13.0, head_col);

    let list_y = head_y + 16.0;
    let list_h = h - (list_y - y) - 6.0;
    draw_line(cx_term - 5.0, list_y, cx_term - 5.0, list_y + list_h, 1.0, with_alpha(WHITE, 0.08));

    panel_scroll(ui, (x, y, w, h), &mut es.scroll_move, labels.len(), list_h);
    for (i, (wt, term)) in labels.iter().enumerate() {
        let ry = list_y + i as f32 * EDITOR_ROW_H - es.scroll_move;
        if ry < list_y - 1.0 || ry + EDITOR_ROW_H > list_y + list_h + 1.0 {
            continue;
        }
        if ui.row((x + 4.0, ry, w - 8.0, EDITOR_ROW_H), i == es.sel_move) {
            es.sel_move = i;
            es.panel = 1;
        }
        draw_text(wt, inner_x + 4.0, ry + 15.0, 15.0, WHITE);
        // The term cell edits in place: a click opens the term dropdown here.
        if cell(ui, (cx_term, ry, cw_term, EDITOR_ROW_H)) {
            es.sel_move = i;
            es.panel = 1;
            es.menu = Some(OpenMenu {
                kind: MenuKind::Term { index: i },
                entity: id.0,
                anchor: (cx_term, ry + EDITOR_ROW_H),
                sub: None,
            });
        }
        draw_text(&fit_text(term, cw_term - 8.0), cx_term + 4.0, ry + 15.0, 15.0, TARGET_COLOR);
    }
    if labels.is_empty() {
        draw_text(
            "(no terms — this unit holds position; add one with + term)",
            x + 12.0,
            list_y + 16.0,
            16.0,
            GRAY,
        );
    }

    // --- toolbar: numeric nudges + structural ops (the term edits inline) ---
    let ty = y + 24.0;
    let mut tb = Toolbar::new(ui, x + 8.0, ty);

    if tb.button("+ term", 58.0) {
        combat.move_gambits.get_mut(&id).unwrap().terms.push(editor::default_term());
    }
    if terms_len == 0 {
        return;
    }
    let i = es.sel_move;
    for (label, delta) in [("wt -", -0.1f32), ("wt +", 0.1)] {
        if tb.button(label, 44.0) {
            let (_, wt) = &mut combat.move_gambits.get_mut(&id).unwrap().terms[i];
            // Round to one decimal so repeated nudges don't accumulate float dust.
            *wt = (((*wt + delta) * 10.0).round() / 10.0).clamp(-5.0, 5.0);
        }
    }
    for (label, delta) in [("rng -", -0.5f32), ("rng +", 0.5)] {
        if tb.button(label, 48.0) {
            let (t, _) = &mut combat.move_gambits.get_mut(&id).unwrap().terms[i];
            editor::adjust_ideal(t, delta);
        }
    }
    if tb.button("up", 36.0) && i > 0 {
        combat.move_gambits.get_mut(&id).unwrap().terms.swap(i, i - 1);
        es.sel_move = i - 1;
    }
    if tb.button("down", 48.0) && i + 1 < terms_len {
        combat.move_gambits.get_mut(&id).unwrap().terms.swap(i, i + 1);
        es.sel_move = i + 1;
    }
    if tb.button("delete", 56.0) {
        combat.move_gambits.get_mut(&id).unwrap().terms.remove(i);
        es.sel_move = es.sel_move.saturating_sub(1);
    }
}

// --- event formatting ------------------------------------------------------

fn format_event(c: &Combat, ev: &Event) -> String {
    let name = |id: EntityId| c.state.entity(id).name.clone();
    let skill = |id: SkillId| c.state.skill(id).name.clone();
    match ev {
        Event::Acted { actor, skill: s, targets } => {
            let ts: Vec<String> = targets.iter().map(|&t| name(t)).collect();
            format!("{} -> {} @ {}", name(*actor), skill(*s), ts.join(", "))
        }
        Event::Waited(a) => format!("{} waits", name(*a)),
        Event::Damage { target, amount, weakness, .. } => {
            let tag = if *weakness { " (weak!)" } else { "" };
            format!("   {} -{amount:.0}{tag}", name(*target))
        }
        Event::Reflected { bearer, attacker, .. } => {
            format!("   {} reflects the spell at {}", name(*bearer), name(*attacker))
        }
        Event::Chained { from, to, .. } => {
            format!("   {} arcs to {}", name(*from), name(*to))
        }
        Event::Heal { target, amount } => format!("   {} +{amount:.0} hp", name(*target)),
        Event::Inflicted { target, kind, stacks } => {
            format!("   {} {kind:?} x{stacks}", name(*target))
        }
        Event::Cleansed { target } => format!("   {} cleansed", name(*target)),
        Event::MpDrained { target, amount } => {
            format!("   {} -{amount:.0} mp", name(*target))
        }
        Event::StartedCast { actor, skill: s, targets } => {
            let ts: Vec<String> = targets.iter().map(|&t| name(t)).collect();
            format!("{} begins {} @ {}", name(*actor), skill(*s), ts.join(", "))
        }
        Event::Fizzled { actor, skill: s } => {
            format!("   {}'s {} fizzles", name(*actor), skill(*s))
        }
        Event::Died(t) => format!("   {} defeated", name(*t)),
        Event::Victory(team) => format!("*** {team:?} wins ***"),
    }
}
