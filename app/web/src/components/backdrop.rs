//! The sign-in backdrop, on the GPU.
//!
//! A single fullscreen quad post-processing the guardian portrait with the
//! shader in `static/backdrop.frag`. Depth parallax, per-channel chromatic
//! aberration, hashed glitch shear, current flowing along the artwork's own
//! circuitry — none of which CSS can express, because every one of them is a
//! per-pixel resample rather than a blend of two layers.
//!
//! **Raw WebGL2, not a renderer.** The scene is two triangles; a scene-graph
//! library would be several hundred kilobytes to draw them, plus a JS shim to
//! keep in step with the Rust that drives it. `web-sys` has the GL bindings, so
//! this stays one language.
//!
//! It is strictly an enhancement. Everything degrades to the CSS layers in
//! app.css §18.3, which stay underneath and are what actually ships the
//! experience when any of the following is true:
//!
//! * WebGL2 is unavailable or the context is lost;
//! * the shader fails to compile (a driver quirk must not blank the sign-in);
//! * the artwork has not loaded yet, or fails to;
//! * the viewer prefers reduced motion.
//!
//! In all of those cases this component renders nothing at all, rather than an
//! empty black canvas over the artwork.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::{
    HtmlCanvasElement, HtmlImageElement, WebGl2RenderingContext as Gl, WebGlProgram, WebGlTexture,
};
use yew::prelude::*;

/// The artwork the shader samples. Resolved through [`crate::asset`] against
/// the skin in effect, because the CSS layer underneath this canvas paints
/// `--img-skynet-hero` — and the two showing different pictures is exactly the
/// seam a skin swap would otherwise open, visible for the one frame before the
/// shader fades in over the CSS.
const PHOTO_STEM: &str = "skynet-hero";

const VERT: &str = r#"#version 300 es
// A fullscreen triangle pair from gl_VertexID — no vertex buffer, no VAO
// bookkeeping, and nothing to leak. The positions are the four corners of clip
// space; `v_uv` is the same in 0..1 with y flipped, because GL samples from the
// bottom and an image decodes from the top.
out vec2 v_uv;
void main() {
    vec2 p = vec2((gl_VertexID << 1) & 2, gl_VertexID & 2) - 1.0;
    v_uv = vec2(p.x * 0.5 + 0.5, 1.0 - (p.y * 0.5 + 0.5));
    gl_Position = vec4(p, 0.0, 1.0);
}
"#;

const FRAG: &str = include_str!("../../static/backdrop.frag");

/// Everything the render loop needs, kept together so one `Rc` carries it.
struct Scene {
    gl: Gl,
    program: WebGlProgram,
    canvas: HtmlCanvasElement,
    /// Smoothed pointer position in -1..1. Raw pointer values make the parallax
    /// snap; this is eased toward the target every frame.
    pointer: RefCell<(f32, f32)>,
    target: Rc<RefCell<(f32, f32)>>,
    start: f64,
}

#[derive(Properties, PartialEq)]
pub struct GlBackdropProps {
    /// The pinned layout, straight from `login.rs`.
    ///
    /// Not read for its value — it is the effect's dependency. The shader
    /// declines to start when the canvas has no layout box (the banner layouts
    /// hide it), and that decision has to be revisited when the layout changes:
    /// without this, switching to vertical and back left the canvas visible,
    /// sized, and permanently dark until a reload, because a mount-only effect
    /// never got the chance to try again.
    #[prop_or_default]
    pub layout: AttrValue,
}

#[function_component(GlBackdrop)]
pub fn gl_backdrop(p: &GlBackdropProps) -> Html {
    let canvas_ref = use_node_ref();
    // Set once the shader is live. Until then nothing is drawn, so the CSS
    // artwork underneath is what the person sees — including forever, if this
    // machine cannot run it.
    let live = use_state(|| false);
    let skin = crate::state::use_store().skin;

    {
        let canvas_ref = canvas_ref.clone();
        let live = live.clone();
        // The skin joins `layout` as a dependency for the same reason it is
        // one: a texture is uploaded once, at start, so changing which picture
        // the shader samples means tearing the context down and building it
        // again. Without the skin here, switching skins on the sign-in screen
        // leaves the previous artwork on the canvas until a reload.
        let photo = crate::asset::img(skin, PHOTO_STEM);
        use_effect_with((p.layout.clone(), skin), move |_| {
            // Reset on every re-run: the previous attempt may have bailed, and a
            // stale `true` would leave the canvas faded in over nothing.
            live.set(false);
            let cleanup: Rc<RefCell<Option<Cleanup>>> = Rc::new(RefCell::new(None));

            if !reduced_motion() {
                if let Some(canvas) = canvas_ref.cast::<HtmlCanvasElement>() {
                    *cleanup.borrow_mut() = start(canvas, live.clone(), &photo);
                }
            }

            let cleanup = cleanup.clone();
            move || {
                if let Some(c) = cleanup.borrow_mut().take() {
                    c.run();
                }
            }
        });
    }

    // `data-live` is what fades the canvas in over the CSS layer once the first
    // frame has actually been drawn, so a slow texture decode cannot show as a
    // black rectangle where the portrait was.
    html! {
        <canvas
            ref={canvas_ref}
            class="fn-login__gl"
            data-live={live.to_string()}
            aria-hidden="true"
        />
    }
}

