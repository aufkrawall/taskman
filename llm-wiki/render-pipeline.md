# Render Pipeline

Last verified: 2026-09-01

Primary sources:
- `vendor/egui/TASKMAN-FORK.md` (what diverges from upstream egui, and why)
- `vendor/egui/crates/egui_software/` (the CPU rasterizer)
- `vendor/egui/crates/eframe/src/native/software_integration.rs`
- `crates/tm-platform/src/win/text_rendering.rs`

## Why there is a fork

Two requirements met in one place:

1. **Sub-pixel (ClearType) text.** Per-channel glyph coverage has to be blended per
   channel. On a GPU that needs dual-source blending, which egui's pipelines do not use
   (upstream `emilk/egui#2639`). On a CPU it is a multiply.
2. **No GPU at all** — and not a software *driver* either. The old `render_mode = software`
   was WARP: a D3D12 driver emulated on the CPU, costing ~14 cores at 2.9 fps because it
   emulates a whole graphics stack to draw a 2D UI.

Requirement 1 can only be satisfied by a renderer that satisfies requirement 2, so they
are one project. egui is vendored at `vendor/egui` (see `TASKMAN-FORK.md` for the
divergence inventory and the rebase runbook).

## The layers

```
tm-platform/win/text_rendering.rs   SPI gates + IDWriteRenderingParams
        |  plain data; no `windows` type goes further up
tm-app/theme.rs                     the ONLY place that decides the sub-pixel mode
        |  written into style.visuals.text_options
[FORK] epaint                       3x rasterization -> per-channel coverage in the atlas
[FORK] egui_software::Painter       shapes -> pixels
        |-- Shape::Text        -> glyph blit, per-channel ClearType blend
        |-- pixel-aligned Rect -> (future) span fill
        \-- everything else    -> epaint::Tessellator -> triangle rasterizer
[FORK] eframe Renderer::Software -> softbuffer -> BitBlt / SHM / wl_shm / CoreGraphics
```

## The rules that are load-bearing

**The software renderer reimplements `egui_glow`'s pipeline; it does not invent one.**
Every decision that could differ is matched deliberately: the gamma-space
`vertex * texel` multiply, premultiplied `ONE / ONE_MINUS_SRC_ALPHA` blending, the
`round()` in clip-rect conversion (copied from `set_clip_rect`), and the half-texel offset
in bilinear sampling. Where a comment says "matches the GPU", changing it changes output.

**No coverage anti-aliasing in the rasterizer.** epaint's tessellator already bakes AA into
the geometry as a feathered fringe with a vertex alpha ramp. Sampling at pixel centres
evaluates that ramp at ±0.5 and yields a hard edge on both sides. Adding coverage AA would
double-count the fringe and make the whole UI look soft.

**Watertight rasterization is not optional.** Edge functions run in 28.4 fixed point with
`i64` accumulators and a strict top-left fill rule. Float edge tests leak: adjacent
triangles either both claim a boundary pixel (a double-blend, which reads as a bright seam
through the chart area fills) or neither does (a crack). `widgets/chart.rs`'s
`area_strip_mesh` is the adversarial case and has its own test.

**A sub-pixel atlas is a contract, not a setting.** With `SubpixelMode::Off` a texel is
`(c, c, c, c)` and any backend can multiply it by the text colour. With sub-pixel coverage
it is `(cov_r, cov_g, cov_b, cov_max)`, meaningful only to a renderer that blends each
channel against its own coverage. Two interlocks enforce this:

- `theme.rs` gates the mode on the active renderer. `TASKMAN_SUBPIXEL=1` cannot override
  it, because the result would be rainbow-tinted text rather than a preference.
- `software_integration.rs` reads the mode from the **atlas** (`f.options().subpixel`),
  not from the style, so the blend mode and the rasterization mode agree by construction.
  Disagreement is the one failure here that does not error — it just draws wrong colours.

## Sub-pixel rendering, concretely

Glyphs are filled at 3x horizontal resolution so each of a pixel's three LCD sub-pixels
gets its own coverage sample, then a symmetric 5-tap FIR filter turns those samples into
per-pixel RGB coverage. Raw 3x samples fringe heavily; the filter trades a little sharpness
for much less colour, which is what `FT_LCD_FILTER_*` does in FreeType.

Two details that make it correct rather than approximately correct:

- **Bounds expand by a whole physical pixel** per side, not a fractional one, so the filter
  tail has room without disturbing the integer `UvRect::offset` that `tessellate_glyphs`'
  pixel rounding — and the renderer's 1:1 glyph blit — both depend on.
- **Hinting is not re-run at 3x.** The outline is already grid-fit against the physical
  pixel grid and only the sampling is finer. This is what FreeType and DirectWrite both do;
  hinting at 3x would snap stems to sub-pixel boundaries.

**BGR panels swap R and B at blend time** rather than rebuilding the atlas. Valid because
the filter is symmetric, so filtering in RGB and swapping is algebraically identical to
filtering in BGR — which means a mixed-panel multi-monitor desk costs nothing.

Blending is gamma-corrected with a contrast boost:

```
a'  = a * (1 + k * (1 - a))                      # enhanced contrast
out = ( src^g * a' + dst^g * (1 - a') ) ^ (1/g)  # gamma-space blend
```

Without it, light-on-dark text comes out bloated and dark-on-light anaemic, because sRGB
values are not proportional to light. **The curve is an empirical match, not a published
specification** — Microsoft has never documented DirectWrite's exact one; this is the model
Skia and Chromium converged on. The *parameters* are not guesswork: they come from
`IDWriteRenderingParams` for the monitor, carrying the user's own `cttune.exe` calibration.

## Text weight, and what `TextSmoothing` now means

`DirectWrite`'s gamma and contrast describe *DirectWrite's* curve. Fed unchanged into this
one they lift a half-covered pixel from 128 to 178 on a dark UI, which reads as fat and
glowing next to native Windows text -- Windows errs thin. So the smoothing profile became
a weight control, which is the useful thing left for it to be now that the coverage ramp it
used to select is not consulted on the sub-pixel path at all:

| profile | grid-fit | binning | blend | ink @150% |
| --- | --- | --- | --- | --- |
| `Sharp` | yes | no | `gamma * 0.72`, no contrast boost | 899 |
| `Standard` | no | no | the display's own parameters | 1117 |
| `Smooth` | no | yes | the display's own parameters | -- |

`theme::cleartype_weight` is the single definition, shared with the comparison harness.

**Measure at 150% scaling, not 100%.** At 100% the three profiles are indistinguishable
(edge sharpness 75.4 / 75.3 / 75.5) and the weight question is invisible. At 150%, where
stems are ~2 px, `Sharp` is 20% less ink *and* sharper. An earlier comparison at 100% led
to the wrong conclusion for exactly this reason.

`crates/tm-app/src/text_compare.rs` renders every profile, filter and blend to magnified
PNGs: `TASKMAN_TEXT_COMPARE=target/t cargo test -p tm-app text_compare`. The LCD filter is
the other real knob -- unfiltered 96.6 sharpness at 92.0 fringe, `FREETYPE_DEFAULT` 75.3 at
64.1, `CLASSIC` 69.9 at 58.8.

## When sub-pixel text is refused

Enabling it where it is not valid looks *worse* than grayscale, so it is gated on:

- `SPI_GETFONTSMOOTHING` off, or `SPI_GETFONTSMOOTHINGTYPE` not ClearType — explicit user
  choices.
- A ClearType level of 0 from the tuner, which means the same thing.
- The process not being per-monitor DPI aware: DWM would bitmap-stretch the window and
  smear the fringes into colour noise. Also covers RDP and Magnifier. winit sets
  per-monitor-v2, so this normally passes — but it is checked, because the failure mode is
  ugly rather than obvious. (It correctly reports "off" inside `cargo test`.)
- Any non-Windows platform, for now. The rasterizer and blend path are
  platform-independent; what is missing is the equivalent signal — fontconfig's `rgba` on
  X11, and on Wayland only when the surface is not fractionally scaled.

## Verification

Everything below is headless; none of it opens a window.

- `cargo test -p egui_software` — the rasterizer's own suite. The two that matter most are
  `a_shared_edge_is_rasterized_exactly_once` and
  `thin_vertical_strips_tile_without_cracks_or_overlap`: they count *hits* rather than
  compare colours, which is what distinguishes a crack from a double-blend.
- `tests/text.rs` — the glyph blitter must agree with the triangle rasterizer to within one
  LSB on a negligible fraction of pixels. Not bit-identity: the blit is the exact
  computation (integer fetch at an integer offset) while the triangle path interpolates UVs
  in float before sampling. Forcing bit-identity would mean fudging the general sampler to
  match the fast path.
- `cargo test -p tm-app render_snapshot` — drives the real `tablekit`, `chart` and `icons`
  code and asserts structural properties of the output, plus the end-to-end sub-pixel
  check: enabling the mode must add channel spread to glyph edges and leave solid fills
  bit-identical.
- `TASKMAN_RENDER_SNAPSHOT=target/frame.png cargo test -p tm-app write_cpu_frame_snapshot`
  writes the frame, and a `-cleartype` variant beside it, to look at.

**Not yet verified:** the window path. Opening a real window is the one thing the headless
tests cannot cover, and the CPU-cost measurements in the plan need it too.

## Still open

- **DirectWrite as the glyph source.** The blend parameters already come from DirectWrite;
  the *rasterization* is still epaint's own 3x path. `IDWriteGlyphRunAnalysis` with
  `DWRITE_TEXTURE_CLEARTYPE_3x1` would give bitmaps identical to Windows'. It also requires
  switching layout to linear (unhinted) advances to match `MEASURING_MODE_NATURAL`, which
  inverts the grid-fitting argument in `fonts.rs`'s module doc — read that before starting.
- **Performance work.** The renderer is correct but unoptimized: `f32` end to end, no SIMD,
  no span fast path for axis-aligned rects, no damage tracking. See the plan for the
  ordering and the reasoning about which of those actually matters (damage tracking is
  worth less than it sounds: a sample tick dirties most of the window anyway).
- **Retiring wgpu and glow.** Both are still compiled in as fallbacks.
