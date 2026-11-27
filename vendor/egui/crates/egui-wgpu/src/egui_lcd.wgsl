// TASKMAN-FORK: sub-pixel (LCD) glyph coverage for the managed font atlas only.
//
// This source is appended to `egui.wgsl` after `enable dual_source_blending;`. A
// sub-pixel atlas texel stores one coverage per color channel and alpha zero as an
// explicit tag. Neutral, tinted and full LCD coverage all carry that tag. Zero coverage
// is (0,0,0,0), while premultiplied color/emoji glyphs retain non-zero alpha.
//
// The fixed-function blend uses `ONE / ONE_MINUS_SRC1`. Fragment output 0 is the
// premultiplied source color; fragment output 1 is the per-channel coverage. That yields
//
//   dst = text_color * coverage + dst * (1 - coverage)
//
// independently for R, G and B.
//
// * On linear (sRGB-converting) framebuffers, hardware already converts `dst` to linear
//   and encodes the blended output back to sRGB, so the shader converts the straight text
//   color to linear *before* multiplying by per-channel coverage (conserving energy).
// * On gamma (non-sRGB `Unorm`) framebuffers, fixed-function blending cannot read `dst`,
//   so the shader applies a foreground-luminance gamma compensation curve to coverage:
//   exact `a'^(1/g)` for white-on-dark, exact `1 - (1 - a')^(1/g)` for black-on-light,
//   interpolated by text luminance in between. This matches the CPU renderer's `TextGamma`
//   weight closely in both dark and light themes without framebuffer reads.

fn is_lcd_texel(tex: vec4<f32>) -> bool {
    return tex.a <= 0.5 / 255.0
        && max(tex.r, max(tex.g, tex.b)) > 0.5 / 255.0;
}

fn enhance_lcd_contrast(coverage: vec3<f32>) -> vec3<f32> {
    return clamp(
        coverage * (1.0 + r_locals.subpixel_contrast * (1.0 - coverage)),
        vec3<f32>(0.0),
        vec3<f32>(1.0),
    );
}

fn gamma_adjust_lcd_coverage(coverage: vec3<f32>, straight_rgb: vec3<f32>, gamma: f32) -> vec3<f32> {
    let g = clamp(gamma, 1.0, 3.0);
    if g <= 1.0001 {
        return coverage;
    }
    let inv_g = 1.0 / g;
    let c = clamp(coverage, vec3<f32>(0.0), vec3<f32>(1.0));
    let lum = clamp(dot(straight_rgb, vec3<f32>(0.299, 0.587, 0.114)), 0.0, 1.0);
    let c_dark_bg = pow(c, vec3<f32>(inv_g));
    let c_light_bg = vec3<f32>(1.0) - pow(vec3<f32>(1.0) - c, vec3<f32>(inv_g));
    return mix(c_light_bg, c_dark_bg, lum);
}

fn calibrated_lcd_blend_weights(
    alpha: vec3<f32>,
    straight_rgb: vec3<f32>,
    gamma: f32,
    bg_luma: f32,
    linear_framebuffer: bool,
) -> vec3<f32> {
    let s = clamp(straight_rgb, vec3<f32>(0.0), vec3<f32>(1.0));
    let a = clamp(alpha, vec3<f32>(0.0), vec3<f32>(1.0));
    let g = clamp(gamma, 1.0, 3.0);
    let lum = clamp(dot(s, vec3<f32>(0.299, 0.587, 0.114)), 0.0, 1.0);

    var target_gamma: vec3<f32>;
    var d: vec3<f32>;
    if bg_luma >= 0.0 {
        let bg = clamp(bg_luma, 0.0, 1.0);
        // Only calibrate toward the theme background when foreground contrast follows the
        // theme polarity; e.g. dark text on a bright accent button in dark mode falls back
        // to the bright endpoint instead of dividing near `bg`.
        let pol = select(vec3<f32>(1.0), vec3<f32>(0.0), lum >= bg);
        let same_side = (bg < 0.5 && lum > bg) || (bg >= 0.5 && lum < bg);
        let side_weight = select(0.0, smoothstep(0.04, 0.16, abs(lum - bg)), same_side);
        let channel_trust = smoothstep(
            vec3<f32>(0.02),
            vec3<f32>(0.12),
            abs(s - vec3<f32>(bg)),
        );
        d = mix(pol, vec3<f32>(bg), vec3<f32>(side_weight) * channel_trust);

        if g <= 1.0001 {
            target_gamma = mix(d, s, a);
        } else {
            let s_g = pow(s, vec3<f32>(g));
            let d_g = pow(d, vec3<f32>(g));
            target_gamma = pow(max(mix(d_g, s_g, a), vec3<f32>(0.0)), vec3<f32>(1.0 / g));
        }
    } else {
        let weighted = gamma_adjust_lcd_coverage(a, s, g);
        if !linear_framebuffer {
            return weighted;
        }
        d = select(vec3<f32>(1.0), vec3<f32>(0.0), lum >= 0.5);
        target_gamma = mix(d, s, weighted);
    }

    if linear_framebuffer {
        let s_lin = linear_from_gamma_rgb(s);
        let d_lin = linear_from_gamma_rgb(d);
        let t_lin = linear_from_gamma_rgb(target_gamma);
        let denom = s_lin - d_lin;
        return select(
            a,
            clamp((t_lin - d_lin) / denom, vec3<f32>(0.0), vec3<f32>(1.0)),
            abs(denom) > vec3<f32>(1e-4),
        );
    }

    let denom = s - d;
    return select(
        a,
        clamp((target_gamma - d) / denom, vec3<f32>(0.0), vec3<f32>(1.0)),
        abs(denom) > vec3<f32>(1e-4),
    );
}