/// Handles that must be released when the component goes away: the frame
/// callback, the resize and pointer listeners, and the GL objects.
struct Cleanup {
    cancel: Box<dyn FnOnce()>,
}

impl Cleanup {
    fn run(self) {
        (self.cancel)();
    }
}

fn reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| {
            w.match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|m| m.matches())
}

/// Bring the shader up. `None` on any failure, which leaves the CSS backdrop as
/// the whole experience — deliberately silent, because a driver that cannot
/// compile a decorative shader is not something to tell someone signing in.
fn start(canvas: HtmlCanvasElement, live: UseStateHandle<bool>, photo: &str) -> Option<Cleanup> {
    // No layout box means the layer is switched off for this layout (app.css
    // hides it in the two banner layouts, where the CSS backdrop is the tuned
    // one). Bailing here turns the render loop off rather than leaving it
    // drawing frames nobody composites.
    if canvas.client_width() == 0 || canvas.client_height() == 0 {
        return None;
    }

    let gl = canvas
        .get_context("webgl2")
        .ok()
        .flatten()?
        .dyn_into::<Gl>()
        .ok()?;

    let program = build_program(&gl)?;

    // Pointer target lives outside the scene so the listener can write it while
    // the frame loop reads it.
    let target = Rc::new(RefCell::new((0.0f32, 0.0f32)));

    let scene = Rc::new(Scene {
        gl,
        program,
        canvas: canvas.clone(),
        pointer: RefCell::new((0.0, 0.0)),
        target: target.clone(),
        start: now(),
    });

    // The texture arrives late. Until it does the loop still runs, drawing the
    // shader against a 1×1 placeholder — which is why the canvas stays hidden
    // until `live` flips.
    let texture = scene.gl.create_texture()?;
    bind_placeholder(&scene.gl, &texture);
    let texture = Rc::new(texture);
    let dims = Rc::new(RefCell::new((1.0f32, 1.0f32)));
    load_photo(
        scene.gl.clone(),
        texture.clone(),
        dims.clone(),
        live.clone(),
        photo,
    );

    // --- pointer ---------------------------------------------------------
    // On the window, not the canvas: the canvas sits behind the form, so it
    // never receives a pointer event of its own.
    let win = web_sys::window()?;
    let move_cb = {
        let target = target.clone();
        Closure::<dyn FnMut(web_sys::PointerEvent)>::new(move |e: web_sys::PointerEvent| {
            if let Some(w) = web_sys::window() {
                let iw = w.inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1.0);
                let ih = w
                    .inner_height()
                    .ok()
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1.0);
                *target.borrow_mut() = (
                    ((e.client_x() as f64 / iw) * 2.0 - 1.0) as f32,
                    ((e.client_y() as f64 / ih) * 2.0 - 1.0) as f32,
                );
            }
        })
    };
    let _ = win.add_event_listener_with_callback("pointermove", move_cb.as_ref().unchecked_ref());

    // --- frame loop ------------------------------------------------------
    let raf: Rc<RefCell<Option<i32>>> = Rc::new(RefCell::new(None));
    let stopped = Rc::new(RefCell::new(false));
    // The frame callback has to reschedule *itself*, so two handles to the same
    // cell are needed: `inner` is moved into the closure and reads it, `outer`
    // stays here to make the first call and to drop it at teardown. One handle
    // cannot do both — it would be borrowed after being moved.
    type FrameHook = Rc<RefCell<Option<Closure<dyn FnMut()>>>>;
    let inner: FrameHook = Rc::new(RefCell::new(None));
    let outer = inner.clone();
    {
        let scene = scene.clone();
        let texture = texture.clone();
        let dims = dims.clone();
        let raf = raf.clone();
        let stopped = stopped.clone();
        *outer.borrow_mut() = Some(Closure::<dyn FnMut()>::new(move || {
            if *stopped.borrow() {
                return;
            }
            let (w, h) = *dims.borrow();
            draw(&scene, &texture, w, h);
            if let (Some(win), Some(cb)) = (web_sys::window(), inner.borrow().as_ref()) {
                *raf.borrow_mut() = win
                    .request_animation_frame(cb.as_ref().unchecked_ref())
                    .ok();
            }
        }));
    }
    if let Some(cb) = outer.borrow().as_ref() {
        *raf.borrow_mut() = win
            .request_animation_frame(cb.as_ref().unchecked_ref())
            .ok();
    }

    let cancel = {
        let scene = scene.clone();
        let texture = texture.clone();
        Box::new(move || {
            *stopped.borrow_mut() = true;
            if let (Some(win), Some(id)) = (web_sys::window(), *raf.borrow()) {
                win.cancel_animation_frame(id).ok();
                let _ = win.remove_event_listener_with_callback(
                    "pointermove",
                    move_cb.as_ref().unchecked_ref(),
                );
            }
            // Drop the closures explicitly; a forgotten `Closure` is a leak
            // that outlives the component.
            drop(move_cb);
            outer.borrow_mut().take();
            scene.gl.delete_texture(Some(&texture));
            scene.gl.delete_program(Some(&scene.program));
        }) as Box<dyn FnOnce()>
    };

    Some(Cleanup { cancel })
}

