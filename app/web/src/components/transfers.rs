//! The transfer rail: what a large upload looks like while it happens.
//!
//! Mounted once in the shell rather than inside the composer, because a 4 GB
//! upload outlives the screen that started it. Navigating from a room to
//! Settings unmounts the composer; if the bar lived there it would vanish
//! mid-transfer and the upload would appear to have stopped. The store holds
//! the state (`state::Transfer`), this only draws it.
//!
//! # Three things it must never do, all learned from a screenshot
//!
//! A phone showed a bar reading `28.0 MB / 28.0 MB`, frozen, sitting on top of
//! the bottom navigation, with no way to cancel it. Every part of that was a
//! separate failure of this component:
//!
//! * **It must not lie about being finished.** The checksum pass ends at 100%,
//!   and if the upload then hangs, a bare percentage says "done" while nothing
//!   is happening. The stage is now always visible next to the number, and a
//!   transfer that stops moving says so in as many words.
//! * **It must not be a dead end.** Anything that can hang needs a way out, so
//!   every row has a cancel control. Cancelling is safe: the session survives
//!   on the server and re-attaching the same file resumes it.
//! * **It must not sit on top of the app.** It clears the composer and the
//!   bottom navigation rather than covering them.

use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::{use_store, Action, TransferDirection, TransferStage};

/// How long a transfer may sit at the same byte count before it is called
/// stalled.
///
/// Long enough not to fire between chunks on a slow link — an 8 MB chunk over
/// a poor connection can legitimately take a while — and short enough that
/// nobody sits watching a frozen bar wondering whether to wait. The `finish`
/// call is the other legitimate pause: the server re-hashes the whole file, so
/// a 4 GB upload can sit at 100% for a few seconds by design, and this must
/// clear that comfortably.
const STALL_AFTER_MS: f64 = 20_000.0;

/// How often the stall check runs.
const TICK_MS: u32 = 2_000;

