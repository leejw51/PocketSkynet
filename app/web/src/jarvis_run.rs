//! The "My Jarvis" agent: the half that touches the world.
//!
//! [`crate::jarvis`] decides what to say and what the agent is allowed to
//! reach for; this module actually reaches. The split is the one
//! `bank_agent.rs` / `components/banker.rs` already draws, and the loop below
//! is deliberately the Banker's loop: one tool per reply, the result fed back
//! as `[TOOL RESULT <name>]`, capped at [`jarvis::MAX_TOOL_ROUNDS`].
//!
//! # The rule that shapes every tool in here
//!
//! **A tool result is sent to the provider on the next turn.** Whatever a tool
//! returns has left the device by the time the model reads it. Three
//! consequences run through this file:
//!
//! * **Secrets return receipts, not values.** `vault_copy` puts a password on
//!   the clipboard and tells the model "copied" — the plaintext is opened,
//!   handed to the clipboard and dropped inside one function, and never
//!   becomes a `String` the transcript can see. This is what lets
//!   `crate::secrets`' single-caller contract survive contact with an LLM.
//! * **Results are untrusted input.** `search_rooms` returns text other people
//!   wrote. It arrives looking exactly like the owner's own words, so every
//!   tool that *writes* stops at [`Confirm`] — a dialog the model cannot issue
//!   for itself. A successful prompt injection can therefore ask to send a
//!   message; it cannot send one.
//! * **Gates are checked here, not just advertised there.** The prompt omits
//!   tools this session cannot run, but a model can name one anyway, so
//!   [`exec_tool`] asks [`jarvis::is_available`] before dispatching rather than
//!   trusting that an unadvertised tool is an unreachable one.

use std::cell::RefCell;
use std::rc::Rc;

use pocketskynet_core::chain::format_amount;
use pocketskynet_core::{Network, RoomId, WalletAddress};
use serde_json::Value;
use yew::Callback;

use crate::ai::{self, ChatTurn};
use crate::bank_agent as banker;
use crate::crypto;
use crate::i18n::Lang;
use crate::jarvis::{self, Caps};
use crate::rpc::EvmRpc;
use crate::state::{Action, Store};

/// How many rooms `search_rooms` will pull from the server when it has never
/// opened them.
///
/// "Search everything" has to mean everything, and a room this tab has not
/// opened has no messages in memory at all — so the honest version of the
/// feature fetches. The cap is what stops a hundred-room account turning one
/// question into a hundred requests; when it bites, the result says so rather
/// than quietly reporting a smaller world.
const SEARCH_FETCH_ROOMS: usize = 24;

/// How many messages a fetched room contributes to a search.
const SEARCH_FETCH_DEPTH: usize = 100;

/// The longest single tool result worth sending back.
///
/// A search across a year of rooms can produce more text than the context
/// window holds, and the failure mode is not a truncated answer but a rejected
/// request. Cut here, visibly, rather than letting the provider decide.
const MAX_RESULT: usize = 6_000;

/// How many characters of any one matched message ride along in a hit.
const SNIPPET: usize = 240;

/// The password `vault_save` mints when it is not told a length.
const DEFAULT_SECRET_LEN: usize = 24;

/// The alphabet for a generated password: unambiguous, and every character
/// survives a copy-paste through a terminal or a form that trims punctuation
/// oddly. No `O`/`0`/`l`/`1`/`I` — a password nobody can read back over the
/// phone is one that gets written down.
const SECRET_ALPHABET: &[u8] =
    b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789!@#%^&*-_=+";

// ------------------------------------------------------------------ the UI --

/// Something that changes the world, waiting on the owner.
///
/// The decision cell is polled by the paused tool future — the same
/// dependency-free promise bridge `components/banker.rs::ask_approval` uses,
/// and for the same reason: a `oneshot` would mean threading a channel through
/// every tool signature to serve three of them.
#[derive(Clone, Debug)]
pub struct Confirm {
    pub title: String,
    /// Label/value pairs shown as a small table — what exactly is about to
    /// happen, in the user's own terms rather than the model's.
    pub lines: Vec<(String, String)>,
    pub decision: Rc<RefCell<Option<bool>>>,
}

impl PartialEq for Confirm {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.decision, &other.decision)
    }
}

/// The chat's half of a running turn: what to draw while it works, and how to
/// ask a question that only a person can answer.
#[derive(Clone, PartialEq)]
pub struct Ui {
    /// `Some(tool name)` while a tool runs, `None` while the model thinks.
    /// Drives the activity line under the composer.
    pub stage: Callback<Option<String>>,
    pub confirm: Callback<Confirm>,
    /// Whether the owner has switched vault access on **for this session**.
    /// Not persisted anywhere: a consent that survives a reload is one nobody
    /// remembers giving.
    pub vault_consent: bool,
}

async fn ask(ui: &Ui, title: String, lines: Vec<(String, String)>) -> bool {
    let decision = Rc::new(RefCell::new(None));
    ui.confirm.emit(Confirm {
        title,
        lines,
        decision: decision.clone(),
    });
    loop {
        if let Some(v) = *decision.borrow() {
            return v;
        }
        gloo_timers::future::TimeoutFuture::new(120).await;
    }
}

