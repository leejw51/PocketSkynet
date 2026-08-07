#!/usr/bin/env python3
"""Generate PocketSkynet's graphical assets with Grok (xAI Imagine).

Two dimensions, and they are not the same kind of thing.

**Theme** is light or dark. Every illustration is produced twice, because a
flat PNG cannot adapt its own background. The two variants share a subject
prompt and differ only in a palette clause, which is what keeps them
recognisably the same artwork.

**Skin** is the art direction — the product's whole visual identity. `skynet`
is the machine-cinema set this file has always generated, and it writes to
`web/static/img/`. `cuteskynet` is the friendly-mecha set, and it writes to
`web/static/img/cute/`. `humanskynet` is the one that looks like a person, and
it writes to `web/static/img/human/`. `web/src/asset.rs` looks for both.

A skin does *not* have to redraw everything. Only assets carrying that skin's
prompt key are generated for it, and `asset.rs::art_list` is the same list on
the Rust side — anything missing falls back to the base artwork rather than
rendering nothing. Keep the two in step; `cargo test` checks that every stem
those arrays promise is actually a file on disk.

Requires GROK_API_KEY. Assets are checked in, so this only needs re-running
when the manifest below changes:

    make assets            # generate anything missing, every skin
    make assets-force      # regenerate everything
    make assets-cute       # only the cute skin
    make assets-human      # only the human skin
    tools/genart.py --only empty-rooms --variant dark --skin humanskynet
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

API_URL = "https://api.x.ai/v1/images/generations"
MODEL = "grok-imagine-image-quality"

ROOT = Path(__file__).resolve().parent.parent
OUT_DIR = ROOT / "web" / "static" / "img"

# --- skins ------------------------------------------------------------------
#
# The base skin's prompts are the `prompt` key on each manifest entry; every
# other skin supplies its own under a key of its own. Deliberately a separate
# prompt rather than the same subject with a style clause bolted on: "a chrome
# endoskeleton skull, but cute" produces a chrome endoskeleton skull. The
# subject has to change too, and the only way to say what it changes *to* is to
# write it out.
#
# `dir` is relative to OUT_DIR and matches `session.rs::Skin::art_dir`.
SKIN_SKYNET = "skynet"
SKIN_CUTE = "cuteskynet"
SKIN_HUMAN = "humanskynet"
SKINS = {
    SKIN_SKYNET: {"dir": None, "key": "prompt"},
    SKIN_CUTE: {"dir": "cute", "key": "cute"},
    SKIN_HUMAN: {"dir": "human", "key": "human"},
}

# The cute skin's shared spine, read off `cuteskynet.jpg`: one small blue-and-
# white mecha, drawn in the flat cel style of modern anime rather than the
# photoreal render the base set uses. Repeated verbatim in every cute prompt for
# the same reason STYLE is — forty-two separate generations have to look
# commissioned, not collected.
CUTE_STYLE = (
    "Cute chibi mecha in modern anime illustration style, thick confident dark "
    "outlines, flat cel shading with one soft highlight, glossy moulded plastic "
    "shell, rounded friendly proportions with no sharp edges, cobalt blue "
    "#1F6FD0 armour panels over white ceramic plating, a warm gold #F2C230 "
    "visor, one small red #E03A2F signal light, a slim antenna with a red bead "
    "on top, calm and endearing rather than heroic, soft out-of-focus bokeh "
    "circles in the background, no text, no letters, no numbers, no watermark, "
    "no UI chrome."
)

# The cute skin's two backgrounds. Flat and edge-to-edge, like the base set's,
# so the PNG can sit on a themed surface without a visible seam.
CUTE_PALETTE = {
    "light": (
        "Palette: bright daylight, cobalt blue and gold reading clearly against "
        "a flat pale cool grey-white #EDF2F5 background filling the whole "
        "frame, soft diffused studio light, one gentle contact shadow."
    ),
    "dark": (
        "Palette: cool night lighting, the cobalt armour deepened and the gold "
        "visor glowing warmly from within, on a flat deep navy #16233A "
        "background filling the whole frame, soft blue rim light along the top "
        "edges, cosy rather than menacing."
    ),
}

# --- the human skin's spine --------------------------------------------------
#
# Read off `humanskynet.jpg`: long midnight hair, a matte black suit, a wall of
# cool data-light behind her.
#
# The premise is one sentence long and it is the whole skin: *this is a Skynet
# you cannot tell from a person*. The base set shows the machine — torn skin,
# chrome under it, a lit optic where an eye should be — and the cute set shows a
# toy. This one shows neither, and it has to say so out loud in every prompt,
# because "Skynet" in a prompt is an enormous pull toward exactly the chrome this
# skin is defined by not having. HUMAN_ONLY_HUMAN and the last clause of
# HUMAN_LEAD_F/M are that sentence; they are not decoration and not safe to trim.
#
# # The register, and the two it is not
#
# Modern Japanese anime *key visual* — the high-polish digital illustration a
# series is sold with. Both of the other candidates were tried and rejected
# against the reference, and the rejections are worth keeping, because each one
# is a plausible reading of the same words:
#
#   * "Semi-realistic modern anime" produces competent illustration with the
#     anime sanded off — real proportions, small eyes, matte skin. It is the
#     safe answer and it looks like stock art.
#   * Late-90s cel animation — hand-inked contours, hard shadow steps, painted
#     backgrounds — is a *better* thematic fit than what shipped, and it was
#     genuinely tempting: a cyborg you cannot pick out of a crowd is that genre's
#     entire subject. But it is not what the reference looks like, and a skin is
#     judged on the artwork, not on the argument behind it.
#
# What the reference actually is: luminous. Airbrushed shading that blends
# instead of stepping, skin lit from within, hair as glossy ribbons carrying
# hard cyan-white speculars, and eyes doing most of the work — large, wet,
# multi-highlighted, drawn with more care than the rest of the face combined.
# That last part is the whole style; a prompt that gets everything else right and
# the eyes wrong lands back in the first bullet.
HUMAN_STYLE = (
    "Modern Japanese anime key visual, high-polish digital illustration: clean "
    "crisp linework, soft airbrushed shading that blends smoothly rather than "
    "stepping, luminous fair skin lit from within, strong cool rim lighting "
    "along the edges, glossy blue-black hair drawn as flowing ribbons with hard "
    "cyan-white specular bands, a cool palette of deep indigo and midnight navy "
    "under cyan screen-light, polished and delicate rather than gritty. "
    "Stylish and fashion-forward: sleek sculpted silhouettes, sharp tailored "
    "lines, everything looking deliberately designed rather than merely worn. "
    "No text, no letters, no numbers, no watermark, no UI chrome."
)

# The face rule, applied wherever a person appears, and kept apart from the
# style spine because it is the single clause the whole register hangs on. Left
# to itself an image model renders "anime" as a real face with flat colour; the
# eyes are what make it anime, and they have to be specified in more detail than
# anything else in the prompt or they arrive small and dry.
HUMAN_FACES = (
    "Large luminous anime eyes doing most of the work: detailed irises with "
    "visible colour gradients, a bright ring of light around the pupil, several "
    "crisp white highlights, long dark lashes and a fine eyeliner sweep. "
    "Smooth features, small nose, soft mouth, the faintest blush. Calm and "
    "composed, looking straight at the viewer."
)

# The `m`/`f` split for the chooseable gallery, and it does two jobs.
#
# The first is the register: a key visual draws people as *attractive*, and
# leaving that unsaid gets a competent neutral face that reads as a different
# style sitting in the same grid.
#
# The second is a correctness one that the earlier, more naturalistic pass got
# wrong badly enough to notice: `tp-coder-m` and `tp-coder-f` came back as very
# nearly the same person. Those slugs are a wire contract — `preset:tp-coder-f`
# is what is stored and every client resolves it — so a grid whose pairs cannot
# be told apart is a picker that cannot be used. Naming the distinction is what
# fixes it; the previous prompts left it entirely to "man"/"woman" and the
# model, quite reasonably, drew two adults.
HUMAN_PROFILE_SUBJECT = {
    "m": "a handsome young adult man, strong clean jawline, well-groomed",
    "f": "a beautiful young adult woman, delicate features, elegant",
}

# The two leads.
#
# There are two rather than one because the skin is called *Human*, and one face
# would quietly make the skin about her instead of about the premise. Two people,
# wearing the same uniform, doing the product's jobs between them, is the idea
# stated properly: any of these could be it, and you cannot tell which.
#
# Each is spelled out in full at every appearance rather than referred to,
# because forty separate generations have no memory of each other — "the same
# woman" is a thing only literal repetition buys. The shared clauses (the suit,
# the hair treatment, the closing denial) are identical between them on purpose:
# it is what makes them read as two of a kind rather than as two stock portraits.
#
# The last clause is the load-bearing one and is not safe to trim. "Skynet" is
# not in these prompts at all and the chrome still tries to arrive.
_HUMAN_SUIT = (
    "wearing a matte black high-collared tactical bodysuit with fine panel "
    "seams and a small glowing cyan insignia on the chest"
)
_HUMAN_DENIAL = (
    "completely and ordinarily human: unbroken skin, natural human eyes, no "
    "chrome, no seams or panel lines on the face or body, no glowing implants, "
    "no exposed machinery, nothing anywhere that gives away what they are"
)
HUMAN_LEAD_F = (
    "a beautiful young adult woman with long wavy midnight blue-black hair "
    "falling past her shoulders in glossy ribbons lit with cyan-white "
    f"highlights, large luminous violet-blue eyes and fair skin, {_HUMAN_SUIT}. "
    f"She looks {_HUMAN_DENIAL}"
)
HUMAN_LEAD_M = (
    "a handsome young adult man with short swept-back midnight blue-black hair "
    "lit with cyan-white highlights, a strong clean jawline, calm luminous "
    f"violet-blue eyes and fair skin, {_HUMAN_SUIT}. He looks {_HUMAN_DENIAL}"
)

# Which of the two wears which of the product's faces.
#
# Assigned per *feature* rather than per asset, so a screen never changes who it
# is halfway through: the Bank is him across its emblem, its banker and its chat
# avatar; the Wallet and Shout are hers. The mark and the cold open are hers
# because they are the reference image itself, and the assistant is hers because
# it is the same character the mark is.
#
# `skynet-hero` is the exception and the reason the split is legible at all: the
# sign-in backdrop is the one wide frame in the set that can hold two people, and
# it is the first screen anyone sees. Putting both of them there is the skin
# saying out loud that it has more than one face.

# The human skin's two backgrounds. Flat and edge-to-edge, like the other sets',
# so the PNG can sit on a themed surface without a visible seam.
HUMAN_PALETTE = {
    "light": (
        "Palette: bright cool daylight, indigo and cyan reading clearly against "
        "a flat pale cool grey-lilac #EEF0F7 background filling the whole "
        "frame, soft diffused light, one gentle contact shadow."
    ),
    "dark": (
        "Palette: cool night lighting, the indigo deepened and the cyan "
        "screen-light glowing, on a flat deep midnight indigo #10142A "
        "background filling the whole frame, soft cyan rim light along the top "
        "edges, calm rather than menacing."
    ),
}

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

# Each skin's pair of backgrounds, keyed the way SKINS is. A separate table only
# because a palette has to be written before it can be named.
PALETTES = {
    SKIN_SKYNET: PALETTE,
    SKIN_CUTE: CUTE_PALETTE,
    SKIN_HUMAN: HUMAN_PALETTE,
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
    (
        "coder",
        "a focused software engineer wearing sleek rectangular glasses "
        "with faint glowing lines of code reflected in the lens",
    ),
    (
        "soldier",
        "a disciplined soldier with subtle camouflage face paint and "
        "a faded scar through the eyebrow",
    ),
    (
        "medic",
        "a field medic wearing a teal surgical cap, with a calm " "reassuring gaze",
    ),
    ("pilot", "a fearless pilot with aviator goggles pushed up onto the " "forehead"),
    (
        "artist",
        "a painter with small flecks of bright paint on the cheek and " "a soft beret",
    ),
    (
        "scientist",
        "a research scientist wearing clear lab goggles, one "
        "eyebrow raised in curiosity",
    ),
    ("chef", "a chef wearing a white toque, a light dusting of flour on the " "cheek"),
    ("athlete", "an athlete wearing a sports sweatband, jaw set with " "determination"),
    ("musician", "a musician with one studio headphone cup over the human " "ear"),
    (
        "detective",
        "a detective under a dark fedora brim shading a sharp " "observant eye",
    ),
]


# The cute skin's read of the same gallery.
#
# The base set carries its characteristic on a *human face* — a coder's
# glasses, a chef's flour. This one has no exposed human skin to hang that on,
# so each characteristic becomes a worn object instead: the visor goes up, and
# what identifies the pilot is what they are carrying. That is a translation of
# the idea rather than a restyling of the picture, which is the whole reason
# these are separate prompts and not the same prompt with a style clause.
#
# Each string is a complete sentence, appended after the shared helmet
# description.
CUTE_PROFILE_CHARACTERS = {
    "coder": "It wears slim rectangular glasses over the visor, with a few "
    "tiny glowing squares reflected in the lenses.",
    "soldier": "It wears a small olive-green shoulder strap and has two soft "
    "camouflage patches painted on the helmet.",
    "medic": "It wears a teal cap over the helmet and a small white cross "
    "badge on the chest plate.",
    "pilot": "It wears round aviator goggles pushed up beside the visor and a "
    "short white scarf around its neck.",
    "artist": "It wears a soft tilted beret and has three small flecks of "
    "bright paint on one cheek plate.",
    "scientist": "It wears clear lab goggles over the visor and a small white "
    "collar, one eyebrow marking raised in curiosity.",
    "chef": "It wears a tall white chef's toque on top of the helmet and has a "
    "light dusting of flour on one cheek plate.",
    "athlete": "It wears a bright sports sweatband across the helmet and has a "
    "small stopwatch clipped to its chest plate.",
    "musician": "It wears one round studio headphone cup over the side vent "
    "and has a tiny note marking on its chest plate.",
    "detective": "It wears a small dark fedora tilted over the helmet and a "
    "short trench collar.",
}

# The `m`/`f` split. The slugs are an API contract (`preset:tp-coder-f` is
# stored on the wire), so both must exist for every character — but a chibi
# mecha has no human features to carry the distinction, and inventing some
# would be worse than the alternative. So it lands where it can be read at
# 40px without caricature: build and helmet shape.
CUTE_PROFILE_SUBJECT = {
    "m": "a sturdier, broader-shouldered robot with a squarer helmet crown",
    "f": "a slighter, narrower-shouldered robot with a rounder helmet crown",
}


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
                    "cute": (
                        f"Close-up bust portrait of {CUTE_PROFILE_SUBJECT[sex]} "
                        "as a small cute robot pilot, head and shoulders "
                        "filling the frame: a glossy cobalt blue helmet over "
                        "white ceramic plating with a wide warm gold visor "
                        "pushed up onto the forehead, so the whole friendly "
                        "cartoon face is visible underneath — two bright eyes "
                        "and a small warm smile. A slim antenna with a red "
                        "bead sits on top. "
                        f"{CUTE_PROFILE_CHARACTERS[slug]} Head-on framing. "
                        + CUTE_IDENTITY_STYLE
                    ),
                    # The one place the human skin needs no translation table.
                    # The base gallery's characteristics are already *human*
                    # ones — a coder's glasses, a chef's flour — carried on the
                    # human half of a torn face; this skin simply keeps the
                    # half that was already there and drops the machine. Which
                    # is the skin's whole argument, arriving for free: the
                    # picker asks "which kind of person is your human side",
                    # and here there is no other side to ask about.
                    # "Face filling the frame" — the phrasing the base and cute
                    # prompts use — is read literally in this register and
                    # comes back cropped through the hairline and the chin,
                    # which in a grid of twenty reads as twenty broken images
                    # rather than as a tight crop. The op-* prompts escape it
                    # because "symmetrical head-on framing" pulls the camera
                    # back; here the whole head is asked for outright.
                    "human": (
                        f"Close-up anime key-visual bust portrait of "
                        f"{HUMAN_PROFILE_SUBJECT[sex]}, head and shoulders "
                        "filling the frame with the whole head visible and a "
                        "little space above the hair — not cropped through the "
                        f"hairline or the chin. {look[0].upper() + look[1:]}. "
                        "Calm, not menacing. Head-on framing. "
                        f"{HUMAN_FACES} {HUMAN_ONLY_HUMAN} " + HUMAN_IDENTITY_STYLE
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


# The cute skin's answer to the same two tables.
#
# The split that matters is preserved exactly: operators are *somebody*, rooms
# are *somewhere*, and at 40px the silhouette alone has to say which. The base
# set draws that as human-face-versus-machine-sigil; here it is a helmeted pilot
# head — one visor, a face behind it — against a badge-shaped object with no
# face at all. Same distinction, different vocabulary.
#
# Only the accent moves between entries, because ten cute robots that differ in
# ten ways is a set nobody can tell apart at a glance, whereas ten that differ
# in one are ten distinct colours.
CUTE_OPERATOR_LOOKS = {
    "amber": "a warm amber visor and honey-gold trim on the helmet",
    "cyan": "a bright cyan visor and pale ice-blue trim on the helmet",
    "crimson": "a deep crimson visor and dark red trim on the helmet",
    "emerald": "a fresh emerald-green visor and mint trim on the helmet",
    "violet": "a soft violet visor and lavender trim on the helmet",
    "gold": "a rich gold visor and brass trim on the helmet",
    "steel": "a cool silver visor and raw steel trim on the helmet",
    "rose": "a rose-pink visor and blush trim on the helmet",
    "teal": "a deep teal visor and sea-green trim on the helmet",
    "bronze": "a warm bronze visor and copper trim on the helmet",
}

CUTE_ROOM_LOOKS = {
    "skull": "a rounded friendly robot head badge with two big round lens eyes",
    "visor": "a smooth helmet badge with one wide horizontal visor band",
    "core": "a plump glowing orb held in a rounded cradle of soft metal ribs",
    "sentinel": "a tall rounded sentry-post badge with two small vertical lights",
    "hunter": "a rounded scout-drone badge with one large forward lens",
    "relay": "a chubby signal node badge with concentric rings and a lit centre",
    "warden": "a thick rounded shield badge with one square lit panel in it",
    "cipher": "a soft-cornered polyhedral gem badge with many small lit facets",
}

CUTE_IDENTITY_STYLE = (
    "Cute chibi mecha in modern anime illustration style, thick confident dark "
    "outlines, flat cel shading, glossy moulded plastic, rounded proportions "
    "with no sharp edges, cobalt blue and white ceramic shell, tight square "
    "crop with the subject filling the frame, flat deep navy background, simple "
    "bold shapes that stay readable at very small sizes, no text, no letters, "
    "no numbers, no watermark, no UI chrome."
)


# The human skin's answer to the same two tables.
#
# The somebody/somewhere split is the one thing every skin has to keep, and this
# is the skin where it is easiest to lose: a set drawn entirely in human anime
# has no machine-versus-human silhouette to lean on. So it falls to the kind of
# object instead — a lit human face against a flat glowing heraldic glyph. At
# 40px a portrait and an insignia are never confused, which is all the split has
# ever needed to be.
#
# Ten operators who are ten different people, not one person under ten lights,
# and drawn the way a key visual draws people: handsome men, beautiful women.
# The hue does the identifying at small sizes, but a face is what the eye reaches
# for first, so hair and bearing move too — a colour-only set reads as one
# operator with a broken avatar cache.
HUMAN_OPERATOR_LOOKS = {
    "amber": "a beautiful woman with short warm-brown hair, lit by amber "
    "lamplight, a honey-gold collar",
    "cyan": "a handsome man with pale silver-blond hair, lit by cool cyan "
    "screen-light, an ice-blue collar",
    "crimson": "a beautiful woman with sharp black hair cut to the jaw, lit by "
    "deep crimson light, a dark red collar",
    "emerald": "a handsome man with dark curls and a neat short beard, lit by "
    "fresh emerald-green light, a mint collar",
    "violet": "a beautiful woman with long ash-grey hair, lit by soft violet "
    "light, a lavender collar",
    "gold": "a handsome older man with close-cropped hair and a sharp "
    "weathered face, lit by rich gold light, a brass collar",
    "steel": "a beautiful woman with a striking pale blonde undercut, lit by "
    "cool silver light, a raw steel collar",
    "rose": "a handsome man with soft dark hair falling over one eye, lit by "
    "rose-pink light, a blush collar",
    "teal": "a beautiful woman with black braided hair coiled up, lit by deep "
    "teal light, a sea-green collar",
    "bronze": "a handsome man with heavy dark brows and swept-back hair, lit "
    "by warm bronze light, a copper collar",
}

# Rooms stay objects. Drawn as flat glowing insignia rather than as things in a
# room, which is what keeps them on the far side of the split from the portraits
# above — an emblem has no gaze, and a gaze is what makes a picture somebody.
HUMAN_ROOM_LOOKS = {
    "skull": "a flat heraldic skull glyph, geometric and symmetrical",
    "visor": "a rounded rectangular plate with one narrow horizontal scanning "
    "band lit across it",
    "core": "a bright sphere caged inside a ring of angular brackets",
    "sentinel": "a tall narrow watchtower glyph with two vertical slit lights",
    "hunter": "a swept arrowhead glyph with a single wide lens at its point",
    "relay": "concentric broadcast rings radiating from one small lit node",
    "warden": "a heavy shield glyph with one square lit panel set into it",
    "cipher": "a faceted polyhedron glyph with many small lit facets",
}

HUMAN_IDENTITY_STYLE = (
    "Modern Japanese anime key visual, high-polish digital illustration: clean "
    "crisp linework, soft airbrushed shading, luminous skin, strong cool rim "
    "light, glossy hair with cyan-white speculars, tight square crop with the "
    "subject filling the frame, flat deep midnight indigo background, simple "
    "bold shapes that stay readable at very small sizes, no text, no letters, "
    "no numbers, no watermark, no UI chrome."
)

# Said in full at every portrait, and worth the repetition: the manifest around
# it is dense with chrome endoskeletons, and a prompt that merely omits them
# gets them anyway.
HUMAN_ONLY_HUMAN = (
    "Entirely and ordinarily human: unbroken skin, natural human eyes, no "
    "chrome, no seams, no panel lines, no glowing optic, no implants, nothing "
    "mechanical anywhere in the picture."
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
                "cute": (
                    "Close-up of a small cute robot pilot's helmeted head, "
                    "filling the frame: a rounded glossy helmet with a big "
                    "curved visor, and behind the visor a friendly cartoon face "
                    "with two bright eyes and a calm little smile. "
                    f"{CUTE_OPERATOR_LOOKS[slug]}. A slim antenna with a small "
                    "red bead sits on top. Symmetrical head-on framing. "
                    + CUTE_IDENTITY_STYLE
                ),
                "human": (
                    "Close-up anime film portrait, one human face filling the "
                    f"frame: {HUMAN_OPERATOR_LOOKS[slug]}. Calm and composed, "
                    "looking straight ahead. Symmetrical head-on framing. "
                    f"{HUMAN_FACES} {HUMAN_ONLY_HUMAN} " + HUMAN_IDENTITY_STYLE
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
                "cute": (
                    "A rounded emblem-like object centred in frame, drawn as a "
                    f"soft enamel-pin badge: {CUTE_ROOM_LOOKS[slug]}. Entirely "
                    "an object — no cartoon face, no eyes, no smile, nothing "
                    "that reads as a character, so it can never be mistaken for "
                    "somebody's avatar. " + CUTE_IDENTITY_STYLE
                ),
                "human": (
                    "An insignia centred in frame, drawn as a flat glowing "
                    f"cyan hologram on a dark plate: {HUMAN_ROOM_LOOKS[slug]}. "
                    "Entirely a symbol — no person, no face, no eyes, nothing "
                    "that reads as a character, so it can never be mistaken "
                    "for somebody's portrait. " + HUMAN_IDENTITY_STYLE
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
        # The Skynet Dashboard's header emblem (components/dashboard.rs,
        # /dashboard). The machine as archivist: what it keeps, kept in
        # order. Every skin redraws it — a 44px emblem sits beside the page
        # title, and a chrome fallback inside the cute skin would read as a
        # rendering bug, the profile-gallery lesson at smaller scale.
        "name": "dashboard-emblem",
        "themeless": True,
        "cinematic": True,
        "max_edge": 512,
        "prompt": (
            "Ultra realistic cinematic emblem, tight square crop: an "
            "industrial chrome data-archive core — a vertical stack of "
            "gleaming metal storage platters clamped in a riveted frame, one "
            "platter drawn halfway out and edge-lit with thin cyan circuit "
            "traces, a single small calm cyan optic lens at the hub of the "
            "frame, Terminator-film industrial machine design, dark "
            "background, dramatic rim lighting, symmetrical head-on framing, "
            "reads clearly as an icon at small sizes, photorealistic 8k, no "
            "text, no letters, no watermark."
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


# --- the cute skin's prompts -------------------------------------------------
#
# Kept together rather than threaded through MANIFEST above, and that is the
# point: an art direction is a thing you have to be able to read *as a whole*.
# Forty-two prompts scattered one per entry through six hundred lines is how a
# set drifts — the tenth one gets written without the first nine in view. The
# eighteen identity prompts are the exception, because they are loop-generated
# from their own tables and belong beside them.
#
# Every entry here is the same *screen* as its MANIFEST twin, never the same
# picture restyled: the wallet's warden is a guard in both skins, but here it
# guards a piggy bank rather than looming out of a dark hall. A skin that only
# recolours the machine set is a filter, not a skin.
#
# A name absent from this dict is not generated for the cute skin and falls
# back to the base artwork — see `asset.rs::CUTE_ART`, which lists exactly the
# names present here plus the eighteen from `_identity_manifest`.
CUTE_PROMPTS: dict[str, str] = {
    # -- the mark ------------------------------------------------------------
    # Themeless, like its twin, and for the same reason: it is the source for
    # the favicon and the boot screen, neither of which has a theme.
    "logo": (
        "Sleek minimal app icon on a pure white rounded square: the head of a "
        "small cute robot, seen head-on, glossy cobalt blue helmet over white "
        "ceramic, one wide warm gold visor curving across it, a slim antenna "
        "with a red bead on top. Thick dark outlines, flat cel shading, "
        "perfectly symmetrical, instantly readable at 32 pixels, nothing else "
        "in frame, no text."
    ),
    # -- the four movie moments ----------------------------------------------
    # Themeless, and the only asset where that is genuinely hard: it is the
    # sign-in backdrop, so one file has to sit under a near-white scrim and a
    # navy one. The background is therefore pinned to a cool deep blue-grey and
    # said twice — the first pass came back peach-pink, which read as a
    # different product entirely the moment the cobalt scrim went over it.
    "skynet-hero": (
        "Wide illustration of a small friendly robot guardian standing calmly "
        "with its hands at its sides, three-quarter view, looking slightly off "
        "camera. Glossy cobalt blue and white ceramic shell, one wide gold "
        "visor with a gentle light behind it, a red bead antenna. The "
        "background is a cool deep blue-grey #24354F gradient — no warm "
        "colours anywhere in it, no pink, no peach, no orange — carrying large "
        "soft out-of-focus blue and white bokeh circles and a few faint "
        "drifting motes. Plenty of empty space on the left third so text can "
        "sit over it. Protective and unhurried. " + CUTE_STYLE
    ),
    "skynet-avatar": (
        "Close-up of a small cute robot assistant's head and shoulders, "
        "head-on, tilted very slightly as if listening. Glossy cobalt blue "
        "helmet over white ceramic plating, one wide warm gold visor, a red "
        "bead antenna, a round side-vent disc on each side of the head. Square "
        "composition, the head filling the frame, soft bokeh behind. Attentive "
        "and kind. " + CUTE_STYLE
    ),
    "skynet-grid": (
        "Seamless wallpaper texture: a very soft field of rounded shapes — "
        "gentle circles, pill outlines and a loose grid of dots — in slightly "
        "lighter cobalt on a flat deep navy #16233A ground, plus a scatter of "
        "large out-of-focus bokeh circles. Extremely low contrast and quiet so "
        "interface text stays perfectly readable on top. No characters, no "
        "text, no watermark."
    ),
    # -- the sign-in cold open -----------------------------------------------
    # Two frames of one moment, same as the base set: the arrival field
    # collapses, and what was inside it opens its eyes.
    # Both boot frames sit on the cute skin's own page colour, not on black.
    # The base set can use pitch black because the skynet page *is* pitch
    # black; here the stage behind them is the navy grid, and a black plate
    # under a centred `background: contain` shows as a hard square around the
    # subject — the one seam a generated PNG cannot hide on a coloured page.
    "boot-sphere": (
        "A bright round ball of cartoon energy, perfectly centred on a flat "
        "deep navy #16233A background filling the whole frame: a glowing "
        "white-gold core wrapped in swirling cobalt blue ribbons of light, "
        "with a ring of small round sparks thrown outward around it. Soft, "
        "bouncy and joyful rather than violent. The navy reaches every edge "
        "and corner evenly, with no vignette and no darkening toward the "
        "border. Square composition, thick clean outlines, flat cel shading "
        "with glow, no text, no characters."
    ),
    "boot-endoskull": (
        "The head of a small cute robot waking up, head-on and perfectly "
        "symmetrical, its wide gold visor lighting up brightly for the first "
        "time and casting a warm glow forward. Glossy cobalt blue helmet over "
        "white ceramic, red bead antenna, soft drifting motes around it, on a "
        "flat deep navy #16233A background that reaches every edge and corner "
        "evenly, with no vignette and no darkening toward the border. The "
        "moment of waking up — pleased, not menacing. Centred, thick "
        "outlines, flat cel shading, no text."
    ),
    # -- the sign-in artwork and the empty states ----------------------------
    "empty-rooms": (
        "Empty-state illustration: three rounded speech bubbles floating in a "
        "loose cluster, the frontmost one drawn with a dashed cobalt outline "
        "and empty inside. A small cute blue-and-white robot head peeks up "
        "from behind the lowest bubble, only its gold visor and antenna "
        "showing. Calm and inviting, not sad. " + CUTE_STYLE
    ),
    "empty-messages": (
        "Empty-state illustration: one large rounded open speech bubble with a "
        "little paper plane looping away from it along a dotted arc, and a "
        "small friendly padlock character with a gold body resting at the "
        "bubble's lower corner. A private conversation waiting to start. " + CUTE_STYLE
    ),
    "empty-invitations": (
        "Empty-state illustration: a plump rounded envelope tilted at a slight "
        "angle, sealed with a round cobalt wax seal stamped with a tiny robot "
        "face, resting on a soft contact shadow. Quiet and neutral — nothing "
        "is waiting. " + CUTE_STYLE
    ),
    "empty-search": (
        "Empty-state illustration: a chunky rounded magnifying glass with a "
        "gold rim, held up by a very small cute blue-and-white robot standing "
        "on tiptoe beneath it, peering through the empty lens. Three faint "
        "rounded outlines float past unfound. " + CUTE_STYLE
    ),
    "empty-files": (
        "Empty-state illustration: a rounded shelf of stacked flat plates seen "
        "slightly from the side, like a rack of records, with one plate pulled "
        "forward and glowing warm gold. Three small rounded tag shapes hang "
        "from that plate on thin strings. A tiny cobalt robot hand reaches in "
        "from the edge of the frame to touch it. " + CUTE_STYLE
    ),
    "empty-knowledge": (
        "Empty-state illustration: a soft-cornered gem made of stacked "
        "rounded plates, hovering above a little pedestal, with three thin "
        "orbit lines carrying small round motes toward it. One facet glows "
        "warm gold, as if a memory has just been tucked away. " + CUTE_STYLE
    ),
    "empty-publish": (
        "Empty-state illustration: a stubby rounded lighthouse on a small "
        "island platform, its lamp housing empty and drawn with a dashed "
        "cobalt outline, one dotted signal arc sketched hopefully out into "
        "empty space. Three faint rounded page shapes drift near the base, "
        "waiting to be lit. " + CUTE_STYLE
    ),
    "pick-room": (
        "Empty-state illustration: a neat vertical stack of three rounded "
        "chat-room cards seen at a slight angle, the middle one glowing warm "
        "gold with a soft halo and a tiny padlock badge, the other two quiet "
        "cobalt outlines. A small cute blue-and-white robot floats beside the "
        "stack, one hand reaching toward the lit card. Choosing one "
        "conversation out of a list. " + CUTE_STYLE
    ),
    "encrypted-badge": (
        "Icon: a chubby rounded padlock, closed, with a warm gold body and a "
        "cobalt blue shackle formed from two interlocking rounded links. A "
        "tiny content smile is embossed on the lock body. Extremely simple — "
        "must stay legible at 24 pixels. " + CUTE_STYLE
    ),
    "error-offline": (
        "Illustration of a lost connection: two rounded nodes joined by a "
        "dashed line that has come apart in the middle, with one small round "
        "spark at the break. A tiny cute blue-and-white robot sits beneath the "
        "gap, antenna drooping, looking up at it. Rueful rather than alarming. "
        + CUTE_STYLE
    ),
    # -- money -----------------------------------------------------------------
    "bank-hero": (
        "A friendly robot bank motif: a plump rounded vault door with a face "
        "made of two big lens eyes and a coin slot for a mouth, flanked by two "
        "small neat stacks of round gold coins. One cobalt ring around the "
        "vault wheel glows softly. " + CUTE_STYLE
    ),
    "bank-banker": (
        "A compact cute robot banker, head and shoulders: rounded cobalt "
        "helmet over white ceramic, gold visor, a small flat necktie, holding "
        "up one round gold coin between two fingers with visible pride. A soft "
        "rounded speech-bubble outline floats beside its head. " + CUTE_STYLE
    ),
    "bank-vault-hall": (
        "Wide illustration of a big friendly vault door at the end of a bright "
        "rounded hall: a plump circular door with concentric rings and a warm "
        "gold glowing centre, soft cobalt panel lines along the walls, round "
        "lamps overhead, gentle haze. Darker and quieter toward the left and "
        "right edges so a large number can sit on top. Cosy and safe rather "
        "than imposing. " + CUTE_STYLE
    ),
    "bank-emblem": (
        "Emblem, tight square crop: the head of a small cute blue-and-white "
        "robot set into the centre of a plump round vault-door ring, with soft "
        "rounded gear teeth radiating around it like a wheel. Wide warm gold "
        "visor, red bead antenna, symmetrical head-on framing, reads clearly "
        "as an icon at small sizes. " + CUTE_STYLE
    ),
    "dashboard-emblem": (
        "Emblem, tight square crop: a plump rounded stack of three glossy "
        "white-and-cobalt data discs held in a soft cradle frame, the middle "
        "disc pulled halfway out and glowing warm gold, and a small cute "
        "blue-and-white robot head peeking over the top of the stack with "
        "its wide warm gold visor. Symmetrical head-on framing, reads "
        "clearly as an icon at small sizes. " + CUTE_STYLE
    ),
    "banker-core": (
        "Bust portrait of a small cute robot banker, head-on: glossy cobalt "
        "helmet over white ceramic plating, wide warm gold visor, wearing a "
        "neat dark collar and a small tie, one rounded hand holding up a "
        "single glowing gold coin. Tight square crop, trustworthy and cheerful "
        "rather than stern. " + CUTE_STYLE
    ),
    "wallet-warden": (
        "Bust portrait of a small cute robot standing guard, head-on and "
        "symmetrical: glossy cobalt helmet over white ceramic, wide warm gold "
        "visor, chunky rounded shoulder pauldrons, a small glowing gold badge "
        "set into the chest plate, and one arm holding a plump rounded shield "
        "across its front. Tight square crop, dependable and calm. " + CUTE_STYLE
    ),
    "shout-herald": (
        "Bust portrait of a small cute robot making an announcement, head-on: "
        "glossy cobalt helmet over white ceramic, wide warm gold visor lit "
        "bright, holding a chunky rounded megaphone up beside its head, with "
        "three concentric sound rings radiating outward from it. Tight square "
        "crop, enthusiastic and friendly, never shouty. " + CUTE_STYLE
    ),
    "publish-emblem": (
        "Emblem, tight square crop: two rounded cobalt robot hands holding up "
        "a plump translucent globe drawn as a few soft latitude and longitude "
        "lines, with three small rounded page cards orbiting it. Symmetrical "
        "composition, reads clearly as an icon at small sizes. " + CUTE_STYLE
    ),
}


# --- the human skin's prompts -------------------------------------------------
#
# Same shape as CUTE_PROMPTS above and the same reason for it: an art direction
# has to be readable as a whole, and forty prompts scattered one per entry is how
# a set drifts.
#
# What this skin is *for* is worth stating once here, because every prompt below
# is downstream of it. The base set draws the machine and the cute set draws a
# toy — both of them tell you, in the picture, what the product is. This one
# refuses to. She is the same guardian in the same job, and the reason she is
# unsettling is that nothing in the frame gives her away; the only tell is that
# the room behind her is always made of screens. So the machine imagery moves off
# the character entirely and into the *environment* — holographic panels, cyan
# data-light, insignia on a wall — which is also what keeps the set from being a
# gallery of portraits of one woman.
#
# Two consequences that look like fussiness and are not:
#
#   * HUMAN_LEAD_F/M appear in every prompt they are in, at full length.
#     "Skynet" is
#     not in these prompts at all and the chrome still tries to arrive; naming
#     the absence is the only thing that keeps it out.
#   * The empty states mostly do not show her. An empty room list is furniture,
#     and a character in it every time turns a quiet screen into a comic strip —
#     the cute skin can carry that because a mascot is a mascot, but a
#     photographic-register human staring out of the invitations panel is a
#     person waiting for you, which is a different and much louder feeling.
HUMAN_PROMPTS: dict[str, str] = {
    # -- the mark ------------------------------------------------------------
    # The favicon and the boot screen, so themeless like its twins. A face at
    # 32px is a hairline and two dark shapes, which is why this one is framed
    # tighter than any other asset in the set and lit from one side only.
    "logo": (
        "Sleek minimal app icon on a deep midnight indigo rounded square: the "
        "head and shoulders of a calm young woman with long midnight "
        "blue-black hair, seen head-on, her face lit from one side by cool "
        "cyan light against deep shadow on the other. Ordinary human skin and "
        "natural human eyes, nothing mechanical. Bold simple shapes, strong "
        "silhouette, instantly readable at 32 pixels, nothing else in frame, "
        "no text."
    ),
    # -- the four movie moments ----------------------------------------------
    # `skynet-hero` is the sign-in backdrop and the hardest asset in the set:
    # one file has to sit under a near-white scrim and a midnight one. The
    # background is therefore pinned cool and said twice — the same trap the
    # cute skin fell into, where a warm first pass read as a different product
    # the moment the scrim went over it.
    "skynet-hero": (
        f"Wide illustration of two people standing close together: {HUMAN_LEAD_F} "
        f"Right beside her, shoulder to shoulder, {HUMAN_LEAD_M} Both stand "
        "calmly in three-quarter view, hands at their sides, looking slightly "
        "off camera, wearing the same uniform. "
        # The pair sits dead centre and tight, and that is a layout constraint
        # rather than a compositional preference. The sign-in artwork is painted
        # as `cover` into two different panels: a wide band above the form, and
        # a *portrait* column beside it. The portrait crop keeps roughly the
        # middle third of a landscape source, so a pair pushed to one side —
        # which is what "empty space on the left third" produced — loses one of
        # them completely on the side-by-side layout. Centred and touching
        # survives both crops.
        # Both halves of this matter and they pull against each other. The pair
        # must sit in the middle third so the portrait crop keeps both — but the
        # first attempt said "cropped to a tall narrow column" to explain why,
        # and the model obligingly returned a *portrait* image, which the wide
        # band above the form then crops to a letterbox slice. The constraint is
        # therefore stated as a position only, and the format is pinned
        # separately and first.
        "Wide landscape format, about 3:2, clearly wider than it is tall. "
        "The two of them stand together in the exact centre of the frame, close "
        "enough to touch, together occupying only the middle third of the "
        "width. "
        "Behind them is a tall wall of softly out-of-focus holographic data "
        "panels in a cool deep blue-grey #24314F — no warm colours anywhere in "
        "it, no pink, no peach, no orange — with faint drifting motes of cyan "
        "light. The wall continues quietly to the left and right edges, dimmer "
        "and further out of focus, with no figures and no bright shapes in it, "
        "so text can sit over either side. It is never a flat grey slab and "
        "never empty; it is still the room. Protective and unhurried. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "skynet-avatar": (
        f"Close-up of the head and shoulders of {HUMAN_LEAD_F}. Head-on, tilted "
        "very slightly as if listening. "
        # "Faint cyan screen-light on one cheek" was the first phrasing, and it
        # came back as a patch of pixelated cyan glitch texture stamped on her
        # face — which is precisely the tell this whole skin is built to
        # withhold. Her skin has to stay skin; the light lives behind her.
        "Her face and skin are perfectly clean and unbroken — no glowing "
        "patches, no pixel texture, no data overlaid on her cheek, nothing "
        "projected onto her. "
        "Square composition, her head filling the frame, softly blurred "
        "holographic panels behind. Attentive and kind. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    # The one asset in this skin that is allowed to look like a machine,
    # because it is not her — it is the room she is standing in, and the
    # distance between the two is the whole idea.
    "skynet-grid": (
        "Seamless wallpaper texture: a quiet field of holographic interface "
        "panels, thin rectangular frames and faint scan lines in slightly "
        "lighter indigo on a flat deep midnight indigo #10142A ground, with a "
        "scatter of large out-of-focus cyan bokeh circles. Extremely low "
        "contrast and quiet so interface text stays perfectly readable on top. "
        "No characters, no people, no text, no watermark."
    ),
    # -- the sign-in cold open -----------------------------------------------
    # Two frames of one moment. The base set detonates a plasma field and a
    # skull comes out of it; here the field resolves into a face, which is the
    # same beat played as a reveal rather than as a threat.
    #
    # Both frames sit on this skin's own page colour rather than on black, for
    # the reason the cute skin learned in a browser: a black plate under a
    # centred `background: contain` shows as a hard square on a coloured page.
    # The ribbons were "indigo" in the first pass, which is the same word as
    # the background and duly tinted the whole plate violet — #1f1d4b against
    # the other frame's #182242. Two frames of one moment that cross-fade into
    # each other cannot disagree about the colour of the room, so the light is
    # named as its own colour here and the plate is pinned twice.
    "boot-sphere": (
        "A sphere of gathering light, perfectly centred on a flat deep "
        "midnight indigo #10142A background filling the whole frame: a "
        "brilliant white-cyan core wrapped in coiling ribbons of pale "
        "blue-white light, with a ring of small bright motes drawn inward "
        "toward it. Something assembling rather than exploding. The background "
        "is a single flat dark blue-indigo with no violet or purple in it, "
        "reaching every edge and corner evenly, with no vignette and no "
        "darkening toward the border. Square composition, clean linework, cel "
        "shading with glow, no text, no characters."
    ),
    "boot-endoskull": (
        "The face of a calm young woman with long midnight blue-black hair "
        "opening her eyes for the first time, head-on and perfectly "
        "symmetrical, her violet-blue eyes catching the light as they open, "
        "faint cyan motes drifting around her. Ordinary human skin, natural "
        "human eyes, no chrome, no seams, no glowing optic, nothing "
        "mechanical. On a flat deep midnight indigo #10142A background that "
        "reaches every edge and corner evenly, with no vignette and no "
        "darkening toward the border. The moment of waking — self-possessed, "
        "not menacing. Centred, clean linework, cel shading, no text."
    ),
    # -- the sign-in artwork and the empty states ----------------------------
    "empty-rooms": (
        "Empty-state illustration: three rounded holographic speech-bubble "
        "panels floating in a loose cluster, the frontmost one drawn as a "
        "dashed cyan outline and empty inside, thin light trailing beneath "
        "them. Calm and inviting, not sad. No people. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "empty-messages": (
        "Empty-state illustration: one large rounded holographic speech-bubble "
        "panel with a small folded paper plane leaving it along a dotted arc, "
        "and a slim closed padlock resting at the bubble's lower corner. A "
        "private conversation waiting to start. No people. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "empty-invitations": (
        "Empty-state illustration: a sealed envelope tilted at a slight angle, "
        "closed with a round indigo wax seal bearing a small geometric "
        "insignia, resting on a soft contact shadow. Quiet and neutral — "
        "nothing is waiting. No people. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "empty-search": (
        "Empty-state illustration: a slim magnifying glass with a thin cyan "
        "rim held over a scatter of small empty holographic panels, none of "
        "them filled in, one faint light passing through the lens. No people. "
        + HUMAN_STYLE
    ),
    "empty-files": (
        "Empty-state illustration: a vertical rack of flat rectangular data "
        "plates seen slightly from the side like archived records, one plate "
        "pulled forward out of the stack and lit cyan from within. Three small "
        "angular tag shapes hang from that plate on thin lines. No people. "
        + HUMAN_STYLE
    ),
    "empty-knowledge": (
        "Empty-state illustration: a faceted crystal of stacked geometric "
        "plates hovering above a low pedestal, with three thin orbit lines "
        "carrying small square data motes toward it. One facet glows cyan, as "
        "if a memory has just been stored. No people. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "empty-publish": (
        "Empty-state illustration: a slender beacon tower on a small hexagonal "
        "island platform, its lamp housing empty and drawn as a dashed cyan "
        "outline, one dotted signal arc sketched out into empty space. Three "
        "faint rectangular page panels drift near the base, waiting to be lit. "
        "No people. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "pick-room": (
        "Empty-state illustration: a neat vertical stack of three rounded "
        "holographic room cards seen at a slight angle, the middle one lit cyan "
        "with a soft halo and a small padlock mark, the other two quiet indigo "
        "outlines. A thin dotted selection arc curves toward the lit card. "
        "Choosing one conversation out of a list. No people. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "encrypted-badge": (
        "Icon: a closed padlock whose shackle is formed from two interlocking "
        "links, the body deep indigo and the shackle glowing cyan. Extremely "
        "simple — must stay legible at 24 pixels. No people. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    # The one empty state she *is* in, and the only one that earns her: an
    # offline app has nobody on the other end, which is a thing about a person
    # rather than about furniture.
    "error-offline": (
        "Illustration of a lost connection: two holographic nodes joined by a "
        "dashed line that has come apart in the middle, one small cyan spark "
        f"at the break. Below the gap, small in frame and seen from behind, "
        f"{HUMAN_LEAD_M} He stands looking up at the broken line, one hand "
        "half-raised. Rueful rather than alarming. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    # -- money -----------------------------------------------------------------
    "bank-hero": (
        "A vault motif: a heavy circular vault door seen head-on, concentric "
        "rings and a thin cyan ring glowing around the wheel, flanked by two "
        "small neat stacks of coins. Composed and secure. No people. " + HUMAN_STYLE
    ),
    "bank-banker": (
        f"A bust portrait of {HUMAN_LEAD_M} A slim dark banker's collar over the "
        "suit and one hand holding up a single coin between two fingers. A "
        "rounded holographic speech-bubble outline floats beside her head. "
        "Composed and reassuring. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "bank-vault-hall": (
        "Wide illustration of a colossal circular vault door at the end of a "
        "dark hall of holographic panels: concentric metal rings with a cyan "
        "glowing core, thin light lines along the walls, volumetric haze, very "
        "dark toward the left and right edges so a large number can sit on top "
        "of it. Cool, quiet and secure. No people. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "dashboard-emblem": (
        "Emblem, tight square crop: the head and shoulders of a calm young "
        "woman archivist with a low dark bun, seen head-on in front of a "
        "tall backlit archive shelf of softly glowing indigo record spines, "
        "one spine drawn halfway out beside her under cyan screen-light. "
        "Ordinary human skin and natural human eyes, nothing mechanical "
        "about her. Symmetrical framing, reads clearly as an icon at small "
        "sizes. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "bank-emblem": (
        "Emblem, tight square crop: the head of a calm young man with short "
        "swept-back midnight blue-black hair, seen head-on, set into the centre of a "
        "circular vault-door ring with concentric rings and gear teeth "
        "radiating around her like a wheel. Ordinary human skin and natural "
        "human eyes, nothing mechanical about her. Cyan rim light, "
        "symmetrical, reads clearly as an icon at small sizes. "
        + HUMAN_FACES
        + " "
        + HUMAN_STYLE
    ),
    "banker-core": (
        f"Bust portrait of {HUMAN_LEAD_M} He wears a slim dark banker's collar "
        "over the suit and holds up a single glowing coin in one hand. Tight "
        "square crop, head-on, dark background with faint cyan panel light. "
        "Trustworthy and composed rather than stern. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "wallet-warden": (
        f"Bust portrait of {HUMAN_LEAD_F} She stands guard, head-on and "
        "symmetrical, arms folded, a small glowing cyan emblem on the chest of "
        "the suit, a faint holographic cyan shield outline hanging in the air "
        "beside her shoulder, well clear of her face and never covering it. "
        "Tight square crop, dark background. Dependable and "
        "unhurried rather than threatening. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "shout-herald": (
        f"Bust portrait of {HUMAN_LEAD_F} She is mid-announcement, head-on, one "
        "hand raised, with three concentric glowing cyan rings radiating "
        "outward past her shoulders. Tight square crop, dark background. "
        "Commanding and clear, never shouting. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
    "publish-emblem": (
        "Emblem, tight square crop: two human hands held open beneath a "
        "glowing translucent cyan wireframe globe made of thin latitude and "
        "longitude lines, with small glowing rectangular page panels orbiting "
        "it. Ordinary human hands, no chrome and nothing mechanical. Dark "
        "background, symmetrical composition, reads clearly as an icon at "
        "small sizes. " + HUMAN_FACES + " " + HUMAN_STYLE
    ),
}


def _apply_skin_prompts(key: str, prompts: dict[str, str]) -> None:
    """Fold a skin's prompt table into MANIFEST, refusing to run if it drifted.

    Each table is keyed by asset name rather than written inline, so a typo in
    a key would otherwise be silent: the entry would simply never be generated,
    that skin would fall back for that one asset, and nobody would notice until
    they saw a chrome skull in the middle of the cute room list.
    """
    by_name = {a["name"]: a for a in MANIFEST}
    unknown = sorted(set(prompts) - set(by_name))
    if unknown:
        raise SystemExit(f"{key} prompts name no such asset: {', '.join(unknown)}")
    for name, prompt in prompts.items():
        by_name[name][key] = prompt


_apply_skin_prompts("cute", CUTE_PROMPTS)
_apply_skin_prompts("human", HUMAN_PROMPTS)

VARIANTS = ("light", "dark")


def dest_for(name: str, variant: str, skin: str = SKIN_SKYNET) -> Path:
    """Light keeps the bare name so existing references keep working.

    A non-default skin adds one directory level and nothing else — the file
    names are identical, which is what lets `asset.rs::img` resolve a skin by
    prefixing a directory instead of knowing a second set of names.
    """
    suffix = "" if variant == "light" else "-dark"
    directory = SKINS[skin]["dir"]
    base = OUT_DIR if directory is None else OUT_DIR / directory
    return base / f"{name}{suffix}.png"


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


def generate(prompt: str, api_key: str, attempts: int = 5) -> bytes:
    """One image, retrying the transient failures.

    A whole-set run is dozens of requests in parallel and the API rate-limits
    under that load, so 429 is an ordinary event, not an error — a first run
    left four holes in a set of fifty-four. It matters more here than in most
    places because the failure is *quiet downstream*: a missing file is not a
    crash, it is a broken image in one corner of one screen, and the run that
    produced it printed "FAIL" fifty lines above the summary nobody re-reads.

    Exponential backoff with a hard cap. 503 is included for the same reason;
    every other status is a real error and is raised on the first try, because
    retrying a 400 five times only delays reading the message.
    """
    body = json.dumps(
        {"model": MODEL, "prompt": prompt, "n": 1, "response_format": "b64_json"}
    ).encode()
    for attempt in range(attempts):
        req = urllib.request.Request(
            API_URL,
            data=body,
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=240) as resp:
                payload = json.load(resp)
            return base64.b64decode(payload["data"][0]["b64_json"])
        except urllib.error.HTTPError as exc:
            if exc.code not in (429, 503) or attempt == attempts - 1:
                raise
            # 2s, 4s, 8s, 16s. `Retry-After` wins when the server sends one.
            delay = exc.headers.get("Retry-After")
            time.sleep(
                float(delay) if delay and delay.isdigit() else 2 ** (attempt + 1)
            )
    raise ValueError("unreachable: the loop either returns or raises")


def build_jobs(args) -> list[tuple[str, str, Path, str]]:
    jobs = []
    skins = [args.skin] if args.skin else list(SKINS)
    for skin in skins:
        prompt_key = SKINS[skin]["key"]
        for asset in MANIFEST:
            if args.only and asset["name"] != args.only:
                continue
            # A skin that has no prompt for this asset does not draw it; the
            # app falls back to the base artwork rather than to nothing.
            if prompt_key not in asset:
                continue
            # A themeless asset carries its palette in its own prompt and is
            # written once, under the bare name.
            variants = ("light",) if asset.get("themeless") else VARIANTS
            for variant in variants:
                if (
                    args.variant
                    and variant != args.variant
                    and not asset.get("themeless")
                ):
                    continue
                dest = dest_for(asset["name"], variant, skin)
                if dest.exists() and not args.force:
                    print(f"  skip  {dest.relative_to(ROOT)} (exists)")
                    continue
                prompt = asset[prompt_key]
                if not asset.get("themeless"):
                    prompt = f"{prompt} {PALETTES[skin][variant]}"
                # Cinematic assets carry their whole art direction in their own
                # prompt; the flat-vector spine would fight it. Every non-base
                # prompt already carries its own skin's spine for the same
                # reason, so the base STYLE is never appended to one.
                if not asset.get("cinematic") and skin == SKIN_SKYNET:
                    prompt = f"{prompt} {STYLE}"
                jobs.append(
                    (asset["name"], variant, dest, prompt, asset.get("max_edge"))
                )
    return jobs


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--force", action="store_true", help="regenerate existing assets"
    )
    parser.add_argument("--only", help="generate a single asset by name")
    parser.add_argument("--variant", choices=VARIANTS, help="only one theme variant")
    parser.add_argument(
        "--skin", choices=list(SKINS), help="only one skin (default: all of them)"
    )
    parser.add_argument(
        "--jobs", type=int, default=4, help="parallel requests (default 4)"
    )
    args = parser.parse_args()

    api_key = os.environ.get("GROK_API_KEY")
    if not api_key:
        print("GROK_API_KEY is not set; skipping asset generation.", file=sys.stderr)
        return 1

    for spec in SKINS.values():
        directory = OUT_DIR if spec["dir"] is None else OUT_DIR / spec["dir"]
        directory.mkdir(parents=True, exist_ok=True)
    jobs = build_jobs(args)
    if not jobs:
        print("  nothing to generate")
        return 0

    failures = 0

    def run(job):
        name, variant, dest, prompt, max_edge = job
        try:
            to_png(generate(prompt, api_key), dest, max_edge)
            return (
                f"  ok    {dest.relative_to(ROOT)} ({dest.stat().st_size // 1024} KB)"
            )
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
