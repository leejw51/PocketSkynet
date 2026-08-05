//! The image lightbox — tap a picture in a room and it takes the screen.
//!
//! A picture posted into a conversation is capped at 400px so a room stays a
//! room, which means every screenshot anybody actually wants to *read* arrives
//! too small to read. This is the way back out: the picture lifts off the
//! bubble, grows to the viewport under a blurred scrim, and lands back in the
//! bubble when dismissed.
//!
//! # The zoom is a FLIP, not a fade
//!
//! Crossfading a small picture into a big one in the middle of the screen
//! reads as two pictures. So the full-screen copy is rendered at its resting
//! size first, measured, and then given the transform that puts it back
//! *exactly* on top of the thumbnail — same place, same size — which the
//! entrance animation removes over 220ms. What the eye follows is one object
//! travelling, which is what actually happened.
//!
//! That measurement is also why the effect is immune to the iOS viewport
//! problem `burst.rs::to_layer` documents: the transform is the *difference*
//! between two `getBoundingClientRect` reads, so whichever viewport the
//! browser is reporting them against cancels out.
//!
//! Both rects are the rect of the *painted pixels*, not of the element box —
//! an attachment thumbnail is `object-fit: contain` inside a fixed-width card,
//! so its element box is letterboxed and zooming from it would start the
//! picture wider than the picture.
//!
//! # Deliberately not theatre
//!
//! [`spotlight`](super::spotlight) — the other full-screen zoom in this
//! product — arrives with orbiting light and a spark burst, because a portrait
//! is a moment. This is not: somebody is trying to read a screenshot, and
//! anything on top of it is in the way. The scrim, the travel and the caption
//! are the whole effect.
//!
//! Singleton like the burst and spotlight layers: mounted once near the root,
//! raised from anywhere with [`zoom`].

use std::cell::{Cell, RefCell};

use wasm_bindgen::JsCast;
use yew::prelude::*;

use crate::i18n::{t, Key};

// ------------------------------------------------------------------ geometry --

/// A rectangle in viewport coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    fn centre(&self) -> (f64, f64) {
        (self.x + self.w / 2.0, self.y + self.h / 2.0)
    }

    /// A box with no area cannot be zoomed from or to. Sub-pixel rects come
    /// from an element that has not been laid out yet.
    fn is_real(&self) -> bool {
        self.w >= 1.0 && self.h >= 1.0
    }

    fn of(r: &web_sys::DomRect) -> Self {
        Self {
            x: r.left(),
            y: r.top(),
            w: r.width(),
            h: r.height(),
        }
    }
}

/// Where an `object-fit: contain` image actually paints inside its box.
///
/// The element box and the picture are the same thing only when the two share
/// an aspect ratio. A 16:9 screenshot in the 420px-wide attachment card is
/// letterboxed top and bottom, and a zoom that starts from the box starts the
/// picture taller than it ever was — a visible jump on the first frame, which
/// is the one frame the whole effect is trying to make invisible.
///
/// Falls back to the box when the natural size is unknown (an image that has
/// not loaded reports 0×0), which is the right answer for every embed that
/// sizes itself to its content anyway.
pub fn painted(area: Rect, natural_w: f64, natural_h: f64) -> Rect {
    if natural_w <= 0.0 || natural_h <= 0.0 || !area.is_real() {
        return area;
    }
    let scale = (area.w / natural_w).min(area.h / natural_h);
    let (w, h) = (natural_w * scale, natural_h * scale);
    let (cx, cy) = area.centre();
    Rect {
        x: cx - w / 2.0,
        y: cy - h / 2.0,
        w,
        h,
    }
}

/// The transform that puts the full-screen picture back over its thumbnail:
/// `translate(dx, dy) scale(s)`, applied about the element's own centre.
///
/// `None` when either rect has no area — the caller then swells the picture in
/// place rather than animating from a garbage origin.
pub fn flip(from: Rect, to: Rect) -> Option<(f64, f64, f64)> {
    if !from.is_real() || !to.is_real() {
        return None;
    }
    let (fx, fy) = from.centre();
    let (tx, ty) = to.centre();
    Some((fx - tx, fy - ty, from.w / to.w))
}

/// How far past its own pixels a picture may be blown up.
///
/// Fitting the viewport unconditionally turns a 96px sticker into a wall of
/// mush; refusing to upscale at all makes the zoom do nothing for anything
/// small, since the thumbnail was already showing it at natural size. Two is
/// the largest factor that still reads as the picture rather than as its
/// pixels.
const MAX_UPSCALE: f64 = 2.0;

