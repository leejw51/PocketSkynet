#version 300 es
// The sign-in backdrop, as a fragment shader (components/backdrop.rs).
//
// One fullscreen quad post-processing the guardian portrait. Everything here is
// something CSS cannot express: per-pixel depth parallax, chromatic aberration
// that varies across the frame, glitch shear driven by hashed noise, and current
// flowing along the edges the photograph already contains.
//
// The rules it plays by:
//   * The form's side of the frame is left alone. `ui` fades every effect out
//     toward the card, because the scrim behind the text exists to buy contrast
//     and an effect painted over it spends that contrast.
//   * Nothing pulses at a rate that pulls the eye. This sits behind a form
//     someone is reading; it must be alive in peripheral vision and ignorable
//     in foveal vision.
//   * No effect ever brightens the whole frame — that reads as a flash, and a
//     login screen that flashes looks broken.

precision highp float;

in vec2 v_uv;
out vec4 outColor;

uniform sampler2D u_photo;
uniform vec2  u_res;      // canvas size in device pixels
uniform vec2  u_photoRes; // intrinsic texture size
uniform float u_time;     // seconds since mount
uniform vec2  u_pointer;  // -1..1, smoothed; (0,0) until the pointer moves
uniform float u_focus;    // 0..1, how far the crop is biased toward the head

// --- noise ----------------------------------------------------------------
// Hash-based, not a texture: one fewer asset to ship, and a texture lookup for
// noise costs more than the arithmetic on every GPU this will ever run on.

float hash11(float p) {
    p = fract(p * 0.1031);
    p *= p + 33.33;
    return fract(p * (p + p));
}

float hash21(vec2 p) {
    vec3 p3 = fract(vec3(p.xyx) * 0.1031);
    p3 += dot(p3, p3.yzx + 33.33);
    return fract((p3.x + p3.y) * p3.z);
}

float valueNoise(vec2 p) {
    vec2 i = floor(p);
    vec2 f = fract(p);
    // Smoothstep the interpolant so the lattice does not show as a grid.
    vec2 u = f * f * (3.0 - 2.0 * f);
    return mix(
        mix(hash21(i), hash21(i + vec2(1.0, 0.0)), u.x),
        mix(hash21(i + vec2(0.0, 1.0)), hash21(i + vec2(1.0, 1.0)), u.x),
        u.y);
}

// `cover` mapping, matching what `background-size: cover` would have done, with
// `bias` reproducing the CSS `background-position` that keeps the head in frame.
vec2 coverUv(vec2 uv, vec2 res, vec2 texRes, float bias) {
    float canvasAspect = res.x / max(res.y, 1.0);
    float texAspect = texRes.x / max(texRes.y, 1.0);
    vec2 scale = canvasAspect > texAspect
        ? vec2(1.0, texAspect / canvasAspect)   // wider than the source: crop vertically
        : vec2(canvasAspect / texAspect, 1.0);  // taller: crop horizontally
    vec2 centred = (uv - 0.5) * scale + 0.5;
    // Vertical bias only when there is vertical slack to spend.
    float slack = max(0.0, 1.0 - scale.y) * 0.5;
    return centred - vec2(0.0, slack * bias);
}

