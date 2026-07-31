# PocketSkynet — Motion

The reference React client (`server/client/src/lib/motion.ts`) drives its animation with
Framer Motion. This client is Yew → WASM and ships no animation runtime, so the same
vocabulary is expressed in CSS: custom properties in `web/static/app.css` §1, keyframes
and utilities in §14.

Two of Framer's four transition kinds port directly — a `duration + ease` tuple *is* a CSS
`transition`. The other two are physical springs, which CSS has no notion of. This document
shows how each spring was reduced to a curve or a keyframe set, and what was lost.

> **The curves are the reference client's. The durations are not.**
>
> Every `--fn-dur-*` token was rescaled in the Netflix-direction pass to land inside
> **150–250ms**, because perceived speed is part of that brief and a 380ms entrance that
> reads as "considered" in isolation is 380ms of a message not being on screen. The tables
> below still give each spring's *derived* settling time, which is what the maths produces
> and what the shape of the curve is based on; the token beside it is what actually ships.
> The one long animation left in the sheet is `floatLoop`, which is ambient and which
> nothing waits for.
>
> Shapes were not touched: overshoot, damping and the choice of curve-vs-keyframes are all
> as derived here.

---

## 1. The curves that port exactly

| motion.ts | CSS token | Value |
|---|---|---|
| `easeOutExpo` | `--fn-ease-expo` / `--fn-dur-expo` | `cubic-bezier(0.22, 1, 0.36, 1)`, 350ms |
| `easeCosine` | `--fn-ease-cosine` / `--fn-dur-cosine` | `cubic-bezier(0.37, 0, 0.63, 1)`, 400ms |
| `viewVariants.exit` ease | `--fn-ease-exit` / `--fn-dur-exit` | `cubic-bezier(0.4, 0, 1, 1)`, 180ms |
| `floatLoop` | `fn-float` / `--fn-dur-float` | `y: 0 → -8px → 0`, 5s, ease-in-out, infinite |

`floatLoop` is `easeInOut`, which Framer maps to `cubic-bezier(0.42, 0, 0.58, 1)`. The
keyframe uses `--fn-ease-cosine` (`0.37, 0, 0.63, 1`) instead — the same half-cosine shape,
already in the token set, and indistinguishable at 5s.

---

## 2. Reducing a spring to a curve

Framer's spring is a damped harmonic oscillator. With mass `m = 1`:

```
ω₀ = √k                      undamped natural frequency (rad/s)
ζ  = c / (2·√k)              damping ratio
```

From those two numbers everything else follows:

```
envelope time constant   τ  = 1 / (ζ·ω₀)
visible duration         T  ≈ 5τ            (settles inside ~1%)
first overshoot          Ov = exp(-πζ / √(1 - ζ²))
damped period            Td = 2π / (ω₀·√(1 - ζ²))
```

`Ov` is the decision point:

* **`Ov` below ~2%** — the motion never visibly crosses its target. A cubic-bezier that
  ends flat reproduces it; only the duration has to be right.
* **`Ov` between ~2% and ~10%** — one small bounce. A cubic-bezier whose second control
  point sits above 1 overshoots by roughly `y₂ - 1` and then settles, which is close enough.
* **`Ov` above ~10%** — multiple visible crossings. No bezier can do this (a cubic-bezier
  timing function crosses `y = 1` at most once). It needs explicit keyframes placed at the
  oscillation's own peaks, which is why `Td` is computed.

### The four springs

| motion.ts | k | c | ω₀ | ζ | T (5τ) | Overshoot | Td | CSS |
|---|---|---|---|---|---|---|---|---|
| `springSnappy` | 460 | 38 | 21.45 | 0.886 | 263ms | 0.3% | — | `--fn-spring-snappy`, `--fn-dur-snappy: 260ms` |
| `springSoft` | 320 | 30 | 17.89 | 0.839 | 335ms | 0.8% | — | `--fn-spring-soft`, `--fn-dur-soft: 340ms` |
| `bubbleVariants` | 380 | 26 | 19.49 | 0.667 | 385ms | 6.0% | 433ms | `--fn-spring-bubble`, `--fn-dur-bubble: 380ms` |
| `springBouncy` | 640 | 16 | 25.30 | 0.316 | 625ms | 35.1% | 262ms | `fn-pop` keyframes, `--fn-dur-bouncy: 520ms` |

**`springSnappy` → `cubic-bezier(0.19, 0.91, 0.26, 1)` at 260ms.**
ζ = 0.886 is very nearly critical: 0.3% overshoot is a third of a pixel on a 100px move.
A flat-landing ease-out with an aggressive early rise matches the envelope. Used for
anything that responds to a press — buttons, the connection pill, modals, toasts, tabs.

