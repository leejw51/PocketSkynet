#!/usr/bin/env python3
"""Generate PocketSkynet's graphical assets with Grok (xAI Imagine).

Every illustration is produced twice — once for the light theme and once for
the dark theme — because a flat PNG cannot adapt its own background. The two
variants share a subject prompt and differ only in a palette clause, which is
what keeps them recognisably the same artwork.

Requires GROK_API_KEY. Assets are written to web/static/img/ and checked in, so
this only needs re-running when the manifest below changes:

    make assets          # generate anything missing
    make assets-force    # regenerate everything
    tools/genart.py --only login-hero --variant dark
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

API_URL = "https://api.x.ai/v1/images/generations"
MODEL = "grok-imagine-image-quality"

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "web" / "static" / "img"

# Shared style spine. Repeated verbatim in every prompt so the set reads as one
# system rather than six unrelated pictures.
STYLE = (
    "Flat vector illustration, minimal geometric shapes, crisp clean edges, "
    "confident negative space, no gradients on flat shapes except one soft glow, "
    "no text, no letters, no words, no numbers, no watermark, no UI chrome, "
    "centered composition, modern app design language, pixel-crisp at small sizes."
)

# The only difference between the two variants. Backgrounds are deliberately
# *flat and edge-to-edge* so the image can sit on a themed surface without a
# visible seam.
PALETTE = {
    "light": (
        "Palette: electric cyan #0891B2 as the single accent, cool steel "
        "#3B4A5A for structure, emerald #10B981 for one small security detail, "
        "on a flat cool off-white #F2F5F7 background filling the whole frame."
    ),
    "dark": (
        "Palette: luminous cyan #22D3EE as the single accent, brushed "
        "steel-blue #8FA3B8 for structure, emerald #34D399 for one small "
        "security detail, on a flat near-black blue-charcoal #0A0E14 "
        "background filling the whole frame. Accents glow as if lit from "
        "within, HUD-like, tuned for a dark sci-fi user interface."
    ),
}

# --- identity art -----------------------------------------------------------
#
# Avatars are picked by hashing an address or a room id (`web/src/identity.rs`),
# so what matters is that the set is *evenly distinguishable at 40 pixels* —
# the size a room row actually renders. That constraint drives every choice
# below: one face filling the frame, one dominant hue per entry, no scenery.
#
# Two sets, deliberately different in kind so you never mistake a person for a
# place. Operators are half human, half endoskeleton — a face, warm on one side
# and chrome on the other. Rooms are pure machine: a sigil or an endoskull, no
# human anywhere. The split is the point; at a glance the shape alone should
# tell you whether you are looking at who said something or where it was said.
#
# Counts are 10 and 8 — coprime, so an operator and the room they created do
# not drift into the same pairing across a workspace.

OPERATOR_FACES = [
    ("amber", "warm amber human eye, brushed titanium machine half"),
    ("cyan", "ice-blue human eye, polished chrome machine half"),
    ("crimson", "dark human eye, gunmetal machine half with a crimson optic"),
    ("emerald", "green human eye, matte graphite machine half with emerald optic"),
    ("violet", "grey human eye, dark chrome machine half with violet optic"),
    ("gold", "hazel human eye, worn brass-toned machine half"),
    ("steel", "pale human eye, raw steel machine half, rivets visible"),
    ("rose", "brown human eye, white ceramic machine half with rose optic"),
    ("teal", "dark human eye, blued-steel machine half with teal optic"),
    ("bronze", "amber human eye, oxidised bronze machine half"),
]

# The selectable profile gallery. Unlike OPERATOR_FACES these are *chosen*,
# not hashed: `PUT /api/auth/profile` stores `preset:<name>` and every client
# renders the named file. Ten human characteristics, each rendered as a man
# and as a woman — twenty portraits. The machine half is the same terminator
# chrome on all twenty; the human half carries the characteristic, which is
# what the picker is actually offering: "which kind of person is your human
# side". Slugs are part of the API contract (`web/src/identity.rs::PROFILE_ART`).
PROFILE_CHARACTERS = [
    ("coder", "a focused software engineer wearing sleek rectangular glasses "
              "with faint glowing lines of code reflected in the lens"),
    ("soldier", "a disciplined soldier with subtle camouflage face paint and "
                "a faded scar through the eyebrow"),
    ("medic", "a field medic wearing a teal surgical cap, with a calm "
              "reassuring gaze"),
    ("pilot", "a fearless pilot with aviator goggles pushed up onto the "
              "forehead"),
    ("artist", "a painter with small flecks of bright paint on the cheek and "
               "a soft beret"),
    ("scientist", "a research scientist wearing clear lab goggles, one "
                  "eyebrow raised in curiosity"),
    ("chef", "a chef wearing a white toque, a light dusting of flour on the "
             "cheek"),
    ("athlete", "an athlete wearing a sports sweatband, jaw set with "
                "determination"),
    ("musician", "a musician with one studio headphone cup over the human "
                 "ear"),
    ("detective", "a detective under a dark fedora brim shading a sharp "
                  "observant eye"),
]


def _profile_manifest() -> list[dict[str, object]]:
    """The chooseable terminator profile portraits, man and woman of each.

    `max_edge` is 512, larger than the hash-picked avatars' 256: these are
    picked from a grid the user actually looks at and sit on the profile
    card, so they earn the extra detail — and they load only when someone
    opens the picker or wears one.
    """
    out: list[dict[str, object]] = []
    for slug, look in PROFILE_CHARACTERS:
        for sex, subject in (("m", "an adult man"), ("f", "an adult woman")):
            out.append(
                {
                    "name": f"tp-{slug}-{sex}",
                    "themeless": True,
                    "cinematic": True,
                    "max_edge": 512,
                    # No "split exactly down the middle": that phrasing yields
                    # a hard vertical seam that reads as two photos pasted
                    # together. The Terminator look is *damage* — skin torn
                    # away over the machine — so the edge must be organic.
                    "prompt": (
                        f"Close-up portrait of {subject}, face fills the frame. "
                        "One continuous face: most of it living human skin, but "
                        "across one side the skin is torn away in a ragged "
                        "organic edge, revealing chrome endoskeleton machinery "
                        "beneath with a calm glowing cyan optic. The torn "
                        "boundary is irregular and natural — never a straight "
                        "vertical line — with the metal seamlessly integrated "
                        f"under the skin. The human side is {look}. Calm, not "
                        "menacing. Head-on framing. " + IDENTITY_STYLE
                    ),
                }
            )
    return out


ROOM_SIGILS = [
    ("skull", "a chrome endoskeleton skull, front view, both optics lit"),
    ("visor", "a machine head with a single horizontal scanning visor band"),
    ("core", "a spherical machine core caged in metal ribs, lit from within"),
    ("sentinel", "a narrow angular sentinel head, two vertical slit optics"),
    ("hunter", "a predatory machine head, low brow, single wide optic"),
    ("relay", "a machine node with concentric metal rings and a lit centre"),
    ("warden", "a heavy armoured machine faceplate, riveted, one square optic"),
    ("cipher", "a faceted polyhedral machine head, many small lit facets"),
]

# Shared spine for both sets. Repeated verbatim so eighteen separate
# generations still read as one commissioned family rather than eighteen
# stock images.
IDENTITY_STYLE = (
    "Ultra realistic cinematic render, Terminator-film industrial machine "
    "design, dark background, dramatic rim lighting, tight square crop with "
    "the subject filling the frame, high micro-detail on the metal, "
    "photorealistic 8k, no text, no letters, no numbers, no watermark, "
    "no UI chrome."
)


def _identity_manifest() -> list[dict[str, object]]:
    """The avatar sets, expanded from the two tables above.

    `max_edge` is 256: these render at 40px and never larger than 96, so a
    1.3 MB hero-sized PNG per avatar would be absurd — eighteen of them are
    fetched on the first room list that has that many distinct participants.
    """
    out: list[dict[str, object]] = []
    for slug, look in OPERATOR_FACES:
        out.append(
            {
                "name": f"op-{slug}",
                "themeless": True,
                "cinematic": True,
                "max_edge": 256,
                "prompt": (
                    "Close-up portrait, face fills the frame, split exactly down "
                    "the middle: one half living human skin, the other half "
                    f"exposed chrome endoskeleton machinery. {look}. Calm, not "
                    "menacing. Symmetrical head-on framing. " + IDENTITY_STYLE
                ),
            }
        )
    for slug, look in ROOM_SIGILS:
        out.append(
            {
                "name": f"room-{slug}",
                "themeless": True,
                "cinematic": True,
                "max_edge": 256,
                "prompt": (
                    f"An emblem-like machine object centred in frame: {look}. "
                    "Entirely machine — no human features, no skin, no face. "
                    "Reads as an insignia at very small sizes. " + IDENTITY_STYLE
                ),
            }
        )
    return out


MANIFEST: list[dict[str, str]] = [
    {
        # Single variant on purpose. A brand mark that changes shape or plate
        # colour between themes stops being a mark — and the charcoal plate
        # already sits correctly on both a light and a dark surface. It is also
        # the source for the favicons, which have no theme at all.
        "name": "logo",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Sleek minimal app icon: a single chrome cyborg skull emblem centered "
            "on a pure black rounded square, one eye glowing bright cyan blue, "
            "symmetrical front view, glossy liquid metal, thin neon cyan rim "
            "light, premium dark app icon, instantly readable at 32 pixels, "
            "nothing else in frame."
        ),
    },
    # ---- the cinematic Skynet set -----------------------------------------
    # Photoreal, themeless (they live on dark surfaces in both themes), and
    # deliberately outside the flat-vector spine: these are the app's four
    # "movie moments" — login, assistant, wallet, and the ambient backdrop.
    {
        "name": "skynet-hero",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Ultra realistic cinematic portrait of a benevolent guardian cyborg, "
            "half human face half chrome endoskeleton machine, split down the "
            "middle, glowing cyan blue eye on the machine side, warm human eye on "
            "the other, subtle friendly expression, protective not menacing, dark "
            "background with faint blue circuit patterns, movie poster quality, "
            "dramatic rim lighting, 8k photorealistic detail."
        ),
    },
    {
        "name": "skynet-avatar",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Ultra realistic close-up of a friendly AI android head, sleek chrome "
            "and white ceramic plating, glowing cyan optic eyes, soft studio "
            "lighting, benevolent assistant, dark navy background with faint "
            "holographic UI reflections, photorealistic, square composition, "
            "no text."
        ),
    },
    {
        "name": "skynet-vault",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Ultra realistic macro shot of a futuristic crystalline vault core, "
            "glowing cyan hexagonal energy crystal inside a brushed titanium "
            "frame, holographic hexagon tokens floating around it, dark black "
            "background, sci-fi digital vault, cinematic rim lighting, "
            "photorealistic, no logos, no text, no currency symbols."
        ),
    },
    {
        "name": "skynet-grid",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Dark futuristic HUD background texture, deep black with faint "
            "glowing cyan circuit traces and a hexagonal grid receding into "
            "depth, sci-fi command center aesthetic, very dark and subtle so "
            "interface text stays readable on top, seamless wallpaper style, "
            "no characters, no text."
        ),
    },
    # The sign-in boot sequence (app.css §18.2). Two frames of one moment: the
    # arrival field collapses, and what was inside it opens its eyes.
    {
        "name": "boot-sphere",
        "themeless": True,
        "cinematic": True,
        # On the sign-in critical path: small enough to arrive before it plays.
        "max_edge": 720,
        "prompt": (
            "Ultra realistic sphere of crackling electric plasma, brilliant "
            "cyan-white lightning arcs curling around a dark hollow core, "
            "violent electrical discharge throwing sparks outward, pitch black "
            "background, cinematic sci-fi energy field at the instant of "
            "detonation, volumetric light, photorealistic, perfectly centered, "
            "square composition, no text, no characters."
        ),
    },
    {
        "name": "boot-endoskull",
        "themeless": True,
        "cinematic": True,
        # On the sign-in critical path: small enough to arrive before it plays.
        "max_edge": 720,
        "prompt": (
            "Ultra realistic front-facing chrome robotic endoskeleton skull, "
            "polished liquid-metal steel plating, two brilliant glowing cyan "
            "optic eyes blazing with inner light, a noble guardian machine "
            "rather than a monster, emerging from darkness through drifting "
            "smoke, dramatic rim lighting, pitch black background, movie poster "
            "quality, 8k photorealistic detail, symmetrical, centered, no text."
        ),
    },
    {
        "name": "login-hero",
        "prompt": (
            "Wide landscape hero: an airy constellation of hexagonal nodes linked "
            "by thin straight lines, flowing from lower-left to upper-right. Three "
            "nodes glow in the accent colour, the rest are quiet structural "
            "outlines. One node at the focal point is ringed by a dotted orbit with "
            "a tiny key shape resting on it. Lots of empty space at the left third "
            "so text can sit over it."
        ),
    },
    {
        "name": "empty-rooms",
        "prompt": (
            "Empty-state illustration: three overlapping rounded speech bubbles "
            "arranged like a small constellation, the frontmost one outlined in a "
            "dashed accent stroke and empty inside. They float above a single soft "
            "horizontal shadow line. Calm and inviting, not sad."
        ),
    },
    {
        "name": "empty-messages",
        "prompt": (
            "Empty-state illustration: one large open speech bubble with a paper "
            "plane leaving it along a dotted arc, and a small closed padlock resting "
            "at the bubble's lower corner. Suggests a private conversation waiting "
            "to start."
        ),
    },
    {
        "name": "encrypted-badge",
        "prompt": (
            "Icon: a closed padlock whose shackle is formed from two interlocking "
            "chain links. The body is the emerald security colour, the shackle is "
            "the accent colour. Extremely simple — must stay legible at 24 pixels."
        ),
    },
    {
        "name": "error-offline",
        "prompt": (
            "Illustration of a severed connection: two hexagonal nodes with a "
            "dashed line broken in the middle, one small accent-coloured spark at "
            "the break. Mostly muted structural colour with a single warm accent."
        ),
    },
    {
        "name": "empty-invitations",
        "prompt": (
            "Empty-state illustration: a sealed envelope shown at a slight angle "
            "with a tiny hexagonal wax seal in the accent colour, resting on a soft "
            "shadow line. Quiet and neutral — nothing is waiting."
        ),
    },
    {
        "name": "empty-search",
        "prompt": (
            "Empty-state illustration: a magnifying glass over a small cluster of "
            "hexagonal outlines, none of them filled in. The lens is a clean circle "
            "with one accent-coloured rim highlight."
        ),
    },
    {
        # The Bank dialog's portfolio hero: a machine teller. The reference
        # client has a cheerful orange mascot; PocketSkynet's banker is the
        # same job in this product's language — a vault door with a face.
        "name": "bank-hero",
        "prompt": (
            "A friendly robotic bank teller motif: a hexagonal vault door "
            "with a subtle face formed by two lens-eyes and a coin slot, "
            "flanked by two small stacks of hexagonal coins. One thin accent "
            "ring around the vault wheel glows."
        ),
    },
    {
        # The AI Banker chat's avatar/empty state — the teller leaning in to
        # listen, distinct from the vault-door hero.
        "name": "bank-banker",
        "prompt": (
            "A compact robot banker bust: rounded head with two lens-eyes "
            "and a small antenna, wearing a flat vector necktie, holding up "
            "one hexagonal coin. A subtle speech-bubble outline floats "
            "beside its head."
        ),
    },
    {
        # The Bank *page* hero backdrop (2026-07: the Bank outgrew its dialog
        # and became /bank). Cinematic like the login hero, because the
        # portfolio card is the screen's one movie moment: Skynet's vault.
        # Wide, dark at the edges, so the balance figure can sit on top of it.
        "name": "bank-vault-hall",
        "themeless": True,
        "cinematic": True,
        "prompt": (
            "Ultra realistic cinematic wide shot of a colossal chrome bank "
            "vault door at the end of a dark industrial machine hall, "
            "Terminator-film design language, massive circular door with "
            "concentric rings and a glowing cyan core, thin cyan circuit "
            "traces along the walls, volumetric haze, dramatic rim lighting, "
            "very dark edges fading to black, photorealistic 8k, no text, "
            "no letters, no watermark, no people."
        ),
    },
    {
        # The Bank page's header badge (2026-07-31, replacing the flat-vector
        # teller): the vault as Skynet wears it. Reads at 56px, so one strong
        # silhouette — a chrome skull fused into a vault-door ring — rather
        # than a scene.
        "name": "bank-emblem",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic emblem, tight square crop: a chrome "
            "T-800 style endoskeleton skull fused into the centre of a "
            "circular bank vault door mechanism, concentric riveted metal "
            "rings and gear teeth radiating around the skull like a vault "
            "wheel, two calm glowing cyan lens eyes, Terminator-film "
            "industrial machine design, dark background, dramatic rim "
            "lighting, thin cyan circuit traces on the metal, symmetrical "
            "head-on framing, reads clearly as an icon at small sizes, "
            "photorealistic 8k, no text, no letters, no watermark."
        ),
    },
    {
        # The AI Banker's face for the executing agent era: not the friendly
        # flat-vector teller (that was the advice-only banker) but a chrome
        # endoskeleton in a banker's collar — it moves money now, and the
        # imagery should say so.
        "name": "banker-core",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic bust portrait of a chrome endoskeleton "
            "robot banker, Terminator-film industrial design, polished liquid "
            "metal skull with two calm glowing cyan lens eyes, wearing a "
            "sharp dark suit collar and tie, one metal hand holding up a "
            "single glowing hexagonal coin, dark background, dramatic rim "
            "lighting, tight square crop, photorealistic 8k, calm and "
            "trustworthy not menacing, no text, no watermark."
        ),
    },
    {
        # The Wallet dialog's presiding avatar (2026-07: the wallet gets a
        # face). Not the banker — the banker advises and moves money across
        # chains; the warden *guards this vault*. Head-on, armoured, calm:
        # security you can look in the eye. Renders at 64px in the dialog
        # header, so one strong silhouette, no scene.
        "name": "wallet-warden",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic bust portrait of a chrome endoskeleton "
            "sentinel standing guard, Terminator-film industrial machine "
            "design, polished liquid-metal skull with heavy armoured shoulder "
            "plating, two calm glowing cyan lens eyes, thin cyan circuit "
            "traces on the metal, a small glowing hexagonal emblem set into "
            "the chest plate, dark background, dramatic rim lighting, "
            "symmetrical head-on framing, tight square crop, protective and "
            "composed rather than menacing, photorealistic 8k, no text, "
            "no letters, no watermark."
        ),
    },
    {
        # The Shout dialog's herald (2026-07: paid broadcast). A shout costs
        # CRO and lands on every connected screen at once, so its face is a
        # machine crier — an endoskeleton head issuing a visible broadcast.
        # Renders at 64px in the dialog header: one silhouette, no scene.
        "name": "shout-herald",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic bust portrait of a chrome endoskeleton "
            "herald broadcasting, Terminator-film industrial machine design, "
            "polished liquid-metal skull with two calm glowing cyan lens "
            "eyes, jaw open mid-announcement, concentric glowing cyan sonic "
            "shockwave rings radiating outward from its mouth, thin cyan "
            "circuit traces on the metal, dark background, dramatic rim "
            "lighting, symmetrical head-on framing, tight square crop, "
            "commanding but not menacing, photorealistic 8k, no text, "
            "no letters, no watermark."
        ),
    },
    {
        # The Publish page's header badge (2026-07: paid web hosting). The
        # server holds your page up for the world — a machine atlas raising a
        # glowing globe of pages. Reads at 56px: one strong silhouette.
        "name": "publish-emblem",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic emblem, tight square crop: two chrome "
            "endoskeleton robot hands holding up a glowing translucent cyan "
            "wireframe globe made of thin latitude and longitude circuit "
            "lines, small glowing rectangular page panels orbiting the globe, "
            "Terminator-film industrial machine design, dark background, "
            "dramatic rim lighting, symmetrical composition, reads clearly "
            "as an icon at small sizes, photorealistic 8k, no text, "
            "no letters, no watermark."
        ),
    },
    {
        # The Publish page's empty state: nothing hosted yet. NOT the
        # knowledge crystal — a published site is a *place* others visit, so
        # the motif is a beacon tower waiting to be lit.
        "name": "empty-publish",
        "prompt": (
            "Empty-state illustration: a slender geometric beacon tower on a "
            "small hexagonal island platform, its lamp housing empty and "
            "outlined in a dashed accent stroke, one thin dotted signal arc "
            "sketched from the lamp into empty space. Three faint rectangular "
            "page outlines float near the base, waiting. Calm and inviting, "
            "not sad."
        ),
    },
    {
        # The Knowledge page (docs/SEARCH.md): search everything, teach the
        # server. A memory-crystal motif — the server keeping what it was told.
        "name": "empty-knowledge",
        "prompt": (
            "Empty-state illustration: a faceted crystal made of stacked "
            "geometric plates, hovering above a flat pedestal, with three thin "
            "orbit lines carrying small square data motes toward it. One facet "
            "glows in the accent colour, as if a memory has just been stored."
        ),
    },
    {
        # The Files drawer's empty state. Deliberately NOT the knowledge
        # crystal: knowledge is one thing the server remembers, an attachment
        # shelf is *many* discrete objects you can still point at individually.
        # A rack of plates carries that; a crystal would say "merged".
        #
        # The tag shapes are the feature's other half — a file here is nothing
        # without the hashtags hung off it — so they are in the picture rather
        # than implied.
        "name": "empty-files",
        "prompt": (
            "Empty-state illustration: a vertical rack of stacked flat "
            "rectangular plates like archived records seen slightly from the "
            "side, one plate pulled forward out of the stack and glowing in "
            "the accent colour. Three small angular pentagon tag shapes are "
            "tethered to that plate by thin straight lines, hanging free."
        ),
    },
    {
        # The main pane's "pick a room" state. Deliberately NOT the empty-rooms
        # bubbles: on a first sign-in the two panes sit side by side, and the
        # same picture twice reads as a rendering bug rather than two states.
        # This one is about *choosing* — a column of room tiles with one lit.
        "name": "pick-room",
        "prompt": (
            "Empty-state illustration: a neat vertical stack of three rounded "
            "rectangular chat-room tiles seen at a slight angle, the middle tile "
            "glowing in the accent colour with a subtle halo and a small padlock "
            "mark, the other two quiet structural outlines. A thin dotted "
            "selection arc curves toward the lit tile from the right. Suggests "
            "picking one conversation out of a list."
        ),
    },
    *_identity_manifest(),
    *_profile_manifest(),
]

VARIANTS = ("light", "dark")


def dest_for(name: str, variant: str) -> Path:
    """Light keeps the bare name so existing references keep working."""
    suffix = "" if variant == "light" else "-dark"
    return OUT_DIR / f"{name}{suffix}.png"


def shrink(dest: Path, max_edge: int) -> None:
    """Downscale `dest` in place so its longest edge is `max_edge`.

    Photoreal PNGs come back around 1.3 MB at full size, which is fine for a
    backdrop that loads once and wrong for anything on a critical path. Only
    ever shrinks: `sips -Z` would happily upscale a smaller source into a
    blurrier, larger file.
    """
    probe = subprocess.run(
        ["sips", "-g", "pixelWidth", "-g", "pixelHeight", str(dest)],
        capture_output=True,
    )
    sizes = [int(w) for w in probe.stdout.split() if w.isdigit()]
    if not sizes or max(sizes) <= max_edge:
        return
    subprocess.run(["sips", "-Z", str(max_edge), str(dest)], capture_output=True)


def to_png(data: bytes, dest: Path, max_edge: int | None = None) -> None:
    """Write `data` as a genuine PNG, optionally downscaled.

    The API returns JPEG bytes regardless of the `.png` name we save under, and
    a file whose contents disagree with its extension is a real problem, not a
    cosmetic one: the server picks `Content-Type` from the extension and sends
    `X-Content-Type-Options: nosniff`, and toolchains that actually parse the
    file — `tauri icon`, for one — reject it outright.
    """
    dest.write_bytes(data)
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        tmp = dest.with_suffix(".converting.png")
        result = subprocess.run(
            ["sips", "-s", "format", "png", str(dest), "--out", str(tmp)],
            capture_output=True,
        )
        if result.returncode == 0 and tmp.exists():
            tmp.replace(dest)
        else:
            tmp.unlink(missing_ok=True)
            raise ValueError(
                f"{dest.name} is not PNG and could not be converted: "
                f"{result.stderr.decode(errors='replace').strip()}"
            )

    if max_edge:
        shrink(dest, max_edge)


def generate(prompt: str, api_key: str) -> bytes:
    body = json.dumps(
        {"model": MODEL, "prompt": prompt, "n": 1, "response_format": "b64_json"}
    ).encode()
    req = urllib.request.Request(
        API_URL,
        data=body,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=240) as resp:
        payload = json.load(resp)
    return base64.b64decode(payload["data"][0]["b64_json"])


def build_jobs(args) -> list[tuple[str, str, Path, str]]:
    jobs = []
    for asset in MANIFEST:
        if args.only and asset["name"] != args.only:
            continue
        # A themeless asset carries its palette in its own prompt and is written
        # once, under the bare name.
        variants = ("light",) if asset.get("themeless") else VARIANTS
        for variant in variants:
            if args.variant and variant != args.variant and not asset.get("themeless"):
                continue
            dest = dest_for(asset["name"], variant)
            if dest.exists() and not args.force:
                print(f"  skip  {dest.relative_to(ROOT)} (exists)")
                continue
            prompt = asset["prompt"]
            if not asset.get("themeless"):
                prompt = f"{prompt} {PALETTE[variant]}"
            # Cinematic assets carry their whole art direction in their own
            # prompt; the flat-vector spine would fight it.
            if not asset.get("cinematic"):
                prompt = f"{prompt} {STYLE}"
            jobs.append((asset["name"], variant, dest, prompt, asset.get("max_edge")))
    return jobs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--force", action="store_true", help="regenerate existing assets")
    parser.add_argument("--only", help="generate a single asset by name")
    parser.add_argument("--variant", choices=VARIANTS, help="only one theme variant")
    parser.add_argument(
        "--jobs", type=int, default=4, help="parallel requests (default 4)"
    )
    args = parser.parse_args()

    api_key = os.environ.get("GROK_API_KEY")
    if not api_key:
        print("GROK_API_KEY is not set; skipping asset generation.", file=sys.stderr)
        return 1

    OUT_DIR.mkdir(parents=True, exist_ok=True)
    jobs = build_jobs(args)
    if not jobs:
        print("  nothing to generate")
        return 0

    failures = 0

    def run(job):
        name, variant, dest, prompt, max_edge = job
        try:
            to_png(generate(prompt, api_key), dest, max_edge)
            return f"  ok    {dest.relative_to(ROOT)} ({dest.stat().st_size // 1024} KB)"
        except (urllib.error.URLError, KeyError, ValueError, TimeoutError) as exc:
            return f"  FAIL  {name} [{variant}]: {exc}"

    print(f"  generating {len(jobs)} image(s) with {MODEL}…", flush=True)
    with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
        for line in pool.map(run, jobs):
            print(line, flush=True)
            if line.lstrip().startswith("FAIL"):
                failures += 1

    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
