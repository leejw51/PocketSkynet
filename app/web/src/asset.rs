//! Where an image lives, given the skin wearing it.
//!
//! # Why this module exists
//!
//! Before skins, every illustration was a literal in the markup:
//! `"/static/img/wallet-warden.png"`, written out at each of the dozen or so
//! places a picture appears. That works exactly as long as there is one set of
//! pictures. The moment a second art direction exists, every one of those
//! literals is a place the app can render the wrong skin's artwork — and the
//! bug is invisible in review, because the literal is *correct*, just not for
//! the skin in effect.
//!
//! So the path stops being a literal and becomes a lookup. [`img`] is the only
//! function in the crate that builds a `/static/img/…` URL; §11 of `app.css`
//! does the same job for the CSS half through the `--img-*` token registry.
//!
//! # Fallback, and why it is not a failure mode
//!
//! A skin does not have to redraw all eighty-seven assets to exist. [`art_list`]
//! gives the stems a skin actually ships, and anything absent from it resolves
//! to the base artwork. That is a deliberate property, not a gap to close later:
//! it means a new skin can be introduced with a dozen pictures and grown one
//! asset at a time, and it means a half-generated asset directory renders a
//! coherent product rather than a page of broken images.
//!
//! Those lists are a contract with the prompt tables in `tools/genart.py` — the
//! generator writes the files, these arrays say which ones to reach for.
//! `art_files_exist` below fails the build's test run if the two drift.

use crate::session::Skin;

/// The stems the cute skin redraws. Everything else falls back to the base
/// artwork — see the module docs; that is the design, not an omission.
///
/// Ordered as the generator emits them: cinematic set, themed illustrations,
/// room sigils, operator faces, profile portraits.
///
/// The twenty `tp-*` portraits were originally left out on the theory that a
/// gallery the user opens rarely could fall back. Putting the skin in front of
/// a browser settled it: the picker is the one screen where the fallback is
/// *side by side with itself*, twenty photoreal endoskeletons in a grid inside
/// an otherwise entirely cute interface, and it read as a rendering bug rather
/// than as a deliberate economy. Fallback works where the base art is quiet;
/// it does not work where the art is the content.
pub const CUTE_ART: [&str; 63] = [
    // Cinematic — one file each, their palette baked into the prompt.
    "logo",
    "skynet-hero",
    "skynet-avatar",
    "skynet-grid",
    "boot-sphere",
    "boot-endoskull",
    "bank-emblem",
    "banker-core",
    "bank-vault-hall",
    "wallet-warden",
    "shout-herald",
    "publish-emblem",
    "dashboard-emblem",
    // Themed illustrations — a light and a dark variant each.
    "empty-rooms",
    "empty-messages",
    "empty-invitations",
    "empty-search",
    "empty-files",
    "empty-knowledge",
    "empty-publish",
    "pick-room",
    "encrypted-badge",
    "error-offline",
    "bank-hero",
    "bank-banker",
    // Room sigils — the rack's posters.
    "room-skull",
    "room-visor",
    "room-core",
    "room-sentinel",
    "room-hunter",
    "room-relay",
    "room-warden",
    "room-cipher",
    // Operator faces — on every message, so they carry the skin further than
    // anything else in the set.
    "op-amber",
    "op-cyan",
    "op-crimson",
    "op-emerald",
    "op-violet",
    "op-gold",
    "op-steel",
    "op-rose",
    "op-teal",
    "op-bronze",
    // The chooseable gallery. `identity::PROFILE_ART` is the same twenty slugs
    // and is the wire contract (`preset:tp-coder-f`); this list only decides
    // which drawing of them a skin reaches for.
    "tp-coder-m",
    "tp-coder-f",
    "tp-soldier-m",
    "tp-soldier-f",
    "tp-medic-m",
    "tp-medic-f",
    "tp-pilot-m",
    "tp-pilot-f",
    "tp-artist-m",
    "tp-artist-f",
    "tp-scientist-m",
    "tp-scientist-f",
    "tp-chef-m",
    "tp-chef-f",
    "tp-athlete-m",
    "tp-athlete-f",
    "tp-musician-m",
    "tp-musician-f",
    "tp-detective-m",
    "tp-detective-f",
];

/// The stems the human skin redraws.
///
/// The same sixty-three as the cute skin, and that is not a coincidence worth
/// hiding behind an alias: the two lists are the same *because both skins chose
/// to redraw everything the base set draws that has a subject in it*, and a
/// third skin that redraws forty would be no less valid. Aliasing them would
/// say the set is fixed, and the next skin would have to break the alias to be
/// allowed to ship a partial one.
///
/// Ordered as the generator emits them, matching [`CUTE_ART`] so the two can be
/// diffed by eye.
pub const HUMAN_ART: [&str; 63] = CUTE_ART;

