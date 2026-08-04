//! Display formatting.
//!
//! Everything here is a **pure function of an explicit `now`**, never of the
//! system clock. That is what makes "yesterday" testable on a build machine in
//! any timezone: the component reads the clock once (`now_ms()`) and passes it
//! down, so the formatting logic itself has no ambient state.
//!
//! Calendar maths is done by hand rather than pulled from `chrono`/`time`: the
//! client only ever needs civil date arithmetic on UTC-offset milliseconds, and
//! a date library is ~200 KB of `.wasm` for four functions.

/// Milliseconds in a day.
const DAY_MS: i64 = 86_400_000;

/// Current wall clock in epoch milliseconds.
///
/// Isolated in one place so the rest of the module stays pure and host-testable.
#[cfg(target_arch = "wasm32")]
pub fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

/// Host fallback so non-wasm test binaries link. Never called by rendering code.
#[cfg(not(target_arch = "wasm32"))]
pub fn now_ms() -> i64 {
    0
}

/// The origin this page was loaded from, e.g. `https://100.64.0.1:9099`.
///
/// The *last* resort for building a shareable link — often `127.0.0.1`, which
/// means something different on every machine. See
/// [`crate::state::AppState::shareable_url`].
#[cfg(target_arch = "wasm32")]
pub fn page_origin() -> String {
    web_sys::window()
        .and_then(|w| w.location().origin().ok())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn page_origin() -> String {
    String::new()
}

/// Join a base URL and an absolute path without doubling or dropping the
/// separator, and leave anything already absolute alone.
///
/// The pass-through matters: message content is user-written, so "the link in
/// this message" may already be a full `https://…` URL that has nothing to do
/// with this server. Prefixing a base onto that would produce nonsense.
pub fn join_url(base: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        return path.to_owned();
    }
    let base = base.trim_end_matches('/');
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// The viewer's timezone offset in minutes **east** of UTC (the negation of
/// JavaScript's `getTimezoneOffset`, which is famously backwards).
#[cfg(target_arch = "wasm32")]
pub fn tz_offset_minutes() -> i32 {
    -(js_sys::Date::new_0().get_timezone_offset() as i32)
}

/// Host fallback: UTC. Tests pass the offset explicitly.
#[cfg(not(target_arch = "wasm32"))]
pub fn tz_offset_minutes() -> i32 {
    0
}

/// A civil date/time, already shifted into the viewer's timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    /// Days since the Unix epoch, in local time. The cheap way to ask "same
    /// calendar day?" without re-deriving y/m/d.
    pub epoch_day: i64,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u32,
}

