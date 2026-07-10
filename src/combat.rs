//! The combat loop: an ATB (active-time-battle) driver in continuous time
//! measured in ticks. `step(dt)` integrates the continuous quantities
//! (movement, bar fill, MP regen) over any fraction of a tick and fires the
//! discrete phases (statuses, cooldowns, cast resolution, gambit decisions
//! via [`decide`]) on whole-tick boundaries; `tick()` = `step(1.0)`.
//! Engine-agnostic â€” no Macroquad â€” so the whole fight is testable.

use std::collections::HashMap;

use crate::battle::{
    BattleState, DamageType, EntityId, Effect, Pos, Skill, SkillId, Status, StatusKind, Team,
    ENTITY_RADIUS,
};
use crate::eval::{self, decide, Action, MoveIntent};
use crate::gambit::{MoveGambit, Node};

/// Action-bar value at which an entity gets to act.
const READY: f32 = 1.0;
/// Damage multiplier applied when a target is weak to the skill's element.
const WEAKNESS_MULT: f32 = 1.5;
/// Per-stack, per-tick amounts for damage-/heal-over-time statuses.
const POISON_PER_STACK: f32 = 3.0;
const BURN_PER_STACK: f32 = 5.0;
const REGEN_PER_STACK: f32 = 4.0;
/// Fraction of incoming damage a `Shield` status absorbs (flat, regardless of
/// stacks â€” like `SNARE_SLOW`, magnitude isn't per-status yet).
const SHIELD_REDUCTION: f32 = 0.5;
/// Outgoing skill-damage bonus while the attacker is `Enrage`d. DoT pulses are
/// unaffected (they have no attacker at pulse time).
const ENRAGE_BONUS: f32 = 0.5;
/// Fraction of dealt damage an [`Effect::Drain`] returns to the actor as healing.
pub const DRAIN_RATIO: f32 = 0.5;
/// Extra damage a target with [`StatusKind::Exposed`] takes, from every source
/// (0.1 == +10%; DoT pulses included — the multiplier lives on the target's
/// side). Flat regardless of stacks, like `SHIELD_REDUCTION`.
pub const EXPOSED_DAMAGE_BONUS: f32 = 0.1;
/// HP returned to an attacker each time it lands a damaging hit on a
/// [`StatusKind::Lifeleech`] bearer. Foes of the bearer only (the mark is
/// authored by *their* side), and DoT pulses proc nothing (no attacker at
/// pulse time).
pub const LEECH_HEAL_ON_HIT: f32 = 3.0;
/// Fraction of incoming healing a [`StatusKind::MortalWound`] on the recipient
/// cuts away â€” every source (Heal, Regen pulse, aura drip, drain-return) is
/// reduced alike. Flat regardless of stacks, like `SHIELD_REDUCTION`.
pub const WOUND_HEAL_REDUCTION: f32 = 0.5;
/// World-unit radius of every aura (see [`StatusKind::is_aura`]): teammates
/// within this distance of a bearer get the aura's benefit, teammates outside
/// don't. Uniform for now â€” one knob, like `ENTITY_RADIUS`.
pub const AURA_RADIUS: f32 = 6.0;
/// HP a `RegenAura` restores per tick to each covered teammate. Deliberately
/// weak next to a Heal (38) or Regen stacks (4/stack) â€” steady drip, not triage.
/// Tune with care: it multiplies across the whole covered team, so it's
/// effectively another fraction of a healer (1.5 here once let the red team
/// sweep the skirmish without a single loss). Continuous (integrates over
/// every step slice, like MP regen), so it never spams per-pulse events.
const AURA_REGEN_PER_TICK: f32 = 0.75;
/// Outgoing damage bonus for attackers covered by a teammate's `MightAura`.
const AURA_MIGHT_BONUS: f32 = 0.05;
/// A hit at or under this distance lands immediately (you're in contact);
/// anything farther is a projectile that has to *travel* â€” its effects apply
/// on impact, not at fire. Per shot, not per skill: a long-range skill fired
/// point-blank still connects instantly.
pub const MELEE_RANGE: f32 = 3.0;
/// World units a projectile flies per tick (homing on its target).
const PROJECTILE_SPEED: f32 = 12.0;
/// World units a gap-closer travels per tick â€” a fast, visible lunge, not a
/// teleport. Well above any `move_speed`, so a dash always catches its mark.
const DASH_SPEED: f32 = 8.0;

/// Something that happened during a tick â€” a log for tests and (later) the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Acted {
        actor: EntityId,
        skill: SkillId,
        targets: Vec<EntityId>,
    },
    /// A ready entity's gambit produced no action (fallthrough exhausted or a
    /// Commit context chose to wait).
    Waited(EntityId),
    Damage {
        target: EntityId,
        amount: f32,
        weakness: bool,
        /// The element the hit carried (None for untyped hits and DoT pulses).
        /// Purely informational â€” weakness is already resolved into `amount`.
        dmg_type: Option<DamageType>,
    },
    Heal {
        target: EntityId,
        amount: f32,
    },
    /// A spell ward on `bearer` ate an incoming damage spell and re-cast it at
    /// `attacker` â€” the rebound's own Damage/Inflicted events follow. The ward
    /// charge is already consumed at this point.
    Reflected {
        bearer: EntityId,
        attacker: EntityId,
        /// The element the reflected spell carries (for the viewer's beam).
        dmg_type: Option<DamageType>,
    },
    /// A chain-damage arc jumped from one victim to the next; the damage it
    /// carried follows as its own `Damage` event. Emitted so the viewer can
    /// draw the arc between the two bodies.
    Chained {
        from: EntityId,
        to: EntityId,
        /// The element the arc carries (the skill's damage type).
        dmg_type: Option<DamageType>,
    },
    Inflicted {
        target: EntityId,
        kind: StatusKind,
        stacks: u32,
    },
    /// One or more harmful statuses were stripped off the target by a Cleanse.
    /// Only emitted when something was actually removed.
    Cleansed {
        target: EntityId,
    },
    /// MP was stolen from the target (already credited to the drainer). Only
    /// emitted when something was actually taken â€” a dry pool drains nothing.
    MpDrained {
        target: EntityId,
        amount: f32,
    },
    /// A cast-time skill was begun; the actor is now rooted until it resolves.
    /// MP and cooldown are already committed at this point.
    StartedCast {
        actor: EntityId,
        skill: SkillId,
        targets: Vec<EntityId>,
    },
    /// An attack came to nothing: a completed cast with no committed target
    /// still valid (dead or out of range), or a projectile/dash whose target
    /// died in flight. The counterplay to committing to a big attack.
    Fizzled {
        actor: EntityId,
        skill: SkillId,
    },
    Died(EntityId),
    Victory(Team),
}

/// A cast-time skill in progress: the chosen action, frozen at cast start, plus
/// the ticks left before it resolves. While present the caster is rooted and its
/// ATB is frozen.
struct Cast {
    action: Action,
    remaining: u32,
}

/// A fired attack in flight: it homes on its committed target at
/// [`PROJECTILE_SPEED`] and the skill's effects land on impact â€” damage is a
/// consequence of the hit *arriving*, never of the trigger pull. Its `pos` is
/// sim state; the viewer draws it directly.
pub struct Flight {
    pub actor: EntityId,
    pub skill: SkillId,
    pub target: EntityId,
    pub pos: Pos,
}

/// A gap-closer in progress: the actor lunges at its primary target at
/// [`DASH_SPEED`] (continuous â€” not a teleport), and the skill's effects land
/// at contact. `budget` is the travel allowance left (the `Effect::Dash` max);
/// exhausting it delivers the hit from wherever the lunge ended.
struct DashRun {
    action: Action,
    budget: f32,
}

/// Owns the mutable battle plus each entity's gambit tree, and advances time.
pub struct Combat {
    pub state: BattleState,
    /// Each entity's action ruleset, keyed by id. An entity with no gambit never acts.
    pub gambits: HashMap<EntityId, Node>,
    /// Each entity's movement gambit (weighted positional-scoring terms), keyed
    /// by id. An entity with no movement gambit holds position (the
    /// pre-movement behaviour).
    pub move_gambits: HashMap<EntityId, MoveGambit>,
    /// Casts currently in flight, keyed by caster. Presence == "is casting".
    casts: HashMap<EntityId, Cast>,
    /// Projectiles currently in the air (see [`Flight`]).
    flights: Vec<Flight>,
    /// Gap-closer lunges in progress, keyed by the dashing actor. While
    /// present the actor is committed: no gambit movement, ATB frozen.
    dashes: HashMap<EntityId, DashRun>,
    /// Each mover's movement intent from the latest tick (goal stand point +
    /// term references), for the viewer's intent lines. Absent while holding
    /// position, casting, stunned, or dead.
    move_intents: HashMap<EntityId, MoveIntent>,
    /// Whole-tick boundaries crossed so far.
    pub time: u32,
    /// Fractional progress (0..1) toward the next tick boundary â€” the
    /// accumulator `step` integrates continuous phases against.
    frac: f32,
    over: bool,
}

impl Combat {
    pub fn new(state: BattleState, gambits: HashMap<EntityId, Node>) -> Self {
        Combat {
            state,
            gambits,
            move_gambits: HashMap::new(),
            casts: HashMap::new(),
            flights: Vec::new(),
            dashes: HashMap::new(),
            move_intents: HashMap::new(),
            time: 0,
            frac: 0.0,
            over: false,
        }
    }

    /// Attach movement gambits (builder-style).
    pub fn with_movement(mut self, move_gambits: HashMap<EntityId, MoveGambit>) -> Self {
        self.move_gambits = move_gambits;
        self
    }

    pub fn is_over(&self) -> bool {
        self.over
    }

    /// Whether `id` is mid-cast (rooted, ATB frozen).
    pub fn is_casting(&self, id: EntityId) -> bool {
        self.casts.contains_key(&id)
    }

    /// Ticks remaining on `id`'s cast, if any (for UI cast bars).
    pub fn cast_remaining(&self, id: EntityId) -> Option<u32> {
        self.casts.get(&id).map(|c| c.remaining)
    }

    /// The committed targets of `id`'s in-flight cast, if any â€” the cast's
    /// intent, drawn by the viewer while the caster is rooted.
    pub fn cast_targets(&self, id: EntityId) -> Option<&[EntityId]> {
        self.casts.get(&id).map(|c| c.action.targets.as_slice())
    }

    /// `id`'s movement intent from the latest tick, if it moved (see
    /// [`eval::MoveIntent`]).
    pub fn move_intent(&self, id: EntityId) -> Option<&MoveIntent> {
        self.move_intents.get(&id)
    }

    /// Projectiles currently in the air â€” sim state the viewer draws directly.
    pub fn flights(&self) -> &[Flight] {
        &self.flights
    }

    /// Whether `id` is mid-lunge (gap-closer in progress).
    pub fn is_dashing(&self, id: EntityId) -> bool {
        self.dashes.contains_key(&id)
    }

    /// The entity `id` is lunging at, if it is mid-dash.
    pub fn dash_target(&self, id: EntityId) -> Option<EntityId> {
        self.dashes.get(&id).and_then(|d| d.action.targets.first().copied())
    }

    /// Run ticks until the battle ends or `max_ticks` is reached, returning the
    /// full event log. The cap is a safety net against never-ending stalemates.
    pub fn run(&mut self, max_ticks: u32) -> Vec<Event> {
        let mut log = Vec::new();
        for _ in 0..max_ticks {
            if self.over {
                break;
            }
            log.extend(self.tick());
        }
        log
    }

    /// Advance the simulation by exactly one whole tick â€” the unit all
    /// durations (cast times, cooldowns, statuses, per-tick DoT) are authored
    /// in. Equivalent to `step(1.0)`; tests use this for exact, reproducible
    /// stepping.
    pub fn tick(&mut self) -> Vec<Event> {
        self.step(1.0)
    }

    /// Advance the simulation by `dt` *ticks* â€” fractions welcome; the viewer
    /// passes real frame time scaled by its tick interval. The sim is the
    /// single source of truth for rendering (the viewer draws `state`
    /// verbatim), so smoothness lives *here*, not in the renderer:
    /// **continuous** quantities (movement, action-bar fill, MP regen)
    /// integrate over every slice, while the **discrete** phases (status
    /// pulses, cooldowns, cast resolution, gambit decisions) fire exactly on
    /// whole-tick boundaries.
    pub fn step(&mut self, mut dt: f32) -> Vec<Event> {
        let mut events = Vec::new();
        while dt > 0.0 && !self.over {
            let slice = dt.min(1.0 - self.frac);
            self.advance_continuous(slice, &mut events);
            self.frac += slice;
            dt -= slice;
            // Float-tolerant boundary test: many small frame slices may sum
            // to fractionally under 1.0.
            if self.frac >= 1.0 - 1e-5 {
                self.frac = 0.0;
                self.boundary(&mut events);
            }
        }
        events
    }

