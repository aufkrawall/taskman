# Changelog

## Unreleased

### Fixed

- **Linux file I/O no longer invents per-file traffic.** The open-files view keeps read/write byte and operation totals unavailable unless a real per-file source measures them; a file position is no longer copied into the read or write columns. `/proc/<pid>/io` is exposed only through the explicit process-wide counter fields under an unavailable per-file accounting state, handle enumeration is identity-bracketed, deleted regular files retain their path/size/descriptor facts through `/proc/<pid>/fd`, and pipes, sockets and devices are not presented as disk files.
- **End process tree now works on Linux and respects process identity.** Descendants come from `/proc` parent links paired with creation ticks, are signalled child-first with `SIGTERM`, and are revalidated immediately before delivery (with `pidfd` where the kernel supports it). A recycled PID is never treated as part of the selected tree, a descendant that exits during the action counts as done, and no implicit `SIGKILL` is added.
- **A failed or skipped blit can no longer leave the window blank, stale or half-painted.** The Windows software renderer now presents through its own Win32 DIB surface: every `BitBlt` result is checked, only the rectangles that were actually blitted are validated, and a `WM_PAINT` update region is merged into the present set even when no egui tile changed. A failed resize, buffer access or present warns, marks the surface unpainted and retries in 50 ms instead of aborting (release builds abort on panic) or silently freezing the old pixels in place.
- **The window repaints after a display-mode change.** `WM_DISPLAYCHANGE` and a DWM composition change now invalidate the client area, so a resolution or refresh change that keeps the client size identical no longer leaves whatever was on screen until something else forces a redraw. A minimized, hidden or zero-sized window also stops running the UI pass, which used to keep a "painted" frame counter advancing while nothing could be seen.
- **Ctrl+Shift+Esc, the tray icon and a second launch only report success once the window is really usable.** The restore no longer acknowledges the request before the queued show/unminimize commands ran: it uncloaks after a frame was painted into the visible window (bounded to 8 frames / 2 s), re-checks DWM's cloak state instead of assuming the uncloak worked, repairs a window left off-screen, and raises itself above a lingering fullscreen game that is not itself `WS_EX_TOPMOST` (a persistently topmost game still outranks a non-topmost window; see `llm-wiki/known-debt.md`). A hidden window whose tray icon never appeared is restored automatically instead of disappearing for good, and close-to-tray now requires the icon to exist rather than merely its thread to have started.
- **Saved window placement survives mixed-DPI layouts and unplugged monitors.** The outer position is stored in physical pixels alongside the legacy logical `position=` line (which is still read and written), applied before the first frame, fitted into the desktop (or onto the nearest monitor's work area when the saved spot falls into a hole in a non-rectangular layout), and re-fitted when the monitor layout changes. A window deliberately spanning two monitors is left alone; only geometry that reaches outside every monitor is moved, and only a window larger than the whole desktop is shrunk. The 720×480 minimum is clamped to the largest monitor, and the UI zoom is applied when the window is created so a non-1.0 zoom no longer resizes the window on every restart.
- **Graphics resource failures no longer abort or wait forever.** GDI allocation returns an error instead of asserting, the software surface owns its device context (no allocator thread to wait on), and a D3D12 surface that cannot be recreated keeps its old entry and schedules a retry — previously the entry was dropped before the recreate attempt, so nothing ever retried and a paused window stayed stale. GDI surface sizes are bounded (64 megapixels) rather than trusted.
- **A drag no longer sticks when Windows takes the mouse capture away.** `WM_CAPTURECHANGED` now synthesizes the button releases, focus loss clears the held-button state, and a release that arrives after the pointer left the window clears the button too — without turning that interrupted press into a click that would activate whatever sat under the cursor.
- **The file pickers and shell actions no longer freeze the window.** The dump-destination picker, the Run dialog's **Browse…** and the shell calls behind **Open file location**, **Properties** and **Open link** (including `ms-settings:` jumps) run off the UI thread on the bounded action executor; the system dialogs keep the main window as their owner (so they cannot be buried behind it), the Run dialog stays responsive while a picker is open, and a cancelled pick is remembered correctly.
- **An action with nothing to report no longer shows an empty toast.** A success message that is empty (or only whitespace) is dropped, so **Open file location** and friends no longer draw an empty frame with a close button that could also push a real message out of the toast queue.
- **The Select columns dialog fits a short or high-zoom window.** Its list height is bounded by the viewport instead of a fixed 330 pt, so the dialog's **Close** button cannot end up below the bottom edge where nothing can reach it.
- **Linux icon themes no longer abort the GUI on a 16-bit PNG.** Theme decoding now normalizes every PNG to 8-bit RGBA (`EXPAND | STRIP_16`), bounds the decoder allocation, validates the decoded length before the texture upload, and alpha-weights box downscaling so transparent borders do not darken large icons into black halos. A legal 16-bit theme icon previously yielded 8 bytes per pixel and tripped `ColorImage::from_rgba_unmultiplied`'s length assert on the UI thread (release builds abort).
- **Start-in-tray no longer flashes the window while the tray icon is being created.** Every pending frame now asserts `Visible(false)`, so eframe's first-frame reveal cannot show the window before `TrayShell` is ready; the 3 s fallback still shows it when the icon can never appear.
- **A window restored into a hole in a non-rectangular monitor layout is moved back onto a real monitor.** `ensure_window_on_screen` now enumerates monitors: geometry that intersects no monitor is fitted to the nearest work area, while a window genuinely spanning two monitors keeps its size.
- **Startup impact no longer double-counts a recycled PID with an unknown identity.** The tracker subtracts a previous baseline only when the creation identity matches; an unknown current identity now resets the baseline instead of charging the new process with the old one's counters.
- **A release synthesized by capture loss can no longer activate the control under the cursor.** `WM_CAPTURECHANGED` now sends `CursorLeft` before the synthetic releases, so egui clears the held buttons without a click; this also covers a middle-button drag.
- **Details no longer offers Open file location / Properties for kernel pseudo-processes** (`Registry`, `Memory Compression`, ...). Both menus now use the same "can this name an image" rule as the icon column, so the entries are disabled exactly as the 0.1.32 notes say.
- **Collapsed Linux process groups keep their selection.** The aggregate row now uses `creation_identity()` (boot-relative ticks) instead of the wall-clock `start_epoch_s`, matching what `identity_is_live` compares.
- **`--mock` no longer writes fabricated usage into the real app-history or startup-impact store.** Both use in-memory stores in mock mode.
- **XDG desktop-file indexing visits every subdirectory and skips shared interpreters.** The scan now tracks depth per directory (three sibling subdirs all resolve), and a `python3`/`java`/`wine`/`flatpak` `Exec=` no longer claims that interpreter's process icon.
- **App History resolves bare command keys on Linux**, so a name-keyed row can use the XDG `.desktop` lookup instead of always painting the default icon.
- **The software renderer backs off after a failed reveal re-present** instead of requesting an immediate redraw every frame, and a restored minimized window is re-fitted after it is unminimized.
- **The icon texture upload budget is enforced per frame, not per row.** A cold start with hundreds of ready icons no longer uploads all of them on the first visible frame.
- **Interrupted atomic saves no longer leave temp files forever.** Stale `.<pid>-<seq>.*.tmp` files older than an hour are swept on the next save, while a recent temp (which may belong to a live concurrent save) is left alone.
- **The Run dialog's Browse button recovers from a dead picker worker.** If the worker unwinds without publishing a result, the slot is marked cancelled instead of leaving Browse inert until the dialog is reopened.

### Improved

- **A stored renderer preference is no longer silently rewritten.** Startup still coerces the value it actually uses on the current platform, but the user's explicit choice survives for a platform that supports it, and the coercion is logged once instead of being persisted as `cpu`.
- **The restore acknowledgement repairs placement before answering.** A minimized window restored after a monitor unplug is re-fitted after it is unminimized, not only on the cloaked path.

- **Text rendering follows the monitor it is actually on.** The startup ClearType query uses the real window handle, a FLAT pixel geometry (rotated or non-RGB-stripe panels) disables sub-pixel rendering, and the theme refresh key now covers the whole text-rendering state, so a change of the display's parameters re-rasterizes the font atlas instead of reusing glyphs built for the previous monitor.

### Changed

- **Broker protocol v12.** The creation-time token now travels as 100-ns ticks instead of seconds. The version bump makes a mixed GUI/service pair fail at the handshake with a clear incompatibility error instead of reporting every protected target as "process exited", and module-ETW revalidation accepts the mixed-precision token.
- **A post-start event-loop failure is reported as such** — "event loop failed after startup" — instead of "failed to start GUI". The original error text is still included.
- **The vendored-fork gate fails instead of silently skipping when PowerShell exists but cannot run**, and the probe has a 10 s timeout.

## 0.1.32 - 2026-11-21

### Improved

- **Graphics backend:** CPU rendering is the default on Windows and Linux. Settings can select D3D12 on Windows or Vulkan on Linux for the next launch. The GPU choices require a compatible hardware adapter. Initialization failures fall back to native CPU rendering, and Settings shows the active backend.

- **Every row icon now comes from the OS icon theme:** rows without an application icon no longer show a glyph of our own. On Windows and Linux, Processes, Details, Users, Startup and App History paint the platform's own default application icon instead — on Windows the shell's generic `.exe` icon (the "no icon defined" window glyph Explorer and Task Manager show), on Linux the desktop theme's `application-x-executable` icon. The accent-colored rounded square that `Memory Compression`, `System Interrupts`, `smss.exe` and other path-less kernel rows used to show is gone; icon-less system images (`lsass.exe`, `winlogon.exe`, `ntoskrnl.exe`) and path-less pseudo-processes now render the same OS glyph. Row icons are also never invented for a row that has no image. macOS has no OS icon source in this backend yet, so its icon cells stay empty (tracked in `llm-wiki/known-debt.md`).

### New

- **Linux application icons from the desktop icon theme:** on Linux, rows on the Processes, Details, Users, Startup and App History pages now resolve each application's icon through the freedesktop icon theme spec — the `.desktop` entry's `Icon=` matched by `Exec=` basename or desktop-file id across the XDG data dirs, then the current GTK icon theme (with its `Inherits` chain), `hicolor`, and the legacy pixmaps dir; PNG icons are decoded and capped at 64 px — instead of always showing the placeholder glyph.

### Fixed

- **Unix process actions:** Linux and macOS confirm that the sampled process generation remained the same across the telemetry refresh. Linux keeps the native boot-relative start ticks stable across system clock corrections.
- **Application icon loading:** Startup icon paths are resolved in the background. Linux resolves bare Startup commands and reads the current X11 session icon theme. Missing icons retain their retry delay when the texture cache is full.

- **App History rows no longer show Explorer's folder icon:** rows whose app had no known executable store a blank icon path, and the shell answers an empty name with the icon of the *current directory* — the yellow folder glyph. Blank history values are dropped during load, and bare names are filtered before Windows shell extraction. Those rows fall back to the running process’s icon or the OS default application icon. The Windows sampler also no longer stores an empty image path in the first place.
- **Process icons no longer randomly revert to the placeholder:** the icon texture cache evicted arbitrary (HashMap-iteration-order) entries once it passed its 512-icon cap, so on-screen rows could lose a just-loaded icon and flash the placeholder; eviction is now true LRU, a repaint is requested whenever textures are uploaded (the last batch no longer waits for an unrelated repaint), and a dead icon worker thread is respawned on the next request.
- **Windows shell icons no longer intermittently come back empty:** `SHGetFileInfoW` returns no icon when several threads extract at the same time, and needs a COM apartment on the calling thread; extraction is now serialized process-wide and initializes COM per thread, so a row does not lose its icon (and fall back to the generic one for 30 s) for no visible reason.
- **Kernel pseudo-process rows and the file actions:** the fabricated `System` → `ntoskrnl.exe` / `Registry` → `regedit.exe` name mapping is gone. It gave `Registry` an icon Windows itself does not show, and — because the same string doubled as the row's executable — enabled **Open file location** / **Properties** for processes that have no file (they failed or opened nothing). Pseudo-processes now show the OS default icon and those menu entries are disabled again, exactly as before.
- **Startup-page icons for env-var/`.lnk` commands on Windows:** the Startup tab's icon lookup now uses the same command resolver as the publisher/impact columns (expands `%VAR%`, resolves `.lnk` targets), so those entries get their real application icon instead of the placeholder.

- **Mock mode OS isolation:** `--mock` now wires an inert `MockActions` backend and skips the Windows Task Manager replacement check and Explorer restart path, preventing simulated UI actions from affecting real host processes or system state.
- **Concurrent settings and history save safety:** atomic file replacement (`replace_file`) now uses unique exclusive temp files (`create_new(true)` per PID/sequence) and cleans them up on failure instead of sharing a fixed `<name>.tmp` path across threads or multiple instances.
- **Installer shutdown PID-reuse TOCTOU:** `stop_running_gui` now opens each target `taskman.exe` process handle with query/terminate/synchronize rights first, verifies its full image path through that pinned handle, and terminates only the verified handle on timeout.
- **Session-scoped Windows Explorer restart:** Explorer restart now fails closed unless the target carries a verified creation timestamp matching the current desktop session's shell window (`GetShellWindow`), preventing cross-session Explorer termination from an elevated instance.
- **Sub-second PID-reuse identity protection:** process creation timestamps now preserve native 100-ns (`FILETIME` on Windows, microsecond `proc_pidinfo` on macOS, clock-tick `/proc/<pid>/stat` on Linux) resolution without ±1 s tolerance, `PlatformActions` checked/unchecked defaults fail closed cleanly, and Linux signals pin targets via `pidfd_open`/`pidfd_send_signal` (with identity-checked fallback when unavailable).
- **Recycled-PID process grouping:** Processes-tab tree parenting, flat same-image family grouping, Details search ancestor reveal, and same-image Windows service account inheritance now verify `is_plausible_parent`, keeping older orphan workers from attaching to a younger unrelated process or collapsing into shared `termination_targets`.
- **Windows service `ControlService` and restart errors:** stopping or restarting a service now passes writable `SERVICE_STATUS` storage instead of a null pointer to `ControlService`, and `Restart` propagates real stop failures rather than silently timing out in `wait_state`.
- **Renderer fallback, `EventLoop` recreation masking, and startup error diagnostics:**
  - `winit::event_loop::EventLoopBuilder::build()` now resets `EVENT_LOOP_CREATED` when backend initialization returns an error instead of leaving the process-wide guard set after no event loop was created.
  - GUI startup (`tm-app`) now aborts renderer fallback immediately on `eframe::Error::WinitEventLoop(_)` instead of retrying other renderers and overwriting the real display-server error with `EventLoop can't be recreated`, flushes early startup logs to disk on fatal failure, and reports a plain failure message naming each attempted renderer instead of `"no usable GPU renderer found: Some(...)"`.
  - Startup records which backend created the window and logs initialization failures before trying the next backend.
- **Linux glibc release cross-build discovery & static-musl startup hint:**
  - `build.py` now locates `zig` across `ZIG`, `PATH`, Python's `ziglang` package, and standard Windows install paths (`WinGet`, `Scoop`, `Chocolatey`, per-user/system Python), supplies `ZIG`/`PATH` to `cargo zigbuild`, cleans stale opposite-flavor (`-musl.tar.gz` vs glibc `.tar.gz`) Linux archives from `dist/`, and falls back from inaccessible WindowsApps `pwsh` aliases to `powershell` in `check_fork()`.
  - `tm-app` now appends an actionable static-musl note directing users to the glibc `linux-x86_64` build (or `--selfcheck`) whenever startup fails with `"Dynamic loading not supported"`.

## 0.1.31 - 2026-11-18

### Improved

- **"Restart" context-menu entry for Windows Explorer:** right-clicking a live `explorer.exe` row on the Processes or Details page now offers **Restart**, matching the top-bar Restart command shown when Explorer is selected — the same entry native Task Manager provides for the shell process.

- **Multi-engine GPU Performance page with Dedicated and Shared memory graphs:**
  - Replaced the single top GPU utilization graph with a 2×2 grid of per-engine charts (`3D`, `Copy`, `Video Encode`, and `Video Decode` by default, collapsing to one column on narrow windows), each with its own engine switcher dropdown, live percentage readout, and right-click menu. Added a persistent `Multiple engines (2×2)` toggle (`gpu_multi_engine`) and per-pane selections (`gpu_graph_engines`) in settings while keeping the single-graph mode available.
  - Added a **Shared GPU memory** rolling chart below **Dedicated GPU memory**, scaling against DXGI's `SharedSystemMemory` capacity on Windows, and updated the bottom GPU summary to show combined **GPU memory**, **Dedicated GPU memory**, **Shared GPU memory**, **Temperature** (via `D3DKMTQueryAdapterInfo` `KMTQAITYPE_ADAPTERPERFDATA`), **Driver date**, **DirectX version**, **Physical location**, **Hardware reserved** memory, and active non-grid engines.

### Fixed

- **"Delete usage history" confirmation dialog no longer opens oversized:** the dialog window now pins its outer size (420×124, stable window id) exactly like the End task confirmation, instead of falling back to egui's default window size (340×420), which made it render roughly three times taller than its content.

- **Background 30 FPS games (such as Fortnite) no longer falsely report "Not responding":**
  - Excluded non-interactive auxiliary top-level windows from application/hung tracking unless `WS_EX_APPWINDOW` is set: `WS_EX_NOACTIVATE` overlays/helpers, owned windows whose owner is already visible (`GW_OWNER`), `Shell_TrayWnd`, and non-desktop shell window bands (retaining UWP `ApplicationFrameWindow` / `CoreWindow` bands).
  - Resolved DWM `"Ghost"` windows back to the underlying hung window via `HungWindowFromGhostWindow` instead of blaming `dwm.exe`, and checked hosted UWP `CoreWindow` child handles directly instead of `ApplicationFrameHost.exe`'s shared frame.
  - Added bounded non-aborting `SendMessageTimeoutW(..., WM_NULL, SMTO_BLOCK, 200 ms)` verification (deduplicated per thread within a 600 ms scan budget) whenever `IsHungAppWindow` fires, so threads actively pumping at 30 FPS that trigger kernel `IsPumpingInputMsgs` false positives (e.g. via filtered `PeekMessage`) clear immediately instead of tripping `NotRespondingHold` while deadlocked/blocked and elevation-gated (`ERROR_ACCESS_DENIED`) hung windows remain detected.

- **In-window mouse movement no longer spins the CPU or repaints the full window:**
  - Stopped `egui`'s tooltip wait loop (`show_tooltips_only_when_still = false` in `tm-ui`) from requesting immediate `ControlFlow::Poll` repaints on every frame while the cursor moves over any tooltipped widget.
  - Replaced the row-wide `POINTER_TRACKING_HINT` with `POINTER_STABLE_RECT` (`egui-winit` #10) so table rows only repaint when crossing into a different cell or status glyph (and anchored row tooltips at the opening pointer position via `hover_text_at_fixed_pointer` instead of chasing cursor jitter), while charts (`Rect::NOTHING`) keep smooth readout tracking.
  - Fixed `hover_target_changed` in `egui-winit` to compare `(contains_pointer, click, drag)` signatures so approaching buttons within `interact_radius` still highlight, cached the previous pass's signature per `(pass_nr, pos)`, short-circuited coalesced moves before hit-testing, coalesced consecutive `PointerMoved` events when no button is down, and cached the display refresh interval for 2 s (invalidated on move/resize/scale/focus) to avoid repeated `EnumDisplaySettingsExW` calls.
  - Added 64×64 tile damage tracking in `egui_software` and `eframe`'s software backend (`present_with_damage` when `buffer.age() == 1`, skipping `present` on zero dirty tiles, and forcing a full present once the window is visible after startup or restore from tray/minimized/occluded) so hover changes and periodic refreshes no longer clear, rasterize, and `BitBlt` all pixels on screen.
  - Skipped installing the taskbar monitor's foreground `EVENT_OBJECT_LOCATIONCHANGE` hook on TaskMan's own process (`sync_foreground_hook` in `tm-platform`), notifying the monitor on TaskMan's own window moves/resizes instead, and avoided per-frame string allocations in `sync_taskbar_monitor`.
  - Added `TASKMAN_INPUT_STATS=1` diagnostics (per-second `CursorMoved` decision counts, software pass/paint/present timings, tile-damage counts, top `repaint_causes()`, and one-shot AccessKit activation logging).

## 0.1.30 - 2026-11-17

### New

- **2 s update speed option:** the Settings dialog's Update speed block now offers a `2 s` choice between Normal (1 s) and Low (4 s), in addition to the existing High, Normal, Low and Paused values. It persists in the config as `update_speed=2s` and makes the engine and update-speed-driven pages sample every two seconds; pages with a longer floor (Services: 5 s) keep that floor, and Paused still freezes updates.

### Fixed

- **Mouse movement outside the TaskMan window no longer raises CPU load while the window has focus:** winit registers all mice and keyboards for raw input with its default `DeviceEvents::WhenFocused`, so on Windows every mouse packet system-wide arrived as `WM_INPUT` while any TaskMan window was in the foreground — even with the cursor 100% outside the window — and each packet was dispatched through the UI thread (and could even trigger a full repaint when the tracked pointer position was stale). The vendored egui fork now unregisters raw device input when the event loop is created (`listen_device_events(DeviceEvents::Never)`); egui receives pointer movement from ordinary window cursor events, so hover, drags and tooltips are unchanged. Measured on the live app with synthetic ~5 kHz mouse movement outside the focused window: ~0.6 cores before, at idle level (~0.04–0.15 cores) after; at a realistic 1 kHz a minimal eframe app went from ~0.14 cores to ~0. Movement over the window itself still repaints per move and is unchanged.

## 0.1.29 - 2026-11-17

### Fixed

- **Process Properties I/O no longer disturbs the inspected process or mislabels its files:** reading each open file's position used to write that position back into the file object the target process shares with TaskMan, so a write the target made at that moment could be rewound; positions are now only queried. Access rights are now read from the handle actually inspected instead of from the earlier handle snapshot, so a handle the target closed and reused in between can no longer make a write-only file show up as read/write (with its byte counts then shown as unavailable).
- **Taskbar stats no longer blink out when a layout check fails:** a timed-out or momentarily empty UI Automation recheck of an unchanged taskbar (common while Explorer is busy or the system is loaded) used to discard the confirmed layout and hide the stats for at least half a second. The widget now stays in place, click-through, on its last confirmed layout while the scanner retries with backoff, and only hides if three consecutive rechecks fail (about 3.5 s). Taskbar geometry, DPI or identity changes still discard the layout immediately.
- **Taskbar stats stay visible with an auto-hide taskbar over maximized windows:** with auto-hide enabled, every maximized window's frame covers the whole monitor, so the widget treated it as a fullscreen app and stayed hidden when the taskbar was revealed. Maximized windows with a standard title bar no longer count as fullscreen, matching how Windows itself keeps revealing the taskbar over them.
- **Taskbar stats values no longer freeze during layout revalidation:** while taskbar button positions are being re-confirmed, new CPU/RAM/network values are now drawn in place at the unchanged footprint instead of waiting for the scan to finish.
- **No single-frame overdraw when the stats move under a menu or topmost window:** a new or growing cut-out (or a hide) is now applied before the moved or resized frame is presented, and a vanishing cut-out is removed only afterwards.
- **Taskbar stats occlusion handling is low-overhead:** occlusion checks reuse the occluder rectangles from the same window enumeration rather than running a second full `EnumWindows` pass, and sub-2-pixel jitter in a popup frame no longer reinstalls the window region, avoiding needless desktop compositor work.
- **Taskbar stats no longer vanish behind overlapping windows:** a partially covered widget now stays visible and is clipped around each overlapping window's frame with a Win32 window region instead of hiding entirely, so the Windows 11 taskbar right-click menu and an always-on-top Task Manager moved a few pixels over the display no longer blank the whole overlay. It still hides when fully covered, when the tray is missing, or when clipping is unavailable, and clears the region as soon as the occluder disappears. The invisible XAML drop-shadow that Windows 11 taskbar flyouts render inside their HWND frame is no longer subtracted from the widget region, so opening the taskbar context menu no longer cuts off the top row of the stats.
- **Taskbar monitor transient disappearances on window switches and tray updates:**
  - Stopped `TrayNotifyWnd` width/position adjustments (such as IME mode indicators, microphone/location privacy icons, or notification tray icons appearing/disappearing when switching windows) from invalidating the UI Automation occupancy cache and hiding the taskbar stats for `120–280 ms`. Only core taskbar geometry changes (`Shell_TrayWnd` identity, `taskbar_rect`, or DPI) now force a hard cache reset; `TrayNotifyWnd`-only changes keep the widget visible, immediately recompute placement against the updated `tray_notify_rect` and cached button rectangles in click-through mode (`WS_EX_TRANSPARENT`), and revalidate via UI Automation in the background.
  - Hardened the background UI Automation scanner against COM timeouts, transient scan failures, and burst starvation: raised `CUIAutomation8` connection/transaction timeouts from `50 ms / 80 ms` to `150 ms / 250 ms`, added immediate one-time session re-initialization on the first scan failure (including any per-element `GetElement` / `CachedBoundingRectangle` failure so partial button lists are never published as complete) before entering progressive backoff, and allowed completed post-invalidation scans superseded by a soft recheck to populate an empty cache (`current = false`) rather than leaving the cache empty across event bursts.
  - Eliminated false fullscreen and occlusion hides: scoped `SHQueryUserNotificationState` (`QUNS_RUNNING_D3D_FULL_SCREEN` / `QUNS_PRESENTATION_MODE`) suppression to foreground windows actually ordered above `Shell_TrayWnd` on the taskbar's monitor, ignored click-through/alpha-0 foreground overlays, verified `saw_tray` (With a mid-scan band-change retry) before trusting a Band-1 occluder in `EnumWindows` so transient `Shell_TrayWnd` promotion to Band 6 (`ZBID_IMMERSIVE_MOGO`) cannot misclassify normal Band-1 windows as above-tray occluders, and armed the 500 ms recovery timer whenever `tray_missing_in_enum` hides the widget.
  - Hooked `EVENT_OBJECT_DESTROY` in the global visibility hook (`LIVE_OCCLUDER_HWND`) so destroyed popup menus and tooltips wake the taskbar monitor immediately even when Windows omits `EVENT_OBJECT_HIDE`, scoped `EVENT_SYSTEM_MOVESIZEEND` layout rechecks to `Shell_TrayWnd`, allowed `TaskbarCreated` / `SHELLHOOK` / theme broadcasts through UIPI when running elevated (`ChangeWindowMessageFilterEx`), and cleared stuck hover highlights when a modal popup steals `WM_MOUSELEAVE`.
  - Filtered non-finite (`NaN` / `±∞`) CPU, RAM, and network rate values to unavailable (`"—"`) instead of fabricating `"0 B/s"`.

### Improved

- **Taskbar stats cost Explorer far less work on window open/close:** taskbar layout rechecks triggered by shell and taskbar notifications are now debounced into a single scan once the burst has been quiet for 300 ms (at most 1 s later), instead of one cross-process UI Automation walk of the taskbar every 80 ms during the burst plus a separate settle rescan. Each walk was measured at roughly 15 ms and about 9 ms of Explorer CPU; startup, invalidation, failure retries and returning from fullscreen still scan immediately, and no scans run while a fullscreen app suppresses the widget.
- **Half the mouse-move wakeups while a fullscreen game is in front:** the foreground-window location hook also receives every cursor move (about 100 M CPU cycles per second of continuous mouse movement). The fullscreen suppressor's thread no longer gets a second, identical location hook when it is the foreground thread (the usual game case); the dedicated hook is installed only when foreground moves to another thread.
- **Fewer cross-process calls per taskbar refresh:** the auto-hide state (`ABM_GETSTATE`, a synchronous message to Explorer that waits on a hung Explorer) is only queried when the taskbar geometry actually looks slid away, and the session-wide fullscreen state query only runs when the foreground window is ordered above the taskbar.
- **Diagnostics for disappearances that are not hides:** a DWM/shell cloak of the taskbar stats window itself (for example during a virtual-desktop switch animation) is now logged at debug level with its cloak reason; the system-wide hooks skip TaskMan's own windows, so these were previously invisible in the logs.
- **Taskbar monitor CPU/GPU overhead and engine sampling isolation:** Unified the per-refresh Z-order, fullscreen, occlusion, and topmost-thread `EnumWindows` passes into a single stack-buffered pass (`ClassNameBuf`, checking `DWMWA_CLOAKED` before `OpenProcess`), cached DirectWrite text formats, static glyph layouts (`↑`, `↓`, `CPU:`, `RAM:`, `C:`, `R:`), sample widths, rendering params, and Direct2D brushes across ticks, skipped redundant DirectWrite layout measurement when metrics and core geometry are unchanged, unhooked topmost-thread WinEvent hooks while disabled or fullscreen-suppressed, and stopped taskbar monitor visibility transitions from re-applying identical sampling intervals or forcing off-schedule full-system engine refreshes while the main TaskMan window is already open.

## 0.1.28 - 2026-11-16

### Improved

- **Faster taskbar monitor reappearance when Alt+Tabbing out of fullscreen:** Alt+Tabbing or switching away from a fullscreen window now restores the taskbar stats display immediately whenever the taskbar geometry and DPI are unchanged, instead of discarding the cached layout and waiting for a fresh UI Automation scan. The widget reappears click-through (`WS_EX_TRANSPARENT`) on its cached placement while UI Automation verifies taskbar button positions in the background, tracks the suppressing fullscreen window across focus handoff, and runs a short bounded transition check if Windows finishes clearing fullscreen Z-order or D3D state slightly after focus changes. Changed taskbar geometry, failed scans, or a full taskbar still wait for fresh UI Automation confirmation.

### Fixed

- **Taskbar monitor hidden during Windows 11 Task View (`Win+Tab`):** kept the taskbar stats display visible when opening Task View / virtual desktop overview by treating Windows 11 Task View overlay canvases (`XamlExplorerHostIslandWindow`, `MultitaskingViewFrame`, and `Windows.UI.Core.CoreWindow`) owned by `%SystemRoot%\explorer.exe` and verified Windows shell host binaries as non-occluding overlays, while still suppressing or hiding behind regular apps, fullscreen File Explorer (`CabinetWClass`), menus, tray overflow, and tooltips.
- **Delayed taskbar monitor reappearance after closing a fullscreen window:** hooked object lifecycle (`EVENT_OBJECT_DESTROY`..`EVENT_OBJECT_HIDE`) and cloaking (`EVENT_OBJECT_CLOAKED`..`EVENT_OBJECT_UNCLOAKED`) events on the tracked fullscreen suppressor thread plus top-level cloak/uncloak events, queued immediate fullscreen wakes on hide/cloak/destroy transitions, and tuned the bounded fullscreen-exit transition recheck (`10 ms × 70`) so slower window teardown or D3D fullscreen exit does not fall back to the `300 ms` settle timer or `5 s` hidden interval.
- **Taskbar monitor UIA failure spin, re-entrancy safety, and background topmost occlusion:**
  - Stopped repeated 80 ms UI Automation retries and message-thread wakes when taskbar button layout is unavailable across consecutive scans (`None` before and after), applying progressive failure backoff (`500` ms -> `1` s -> `2` s -> `5` s) queued directly inside the UIA worker so recovery continues even when sampling is paused while still waking immediately whenever valid button geometry disappears or a geometry change arrives.
  - Eliminated Rust aliasing UB (`&mut WindowCtx` across synchronously re-entrant Win32 calls and `TrackPopupMenuEx`'s modal menu loop) in the taskbar monitor window procedure via `RefCell` interior mutability (`WndProcState`), queuing re-entrant refresh/layout/theme/close flags until the outer borrow drops and guarding against nested context menus.
  - Added a bounded 500 ms topmost-occlusion check (`TOPMOST_RECHECK_TIMER_ID`) active only while the Band-16 widget is visible or occlusion-hidden so background windows promoted to `HWND_TOPMOST` without move, size, activation, WinEvent, or `SHELLHOOK` notifications are detected, and clear occlusion of the blocked placement candidate (`last_occluded_rect` before `last_rect()`) restores visibility cleanly even when paused.
- **Taskbar monitor visibility with Windows 11 Start, Search, and shell flyouts open:** Opening the Windows 11 Start Menu, Search, or tray flyouts no longer hides the taskbar stats. On Windows 11, opening Start or Search promotes `Shell_TrayWnd` into immersive band 6 (`ZBID_IMMERSIVE_MOGO`), which `EnumWindows` omits from non-immersive callers (`tray_missing_in_enum`), and places `Windows.UI.Core.CoreWindow` overlay canvases (`StartMenuExperienceHost.exe`, `SearchHost.exe`, `SearchApp.exe`, `ShellExperienceHost.exe`, `ShellHost.exe`) in band 6 above the taskbar. Occlusion and fullscreen scans now skip lower-band windows (`band < tray_band`), keep Band-16 stats visible while a live `Shell_TrayWnd` resides in an elevated band (> 1) omitted by `EnumWindows`, ignore those shell host `CoreWindow` canvases (while still hiding behind genuine overlapping popup menus such as `#32768` or `Xaml_WindowedPopupClass`), and ignore cross-band higher windows in band-1 fallback ordering.
- **Taskbar monitor focus flicker eliminated via elevated window band:** Clicking the taskbar after another window had focus no longer flashes or briefly hides the taskbar stats text. On Windows hosts supporting `CreateWindowInBand`, the unowned taskbar monitor now runs in window band 16 (`ZBID_GENUINE_WINDOWS`) instead of sharing band 1 with `Shell_TrayWnd`, preventing taskbar activation from momentarily raising `Shell_TrayWnd` over the widget before async hooks run—without Explorer ownership or input-queue attachment. Overlapping popup menus, tooltips, and always-on-top windows above the taskbar hide the widget while overlapping its footprint, its own right-click context menu anchors just outside the taskbar edge so menus stay unobscured, and hosts denying band 16 fall back cleanly to standard band-1 Z-order tracking.
- **Taskbar monitor click flicker:** Opening/restoring TaskMan and taskbar control notifications now keep the existing display visible and stationary while layout is revalidated, temporarily passing clicks through to protect controls underneath. Fresh results restore interaction or reposition/hide the display; failed scans and actual taskbar geometry changes still hide it. Focus-time stacking repair runs before queued layout/render work, ignoring hidden/cloaked bookkeeping windows while staying below visible popups. The monitor remains unowned and keeps its input queue independent of Explorer.
- **Taskbar monitor isolation and background work:** The Windows taskbar monitor no longer shares an input queue with Explorer through cross-process window ownership. It follows taskbar Z-order without modifying Explorer, hides if safe positioning is denied, and restores the five-second background sampling interval whenever both the main window and taskbar monitor are hidden. Showing the monitor requests a fresh sample while preserving Pause.
- **Taskbar button access and visibility checks:** The monitor stays hidden until UI Automation confirms taskbar occupancy, discards failed or stale layout results, and no longer places itself over buttons when layout information is unavailable. Z-order regression checks now handle desktops with more than 128 intervening windows.

## 0.1.27 - 2026-11-15

### Fixed

- **Taskbar monitor vertical, small-bar, and accent-color handling:** The Windows taskbar performance monitor now stacks network and CPU/RAM sections vertically when a left- or right-pinned vertical taskbar is narrower than side-by-side columns, steps down font size (down to `6.5` pt) when needed to fit short horizontal or narrow vertical taskbars without clipping or `clamp` panics, and hides cleanly rather than overflowing if the taskbar remains too narrow. In `Auto` color mode, contrast detection now also evaluates Windows `ColorPrevalence` and `DWM\AccentColor` luminance on `WM_SETTINGCHANGE` and `WM_DWMCOLORIZATIONCOLORCHANGED` so dark or light accent-colored taskbars get matching text contrast without screen pixel reads.
- **Taskbar monitor visibility while holding Alt+Tab in fullscreen apps:** The Windows taskbar performance monitor no longer pops up while holding `Alt+Tab` over a fullscreen game or app when the taskbar itself remains hidden behind the fullscreen window. Visibility detection now checks Z-order for uncloaked, non-transparent fullscreen windows still sitting above `Shell_TrayWnd` even after the task switcher takes foreground focus, and non-topmost demotion keeps the widget anchored right above `Shell_TrayWnd`.

## 0.1.26 - 2026-11-14

### Improved

- **Collision-free Windows taskbar monitor placement:** The taskbar stats overlay now inspects visible `Shell_TrayWnd` UI Automation bounds on a dedicated event-driven MTA thread and places itself into open taskbar space—stepping past XAML Widgets/Copilot controls beside the tray, relocating with hysteresis to an open alternate gap (such as the left side of a crowded centered Windows 11 taskbar), switching to a narrower compact layout (`C`/`R` labels) when tight, and cleanly hiding instead of overlapping taskbar buttons if the taskbar fills end-to-end (with `TrayNotifyWnd`-relative fallback if UIA is unavailable).

## 0.1.25 - 2026-11-14

### New

- **Windows taskbar performance monitor:** Added an optional taskbar overlay in **Settings** that renders live CPU usage, RAM usage, and total network upload/download rates next to the notification tray using software Direct2D/DirectWrite + `UpdateLayeredWindow` (without `GetDC(NULL)`/`GetPixel` screen reads or Explorer `SetParent` hooks). Supports toggling individual metrics, font family/size, network speed units (`Auto`, `KB/s`/`Kbps`, `MB/s`/`Mbps`, Bytes/s vs bits/s), and `Auto` (taskbar theme) or custom RGB text color; automatically hides when the taskbar auto-hides or a fullscreen window covers its monitor, and keeps TaskMan alive in the system tray on window close while enabled.
- **Row spacing option in Processes and Details overflow menus:** Added a **Row spacing** (**Normal** / **Compact**) submenu to the top-right `…` menu on both the Processes and Details pages. Processes defaults to **Normal** (32 px) and Details defaults to **Compact** (20 px), and each page's choice is persisted independently in `config.ini`.

### Changed

- **Installer post-install launch enabled by default:** The "Launch Task Manager when setup completes" checkbox in `taskman-setup.exe` is now ticked by default (and silent CLI installs launch Task Manager unless `--no-launch` is passed).

## 0.1.24 - 2026-11-10

### New

- **Per-file read and written byte tracking in Process Properties I/O dialog:** Added a sortable file activity table below the I/O graphs and summary counters in the Process Properties dialog. The table enumerates open and active files for the inspected process, displaying exact amounts read and written, total transfer size, and on-disk file size. Users can interactively sort by written most, read from most, total transfer, file size, or filename (toggling ascending/descending order with indicators), filter files with a live search box, and access context-menu actions to open the file location in Explorer, copy the path, or open file properties.
- **Process Properties I/O page with inter-process (IPC) telemetry:** Added a dedicated "I/O" tab to the Process Properties dialog featuring Process Hacker / System Informer style rolling history graphs for Read (yellow), Write (purple), and Inter-Process / Other (cyan) I/O, along with a "Disk & Inter-Process I/O" table breaking down cumulative operation counts (Reads, Writes, Other), cumulative transfer bytes, interval deltas, and current transfer rates. Inter-process I/O captures operations not involving physical disks (such as named pipes, ALPC, device controls/IOCTLs, and socket communications) read directly from the Windows kernel process table's `IO_COUNTERS` block without requiring elevated process handles.

### Improved

- **Process Properties title and heading process name alignment:** The title bar of the Process Properties dialog and the program heading on the General tab now display the executable process name (e.g. `explorer.exe`) rather than the localized process description (e.g. `Windows-Explorer`).
- **Process Properties I/O tab cleanup and files table path column:** Removed the redundant "Options" dropdown menu button and the elevation "Protection" label along with the vertical gap below the I/O summary table. The files table now includes a dedicated "Path" column with interactive sorting and truncated path display, and right-clicking any table entry opens a context menu with "Copy" (copying active text selection or cell text), "Select all" (selecting text in that entry), and "Open file location" (opening Windows Explorer with the target file preselected).
- **Parent process name and binary display on Process Properties:** The Parent process row on the General tab of the Process Properties dialog now displays both the friendly process name and the executable/binary name alongside the PID (e.g. `Services and Controller app (services.exe) (1496)` or `Userinit Logon Application (userinit.exe) (5428) (Non-existent process)`), rather than only the friendly description or a raw numeric PID. For elevated processes, system services, and processes in other sessions, parent process resolution routes through the authenticated LocalSystem elevation service (`TaskmanCore`) via protocol v11 `ProcessSecurityInfo` with verified creation identity, resolving the parent process handle, image path, binary name, and FileDescription even when non-elevated GUI tokens are denied access. When the parent process has terminated (such as `userinit.exe` for `explorer.exe`), resolution automatically falls back from standard Win32 format to native NT image paths (`PROCESS_NAME_NATIVE`) on lingering kernel process objects and inspects `GetExitCodeProcess != STILL_ACTIVE`, accurately discovering the parent binary name and marking it as terminated, supported by an in-session bounded process history cache and `CpuLoadSampler` kernel-table parent PID fallbacks.
- **Process Properties I/O graphs theming and pinned hover readout:** Process Properties I/O tab graphs now match the styling and theming of the Performance page for disks: dark/light adaptive sunken inset card surfaces (`pal.card_bg_sunken` to visually distinguish chart cells from the modal dialog window background), subtle gridlines, and coordinated series colors (green for Read, amber for Write, and teal for Inter-Process/Other) with translucent area fills, replacing retro green-on-black canvases. Cursor hover tooltips now appear directly at the pointer position with series swatches, and stay pinned to the inspected sample timestamp across rolling graph updates until the pointer is moved again. In the Disk & Inter-Process I/O table, the values for the trailing "Other (IPC)" column now use measured bounded cell allocations so their spacing matches the clean 16 px gap of the Read and Write columns instead of stretching to the window border.

### Fixed

- **Process Properties I/O handle access for services and system processes:** Resolved `OpenProcess: Zugriff verweigert (0x80070005)` errors when opening the I/O tab for services (such as background services) or system processes. File I/O queries now route through the authenticated LocalSystem elevation service (`TaskmanCore`) via protocol v10 `ProcessFileIo` endpoint, allowing an unelevated GUI to inspect open files and read/write byte positions for services. For processes where handle duplication is restricted by the kernel (such as PID 4 System or protected PPL processes), queries fall back to reading process-level kernel `IO_COUNTERS` via `PROCESS_QUERY_LIMITED_INFORMATION` or `SystemProcessInformation` without failing, rendering file availability as unavailable rather than reporting an error.
- **Process Properties I/O per-file attribution honesty and accuracy:** Eliminated false byte counts in the file list on the Process Properties I/O tab, where write-only file handles on existing files previously fabricated their entire file size as written bytes, and read/write handles duplicated their seek pointer into both columns and double-counted in total. Handle pointer offsets are now strictly separated from cumulative transfer counts, read/write access is accurately differentiated, and an explanatory tooltip and non-file I/O notice clarify why kernel process I/O (network sockets, named pipes, IPC, and closed temporary files) in the graphs above exceeds the open disk file handles below.


## 0.1.23 - 2026-11-03

### New

- **Batch process dump creation for multi-selections:** When multiple processes
  are selected in either the Details or Processes tabs (or when right-clicking an
  aggregate process group), the "Create dump file (N)" command prompts for a target
  directory via a folder picker and creates memory dumps sequentially for all
  selected processes (`<name>_<pid>_<timestamp>.dmp`). A live progress modal
  shows current file progress, overall completion (`[i/N]`), transfer rate, and
  responsive cooperative cancellation.

### Improved

- **Process command-line hover for all processes:** Hovering over the Name or Command Line columns on the Processes and Details tabs now reliably spawns a tooltip for every concrete process, falling back to the executable path or image name if an explicit command line was not recorded or restricted, while keeping uncollapsed process groups quiet and preventing truncated names from suppressing command-line visibility.

### Fixed

- **Battle.net name rendering and Unicode dot leaders:** The friendly process description for Battle.net (`Battle.net.exe`) previously rendered a missing glyph tofu box placeholder (`▢`) instead of a period because Blizzard's PE `FileDescription` metadata uses the Unicode ONE DOT LEADER (`\u{2024}`) rather than an ASCII period, and Windows' `Segoe UI Variable` lacks that glyph. Version string extraction now normalizes Unicode dot leaders (`\u{2024}`) to ASCII periods for reliable rendering, search filtering, online searches, and clipboard copying; in addition, `Segoe UI Symbol` (`seguisym.ttf`) has been added as a system fallback font in `tm-ui` so arbitrary punctuation and mathematical symbols in process names and telemetry render properly.
- **Process properties text selection and context menu:** Selecting text with the left mouse button across labels and property rows in the Process Properties dialog was previously discarded immediately upon right-clicking because pointer interaction unconditionally collapsed the selection. Right-clicking now preserves the active selection and opens a context menu offering "Copy" (copying the marked text, or the row's full value if nothing is selected) and "Select all" (selecting all text in that property row).
- **Foreground and input focus over fullscreen 3D games when restored from minimized:** When TaskMan
  was evoked via Ctrl+Shift+Esc while minimized to the taskbar and a borderless or exclusive
  fullscreen 3D game (such as DOOM Eternal) held focus, the window would visually render on top
  but input and mouse clicks were still captured by the game because Windows denied foreground
  transfer to a target thread without merged input rights and cursor clipping remained active.
  `force_foreground_attached` now merges both the worker and target threads with the foreground
  queue for the activation, waits for the asynchronous transition to complete before detaching,
  and releases Win32 cursor clipping (`ClipCursor(None)`).
- **Search bar focus indication on window restore/activation:** Fixed an issue
  where the search bar intermittently lacked its blue focus ring and blinking text
  caret despite accepting keyboard input when the window was activated or restored
  from the system tray, taskbar, or title bar. Synchronizing Win32 focus on the
  foreground window ensures viewport focus state and caret animation reflect
  active keyboard focus immediately.
- **TCP listener state and remote peer on Windows 11:** Listening sockets
  on modern Windows 11 builds, which report `dwState = 0` via `GetExtendedTcpTable`,
  are now properly recognized as `Listen` rather than raw state `0`, and placeholder
  endpoints (`0.0.0.0:0` or `[::]:0`) are no longer exposed as active remote peers.
- **Table hover tooltips:** Hovering over metric columns (such as CPU, Memory, Disk,
  or Network) in the Processes and Details tabs no longer displays the process command-line
  tooltip over the entire row, restricting command-line tooltips to the Name and Command Line
  columns.
- **Process metadata consistency across timestamp skew:** Base priority, session ID,
  working set, handle count, and thread count in `CpuLoadAccountant` now tolerate the same
  1-second rounding discrepancy between `sysinfo` process creation times and native kernel
  process tables as process lifetime checks, preventing intermittent telemetry drops.
- **App history multi-core scaling:** Fallback CPU usage accumulation when cumulative
  process CPU time is unavailable now scales system-normalized CPU percentages by the
  machine's logical core count, avoiding core-count-factor under-reporting.

## 0.1.22 - 2026-10-26

### New

- **Thread search:** Process Properties now has the same live filter-style
  search box on Threads that Modules already offered. It matches decimal and
  hexadecimal thread IDs, hexadecimal start addresses, `module+offset`, the
  module path, priority, state, activity, CPU percentage, and CPU time.

### Improved

- **Process command-line hover:** Processes and Details show an available
  command line when hovering a process, even when its text is not clipped.
- **Thread start addresses for elevated processes:** Process Properties now
  asks the authenticated LocalSystem core service for thread start addresses
  and cycle counters that the unelevated GUI cannot read itself. This fills
  the previously blank fields for targets such as Windows Task Manager while
  preserving honest "—" values when Windows still refuses access. The page
  reports how many thread records remain partial, sorts Start address by the
  numeric address instead of the displayed module name, and keeps the
  handle-free local read as a fallback when the service is unavailable.
- **Executing-module evidence:** Modules now shows Code samples and Sample
  share from an on-demand privileged system-profile ETW window. Samples are
  attributed to the module ranges of the exact process, so an injected hook or
  overlay such as RTSS's `rtsshooks64.dll` can show execution evidence even
  when it starts no threads of its own. These are sampled execution/share
  evidence, not exact CPU-time attribution; lost or unknown ETW loss renders
  counts as lower bounds and never turns missing evidence into a zero.
- **Protected-process module inventory:** when ToolHelp cannot open a target's
  module list, the LocalSystem service reconstructs it from kernel loader
  load/unload and rundown events, including the legacy load-only, compact, and
  current full Image_Load layouts. This covers protected targets such as
  `csrss.exe`, which deny direct module enumeration. The first request starts
  the rundown and the next refresh exposes the completed list; target decode,
  loss, or completeness failures remain unavailable instead of showing a
  partial list.
- **Core-service profile privilege:** the LocalSystem broker now whitelists
  `SeSystemProfilePrivilege` alongside `SeDebugPrivilege`. Without it, Windows
  denied every profile `StartTrace` with `ERROR_PRIVILEGE_NOT_HELD`, leaving
  Code samples unavailable for every process.
- **Injected overlay module semantics:** The Modules page now labels its count
  as threads started in the module and explains that a loaded hook or overlay
  DLL, such as RTSS's `rtsshooks64.dll`, can correctly show zero. Those DLLs
  execute on existing game/render threads rather than creating threads that
  start inside the injected module; the new code-sample columns provide the
  complementary execution evidence.
- **Thread and module activity reporting:** Process Properties now distinguishes
  threads that are currently running, ran recently, are provably idle, or are
  still being observed. The Threads page shows cumulative CPU time plus
  current and observed-window average CPU, kernel/user time, cycle details,
  sample age, and elapsed idle time. The Modules page labels its verdict as
  origin-thread activity and explicitly distinguishes own threads from no own
  threads, loading, unavailable data, and unknown start addresses.

### Fixed

- **Process row tooltips:** hover information on Processes and Details now
  appears near the pointer instead of at the left edge of the row.

- **False thread and module activity:** Ready, Standby, and DeferredReady
  threads no longer count as having executed. Activity observed across Paused
  mode, another properties tab, sleep, or another long sampling gap is now
  re-observed instead of being reported as recent; unchanged counters across
  such a gap remain valid idle evidence.
- **System and service process account names in CI / domain environments:**
  `sysinfo`'s user enumeration falls back to the LSA logon session user name
  when its internal `LookupAccountSidW` query fails to allocate a domain buffer.
  In elevated Windows Server/Azure runner environments, this reported the computer
  account (e.g. `runnerxyz$`) rather than `SYSTEM`. The sampler now canonicalizes
  well-known SIDs and computer account names from user enumeration to canonical
  account names (`SYSTEM`, `LOCAL SERVICE`, etc.) and pre-seeds well-known accounts.

### Changed

- **Core service protocol v9:** the service gains an identity-bound
  `ProcessModuleCodeSamples` request for sampled module execution and now uses
  the protected loader rundown for module inventory when direct access is
  denied. The app and service must be updated together.

## 0.1.21 - 2026-10-21

### New

- **Threads, Network and Services pages in Process Properties:** like System
  Informer, the properties dialog now lists a process's threads (TID, CPU,
  cycles delta, start address as `module+offset`, symbolic priority, state
  with wait reason), its TCP/UDP endpoints next to its live receive/send
  rates, and the services it hosts (double-click or "Go to service(s)" jumps
  to the Services page). The thread list also works for service and SYSTEM
  processes without administrator rights; their start addresses and cycle
  counts need thread access and read "—" where Windows denies it. All list
  pages refresh themselves while shown, at the update speed (frozen while
  paused), and F5 refreshes the open page.
- **Dormant threads and modules are marked:** a thread that went at least
  5 s without being scheduled once, and without using CPU time or cycles, is
  labelled *Dormant* and dimmed ("Hide dormant" filters them). On the Modules
  page, a module whose own threads are all dormant is marked the same way;
  a module that starts no thread of its own is never called dormant, because
  its code runs on other modules' threads.
- **Priority and affinity on the Processes page:** the context menu gains
  the Priority submenu and "Set affinity…" from Details. On a group row they
  apply to every member; a priority is ticked only when all members share it,
  and "Save priority for this program" is offered only when every member is
  the same program.

### Changed

- **Modules moved into Process Properties:** the module list, including
  unload, is now the Modules page of the properties dialog and gains Threads
  and Activity columns. It now updates itself — DLLs loaded later appear
  without a manual refresh — so its Refresh button is gone, as are the
  separate "View modules" context-menu entry and button. An unload confirmed
  during a refresh waits for it instead of failing.

- **One Performance card per physical disk:** disks are listed like Task
  Manager — "Disk 0 (C: D:)" — instead of one card per drive letter, which
  showed two volumes of one disk as two cards each claiming the whole disk's
  activity. Disks without a drive letter now get a card too (capacity rows
  are omitted for them). The core service protocol moves to v7, so the
  service must be updated together with the app.

### Fixed

- **List lost keyboard focus after any dialog:** after closing the End
  process confirmation, Process Properties or any other dialog, the row
  stayed selected but Delete and the arrow keys did nothing until it was
  clicked again. Focus now returns to the list (or whatever held it) when a
  dialog closes, including dialogs opened from a right-click menu.
- **Blue-filled confirmation buttons:** the action button of confirmation
  dialogs (End process, Unload module, service Stop/Restart, Apply affinity
  and the other two-button dialogs) was painted solid blue with a white ring,
  left over from an earlier theming attempt. It now looks exactly like the
  Cancel button; whichever button Enter would press carries a blue outline.
- **Ctrl+Shift+Esc did nothing in some games:** a game or tool with its own
  low-level keyboard hook could swallow the combo before TaskMan saw it —
  DOOM Eternal blocks Escape whenever Ctrl is held — so neither TaskMan nor
  Windows opened anything. TaskMan's hook now moves itself ahead of such
  hooks the moment Ctrl+Shift is pressed; all other keys still reach them
  unchanged. While TaskMan is the Task Manager replacement, tools that
  deliberately capture Ctrl+Shift+Esc (key remappers, kiosk software) no
  longer receive it.
- **Minimized TaskMan stayed behind fullscreen games:** with TaskMan's
  window minimized (rather than closed to the tray), Ctrl+Shift+Esc in a
  fullscreen game left it hidden behind the game. The window is now restored
  first and brought to the front afterwards.
- **Hotkey lost while TaskMan's window was frozen:** a press caught by the
  hook vanished when the window could not answer it. TaskMan now waits for
  the window to confirm, starts a fresh task manager when it provably hangs,
  and lets further presses reach Windows until the window responds again.
- **Hotkey dead until restart after Windows dropped the hook:** Windows
  silently removes a keyboard hook that answers too slowly even once. Any
  other way of opening the Task Manager (Start menu, taskbar,
  Ctrl+Alt+Del screen) now restores it.
- **GPU rows above the GPU total:** a process rendering on the software
  "Microsoft Basic Render Driver" (WARP/headless renderers, test runners)
  showed up to 100 % GPU while the column header read 1 %. The header and the
  Performance page leave that adapter out when a real GPU is present, but the
  per-process values still counted it. Per-process GPU utilization, engine
  label and GPU memory now cover exactly the adapters the totals come from.
- **GPU total understated under shared load:** an adapter's utilization was
  the busiest single process instead of the busiest engine, so two programs
  sharing the 3D engine at 60 % and 30 % read 60 % instead of 90 %. Engine
  load is now the sum of every process's share of it, matching Windows Task
  Manager; the per-engine graphs on the Performance page follow.
- **Group GPU above the machine total:** app families on the Processes page
  and session/app rollups on the Users page added up their members' GPU
  values, so a game on the 3D engine plus a video on the decoder could read
  130 %. Groups now report their busiest engine like the total does, and
  sorting by a resource column orders app rows by the values they show.
- **Search box lost focus on Left/Right:** moving the caret with the arrow
  keys jumped keyboard focus to the sidebar or a toolbar button, so the box
  dropped its blue frame and caret while still showing the text being
  edited (regression from 2026-09-27). The arrows now move the caret.
- **Search caret missing after keyboard focus:** focusing the search box
  with Ctrl+F, F6 or Tab, or switching back to the window, could leave the
  caret hidden for up to half a second; it now appears immediately.
- **DPC time counted as "Terminated processes":** the "System interrupts"
  row only subtracted interrupt time, so driver DPC load (network, audio,
  storage) showed up as CPU of terminated processes, often with an empty
  exit list. Interrupt and DPC time now come from the same per-processor
  records and window as the CPU residual they are split from.
- **Stale "Terminated processes" value:** a row held visible through quiet
  ticks kept showing the share that armed it (e.g. 35 % for ~4 s after a
  build ended under a 2 % CPU total); it now shows the current share.
- **Network total read too high:** the Processes/Users network header added
  receive + send across all adapters, so a full-duplex link at 60 % each way
  read 100 % and a virtual adapter (Hyper-V vEthernet, VPN) counted its
  physical adapter's traffic twice. It is now the busiest adapter in its
  busiest direction, and reads "—" when no adapter reports a link speed.
- **Loopback traffic in the per-process Network column:** local traffic to
  127.0.0.0/8 or ::1 (a local database or dev server) was counted as network
  traffic although it never reaches an adapter; it is now excluded, also from
  the Users and App history network totals.
- **Disk active time attributed across the wrong disk:** a process's Disk
  active time was its share of the service time summed over every disk,
  scaled by the busiest disk, so a process saturating a slow disk could read
  a few percent next to a busy fast disk, and two processes each saturating
  their own disk read 50 % apiece. Each process now gets its share of the
  disk that served it, scaled by that disk's own active time, and shows its
  busiest disk; app families and Users rows show their busiest disk instead
  of a sum. A reading carried over a very short refresh window now follows
  the disks' current activity instead of keeping its old percentage.
- **GPU engine column named the wrong GPU:** the Details "GPU engine" label
  used the adapter's internal node index, so every GPU read "GPU 0"; it now
  uses the GPU number shown on the Performance page.

## 0.1.20 - 2026-10-06

### Fixed

- **App icons on App History tab:** the App History ("App-Verlauf") page was
  the only table drawing the generic placeholder window icon instead of
  loading real application icons. The table now queries the icon cache using
  the recorded executable path or active process identity, matching the
  Processes, Details, Startup, and Users pages.

## 0.1.19 - 2026-10-06

### Fixed

- **Command line arguments for svchost and Windows services:** `svchost.exe` and
  other Windows background services running in Session 0 (SYSTEM, LocalService,
  NetworkService) frequently showed "—" in the Details Command Line column when
  TaskMan was run unelevated, because interactive processes cannot open those
  service hosts with `PROCESS_QUERY_LIMITED_INFORMATION` to query their process
  parameters. TaskMan now queries the Service Control Manager catalog for
  configured service binary command lines, properly showing arguments (e.g.
  `-k <group> -p -s <service>`) for svchost instances and services across the
  entire system. Search matching on command lines now works for services as well.

### Changed

- **Wi-Fi details are opt-in — no more Windows location prompts:** the SSID
  and signal-strength rows on the Performance network card were read through
  `WlanQueryInterface`, which Windows treats as precise-location data. On
  current Windows 11 that made TaskMan pop the one-time location permission
  prompt (once per build/executable identity) and re-surface in the
  "location in use" tray activity every few seconds, without consent ever
  yielding data. TaskMan now never touches location-gated APIs by default:
  the rows are collected only after enabling "Show Wi-Fi details" in
  Settings (the explicit action at which Windows may show its one-time
  consent prompt), only while the network card is actually on screen — never
  from the tray or another page — and at most once per 30 s. When Windows
  refuses the query, the card says so and links to the Location settings
  instead of silently dropping the rows.

## 0.1.18 - 2026-10-03

### Fixed

- **User name / Elevated for protected processes:** the Details rows of
  boot-critical Windows processes (smss, csrss, wininit, winlogon, services,
  lsass, dwm), kernel pseudo-processes (Registry, Memory Compression, Secure
  System) and the font/session hosts again resolve their owning account
  ("SYSTEM", "DWM-\<session\>", "UMFD-\<session\>") with their SID and
  elevation instead of showing "—" / "Unknown". A spoofed image name still
  earns nothing: only a verified Windows image or the kernel's own
  pseudo-process name is accepted as evidence.
- **Version metadata on localized Windows:** FileDescription / CompanyName are
  read from each file's own version-resource translation table instead of the
  English blocks only. Binaries that ship only a localized table (csrss and
  friends on a German Windows) used to lose their friendly name and company,
  which also disabled the Windows-image evidence behind the system-role
  fallbacks above.
- **Elevated after account inheritance:** a process that inherits a service
  account from a same-image service parent now reports the matching elevation
  and UAC virtualization instead of "Unknown".
- **End process tree killed unrelated processes:** a process whose original
  parent had exited was treated as a child of whatever process later received
  that recycled PID, so ending that process's tree could also end Explorer or
  any other long-running program. Descendants now come from one kernel
  snapshot of PIDs, parent links and creation times, and a "child" older than
  its parent is no longer part of the tree.
- **End process tree reported spurious failures:** children that exited on
  their own while the tree was being ended no longer turn a complete success
  into an error, and a failure to end the selected process itself is now the
  error shown instead of a child's.
- **"Process not found" errors:** actions on a process that has already exited
  report that it no longer exists instead of a raw "The parameter is
  incorrect" platform error.
- **CPU cache sizes:** the Performance CPU page divided every cache by the
  number of threads sharing it, so a Ryzen 7 5700X showed L1 256 KB, L2 2 MB
  and L3 2 MB instead of 512 KB, 4.0 MB and 32.0 MB. Each cache instance now
  counts once at its full size, as in Task Manager.
- **Virtualization row:** reports whether virtualization is enabled in the
  firmware (or a hypervisor is running) instead of whether the CPU merely
  supports it, which showed "Enabled" on machines where it is switched off.
- **GPU, disk and CPU-speed telemetry after hiding the window:** once the
  window had been hidden to the tray (or the consuming page left) for 30 s,
  these counters never came back for the rest of the session and showed "—".
  They now resume when they are needed again.
- **Disk counters for C: on a shared disk:** a volume that shared its physical
  disk with a later drive letter (C: and D: on one SSD) got no active time or
  transfer rates.
- **Process path after PID reuse:** a process whose PID previously belonged to
  an exited process no longer inherits that process's path, description,
  company or saved scheduling rules.
- **Network link speed:** adapters that report an unknown link speed (Wi-Fi
  Direct and some VPN adapters) no longer show an absurd multi-million Tbit/s
  speed or zero out network utilization.
- **Legacy icons on scaled displays:** icons without an alpha channel whose
  width is an odd multiple of 16 px no longer get a sheared transparency mask.
- **Users page "Sign out" froze the window:** choosing Sign out from a user
  row's context menu (or Enter on a row) deadlocked the UI thread.
- **Services page "Start" froze the window:** starting a service from a row's
  context menu deadlocked the UI thread.
- **Suspend/Resume reported success on failure:** suspending or resuming a
  process that refused it (access denied, already exited) now shows the error,
  with the completed count when only part of a selection failed.
- **Efficiency mode and Resume on grouped rows:** on a collapsed group such as
  a browser, the checkmark and "Resume" reflect any member, but the click only
  acted on the group's head process — clearing the checkmark could turn
  efficiency mode ON. The action now applies to every member of the group.
- **Startup impact sorting:** sorting by Startup impact now orders by the
  measured impact the column shows instead of leaving enabled entries in name
  order.
- **Unknown values shown as zero:** the GPU total above the Processes and
  Users tables reads "—" instead of "0 %" while no process has GPU telemetry,
  and an unknown socket or core count on the CPU page reads "—" instead of
  "0" (or a guessed "1").
- **App history inflated by tab switches:** after a tick without per-process
  network or CPU counters (another page shown, or the background service
  missing a tick), the next reading was counted from zero, so every return to
  Processes, Users or App history credited a program's entire traffic and CPU
  time since it started again — and saved it.
- **"Paused" update speed ignored at startup:** with Update speed set to
  Paused, the app came up sampling anyway while the menu said Paused. It now
  starts paused with one initial snapshot.
- **Extra and duplicate samples:** switching pages, hiding or restoring the
  window and similar changes each forced an off-schedule sample, and Refresh
  (F5) sampled twice. Rates from those near-zero windows showed as one-tick
  spikes and dips in graphs and columns.
- **Startup impact undercounted:** an app that started after boot is now
  charged everything it did since it started, not only what happened after
  TaskMan first saw it, so a launcher that does its work in its first second
  is no longer "Low" or "Not measured".
- **Saved GPU graph engine reset on restart:** an engine name containing
  spaces (e.g. "High Priority Compute") is kept instead of reverting to the
  overall graph.
- **Background service stopped under load:** when every connection slot of
  the service's control pipe was in use (a burst of actions, or clients that
  kept their connection open after being refused), the service gave up within
  microseconds and exited until Windows restarted it 5–60 s later. It now
  waits for a slot to free up.
- **Linux priority read back wrong:** a process set to High showed as Above
  normal (so a saved per-program priority stored the wrong class), and
  priority and affinity changes only reached a process's main thread. Both now
  apply to every thread and read back as the class that was set.
- **Linux CPU caches undercounted:** L1 and L2 showed one core's caches (and
  dropped the instruction cache); every distinct cache on every CPU is now
  counted.
- **Linux disks on LVM/LUKS:** volumes mounted through `/dev/mapper` now get
  transfer rates and active time, and the first sample shows active time as
  unknown instead of 0 %.
- **Linux GPUs without telemetry:** cards whose driver publishes no
  utilization (Intel, Nouveau, NVIDIA, simpledrm) are no longer shown as an
  idle GPU at 0 % with 0 B memory.
- **Linux services without systemd:** a system where `systemctl` is unavailable
  reports the error instead of an empty service list, and service
  descriptions are no longer cut short when a unit name contains its state.
- **Linux fonts:** a system font that is not TrueType/OpenType (PCF, Type 1) no
  longer crashes the app at startup; the next candidate is used.
- **macOS suspended processes:** a suspended process now shows as Suspended
  and offers Resume. macOS also reports disk transfer rates, the open file
  handle count, the real host name and each startup item's enabled state.
- **Installed memory with 32 GB modules:** each 32 GB (or larger) RAM module
  was counted as 32 MB, which made "Hardware reserved" read a fabricated 0.
- **Wi-Fi name and signal:** the Performance page's Wi-Fi SSID and signal
  strength never appeared because adapters were matched to WLAN interfaces by
  the wrong GUID.
- **Startup entries stored as expandable strings:** Run entries such as
  Windows Security's showed "N bytes" instead of their command, publisher and
  impact.
- **Commit size of unreadable processes:** shows "—" instead of a fabricated
  "0 K" when no source reports it.
- **GPU after a driver update:** a GPU that got a new adapter identity (driver
  update, device restart, external GPU) is picked up again instead of showing
  0 % until TaskMan restarts.
- **Per-core CPU layout:** on systems where interrupt time is at least DPC
  time, per-core CPU records could be decoded with the wrong layout, and a
  counter running backwards could show a "Terminated processes" row near
  100 %.
- **Services list after a service was installed:** a service registered while
  the list was being read no longer produces an empty Services page.
- **Per-process network and disk frozen at zero:** when the event trace
  behind the Network and Disk activity columns was stopped from outside (by
  another TaskMan instance reclaiming it, or by `logman`), every process kept
  showing 0 as if idle. TaskMan now detects the stopped trace, shows "—", and
  restarts it (immediately in the background service, after a short back-off
  in the app).
- **Background service file logging after setup:** every install made by the
  setup left a `setup.log` in the service's log folder, which made the service
  refuse that folder and silently run without its log file. Setup now logs
  elsewhere and removes the old file.
- **Interrupted installs left no uninstall entry:** setup registers itself in
  Settings > Apps right after installing the background service, so a later
  failure (such as a shortcut that cannot be created) no longer leaves a
  service behind with no way to uninstall it. Running setup from the install
  folder itself also works now.
- **Stale setup artifacts:** a packaging run whose setup embedding or
  verification fails no longer leaves a broken or older `*-setup.exe` in
  `dist/`.
- **Selection jumped to the top after End task:** when the selected process
  exited (End task, or on its own) while the Processes or Details table had
  focus, the selection and the view snapped to the first row. The row that
  takes its place is selected instead, like in Task Manager.

### Improved

- **Per-process identity for unopenable processes:** the LocalSystem service
  answers a bounded, PID+creation-time-bound token identity read (account,
  SID, elevation, UAC virtualization) for the SYSTEM/service processes the
  interactive GUI's own token cannot open at all — the rows that used to be
  the last ones with a blank User name. An unanswered read still renders "—"
  and is retried on a bounded horizon; nothing is ever guessed.
- **Lower sampling cost:** per-process thread counts are read from the kernel
  process table the sampler already queries, instead of a Toolhelp thread
  snapshot that cost about 30 ms of CPU on every tick on a typical desktop.
- **Startup page responsiveness:** resolving each entry's executable and
  reading the last BIOS time now happen when the list is fetched, not for
  every row on every repaint; an entry pointing at an unreachable network path
  no longer stalls the window on mouse movement.
- **Safer settings and history files:** settings, app history and startup
  impact data are flushed to disk before they replace the previous file, so a
  power loss cannot leave an empty file behind. A file that exists but cannot
  be used (corrupt, oversized, unreadable) is kept as `<name>.bad` instead of
  being overwritten by defaults on the next save.
- **Faster Run and native Task Manager launch:** launching a task from the
  Run dialog, and the "open Windows Task Manager" escape hatch, no longer wait
  an extra half second after the program has started.
- **Much lower sampling cost:** each sampling tick re-read the configuration
  of every running Windows service (~36 ms on a typical desktop) and probed
  every process with up to three `OpenProcess` calls. Service configuration
  is now cached and only processes the kernel no longer lists are probed.
- **Lighter repaints on every table page:** Processes, Details, Users,
  Services, Startup and App history measured the text width of every cell of
  the whole list on each repaint, only to support double-clicking a column
  separator. That measurement now runs only on the double-click itself.

### Changed

- **Core service protocol v6:** the identity read above is a wire addition, so
  GUI and service must be upgraded together (setup does exactly that). Against
  an older service, privileged broker calls are refused with "action state is
  unknown" instead of being misapplied, and identity reads answer nothing —
  the affected rows keep "—" until the service is upgraded.

### Security

- **Setup DLL planting:** the elevated setup no longer loads system DLLs (and
  no longer needs the VC++ runtime DLL) from the folder it was started from.
  A DLL planted next to `taskman-setup.exe` in Downloads would previously run
  with administrator rights before the wizard appeared.
- **Setup payload substitution:** setup reads its embedded payload and the
  uninstaller copy through one handle to its own running image, opened and
  verified at startup and locked against changes. Renaming the running setup
  and putting another file at its path after the UAC prompt no longer changes
  what gets installed.
- **Setup log location:** the elevated setup log moved to an
  administrator-only folder (`%SystemRoot%\Logs\TaskMan\setup.log`) and is never
  written through a junction or symbolic link, instead of a folder ordinary
  users can prepare before the first install.
- **Uninstall rollback:** a failed uninstall can only move the uninstaller
  itself back into Program Files; whatever else appears at its temporary
  location is never moved in.
- **Oversized setup payloads:** a manipulated payload entry is rejected as
  soon as it inflates past its declared size instead of being decompressed in
  full first.

## 0.1.17 - 2026-10-02

### Fixed

- **Setup launch access denied:** fixed post-install desktop launch failing with
  `0x80070005` after program files had already been installed. TaskMan still
  launches as the unelevated desktop user.
- **Release artifact names:** host architecture follows the active Rust
  compiler even when the environment omits machine information, preventing
  archives and setup installers with an empty architecture suffix.
- **Windows uninstall:** restores the built-in Task Manager before deleting
  an installed replacement, preserves unrelated and portable replacements,
  waits for the background service to stop, and keeps the uninstall entry
  until program files have been removed.
- **Service-free upgrades:** opting out removes an existing broker before
  updating its binaries, avoiding locked-file failures and an invalid broker
  manifest. Program files use the same protected-copy rules as service installs.
- **Setup user and launch:** broker enrollment and shortcuts use the desktop
  user even when UAC credentials belong to another administrator. Post-install
  launch uses the desktop user's unelevated token and environment; an unavailable
  unelevated token is reported before installation begins.
- **Setup interruption and errors:** window close is blocked while work runs;
  failed steps and worker-start failures are shown and logged. The wizard can
  be resized for longer messages.
- **Disk graphs:** unavailable activity leaves gaps in the graph and resource
  sparkline; hover and keyboard readouts show “—”. Measured idle remains zero.
- **Batch action errors:** bulk End task, efficiency, priority and affinity
  actions preserve failure reasons; partial success reports the completed count
  and first failure instead of success alone.

### Security

- **Installer staging:** elevated helpers are extracted into freshly created,
  administrator/System-only staging under Program Files instead of a predictable
  user temporary directory. Staging is cleaned on both success and failure.

## 0.1.16 - 2026-09-30

### New

- **Windows setup installer (`taskman-setup.exe`):** releases now ship a
  self-contained, 100%-Rust setup executable (modeled on the green-curve
  installer) as `dist/taskman-v<version>-windows-<arch>-setup.exe`. It
  installs Task Manager to the protected `%ProgramFiles%\TaskMan` location,
  optionally registers and starts the background service so the first GUI
  start reaches the broker without a UAC prompt, creates Start menu and
  opt-in desktop shortcuts, registers an uninstall entry in Settings > Apps,
  and upgrades existing installations in place while preserving settings.
  The wizard mirrors the app's Windows 11 dark and light theming (shared
  `tm-ui` theme, product icon, header/footer bands) and reviews the MIT
  license before installing. Running a bare `taskman-setup` build output
  (no embedded payload) is refused up front with guidance to the packaged
  installer, and packaging verifies the produced artifact's embedded payload
  before it reaches `dist/`. Unattended
  use: `/S`, `--uninstall`, `--no-start-menu`, `--desktop`, `--launch`,
  `--no-service`, `--dry-run`. The install directory is fixed (`/D=` is
  rejected with an explanation) because the service is pinned to the
  protected location. The embedded payload is deflate-compressed and SHA-256
  verified before any file is written; uninstall moves the running uninstaller
  out of the install tree instead of leaving files behind.
