//! Inline SVG icons.
//!
//! No icon font, no sprite sheet, no runtime fetch: an icon font is a second
//! network request and a flash of missing glyphs, and this client's whole
//! premise is cold-start speed. Every icon is `stroke="currentColor"` so it
//! inherits state colour from its button without a second class.

use yew::prelude::*;

/// The shared wrapper: `viewBox="0 0 24 24"`, stroke 2.4, round caps
/// (DESIGN.md §2.6). `aria-hidden` always — an icon-only button carries its
/// meaning in `aria-label`, and a labelled icon inside it would be read twice.
fn icon(size: u16, children: Html) -> Html {
    html! {
        <svg
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            width={size.to_string()}
            height={size.to_string()}
            fill="none"
            stroke="currentColor"
            stroke-width="2.4"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden="true"
            focusable="false"
        >
            { children }
        </svg>
    }
}

macro_rules! icons {
    ($($name:ident => $body:expr;)*) => {
        $(
            #[allow(dead_code)]
            pub fn $name(size: u16) -> Html {
                icon(size, $body)
            }
        )*
    };
}

icons! {
    plus => html! { <><path d="M12 5v14"/><path d="M5 12h14"/></> };
    search => html! { <><circle cx="11" cy="11" r="7"/><path d="m20 20-3.2-3.2"/></> };
    // An open book — two pages and a spine. The closed cover this replaced was
    // a rounded rectangle whose only interior stroke (the shelf lip) landed on
    // its own bottom edge, so at 18px it merged into a blank square and people
    // could not find the Knowledge screen behind it. The curved page edges and
    // the centre spine keep it clear of `columns`, the other two-part glyph in
    // the same row: hard rectangles with a gap, against soft lobes with a line.
    book => html! { <><path d="M12 7.5v11.9"/><path d="M12 7.5c-1.9-1.7-4.4-2.4-7.5-2.2v11.9c3.1-.2 5.6.5 7.5 2.2"/><path d="M12 7.5c1.9-1.7 4.4-2.4 7.5-2.2v11.9c-3.1-.2-5.6.5-7.5 2.2"/></> };
    bank => html! { <><path d="M3 9.5 12 4l9 5.5"/><path d="M4.5 10v7"/><path d="M9.2 10v7"/><path d="M14.8 10v7"/><path d="M19.5 10v7"/><path d="M3 20h18"/></> };
    // Two stacked rack units with an activity lamp on each — the shape people
    // read as "a server", rather than a cloud, which reads as somebody else's.
    server => html! { <><rect x="3" y="4" width="18" height="7" rx="1.5"/><rect x="3" y="13" width="18" height="7" rx="1.5"/><path d="M7 7.5h.01"/><path d="M7 16.5h.01"/></> };
    back => html! { <><path d="M15 5 8 12l7 7"/></> };
    close => html! { <><path d="M6 6l12 12"/><path d="M18 6 6 18"/></> };
    check => html! { <><path d="m5 13 4 4L19 7"/></> };
    refresh => html! { <><path d="M20 11a8 8 0 1 0-2.3 6"/><path d="M20 5v6h-6"/></> };
    more => html! { <><circle cx="12" cy="5" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="12" cy="19" r="1.4"/></> };
    // The same three dots lying down. `more` (upright) is the row-level "act on
    // this thing" menu; this one is the bottom nav's fifth tab, where the dots
    // sit above a label and an upright glyph would read as a column of bullets.
    ellipsis => html! { <><circle cx="5" cy="12" r="1.4"/><circle cx="12" cy="12" r="1.4"/><circle cx="19" cy="12" r="1.4"/></> };
    smile => html! { <><circle cx="12" cy="12" r="9"/><path d="M8.5 14.5a4.5 4.5 0 0 0 7 0"/><path d="M9 9.5h.01"/><path d="M15 9.5h.01"/></> };
    send => html! { <><path d="M4 12 20 4l-8 16-2.2-6.2L4 12Z"/></> };
    lock => html! { <><rect x="4.5" y="10.5" width="15" height="10" rx="2"/><path d="M8 10.5V7.5a4 4 0 0 1 8 0v3"/></> };
    // A shield with a tick: "this file is what it says it is". Distinct from
    // `check` alone, which already means "done" on buttons all over the app —
    // a verification is a claim about integrity, not about completion.
    shield => html! { <><path d="M12 3.5 5 6.2v5.4c0 4 2.8 7.6 7 8.9 4.2-1.3 7-4.9 7-8.9V6.2L12 3.5Z"/><path d="m9 12 2.2 2.2L15.5 10"/></> };
    crown => html! { <><path d="M4 8l3.5 4L12 5l4.5 7L20 8l-1.5 10h-13L4 8Z"/></> };
    envelope => html! { <><rect x="3" y="5.5" width="18" height="13" rx="2"/><path d="m3.5 7 8.5 6 8.5-6"/></> };
    people => html! { <><circle cx="9" cy="8" r="3.4"/><path d="M3 19a6 6 0 0 1 12 0"/><path d="M16 5.4a3.4 3.4 0 0 1 0 6.6"/><path d="M18 19a6 6 0 0 0-2-4.3"/></> };
    // Sliders, not a spoked cog: at 18px a stroked cog is a circle with eight
    // ticks — which is exactly what the `moon_sun` appearance toggle is, and
    // the two sat four buttons apart in the top bar reading as twins. Three
    // faders cannot be mistaken for a sun.
    gear => html! { <><path d="M5 21v-6M5 11V3"/><path d="M12 21v-9M12 8V3"/><path d="M19 21v-4M19 13V3"/><path d="M2.5 13h5M9.5 10h5M16.5 15h5"/></> };
    chat => html! { <><path d="M20.5 12.5a8 8 0 0 1-11.6 7.1L4 21l1.5-4.6A8 8 0 1 1 20.5 12.5Z"/></> };
    power => html! { <><path d="M12 4v8"/><path d="M7.5 7.2a7 7 0 1 0 9 0"/></> };
    spark => html! { <><path d="M12 3l1.9 5.6L19.5 10l-5.6 1.9L12 17.5l-1.9-5.6L4.5 10l5.6-1.4L12 3Z"/><path d="M18.5 15.5l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8.8-2.2Z"/></> };
    trash => html! { <><path d="M4.5 7h15"/><path d="M9.5 7V5h5v2"/><path d="M6.5 7l1 13h9l1-13"/></> };
    warn => html! { <><path d="M12 4 2.5 20h19L12 4Z"/><path d="M12 10v5"/><path d="M12 17.6v.4"/></> };
    copy => html! { <><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h8"/></> };
    eye => html! { <><path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z"/><circle cx="12" cy="12" r="3"/></> };
    eye_off => html! { <><path d="M4 4l16 16"/><path d="M9.5 6.2A9.4 9.4 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-3.3 4"/><path d="M6.4 8.2A17 17 0 0 0 2.5 12S6 18.5 12 18.5a9.3 9.3 0 0 0 3.4-.6"/></> };
    ban => html! { <><circle cx="12" cy="12" r="8.5"/><path d="m6 6 12 12"/></> };
    minus_circle => html! { <><circle cx="12" cy="12" r="8.5"/><path d="M8 12h8"/></> };
    external => html! { <><path d="M14 4h6v6"/><path d="m20 4-8.5 8.5"/><path d="M18 14v4a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h4"/></> };
    download => html! { <><path d="M12 4v10"/><path d="m8 11 4 4 4-4"/><path d="M4 19h16"/></> };
    upload => html! { <><path d="M12 20V9"/><path d="m8 12 4-4 4 4"/><path d="M4 5h16"/></> };
    bolt => html! { <><path d="M13.5 3 5 13.5h5.5L10 21l8.5-10.5H13l.5-7.5Z"/></> };
    // A megaphone: the horn as one wedge, sound rings leaving it. The paid
    // broadcast's mark everywhere — top bar, dialog, banner.
    megaphone => html! { <><path d="M3 10v4a1 1 0 0 0 1 1h2l3 4h2v-5"/><path d="M6 10l9-5v14l-6.6-3.7"/><path d="M19 9a4 4 0 0 1 0 6"/></> };
    // A paperclip drawn as one open curve rather than the usual closed loop:
    // at 18px the loop fills in and reads as a blob.
    paperclip => html! { <><path d="M20 11.5l-7.8 7.8a4.6 4.6 0 0 1-6.5-6.5l8.1-8.1a3 3 0 0 1 4.3 4.3l-8.1 8.1a1.5 1.5 0 0 1-2.1-2.1l7.3-7.3"/></> };
    // A stack of plates, matching the empty-files artwork.
    files => html! { <><path d="M4 7.5 12 4l8 3.5-8 3.5-8-3.5Z"/><path d="m4 12 8 3.5 8-3.5"/><path d="m4 16.5 8 3.5 8-3.5"/></> };
    globe => html! { <><circle cx="12" cy="12" r="8.5"/><path d="M3.5 12h17"/><path d="M12 3.5c2.4 2.6 3.6 5.4 3.6 8.5S14.4 18.4 12 20.5c-2.4-2.1-3.6-5.4-3.6-8.5S9.6 6.1 12 3.5Z"/></> };
    moon_sun => html! { <><circle cx="12" cy="12" r="4.2"/><path d="M12 2.8v2M12 19.2v2M2.8 12h2M19.2 12h2M5.6 5.6 7 7M17 17l1.4 1.4M18.4 5.6 17 7M7 17l-1.4 1.4"/></> };
    // A dollar sign, not a billfold. The button opens balances and sends
    // value, and at 18px a card-with-a-stripe reads as "settings" or "ID" as
    // readily as it reads as "money" — the currency glyph is unambiguous at
    // any size and in any locale that has met a price tag. Drawn as strokes
    // rather than set as text so it inherits `currentColor` and the same
    // 2.4 stroke weight as every sibling icon, instead of arriving as a
    // font-dependent character that shifts with the user's type settings.
    wallet => html! { <><path d="M12 3.2v17.6"/><path d="M16.2 7.1c-.9-1.2-2.4-1.9-4.2-1.9-2.4 0-4.2 1.2-4.2 3s1.6 2.6 4.2 3.1c2.6.5 4.4 1.3 4.4 3.2s-1.9 3.2-4.4 3.2c-2 0-3.6-.8-4.4-2.1"/></> };
    // `moon_sun` above is the sun on its own; this is its counterpart, so the
    // appearance switch can show the two choices rather than one glyph that
    // means "appearance" and tells you nothing about which one you are on.
    moon => html! { <><path d="M20.5 14.6A8.6 8.6 0 0 1 9.4 3.5a8.6 8.6 0 1 0 11.1 11.1Z"/></> };
    // The two arrangements, drawn as what they are: panels stacked, and panels
    // side by side.
    rows => html! { <><rect x="3.5" y="4" width="17" height="6.4" rx="1.6"/><rect x="3.5" y="13.6" width="17" height="6.4" rx="1.6"/></> };
    columns => html! { <><rect x="4" y="3.5" width="6.4" height="17" rx="1.6"/><rect x="13.6" y="3.5" width="6.4" height="17" rx="1.6"/></> };
    // Two counter-flowing arrows: the swap tab. Horizontal, because the form
    // below it reads "from → to" left to right.
    swap => html! { <><path d="M4 8h13"/><path d="m14 4.5 3.5 3.5L14 11.5"/><path d="M20 16H7"/><path d="m10 12.5L6.5 16l3.5 3.5"/></> };
    // Two overlapping discs: tokens. The classic "coins" at stroke weight.
    coins => html! { <><circle cx="9" cy="9.5" r="5.5"/><path d="M14.8 6.6a5.5 5.5 0 1 1-5.6 9.3"/></> };
    // The machine teller: a visor head with one optic band — deliberately the
    // product's endoskeleton iconography rather than a friendly bellhop.
    robot => html! { <><rect x="5" y="7.5" width="14" height="11" rx="2.5"/><path d="M12 7.5V4"/><path d="M8.5 12.5h.01"/><path d="M15.5 12.5h.01"/><path d="M9 15.8h6"/></> };
    // An opening quotation mark: the Greeter demo contract speaks.
    quote => html! { <><path d="M9.5 7.5c-2.6.8-4 2.6-4 5v4h4.6v-4.6H7.6c0-1.4.8-2.4 2.3-3z"/><path d="M18.5 7.5c-2.6.8-4 2.6-4 5v4h4.6v-4.6h-2.5c0-1.4.8-2.4 2.3-3z"/></> };
    // A large and a small A: type size. Text drawn as strokes so it scales
    // and recolours like every sibling.
    type_size => html! { <><path d="m3.5 18 4.5-11 4.5 11"/><path d="M5.4 14.4h5.2"/><path d="m14.5 18 3-7 3 7"/><path d="M15.9 15.6h3.2"/></> };
    // A capital T on a baseline: the typeface picker.
    type_face => html! { <><path d="M5 6.5V4.5h14v2"/><path d="M12 4.5V19"/><path d="M9 19h6"/></> };
    // A painter's palette: the skin picker. Deliberately not a second
    // brightness glyph — `moon_sun` next door already owns that idea, and two
    // rows in the same list whose icons both mean "how it looks" is how a
    // reader ends up clicking the wrong one. Three wells rather than four:
    // at 18px a fourth becomes a smudge.
    palette => html! { <><path d="M12 3.2a8.8 8.8 0 0 0 0 17.6 1.9 1.9 0 0 0 1.5-3.1 1.9 1.9 0 0 1 1.5-3.1h1.7a4 4 0 0 0 4-4.2c-.4-4-4-7-8.7-7z"/><path d="M7.4 12.2h.01"/><path d="M9.6 8.2h.01"/><path d="M14.2 7.6h.01"/></> };
}
