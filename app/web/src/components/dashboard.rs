//! The Skynet Dashboard — an operator's view of the whole deployment
//! (`/dashboard`): the server in counts, then the disk in files.
//!
//! Offered only to wallets the *server* names as administrators, exactly like
//! the admin console: hiding the route is a courtesy, the access control is
//! the `ServerAdmin` extractor behind every fetch this page makes. A
//! non-admin who types the URL gets the refusal screen, not a blank page.
//!
//! # What it deliberately does not do
//!
//! Nothing here opens a conversation, and nothing opens a file. The server
//! half is counts — rooms by kind, message volume by day, heads connected —
//! and the files half is metadata: names, sizes, rooms, uploaders, dates.
//! The API it reads from carries no download URL and honours none an admin
//! could build: the bytes stay behind the room membership check
//! (`GET /api/files/{id}/raw`), admin or not. An attachment is part of a
//! conversation, and this dashboard is not a way to read one.
//!
//! # The charts
//!
//! Hand-rolled SVG and CSS bars, no charting library — the client is WASM and
//! every dependency is paid for at cold start. They follow the house palette
//! rather than a chart palette: identity lives in the row labels and direct
//! values, the marks themselves are one recessive ink (DESIGN.md §1 — one
//! accent, and most elements have no colour at all), so nothing is said by
//! colour alone.

use yew::prelude::*;

use crate::api::admin::{AdminFile, AdminStats, AdminStorage, FlowStats, GrowthPoint, MessageDay};
use crate::components::common::{Empty, Skeleton};
use crate::components::transfers::human_bytes;
use crate::i18n::{t, Key, Lang};
use crate::state::{use_store, Load};

/// How many table rows are rendered at once. The API caps its listing at
/// 2 000; painting all of them costs more than anybody scrolls, and past a
/// few hundred the filter is the tool, not the scrollbar.
const TABLE_ROWS: usize = 300;

/// The window the growth chart draws, matching the server's aggregation.
const GROWTH_DAYS: i64 = 30;

// ----------------------------------------------------------- pure helpers --

/// A throughput readout: `"12.4 MB/s"`, or `"—"` when nothing has moved.
///
/// The server ships integers (bytes, milliseconds) and the division happens
/// here, so the arithmetic is host-testable and the wire stays float-free.
pub fn rate_label(bytes: i64, millis: i64) -> String {
    if bytes <= 0 || millis <= 0 {
        return "—".to_owned();
    }
    let per_second = bytes as f64 / (millis as f64 / 1000.0);
    format!("{}/s", human_bytes(per_second))
}

/// Days since the civil epoch for a calendar date (Howard Hinnant's
/// algorithm) — the inverse of `format::civil_from_ms` at day resolution.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `"2026-08-07"` → days since epoch. Anything malformed is `None` rather
/// than a bar drawn in the wrong place.
fn parse_day(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: i64 = parts.next()?.parse().ok()?;
    let d: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Days since epoch → `"YYYY-MM-DD"`, via the formatter the rest of the app
/// already trusts.
fn day_label(day: i64) -> String {
    let civil = crate::format::civil_from_ms(day * 86_400_000, 0);
    format!("{:04}-{:02}-{:02}", civil.year, civil.month, civil.day)
}

/// The dense trailing window both day charts draw: `(first day, today)`.
fn day_window(now_ms: i64, days: i64) -> (i64, i64) {
    let today = now_ms.div_euclid(86_400_000);
    (today - (days - 1), today)
}

/// Uptime as a machine readout: `"3d 04h"`, `"4h 12m"`, `"09m"`. The unit
/// letters stay untranslated like every HUD code in this product — they are
/// telemetry, not prose.
pub fn uptime_label(secs: i64) -> String {
    let secs = secs.max(0);
    let d = secs / 86_400;
    let h = (secs % 86_400) / 3_600;
    let m = (secs % 3_600) / 60;
    if d > 0 {
        format!("{d}d {h:02}h")
    } else if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m:02}m")
    }
}

/// Lay the server's sparse day buckets onto a dense trailing window, oldest
/// first. The server sends only days that saw an upload — a wire format
/// padded with zeros is just a bigger wire format — but a bar chart with the
/// silent days removed lies about the rhythm, so the gaps are restored here.
pub fn fill_growth(points: &[GrowthPoint], now_ms: i64, days: i64) -> Vec<GrowthPoint> {
    let (start, today) = day_window(now_ms, days);
    let mut series: Vec<GrowthPoint> = (start..=today)
        .map(|day| GrowthPoint {
            day: day_label(day),
            files: 0,
            bytes: 0,
        })
        .collect();
    for point in points {
        if let Some(day) = parse_day(&point.day) {
            if day >= start && day <= today {
                let slot = &mut series[(day - start) as usize];
                slot.files = point.files;
                slot.bytes = point.bytes;
            }
        }
    }
    series
}