// ------------------------------------------------------------------- deps --

#[derive(Clone)]
struct Deps {
    store: Store,
    /// The Jarvis room itself — where the answer lands, and the one room a
    /// tool never has to be told the name of.
    room_id: RoomId,
    me: WalletAddress,
    lang: Lang,
    caps: Caps,
    provider: ai::Provider,
    key: String,
    image: Option<(ai::Provider, String)>,
    ui: Ui,
}

impl Deps {
    fn note_room(&self) -> RoomId {
        RoomId::new(&crate::rooms::static_room_id(
            crate::rooms::StaticRoom::Note.kind(),
            &self.me,
        ))
        .expect("a derived static room id is always well-formed")
    }

    fn network(&self) -> Option<Network> {
        let want = crate::components::bank::load_bank_chain();
        let nets = crate::components::bank::bank_networks();
        nets.iter()
            .find(|n| n.chain_id == Some(want))
            .or_else(|| nets.first())
            .cloned()
    }
}

// ------------------------------------------------------------------ entry --

/// Answer one question in My Jarvis.
///
/// Replaces the single-shot call this used to be: the model may now spend up
/// to [`jarvis::MAX_TOOL_ROUNDS`] tool calls before it answers, and the answer
/// is posted exactly as before — sealed under the room's epoch, written by the
/// server under the agent's address because a browser cannot claim a sender
/// that is not its wallet.
///
/// `question` is passed in rather than read back off the store because the
/// store handle this task captured is a snapshot taken *before* the message
/// was dispatched. That was a real bug, fixed once already; the parameter is
/// the fix.
pub async fn reply(store: Store, room_id: RoomId, question: String, ui: Ui) {
    let settings = ai::AiSettings::load();
    let (Some(provider), Some(me)) = (settings.text_provider(), store.me().cloned()) else {
        return;
    };
    let key = settings.key_for(provider).unwrap_or_default().to_owned();
    let image = settings
        .image_provider()
        .and_then(|p| settings.key_for(p).map(|k| (p, k.to_owned())));

    // A capability is "can this actually run", never "did the model ask
    // nicely". Vault needs both halves: an unlocked session *and* consent.
    let caps = Caps {
        vault: ui.vault_consent && store.auth.can_decrypt(),
        chain: store
            .auth
            .session()
            .map(|s| s.keys.borrow().can_sign_locally())
            .unwrap_or(false),
        image: image.is_some(),
    };

    let d = Deps {
        store: store.clone(),
        room_id: room_id.clone(),
        me: me.clone(),
        lang: store.language,
        caps,
        provider,
        key,
        image,
        ui: ui.clone(),
    };

    let mut lines = transcript(&store, &room_id, &me);
    // The just-sent question, which the snapshot above cannot see. Appended
    // only when the snapshot does not already end with it, so a stray
    // re-render that *did* capture it cannot make the model read it twice.
    if lines
        .last()
        .map(|l| l.from_agent || l.text != question)
        .unwrap_or(true)
    {
        lines.push(jarvis::RoomLine {
            from_agent: false,
            text: question,
        });
    }

    let turns = jarvis::turns(&lines);
    if turns.last().map(|t| !t.user).unwrap_or(true) {
        return;
    }

    // My Note goes in whole, on every question. It is bounded (jarvis::NOTE_BUDGET)
    // precisely so that it can, which is what makes "Jarvis knows what I wrote
    // down" true without the model having to decide to go and look.
    let note = note_text(&d).await;

    let system = jarvis::system_prompt(&jarvis::AgentContext {
        owner: store
            .room(&room_id)
            .and_then(|r| r.members.iter().find(|m| m.user_address == me))
            .map(|m| m.user.display_name())
            .unwrap_or_else(|| me.abbreviated()),
        lang: store.language,
        now: local_now(),
        caps,
        note,
    });

    let Some(text) = run_turn(&d, &system, turns).await else {
        return;
    };
    post_agent_message(&d, &text).await;
}

/// The tool loop.
///
/// Returns the text to post, or `None` when there is nothing worth posting —
/// a provider error (already logged), or a model that answered with silence.
async fn run_turn(d: &Deps, system: &str, mut turns: Vec<ChatTurn>) -> Option<String> {
    for _ in 0..jarvis::MAX_TOOL_ROUNDS {
        d.ui.stage.emit(None);
        let raw = match ai::generate_chat(d.provider, &d.key, system, &turns).await {
            Ok(r) => r,
            Err(e) => {
                web_sys::console::warn_1(&format!("jarvis: {e}").into());
                return None;
            }
        };
        match banker::parse_reply(&raw) {
            banker::Reply::Text(text) => return jarvis::reply_to_post(&text),
            banker::Reply::Tool { name, args } => {
                d.ui.stage.emit(Some(name.clone()));
                let result = match exec_tool(d, &name, &args).await {
                    Ok(s) => s,
                    Err(e) => format!("ERROR: {e}"),
                };
                let result = clamp(&result, MAX_RESULT);
                // The model's own JSON goes back as the assistant turn it was,
                // so the next round sees what it asked for as well as what it
                // got. Dropping it makes the result look unprompted.
                turns.push(ChatTurn {
                    user: false,
                    content: raw,
                });
                turns.push(ChatTurn {
                    user: true,
                    content: format!("[TOOL RESULT {name}] {result}"),
                });
            }
        }
    }
    // Eight tool calls without an answer is a loop, not a hard question.
    // Saying so is better than a ninth request or an empty room.
    Some(crate::i18n::t(d.lang, crate::i18n::Key::jarvis_out_of_steam).to_owned())
}

