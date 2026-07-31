//! The particle burst layer — the reference client's `FruitBurst.tsx`,
//! ported (MOTION.md §7).
//!
//! The reference sprays fruit; PocketSkynet sprays sparks — same physics,
//! this product's iconography. Two variants, same as the original:
//!
//! * **pop** — radial celebration, biased upward "so it feels like a joyful
//!   pop, not a splat" (their comment, kept because it is the whole spec).
//!   Fired from the send button.
//! * **poof** — a falling fizzle, for removals. Fired at a message bubble
//!   the moment its deletion is confirmed, alongside the CSS dissolve.
//!
//! Framer-motion drove the original's per-particle keyframes; here each
//! particle is one `<span>` whose trajectory rides CSS custom properties
//! into a single shared `@keyframes` (app.css §7) — the compositor does the
//! animating, Rust only places the endpoints. Trajectories come from a tiny
//! xorshift seeded by the burst id: deterministic, so the geometry is
//! host-testable, and two bursts still never look alike.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use yew::prelude::*;

/// `pop` = radial celebration; `poof` = falling fizzle (removals).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    Pop,
    Poof,
}

#[derive(Clone, PartialEq)]
pub struct Burst {
    id: u64,
    x: f64,
    y: f64,
    variant: Variant,
    count: usize,
}

thread_local! {
    /// The mounted layer's inbox. A module-level emitter rather than a prop
    /// drilled through six components — same design as the reference's
    /// singleton `burstAt`.
    static EMIT: RefCell<Option<Callback<Op>>> = const { RefCell::new(None) };
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_id() -> u64 {
    NEXT_ID.with(|n| {
        let id = n.get();
        n.set(id + 1);
        id
    })
}

fn send(op: Op) {
    EMIT.with(|e| {
        if let Some(cb) = e.borrow().as_ref() {
            cb.emit(op);
        }
    });
}

/// How long the machine visibly thinks before it acts.
pub const PROC_MS: u32 = 3_000;

/// How long after a discharge the thing it produced should appear.
///
/// Tuned to the origin flash (300ms in `fn-burst-flash`) rather than to the
/// particles, which fly for over a second: the arrival wants to land while the
/// streaks are still in the air, so the burst reads as its cause. Waiting for
/// the whole burst instead reads as two unrelated animations.
pub const SPARK_LEAD_MS: u32 = 260;

/// Viewport coordinates → coordinates inside the particle layer.
///
/// This is the whole fix for the effect landing in the wrong place on iPhone.
/// Particles used to be `position: fixed` placed at raw `getBoundingClientRect`
/// values, which assumes those two share an origin. On iOS Safari they do not:
/// with the on-screen keyboard up — i.e. exactly when you have just typed a
/// message and hit send — client rects are reported against the *visual*
/// viewport while `fixed` boxes are laid out against the *layout* viewport, so
/// the burst appeared hundreds of pixels from the button that fired it.
///
/// Measuring the layer the same way the target was measured makes the units
/// cancel: whichever viewport the browser is reporting, the difference between
/// two rects taken from it is correct. The layer is therefore always mounted
/// (even with nothing in flight) so there is always something to measure, and
/// particles are `absolute` inside it. This also immunises the effect against
/// a transformed ancestor, which would otherwise re-anchor `fixed` children.
fn to_layer(x: f64, y: f64) -> (f64, f64) {
    let rect = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(".fn-burstlayer").ok().flatten())
        .map(|el| el.get_bounding_client_rect());
    match rect {
        Some(r) => (x - r.left(), y - r.top()),
        None => (x, y),
    }
}

/// Fire a burst at viewport coordinates. No-op if the layer isn't mounted.
pub fn burst_at(x: f64, y: f64, variant: Variant, count: usize) {
    let (x, y) = to_layer(x, y);
    send(Op::Add(Burst {
        id: next_id(),
        x,
        y,
        variant,
        count,
    }));
}

/// Raise the processing HUD. Returns the id to hand back to [`proc_end`].
///
/// Deliberately centred on screen rather than pinned to what fired it: it is
/// the machine's own readout, it must not be clipped by a corner, and at this
/// size it would cover the thing it is talking about.
pub fn proc_start(variant: Variant) -> u64 {
    let id = next_id();
    send(Op::ProcStart(Proc { id, variant }));
    id
}