/// The message twin of [`fill_growth`], with the identical contract.
pub fn fill_activity(points: &[MessageDay], now_ms: i64, days: i64) -> Vec<MessageDay> {
    let (start, today) = day_window(now_ms, days);
    let mut series: Vec<MessageDay> = (start..=today)
        .map(|day| MessageDay {
            day: day_label(day),
            messages: 0,
        })
        .collect();
    for point in points {
        if let Some(day) = parse_day(&point.day) {
            if day >= start && day <= today {
                series[(day - start) as usize].messages = point.messages;
            }
        }
    }
    series
}

/// What the table can be ordered by. `Date` descending is the default — the
/// same "newest first" every listing in this product opens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Name,
    Size,
    Kind,
    Room,
    Uploader,
    Date,
}

/// Order the listing. Stable, so equal keys keep the server's newest-first
/// arrival order and re-sorting never shuffles ties.
pub fn sort_files(files: &mut [AdminFile], key: SortKey, ascending: bool) {
    files.sort_by(|a, b| {
        let ordering = match key {
            SortKey::Name => a.filename.to_lowercase().cmp(&b.filename.to_lowercase()),
            SortKey::Size => a.size_bytes.cmp(&b.size_bytes),
            SortKey::Kind => a.category.cmp(&b.category),
            SortKey::Room => a.room_name.to_lowercase().cmp(&b.room_name.to_lowercase()),
            SortKey::Uploader => uploader_label(a)
                .to_lowercase()
                .cmp(&uploader_label(b).to_lowercase()),
            SortKey::Date => a.created_at.cmp(&b.created_at),
        };
        if ascending {
            ordering
        } else {
            ordering.reverse()
        }
    });
}

/// Whether a row survives the text filter. Matched against everything a
/// person might remember about a file: its name, extension, room, uploader
/// name, and uploader address. Case-insensitive, whole-query substring.
pub fn matches_filter(file: &AdminFile, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    file.filename.to_lowercase().contains(&query)
        || file.extension.to_lowercase().contains(&query)
        || file.room_name.to_lowercase().contains(&query)
        || file.uploader.to_lowercase().contains(&query)
        || file
            .uploader_name
            .as_deref()
            .is_some_and(|name| name.to_lowercase().contains(&query))
}

/// The name a row shows for its uploader: the username while the profile
/// exists, the abbreviated address forever.
pub fn uploader_label(file: &AdminFile) -> String {
    match file.uploader_name.as_deref().map(str::trim) {
        Some(name) if !name.is_empty() => name.to_owned(),
        _ => abbreviated(&file.uploader),
    }
}

/// `0x742d…6b22` — the display form PROTOCOL.md §2 names.
fn abbreviated(address: &str) -> String {
    if address.len() > 12 {
        format!("{}…{}", &address[..6], &address[address.len() - 4..])
    } else {
        address.to_owned()
    }
}

/// `"2026-08-07T12:34:56.789Z"` → `"2026-08-07 12:34"`. A machine readout on
/// purpose (DESIGN.md §1): this column is telemetry, sorted and scanned, and
/// a locale-shaped date would jitter the table where tabular figures don't.
fn compact_timestamp(iso: &str) -> String {
    if iso.len() >= 16 {
        iso[..16].replace('T', " ")
    } else {
        iso.to_owned()
    }
}

fn category_key(category: &str) -> Key {
    match category {
        "image" => Key::dash_cat_image,
        "video" => Key::dash_cat_video,
        "audio" => Key::dash_cat_audio,
        "document" => Key::dash_cat_document,
        "archive" => Key::dash_cat_archive,
        _ => Key::dash_cat_other,
    }
}

// ------------------------------------------------------------- the screen --

