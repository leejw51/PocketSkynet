//! The portrait spotlight — tap a face, get a moment.
//!
//! Clicking any identity image (your operator portrait in the top bar, the
//! AI Banker's endoskeleton, the Bank's vault emblem) raises a full-screen
//! stage: the portrait zooms in under a ring of orbiting light, a particle
//! swarm draws glowing trails around it on a 2D canvas, and the whole stage
//! tilts in 3D toward the pointer. Pure theatre, on purpose.
//!
//! No scene-graph library. The same reasoning as the GL backdrop
//! (`backdrop.rs`): three.js is ~600KB plus a JS shim to keep in step with
//! the Rust that drives it, and this scene is a hundred glowing arcs. The
//! trails come from the oldest trick in the canvas book — erase a fraction
//! of the previous frame with `destination-out`, draw the new positions
//! with `lighter` — and the orbit maths is pure Rust, host-tested below.
//!
//! Singleton like the burst layer: mounted once near the root, fired from
//! anywhere with [`show`]. One spotlight at a time; Escape, the close
//! button or a scrim click dismisses it. Under `prefers-reduced-motion`
//! the canvas and the orbit rings stay dark and the portrait simply
//! appears — the information (the big portrait, the caption, the copy
//! button) is all still there.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::JsCast;
use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::use_store;

use super::toast;

// ------------------------------------------------------------------ the API --

/// What the stage shows.
#[derive(Clone, PartialEq)]
pub struct Spot {
    /// The image URL (an `/static/img/…` portrait).
    pub image: String,
    /// The name under the portrait, set in the display face.
    pub title: String,
    /// A quieter second line — an address, a role.
    pub subtitle: Option<String>,
    /// When set, a copy button appears and copies this value (the wallet
    /// address, so the old top-bar copy gesture survives inside the stage).
    pub copy: Option<String>,
    /// The accent hue the trails burn in. Identity hue for people, the
    /// product cyan (190) for the machines.
    pub hue: u16,
}

enum Op {
    Open(Spot),
    Close,
}

thread_local! {
    static EMIT: RefCell<Option<Callback<Op>>> = const { RefCell::new(None) };
}

fn send(op: Op) {
    EMIT.with(|e| {
        if let Some(cb) = e.borrow().as_ref() {
            cb.emit(op);
        }
    });
}

/// Raise the spotlight. No-op if the layer isn't mounted.
pub fn show(spot: Spot) {
    send(Op::Open(spot));
}

/// Raise the spotlight for an identity tile: a person (wallet address) or a
/// room (room id). The portrait is exactly what the tile itself renders —
/// the chosen profile image when the caller has one at hand, else the
/// hash-picked art — because the spotlight is the zoomed view of the tile
/// that was tapped, and the two showing different faces reads as a bug.
pub fn show_identity(
    seed: &str,
    image: Option<&str>,
    title: String,
    subtitle: Option<String>,
    copy: Option<String>,
) {
    show(Spot {
        image: image
            .and_then(crate::identity::avatar_src)
            .unwrap_or_else(|| format!("/static/img/{}.png", crate::identity::art_for(seed))),
        title,
        subtitle,
        copy,
        hue: crate::identity::hue_for(seed),
    });
}

// ------------------------------------------------------------- orbit maths --

