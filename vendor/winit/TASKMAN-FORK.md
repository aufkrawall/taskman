# TaskMan fork of winit

This directory is a vendored copy of [`winit`](https://github.com/rust-windowing/winit)
**0.30.13** (crates.io), reached only through `[patch.crates-io]` in the root
`Cargo.toml`. It carries four patches.

## 1. Window band for always-on-top

Native Windows Task Manager creates its main window with `CreateWindowInBand`
in **window band 16**. That band is above the Start menu's band 6, which is
why Task Manager's "Always on top" stays above the shell. Two properties were
measured on Windows 11 (2026-09-09):

- `SetWindowBand` cannot move an existing window into band 16
  (`ERROR_ACCESS_DENIED`), so the band must be chosen when the window is
  created.
- A band-16 window is **implicitly `WS_EX_TOPMOST` and cannot be demoted**
  (`SetWindowPos(HWND_NOTOPMOST)` and clearing the exstyle both leave the
  window topmost). Bands 2–14/17/18 are denied to normal processes; bands
  15/19/20 are invalid; only bands 0/1 (normal) and 16 (forced topmost) are
  available.

Because of the second property, `TASKMAN_WINDOW_BAND=16` must only be set when
always-on-top is enabled **at startup**. `crates/tm-app/src/main.rs` does that
based on the persisted setting, and `crates/tm-platform` clears the variable
once the window exists so child processes do not inherit it. The settings
dialog shows the existing "Takes effect at the next start." hint while the live
value differs from the startup value.

### Files touched

- `src/platform_impl/windows/window.rs`
  - `create_window_native()` reads `TASKMAN_WINDOW_BAND`, resolves
    `CreateWindowInBand` from `user32.dll` with `GetProcAddress` (it is not in
    the user32 import library — Task Manager resolves it dynamically too), and
    falls back to the stock `CreateWindowExW` path when the variable is unset,
    zero, or unparseable.
  - `init()` calls `create_window_native()` instead of `CreateWindowExW`
    directly. The call site carries a `TaskMan fork:` comment of its own so
    the rebase grep lands on it.
  - Imports for `GetModuleHandleW`/`GetProcAddress`, `HINSTANCE`, `HMENU`,
    `WINDOW_EX_STYLE` and `WINDOW_STYLE`, with the same comment on the
    `LibraryLoader` import: a reapply that restores only
    `create_window_native()` would otherwise leave the call and the imports
    untouched.

Nothing else in `windows/window.rs` is changed. The reapply grep is
`grep -n "TaskMan fork:" vendor/winit/src`, not `TASKMAN_WINDOW_BAND`: the
import comment at the top of the file and the `init()` call site both carry
the marker, and neither names the variable. `TASKMAN_WINDOW_BAND` is only the
runtime knob: setting it to `16` asks for band 16 at window creation.

## 2. Do not poison `EVENT_LOOP_CREATED` when `EventLoopBuilder::build()` fails

In `src/event_loop.rs`, `EventLoopBuilder::build()` swaps `EVENT_LOOP_CREATED` to
`true` before calling `platform_impl::EventLoop::new(...)`. When backend creation
returned `Err`, the flag stayed `true` even though no `EventLoop` existed, so
every subsequent `build()` call failed with `EventLoopError::RecreationAttempt`
(`"EventLoop can't be recreated"`), masking the real display-connection error.

### Files touched

- `src/event_loop.rs`
  - `EventLoopBuilder::build()` resets `EVENT_LOOP_CREATED` to `false` if
    `platform_impl::EventLoop::new()` returns `Err`, marked with `TaskMan fork:`.

## 3. Windows window reliability: paint, display change, capture, DPI

Everything here fixes a path where a window could be blank, stale or stuck without an
error surfacing. All of it is Windows-only and marked with a `TaskMan fork:` comment.

### Files touched

- `src/platform_impl/windows/event_loop.rs`
  - `WM_PAINT`, buffered redraw case: the update rect is captured with
    `GetUpdateRect` and the window re-invalidated with
    `RDW_INVALIDATE | RDW_INTERNALPAINT` instead of being validated away with
    nothing painted. This is what lets the software renderer blit exactly the
    region Windows asked for even when no egui tile changed.
  - `WM_DISPLAYCHANGE` and `WM_DWMCOMPOSITIONCHANGED` invalidate the whole
    client area (`InvalidateRgn(window, 0, FALSE)`), so a mode change that keeps
    the client size identical still produces a repaint.
  - `WM_CAPTURECHANGED` with a foreign `lparam` and a nonzero capture count sends
    `CursorLeft` and then synthesizes `MouseInput { Released }` for
    Left/Right/Middle/Back/Forward, so egui never keeps a drag pressed after
    Windows took the capture away. The `CursorLeft` is what makes egui take its
    "released outside" path: a release with a still-cached position would be
    eligible to register as a click (and a synthetic Left release during a
    middle-button drag could activate the control under the cursor).
  - `WM_DPICHANGED` no longer `expect`s the old inner rect; a failed
    `GetWindowRect` logs a `tracing::warn!`, restores the previous scale factor
    and skips the resize instead of panicking. Nothing that announces or applies
    the change happens on that path — no `ScaleFactorChanged`, no `InnerSizeWriter`
    resize, no `SetWindowPos` on the suggested rect — so publishing the new value
    alone would half-apply a DPI change (egui-winit re-reads
    `Window::scale_factor()` every frame, so the toolkit would move to the new
    point scale while the client area kept its old pixel size). Restoring also
    keeps the retry alive: with the new value stored, the next `WM_DPICHANGED`
    would take its `new_scale_factor == old_scale_factor` early return and never
    apply the change at all. The window keeps the previous scale — visibly stale
    on the new-DPI monitor — until the next event.
- `src/platform_impl/windows/window.rs`
  - `outer_position`, `inner_position`, `inner_size` and `outer_size` return
    `NotSupportedError`/zero sizes instead of panicking when
    `ClientToScreen`, `GetClientRect` or `GetWindowRect` fails, and subtract
    borders with `saturating_sub`.
  - **Note:** this file also carries the window-band change from §1; keep both
    when rebasing.

### Why these are fork-local

`WM_PAINT` buffering and the DPI accessors are upstream's design and are being
fixed upstream; the synthesized button release and the display-change
invalidation are defensible upstream too, but this fork needs them now and
carries them as one reviewed patch.

## 4. Wayland suspended toplevels report minimized

Wayland has no direct client query for window visibility or compositor minimize
state. xdg-shell v6 added the `SUSPENDED` toplevel state to tell a client that
repaint is suspended; SCTK already records it in `WindowConfigure.state`.

### Files touched

- `src/platform_impl/linux/wayland/window/state.rs`
  - `WindowState::is_suspended()` tests `XdgWindowState::SUSPENDED` on the last
    configure and has pure bitflags regression tests. It answers `Option<bool>`:
    `None` — winit's documented answer for this platform — whenever the state
    cannot be reported, i.e. before the first configure and on an xdg-shell older
    than v6, which has no `suspended` state to send. `reported_suspended()` is the
    pure rule behind it, tested for both `None` cases.
  - `WindowState::configure()` resets a pending frame-callback throttle when the
    `SUSPENDED` bit changes, so the configure-triggered redraw is delivered even
    when an invisible surface no longer receives its frame callback.
- `src/platform_impl/linux/wayland/window/mod.rs`
  - `Window::is_minimized()` returns `window_state.is_suspended()` instead of
    always returning `None`, so it forwards the `None` cases above unchanged.

Winit already queues a redraw for every configure, so eframe's visibility pass
observes both suspension and resume. v6's hint also covers occlusion and screen
locking, so `Some(true)` means "the compositor says this toplevel is not visible",
not strictly "minimized". A compositor that does not advertise xdg-shell v6's
suspended state keeps the `None` answer: reporting `Some(false)` there would
fabricate a measurement, and every hidden window would claim to be visible.

## Rebase procedure

1. Copy the new upstream crate over this directory (keep the deleted
   `Cargo.lock` deleted).
2. Re-apply the four changes above; the marker comments start with
   `TaskMan fork:` (`grep -n "TaskMan fork:" vendor/winit/src` lists them).
3. `pwsh tools/check-fork.ps1` (clippy + tests for this fork; `cargo fmt` is
   deliberately skipped because upstream's tree is not formatted with this
   nightly rustfmt).
4. `python build.py --check` for the full workspace gate.
5. Verify visually: enable always-on-top, open the Start menu, and confirm the
   TaskMan window stays above it (`GetWindowBand` on the root HWND must report
   16; see `llm-wiki/log/recent.md`, 2026-09-09).

## Why not upstream?

`winit` has no window-band API, and eframe/egui-winit cannot choose the band
either. Patching the one creation call is smaller and more predictable than
reparenting the window under a band-16 host or hooking `CreateWindowExW`.