/// The resting geometry, handed to CSS rather than computed here.
///
/// `--lb-ar` gives the image a definite box before a single byte of it has
/// arrived, so the entrance can be measured on the frame it mounts; `--lb-cap`
/// is the upscale ceiling. Both are expressed so the browser keeps owning the
/// layout — a window resize while the lightbox is open re-fits the picture
/// with no Rust involved.
fn rest_style(shot: &Shot) -> String {
    let (ar, cap) = match shot.natural {
        Some((w, h)) if w > 0.0 && h > 0.0 => (w / h, format!("{:.0}px", w * MAX_UPSCALE)),
        // No natural size: hold the thumbnail's shape and let it fill.
        _ => (
            shot.origin.filter(Rect::is_real).map_or(1.0, |o| o.w / o.h),
            "100%".to_string(),
        ),
    };
    format!("--lb-ar:{ar:.4}; --lb-cap:{cap};")
}

/// The entrance, in the terms the keyframes need.
fn enter_style(enter: Enter) -> String {
    match enter {
        Enter::Pending => String::new(),
        // Opacity 1: the picture does not fade in, because it is already on
        // screen — it is the same picture, moving.
        Enter::From(dx, dy, s) => {
            format!(" --lb-dx:{dx:.1}px; --lb-dy:{dy:.1}px; --lb-s:{s:.4}; --lb-o:1;")
        }
        Enter::Swell => " --lb-dx:0px; --lb-dy:0px; --lb-s:0.92; --lb-o:0;".to_string(),
    }
}

/// How the picture gets from its thumbnail to the screen.
#[derive(Clone, Copy, PartialEq)]
enum Enter {
    /// Not measured yet. Lasts exactly one frame, during which the picture is
    /// laid out but not painted.
    Pending,
    /// Measured: start life exactly over the thumbnail and travel from there.
    From(f64, f64, f64),
    /// No usable origin (the thumbnail scrolled away, or never had a box).
    /// Swell up from 92% and fade instead.
    Swell,
}

// ------------------------------------------------------------------ the API --

/// What the lightbox is showing.
#[derive(Clone, PartialEq)]
pub struct Shot {
    /// The image URL — a same-origin path, an external URL, or a blob URL for
    /// an attachment, whose bytes the embed has already fetched.
    pub src: String,
    pub alt: String,
    /// A line under the picture: an attachment's filename. `None` for an
    /// inline image, whose URL is already in the message above it.
    pub caption: Option<String>,
    /// The painted rect of the thumbnail this was raised from.
    pub origin: Option<Rect>,
    /// Natural pixel size, when the thumbnail knows it.
    pub natural: Option<(f64, f64)>,
}

enum Op {
    Open(Shot),
    Close,
}

thread_local! {
    static EMIT: RefCell<Option<Callback<Op>>> = const { RefCell::new(None) };
}

fn send(op: Op) {
    EMIT.with(|e| {
        if let Some(cb) = e.borrow().as_ref() {
            cb.emit(op);
        }
    });
}

/// Raise the lightbox for exactly what it says. No-op if the layer isn't
/// mounted.
pub fn show(shot: Shot) {
    send(Op::Open(shot));
}

/// Raise the lightbox for an `<img>` that is on screen right now.
///
/// Everything the entrance needs is read off the element at the moment of the
/// tap — source, alt text, natural size and where it is sitting — because a
/// rect captured any later is a rect from after the list moved.
pub fn zoom(node: &NodeRef, caption: Option<String>) {
    let Some(img) = node.cast::<web_sys::HtmlImageElement>() else {
        return;
    };
    let natural = (
        f64::from(img.natural_width()),
        f64::from(img.natural_height()),
    );
    let box_rect = Rect::of(&img.get_bounding_client_rect());
    show(Shot {
        src: img.src(),
        alt: img.alt(),
        caption,
        origin: Some(painted(box_rect, natural.0, natural.1)),
        natural: (natural.0 > 0.0 && natural.1 > 0.0).then_some(natural),
    });
}

/// Matches `.fn-lightbox[data-closing]` in app.css §23. The two constants are
/// one timeline and nothing enforces it, so they are documented as one.
const EXIT_MS: u32 = 140;

// ---------------------------------------------------------------- the layer --