/// The pure half, shaped for testing: everything the canvas needs each frame
/// is a function of (seed, time).
pub mod fx {
    /// One orbiting spark. Distances are in units of the portrait radius, so
    /// the renderer scales them by whatever the layout decided.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Orb {
        /// Semi-axes of the elliptical orbit, ≥ 1.05 so nothing spawns
        /// inside the portrait.
        pub rx: f64,
        pub ry: f64,
        /// Radians per second; signed, so the swarm counter-rotates.
        pub speed: f64,
        /// Where on the orbit it starts.
        pub phase: f64,
        /// The whole ellipse is rotated by this much — a hundred aligned
        /// ellipses read as a badge, tilted ones read as a swarm.
        pub tilt: f64,
        /// Core radius in device pixels at dpr 1.
        pub size: f64,
        /// Offset from the accent hue. Mostly small; every ninth orb burns
        /// hot crimson — the Terminator note in an otherwise cyan swarm.
        pub hue_off: i16,
    }

    /// xorshift64* — the same eight lines the burst layer scatters with.
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

    /// Deterministic swarm: the same seed always burns the same way, which
    /// is what makes this testable — and two portraits still differ, because
    /// the seed is the accent hue.
    pub fn seed_orbs(count: usize, seed: u64) -> Vec<Orb> {
        let mut rng = Rng::new(seed);
        (0..count)
            .map(|i| {
                let rx = 1.05 + rng.next() * 1.15;
                Orb {
                    rx,
                    // Flatter than round: the swarm reads as a disc seen at an
                    // angle, which is most of the "3D" for one multiply.
                    ry: rx * (0.35 + rng.next() * 0.4),
                    speed: (0.25 + rng.next() * 0.9) * if i % 2 == 0 { 1.0 } else { -1.0 },
                    phase: rng.next() * std::f64::consts::TAU,
                    tilt: rng.next() * std::f64::consts::TAU,
                    size: 0.8 + rng.next() * 1.9,
                    hue_off: if i % 9 == 8 {
                        // Crimson: roughly 170° away from the cyan home hue.
                        170
                    } else {
                        (rng.next() * 30.0 - 15.0) as i16
                    },
                }
            })
            .collect()
    }

    /// Where an orb is at time `t` (seconds), in portrait-radius units
    /// centred on the portrait. The third value is depth in [-1, 1]:
    /// positive is nearer the viewer — the renderer brightens and enlarges
    /// with it, which is what sells the orbit as circling *around* rather
    /// than sliding *over*.
    pub fn orbit_at(orb: &Orb, t: f64) -> (f64, f64, f64) {
        let a = orb.phase + orb.speed * t;
        let (x, y) = (a.cos() * orb.rx, a.sin() * orb.ry);
        let (s, c) = orb.tilt.sin_cos();
        (x * c - y * s, x * s + y * c, a.sin())
    }
}

// ---------------------------------------------------------------- component --