/// Drop the processing HUD. Safe to call for an id that is already gone.
pub fn proc_end(id: u64) {
    send(Op::ProcEnd(id));
}

// ------------------------------------------------------------------ tx HUD --

/// Where a transaction currently is. The labels are HUD codes in the same
/// register as the boot screen and [`proc_hud`] — deliberately untranslated,
/// like every other machine-voice string in this product.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxPhase {
    /// Reading nonce and gas from the chain.
    Uplink,
    /// The key is producing the signature.
    Sign,
    /// The raw transaction is going out.
    Broadcast,
    /// Waiting for a receipt.
    Confirm,
}

impl TxPhase {
    pub const ALL: [TxPhase; 4] = [
        TxPhase::Uplink,
        TxPhase::Sign,
        TxPhase::Broadcast,
        TxPhase::Confirm,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TxPhase::Uplink => "UPLINK · CHAIN STATE",
            TxPhase::Sign => "SIGNING PAYLOAD",
            TxPhase::Broadcast => "BROADCASTING",
            TxPhase::Confirm => "AWAITING CONFIRMATION",
        }
    }

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }
}

/// One in-flight transaction's readout.
#[derive(Clone, PartialEq)]
struct TxHud {
    id: u64,
    phase: TxPhase,
    /// `Some(ok)` once the outcome is known; the HUD flashes it, then reaps.
    verdict: Option<bool>,
}

/// How long the verdict stays on screen before the HUD reaps itself.
const TX_VERDICT_MS: u32 = 1_100;

/// Raise the transaction HUD — the Skynet relay readout that runs while a
/// transaction is signed, broadcast and confirmed. Returns the id for
/// [`tx_phase`] / [`tx_end`].
pub fn tx_start() -> u64 {
    let id = next_id();
    send(Op::TxStart(id));
    id
}

/// Advance the readout. Safe for an id that is already gone.
pub fn tx_phase(id: u64, phase: TxPhase) {
    send(Op::TxPhase(id, phase));
}

/// Show the verdict, then drop the HUD.
pub fn tx_end(id: u64, ok: bool) {
    send(Op::TxVerdict(id, ok));
}

fn centre_of(el: &web_sys::Element) -> (f64, f64) {
    let rect = el.get_bounding_client_rect();
    (
        rect.left() + rect.width() / 2.0,
        rect.top() + rect.height() / 2.0,
    )
}

/// Viewport centre of the first element `selector` matches. `None` when nothing
/// matches — an effect must never be the thing that panics.
///
/// Worth capturing separately from firing when a delay sits between the two: a
/// position read three seconds late is a position read after the layout moved.
pub fn centre_of_selector(selector: &str) -> Option<(f64, f64)> {
    web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(selector).ok().flatten())
        .map(|el| centre_of(&el))
}

/// Viewport centre of a rendered node (the send button).
pub fn centre_of_node(node: &NodeRef) -> Option<(f64, f64)> {
    node.cast::<web_sys::Element>().map(|el| centre_of(&el))
}

/// Burst from the centre of the first element `selector` matches.
pub fn burst_from_selector(selector: &str, variant: Variant, count: usize) {
    if let Some((x, y)) = centre_of_selector(selector) {
        burst_at(x, y, variant, count);
    }
}

/// Burst from a rendered node (the send button).
pub fn burst_from_node(node: &NodeRef, variant: Variant, count: usize) {
    if let Some((x, y)) = centre_of_node(node) {
        burst_at(x, y, variant, count);
    }
}

/// Debris glyphs. The reference sprayed fruit and the first port sprayed
/// sparkles; neither reads as *current*. A hard angular set does, and it leaves
/// the soft round shapes to the streaks, which carry the light.
const GLYPHS: [&str; 4] = ["⚡", "✦", "◆", "▰"];

/// What a particle is made of. A spark is mostly current and a little wreckage,
/// so the set mixes the two: streaks are the discharge, glyphs are what it tore
/// loose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// A bare bright line, drawn along its own velocity. This is the spark.
    Streak,
    /// A glyph tumbling on its own axis — debris caught in the discharge.
    Glyph(&'static str),
}