- **Shared theme crate (`tm-ui`):** the Windows-11-Task-Manager palette and
  OS-native font setup moved out of `tm-app` into a shared crate so the setup
  installer renders from the same theme source as the app. `tm-app` re-exports
  both modules unchanged.

### Changed

- **Core service start type:** the TaskMan Core Service now installs as plain
  "Automatic" instead of "Automatic (Delayed Start)", so the privileged broker
  is ready as soon as the desktop is up and privileged actions work right
  after boot instead of after the delayed-start grace period. Existing
  installations pick this up on the next install/repair: the delayed flag is
  cleared explicitly because updating a service's config never touches it.

### Improved

- **Core service failure visibility:** when the installed TaskMan binaries no
  longer match the pinned broker manifest, the service now reports that under
  its own SCM exit code and logs which binary drifted together with the
  expected/actual hash fingerprints, and Settings › Advanced shows
  "integrity check failed" with the Repair action instead of a bare
  "stopped". Previously this permanent, repairable condition shared one
  generic exit code with every other broker fault — the System event log only
  showed an opaque "service-specific error 1", the real reason sat in an
  admin-only log, and SCM kept restarting the service once per minute
  indefinitely. Ordinary broker failures are unchanged.
- **Setup wizard spacing:** the installer's body content is now inset to match
  the header/footer bands and each option's description is indented under its
  checkbox, so no text hugs the window's rounded edge or reads as truncated /
  overlapping against the surrounding labels.
