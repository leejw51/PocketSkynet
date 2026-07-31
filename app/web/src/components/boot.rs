//! The sign-in boot sequence — the app's cold open (app.css §18.2).
//!
//! Signing in is the one moment where making the user wait is the *point*:
//! the credential is already verified before this plays, so the time buys
//! nothing technical and everything atmospheric. Four acts, ~4.7 seconds:
//!
//! 1. **Arrival** — a plasma sphere detonates into existence, throwing sparks.
//! 2. **Materialize** — it collapses, and the machine inside opens its eyes.
//! 3. **Boot log** — "SKYNET ACTIVATING" over a terminal roll-call.
//! 4. **Handoff** — a cyan wipe into the app.
//!
//! Every act is CSS keyframes sharing one timeline, sequenced purely by
//! per-element `animation-delay`. There is no phase state and no rAF loop: the
//! compositor runs the whole thing, and Rust owns exactly one timer — the one
//! that says it is over. That is also why [`TOTAL_MS`] has to be kept in step
//! with the CSS by hand; nothing enforces it, so the two are documented as one
//! timeline in app.css §18.2.
//!
//! Two escape hatches, because a cutscene you cannot leave is a bug: a SKIP
//! button, and `prefers-reduced-motion`, which collapses the whole sequence to
//! [`REDUCED_MS`] — long enough to read the title, short enough not to be
//! motion at all.

use crate::i18n::{t, Key};
use yew::prelude::*;

/// Full runtime of the sequence. Must match the CSS timeline in §18.2.
const TOTAL_MS: u32 = 4_700;

/// Runtime under `prefers-reduced-motion: reduce`. The §17 blanket rule has
/// already flattened the animations by the time this matters; without a
/// shorter timer the user would sit on a static frame for the full 4.7s.
const REDUCED_MS: u32 = 700;

/// Sparks thrown by the arrival, placed on a circle by index. Must divide 360
/// — the inline angle uses integer division, so a count that does not would
/// leave a visible wedge with no sparks in it.
const SPARKS: u32 = 18;

/// The boot roll-call. Rendered with a per-line stagger; the last line is
/// filled in with the operator's name, which is the point at which this stops
/// being a movie and starts being *their* account.
const LINES: [(&str, &str); 4] = [
    ("NEURAL NET PROCESSOR", "ONLINE"),
    ("CRYPTO CORE / AES-256", "ONLINE"),
    ("CHAIN LINK", "SYNCED"),
    ("OPERATOR", ""), // filled with the username
];

#[derive(Properties, PartialEq)]
pub struct BootProps {
    /// Shown as the operator on the last boot line.
    pub username: String,
    /// Fired exactly once, when the sequence is over or skipped.
    pub on_done: Callback<()>,
}

#[function_component(BootSequence)]
pub fn boot_sequence(p: &BootProps) -> Html {
    // Guards `on_done` to a single call: the skip button and the timer race,
    // and firing twice would dispatch the sign-in twice.
    let lang = crate::state::use_store().language;
    let fired = use_mut_ref(|| false);

    let finish = {
        let fired = fired.clone();
        let on_done = p.on_done.clone();
        Callback::from(move |_: ()| {
            if !*fired.borrow() {
                *fired.borrow_mut() = true;
                on_done.emit(());
            }
        })
    };

    {
        let finish = finish.clone();
        use_effect_with((), move |_| {
            let ms = if prefers_reduced_motion() {
                REDUCED_MS
            } else {
                TOTAL_MS
            };
            wasm_bindgen_futures::spawn_local(async move {
                sleep(ms).await;
                finish.emit(());
            });
            || ()
        });
    }

    let skip = {
        let finish = finish.clone();
        Callback::from(move |_: MouseEvent| finish.emit(()))
    };

    html! {
        // `role="status"` rather than `alert`: this is progress, not a problem.
        // The whole overlay is `aria-live` so a screen reader hears the boot
        // lines land instead of silently waiting out the animation.
        <div class="fn-boot" role="status" aria-live="polite">
            <div class="fn-boot__sky" aria-hidden="true" />

            <div class="fn-boot__stage" aria-hidden="true">
                <div class="fn-boot__sphere" />
                <div class="fn-boot__sparks">
                    { for (0..SPARKS).map(|i| html! {
                        <i style={format!(
                            "--a:{}deg;--d:{}ms;--r:{}px",
                            i * (360 / SPARKS),
                            40 * (i % 5),
                            120 + 26 * (i % 4),
                        )} />
                    }) }
                </div>
                <div class="fn-boot__skull" />
                <div class="fn-boot__bloom" />
            </div>

            <div class="fn-boot__flash" aria-hidden="true" />

            <div class="fn-boot__panel">
                <p class="fn-boot__title">{ "SKYNET ACTIVATING" }</p>
                <ul class="fn-boot__log">
                    { for LINES.iter().enumerate().map(|(i, (label, status))| {
                        let status = if status.is_empty() {
                            p.username.to_uppercase()
                        } else {
                            (*status).to_owned()
                        };
                        html! {
                            <li style={format!("--i:{i}")}>
                                <span class="fn-boot__label">{ *label }</span>
                                <span class="fn-boot__dots" aria-hidden="true" />
                                <span class="fn-boot__status">{ status }</span>
                            </li>
                        }
                    }) }
                </ul>
                <div class="fn-boot__bar" aria-hidden="true"><span /></div>
            </div>

            <button type="button" class="fn-boot__skip" onclick={skip}>
                { t(lang, Key::skip) }
            </button>
        </div>
    }
}

/// Whether the user asked the OS for less motion.
#[cfg(target_arch = "wasm32")]
fn prefers_reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches())
}

#[cfg(not(target_arch = "wasm32"))]
fn prefers_reduced_motion() -> bool {
    false
}

#[cfg(target_arch = "wasm32")]
async fn sleep(ms: u32) {
    gloo_timers::future::TimeoutFuture::new(ms).await;
}

/// Host builds never render, but the crate must link for `cargo test`.
#[cfg(not(target_arch = "wasm32"))]
async fn sleep(_ms: u32) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reduced_timeline_is_shorter_than_the_full_one() {
        // If these ever cross, reduced-motion users would wait *longer* on a
        // frozen frame than everyone else spends watching the animation.
        const { assert!(REDUCED_MS < TOTAL_MS) };
    }

    #[test]
    fn sparks_divide_the_circle_evenly() {
        // The inline `--a` uses integer division; a count that does not divide
        // 360 would leave a visible gap in the burst.
        assert_eq!(360 % SPARKS, 0);
    }

    #[test]
    fn the_last_boot_line_is_the_operator_slot() {
        // The empty status is the sentinel the renderer replaces with the
        // username — a non-empty one there would silently drop the name.
        let (label, status) = LINES[LINES.len() - 1];
        assert_eq!(label, "OPERATOR");
        assert!(status.is_empty());
        assert!(LINES[..LINES.len() - 1].iter().all(|(_, s)| !s.is_empty()));
    }
}