/// Mount exactly once, near the app root, beside the burst layer.
#[function_component(SpotlightLayer)]
pub fn spotlight_layer() -> Html {
    let store = use_store();
    let spot = use_state(|| Option::<Spot>::None);
    let canvas_ref = use_node_ref();
    let tilt_ref = use_node_ref();

    let reduced = web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches());

    // The inbox.
    {
        let spot = spot.clone();
        use_effect_with((), move |_| {
            let cb = Callback::from(move |op: Op| match op {
                Op::Open(s) => spot.set(Some(s)),
                Op::Close => spot.set(None),
            });
            EMIT.with(|e| *e.borrow_mut() = Some(cb));
            || EMIT.with(|e| *e.borrow_mut() = None)
        });
    }

    // Escape closes, while open.
    {
        use_effect_with(spot.is_some(), move |open| {
            let listener = open.then(|| {
                gloo_events::EventListener::new(
                    &web_sys::window().expect("a browser window"),
                    "keydown",
                    move |e| {
                        if let Some(e) = e.dyn_ref::<web_sys::KeyboardEvent>() {
                            if e.key() == "Escape" {
                                send(Op::Close);
                            }
                        }
                    },
                )
            });
            move || drop(listener)
        });
    }

    // The particle engine: one rAF loop for the life of the spotlight.
    {
        let canvas_ref = canvas_ref.clone();
        let key = spot.as_ref().map(|s| s.hue);
        use_effect_with(key, move |hue| {
            let cancelled = Rc::new(Cell::new(false));
            if let Some(hue) = *hue {
                if !reduced {
                    run_swarm(canvas_ref, hue, cancelled.clone());
                }
                // The arrival discharge, from the centre of the stage.
                super::burst::burst_from_selector(".fn-spot__img", super::burst::Variant::Pop, 14);
            }
            move || cancelled.set(true)
        });
    }

    let Some(s) = (*spot).clone() else {
        return Html::default();
    };

    let close = Callback::from(|_: MouseEvent| send(Op::Close));

    // The 3D lean: the stage looks at the pointer. Written straight onto the
    // node's style attribute — a re-render per mousemove would be absurd.
    let on_move = {
        let tilt_ref = tilt_ref.clone();
        Callback::from(move |e: MouseEvent| {
            let Some(el) = tilt_ref.cast::<web_sys::Element>() else {
                return;
            };
            let (w, h) = viewport();
            let nx = f64::from(e.client_x()) / w.max(1.0) - 0.5;
            let ny = f64::from(e.client_y()) / h.max(1.0) - 0.5;
            let _ = el.set_attribute(
                "style",
                &format!(
                    "transform: rotateX({:.2}deg) rotateY({:.2}deg)",
                    -ny * 12.0,
                    nx * 16.0
                ),
            );
        })
    };

    let copy = s.copy.clone().map(|value| {
        let store = store.clone();
        Callback::from(move |e: MouseEvent| {
            // The stage stays up — the scrim's close handler must not see
            // this click.
            e.stop_propagation();
            if super::common::copy_to_clipboard(&value) {
                toast::success(&store, t(store.language, Key::address_copied));
            }
        })
    });

    html! {
        <div
            class="fn-spot"
            role="dialog"
            aria-modal="true"
            aria-label={s.title.clone()}
            style={format!("--spot-h:{}", s.hue)}
            onclick={close.clone()}
            onmousemove={on_move}
        >
            <canvas class="fn-spot__fx" ref={canvas_ref} aria-hidden="true"></canvas>
            <div class="fn-spot__tilt" ref={tilt_ref}>
                <figure class="fn-spot__stage">
                    if !reduced {
                        <i class="fn-spot__ring fn-spot__ring--a" aria-hidden="true"></i>
                        <i class="fn-spot__ring fn-spot__ring--b" aria-hidden="true"></i>
                    }
                    <img class="fn-spot__img" src={s.image.clone()} alt="" />
                    <figcaption class="fn-spot__caption">
                        <strong class="fn-spot__title">{ &s.title }</strong>
                        if let Some(sub) = &s.subtitle {
                            <span class="fn-spot__sub fn-nums">{ sub }</span>
                        }
                        if let Some(copy) = copy {
                            <button type="button" class="topcoat-button" onclick={copy}>
                                { t(store.language, Key::copy_address) }
                            </button>
                        }
                    </figcaption>
                </figure>
            </div>
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-spot__close"
                aria-label={t(store.language, Key::close)}
                onclick={close}
            >
                { super::icons::close(20) }
            </button>
        </div>
    }
}

fn viewport() -> (f64, f64) {
    let w = web_sys::window().expect("a browser window");
    (
        w.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1.0),
        w.inner_height()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(1.0),
    )
}