/// Everything a particle needs; app.css §7 turns these into motion.
#[derive(Debug, Clone, PartialEq)]
pub struct Particle {
    pub ink: Ink,
    pub dx: f64,
    pub dy: f64,
    pub fall: f64,
    pub rotate: f64,
    pub scale: f64,
    pub duration_ms: u32,
    pub delay_ms: u32,
    /// Degrees, the direction of travel. A streak is rotated by this so it lies
    /// along its own path — a spark crosswise to its motion reads as a twig.
    pub angle: f64,
}

/// xorshift64* — eight lines of deterministic scatter. Seeded per burst so a
/// burst's shape is a pure function of its id, which is what the tests pin.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// Uniform in [0, 1).
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// The trajectory maths. The reference's three-point path [origin, apex, rest]
/// and its xorshift scatter are kept; the tuned constants are not, because the
/// reference was throwing fruit and this throws current. Sparks leave faster,
/// travel further and die sooner than confetti does — a discharge is over
/// before a celebration has finished rising.
///
/// Every third particle is debris and the rest are streaks: enough wreckage to
/// read as something breaking, not enough to read as a shape.
pub fn make_particles(id: u64, variant: Variant, count: usize) -> Vec<Particle> {
    let mut rng = Rng::new(id);
    (0..count)
        .map(|i| {
            let ink = if i % 3 == 2 {
                Ink::Glyph(GLYPHS[(i / 3) % GLYPHS.len()])
            } else {
                Ink::Streak
            };
            match variant {
                // TERMINATION. A bubble does not fizzle out, it is unmade: the
                // discharge blows sideways and *then* the debris falls. Wider
                // and faster than the fizzle it replaces, and short enough that
                // the fall lands under the collapsing row rather than after it.
                Variant::Poof => {
                    let spread = (rng.next() - 0.5) * 150.0;
                    let lift = -28.0 - rng.next() * 34.0;
                    Particle {
                        ink,
                        dx: spread,
                        dy: lift,
                        fall: 78.0 + rng.next() * 66.0,
                        rotate: (rng.next() - 0.5) * 420.0,
                        scale: 0.65 + rng.next() * 0.5,
                        duration_ms: (430.0 + rng.next() * 210.0) as u32,
                        delay_ms: (rng.next() * 90.0) as u32,
                        angle: spread.atan2(lift).to_degrees(),
                    }
                }
                // TRANSMISSION. A ring discharge off the send button. Still
                // biased upward — it should leave, not spill — but the ring is
                // tighter and hotter than the reference's joyful pop.
                Variant::Pop => {
                    let angle =
                        (i as f64 / count as f64) * std::f64::consts::TAU + rng.next() * 0.5;
                    let dist = 70.0 + rng.next() * 85.0;
                    let dx = angle.cos() * dist;
                    let dy = angle.sin() * dist * 0.8 - 30.0;
                    Particle {
                        ink,
                        dx,
                        dy,
                        fall: 34.0 + rng.next() * 52.0,
                        rotate: (rng.next() - 0.5) * 380.0,
                        scale: 0.75 + rng.next() * 0.65,
                        duration_ms: (520.0 + rng.next() * 300.0) as u32,
                        delay_ms: (rng.next() * 70.0) as u32,
                        angle: dy.atan2(dx).to_degrees(),
                    }
                }
            }
        })
        .collect()
}

/// Longest possible particle (duration + delay) plus the reference's 120ms
/// grace — one constant instead of a per-burst max-scan.
const BURST_TTL_MS: u32 = 1_300;

/// Never more than this many live bursts; a delete-spam session must not
/// accumulate layers of confetti. Same cap as the reference (`slice(-7)`).
const MAX_BURSTS: usize = 8;

/// A processing pass — the machine thinking out loud before it acts.
#[derive(Clone, PartialEq)]
struct Proc {
    id: u64,
    variant: Variant,
}

#[derive(Default, PartialEq)]
struct Layer {
    bursts: Vec<Burst>,
    procs: Vec<Proc>,
    txs: Vec<TxHud>,
}

enum Op {
    Add(Burst),
    Remove(u64),
    ProcStart(Proc),
    ProcEnd(u64),
    TxStart(u64),
    TxPhase(u64, TxPhase),
    TxVerdict(u64, bool),
    TxRemove(u64),
}

