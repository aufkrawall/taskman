# TaskMan fork of winit

This directory is a vendored copy of [`winit`](https://github.com/rust-windowing/winit)
**0.30.13** (crates.io), reached only through `[patch.crates-io]` in the root
`Cargo.toml`. It exists for exactly one change.

## The change: window band for always-on-top

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
  - `create_window()` calls `create_window_native()` instead of
    `CreateWindowExW` directly.
  - Imports for `GetModuleHandleW`/`GetProcAddress`, `HINSTANCE`, `HMENU`,
    `WINDOW_EX_STYLE` and `WINDOW_STYLE`.

Nothing else is changed. `grep -n TASKMAN_WINDOW_BAND vendor/winit/src` lists
every taskman-specific line.

## Rebase procedure

1. Copy the new upstream crate over this directory (keep the deleted
   `Cargo.lock` deleted).
2. Re-apply the change above; the marker comment in `create_window_native`
   starts with `TaskMan fork:`.
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
