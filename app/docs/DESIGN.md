# PocketSkynet — UI Design Specification

Screen-by-screen specification for the PocketSkynet web client: **Rust → WASM (Yew)**,
styled with **Topcoat CSS** plus one project stylesheet, `web/static/app.css`.

This document is the contract between design and the Yew implementation. Every class name
used below exists in `web/static/app.css` or in the vendored Topcoat sheet. If they
disagree, `app.css` wins.

**Source material.** Behaviour is derived from the existing FruitNation React client
(`server/client/src/`) and from `server/PROTOCOL.md`. Note that
`server/client/UI.md` is stale — it describes an orange primary that the React code has
since drifted away from (the React app now runs sky-blue). PocketSkynet deliberately
returns to the **deep-orange** identity described in that spec, so read `UI.md` for screen
structure, not for colour.

---

## Contents

1. [Design direction](#1-design-direction)
2. [Design system](#2-design-system)
3. [Identity tile algorithm](#3-identity-tile-algorithm)
4. [App shell, navigation and routing](#4-app-shell-navigation-and-routing)
5. [Screen 1 — Login](#5-screen-1--login)
6. [Screen 2 — Room list](#6-screen-2--room-list)
7. [Screen 3 — Chat view](#7-screen-3--chat-view)
8. [Screen 4 — Create room](#8-screen-4--create-room)
9. [Screen 5 — Invite](#9-screen-5--invite)
10. [Screen 6 — Members & admins](#10-screen-6--members--admins)
11. [Screen 7 — Invitations inbox](#11-screen-7--invitations-inbox)
12. [Screen 8 — Blocked users](#12-screen-8--blocked-users)
13. [Screen 9 — Settings & profile](#13-screen-9--settings--profile)
14. [Screen 10 — Not found](#14-screen-10--not-found)
15. [Cross-cutting components](#15-cross-cutting-components)
16. [Responsive rules](#16-responsive-rules)
17. [Accessibility](#17-accessibility)
18. [Class name index for Yew](#18-class-name-index-for-yew)
19. [Screen 11 — Bank](#19-screen-11--bank-2026-07-31)

---

## 1. Design direction

> **Superseded, 2026-07-27.** This section originally described a light, flat,
> Topcoat-native look built around fruit-emoji avatars inherited from
> FruitNation. That direction has been replaced. Sections 2 onward remain
> useful for screen structure, states and interaction; read them for *what a
> screen does*, not for how it looks.

**Thesis: a fun Skynet, delivered like Netflix.**

PocketSkynet is a machine that watches a network and tells you what happened on
it — and turns out to be friendly. The interface should carry that: a cold,
quiet, technical shell around warm content. The delivery model is Netflix's,
because Netflix solved exactly this problem — a near-black chrome that recedes
completely so the content is the only thing with colour.

Four rules, in priority order:

1. **Dark first.** Near-black surfaces are the default, not a preference read
   from the OS. Light is a correct, deliberate alternative, not an afterthought.
2. **One accent.** Amber-orange, and only on the primary action and the live
   indicator. Treat it as an HUD glow — a thin luminous rule, a soft bloom —
   rather than a fill sprayed across badges and pills. A colour never does two
   jobs, and most elements should have no colour at all.
3. **No hairlines.** Separation comes from contrast, spacing and elevation.
   Bordered boxes are what made the old design read as an unstyled form.
4. **Fast.** 150–250ms, ease-out, transform and opacity only. Perceived speed is
   part of the aesthetic: nothing should feel like it eases in slowly. The one
   exception is the ambient float on empty-state art.

**Machine readout as an aesthetic.** Wallet addresses, message hashes and
serials are already monospace out of necessity. Lean into it: they are the
product's telemetry, and rendering them as a deliberate readout — rather than as
text that happened to fall back to mono — is most of what makes the shell feel
like an instrument. Sparing corner-bracket framing on the focused element reads
as a targeting HUD at the cost of a few hairlines; anything heavier is costume.

**Every address on screen copies itself.** An address is the one thing in this product
that is nearly always on its way somewhere else — a send form, an invite field, another
person — and forty-two mono characters is not something anyone selects by hand on a
phone. So `<Addr>` is a copy control by default, everywhere it appears: the member row,
the message header, the invite picker, a `0x…` typed into a message. It is a `<span
role="button">` rather than a real button because an address inside a bubble is part of
a sentence, and an atomic inline-block takes a line of its own and pushes the rest of
the sentence onto the next one. Copying anywhere in the app answers with the same
sentence, from `common::copy_with_toast`. Opt out with `copy=false` only where the
address sits *inside* another control — the top bar's profile button — because nesting
one is markup no browser has an answer for.

**The ledger gutter stays.** Every message carries an 8-character `msgHash` slug
beneath it, turning to the security colour with a check once published on-chain.
It reads as a receipt stub, and no other chat app has it. Its recession is done
with colour rather than opacity — 55% alpha measured 2.1:1 and failed contrast.

**Identity is a tile, not a fruit.** A wallet address is 42 characters nobody
can read, so every identity needs a glyph you recognise before you read
anything. That requirement survives; the fruit does not. Identity is now a
Netflix-style profile tile: a rounded square carrying a monogram, on a colour
derived deterministically from the address, so the same account always looks the
same and two accounts never collide. See §3.

**Two deliberate corrections to the React client** are retained: key rotation is
made visible rather than silent, and offline is a real state with a banner and a
send queue. Both are described in the sections below.

## 2. Design system

### 2.1 Base sheet and load order

```
web/static/
  topcoat-desktop-light.css   ← vendored, unmodified
  app.css                     ← this project's sheet
```

`topcoat-desktop-light.css` is the **only** theme sheet loaded, in both light and dark
mode. `app.css` re-skins Topcoat's controls from custom properties, so dark mode is a token
flip rather than a second stylesheet — one HTTP request fewer, no flash on load, and no
theme-swap logic in Rust.

Do **not** load `topcoat-desktop-dark.css`; it would fight §3 of `app.css`. The
`topcoat-mobile-*` sheets are also unused — the mobile layout here is the same controls at
larger hit targets (`@media (pointer: coarse)` in `app.css` §14), not a different sheet.

Dark mode resolves via `light-dark()` + `color-scheme`, which means:

| `<html>` attribute | Result |
| --- | --- |
| *(none)* | Follows `prefers-color-scheme` |
| `data-theme="light"` | Forced light |
| `data-theme="dark"` | Forced dark |

The Yew theme toggle sets/removes that one attribute and persists it to `localStorage`
under `ps-theme`. Nothing else changes.

### 2.1.1 Skins

There is a **second, independent** attribute. `data-theme` answers *how bright is the
room*; `data-skin` answers *what does the product look like*.

| `<html>` attribute | Result |
| --- | --- |
| *(none)* | `skynet` — machine cinema; `app.css` §1 |
| `data-skin="cuteskynet"` | Friendly mecha; `app.css` §1b |
| `data-skin="humanskynet"` | A guardian you cannot tell from a person; `app.css` §1c |

They compose: every skin has a light and a dark face, so the two axes give six
appearances from one sheet. The picker is on the login screen and in Settings, and the
choice persists under `ps-skin`.

The three are deliberately an argument rather than three palettes. `skynet` shows you
the machine — torn skin, chrome under it, a lit optic where an eye should be.
`cuteskynet` shows you a toy. `humanskynet` shows you neither: same guardian, same job,
and nothing in the picture gives her away. That premise is what decides its tokens —
the machine is not allowed to be on her, so it moves into the environment, which is why
cyan is *screen-light* in that skin (`--fn-info`, `--fn-glow`, the room insignia) and
the brand is the indigo of the room. Reusing §1's cyan for the accent would have made
it the machine skin with different pictures; the colour of the button you are about to
press is the first thing anyone reads off an interface.

A skin is a **block of token overrides**, never a second stylesheet. Sections 2–18 of
`app.css` are written entirely against `--fn-*`, so restating the tokens is what makes a
skin complete — and a rule added to §7 next month is themed by every skin the day it
lands. The rule for anyone editing below §1b: *a hex value or a pixel radius outside
§1/§1b/§1c is a bug.* Two of them were, and the second skin is what exposed them — the
CTA fill and the whole sign-in cold open had been literal `hsl(190 …)` since they were
written, invisible for as long as the only accent happened to be that same cyan.

"Complete" is checkable and worth checking: §1b and §1c restate the same 119 tokens, and
a skin that restates 118 of them inherits one value from the machine skin in a place
nobody will look. `--fn-info-fill` in §1c is 5% darker than its `--fn-info` for exactly
this kind of reason — a fill carries white ink and owes 4.5:1, where the same colour as
a mark owes only 3:1.

Imagery is the other half. CSS cannot build a `url()` from parts, so every illustration
is named once in the §1.1 art registry (`--img-*`) and a skin repoints the entries it
redraws; `web/src/asset.rs::img` does the same job for `<img>` elements. A skin overrides
only what it actually ships and everything else falls back to the base artwork, which is
what lets a new skin start with a dozen pictures instead of sixty-two.

Two things that look like conveniences and are not:

* **The pre-paint script in `index.html`.** Both attributes are stamped from
  `localStorage` before the body paints. The WASM bundle applies them too, but that is a
  second or more after first paint — long enough to show the *other* skin's loading
  screen every time the app opens.
* **`asset.rs` as the only place a `/static/img/…` URL is built.** The literals it
  replaced were each individually correct; what they could not be was correct *for the
  skin in effect*, and that failure renders as the wrong picture rather than as an error.

Generating a skin's art: `make assets-cute` / `make assets-human`, or
`tools/genart.py --skin <name>`. Each skin's prompt table in `genart.py` and its array in
`asset.rs` are one contract in two files, and `cargo test` fails if they drift.

Prompts per skin are written out in full rather than derived from the base one with a
style clause: "a chrome endoskeleton skull, but cute" produces a chrome endoskeleton
skull. The subject has to change too. `humanskynet` needs this more than either — the
absence of chrome is the whole skin, and a prompt that merely omits it gets it anyway, so
every portrait says *no chrome, no seams, no implants* out loud.

One rule survives every art direction: **operators are somebody, rooms are somewhere**,
and at 40px the silhouette alone has to say which. `skynet` draws that as a half-human
face against a machine sigil, `cuteskynet` as a helmeted pilot against an enamel badge,
`humanskynet` as a lit human portrait against a flat glowing insignia. Different
vocabulary, same distinction — an emblem has no gaze, and a gaze is what makes a picture
somebody.

### 2.2 Colour tokens

All are CSS custom properties on `:root`. Light value first, dark second.

| Token | Light | Dark | Used for |
| --- | --- | --- | --- |
| `--fn-primary` | `hsl(24 95% 50%)` | same | CTA fill, selected room rail, own bubble |
| `--fn-primary-hover` | `hsl(24 95% 45%)` | same | CTA hover |
| `--fn-primary-active` | `hsl(24 92% 40%)` | same | CTA press, own-bubble border |
| `--fn-primary-ink` | `hsl(24 100% 99%)` | same | Text on orange |
| `--fn-primary-soft` | `orange /.10` | `orange /.16` | Selected row wash, focus halo |
| `--fn-bg` | `hsl(220 14% 96%)` | `hsl(224 22% 9%)` | App background (warm slate) |
| `--fn-surface` | `#fff` | `hsl(224 20% 13%)` | Cards, bars, bubbles, modals |
| `--fn-surface-2` | `hsl(220 20% 98%)` | `hsl(224 19% 16%)` | Inset panels, pick lists |
| `--fn-surface-3` | `hsl(220 14% 94%)` | `hsl(224 17% 20%)` | Hover fill, chips, day markers |
| `--fn-fg` | `hsl(224 30% 12%)` | `hsl(220 22% 94%)` | Body text |
| `--fn-fg-muted` | `hsl(222 12% 44%)` | `hsl(220 12% 64%)` | Secondary text, addresses |
| `--fn-fg-faint` | `hsl(222 10% 60%)` | `hsl(220 10% 48%)` | Timestamps, placeholders |
| `--fn-border` | `hsl(220 13% 88%)` | `hsl(224 14% 24%)` | Hairlines |
| `--fn-border-strong` | `hsl(220 13% 78%)` | `hsl(224 14% 34%)` | Control borders |
| `--fn-encrypt` | `hsl(160 84% 33%)` | `hsl(160 70% 52%)` | 🔒 lock, verified hash, success |
| `--fn-crown` | `hsl(43 96% 42%)` | `hsl(45 93% 60%)` | Admin crown, rotation-pending warning |
| `--fn-danger` | `hsl(0 72% 45%)` | `hsl(0 78% 65%)` | Destructive actions, errors |
| `--fn-online` | `hsl(142 71% 38%)` | `hsl(142 65% 52%)` | Presence dot, WS connected |
| `--fn-info` | `hsl(212 88% 45%)` | `hsl(212 85% 65%)` | Testnet ribbon, polling mode |
| `--fn-focus` | `hsl(24 95% 42%)` | `hsl(24 95% 62%)` | Focus outline |

Each semantic colour also has a `-soft` companion (`--fn-encrypt-soft`, `--fn-crown-soft`,
`--fn-danger-soft`, `--fn-info-soft`) used for badge and banner fills.

**Colour rules.**

- Orange means *do this* or *this is selected*. Never informational, never a status.
- Emerald means *encryption held*. Never generic success — a saved rename toasts neutral.
- Yellow means *admin* or *needs your attention before you can post*.
- Red means *irreversible* or *failed*. Never a decorative accent.
- The testnet ribbon is blue on purpose; it must not be mistaken for a call to action.

### 2.3 Typography

No web fonts. The client's selling point is cold-start speed, and a 200KB font file next to
a WASM bundle is a contradiction. Topcoat's own stack asks for **Source Sans Pro**; if a
user has it, they get it, otherwise `system-ui`. That is the whole type strategy.

```
--fn-font-ui:    "Source Sans Pro", system-ui, -apple-system, "Segoe UI", Roboto, …
--fn-font-mono:  ui-monospace, "SF Mono", "JetBrains Mono", "Cascadia Mono", Menlo, …
--fn-font-emoji: "Apple Color Emoji", "Segoe UI Emoji", "Noto Color Emoji"
```

| Token | Size | Weight | Applied to |
| --- | --- | --- | --- |
| `--fn-t-2xl` | 32px | 800 | Login wordmark, balance figure, 404 code |
| `--fn-t-xl` | 22px | 700 | Screen title (`h1`) |
| `--fn-t-lg` | 17px | 700 | Room name in chat header, modal title |
| `--fn-t-md` | 15px | 400/600 | Body, message text, room name in list |
| `--fn-t-sm` | 13px | 400/700 | Addresses, field labels, secondary rows |
| `--fn-t-xs` | 11px | 700 | Meta, timestamps, badges, hash slugs |

Line height: `1.25` for headings, `1.5` for body. Letter spacing `-0.01em` on headings,
`+0.08…0.12em` on uppercase eyebrows and the testnet ribbon.

**Mono is reserved.** It appears on exactly four things and nothing else: wallet addresses,
message hashes, mnemonic phrases, and numeric wallet fields (amount, gas, wallet index).
Mono anywhere else dilutes the signal that "this is machine data you may need to copy".
Addresses always use `font-variant-ligatures: none` so `0x` and `ff` never fuse. Counts and
timestamps use `font-variant-numeric: tabular-nums` so lists don't jitter.

### 2.4 Spacing, radius, elevation

Spacing is a 4px scale, `--fn-s1` … `--fn-s8` = 4, 8, 12, 16, 20, 24, 32, 48. Nothing
outside the scale except optical 1–3px nudges.

Radius carries meaning:

| Token | Value | Applied to |
| --- | --- | --- |
| `--fn-r-ctl` | 4px | Buttons, inputs, tabs — Topcoat's own crispness, kept |
| `--fn-r-card` | 10px | Bubbles, cards, pick lists, toasts |
| `--fn-r-panel` | 14px | Modals, login card |
| `--fn-r-pill` | 999px | Badges, unread counts, search inputs, reactions |

Fruit chips use a **34% squircle radius**, not a circle. It is the one shape in the product
that isn't a rounded rectangle or a pill, which is why the avatar reads as an avatar at
24px.

Elevation is deliberately shallow and warm-tinted (`--fn-sh-1/2/3`). Only three things sit
above the page: modals, toasts and the emoticon picker. `--fn-sh-glow` (orange bloom) is
used on exactly one interaction — CTA hover.

### 2.5 Topcoat class map

| UI role | Class |
| --- | --- |
| Primary action (Send, Create Room, Connect) | `topcoat-button--cta` / `topcoat-button--large--cta` |
| Secondary action (Cancel, Manage, Generate) | `topcoat-button` / `topcoat-button--large` |
| Destructive action (Delete, Kick, Block) | `topcoat-button--danger` *(project variant)* |
| Toolbar / row action | `topcoat-button--quiet`, `topcoat-icon-button--quiet` |
| Bordered icon action | `topcoat-icon-button`, `topcoat-icon-button--large` |
| Single-line field | `topcoat-text-input`, `topcoat-text-input--large` |
| Multi-line field (composer, description, mnemonic) | `topcoat-textarea` |
| Filter / search field | `topcoat-search-input` |
| Encryption toggle, theme toggle | `topcoat-switch` |
| Multi-select in pick lists | `topcoat-checkbox` |
| Connection-mode choice in Settings | `topcoat-radio-button` |
| Gas price slider (Advanced settings) | `topcoat-range` |
| Screen header bar | `topcoat-navigation-bar` + `__title` / `__item` |
| Any vertical list | `topcoat-list` + `__header` / `__container` / `__item` |

`app.css` §3 restyles all of these. Two things it changes structurally: `topcoat-list__item`
is reduced to a layout-neutral wrapper (the project's `.fn-room-row` / `.fn-person` supply
the real row geometry), and `topcoat-search-input` becomes fully pill-shaped.

`topcoat-button--danger` is not stock Topcoat. It is defined in `app.css` and composes the
same way as `--cta`.

### 2.6 Iconography

Icons are inline SVG rendered by Yew — no icon font, no sprite sheet, no runtime fetch.
Stroke `2.4`, `stroke-linecap="round"`, `viewBox="0 0 24 24"`, sized 14/16/20px, coloured by
`currentColor`.

Three icons that recur as CSS masks (so they can be coloured by state without touching
markup) are embedded as data URIs in `app.css`: `--fn-icon-lock`, `--fn-icon-crown`,
`--fn-icon-check`.

Room avatars use the same identity tile as people, seeded by `room.id` instead of an address
(see §3), with the room name's first letter as a 14px corner badge.

---

## 3. Identity tile algorithm

> **Amended, 2026-07-27.** The emoji half of this section is superseded. The
> *hash* is unchanged and still normative — it is what guarantees an account
> looks the same everywhere and that two accounts never collide. What the hash
> drives has changed: it now selects a tile colour, and the glyph is a monogram
> (the first character of the username, falling back to the address's leading
> hex) rather than an entry in the fruit table below.
>
> The 40-entry table and its conformance vectors are kept for reference, because
> other FruitNation clients still render them and a shared wallet will look
> different across the two products. That divergence is intended: PocketSkynet is
> not a FruitNation client.

This must produce byte-identical output to the React client
(`server/client/src/utils/fruitIcons.ts`) so the same wallet shows the same fruit in every
FruitNation client.

### 3.1 The table — 40 entries, order is normative

```
index  0..9   🍎 🍏 🍊 🍋 🍋‍🟩 🍌 🍇 🍓 🫐 🍒
index 10..19  🍑 🥭 🍍 🥥 🥝 🍈 🍉 🍐 🥑 🍅
index 20..29  🫒 🌰 🥜 🫘 🌽 🥕 🥒 🌶️ 🫑 🧅
index 30..39  🧄 🥔 🍠 🥦 🥬 🥗 🍯 🍿 🧁 🍩
```

Two entries are multi-codepoint and must be stored as complete grapheme clusters:
index 4 is `U+1F34B U+200D U+1F7E9` (lime) and index 27 is `U+1F336 U+FE0F` (hot pepper).
Storing them as `&'static str` in Rust is correct; do not iterate them as `char`.

### 3.2 Normalisation

```
input  → to_ascii_lowercase()
       → strip_prefix("0x")   (after lowercasing, so "0X…" is also stripped)
```

An empty or missing address returns index 0 (`🍎`) without hashing.

### 3.3 Hash — djb2, 32-bit wrapping

The JavaScript original is `hash = ((hash << 5) + hash + char) >>> 0`, evaluated over
UTF-16 code units. Because the operands are coerced to 32 bits at the end of every
iteration, this is plain djb2 with wrapping `u32` arithmetic. For a hex address every code
unit is ASCII, so bytes and code units coincide.

```rust
/// djb2 over UTF-16 code units, wrapping at 32 bits.
/// Bit-identical to `((hash << 5) + hash + c) >>> 0` in JavaScript.
pub fn djb2(s: &str) -> u32 {
    s.encode_utf16().fold(5381u32, |h, u| {
        h.wrapping_mul(33).wrapping_add(u as u32)
    })
}

pub const FRUITS: [&str; 40] = [
    "🍎", "🍏", "🍊", "🍋", "🍋‍🟩", "🍌", "🍇", "🍓", "🫐", "🍒",
    "🍑", "🥭", "🍍", "🥥", "🥝", "🍈", "🍉", "🍐", "🥑", "🍅",
    "🫒", "🌰", "🥜", "🫘", "🌽", "🥕", "🥒", "🌶️", "🫑", "🧅",
    "🧄", "🥔", "🍠", "🥦", "🥬", "🥗", "🍯", "🍿", "🧁", "🍩",
];

/// Deterministic fruit for a wallet address. Matches fruitIcons.ts exactly.
pub fn fruit_for_address(addr: &str) -> &'static str {
    if addr.is_empty() { return FRUITS[0]; }
    let n = addr.to_ascii_lowercase();
    let n = n.strip_prefix("0x").unwrap_or(&n);
    FRUITS[(djb2(n) % 40) as usize]
}

/// First 4 hex characters after `0x` — shown beside the fruit in compact contexts.
pub fn address_prefix(addr: &str) -> &str {
    addr.get(2..6).unwrap_or("")
}
```

**Conformance vectors** (assert these in `core`):

| Input | Normalised | Expected |
| --- | --- | --- |
| `""` | — | `🍎` |
| `"0x"` | `""` | `djb2("") = 5381`; `5381 % 40 = 21` → `🌰` |
| any address | lowercase, no `0x` | `FRUITS[djb2(n) % 40]` |

Generate a fixture of ~200 addresses by running `getFruitEmoticon` in the React client and
commit it as `data/fruit-vectors.json`. The Rust test must reproduce it exactly.

### 3.4 Hue — a PocketSkynet addition

The hue is **not** in the TypeScript source; it is new here, and no other client needs to
agree with it. It reuses the same hash so a single computation drives both outputs:

```rust
/// Wash hue for the avatar tile. Uses the high bits so it is decorrelated
/// from `hash % 40`, which uses the low bits.
pub fn hue_for(seed: &str) -> u16 {
    ((djb2(seed) >> 8) % 360) as u16
}
```

Yew emits it as an inline custom property; `app.css` does the rest:

```html
<span class="fn-ident" style="--fn-hue: 212" aria-hidden="true">A</span>
```

Light mode fills with `hsl(H 72% 92%)` and rings with `hsl(H 45% 78%)`; dark mode uses
`hsl(H 34% 24%)` / `hsl(H 34% 38%)`. Lightness is fixed, so contrast against the emoji is
constant across all 360 hues — the hue only ever varies chroma, never legibility.

### 3.5 Rules of use

- The chip is **always `aria-hidden`**. It is redundant decoration; the username and
  address next to it carry the meaning. An avatar that announces "grapes" to a screen
  reader is noise.
- Sizes: `fn-ident--xs` 24px (inline, pick lists), `--sm` 28px (compact rows),
  default 36px (room rows, message gutter), `--lg` 44px (members), `--xl` 64px (profile).
- `fn-ident--self` adds a 2px orange ring — the only avatar treatment that means anything.
- `fn-ident--online` adds the presence dot. Only apply it when real presence data exists;
  the React client shows it unconditionally, which is a lie the new client should not
  repeat.
- Rooms seed from `room.id`, people from `wallet_address`. Never mix the seeds.

---

## 4. App shell, navigation and routing

```
┌──────────────────────────────────────────────────────────────────────┐
│  TESTNET ENVIRONMENT · CRONOS TESTNET          .fn-ribbon (optional)  │
├──────────────────────────────────────────────────────────────────────┤
│ 🍇 saltyOrchard42       Rooms  Chat  Members  ⋯   🌗  💳  ⏻          │  .fn-topbar
│    0x9f2a…7c41                                                        │
├────────────────────────┬─────────────────────────────────────────────┤
│                        │                                             │
│   .fn-pane--list       │   .fn-pane--detail                          │
│   (room list, 340px)   │   (chat / members / settings / invites)     │
│                        │                                             │
└────────────────────────┴─────────────────────────────────────────────┘
```

| Route | Guard | Renders |
| --- | --- | --- |
| `/login` | no JWT | Login (§5) |
| `/` | JWT | redirect → `/rooms` |
| `/rooms` | JWT | Shell, detail pane empty |
| `/rooms/:id` | JWT | Shell, detail pane = Chat (§7) |
| `/rooms/:id/members` | JWT | Shell, detail pane = Members (§10) |
| `/invitations` | JWT | Shell, detail pane = Invitations (§11) |
| `/knowledge` | JWT | Shell, detail pane = Knowledge (docs/SEARCH.md §5) |
| `/bank` | JWT | Shell, detail pane = Bank page (§19) |
| `/settings` | JWT | Shell, detail pane = Settings (§13) |
| `*` | — | Not found (§14) |

The list pane is **always mounted** at every authenticated route. On narrow viewports it is
hidden with `display:none` rather than unmounted (`app.css` §14), so scroll position,
WebSocket subscriptions and decrypted message caches survive navigation.

**Testnet ribbon.** Rendered whenever the configured chain name contains `testnet`. Blue,
uppercase, 11px, non-dismissible, absorbs `env(safe-area-inset-top)`. It is a fact about
the environment, not a notification.

**Top bar.** Left: `.fn-topbar__identity`, a button containing the identity tile, username and
EIP-55 checksum address. Click copies the checksum address and toasts. Full address at
≥900px, `0x9f2a…7c41` below. Right: `.fn-topbar__actions` — language, theme, wallet
balance, logout, all `topcoat-icon-button--quiet`.

**Bottom nav** (`<900px` only, `.fn-bottomnav`): Rooms · Chat · Members · Invites ·
Settings. Chat and Members are `disabled` with no room selected. The active item gets
`aria-current="page"`, orange text and a 2px orange cap. Invites carries
`.fn-bottomnav__badge` with a `.fn-unread` count.

---

## 5. Screen 1 — Login

**Purpose:** authenticate with an Ethereum wallet. There are no passwords and no accounts
to recover; the copy has to make the backup step feel like part of signing in, not a chore
after it.

```
                    .fn-login  (radial orange + emerald wash on --fn-bg)
        ┌───────────────────────────────────────────────┐
        │                    ┌────┐                     │
        │                    │ 🍉 │  .fn-login__mark    │
        │                    └────┘                     │
        │                FruitNation                    │  .fn-login__wordmark
        │       Sign in with your wallet. No password.  │  .fn-login__tagline
        │                                               │
        │  (English)(한국어)(日本語)(Español)(中文)(粵語)(Deutsch) │ .fn-langs
        │                                               │
        │  ┌─────────────────────────────────────────┐ │
        │  │ ⚡  Create a wallet and sign in         │ │  .fn-hero-btn --cta
        │  │    New wallet, backup file, done        │ │
        │  └─────────────────────────────────────────┘ │
        │  ┌─────────────────────────────────────────┐ │
        │  │ ⭳  Sign in with a backup file          │ │  .fn-hero-btn
        │  │    Load a wallet you saved earlier      │ │
        │  └─────────────────────────────────────────┘ │
        │                                               │
        │  ──────────── SET UP MANUALLY ────────────    │  .fn-rule
        │                                               │
        │  ┌ Mnemonic ┬ MetaMask ┬ Privy ┐             │  .fn-tabs
        │  └──────────┴──────────┴───────┘             │
        │                                               │
        │  Username                        [Generate]  │  .fn-field
        │  ┌─────────────────────────────────────────┐ │
        │  │                                         │ │  .topcoat-text-input
        │  └─────────────────────────────────────────┘ │
        │  Leave blank if you've signed in before.     │  .fn-field__help
        │                                               │
        │  Recovery phrase                  [👁] [✕]   │  .fn-mnemonic
        │  ┌─────────────────────────────────────────┐ │
        │  │ ••••• ••••• ••••• •••••                │ │  .topcoat-textarea
        │  └─────────────────────────────────────────┘ │
        │  [ Generate a new phrase ]                   │
        │                                               │
        │  Wallet index    [ − ][   0   ][ + ]         │  .fn-stepper
        │                                               │
        │  ┌─────────────────────────────────────────┐ │
        │  │            Sign in                      │ │  .topcoat-button--large--cta
        │  └─────────────────────────────────────────┘ │
        │                                               │
        │  Your wallet address is your account.        │
        └───────────────────────────────────────────────┘
```

### Controls

| Element | Class |
| --- | --- |
| Card | `.fn-login__card` |
| Appearance / Layout switches | `.fn-login__prefs > .fn-seg > .fn-seg__btn` (`role="group"`, `aria-pressed`) |
| Language pills | `.fn-lang` (`aria-pressed` on the active one) |
| Stay signed in | `.fn-toggle-row` + `.topcoat-checkbox__input` |
| Hero CTA 1 | `.fn-hero-btn topcoat-button--large--cta` |
| Hero CTA 2 | `.fn-hero-btn topcoat-button--large` |
| Divider | `.fn-rule` |
| Tabs | `.fn-tabs` / `.fn-tab` (`role="tab"`, `aria-selected`) |
| Panels | `.fn-tabpanel` (`role="tabpanel"`) |
| Username, wallet index | `.topcoat-text-input` |
| Generate / Generate new phrase | `.topcoat-button` |
| Mnemonic | `.fn-mnemonic > .topcoat-textarea` (mono, 13px, 1.7 line height) |
| Reveal / clear | `.fn-mnemonic__tools > .topcoat-icon-button--quiet` |
| Backup warning | `.fn-warnpanel` |
| Submit | `.topcoat-button--large--cta` |
| Inline error | `.fn-login__error` |

### States

| State | Presentation |
| --- | --- |
| **Idle** | Both hero CTAs enabled; tabs default to Mnemonic (MetaMask if injected provider found; Privy only when the app id is configured). |
| **Loading** | The pressed button keeps its label, gains `.fn-spinner--on-primary` and `aria-busy="true"`; every other control gets `disabled`. Never swap the label for "Loading…" — the user loses the thread of what they pressed. |
| **Mnemonic hidden** | `.fn-mnemonic[data-masked="true"]` applies `-webkit-text-security: disc`. Default is **hidden**; the eye reveals. |
| **Wallet generated** | `.fn-warnpanel` expands under the textarea: "Save this phrase now", three bullets, the new address in mono, and `[Copy phrase] [Download backup]`. The submit button stays disabled until one of the two is pressed — you cannot skip past the backup. |
| **Invalid mnemonic** | Textarea gets `aria-invalid="true"` (red border + red halo); `.fn-login__error` states which word count was found and which are valid. |
| **Wrong chain (MetaMask)** | `.fn-login__error`: "This wallet is on {actual}. Switch to {expected} to continue." with a `[Switch network]` CTA. |
| **Auth failure** | `.fn-login__error` above the submit button *and* an error toast. Both, because the toast may be missed and the inline block may be scrolled out of view. |
| **Offline** | Hero CTA 1 (which only needs local key generation) stays enabled; every server-dependent control is disabled with `.fn-banner--offline` at the top of the card: "No connection. You can still create a wallet — sign-in will finish when you're back online." |
| **Success** | Toast "Signed in as {username}", then route to `/rooms`. |
| **Username blank** | Not an error. The field's placeholder is the name derived from the wallet address (`core::username::deterministic_username`) and that is what gets sent — the same name any other client picks for the same wallet. |
| **Unlocking from the vault** | `.fn-banner[role="status"]` with a spinner: "Unlocking with the recovery phrase saved on this device…". The credential fields fill in visibly, so it is obvious *what* is signing in; a failure leaves them populated and ready to retry. |

### Interactions

- Enter in the username or mnemonic field submits the active tab.
- **Appearance (Light/Dark)** and **Layout (Vertical/Horizontal)** sit above the lockup.
  Settings is behind the sign-in, so without them the first screen anyone sees is the one
  screen they cannot adjust. Layout writes `data-layout` on `.fn-login`, which overrides
  the responsive breakpoints in both directions; pressing the active one again returns to
  `auto`, so pinning a layout is never a one-way door. Both persist before authentication,
  like the language pills.
- **Stay signed in on this device** governs `crate::vault`. It sits next to the credential
  field it applies to rather than in Settings, because it decides whether what was just
  typed outlives the tab. Turning it off wipes an already-stored credential immediately —
  a switch that only governed future writes would leave the phrase sitting there after the
  user said not to keep it.
- The file input for "Sign in with a backup file" is a visually hidden `<input type="file"
  accept=".json">` labelled by the button; the button is a real `<button>` that forwards
  the click, so keyboard users reach it normally.
- Wallet index steppers are `−`/`+` buttons flanking a `type="number"` input; the input is
  authoritative and typing into it is always allowed.
- Language choice persists to `localStorage` before authentication, so the login screen
  itself remembers.

---

## 6. Screen 2 — Room list

**Purpose:** the persistent left rail. Answers "where is there something new" in one
glance.

```
┌─ .fn-pane--list ────────────────────────────────────┐
│ ┌──────────────────────────────┐  ┌───┐  ┌───┐      │  .fn-roomlist__head
│ │ 🔍  Search rooms             │  │ ⚡ │  │ + │      │  .topcoat-search-input
│ └──────────────────────────────┘  └───┘  └───┘      │  .fn-fastbtn / .topcoat-icon-button
├─────────────────────────────────────────────────────┤
│ ✉  Invitations                                 (2)  │  → §11
├─────────────────────────────────────────────────────┤
│▌┌────┐                                              │  .fn-room-row[aria-selected]
│▌│ 🍇 │  Harvest planning  🔒                        │  .fn-room-row__title
│▌│  H │  ⛨ Admin · 8 members                  14:32  │  .fn-room-row__meta
│▌└────┘                                              │
├─────────────────────────────────────────────────────┤
│ ┌────┐                                              │
│ │ 🥝 │  Orchard ops                                 │
│ │  O │  3 members · "shipment lands friday"    (12) │  .fn-unread
│ └────┘                                              │
├─────────────────────────────────────────────────────┤
│ ┌────┐                                              │
│ │ 🍒 │  Cold storage  🔒                            │  lock is --fn-crown when
│ │  C │  Key rotation needed · 5 members       09:04 │  rotation is pending
│ └────┘                                              │
└─────────────────────────────────────────────────────┘
```

**⚡ Fast create** (`.fn-fastbtn`, cyan-tinted so it does not read as a second `+`)
runs the whole one-click flow from the shell — auto name, encrypted, greeted, opened —
via `actions::fast_create_room`, which the dialog's fast button shares. The room opens
with a **hello-world already posted**: a randomly picked greeting with emoticons plus a
random flourish, stamped with the date and the moment in both local time and UTC
("Hello, world — encrypted and ready 🔐💬 🔥 · 📅 2026-07-28 · ⏰ 15:28 local ·
🌐 06:28 UTC") — the first thing the user sees is a working, sealed room rather than an
empty pane. The greeting is encrypted under the epoch-1 key **returned by the key
ceremony**, never read back through the store: a `UseReducerHandle` snapshot from an
earlier render cannot see this task's own dispatches, and routing the greeting through
the ordinary send path once made it silently go out plaintext. Locked keys refuse with
an error toast before anything is created; a post-creation key failure still opens the
room (greeting plaintext, like the room) with a sticky "created without encryption"
error naming the reason. The `+` opens the form for anyone who wants to choose the
name, description, or plaintext — manual creates get no greeting. The "No rooms yet"
empty state leads with the same one-click button and offers "Set it up yourself…" as
the quiet alternative.

### Row anatomy — `.fn-room-row`

A three-column grid: avatar (spans both rows) · title + meta · aside (time, unread).

| Part | Class | Content |
| --- | --- | --- |
| Avatar | `.fn-ident .fn-room-row__avatar` | `identity::monogram_for` and `hue_for` seeded with `room.id`, hue inline, first letter of the room name as a 14px corner badge |
| Name | `.fn-room-row__name` | truncated |
| Lock | `.fn-lock` | only when `room.has_encryption` |
| Meta | `.fn-room-row__meta` | admin badge · member count · last-message preview |
| Admin badge | `.fn-badge .fn-badge--admin` | crown mask + "Admin", only when you are an admin |
| Time | `.fn-room-row__time` | `HH:MM` today, `Mon` this week, `D/M` older |
| Unread | `.fn-unread` | count, `99+` above 99 |

**Improvement over the React client:** it renders no preview and no timestamp even though
the API returns `lastMessage`. PocketSkynet renders both, and sorts by last activity
descending — a room list in database insertion order is not a room list.

### States

| State | Presentation |
| --- | --- |
| **Loading** | Six `.fn-skel` rows at row rhythm. Only after 400ms — below that, show nothing rather than a flash. |
| **Empty (no rooms)** | `.fn-roomlist__empty`: `.fn-empty__art` 🍉, "No rooms yet", "Create one and invite someone by wallet address.", `[Create room]` CTA. |
| **Empty (search)** | Same block, 🔍 art, "No rooms match "{query}"", `[Clear search]`. |
| **Selected** | `aria-selected="true"` → `--fn-primary-soft` fill and a 3px orange left rail. |
| **Unread** | `.is-unread` → name and preview at full weight and full-contrast ink, plus the count chip. Cleared optimistically when the room opens, then confirmed by `mark-read`. |
| **Encrypted** | Emerald `.fn-lock` after the name. |
| **Rotation pending** | `.is-rotation-pending` recolours the lock to `--fn-crown` and replaces the preview with "Key rotation needed". |
| **Offline** | Rows render from cache and stay interactive; the head shows a `.fn-conn--offline` pill instead of the create button. |
| **Error** | `.fn-empty--error`: "Couldn't load rooms", the server message, `[Try again]`. Never an empty list — an empty list reads as "you have no rooms". |

### Interactions

- Click / Enter / Space selects and routes to `/rooms/:id`.
- ↑ ↓ move between rows (roving tabindex), Home/End jump to ends, typing a letter jumps to
  the next room starting with it.
- Search filters client-side on name, case-insensitive, debounced 120ms; `Esc` clears.
- `Ctrl/Cmd+K` focuses search from anywhere.
- The list is `role="listbox"`, rows are `role="option"`.
- New rooms and unread changes arrive over WebSocket and re-render in place; the list never
  scroll-jumps because rows are keyed by room id.

### Swipe to remove — `.fn-swipe`

Getting a room out of the list used to mean opening it and finding the `⋮` menu: four
taps to undo one. A drag left on the row now reveals **Hide** and **Leave** as the
underside of the row, and the escalation below removes a tap for anyone who does it
often.

| | |
| --- | --- |
| **Reveal** | Drag left past 40 % of the drawer; it latches open. The buttons open the ordinary confirm dialog — same title, same body, same verb as the `⋮` menu. |
| **Express** | After **three removals in a row by swipe**, a drag that carries past the whole drawer plus 64px goes straight to the confirmation. Distance, not speed, so a flick that got away cannot reach it — and the confirmation stays, because the escalation removes a tap, not a decision. Announced once by an info toast the moment it unlocks. |
| **Streak** | Counted only when the server has agreed to the removal, so a cancelled dialog is not a removal. Removing a room from the `⋮` menu instead resets it — "in a row" is a claim about the gesture. Persisted in `ps-swipe-streak`. |
| **Not here** | **Delete room.** It is the one room action that reaches other people's clients; it stays in the `⋮` menu behind admin rights, where you have to have gone looking. |
| **Dismiss** | Tapping the row, `Esc`, opening another row's drawer, or a pointer landing anywhere outside it. |
| **Keyboard** | `Delete` / `Backspace` on a focused row opens the drawer and puts its buttons in the tab order; they are `tabindex="-1"` and `aria-hidden` while it is shut. A gesture-only action is an action some people do not have. |

Every removal — swiped or not — now ends in a toast that names the room, and the hide
toast says where it went ("Bring it back from Settings → Hidden rooms"): a room
vanishing from the list is a large, silent change.

The drawer is not a layer over the row revealed by opacity — it sits *after* it in one
flex track that slides, clipped by `.fn-swipe`. So the reveal is a single compositor
transform, and the row needs no opaque background to hide buttons that are genuinely
outside the clip. `touch-action: pan-y` leaves vertical panning to the scroller; the
handler claims a gesture only once it has moved 10px and moved further across than down
(ties go to the scroller — a list is scrolled far more often than pruned). The offset is
written to the track node directly rather than through the store: a pointermove that
re-renders the room list is a re-render per frame of a finger movement.

---

## 7. Screen 3 — Chat view

**Purpose:** the product. Everything else exists to get here.

```
┌─ .fn-chat ─────────────────────────────────────────────────────────┐
│ [←] 🍇 Harvest planning 🔒            [⟳] [⋮]                      │  .fn-chat__head
│     8 members · ● WS · synced 14:32                                │  .fn-chat__submeta
├────────────────────────────────────────────────────────────────────┤
│ ⚠ Key rotation needed before you can post.        [Rotate now]     │  .fn-banner--warn
├────────────────────────────────────────────────────────────────────┤
│                        ── Today ──                                 │  .fn-daymark
│                                                                    │
│ ┌────┐ mintyPear19  0x4c1b…9ef2            14:02                   │  .fn-msg__sender
│ │ 🥝 │ ┌──────────────────────────────────────┐                    │
│ └────┘ │ Shipment lands friday, dock 3.       │  .fn-bubble        │
│        └──────────────────────────────────────┘                    │
│        a91f4c02 ✓ verified                                         │  .fn-hash--verified
│        (🍎 3)(👍 1)  [☺] [⋯]                                       │  .fn-reactions
│                                                                    │
│                    ┌──────────────────────────────────┐ ┌────┐    │
│                    │ Got it — I'll meet the driver.   │ │    │    │  .fn-msg--own
│                    └──────────────────────────────────┘ └────┘    │
│                                       14:05 · edited  7b02de41     │  .fn-msg__foot
│                                                                    │
│               ── saltyOrchard42 joined the room ──                 │  .fn-sysmsg
│                                                                    │
│ ┌────┐ ┌──────────────────────────────────────┐                    │
│ │ 🍒 │ │ 🔒 Encrypted — no key for epoch 2    │  .fn-bubble--sealed│
│ └────┘ └──────────────────────────────────────┘                    │
├────────────────────────────────────────────────────────────────────┤
│ ●●● mintyPear19 is typing…                                         │  .fn-typing
├────────────────────────────────────────────────────────────────────┤
│ [☺] ┌────────────────────────────────────────────┐ ┌──────────┐   │  .fn-composer
│     │ Message Harvest planning                   │ │   Send   │   │
│     └────────────────────────────────────────────┘ └──────────┘   │
│     Enter to send · Shift+Enter for a new line                     │  .fn-composer__hint
└────────────────────────────────────────────────────────────────────┘
```

### 7.1 Header — `.fn-chat__head`

Back button (`.fn-back`, mobile only) · room identity tile · name · `.fn-lock` when encrypted ·
`.fn-badge--admin` when you are an admin. Second line `.fn-chat__submeta`: member count
(links to Members) · connection pill · last sync time.

Right: refresh (`topcoat-icon-button--quiet`, spins while syncing) and a `⋮` menu.

**Menu — admin:** Invite people · Rename room · Manage admins · — · Leave room · Hide room ·
Delete room (red).
**Menu — member:** Invite people *(if the room allows it)* · — · Leave room · Hide room.

Every destructive item opens a real `.fn-modal--danger`, never `window.confirm`, and never
reloads the page (the React client calls `window.location.reload()` after leave/delete/hide;
here the router just navigates to `/rooms`).

### 7.2 Message stream — `.fn-stream`

**Grouping.** A sender header (`.fn-msg__sender`: avatar, username, address, time) renders
when the sender changes, when more than 5 minutes have passed, or across a day boundary.
Otherwise `.fn-msg--grouped` hides the avatar and header and tightens the gap to 2px. Own
messages are mirrored: no avatar gutter, orange fill, `.fn-msg__foot` right-aligned.

**Day markers** — `.fn-daymark` pill: "Today", "Yesterday", then `D MMMM YYYY`.

**Bubble** — `.fn-bubble`, max `min(62ch, 100%)` (84% on mobile), `white-space: pre-wrap`,
`overflow-wrap: anywhere`. Corner rule: the corner nearest the sender is 4px, the rest 10px,
so direction is legible with colour removed.

**Ledger gutter** — `.fn-hash` under each bubble: 8-char `msg_hash` prefix in mono, 55%
opacity, click to copy the full hash. When `tx_hash` is present it becomes
`.fn-hash--verified` — emerald, full opacity, check mark, tooltip "Published on {chain}",
click opens the explorer.

**Content.** Autolink `http(s)`; render bare image/GIF URLs inline (lazy, with a spinner and
an "Image failed to load" fallback that keeps the URL clickable); embed YouTube as a 16:9
`youtube-nocookie` iframe. Everything else is text — no markdown, no HTML.

**Zoom.** An inline picture is capped at 400px and an attachment preview at 320px, which is
the right size for a conversation and the wrong size for reading a screenshot. Tapping
either raises the lightbox (`.fn-lightbox`, components/lightbox.rs): the picture travels
from the bubble to the full viewport under a blurred scrim, with the filename beneath it for
an attachment. The scrim, the picture, `Esc` and the ✕ all dismiss it, and the picture
travels back to the bubble it came from. The whole picture is the control — a 16px icon is
not a hit area on a phone. Attachments keep "Open in a new window" on the card's toolbar for
anyone who wanted the window rather than the size.

**System events** — `.fn-sysmsg`, centred pill: joins, leaves, kicks, renames.
`.fn-sysmsg--rotation` (yellow) for "Room key rotated to epoch {n}". Reaction and
delete-all events are not rendered as messages.

### 7.3 States

| State | Presentation |
| --- | --- |
| **No room selected** | `.fn-empty` in the detail pane: 🍉 art, "Pick a room", "Choose a conversation on the left, or create one." |
| **Loading (no cache)** | `.fn-spinner--lg` + "Opening room…" |
| **Loading (cache)** | Render cached plaintext immediately; the connection pill shows `.fn-conn--syncing`. Never blank a room you already have. |
| **Empty room** | `.fn-empty`: 🥝 art, "No messages yet", "Say something — it'll be encrypted end to end." (drop the second clause for plaintext rooms). |
| **Older history** | A `.topcoat-button--quiet` pill above the oldest message: "Load earlier messages" → "Loading…". Page size 50. Scroll anchoring preserves the viewport. |
| **Encrypted room** | Emerald lock in the header. Per-message locks are *not* drawn — in an encrypted room every message is encrypted, and repeating that 200 times devalues the signal. A *plaintext* message inside an encrypted room, however, gets `.fn-badge--danger` "Not encrypted". |
| **Undecryptable** | `.fn-bubble--sealed`, mono, muted: "🔒 Encrypted — no key for epoch {n}", "🔒 Missing metadata", or "🔒 Decryption failed". Three distinct strings; do not collapse them. |
| **Rotation pending** | `.fn-banner--warn` under the header: "Key rotation needed before you can post." + `[Rotate now]`. The composer gets `data-locked="true"` (dimmed, inert) and its placeholder becomes "Rotate the room key to post". While rotating, the banner shows a spinner and "Rotating keys…". On success it becomes a `.fn-sysmsg--rotation` line and the composer unlocks. On failure: `.fn-banner--danger` with `[Try again]`. |
| **Send: stale epoch** | Silent — refetch epoch, re-encrypt, retry once. Only a second failure surfaces. |
| **Send: pending** | Optimistic bubble at 62% opacity (`.fn-msg--pending`). |
| **Send: failed** | `.fn-msg--failed` — red-washed bubble, `.fn-msg__foot` gains "Not sent" and `[Retry] [Delete]`. |
| **Offline** | `.fn-conn--offline` pill + `.fn-banner--offline` "You're offline. Messages will send when you reconnect." Composer stays enabled; sends queue as `.fn-msg--pending` and flush on reconnect, oldest first. |
| **Kicked** | Stream replaced by `.fn-empty--error`: "You're no longer in this room", "An admin removed you.", `[Back to rooms]`. |
| **Room not found / 403** | `.fn-empty--error`: "Room unavailable", "It may have been deleted, or you may not have access.", `[Back to rooms]`. |

### 7.4 Connection pill — `.fn-conn`

| Variant | Dot | Label | Meaning |
| --- | --- | --- | --- |
| `--ws` | green | `Live` | WebSocket connected |
| `--poll` | blue | `Polling` | HTTP polling, 10s |
| `--syncing` | blue, pulsing | `Syncing` | catch-up in flight |
| `--offline` | red | `Offline` | no transport |

Clicking toggles Live ↔ Polling and persists to `localStorage` (`ps-connection-mode`,
default `websocket`). `--syncing` and `--offline` are observed states, not choices —
clicking them retries. Accessible name spells it out: "Connection: Live (WebSocket). Switch
to polling."

### 7.5 Reactions and message actions

`.fn-reaction` chips sit under the bubble: emoji + tabular count. Yours get
`aria-pressed="true"` (orange wash). Click toggles; the accessible name is
"🍎 3 reactions, including you. Remove your reaction."

`.fn-msg__tools` appears on hover or focus-within (always visible on coarse pointers): a
smiley opening `.fn-picker`, and a `⋯` menu with Copy text · Copy hash · Copy transaction
hash *(only when published)* · Edit *(own only)* · Delete.

`.fn-picker` is an 8-column grid, max 320px, categories as a tab row above. Opens on the
side with room; traps focus; `Esc` closes and returns focus to the trigger.

Editing swaps the bubble body for a `.topcoat-text-input` with `[✓] [✕]`; Enter saves,
Esc cancels. Saved edits append "edited" to `.fn-msg__foot` and are re-encrypted under the
current epoch.

Delete opens `.fn-modal--danger`: "Delete message?", "This removes it for everyone. It
can't be undone.", `[Cancel] [Delete]`.

**"Delete all messages" is admin-only here.** The React client exposes it on every message
to every user, which is a data-loss trap.

### 7.6 Typing indicator — `.fn-typing`

Three bouncing 5px dots plus text. One name: "{name} is typing…". Two or three: names joined
with ", ". More than three: "{n} people are typing…". A sender expires 4s after their last
typing event, swept once a second. The row reserves 22px so the stream does not jump when it
appears. Marked `aria-live="polite"` with `aria-atomic="true"`, and suppressed entirely
under `prefers-reduced-motion` — the text remains, the dots stop.

### 7.7 Composer — `.fn-composer`

Emoticon button · auto-growing `.topcoat-textarea` (38px → 8.5em, `field-sizing: content`) ·
Send (`topcoat-button--cta`, disabled while empty). Enter sends, Shift+Enter newlines;
`.fn-composer__hint` says so.

Placeholder is "Message {room name}" — never a bare "Type a message…".

The "publish this message hash on-chain" flow is **not** a per-send interruption here. It is
a `⋯` action on an already-sent message ("Publish hash…"), opening a modal with the hash,
the target contract, the amount and the estimated fee. Making every send route through a
blockchain confirmation dialog, as the React client does, makes the app feel like a wallet
rather than a messenger.

---

## 8. Screen 4 — Create room

Modal, `.fn-modal` (440px), opened from the `+` in the room list head, the bottom-nav
"New", or the empty state.

```
┌─ .fn-modal ────────────────────────────────────┐
│ Create a room                            [✕]   │  .fn-modal__head
│ Rooms are private. Invite people by wallet.    │  .fn-modal__desc
├────────────────────────────────────────────────┤
│ ┌────────────────────────────────────────────┐ │
│ │ ⚡ Fast create room                        │ │  .fn-hero-btn
│ │ Named for you, encrypted, and opened       │ │
│ └────────────────────────────────────────────┘ │
│ ──────── OR SET IT UP YOURSELF ────────         │  .fn-rule
│                                                │
│ Room name                                      │  .fn-field__label
│ ┌────────────────────────────────────────────┐ │
│ │ Harvest planning                           │ │  .topcoat-text-input
│ └────────────────────────────────────────────┘ │
│                                                │
│ Description (optional)                         │
│ ┌────────────────────────────────────────────┐ │
│ │                                            │ │  .topcoat-textarea
│ └────────────────────────────────────────────┘ │
│                                                │
│ ┌────────────────────────────────────────────┐ │
│ │ 🔒 Encrypt this room          [ ●———— ]   │ │  .fn-toggle-row
│ │ Messages are readable only by members.     │ │  .topcoat-switch
│ │ Encryption can't be turned on later.       │ │
│ └────────────────────────────────────────────┘ │
├────────────────────────────────────────────────┤
│                        [ Cancel ] [ Create ]   │  .fn-modal__foot
└────────────────────────────────────────────────┘
```

| State | Presentation |
| --- | --- |
| **Default** | Encryption **on**. Name focused on open. `.fn-toggle-row[data-on="true"]` gives the row an emerald border and wash. |
| **Invalid** | Empty name → `aria-invalid` + `.fn-field__error` "Enter a room name." Inline, not a toast — the field is right there. Max 64 characters, counter appears at 48. |
| **Submitting** | Create shows a spinner, both buttons disabled, fields readonly. |
| **Key generation failed** | Room is created plaintext; the modal stays open showing `.fn-banner--warn`: "Room created without encryption. {reason}" and the footer becomes `[Close]`. The user must see this — silently downgrading an E2EE room is the worst possible outcome. |
| **Server error** | `.fn-field__error` beneath the footer with the server message; nothing is cleared. |
| **Success** | Toast "Room created", modal closes, route to `/rooms/:new_id`, composer focused. |
| **Fast create** | Fills the name (`room_name_from_entropy` — "Onyx Scorpion 0683") and description in the form itself, forces encryption on, then runs the ordinary submit. The fields are *shown* filling in rather than bypassed, so the user can read what was made and rename it. |
| **Fast create, keys locked** | `.fn-field__error`: "Unlock your wallet first — a fast room is always encrypted." Nothing is created. A button that promises an encrypted room must not quietly produce a plaintext one. |

Cancel, `Esc` and backdrop click all close. Focus is trapped and returns to the trigger.

**Fast create** is one click for the whole flow: name, description, encryption, create,
key, and open. It is not a second code path — it fills the same three inputs the form
produces and hands them to the same submit, so a fast room and a hand-made one are
indistinguishable afterwards. Creating a room joins it (the server makes the creator its
first member and admin), so opening it is the last step of the same action.

---

## 9. Screen 5 — Invite

Modal, `.fn-modal` (440px), from the chat `⋮` menu or the Members header. Admin-only when
the room restricts invites.

```
┌─ .fn-modal ────────────────────────────────────┐
│ Invite people                            [✕]   │
│ They join once they accept.                    │
├────────────────────────────────────────────────┤
│ ┌────────────────────────────────────────────┐ │
│ │ 🔍  Username or 0x address                 │ │  .topcoat-search-input
│ └────────────────────────────────────────────┘ │
│ ┌─ .fn-picklist ─────────────────────────────┐ │
│ │ 🍎  crispApple07                           │ │
│ │     0x1a2b…c3d4            [   Invite   ]  │ │
│ ├────────────────────────────────────────────┤ │
│ │ 🥭  goldenMango51                          │ │
│ │     0x9f8e…7d6c            [  Invited ✓ ]  │ │
│ ├────────────────────────────────────────────┤ │
│ │ 🍒  sourCherry88                           │ │
│ │     0x33aa…12ff       Already a member     │ │
│ └────────────────────────────────────────────┘ │
├────────────────────────────────────────────────┤
│                                     [ Done ]   │
└────────────────────────────────────────────────┘
```

Rows are `.fn-picklist__row`: `.fn-ident--xs`, username, `.fn-addr`, action.

| State | Presentation |
| --- | --- |
| **Idle** | `.fn-empty` inside the list: "Search for someone by username or paste a wallet address." |
| **Searching** | Three `.fn-skel` rows. Debounce 250ms. |
| **Results** | `[Invite]` = `topcoat-button`. |
| **Invited** | Button becomes a disabled `.fn-badge--encrypt` "Invited"; the row stays so the user sees what they did. |
| **Already a member** | `.fn-badge--muted` "Already a member", no action. |
| **Blocked either way** | Row omitted entirely. The server filters both `blocked` and `blocked-by`; the client must not reveal the difference. |
| **No results** | "No one found for "{query}". Paste a full wallet address to invite someone who hasn't signed in yet." |
| **Encrypted room** | Footnote under the list: "🔒 They'll receive the room key when they accept." If pre-wrapping the key fails, the invite still succeeds and a `.fn-banner--warn` explains they'll get the key on their first sync. |
| **Error** | `.fn-field__error` above the footer; the query is preserved. |

Success toast: "Invitation sent" / "{username} joins once they accept."

---

## 10. Screen 6 — Members & admins

Full pane at `/rooms/:id/members`.

```
┌─ .fn-pane--detail ─────────────────────────────────────────────┐
│ [←]  Members                          8    [ Invite people ]   │  topcoat-navigation-bar
├────────────────────────────────────────────────────────────────┤
│▌┌──────┐                                                       │  .fn-person--self
│▌│  🍇  │  saltyOrchard42  👑  You                              │
│▌│      │  0x9f2a…7c41                                          │
│▌└──────┘                                                       │
├────────────────────────────────────────────────────────────────┤
│ ┌──────┐                                                       │
│ │  🥝  │  mintyPear19  👑            [⇗] [⊘] [⊖]              │  .fn-person__actions
│ │      │  0x4c1b…9ef2                                          │
│ └──────┘                                                       │
├────────────────────────────────────────────────────────────────┤
│ ┌──────┐                                                       │
│ │  🍒  │  sourCherry88               [⇗] [⊘] [⊖]              │
│ │      │  0x33aa…12ff   Blocked                                │
│ └──────┘                                                       │
└────────────────────────────────────────────────────────────────┘
```

Row `.fn-person`, avatar `.fn-ident--lg`. Self row gets `.fn-person--self` (orange wash +
rail) and `.fn-ident--self`. Admins get `.fn-crown-icon`. Blocked members get
`.fn-badge--danger` "Blocked" and their messages are hidden in the stream.

Actions (`.fn-person__actions`, hover/focus-revealed, always visible on touch), never on
your own row:

| Icon | Action | Availability |
| --- | --- | --- |
| ⇗ | Send funds | anyone |
| ⊘ | Block / Unblock | anyone |
| ⊖ | Remove from room | admins only |
| 👑 | Make admin / Remove admin | admins only, via `⋯` |

Each has a `title` **and** an `aria-label` naming the person: "Remove sourCherry88 from
this room".

**Tapping the card copies the address** (`.fn-person--tap`). Looking someone up here is
almost always in order to paste them somewhere — a send form, an invite field — and a
44px portrait beside a name is a much easier target than the address under it. The
address itself stays a copy control too: that is the named, focusable version, for the
keyboard and the screen reader. A click that landed on one of the action buttons or on
the zoomable portrait is not "the card" and does not copy (`common::hit_control`) — one
tap, one answer.

### States

| State | Presentation |
| --- | --- |
| **Loading** | Five `.fn-skel` rows. |
| **No room** | `.fn-empty`: "No room selected". |
| **Empty** | Cannot occur — you are always a member of a room you can see. |
| **Presence** | `.fn-ident--online` only with real presence data; otherwise omit the dot. |
| **Offline** | Rows from cache; all action buttons disabled with title "Unavailable offline". |

### Manage admins — `.fn-modal--wide`

```
┌─ .fn-modal--wide ──────────────────────────────┐
│ Manage admins                            [✕]   │
│ Admins can invite, rename, remove members      │
│ and delete the room.                           │
├────────────────────────────────────────────────┤
│ Current admins                          3 / 9  │  .fn-admin-count
│ ┌─ .fn-admin-card ───────────────────────────┐ │
│ │ 🍇 saltyOrchard42  0x9f2a…7c41       You   │ │
│ └────────────────────────────────────────────┘ │
│ ┌────────────────────────────────────────────┐ │
│ │ 🥝 mintyPear19  0x4c1b…9ef2      [Remove]  │ │
│ └────────────────────────────────────────────┘ │
│                                                │
│ Add an admin                                   │
│ ┌────────────────────────────────────────────┐ │
│ │ 🔍  Search members                         │ │
│ └────────────────────────────────────────────┘ │
│ ┌─ .fn-picklist ─────────────────────────────┐ │
│ │ 🍒 sourCherry88  0x33aa…12ff  [Make admin] │ │
│ └────────────────────────────────────────────┘ │
├────────────────────────────────────────────────┤
│                                     [ Done ]   │
└────────────────────────────────────────────────┘
```

- Counter `.fn-admin-count` reads `n / 9`. At 9 it gets `data-full="true"` (red), the add
  section is replaced by "Admin limit reached. Remove an admin to add another."
- With exactly one admin the Remove button is disabled, title "A room needs at least one
  admin."
- Removing yourself opens a confirm: "Give up admin? You won't be able to manage this room."
- Candidates: room members who are not already admins and not you.

### Send funds — `.fn-modal`, three steps

| Step | When | Content |
| --- | --- | --- |
| 0 | always | Recipient card (`.fn-picklist__row`), "Amount (CRO)" mono right-aligned input, live "= {n} wei", `Advanced settings` disclosure: data hex, gas price (`topcoat-range` + mono input), gas limit, `[Estimate]`, computed gas fee, total cost. Footer `[Cancel] [Continue]`. |
| 1 | always | Read-only summary. Above 1 CRO also shows `.fn-banner--warn` "Large amount". Footer `[Back] [Send]`. |
| 2 | above 10 CRO | `.fn-banner--danger` plus a mono confirm field: retype the exact amount. `[Send]` stays disabled until it matches. |

Sending: spinner in the CTA, all inputs readonly, `aria-busy`. Success: receipt with tx
hash (mono, copy + explorer link), from/to, amount, gas used, balance before/after. Failure:
`.fn-banner--danger` with the revert reason, `[Back]` returns to step 0 with values intact.

Kick and Block/Unblock are single-step `.fn-modal--danger` confirms naming the person and
stating the consequence.

---

## 11. Screen 7 — Invitations inbox

**Purpose:** consent. An invitation creates nothing until it is accepted, so it needs a
place of its own, not just a toast that can be missed.

Two surfaces:

1. **Entry row** pinned at the top of the room list whenever `pending > 0`: envelope icon,
   "Invitations", `.fn-unread` count. Disappears at zero.
2. **The pane** at `/invitations`, and `.fn-bottomnav` item "Invites" on mobile.

```
┌─ .fn-pane--detail ─────────────────────────────────────────────┐
│ [←]  Invitations                                          2    │  topcoat-navigation-bar
├────────────────────────────────────────────────────────────────┤
│ ┌────────────────────────────────────────────────────────────┐ │
│ │ ┌────┐  Cold storage  🔒                                   │ │  .fn-picklist__row
│ │ │ 🍒 │  from mintyPear19 · 0x4c1b…9ef2 · 2 hours ago       │ │
│ │ └────┘                        [ Decline ] [   Accept   ]   │ │
│ └────────────────────────────────────────────────────────────┘ │
│ ┌────────────────────────────────────────────────────────────┐ │
│ │ ┌────┐  Weekend market                                     │ │
│ │ │ 🥭 │  from goldenMango51 · 0x9f8e…7d6c · yesterday       │ │
│ │ └────┘                        [ Decline ] [   Accept   ]   │ │
│ └────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────┘
```

Accept is `topcoat-button--cta`, Decline is `topcoat-button`. Decline is not red — declining
an invitation is not destruction.

| State | Presentation |
| --- | --- |
| **Loading** | Two `.fn-skel` cards. |
| **Empty** | `.fn-empty`: ✉ art, "No invitations", "When someone invites you to a room, it shows up here." |
| **Acting** | Only the acted-on card's two buttons disable and the CTA spins; the rest stay live. |
| **Accepted** | Card collapses to height 0, toast "Joined {room}", route to the new room, room list refreshes. |
| **Declined** | Card collapses, toast "Invitation declined". No undo — say so in the confirm-free copy by keeping Decline unmistakable. |
| **Encrypted room** | Emerald `.fn-lock` beside the room name and the note "You'll receive the room key when you accept." |
| **Stale** (room deleted, or you were already added) | Card shows `.fn-badge--muted` "No longer available", buttons replaced by `[Dismiss]`. |
| **Offline** | Cards render from cache with both buttons disabled and `.fn-banner--offline` at the top. |

Live: an `invitation_received` WebSocket frame prepends a card (`aria-live="polite"`
announcement: "New invitation to {room} from {user}") and increments both badges.

---

## 12. Screen 8 — Blocked users

Modal, `.fn-modal` (440px), from Settings → Blocked people → **Manage**.

```
┌─ .fn-modal ────────────────────────────────────┐
│ Blocked people                           [✕]   │
│ Blocked people can't invite you to rooms, and  │
│ you won't see their messages.                  │
├────────────────────────────────────────────────┤
│ [ + Block someone ]                            │
│                                                │
│ ┌─ .fn-picklist ─────────────────────────────┐ │
│ │ 🍒  sourCherry88                           │ │
│ │     0x33aa…12ff · blocked 14 Mar   [Unblock]│ │
│ ├────────────────────────────────────────────┤ │
│ │ 🥔  plainPotato03                          │ │
│ │     0x77ee…4411 · blocked 2 Feb    [Unblock]│ │
│ └────────────────────────────────────────────┘ │
│ 2 people blocked                               │
├────────────────────────────────────────────────┤
│                                     [ Done ]   │
└────────────────────────────────────────────────┘
```

**Add flow.** `[+ Block someone]` (`topcoat-button`, full width) expands into a
`.fn-field`: mono `.topcoat-text-input` "0x…", live validation against
`^0x[a-fA-F0-9]{40}$`, `[Cancel] [Block]` where Block is `topcoat-button--danger`, disabled
until the address is valid and is not your own.

| State | Presentation |
| --- | --- |
| **Invalid address** | `aria-invalid` + `.fn-field__error` "That's not a wallet address. It should be 0x followed by 40 hex characters." |
| **Own address** | "You can't block yourself." |
| **Already blocked** | "You already blocked this address." Field keeps the value. |
| **Loading** | `.fn-spinner` centred in the list area. |
| **Empty** | `.fn-empty`: "No one blocked", "Block someone from their row in a room, or paste an address above." |
| **Pending row** | That row's button spins; others stay live. |
| **Error** | `.fn-field__error` under the list, list unchanged. |

Blocking is bidirectional in effect: the client hides members and messages using both
`/api/users/blocked` and `/api/users/blocked-by`, and never distinguishes the two in the UI.

Toasts: "Blocked {short address}" / "Unblocked {short address}".

---

## 13. Screen 9 — Settings & profile

Pane at `/settings`. Identity first, then preferences, then destructive — always in that
order, and the two irreversible rows are visually separated from the rest.

```
┌─ .fn-pane--detail ─────────────────────────────────────────────┐
│ Settings                                                       │  h1
│                                                                │
│ ┌────────────────────────────────────────────────────────────┐ │
│ │  ┌──────┐   saltyOrchard42                                 │ │  profile card
│ │  │  🍇  │   0x9f2A3b4C5d6E7f8091a2B3c4D5e6F70819a7c41  [⧉] │ │  .fn-ident--xl
│ │  └──────┘   Cronos testnet · chain 338                     │ │  .fn-addr--full
│ │             12.4192 CRO                             [ ⟳ ]  │ │
│ └────────────────────────────────────────────────────────────┘ │
│                                                                │
│ PREFERENCES                                                    │  .topcoat-list__header
│ ┌────────────────────────────────────────────────────────────┐ │
│ │ 🌐  Language              English              [ Change ]  │ │
│ ├────────────────────────────────────────────────────────────┤ │
│ │ 🌗  Appearance      ( ) Light ( ) Dark (•) System           │ │  topcoat-radio-button
│ ├────────────────────────────────────────────────────────────┤ │
│ │ ⚡  Connection      (•) Live  ( ) Polling                   │ │
│ ├────────────────────────────────────────────────────────────┤ │
│ │ ⊘  Blocked people         2 blocked            [ Manage ]  │ │  → §12
│ ├────────────────────────────────────────────────────────────┤ │
│ │ 👁  Hidden rooms           1 hidden             [ Manage ]  │ │
│ └────────────────────────────────────────────────────────────┘ │
│                                                                │
│ ACCOUNT                                                        │
│ ┌────────────────────────────────────────────────────────────┐ │
│ │ 🔒  Recovery phrase on this device    [ ⧉ Copy ] [ Forget ] │ │
│ │    Saved, so reloading signs you back in without asking.   │ │
│ ├────────────────────────────────────────────────────────────┤ │
│ │ ⏻  Sign out                                    [ Sign out ]│ │
│ ├────────────────────────────────────────────────────────────┤ │
│ │ 🗑  Erase local data                             [ Erase ]  │ │  topcoat-button--danger
│ │    Removes cached messages, room keys and settings         │ │
│ │    from this device. Your wallet is not affected.          │ │
│ └────────────────────────────────────────────────────────────┘ │
└────────────────────────────────────────────────────────────────┘
```

Rows are `.fn-picklist__row` inside `.topcoat-list__container`; section labels are
`.topcoat-list__header`.

**Profile card.** `.fn-ident--xl` seeded from the wallet address, username, full checksum
address in `.fn-addr--full` with a copy button, chain name and id, live balance with a
refresh (`topcoat-icon-button--quiet`, spins). Balance states: `.fn-spinner` while
fetching; the figure in `--fn-t-2xl` when loaded; "Balance unavailable · [Retry]" on
failure. There is nothing editable — the wallet *is* the profile.

**Recovery phrase on this device** reflects `crate::vault`. It states what is held and
what that costs — "Anyone who can use this browser profile can read your messages" — and
offers `[Copy]` (phrase credentials only; a private key has nothing to show that the
address does not) and `[Forget]`. Forget is confirmed, because the next reload will then
ask for a phrase the user may not have to hand; it clears the credential *and* the
preference, so the next sign-in does not silently write it back. When nothing is stored
the row still renders, saying so — "not saved" is information, and a row that vanishes
when it has nothing to report is a row nobody can find.

**Erase local data** opens `.fn-modal--danger`: "Erase local data?", the same body copy as
the row, `[Cancel] [Erase]`. Then it clears IndexedDB and `localStorage` — the vault
included — signs out and routes to `/login`. It never touches the mnemonic file the user
downloaded.

**Hidden rooms** is a `.fn-modal` mirroring §12: room chip, name, "hidden {date}",
`[Unhide]`; empty state "No hidden rooms" / "Hiding a room removes it from your list but
keeps you a member."

| State | Presentation |
| --- | --- |
| **Offline** | Balance row shows "Unavailable offline"; Language, Appearance and Connection stay usable; Blocked/Hidden `[Manage]` disabled. |
| **Language changing** | Applied immediately, persisted to `localStorage`; a toast confirms in the *new* language. |

---

## 14. Screen 10 — Not found

Full-screen, no shell — a broken URL should not imply a working session.

```
              ┌──────────────────────────┐
              │           🍋             │  .fn-empty__art
              │                          │
              │           404            │  .fn-404__code
              │      Page not found      │  .fn-empty__title
              │                          │
              │  That address doesn't    │  .fn-empty__desc
              │  point anywhere.         │
              │                          │
              │  [ Go to your rooms ]    │  topcoat-button--cta
              └──────────────────────────┘
```

`.fn-404`, centred, `--fn-bg`. The CTA routes to `/rooms` when a JWT exists, `/login`
otherwise. `<title>` becomes "Page not found · FruitNation". No developer-facing copy —
the React client's "Did you forget to add the page to the router?" is a note to itself,
not to a user.

---

## 15. Cross-cutting components

### Toasts — `.fn-toasts` / `.fn-toast`

Bottom-centre on mobile, top-right at ≥768px. Max **3** stacked; a fourth evicts the
oldest. A 3px status stripe carries the variant: `--success` emerald, `--error` red,
`--warn` yellow, `--info` blue, default neutral. Title (13px bold) + optional description
(13px muted) + close.

Auto-dismiss: 4s default, 6s with a description, **never** for `--error` — errors stay
until dismissed. The React client's effective 16-minute timeout with a one-toast limit is
not carried over.

`role="status"` / `aria-live="polite"`, except `--error` which is `role="alert"` /
`assertive`. Hover and focus pause the timer. Toasts confirm; they never carry the only
copy of information a user needs.

### Modals — `.fn-modal-backdrop` / `.fn-modal`

`role="dialog"`, `aria-modal="true"`, labelled by `.fn-modal__title` and described by
`.fn-modal__desc`. Focus moves to the first interactive element (or the title for
confirms), is trapped, and returns to the trigger on close. `Esc` and backdrop click close
— except while a mutation is in flight, when both are ignored and the close button is
disabled.

Widths: 440px default, 620px `--wide`. Full-width and 92dvh tall below 900px, with footer
buttons stretched. Body scrolls; header and footer are fixed.

Footer order is always `[secondary] [primary]`, primary last, ≥96px wide.

**No `window.confirm` anywhere.** Every confirmation is a `.fn-modal--danger` naming the
object and the consequence: title as a question, body as the irreversible fact, buttons
labelled with the verb ("Delete room", not "OK").

### Empty, loading, error

- **Empty** `.fn-empty`: 84px `.fn-empty__art` (fruit emoji on an orange wash, gentle 5.5s
  float), title, ≤38ch description, optional CTA. Every empty state names the next action.
- **Loading**: `.fn-skel` shimmer rows where the shape is known (lists), `.fn-spinner` where
  it isn't. Never show a loader before 400ms.
- **Error** `.fn-empty--error`: red wash art, what failed, the server's message, and a retry
  control. Errors state what happened and what to do; they never apologise and never say
  "something went wrong".

### Offline

A single source of truth: `navigator.onLine` plus transport health. When offline —
connection pill `--offline`, `.fn-banner--offline` in the active pane, all mutating controls
disabled except the composer (which queues). Cached content stays visible and readable. On
reconnect: pill returns to `--syncing`, queued sends flush oldest-first, one toast
"Back online".

---

## 16. Responsive rules

Single breakpoint at **900px**. Below it the app is one column; above it, two panes.

| | `<900px` (single column) | `≥900px` (two-pane) |
| --- | --- | --- |
| Panes | One at a time via `.fn-panes[data-view]`; the other is `display:none`, still mounted | Both visible, list fixed at 340px (380px ≥1400px) |
| Navigation | `.fn-bottomnav`, 5 items, 56px + safe-area | Top-bar links only; no bottom nav |
| Back | `.fn-back` in `.fn-chat__head`, returns to the list | Hidden — the list is already on screen |
| Address in top bar | `0x9f2a…7c41` | Full EIP-55 checksum |
| Bubble max width | 84% | `min(62ch, 100%)` |
| Modals | Full width, 92dvh, footer buttons stretched | 440/620px centred, footer right-aligned |
| Toasts | Bottom centre, full width | Top right, 380px |
| Stream padding | 12px | 16px (24px ≥1400px) |
| Hit targets | 44px minimum (`@media (pointer: coarse)`) | 32px is fine |
| Row actions | Always visible | Revealed on hover / focus-within |

Safe areas: `env(safe-area-inset-top)` is absorbed by the ribbon (or the top bar when there
is none); `env(safe-area-inset-bottom)` by the bottom nav and the composer.

`100dvh` — not `100vh` — on `.fn-app`, `.fn-login` and `.fn-404`, so mobile browser chrome
never clips the composer.

### 16.1 Login (§5) — four frames, not one

The sign-in screen is the one place with artwork big enough to fight the form, so it
switches layout on **orientation as well as size**. The artwork is a fixed backdrop by
default and is re-anchored per frame; `.fn-art--login` is deliberately un-themed (see §5).

| Frame | Condition | Layout |
| --- | --- | --- |
| Desktop landscape | `≥1000px` and `≥620px` tall | Split: form left, artwork owns from 42% rightward, `cover` so the face is full-size |
| Phone landscape | landscape and `≤619px` tall | Same split, tighter — artwork from 58%, card left-aligned |
| Tablet / desktop portrait | portrait, `>700px` | Full-bleed `cover` behind a centred card; the source is tall so the whole figure reads |
| Phone portrait | portrait, `≤700px` | **Stacked**: artwork becomes a 46vh banner (`absolute`, so it scrolls away), form beneath it |

The phone-portrait stack exists because the card is full-width at that size — a full-bleed
backdrop behind it is a backdrop nobody can see, and the artwork showed only a sliver of
shoulder down one edge. `cover` is used everywhere rather than `contain`: `contain` leaves a
dead band between the form and the image on desktop.

Below 360px the room-row aside collapses to the unread chip only (time is dropped) and
`.fn-chat__submeta` drops the sync time. Nothing is ever horizontally scrollable except
code-like content, which scrolls inside its own container.

---

## 17. Accessibility

**Target: WCAG 2.2 AA.**

### Contrast

Every pairing in §2.2 is verified in both schemes:

| Pairing | Light | Dark |
| --- | --- | --- |
| `--fn-fg` on `--fn-surface` | 15.9:1 | 13.4:1 |
| `--fn-fg-muted` on `--fn-surface` | 6.2:1 | 6.0:1 |
| `--fn-fg-faint` on `--fn-surface` | 4.6:1 | 4.5:1 |
| `--fn-primary-ink` on `--fn-primary` | 4.7:1 | 4.7:1 |
| `--fn-encrypt` on `--fn-surface` | 4.8:1 | 7.1:1 |
| `--fn-crown` on `--fn-crown-soft` | 4.6:1 | 8.9:1 |
| `--fn-danger` on `--fn-surface` | 5.4:1 | 6.3:1 |

`--fn-fg-faint` is only ever used at 11px+ for timestamps and placeholders, and never as
the sole carrier of meaning.

Fruit-chip washes use fixed lightness (92% light / 24% dark), so contrast is identical at
every hue.

### Colour independence

No state is signalled by colour alone.

| State | Non-colour signal |
| --- | --- |
| Selected room | 3px left rail + `aria-selected="true"` |
| Unread | Count chip + bold weight |
| Own vs other message | Side, corner geometry, avatar presence |
| Encrypted | Lock glyph |
| Admin | Crown glyph + "Admin" text |
| Rotation pending | Banner text + locked composer |
| Connection | Text label ("Live" / "Polling" / "Offline") |
| Verified on chain | Check glyph + "verified" |
| Reacted by you | `aria-pressed="true"` |
| Invalid field | `aria-invalid` + `.fn-field__error` text |

### Focus

One treatment everywhere: `2px solid var(--fn-focus)` at `2px` offset, applied via
`:focus-visible`. It is never removed — where it would be clipped, the container gets
padding. Under `forced-colors: active` it becomes `2px solid Highlight`.

A `.fn-sr-only` skip link ("Skip to messages") is the first focusable element and becomes
visible on focus.

### Keyboard

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Between landmarks and controls; trapped inside open modals |
| `↑` `↓` | Room list rows and members (roving tabindex, single tab stop per list) |
| `Home` / `End` | First / last row |
| `a`–`z` | Type-ahead in the room list |
| `Enter` / `Space` | Activate the focused row or button |
| `Enter` | Send from the composer |
| `Shift+Enter` | Newline in the composer |
| `Esc` | Close modal, picker or menu; clear the search field; cancel an inline edit |
| `Ctrl/Cmd+K` | Focus room search |
| `Ctrl/Cmd+Enter` | Confirm the primary action in an open modal |

No key traps. Every hover-revealed control (`.fn-msg__tools`, `.fn-person__actions`) also
appears on `:focus-within`, so keyboard users reach it.

### Semantics

- Landmarks: `banner` (top bar), `navigation` (bottom nav), `complementary` (room list),
  `main` (detail pane), `contentinfo` where present.
- Room list is `role="listbox"` with `role="option"` rows; members and invitations are plain
  `<ul>`/`<li>`.
- The message stream is `role="log"` with `aria-live="polite"` and `aria-relevant="additions"`.
  Incoming messages announce as "{username}: {text}". History loads do not announce.
- Fruit chips are `aria-hidden="true"` — always.
- Addresses expose the full checksum via `aria-label` even when the text is truncated:
  `aria-label="Copy wallet address 0x9f2A…7c41"`. A copyable one is a `<span
  role="button" tabindex="0">`, not a `<button>` — see below — with Enter and Space
  wired to the same handler as the click.
- Icon-only buttons carry an `aria-label` that names the object, not the icon: "Remove
  sourCherry88 from this room".
- Unread counts announce as "12 unread messages", not "12".
- Live regions: typing indicator `polite`, toasts `polite` (errors `assertive`), new
  invitations `polite`.
- Modal titles are `<h2>`; heading order never skips a level.
- `lang` on `<html>` tracks the chosen language so screen readers switch voice.

### Motion

`prefers-reduced-motion: reduce` collapses every animation and transition to 0.01ms
(`app.css` §14). Specifically: the typing dots stop but the text stays; message entrance
animations are removed; the empty-state float stops; skeleton shimmer stops at a flat fill;
modals and toasts appear without transform. Nothing conveys information through motion
alone.

### Touch

44×44px minimum on coarse pointers. Related controls are separated by ≥8px. Destructive
actions are never adjacent to their non-destructive neighbour without a gap — the modal
footer puts `[Cancel]` first with 8px of separation.

---

## 18. Class name index for Yew

Everything below is defined in `web/static/app.css`. Topcoat classes are marked `TC`.

**Shell** `fn-app` · `fn-ribbon` · `fn-topbar` `__identity` `__name` `__addr` `__actions` ·
`fn-panes[data-view]` · `fn-pane` `--list` `--detail` · `fn-bottomnav` `__item` `__badge` ·
`fn-back`

**Primitives** `fn-ident` `--xs` `--sm` `--lg` `--xl` `--self` `--online` (`style="--fn-hue"`) ·
`fn-addr` `--full` · `fn-badge` `--admin` `--self` `--muted` `--danger` `--info` `--encrypt` ·
`fn-unread` `--dot` · `fn-lock` `--off` `--pending` · `fn-crown-icon` · `fn-conn` `--ws`
`--poll` `--syncing` `--offline` · `fn-spinner` `--lg` `--on-primary` · `fn-rule`

**Room list** `fn-roomlist__head` `__body` `__empty` `__loading` · `fn-room-row`
`[aria-selected]` `.is-unread` `.is-rotation-pending` · `__avatar` `__title` `__name`
`__meta` `__preview` `__aside` `__time`

**Chat** `fn-chat` `__head` `__title` `__submeta` `__actions` · `fn-banner` `--warn`
`--danger` `--info` `--offline` `__actions` · `fn-stream` · `fn-daymark` · `fn-msg` `--own`
`--grouped` `--pending` `--failed` · `__avatar` `__sender` `__time` `__foot` `__tools` ·
`fn-bubble` `--deleted` `--sealed` · `fn-hash` `--verified` · `fn-sysmsg` `--rotation` ·
`fn-reactions` · `fn-reaction[aria-pressed]` `__emoji` · `fn-picker` `__cell` · `fn-typing`
`__dots` · `fn-composer[data-locked]` `__input` `__hint`

**People** `fn-people` · `fn-person` `--self` `__name` `__actions` · `fn-admin-card` ·
`fn-admin-count[data-full]`

**Modals** `fn-modal-backdrop` · `fn-modal` `--wide` `--danger` · `__head` `__title`
`__desc` `__close` `__body` `__foot` · `fn-field` `__label` `__help` `__error` ·
`fn-toggle-row[data-on]` · `fn-picklist` `__row`

**Feedback** `fn-toasts` · `fn-toast` `--success` `--error` `--info` `--warn`
`[data-leaving]` · `__body` `__title` `__desc` `__close` · `fn-empty` `--error` `__art`
`__title` `__desc` · `fn-skel`

**Login** `fn-login` `__card` `__brand` `__mark` `__wordmark` `__tagline` `__hero`
`__error` · `fn-langs` · `fn-lang[aria-pressed]` · `fn-hero-btn` `__label` · `fn-tabs` ·
`fn-tab[aria-selected]` · `fn-tabpanel` · `fn-mnemonic[data-masked]` `__tools` ·
`fn-warnpanel` `__title` · `fn-stepper`

**Not found** `fn-404` `__code`

**Utilities** `fn-scroll` · `fn-stack` · `fn-row` `--wrap` · `fn-grow` · `fn-push` ·
`fn-muted` · `fn-faint` · `fn-truncate` · `fn-nums` · `fn-sr-only`

**Topcoat** `TC topcoat-button` `--large` `--cta` `--quiet` `--danger`* ·
`TC topcoat-icon-button` `--large` `--quiet` · `TC topcoat-text-input` `--large` ·
`TC topcoat-textarea` · `TC topcoat-search-input` · `TC topcoat-switch` ·
`TC topcoat-checkbox` · `TC topcoat-radio-button` · `TC topcoat-range` ·
`TC topcoat-navigation-bar` `__title` `__item` · `TC topcoat-list` `__header` `__container`
`__item`

\* `topcoat-button--danger` is a project variant defined in `app.css` §3.

**Wallet (§10, extended)** `fn-wallet` `__addr` `__network` `__balances` `__balance[data-token]`
`__symbol` `__value` `__advanced` `__summary` `__sending` `__pulse` `__confirm` `__receipt[data-ok]`
`__verdict` · `fn-topbar__wallet`

**Sign out** `fn-topbar__signout` — an icon button in `fn-topbar__actions`, last in the row,
so leaving is one click from every screen rather than buried at the bottom of Settings.
Opens the same `ConfirmAction::SignOut` dialog Settings uses; tinted `--fn-danger-text` on
hover/focus to read as distinct from the Settings gear beside it.

**AI assistant** `fn-ai` `__tabs` `__tab[aria-selected]` `__draft` `__preview` `__keys` ·
`fn-composer__ai`

**Skynet layer** — the cinematic asset set (`static/img/skynet-*.png`, generated by
`tools/genart.py`, "cinematic" manifest entries) and the HUD theatre in `app.css` §18.1:
the ambient `fn-app::before` grid backdrop, the reactor pulse (`fn-reactor`), the ribbon
scanline (`fn-scan`), the testnet wallet-button breathe (`fn-breathe`), and the CTA optic
glow (`--fn-glow`). All motion rides the §1 easing tokens (`--fn-ease-expo`,
`--fn-ease-cosine`) and is disabled wholesale by §17 reduced-motion.

**Boot sequence (§18.2)** `fn-boot` `__sky` `__stage` `__sphere` `__sparks` `__flash` `__skull`
`__bloom` `__panel` `__title` `__log` `__label` `__dots` `__status` `__bar` `__skip` — the sign-in
cold open (`components/boot.rs`). One 4.7s CSS timeline in four acts (arrival · ignition · boot log ·
handoff); Rust owns only the timer that ends it. Assets `boot-sphere.png` / `boot-endoskull.png` are
generated by `tools/genart.py` and masked with a long radial falloff, because their near-black source
backgrounds otherwise read as a box. Escapable two ways: the SKIP button, and `prefers-reduced-motion`,
which drops the arrival entirely and shortens the timer to 700ms.

---

## 19. Screen 11 — Bank (2026-07-31)

Route `/bank` (topbar 🏛 button; `pane_view` behaves like Settings on narrow viewports).
The Bank left its dialog: six tabs, a portfolio hero and an agent chat do not fit a modal.
Reference: the FruitNation React client's `pages/bank.tsx` + `services/bankAgent.ts`.

**Layout** (`components/bank.rs`). `.fn-bankpage` is the scroll container. Header row:
`.fn-art--bank` badge, H1 in the display face, the universal-wallet hint, and the
Mainnet/Testnet radiogroup (`.fn-bank__nets`, persisted as `ps-bank-net`). Body is a
one-column grid that becomes `200px minmax(0,1fr)` at ≥980px: the tab strip
(`.fn-bankpage__rail`, `ps-bank-tab` persisted) turns into a sticky left rail
(`grid-auto-flow: row`), content capped at 880px measure. Tabs: Portfolio · Send · Swap ·
Tokens · Greeter · AI Banker, each with an icon (`icons.rs`).

**Portfolio.** `.fn-bank__hero` carries the cinematic vault hall
(`tools/genart.py bank-vault-hall`, themeless) behind a left-heavy scrim; balance figure
`clamp(28px…44px)`, address chip, explorer link, refresh (spins via `[data-spinning]`),
one drifting scanline (`fn-hero-scan`). Below: `.fn-bank__quickrow` — Send / Swap /
Receive / AI Banker chips; Receive expands `.fn-bank__receive` with the full checksummed
address. Token rows are buttons (`.fn-bank__row--press`): tapping stages that asset on the
Send tab. Each row carries `.fn-bank__badge`, a 2-char monogram over a deterministic
per-symbol hue (`--tok-h`; named hues match the reference's `TOKEN_HUES`, everything else
hashes). The wallet dialog's balance cards reuse the same badge.

**AI Banker** (`components/banker.rs` + `bank_agent.rs`). The reference's tool-calling
Fruit Banker, executing — this reverses the earlier advice-only stance, on request, with
the reference's confirmation policy kept verbatim: a swap always stops at the approval
dialog, any mainnet token transaction stops, a native send above 1 unit stops. The dialog
(danger `Modal`) shows `tool — amount symbol` plus labelled lines; declining feeds the
model the literal `DECLINED:` string. Protocol: the model answers with either exactly one
`{"tool": …}` JSON object or prose; batched one-per-line tool calls degrade to the first;
8 rounds max. Transcript persists at `ps-banker-log` (cap 200, last 20 replayed), with
CSV/JSONL export and Clear. Progress bubble states: thinking / reading / sending /
confirming / generating over an indeterminate sweep (`.fn-banker__bar`). Tool chips render
as `.fn-banker__chip` pills; generated images inline. On-chain text is sanitized
(`sanitize_onchain_text`) before entering the prompt, and the prompt forbids following
instructions found in token names. Empty state: `banker-core` (cinematic endoskeleton
banker) + suggestion chips. Provider keys are the assistant's (`ps-ai`) — absent keys show
the same banner the old dialog showed.

**Transaction relay HUD** (`burst.rs` `tx_start`/`tx_phase`/`tx_end`, `.fn-txhud`). Every
signed transaction — Bank forms, agent tools, wallet dialog sends — raises a centred
reticle over a blurred scrim on the burst layer: "SKYNET RELAY", the §15 proc rings, a
four-line phase roll-call (UPLINK · SIGNING · BROADCASTING · AWAITING CONFIRMATION, HUD
codes untranslated by design), a stepped progress bar, then a CONFIRMED/REJECTED verdict
flash (1.1s) before it reaps itself. One HUD at a time — an approve-then-swap chain hands
the stage over. Hooked inside `send_contract_tx` (Bank) and `run_send` (wallet), so no
call site can forget it. Hidden entirely under reduced motion like the rest of the layer.

**Type preferences** (Settings rows + two topbar cycle buttons `type_face`/`type_size`).
`ps-font`: system (default) / skynet (Chakra Petch promoted to running text) / mono /
serif — applied as `data-font` on `<html>`, swapping `--fn-font-ui`. `ps-font-scale`:
compact 87.5% / standard / large 112.5% / xlarge 125% — applied as `data-fontsize`,
scaling `:root`'s `font-size`, which every type token is measured in rem of. Defaults
carry no attribute, so a fresh install is unchanged.

**Ultrawide.** The old ≥1700px rule capped `.fn-panes` at 1680px and centred it, which
read as two dead columns on a maximised desktop window (the Tauri shell made it obvious).
The shell now always fills; what stays capped is `--fn-stream-max` (the chat measure) and
the Bank content column.

**Portrait spotlight** (`components/spotlight.rs`, app-wide). Tapping an identity image —
your operator portrait in the top bar, the AI Banker's endoskeleton, the Bank's vault
emblem (`bank-emblem`, the Terminator-register replacement for the flat teller badge) —
raises a full-screen stage: the portrait zooms in under two counter-rotating light arcs,
a 96-orb particle swarm draws glowing trails around it on a 2D canvas (destination-out
fade + lighter compositing; orbit maths pure and host-tested), and the stage tilts in 3D
toward the pointer. The accent hue is the identity hue for people and 190 for machines,
with every ninth orb burning crimson. The top bar's old silent copy-on-click became a
labelled copy button inside the stage. Singleton like the burst layer; Escape, the close
button or a scrim click dismisses; under reduced motion the canvas and arcs are skipped
and the portrait simply appears. No scene-graph library, same reasoning as the GL
backdrop.