void main() {
    vec2 uv = v_uv;

    // How much this pixel is allowed to be affected. 0 over the form, 1 on the
    // far side. Squared so the ramp is gentle where the card's edge lands.
    float ui = smoothstep(0.30, 0.72, uv.x);
    ui *= ui;

    // --- depth parallax --------------------------------------------------
    // There is no depth buffer for a photograph, so luminance stands in for
    // one: the chrome half and the lit face are bright and near, the circuit
    // background is dark and far. Sampling the photo once up front to build it
    // costs one extra fetch and buys the whole illusion.
    vec2 baseUv = coverUv(uv, u_res, u_photoRes, u_focus);
    float lum = dot(texture(u_photo, baseUv).rgb, vec3(0.299, 0.587, 0.114));
    // Bias so mid-greys sit at rest and only the extremes move.
    float depth = lum - 0.42;

    // Pointer parallax plus a slow autonomous drift, so the frame is never
    // still even when nobody touches it.
    vec2 drift = vec2(sin(u_time * 0.11), cos(u_time * 0.083)) * 0.5;
    vec2 push = (u_pointer * 1.6 + drift) * 0.012 * ui;
    vec2 pUv = baseUv + push * depth;

    // --- glitch shear ----------------------------------------------------
    // Rare, brief, and horizontal. `hash11` on a quantised clock gives bursts
    // at unpredictable intervals rather than a metronome; most seconds pass
    // with no glitch at all, which is what makes the ones that land register.
    float epoch = floor(u_time * 1.6);
    float burst = step(0.90, hash11(epoch));
    float within = fract(u_time * 1.6);
    // Decay across the burst so it snaps in and falls away.
    float glitchAmt = burst * pow(1.0 - within, 3.0);
    // Coarse horizontal bands, so entire strips shear rather than pixels.
    float band = floor(uv.y * 44.0);
    float shear = (hash11(band + epoch * 7.0) - 0.5) * 0.05 * glitchAmt * ui;
    pUv.x += shear;

    // --- chromatic aberration --------------------------------------------
    // Grows toward the edge of the frame the way a real lens does, and opens up
    // during a glitch. This is the effect that most plainly cannot be done in
    // CSS: it is a per-channel resample, not a blend of two layers.
    float edge = length(uv - vec2(0.62, 0.5));
    // Halved from the first pass. At the original strength the chrome half's
    // high-contrast edges fringed prismatically and read as a rendering fault
    // rather than as a lens — the effect has to be findable, not noticeable.
    // The glitch still opens it up, because *there* it is meant to be seen.
    float ca = (0.0005 + edge * 0.0010) * ui + glitchAmt * 0.005 * ui;
    vec2 dir = normalize(uv - vec2(0.62, 0.5) + 1e-5);
    vec3 photo;
    photo.r = texture(u_photo, pUv + dir * ca).r;
    photo.g = texture(u_photo, pUv).g;
    photo.b = texture(u_photo, pUv - dir * ca).b;

    vec3 col = photo;

    // --- current in the traces -------------------------------------------
    // The source already has circuitry in it. Rather than draw new lines, find
    // where the image has cyan-ish energy in shadow and pulse *that* — the glow
    // then follows the artwork instead of sitting on top of it.
    float cyanness = clamp(photo.b - photo.r, 0.0, 1.0);
    float inShadow = 1.0 - smoothstep(0.10, 0.52, lum);
    float traces = cyanness * inShadow;
    // A wave travelling diagonally, so the pulse reads as flow with direction.
    float flow = sin((uv.x * 5.0 + uv.y * 3.0) - u_time * 1.25);
    flow = flow * 0.5 + 0.5;
    col += vec3(0.10, 0.55, 0.75) * traces * (0.25 + 0.75 * flow) * 0.42 * ui;

    // --- heat shimmer ----------------------------------------------------
    // Very low amplitude, low frequency: enough that a still frame and the next
    // are not identical, not enough to read as wobble.
    float shimmer = valueNoise(uv * 7.0 + vec2(0.0, u_time * 0.35)) - 0.5;
    col += shimmer * 0.012 * ui;

    // --- scanlines and a rolling bar -------------------------------------
    // Tied to device pixels, so the line pitch does not change with zoom.
    float lines = sin(uv.y * u_res.y * 1.35) * 0.5 + 0.5;
    col *= 1.0 - lines * 0.028 * ui;
    // One soft bar working down the frame every ~9s. Darkening, never
    // brightening — a bright bar on a face looks like a rendering fault.
    float bar = fract(uv.y * 0.85 - u_time * 0.11);
    col *= 1.0 - smoothstep(0.97, 1.0, bar) * 0.10 * ui;

    // --- optic bloom -----------------------------------------------------
    // The machine eye, breathing on an irregular clock built from two
    // incommensurable sines so it never settles into an obvious loop.
    vec2 optic = vec2(0.735, 0.455);
    // Aspect-correct the distance, or the bloom is an ellipse on wide frames.
    vec2 d = (uv - optic) * vec2(u_res.x / max(u_res.y, 1.0), 1.0);
    float glow = exp(-dot(d, d) * 26.0);
    float beat = 0.72 + 0.28 * (sin(u_time * 2.3) * 0.6 + sin(u_time * 3.7) * 0.4);
    col += vec3(0.25, 0.75, 1.0) * glow * beat * 0.55 * ui;

    // --- grade ------------------------------------------------------------
    // Vignette toward the far corners only; the form's side is already dark.
    float vig = 1.0 - smoothstep(0.42, 1.05, length((uv - vec2(0.58, 0.5)) * vec2(1.1, 1.0)));
    col *= mix(1.0, 0.72 + 0.28 * vig, ui);
    // A whisper of dither. Banding is visible on a large near-black gradient at
    // 8 bits, and this costs one hash to remove.
    col += (hash21(uv * u_res + u_time) - 0.5) / 255.0;

    outColor = vec4(col, 1.0);
}