// Only a one-texel-per-pixel, center-aligned mapping preserves the panel's horizontal
// stripe order. A rotated, scaled or fractionally translated glyph would smear the three
// coverages together, so it takes the grayscale fallback instead.
fn lcd_texels_on_pixels(in: VertexOutput) -> bool {
    let size = vec2<f32>(textureDimensions(r_tex_color, 0));
    let dx = dpdx(in.tex_coord) * size;
    let dy = dpdy(in.tex_coord) * size;
    let expected = abs(dx - vec2<f32>(1.0, 0.0)) + abs(dy - vec2<f32>(0.0, 1.0));
    let texel_center = abs(fract(in.tex_coord * size) - vec2<f32>(0.5));
    return expected.x + expected.y < 0.01
        && texel_center.x < 0.02
        && texel_center.y < 0.02;
}

struct LcdOutput {
    @location(0) @blend_src(0) color: vec4<f32>,
    @location(0) @blend_src(1) coverage: vec4<f32>,
};

fn lcd_output(in: VertexOutput, linear_framebuffer: bool) -> LcdOutput {
    let sampled = sample_texture(in);
    // Derivatives are evaluated before the branch so control flow stays uniform.
    let aligned = lcd_texels_on_pixels(in);
    let subpixel_texel = is_lcd_texel(sampled);

    let has_alpha = in.color.a > 0.0;
    let straight_rgb = select(
        vec3<f32>(0.0),
        clamp(in.color.rgb / max(in.color.a, 1e-5), vec3<f32>(0.0), vec3<f32>(1.0)),
        has_alpha,
    );

    var color: vec4<f32>;
    var coverage: vec4<f32>;
    if subpixel_texel {
        var rgb = sampled.rgb;
        if aligned {
            if r_locals.subpixel_order == 2u {
                rgb = rgb.bgr;
            }
        } else {
            // LCD texels sampled off the pixel grid collapse to their mean coverage so
            // rotated/scaled/fractional text cannot smear colored fringes.
            let gray = (rgb.r + rgb.g + rgb.b) / 3.0;
            rgb = vec3<f32>(gray);
        }

        let contrasted = enhance_lcd_contrast(rgb);
        let channel_coverage = calibrated_lcd_blend_weights(
            contrasted * in.color.a,
            straight_rgb,
            r_locals.subpixel_gamma,
            r_locals.subpixel_bg_luma,
            linear_framebuffer,
        );
        let mean_alpha = (channel_coverage.r + channel_coverage.g + channel_coverage.b) / 3.0;
        if linear_framebuffer {
            // Hardware blends in linear space: convert the straight text color first,
            // then premultiply by calibrated linear weights so results match TextGamma.
            let linear_rgb = linear_from_gamma_rgb(straight_rgb);
            color = vec4<f32>(linear_rgb * channel_coverage, mean_alpha);
            coverage = vec4<f32>(channel_coverage, mean_alpha);
        } else {
            var gamma_rgb = straight_rgb * channel_coverage;
            if r_locals.dithering == 1 && max(channel_coverage.r, max(channel_coverage.g, channel_coverage.b)) > 0.0 {
                gamma_rgb = dither_interleaved(gamma_rgb, 256.0, in.position);
            }
            color = vec4<f32>(gamma_rgb, mean_alpha);
            coverage = vec4<f32>(channel_coverage, mean_alpha);
        }
    } else {
        // Color glyphs, grayscale glyphs, and WHITE_UV untextured geometry merged into
        // the same mesh pass through unchanged on the ordinary premultiplied blend.
        color = in.color * sampled;
        coverage = vec4<f32>(color.a);
        if r_locals.dithering == 1 && color.a > 0.0 {
            color = vec4<f32>(
                dither_interleaved(color.rgb, 256.0, in.position),
                color.a,
            );
        }
        if linear_framebuffer {
            color = vec4<f32>(linear_from_gamma_rgb(color.rgb), color.a);
        }
    }

    var out: LcdOutput;
    out.color = color;
    out.coverage = coverage;
    return out;
}

@fragment
fn fs_lcd_linear_framebuffer(in: VertexOutput) -> LcdOutput {
    return lcd_output(in, true);
}

@fragment
fn fs_lcd_gamma_framebuffer(in: VertexOutput) -> LcdOutput {
    return lcd_output(in, false);
}