    /// The between-boundaries phases, each scaled by the tick-fraction `dt`
    /// that elapsed: drift movement, dash lunges, projectile flight, action-bar
    /// fill, and MP regen. Dash contacts and projectile impacts deliver their
    /// effects *here*, mid-slice â€” landing is a moment in continuous time.
    fn advance_continuous(&mut self, dt: f32, events: &mut Vec<Event>) {
        // Movement integrates before each boundary, so a caster stays rooted
        // through the tick its cast resolves on ("rooted until it resolves")
        // and roots the instant its cast starts.
        self.tick_movement(dt);
        self.advance_dashes(dt, events);
        self.advance_flights(dt, events);

        // Fill bars, capped at READY so a waiting entity doesn't accumulate.
        // Casting units are frozen (bar stays at 0 until the cast resolves),
        // stunned ones too (held until the stun wears off), and dashing ones
        // (committed to the lunge until it connects).
        for e in &mut self.state.entities {
            if e.is_alive()
                && !e.is_stunned()
                && !self.casts.contains_key(&e.id)
                && !self.dashes.contains_key(&e.id)
            {
                e.action_bar = (e.action_bar + e.atb_speed * dt).min(READY);
            }
        }

        self.tick_mp(dt);
        self.tick_auras(dt);
    }

    /// Regen-aura upkeep: every living entity covered by a teammate's
    /// `RegenAura` (see [`covered_by_aura`]) recovers `AURA_REGEN_PER_TICK`,
    /// scaled by the slice. Continuous like MP regen â€” a steady drip the HP
    /// bar shows directly, with no per-pulse events to spam the log. Coverage
    /// is sampled per slice, so drifting out of the radius cuts the drip that
    /// instant.
    fn tick_auras(&mut self, dt: f32) {
        let bearers: Vec<(Team, Pos)> = self
            .state
            .entities
            .iter()
            .filter(|e| e.is_alive() && e.status(StatusKind::RegenAura).is_some())
            .map(|e| (e.team, e.pos))
            .collect();
        if bearers.is_empty() {
            return;
        }
        for e in &mut self.state.entities {
            if !e.is_alive() {
                continue;
            }
            let covered = bearers
                .iter()
                .any(|&(team, pos)| team == e.team && pos.dist(e.pos) <= AURA_RADIUS);
            if covered {
                // The aura drip is healing too â€” a MortalWound cuts it alike.
                let mult = if e.status(StatusKind::MortalWound).is_some() {
                    1.0 - WOUND_HEAL_REDUCTION
                } else {
                    1.0
                };
                e.hp = (e.hp + AURA_REGEN_PER_TICK * mult * dt).min(e.max_hp);
            }
        }
    }

    /// A whole-tick boundary: apply status pulses, tick down cooldowns,
    /// resolve casts, and let every ready entity's gambit decide.
    fn boundary(&mut self, events: &mut Vec<Event>) {
        self.time += 1;

        self.tick_statuses(events);
        if self.check_over(events) {
            return;
        }
        self.tick_cooldowns();

        // Advance in-flight casts; any that complete resolve (or fizzle) now.
        self.advance_casts(events);
        if self.check_over(events) {
            return;
        }

        // Everyone at/over the threshold acts this tick, fullest bar first
        // (ties broken by id for determinism). Stunned units can't act.
        let mut ready: Vec<EntityId> = self
            .state
            .entities
            .iter()
            .filter(|e| e.is_alive() && !e.is_stunned() && e.action_bar >= READY)
            .map(|e| e.id)
            .collect();
        ready.sort_by(|&a, &b| {
            let ba = self.state.entity(a).action_bar;
            let bb = self.state.entity(b).action_bar;
            bb.partial_cmp(&ba)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.0.cmp(&b.0))
        });