**`springSoft` → `cubic-bezier(0.21, 0.84, 0.29, 1)` at 340ms.**
Same treatment, 27% slower and slightly gentler off the line. Used for list entrances.

**`bubbleVariants` (380/26) → `cubic-bezier(0.16, 1.06, 0.32, 1)` at 380ms.**
6% overshoot, peaking at ~216ms. The second control point's `y = 1.06` produces an
overshoot of about the same magnitude at about the same fraction of the run. A message
bubble travelling 26px therefore overshoots by ~1.5px and settles, exactly like the spring.

**`springBouncy` (640/16) → the `fn-pop` keyframes at 520ms.**
ζ = 0.316 is the outlier. The oscillation crosses its target three times before it is
invisible:

```
t = 0          scale 0.30   (hidden: gridItem starts at 0.3)
t = Td/2 =131ms  peak  +35%   → 1.35
t = Td   =262ms  trough -12%  → 0.88
t = 3Td/2=393ms  peak   +4%   → 1.04
t = 2Td  =524ms  settled       → 1.00
```

Those four instants are 25% / 50% / 75% / 100% of a 524ms run, which is where the `fn-pop`
keyframes sit — so the keyframe percentages are the spring's own zero-crossings rather than
eyeballed numbers. `--fn-spring-bouncy` (`cubic-bezier(0.33, 0, 0.67, 1)`, a symmetric
ease-in-out) is the *per-segment* easing between those stops, matching the sinusoid between
successive extremes. Used for emoticon cells, reaction chips, badges and unread counts.

---

## 3. Stagger

`listContainer` staggers children by `0.04s`; `gridContainer` by `0.012s`. CSS has no
parent-driven stagger, so the index is passed down as a custom property — the Yew
components set `style="--i: {n}"` on each row — and the delay is computed per element:

```css
animation-delay: min(calc(var(--i, 0) * var(--fn-stagger)), var(--fn-stagger-max));
```

The cap matters. Framer's stagger is also unbounded, but its lists are virtualised; this
room list is not. Without `min()`, row 200 would enter eight seconds after row 1. The caps
are 320ms (8 rows) for lists and 190ms (16 cells) for the emoticon grid — past those,
everything remaining arrives together, which is what a fast stagger looks like anyway.

`--i` is set on: room rows, member rows, pick-list rows (invitations, invite search, blocked
users, hidden rooms, admin candidates), admin cards, and emoticon grid cells.

---

## 4. Where each preset is applied

| motion.ts preset | CSS | Applied to |
|---|---|---|
| `listItem` + `listContainer` | `fn-list-in` + `--i` stagger | room rows, member rows, pick-list rows, admin cards, day markers, warning panel |
| `bubbleVariants.show` | `fn-bubble-in` | `.fn-msg` |
| `gridItem` + `gridContainer` | `fn-pop` + `--i` grid stagger | `.fn-picker__cell` |
| `springBouncy` | `fn-pop` | `.fn-badge`, `.fn-unread`, `.fn-reaction`, `.fn-conn--ws`, `.fn-conn--poll` |
| `tapScale` | `--fn-tap-hover` / `--fn-tap-press` + `.fn-tap` | every Topcoat button and icon button, the connection pill, reaction chips, emoticon cells, language pills, the identity button, room-row avatars |
| `viewVariants.show` | `fn-view-in` | `.fn-view` (route section), `.fn-empty`, `.fn-tabpanel` |
| `easeOutExpo` | `--fn-ease-expo` | view transitions, login card |
| `easeCosine` | `--fn-ease-cosine` | backdrop cross-fade, typing dots, pulse, float |
| `floatLoop` | `fn-float` | `.fn-empty__art`, `.fn-login__mark`, the boot logo |
| — (new) | `fn-modal-in` / `fn-fade` | dialog panel / backdrop |
| — (new) | `fn-toast-in` / `fn-toast-out` | toasts |
| — (new) | `fn-menu-in` | `.fn-picker` (both the emoticon grid and the `⋮` menus) |
| — (new) | `fn-shake` | offline pill, login error |
| — (new) | `fn-banner-in` | `.fn-banner` |
| — (new) | `fn-nav-underline` | active bottom-nav item |
| — (new) | `fn-breathe` | unacknowledged (pending) bubbles |

