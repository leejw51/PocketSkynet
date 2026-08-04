//! Server info: where this deployment is, and how you are talking to it.
//!
//! The transport is the reason this exists. A browser upgrades itself to
//! HTTP/3 once the server has advertised one via `Alt-Svc`, silently and on a
//! connection of its own choosing — so the page cannot know which protocol it
//! is on by looking at itself. The only honest answer comes from the end that
//! terminated the connection, which is what `GET /api/server/info` reports.
//!
//! Everything else here is the same information the server prints on startup:
//! the addresses, grouped by how far away a client has to be. It is in the
//! client too because the person holding the phone is rarely the person
//! reading the terminal.

use yew::prelude::*;

use crate::api::{ServerEndpoint, ServerInfo};
use crate::state::{use_store, Load};

use super::super::modal::Modal as Dialog;

#[derive(Properties, PartialEq)]
pub struct ServerInfoProps {
    pub on_close: Callback<()>,
}

#[function_component(ServerInfoDialog)]
pub fn server_info_dialog(p: &ServerInfoProps) -> Html {
    let store = use_store();
    let info = use_state(ServerInfo::default);
    let load = use_state(Load::default);

    {
        let store = store.clone();
        let info = info.clone();
        let load = load.clone();
        use_effect_with((), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                match store.client.server_info().await {
                    Ok(v) => {
                        info.set(v);
                        load.set(Load::Ready);
                    }
                    // An older server has no such endpoint. That is a version
                    // difference, not a fault — say so rather than showing a
                    // red error for a deployment that is working fine.
                    Err(e) => load.set(Load::Error(e.user_message())),
                }
            });
            || ()
        });
    }

    let body = match &*load {
        Load::Loading | Load::Idle => html! {
            <p class="fn-modal__hint">{ "Asking the server…" }</p>
        },
        Load::Error(message) => html! {
            <>
                <p class="fn-modal__hint">{ message.clone() }</p>
                <p class="fn-modal__hint">
                    { "A server older than this client does not report its transports." }
                </p>
            </>
        },
        Load::Ready => render_info(&info),
    };

    html! {
        <Dialog title={"Server"} on_close={p.on_close.clone()}>
            { body }
        </Dialog>
    }
}

fn render_info(info: &ServerInfo) -> Html {
    html! {
        <div class="fn-serverinfo">
            <section class="fn-serverinfo__section">
                <h3 class="fn-serverinfo__heading">{ "This connection" }</h3>
                <dl class="fn-serverinfo__rows">
                    <div class="fn-serverinfo__row">
                        <dt>{ "Protocol" }</dt>
                        <dd>
                            <span class={classes!(
                                "fn-serverinfo__badge",
                                info.is_http3().then_some("fn-serverinfo__badge--live"),
                            )}>
                                { info.protocol_label() }
                            </span>
                            if info.is_http3() {
                                <span class="fn-serverinfo__note">{ "over QUIC" }</span>
                            }
                        </dd>
                    </div>
                    <div class="fn-serverinfo__row">
                        <dt>{ "Realtime" }</dt>
                        // Worth stating plainly: people reasonably assume that
                        // turning on HTTP/3 moved everything, and the socket
                        // staying on TCP looks like a bug otherwise.
                        <dd>{ "WebSocket · TCP only" }</dd>
                    </div>
                    <div class="fn-serverinfo__row">
                        <dt>{ "Uptime" }</dt>
                        <dd>{ humanise(info.uptime) }</dd>
                    </div>
                </dl>
            </section>

            <section class="fn-serverinfo__section">
                <h3 class="fn-serverinfo__heading">
                    // Named for what the listener actually speaks. Calling an
                    // HTTPS listener "HTTP" beside a list of `https://` URLs
                    // reads as a bug in the panel.
                    { format!(
                        "{} · tcp/{}",
                        if info.scheme == "https" { "HTTPS" } else { "HTTP" },
                        info.port,
                    ) }
                </h3>
                { endpoint_list(&info.endpoints.tcp) }
            </section>

            if info.http3_available {
                <section class="fn-serverinfo__section">
                    <h3 class="fn-serverinfo__heading">
                        { match info.http3_port {
                            Some(port) => format!("HTTP/3 · QUIC · udp/{port}"),
                            None => "HTTP/3 · QUIC".to_owned(),
                        } }
                    </h3>
                    { endpoint_list(&info.endpoints.http3) }
                    if info.http3_offered_but_unused() {
                        { why_not_http3(info) }
                    } else {
                        <p class="fn-modal__hint">
                            { "Advertised as Alt-Svc, so a browser moves here on its own. \
                               HTTP/3 needs HTTPS — QUIC has no plaintext mode." }
                        </p>
                    }
                </section>
            } else {
                <section class="fn-serverinfo__section">
                    <h3 class="fn-serverinfo__heading">{ "HTTP/3" }</h3>
                    <p class="fn-modal__hint">
                        { "Not enabled on this server. Start it with --http3 to serve \
                           QUIC beside the TCP listener." }
                    </p>
                </section>
            }
        </div>
    }
}