        for actor in ready {
            if self.over {
                break;
            }
            // May have died to a same-tick effect, or lost readiness.
            if !self.state.entity(actor).is_alive() || self.state.entity(actor).action_bar < READY {
                continue;
            }
            let decision = self
                .gambits
                .get(&actor)
                .and_then(|tree| decide(tree, actor, &self.state));

            match decision {
                Some(action) => {
                    let skill = self.state.skill(action.skill).clone();
                    // Acting breaks stealth: committing to any action â€” an
                    // instant or a cast start â€” reveals a sneaking actor.
                    // (A sneak skill re-applies its status a moment later,
                    // when its own effects resolve.)
                    self.state.entities[actor.0]
                        .statuses
                        .retain(|s| s.kind != StatusKind::Sneak);
                    // Spend the turn and commit MP + cooldown up front â€” this is
                    // "commit at cast start" for cast-time skills.
                    self.commit_cost(actor, action.skill, &skill);
                    self.state.entities[actor.0].action_bar = 0.0;

                    if skill.cast_time > 0 {
                        // Root the caster; the action resolves later, re-validated.
                        events.push(Event::StartedCast {
                            actor,
                            skill: action.skill,
                            targets: action.targets.clone(),
                        });
                        self.casts.insert(
                            actor,
                            Cast {
                                remaining: skill.cast_time,
                                action,
                            },
                        );
                    } else {
                        // Instant: the *decision* resolves now, but the effects
                        // land when the attack does â€” immediately at point-blank,
                        // on impact for a projectile, at contact for a dash.
                        events.push(Event::Acted {
                            actor,
                            skill: action.skill,
                            targets: action.targets.clone(),
                        });
                        self.deliver(actor, action, &skill, events);
                        self.check_over(events);
                    }
                }
                None => {
                    // Keep the bar full and re-evaluate next tick (e.g. once a
                    // cooldown expires the entity can finally act).
                    events.push(Event::Waited(actor));
                }
            }
        }
    }

    /// Drift every alive, non-casting entity along its movement gambit, scaled
    /// by the tick-fraction `dt`.
    fn tick_movement(&mut self, dt: f32) {
        // Intents describe *this* slice's movement only â€” stale ones (a mover
        // that died, started casting, or now holds) must not linger.
        self.move_intents.clear();
        let movers: Vec<EntityId> = self
            .state
            .entities
            .iter()
            .filter(|e| {
                e.is_alive()
                    && !e.is_stunned()
                    && !self.casts.contains_key(&e.id)
                    && !self.dashes.contains_key(&e.id) // the lunge owns the legs
                    && self.move_gambits.contains_key(&e.id)
            })
            .map(|e| e.id)
            .collect();

        for id in movers {
            if let Some(gambit) = self.move_gambits.get(&id) {
                if let Some(intent) = eval::move_intent(gambit, id, &self.state, dt) {
                    let from = self.state.entity(id).pos;
                    let mut resolved = self.resolve_collisions(id, intent.step);
                    // A step eaten by a body merely standing *in the way*
                    // slides along that body's tangent instead of halting, so
                    // a unit shoulders past a scrum toward its goal. Movers
                    // still stop at their actual quarry (see
                    // [`slide_around_block`]) â€” this only unblocks through
                    // traffic.
                    if resolved.dist(from) < 0.25 * from.dist(intent.step)
                        && let Some(d) = self.slide_around_block(id, intent.step, intent.goal)
                    {
                        let slid = self.resolve_collisions(id, d);
                        if slid.dist(from) > resolved.dist(from) {
                            resolved = slid;
                        }
                    }
                    self.state.entities[id.0].pos = resolved;
                    self.move_intents.insert(id, intent);
                }
            }
        }
    }

    /// When a drift step stalls against another unit's body, find the
    /// tangential detour around it. `step_dest` is the blocked one-slice step,
    /// `goal` the stand point the mover is ultimately heading for. Returns a
    /// slide destination, or `None` when halting is the *correct* outcome:
    /// nothing is actually in front, or the blocker is the mover's own quarry
    /// (the goal sits on its body â€” you stop at what you're hunting, you slide
    /// around what's merely in the way).
    fn slide_around_block(&self, mover: EntityId, step_dest: Pos, goal: Pos) -> Option<Pos> {
        let from = self.state.entity(mover).pos;
        let (vx, vy) = (step_dest.x - from.x, step_dest.y - from.y);
        let vlen = (vx * vx + vy * vy).sqrt();
        if vlen <= f32::EPSILON {
            return None;
        }
        let min_dist = 2.0 * ENTITY_RADIUS;
        // The body in the way: the nearest other unit ahead of the motion and
        // close enough for this step to have run into it.
        let blocker = self
            .state
            .living()
            .filter(|&o| o != mover)
            .map(|o| self.state.entity(o).pos)
            .filter(|bp| {
                (bp.x - from.x) * vx + (bp.y - from.y) * vy > 0.0
                    && from.dist(*bp) <= min_dist + vlen + 1e-3
            })
            .min_by(|a, b| from.dist(*a).total_cmp(&from.dist(*b)))?;
        if goal.dist(blocker) <= min_dist {
            return None; // the blocker IS the destination â€” halt at contact
        }
        let (mut nx, mut ny) = (from.x - blocker.x, from.y - blocker.y);
        let nlen = (nx * nx + ny * ny).sqrt();
        if nlen <= f32::EPSILON {
            return None;
        }
        nx /= nlen;
        ny /= nlen;
        // Keep the motion's component along the contact tangent, at full step
        // length. Dead-aligned motion (no tangent component) deflects to
        // whichever side leads toward the goal.
        let dot = vx * nx + vy * ny;
        let (mut tx, mut ty) = (vx - dot * nx, vy - dot * ny);
        let tlen = (tx * tx + ty * ty).sqrt();
        if tlen <= vlen * 0.05 {
            let (px, py) = (-ny, nx);
            let side = (goal.x - from.x) * px + (goal.y - from.y) * py;
            let s = if side >= 0.0 { 1.0 } else { -1.0 };
            (tx, ty) = (px * s, py * s);
        } else {
            (tx, ty) = (tx / tlen, ty / tlen);
        }
        Some(Pos {
            x: from.x + tx * vlen,
            y: from.y + ty * vlen,
        })
    }

    /// Radius-aware separation for a moving entity: keep its circle inside the
    /// arena and out of every other living entity's circle. Only the mover is
    /// displaced (movers are resolved one at a time, in id order), so this is
    /// order-stable and always terminates. A few relaxation passes settle the
    /// common case of touching several neighbours at once. This is the "don't
    /// obliviously stack on top of each other" sanity â€” true steering/avoidance
    /// arrives with the terrain layer.
    fn resolve_collisions(&self, mover: EntityId, dest: Pos) -> Pos {
        let r = ENTITY_RADIUS;
        let from = self.state.entity(mover).pos; // where the mover started this tick
        let min_dist = r + ENTITY_RADIUS; // uniform radius: same for every pair
        let mut p = self.state.clamp_within(dest, r);

        for _ in 0..4 {
            let mut adjusted = false;
            for other in self.state.living() {
                if other == mover {
                    continue;
                }
                let o = self.state.entity(other);
                let dx = p.x - o.pos.x;
                let dy = p.y - o.pos.y;
                let d2 = dx * dx + dy * dy;
                if d2 < min_dist * min_dist {
                    let d = d2.sqrt();
                    // Push the mover out to just-touching along the contact
                    // normal. If the centres coincide (a full overshoot onto the
                    // target), separate back toward where the mover came from so
                    // it stops on the near side, not teleporting past.
                    let (nx, ny) = if d > f32::EPSILON {
                        (dx / d, dy / d)
                    } else {
                        let (bx, by) = (from.x - o.pos.x, from.y - o.pos.y);
                        let bd = (bx * bx + by * by).sqrt();
                        if bd > f32::EPSILON {
                            (bx / bd, by / bd)
                        } else {
                            (1.0, 0.0)
                        }
                    };
                    p = self.state.clamp_within(
                        Pos {
                            x: o.pos.x + nx * min_dist,
                            y: o.pos.y + ny * min_dist,
                        },
                        r,
                    );
                    adjusted = true;
                }
            }
            if !adjusted {
                break;
            }
        }

        // Terrain backstop (the implicit "don't walk into a wall" sanity): if
        // entity separation shoved the mover onto an impassable tile or across a
        // cliff, hold at the start position instead â€” `from` was valid. A* and
        // the flee/steer step already avoid walls; this only catches the rare
        // push-into-wall case, so a plain hold is enough (no re-search needed).
        if let Some(t) = self.state.terrain.as_ref()
            && !t.walkable(t.tile_of(from), t.tile_of(p))
        {
            return from;
        }
        p
    }

    /// Tick down every in-flight cast; resolve or fizzle the ones that complete.
    fn advance_casts(&mut self, events: &mut Vec<Event>) {
        let casting: Vec<EntityId> = self.casts.keys().copied().collect();
        let mut completed: Vec<(EntityId, Action)> = Vec::new();
        for id in casting {
            if !self.state.entity(id).is_alive() {
                self.casts.remove(&id); // caster died mid-cast â€” the cast is lost
                continue;
            }
            let cast = self.casts.get_mut(&id).unwrap();
            cast.remaining -= 1;
            if cast.remaining == 0 {
                completed.push((id, self.casts.remove(&id).unwrap().action));
            }
        }

        for (actor, action) in completed {
            if self.over {
                break;
            }
            self.resolve_cast(actor, action, events);
        }
    }

    /// Resolve a completed cast, re-validating its committed targets against the
    /// *current* world: a target that has died, drifted out of range, or
    /// vanished into sneak is dropped, and a cast with no valid target left
    /// fizzles.
    fn resolve_cast(&mut self, actor: EntityId, mut action: Action, events: &mut Vec<Event>) {
        let skill = self.state.skill(action.skill).clone();
        let actor_pos = self.state.entity(actor).pos;
        action.targets.retain(|&t| {
            let e = self.state.entity(t);
            e.is_alive()
                && actor_pos.dist(e.pos) <= skill.range
                && self.state.visible_to(actor, t)
        });

        if action.targets.is_empty() {
            events.push(Event::Fizzled {
                actor,
                skill: action.skill,
            });
            return;
        }

        events.push(Event::Acted {
            actor,
            skill: action.skill,
            targets: action.targets.clone(),
        });
        self.deliver(actor, action, &skill, events);
        self.check_over(events);
    }

    // --- resolution ------------------------------------------------------

    /// Pay a skill's MP cost and start its cooldown. Done at action time â€”
    /// which for a cast-time skill is *cast start*, not resolution.
    fn commit_cost(&mut self, actor: EntityId, skill_id: SkillId, skill: &Skill) {
        let a = &mut self.state.entities[actor.0];
        a.mp = (a.mp - skill.cost as f32).max(0.0);
        if skill.cooldown > 0 {
            a.cooldowns.insert(skill_id, skill.cooldown);
        }
    }

    /// Hand a resolved action to the world. Gap-closers start a dash run; a
    /// target beyond [`MELEE_RANGE`] gets a projectile spawned at the actor;
    /// a point-blank target takes the effects immediately. Cost/cooldown are
    /// paid separately (see [`commit_cost`]) so cast completions don't pay
    /// twice. Damage/heal/status land when the attack *arrives* â€” the sim owns
    /// impact timing; the viewer just draws it.
    fn deliver(&mut self, actor: EntityId, action: Action, skill: &Skill, events: &mut Vec<Event>) {
        let dash_max = skill.effects.iter().find_map(|e| match e {
            Effect::Dash { max } => Some(*max),
            _ => None,
        });
        if let Some(max) = dash_max {
            self.dashes.insert(actor, DashRun { action, budget: max });
            return;
        }

        let from = self.state.entity(actor).pos;
        for &tgt in &action.targets {
            if from.dist(self.state.entity(tgt).pos) > MELEE_RANGE {
                self.flights.push(Flight {
                    actor,
                    skill: action.skill,
                    target: tgt,
                    pos: from,
                });
            } else {
                self.apply_effects_to(actor, tgt, skill, events);
            }
        }
    }

    /// Advance every dash lunge by `dt` ticks' worth of [`DASH_SPEED`]: chase
    /// the target's *current* position (straight-line â€” a lunge is committed,
    /// not routed), and deliver the skill's effects at contact or when the
    /// travel budget runs out. A dash whose target dies mid-lunge fizzles;
    /// a stunned dasher is held mid-lunge until the stun wears off.
    fn advance_dashes(&mut self, dt: f32, events: &mut Vec<Event>) {
        let mut ids: Vec<EntityId> = self.dashes.keys().copied().collect();
        ids.sort_unstable_by_key(|id| id.0); // deterministic resolution order
        for id in ids {
            if self.over {
                return;
            }
            if !self.state.entity(id).is_alive() {
                self.dashes.remove(&id); // the dasher died mid-lunge
                continue;
            }
            if self.state.entity(id).is_stunned() {
                continue;
            }
            let run = &self.dashes[&id];
            let Some(&tgt) = run.action.targets.first() else {
                self.dashes.remove(&id);
                continue;
            };
            if !self.state.entity(tgt).is_alive() {
                let run = self.dashes.remove(&id).unwrap();
                events.push(Event::Fizzled { actor: id, skill: run.action.skill });
                continue;
            }

            let from = self.state.entity(id).pos;
            let tp = self.state.entity(tgt).pos;
            let contact = 2.0 * ENTITY_RADIUS;
            let d = from.dist(tp);
            let step = (DASH_SPEED * dt).min(run.budget);
            let arrives = d - contact <= step;
            let travel = (d - contact).clamp(0.0, step);
            if travel > f32::EPSILON {
                let dest = Pos {
                    x: from.x + (tp.x - from.x) / d * travel,
                    y: from.y + (tp.y - from.y) / d * travel,
                };
                let resolved = self.resolve_collisions(id, dest);
                self.state.entities[id.0].pos = resolved;
            }

            let run = self.dashes.get_mut(&id).unwrap();
            run.budget -= step;
            // Budget always shrinks by the intended step, so a lunge blocked by
            // a wall or a body still terminates â€” the hit lands from wherever
            // the lunge ended, exactly like an out-of-budget one.
            if arrives || run.budget <= f32::EPSILON {
                let run = self.dashes.remove(&id).unwrap();
                let skill = self.state.skill(run.action.skill).clone();
                for &t in &run.action.targets {
                    self.apply_effects_to(id, t, &skill, events);
                }
                self.check_over(events);
            }
        }
    }

    /// Advance every projectile by `dt` ticks' worth of [`PROJECTILE_SPEED`],
    /// homing on its target's *current* position. Reaching the target's body
    /// is the impact: the skill's effects apply there and then. A flight whose
    /// target died first fizzles away.
    fn advance_flights(&mut self, dt: f32, events: &mut Vec<Event>) {
        let step = PROJECTILE_SPEED * dt;
        let mut i = 0;
        while i < self.flights.len() {
            if self.over {
                return;
            }
            let (target, fpos) = {
                let f = &self.flights[i];
                (f.target, f.pos)
            };
            // Sneaking does NOT shake a projectile already in the air â€”
            // stealth blocks new targeting, not physics (invisibility, not
            // invulnerability). Only death fizzles a flight.
            if !self.state.entity(target).is_alive() {
                let f = self.flights.swap_remove(i);
                events.push(Event::Fizzled { actor: f.actor, skill: f.skill });
                continue;
            }
            let tp = self.state.entity(target).pos;
            let d = fpos.dist(tp);
            if d <= step + ENTITY_RADIUS {
                // Impact this slice.
                let f = self.flights.swap_remove(i);
                let skill = self.state.skill(f.skill).clone();
                self.apply_effects_to(f.actor, f.target, &skill, events);
                self.check_over(events);
                continue;
            }
            self.flights[i].pos = Pos {
                x: fpos.x + (tp.x - fpos.x) / d * step,
                y: fpos.y + (tp.y - fpos.y) / d * step,
            };
            i += 1;
        }
    }

    /// Apply a skill's per-target effects (damage/heal/status) to one target â€”
    /// the moment an attack lands. `actor` is whoever landed the hit (for
    /// enrage scaling and drain return). `Effect::Dash` is actor-centric and
    /// handled by the dash run, never here.
    fn apply_effects_to(
        &mut self,
        actor: EntityId,
        target: EntityId,
        skill: &Skill,
        events: &mut Vec<Event>,
    ) {
        if self.try_reflect(actor, target, skill, events) {
            return;
        }
        for effect in &skill.effects {
            match effect {
                Effect::Damage(base) => {
                    self.apply_damage(Some(actor), target, *base, skill.damage_type, events);
                }
                Effect::ChainDamage { base, jumps, falloff, jump_range } => {
                    // Full damage on the primary target, then arc: each jump
                    // strikes the nearest not-yet-struck foe within range and
                    // sight of the *last victim*, at falloffÃ— the previous hit.
                    // Nearest-first with ties broken by entity order keeps the
                    // arc path deterministic.
                    let mut amount = *base;
                    self.apply_damage(Some(actor), target, amount, skill.damage_type, events);
                    let actor_team = self.state.entity(actor).team;
                    let mut struck = vec![target];
                    let mut from = target;
                    for _ in 0..*jumps {
                        let from_pos = self.state.entity(from).pos;
                        let next = self
                            .state
                            .entities
                            .iter()
                            .filter(|e| {
                                e.is_alive()
                                    && e.team != actor_team
                                    && !struck.contains(&e.id)
                                    && from_pos.dist(e.pos) <= *jump_range
                                    && self.state.line_of_sight(from_pos, e.pos)
                                // No visibility check: an arc is loose energy,
                                // not aimed targeting â€” it finds a sneaking
                                // foe just fine (invisibility, not
                                // invulnerability).
                            })
                            .min_by(|a, b| {
                                from_pos
                                    .dist(a.pos)
                                    .partial_cmp(&from_pos.dist(b.pos))
                                    .unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|e| e.id);
                        let Some(next) = next else { break };
                        amount *= falloff;
                        events.push(Event::Chained {
                            from,
                            to: next,
                            dmg_type: skill.damage_type,
                        });
                        self.apply_damage(Some(actor), next, amount, skill.damage_type, events);
                        struck.push(next);
                        from = next;
                    }
                }
                Effect::ExecuteDamage(base) => {
                    // 1% more per 1% of the target's missing HP: Ã—1 at full
                    // health up to Ã—2 at death's door. Scaled off HP *before*
                    // this hit, then fed through the normal multiplier stack.
                    let missing = 1.0 - self.state.entity(target).hp_pct();
                    let amount = base * (1.0 + missing.clamp(0.0, 1.0));
                    self.apply_damage(Some(actor), target, amount, skill.damage_type, events);
                }
                Effect::Drain(base) => {
                    // Heal back a cut of what actually landed â€” a resisted or
                    // shielded hit returns less, a dead-target hit nothing.
                    let dealt =
                        self.apply_damage(Some(actor), target, *base, skill.damage_type, events);
                    if dealt > 0.0 {
                        self.apply_heal(actor, dealt * DRAIN_RATIO, events);
                    }
                }
                Effect::Heal(amount) => self.apply_heal(target, *amount, events),
                Effect::Inflict {
                    kind,
                    stacks,
                    duration,
                } => self.apply_status(target, *kind, *stacks, *duration, events),
                Effect::Cleanse => self.apply_cleanse(target, events),
                Effect::DrainMp(amount) => {
                    let t = &mut self.state.entities[target.0];
                    if t.is_alive() {
                        let taken = amount.min(t.mp);
                        if taken > 0.0 {
                            t.mp -= taken;
                            let a = &mut self.state.entities[actor.0];
                            a.mp = (a.mp + taken).min(a.max_mp);
                            events.push(Event::MpDrained { target, amount: taken });
                        }
                    }
                }
                Effect::Dash { .. } => {}
            }
        }
    }

    /// The `SpellWard` parry: if a hostile damage *spell* (elemental,
    /// non-physical damage type) lands on a warded target, burn one ward
    /// charge and re-cast the whole skill from the bearer at the attacker â€”
    /// so a reflected chain arcs on through the attacker's side, a reflected
    /// drain feeds the bearer, and any status riders land on the caster.
    /// Returns true if the hit was consumed. A warded attacker bounces the
    /// rebound right back (each pass burns a charge, so it terminates);
    /// physical hits, heals, and DoT pulses are never reflected.
    fn try_reflect(
        &mut self,
        actor: EntityId,
        target: EntityId,
        skill: &Skill,
        events: &mut Vec<Event>,
    ) -> bool {
        let is_spell = matches!(skill.damage_type, Some(dt) if dt != DamageType::Physical);
        let is_damaging = skill.effects.iter().any(|e| {
            matches!(
                e,
                Effect::Damage(_)
                    | Effect::ExecuteDamage(_)
                    | Effect::Drain(_)
                    | Effect::ChainDamage { .. }
            )
        });
        if !is_spell
            || !is_damaging
            || self.state.entity(actor).team == self.state.entity(target).team
        {
            return false;
        }
        let t = &mut self.state.entities[target.0];
        if !t.is_alive() || t.status(StatusKind::SpellWard).is_none() {
            return false;
        }
        for s in &mut t.statuses {
            if s.kind == StatusKind::SpellWard {
                s.stacks -= 1;
            }
        }
        t.statuses
            .retain(|s| s.kind != StatusKind::SpellWard || s.stacks > 0);
        events.push(Event::Reflected {
            bearer: target,
            attacker: actor,
            dmg_type: skill.damage_type,
        });
        self.apply_effects_to(target, actor, skill, events);
        true
    }

    /// Resolve one landed hit: enrage and a covering `MightAura` scale it up,
    /// weakness multiplies it, a shield on the target soaks half, an `Exposed`
    /// target takes [`EXPOSED_DAMAGE_BONUS`] extra. `source` is the attacking
    /// entity (None for DoT pulses, which have no attacker at pulse time and
    /// thus no attacker-side scaling — and no `Lifeleech` payback). Returns
    /// the damage actually dealt.
    fn apply_damage(
        &mut self,
        source: Option<EntityId>,
        target: EntityId,
        base: f32,
        dmg_type: Option<DamageType>,
        events: &mut Vec<Event>,
    ) -> f32 {
        let enraged = source
            .is_some_and(|s| self.state.entity(s).status(StatusKind::Enrage).is_some());
        let empowered = source.is_some_and(|s| self.covered_by_aura(s, StatusKind::MightAura));
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return 0.0;
        }
        let weak = matches!(dmg_type, Some(dt) if e.weaknesses.contains(&dt));
        let shielded = e.status(StatusKind::Shield).is_some();
        let exposed = e.status(StatusKind::Exposed).is_some();
        let leeched = e.status(StatusKind::Lifeleech).is_some();
        let target_team = e.team;
        let amount = base
            * if enraged { 1.0 + ENRAGE_BONUS } else { 1.0 }
            * if empowered { 1.0 + AURA_MIGHT_BONUS } else { 1.0 }
            * if weak { WEAKNESS_MULT } else { 1.0 }
            * if shielded { 1.0 - SHIELD_REDUCTION } else { 1.0 }
            * if exposed { 1.0 + EXPOSED_DAMAGE_BONUS } else { 1.0 };
        e.hp = (e.hp - amount).max(0.0);
        let died = !e.is_alive();
        events.push(Event::Damage {
            target,
            amount,
            weakness: weak,
            dmg_type,
        });
        if died {
            events.push(Event::Died(target));
            // A kill refreshes the killer's stealth (see
            // [`reset_stealth_cooldowns`]). DoT deaths have no killer at pulse
            // time (`source` is None), so a poison bleed-out refreshes nothing.
            if let Some(killer) = source {
                self.reset_stealth_cooldowns(killer);
            }
        }
        // A leech mark pays the attacker back on every landed hit — the
        // killing blow included, and through the attacker's own MortalWound
        // like any other heal.
        if leeched {
            if let Some(attacker) = source {
                if self.state.entity(attacker).team != target_team {
                    self.apply_heal(attacker, LEECH_HEAL_ON_HIT, events);
                }
            }
        }
        amount
    }

    /// The assassin-genre kill reset: every skill the killer knows that grants
    /// [`StatusKind::Sneak`] comes off cooldown immediately. Keyed on the
    /// *skill's effect*, never on an entity type â€” any unit equipping a
    /// stealth skill gets the reset (like `is_aura`, a rule of the status
    /// itself).
    fn reset_stealth_cooldowns(&mut self, killer: EntityId) {
        let refreshed: Vec<SkillId> = self.state.entities[killer.0]
            .skills
            .iter()
            .copied()
            .filter(|&sid| {
                self.state.skill(sid).effects.iter().any(|e| {
                    matches!(
                        e,
                        Effect::Inflict { kind: StatusKind::Sneak, .. }
                    )
                })
            })
            .collect();
        for sid in refreshed {
            self.state.entities[killer.0].cooldowns.remove(&sid);
        }
    }

    /// Whether `id` sits inside a living teammate's aura of the given kind â€”
    /// distance measured at *this* instant (bearer included: its own aura
    /// always covers it). Multiple bearers don't stack; coverage is coverage.
    fn covered_by_aura(&self, id: EntityId, kind: StatusKind) -> bool {
        let me = self.state.entity(id);
        self.state.entities.iter().any(|a| {
            a.is_alive()
                && a.team == me.team
                && a.status(kind).is_some()
                && a.pos.dist(me.pos) <= AURA_RADIUS
        })
    }

    /// Strip every harmful status off the target; emits `Cleansed` only if
    /// something actually came off.
    fn apply_cleanse(&mut self, target: EntityId, events: &mut Vec<Event>) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        let before = e.statuses.len();
        e.statuses.retain(|s| !s.kind.is_harmful());
        if e.statuses.len() < before {
            events.push(Event::Cleansed { target });
        }
    }

    fn apply_heal(&mut self, target: EntityId, amount: f32, events: &mut Vec<Event>) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        // A grievous wound cuts what actually arrives; the event reports the
        // reduced amount (what the bar really gained).
        let amount = if e.status(StatusKind::MortalWound).is_some() {
            amount * (1.0 - WOUND_HEAL_REDUCTION)
        } else {
            amount
        };
        e.hp = (e.hp + amount).min(e.max_hp);
        events.push(Event::Heal { target, amount });
    }

    fn apply_status(
        &mut self,
        target: EntityId,
        kind: StatusKind,
        stacks: u32,
        duration: u32,
        events: &mut Vec<Event>,
    ) {
        let e = &mut self.state.entities[target.0];
        if !e.is_alive() {
            return;
        }
        // One aura at a time: singing a new chant ends whatever the entity was
        // already projecting.
        if kind.is_aura() {
            e.statuses.retain(|s| s.kind == kind || !s.kind.is_aura());
        }
        // Stack onto an existing status of the same kind, refreshing duration â€”
        // except auras, which refresh without stacking (re-singing the same
        // chant sustains the field, it doesn't intensify it).
        if let Some(s) = e.statuses.iter_mut().find(|s| s.kind == kind) {
            if !kind.is_aura() {
                s.stacks += stacks;
            }
            s.duration = s.duration.max(duration);
        } else {
            e.statuses.push(Status {
                kind,
                stacks,
                duration,
            });
        }
        events.push(Event::Inflicted {
            target,
            kind,
            stacks,
        });
    }

    // --- per-tick upkeep -------------------------------------------------

    fn tick_statuses(&mut self, events: &mut Vec<Event>) {
        for i in 0..self.state.entities.len() {
            if !self.state.entities[i].is_alive() {
                continue;
            }
            let (mut dmg, mut heal) = (0.0f32, 0.0f32);
            for s in &self.state.entities[i].statuses {
                match s.kind {
                    StatusKind::Poison => dmg += POISON_PER_STACK * s.stacks as f32,
                    StatusKind::Burn => dmg += BURN_PER_STACK * s.stacks as f32,
                    StatusKind::Regen => heal += REGEN_PER_STACK * s.stacks as f32,
                    _ => {}
                }
            }
            let id = self.state.entities[i].id;
            if dmg > 0.0 {
                self.apply_damage(None, id, dmg, None, events);
            }
            if heal > 0.0 && self.state.entities[i].is_alive() {
                self.apply_heal(id, heal, events);
            }

            // Age statuses and drop the expired ones.
            let e = &mut self.state.entities[i];
            for s in &mut e.statuses {
                s.duration = s.duration.saturating_sub(1);
            }
            e.statuses.retain(|s| s.duration > 0);
        }
    }

    fn tick_cooldowns(&mut self) {
        for e in &mut self.state.entities {
            e.cooldowns.retain(|_, remaining| {
                *remaining = remaining.saturating_sub(1);
                *remaining > 0
            });
        }
    }

    /// Regenerate MP: every alive entity recovers its `mp_regen` per tick, capped
    /// at `max_mp`. This is what keeps a costed skill (e.g. a healer's mend) from
    /// permanently drying up â€” and the hook future MP-drain / regen-aura effects
    /// will push against. Casting units regen too (a cast doesn't stop the clock).
    fn tick_mp(&mut self, dt: f32) {
        for e in &mut self.state.entities {
            if e.is_alive() && e.mp_regen != 0.0 {
                e.mp = (e.mp + e.mp_regen * dt).clamp(0.0, e.max_mp);
            }
        }
    }

    /// End the battle once one whole team is down. Returns true if over.
    fn check_over(&mut self, events: &mut Vec<Event>) -> bool {
        if self.over {
            return true;
        }
        let players = self
            .state
            .entities
            .iter()
            .any(|e| e.is_alive() && e.team == Team::Player);
        let enemies = self
            .state
            .entities
            .iter()
            .any(|e| e.is_alive() && e.team == Team::Enemy);
        if players && enemies {
            return false;
        }
        self.over = true;
        let winner = if players { Team::Player } else { Team::Enemy };
        events.push(Event::Victory(winner));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle::*;
    use crate::gambit::*;

    /// Fluent scenario builder shared by the combat tests.
    struct Arena {
        state: BattleState,
        gambits: HashMap<EntityId, Node>,
        move_gambits: HashMap<EntityId, MoveGambit>,
    }

    impl Arena {
        fn new() -> Self {
            Arena {
                state: BattleState {
                    entities: Vec::new(),
                    skills: Vec::new(),
                    bounds: (100.0, 100.0),
                    terrain: None,
                },
                gambits: HashMap::new(),
                move_gambits: HashMap::new(),
            }
        }

        fn skill(&mut self, s: Skill) -> SkillId {
            let id = SkillId(self.state.skills.len());
            self.state.skills.push(s);
            id
        }

        fn add(&mut self, name: &str, team: Team, hp: f32, speed: f32) -> EntityId {
            self.add_at(name, team, hp, speed, 0.0, 0.0)
        }

        fn add_at(
            &mut self,
            name: &str,
            team: Team,
            hp: f32,
            speed: f32,
            x: f32,
            move_speed: f32,
        ) -> EntityId {
            let id = EntityId(self.state.entities.len());
            self.state.entities.push(Entity {
                id,
                name: name.into(),
                team,
                hp,
                max_hp: 100.0,
                mp: 100.0,
                max_mp: 100.0,
                mp_regen: 0.0, // off by default so MP-cost assertions stay exact
                pos: Pos { x, y: 0.0 },
                statuses: Vec::new(),
                weaknesses: Vec::new(),
                skills: Vec::new(),
                cooldowns: HashMap::new(),
                atb_speed: speed,
                move_speed,
                action_bar: 0.0,
            });
            id
        }

        fn ent(&mut self, id: EntityId) -> &mut Entity {
            &mut self.state.entities[id.0]
        }

        fn gambit(&mut self, id: EntityId, tree: Node) {
            self.gambits.insert(id, tree);
        }

        fn move_gambit(&mut self, id: EntityId, gambit: MoveGambit) {
            self.move_gambits.insert(id, gambit);
        }

        fn into_combat(self) -> Combat {
            Combat::new(self.state, self.gambits).with_movement(self.move_gambits)
        }
    }

    fn damage_skill(name: &str, amount: f32, dt: Option<DamageType>, cooldown: u32) -> Skill {
        Skill {
            name: name.into(),
            cost: 0,
            range: 1000.0,
            cooldown,
            cast_time: 0,
            damage_type: dt,
            effects: vec![Effect::Damage(amount)],
        }
    }

    #[test]
    fn battle_ends_with_a_victor() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 0.5);
        let dummy = a.add("dummy", Team::Enemy, 30.0, 0.0); // never acts
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), hit));

        let log = a.into_combat().run(100);

        assert!(log.contains(&Event::Died(dummy)));
        assert!(log.contains(&Event::Victory(Team::Player)));
    }

    #[test]
    fn weakness_multiplier_applies() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        a.ent(enemy).weaknesses.push(DamageType::Fire);
        let fireball = a.skill(damage_skill("Fireball", 20.0, Some(DamageType::Fire), 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), fireball));

        let log = a.into_combat().tick(); // hero acts once

        let dmg = log
            .iter()
            .find_map(|e| match e {
                Event::Damage { amount, weakness, .. } => Some((*amount, *weakness)),
                _ => None,
            })
            .expect("a damage event");
        assert!(dmg.1, "should be flagged as a weakness hit");
        assert_eq!(dmg.0, 30.0); // 20 * 1.5
    }

    /// Regression for the ogre-parked-in-the-void bug: with a `Crowd` +
    /// `Near(nearest)` blend, a mover amid *spread-out* foes (no crowd
    /// anywhere) commits to the nearest one instead of idling at their empty
    /// centroid with nobody in reach.
    #[test]
    fn crowd_mover_commits_to_a_foe_instead_of_the_empty_middle() {
        let mut a = Arena::new();
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 23.0, 1.0);
        let near_foe = a.add_at("near_foe", Team::Enemy, 100.0, 0.0, 10.0, 0.0);
        let far_a = a.add_at("far_a", Team::Enemy, 100.0, 0.0, 30.0, 0.0);
        let far_b = a.add_at("far_b", Team::Enemy, 100.0, 0.0, 30.0, 0.0);
        a.ent(mover).pos.y = 30.0; // â‰ˆ the trio's centroid
        a.ent(near_foe).pos.y = 30.0;
        a.ent(far_a).pos.y = 12.0;
        a.ent(far_b).pos.y = 48.0;
        a.move_gambit(
            mover,
            MoveGambit::new(vec![
                (Term::Crowd(TargetQuery::new(Pool::Enemies).pick(Pick::All), 5.0), 2.5),
                (
                    Term::Near(
                        TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
                        0.0,
                    ),
                    1.0,
                ),
            ]),
        );
        let mut combat = a.into_combat();

        combat.run(40);

        let m = combat.state.entity(mover).pos;
        let closest = combat.state.entity(near_foe).pos;
        assert!(
            m.dist(closest) < 2.2,
            "should stand beside the nearest foe, not at the empty middle: {m:?}"
        );
    }

    /// The other half of the blend: a *clump* within reach outscores a nearer
    /// loner, so the mover wades into the pack â€” where a 360Â° sweep pays â€”
    /// rather than duelling at the edge.
    #[test]
    fn crowd_mover_prefers_the_clump_over_a_nearer_loner() {
        let mut a = Arena::new();
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 20.0, 1.0);
        let loner = a.add_at("loner", Team::Enemy, 100.0, 0.0, 16.0, 0.0);
        let c1 = a.add_at("c1", Team::Enemy, 100.0, 0.0, 26.0, 0.0);
        let c2 = a.add_at("c2", Team::Enemy, 100.0, 0.0, 27.0, 0.0);
        let c3 = a.add_at("c3", Team::Enemy, 100.0, 0.0, 27.0, 0.0);
        for id in [mover, loner, c1] {
            a.ent(id).pos.y = 30.0;
        }
        a.ent(c2).pos.y = 29.0;
        a.ent(c3).pos.y = 31.0;
        a.move_gambit(
            mover,
            MoveGambit::new(vec![
                (Term::Crowd(TargetQuery::new(Pool::Enemies).pick(Pick::All), 5.0), 2.5),
                (
                    Term::Near(
                        TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
                        0.0,
                    ),
                    1.0,
                ),
            ]),
        );
        let mut combat = a.into_combat();

        combat.run(40);

        let m = combat.state.entity(mover).pos;
        assert!(
            m.dist(combat.state.entity(c1).pos) < 2.5,
            "should wade to the clump, got {m:?}"
        );
        assert!(
            m.dist(combat.state.entity(loner).pos) > 5.0,
            "not duel the nearer loner, got {m:?}"
        );
    }

    /// A mover whose path is blocked by a body merely standing in the way
    /// slides around it and carries on â€” no more bodyblock pin. It still
    /// stops at contact with its actual quarry.
    #[test]
    fn mover_slides_around_a_body_in_the_way() {
        let mut a = Arena::new();
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 0.0, 1.0);
        let bystander = a.add_at("bystander", Team::Player, 100.0, 0.0, 3.0, 0.0);
        let quarry = a.add_at("quarry", Team::Enemy, 100.0, 0.0, 10.0, 0.0);
        a.ent(mover).pos.y = 5.0;
        a.ent(bystander).pos.y = 5.0; // dead on the line to the quarry
        a.ent(quarry).pos.y = 5.0;
        a.move_gambit(
            mover,
            MoveGambit::toward(TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)),
        );
        let mut combat = a.into_combat();

        combat.run(30);

        let m = combat.state.entity(mover).pos;
        assert!(
            m.x > 4.0,
            "the mover should round the bystander instead of pinning on it, got {m:?}"
        );
        // It carries on to the quarry and settles beside it (the lattice
        // argmax can park a fraction shy of exact contact), never inside it.
        let sep = m.dist(combat.state.entity(quarry).pos);
        let contact = 2.0 * ENTITY_RADIUS;
        assert!(
            (contact - 1e-3..2.0).contains(&sep),
            "should end beside its quarry, got {sep}"
        );
    }

    /// A grievous wound halves incoming healing while it lasts, and healing
    /// returns to full strength once it expires.
    #[test]
    fn mortal_wound_halves_incoming_healing() {
        let mut a = Arena::new();
        let healer = a.add("healer", Team::Player, 100.0, 1.0);
        let patient = a.add("patient", Team::Player, 100.0, 0.0);
        let _foe = a.add("foe", Team::Enemy, 100.0, 0.0); // keeps the battle live
        a.ent(patient).hp = 20.0;
        a.ent(patient).statuses.push(Status {
            kind: StatusKind::MortalWound,
            stacks: 1,
            duration: 2,
        });
        let mend = a.skill(Skill {
            name: "Mend".into(),
            cost: 0,
            range: 100.0,
            cooldown: 2,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(40.0)],
        });
        a.gambit(
            healer,
            Node::act(
                TargetQuery::new(Pool::Allies).filter(Filter::HpPctBelow(0.9)),
                mend,
            ),
        );
        let mut combat = a.into_combat();

        let log = combat.tick(); // heal lands through the wound
        let healed = log.iter().find_map(|e| match e {
            Event::Heal { amount, .. } => Some(*amount),
            _ => None,
        });
        assert_eq!(healed, Some(20.0), "the wound halves the mend");
        assert_eq!(combat.state.entity(patient).hp, 40.0);

        combat.tick(); // wound expires; cooldown runs
        combat.tick(); // second mend at full strength
        assert_eq!(combat.state.entity(patient).hp, 80.0, "20 + 20 halved + 40 full");
    }

    /// A kill refreshes stealth: the killer's Sneak-granting skill comes off
    /// cooldown the moment its hit fells an enemy.
    #[test]
    fn a_kill_resets_the_killers_sneak_cooldown() {
        let mut a = Arena::new();
        let rogue = a.add("rogue", Team::Player, 100.0, 1.0);
        let victim = a.add("victim", Team::Enemy, 10.0, 0.0);
        let vanish = a.skill(Skill {
            name: "Vanish".into(),
            cost: 0,
            range: 100.0,
            cooldown: 80,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict {
                kind: StatusKind::Sneak,
                stacks: 1,
                duration: 20,
            }],
        });
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.ent(rogue).skills = vec![vanish, hit];
        a.ent(rogue).cooldowns.insert(vanish, 50); // deep mid-cooldown
        a.gambit(rogue, Node::act(TargetQuery::new(Pool::Enemies), hit));
        let mut combat = a.into_combat();

        let log = combat.tick(); // the hit kills the 10-HP victim

        assert!(log.contains(&Event::Died(victim)));
        assert_eq!(
            combat.state.entity(rogue).cooldown_remaining(vanish),
            0,
            "the kill should hand Sneak straight back"
        );
    }

    /// A 360Â° point-blank AoE (`Pick::All` + short range): every foe inside
    /// the reach takes the hit in the same action; one outside is untouched.
    #[test]
    fn aoe_hits_everyone_in_reach_at_once() {
        let mut a = Arena::new();
        let ogre = a.add("ogre", Team::Player, 100.0, 1.0);
        let near_east = a.add_at("near_east", Team::Enemy, 100.0, 0.0, 2.0, 0.0);
        let near_west = a.add_at("near_west", Team::Enemy, 100.0, 0.0, 1.0, 0.0);
        let far = a.add_at("far", Team::Enemy, 100.0, 0.0, 10.0, 0.0);
        let rend = a.skill(Skill {
            name: "Rend".into(),
            cost: 0,
            range: 3.0,
            cooldown: 40,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Damage(14.0)],
        });
        a.gambit(ogre, Node::act(TargetQuery::new(Pool::Enemies).pick(Pick::All), rend));
        let mut combat = a.into_combat();

        let log = combat.tick();

        assert_eq!(combat.state.entity(near_east).hp, 86.0);
        assert_eq!(combat.state.entity(near_west).hp, 86.0);
        assert_eq!(combat.state.entity(far).hp, 100.0, "out of the sweep's reach");
        let both_at_once = log.iter().any(|e| matches!(
            e,
            Event::Acted { targets, .. }
                if targets.contains(&near_east) && targets.contains(&near_west)
        ));
        assert!(both_at_once, "one action, every foe in reach");
    }

    /// A sneaking entity doesn't exist to hostile targeting: an enemy with a
    /// ready attack simply waits (nothing visible to hit), and takes nothing.
    #[test]
    fn sneak_hides_from_hostile_targeting() {
        let mut a = Arena::new();
        let brute = a.add("brute", Team::Player, 100.0, 1.0);
        let rogue = a.add("rogue", Team::Enemy, 100.0, 0.0);
        a.ent(rogue).statuses.push(Status {
            kind: StatusKind::Sneak,
            stacks: 1,
            duration: 10,
        });
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(brute, Node::act(TargetQuery::new(Pool::Enemies), hit));
        let mut combat = a.into_combat();

        let log = combat.tick();

        assert!(log.contains(&Event::Waited(brute)), "nothing visible to hit");
        assert_eq!(combat.state.entity(rogue).hp, 100.0);
    }

    /// Acting breaks stealth: a sneaking attacker can still strike, but the
    /// hit strips its own Sneak.
    #[test]
    fn acting_breaks_sneak() {
        let mut a = Arena::new();
        let rogue = a.add("rogue", Team::Player, 100.0, 1.0);
        let dummy = a.add("dummy", Team::Enemy, 100.0, 0.0);
        a.ent(rogue).statuses.push(Status {
            kind: StatusKind::Sneak,
            stacks: 1,
            duration: 10,
        });
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(rogue, Node::act(TargetQuery::new(Pool::Enemies), hit));
        let mut combat = a.into_combat();

        combat.tick();

        assert_eq!(combat.state.entity(dummy).hp, 80.0, "the sneaker can still strike");
        assert!(
            combat.state.entity(rogue).status(StatusKind::Sneak).is_none(),
            "attacking reveals the sneaker"
        );
    }

    /// Invisibility, not invulnerability: sneaking does not shake a projectile
    /// that is already in the air â€” it still homes in and lands.
    #[test]
    fn sneak_does_not_dodge_a_projectile_in_the_air() {
        let mut a = Arena::new();
        let archer = a.add("archer", Team::Player, 100.0, 1.0);
        // Far enough that the shot flies instead of landing instantly.
        let rogue = a.add_at("rogue", Team::Enemy, 100.0, 0.0, 30.0, 0.0);
        let shot = a.skill(damage_skill("Shot", 20.0, None, 0));
        a.gambit(archer, Node::act(TargetQuery::new(Pool::Enemies), shot));
        let mut combat = a.into_combat();

        combat.tick(); // fires: a projectile is now in the air
        assert_eq!(combat.flights().len(), 1);
        combat.state.entities[rogue.0].statuses.push(Status {
            kind: StatusKind::Sneak,
            stacks: 1,
            duration: 20,
        });
        combat.run(5); // plenty of time for the shot to arrive

        assert_eq!(
            combat.state.entity(rogue).hp,
            80.0,
            "exactly the one in-flight shot lands; no new shots while hidden"
        );
    }

    /// Sneaking mid-cast dodges the committed nuke: the resolving cast finds
    /// its mark vanished and fizzles â€” unlike a projectile, nothing has
    /// launched yet.
    #[test]
    fn sneaking_mid_cast_fizzles_the_committed_nuke() {
        let mut a = Arena::new();
        let caster = a.add("caster", Team::Player, 100.0, 1.0);
        let rogue = a.add("rogue", Team::Enemy, 100.0, 0.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 99,
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(30.0)],
        });
        a.gambit(caster, Node::act(TargetQuery::new(Pool::Enemies), nuke));
        let mut combat = a.into_combat();

        combat.tick(); // cast begins, committed at the rogue
        assert!(combat.is_casting(caster));
        combat.state.entities[rogue.0].statuses.push(Status {
            kind: StatusKind::Sneak,
            stacks: 1,
            duration: 10,
        });
        combat.tick();
        let log = combat.tick(); // cast completes â€” into thin air

        assert!(
            log.iter().any(|e| matches!(e, Event::Fizzled { actor, .. } if *actor == caster)),
            "the cast should fizzle on a vanished mark"
        );
        assert_eq!(combat.state.entity(rogue).hp, 100.0);
    }

    /// A spell ward eats the next hostile damage spell and hurls it back: the
    /// bearer takes nothing, the caster takes its own hit, the charge is
    /// consumed â€” and the next spell lands normally.
    #[test]
    fn spell_ward_reflects_one_damage_spell() {
        let mut a = Arena::new();
        let caster = a.add("caster", Team::Enemy, 100.0, 1.0); // acts every tick
        let rogue = a.add("rogue", Team::Player, 100.0, 0.0);
        a.ent(rogue).statuses.push(Status {
            kind: StatusKind::SpellWard,
            stacks: 1,
            duration: 10,
        });
        let bolt = a.skill(damage_skill("Bolt", 20.0, Some(DamageType::Fire), 0));
        a.gambit(caster, Node::act(TargetQuery::new(Pool::Enemies), bolt));
        let mut combat = a.into_combat();

        let log = combat.tick();

        assert_eq!(combat.state.entity(rogue).hp, 100.0, "the ward ate the spell");
        assert_eq!(combat.state.entity(caster).hp, 80.0, "the spell rebounded");
        assert!(log.iter().any(|e| matches!(e, Event::Reflected { .. })));
        assert!(
            combat.state.entity(rogue).status(StatusKind::SpellWard).is_none(),
            "the charge is consumed"
        );

        combat.tick(); // second bolt: no ward left
        assert_eq!(combat.state.entity(rogue).hp, 80.0, "one charge reflects one spell");
    }

    /// The ward is a *spell* counter: physical hits pass straight through it,
    /// leaving the charge intact.
    #[test]
    fn spell_ward_ignores_physical_hits() {
        let mut a = Arena::new();
        let archer = a.add("archer", Team::Enemy, 100.0, 1.0);
        let rogue = a.add("rogue", Team::Player, 100.0, 0.0);
        a.ent(rogue).statuses.push(Status {
            kind: StatusKind::SpellWard,
            stacks: 1,
            duration: 10,
        });
        let shot = a.skill(damage_skill("Shot", 20.0, Some(DamageType::Physical), 0));
        a.gambit(archer, Node::act(TargetQuery::new(Pool::Enemies), shot));
        let mut combat = a.into_combat();

        let log = combat.tick();

        assert_eq!(combat.state.entity(rogue).hp, 80.0, "an arrow is not a spell");
        assert_eq!(combat.state.entity(archer).hp, 100.0);
        assert!(log.iter().all(|e| !matches!(e, Event::Reflected { .. })));
        assert!(
            combat.state.entity(rogue).status(StatusKind::SpellWard).is_some(),
            "the charge is still there"
        );
    }

    /// Chain damage arcs from the primary target to nearby foes with falloff:
    /// each jump strikes the nearest unstruck enemy within jump range of the
    /// last victim, and the arc stops at the jump cap.
    #[test]
    fn chain_damage_arcs_with_falloff() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let first = a.add_at("first", Team::Enemy, 100.0, 0.0, 2.0, 0.0);
        let second = a.add_at("second", Team::Enemy, 100.0, 0.0, 4.0, 0.0);
        let third = a.add_at("third", Team::Enemy, 100.0, 0.0, 6.0, 0.0);
        let fourth = a.add_at("fourth", Team::Enemy, 100.0, 0.0, 8.0, 0.0);
        let bolt = a.skill(Skill {
            name: "Chain Bolt".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 99,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::ChainDamage {
                base: 20.0,
                jumps: 2,
                falloff: 0.5,
                jump_range: 3.0,
            }],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), bolt));

        let mut combat = a.into_combat();
        let log = combat.tick();

        let arcs = log
            .iter()
            .filter(|e| matches!(e, Event::Chained { .. }))
            .count();
        assert_eq!(arcs, 2, "two jumps after the primary hit");
        let dmg: Vec<(EntityId, f32)> = log
            .iter()
            .filter_map(|e| match e {
                Event::Damage { target, amount, .. } => Some((*target, *amount)),
                _ => None,
            })
            .collect();
        assert_eq!(
            dmg,
            vec![(first, 20.0), (second, 10.0), (third, 5.0)],
            "full base on the primary, then falloff per arc"
        );
        assert_eq!(
            combat.state.entity(fourth).hp,
            100.0,
            "the fourth foe is past the jump cap"
        );
    }

    /// A chain arc never leaps farther than its jump range, never strikes the
    /// actor's own team, and a lone primary target just takes a single hit.
    #[test]
    fn chain_damage_respects_jump_range_and_teams() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let ally = a.add_at("ally", Team::Player, 100.0, 0.0, 3.0, 0.0);
        let near = a.add_at("near", Team::Enemy, 100.0, 0.0, 2.0, 0.0);
        let far = a.add_at("far", Team::Enemy, 100.0, 0.0, 20.0, 0.0);
        let bolt = a.skill(Skill {
            name: "Chain Bolt".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 99,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::ChainDamage {
                base: 20.0,
                jumps: 3,
                falloff: 0.5,
                jump_range: 5.0,
            }],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), bolt));

        let mut combat = a.into_combat();
        let log = combat.tick();

        assert_eq!(combat.state.entity(near).hp, 80.0);
        assert_eq!(
            combat.state.entity(far).hp,
            100.0,
            "an arc can't leap 18 units"
        );
        assert_eq!(
            combat.state.entity(ally).hp,
            100.0,
            "an ally inside jump range is never a chain victim"
        );
        assert!(
            log.iter().all(|e| !matches!(e, Event::Chained { .. })),
            "no valid jump, no arc events"
        );
    }

    #[test]
    fn cooldown_forces_fallback_in_the_loop() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0); // acts every tick
        let _enemy = a.add("enemy", Team::Enemy, 500.0, 0.0); // soaks hits
        let strong = a.skill(damage_skill("Strong", 50.0, None, 5)); // 5-tick cooldown
        let weak = a.skill(damage_skill("Weak", 5.0, None, 0));

        // Prefer Strong; fall back to Weak while it recharges.
        a.gambit(
            hero,
            Node::context(
                Condition::Always,
                GroupMode::Fallthrough,
                vec![
                    Node::act(TargetQuery::new(Pool::Enemies), strong),
                    Node::act(TargetQuery::new(Pool::Enemies), weak),
                ],
            ),
        );

        let log = a.into_combat().run(3);
        let skills_used: Vec<SkillId> = log
            .iter()
            .filter_map(|e| match e {
                Event::Acted { skill, .. } => Some(*skill),
                _ => None,
            })
            .collect();

        // Tick 1 Strong, then Strong is on cooldown so ticks 2 & 3 use Weak.
        assert_eq!(skills_used, vec![strong, weak, weak]);
    }

    #[test]
    fn damage_over_time_ticks_down_and_expires() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        // One-shot: inflict 2 stacks of poison for 3 ticks, then never again.
        let venom = a.skill(Skill {
            name: "Venom".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 99,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict {
                kind: StatusKind::Poison,
                stacks: 2,
                duration: 3,
            }],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), venom));

        let mut combat = a.into_combat();
        let log = combat.run(10);

        // 2 stacks * 3.0 per stack * 3 ticks = 18 total poison damage.
        let poison_dmg: f32 = log
            .iter()
            .filter_map(|e| match e {
                Event::Damage { target, amount, .. } if *target == enemy => Some(*amount),
                _ => None,
            })
            .sum();
        assert_eq!(poison_dmg, 18.0);
        assert_eq!(combat.state.entity(enemy).hp, 82.0);
        // Status expired, not lingering.
        assert!(combat.state.entity(enemy).statuses.is_empty());
    }

    /// A unit with a movement gambit but too short a range drifts into melee
    /// over several ticks and only then lands a hit â€” movement and the action
    /// bar advance concurrently, never one instead of the other.
    #[test]
    fn movement_closes_into_melee_range() {
        let mut a = Arena::new();
        //             name    team          hp     atb   x     move
        let hero = a.add_at("hero", Team::Player, 100.0, 1.0, 0.0, 1.0);
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 5.0, 0.0);
        let jab = a.skill(Skill {
            name: "Jab".into(),
            cost: 0,
            range: 2.0, // can't reach the enemy 5 units away at the start
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Damage(10.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), jab));
        a.move_gambit(
            hero,
            MoveGambit::toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ),
        );

        let mut combat = a.into_combat();

        // First couple of ticks: out of range, so the hero waits but still moves.
        combat.tick();
        assert!(combat.state.entity(hero).pos.x > 0.0, "hero should have moved");
        assert_eq!(combat.state.entity(enemy).hp, 100.0, "no hit while out of range");

        // Given enough ticks it closes the gap and starts landing hits.
        combat.run(10);
        assert!(
            combat.state.entity(enemy).hp < 100.0,
            "hero should have closed in and hit"
        );
    }

    /// Two entities can't occupy the same space: a mover charging a stationary
    /// blocker stops when their circles touch, never on top of it.
    #[test]
    fn movement_stops_at_hitbox_contact() {
        let mut a = Arena::new();
        //             name       team          hp    atb  x    move
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 0.0, 10.0);
        let blocker = a.add_at("blocker", Team::Enemy, 100.0, 0.0, 5.0, 0.0);
        // Put both on the same interior row so the geometry is purely along x
        // (away from the y-edge, which would otherwise lift a y=0 centre).
        a.ent(mover).pos.y = 5.0;
        a.ent(blocker).pos.y = 5.0;
        a.move_gambit(
            mover,
            MoveGambit::toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ),
        );

        let mut combat = a.into_combat();
        combat.run(20); // plenty of ticks to try (and fail) to overlap

        let m = combat.state.entity(mover).pos;
        let b = combat.state.entity(blocker).pos;
        let sep = m.dist(b);
        // Stops just-touching (sum of the two equal radii), on the near side.
        let contact = 2.0 * ENTITY_RADIUS;
        assert!(
            (sep - contact).abs() < 1e-3,
            "expected contact at {contact}, got {sep}"
        );
        assert!(m.x < b.x, "mover should stop before the blocker, not pass it");
    }

    /// A unit fleeing to the arena edge keeps its whole body in â€” its centre
    /// stops a radius short of the wall, never past it.
    #[test]
    fn movement_keeps_body_inside_bounds() {
        let mut a = Arena::new();
        a.state.bounds = (10.0, 10.0);
        let runner = a.add_at("runner", Team::Player, 100.0, 0.0, 5.0, 5.0);
        let chaser = a.add_at("chaser", Team::Enemy, 100.0, 0.0, 9.0, 0.0);
        a.ent(runner).pos.y = 5.0; // centre of the arena, chaser to the east
        a.ent(chaser).pos.y = 5.0;
        a.move_gambit(
            runner,
            MoveGambit::new(vec![(
                Term::AwayFrom(TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)),
                1.0,
            )]),
        );

        let mut combat = a.into_combat();
        combat.run(20);

        // Fled west into the wall region: the radius keeps the whole body in â€”
        // both coordinates sit at least a radius from every edge.
        let p = combat.state.entity(runner).pos;
        assert!(p.x < 5.0, "runner should have fled away from the chaser, x = {}", p.x);
        for (c, hi) in [(p.x, 10.0), (p.y, 10.0)] {
            assert!(
                (ENTITY_RADIUS - 1e-3..=hi - ENTITY_RADIUS + 1e-3).contains(&c),
                "body must stay inside bounds, got {p:?}"
            );
        }
        // And the flight actually opened the gap.
        let gap = p.dist(combat.state.entity(chaser).pos);
        assert!(gap > 4.0, "flee should open the gap, got {gap}");
    }

    /// A cast-time skill roots the caster, freezes its ATB, pays its cost once
    /// up front, and resolves only after `cast_time` ticks.
    #[test]
    fn cast_time_roots_then_resolves() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0); // ready every tick
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 10,
            range: 1000.0,
            cooldown: 5, // can't immediately recast after it lands
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(30.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), nuke));

        let mut combat = a.into_combat();

        combat.tick(); // tick 1: cast begins
        assert!(combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 100.0);
        assert_eq!(combat.state.entity(hero).mp, 90.0); // paid at cast start

        combat.tick(); // tick 2: still casting
        assert!(combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 100.0);

        combat.tick(); // tick 3: resolves
        assert!(!combat.is_casting(hero));
        assert_eq!(combat.state.entity(enemy).hp, 70.0);
        assert_eq!(combat.state.entity(hero).mp, 90.0); // not charged twice
    }

    /// Casting roots a unit that would otherwise drift: no movement from the
    /// tick after the cast begins through â€” and including â€” the tick it
    /// resolves on. (The drift on the start tick itself is fine: it happened
    /// before the actor's bar fired.)
    #[test]
    fn casting_suppresses_drift_until_resolution() {
        let mut a = Arena::new();
        let caster = a.add_at("caster", Team::Player, 100.0, 1.0, 0.0, 1.0);
        let _enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 20.0, 0.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 5, // so resolving doesn't chain straight into a recast
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(10.0)],
        });
        a.gambit(caster, Node::act(TargetQuery::new(Pool::Enemies), nuke));
        a.move_gambit(
            caster,
            MoveGambit::toward(TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc)),
        );
        let mut combat = a.into_combat();

        combat.tick(); // drifts (not yet casting), then begins the cast
        assert!(combat.is_casting(caster));
        let rooted_at = combat.state.entity(caster).pos;

        combat.tick(); // mid-cast: frozen
        assert!(combat.is_casting(caster));
        assert_eq!(combat.state.entity(caster).pos, rooted_at);

        combat.tick(); // resolves this tick â€” still no drift
        assert!(!combat.is_casting(caster));
        assert_eq!(combat.flights().len(), 1, "the nuke is on its way to the far target");
        assert_eq!(
            combat.state.entity(caster).pos,
            rooted_at,
            "no free move on the resolution tick"
        );

        combat.tick(); // cast done: drifting resumes
        assert_ne!(combat.state.entity(caster).pos, rooted_at);
    }

    /// If every committed target becomes invalid mid-cast (here: killed by an
    /// ally), the cast fizzles instead of resolving â€” the interrupt/counterplay.
    #[test]
    fn cast_fizzles_when_target_dies_midcast() {
        let mut a = Arena::new();
        let caster = a.add("caster", Team::Player, 100.0, 1.0);
        // The nuke's victim: lowest-HP enemy, killed mid-cast. A second, healthy
        // enemy keeps the battle going past its death so the cast can resolve.
        let victim = a.add("victim", Team::Enemy, 25.0, 0.0);
        let _bystander = a.add("bystander", Team::Enemy, 100.0, 0.0);
        let ally = a.add("ally", Team::Player, 100.0, 1.0);
        let nuke = a.skill(Skill {
            name: "Nuke".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 2,
            damage_type: None,
            effects: vec![Effect::Damage(50.0)],
        });
        let jab = a.skill(damage_skill("Jab", 20.0, None, 0));
        // Both caster and ally focus the lowest-HP enemy (the victim).
        let focus_weakest =
            || TargetQuery::new(Pool::Enemies).sort(SortKey::Hp, Order::Asc);
        a.gambit(caster, Node::act(focus_weakest(), nuke));
        a.gambit(ally, Node::act(focus_weakest(), jab));

        let mut combat = a.into_combat();
        let log = combat.run(5);

        // The ally's two 20-damage jabs (tick 1 and 2) kill the 25-HP victim
        // before the caster's tick-3 resolution, so the nuke fizzles on it.
        assert!(log.contains(&Event::Died(victim)));
        assert!(log.iter().any(|e| matches!(
            e,
            Event::Fizzled { actor, skill } if *actor == caster && *skill == nuke
        )));
        // The dead victim never absorbed the nuke's 50 damage â€” only the 20s.
        assert!(!log.iter().any(|e| matches!(
            e,
            Event::Damage { target, amount, .. } if *target == victim && *amount == 50.0
        )));
    }

    /// A mover routes *around* an impassable wall (down through a gap and back
    /// up) to reach a target it couldn't walk to in a straight line â€” the payoff
    /// of A\* over pure steering, which would jam into the wall and stop.
    #[test]
    fn pathfinding_routes_around_a_wall() {
        use crate::terrain::{Terrain, Tile3};

        let mut a = Arena::new();
        //              name    team          hp    atb  x    move
        let mover = a.add_at("mover", Team::Player, 100.0, 0.0, 0.5, 0.5);
        let target = a.add_at("target", Team::Enemy, 100.0, 0.0, 5.5, 0.0);
        // Put them on row 0 (behind the wall) with the only gap on row 2.
        a.ent(mover).pos.y = 0.5;
        a.ent(target).pos.y = 0.5;

        // 6Ã—3 grid; wall at column 3 across rows 0..=1, leaving row 2 open.
        let mut terrain = Terrain::flat(6, 3, 1.0);
        for r in 0..=1 {
            terrain.set(3, r, Tile3 { elevation: 4, passable: false });
        }
        a.state.bounds = terrain.world_extent();
        a.state.terrain = Some(terrain);

        a.move_gambit(
            mover,
            MoveGambit::toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ),
        );

        let mut combat = a.into_combat();
        combat.run(100); // no attacks: just drives movement to convergence

        let m = combat.state.entity(mover).pos;
        let tpos = combat.state.entity(target).pos;
        assert!(m.x > 3.0, "mover should have crossed to the far side, x = {}", m.x);
        // Arrived at the target's hitbox on the far side, never through the wall.
        let sep = m.dist(tpos);
        assert!(sep < 1.3, "mover should have reached the target, separation = {sep}");
    }

    #[test]
    fn gambit_heals_self_when_hurt() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 25.0, 1.0); // below 30%
        let _enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        let heal = a.skill(Skill {
            name: "Heal".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(50.0)],
        });
        a.gambit(
            hero,
            Node::act(TargetQuery::new(Pool::Myself), heal).when(Condition::Exists(
                TargetQuery::new(Pool::Myself).filter(Filter::HpPctBelow(0.3)),
            )),
        );

        let mut combat = a.into_combat();
        let log = combat.tick();

        assert!(log
            .iter()
            .any(|e| matches!(e, Event::Heal { target, .. } if *target == hero)));
        assert_eq!(combat.state.entity(hero).hp, 75.0);
    }

    /// A charge/gap-closer (`Effect::Dash`) is a continuous lunge, not a
    /// teleport: committing starts the run with nothing landed yet, the actor
    /// travels at DASH_SPEED, and the skill's damage and stun land at contact.
    #[test]
    fn charge_dashes_to_contact_deals_damage_and_stuns() {
        let mut a = Arena::new();
        //             name    team          hp    atb  x    move
        let hero = a.add_at("hero", Team::Player, 100.0, 1.0, 0.0, 0.0); // ready, no drift
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 8.0, 0.0);
        a.ent(hero).pos.y = 5.0; // interior row: pure-x geometry, off the y-edge
        a.ent(enemy).pos.y = 5.0;
        let charge = a.skill(Skill {
            name: "Charge".into(),
            cost: 0,
            range: 10.0,
            cooldown: 5, // so landing doesn't chain straight into a re-charge
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![
                Effect::Dash { max: 10.0 },
                Effect::Damage(15.0),
                Effect::Inflict { kind: StatusKind::Stun, stacks: 1, duration: 3 },
            ],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), charge));

        let mut combat = a.into_combat();

        combat.tick(); // commits to the charge: the lunge begins, nothing landed
        assert!(combat.is_dashing(hero));
        assert_eq!(combat.dash_target(hero), Some(enemy));
        assert_eq!(combat.state.entity(enemy).hp, 100.0, "damage waits for contact");
        assert!(!combat.state.entity(enemy).is_stunned());

        combat.tick(); // the 8-unit gap is within one tick of DASH_SPEED: contact
        assert!(!combat.is_dashing(hero));
        let sep = combat.state.entity(hero).pos.dist(combat.state.entity(enemy).pos);
        let contact = 2.0 * ENTITY_RADIUS;
        assert!((sep - contact).abs() < 1e-3, "should stop at contact, sep = {sep}");
        // The hit and the stun both landed â€” at contact, not at commit.
        assert_eq!(combat.state.entity(enemy).hp, 85.0);
        assert!(combat.state.entity(enemy).is_stunned());
    }

    /// The lunge is visibly *in between* on the way: after a partial advance
    /// the dasher stands strictly between its start and its mark, still
    /// committed, with the payload still unlanded.
    #[test]
    fn dash_travels_continuously_not_teleporting() {
        let mut a = Arena::new();
        let hero = a.add_at("hero", Team::Player, 100.0, 1.0, 0.0, 0.0);
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 12.0, 0.0);
        a.ent(hero).pos.y = 5.0;
        a.ent(enemy).pos.y = 5.0;
        let dash = a.skill(Skill {
            name: "Dash".into(),
            cost: 0,
            range: 15.0,
            cooldown: 8,
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Dash { max: 20.0 }, Effect::Damage(10.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), dash));

        let mut combat = a.into_combat();
        combat.tick(); // commit â€” still at the start
        assert_eq!(combat.state.entity(hero).pos.x, 0.0);

        combat.tick(); // one tick of DASH_SPEED: 12-unit gap not yet closed
        let x = combat.state.entity(hero).pos.x;
        assert!(x > 0.0 && x < 12.0, "mid-lunge, got x = {x}");
        assert!(combat.is_dashing(hero));
        assert_eq!(combat.state.entity(enemy).hp, 100.0, "still nothing landed");

        combat.tick(); // remaining gap closes: contact, damage lands
        assert!(!combat.is_dashing(hero));
        assert_eq!(combat.state.entity(enemy).hp, 90.0);
    }

    /// Ranged damage lands when the projectile does, not when it's fired: the
    /// shot spends ticks in the air crossing the arena while the target's HP
    /// holds, then the hit applies on impact.
    #[test]
    fn projectile_damage_lands_on_impact_not_at_fire() {
        let mut a = Arena::new();
        let archer = a.add_at("archer", Team::Player, 100.0, 1.0, 0.0, 0.0);
        let mark = a.add_at("mark", Team::Enemy, 100.0, 0.0, 30.0, 0.0);
        a.ent(archer).pos.y = 5.0;
        a.ent(mark).pos.y = 5.0;
        let bow = a.skill(Skill {
            name: "Longshot".into(),
            cost: 0,
            range: 100.0,
            cooldown: 10, // a single arrow in the air at a time
            cast_time: 0,
            damage_type: Some(DamageType::Physical),
            effects: vec![Effect::Damage(10.0)],
        });
        a.gambit(archer, Node::act(TargetQuery::new(Pool::Enemies), bow));

        let mut combat = a.into_combat();
        let events = combat.tick(); // fires: Acted now, damage later
        assert!(events.iter().any(|e| matches!(e, Event::Acted { .. })));
        assert_eq!(combat.flights().len(), 1);
        assert_eq!(combat.state.entity(mark).hp, 100.0, "arrow still in the air");

        combat.tick(); // 12 of 30 units covered
        combat.tick(); // 24 of 30
        assert_eq!(combat.state.entity(mark).hp, 100.0, "still in the air");
        assert_eq!(combat.flights().len(), 1);

        combat.tick(); // within reach â€” impact
        assert!(combat.flights().is_empty());
        assert_eq!(combat.state.entity(mark).hp, 90.0, "landed");
    }

    /// A point-blank hit (inside MELEE_RANGE) is contact â€” it lands the moment
    /// the actor acts, with no flight involved.
    #[test]
    fn point_blank_hits_land_immediately() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        a.ent(enemy).pos.x = 1.5; // inside MELEE_RANGE
        let strike = a.skill(damage_skill("Strike", 10.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), strike));

        let mut combat = a.into_combat();
        combat.tick();
        assert!(combat.flights().is_empty());
        assert_eq!(combat.state.entity(enemy).hp, 90.0);
    }

    /// A stunned unit can neither act nor move, and its action bar is frozen â€”
    /// until the stun expires, after which it behaves normally again.
    #[test]
    fn stun_freezes_action_and_movement() {
        let mut a = Arena::new();
        let hero = a.add_at("hero", Team::Player, 100.0, 1.0, 0.0, 2.0); // would act + drift
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 5.0, 0.0);
        a.ent(hero).pos.y = 5.0;
        a.ent(enemy).pos.y = 5.0;
        a.ent(hero)
            .statuses
            .push(Status { kind: StatusKind::Stun, stacks: 1, duration: 3 });
        let jab = a.skill(damage_skill("Jab", 20.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), jab));
        a.move_gambit(
            hero,
            MoveGambit::toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ),
        );

        let mut combat = a.into_combat();
        let startx = combat.state.entity(hero).pos.x;
        combat.tick(); // first tick: still stunned

        assert_eq!(combat.state.entity(enemy).hp, 100.0, "stunned unit can't act");
        assert_eq!(combat.state.entity(hero).pos.x, startx, "stunned unit can't move");
        assert_eq!(combat.state.entity(hero).action_bar, 0.0, "stunned bar is frozen");

        // The stun (duration 3) wears off, after which the hero closes and hits.
        combat.run(30);
        assert!(
            combat.state.entity(enemy).hp < 100.0,
            "acts and attacks once the stun wears off"
        );
    }

    /// MP regenerates each tick up to `max_mp`, and a costed skill becomes
    /// feasible again once enough has recovered â€” so a healer that spent itself
    /// dry starts healing again instead of falling through to its plink forever.
    #[test]
    fn mp_regenerates_and_reenables_a_costed_skill() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0); // ready every tick
        let ally = a.add("ally", Team::Player, 10.0, 0.0); // stays hurt: heal has a target
        let _enemy = a.add("enemy", Team::Enemy, 500.0, 0.0); // keeps the battle going
        a.ent(hero).mp = 5.0; // can't afford the heal yet
        a.ent(hero).max_mp = 100.0;
        a.ent(hero).mp_regen = 3.0; // ...but recovers 3/tick
        let heal = a.skill(Skill {
            name: "Heal".into(),
            cost: 10,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(5.0)],
        });
        let plink = a.skill(damage_skill("Plink", 1.0, None, 0));
        // Prefer to heal the hurt ally; fall back to plinking if it can't afford it.
        a.gambit(
            hero,
            Node::context(
                Condition::Always,
                GroupMode::Fallthrough,
                vec![
                    Node::act(TargetQuery::new(Pool::Allies).filter(Filter::HpPctBelow(0.7)), heal),
                    Node::act(TargetQuery::new(Pool::Enemies), plink),
                ],
            ),
        );

        let mut combat = a.into_combat();
        // Tick 1: only 5 MP (+3 regen = 8) â€” still under 10, so it plinks.
        let log = combat.tick();
        assert!(log.iter().any(|e| matches!(e, Event::Acted { skill, .. } if *skill == plink)));
        assert!(combat.state.entity(hero).mp < 10.0);

        // A few ticks later the regen has cleared the cost and the heal fires.
        let log = combat.run(5);
        assert!(
            log.iter().any(|e| matches!(e, Event::Heal { target, .. } if *target == ally)),
            "the healer should heal again once MP regenerates past the cost"
        );
    }

    /// MP regen never overfills the pool past `max_mp`.
    #[test]
    fn mp_regen_caps_at_max() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 0.0);
        let _enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        a.ent(hero).mp = 98.0;
        a.ent(hero).max_mp = 100.0;
        a.ent(hero).mp_regen = 5.0;

        let mut combat = a.into_combat();
        combat.run(10);
        assert_eq!(combat.state.entity(hero).mp, 100.0, "regen shouldn't exceed max_mp");
    }

    /// Execute damage scales with the target's *missing* HP: +1% per 1% missing,
    /// so a half-dead target takes 1.5Ã— the base and a full-HP one just the base.
    #[test]
    fn execute_damage_scales_with_missing_hp() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let hurt = a.add("hurt", Team::Enemy, 50.0, 0.0); // at 50% of max_hp 100
        let reap = a.skill(Skill {
            name: "Reap".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::ExecuteDamage(10.0)],
        });
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), reap));

        let log = a.into_combat().tick();

        let amount = log
            .iter()
            .find_map(|e| match e {
                Event::Damage { amount, .. } => Some(*amount),
                _ => None,
            })
            .expect("a damage event");
        assert_eq!(amount, 15.0, "10 base * (1 + 0.5 missing)");
    }

    /// A drain heals the actor for `DRAIN_RATIO` of the damage actually dealt.
    #[test]
    fn drain_heals_the_attacker_for_half_the_damage() {
        let mut a = Arena::new();
        let vamp = a.add("vamp", Team::Player, 60.0, 1.0); // hurt: room to heal into
        let victim = a.add("victim", Team::Enemy, 100.0, 0.0);
        let siphon = a.skill(Skill {
            name: "Siphon".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Drain(20.0)],
        });
        a.gambit(vamp, Node::act(TargetQuery::new(Pool::Enemies), siphon));

        let mut combat = a.into_combat();
        combat.tick();

        assert_eq!(combat.state.entity(victim).hp, 80.0);
        assert_eq!(combat.state.entity(vamp).hp, 70.0, "healed for half of 20 dealt");
    }

    /// A Shield status halves incoming damage while it lasts.
    #[test]
    fn shield_halves_incoming_damage() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let tank = a.add("tank", Team::Enemy, 100.0, 0.0);
        a.ent(tank)
            .statuses
            .push(Status { kind: StatusKind::Shield, stacks: 1, duration: 10 });
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), hit));

        let mut combat = a.into_combat();
        combat.tick();

        assert_eq!(combat.state.entity(tank).hp, 90.0, "20 halved to 10 by the shield");
    }

    /// An enraged attacker deals `1 + ENRAGE_BONUS` times damage â€” and the bonus
    /// stacks multiplicatively with a weakness hit.
    #[test]
    fn enrage_boosts_outgoing_damage() {
        let mut a = Arena::new();
        let bruiser = a.add("bruiser", Team::Player, 100.0, 1.0);
        let victim = a.add("victim", Team::Enemy, 100.0, 0.0);
        a.ent(bruiser)
            .statuses
            .push(Status { kind: StatusKind::Enrage, stacks: 1, duration: 10 });
        a.ent(victim).weaknesses.push(DamageType::Fire);
        let burn = a.skill(damage_skill("Burn", 10.0, Some(DamageType::Fire), 0));
        a.gambit(bruiser, Node::act(TargetQuery::new(Pool::Enemies), burn));

        let mut combat = a.into_combat();
        combat.tick();

        // 10 * 1.5 (enrage) * 1.5 (weakness) = 22.5
        assert_eq!(combat.state.entity(victim).hp, 77.5);
    }

    /// An `Exposed` target takes `1 + EXPOSED_DAMAGE_BONUS` times damage —
    /// the multiplier lives on the target's side, so every source pays it.
    #[test]
    fn exposed_amplifies_incoming_damage() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 100.0, 1.0);
        let victim = a.add("victim", Team::Enemy, 100.0, 0.0);
        a.ent(victim)
            .statuses
            .push(Status { kind: StatusKind::Exposed, stacks: 1, duration: 10 });
        let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), hit));

        let mut combat = a.into_combat();
        combat.tick();

        let hp = combat.state.entity(victim).hp;
        assert!((hp - 78.0).abs() < 1e-3, "20 amplified to 22 by Exposed, got hp {hp}");
    }

    /// Every damaging hit an enemy lands on a `Lifeleech` bearer heals the
    /// attacker for `LEECH_HEAL_ON_HIT`; a DoT pulse on the bearer (no
    /// attacker at pulse time) procs nothing.
    #[test]
    fn lifeleech_heals_attackers_who_hit_the_bearer() {
        let mut a = Arena::new();
        let hero = a.add("hero", Team::Player, 60.0, 1.0); // hurt: room to heal into
        let mark = a.add("mark", Team::Enemy, 100.0, 0.0);
        a.ent(mark)
            .statuses
            .push(Status { kind: StatusKind::Lifeleech, stacks: 1, duration: 10 });
        // A poison pulse also damages the mark this tick — it must leech nothing.
        a.ent(mark)
            .statuses
            .push(Status { kind: StatusKind::Poison, stacks: 1, duration: 10 });
        let hit = a.skill(damage_skill("Hit", 10.0, None, 0));
        a.gambit(hero, Node::act(TargetQuery::new(Pool::Enemies), hit));

        let mut combat = a.into_combat();
        combat.tick();

        assert_eq!(
            combat.state.entity(hero).hp,
            63.0,
            "exactly the one landed hit should leech 3 back"
        );
    }

    /// Cleanse strips every harmful status but leaves beneficial ones alone.
    #[test]
    fn cleanse_strips_harmful_statuses_only() {
        let mut a = Arena::new();
        let cleric = a.add("cleric", Team::Player, 100.0, 1.0);
        let ally = a.add("ally", Team::Player, 100.0, 0.0);
        let _enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        for kind in [StatusKind::Poison, StatusKind::Snare] {
            a.ent(ally).statuses.push(Status { kind, stacks: 2, duration: 10 });
        }
        a.ent(ally)
            .statuses
            .push(Status { kind: StatusKind::Regen, stacks: 1, duration: 10 });
        let purify = a.skill(Skill {
            name: "Purify".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Cleanse],
        });
        a.gambit(
            cleric,
            Node::act(
                TargetQuery::new(Pool::Allies).filter(Filter::HasStatus(StatusKind::Poison)),
                purify,
            ),
        );

        let mut combat = a.into_combat();
        let log = combat.tick();

        assert!(log.contains(&Event::Cleansed { target: ally }));
        let statuses = &combat.state.entity(ally).statuses;
        assert!(
            statuses.iter().all(|s| !s.kind.is_harmful()),
            "harmful statuses should be gone, got {statuses:?}"
        );
        assert!(
            statuses.iter().any(|s| s.kind == StatusKind::Regen),
            "the beneficial Regen must survive the cleanse"
        );
    }

    /// A `Pick::All` heal is a group heal: every hurt ally in range is mended by
    /// the one action.
    #[test]
    fn pick_all_heal_mends_the_whole_group() {
        let mut a = Arena::new();
        let cleric = a.add("cleric", Team::Player, 100.0, 1.0);
        let ally1 = a.add("ally1", Team::Player, 40.0, 0.0);
        let ally2 = a.add("ally2", Team::Player, 60.0, 0.0);
        let _enemy = a.add("enemy", Team::Enemy, 100.0, 0.0);
        // Point-blank so both heals land instantly (no flights to wait out).
        a.ent(ally1).pos.x = 1.0;
        a.ent(ally2).pos.x = 2.0;
        let prayer = a.skill(Skill {
            name: "Prayer".into(),
            cost: 0,
            range: 8.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Heal(10.0)],
        });
        a.gambit(
            cleric,
            Node::act(
                TargetQuery::new(Pool::Allies)
                    .filter(Filter::HpPctBelow(0.9))
                    .pick(Pick::All),
                prayer,
            ),
        );

        let mut combat = a.into_combat();
        combat.tick();

        assert_eq!(combat.state.entity(ally1).hp, 50.0);
        assert_eq!(combat.state.entity(ally2).hp, 70.0);
    }

    /// A regen aura drips HP to teammates inside its radius â€” and only those:
    /// an ally beyond `AURA_RADIUS` and an enemy standing right in the field
    /// both get nothing.
    #[test]
    fn regen_aura_covers_only_nearby_teammates() {
        let mut a = Arena::new();
        let chanter = a.add_at("chanter", Team::Player, 50.0, 0.0, 5.0, 0.0);
        let near = a.add_at("near", Team::Player, 50.0, 0.0, 8.0, 0.0); // 3 away: covered
        let far = a.add_at("far", Team::Player, 50.0, 0.0, 20.0, 0.0); // 15 away: outside
        let foe = a.add_at("foe", Team::Enemy, 50.0, 0.0, 6.0, 0.0); // in the field, wrong team
        a.ent(chanter)
            .statuses
            .push(Status { kind: StatusKind::RegenAura, stacks: 1, duration: 100 });

        let mut combat = a.into_combat();
        combat.run(4);

        let hp = |id| combat.state.entity(id).hp;
        assert_eq!(hp(chanter), 53.0, "the bearer is inside its own aura: +0.75 x 4");
        assert_eq!(hp(near), 53.0, "a covered ally drips up");
        assert_eq!(hp(far), 50.0, "outside the radius: no benefit");
        assert_eq!(hp(foe), 50.0, "enemies never benefit");
    }

    /// A might aura scales an attacker's damage by 5% â€” but only while the
    /// attacker stands inside the field.
    #[test]
    fn might_aura_boosts_allies_inside_the_radius_only() {
        for (attacker_x, expected) in [(5.0, 21.0), (20.0, 20.0)] {
            let mut a = Arena::new();
            let chanter = a.add_at("chanter", Team::Player, 100.0, 0.0, 5.0, 0.0);
            let hitter = a.add_at("hitter", Team::Player, 100.0, 1.0, attacker_x, 0.0);
            let victim = a.add_at("victim", Team::Enemy, 100.0, 0.0, attacker_x + 1.0, 0.0);
            a.ent(chanter)
                .statuses
                .push(Status { kind: StatusKind::MightAura, stacks: 1, duration: 100 });
            let hit = a.skill(damage_skill("Hit", 20.0, None, 0));
            a.gambit(hitter, Node::act(TargetQuery::new(Pool::Enemies), hit));

            let mut combat = a.into_combat();
            combat.tick();

            let dealt = 100.0 - combat.state.entity(victim).hp;
            assert!(
                (dealt - expected).abs() < 1e-3,
                "attacker at x={attacker_x}: expected {expected} damage, dealt {dealt}"
            );
        }
    }

    /// An entity holds one aura at a time â€” a new chant replaces the old â€” and
    /// re-singing the same chant refreshes its duration without stacking.
    #[test]
    fn one_aura_at_a_time_and_no_aura_stacking() {
        let mut a = Arena::new();
        let chanter = a.add("chanter", Team::Player, 100.0, 1.0);
        let _foe = a.add("foe", Team::Enemy, 100.0, 0.0);
        let sing = |name: &str, kind, cooldown| Skill {
            name: name.into(),
            cost: 0,
            range: 1000.0,
            cooldown,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::Inflict { kind, stacks: 1, duration: 10 }],
        };
        // The regen chant's long cooldown sequences the ticks deterministically:
        // it fires once, then every later action falls through to the might chant.
        let regen_chant = a.skill(sing("Life Chant", StatusKind::RegenAura, 100));
        let might_chant = a.skill(sing("War Chant", StatusKind::MightAura, 0));
        a.gambit(
            chanter,
            Node::context(
                Condition::Always,
                GroupMode::Fallthrough,
                vec![
                    Node::act(TargetQuery::new(Pool::Myself), regen_chant),
                    Node::act(TargetQuery::new(Pool::Myself), might_chant),
                ],
            ),
        );

        let mut combat = a.into_combat();
        combat.tick(); // sings Life Chant
        assert!(combat.state.entity(chanter).status(StatusKind::RegenAura).is_some());

        combat.tick(); // sings War Chant â€” which must displace the regen aura
        let e = combat.state.entity(chanter);
        assert!(e.status(StatusKind::RegenAura).is_none(), "one aura at a time");
        assert!(e.status(StatusKind::MightAura).is_some());

        combat.tick(); // re-sings War Chant: refresh, not stack
        assert_eq!(combat.state.entity(chanter).status_stacks(StatusKind::MightAura), 1);
    }

    /// An MP drain steals up to its amount â€” capped by what the target has â€”
    /// and credits it to the actor.
    #[test]
    fn drain_mp_steals_capped_by_the_targets_pool() {
        let mut a = Arena::new();
        let rend = a.add("rend", Team::Player, 100.0, 1.0);
        let caster = a.add("caster", Team::Enemy, 100.0, 0.0);
        a.ent(rend).mp = 10.0;
        a.ent(caster).mp = 8.0; // less than the drain amount
        let mana_rend = a.skill(Skill {
            name: "Mana Rend".into(),
            cost: 0,
            range: 1000.0,
            cooldown: 0,
            cast_time: 0,
            damage_type: None,
            effects: vec![Effect::DrainMp(15.0)],
        });
        a.gambit(rend, Node::act(TargetQuery::new(Pool::Enemies), mana_rend));

        let mut combat = a.into_combat();
        let log = combat.tick();

        assert_eq!(combat.state.entity(caster).mp, 0.0);
        assert_eq!(combat.state.entity(rend).mp, 18.0, "credited what was actually there");
        assert!(log.iter().any(|e| matches!(
            e,
            Event::MpDrained { target, amount } if *target == caster && *amount == 8.0
        )));
    }

    /// A snare cuts drift by `SNARE_SLOW`: a snared mover covers only the reduced
    /// fraction of its `move_speed` each tick.
    #[test]
    fn snare_slows_drift() {
        let mut a = Arena::new();
        let hero = a.add_at("hero", Team::Player, 100.0, 0.0, 0.0, 2.0); // move_speed 2
        let enemy = a.add_at("enemy", Team::Enemy, 100.0, 0.0, 100.0, 0.0);
        a.ent(hero).pos.y = 5.0;
        a.ent(enemy).pos.y = 5.0; // same row -> drift is purely along x
        a.ent(hero)
            .statuses
            .push(Status { kind: StatusKind::Snare, stacks: 1, duration: 5 });
        a.move_gambit(
            hero,
            MoveGambit::toward(
                TargetQuery::new(Pool::Enemies).sort(SortKey::Distance, Order::Asc),
            ),
        );

        let mut combat = a.into_combat();
        let x0 = combat.state.entity(hero).pos.x;
        combat.tick();

        // 2.0 * (1 - 0.6) = 0.8 units this tick, not the full 2.0.
        let moved = combat.state.entity(hero).pos.x - x0;
        assert!((moved - 0.8).abs() < 1e-3, "snared drift should be 0.8, got {moved}");
    }
}