- **Single launch prompt:** "run Task Manager when setup completes" is now
  offered once, as the Options checkbox. The completion page no longer repeats
  it with a separate "Launch Task Manager" button — the checkbox already starts
  the app at the end of the install.

## 0.1.15 - 2026-09-27

### New

- **Keyboard page switching:** Ctrl+Tab and Ctrl+Shift+Tab cycle through the
  pages (wrapping around), Ctrl+1 through Ctrl+9 jump directly to a page.
  While a dialog is open it keeps keyboard ownership, so the shortcuts stand
  down — the same gate the Delete shortcut uses.
- **Run new task and Settings shortcuts:** Ctrl+N opens the "Run new task"
  dialog and Ctrl+, opens the Settings dialog from anywhere in the application,
  matching native Windows 11 Task Manager shortcuts. Both are documented in
  the F1 shortcut help overlay.
- **F1 shortcut help:** F1 toggles a German/English overlay listing every
  keyboard shortcut; Esc closes it. The same list is documented in a new
  "Keyboard shortcuts" section in the README.
- **Keyboard navigation for the remaining lists:** the Services, Startup,
  Users and App history rows and the Performance resource cards follow the
  arrow keys plus Home/End/PageUp/PageDown to move the selection. Enter
  opens the row menu on Services, Startup and Users, and the Menu key or
  Shift+F10 opens the selection's context menu. App history gains a visible
  row selection.
