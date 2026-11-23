# taskman's egui fork

This tree is a vendored copy of [emilk/egui](https://github.com/emilk/egui) at tag
**0.36.1** (`05297424601afdf5966e5e01c8c63eef2696df54`), added with

```
git subtree add --prefix=vendor/egui https://github.com/emilk/egui 0.36.1 --squash
```

It started for two rendering things taskman needs that are not reachable from stock egui:

1. **Sub-pixel (ClearType-like) text.** Per-channel glyph coverage needs per-channel alpha
   blending. On a CPU it is a per-channel multiply. `egui-wgpu` now has a second
   dual-source font-atlas pipeline for D3D12/Vulkan devices that actually enabled the
   feature; Glow and unsupported WGPU devices remain grayscale.
2. **A native CPU renderer.** eframe offers only glow and wgpu; `RenderMode::Software`
   used to mean WARP, a D3D12 driver emulated on the CPU at ~14 cores and 2.9 fps.

The CPU backend serves both without a GPU stack; the later WGPU pipeline reuses the same
atlas and falls back cleanly where dual-source blending is unavailable. The inventory
below also carries later, unrelated additions: the keyboard Menu/Application key (#5) and modal
windows plus public accesskit reparenting (#6), both of which the keyboard and
screen-reader model in `llm-wiki/current.md` depends on, a text-cursor blink restart
(#7), text-selection preservation on secondary click (#8), not registering for raw
device input (#9), coalescing pointer-move repaints to the display refresh rate and
skipping moves that cannot change the hover target (#10, with the `hit_test_last_pass`
accessor #11 in `egui` itself), and coalescing wheel repaint requests to the same cadence
(#14). Divergence #15 extends the original sub-pixel feature to a dual-source `egui-wgpu`
font-atlas pipeline and makes transformed software text use a grayscale fallback.

The full design, the blend math, and the verification plan live in
`llm-wiki/render-pipeline.md` in the parent repo. This file is only the *inventory* of what
diverges from upstream, so a rebase is a checklist rather than an investigation.

## Ground rules

- **New files over edits.** New directories never conflict in a subtree merge. Every edit
  to a file that upstream also maintains is permanent rebase surface and must earn its place
  here.
- **The fork must not depend on the `windows` crate.** All platform knowledge (DirectWrite,
  `SystemParametersInfo`, DPI awareness) lives in the parent repo's `tm-app` / `tm-platform`
  and reaches the fork through a plain trait and plain structs. This is what keeps the
  epaint change plausibly upstreamable.
- Nothing here may change behaviour when the new features are switched off. The grayscale /
  GPU paths must stay byte-identical to upstream.

## Divergence inventory

Ordered as the commits that introduce them.

### 0. Build hygiene (droppable — expect upstream to fix these)

Cargo applies `--cap-lints allow` to registry dependencies but **not** to path
dependencies. Vendoring therefore surfaces upstream warnings in every taskman build. Two
were loud enough to drown our own output:

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/containers/scene.rs:79` | `f32::EPSILON` → `<f32>::EPSILON` | The bare path resolves to the deprecated `std::f32::EPSILON` module constant rather than the associated const. |
| `crates/egui-wgpu/src/lib.rs` | added `#![recursion_limit = "256"]` | wgpu 30's `Global` type graph exceeds the default 128-step auto-trait solver budget. rustc is phasing this warning into a hard error (rust-lang/rust#159228), so this is defensive, not cosmetic. |

**Drop both on any rebase where upstream has fixed them.** They are marked in-source with
`TASKMAN-FORK:` where a comment fits.

### 1. `epaint`: reach the tessellator from outside

Two accessors upstream simply forgot. `Tessellator::new` is public and takes the atlas
size and its prepared discs, but neither was reachable from outside epaint, so that public
constructor could not actually be called. `egui_software` needs it to tessellate the
non-text shapes itself.

| File | Change | Why |
| --- | --- | --- |
| `crates/epaint/src/lib.rs` | re-export `PreparedDisc` | it appears in `Tessellator::new`s signature but was unnameable |
| `crates/epaint/src/text/fonts.rs` | add `FontsImpl::texture_atlas()` | `FontsView.fonts` is public but the atlas behind it was not |

Both are upstreamable as plain oversight fixes; drop them if upstream adds equivalents.

### 2. `epaint`: opt-in sub-pixel glyph rasterization

Implemented as the atlas contract below. There is no separate `rasterizer.rs` in this
0.36.1 tree; the customization lives directly in `font.rs`, with the modes/filter/filtering
math in the new `subpixel.rs`.

| File | Nature |
| --- | --- |
| `crates/epaint/src/text/subpixel.rs` | **new** — `SubpixelMode`, LCD filters and coverage filtering |
| `crates/epaint/src/text/font.rs` | **edit** — 3x horizontal fill, one-pixel bounds expansion, RGB filtering, alpha-min sentinel |
| `crates/epaint/src/text/mod.rs` | **edit** — `subpixel`, `lcd_filter`, `cleartype_level`, `text_gamma`, `text_contrast` |
| `crates/epaint/src/lib.rs` | **edit** — re-exports `LcdFilter` and `SubpixelMode` |

The rasterizer branch is still shaped so grayscale stays isolated:

```rust
if opts.subpixel.is_off() {
    // ...byte-identical upstream body...
} else {
    self.rasterize_subpixel(/* ... */)
}
```

so the conflict, when it comes, is one block and not a merge of two rasterizers. Alpha is
the minimum RGB coverage, not the maximum: premultiplied color glyphs never have a channel
above alpha, so `max(rgb) > alpha` is the exact GPU/CPU sentinel for LCD coverage.

### 3. `egui_software`: the CPU painter

`crates/egui_software/` — **entirely new, zero rebase surface.**

### 4. `eframe`: `Renderer::Software`

| File | Nature |
| --- | --- |
| `crates/eframe/src/native/software_integration.rs` | **new**, but a *derived copy* — see below |
| `crates/eframe/src/epi.rs` | **edit** — one enum variant + `Default`/`Display`/`FromStr` arms |
| `crates/eframe/src/lib.rs` | **edit** — dispatch arms |
| `crates/eframe/src/native/mod.rs` | **edit** — one `mod` line |
| `crates/eframe/src/native/run.rs` | **edit** — `run_software{,_and_return}` |
| `crates/eframe/Cargo.toml` | **edit** — `softbuffer` dep + `software` feature |

**Deliberate limitation: the software backend supports the ROOT VIEWPORT ONLY.** Most of
`glow_integration.rs` is multi-window machinery -- deferred viewports, immediate
viewports, per-viewport surfaces, and parent/child repaint routing. Child viewports are
logged and ignored rather than half-supported; a partial implementation of viewport
lifetime is the kind of thing that works in testing and strands a window on someone's
desktop in production. `ViewportCommand`s acting on the root window (title, visibility,
close, decorations, always-on-top) are handled in full through `egui_winit`. taskman uses
no child viewports.

`software_integration.rs` is adapted from `glow_integration.rs` with glutin/GL removed. It
is the single largest recurring maintenance cost in this fork: upstream fixes to viewport
lifetime, AccessKit teardown and immediate-viewport handling land in glow's copy and must be
ported by hand. It therefore carries a provenance header:

```rust
//! Derived from eframe's `glow_integration.rs`.
//! UPSTREAM_BASE: <sha of glow_integration.rs at the last rebase>
```

**Three things in that file are easy to get wrong and produce silent breakage.** All
three were shipped broken once and are called out here so a future port does not repeat
them:

- **`egui_winit` does not raise the close event.** `WindowEvent::CloseRequested` only
  returns `repaint: true`; the backend must push `egui::ViewportEvent::Close` into its own
  `ViewportInfo`, or the app never learns the user clicked the close button. taskman vetoes
  the close and hides to the tray, so this is the path that feature runs on.
- **Return `EventResult::CloseRequested`, not `Exit`.** The wrapper runs `save_and_destroy`
  only on the former, and windows must be dropped while the event loop still runs.
- **Return `EventResult::Exit` once `running` is `None`.** `CloseRequested` tears the
  window down but does *not* end the loop; the exit comes from the next event arriving
  after the teardown. Returning `Wait` there leaves a live process with no window, which
  looks exactly like "the close button does nothing".

Also: `ViewportInfo::events` must be cleared once handed to egui, or a Close event repeats
every frame.

`tools/fork-rebase.sh` in the parent repo diffs that sha against the new tag so the port is
a small review, not a re-derivation. **Update `UPSTREAM_BASE` in the same commit as the
port.**

### 5. `egui` + `egui-winit`: the Menu/Application key exists

Upstream's `Key` enum has no entry for the Menu key (the Application key next to
Right Ctrl), and egui-winit drops it in both key translations, so `key_pressed` could
never see it. taskman binds it (and Shift+F10) to "open the context menu of the
current selection".

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/data/key.rs` | `Key::ContextMenu` variant + `Key::ALL` + name mapping (accepts "ContextMenu"/"Menu"/"Apps") | The key must exist before it can be pressed. |
| `crates/egui-winit/src/lib.rs` | `NamedKey::ContextMenu` and `KeyCode::ContextMenu` map to it in `key_from_named_key` / `key_from_key_code` | Windows reports the Menu key as both; without either mapping the press dies in the backend. |
| `crates/egui/src/containers/popup.rs` | `Popup::show` remembers `PopupAnchor::Position(pos)` with `open_popup_at` and isolates popup-local scroll targets from surrounding `ScrollArea`s | Keyboard menus must persist across frames, and popup focus/navigation must never scroll the list behind the menu. |

The scroll-target isolation preserves any request that already belonged to the surrounding
UI, builds the popup with a clean target slot, then discards any unconsumed popup-local
request before returning to the surrounding content closure. A `ScrollArea` inside the popup
still consumes its own requests normally. This prevents row context menus from moving the
underlying virtualized list while keeping intentional outer scroll requests intact.

Droppable when upstream adds the key; the key-specific pieces sit with the command keys, so
a rebase conflict there is a prompt to check this table. Keep the popup scroll isolation
until upstream provides equivalent popup/scroll-area separation.

### 6. `egui`: real modal windows, and public accesskit reparenting

Upstream ships an `egui::Modal` container that taskman never used: it is a
separate widget, and it does not give a plain `egui::Window` the modality taskman needs
(a dialog must also BLOCK the page behind it, not just paint over it). Two small changes
instead:

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/containers/window.rs` | `Window::modal(bool)`; raises the area to `Order::Foreground` and calls `memory.set_modal_layer(area_layer_id)` | Every taskman dialog is a `Window`; modality has to be an option on it, and the app keeps choosing its own backdrop style. |
| `crates/egui/src/memory/mod.rs` | `Focus::modal_interested` collects the focusable ids met on the top modal layer in Tab order; `end_pass` redirects a pending focus request into the dialog and clamps Tab to its members | Without this, a focus request for a widget BEHIND an open dialog (e.g. the app handing focus back to a table row) lands behind it, and Tab walks out of the dialog into background chrome. |
| `crates/egui/src/memory/mod.rs` | `Memory::set_focus_lock_filter` allows setting filter when `has_focus(id)` without requiring `had_focus_last_frame(id)` | Widgets gaining focus mid-frame can immediately lock Tab and arrow keys on their landing frame, eliminating 1-frame spatial-arrow and Tab leakage. |
| `crates/egui/src/context.rs` | `register_accesskit_parent` made `pub`, and it repairs the parent link when it changes after the node was already built | taskman's rows/cards are deliberately NOT egui focus targets, so their accesskit nodes are attached to the collection by hand. The old `pub(crate)` signature plus insert-before-create ordering silently lost those nodes. |

The `end_pass` clamp is deliberately minimal: it only acts when a top modal layer exists
this frame, and it only redirects `id_next_frame` / `give_to_next`. Spatial focus movement
is untouched, so a taskman app-side key contract still owns what a given dialog does with
Tab — see `llm-wiki/current.md` § Keyboard interaction model.

Not droppable while taskman relies on it. A rebase that touches `Focus::end_pass` or
`Memory::interested_in_focus` is a prompt to check this table: both are hot paths where
upstream refactors freely.

### 7. `egui`: the text cursor blink restarts when the cursor appears

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/widgets/text_edit/state.rs` | `TextEditState::cursor_showing` (serde-skipped) | Remembers whether the cursor was painted on the previous run of that `TextEdit`. |
| `crates/egui/src/widgets/text_edit/builder.rs` | resets `last_interaction_time` on the first run that paints the cursor again | Upstream restarts the blink only on a click or an edit. Keyboard focus (taskman's Ctrl+F, F6, Tab hand-offs, focus restored after a dialog) and window activation landed in an arbitrary phase, so the focused search box showed no cursor for up to `off_duration`. |

Behaviour-only and tiny; droppable once upstream restarts the blink on focus gain.

### 8. `egui`: text selection preservation on secondary click and queryable label selection

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/text_selection/text_cursor_state.rs` | `PointerButton::Primary` instead of `any_pressed()` for drag/click start | Secondary (right) click must not collapse existing text selection to a point. Right click is used for context menus (Copy / Select all). |
| `crates/egui/src/text_selection/label_text_selection.rs` | Primary-only dragging, deselect only on primary click outside and when no popup is open, tracking `selected_text`, `active_selection()`, and `select_all_in_widget()` | Allows context menus to preserve and inspect selected text, copy it, or programmatically select all text in a label widget. |

### 9. `eframe`: do not register for raw device input

| File | Change | Why |
| --- | --- | --- |
| `crates/eframe/src/native/run.rs` | `create_event_loop()` calls `event_loop.listen_device_events(DeviceEvents::Never)` after `builder.build()` | winit registers all mice/keyboards for raw input with its default `DeviceEvents::WhenFocused`, so on Windows every mouse packet system-wide arrives as `WM_INPUT` while any of our windows is in the foreground, and winit dispatches up to three `DeviceEvent`s per packet. taskman does not need device events — `egui-winit` feeds pointer positions from window `CursorMoved` events; the raw `MouseMotion` deltas only add `Event::MouseMoved` (for `pointer.motion()`, which nothing in taskman reads) and repaint the UI. Measured on Windows 11 at 1 kHz mouse movement: a focused window with the cursor outside burned ~0.14 cores per second of movement in a minimal eframe app (and up to a full core once the tracked pointer position goes stale); with the registration removed the same movement costs ~0. |

One line through winit's public API; on macOS/Wayland/Android it is a no-op and on X11 it only drops the raw XI2 events.

**Behaviour change.** With device events off, eframe never emits `Event::MouseMoved` on Windows and X11, so `PointerState::motion()` stays `None` and `Response::drag_motion()` (documented as raw, unaccelerated, unclamped movement) falls back to the screen-clamped `pointer.delta()`. macOS and Wayland still deliver `DeviceEvent::MouseMotion`, so raw motion remains there. taskman reads neither, so nothing regresses here. The call is unconditional, so it is *not* upstreamable as is — upstream would need an opt-out (a `NativeOptions` field defaulting to today's behaviour).

**Coverage.** Pinned on Windows by `native::run::tests::event_loop_creation_unregisters_raw_mouse_and_keyboard_input`, which reads `GetRegisteredRawInputDevices` after `create_event_loop`. `eframe::create_native` with a caller-supplied event loop does not go through `create_event_loop` and is not covered; taskman uses `run_native`.

### 10. `egui-winit`: coalesce pointer-move repaints and skip non-changing moves

| File | Change | Why |
| --- | --- | --- |
| `crates/egui-winit/src/lib.rs` | the `WindowEvent::CursorMoved` arm no longer returns `repaint: true` unconditionally. A move is skipped entirely when its position in points is unchanged (sub-point jitter), when a trailing repaint is already armed (`has_armed_trailing`), or when the hover/interact targets under the pointer (`contains_pointer`, `click`, `drag` — catching `interact_radius` approaches) match the last painted frame (`Context::hit_test_last_pass`, #11, cached per `(pass_nr, last_pos)`) and the pointer remains inside any app-declared `POINTER_STABLE_RECT`. Remaining moves are coalesced by a `PointerMoveThrottle` to one frame per display refresh (monitor refresh rate cached for 2 s and invalidated on `Moved`/`Resized`/`ScaleFactorChanged`/`Focused(true)`, fallback 60 Hz, floor 2 ms), arming one trailing repaint via `Context::request_repaint_after_at_least_for` so the final position is always painted. Consecutive `Event::PointerMoved` entries at the tail of `egui_input.events` are coalesced when no pointer button is down. Moves while a pointer button is down or egui is using the pointer always repaint immediately. New public items: `POINTER_STABLE_RECT`, `set_pointer_stable_rect`, `clear_pointer_stable_rect`, `PointerMoveStats`, `pointer_move_stats_take` | Upstream runs a full UI pass (layout, tessellation, paint) per `CursorMoved`, so a mouse reporting at up to 1 kHz drives up to 1 kHz frames; Windows Task Manager only repaints when a hover actually changes. With the throttle, `POINTER_STABLE_RECT` sub-widget skip, refresh-interval cache, and trailing short-circuit, moving over blank areas or within a single stable table cell repaints never and avoids `EnumDisplaySettingsExW` / hit-test churn, while hover-driven frames elsewhere are capped at the monitor refresh rate |

The app sets `set_pointer_stable_rect` per frame where sub-widget pointer position changes output: table rows whose tooltip depends on the hovered cell/glyph pass that cell's or glyph's sub-rect, and chart hover readouts pass `Rect::NOTHING` so they keep repainting per move (throttled). Repeated calls in one frame intersect. The rect is `IdTypeMap` temp data cleared at frame start (`clear_pointer_stable_rect`). The trailing repaint goes through the existing `UserEvent::RequestRepaint` → `EventResult::RepaintAt` plumbing, so no `eframe` change is needed. The throttle lives per `egui_winit::State` (i.e. per window/viewport). **Behaviour change:** hover highlight and cursor-icon updates refresh only when the hovered/interact-radius target changes or exits the stable rect, and continuous pointer-anchored visuals at most once per display period — drags/clicks are unaffected (always immediate). Not upstreamable as is — upstream would want the cadence and the skip configurable. **Coverage:** `pointer_move_throttle_tests` in `egui-winit/src/lib.rs` pin the throttle decision table (first move immediate, dedupe, single armed trailing repaint, cadence restart, active-pointer bypass), `hover_target_change_detection` (including `interact_radius` proximity outside widget rect), `pointer_stable_rect_roundtrip_and_intersection`, `interval_cache_validity_and_invalidation`, `pointer_moved_coalescing_keeps_latest_and_preserves_button_order`, and `pointer_move_stats_record_decisions`.

### 11. `egui`: public hit-test of the previous pass's widgets and AccessKit status

| File | Change | Why |
| --- | --- | --- |
| `crates/egui/src/lib.rs` | re-export `hit_test::WidgetHits` | the type is returned by the new accessor but the `hit_test` module stays private |
| `crates/egui/src/context.rs` | new `Context::hit_test_last_pass(pos)` that runs the same `hit_test` over `prev_pass.widgets` (interactable layers only, sorted, layer transforms applied, `interact_radius`) that `begin_pass` runs for the live pointer; new `Context::is_accesskit_enabled()` | lets integrations answer "would the hovered widgets change if the pointer were at `pos`?" between frames without running a pass (#10), and lets the app log once if UI Automation activates AccessKit |

Plain oversight-style accessors, plausibly upstreamable. **Coverage:** exercised through `egui-winit`'s `hover_target_change_detection` test, which runs a real pass with a widget and checks same-widget/off-widget/interact-radius/blank comparisons.

### 12. `egui_software` + `eframe`: 64×64 tile damage tracking and per-frame stats

| File | Change | Why |
| --- | --- | --- |
| `crates/egui_software/src/damage.rs` | **new** — `DamageTracker`, `DamageStats`, 64×64 tile hashing, disjoint rect merging (`merge_dirty_tiles`), conservative per-shape pixel bounds, ABA-safe `Arc<Galley>` retention, content-hashed `Mesh`es | avoids clearing, rasterizing, and presenting all 3.6M pixels on hover changes or periodic ticks when only a small region changed, and skips painting/presenting completely when zero tiles changed |
| `crates/egui_software/src/texture.rs` | per-texture monotonic `generation` counter (bumped on full upload, non-atlas partial update, and free) | ensures texture replacements/icon loads invalidate affected tiles |
| `crates/egui_software/src/gamma.rs` | `TextGamma::gamma()` and `TextGamma::contrast()` getters | included in the frame damage key |
| `crates/egui_software/src/lib.rs` | `Painter::paint_shapes_damaged`, `paint_with_scissor`, shared `paint_flat_shapes_in_rects` | clears and repaints only disjoint dirty rects with strict per-rect scissor clipping across fast rects, text + underlines, and tessellated batches |
| `crates/eframe/src/native/software_integration.rs` | uses `paint_shapes_damaged`, forces full paint/present when `!has_presented_once \|\| !presented_while_visible \|\| buffer.age() != 1`, re-presents immediately once `post_rendering` reveals the initially hidden window, skips `present` when 0 tiles are dirty, calls `present_with_damage` for partial updates, records `SoftwareFrameStats` | eliminates full-window GDI `BitBlt`s when idle or partially damaged while preventing white unblitted tiles on startup/tray-restore/un-occlude |
| `crates/eframe/src/lib.rs`, `src/native/mod.rs` | `SoftwareFrameStats`, `software_frame_stats_take()` (stubbed when `software` feature is off) | exposes pass/paint/present timings and tile-damage counts for `TASKMAN_INPUT_STATS=1` |

**Coverage:** `egui_software`'s `damage::tests` (zero-damage identical frames, single-row change partial damage matching full paint bit-for-bit, draw-order swap detection, disjoint tile merging) and `tm-app`'s `render_snapshot::tests::damaged_repaint_matches_full_paint_on_real_widgets` (real `tablekit`/`chart`/`icons` widgets).

### 13. `eframe` + `egui-winit` + `egui-wgpu`: window-reliability hardening

Divergences added for the GUI-reliability audit. Everything here exists because a window
could end up blank, stale, invisible or unresponsive without any error surfacing.

| File | Change | Why |
| --- | --- | --- |
| `crates/eframe/src/native/software_integration.rs` | **Windows surface replaced**: `mod platform_surface` now owns a `CreateCompatibleDC` + top-down `CreateDIBSection` buffer instead of `softbuffer`, presents with per-rect `BitBlt` whose result is checked, `ValidateRect`s only what it blitted, merges `GetUpdateRect` into the present set (`rects_to_present`), returns `Result` from every GDI step, retries a failed resize/buffer/present after 50 ms instead of aborting, and skips running `ui` at all for hidden/minimized/0×0 windows after the first frame (`update_logic_only`). Non-Windows keeps `softbuffer` behind the same `FrameBuffer`/`SurfaceState` shape | softbuffer 0.4.8 ignored `BitBlt` failures and validated the whole window, so a failed blit froze garbage; a `WM_PAINT` with no dirty egui tiles skipped the blit entirely; GDI allocation asserted (fatal in release); and a paint error from inside `WM_PAINT` used to re-enter the paint |
| `crates/eframe/src/native/run.rs` | `throttle_invisible_repaint` (a burst is delayed to `last + 100 ms`, an idle window wakes immediately), `in_check_redraw` re-entrancy guard, `windows_next_repaint_times` cleared before painting | the invisible-window repaint loop could recurse into `check_redraw_requests` and throttled tray-restore/hotkey wakeups to 100 ms+ |
| `crates/eframe/src/native/{epi_integration,glow_integration,wgpu_integration}.rs` | `min_inner_size` clamped to the largest monitor's point size; `is_visible` also honours `window.is_visible()` and a logic-only reveal returns `EventResult::RepaintNext`; a transient `set_window` failure warns and retries instead of ending the loop; `post_rendering(window, reveal_first_frame)` does not reveal the window when the app itself asked for it to stay hidden in the same frame (`note_visibility_commands` in the software backend) | a 720×480 logical minimum exceeded small/high-zoom viewports, a revealed-but-unpainted window waited for an unrelated event, and a start-in-tray launch had its `ViewportCommand::Visible(false)` overridden by eframe's first-frame reveal — leaving the window visible with nothing left to hide it |
| `crates/egui-wgpu/src/winit.rs` | zero-size surfaces return instead of `expect`-ing a default config; a failed `recreate_surface` schedules a retry; `recreate_surface` creates the replacement **before** dropping the old surface | a lost surface whose recreation failed used to return with the map entry gone, so nothing ever retried and the paused window stayed stale |
| `crates/egui-winit/src/lib.rs` | focus loss clears `any_pointer_button_down`; a button release that arrives with `pointer_pos_in_points == None` is forwarded at the last known position and followed by `PointerGone` | Windows can take mouse capture away mid-drag (`WM_CAPTURECHANGED`, cursor leaving the window); egui then kept the button pressed forever |
| `crates/egui/src/input_state/mod.rs` + `crates/egui/src/data/input/event.rs` | new `Event::PointerButtonReleasedOutside { button }`, handled by `PointerState::begin_pass` as a release with `click: None` (and `WindowFocused(false)` now shares `release_all_buttons_without_click`); the state is cleared without a click | egui-winit synthesizes the release when the platform lost the capture, and a normal `PointerButton { pressed: false }` at the press position would register a **click** under `could_any_button_be_click` — activating a control the user never released over |

**Coverage:** `throttle_invisible_repaint_...` (eframe `run.rs`), `software_integration`'s
`rects_to_present`/`should_force_full_paint` tests, `mouse_release_outside_window_after_cursor_left_releases_held_button`
and `window_focus_loss_releases_held_pointer_buttons_without_click` (egui-winit/egui), plus
`tm-platform`'s `fit_rect_keeps_windows_fully_inside_the_desktop` and `screen_repairs_ignore_dead_handles`.

### 14. `egui-winit`: coalesce wheel repaint requests

| File | Change | Why |
| --- | --- | --- |
| `crates/egui-winit/src/lib.rs` | `WindowEvent::MouseWheel` no longer returns `repaint: true` unconditionally. A per-window `WheelRepaintThrottle` allows the first wheel event through immediately, then limits further wheel repaints to one per display refresh and arms exactly one trailing repaint through `Context::request_repaint_after_at_least_for`. Adjacent line-unit `Move` events with identical modifiers are summed componentwise in a `WheelEventBuffer`; pixel-unit and phased touchpad events remain individual events. Zero-delta Start/End/Cancel phases still request their delivery frame so touchpad scroll state cannot remain stale. `take_egui_input` supplies the measured display interval as `RawInput::predicted_dt`. Buffered input is flushed before any non-wheel event and again in `take_egui_input`, preserving unit, modifier, phase and click order while the trailing frame carries the final accumulated delta. New `WheelRepaintStats` / `wheel_repaint_stats_take` counters are emitted by `TASKMAN_INPUT_STATS=1` as wheel, coalesced, immediate and trailing counts | On Windows every wheel event caused a synchronous full UI pass, so a high-resolution or rapidly spinning wheel can run layout, tessellation, rasterization and presentation many times per display refresh. The cadence makes scrolling update at the rate that can be seen while keeping the immediate first response and a guaranteed final frame; line deltas are accumulated without changing horizontal/vertical direction, and point events are deliberately not summed because egui uses their per-event magnitude for touchpad smoothing |
| `crates/egui/src/context.rs` | `request_repaint_after_at_least_for` schedules without subtracting the predicted frame time. Input-driven follow-up frames use it, while ordinary at-most repaint requests retain upstream behavior | `request_repaint_after_for` intentionally wakes early by one predicted frame, which collapsed every sub-16 ms delay used by high-refresh input throttles to an immediate request |
| `crates/egui/src/input_state/mod.rs` + `wheel_state.rs` | wheel-only Move input no longer counts as an immediate repaint trigger; Start/End/Cancel still do. Remaining line-wheel smoothing requests one frame at `predicted_dt`; any non-wheel event still requests an immediate frame. Scroll and zoom smoothing are tracked separately, so modifier groups in one batch cannot recolor the whole batch. End commits a same-pass Move before resetting touch state | The input state used to schedule back-to-back frames for every smoothing pass, bypassing the wheel throttle and keeping the UI at event frequency instead of the display cadence; batched modifier changes could also apply the wrong zoom mode |
| `crates/tm-app/src/app.rs` | consumes `wheel_repaint_stats_take()` into the existing once-per-second `TASKMAN_INPUT_STATS=1` record | keeps the wheel decision counters in the same diagnostic line as pointer-move and frame timings, with no per-event logging |

**Coverage:** `wheel_repaint_tests` pins bursts and cadence, the final accumulated line
delta, mixed units/axes/modifiers and order, touchpad phase preservation, click boundaries,
zero-delta lifecycle phases, the exact callback delay on a high-refresh cadence, and the
diagnostics counters. `input_state` tests pin wheel-only cadence versus mixed immediate input,
zero-delta lifecycle delivery, Start+Move+End delta preservation, and per-event scroll/zoom
splitting. The context test proves egui no longer turns a sub-16 ms smoothing delay into zero,
and the winit test advances past an unconsumed trailing token to prove it cannot wedge later
wheel reports. The tests are deterministic and do not sleep.

### 15. `egui-wgpu` + software fallback: dual-source LCD text

The WGPU extension consumes the atlas contract from divergence #2. It is opt-in by atlas
contents and by device capability; with sub-pixel text off, or on a device without the
feature, the ordinary pipeline remains selected.

| File | Change | Why |
| --- | --- | --- |
| `crates/egui-wgpu/src/egui_lcd.wgsl` | **new** — dual-source fragment outputs, per-channel coverage blend, RGB/BGR swap, contrast curve, one-texel-per-pixel derivative/center test | A fixed-function GPU needs source color and per-channel source alpha separately; one scalar alpha cannot express LCD stripes |
| `crates/egui-wgpu/src/renderer.rs` | second optional pipeline; `ONE / ONE_MINUS_SRC1`; managed-atlas sub-pixel state; pipeline selection only for `TextureId::default()`; renderer-level order/contrast uniform | Keep non-font textures, color glyphs and unsupported devices on the byte-identical ordinary path |
| `crates/egui-wgpu/src/setup.rs` | request `DUAL_SOURCE_BLENDING` as `adapter.features() & DUAL_SOURCE_BLENDING` | Request only what the selected adapter advertises; never make device creation fail for an unsupported adapter |
| `crates/egui-wgpu/src/winit.rs` | publish current `TextOptions` to the renderer each frame; expose whether the LCD pipeline exists | The renderer needs the atlas order and weight but must not guess them from colors |
| `crates/egui_software/src/{raster.rs,texture.rs,lib.rs}` | transformed glyph triangles convert LCD texels to mean grayscale before filtering | Rotated/scaled/fractionally aligned software text must not smear color fringes |
| `crates/egui_kittest/tests/subpixel_text.rs` | **new** — headless GPU color/BGR/transform coverage with explicit no-adapter skip | Detect shader/pipeline regressions when a suitable WGPU adapter is available |

The GPU path carries the selected `TextOptions::text_contrast` and RGB/BGR order through a
uniform. It deliberately does not claim to implement the software renderer's nonlinear
destination gamma: fixed-function blending cannot read the destination. The software path
keeps its full gamma/contrast model.

**Coverage:** `egui-wgpu` unit tests cover feature detection/request, full/partial atlas
state transitions, default-texture-only selection, transform mapping and order/contrast
fields. `egui_software` covers grayscale conversion of LCD texels while leaving color
glyphs unchanged. The `egui_kittest` test is not part of the shipped fork gate but is
available as an explicit hardware check.

## Rebase runbook

```
git remote add egui-upstream https://github.com/emilk/egui   # once
GIT_LFS_SKIP_SMUDGE=1 git subtree pull --prefix=vendor/egui egui-upstream <new-tag> --squash
```

`GIT_LFS_SKIP_SMUDGE=1` is **required**: egui stores its demo/kittest snapshot PNGs in
git-LFS and those objects currently 404 from the upstream server. We never build those
crates, so checking the pointer files out instead is harmless — but without the variable the
subtree operation aborts partway.

### The snapshot stubs are deleted, and must stay deleted

`git subtree pull` re-adds ~230 git-LFS pointer stubs (demo/kittest/example snapshot PNGs
and GIFs). **Delete them again in the same commit.** They make the entire repository
unpushable, which is not a style preference:

```
remote: error: GH008: Your push referenced at least 214 unknown Git LFS objects
remote: ! [remote rejected] (pre-receive hook declined)
```

The objects behind those pointers exist nowhere — not here, not on upstream's LFS server,
which 404s them. GitHub validates every pointer in a push against the repository's LFS
store and declines the whole push when one is missing. There is no way to satisfy it: the
content is gone. Locally the same thing happens one step earlier, in git-LFS's own
`pre-push` hook (`Git LFS upload failed: (missing) ...`).

Neither check consults `.gitattributes`, so commenting the `*.png filter=lfs` rules out —
which this fork does, and which correctly stops a clone from trying to smudge them — has
no effect on either. Only the absence of the pointer FILES does.

They were removed from the fork's history with a `git filter-branch --index-filter` keyed
on **blob OID**, not on path. Two path-based passes both left files behind:
`git rev-list --objects` names each blob once, under whichever path it met first, so paths
sharing a blob are invisible to it — and the synthetic `Squashed 'vendor/egui/' content`
commit spells every path without the `vendor/egui/` prefix, so nothing matched there at
all. The OID set has neither problem. Nothing this workspace builds referenced them; the
full gate passes unchanged.

If a clone ever ends up with LFS active again (a stray `git lfs install`), this stops the
hook without touching history:

```bash
git lfs uninstall --local          # removes .git/hooks/{pre-push,post-*} and filter.lfs.*
git config --local lfs.allowincompletepush true
```

After pulling:

1. `cargo tree -d` from the parent repo — there must be no duplicated egui-repo crate. Two
   copies of egui produce errors like ``expected `egui::Context`, found `egui::Context` ``.
2. Re-check the divergence inventory above; drop anything upstream has adopted.
3. `tools/check-fork.ps1` (lints this workspace) **and** `python build.py --check` (lints the
   parent). Neither covers the other: `cargo clippy --workspace` in the parent does not deny
   warnings in an excluded path dependency.
4. The raw-input test in `cargo test -p eframe` (part of `tools/check-fork.ps1`) must stay green: it
   goes red when a conflict resolution loses the `listen_device_events(Never)` call (#9) or a winit
   bump changes where raw input is registered. Fix the call site; do not delete the test.

### Rebase tripwires

Things that are safe today but would silently break the sub-pixel path:

- **Atlas alpha sentinel.** LCD texels use `alpha = min(rgb)`, which works because
  premultiplied color glyphs never exceed alpha. A future color format that violates that
  invariant needs a real per-texel coverage/color tag; do not infer LCD coverage from color
  alone.
- **WGPU pipeline selection.** The LCD pipeline must remain restricted to
  `TextureId::default()`, a non-empty dual-source capability, the active sub-pixel mode and
  an atlas state that actually contains LCD texels. Relaxing any one of those conditions
  reinterprets ordinary textures or grayscale/color glyphs as coverage.
- **`TextOptions` gaining fields upstream.** It is compared by value to decide atlas
  invalidation; a new field that we do not thread through means stale atlases.
- **`epaint::Vertex` layout changes.** The software rasterizer reproduces the glow shader
  against this exact layout.
- **`RowVisuals` / `glyph_vertex_range` changes** in `text_layout.rs`. The software text path
  consumes that mesh directly; it is the reason our glyph geometry is provably identical to
  the GPU backends rather than a reimplementation.