The connection pill uses a trick worth naming: each transport state carries its *own*
`animation`. Because the modifier class changes when the transport changes, the animation
re-runs. That is the only way to get "animate on state change" out of pure CSS, and it is
why `.fn-conn--ws`, `.fn-conn--poll` and `.fn-conn--offline` each declare one.

---

## 5. What did not port

**Exit animations.** `viewVariants.exit`, `bubbleVariants.exit` and Framer's
`AnimatePresence` all depend on keeping an unmounted subtree alive long enough to animate
it out. Yew removes the node immediately. The tokens (`--fn-ease-exit`, `--fn-dur-exit`)
exist and are used where the leaving element *is* still under our control — the toast
stack, which sets `data-leaving="true"` before removal. Everywhere else, exits are
instantaneous. Fixing this properly needs a presence wrapper in Yew, not more CSS.

**The fruit-herald choreography.** `fruitHeraldVariants` and `bubbleBloomVariants` — the
two-beat "sender's fruit tumbles in, bursts, and the bubble blooms out of it" sequence — are
deliberately not reproduced. They need a per-message overlay element positioned over the
bubble, animating `rotate`, `scale`, `filter: blur()` and `originX` on independent
timelines with different delays, plus a particle burst. That is markup and state, not
styling, and this pass was scoped to class names and stylesheet. `.fn-msg` gets the row
entrance (`bubbleVariants`) only.

**Retriggering on value change.** A CSS animation runs on mount. An unread count going
from 3 to 4 re-renders the text but does not remount `.fn-unread`, so its `fn-pop` does not
replay. Framer animates on every value change. Where the *class* changes (the connection
pill) this is worked around; where only text changes it is not.

---

## 6. Reduced motion

`@media (prefers-reduced-motion: reduce)` in app.css §17 collapses every animation and
transition to `0.01ms` and — importantly — forces `animation-delay: 0ms`. A zero-duration
animation with a 320ms stagger delay is still a row that appears 320ms late for no reason,
which is precisely the complaint the media query exists to answer.

The block additionally pins every transform-based resting state (`tapScale` hovers, the
hover-revealed message tool rail) to `none !important`, so a transition cancelled mid-flight
cannot strand an element at 1.015× scale.

Anything new added to this stylesheet must be reachable from that block. Two rules of
thumb: prefer animating `opacity`/`transform` (the block already covers them), and never
express a *resting* state as a transform that only a `:hover` rule removes.

## 7. The choreography ports (2026-07-29)

Three effects from the reference client that §5 originally listed as not ported now are —
re-expressed, not re-implemented: framer-motion drove them per-frame from JS; here the
compositor runs CSS keyframes and Rust only computes endpoints.

**Bubble bloom** (`bubbleBloomVariants` → `fn-bubble-bloom`, app.css §7). The newest
message's bubble springs from its tail corner: scale `0 → 1.14 → 0.92 → 1.05 → 1` at the
reference's own times `[0, .38, .62, .82, 1]` over 550ms, sharpening from a 5px blur.
Each swing is ~60% of the previous — a damped exponential by construction, so no bezier
could carry it and the keyframes do (per-segment easing `--fn-spring-bouncy`, the §2
convention). Bound to `.fn-msg:last-child` so displacement *removes* the animation rather
than restarting anything; history loads stay still. A pending (just-sent) bubble blooms
and then resumes its breathing loop, comma-joined in one `animation` list.

**Arrival lock-on** (`fn-bubble-charge` + `fn-bubble-scan` + `fn-msg-arc`, app.css §7).
Three passes ride the bloom so a row reads as *made by a machine* rather than moved into
place. The **beam** is one hard seam with a white core, not a wide sheen — a soft wash reads
as polish, a bright line reads as something writing the text. The **charge** is a border
flash on `steps(1, end)`: two beats of glow with a dropout between them, discrete because a
machine confirming something does not fade in. The **arc** flings four sparks off the tail
corner from a single box-shadow node, mounted on `.fn-msg` rather than `.fn-bubble` so the
bubble's `overflow: hidden` cannot clip it (`--own` flips it to the opposite corner). All
three are suppressed under reduced motion, leaving the bloom alone.

**Disintegration** (`fn-bubble-dissolve` + `fn-debris`, app.css §7). The row is overloaded
before it collapses: a stepped double flare past full brightness with a `hue-rotate` toward
the termination tone, *then* the snap to a line and out. Going straight to the line read as
a window closing rather than something being unmade. The debris node is cast in the same
hot orange-red and scattered asymmetrically — an even ring reads as decoration, a lopsided
one reads as a blast. Cyan is deliberately absent here: it is the colour of the system
working, and deleting is not that.

