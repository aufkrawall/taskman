# Render Pipeline

Last verified: 2026-11-24 (GPU LCD path, manual D3D12/Vulkan validation, and Linux fontconfig)

Primary sources:
- `vendor/egui/TASKMAN-FORK.md` (what diverges from upstream egui, and why)
- `vendor/winit/TASKMAN-FORK.md` (window-band, event-loop, Win32 reliability, and Wayland
  suspended-state patches)
- `vendor/egui/crates/egui_software/` (the CPU rasterizer)
- `vendor/egui/crates/egui-wgpu/src/{renderer.rs,egui_lcd.wgsl}` (the GPU LCD path)
- `vendor/egui/crates/eframe/src/native/software_integration.rs`
- `crates/tm-platform/src/{fontconfig_text.rs,win/text_rendering.rs}`

## Why there is a fork

Two requirements met in one place:

1. **Sub-pixel (ClearType-like) text.** Per-channel glyph coverage has to be blended per
   channel. On the CPU that is a multiply. On D3D12/Vulkan it is a second `egui-wgpu`
   pipeline using dual-source blending, created only when the device enabled that feature.
   Glow and unsupported WGPU devices stay grayscale.
2. **No GPU at all** — and not a software *driver* either. The old `render_mode = software`
   was WARP: a D3D12 driver emulated on the CPU, costing ~14 cores at 2.9 fps because it
   emulates a whole graphics stack to draw a 2D UI.

The CPU renderer still satisfies both requirements without a GPU stack. The vendored
WGPU path extends the same atlas contract to D3D12/Vulkan when the hardware exposes
dual-source blending. egui is vendored at `vendor/egui` (see `TASKMAN-FORK.md` for the
divergence inventory and the rebase runbook).

## The layers

