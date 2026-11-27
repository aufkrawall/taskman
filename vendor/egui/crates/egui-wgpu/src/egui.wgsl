// Vertex shader bindings

struct VertexOutput {
    @location(0) tex_coord: vec2<f32>,
    @location(1) color: vec4<f32>, // gamma 0-1
    @builtin(position) position: vec4<f32>,
};

struct Locals {
    screen_size: vec2<f32>,

    /// 1 if dithering is enabled, 0 otherwise
    dithering: u32,

    /// 1 to do manual filtering for more predictable kittest snapshot images.
    /// See also https://github.com/emilk/egui/issues/5295
    predictable_texture_filtering: u32,

    /// 0 = off, 1 = RGB stripes, 2 = BGR stripes. Used only by egui_lcd.wgsl.
    subpixel_order: u32,

    /// Enhanced-contrast coverage weight for LCD text.
    subpixel_contrast: f32,

    /// Blend-space gamma for LCD text weight compensation on non-sRGB targets.
    subpixel_gamma: f32,

    /// Expected sRGB background luminance in 0..=1 (`TextOptions::text_background_luma`), or
    /// < 0.0 when unknown. Calibrates the fixed-function LCD blend and decides grayscale
    /// text polarity.
    subpixel_bg_luma: f32,

    /// TASKMAN-FORK: render-time grayscale glyph exponents for raw-coverage atlases
    /// (`FontColorTransferFunction::PolarityGamma`); 0 when the atlas holds final alpha.
    gray_text_gamma_light: f32,
    gray_text_gamma_dark: f32,

    /// 1 while the bound texture is the managed font atlas (the font-atlas uniform bind
    /// group), 0 for every other texture.
    font_atlas: u32,

    /// Expected luminance under opposite-polarity text (`TextOptions::
    /// text_inverse_background_luma`), or < 0.0 when unknown. Moves the polarity split and
    /// calibrates the LCD blend for that text.
    subpixel_inverse_bg_luma: f32,
};
@group(0) @binding(0) var<uniform> r_locals: Locals;


// -----------------------------------------------
// Adapted from
// https://www.shadertoy.com/view/llVGzG
// Originally presented in:
// Jimenez 2014, "Next Generation Post-Processing in Call of Duty"
//
// A good overview can be found in
// https://blog.demofox.org/2022/01/01/interleaved-gradient-noise-a-different-kind-of-low-discrepancy-sequence/
// via https://github.com/rerun-io/rerun/
fn interleaved_gradient_noise(n: vec2<f32>) -> f32 {
    let f = 0.06711056 * n.x + 0.00583715 * n.y;
    return fract(52.9829189 * fract(f));
}

fn dither_interleaved(rgb: vec3<f32>, levels: f32, frag_coord: vec4<f32>) -> vec3<f32> {
    var noise = interleaved_gradient_noise(frag_coord.xy);
    // scale down the noise slightly to ensure flat colors aren't getting dithered
    noise = (noise - 0.5) * 0.95;
    return rgb + noise / (levels - 1.0);
}