impl Reducible for Layer {
    type Action = Op;

    fn reduce(self: Rc<Self>, op: Op) -> Rc<Self> {
        let mut bursts = self.bursts.clone();
        let mut procs = self.procs.clone();
        let mut txs = self.txs.clone();
        match op {
            Op::Add(b) => {
                if bursts.len() >= MAX_BURSTS {
                    bursts.remove(0);
                }
                bursts.push(b);
            }
            Op::Remove(id) => bursts.retain(|b| b.id != id),
            // One readout at a time. A second surge replaces the first rather
            // than stacking two HUDs on top of each other.
            Op::ProcStart(p) => procs = vec![p],
            Op::ProcEnd(id) => procs.retain(|p| p.id != id),
            // Same rule for transactions: an approval flow can chain two txs
            // (approve, then swap) — the second relay takes the stage over.
            Op::TxStart(id) => {
                txs = vec![TxHud {
                    id,
                    phase: TxPhase::Uplink,
                    verdict: None,
                }]
            }
            Op::TxPhase(id, phase) => {
                for t in &mut txs {
                    if t.id == id && t.verdict.is_none() {
                        t.phase = phase;
                    }
                }
            }
            Op::TxVerdict(id, ok) => {
                for t in &mut txs {
                    if t.id == id {
                        t.verdict = Some(ok);
                    }
                }
            }
            Op::TxRemove(id) => txs.retain(|t| t.id != id),
        }
        Rc::new(Layer { bursts, procs, txs })
    }
}

/// Mount exactly once, near the app root (`app.rs`), like the reference's
/// `<FruitBurstLayer/>`.
#[function_component(BurstLayer)]
pub fn burst_layer() -> Html {
    let layer = use_reducer(Layer::default);

    {
        let layer = layer.clone();
        use_effect_with((), move |_| {
            let cb = Callback::from(move |op: Op| {
                // A burst reaps itself once its particles have landed; a proc
                // is reaped by whoever raised it, since only the caller knows
                // when the work it stands for is done. A tx HUD reaps itself
                // a beat after its verdict lands, so the outcome is readable.
                let reap = match &op {
                    Op::Add(b) => Some((Op::Remove(b.id), BURST_TTL_MS)),
                    Op::TxVerdict(id, _) => Some((Op::TxRemove(*id), TX_VERDICT_MS)),
                    _ => None,
                };
                layer.dispatch(op);
                if let Some((op, after_ms)) = reap {
                    let layer = layer.clone();
                    wasm_bindgen_futures::spawn_local(async move {
                        gloo_timers::future::TimeoutFuture::new(after_ms).await;
                        layer.dispatch(op);
                    });
                }
            });
            EMIT.with(|e| *e.borrow_mut() = Some(cb));
            || EMIT.with(|e| *e.borrow_mut() = None)
        });
    }

    // Reduced motion: no particles at all, same as the reference. (§17's
    // global duration clamp would hide them anyway; not mounting them spares
    // the DOM churn too.)
    let reduced = web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches());
    if reduced {
        return Html::default();
    }

    // Always mounted, even with nothing in flight: `to_layer` measures this
    // element to convert coordinates, so it has to exist before the first
    // burst rather than appearing with it.
    html! {
        <div class="fn-burstlayer" aria-hidden="true">
            { for layer.txs.iter().map(tx_hud) }
            { for layer.procs.iter().map(proc_hud) }
            { for layer.bursts.iter().map(burst_group) }
        </div>
    }
}

