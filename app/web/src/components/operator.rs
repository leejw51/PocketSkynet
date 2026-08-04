//! The operator's file — clearance, standing orders, the trophy wall, and
//! this server's ladder.
//!
//! Everything above the ladder is computed locally from
//! [`crate::progression`], which is why this screen paints instantly and
//! keeps working with the server down. The ladder is the one part that needs
//! the network, and it hides itself rather than erroring when the server has
//! never heard of the endpoint.

use pocketskynet_core::progression::{self, Measure, Tier, Trophy, TROPHIES};
use yew::prelude::*;

use crate::api::operators::{OperatorFile, OperatorReport};
use crate::i18n::{t, Key};
use crate::progression::Progression;
use crate::state::Store;

#[derive(Properties, PartialEq)]
pub struct OperatorProps {
    pub store: Store,
}

#[function_component(OperatorPage)]
pub fn operator_page(p: &OperatorProps) -> Html {
    let lang = p.store.language;
    let file = use_state(Progression::load_stored);
    let board = use_state(Vec::<OperatorFile>::new);
    let board_unavailable = use_state(|| false);

    // Report in, then read the board back. Both halves are best-effort: a
    // server that predates the endpoint answers 404, and the right response is
    // a hidden section, not an error on a screen that is otherwise local.
    {
        let store = p.store.clone();
        let file = file.clone();
        let board = board.clone();
        let board_unavailable = board_unavailable.clone();
        use_effect_with((), move |_| {
            wasm_bindgen_futures::spawn_local(async move {
                let snapshot = (*file).clone();
                let report = OperatorReport {
                    load: snapshot.load,
                    rank_level: snapshot.rank().level as i64,
                    streak: snapshot.streak,
                    orders: snapshot.orders_completed,
                    trophies: snapshot.trophies().len() as i64,
                };
                if store.client.report_operator(&report).await.is_err() {
                    board_unavailable.set(true);
                    return;
                }
                match store.client.leaderboard(25).await {
                    Ok(rows) => board.set(rows),
                    Err(_) => board_unavailable.set(true),
                }
            });
            || ()
        });
    }

    let rank = file.rank();
    let fraction = file.fraction();
    let snapshot = file.snapshot();
    let earned = file.trophies().len();
    let me = p
        .store
        .auth
        .address()
        .map(|a| a.as_str().to_lowercase())
        .unwrap_or_default();

    html! {
        <div class="fn-operator">
            // --- dossier ---------------------------------------------------
            <section class="fn-op-card fn-op-dossier">
                <div class="fn-op-emblem" data-rank={rank.level.to_string()}>
                    <span class="fn-op-level">{ rank.level }</span>
                </div>
                <h2 class="fn-op-designation">{ rank.designation }</h2>
                <p class="fn-op-mandate">{ rank.mandate }</p>

                <div class="fn-op-bar" role="progressbar"
                     aria-valuenow={((fraction * 100.0) as i64).to_string()}
                     aria-valuemin="0" aria-valuemax="100">
                    <div class="fn-op-bar-fill"
                         style={format!("width:{:.1}%", fraction * 100.0)} />
                </div>

                <dl class="fn-op-stats">
                    { stat(t(lang, Key::op_synaptic_load), file.load.to_string()) }
                    { stat(t(lang, Key::op_streak), format!("{}d", file.streak)) }
                    { stat(t(lang, Key::op_orders), file.orders_completed.to_string()) }
                    { stat(t(lang, Key::op_trophies), format!("{}/{}", earned, TROPHIES.len())) }
                </dl>
            </section>

            // --- standing orders -------------------------------------------
            <section class="fn-op-card fn-op-orders">
                <header class="fn-op-head">
                    <h3>{ t(lang, Key::op_standing_orders) }</h3>
                    <span class="fn-op-count">
                        { format!("{}/{}", file.completed_today(), file.today().len()) }
                    </span>
                </header>
                { for file.today().iter().map(|directive| {
                    let done = file.is_complete(directive);
                    let have = file.directive_progress(directive);
                    let pct = (have as f64 / directive.goal.max(1) as f64 * 100.0).min(100.0);
                    html! {
                        <div class={classes!("fn-op-order", done.then_some("is-done"))}>
                            <div class="fn-op-order-row">
                                <span class="fn-op-order-text">{ directive.order }</span>
                                <span class="fn-op-bounty">{ format!("+{}", directive.bounty) }</span>
                            </div>
                            <div class="fn-op-bar fn-op-bar--thin">
                                <div class="fn-op-bar-fill" style={format!("width:{pct:.1}%")} />
                            </div>
                            <span class="fn-op-order-count">
                                { format!("{}/{}", have, directive.goal) }
                            </span>
                        </div>
                    }
                }) }
                <p class="fn-op-note">{ t(lang, Key::op_reissued) }</p>
            </section>

            // --- trophy wall -----------------------------------------------
            <section class="fn-op-card fn-op-file">
                <header class="fn-op-head">
                    <h3>{ t(lang, Key::op_file) }</h3>
                    <span class="fn-op-count">{ format!("{}/{}", earned, TROPHIES.len()) }</span>
                </header>
                <div class="fn-op-trophies">
                    { for TROPHIES.iter().map(|trophy| {
                        let has = trophy.earned(&snapshot);
                        let pct = trophy.meter(&snapshot) * 100.0;
                        html! {
                            <div class={classes!("fn-op-trophy", has.then_some("is-earned"),
                                                 tier_class(trophy.tier))}
                                 title={if has { trophy.dossier } else { "" }}>
                                <span class="fn-op-trophy-name">
                                    { if has { trophy.name } else { t(lang, Key::op_classified) } }
                                </span>
                                if !has {
                                    <div class="fn-op-bar fn-op-bar--thin">
                                        <div class="fn-op-bar-fill" style={format!("width:{pct:.1}%")} />
                                    </div>
                                    <span class="fn-op-trophy-goal">
                                        { format!("{}/{}", trophy.progress(&snapshot), trophy.goal) }
                                    </span>
                                }
                            </div>
                        }
                    }) }
                </div>
            </section>

            // --- ladder -----------------------------------------------------
            if !*board_unavailable {
                <section class="fn-op-card fn-op-ladder">
                    <header class="fn-op-head">
                        <h3>{ t(lang, Key::op_ladder) }</h3>
                        <span class="fn-op-count">{ t(lang, Key::op_this_server) }</span>
                    </header>
                    if board.is_empty() {
                        <p class="fn-op-note">{ t(lang, Key::op_no_report) }</p>
                    } else {
                        { for board.iter().enumerate().map(|(index, entry)| {
                            let mine = entry.wallet_address.to_lowercase() == me;
                            let their_rank = progression::rank_for(entry.load);
                            html! {
                                <div class={classes!("fn-op-rung", mine.then_some("is-me"))}>
                                    <span class="fn-op-pos">{ index + 1 }</span>
                                    <span class="fn-op-who">
                                        <strong>{ &entry.username }</strong>
                                        <em>{ their_rank.designation }</em>
                                    </span>
                                    if entry.streak > 1 {
                                        <span class="fn-op-rung-streak">{ format!("{}d", entry.streak) }</span>
                                    }
                                    <span class="fn-op-rung-load">{ entry.load }</span>
                                </div>
                            }
                        }) }
                    }
                    <p class="fn-op-note">{ t(lang, Key::op_ladder_note) }</p>
                </section>
            }
        </div>
    }
}

fn stat(label: &'static str, value: String) -> Html {
    html! {
        <div class="fn-op-stat">
            <dd>{ value }</dd>
            <dt>{ label }</dt>
        </div>
    }
}

fn tier_class(tier: Tier) -> &'static str {
    match tier {
        Tier::Bronze => "tier-bronze",
        Tier::Silver => "tier-silver",
        Tier::Gold => "tier-gold",
        Tier::Machine => "tier-machine",
    }
}

/// Kept so the `Measure` import earns its place: the trophy list is data, and
/// a future screen that groups by what a trophy measures reads it from here.
#[allow(dead_code)]
fn measure_label(trophy: &Trophy) -> &'static str {
    match trophy.measure {
        Measure::Award(award) => award.citation(),
        Measure::Level => "RANK",
        Measure::Streak => "STREAK",
        Measure::Directives => "ORDERS",
    }
}