- **F6 focus regions:** F6 and Shift+F6 step through the search box, the
  sidebar, the current page's command buttons and the page content, so the
  whole window is reachable without hunting for the next Tab stop. The
  Performance tab has no search box and is skipped.
- **Keyboard-scrubbable charts:** a focused Performance graph walks its
  samples with Left/Right and jumps to the oldest or newest with Home/End.
  Up/Down moves between the series of a multi-series graph, and between the
  logical processor charts in the per-core CPU grid. The focused chart paints
  a focus ring and shows the same hover readout the mouse produces, pinned to
  the selected sample.
- **Reorderable columns from the keyboard:** Alt+Left/Alt+Right on a focused
  column header moves a movable column one position, matching the mouse drag.
- **Resizable Performance card column from the keyboard:** the splitter
  between the resource cards and the detail area is a focusable stop that
  takes Left/Right in 8 px steps (Shift for 32 px), reported to assistive
  technology with its current width.
- **Screen reader support:** tables, the Performance card list, the charts and
  the dialogs expose a structured accessibility tree — list/list-item roles
  with set position and size, labelled buttons and splitters, and the
  keyboard-selected chart sample published as the chart's value.

### Improved

- **Tables follow the native Task Manager model:** table rows are no longer
  Tab stops, so the arrow keys move the visible selection from anywhere on
  the page instead of dead-ending on a focused row. Shift+arrows extend the
  multi-selection (previously unreachable), Ctrl+arrows move it without
  changing it, Space toggles the primary row in and out of the selection,
  Enter runs the primary row action (Processes: Go to details; Details:
  Process Properties), and PgUp/PgDn page by the visible table span.
