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

use battle::{Entity, EntityId, SkillId, Team, ENTITY_RADIUS};
use combat::{Combat, Event};
use terrain::{Terrain, Tile3};

/// Seconds of real time per simulation tick.
const TICK_INTERVAL: f32 = 0.25;
/// Width reserved on the right for the event log.
const LOG_W: f32 = 300.0;

/// Arena size in world units. Taken from the running battle's bounds so
/// differently-sized scenario maps all render to fit.
type World = (f32, f32);

/// A snapshot of the per-entity values the viewer animates, captured just before
/// a tick so the draw pass can interpolate from the previous tick to the current
/// one. The sim still advances in discrete `TICK_INTERVAL` steps; this only
/// smooths what's *shown*, so nothing here touches the ATB or sim cadence.
#[derive(Clone, Copy)]
struct Snap {
    pos: battle::Pos,
    action_bar: f32,
    hp: f32,
}

impl Snap {
    fn of(e: &Entity) -> Snap {
        Snap { pos: e.pos, action_bar: e.action_bar, hp: e.hp }
    }
}

/// Snapshot every entity's animated state, keyed by id.
fn capture(combat: &Combat) -> HashMap<EntityId, Snap> {
    combat.state.entities.iter().map(|e| (e.id, Snap::of(e))).collect()
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
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
    let mut acc = 0.0f32;
    let mut paused = false;
    // State one tick behind the sim, so the draw pass can interpolate toward the
    // current tick and render smoothly instead of jumping 4×/sec.
    let mut prev: HashMap<EntityId, Snap> = HashMap::new();

    loop {
        match screen {
            Screen::Menu => {
                // Number keys pick a scenario and drop into the battle.
                for i in 0..scenarios.len() {
                    if digit_key(i).is_some_and(is_key_pressed) {
                        let c = scenarios[i].1();
                        prev = capture(&c);
                        combat = Some(c);
                        current = i;
                        log.clear();
                        acc = 0.0;
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
                    let c = scenarios[current].1();
                    prev = capture(&c);
                    combat = Some(c);
                    log.clear();
                    acc = 0.0;
                    paused = false;
                }
                if is_key_pressed(KeyCode::M) || is_key_pressed(KeyCode::Escape) {
                    screen = Screen::Menu;
                }

                let Some(combat) = combat.as_mut() else {
                    continue;
                };

                // --- update: step the sim on a fixed timer ---
                if !paused && !combat.is_over() {
                    acc += get_frame_time();
                    let mut steps = 0;
                    while acc >= TICK_INTERVAL && steps < 4 {
                        acc -= TICK_INTERVAL;
                        steps += 1;
                        // Remember the state entering this tick so the draw pass
                        // can interpolate from it toward the post-tick state.
                        prev = capture(combat);
                        for ev in combat.tick() {
                            log.push(format_event(combat, &ev));
                        }
                    }
                    // Keep the log from growing without bound.
                    const MAX_LOG: usize = 500;
                    if log.len() > MAX_LOG {
                        log.drain(0..log.len() - MAX_LOG);
                    }
                }

                // --- draw ---
                let world = combat.state.bounds;
                clear_background(Color::new(0.10, 0.11, 0.13, 1.0));
                draw_arena(world);
                if let Some(t) = combat.state.terrain.as_ref() {
                    draw_terrain(world, t);
                }
                // Interpolate from the previous tick toward the current one so
                // motion and bars read smoothly. Frozen when paused/over so the
                // view reflects the true sim state.
                let alpha = if paused || combat.is_over() {
                    1.0
                } else {
                    (acc / TICK_INTERVAL).clamp(0.0, 1.0)
                };
                for e in &combat.state.entities {
                    let p = prev.get(&e.id).copied().unwrap_or_else(|| Snap::of(e));
                    draw_entity(world, e, combat.is_casting(e.id), p, alpha);
                }
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
        "In battle:  Space pause  ·  R restart  ·  M / Esc menu",
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

fn draw_entity(world: World, e: &Entity, casting: bool, prev: Snap, alpha: f32) {
    // Interpolated (rendered) values — the sim itself still lives at e.*.
    let px = lerp(prev.pos.x, e.pos.x, alpha);
    let py = lerp(prev.pos.y, e.pos.y, alpha);
    let hp = lerp(prev.hp, e.hp, alpha);
    let action_bar = lerp(prev.action_bar, e.action_bar, alpha);
    let (sx, sy) = world_to_screen(world, px, py);
    let alive = e.is_alive();
    // Draw the token at the shared collision radius, so overlaps (or the lack of
    // them) are visible. A small floor keeps it legible when zoomed out.
    let r = (ENTITY_RADIUS * world_scale(world)).max(6.0);
    let base = if e.team == Team::Player {
        Color::new(0.35, 0.55, 0.95, 1.0)
    } else {
        Color::new(0.9, 0.4, 0.35, 1.0)
    };
    let col = if alive {
        base
    } else {
        Color::new(0.3, 0.3, 0.32, 1.0)
    };
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
    draw_text(&format!("{:.0}", hp), sx - 9.0, sy + 5.0, 16.0, WHITE);

    let bw = 54.0;
    let bh = 6.0;
    let bx = sx - bw / 2.0;

    // HP bar (above the token).
    let hy = sy - r - 8.0;
    draw_rectangle(bx, hy, bw, bh, Color::new(0.25, 0.05, 0.05, 1.0));
    let hp_frac = (hp / e.max_hp).clamp(0.0, 1.0);
    draw_rectangle(bx, hy, bw * hp_frac, bh, Color::new(0.35, 0.8, 0.4, 1.0));

    // Action bar (below the token).
    let ay = sy + r + 2.0;
    draw_rectangle(bx, ay, bw, bh, Color::new(0.15, 0.15, 0.17, 1.0));
    let ab = action_bar.clamp(0.0, 1.0);
    draw_rectangle(bx, ay, bw * ab, bh, Color::new(0.95, 0.85, 0.3, 1.0));

    // Compact status readout (e.g. "Poison x2").
    if !e.statuses.is_empty() {
        let s: Vec<String> = e
            .statuses
            .iter()
            .map(|st| format!("{:?}x{}", st.kind, st.stacks))
            .collect();
        draw_text(&s.join(" "), bx, ay + 16.0, 14.0, Color::new(0.8, 0.7, 0.9, 1.0));
    }
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
        "{scenario}  |  tick {}  [{}]   —   Space: pause · R: restart · M: menu",
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
        Event::Damage { target, amount, weakness } => {
            let tag = if *weakness { " (weak!)" } else { "" };
            format!("   {} -{amount:.0}{tag}", name(*target))
        }
        Event::Heal { target, amount } => format!("   {} +{amount:.0} hp", name(*target)),
        Event::Inflicted { target, kind, stacks } => {
            format!("   {} {kind:?} x{stacks}", name(*target))
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