/// The transaction relay readout: the same reticle as [`proc_hud`], held for
/// as long as the transaction runs, with a phase roll-call driven by
/// [`tx_phase`] and a verdict flash at the end. Skynet watching your money
/// move — which is exactly what is happening.
fn tx_hud(t: &TxHud) -> Html {
    let tone = match t.verdict {
        Some(false) => "fn-burst--term",
        _ => "fn-burst--live",
    };
    let active = t.phase.index();
    html! {
        <div key={t.id} class={classes!("fn-txhud", tone)} data-verdict={t.verdict.map(|ok| if ok { "ok" } else { "fail" })}>
            <div class="fn-txhud__frame">
                <span class="fn-txhud__title">{ "SKYNET RELAY" }</span>
                <div class="fn-proc__rings">
                    <i class="fn-proc__ring fn-proc__ring--a" />
                    <i class="fn-proc__ring fn-proc__ring--b" />
                    <i class="fn-proc__ring fn-proc__ring--c" />
                    <i class="fn-proc__core" />
                    <i class="fn-proc__sweep" />
                </div>
                if let Some(ok) = t.verdict {
                    <span class="fn-txhud__verdict">
                        { if ok { "CONFIRMED" } else { "REJECTED" } }
                    </span>
                } else {
                    <ul class="fn-txhud__phases">
                        { for TxPhase::ALL.iter().map(|p| {
                            let state = if p.index() < active { "done" }
                                        else if p.index() == active { "active" }
                                        else { "pending" };
                            html! {
                                <li key={p.label()} class="fn-txhud__phase" data-state={state}>
                                    <i class="fn-txhud__tick" />
                                    { p.label() }
                                </li>
                            }
                        }) }
                    </ul>
                }
                <div class="fn-txhud__bar">
                    <i style={format!(
                        "inline-size: {}%;",
                        match t.verdict {
                            Some(_) => 100,
                            None => (active + 1) * 100 / TxPhase::ALL.len(),
                        }
                    )} />
                </div>
            </div>
        </div>
    }
}

/// The readout. Three phases at a second each, stepped so the text changes
/// like a machine's display rather than crossfading like a slideshow.
fn proc_hud(p: &Proc) -> Html {
    let (tone, phases) = match p.variant {
        Variant::Pop => (
            "fn-burst--live",
            ["LINK ESTABLISHED", "ENCRYPTING PAYLOAD", "TRANSMITTING"],
        ),
        Variant::Poof => (
            "fn-burst--term",
            ["TARGET ACQUIRED", "PURGING RECORD", "TERMINATED"],
        ),
    };
    html! {
        <div key={p.id} class={classes!("fn-proc", tone)}>
            <div class="fn-proc__rings">
                <i class="fn-proc__ring fn-proc__ring--a" />
                <i class="fn-proc__ring fn-proc__ring--b" />
                <i class="fn-proc__ring fn-proc__ring--c" />
                <i class="fn-proc__core" />
                <i class="fn-proc__sweep" />
            </div>
            <div class="fn-proc__readout">
                { for phases.iter().enumerate().map(|(i, label)| html! {
                    <span
                        class="fn-proc__phase"
                        style={format!("--i:{i};")}
                    >{ *label }</span>
                }) }
            </div>
            <div class="fn-proc__bar"><i /></div>
        </div>
    }
}