- **Cyclic keyboard navigation:** Pressing Tab from an active list navigates
  forward into enabled toolbar buttons and table column headers, while
  Shift+Tab returns directly to the search bar. In the Performance tab,
  Shift+F10 and the Menu key open graph context menus from the keyboard.
- **Keyboard navigation flow and focus continuity:** Audited and perfected
  keyboard-only navigation across the shell and tables. Tab and Shift+Tab form
  an unbroken cycle across the search bar, sidebar, command toolbar, table
  headers, and table content with no focus traps or dead ends when toolbar
  buttons are empty or disabled. Active vs. inactive row selections match
  Windows 11 Task Manager visuals (vibrant accent fill with vertical accent
  indicator pill when table content holds focus, dimmed fill without pill when
  unfocused).
- **List navigation and type-ahead:** Clamped navigation indices when lists
  shrink to prevent out-of-bounds selection jumps; PageUp without an active
  selection navigates from the bottom of the list. Switching tabs or committing
  search on Users and App History tabs automatically selects the first matching
  row in live display order and scrolls it into view. Added type-ahead search
  support across Services, Startup, Users, and App History.
- **Visible keyboard focus:** sidebar entries, header and command buttons,
  the search box with its clear button, table header cells and the toast
  close buttons paint a focus ring while focused. Disabled command buttons
  leave the Tab order entirely, and the titlebar drag regions no longer sit
  in it as invisible stops.