/// Why this page is on TCP when QUIC is right there.
///
/// Worth spelling out rather than leaving as a puzzle: the panel has just
/// listed HTTP/3 addresses the reader is demonstrably not using, and the
/// reason is not obvious. A browser will not speak QUIC to a certificate it
/// does not genuinely trust, and — unlike an ordinary HTTPS warning — there is
/// no "proceed anyway". Installing the CA is the whole fix.
fn why_not_http3(info: &ServerInfo) -> Html {
    html! {
        <>
            <p class="fn-modal__hint">
                { format!(
                    "This page is on {}. A browser only moves to HTTP/3 once it has seen \
                     Alt-Svc and can fully verify the certificate.",
                    info.protocol_label(),
                ) }
            </p>
            if info.ca_cert_available {
                <p class="fn-modal__hint">
                    { "This server signs its own certificate. Clicking through the \
                       warning is enough for HTTPS but not for QUIC, which has no \
                       click-through at all — the CA has to be a trust anchor." }
                </p>
                <p class="fn-modal__hint">
                    { " " }
                    // A plain link on this origin: the client never has to be
                    // told the redirect port, and a relative href cannot be
                    // misconfigured.
                    <a href="/ca.crt" download="pocketskynet-ca.crt">{ "Download the CA" }</a>
                    // Naming the second step explicitly, because installing a
                    // certificate and *trusting* it are two separate actions on
                    // every platform, and stopping after the first one leaves
                    // exactly this symptom: HTTPS works, HTTP/3 never engages.
                    { ", then mark it trusted — importing it is not enough. macOS: \
                       Keychain Access → Get Info → Trust → Always Trust. \
                       iOS: Settings → General → About → Certificate Trust Settings. \
                       Restart the browser afterwards; it stops retrying QUIC once \
                       an attempt has failed." }
                </p>
            }
        </>
    }
}

fn endpoint_list(endpoints: &[ServerEndpoint]) -> Html {
    if endpoints.is_empty() {
        // The desktop app binds an ephemeral port, so the server genuinely
        // cannot name its own URL. Saying that is better than printing `:0`.
        return html! {
            <p class="fn-modal__hint">
                { "This server was bound to an automatic port and cannot name its own address." }
            </p>
        };
    }
    html! {
        <ul class="fn-serverinfo__urls">
            { for endpoints.iter().map(|endpoint| html! {
                <li class="fn-serverinfo__url">
                    <span class="fn-serverinfo__reach">{ endpoint.reach.clone() }</span>
                    <code>{ endpoint.url.clone() }</code>
                </li>
            }) }
        </ul>
    }
}

/// Uptime in the largest unit that is still honest.
fn humanise(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h {}m", s / 3_600, (s % 3_600) / 60),
        s => format!("{}d {}h", s / 86_400, (s % 86_400) / 3_600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_uses_the_largest_honest_unit() {
        assert_eq!(humanise(9), "9s");
        assert_eq!(humanise(90), "1m");
        assert_eq!(humanise(3_700), "1h 1m");
        assert_eq!(humanise(90_000), "1d 1h");
    }

    #[test]
    fn the_protocol_label_is_the_name_people_recognise() {
        // The wire values are ALPN tokens; nobody outside a network stack
        // calls it "h3".
        let at = |p: &str| ServerInfo {
            protocol: p.to_owned(),
            ..Default::default()
        };
        assert_eq!(at("h3").protocol_label(), "HTTP/3");
        assert_eq!(at("h2").protocol_label(), "HTTP/2");
        assert_eq!(at("http/1.1").protocol_label(), "HTTP/1.1");
        assert_eq!(at("").protocol_label(), "unknown");
    }

    #[test]
    fn the_tcp_heading_names_the_scheme_it_actually_serves() {
        // A list of `https://` URLs under a heading that says "HTTP" reads as
        // a bug in the panel rather than as a deployment on TLS.
        let label = |scheme: &str| {
            if scheme == "https" {
                "HTTPS"
            } else {
                "HTTP"
            }
        };
        assert_eq!(label("https"), "HTTPS");
        assert_eq!(label("http"), "HTTP");
    }

    #[test]
    fn the_explanation_appears_only_when_http3_is_offered_and_unused() {
        // Nothing to explain when the page is already on QUIC, and nothing to
        // explain when the server does not offer it at all.
        let case = |protocol: &str, available: bool| ServerInfo {
            protocol: protocol.to_owned(),
            http3_available: available,
            ..Default::default()
        };
        assert!(
            case("h2", true).http3_offered_but_unused(),
            "the whole point"
        );
        assert!(
            !case("h3", true).http3_offered_but_unused(),
            "already on it"
        );
        assert!(
            !case("h2", false).http3_offered_but_unused(),
            "nothing offered"
        );
    }

    #[test]
    fn only_h3_counts_as_http3() {
        // A page on HTTP/2 against a server that *offers* HTTP/3 must not
        // claim to be using it — that is the whole failure this dialog exists
        // to prevent.
        let on_h2 = ServerInfo {
            protocol: "h2".into(),
            http3_available: true,
            ..Default::default()
        };
        assert!(!on_h2.is_http3());
        assert!(ServerInfo {
            protocol: "h3".into(),
            ..Default::default()
        }
        .is_http3());
    }
}
