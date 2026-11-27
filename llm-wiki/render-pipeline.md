# Render Pipeline

Last verified: 2026-11-27 (render-time polarity grayscale curve in all renderers, inverse-surface GPU LCD calibration, weight measured against native DirectWrite, hardware CPU/GPU parity numbers)

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
  is preserved; the next WGPU frame uploads the new order/gamma/contrast from `TextOptions`.
  Regaining window focus (`Event::WindowFocused(true)`) also forces a fresh DirectWrite query
  so live `cttune.exe` / Windows Settings changes apply immediately even when the window did
  not move or resize.
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

**A tile hash is only sound when every global input that changes pixels is in the key.**
`damage::FrameKey` covers the target size, `pixels_per_point`, the clear colour, text and
sub-pixel mode, gamma/contrast, `round_rects_to_pixels`, `round_text_to_pixels`,
`round_line_segments_to_pixels`, `coarse_tessellation_culling`, `feathering`,
`feathering_size_in_pixels`, `prerasterized_discs`, `bezier_tolerance`, `epsilon` and the
font atlas size, and all three `debug_*` toggles. The toggles are keyed because they are
applied after `plan()` hashed the input shapes: `debug_paint_text_rects` injects its stroke
inside `tessellate_shape`, which reaches the default `Blit` path for rotated text, and the two
clip-rect toggles are applied at the end of `tessellate_shapes`, which `Blit` reaches for the
underline stroke. Without them in the key a runtime toggle repaints nothing and the new
primitives land outside the tiles that claim to be current. `Shape::Circle` is hashed
together with the font atlas even though it names no texture, because a small filled circle
can be tessellated through a prerasterized disc that lives in that atlas. A missing entry
does not fail a test — it shows up as a stale region — so
`global_render_state_changes_force_full_damage` changes one input at a time against a
shape-free frame, where only the key can force the repaint.

**An atlas patch that rewrites a live texel invalidates the tiles sampling it.** epaint
reuses atlas slots when the atlas overflows (its cursor restarts inside the image and live
glyph cells are overwritten), so a partial update that changes texels which already held
content bumps the texture generation, while filling empty atlas space bumps nothing and a
newly rasterized glyph therefore costs no repaint. The "was it empty" test is sound because
an allocated cell is always written in full — both rasterizers loop over the whole
width × height rect, zero coverage included — and a zero-sized glyph is never allocated.
One case escapes it: right after an overflow restart a new cell of a different size can
write content into a still-live cell's transparent texels. That cannot outlive the next
pass, because overflow pins `fill_ratio()` at 1.0 and the full upload that follows does
bump the generation — at worst one frame of stale glyphs, in the state epaint already
warns about.

**Resident WGPU state follows viewport visibility.** eframe creates a device and surface only
for a visible viewport. When no viewport remains on screen, hidden logic ticks continue, but
`EpiIntegration::frame.wgpu_render_state` is cleared, all surfaces/depth/MSAA/capture state
and managed renderer textures are freed, and the painter's `RenderState` and device are
dropped. A later visible repaint recreates the device/renderer/surface. Managed user textures are snapshotted on the CPU as egui uploads full or partial images, while
`TextureId::default()` records only its `TextureOptions` and replays live from
`Context::fonts(|f| f.image())` so the replacement renderer restores both the font atlas and
cached row icons without keeping a permanent second CPU copy of the atlas. An explicit
`Visible(false)` suppresses eframe's first-frame reveal.

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

## Presentation gate

The desktop must never compose the main window before that window holds a real
frame. `tm_platform::install_present_gate` (Windows: `win/present_gate.rs`)
owns that invariant; the app only drives it.

Three mechanisms, and all three are needed:

1. **Cloak at install.** `DWMWA_CLOAK` on the HWND inside eframe's creation
   callback, before winit's first show. A cloaked window is still visible to
   Windows — taskbar button, focus, `WM_PAINT` — but DWM draws nothing for it.