**Spark burst** (`FruitBurst.tsx` → `burst.rs` + `fn-burst-p`). The particle layer keeps
the reference's structure — three-point path [origin, apex, rest], xorshift scatter seeded
by burst id, one span per particle driven by custom properties — but not its tuned
constants, because the reference was throwing fruit and this throws current. Sparks leave
faster, travel further and die sooner than confetti: a discharge is over before a
celebration has finished rising.

Two thirds of a burst are **streaks** (`Ink::Streak`) — hairline rects, white at the head
and toned behind, rotated by `--a` to lie along their own velocity, stretched on X only so
they thin out rather than bloat. The rest are **debris** glyphs (⚡ ✦ ◆ ▰) tumbling on
their own axis. A bare emoji spray never read as electrical; the streaks are what carry the
light and the glyphs are what the discharge tore loose. Each burst also places one
`fn-burst__flash` ring at the origin, stepped-flickered over 300ms, so the particles are
visibly *thrown* by something instead of appearing out of nothing.

Coordinates are converted into the layer's own box by `burst.rs::to_layer` before anything
is placed, and particles are `position: absolute` inside it — **not** `fixed` at raw
`getBoundingClientRect` values, which is what put the effect in the wrong place on iPhone.
iOS Safari reports client rects against the *visual* viewport while laying `fixed` boxes out
against the *layout* viewport, and with the keyboard up — exactly when you have just typed a
message — those differ by hundreds of pixels. Measuring the layer the same way the target
was measured makes the units cancel whichever viewport is being reported, and also
immunises the effect against a transformed ancestor re-anchoring `fixed` children. The layer
is therefore mounted even when idle, so there is always something to measure.

Tone is semantic and the two must never be confused at a glance: `fn-burst--live` is cyan
(the system running — a message left the machine), `fn-burst--term` is hot orange-red (the
system destroying something). Fired from the send button (**pop**, 12 sparks, live) and at
a deleted message's bubble alongside the §7 disintegration (**poof**, 12 sparks, term).
Poof is deliberately wider (±75px, was ±30) and shorter than pop — `burst.rs` pins that
ordering as a test, since if a termination ever outlasts a transmission the two effects
have swapped character. The layer caps at 8 concurrent bursts and unmounts entirely under
reduced motion.

**Processing readout** (`burst.rs::proc_hud` + `.fn-proc`). Three seconds of the machine
thinking before it acts, centred on the layer rather than pinned to whatever fired it: it is
the system's own display, a corner would clip it, and at this size it would cover the thing
it is talking about. Three counter-rotating reticle rings at different rates (a reticle
*tracks* something; one spinning circle reads as a loading spinner), a conic sweep radius
scanning the dish, a stepped core, a monospace phase readout on `steps(1)` so labels *switch*
rather than crossfade (and `animation-fill-mode: forwards`, **not** `both` — backwards fill
applies the first keyframe throughout the delay, which rendered all three labels stacked on
top of each other), and a linear progress bar — easing it would lie about the work. The
assembly implodes over its last 8% so the particles emerge from the collapse. Phases are
`LINK ESTABLISHED → ENCRYPTING PAYLOAD → TRANSMITTING` (live) and `TARGET ACQUIRED → PURGING
RECORD → TERMINATED` (term); untranslated on purpose, being machine readout rather than
prose.

**Sequencing, and it is load-bearing.** Both paths run *process → discharge → outcome*, in
that order, because the outcome arriving first reads as an unrelated animation playing over
a done deal:

- **Send** holds `on_send` for `PROC_MS`, fires the burst, then emits after `SPARK_LEAD_MS`
  (260ms — tuned to the origin flash, not the full 1.3s particle flight) so the bubble blooms
  *out of* the sparks while the streaks are still in the air. The send is genuinely delayed;
  that is the trade the sequence asks for.
- **Delete** waits the full `PROC_MS` before anything is destroyed — a failure during the
  readout still has a row to restore — then dispatches `Dissolve` and the burst on the same
  tick so the sparks read as the cause of the collapse, then waits `DISSOLVE_MS` before the
  request.

Positions are measured *after* the wait in both cases: the list may have scrolled or the
composer reflowed, and a stale rect puts the blast where the message no longer is.

**Modal settle** (`fn-modal-in`). The confirm dialog's entrance gained a second
crossing — overshoot 2%, counter-swing 0.5%, settle — at 300ms, per-segment
`--fn-spring-bouncy`. One crossing at 200ms read as a bump; the settle is what makes the
danger confirm feel deliberate rather than sprung. Exit is unchanged: something arriving
should settle, something leaving should just go (§4).