```
tm-platform/win/text_rendering.rs   SPI gates + IDWriteRenderingParams
        |  plain data; no `windows` type goes further up
tm-app/theme.rs                     the ONLY place that decides the sub-pixel mode
        |  written into style.visuals.text_options after renderer capability
[FORK] epaint                       3x rasterization -> per-channel coverage in the atlas
[FORK] egui-wgpu                    optional dual-source font-atlas pipeline
[FORK] egui_software::Painter       shapes -> pixels
        |-- Shape::Text        -> glyph blit, per-channel ClearType blend
        |-- pixel-aligned Rect -> span::fast_rect fill (flat + Shape::Vec containers)
        \-- everything else    -> epaint::Tessellator -> triangle rasterizer
[FORK] eframe Renderer::Software
        |-- Windows: own Win32 DIB surface -> BitBlt (checked) + ValidateRect per blitted rect
        \-- X11/Wayland/macOS: softbuffer -> SHM / wl_shm / CoreGraphics
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
every non-empty texel is `(cov_r, cov_g, cov_b, 0)`, meaningful only to a renderer that
blends each channel against its own coverage. Alpha is an explicit tag, never coverage:
neutral `(c,c,c,0)`, tinted `(r,g,b,0)` and full `(255,255,255,0)` coverage are equally
identifiable. Zero coverage is `(0,0,0,0)` and draws nothing; premultiplied color glyphs
keep non-zero alpha. Transformed text and GPU fallback recover mean coverage from RGB and
rebuild premultiplied grayscale alpha. These interlocks enforce the contract:

- `theme.rs` gates the mode on the active renderer capability. Software is always capable;
  WGPU is capable only when its device actually enabled `DUAL_SOURCE_BLENDING`; Glow and
  unsupported WGPU devices stay off. `TASKMAN_SUBPIXEL=1` cannot override capability.
- The software painter and the WGPU painter read the mode from the **atlas**
  (`fonts.options().subpixel`), not from an independent style copy, so blend order and
  rasterization order agree.
- `egui-wgpu` tracks both the active mode and whether the managed font texture actually
  contains the zero-alpha LCD tag. The LCD pipeline is selected only for
  `TextureId::default()` when the state is true **and** the mesh is not all `WHITE_UV`;
  solid untextured geometry therefore stays on the ordinary pipeline. A full atlas
  replacement recomputes the state; after the state is true, partial updates short-circuit
  without rescanning unrelated glyph uploads.

**Transformed text falls back to grayscale.** The WGPU shader checks the texture-coordinate
derivatives and the sampled texel center. Rotated, scaled or fractionally translated glyphs
do not map one source texel to one display pixel, so they average RGB coverage and use the
ordinary grayscale blend. The software renderer applies the same policy: its triangle path
converts LCD texels to mean coverage before filtering. This is intentional weight loss,
not a failure to find a better sampler; preserving the fringes would be visibly wrong.

The software direct 1:1 blitter decides per texel: tagged LCD texels use per-channel
coverage, while grayscale and premultiplied color glyphs in the same atlas use the ordinary
blend. A mixed-atlas regression pins that distinction.

**Windows monitor policy follows the window without recreating the renderer.**
`NativeApp` keeps a cheap placement key (window rectangle, current monitor handle and
rectangle, effective DPI). A same-sized move, monitor topology change or DPI change runs a
fresh DirectWrite query for every monitor intersecting the window rectangle.

- All intersecting monitors agreeing updates only the theme blend parameters. `SUBPIXEL_CAPABLE`
  is preserved; the next WGPU frame uploads the new order/contrast from `TextOptions`.
- Any unavailable DirectWrite query, disagreement in validated parameters or RGB/BGR order,
  or malformed parameter set disables LCD for the whole window/viewport. Choosing one half
  of a straddling window or assuming RGB after a failed query is never allowed.
- Moving back to agreement re-enables LCD without restarting the process or renderer.
- `TASKMAN_SUBPIXEL=0` remains an unconditional off switch. `TASKMAN_SUBPIXEL=1` is an A/B
  request only; it cannot override unsupported channels, a renderer without the capability,
  or a fail-closed monitor policy.

**On Windows the presentation path is ours, not softbuffer's.** `platform_surface` in
`software_integration.rs` owns a `CreateCompatibleDC` + top-down 32-bpp `CreateDIBSection`
surface and presents with `BitBlt`, for four reasons that are all failure behaviour:

- softbuffer 0.4.8 ignores `BitBlt`'s return value, `ValidateRect`s the **whole** window and
  still marks the buffer presented; a failed blit therefore froze garbage in place with no
  error. Ours checks every `BitBlt`, validates only the rects that succeeded, and on failure
  clears `age()` and retries in 50 ms.
- A `WM_PAINT` and an app-requested repaint arrive as the same winit `RedrawRequested`.
  `GetUpdateRect` (queried **before** `DefWindowProcW` validates it) is merged into the
  present set, so a frame with zero dirty egui tiles still blits the region Windows needs.
- GDI allocation is fallible: every step returns `Result`, nothing asserts. The old path
  aborted the process on a failed allocation, because release builds panic on assert.
- The buffer is a memory DIB under our control, so the "presented while visible" flag
  (`should_force_full_paint`) can force a full repaint after a reveal or a display change
  instead of trusting a frame counter.

Paint and present failures are **never fatal**: they warn, mark the surface unpainted and
schedule a 50 ms retry. `EventResult::Wait` (not `RepaintNow`) is returned from inside
`WM_PAINT`, because scheduling a repaint synchronously there recurses into the same paint.

**Resident WGPU state follows viewport visibility.** eframe creates a device and surface only
for a visible viewport. When no viewport remains on screen, hidden logic ticks continue, but
`EpiIntegration::frame.wgpu_render_state` is cleared, all surfaces/depth/MSAA/capture state
and managed renderer textures are freed, and the painter's `RenderState` and device are
dropped. A later visible repaint recreates the device/renderer/surface. Managed textures are
snapshotted on the CPU as egui uploads full or partial images; the latest full image is
replayed into the replacement renderer so the font atlas and cached row icons survive the
round-trip. An explicit `Visible(false)` suppresses eframe's first-frame reveal.

Wayland has no direct window-visibility or minimize query. The vendored winit exposes
xdg-shell v6's `SUSPENDED` configure hint through `Window::is_minimized()`, which makes a
compositor-minimized or repaint-suspended toplevel enter the same hidden path. A configure
that changes the suspended bit clears winit's pending frame-callback throttle, so the redraw
is not lost when an invisible compositor stops delivering frame callbacks. Compositors that do
not advertise that hint remain a platform limitation.

Visible WGPU sessions use `wgpu::MemoryHints::MemoryUsage`, cap D3D12's eager non-sampler
descriptor heap at 8,192 bindings, and shrink historical vertex/index buffers after sustained
low use. These settings lower allocation floors; they do not remove the LCD pipeline or change
rendering. Device-loss recovery and custom callback/`WgpuSetup::Existing` resources remain
outside the managed restore guarantee.

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
filtering in BGR. The mode is still process/window-wide; Linux fontconfig does not expose
per-monitor orders, so mixed-panel LCD layouts remain a documented limitation.

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

### WGPU dual-source path

`egui-wgpu` asks for `DUAL_SOURCE_BLENDING` only when the selected adapter advertises it
(`adapter.features() & DUAL_SOURCE_BLENDING` in the device descriptor). `Renderer::new`
creates the LCD pipeline only when the resulting device feature is present. A custom
device descriptor or an existing device that did not request the feature therefore fails
closed.

Before device shader creation, the combined WGSL is parsed and validated with Naga on the
CPU. The same test runs in the normal `egui-wgpu` suite and covers both linear- and
gamma-framebuffer fragment entry points, the extension declaration, `@blend_src` interface
and derivative use. Device shader/pipeline validation then runs inside Validation, Internal
and OutOfMemory error scopes with a bounded 250 ms resolution wait, popped in LIFO order.
Any parse, shader or pipeline failure logs, drops only `lcd_pipeline`, retains the ordinary pipeline and leaves
`has_subpixel_pipeline()` false. It cannot panic or block startup indefinitely.
HLSL/SPIR-V generation is deliberately not part of that CPU test: it would require
enabling additional Naga backend features in the vendored workspace, and it does not
replace the required per-driver pipeline run.

For the managed atlas, `egui_lcd.wgsl` emits source color at location 0 / source 0 and
per-channel coverage at location 0 / source 1. The pipeline blends `ONE` against
`ONE_MINUS_SRC1`, so RGB each use their own coverage. BGR is a blend-time channel swap;
the atlas remains RGB-filtered. The same uniform carries the active order and
`TextOptions::text_contrast`, so Sharp/Standard/Smooth still change weight without a
hardcoded color-dependent approximation.

**GPU gamma limitation:** fixed-function dual-source blending cannot read the framebuffer,
so it cannot implement the software path's nonlinear `dst^gamma` term. WGPU applies the
contrast curve and preserves the configured stripe order, but the result is not claimed to
be DirectWrite-gamma-exact. The software renderer keeps the full gamma/contrast model.

## Text weight, and what `TextSmoothing` now means

`DirectWrite`'s gamma and contrast describe *DirectWrite's* curve. Fed unchanged into this
one they lift a half-covered pixel from 128 to 178 on a dark UI, which reads as fat and
glowing next to native Windows text -- Windows errs thin. So the smoothing profile became
a weight control, which is the useful thing left for it to be now that the coverage ramp it
used to select is not consulted on the sub-pixel path at all:

| profile | grid-fit | binning | blend | ink @150% |
| --- | --- | --- | --- | --- |
| `Sharp` | yes | no | `gamma * 0.88`, contrast 0.25 | 991 |
| `Standard` | no | no | the display's own parameters | 1117 |
| `Smooth` | no | yes | the display's own parameters | -- |

`theme::cleartype_weight` is the single definition, shared with the comparison harness.
The software renderer consumes both returned values. The WGPU path carries the contrast
value and its RGB/BGR order through its uniform, but cannot carry the gamma term into a
fixed-function destination blend (see above).

**Measure at 150% scaling, not 100%.** At 100% the three profiles are indistinguishable
(edge sharpness 75.4 / 75.3 / 75.5) and the weight question is invisible. At 150%, where
stems are ~2 px, `Sharp` is ~11% less ink *and* sharper. (0.72/0.0 was tried first and
read as too thin -- these constants are set by eye against native Windows text.) An earlier comparison at 100% led
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
- The active GPU backend is Glow, or WGPU did not enable dual-source blending. These are
  renderer capabilities, not user preferences.
- Linux fontconfig reports `antialias: False`, an unknown/vertical/none `rgba`, a
  malformed value, or no explicit antialias state. Only horizontal `rgb`/`bgr` enables
  per-channel coverage. This is *ClearType-like*, not Microsoft ClearType.
- A glyph quad is transformed or fractionally aligned, as described above.

Linux `fc-match` is resolved from absolute root-owned system locations rather than
`PATH`, but it inherits the user's environment and reads the effective per-user KDE/GNOME
fontconfig files. The result is cached process-wide; no frame spawns `fc-match`.
`lcdfilter` maps to FreeType default, light, legacy/classic and none. The system-font lookup
uses the same canonical absolute executable and parent-directory policy as the policy
query. One 750 ms absolute deadline covers process output, which is capped at 64 KiB; a
timed-out or oversized helper is killed and reaped, and every failure returns unavailable
grayscale rather than the user's `PATH` or an invented policy.

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
- `(cd vendor/egui && cargo test -p egui-wgpu)` - pure coverage for feature detection/request policy, atlas
  full/partial state transitions, managed-texture-only pipeline selection, transform
  fallback policy, all-white untextured exclusion, RGB/BGR/contrast uniform fields, the
  32-byte uniform layout, and Naga parse/validation of both LCD fragment entry points.
- `cargo test -p tm-platform` — fontconfig parser policy for RGB/BGR, unknown/vertical/none,
  antialias false, malformed output, lcd filter mapping, trusted program/parent metadata,
  absolute font-file policy, bounded output and timeout kill/reap. Windows tests cover
  placement-key changes and conflicting-monitor fail-closed policy.
- **Manual GPU path:** `(cd vendor/egui && TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1 cargo test
  -p egui_kittest --features wgpu --test subpixel_text -- --ignored --test-threads=1)`. The tests are
  ignored by default and fail if the opt-in variable or a non-CPU D3D12/Vulkan dual-source
  adapter is missing. They cover plain/off-grid text, BGR, neutral/tinted contrast,
  opaque/translucent and colored text, hardware filtering, dithering and the sRGB
  framebuffer entry point. This is manual hardware coverage, not portable CI coverage.
- `TASKMAN_RENDER_SNAPSHOT=target/frame.png cargo test -p tm-app write_cpu_frame_snapshot`
  writes the frame, and a `-cleartype` variant beside it, to look at.

**Validation after the 2026-11-23 correction pass:** the task workers were prohibited from
compiling or running tests, but parent-side integration then passed `cargo check` for the
app/installer, the normal `egui-wgpu` Naga/unit suite, and the manual D3D12/Vulkan hardware
suite. The full release gate and a user-visible visual review remain final checks. Current
epaint has no color-glyph source, so hardware color-glyph coverage is not practical; the
software and renderer-policy tests cover synthetic premultiplied color texels.

**Validation after the 2026-11-24 WGPU-memory pass:** `python build.py --check` passed fmt,
clippy, the workspace and fork tests, Linux target checks, host/Linux release builds, and
packaging. On the D3D12 host, the isolated taskbar-overlay probe held 0 B dedicated/shared
through five samples after the hidden main window was proven absent, visible mode peaked at
55.83 MB dedicated + 30.13 MB shared, and a same-process minimize/restore returned to the
visible footprint with no missing-texture warnings. Vulkan shares the lifecycle, allocator
hint, and Wayland `SUSPENDED` path and compiled for Linux; no Vulkan hardware was available
on this host.

## Still open

- **Window-lifecycle recovery is tested, not seen.** The present/restore paths added for
  the reliability audit (`WM_DISPLAYCHANGE` invalidation, OS-update-rect merge, paint-error
  retry, paint-before-acknowledge restore, tray-failure rescue, off-screen placement repair)
  are covered by unit tests over their pure policy and by the whole workspace gate, but the
  scenario that matters — a display mode change, an exclusive-fullscreen game, a hung
  renderer — cannot be reproduced headlessly. `tools/test-resize.ps1` and the
  `TASKMAN_FPS_PROBE=1` overlay are the intended manual checks.
- **DirectWrite as the glyph source.** The blend parameters already come from DirectWrite;
  the *rasterization* is still epaint's own 3x path. `IDWriteGlyphRunAnalysis` with
  `DWRITE_TEXTURE_CLEARTYPE_3x1` would give bitmaps identical to Windows'. It also requires
  switching layout to linear (unhinted) advances to match `MEASURING_MODE_NATURAL`, which
  inverts the grid-fitting argument in `fonts.rs`'s module doc — read that before starting.
- **Linux per-monitor LCD policy.** fontconfig supplies one process-wide `rgba`/`lcdfilter`
  result. Mixed-panel desktops or a monitor with a different stripe order cannot be
  resolved per window from that signal alone. Fractional Wayland scaling is handled
  conservatively by the transformed-glyph fallback, but a compositor that resamples an
  otherwise 1:1 surface remains a deployment risk rather than something fontconfig can
  prove.
- **GPU hardware validation is manual.** The normal CPU-side Naga test proves WGSL syntax
  and module validity but not per-driver dual-source blend behavior. Release verification
  must run the ignored `egui_kittest` suite on D3D12 and Vulkan hardware.
- **Performance work.** The rasterizer and application loops are optimized for low CPU load:
  - Axis-aligned rect fast path (`span::fast_rect` with `Shape::Vec` recursive flattening for containers).
  - Uniform untextured triangle fast paths (opaque and translucent) with single-branch edge testing.
  - Text blitting with hoisted `linear_u16` LUTs, clamped row slices, and solid-glyph interior bypass.
  - Full-target memset clear path.
  - 64×64 tile damage tracking (`egui_software::damage`, `Painter::paint_shapes_damaged`, the `age() == 1` present-in-damage path — `BitBlt` + `ValidateRect` per dirty rect on Windows, `softbuffer::Buffer::present_with_damage` elsewhere — and not restored from hidden), skipping unchanged frames completely and repainting/presenting only dirty tiles on partial updates.
  - Taskman process table caching by reference, avoiding per-frame vector cloning.
  - IconCache O(1) lazy expiration and direct unmultiplied upload.
  Remaining potential work: SIMD for span fills.
- **Renderer startup.** CPU is the default. Settings can prefer the platform
  WGPU backend, which falls back to CPU on a renderer initialization failure.
  OpenGL remains a diagnostic and final fallback. An event-loop error stops
  fallback because the renderers share that display connection. Startup
  records the backend that successfully creates the window.