2. **Re-cloak from a WndProc subclass** (`SetWindowSubclass`), before returning
   from the message that would make the window composable. The app cannot do
   this part: a taskbar restore is performed by Windows before the next
   `update()`, so a cloak set from the app was always one frame too late.

   | Message | Relied on for | Gate |
   | --- | --- | --- |
   | `WM_SHOWWINDOW` (TRUE) | every hide→show, incl. the tray | unconditional |
   | `WM_WINDOWPOSCHANGED` + `SWP_SHOWWINDOW` | a show the app did not ask for | while held |
   | `WM_SYSCOMMAND` `SC_RESTORE`/`SC_MAXIMIZE` | the *pre*-restore notification of a taskbar click | while `IsIconic` |
   | `WM_SIZE` `SIZE_RESTORED`/`SIZE_MAXIMIZED` | backstop when no `SYSCOMMAND` arrives | while held or after a remembered minimize (`WM_SIZE` `SIZE_MINIMIZED`) |

3. **Never white, worst case.** GDI has no class background to erase with —
   winit registers with `hbrBackground: 0` — so the gate installs the app
   background as the class brush and fills `WM_ERASEBKGND` with it. A frame
   that slipped through would be app-coloured, not system white. The caption,
   border and DWM backdrop are applied at creation (`apply_title_bar`) rather
   than in the first `ui`, so a dark-theme app never composes a light-mode
   caption.

**One uncloak door.** `present_gate_release` is the only reveal, and
`set_window_cloaked(.., false)` plus `ensure_presentable` refuse while the gate
holds. A zero HWND falls back to the handle the gate stored, so losing the
cached handle cannot strand a cloak. The hold is released from
`NativeApp::finish_reveal` on the pass after a painted frame
(`REVEAL_MIN_TICKS = 2`), bounded by 8 passes / 2 s — a held window is always
revealed, because an invisible task manager is worse than a flash.

A third-party uncloak (virtual-desktop switch, shell) carries no message, so it
is not a gate transition: the cloak is cleared on release if DWM still reports
the window cloaked.

### What the D3D12 path used to cost against the gate

The gate makes "window visible" mean "a frame of ours is in the compositor",
which is right, but it also handed the renderer's init a deadline: whatever
eframe does before its first painted frame is now time the user waits with no
window at all. Two costs sat in that gap.

- **The reveal waited for the pipelines.** `Painter::set_window` built the
  adapter, the device and every pipeline, the first frame painted, and only
  then did `post_rendering` reveal; the CPU renderer has no comparable init.
  `egui-wgpu` now builds the device and the surface only, and
  `Painter::present_clear` presents one frame cleared to `App::clear_color`
  with no pipeline involved. eframe reveals on that frame — gated on
  `is_first_frame && reveal_first_frame && needs_primer(ROOT)` and on
  `present_clear` returning `true`, i.e. only when the frame really reached the
  screen (a zero-sized, occluded, outdated or lost surface presents nothing and
  keeps the primer owed) — so the primer runs once before the first reveal and a
  restore of an already-visible window paints the real ui in its own frame.
  Residual: the *fallback* reveal later in the same pass (`post_rendering` after
  `paint_and_update_textures`) is unconditional, and the paint has the same
  non-presenting arms, so a pass that skips both still reveals an unpainted
  frame and no later pass asks for the primer again. On the shipped
  Windows/D3D12 path `acquire_texture` cannot return `Outdated`/`Lost`, so
  `Ok(false)` is reachable there only from a zero-sized first frame, which a
  `Resized` heals; the Vulkan/Metal shapes are the ones that stay open.
  `Renderer` is created lazily inside the
  first `paint_and_update_textures`, and `RenderState.renderer` is a
  clone-shared `Arc<OnceLock<..>>` because the `CreationContext` copy is made
  before that paint (a `None` snapshot there silences `PaintCallback`).
