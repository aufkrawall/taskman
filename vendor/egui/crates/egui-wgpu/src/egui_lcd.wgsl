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
// * On both gamma (`Unorm`) and linear (sRGB-converting) framebuffers, fixed-function
//   dual-source blending cannot read `dst`, so `calibrated_lcd_blend_weights` solves for
//   the per-channel blend weight that lands on the software renderer's `TextGamma` result
//   `(s^g * a + d^g * (1 - a))^(1/g)` against `r_locals.subpixel_bg_luma`. Text whose
//   polarity opposes the theme (white toast text in a light theme) is solved against
//   `r_locals.subpixel_inverse_bg_luma` instead, or against the pure black/white endpoint
//   when that is unknown; with `subpixel_bg_luma < 0` the foreground-luminance polarity
//   compensation is used. On sRGB targets, the target color and endpoints are converted to
//   linear sRGB before solving linear blend weights. The weight is exact only on the
//   assumed background; on others (heat cells, selection) it is the chord through that
//   background and the text colour, a few LSB off the true curve.
// * LCD sub-pixel edges intentionally skip dithering so `Unorm`, `sRGB`, and CPU output
//   stay sharp and deterministic without spatial edge sparkle.

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
    inverse_bg_luma: f32,
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
        // Polarity splits exactly like the grayscale curve's. Text whose polarity matches
        // the theme sits on the page; the rest on the inverse surface, or on the pure
        // black/white endpoint when that is unknown.
        let light_text = lum > polarity_split(bg, inverse_bg_luma);
        let surface = select(inverse_bg_luma, bg, light_text == (bg < 0.5));
        let endpoint = select(vec3<f32>(1.0), vec3<f32>(0.0), light_text);
        d = endpoint;
        if surface >= 0.0 {
            // Calibrate only against a surface the text actually contrasts with, so the
            // solve never divides near `s == d`.
            // Parenthesized: WGSL would read `a < b, c > d` as a template list.
            let on_side = select((lum < surface), (lum > surface), light_text);
            let side_weight = select(0.0, smoothstep(0.04, 0.16, abs(lum - surface)), on_side);
            let channel_trust = smoothstep(
                vec3<f32>(0.02),
                vec3<f32>(0.12),
                abs(s - vec3<f32>(surface)),
            );
            d = mix(endpoint, vec3<f32>(surface), vec3<f32>(side_weight) * channel_trust);
        }

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
            r_locals.subpixel_inverse_bg_luma,
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
            let gamma_rgb = straight_rgb * channel_coverage;
            color = vec4<f32>(gamma_rgb, mean_alpha);
            coverage = vec4<f32>(channel_coverage, mean_alpha);
        }
    } else {
        // Color glyphs, grayscale glyphs, and WHITE_UV untextured geometry merged into
        // the same mesh use the ordinary premultiplied blend, with the render-time grayscale
        // curve applied to white coverage exactly as `fs_main_*` does.
        color = in.color * gray_text_texel(sampled, in.color);
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