- **Keyboard column control:** a focused header cell sorts with Enter or
  Space, and the Menu key opens its column menu — the Details column chooser
  is reachable without the mouse. A focused column resize handle adjusts the
  width with Left/Right (Shift for larger steps).
- **Dialog keyboard model:** Settings tabs through its controls with an
  anchored initial focus and scrolls freshly focused controls into view, and
  Enter closes a dialog only while no activatable control is focused.
  Closing a dialog returns focus to the widget that opened it. The affinity
  dialog's CPU checkboxes become keyboard-reachable (its button row follows
  real focus instead of pinning it), the destructive confirms keep their
  safe-button Tab trap, and with a dialog open the pages' table navigation
  (arrows, type-ahead, Enter/Space, context-menu key) stands down.
- **Context menus from the keyboard:** Tab closes an open menu instead of
  stranding it, ArrowRight/ArrowLeft open and close submenus, and the Menu
  key now works while any non-text widget holds focus. Closing a menu returns
  focus to the control that opened it.
- **Dialogs keep focus inside:** an open dialog holds keyboard focus and
  pointer interaction for itself — Tab cycles only its own controls and the
  page behind it no longer reacts to clicks or stray keys while it is up.
- **App History on Linux and macOS:** non-system processes now accumulate
  local usage history even where window ownership cannot be determined.