- **A restore replayed the whole init.** The Windows minimize signal
  (`Resized(0x0)`) made the frame function call `Painter::suspend()`, which
  dropped device, renderer, managed textures and surfaces. Restoring therefore
  paid device creation, pipeline compilation and the atlas replay before the
  gate saw a frame — seconds on D3D12, milliseconds on CPU.
  `Painter::pause()` now frees the surfaces, the depth/MSAA and capture views
  and the size-dependent targets, keeps the device, the pipelines and the
  texture snapshots, and polls without blocking. eframe pauses on the first
  hidden frame and only calls `suspend()` once the window has stayed hidden for
  `HIDDEN_RESIDENCY_GRACE` (10 s), so the tray still ends at near-zero
  resident memory while a minimize/restore is a surface reconfigure.
- **A restore past the grace re-enumerated every adapter.** Measured from the
  logs, device creation is 240–430 ms on this host and the pipelines ~36 ms
  (FXC; DXC would not pay for its size). Most of the device cost is
  `enumerate_adapters`: wgpu-hal's D3D12 `Adapter::expose` opens an
  `ID3D12Device` on every listed adapter — WARP and any second GPU included —
  and `Adapter::open` reuses that probe device. `Painter::suspend` now keeps
  the selected `wgpu::Adapter` (`retained_adapter`, `CreateNew` only, never the
  enumerated list) and `add_surface` re-opens it through
  `RenderState::create_on_adapter`, falling back to the full path when the
  adapter cannot present to the surface or its device is gone. The attempt owns
  the adapter so a removed device is released before the enumeration: D3D12
  hands back the same per-adapter device while anything holds it. Every
  re-create logs `wgpu device re-created in <n> ms from the <source>`.
  Stale-risk: the hidden-state GPU-memory counters for a held empty adapter
  device were not measured here (the user tests GUI restores manually).

Consequence worth knowing before touching either: the gate's `painted` flag is
still what decides the reveal, so the primer only moves *when* the gate gets its
first frame. Do not shorten the primer away, and do not expect a change here to
lower `REVEAL_MIN_TICKS`.

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
- **LCD hinting mode only where it means "LCD".** Under sub-pixel text, fonts hinted by their
  own TrueType/CFF instructions (Segoe UI Variable) get skrifa's `SmoothMode::Lcd`, which
  only flips GETINFO from grayscale to LCD ClearType. Autohinted fonts (many Linux faces)
  keep the requested mode: for the autohinter LCD mode, like FreeType's `FT_LOAD_TARGET_LCD`,
  turns horizontal hinting and stem adjustment off, which would silently drop the horizontal
  grid-fitting `Sharp` asks for (`epaint` `lcd_aware_hinting_target`).

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
the atlas remains RGB-filtered. The 48-byte `Locals` uniform carries the active order,
`TextOptions::text_contrast`, `text_gamma`, `text_background_luma`,
`text_inverse_background_luma` and the grayscale-curve exponents; the font atlas is drawn with a
second uniform bind group whose `font_atlas` flag is 1, which is how `egui.wgsl` tells glyph
coverage from user textures without a third pipeline. LCD sub-pixel edges skip dithering so
`Unorm`, `sRGB`, and CPU output stay deterministic without edge sparkle; ordinary non-LCD
geometry in the same mesh keeps covered-only dithering.