fn burst_group(b: &Burst) -> Html {
    let particles = make_particles(b.id, b.variant, b.count);
    // Cyan is this system running; red is this system destroying something.
    // The two bursts must never be mistaken for each other at a glance.
    let tone = match b.variant {
        Variant::Pop => "fn-burst--live",
        Variant::Poof => "fn-burst--term",
    };
    html! {
        <>
        // The discharge itself: one ring at the origin, gone in 300ms. Without
        // it the particles appear from nothing; with it they are thrown.
        <span
            key={format!("{}-flash", b.id)}
            class={classes!("fn-burst__flash", tone)}
            style={format!("left:{:.1}px; top:{:.1}px;", b.x, b.y)}
        />
        { for particles.into_iter().enumerate().map(|(i, p)| {
            let style = format!(
                "left:{x:.1}px; top:{y:.1}px; \
                 --dx:{dx:.1}px; --dy:{dy:.1}px; --fall:{fall:.1}px; \
                 --r1:{r1:.0}deg; --r2:{r2:.0}deg; --s:{s:.2}; --a:{a:.0}deg; \
                 animation-duration:{dur}ms; animation-delay:{delay}ms;",
                x = b.x,
                y = b.y,
                dx = p.dx,
                dy = p.dy,
                fall = p.fall,
                r1 = p.rotate * 0.6,
                r2 = p.rotate,
                s = p.scale,
                a = p.angle,
                dur = p.duration_ms,
                delay = p.delay_ms,
            );
            let (kind, glyph) = match p.ink {
                Ink::Streak => ("fn-burst__p--streak", None),
                Ink::Glyph(g) => ("fn-burst__p--glyph", Some(g)),
            };
            html! {
                <span
                    key={format!("{}-{i}", b.id)}
                    class={classes!("fn-burst__p", kind, tone)}
                    {style}
                >
                    { glyph }
                </span>
            }
        }) }
        </>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_is_a_pure_function_of_its_id() {
        assert_eq!(
            make_particles(7, Variant::Pop, 10),
            make_particles(7, Variant::Pop, 10)
        );
        assert_ne!(
            make_particles(7, Variant::Pop, 10),
            make_particles(8, Variant::Pop, 10)
        );
    }

    #[test]
    fn pop_scatters_radially_and_rises() {
        let particles = make_particles(1, Variant::Pop, 12);
        assert_eq!(particles.len(), 12);
        // Radial: some go left, some go right.
        assert!(particles.iter().any(|p| p.dx < 0.0));
        assert!(particles.iter().any(|p| p.dx > 0.0));
        // The upward bias: on average the first leg points up (negative y).
        let mean_dy: f64 = particles.iter().map(|p| p.dy).sum::<f64>() / particles.len() as f64;
        assert!(mean_dy < 0.0, "pop should rise, mean dy {mean_dy}");
    }

    #[test]
    fn poof_blows_outward_then_always_falls() {
        for p in make_particles(2, Variant::Poof, 10) {
            assert!(p.dy < 0.0, "the discharge throws debris up first");
            // Net travel is downward: the fall outweighs the lift.
            assert!(p.dy + p.fall > 0.0, "a poof must end below where it began");
            // Deliberately wide — a termination blows sideways. The old fizzle
            // capped at 30px; this is the change, so it is pinned, not relaxed.
            assert!(p.dx.abs() <= 75.0, "a poof stays bounded, dx {}", p.dx);
        }
    }

    #[test]
    fn a_burst_is_mostly_current_and_a_little_wreckage() {
        let particles = make_particles(9, Variant::Pop, 9);
        let streaks = particles.iter().filter(|p| p.ink == Ink::Streak).count();
        let debris = particles.len() - streaks;
        assert!(streaks > debris, "sparks read as current, not confetti");
        assert!(debris > 0, "some wreckage sells that something broke");
    }

    #[test]
    fn a_streak_lies_along_its_own_velocity() {
        // A spark drawn crosswise to its motion reads as a twig, so the angle
        // must track the direction of travel, not the tumble.
        for p in make_particles(11, Variant::Pop, 12) {
            let expected = p.dy.atan2(p.dx).to_degrees();
            assert!(
                (p.angle - expected).abs() < 1e-9,
                "angle {} should follow velocity {expected}",
                p.angle
            );
        }
    }

    #[test]
    fn every_particle_finishes_inside_the_layer_ttl() {
        for variant in [Variant::Pop, Variant::Poof] {
            for p in make_particles(3, variant, 16) {
                assert!(p.duration_ms + p.delay_ms < BURST_TTL_MS);
            }
        }
    }

    #[test]
    fn scales_and_durations_stay_in_their_tuned_bands() {
        for p in make_particles(4, Variant::Pop, 10) {
            assert!((0.75..=1.4).contains(&p.scale));
            assert!((520..=820).contains(&p.duration_ms));
        }
        for p in make_particles(5, Variant::Poof, 10) {
            assert!((0.65..=1.15).contains(&p.scale));
            assert!((430..=640).contains(&p.duration_ms));
        }
    }

    // ---- the transaction relay HUD state machine ---------------------------

    /// Drive the pure reducer the way the component does.
    fn reduce(layer: Layer, op: Op) -> Layer {
        let next = Rc::new(layer).reduce(op);
        Layer {
            bursts: next.bursts.clone(),
            procs: next.procs.clone(),
            txs: next.txs.clone(),
        }
    }

    #[test]
    fn tx_phases_are_ordered_like_a_transaction() {
        // The roll-call renders in `ALL` order and the bar fills by index, so
        // the order IS the animation. Pin it.
        assert_eq!(
            TxPhase::ALL.map(|p| p.index()),
            [0, 1, 2, 3],
            "index() must walk ALL in order"
        );
        assert_eq!(TxPhase::Uplink.index(), 0);
        assert_eq!(TxPhase::Confirm.index(), TxPhase::ALL.len() - 1);
        // Labels are distinct — two phases with one label would make the
        // roll-call lie about progress.
        let mut labels: Vec<_> = TxPhase::ALL.iter().map(|p| p.label()).collect();
        labels.dedup();
        assert_eq!(labels.len(), TxPhase::ALL.len());
    }

    #[test]
    fn a_tx_hud_walks_start_phase_verdict() {
        let mut layer = reduce(Layer::default(), Op::TxStart(1));
        assert_eq!(layer.txs.len(), 1);
        assert_eq!(
            layer.txs[0].phase,
            TxPhase::Uplink,
            "a relay starts at uplink"
        );
        assert_eq!(layer.txs[0].verdict, None);

        layer = reduce(layer, Op::TxPhase(1, TxPhase::Broadcast));
        assert_eq!(layer.txs[0].phase, TxPhase::Broadcast);

        layer = reduce(layer, Op::TxVerdict(1, true));
        assert_eq!(layer.txs[0].verdict, Some(true));

        layer = reduce(layer, Op::TxRemove(1));
        assert!(layer.txs.is_empty());
    }

    #[test]
    fn a_second_relay_takes_the_stage_over() {
        // One HUD at a time: an approve-then-swap chain must replace, not
        // stack — two reticles on top of each other read as a glitch.
        let layer = reduce(Layer::default(), Op::TxStart(1));
        let layer = reduce(layer, Op::TxStart(2));
        assert_eq!(layer.txs.len(), 1);
        assert_eq!(layer.txs[0].id, 2);
    }

    #[test]
    fn a_verdict_freezes_the_phase() {
        // The reap timer fires 1.1s after the verdict; a late phase update
        // arriving in that window must not un-say CONFIRMED.
        let layer = reduce(Layer::default(), Op::TxStart(1));
        let layer = reduce(layer, Op::TxVerdict(1, false));
        let layer = reduce(layer, Op::TxPhase(1, TxPhase::Confirm));
        assert_eq!(layer.txs[0].phase, TxPhase::Uplink, "phase must not move");
        assert_eq!(layer.txs[0].verdict, Some(false));
    }

    #[test]
    fn tx_ops_for_a_dead_id_are_safe_no_ops() {
        let layer = reduce(Layer::default(), Op::TxPhase(9, TxPhase::Sign));
        assert!(layer.txs.is_empty());
        let layer = reduce(layer, Op::TxVerdict(9, true));
        assert!(layer.txs.is_empty());
        let layer = reduce(layer, Op::TxRemove(9));
        assert!(layer.txs.is_empty());
    }

    #[test]
    fn the_relay_never_disturbs_bursts_or_procs() {
        // The three families share one layer; a tx op must not reap a burst
        // mid-flight or dismiss a processing reticle.
        let layer = reduce(
            Layer::default(),
            Op::Add(Burst {
                id: 1,
                x: 0.0,
                y: 0.0,
                variant: Variant::Pop,
                count: 3,
            }),
        );
        let layer = reduce(
            layer,
            Op::ProcStart(Proc {
                id: 2,
                variant: Variant::Pop,
            }),
        );
        let layer = reduce(layer, Op::TxStart(3));
        let layer = reduce(layer, Op::TxVerdict(3, true));
        let layer = reduce(layer, Op::TxRemove(3));
        assert_eq!(layer.bursts.len(), 1);
        assert_eq!(layer.procs.len(), 1);
        assert!(layer.txs.is_empty());
    }

    #[test]
    fn a_discharge_outruns_a_transmission() {
        // Tone check, not decoration: termination must feel like a snap and
        // sending like a throw. If these ever cross, the two effects have
        // swapped character.
        let slowest_poof = make_particles(6, Variant::Poof, 12)
            .iter()
            .map(|p| p.duration_ms)
            .max()
            .unwrap();
        let slowest_pop = make_particles(6, Variant::Pop, 12)
            .iter()
            .map(|p| p.duration_ms)
            .max()
            .unwrap();
        assert!(slowest_poof < slowest_pop, "{slowest_poof} < {slowest_pop}");
    }
}