#[function_component(Dashboard)]
pub fn dashboard() -> Html {
    let store = use_store();
    let lang = store.language;

    let stats = use_state(AdminStats::default);
    let storage = use_state(AdminStorage::default);
    let files = use_state(Vec::<AdminFile>::new);
    let load = use_state(Load::default);

    // Table controls. Sort is (key, ascending); the default mirrors the
    // server's own order so the first paint and the first click agree.
    let sort = use_state(|| (SortKey::Date, false));
    let query = use_state(String::new);
    let category = use_state(|| None::<String>);

    {
        let store = store.clone();
        let stats = stats.clone();
        let storage = storage.clone();
        let files = files.clone();
        let load = load.clone();
        use_effect_with((), move |_| {
            load.set(Load::Loading);
            wasm_bindgen_futures::spawn_local(async move {
                let client = store.client.clone();
                match futures::join!(
                    client.admin_stats(),
                    client.admin_storage(),
                    client.admin_files()
                ) {
                    (Ok(st), Ok(s), Ok(f)) => {
                        stats.set(st);
                        storage.set(s);
                        files.set(f);
                        load.set(Load::Ready);
                    }
                    (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                        load.set(Load::Error(e.user_message()))
                    }
                }
            });
            || ()
        });
    }

    let body = match &*load {
        Load::Idle | Load::Loading => html! { <Skeleton rows={6} /> },
        // The refusal screen a non-admin reaches by typing the URL: the
        // server's own message, not a pretence that the page half-works.
        Load::Error(e) => html! {
            <Empty art="⚠️" title={t(lang, Key::dash_error)}
                   description={e.clone()} is_error=true />
        },
        Load::Ready => {
            // The files half stands down to its empty state on a server with
            // no attachments; the server half always has something to say —
            // a deployment with zero rooms is itself a fact worth a tile.
            let files_half = if storage.totals.files == 0 {
                html! {
                    <Empty art="🗄️" art_class={classes!("fn-art--files")}
                           title={t(lang, Key::dash_empty_title)}
                           description={t(lang, Key::dash_empty_desc)} />
                }
            } else {
                html! {
                    <>
                        { tiles(lang, &storage) }
                        <div class="fn-dash__grid">
                            { breakdown_card(lang, &storage) }
                            { growth_card(lang, &storage) }
                            { rooms_card(lang, &storage) }
                            { largest_card(lang, &storage) }
                        </div>
                        { activity_card(lang, &storage) }
                        { files_table(lang, &files, &sort, &query, &category) }
                    </>
                }
            };
            html! {
                <>
                    { section_head(lang, Key::dash_section_server) }
                    { server_tiles(lang, &stats) }
                    <div class="fn-dash__grid">
                        { message_activity_card(lang, &stats) }
                        { busiest_card(lang, &stats) }
                    </div>
                    { section_head(lang, Key::dash_section_files) }
                    { files_half }
                </>
            }
        }
    };

    html! {
        <div class="fn-dash fn-scroll">
            <header class="fn-dash__head">
                <div class="fn-art fn-art--dashboard-emblem fn-dash__emblem"
                     aria-hidden="true"></div>
                <h1>{ t(lang, Key::dash_title) }</h1>
                <p class="fn-dash__sub">{ t(lang, Key::dash_subtitle) }</p>
            </header>
            { body }
        </div>
    }
}

// ---------------------------------------------------------- server section --

/// A section eyebrow: the dashboard reads as one instrument with two panels,
/// and these are the panel labels.
fn section_head(lang: Lang, key: Key) -> Html {
    html! { <h2 class="fn-dash__eyebrow">{ t(lang, key) }</h2> }
}

fn msg_count_label(lang: Lang, n: i64) -> String {
    t(
        lang,
        if n == 1 {
            Key::admin_message_one
        } else {
            Key::admin_message_many
        },
    )
    .replace("{n}", &n.to_string())
}

fn member_count_label(lang: Lang, n: i64) -> String {
    t(
        lang,
        if n == 1 {
            Key::member_count_one
        } else {
            Key::member_count_many
        },
    )
    .replace("{n}", &n.to_string())
}

fn server_tiles(lang: Lang, stats: &AdminStats) -> Html {
    let rooms_foot = t(lang, Key::dash_rooms_split)
        .replace("{channels}", &stats.rooms.channels.to_string())
        .replace("{dms}", &stats.rooms.direct_messages.to_string())
        .replace("{encrypted}", &stats.rooms.encrypted.to_string());
    let people_foot = t(lang, Key::dash_people_foot)
        .replace("{rooms}", &stats.people.in_rooms.to_string())
        .replace("{suspended}", &stats.people.suspended.to_string());
    let messages_foot = t(lang, Key::dash_messages_foot)
        .replace("{threads}", &stats.messages.thread_replies.to_string())
        .replace("{reactions}", &stats.messages.reactions.to_string());

    html! {
        <div class="fn-dash__tiles">
            { tile(t(lang, Key::dash_uptime),
                   uptime_label(stats.uptime_seconds),
                   t(lang, Key::dash_counters_note).to_owned()) }
            { tile(t(lang, Key::dash_online_now),
                   stats.presence.online.to_string(),
                   t(lang, Key::dash_away_foot)
                       .replace("{n}", &stats.presence.away.to_string())) }
            { tile(t(lang, Key::admin_people), stats.people.total.to_string(), people_foot) }
            { tile(t(lang, Key::admin_rooms), stats.rooms.total.to_string(), rooms_foot) }
            { tile(t(lang, Key::dash_messages_tile),
                   stats.messages.total.to_string(), messages_foot) }
        </div>
    }
}