#[function_component(TransferRail)]
pub fn transfer_rail() -> Html {
    let store = use_store();
    let lang = store.language;

    // (id, bytes, when) for the row being watched, so "has it moved?" can be
    // answered without the store growing a timestamp that only this component
    // would ever read.
    let seen = use_mut_ref(Vec::<(u64, f64, f64)>::new);
    let tick = use_state(|| 0u32);

    {
        let tick = tick.clone();
        use_effect_with((), move |_| {
            let handle = gloo_timers::callback::Interval::new(TICK_MS, move || {
                tick.set(*tick + 1);
            });
            move || drop(handle)
        });
    }

    if store.transfers.is_empty() {
        // Nothing in flight: forget what was being watched, or a later transfer
        // that happens to reuse an id would inherit a stale timestamp and be
        // called stalled the moment it appeared.
        seen.borrow_mut().clear();
        return Html::default();
    }

    let now = js_sys::Date::now();
    let mut marks = seen.borrow_mut();
    marks.retain(|(id, _, _)| store.transfers.iter().any(|t| t.id == *id));

    html! {
        // `role="status"` and `aria-live="polite"`: progress is not an alert,
        // and a screen reader interrupting every chunk would be unusable.
        <div class="fn-transfers" role="status" aria-live="polite">
            { for store.transfers.iter().map(|tr| {
                let pct = tr.percent();

                let done = tr.stage == TransferStage::Done;

                // Has this row moved since it was last looked at? A finished
                // row is exempt: it is not waiting on anything, and "stalled"
                // over a full green bar would be nonsense.
                let stalled = !done && match marks.iter_mut().find(|(id, _, _)| *id == tr.id) {
                    Some(mark) => {
                        if (mark.1 - tr.done).abs() > f64::EPSILON {
                            mark.1 = tr.done;
                            mark.2 = now;
                            false
                        } else {
                            now - mark.2 > STALL_AFTER_MS
                        }
                    }
                    None => {
                        marks.push((tr.id, tr.done, now));
                        false
                    }
                };

                let label = match (tr.direction, tr.stage) {
                    (_, TransferStage::Done) => t(lang, Key::transfer_done),
                    (TransferDirection::Download, _) => t(lang, Key::transfer_downloading),
                    (_, TransferStage::Checksum) => t(lang, Key::transfer_checksum),
                    (TransferDirection::Upload, _) => t(lang, Key::transfer_uploading),
                };

                let cancel = {
                    let store = store.clone();
                    let id = tr.id;
                    Callback::from(move |_: MouseEvent| {
                        // Only takes the row off screen. The session is still on
                        // the server, so re-attaching the same file resumes it
                        // rather than starting again — which is precisely why
                        // offering cancel is safe.
                        store.dispatch(Action::TransferEnded(id));
                    })
                };

                html! {
                    <div
                        class="fn-transfer"
                        key={tr.id}
                        data-stalled={stalled.to_string()}
                        data-done={done.to_string()}
                    >
                        <div class="fn-transfer__head">
                            <span class="fn-transfer__name" title={tr.name.clone()}>
                                { tr.name.clone() }
                            </span>
                            <button
                                type="button"
                                class="fn-transfer__cancel"
                                aria-label={t(lang, Key::transfer_cancel)}
                                title={t(lang, Key::transfer_cancel)}
                                onclick={cancel}
                            >{ super::icons::close(14) }</button>
                        </div>

                        <div class="fn-transfer__meta">
                            // The stage sits *next to* the percentage on
                            // purpose. A bare "100%" is what made a hung
                            // checksum look like a finished upload.
                            <span class="fn-transfer__stage">
                                { if stalled { t(lang, Key::transfer_stalled) } else { label } }
                            </span>
                            <span class="fn-transfer__pct">{ format!("{pct}%") }</span>
                        </div>

                        <div
                            class="fn-transfer__track"
                            role="progressbar"
                            aria-valuemin="0"
                            aria-valuemax="100"
                            aria-valuenow={pct.to_string()}
                        >
                            <i
                                class="fn-transfer__fill"
                                data-stage={match tr.stage {
                                    TransferStage::Checksum => "checksum",
                                    TransferStage::Moving => "moving",
                                    TransferStage::Done => "done",
                                }}
                                style={format!("width:{pct}%")}
                            />
                        </div>

                        <div class="fn-transfer__size">
                            { format!("{} / {}", human_bytes(tr.done), human_bytes(tr.total)) }
                            if stalled {
                                <span class="fn-transfer__hint">
                                    { t(lang, Key::transfer_stalled_hint) }
                                </span>
                            }
                        </div>
                    </div>
                }
            }) }
        </div>
    }
}

/// Bytes as something a person can read at a glance.
///
/// Binary units, because that is what a file manager shows and a transfer that
/// disagrees with the operating system about how big the file is invites the
/// wrong kind of question.
pub fn human_bytes(n: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if !n.is_finite() || n <= 0.0 {
        return "0 B".to_owned();
    }
    let mut value = n;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    // Whole bytes are never fractional; everything else gets one decimal,
    // which is enough to see a bar move without the number jittering.
    if unit == 0 {
        format!("{:.0} {}", value, UNITS[unit])
    } else {
        format!("{:.1} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_read_the_way_a_file_manager_shows_them() {
        assert_eq!(human_bytes(0.0), "0 B");
        assert_eq!(human_bytes(512.0), "512 B");
        assert_eq!(human_bytes(1024.0), "1.0 KB");
        assert_eq!(human_bytes(1536.0), "1.5 KB");
        assert_eq!(human_bytes(1024.0 * 1024.0), "1.0 MB");
        assert_eq!(human_bytes(4.0 * 1024.0 * 1024.0 * 1024.0), "4.0 GB");
        // The scale tops out rather than indexing past the end of UNITS. The
        // number is absurd, which is the point — it must not panic.
        assert!(human_bytes(f64::MAX).ends_with(" TB"));
    }

    #[test]
    fn a_nonsense_size_does_not_render_as_nonsense() {
        // NaN and negatives can only come from a malformed response, and both
        // would otherwise print as "NaN B" next to a working progress bar.
        assert_eq!(human_bytes(f64::NAN), "0 B");
        assert_eq!(human_bytes(-1.0), "0 B");
        assert_eq!(human_bytes(f64::INFINITY), "0 B");
    }
}
