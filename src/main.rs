// The gambit/battle API surface is defined ahead of its consumers (many enum
// variants, filters, and builders aren't exercised until the game is built on
// top), so allow dead code crate-wide for now.
#![allow(dead_code)]

//! gambit — a 2D semi-turn-based RPG built around a modular gambit system.
//!
//! This binary is the Macroquad viewer for the combat core: it steps `Combat` on
//! a fixed timer and draws the terrain (elevation shading, walls, pits) plus each
//! entity's HP and action bars, movement, and casting state, and a live event
//! log. See CLAUDE.md for the design and `cargo test` for the behaviour specs.

mod battle;
mod combat;
mod eval;
mod gambit;
mod nav;
mod scenario;
mod terrain;

use std::collections::HashMap;

use macroquad::prelude::*;

use battle::{
    DamageType, Effect, Entity, EntityId, Pos, Skill, SkillId, StatusKind, Team, ENTITY_RADIUS,
};
use combat::{Combat, Event};
use eval::Pull;
use terrain::{Terrain, Tile3};

/// Seconds of real time per simulation tick.
const TICK_INTERVAL: f32 = 0.25;
/// Width reserved on the right for the event log.
const LOG_W: f32 = 300.0;

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

/// Which screen the viewer is showing: the scenario picker or a live battle.
enum Screen {
    Menu,
    Playing,
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
                        paused = false;
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
                    combat = Some(scenarios[current].1());
                    log.clear();
                    vfx.clear();
                    flash.clear();
                    paused = false;
                }
                if is_key_pressed(KeyCode::M) || is_key_pressed(KeyCode::Escape) {
                    screen = Screen::Menu;
                }
                if is_key_pressed(KeyCode::I) {
                    show_intent = !show_intent;
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

                // --- draw ---
                let world = combat.state.bounds;
                clear_background(Color::new(0.10, 0.11, 0.13, 1.0));
                draw_arena(world);
                if let Some(t) = combat.state.terrain.as_ref() {
                    draw_terrain(world, t);
                }
                // Everything below draws combat.state directly — the sim is the
                // single source of truth; nothing is interpolated or predicted.
                // Intent lines go under the tokens: where each unit is heading
                // and what it's positioning relative to (or casting at).
                if show_intent {
                    for e in &combat.state.entities {
                        if e.is_alive() {
                            draw_intent(world, e, combat);
                        }
                    }
                }
                for e in &combat.state.entities {
                    let fl = flash
                        .get(&e.id)
                        .map(|&(remaining, c)| (remaining / FLASH_LIFE, c));
                    draw_entity(world, e, combat.is_casting(e.id), fl);
                }
                // Attack visuals ride on top of the tokens: fading pierce-beams
                // (event decoration) and live projectiles (sim state).
                for v in &vfx {
                    draw_vfx(world, v);
                }
                draw_flights(world, combat);
                draw_log(&log);
                draw_hud(combat, paused, scenarios[current].0);
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
        "In battle:  Space pause  ·  R restart  ·  I intent lines  ·  M / Esc menu",
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

/// World-units → screen-pixels factor (uniform, so hitboxes read true).
fn world_scale(world: World) -> f32 {
    let (_, _, aw, ah) = arena_rect();
    (aw / world.0).min(ah / world.1)
}

fn world_to_screen(world: World, wx: f32, wy: f32) -> (f32, f32) {
    let (ax, ay, _, _) = arena_rect();
    let scale = world_scale(world);
    (ax + wx * scale, ay + wy * scale)
}

// --- drawing ---------------------------------------------------------------

fn draw_arena(world: World) {
    let (sx, sy) = world_to_screen(world, 0.0, 0.0);
    let (ex, ey) = world_to_screen(world, world.0, world.1);
    draw_rectangle(sx, sy, ex - sx, ey - sy, Color::new(0.14, 0.16, 0.19, 1.0));
    draw_rectangle_lines(sx, sy, ex - sx, ey - sy, 2.0, Color::new(0.3, 0.34, 0.4, 1.0));
}

/// Shade each tile by elevation; walls (raised, impassable) read as stone, pits
/// (low, impassable) as dark holes, walkable ground lightens with height. A faint
/// elevation label marks anything off the ground plane.
fn draw_terrain(world: World, t: &Terrain) {
    let ts = t.tile_size;
    for r in 0..t.rows {
        for c in 0..t.cols {
            let Some(tile) = t.tile(c, r) else { continue };
            let (sx, sy) = world_to_screen(world, c as f32 * ts, r as f32 * ts);
            let (ex, ey) = world_to_screen(world, (c + 1) as f32 * ts, (r + 1) as f32 * ts);
            let (w, h) = (ex - sx, ey - sy);
            draw_rectangle(sx, sy, w, h, tile_color(tile));
            draw_rectangle_lines(sx, sy, w, h, 1.0, Color::new(0.0, 0.0, 0.0, 0.18));
            if tile.elevation != 0 {
                let label = format!("{}", tile.elevation);
                draw_text(&label, sx + 3.0, sy + 13.0, 13.0, Color::new(1.0, 1.0, 1.0, 0.35));
            }
        }
    }
}

fn tile_color(t: Tile3) -> Color {
    if !t.passable {
        if t.elevation >= 1 {
            Color::new(0.32, 0.30, 0.34, 1.0) // wall / raised block
        } else {
            Color::new(0.05, 0.05, 0.07, 1.0) // pit
        }
    } else {
        // Walkable ground: lightens with elevation (slightly green).
        let l = (0.16 + t.elevation as f32 * 0.07).clamp(0.08, 0.6);
        Color::new(l * 0.9, l, l * 0.85, 1.0)
    }
}

fn draw_entity(world: World, e: &Entity, casting: bool, flash: Option<(f32, Color)>) {
    let (sx, sy) = world_to_screen(world, e.pos.x, e.pos.y);
    let alive = e.is_alive();
    // Draw the token at the shared collision radius, so overlaps (or the lack of
    // them) are visible. A small floor keeps it legible when zoomed out.
    let r = (ENTITY_RADIUS * world_scale(world)).max(6.0);
    let base = if e.team == Team::Player {
        Color::new(0.35, 0.55, 0.95, 1.0)
    } else {
        Color::new(0.9, 0.4, 0.35, 1.0)
    };
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
    draw_circle(sx, sy, r, col);
    draw_circle_lines(sx, sy, r, 2.0, Color::new(0.0, 0.0, 0.0, 0.4));

    // A casting unit is rooted mid-spell — ring it and label it.
    if alive && casting {
        draw_circle_lines(sx, sy, r + 6.0, 2.5, Color::new(0.95, 0.85, 0.3, 0.9));
        draw_text("casting", sx - 24.0, sy + r + 22.0, 16.0, Color::new(0.95, 0.85, 0.3, 1.0));
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

    // HP bar (above the token).
    let hy = sy - r - 8.0;
    draw_rectangle(bx, hy, bw, bh, Color::new(0.25, 0.05, 0.05, 1.0));
    let hp_frac = (e.hp / e.max_hp).clamp(0.0, 1.0);
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

/// Draw one entity's *intent* — what it is trying to do before anything
/// resolves. Movement intent is a chosen destination, not a target: the dim
/// line + ring marks the stand point the gambit picked, and the colored lines
/// show what it is positioning relative to (red = drawn toward, teal = pushed
/// away — drawn from the threat *through* the mover so the flee reads as a
/// push, not a pursuit). A rooted caster instead gets a gold line to each
/// committed target. No lines at all = holding position by choice.
fn draw_intent(world: World, e: &Entity, combat: &Combat) {
    let (sx, sy) = world_to_screen(world, e.pos.x, e.pos.y);

    // Mid-lunge: the dash is the intent — mark who it's diving on.
    if let Some(t) = combat.dash_target(e.id) {
        let tp = combat.state.entity(t).pos;
        let (tx, ty) = world_to_screen(world, tp.x, tp.y);
        draw_line(sx, sy, tx, ty, 2.0, INTENT_TOWARD);
        return;
    }

    // Casting: movement is suppressed, the committed targets are the intent.
    if let Some(targets) = combat.cast_targets(e.id) {
        for &t in targets {
            let tp = combat.state.entity(t).pos;
            let (tx, ty) = world_to_screen(world, tp.x, tp.y);
            draw_line(sx, sy, tx, ty, 1.5, INTENT_CAST);
        }
        return;
    }

    let Some(intent) = combat.move_intent(e.id) else {
        return;
    };
    // Where it's heading — always truthful, even for reference-free intents
    // like seeking high ground.
    let (gx, gy) = world_to_screen(world, intent.goal.x, intent.goal.y);
    draw_line(sx, sy, gx, gy, 1.5, INTENT_GOAL);
    draw_circle_lines(gx, gy, 4.0, 1.5, INTENT_GOAL);
    // What it's positioning relative to.
    for &(r, pull) in &intent.refs {
        let (rx, ry) = world_to_screen(world, r.x, r.y);
        match pull {
            Pull::Toward => draw_line(sx, sy, rx, ry, 1.5, INTENT_TOWARD),
            Pull::Away => {
                let (dx, dy) = (e.pos.x - r.x, e.pos.y - r.y);
                let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
                let (ex, ey) = world_to_screen(
                    world,
                    e.pos.x + dx / len * AWAY_STUB,
                    e.pos.y + dy / len * AWAY_STUB,
                );
                draw_line(rx, ry, ex, ey, 1.5, INTENT_AWAY);
            }
        }
    }
}

/// Draw one transient combat visual, keyed by kind. Everything eases on the
/// same normalized age `t` and fades out by end of life.
fn draw_vfx(world: World, v: &Vfx) {
    let t = (v.age / v.life).clamp(0.0, 1.0);
    let scale = world_scale(world);
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
            let (sx, sy) = world_to_screen(world, from.x, from.y);
            let (ex, ey) = world_to_screen(world, to.x + nx * ext, to.y + ny * ext);
            let w = (ENTITY_RADIUS * scale * 0.5).max(3.0);
            draw_line(sx, sy, ex, ey, w * 1.6, with_alpha(v.color, 0.28 * a)); // glow
            draw_line(sx, sy, ex, ey, w * 0.7, with_alpha(WHITE, a)); // bright core
        }
        // Impact burst: a ring that blows outward fast then fades, with a hot
        // white flash at the moment of contact.
        VfxKind::Burst { at, big } => {
            let (sx, sy) = world_to_screen(world, at.x, at.y);
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
            let (sx, sy) = world_to_screen(world, at.x, at.y);
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
            let (sx, sy) = world_to_screen(world, at.x, at.y);
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
            let (sx, sy) = world_to_screen(world, at.x, at.y);
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
fn draw_flights(world: World, combat: &Combat) {
    let scale = world_scale(world);
    for f in combat.flights() {
        let color = skill_color(combat.state.skill(f.skill));
        let (hx, hy) = world_to_screen(world, f.pos.x, f.pos.y);
        // Trail points back away from the target it's homing on.
        let tp = combat.state.entity(f.target).pos;
        let (dx, dy) = (f.pos.x - tp.x, f.pos.y - tp.y);
        let len = (dx * dx + dy * dy).sqrt().max(f32::EPSILON);
        let tail = 1.2; // world units
        let (tx, ty) =
            world_to_screen(world, f.pos.x + dx / len * tail, f.pos.y + dy / len * tail);
        let r = (ENTITY_RADIUS * scale * 0.5).max(4.0);
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

fn draw_log(log: &[String]) {
    let x = screen_width() - LOG_W + 10.0;
    draw_text("Combat log", x, 30.0, 22.0, WHITE);

    let line_h = 18.0;
    let top = 52.0;
    let rows = ((screen_height() - top) / line_h) as usize;
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
        "{scenario}  |  tick {}  [{}]   —   Space: pause · R: restart · I: intent · M: menu",
        combat.time, state
    );
    draw_text(&hud, 20.0, 28.0, 22.0, WHITE);
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
        Event::Heal { target, amount } => format!("   {} +{amount:.0} hp", name(*target)),
        Event::Inflicted { target, kind, stacks } => {
            format!("   {} {kind:?} x{stacks}", name(*target))
        }
        Event::Cleansed { target } => format!("   {} cleansed", name(*target)),
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