/// The month of message volume — the conversation twin of [`growth_card`],
/// drawn by the same shared chart so the two share an x-axis a reader can
/// hold side by side.
fn message_activity_card(lang: Lang, stats: &AdminStats) -> Html {
    let series = fill_activity(&stats.activity, crate::format::now_ms(), GROWTH_DAYS);
    let total: i64 = series.iter().map(|p| p.messages).sum();
    let max = series.iter().map(|p| p.messages).max().unwrap_or(0);

    let aside =
        t(lang, Key::dash_msg_activity_total).replace("{messages}", &msg_count_label(lang, total));

    let body = if max == 0 {
        html! { <p class="fn-dash__quiet">{ t(lang, Key::dash_msg_activity_empty) }</p> }
    } else {
        let columns: Vec<ChartColumn> = series
            .iter()
            .map(|point| ChartColumn {
                day: point.day.clone(),
                value: point.messages,
                hover: format!("{} · {}", point.day, msg_count_label(lang, point.messages)),
            })
            .collect();
        let label = t(lang, Key::dash_msg_activity_label)
            .replace("{days}", &GROWTH_DAYS.to_string())
            .replace("{messages}", &msg_count_label(lang, total));
        let peak = t(lang, Key::dash_growth_peak).replace("{bytes}", &max.to_string());
        column_chart(&columns, label, peak)
    };
    card(t(lang, Key::dash_msg_activity), Some(aside), body)
}

fn busiest_card(lang: Lang, stats: &AdminStats) -> Html {
    let max = stats
        .busiest
        .iter()
        .map(|r| r.messages)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let rows = stats
        .busiest
        .iter()
        .map(|room| {
            // The lock rides the name as a glyph, the way the room list wears
            // it — never colour, and never only an icon: the tooltip-less bar
            // row still reads because the glyph is beside the text.
            let label = if room.has_encryption {
                format!("{} 🔒", room.name)
            } else {
                room.name.clone()
            };
            bar_row(
                label,
                member_count_label(lang, room.members),
                msg_count_label(lang, room.messages),
                room.messages as f64 / max,
            )
        })
        .collect::<Html>();
    card(t(lang, Key::dash_busiest), None, rows)
}

// ------------------------------------------------------------------ tiles --

fn tiles(lang: Lang, storage: &AdminStorage) -> Html {
    let totals = &storage.totals;
    let up = &storage.activity.uploads;
    let down = &storage.activity.downloads;

    let dedupe_foot = t(lang, Key::dash_disk_foot)
        .replace("{files}", &count_label(lang, totals.files))
        .replace("{blobs}", &totals.blobs.to_string());

    html! {
        <div class="fn-dash__tiles">
            { tile(t(lang, Key::dash_disk_used),
                   human_bytes(totals.disk_bytes as f64), dedupe_foot) }
            { tile(t(lang, Key::dash_rooms_with_files),
                   totals.rooms_with_files.to_string(),
                   t(lang, Key::dash_rooms_foot)
                       .replace("{bytes}", &human_bytes(totals.logical_bytes as f64))) }
            { tile(t(lang, Key::dash_received),
                   human_bytes(up.bytes as f64),
                   t(lang, Key::dash_avg_rate)
                       .replace("{rate}", &rate_label(up.bytes, up.millis))) }
            { tile(t(lang, Key::dash_served),
                   human_bytes(down.bytes as f64),
                   t(lang, Key::dash_avg_rate)
                       .replace("{rate}", &rate_label(down.bytes, down.millis))) }
        </div>
    }
}

fn tile(label: &str, value: String, foot: String) -> Html {
    html! {
        <div class="fn-dash__tile">
            <span class="fn-dash__tile-label">{ label }</span>
            <span class="fn-dash__tile-value fn-nums">{ value }</span>
            <span class="fn-dash__tile-foot">{ foot }</span>
        </div>
    }
}

// ------------------------------------------------------------------ cards --

fn card(title: &str, aside: Option<String>, body: Html) -> Html {
    html! {
        <section class="fn-dash__card">
            <header class="fn-dash__card-head">
                <h3>{ title }</h3>
                if let Some(aside) = aside {
                    <span class="fn-dash__card-aside fn-nums">{ aside }</span>
                }
            </header>
            { body }
        </section>
    }
}

/// A labelled horizontal bar: the one mark both breakdown cards use.
/// Identity is the text, the value is the text — the bar only gives the
/// column of numbers a shape, which is why one recessive ink is enough.
fn bar_row(label: String, detail: String, value: String, fraction: f64) -> Html {
    // A zero draws no fill at all. The fill's 2px minimum exists so a tiny
    // value stays visible; letting it apply to zero would put a nub of bar
    // on an empty row — a small lie, but a lie in a chart.
    let width = if fraction <= 0.0 {
        "width:0;min-width:0".to_owned()
    } else {
        format!("width:{:.2}%", (fraction * 100.0).clamp(0.0, 100.0))
    };
    html! {
        <div class="fn-dash__row">
            <span class="fn-dash__row-label fn-truncate">{ label }</span>
            <span class="fn-dash__row-detail fn-nums">{ detail }</span>
            <span class="fn-dash__row-value fn-nums">{ value }</span>
            <div class="fn-dash__track" aria-hidden="true">
                <div class="fn-dash__fill" style={width} />
            </div>
        </div>
    }
}