### Fixed

- **Context menu keyboard focus and arrow navigation:** Fixed context menus opened via the
  keyboard Menu key or Shift+F10 failing to receive keyboard focus or respond to arrow keys.
  The first enabled item now receives focus immediately upon opening, ArrowUp/ArrowDown wrap
  circularly skipping disabled items, Home/End jump to the start/end, and directional keys are
  locked within the popup to prevent focus from escaping into background table headers or rows.
- **Keyboard navigation visual selection flicker:** Eliminated 1-frame visual
  anomalies where unintended UI elements briefly flashed as focused or selected
  before focus/selection moved to the intended element. Patched `set_focus_lock_filter`
  in egui so mid-frame focus handoffs immediately install event filters without
  requiring prior-frame history, processed sidebar Settings navigation keys before
  the tabs loop renders, and installed directional/tab event filters on command
  toolbar buttons, table headers, and the global search box.
- **Sidebar arrow navigation jumping:** Resolved unexpected jumping and overshooting
  when pressing ArrowUp or ArrowDown in the sidebar. Installed strict directional focus
  locks (`EventFilter`) and explicit focus direction clearing on all sidebar items
  (hamburger toggle, tabs, and settings) so egui's default spatial navigation never
  interferes or causes double jumps. ArrowDown and ArrowUp now navigate sequentially
  one item at a time across the entire column with clean boundary stops at Hamburger
  and Settings, while Tab and Shift+Tab smoothly cycle into and out of the sidebar
  preserving the currently active tab.