/// The stems `skin` ships its own drawing of. Empty for the base skin, whose
/// art *is* the fallback.
fn art_list(skin: Skin) -> &'static [&'static str] {
    match skin {
        Skin::Skynet => &[],
        Skin::Cute => &CUTE_ART,
        Skin::Human => &HUMAN_ART,
    }
}

/// Whether `skin` ships its own drawing of `stem`.
fn overrides(skin: Skin, stem: &str) -> bool {
    art_list(skin).contains(&stem)
}

/// The `src` for an illustration, in the skin currently worn.
///
/// `stem` is the bare file name without extension or directory —
/// `"wallet-warden"`, not `"/static/img/wallet-warden.png"`. Callers that
/// already hold a full URL (a server-hosted `/api/images/…` upload) must not
/// come through here; this function is for the shipped set only.
pub fn img(skin: Skin, stem: &str) -> String {
    match skin.art_dir() {
        Some(dir) if overrides(skin, stem) => format!("/static/img/{dir}/{stem}.png"),
        _ => format!("/static/img/{stem}.png"),
    }
}

/// The same resolution as a CSS `url(…)` value, for the inline styles that
/// paint a background rather than fill an `<img>` (the room rack's posters).
pub fn img_url(skin: Skin, stem: &str) -> String {
    format!("url('{}')", img(skin, stem))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_skin_never_reaches_into_a_skin_directory() {
        assert_eq!(img(Skin::Skynet, "logo"), "/static/img/logo.png");
        // Even for a stem the *other* skin overrides.
        assert_eq!(img(Skin::Skynet, "room-core"), "/static/img/room-core.png");
    }

    #[test]
    fn a_skin_takes_its_own_art_where_it_has_it() {
        assert_eq!(img(Skin::Cute, "logo"), "/static/img/cute/logo.png");
        assert_eq!(img(Skin::Human, "logo"), "/static/img/human/logo.png");
    }

    #[test]
    fn a_skin_falls_back_where_it_does_not() {
        // Nothing shipped is currently outside the override lists, so this
        // uses a name that is not in the manifest at all — which is also the
        // case that matters: a stem added to a component before its art exists
        // must render the base file rather than a broken image.
        for skin in Skin::ALL {
            assert_eq!(
                img(skin, "not-a-generated-asset"),
                "/static/img/not-a-generated-asset.png"
            );
        }
    }

    /// The gallery is a wire contract: `profileImage` stores `preset:<slug>`
    /// and every client resolves it. A skin may redraw those portraits but it
    /// must never change which slugs exist, so if it draws one it draws all
    /// twenty — a half-covered gallery renders two art directions in one grid.
    #[test]
    fn the_profile_gallery_is_all_or_nothing_per_skin() {
        for skin in Skin::ALL {
            let covered = crate::identity::PROFILE_ART
                .iter()
                .filter(|slug| overrides(skin, slug))
                .count();
            assert!(
                covered == 0 || covered == crate::identity::PROFILE_ART.len(),
                "the {} skin redraws {covered} of {} profile portraits; \
                 it must draw all of them or none",
                skin.as_str(),
                crate::identity::PROFILE_ART.len()
            );
        }
    }

    #[test]
    fn no_override_list_has_duplicates() {
        for skin in Skin::ALL {
            let mut seen = art_list(skin).to_vec();
            seen.sort_unstable();
            let before = seen.len();
            seen.dedup();
            assert_eq!(
                before,
                seen.len(),
                "the {} skin's list names a stem twice",
                skin.as_str()
            );
        }
    }

    /// Every skin that claims a directory must have art in it, and every skin
    /// that does not must have no art to place — the pair that [`img`] joins.
    #[test]
    fn only_a_skin_with_a_directory_overrides_anything() {
        for skin in Skin::ALL {
            assert_eq!(
                skin.art_dir().is_some(),
                !art_list(skin).is_empty(),
                "the {} skin's directory and override list disagree",
                skin.as_str()
            );
        }
    }

    /// The contract with `tools/genart.py`: every stem a skin promises must be
    /// a file on disk. A missing file is not a crash — [`img`] would still
    /// return its path and the browser would render nothing — so it has to be
    /// caught here.
    #[test]
    fn art_files_exist() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("static/img");
        for skin in Skin::ALL {
            let Some(dir) = skin.art_dir().map(|d| root.join(d)) else {
                continue;
            };
            if !dir.exists() {
                // The generator has not been run in this checkout. Not a
                // failure: `make assets` is opt-in and needs a GROK_API_KEY.
                continue;
            }
            let missing: Vec<&str> = art_list(skin)
                .iter()
                .copied()
                .filter(|stem| !dir.join(format!("{stem}.png")).exists())
                .collect();
            assert!(
                missing.is_empty(),
                "{} art missing for: {missing:?}",
                skin.as_str()
            );
        }
    }
}