// 0-1 linear  from  0-1 sRGB gamma
fn linear_from_gamma_rgb(srgb: vec3<f32>) -> vec3<f32> {
    let cutoff = srgb < vec3<f32>(0.04045);
    let lower = srgb / vec3<f32>(12.92);
    let higher = pow((srgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return select(higher, lower, cutoff);
}

// 0-1 sRGB gamma  from  0-1 linear
fn gamma_from_linear_rgb(rgb: vec3<f32>) -> vec3<f32> {
    let cutoff = rgb < vec3<f32>(0.0031308);
    let lower = rgb * vec3<f32>(12.92);
    let higher = vec3<f32>(1.055) * pow(rgb, vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(higher, lower, cutoff);
}

// 0-1 sRGBA gamma  from  0-1 linear
fn gamma_from_linear_rgba(linear_rgba: vec4<f32>) -> vec4<f32> {
    return vec4<f32>(gamma_from_linear_rgb(linear_rgba.rgb), linear_rgba.a);
}

// [u8; 4] SRGB as u32 -> [r, g, b, a] in 0.-1
fn unpack_color(color: u32) -> vec4<f32> {
    return vec4<f32>(
        f32(color & 255u),
        f32((color >> 8u) & 255u),
        f32((color >> 16u) & 255u),
        f32((color >> 24u) & 255u),
    ) / 255.0;
}

fn position_from_screen(screen_pos: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(
        2.0 * screen_pos.x / r_locals.screen_size.x - 1.0,
        1.0 - 2.0 * screen_pos.y / r_locals.screen_size.y,
        0.0,
        1.0,
    );
}

@vertex
fn vs_main(
    @location(0) a_pos: vec2<f32>,
    @location(1) a_tex_coord: vec2<f32>,
    @location(2) a_color: u32,
) -> VertexOutput {
    var out: VertexOutput;
    out.tex_coord = a_tex_coord;
    out.color = unpack_color(a_color);
    out.position = position_from_screen(a_pos);
    return out;
}

// Fragment shader bindings

@group(1) @binding(0) var r_tex_color: texture_2d<f32>;
@group(1) @binding(1) var r_tex_sampler: sampler;

fn sample_texture(in: VertexOutput) -> vec4<f32> {
    if r_locals.predictable_texture_filtering == 0 {
        // Hardware filtering: fast, but varies across GPUs and drivers.
        return textureSample(r_tex_color, r_tex_sampler, in.tex_coord);
    } else {
        // Manual bilinear filtering with four taps at pixel centers using textureLoad
        let texture_size = vec2<i32>(textureDimensions(r_tex_color, 0));
        let texture_size_f = vec2<f32>(texture_size);
        let pixel_coord = in.tex_coord * texture_size_f - 0.5;
        let pixel_fract = fract(pixel_coord);
        let pixel_floor = vec2<i32>(floor(pixel_coord));

        // Manual texture clamping
        let max_coord = texture_size - vec2<i32>(1, 1);
        let p00 = clamp(pixel_floor + vec2<i32>(0, 0), vec2<i32>(0, 0), max_coord);
        let p10 = clamp(pixel_floor + vec2<i32>(1, 0), vec2<i32>(0, 0), max_coord);
        let p01 = clamp(pixel_floor + vec2<i32>(0, 1), vec2<i32>(0, 0), max_coord);
        let p11 = clamp(pixel_floor + vec2<i32>(1, 1), vec2<i32>(0, 0), max_coord);

        // Load at pixel centers
        let tl = textureLoad(r_tex_color, p00, 0);
        let tr = textureLoad(r_tex_color, p10, 0);
        let bl = textureLoad(r_tex_color, p01, 0);
        let br = textureLoad(r_tex_color, p11, 0);

        // Manual bilinear interpolation
        let top = mix(tl, tr, pixel_fract.x);
        let bottom = mix(bl, br, pixel_fract.x);
        return mix(top, bottom, pixel_fract.y);
    }
}

// TASKMAN-FORK: render-time grayscale glyph weight for raw-coverage atlases
// (`FontColorTransferFunction::PolarityGamma`). Premultiplied white coverage `(c, c, c, c)`
// from the managed font atlas becomes `(a, a, a, a)` with `a = c^e`: `e` is the light-text
// exponent for text brighter than the expected background and the dark-text exponent
// otherwise (`epaint::text_is_light`). `egui_software`'s `gray` module applies the same rule
// through two LUTs; anything that is not white coverage passes through untouched.
// TASKMAN-FORK: the text luma above which text counts as light-on-dark
// (`epaint::polarity_split`): the midpoint of the page and inverse surface when both are
// known, the page alone when only it is, 0.5 when neither is.
fn polarity_split(bg_luma: f32, inverse_bg_luma: f32) -> f32 {
    if bg_luma < 0.0 {
        return 0.5;
    }
    if inverse_bg_luma < 0.0 {
        return min(bg_luma, 1.0);
    }
    return 0.5 * (min(bg_luma, 1.0) + min(inverse_bg_luma, 1.0));
}

fn gray_text_texel(tex: vec4<f32>, color: vec4<f32>) -> vec4<f32> {
    if r_locals.font_atlas == 0u || r_locals.gray_text_gamma_light <= 0.0 {
        return tex;
    }
    let c = tex.a;
    if c <= 0.0 || c >= 1.0 || any(abs(tex.rgb - vec3<f32>(c)) > vec3<f32>(0.5 / 255.0)) {
        return tex;
    }
    let straight = select(
        vec3<f32>(0.0),
        clamp(color.rgb / max(color.a, 1e-5), vec3<f32>(0.0), vec3<f32>(1.0)),
        color.a > 0.0,
    );
    let luma = dot(straight, vec3<f32>(0.299, 0.587, 0.114));
    let split = polarity_split(r_locals.subpixel_bg_luma, r_locals.subpixel_inverse_bg_luma);
    let e = select(
        r_locals.gray_text_gamma_dark,
        r_locals.gray_text_gamma_light,
        luma > split,
    );
    return vec4<f32>(pow(c, e));
}

@fragment
fn fs_main_linear_framebuffer(in: VertexOutput) -> @location(0) vec4<f32> {
    // We expect "normal" textures that are NOT sRGB-aware.
    let tex_gamma = gray_text_texel(sample_texture(in), in.color);
    var out_color_gamma = in.color * tex_gamma;
    // Dither the float color down to eight bits to reduce banding.
    // This step is optional for egui backends.
    // Note that dithering is performed on the gamma encoded values,
    // because this function is used together with a srgb converting target.
    if r_locals.dithering == 1 {
        let out_color_gamma_rgb = dither_interleaved(out_color_gamma.rgb, 256.0, in.position);
        out_color_gamma = vec4<f32>(out_color_gamma_rgb, out_color_gamma.a);
    }
    let out_color_linear = linear_from_gamma_rgb(out_color_gamma.rgb);
    return vec4<f32>(out_color_linear, out_color_gamma.a);
}

@fragment
fn fs_main_gamma_framebuffer(in: VertexOutput) -> @location(0) vec4<f32> {
    // We expect "normal" textures that are NOT sRGB-aware.
    let tex_gamma = gray_text_texel(sample_texture(in), in.color);
    var out_color_gamma = in.color * tex_gamma;
    // Dither the float color down to eight bits to reduce banding.
    // This step is optional for egui backends.
    if r_locals.dithering == 1 {
        let out_color_gamma_rgb = dither_interleaved(out_color_gamma.rgb, 256.0, in.position);
        out_color_gamma = vec4<f32>(out_color_gamma_rgb, out_color_gamma.a);
    }
    return out_color_gamma;
}