/// The singleton. Mount once, near the root.
#[function_component(LightboxLayer)]
pub fn lightbox_layer() -> Html {
    let lang = crate::state::use_store().language;
    // The id rises with every open, so raising a *second* lightbox on the same
    // src still re-runs the measurement — otherwise a picture zoomed, closed
    // and zoomed again would keep the first thumbnail's origin.
    let shot = use_state(|| Option::<(u64, Shot)>::None);
    let enter = use_state(|| Enter::Pending);
    let closing = use_state(|| false);
    let img_ref = use_node_ref();

    // The inbox.
    {
        let shot = shot.clone();
        let enter = enter.clone();
        let closing = closing.clone();
        use_effect_with((), move |_| {
            let seq = Cell::new(0u64);
            let cb = Callback::from(move |op: Op| match op {
                Op::Open(s) => {
                    seq.set(seq.get() + 1);
                    enter.set(Enter::Pending);
                    closing.set(false);
                    shot.set(Some((seq.get(), s)));
                }
                Op::Close => shot.set(None),
            });
            EMIT.with(|e| *e.borrow_mut() = Some(cb));
            || EMIT.with(|e| *e.borrow_mut() = None)
        });
    }

    // Measure, one frame after mount: the picture is laid out (its box comes
    // from `--lb-ar`, not from the bytes) but not yet painted, so the
    // transform back onto the thumbnail can be computed before anything is
    // visible.
    {
        let img_ref = img_ref.clone();
        let enter = enter.clone();
        let origin = shot.as_ref().and_then(|(_, s)| s.origin);
        let natural = shot.as_ref().and_then(|(_, s)| s.natural);
        use_effect_with(shot.as_ref().map(|(id, _)| *id), move |open| {
            if open.is_none() {
                return;
            }
            // The painted rect again, not the element box: clamping a tall
            // picture to the viewport leaves the box wider than the picture
            // (app.css §23), and travelling to the box would land the
            // thumbnail's edges somewhere the picture never was.
            let to = img_ref.cast::<web_sys::Element>().map(|el| {
                let (nw, nh) = natural.unwrap_or((0.0, 0.0));
                painted(Rect::of(&el.get_bounding_client_rect()), nw, nh)
            });
            enter.set(match (origin, to) {
                (Some(from), Some(to)) => match flip(from, to) {
                    Some((dx, dy, s)) => Enter::From(dx, dy, s),
                    None => Enter::Swell,
                },
                _ => Enter::Swell,
            });
        });
    }

    // Every dismissal routes through here: Escape, the scrim, the picture
    // itself, the close button. The picture animates *back* to its thumbnail
    // before the node goes, which is the half of the pair CSS cannot do alone.
    let request_close = {
        let closing = closing.clone();
        Callback::from(move |_: ()| {
            if *closing {
                return;
            }
            closing.set(true);
            wasm_bindgen_futures::spawn_local(async move {
                super::common::exit_sleep(EXIT_MS).await;
                send(Op::Close);
            });
        })
    };

    // Escape closes, while open.
    {
        let request_close = request_close.clone();
        use_effect_with(shot.is_some(), move |open| {
            let listener = open.then(|| {
                gloo_events::EventListener::new(
                    &web_sys::window().expect("a browser window"),
                    "keydown",
                    move |e| {
                        if let Some(e) = e.dyn_ref::<web_sys::KeyboardEvent>() {
                            if e.key() == "Escape" {
                                request_close.emit(());
                            }
                        }
                    },
                )
            });
            move || drop(listener)
        });
    }

    let Some((_, s)) = (*shot).clone() else {
        return Html::default();
    };

    let close = {
        let request_close = request_close.clone();
        Callback::from(move |_: MouseEvent| request_close.emit(()))
    };

    html! {
        <div
            class="fn-lightbox"
            role="dialog"
            aria-modal="true"
            aria-label={s.caption.clone().unwrap_or_else(|| s.alt.clone())}
            // Absent until measured: the rule that holds the picture back for
            // that one frame keys on the attribute being missing, so there is
            // no state in which a stale transform can paint.
            data-ready={(*enter != Enter::Pending).then_some("true")}
            data-closing={closing.then_some("true")}
            style={format!("{}{}", rest_style(&s), enter_style(*enter))}
            // Anywhere. A picture at full screen has no interior anyone needs
            // to click, and hunting for the ✕ to put it away is friction on
            // the most common action there is.
            onclick={close.clone()}
        >
            <figure class="fn-lightbox__frame">
                <img ref={img_ref} class="fn-lightbox__img" src={s.src.clone()} alt={s.alt.clone()} />
                if let Some(c) = &s.caption {
                    <figcaption class="fn-lightbox__caption fn-truncate">{ c }</figcaption>
                }
            </figure>
            <button
                type="button"
                class="topcoat-icon-button--quiet fn-lightbox__close"
                aria-label={t(lang, Key::close)}
                onclick={close}
            >
                { super::icons::close(20) }
            </button>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn a_letterboxed_thumbnail_zooms_from_its_picture_not_its_card() {
        // The attachment card: 420 wide, 320 tall, holding a 16:9 shot. The
        // picture paints 420×236 and is centred, so 42px of card above and
        // below it are not the picture.
        let card = r(100.0, 200.0, 420.0, 320.0);
        let shot = painted(card, 1600.0, 900.0);
        assert!((shot.w - 420.0).abs() < 0.5);
        assert!((shot.h - 236.25).abs() < 0.5);
        assert!((shot.x - 100.0).abs() < 0.5);
        assert!((shot.y - 241.875).abs() < 0.5, "centred in the card");
    }

    #[test]
    fn an_image_that_hugs_its_box_is_its_own_painted_rect() {
        let box_ = r(10.0, 20.0, 300.0, 200.0);
        assert_eq!(painted(box_, 1500.0, 1000.0), box_);
        // Nothing known about the picture: the box is the best answer there
        // is, and it is never worse than guessing.
        assert_eq!(painted(box_, 0.0, 0.0), box_);
    }

    #[test]
    fn the_entrance_transform_lands_the_big_picture_on_the_small_one() {
        let from = r(40.0, 600.0, 200.0, 150.0);
        let to = r(300.0, 100.0, 800.0, 600.0);
        let (dx, dy, s) = flip(from, to).expect("both rects have area");

        // Apply the transform the way CSS does — scale about the centre, then
        // translate — and the result must be the thumbnail, to the pixel.
        let (tcx, tcy) = to.centre();
        let landed = Rect {
            x: tcx + dx - to.w * s / 2.0,
            y: tcy + dy - to.h * s / 2.0,
            w: to.w * s,
            h: to.h * s,
        };
        assert!((landed.x - from.x).abs() < 1e-9);
        assert!((landed.y - from.y).abs() < 1e-9);
        assert!((landed.w - from.w).abs() < 1e-9);
        assert!((landed.h - from.h).abs() < 1e-9);
    }

    #[test]
    fn a_rect_with_no_area_is_not_an_origin() {
        assert!(flip(r(0.0, 0.0, 0.0, 0.0), r(0.0, 0.0, 800.0, 600.0)).is_none());
        assert!(flip(r(0.0, 0.0, 200.0, 150.0), r(0.0, 0.0, 0.0, 0.0)).is_none());
    }

    fn shot(natural: Option<(f64, f64)>, origin: Option<Rect>) -> Shot {
        Shot {
            src: "/api/images/x.png".into(),
            alt: "Image".into(),
            caption: None,
            origin,
            natural,
        }
    }

    #[test]
    fn the_resting_box_is_the_pictures_own_shape_capped_at_twice_its_pixels() {
        let style = rest_style(&shot(Some((1024.0, 768.0)), None));
        assert!(style.contains("--lb-ar:1.3333"), "{style}");
        assert!(style.contains("--lb-cap:2048px"), "{style}");
    }

    #[test]
    fn an_unmeasured_picture_borrows_the_shape_of_its_thumbnail() {
        // Natural size unknown — but the thumbnail on screen has one, and it
        // is the same picture. Without this the box would be square and the
        // entrance would start from the wrong shape.
        let style = rest_style(&shot(None, Some(r(0.0, 0.0, 300.0, 150.0))));
        assert!(style.contains("--lb-ar:2.0000"), "{style}");
        assert!(style.contains("--lb-cap:100%"), "{style}");
    }

    #[test]
    fn a_measured_entrance_does_not_fade_and_an_unmeasured_one_does() {
        // The FLIP is one object moving: fading it would say "two pictures".
        assert!(enter_style(Enter::From(-260.0, 500.0, 0.25)).contains("--lb-o:1"));
        assert!(enter_style(Enter::From(-260.0, 500.0, 0.25)).contains("--lb-dx:-260.0px"));
        // With nowhere to travel from, the fade is the whole entrance.
        assert!(enter_style(Enter::Swell).contains("--lb-o:0"));
        // Nothing at all until measured, so no stale transform can paint.
        assert!(enter_style(Enter::Pending).is_empty());
    }
}