/// Milliseconds, monotonic enough for an animation clock. `js_sys::Date` rather
/// than `Performance` so no extra web-sys feature is needed, and the codebase
/// already reads the clock this way in the composer's typing throttle.
fn now() -> f64 {
    js_sys::Date::now()
}

fn build_program(gl: &Gl) -> Option<WebGlProgram> {
    let vs = compile(gl, Gl::VERTEX_SHADER, VERT)?;
    let fs = compile(gl, Gl::FRAGMENT_SHADER, FRAG)?;
    let program = gl.create_program()?;
    gl.attach_shader(&program, &vs);
    gl.attach_shader(&program, &fs);
    gl.link_program(&program);
    // Shaders are attached to the program; once linked the objects themselves
    // are no longer needed and would otherwise sit in driver memory.
    gl.delete_shader(Some(&vs));
    gl.delete_shader(Some(&fs));

    if gl
        .get_program_parameter(&program, Gl::LINK_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Some(program)
    } else {
        gl.delete_program(Some(&program));
        None
    }
}

fn compile(gl: &Gl, kind: u32, src: &str) -> Option<web_sys::WebGlShader> {
    let shader = gl.create_shader(kind)?;
    gl.shader_source(&shader, src);
    gl.compile_shader(&shader);
    if gl
        .get_shader_parameter(&shader, Gl::COMPILE_STATUS)
        .as_bool()
        .unwrap_or(false)
    {
        Some(shader)
    } else {
        // Logged, not surfaced: this is a decorative layer, and the CSS
        // backdrop is already behind it. The log is for whoever is porting to
        // a driver that rejected something.
        if let Some(log) = gl.get_shader_info_log(&shader) {
            web_sys::console::warn_1(&JsValue::from_str(&format!("backdrop shader: {log}")));
        }
        gl.delete_shader(Some(&shader));
        None
    }
}

/// A 1×1 near-black pixel, so the first frames have something to sample and the
/// shader does not have to branch on "texture not ready".
fn bind_placeholder(gl: &Gl, texture: &WebGlTexture) {
    gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
    let _ = gl.tex_image_2d_with_i32_and_i32_and_i32_and_format_and_type_and_opt_u8_array(
        Gl::TEXTURE_2D,
        0,
        Gl::RGBA as i32,
        1,
        1,
        0,
        Gl::RGBA,
        Gl::UNSIGNED_BYTE,
        Some(&[6, 10, 16, 255]),
    );
    set_sampling(gl);
}

/// Clamp and linear-filter, no mipmaps: the quad is drawn at roughly 1:1 and a
/// mip chain on a 1200px photo is memory spent to blur nothing.
fn set_sampling(gl: &Gl) {
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_S, Gl::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_WRAP_T, Gl::CLAMP_TO_EDGE as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MIN_FILTER, Gl::LINEAR as i32);
    gl.tex_parameteri(Gl::TEXTURE_2D, Gl::TEXTURE_MAG_FILTER, Gl::LINEAR as i32);
}