**GPU gamma handling:** fixed-function dual-source blending cannot read the destination pixel,
so the shader solves for the blend weight that lands on the software renderer's `TextGamma`
result `(s^g * a + d^g * (1 - a))^(1/g)` for an ASSUMED background `d`, on `Unorm` and sRGB
targets alike (sRGB solves in linear space). Text polarity splits exactly like the grayscale
curve (`epaint::polarity_split`, below): text of the theme's polarity is assumed on the page
(`text_background_luma`: `0.13` Dark, `0.96` Light), the rest on the inverse surface
(`text_inverse_background_luma`: `0.13` in the Light theme, where light text is the dark toast
UI; unknown in the Dark theme, which draws no dark text) or on the pure black/white endpoint
when that is unknown. Calibration is exact only on the assumed surface; elsewhere the weight is
the chord through that surface and the text colour. Modelled errors on real surfaces: <= 2 LSB on
the pages, ~3-7 LSB at worst partial coverage on heat cells and selection rows. Measured on
D3D12 and Vulkan by the hardware suite (`Unorm`, bundled face and Segoe UI Variable, 1.0/1.25/
1.5/2.0 `ppp`): mean difference <= 1.04 LSB on upright text (<= 1.61 on italics), isolated pixels up to 7 LSB where
two glyph quads overlap (the second glyph is solved against the surface, but the pixel already
holds the first glyph's ink), italic rows up to 8 LSB (the CPU rasterizer snaps vertices to
1/16 px, the GPU to 1/256 px, which shifts sheared edges by a fraction of a texel). Before the
inverse surface existed, the Light theme's white toast text was solved against pure black and
drew up to ~16 LSB heavier on the GPU. Both CPU and GPU apply enhanced contrast to glyph coverage
*before* scaling by straight text opacity, and both apply `TextGamma` to transformed/italic/
off-grid LCD grayscale fallbacks. LCD sub-pixel edges skip dithering. (With the earlier 1.98 Light gamma the same pixels reached 10/13 LSB; the steeper the
curve, the more a coverage difference shows.)

## Text weight, and what `TextSmoothing` now means

**The weight is measured against native Windows text, not judged by eye.**
`crates/tm-app/src/dwrite_reference.rs` renders a string with DirectWrite's own rasterizer and
blender (an `IDWriteBitmapRenderTarget`: a GDI DIB, no window, no GPU) using Windows' default
parameters (gamma 1.8, contrast 0.5, level 1, RGB) and `DWRITE_RENDERING_MODE_DEFAULT` -- natural
symmetric at these sizes, what WinUI/WPF use -- on the same face (Segoe UI Variable Text), size
and colours. `text_compare::the_weight_matches_native_directwrite_text` compares summed luma
contrast ("ink") at 100/125/150 % in both themes, ClearType and grayscale, and prints every ratio.
Measured 2026-11-27:

| profile | ClearType Dark | ClearType Light | grayscale Dark | grayscale Light |
| --- | --- | --- | --- | --- |
| `Sharp` | 0.89-0.93 | 0.89-0.93 | 0.90-0.93 | 0.90-0.91 |
| `Standard` | 1.09-1.13 | 1.00-1.03 | 0.98-1.00 | 1.00 |

`Sharp` is thin and crisp by design (~0.9 of native ink, between DirectWrite natural and GDI
classic in Dark: GDI classic measures ~1.04 there). `Standard` feeds the display's parameters into
this blend model unmodified, which in Dark ClearType lands ~10 % heavier than DirectWrite's own
curve. The test bands are `Sharp` 0.85-0.97, `Standard` 0.92-1.17. Before this measurement the
Light theme's `Sharp` ClearType sat at ~0.83 and its grayscale at ~0.81 -- both lighter than every
native mode -- and the grayscale dark-text exponents assumed sRGB-space blending made dark-on-light
text fat, while native DirectWrite dark-on-light text is in fact heavier than raw coverage.

`DirectWrite`'s gamma and contrast describe *DirectWrite's* curve; fed unchanged into this one they
lift a half-covered pixel from 128 to 178 on a dark UI. Under sub-pixel rendering `Sharp` therefore
pairs `LcdFilter::LIGHT` (`[0, 85, 86, 85, 0]`, omitting `FREETYPE_DEFAULT`'s `±2` outer sub-pixel
taps when the platform did not specify a filter, e.g. Windows; explicit Linux Fontconfig
`lcdfilter: default` stays `FREETYPE_DEFAULT`) with `contrast = 0.0` and scaled gamma.

