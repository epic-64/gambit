// The gambit/battle API surface is defined ahead of its consumers (many enum
// variants, filters, and builders aren't exercised until the game is built on
// top), so allow dead code crate-wide for now.
#![allow(dead_code)]

//! gambit — a 2D semi-turn-based RPG built around a modular gambit system.
//!
//! This binary is the Macroquad viewer for the (still flat, terrain-free)
//! combat core: it steps `Combat` on a fixed timer and draws each entity's HP
//! and action bars, their movement, and casting state, plus a live event log.
//! See CLAUDE.md for the design and `cargo test` for the behaviour specs.

mod battle;
mod combat;
mod eval;
mod gambit;
mod scenario;

use macroquad::prelude::*;

use battle::{Entity, EntityId, SkillId, Team, ENTITY_RADIUS};
use combat::{Combat, Event};

/// Arena size in world units (entity positions live in this space).
const WORLD_W: f32 = 20.0;
const WORLD_H: f32 = 12.0;
/// Seconds of real time per simulation tick.
const TICK_INTERVAL: f32 = 0.25;
/// Width reserved on the right for the event log.
const LOG_W: f32 = 300.0;

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
    let mut combat = scenario::demo();
    let mut log: Vec<String> = Vec::new();
    let mut acc = 0.0f32;
    let mut paused = false;

    loop {
        // --- input ---
        if is_key_pressed(KeyCode::Space) {
            paused = !paused;
        }
        if is_key_pressed(KeyCode::R) {
            combat = scenario::demo();
            log.clear();
            acc = 0.0;
            paused = false;
        }

        // --- update: step the sim on a fixed timer ---
        if !paused && !combat.is_over() {
            acc += get_frame_time();
            let mut steps = 0;
            while acc >= TICK_INTERVAL && steps < 4 {
                acc -= TICK_INTERVAL;
                steps += 1;
                for ev in combat.tick() {
                    log.push(format_event(&combat, &ev));
                }
            }
            // Keep the log from growing without bound.
            const MAX_LOG: usize = 500;
            if log.len() > MAX_LOG {
                log.drain(0..log.len() - MAX_LOG);
            }
        }

        // --- draw ---
        clear_background(Color::new(0.10, 0.11, 0.13, 1.0));
        draw_arena();
        for e in &combat.state.entities {
            draw_entity(e, combat.is_casting(e.id));
        }
        draw_log(&log);
        draw_hud(&combat, paused);

        next_frame().await;
    }
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
fn world_scale() -> f32 {
    let (_, _, aw, ah) = arena_rect();
    (aw / WORLD_W).min(ah / WORLD_H)
}

fn world_to_screen(wx: f32, wy: f32) -> (f32, f32) {
    let (ax, ay, _, _) = arena_rect();
    let scale = world_scale();
    (ax + wx * scale, ay + wy * scale)
}

// --- drawing ---------------------------------------------------------------

fn draw_arena() {
    let (sx, sy) = world_to_screen(0.0, 0.0);
    let (ex, ey) = world_to_screen(WORLD_W, WORLD_H);
    draw_rectangle(sx, sy, ex - sx, ey - sy, Color::new(0.14, 0.16, 0.19, 1.0));
    draw_rectangle_lines(sx, sy, ex - sx, ey - sy, 2.0, Color::new(0.3, 0.34, 0.4, 1.0));
}

fn draw_entity(e: &Entity, casting: bool) {
    let (sx, sy) = world_to_screen(e.pos.x, e.pos.y);
    let alive = e.is_alive();
    // Draw the token at the shared collision radius, so overlaps (or the lack of
    // them) are visible. A small floor keeps it legible when zoomed out.
    let r = (ENTITY_RADIUS * world_scale()).max(6.0);
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

fn draw_hud(combat: &Combat, paused: bool) {
    let state = if combat.is_over() {
        "OVER"
    } else if paused {
        "PAUSED"
    } else {
        "RUNNING"
    };
    let hud = format!(
        "gambit  |  tick {}  [{}]   —   Space: pause · R: restart",
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