fn load_photo(
    gl: Gl,
    texture: Rc<WebGlTexture>,
    dims: Rc<RefCell<(f32, f32)>>,
    live: UseStateHandle<bool>,
    photo: &str,
) {
    let Ok(img) = HtmlImageElement::new() else {
        return;
    };
    let img = Rc::new(img);
    let onload = {
        let img = img.clone();
        let gl = gl.clone();
        Closure::<dyn FnMut()>::new(move || {
            gl.bind_texture(Gl::TEXTURE_2D, Some(&texture));
            if gl
                .tex_image_2d_with_u32_and_u32_and_html_image_element(
                    Gl::TEXTURE_2D,
                    0,
                    Gl::RGBA as i32,
                    Gl::RGBA,
                    Gl::UNSIGNED_BYTE,
                    &img,
                )
                .is_err()
            {
                return;
            }
            set_sampling(&gl);
            *dims.borrow_mut() = (img.natural_width() as f32, img.natural_height() as f32);
            // Only now is there a real picture to show.
            live.set(true);
        })
    };
    img.set_onload(Some(onload.as_ref().unchecked_ref()));
    // Deliberately leaked: the image fires once and the closure must outlive
    // this call. One closure for the life of the page, not per frame.
    onload.forget();
    img.set_src(photo);
}

fn draw(scene: &Scene, texture: &WebGlTexture, photo_w: f32, photo_h: f32) {
    let gl = &scene.gl;

    // Resize to the backing store, capped at 2× so a 5K display does not render
    // eight megapixels of decoration every frame.
    let dpr = web_sys::window()
        .map(|w| w.device_pixel_ratio())
        .unwrap_or(1.0)
        .min(2.0);
    let css_w = scene.canvas.client_width() as f64;
    let css_h = scene.canvas.client_height() as f64;
    let w = (css_w * dpr).round().max(1.0) as u32;
    let h = (css_h * dpr).round().max(1.0) as u32;
    if scene.canvas.width() != w || scene.canvas.height() != h {
        scene.canvas.set_width(w);
        scene.canvas.set_height(h);
    }
    gl.viewport(0, 0, w as i32, h as i32);

    // Ease the pointer toward its target. 0.06 per frame is ~250ms to settle at
    // 60fps: the parallax follows the hand without snapping to it.
    {
        let target = *scene.target.borrow();
        let mut p = scene.pointer.borrow_mut();
        p.0 += (target.0 - p.0) * 0.06;
        p.1 += (target.1 - p.1) * 0.06;
    }
    let (px, py) = *scene.pointer.borrow();

    gl.use_program(Some(&scene.program));

    let u = |name: &str| gl.get_uniform_location(&scene.program, name);
    gl.active_texture(Gl::TEXTURE0);
    gl.bind_texture(Gl::TEXTURE_2D, Some(texture));
    gl.uniform1i(u("u_photo").as_ref(), 0);
    gl.uniform2f(u("u_res").as_ref(), w as f32, h as f32);
    gl.uniform2f(u("u_photoRes").as_ref(), photo_w, photo_h);
    gl.uniform1f(
        u("u_time").as_ref(),
        ((now() - scene.start) / 1000.0) as f32,
    );
    gl.uniform2f(u("u_pointer").as_ref(), px, py);
    // Matches the CSS crop bias that keeps the head in frame rather than the
    // shoulder (app.css §12, `background-position`).
    gl.uniform1f(u("u_focus").as_ref(), 0.64);

    gl.draw_arrays(Gl::TRIANGLE_STRIP, 0, 4);
}

#[cfg(test)]
mod tests {
    /// The shader is compiled by the GPU, not by rustc, so the only thing a host
    /// test can check is that it is present and self-consistent — which is worth
    /// checking, because `include_str!` of a missing file is a build error but a
    /// *renamed uniform* is a silent black screen.
    #[test]
    fn the_shader_declares_every_uniform_the_renderer_sets() {
        let frag = super::FRAG;
        for uniform in [
            "u_photo",
            "u_res",
            "u_photoRes",
            "u_time",
            "u_pointer",
            "u_focus",
        ] {
            assert!(
                frag.contains(uniform),
                "{uniform} is set by draw() but not declared in the shader"
            );
        }
    }

    #[test]
    fn the_shader_and_the_vertex_stage_agree_on_the_varying() {
        assert!(super::VERT.contains("out vec2 v_uv"));
        assert!(super::FRAG.contains("in vec2 v_uv"));
    }

    #[test]
    fn both_stages_declare_an_es_300_version_on_the_first_line() {
        // `#version` must be the very first token or the compile fails with a
        // message that says nothing useful.
        for src in [super::VERT, super::FRAG] {
            assert_eq!(src.lines().next(), Some("#version 300 es"));
        }
    }

    #[test]
    fn every_effect_is_gated_on_the_ui_mask() {
        // The form's side of the frame must stay untouched: the scrim behind the
        // text is what buys its contrast. Any effect that forgets `* ui` spends
        // it, so the mask is expected to appear once per effect.
        let frag = super::FRAG;
        assert!(
            frag.matches("ui").count() >= 10,
            "effects should be masked away from the form"
        );
        assert!(frag.contains("float ui = smoothstep"));
    }
}