- **Selection focus indicators:** Removed the whole-section accent border that
  framed the entire table viewport (Processes, Details, Services, etc.) or card
  column (Performance) when focused. Focus and selection are now indicated
  solely by the clicked/focused row or resource card itself, matching native
  Task Manager behavior.
- **Process tree collapse selection warp:** Fixed a defect where pressing
  Left Arrow on an aggregate child row collapsed the parent without moving the
  stored selection identity, leaving an orphaned selection on a hidden child.
  On the subsequent keystroke, the table failed to resolve the selected row and
  warped selection to row 0. Left Arrow on a child row now transfers selection
  directly to the parent.
- **Sidebar and toolbar Tab dead ends:** Fixed focus traps where tabbing from
  Settings or shifting tab from table headers stranded focus when no toolbar
  command buttons were active or enabled. Focus now smoothly falls through to
  headers or content.
- **Search clear button Tab stop:** Changed the clear search button sense to
  prevent it from capturing a redundant Tab stop inside the search box.

- **Tab navigation to sidebar tabs and tab switching on focus:** Pressing Tab from
  an active list or table now reliably navigates into the sidebar navigation pane,
  focusing the active page tab directly. Navigating through the sidebar tabs via
  Tab, Shift+Tab, or ArrowUp/ArrowDown automatically activates and switches to that
  page, and pressing Enter, ArrowRight, or Escape surrenders focus into the active
  page content. When the search field has no text, Tab flows forward into the sidebar
  tabs instead of getting trapped in the list, while non-empty search queries continue
  to commit and jump directly to the matching row on Tab or Down Arrow. Shift+Tab from
  the list now returns to the search bar across all tabs including Performance.
- **Concurrent sidebar and list input conflict:** Keystrokes such as Tab or
  ArrowDown/ArrowUp no longer register simultaneously in the sidebar and the
  process/details table list. List navigation gates strictly stand down when
  any chrome widget (sidebar item, hamburger button, toolbar button, search
  input) holds keyboard focus. Committing search with Tab or Down Arrow
  intercepts the key before text edit focus traversal, preventing egui from
  leaking focus to the sidebar hamburger button. Sidebar items and toolbar
  buttons now reliably surrender focus directly to the table rows upon
  pressing ArrowRight, ArrowDown, or Escape.
- **Search bar Tab jump to list:** Pressing Tab or Down Arrow from the search
  field commits the search match and immediately hands keyboard focus to the
  table rows, parking the selection into view. The clear ('X') button is
  excluded from the Tab cycle so keyboard navigation never gets stranded in
  the search box chrome.
- **Table header focus escape:** Pressing Down Arrow or Escape on a focused
  table column header or resize handle surrenders focus back to table rows,
  preventing keyboard navigation from getting locked in the header.
- **Core-service upgrades:** replacing a running service now waits for SCM to
  report it stopped without requiring access to the LocalSystem process
  handle. A failed repair or switch also re-enables its Settings control.
- **Wrong executable identity:** unreadable Windows processes no longer borrow
  another same-named process's path, publisher, or saved scheduling rules;
  their path stays unknown unless a PID-bound source resolves it.
- **App History loss and overcounting:** a slow database load can no longer be
  overwritten by autosave; new observations merge after it completes. The
  first sighting of a busy process no longer adds an invented interval of CPU
  or network usage.
- **Misidentified service helpers:** Session 0 or a familiar process name
  alone no longer labels an unreadable process as SYSTEM or assigns it
  SYSTEM's SID.
- **Stalled or crowded telemetry:** a core-service telemetry request has a
  bounded wait, so a silent broker cannot freeze the sampling engine. A
  response with more active processes than its frame can hold reports
  unavailable rather than false zero readings.
- **Linux release fallback:** an installed `cargo-zigbuild` without the `zig`
  executable now selects the available self-contained musl build.
- **Search bar held focus hostage:** committing a search with Enter left the
  field focused, so the arrow keys could not move the row selection it had
  just set up. Enter now commits the jump to the first match and releases
  the field (without also firing the row action on the same keypress), a
  click outside the field releases it, and Esc clears the search globally
  whenever no dialog or menu is open — not only while the field is focused.

## 0.1.14 - 2026-09-23

### Fixed

- **Ctrl+Shift+Esc dead while TaskMan was unresponsive:** if TaskMan's window
  stopped responding, the next Ctrl+Shift+Esc left the thread that handles
  the hotkey waiting on it indefinitely. From then on every press was
  swallowed without effect — Explorer never got to start a fresh task
  manager — and turning the Task Manager replacement off no longer removed
  the keyboard hook. The hotkey thread no longer waits on the window, and the
  combo now passes through to Windows whenever TaskMan cannot answer it
  (still starting up, or still busy with the previous press), so a new
  instance opens instead.
- **Game input stalled while TaskMan took the foreground:** raising TaskMan
  over a fullscreen game briefly shared the game's input queue with
  TaskMan's own UI thread, so a slow TaskMan frame delayed the game's input
  until the switch completed. Only the foreground thread is shared now, and
  only for the single activation call.
- **Escape stuck after releasing Ctrl+Shift first:** holding Escape after
  letting go of Ctrl+Shift sent auto-repeat Escape presses to the focused app
  but still swallowed the release, leaving Escape held down there.
- **White flash when Ctrl+Shift+Esc restored TaskMan from the tray:** the
  hotkey path showed the window before it had painted, bypassing the cloak
  the tray restore uses; the window is now shown only by its own UI thread.
- **Efficiency mode reporting success when nothing changed:** toggling
  Efficiency mode on a multi-selection discarded every per-process failure and
  toasted "changed for N processes" even when all of them were refused (target
  already exited, access denied). The batch now fails with the real error when
  nothing succeeded and reports `(done/total)` on a partial batch, matching
  what priority and affinity changes already did.
- **Services tab stuck on "Gathering data":** the Services list was the only
  tab that spawned a raw thread instead of using the action executor; if that
  spawn failed, the in-flight flag never cleared and no later refresh could
  start. It now runs through the same bounded executor as Startup and Users,
  with the queue-full/failed fallbacks those tabs already had.
- **Settings writes on the UI thread:** eleven call sites saved `config.ini`
  synchronously inside a frame, so a slow or synced (OneDrive) config file
  stalled rendering. All settings persistence now goes through the existing
  coalescing writer thread; the now-unused synchronous `Settings::save` entry
  point is removed so the blocking path cannot be reintroduced.
- **Keyboard-hook callback could panic on a poisoned lock:** the hotkey
  worker's callback registry now uses the poisoning-recovery lock helper the
  rest of the codebase uses; with `panic = "abort"`, a poisoned mutex here
  would have taken down the process that carries every keystroke on the
  desktop.

- **System-wide keyboard lag from the Ctrl+Shift+Esc hook:** the thread that
  carries every keystroke on the desktop while TaskMan is the registered Task
  Manager replacement is now exempt from Windows' managed power throttling
  (EcoQoS). Windows throttles processes it considers background — which is
  exactly a task manager parked in the notification area — and scheduling
  priority alone does not cover that, so typing anywhere on the machine was
  paying for the efficiency core the hook thread had been moved to.
- **Suspending or throttling TaskMan from its own process list:** Suspend,
  priority, processor affinity and Efficiency mode now refuse to target the
  running copy. Suspending it stopped the keyboard hook outright, stalling
  every keystroke on the desktop until Windows dropped the hook — and the
  window that could have undone it was the one that had just been suspended.
  Turning Efficiency mode off and resuming stay available so an earlier
  mistake can still be repaired. A saved per-program rule is no longer
  replayed against TaskMan's own process on every start.
- **Frozen input in the foreground application while TaskMan was raised:**
  activating the window merged its input queue with the foreground
  application's — MSDN's documented consequence being that both stop
  responding together — from threads that could then block for as long as
  another process took to answer. The merge is now opt-in, used only by the
  hotkey worker (which pumps its own message queue), refused when either
  window is hung, and no longer wraps the show/restore/re-stack calls that
  wait on the target's thread.
- **Stray Alt keypress sent to the app you were using:** raising TaskMan asked
  winit to focus the window, which synthesizes a left-Alt press and release
  into the system input stream to shake off the foreground lock. That Alt went
  to whatever still held focus — usually the fullscreen game Ctrl+Shift+Esc
  was pressed in — where a bare Alt opens the menu bar and swallows the next
  keystroke. TaskMan's own activation already did strictly more, so the
  request is gone.
- **Escape stuck down in another application:** when the Ctrl+Shift+Esc press
  was swallowed but its release landed on a desktop the hook cannot see (the
  UAC prompt, the lock screen, a session switch), the next unrelated Escape
  release anywhere on the machine was eaten in its place, leaving that
  application with a key it never saw released. A release is now only
  swallowed when it still matches a recent press.
- **Empty StartupApproved registry value rendered as disabled:** `list_startup`
  treated an empty `StartupApproved` value as disabled because `data.first()`
  was `None`, even though Windows considers an empty entry enabled and runs it
  at logon. Empty entries now correctly report enabled.
- **Kernel ETW sessions outliving an application crash:** ETW disk and network
  trace sessions are kernel objects that survived application aborts
  (`panic = "abort"`) because cleanup was `Drop`-only. An unhandled abort now
  runs emergency teardown (`stop_live_sessions`) before the process exits so
  orphaned kernel traces do not continue tracing machine-wide I/O.
- **Log files truncated to a single line and crash diagnostics lost:** the
  file logging `WorkerGuard` was dropped immediately after startup check in
  `main.rs`, silently discarding subsequent logs, and normal GUI launches did
  not attach file logging. The logging worker guard now lives for the process
  lifetime, file logging is attached on the engine frame, and fatal panics
  synchronously flush a `.crash` log with thread backtraces.

### Improved

- **Sort comparisons no longer allocate:** every table tab (Processes,
  Details, Users, Services, Startup, App History, Modules) sorted text columns
  by lowercasing both strings per comparison — O(n log n) temporary `String`s
  per sort. The tabs now share one iterator-based `tablekit::cmp_ignore_case`,
  which also makes sorting consistently case-insensitive for non-ASCII names,
  plus shared `directed`/`apply_auto_fit` helpers.
- **`unsafe` justification:** the riskiest unsafe sites (the cross-process
  `FreeLibrary` transmute, the sampler's termination check, the keyboard
  hook's struct deref, and the broker's handle/SID ownership transfers) now
  carry `SAFETY:` comments stating the invariant they rely on.
- **Live-kernel verification tool:** added `python build.py --live-tests` to
  validate elevated NT kernel structure offsets and ETW payload decoders
  against live events.
