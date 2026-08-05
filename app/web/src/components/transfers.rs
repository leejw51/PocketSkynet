//! The transfer rail: what a large upload looks like while it happens.
//!
//! Mounted once in the shell rather than inside the composer, because a 4 GB
//! upload outlives the screen that started it. Navigating from a room to
//! Settings unmounts the composer; if the bar lived there it would vanish
//! mid-transfer and the upload would appear to have stopped. The store holds
//! the state (`state::Transfer`), this only draws it.
//!
//! Two labels rather than one, because there are genuinely two passes. A file
//! is read once to checksum it — locally, before anything is sent — and again
//! to upload it. An unlabelled bar that fills, resets and fills again reads as
//! a bug; "Checking" then "Uploading" reads as what it is. See
//! `api/uploads.rs` for why the checksum is a separate pass at all.

use yew::prelude::*;

use crate::i18n::{t, Key};
use crate::state::{use_store, TransferDirection, TransferStage};

#[function_component(TransferRail)]
pub fn transfer_rail() -> Html {
    let store = use_store();
    let lang = store.language;

    if store.transfers.is_empty() {
        return Html::default();
    }

    html! {
        // `role="status"` and `aria-live="polite"`: progress is not an alert,
        // and a screen reader interrupting every chunk would be unusable. The
        // percentage is what gets announced, not the bar.
        <div class="fn-transfers" role="status" aria-live="polite">
            { for store.transfers.iter().map(|tr| {
                let pct = tr.percent();
                let label = match (tr.direction, tr.stage) {
                    (_, TransferStage::Checksum) if tr.direction == TransferDirection::Download =>
                        t(lang, Key::transfer_verifying),
                    (_, TransferStage::Checksum) => t(lang, Key::transfer_checksum),
                    (TransferDirection::Upload, _) => t(lang, Key::transfer_uploading),
                    (TransferDirection::Download, _) => t(lang, Key::transfer_verifying),
                };
                html! {
                    <div class="fn-transfer" key={tr.id}>
                        <div class="fn-transfer__row">
                            <span class="fn-transfer__name" title={tr.name.clone()}>
                                { tr.name.clone() }
                            </span>
                            <span class="fn-transfer__stage">{ label }</span>
                            <span class="fn-transfer__pct">{ format!("{pct}%") }</span>
                        </div>
                        <div
                            class="fn-transfer__track"
                            role="progressbar"
                            aria-valuemin="0"
                            aria-valuemax="100"
                            aria-valuenow={pct.to_string()}
                        >
                            // Width rather than a transform scale: the bar has a
                            // glow on its leading edge, and scaling would stretch
                            // that into a smear.
                            <i
                                class="fn-transfer__fill"
                                data-stage={match tr.stage {
                                    TransferStage::Checksum => "checksum",
                                    TransferStage::Moving => "moving",
                                }}
                                style={format!("width:{pct}%")}
                            />
                        </div>
                        <div class="fn-transfer__size">
                            { format!("{} / {}", human_bytes(tr.done), human_bytes(tr.total)) }
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