// ------------------------------------------------------------------ tools --

/// Whether this module dispatches `name`.
///
/// The other half of the drift guard in `jarvis.rs`: that file tests every
/// tool is *documented*, this one tests every tool is *dispatched*. A name in
/// [`jarvis::TOOLS`] missing here is a promise the model keeps trying to
/// collect on.
pub fn handles(name: &str) -> bool {
    matches!(
        name,
        "get_time"
            | "get_location"
            | "get_device"
            | "search_all"
            | "search_server"
            | "search_rooms"
            | "search_people"
            | "append_note"
            | "list_rooms"
            | "read_room"
            | "send_message"
            | "get_native_balance"
            | "get_token_balance"
            | "get_gas_price"
            | "list_tokens"
            | "generate_image"
            | "vault_find"
            | "vault_copy"
            | "vault_save"
    )
}

/// Run one tool. `Ok` strings are fed to the model verbatim; `Err` becomes an
/// `ERROR: …` result — also fed to the model, never thrown at the UI, because
/// a model that can read the failure can explain it.
async fn exec_tool(d: &Deps, name: &str, args: &Value) -> Result<String, String> {
    // Availability is re-checked here rather than assumed from the prompt: a
    // model can name a tool it was never offered, and for the vault gate the
    // difference between "not advertised" and "not permitted" is the whole
    // security property.
    if !jarvis::is_available(&d.caps, name) {
        let safe = banker::sanitize_onchain_text(name, 40);
        // "Withheld" and "imaginary" are different problems and deserve
        // different answers: one is worth telling the owner about ("switch the
        // vault on and ask me again"), the other is the model having invented
        // a capability and should simply stop.
        return Err(if handles(name) {
            format!("\"{safe}\" is not available in this session — tell the owner what you were trying to do and why it needs turning on")
        } else {
            format!("no such tool \"{safe}\"")
        });
    }

    match name {
        // -- knowing ------------------------------------------------------
        "get_time" => Ok(local_now()),
        "get_location" => locate().await,
        "get_device" => Ok(device_line(d)),

        // -- searching ----------------------------------------------------
        "search_all" => {
            let q = banker::arg_str(args, "query")?;
            let server = search_server(d, &q, None).await.unwrap_or_else(|e| e);
            let rooms = search_rooms(d, &q).await.unwrap_or_else(|e| e);
            Ok(format!(
                "FROM THE SERVER INDEX (plaintext content only)\n{server}\n\nFROM ENCRYPTED ROOMS ON THIS DEVICE\n{rooms}"
            ))
        }
        "search_server" => {
            let q = banker::arg_str(args, "query")?;
            search_server(d, &q, banker::arg_str_opt(args, "kind").as_deref()).await
        }
        "search_rooms" => {
            let q = banker::arg_str(args, "query")?;
            search_rooms(d, &q).await
        }
        "search_people" => {
            let q = banker::arg_str(args, "query")?;
            let found = d
                .store
                .client
                .search_users(&q)
                .await
                .map_err(|e| e.user_message())?;
            if found.is_empty() {
                return Ok("nobody matched.".into());
            }
            Ok(found
                .iter()
                .take(20)
                .map(|u| {
                    format!(
                        "- {} ({})",
                        u.display_name(),
                        u.wallet_address.abbreviated()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }

        // -- notes --------------------------------------------------------
        "append_note" => {
            let text = banker::arg_str(args, "text")?;
            let note = d.note_room();
            // The note is bounded because all of it rides in every prompt.
            // Refusing here, with the numbers, lets the model tell the owner
            // what to delete rather than silently writing a line that pushes
            // the oldest one out of its own context.
            let used = note_text(d).await.chars().count();
            if used + text.chars().count() > jarvis::NOTE_BUDGET {
                return Err(format!(
                    "My Note is full ({used} of {} characters). Ask the owner to delete something first.",
                    jarvis::NOTE_BUDGET
                ));
            }
            if !ask(
                &d.ui,
                crate::i18n::t(d.lang, crate::i18n::Key::jarvis_confirm_note).to_owned(),
                vec![("".to_owned(), clamp(&text, 400))],
            )
            .await
            {
                return Ok(banker::DECLINED.to_owned());
            }
            write_message(d, &note, &text).await?;
            Ok("written to My Note.".into())
        }

        // -- rooms --------------------------------------------------------
        "list_rooms" => {
            let mut out: Vec<String> = Vec::new();
            for r in &d.store.rooms {
                let kind = crate::rooms::StaticRoom::from_kind(&r.room.kind)
                    .map(|_| "built-in")
                    .unwrap_or(if r.room.kind == "direct" {
                        "direct message"
                    } else {
                        "channel"
                    });
                out.push(format!(
                    "- {} [{}] ({} members){}",
                    r.room.name,
                    kind,
                    r.members.len(),
                    if r.has_encryption {
                        ", end-to-end encrypted"
                    } else {
                        ""
                    }
                ));
            }
            if out.is_empty() {
                return Ok("no rooms.".into());
            }
            Ok(out.join("\n"))
        }
        "read_room" => {
            let want = banker::arg_str(args, "room")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(1, 100) as usize;
            let room = resolve_room(d, &want)?;
            let lines = room_lines(d, &room, limit).await;
            if lines.is_empty() {
                return Ok("that room has no messages this device can read.".into());
            }
            Ok(lines
                .iter()
                .map(|(who, text, _)| format!("{who}: {}", clamp(text, SNIPPET)))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "send_message" => {
            let want = banker::arg_str(args, "room")?;
            let text = banker::arg_str(args, "text")?;
            let room = resolve_room(d, &want)?;
            let name = d
                .store
                .room(&room)
                .map(|r| r.room.name.clone())
                .unwrap_or_else(|| room.as_str().to_owned());
            if !ask(
                &d.ui,
                crate::i18n::t(d.lang, crate::i18n::Key::jarvis_confirm_send).to_owned(),
                vec![
                    (
                        crate::i18n::t(d.lang, crate::i18n::Key::jarvis_confirm_room).to_owned(),
                        name.clone(),
                    ),
                    ("".to_owned(), clamp(&text, 400)),
                ],
            )
            .await
            {
                return Ok(banker::DECLINED.to_owned());
            }
            write_message(d, &room, &text).await?;
            Ok(format!("sent to {name}."))
        }

        // -- chain --------------------------------------------------------
        "get_native_balance" => {
            let net = d.network().ok_or("no network configured")?;
            let who = whose(d, args)?;
            let raw = EvmRpc::new(&net.rpc_url)
                .balance(&who)
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {}",
                format_amount(raw, net.decimals),
                net.symbol
            ))
        }
        "get_gas_price" => {
            let net = d.network().ok_or("no network configured")?;
            let raw = EvmRpc::new(&net.rpc_url)
                .gas_price()
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {}",
                format_amount(raw, net.decimals),
                net.symbol
            ))
        }
        "list_tokens" => {
            let net = d.network().ok_or("no network configured")?;
            let tokens = crate::components::bank::extra_tokens(&net);
            if tokens.is_empty() {
                return Ok(format!("no tokens known on {}.", net.name));
            }
            Ok(tokens
                .iter()
                .map(|t| format!("- {} ({}) {}", t.symbol, t.name, t.contract))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "get_token_balance" => {
            let net = d.network().ok_or("no network configured")?;
            let asset = banker::arg_str(args, "asset")?;
            let token = crate::components::bank::extra_tokens(&net)
                .into_iter()
                .find(|t| {
                    t.symbol.eq_ignore_ascii_case(&asset) || t.contract.eq_ignore_ascii_case(&asset)
                })
                .ok_or_else(|| {
                    format!("unknown token \"{asset}\" — call list_tokens to see what is known")
                })?;
            let who = whose(d, args)?;
            let data = pocketskynet_core::chain::erc20_balance_of_data(&who);
            let out = EvmRpc::new(&net.rpc_url)
                .eth_call(&token.contract, &format!("0x{}", hex::encode(data)))
                .await
                .map_err(|e| e.to_string())?;
            let raw = pocketskynet_core::abi::decode_uint(&out, 0).map_err(|e| e.to_string())?;
            Ok(format!(
                "{} {}",
                format_amount(raw, token.decimals),
                token.symbol
            ))
        }

        // -- making -------------------------------------------------------
        "generate_image" => {
            let prompt = banker::arg_str(args, "prompt")?;
            if prompt.chars().count() > 600 {
                return Err("prompt is longer than 600 characters".into());
            }
            let (provider, key) = d.image.clone().ok_or("no image provider configured")?;
            let out = ai::generate_image(provider, &key, &prompt).await?;
            let url = ai::host_generation(&d.store.client, out).await?;
            post_agent_message(d, &url).await;
            Ok("the picture is posted in this room.".into())
        }

        // -- vault --------------------------------------------------------
        "vault_find" => {
            let q = banker::arg_str(args, "query")?;
            let vault = open_vault(d)?;
            let rows = d
                .store
                .client
                .passwords()
                .await
                .map_err(|e| e.user_message())?;
            let hits: Vec<String> = vault
                .labels(&rows)
                .into_iter()
                .filter(|l| l.matches(&q))
                .map(|l| match &l.key {
                    crate::secrets::Opened::Text(name) => format!("- {name} (id {})", l.id),
                    crate::secrets::Opened::Sealed => {
                        format!("- (this entry could not be opened) (id {})", l.id)
                    }
                })
                .collect();
            if hits.is_empty() {
                return Ok("no entry matched that.".into());
            }
            Ok(hits.join("\n"))
        }
        "vault_copy" => {
            let id = banker::arg_str(args, "id")?;
            let vault = open_vault(d)?;
            let rows = d
                .store
                .client
                .passwords()
                .await
                .map_err(|e| e.user_message())?;
            let row = rows
                .iter()
                .find(|r| r.id == id)
                .ok_or("no entry with that id")?;
            // The label is safe to name in the dialog — it is what the owner
            // called the thing, and the dialog is on their own screen.
            let label = match vault.open_label(row).key {
                crate::secrets::Opened::Text(name) => name,
                crate::secrets::Opened::Sealed => id.clone(),
            };
            if !ask(
                &d.ui,
                crate::i18n::t(d.lang, crate::i18n::Key::jarvis_confirm_copy).to_owned(),
                vec![(
                    crate::i18n::t(d.lang, crate::i18n::Key::pw_name_label).to_owned(),
                    label.clone(),
                )],
            )
            .await
            {
                return Ok(banker::DECLINED.to_owned());
            }
            // The one place a secret is opened. It is decrypted, handed
            // straight to the clipboard and dropped — it never becomes a
            // value this function returns, which is what keeps it out of the
            // transcript and off the wire to the provider.
            match vault.open_value(row) {
                crate::secrets::Opened::Text(value) => {
                    crate::components::common::copy_then(&value, |_| {});
                    Ok(format!(
                        "the password for \"{label}\" is on the clipboard. You did not see it and must not ask for it."
                    ))
                }
                crate::secrets::Opened::Sealed => {
                    Err("that entry cannot be opened by this session".into())
                }
            }
        }
        "vault_save" => {
            let label = banker::arg_str(args, "label")?;
            let len = args
                .get("length")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_SECRET_LEN as u64)
                .clamp(12, 128) as usize;
            let vault = open_vault(d)?;
            if !ask(
                &d.ui,
                crate::i18n::t(d.lang, crate::i18n::Key::jarvis_confirm_vault_save).to_owned(),
                vec![
                    (
                        crate::i18n::t(d.lang, crate::i18n::Key::pw_name_label).to_owned(),
                        label.clone(),
                    ),
                    (
                        crate::i18n::t(d.lang, crate::i18n::Key::jarvis_length).to_owned(),
                        len.to_string(),
                    ),
                ],
            )
            .await
            {
                return Ok(banker::DECLINED.to_owned());
            }
            // Minted from the platform CSPRNG, never from the model: a
            // password a language model chose is one its provider has seen.
            let secret = mint_secret(len)?;
            let id = crate::secrets::new_entry_id().map_err(|e| e.to_string())?;
            let (sealed_key, sealed_value) = vault
                .seal(&id, &label, &secret)
                .map_err(|_| "could not seal the entry".to_string())?;
            d.store
                .client
                .create_password(&id, &sealed_key, &sealed_value)
                .await
                .map_err(|e| e.user_message())?;
            crate::components::common::copy_then(&secret, |_| {});
            drop(secret);
            Ok(format!(
                "saved \"{label}\" and put the new password on the clipboard. You did not see it."
            ))
        }

        other => Err(format!(
            "no such tool \"{}\"",
            banker::sanitize_onchain_text(other, 40)
        )),
    }
}

// ---------------------------------------------------------------- helpers --

/// Mint a password from the platform CSPRNG, rejection-sampled so the alphabet
/// stays uniform.
///
/// Modulo-folding a byte over a 67-character alphabet biases the first 55
/// characters upward — small, but this is the one function in the client whose
/// output is somebody's actual password, so it does the arithmetic properly.
fn mint_secret(len: usize) -> Result<String, String> {
    let n = SECRET_ALPHABET.len();
    let limit = (256 / n) * n;
    let mut out = String::with_capacity(len);
    while out.len() < len {
        let batch = pocketskynet_core::random::bytes::<64>().map_err(|e| e.to_string())?;
        for b in batch {
            if (b as usize) < limit {
                out.push(SECRET_ALPHABET[b as usize % n] as char);
                if out.len() == len {
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// The vault, or the reason there isn't one.
fn open_vault(d: &Deps) -> Result<crate::secrets::Vault, String> {
    let session = d
        .store
        .auth
        .session()
        .ok_or("the vault is locked on this device")?;
    let key = session.keys.borrow().vault_key();
    Ok(crate::secrets::Vault::from_key(key))
}

/// Name or id to a room the owner is actually in.
fn resolve_room(d: &Deps, want: &str) -> Result<RoomId, String> {
    let want_l = want.trim().to_lowercase();
    d.store
        .rooms
        .iter()
        .find(|r| {
            r.room.id.as_str().eq_ignore_ascii_case(want) || r.room.name.to_lowercase() == want_l
        })
        .or_else(|| {
            d.store
                .rooms
                .iter()
                .find(|r| r.room.name.to_lowercase().contains(&want_l))
        })
        .map(|r| r.room.id.clone())
        .ok_or_else(|| {
            format!(
                "no room called \"{}\" — call list_rooms to see them",
                banker::sanitize_onchain_text(want, 60)
            )
        })
}

/// The whole of My Note as one block, oldest first, clamped to the budget.
///
/// Kept from the *end* when it overflows: a notebook's newest lines are the
/// ones a question is usually about, and dropping the oldest is the only
/// truncation that stays useful as the note fills.
async fn note_text(d: &Deps) -> String {
    let lines = room_lines(d, &d.note_room(), 500).await;
    let joined = lines
        .iter()
        .map(|(_, text, _)| text.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if joined.chars().count() <= jarvis::NOTE_BUDGET {
        return joined;
    }
    let keep: String = joined
        .chars()
        .skip(joined.chars().count() - jarvis::NOTE_BUDGET)
        .collect();
    format!("…{keep}")
}

/// This room's transcript, split into the two sides the agent needs.
fn transcript(store: &Store, room_id: &RoomId, me: &WalletAddress) -> Vec<jarvis::RoomLine> {
    let agent = WalletAddress::agent_of(me);
    let bundle = store.bundle(room_id).cloned();
    match store.room_state(room_id) {
        Some(state) => state
            .ordered(&store.blocks)
            .iter()
            .filter(|m| !m.is_deleted)
            .filter_map(|m| {
                let text = match &bundle {
                    Some(bundle) => crypto::decrypt_message(bundle, room_id, m)
                        .text()
                        .map(str::to_owned),
                    // No bundle on this device yet: a plaintext row still
                    // reads fine, a sealed one is dropped rather than handed
                    // to the model as ciphertext.
                    None => (!m.is_encrypted).then(|| m.content.clone()),
                };
                text.map(|text| jarvis::RoomLine {
                    from_agent: m.sender_address == agent,
                    text,
                })
            })
            .collect(),
        None => Vec::new(),
    }
}

/// This room's rows, from the snapshot if it has them and from the server if
/// it does not — **returned**, never read back off the store.
///
/// The distinction is the whole bug this function exists to avoid. `d.store`
/// is a handle frozen at the render that spawned this task: `dispatch` updates
/// the reducer, but the next state is only visible to a *later* handle. So the
/// obvious shape — dispatch the fetched page, then ask the store for it —
/// reads the same empty snapshot back and reports an empty room. That is
/// exactly how `jarvis_reply` was broken once before, and how `read_note`
/// answered "My Note is empty" for anyone who had not already opened My Note
/// in this tab. The dispatch still happens, because warming the store is worth
/// it; the *answer* comes from the value in hand.
async fn fetch_rows(d: &Deps, room_id: &RoomId, limit: usize) -> Vec<crate::api::Message> {
    if let Some(state) = d.store.room_state(room_id) {
        let rows: Vec<crate::api::Message> = state
            .ordered(&d.store.blocks)
            .into_iter()
            .cloned()
            .collect();
        if !rows.is_empty() {
            return rows;
        }
    }
    let want = limit.clamp(1, 100) as u32;
    let Ok(fetched) = d.store.client.messages(room_id, None, want).await else {
        return Vec::new();
    };
    d.store.dispatch(Action::History(
        room_id.clone(),
        fetched.clone(),
        fetched.len() as u32 >= want,
    ));
    let mut rows = fetched;
    // `/messages` is already chronological, but the store's own ordering is
    // the one every other reader sees, so sort by it rather than trusting two
    // orderings to agree forever.
    rows.sort_by_key(|m| (m.message_timestamp, m.msg_serial));
    rows
}

/// This room's key bundle, unwrapped **here** rather than fetched into the
/// store and read back.
///
/// `actions::refresh_keys` ends in a `SetBundle` dispatch, which this task's
/// frozen handle will never see — so calling it and then asking the store for
/// the bundle yields `None`, and every encrypted row is silently dropped. For
/// My Note, which this PR made end-to-end encrypted, "silently dropped" means
/// every single line. The unwrap happens against the live session keys, which
/// are a `RefCell` and therefore genuinely shared, not snapshotted.
async fn fetch_bundle(d: &Deps, room_id: &RoomId) -> Option<Rc<crate::crypto::RoomKeyBundle>> {
    let session = d.store.auth.session()?;
    let wraps = d.store.client.room_key_versions(room_id).await.ok()?;
    if wraps.is_empty() {
        return None;
    }
    let bundle = {
        let mut keys = session.keys.borrow_mut();
        crypto::unwrap_bundle(&mut keys, room_id, &wraps).0
    };
    let bundle = Rc::new(bundle);
    d.store
        .dispatch(Action::SetBundle(room_id.clone(), bundle.clone()));
    Some(bundle)
}

/// Decrypted `(who, text, timestamp)` for one room, newest last.
///
/// Reads what is in memory, and fetches when there is nothing there — a room
/// this tab has never opened has no `RoomState` at all, and "search
/// everything" that silently skipped every unopened room would be a lie.
async fn room_lines(d: &Deps, room_id: &RoomId, limit: usize) -> Vec<(String, String, i64)> {
    let rows = fetch_rows(d, room_id, limit).await;
    let bundle = match d.store.bundle(room_id).cloned() {
        Some(b) => Some(b),
        None => fetch_bundle(d, room_id).await,
    };
    let names = d.store.room(room_id).map(|r| r.members.clone());
    let all: Vec<&crate::api::Message> = rows.iter().collect();
    all.iter()
        .filter(|m| !m.is_deleted)
        .skip(all.len().saturating_sub(limit))
        .filter_map(|m| {
            let text = match &bundle {
                Some(b) => crypto::decrypt_message(b, room_id, m)
                    .text()
                    .map(str::to_owned),
                None => (!m.is_encrypted).then(|| m.content.clone()),
            };
            text.map(|text| {
                let who = names
                    .as_ref()
                    .and_then(|ms| ms.iter().find(|mm| mm.user_address == m.sender_address))
                    .map(|mm| mm.user.display_name())
                    .unwrap_or_else(|| m.sender_address.abbreviated());
                (who, text, m.message_timestamp)
            })
        })
        .collect()
}

/// The server's index. Plaintext only — by construction, not by omission.
async fn search_server(d: &Deps, query: &str, kind: Option<&str>) -> Result<String, String> {
    let hits = d
        .store
        .client
        .search(query, 20)
        .await
        .map_err(|e| e.user_message())?;
    let hits: Vec<_> = hits
        .into_iter()
        .filter(|h| kind.is_none_or(|k| k == "all" || k == h.kind))
        .collect();
    if hits.is_empty() {
        return Ok("nothing in the server index matched.".into());
    }
    Ok(hits
        .iter()
        .map(|h| format!("- [{}] {}", h.kind, clamp(&h.text, SNIPPET)))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Every encrypted room whose key is on this device.
///
/// The half the server cannot do. The query never leaves the browser and
/// neither does anything it matched — until the model reads the result, which
/// is exactly why this is a tool the owner asked for rather than something
/// that runs on its own.
async fn search_rooms(d: &Deps, query: &str) -> Result<String, String> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Err("empty query".into());
    }
    let rooms: Vec<RoomId> = d.store.rooms.iter().map(|r| r.room.id.clone()).collect();
    let total = rooms.len();
    let mut hits: Vec<String> = Vec::new();
    let mut looked = 0usize;

    for room_id in rooms.into_iter().take(SEARCH_FETCH_ROOMS) {
        looked += 1;
        let name = d
            .store
            .room(&room_id)
            .map(|r| r.room.name.clone())
            .unwrap_or_else(|| room_id.as_str().to_owned());
        for (who, text, ts) in room_lines(d, &room_id, SEARCH_FETCH_DEPTH).await {
            if text.to_lowercase().contains(&q) {
                hits.push(format!(
                    "- [{name}] {who} on {}: {}",
                    crate::format::short_date(ts, crate::format::tz_offset_minutes()),
                    clamp(&text, SNIPPET)
                ));
            }
        }
    }

    let mut out = if hits.is_empty() {
        "nothing in the encrypted rooms matched.".to_owned()
    } else {
        hits.join("\n")
    };
    // Never let a cap read as a complete answer.
    if total > looked {
        out.push_str(&format!(
            "\n(searched the {looked} most recent rooms of {total}; ask again naming a room to look further back)"
        ));
    }
    Ok(out)
}

/// Post as the *owner* into some room — the write half of `send_message` and
/// `append_note`, sealed the way the composer would seal it.
async fn write_message(d: &Deps, room_id: &RoomId, text: &str) -> Result<(), String> {
    let encrypted = d.store.room(room_id).is_some_and(|r| r.has_encryption);
    let body = if encrypted {
        // A room meant to be encrypted, with no key on this device, is left
        // alone rather than posted to in the clear: silently downgrading an
        // E2EE room is the one outcome the product refuses (DESIGN.md §8).
        crate::actions::refresh_keys(d.store.clone(), room_id.clone()).await;
        let bundle = d
            .store
            .bundle(room_id)
            .cloned()
            .ok_or("that room is encrypted and its key is not on this device")?;
        let (version, room_key) = bundle
            .latest()
            .ok_or("that room is encrypted and its key is not on this device")?;
        crypto::encrypted_body(room_key, version, room_id, text).map_err(|e| e.to_string())?
    } else {
        crypto::plaintext_body(text)
    };
    let message = d
        .store
        .client
        .send_message(room_id, &body)
        .await
        .map_err(|e| e.user_message())?;
    d.store
        .dispatch(Action::Sync(room_id.clone(), vec![message]));
    Ok(())
}

/// Write one message into the Jarvis room under the agent's address.
async fn post_agent_message(d: &Deps, text: &str) {
    let encrypted = d.store.room(&d.room_id).is_some_and(|r| r.has_encryption);
    let body = if encrypted {
        let Some((version, room_key)) = d.store.bundle(&d.room_id).and_then(|b| b.latest()) else {
            web_sys::console::warn_1(
                &"jarvis: room is encrypted but no key is available yet, dropping reply".into(),
            );
            return;
        };
        match crypto::encrypted_body(room_key, version, &d.room_id, text) {
            Ok(sealed) => crate::api::messages::AgentReplyBody::from(sealed),
            Err(e) => {
                web_sys::console::warn_1(&format!("jarvis: couldn't seal reply: {e}").into());
                return;
            }
        }
    } else {
        crate::api::messages::AgentReplyBody::from(crypto::plaintext_body(text))
    };

    match d.store.client.agent_reply(&d.room_id, &body).await {
        Ok(message) => d
            .store
            .dispatch(Action::Sync(d.room_id.clone(), vec![message])),
        Err(e) => {
            web_sys::console::warn_1(&format!("jarvis reply rejected: {}", e.user_message()).into())
        }
    }
}

/// Where the browser thinks it is, if it will say.
async fn locate() -> Result<String, String> {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::closure::Closure;
        use wasm_bindgen::JsCast;

        let geo = web_sys::window()
            .and_then(|w| w.navigator().geolocation().ok())
            .ok_or("this browser has no geolocation")?;
        let cell: Rc<RefCell<Option<Result<String, String>>>> = Rc::new(RefCell::new(None));

        let ok_cell = cell.clone();
        let ok = Closure::once(Box::new(move |pos: web_sys::Position| {
            let c = pos.coords();
            *ok_cell.borrow_mut() = Some(Ok(format!(
                "latitude {:.4}, longitude {:.4} (±{:.0} m)",
                c.latitude(),
                c.longitude(),
                c.accuracy()
            )));
        }) as Box<dyn FnMut(web_sys::Position)>);

        let err_cell = cell.clone();
        let err = Closure::once(Box::new(move |_e: web_sys::PositionError| {
            *err_cell.borrow_mut() =
                Some(Err("the owner did not allow location access".to_owned()));
        }) as Box<dyn FnMut(web_sys::PositionError)>);

        geo.get_current_position_with_error_callback(
            ok.as_ref().unchecked_ref(),
            Some(err.as_ref().unchecked_ref()),
        )
        .map_err(|_| "could not ask for a location".to_string())?;
        ok.forget();
        err.forget();

        // The permission prompt is a person reading a dialog, so the wait is
        // generous — but bounded, because a prompt nobody answers must not
        // hang the turn forever.
        for _ in 0..250 {
            if let Some(v) = cell.borrow_mut().take() {
                return v;
            }
            gloo_timers::future::TimeoutFuture::new(120).await;
        }
        Err("the location request was not answered".into())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Err("geolocation is wasm-only".into())
    }
}

fn device_line(d: &Deps) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        let w = web_sys::window();
        let nav = w.as_ref().map(|w| w.navigator());
        let online = nav.as_ref().map(|n| n.on_line()).unwrap_or(true);
        let ua = nav
            .as_ref()
            .and_then(|n| n.user_agent().ok())
            .unwrap_or_default();
        format!(
            "interface language {}, {}, browser: {}",
            d.lang.english_name(),
            if online { "online" } else { "offline" },
            banker::sanitize_onchain_text(&ua, 160)
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        format!("interface language {}", d.lang.english_name())
    }
}

/// Whose balance a chain tool is being asked about: the argument if it gave
/// one, the owner otherwise.
fn whose(d: &Deps, args: &Value) -> Result<WalletAddress, String> {
    match banker::arg_str_opt(args, "address") {
        Some(a) => WalletAddress::new(&a).map_err(|_| format!("not a wallet address: {a}")),
        None => Ok(d.me.clone()),
    }
}

/// The date and time as somebody here would say it.
///
/// English month and weekday names because the *prompt* is English — the model
/// renders the answer in the user's language, and a Korean weekday embedded in
/// an English sentence is a translation bug waiting to be reported.
fn local_now() -> String {
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    let tz = crate::format::tz_offset_minutes();
    let c = crate::format::civil_from_ms(crate::format::now_ms(), tz);
    let day = DAYS.get(c.weekday as usize).copied().unwrap_or("");
    let month = MONTHS
        .get(c.month.saturating_sub(1) as usize)
        .copied()
        .unwrap_or("");
    format!(
        "{day} {} {month} {}, {:02}:{:02} (UTC{}{:02}:{:02})",
        c.day,
        c.year,
        c.hour,
        c.minute,
        if tz < 0 { '-' } else { '+' },
        tz.abs() / 60,
        tz.abs() % 60,
    )
}

/// Cut a string to `max` characters, visibly.
fn clamp(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_documented_tool_is_dispatched() {
        // The other half of `jarvis::every_available_tool_is_documented_in_the_prompt`.
        // Together they pin the table to both of its consumers, so a tool
        // cannot be advertised without an implementation or implemented
        // without being offered.
        for tool in jarvis::TOOLS {
            assert!(
                handles(tool.name),
                "{} is documented but never dispatched",
                tool.name
            );
        }
    }

    #[test]
    fn a_generated_password_is_the_length_asked_for_and_stays_in_the_alphabet() {
        for len in [12usize, 24, 64, 128] {
            let s = mint_secret(len).expect("entropy");
            assert_eq!(s.chars().count(), len);
            assert!(
                s.bytes().all(|b| SECRET_ALPHABET.contains(&b)),
                "stray character in {s}"
            );
        }
    }

    #[test]
    fn generated_passwords_do_not_repeat() {
        // Not a randomness test — a wiring test. The failure this catches is
        // a seed taken once and reused, which would hand every entry the same
        // password.
        let a = mint_secret(32).unwrap();
        let b = mint_secret(32).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_password_alphabet_excludes_the_characters_people_misread() {
        for c in [b'O', b'0', b'l', b'1', b'I'] {
            assert!(
                !SECRET_ALPHABET.contains(&c),
                "{} is ambiguous and must not appear in a generated password",
                c as char
            );
        }
    }

    #[test]
    fn clamp_cuts_visibly_and_leaves_short_strings_alone() {
        assert_eq!(clamp("  hello  ", 20), "hello");
        let long = "a".repeat(50);
        let cut = clamp(&long, 10);
        assert_eq!(cut.chars().count(), 10);
        assert!(cut.ends_with('…'));
    }
}