fn count_label(lang: Lang, n: i64) -> String {
    t(
        lang,
        if n == 1 {
            Key::dash_file_one
        } else {
            Key::dash_file_many
        },
    )
    .replace("{n}", &n.to_string())
}

fn breakdown_card(lang: Lang, storage: &AdminStorage) -> Html {
    let max = storage
        .categories
        .iter()
        .map(|c| c.bytes)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let rows = storage
        .categories
        .iter()
        .map(|slice| {
            bar_row(
                t(lang, category_key(&slice.category)).to_owned(),
                count_label(lang, slice.files),
                human_bytes(slice.bytes as f64),
                slice.bytes as f64 / max,
            )
        })
        .collect::<Html>();
    card(t(lang, Key::dash_breakdown), None, rows)
}

fn rooms_card(lang: Lang, storage: &AdminStorage) -> Html {
    let max = storage
        .rooms
        .iter()
        .map(|r| r.bytes)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let rows = storage
        .rooms
        .iter()
        .map(|room| {
            bar_row(
                room.name.clone(),
                count_label(lang, room.files),
                human_bytes(room.bytes as f64),
                room.bytes as f64 / max,
            )
        })
        .collect::<Html>();
    card(t(lang, Key::dash_rooms_card), None, rows)
}

fn largest_card(lang: Lang, storage: &AdminStorage) -> Html {
    let max = storage
        .largest
        .iter()
        .map(|f| f.size_bytes)
        .max()
        .unwrap_or(0)
        .max(1) as f64;
    let rows = storage
        .largest
        .iter()
        .map(|file| {
            bar_row(
                file.filename.clone(),
                file.room_name.clone(),
                human_bytes(file.size_bytes as f64),
                file.size_bytes as f64 / max,
            )
        })
        .collect::<Html>();
    card(t(lang, Key::dash_largest), None, rows)
}

/// One column of a day chart, ready to draw.
struct ChartColumn {
    day: String,
    value: i64,
    /// The sentence the `<title>` shows on hover.
    hover: String,
}

/// The one SVG day chart both halves of the dashboard draw.
///
/// Hand-rolled on purpose (module docs). The geometry is all that lives in
/// SVG; labels sit in HTML where the type tokens already apply. Every column
/// carries a `<title>`, so hovering names the day and its value — and the
/// whole figure has one accessible sentence, because thirty unlabeled rects
/// are noise to a screen reader.
fn column_chart(columns: &[ChartColumn], aria_label: String, peak: String) -> Html {
    let max = columns.iter().map(|c| c.value).max().unwrap_or(0).max(1);
    // 20 units per day and a 2-unit gap — the dataviz spacer rule — on a
    // 100-unit height; `preserveAspectRatio="none"` lets the card decide
    // the on-screen size while the ratios stay true.
    let w = 20.0;
    let gap = 2.0;
    let height = 100.0;
    let view_w = columns.len() as f64 * w;
    let rects = columns
        .iter()
        .enumerate()
        .map(|(i, column)| {
            let h = (column.value as f64 / max as f64) * (height - 4.0);
            // Activity happened but rounded to nothing: draw a 1-unit stub
            // so a day with traffic is never pixel-identical to silence.
            let h = if column.value > 0 { h.max(1.0) } else { 0.0 };
            let x = i as f64 * w + gap / 2.0;
            html! {
                <rect
                    key={column.day.clone()}
                    class="fn-dash__col"
                    x={format!("{x:.1}")}
                    y={format!("{:.1}", height - h)}
                    width={format!("{:.1}", w - gap)}
                    height={format!("{h:.1}")}
                >
                    <title>{ column.hover.clone() }</title>
                </rect>
            }
        })
        .collect::<Html>();
    html! {
        <>
            <svg
                class="fn-dash__chart"
                viewBox={format!("0 0 {view_w} {height}")}
                preserveAspectRatio="none"
                role="img"
                aria-label={aria_label}
            >
                <line class="fn-dash__axis" x1="0" y1={height.to_string()}
                      x2={view_w.to_string()} y2={height.to_string()} />
                { rects }
            </svg>
            <div class="fn-dash__chart-foot fn-nums" aria-hidden="true">
                <span>{ columns.first().map(|c| c.day.clone()).unwrap_or_default() }</span>
                <span>{ peak }</span>
                <span>{ columns.last().map(|c| c.day.clone()).unwrap_or_default() }</span>
            </div>
        </>
    }
}