/// Convert epoch milliseconds to a local civil date.
///
/// Uses Howard Hinnant's `civil_from_days` algorithm, which is exact for the
/// whole proleptic Gregorian range and needs no lookup tables.
pub fn civil_from_ms(ms: i64, tz_offset_minutes: i32) -> Civil {
    let local = ms + (tz_offset_minutes as i64) * 60_000;
    // Floor division: negative timestamps (pre-1970) must round *down*.
    let epoch_day = local.div_euclid(DAY_MS);
    let time_of_day = local.rem_euclid(DAY_MS);

    let z = epoch_day + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    Civil {
        year: year as i32,
        month: m as u32,
        day: d as u32,
        hour: (time_of_day / 3_600_000) as u32,
        minute: (time_of_day % 3_600_000 / 60_000) as u32,
        epoch_day,
        // 1970-01-01 was a Thursday, i.e. index 3 in a Monday-first week.
        weekday: (epoch_day + 3).rem_euclid(7) as u32,
    }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
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

/// `HH:MM` in 24-hour form. Timestamps are machine data; a 12-hour clock with
/// am/pm costs three extra characters and buys ambiguity.
pub fn hhmm(ms: i64, tz: i32) -> String {
    let c = civil_from_ms(ms, tz);
    format!("{:02}:{:02}", c.hour, c.minute)
}

/// Room-list timestamp: `HH:MM` today, `Mon` within the last week, `D/M` older
/// (DESIGN.md §6).
pub fn room_list_time(ms: i64, now: i64, tz: i32) -> String {
    let then = civil_from_ms(ms, tz);
    let today = civil_from_ms(now, tz);
    let age_days = today.epoch_day - then.epoch_day;

    if age_days <= 0 {
        format!("{:02}:{:02}", then.hour, then.minute)
    } else if age_days < 7 {
        WEEKDAYS[then.weekday as usize].to_owned()
    } else {
        format!("{}/{}", then.day, then.month)
    }
}

/// Day-marker label for the message stream: "Today", "Yesterday", then
/// `D MMMM YYYY` (DESIGN.md §7.2).
pub fn day_marker(ms: i64, now: i64, tz: i32) -> String {
    let then = civil_from_ms(ms, tz);
    let today = civil_from_ms(now, tz);
    match today.epoch_day - then.epoch_day {
        d if d <= 0 => "Today".to_owned(),
        1 => "Yesterday".to_owned(),
        _ => format!(
            "{} {} {}",
            then.day,
            MONTHS[(then.month - 1) as usize],
            then.year
        ),
    }
}

/// Coarse relative time for invitation cards ("2 hours ago", "yesterday").
///
/// Deliberately coarse: an invitation is not time-critical, and "just now" for
/// anything under a minute reads better than "0 minutes ago".
pub fn relative_time(ms: i64, now: i64) -> String {
    let delta = now.saturating_sub(ms);
    if delta < 0 {
        // A clock skew between server and client should not print "in -3 hours".
        return "just now".to_owned();
    }
    let mins = delta / 60_000;
    let hours = delta / 3_600_000;
    let days = delta / DAY_MS;
    match (mins, hours, days) {
        (m, _, _) if m < 1 => "just now".to_owned(),
        (1, _, _) => "1 minute ago".to_owned(),
        (m, 0, _) => format!("{m} minutes ago"),
        (_, 1, _) => "1 hour ago".to_owned(),
        (_, h, 0) => format!("{h} hours ago"),
        (_, _, 1) => "yesterday".to_owned(),
        (_, _, d) if d < 30 => format!("{d} days ago"),
        (_, _, d) => format!("{} months ago", d / 30),
    }
}

/// A stopwatch reading for something happening *now* — an AI generation in
/// flight, where the number is the whole point.
///
/// Counts in seconds up to a minute and then in `m:ss`, and never rounds away
/// the seconds: this is read while waiting, and a display that sat on
/// "1 minute" for sixty seconds would look stopped. Language-free by design,
/// so it needs no translation to be correct.
pub fn elapsed_clock(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// `YYYY-MM-DD`, for "blocked 14 Mar"-style secondary rows where a precise date
/// beats a fuzzy interval.
pub fn short_date(ms: i64, tz: i32) -> String {
    let c = civil_from_ms(ms, tz);
    format!("{} {}", c.day, &MONTHS[(c.month - 1) as usize][..3])
}

/// Unread badge text. Anything above 99 becomes `99+` so the pill keeps a fixed
/// width and the row never reflows (DESIGN.md §6).
pub fn unread_badge(n: u32) -> String {
    if n > 99 {
        "99+".to_owned()
    } else {
        n.to_string()
    }
}

/// The accessible name for an unread count: screen readers must hear
/// "12 unread messages", never a bare "12" (DESIGN.md §17).
pub fn unread_label(n: u32) -> String {
    match n {
        1 => "1 unread message".to_owned(),
        n => format!("{n} unread messages"),
    }
}

/// Parse an ISO-8601 UTC timestamp of the shape the API emits
/// (`2025-06-11T14:39:06.000Z`) into epoch milliseconds.
///
/// Hand-rolled rather than delegated to `Date.parse` so it is testable on the
/// host and so a malformed value yields `None` instead of `NaN` propagating
/// through arithmetic. Only the exact server shape is accepted; anything else
/// is a protocol violation we would rather notice than paper over.
pub fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s.get(from..to)?.parse::<i64>().ok() };

    let year = num(0, 4)?;
    let month = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let minute = num(14, 16)?;
    let second = num(17, 19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let millis = if b.get(19) == Some(&b'.') {
        num(20, 23).unwrap_or(0)
    } else {
        0
    };

    // days_from_civil, the inverse of the algorithm in `civil_from_ms`.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    Some(days * DAY_MS + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
}

/// Join a list of names the way English expects, for the typing indicator.
pub fn typing_label(names: &[String]) -> Option<String> {
    match names.len() {
        0 => None,
        1 => Some(format!("{} is typing…", names[0])),
        2 | 3 => Some(format!("{} are typing…", names.join(", "))),
        n => Some(format!("{n} people are typing…")),
    }
}

/// Truncate a preview line without splitting a UTF-8 scalar.
///
/// `String::truncate` panics on a non-boundary index; a message preview is the
/// last place we want a panic, because the content is entirely attacker-chosen.
pub fn preview(text: &str, max_chars: usize) -> String {
    let mut out: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect();
    if text.chars().filter(|c| !c.is_control()).count() > max_chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2025-06-11T14:39:06.000Z
    const T: i64 = 1_749_652_746_000;

    #[test]
    fn iso8601_round_trips_against_the_civil_conversion() {
        let ms = parse_iso8601_ms("2025-06-11T14:39:06.000Z").unwrap();
        let c = civil_from_ms(ms, 0);
        assert_eq!(
            (c.year, c.month, c.day, c.hour, c.minute),
            (2025, 6, 11, 14, 39)
        );
    }

    #[test]
    fn iso8601_rejects_garbage_instead_of_guessing() {
        for bad in ["", "not a date", "2025/06/11", "2025-06-11", "20250611T00Z"] {
            assert!(parse_iso8601_ms(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn iso8601_handles_the_epoch_and_a_leap_day() {
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00.000Z"), Some(0));
        let leap = parse_iso8601_ms("2024-02-29T12:00:00.000Z").unwrap();
        let c = civil_from_ms(leap, 0);
        assert_eq!((c.year, c.month, c.day), (2024, 2, 29));
    }

    #[test]
    fn a_shareable_link_joins_cleanly_and_leaves_absolute_urls_alone() {
        assert_eq!(
            join_url("http://100.64.0.7:9099", "/api/images/a.png"),
            "http://100.64.0.7:9099/api/images/a.png"
        );
        // A trailing slash on the base, or a missing leading slash on the
        // path, must not produce `//` or a glued-together host.
        assert_eq!(join_url("https://host/", "/a"), "https://host/a");
        assert_eq!(join_url("https://host", "a"), "https://host/a");

        // Already absolute: a URL somebody typed into a message belongs to
        // whatever host it names, not to this server.
        assert_eq!(
            join_url("https://host", "https://example.com/x.png"),
            "https://example.com/x.png"
        );
        assert_eq!(
            join_url("https://host", "http://example.com/x.png"),
            "http://example.com/x.png"
        );

        // No base at all (loopback-only server, non-wasm build): the path is
        // left as it was rather than turned into something wrong.
        assert_eq!(join_url("", "/api/images/a.png"), "/api/images/a.png");
    }

    #[test]
    fn the_elapsed_clock_keeps_counting_seconds_past_a_minute() {
        assert_eq!(elapsed_clock(0), "0s");
        assert_eq!(elapsed_clock(9), "9s");
        assert_eq!(elapsed_clock(59), "59s");
        // The seconds must keep moving after a minute — a reading that sat on
        // "1 minute" for sixty seconds would look like a hang, which is the
        // exact thing this display exists to rule out.
        assert_eq!(elapsed_clock(60), "1:00");
        assert_eq!(elapsed_clock(61), "1:01");
        assert_eq!(elapsed_clock(599), "9:59");
    }

    #[test]
    fn civil_conversion_survives_pre_epoch_timestamps() {
        // Floor division, not truncation: 1969-12-31T23:00Z must not land on
        // 1970-01-01.
        let c = civil_from_ms(-3_600_000, 0);
        assert_eq!((c.year, c.month, c.day, c.hour), (1969, 12, 31, 23));
        assert_eq!(c.epoch_day, -1);
    }

    #[test]
    fn weekday_index_is_monday_first() {
        // 1970-01-01 was a Thursday.
        assert_eq!(civil_from_ms(0, 0).weekday, 3);
        assert_eq!(WEEKDAYS[civil_from_ms(0, 0).weekday as usize], "Thu");
        // 2025-06-11 was a Wednesday.
        assert_eq!(WEEKDAYS[civil_from_ms(T, 0).weekday as usize], "Wed");
    }

    #[test]
    fn timezone_offset_shifts_the_calendar_day_not_just_the_clock() {
        // 2025-06-11T14:39Z is 2025-06-11 23:39 in UTC+9 …
        let c = civil_from_ms(T, 9 * 60);
        assert_eq!((c.day, c.hour), (11, 23));
        // … and 2025-06-12 00:39 in UTC+10, a different calendar day.
        let c = civil_from_ms(T, 10 * 60);
        assert_eq!((c.day, c.hour), (12, 0));
    }

    #[test]
    fn room_list_time_switches_format_at_the_documented_boundaries() {
        assert_eq!(room_list_time(T, T + 60_000, 0), "14:39");
        // Same day, much later in the day: still a clock time.
        assert_eq!(room_list_time(T, T + 8 * 3_600_000, 0), "14:39");
        // Yesterday relative to now → weekday name.
        assert_eq!(room_list_time(T, T + DAY_MS, 0), "Wed");
        assert_eq!(room_list_time(T, T + 6 * DAY_MS, 0), "Wed");
        // A week or more → D/M.
        assert_eq!(room_list_time(T, T + 7 * DAY_MS, 0), "11/6");
    }

    #[test]
    fn day_markers_read_as_words_before_they_read_as_dates() {
        assert_eq!(day_marker(T, T, 0), "Today");
        assert_eq!(day_marker(T, T + DAY_MS, 0), "Yesterday");
        assert_eq!(day_marker(T, T + 2 * DAY_MS, 0), "11 June 2025");
    }

    #[test]
    fn day_marker_uses_calendar_days_not_elapsed_hours() {
        // 23:59 → 00:01 is two minutes but a different day, and must say so.
        let late = parse_iso8601_ms("2025-06-11T23:59:00.000Z").unwrap();
        let early = parse_iso8601_ms("2025-06-12T00:01:00.000Z").unwrap();
        assert_eq!(day_marker(late, early, 0), "Yesterday");
    }

    #[test]
    fn relative_time_is_coarse_and_never_negative() {
        assert_eq!(relative_time(T, T), "just now");
        assert_eq!(relative_time(T, T + 30_000), "just now");
        assert_eq!(relative_time(T, T + 60_000), "1 minute ago");
        assert_eq!(relative_time(T, T + 5 * 60_000), "5 minutes ago");
        assert_eq!(relative_time(T, T + 3_600_000), "1 hour ago");
        assert_eq!(relative_time(T, T + 2 * 3_600_000), "2 hours ago");
        assert_eq!(relative_time(T, T + DAY_MS), "yesterday");
        assert_eq!(relative_time(T, T + 3 * DAY_MS), "3 days ago");
        assert_eq!(relative_time(T, T + 90 * DAY_MS), "3 months ago");
        // Server clock ahead of ours must not print a negative interval.
        assert_eq!(relative_time(T + 10_000, T), "just now");
    }

    #[test]
    fn unread_badge_caps_so_the_pill_never_reflows() {
        assert_eq!(unread_badge(0), "0");
        assert_eq!(unread_badge(99), "99");
        assert_eq!(unread_badge(100), "99+");
        assert_eq!(unread_badge(u32::MAX), "99+");
    }

    #[test]
    fn unread_label_is_a_sentence_not_a_number() {
        assert_eq!(unread_label(1), "1 unread message");
        assert_eq!(unread_label(12), "12 unread messages");
    }

    #[test]
    fn typing_label_follows_the_spec_thresholds() {
        let n = |s: &[&str]| s.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(typing_label(&[]), None);
        assert_eq!(typing_label(&n(&["ann"])).unwrap(), "ann is typing…");
        assert_eq!(
            typing_label(&n(&["ann", "bo"])).unwrap(),
            "ann, bo are typing…"
        );
        assert_eq!(
            typing_label(&n(&["ann", "bo", "cy"])).unwrap(),
            "ann, bo, cy are typing…"
        );
        assert_eq!(
            typing_label(&n(&["ann", "bo", "cy", "di"])).unwrap(),
            "4 people are typing…"
        );
    }

    #[test]
    fn preview_never_splits_a_scalar_and_strips_control_characters() {
        assert_eq!(preview("hello", 10), "hello");
        assert_eq!(preview("hello world", 5), "hello…");
        // Multi-byte input at the truncation boundary.
        assert_eq!(preview("한글메시지입니다", 4), "한글메시…");
        // A newline in a preview would break the single-line row layout.
        assert_eq!(preview("a\nb\tc", 10), "abc");
    }

    #[test]
    fn short_date_is_stable() {
        assert_eq!(short_date(T, 0), "11 Jun");
    }

    #[test]
    fn hhmm_zero_pads_both_fields() {
        let ms = parse_iso8601_ms("2025-01-02T03:04:05.000Z").unwrap();
        assert_eq!(hhmm(ms, 0), "03:04");
    }
}
