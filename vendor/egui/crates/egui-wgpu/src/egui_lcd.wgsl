// TASKMAN-FORK: sub-pixel (LCD) glyph coverage for the managed font atlas only.
//
// This source is appended to `egui.wgsl` after `enable dual_source_blending;`. A
// sub-pixel atlas texel stores one coverage per color channel and alpha is the minimum
// of those channels. A premultiplied color glyph never has a channel above its alpha, so
// `max(rgb) > alpha` distinguishes LCD coverage from grayscale glyphs and color/emoji
// glyphs sharing the atlas.
//
// The fixed-function blend uses `ONE / ONE_MINUS_SRC1`. Fragment output 0 is the
// premultiplied source color; fragment output 1 is the per-channel coverage. That yields
//
//   dst = text_color * coverage + dst * (1 - coverage)
//
// independently for R, G and B. This cannot reproduce the software renderer's nonlinear
// destination-gamma blend, because a fixed-function blend cannot read the destination in
// the fragment shader. We apply the requested contrast/weight curve and document the
// gamma limitation rather than pretending the result is DirectWrite-exact.

fn is_lcd_texel(tex: vec4<f32>) -> bool {
    return max(tex.r, max(tex.g, tex.b)) > tex.a + 0.5 / 255.0;
}

fn enhance_lcd_contrast(coverage: vec3<f32>) -> vec3<f32> {
    return coverage * (1.0 + r_locals.subpixel_contrast * (1.0 - coverage));
}

fn lcd_fallback_gray(tex: vec4<f32>) -> vec4<f32> {
    let gray = (tex.r + tex.g + tex.b) / 3.0;
    let weighted = enhance_lcd_contrast(vec3<f32>(gray)).x;
    return vec4<f32>(weighted);
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

    var color: vec4<f32>;
    var coverage: vec4<f32>;
    if subpixel_texel && aligned {
        var rgb = sampled.rgb;
        if r_locals.subpixel_order == 2u {
            rgb = rgb.bgr;
        }
        let weighted = enhance_lcd_contrast(rgb);
        let channel_coverage = weighted * in.color.a;
        color = vec4<f32>(
            in.color.rgb * weighted,
            in.color.a * (weighted.r + weighted.g + weighted.b) / 3.0,
        );
        coverage = vec4<f32>(channel_coverage, color.a);
    } else {
        // Color glyphs pass through unchanged. LCD texels sampled off the pixel grid are
        // drawn as their mean coverage, which is correct grayscale text and cannot smear
        // colored fringes.
        let tex = select(sampled, lcd_fallback_gray(sampled), subpixel_texel);
        color = in.color * tex;
        coverage = vec4<f32>(color.a);
    }

    if r_locals.dithering == 1 {
        color = vec4<f32>(
            dither_interleaved(color.rgb, 256.0, in.position),
            color.a,
        );
    }
    if linear_framebuffer {
        color = vec4<f32>(linear_from_gamma_rgb(color.rgb), color.a);
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