/// Drive the swarm until `cancelled` flips. The canvas is sized once per
/// open — a spotlight outliving a window resize just letterboxes, which for
/// a three-second moment is the right trade against a resize observer.
fn run_swarm(canvas_ref: NodeRef, hue: u16, cancelled: Rc<Cell<bool>>) {
    let Some(canvas) = canvas_ref.cast::<web_sys::HtmlCanvasElement>() else {
        return;
    };
    let Ok(Some(ctx)) = canvas.get_context("2d") else {
        return;
    };
    let Ok(ctx) = ctx.dyn_into::<web_sys::CanvasRenderingContext2d>() else {
        return;
    };

    let (vw, vh) = viewport();
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
        .min(2.0);
    canvas.set_width((vw * dpr) as u32);
    canvas.set_height((vh * dpr) as u32);

    let orbs = fx::seed_orbs(96, u64::from(hue) + 7);
    let (cx, cy) = (vw * dpr / 2.0, vh * dpr / 2.0);
    // The portrait renders at min(56vmin, 420px); its radius anchors orbit
    // scale. Read the constant, not the element — the element is still
    // zooming when the first frames draw.
    let scale = (vw.min(vh) * 0.28).min(210.0) * dpr;
    let start = js_sys::Date::now();

    // The self-rescheduling rAF closure, same two-handle shape as the GL
    // backdrop.
    type FrameHook = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;
    let inner: FrameHook = Rc::new(RefCell::new(None));
    let outer = inner.clone();

    *outer.borrow_mut() = Some(Closure::new(move || {
        if cancelled.get() {
            *inner.borrow_mut() = None;
            return;
        }
        let t = (js_sys::Date::now() - start) / 1000.0;

        // Fade what was there toward transparent: this is the light trail.
        ctx.set_global_composite_operation("destination-out").ok();
        ctx.set_fill_style_str("rgba(0, 0, 0, 0.13)");
        ctx.fill_rect(0.0, 0.0, cx * 2.0, cy * 2.0);

        // Additive sparks on top.
        ctx.set_global_composite_operation("lighter").ok();
        for orb in &orbs {
            let (x, y, depth) = fx::orbit_at(orb, t);
            let px = cx + x * scale;
            let py = cy + y * scale;
            // Near passes burn brighter and bigger; far passes recede.
            let near = 0.55 + 0.45 * depth;
            let r = orb.size * dpr * (0.6 + 0.5 * near);
            let hue = i32::from(hue) + i32::from(orb.hue_off);
            ctx.begin_path();
            let _ = ctx.arc(px, py, r * 2.2, 0.0, std::f64::consts::TAU);
            ctx.set_fill_style_str(&format!("hsl({hue} 95% 60% / {:.3})", 0.10 * near));
            ctx.fill();
            ctx.begin_path();
            let _ = ctx.arc(px, py, r, 0.0, std::f64::consts::TAU);
            ctx.set_fill_style_str(&format!("hsl({hue} 95% 72% / {:.3})", 0.75 * near));
            ctx.fill();
        }

        if let Some(w) = web_sys::window() {
            let _ = w
                .request_animation_frame(inner.borrow().as_ref().unwrap().as_ref().unchecked_ref());
        }
    }));

    if let Some(w) = web_sys::window() {
        let _ =
            w.request_animation_frame(outer.borrow().as_ref().unwrap().as_ref().unchecked_ref());
    }
}

#[cfg(test)]
mod tests {
    use super::fx::*;

    #[test]
    fn a_swarm_is_a_pure_function_of_its_seed() {
        assert_eq!(seed_orbs(96, 190), seed_orbs(96, 190));
        assert_ne!(seed_orbs(96, 190), seed_orbs(96, 25));
    }

    #[test]
    fn no_orb_spawns_inside_the_portrait() {
        // rx/ry are in portrait-radius units; anything under 1.0 would orbit
        // through the face.
        for orb in seed_orbs(96, 190) {
            assert!(orb.rx >= 1.05, "rx {}", orb.rx);
            assert!(orb.ry >= 1.05 * 0.35, "ry {}", orb.ry);
        }
    }

    #[test]
    fn the_swarm_counter_rotates_and_carries_a_crimson_note() {
        let orbs = seed_orbs(96, 190);
        assert!(orbs.iter().any(|o| o.speed > 0.0));
        assert!(
            orbs.iter().any(|o| o.speed < 0.0),
            "all one direction reads as a badge"
        );
        let crimson = orbs.iter().filter(|o| o.hue_off == 170).count();
        assert_eq!(crimson, 96 / 9, "every ninth orb burns crimson");
        // The rest hold close to the accent hue.
        assert!(orbs
            .iter()
            .filter(|o| o.hue_off != 170)
            .all(|o| o.hue_off.abs() <= 15));
    }

    #[test]
    fn orbits_close_and_stay_bounded() {
        let orbs = seed_orbs(12, 190);
        for orb in &orbs {
            // Depth stays in [-1, 1] — the renderer multiplies brightness by
            // it and would over-drive the alpha otherwise.
            for step in 0..200 {
                let (x, y, depth) = orbit_at(orb, f64::from(step) * 0.05);
                assert!((-1.0..=1.0).contains(&depth));
                let reach = orb.rx.max(orb.ry);
                assert!(x.abs() <= reach + 1e-9 && y.abs() <= reach + 1e-9);
            }
            // One full period returns home: the orbit is closed, not a spiral.
            let period = std::f64::consts::TAU / orb.speed.abs();
            let (x0, y0, _) = orbit_at(orb, 0.0);
            let (x1, y1, _) = orbit_at(orb, period);
            assert!((x0 - x1).abs() < 1e-6 && (y0 - y1).abs() < 1e-6);
        }
    }
}