/// The month of upload volume, on the shared day chart.
fn growth_card(lang: Lang, storage: &AdminStorage) -> Html {
    let series = fill_growth(&storage.growth, crate::format::now_ms(), GROWTH_DAYS);
    let total_bytes: i64 = series.iter().map(|p| p.bytes).sum();
    let total_files: i64 = series.iter().map(|p| p.files).sum();
    let max = series.iter().map(|p| p.bytes).max().unwrap_or(0);

    let aside = t(lang, Key::dash_growth_total)
        .replace("{bytes}", &human_bytes(total_bytes as f64))
        .replace("{files}", &count_label(lang, total_files));

    let body = if max == 0 {
        html! { <p class="fn-dash__quiet">{ t(lang, Key::dash_growth_empty) }</p> }
    } else {
        let columns: Vec<ChartColumn> = series
            .iter()
            .map(|point| ChartColumn {
                day: point.day.clone(),
                value: point.bytes,
                hover: format!(
                    "{} · {} · {}",
                    point.day,
                    human_bytes(point.bytes as f64),
                    count_label(lang, point.files),
                ),
            })
            .collect();
        let label = t(lang, Key::dash_growth_label)
            .replace("{days}", &GROWTH_DAYS.to_string())
            .replace("{bytes}", &human_bytes(total_bytes as f64));
        let peak = t(lang, Key::dash_growth_peak).replace("{bytes}", &human_bytes(max as f64));
        column_chart(&columns, label, peak)
    };
    card(t(lang, Key::dash_growth), Some(aside), body)
}

// --------------------------------------------------------------- activity --

fn activity_card(lang: Lang, storage: &AdminStorage) -> Html {
    let body = html! {
        <>
            <div class="fn-dash__flows">
                { flow(lang, Key::dash_uploads, &storage.activity.uploads) }
                { flow(lang, Key::dash_downloads, &storage.activity.downloads) }
            </div>
            // The honesty line presence taught: these are in-process
            // counters, and a restart starting them over is the design.
            <p class="fn-dash__quiet">{ t(lang, Key::dash_counters_note) }</p>
        </>
    };
    card(t(lang, Key::dash_activity), None, body)
}

fn flow(lang: Lang, label: Key, stats: &FlowStats) -> Html {
    html! {
        <div class="fn-dash__flow">
            <h4>{ t(lang, label) }</h4>
            <span class="fn-dash__flow-big fn-nums">{ human_bytes(stats.bytes as f64) }</span>
            <dl class="fn-dash__flow-stats">
                <div>
                    <dt>{ t(lang, Key::dash_transfers) }</dt>
                    <dd class="fn-nums">{ stats.transfers }</dd>
                </div>
                <div>
                    <dt>{ t(lang, Key::dash_rate_avg) }</dt>
                    <dd class="fn-nums">{ rate_label(stats.bytes, stats.millis) }</dd>
                </div>
                <div>
                    <dt>{ t(lang, Key::dash_rate_recent) }</dt>
                    <dd class="fn-nums">{ rate_label(stats.recent_bytes, stats.recent_millis) }</dd>
                </div>
            </dl>
        </div>
    }
}

// ------------------------------------------------------------------ table --