**Grayscale weight is applied by the renderer, per text colour.** With sub-pixel rendering off
(`SubpixelMode::Off` -- Linux `rgba=none`, RDP, portrait displays, non-dual-source GPUs) the atlas
holds raw coverage (`FontColorTransferFunction::PolarityGamma`) and every renderer -- CPU blit and
triangle paths, `egui.wgsl`, the LCD shader's ordinary-texel branch, Glow -- applies
`alpha = coverage^e`, with `e` the light-text exponent when the text colour's luma is above
`polarity_split` and the dark-text exponent otherwise. `polarity_split` is the midpoint of the
page and inverse-surface lumas when both are known (Light theme: `0.545`, so light-gray toast text
counts as light), the page luma when only it is (Dark theme: `0.13`, so mid-luma accent text such
as `#3a63d9` stays light-on-dark), and `0.5` when neither is. A curve chosen by THEME, as before,
thinned the Light theme's white toast text with the dark-text exponent: ~17 % below raw coverage
(`text_compare::opposite_polarity_grayscale_text_is_not_thinned`).

| profile | grid-fit | binning | platform-default LCD filter | LCD gamma (Dark / Light) | grayscale exponent (light / dark text) |
| --- | --- | --- | --- | --- | --- |
| `Sharp` | full on Windows/default and Fontconfig `hintmedium`/`hintfull`; light for `hintslight`; off for `hintnone` | no | `LIGHT` (explicit Fontconfig `default` stays `FREETYPE_DEFAULT`) | `gamma * 0.78`, `0.0` / `gamma * 0.88`, `0.0` | `0.80` / `1.15` |
| `Standard` | follows Fontconfig `hintstyle` (`hintmedium` = full, as FreeType's normal target) | no | `FREETYPE_DEFAULT` | display params | `0.65` / `0.95` |
| `Smooth` | default (`preserve_linear_metrics`) | yes | `FREETYPE_DEFAULT` | display params | `0.65` / `0.95` |

`theme::configure_text_options` is the one definition of every text option (filter, blend,
grayscale curve, polarity surfaces, hinting switch); `apply_text_aa` and the comparison harness
both call it, and the harness also uses the app's `fonts::hinting_target`, so `text_compare`
renders exactly what the app renders. `TASKMAN_TEXT_COMPARE=<prefix> cargo test -p tm-app
text_compare` writes magnified PNGs of every profile, filter and blend, plus both themes x
100/150 % x LCD/grayscale. The LCD filter is the other real knob -- unfiltered 96.6 sharpness at
92.0 fringe, `FREETYPE_DEFAULT` 75.3 at 64.1, `CLASSIC` 69.9 at 58.8.

## When sub-pixel text is refused

Enabling it where it is not valid looks *worse* than grayscale, so it is gated on:

- `SPI_GETFONTSMOOTHING` off, or `SPI_GETFONTSMOOTHINGTYPE` not ClearType — explicit user
  choices.
- Remote Desktop session (`GetSystemMetrics(SM_REMOTESESSION) != 0`), where compression and
  client rescaling turn sub-pixel fringes into color noise.
- Portrait-rotated displays (`DMDO_90` / `DMDO_270` from `EnumDisplaySettingsW`) or
  `DWRITE_PIXEL_GEOMETRY_FLAT`, where horizontal RGB/BGR stripes do not exist.
- A ClearType level of 0 from the tuner, which means the same thing.
- The process not being per-monitor DPI aware: DWM would bitmap-stretch the window and
  smear the fringes into colour noise. winit sets
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
Linux sets `contrast = 0.0` (no DirectWrite enhanced-contrast boost), parses `hinting` and
`hintstyle` for `TextSmoothing::Standard`, maps `lcdfilter` to FreeType default, light,
legacy/classic and none, and resolves both file path and `.ttc` face index (`%{index}`) for
system-font selection. The system-font lookup uses the same canonical absolute executable and
parent-directory policy as the policy query. One 750 ms absolute deadline covers process
output, which is capped at 64 KiB; a timed-out or oversized helper is killed and reaped, and
every failure returns unavailable grayscale rather than the user's `PATH` or an invented
policy.

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
  bit-identical. The same test binary carries the chart-geometry gate: adversarial series
  (NaN, ±inf, a stamp an hour outside the axis, fewer timestamps than samples) are
  tessellated through a windowless `Context`, and every vertex must be finite and inside
  the tile's bounded lead-in overhang, each measured run must be its own stroke that never
  spans more than one sample interval (one tile width where a fixture's spacing is
  irregular), and the fill must not close over a gap. The mismatched kernel slice has its
  own case in `widgets::chart`'s unit tests, which is also where the geometry rules
  (`ChartGeometry`) are pinned. The gate exists for the artifact where a missing sample was
  drawn as a straight stroke across the plot.
- `(cd vendor/egui && cargo test -p egui-wgpu)` - pure coverage for feature detection/request policy, atlas
  full/partial state transitions, managed-texture-only pipeline selection, transform
  fallback policy, all-white untextured exclusion, RGB/BGR/contrast uniform fields, the
  32-byte uniform layout, and Naga parse/validation of both LCD fragment entry points.
- `cargo test -p tm-platform` — fontconfig parser policy for RGB/BGR, unknown/vertical/none,
  antialias false, malformed output, lcd filter mapping, trusted program/parent metadata,
  absolute font-file policy, bounded output and timeout kill/reap. Windows tests cover
  placement-key changes and conflicting-monitor fail-closed policy.
- **Manual GPU path:** `(cd vendor/egui && TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1 cargo test
  -p egui_kittest --features wgpu --test subpixel_text -- --ignored --test-threads=1 --nocapture)`.
  The tests are ignored by default and fail if the opt-in variable or a non-CPU D3D12/Vulkan
  dual-source adapter is missing (`TASKMAN_GPU_BACKEND=dx12|vulkan` narrows the run). They cover
  plain/off-grid text, BGR, neutral/tinted contrast, opaque/translucent and colored text,
  hardware filtering, dithering and the sRGB entry point, and the CPU-vs-GPU parity of the real
  shaders: ClearType and grayscale, page and toast surfaces, the bundled face and the system UI
  face with `Sharp` hinting, 1.0-2.0 `ppp`. Each case prints its measured upright/italic
  max/mean; bounds are the measurements above plus a small margin, and
  `TASKMAN_PARITY_DUMP=<dir>` keeps both frames of a failing case. Grayscale on sRGB targets is
  reported, not bounded: the stock ordinary pipeline blends every fill in linear space there,
  and TaskMan always selects `Unorm`. The `egui-wgpu` unit test of the calibration is a Rust
  MODEL of the shader (Naga validates WGSL but cannot run it); this suite is the check for the
  shader itself. Manual hardware coverage, not portable CI coverage.
- `cargo test -p tm-app the_weight_matches_native_directwrite_text -- --nocapture` (Windows) -- the
  native-weight comparison above.
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
  must run the ignored `egui_kittest` suite on D3D12 and Vulkan hardware (both passed on an
  NVIDIA host on 2026-11-27; no AMD/Intel or Linux Vulkan driver measured yet).
- **GPU ClearType cannot see the real background.** Exact only on the assumed page/inverse
  surface; heat cells, selection rows and overlapping glyph margins stay a few LSB off the CPU
  result. Removing that needs destination reads (framebuffer fetch or an offscreen copy),
  which fixed-function dual-source blending cannot provide.
- **Only Windows' 100/125/150 % are measured against native text in the gate**, on Segoe UI
  Variable; 175/200 % have CPU/GPU parity coverage (2.0 `ppp`) but no native reference.
- **Linux weight against FreeType + cairo is measured, not gated.**
  `TASKMAN_LINUX_TEXT_REFERENCE=1 cargo test -p tm-app linux_weight_against_freetype_cairo
  -- --nocapture` renders the Fontconfig `sans-serif` face with `/usr/bin/pango-view` (the
  GTK/cairo stack) and prints ratios. Measured 2026-11-27 in Arch WSL (Adwaita Sans,
  `hintslight`; the rasterizer is the same code as bare metal): Dark Sharp 1.02-1.10, Standard
  1.08-1.19 (TaskMan gamma-corrects, cairo blends in sRGB, which thins light-on-dark text);
  Light grayscale Sharp 0.92-0.94, Standard ~1.0; **Light subpixel Sharp 0.85-0.87 and
  Standard 0.80-0.83** -- lighter than cairo, and Standard lighter than Sharp, because Linux
  reports `contrast = 0` and Standard then blends dark ink at the raw gamma 1.8 while Sharp
  scales it down to ~1.58. Open decision: calibrate Linux LCD weight toward cairo (the
  desktop's look) or keep the Windows-calibrated gamma-corrected look; either way the
  Standard/Sharp inversion should go. Qt and GTK 4 blend differently from cairo's image
  surface, so this reference is the GTK3/cairo look, not "Linux".
- **WSL cannot test the GPU LCD path.** Its Vulkan device is Mesa Dozen on the Windows
  D3D12 driver and reports `dualSrcBlend = false`, so TaskMan falls back to grayscale there and
  the hardware suite fails (correctly, not a skip); the test process then segfaulted at teardown
  inside the Dozen/loader stack. WSLg presents through Weston + RDP, so visual checks there are
  not representative either. RADV/ANV/NVIDIA-Linux need bare-metal runs.
- **Performance work.** The rasterizer and application loops are optimized for low CPU load:
  - Axis-aligned rect fast path (`span::fast_rect` with `Shape::Vec` recursive flattening for containers).
  - Uniform untextured triangle fast paths (opaque and translucent) with single-branch edge testing.
  - Text blitting with hoisted `linear_u16` LUTs, clamped row slices, and solid-glyph interior bypass.
  - Full-target memset clear path.
  - 64×64 tile damage tracking (`egui_software::damage`, `Painter::paint_shapes_damaged`, the `age() == 1` present-in-damage path — `BitBlt` + `ValidateRect` per dirty rect on Windows, `softbuffer::Buffer::present_with_damage` elsewhere — and not restored from hidden), skipping unchanged frames completely and repainting/presenting only dirty tiles on partial updates. The buffer's age is the SURFACE's to report: the tracker compares shapes, not pixels, so an incremental paint is sound only when the buffer it is handed really holds the previous frame. Windows answers with its DIB's presented flag; the softbuffer surface must answer with `softbuffer::Buffer::age()`. A local "we presented something" flag is not equivalent and was the Linux corruption: nothing in it changes when the backend hands back a *different* buffer. Softbuffer's Wayland backend keeps two `wl_shm` buffers and swaps them per present, so a leased buffer is 0 or 2 frames old and never 1 — Wayland therefore full-paints every frame, while X11 (one SHM segment per surface, age reset when it is reallocated) keeps the incremental path and a resize forces a full paint instead of painting into a reallocated segment. Restoring tile skipping behind a ring needs a damage cache per buffer slot; see `known-debt.md`.
  - Taskman process table caching by reference, avoiding per-frame vector cloning.
  - IconCache O(1) lazy expiration and direct unmultiplied upload.
  Remaining potential work: SIMD for span fills.
- **Renderer startup.** CPU is the default. Settings can prefer the platform
  WGPU backend, which falls back to CPU on a renderer initialization failure.
  OpenGL remains a diagnostic and final fallback. An event-loop error stops
  fallback because the renderers share that display connection. Startup
  records the backend that successfully creates the window.