fn files_table(
    lang: Lang,
    files: &UseStateHandle<Vec<AdminFile>>,
    sort: &UseStateHandle<(SortKey, bool)>,
    query: &UseStateHandle<String>,
    category: &UseStateHandle<Option<String>>,
) -> Html {
    let (key, ascending) = **sort;

    let mut visible: Vec<AdminFile> = files
        .iter()
        .filter(|f| matches_filter(f, query))
        .filter(|f| category.as_ref().is_none_or(|wanted| &f.category == wanted))
        .cloned()
        .collect();
    sort_files(&mut visible, key, ascending);
    let matched = visible.len();
    visible.truncate(TABLE_ROWS);

    let oninput = {
        let query = query.clone();
        Callback::from(move |e: InputEvent| {
            let input: web_sys::HtmlInputElement = e.target_unchecked_into();
            query.set(input.value());
        })
    };

    let chip = |slot: Option<String>, label: String| -> Html {
        let pressed = **category == slot;
        let category = category.clone();
        let value = slot;
        html! {
            <button
                type="button"
                class="fn-dash__chip"
                aria-pressed={pressed.to_string()}
                onclick={Callback::from(move |_: MouseEvent| category.set(value.clone()))}
            >{ label }</button>
        }
    };

    let header = |lang: Lang, label: Key, this: SortKey| -> Html {
        let active = key == this;
        let sort = sort.clone();
        let aria = if !active {
            "none"
        } else if ascending {
            "ascending"
        } else {
            "descending"
        };
        // A repeated click flips the direction; a new column starts with the
        // reading most people want from it (names A→Z, everything else
        // biggest/newest first).
        let onclick = Callback::from(move |_: MouseEvent| {
            let next = if key == this {
                (this, !ascending)
            } else {
                (
                    this,
                    matches!(this, SortKey::Name | SortKey::Room | SortKey::Uploader),
                )
            };
            sort.set(next);
        });
        html! {
            <th aria-sort={aria} scope="col">
                <button type="button" class="fn-dash__sort" {onclick}>
                    { t(lang, label) }
                    if active {
                        <span aria-hidden="true">{ if ascending { " ↑" } else { " ↓" } }</span>
                    }
                </button>
            </th>
        }
    };

    let rows = visible
        .iter()
        .map(|file| {
            html! {
                <tr key={file.id.clone()}>
                    <td class="fn-dash__cell-name">
                        <span class="fn-truncate">{ &file.filename }</span>
                    </td>
                    <td class="fn-nums">{ human_bytes(file.size_bytes as f64) }</td>
                    <td>
                        <span class="fn-dash__kind">
                            { t(lang, category_key(&file.category)) }
                        </span>
                    </td>
                    <td><span class="fn-truncate">{ &file.room_name }</span></td>
                    <td><span class="fn-truncate">{ uploader_label(file) }</span></td>
                    <td class="fn-dash__cell-date fn-nums">
                        { compact_timestamp(file.created_at.as_deref().unwrap_or("")) }
                    </td>
                </tr>
            }
        })
        .collect::<Html>();

    let counted = t(lang, Key::dash_table_count)
        .replace("{shown}", &visible.len().to_string())
        .replace("{total}", &matched.to_string());

    let body = html! {
        <>
            <div class="fn-dash__toolbar">
                <input
                    type="search"
                    class="topcoat-search-input fn-dash__filter"
                    placeholder={t(lang, Key::dash_filter)}
                    aria-label={t(lang, Key::dash_filter)}
                    value={(**query).clone()}
                    {oninput}
                />
                <div class="fn-dash__chips" role="group" aria-label={t(lang, Key::dash_kind)}>
                    { chip(None, t(lang, Key::dash_all_kinds).to_owned()) }
                    { for crate::api::admin::CATEGORY_ORDER.iter().map(|c| {
                        chip(Some((*c).to_owned()), t(lang, category_key(c)).to_owned())
                    }) }
                </div>
            </div>
            if matched == 0 {
                <Empty art="🔍" title={t(lang, Key::dash_no_match)}
                       description={t(lang, Key::dash_no_match_hint)} />
            } else {
                <div class="fn-dash__tablewrap">
                    <table class="fn-dash__table">
                        <thead>
                            <tr>
                                { header(lang, Key::dash_col_name, SortKey::Name) }
                                { header(lang, Key::dash_col_size, SortKey::Size) }
                                { header(lang, Key::dash_kind, SortKey::Kind) }
                                { header(lang, Key::dash_col_room, SortKey::Room) }
                                { header(lang, Key::dash_col_uploader, SortKey::Uploader) }
                                { header(lang, Key::dash_col_date, SortKey::Date) }
                            </tr>
                        </thead>
                        <tbody>{ rows }</tbody>
                    </table>
                </div>
                <p class="fn-dash__quiet fn-nums">{ counted }</p>
            }
        </>
    };
    card(t(lang, Key::dash_all_files), None, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: i64, room: &str, who: Option<&str>, at: &str) -> AdminFile {
        AdminFile {
            id: name.to_owned(),
            filename: name.to_owned(),
            extension: name.rsplit('.').next().unwrap_or("").to_owned(),
            category: "document".to_owned(),
            size_bytes: size,
            room_id: format!("room_{room}"),
            room_name: room.to_owned(),
            uploader: "0xaabbccddeeff00112233445566778899aabbccdd".to_owned(),
            uploader_name: who.map(str::to_owned),
            created_at: Some(format!("{at}T10:00:00.000Z")),
        }
    }

    #[test]
    fn rates_divide_and_refuse_to_divide_by_zero() {
        assert_eq!(rate_label(0, 0), "—");
        assert_eq!(rate_label(1000, 0), "—", "no elapsed time, no rate");
        // 1 MB in 1 s.
        assert_eq!(rate_label(1024 * 1024, 1000), "1.0 MB/s");
        // 9 bytes over 3 s.
        assert_eq!(rate_label(9, 3000), "3 B/s");
    }

    #[test]
    fn growth_fills_the_silent_days() {
        // Noon UTC on 2026-08-07, derived through the same arithmetic the
        // chart uses, so the test pins the windowing rather than a constant.
        let now_ms = parse_day("2026-08-07").unwrap() * 86_400_000 + 43_200_000;
        let sparse = vec![
            GrowthPoint {
                day: "2026-08-05".into(),
                files: 2,
                bytes: 30,
            },
            GrowthPoint {
                day: "2026-08-07".into(),
                files: 1,
                bytes: 10,
            },
            // Before the window: must be dropped, not drawn at slot zero.
            GrowthPoint {
                day: "2026-01-01".into(),
                files: 9,
                bytes: 999,
            },
        ];
        let series = fill_growth(&sparse, now_ms, 30);
        assert_eq!(series.len(), 30);
        assert_eq!(series.first().unwrap().day, "2026-07-09");
        assert_eq!(series.last().unwrap().day, "2026-08-07");
        assert_eq!(series.last().unwrap().bytes, 10);
        assert_eq!(series[27].day, "2026-08-05");
        assert_eq!(series[27].bytes, 30);
        // The silent day between them is present and zero — that is the point.
        assert_eq!(series[28].day, "2026-08-06");
        assert_eq!(series[28].bytes, 0);
        assert_eq!(series.iter().map(|p| p.bytes).sum::<i64>(), 40);
    }

    #[test]
    fn malformed_days_are_dropped_not_misplaced() {
        let now_ms = parse_day("2026-08-07").unwrap() * 86_400_000;
        let bad = vec![GrowthPoint {
            day: "not-a-date".into(),
            files: 1,
            bytes: 50,
        }];
        let series = fill_growth(&bad, now_ms, 7);
        assert_eq!(series.iter().map(|p| p.bytes).sum::<i64>(), 0);
        assert_eq!(parse_day("2026-13-01"), None);
        assert_eq!(parse_day("2026-08-07-extra"), None);
    }

    #[test]
    fn day_arithmetic_round_trips() {
        for day in ["1970-01-01", "2000-02-29", "2026-08-07", "1969-12-31"] {
            let n = parse_day(day).unwrap();
            assert_eq!(day_label(n), day, "round trip failed for {day}");
        }
        assert_eq!(parse_day("1970-01-01"), Some(0));
    }

    #[test]
    fn uptime_reads_like_telemetry() {
        assert_eq!(uptime_label(0), "00m");
        assert_eq!(uptime_label(59), "00m");
        assert_eq!(uptime_label(12 * 60), "12m");
        assert_eq!(uptime_label(3 * 3600 + 4 * 60), "3h 04m");
        assert_eq!(uptime_label(2 * 86400 + 5 * 3600), "2d 05h");
        assert_eq!(
            uptime_label(-5),
            "00m",
            "a clock that ran backwards is not a crash"
        );
    }

    #[test]
    fn message_activity_fills_like_file_growth() {
        let now_ms = parse_day("2026-08-07").unwrap() * 86_400_000;
        let sparse = vec![MessageDay {
            day: "2026-08-06".into(),
            messages: 7,
        }];
        let series = fill_activity(&sparse, now_ms, 7);
        assert_eq!(series.len(), 7);
        assert_eq!(series[5].day, "2026-08-06");
        assert_eq!(series[5].messages, 7);
        assert_eq!(series.iter().map(|p| p.messages).sum::<i64>(), 7);
    }

    #[test]
    fn sorting_is_stable_and_reversible() {
        let mut files = vec![
            file("beta.pdf", 200, "Design", Some("bob"), "2026-08-02"),
            file("Alpha.pdf", 100, "films", Some("alice"), "2026-08-03"),
            file("gamma.pdf", 300, "Archive", None, "2026-08-01"),
        ];
        sort_files(&mut files, SortKey::Name, true);
        let names: Vec<_> = files.iter().map(|f| f.filename.as_str()).collect();
        // Case-insensitive: "Alpha" before "beta".
        assert_eq!(names, ["Alpha.pdf", "beta.pdf", "gamma.pdf"]);

        sort_files(&mut files, SortKey::Size, false);
        assert_eq!(files[0].size_bytes, 300);

        sort_files(&mut files, SortKey::Date, false);
        assert_eq!(files[0].filename, "Alpha.pdf", "newest first");

        // The uploader column falls back to the address when the name is
        // gone, and digits sort ahead of letters — the address row leads.
        sort_files(&mut files, SortKey::Uploader, true);
        assert_eq!(uploader_label(&files[0]), "0xaabb…ccdd");
        assert_eq!(uploader_label(&files[2]), "bob");
    }

    #[test]
    fn the_filter_reads_every_column_a_person_remembers() {
        let f = file(
            "Q3 report.pdf",
            10,
            "Finance",
            Some("goldenMango51"),
            "2026-08-01",
        );
        for hit in ["q3", "REPORT", "pdf", "finance", "mango", "0xaabb"] {
            assert!(matches_filter(&f, hit), "{hit} should match");
        }
        assert!(!matches_filter(&f, "video"));
        assert!(matches_filter(&f, "  "), "whitespace is no filter at all");
    }

    #[test]
    fn timestamps_compact_to_a_readout() {
        assert_eq!(
            compact_timestamp("2026-08-07T12:34:56.789Z"),
            "2026-08-07 12:34"
        );
        assert_eq!(compact_timestamp(""), "");
    }
}
