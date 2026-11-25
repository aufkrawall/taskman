# Known and Accepted Debt

Last verified: 2026-11-25 (Linux sampling cost, positive-only caches, and the absence of a Linux test runner)

Primary sources:
- `AGENTS.md`
- `audit.md` (2026 parity audit; phased plan)
- `crates/` (current behavior is the source of truth)

## Purpose

Debt that has been deliberately accepted, with the reasoning, so later
audits do not re-derive it and later agents do not "fix" it without
weighing the same trade-off. Items here are *recorded*, not endorsed.

## Remaining parity work (accepted scope, not yet implemented)

The 2026 audit (`audit.md`) used six phases, but they are no longer cleanly
"open" or "closed": the 2026-08-31 pass completed the high-value table,
keyboard, Performance visual, network-adapter, dump, and process-diagnostics
items across Phases 2–6. The following concrete gaps remain:

- **Column surface:** Details has the typed, persisted Select-columns catalog
  (now including an optional User SID column), but Processes and Users still
  expose fixed column sets rather than every native optional header-menu
  column. A shared cross-tab column registry is still desirable when the
  missing telemetry providers below are added.
  **Header context menu: PARTIAL** (2026-09-17) — Details opens Select-columns
  from a header right-click; Processes/Users still have no header menu, and
  `TmTable::header` still does not return its own `Response`, so each tab that
  wants one repeats the computed-header-rect hit test.
- **Telemetry fidelity:** native SRUM App History;
  packaged/MSIX startup tasks; full `.lnk` target
  resolution through `IShellLink`/`IPersistFile`; static GPU details.
  (Per-GPU-engine histories landed
  2026-09-01 — `HistoryPoint.gpu_engines` plus the "change graph to" menu.)
  **Measured Startup impact: DONE** (2026-09-17) — measured by this app over
  the boot window rather than read from Windows, because research established
  that Task Manager's startup impact is not stored in SRUM (SRUDB.dat carries
  hourly App History aggregates; Microsoft documents the impact thresholds but
  not a per-entry store). Remaining limitation, deliberately accepted: a
  process that starts AND exits between two sampler ticks is not attributed,
  and a session that starts after the 120 s window leaves the column
  "Not measured" until the next boot. Do not "fix" that by extrapolating from
  unrelated uptime.
- **Memory composition: DONE** (2026-09-17). In use / modified / standby /
  free come from `NtQuerySystemInformation(SystemMemoryListInformation)`
  (`win/perfcounters.rs::query_windows_memory_lists`, hand-written NT struct
  with a live kernel test). `MemoryInfo.modified_bytes/standby_bytes/free_bytes`
  are 0 when unreported and the bar is then not drawn. Note `MemoryInfo.cached_bytes`
  is still `GetPerformanceInfo::SystemCache` (file cache), NOT the standby
  list — standby is its own field now; do not conflate them.
- **Current 2026 optional columns:** NPU, NPU Engine, NPU Dedicated Memory,
  NPU Shared Memory, and Isolation/AppContainer, plus neural-engine
  Performance entries. These require capability-gated model/collector work;
  absent hardware or telemetry must remain `—`, never zero.
- **Details tree roots:** `System` (pid 4) reports parent pid 0 and sysinfo
  maps that to `None`, so it renders as a root next to `[System Process]`
  instead of nested beneath it the way System Informer shows it. Restoring
  the link would mean re-introducing a parent the collector API did not
  report; the honest root is preferred over a synthesized edge.
- **Sub-pixel (ClearType-like) text: DONE with platform residuals** (updated
  2026-11-23). epaint always stored `Color32`, so per-channel coverage needed no new
  texture format; the contract now uses RGB coverage with alpha zero as an explicit LCD
  tag. Neutral, tinted and full coverage are equally identifiable; zero coverage is
  `(0,0,0,0)`, and mixed premultiplied color glyphs retain non-zero alpha. The software
  renderer applies the full gamma/contrast model and routes direct-blit texels
  individually. WGPU uses a second dual-source font-atlas pipeline only when the selected
  device enabled `DUAL_SOURCE_BLENDING`; the pipeline is selected only for non-white-UV
  managed-font meshes with LCD content. Glow and unsupported adapters stay grayscale.
  - **Accepted GPU limitation:** fixed-function dual-source blending cannot reproduce the
    software renderer's nonlinear destination-gamma term. WGPU carries RGB/BGR order and
    contrast, but its output is not claimed to be DirectWrite-gamma-exact.
  - **Windows monitor policy:** the window placement/monitor/DPI key refreshes the
    DirectWrite policy after moves, including same-sized moves. Multiple intersecting
    monitors must agree on validated parameters and stripe order; any failed/malformed
    query or conflict disables LCD for that window instead of assuming RGB or choosing one
    half. Linux has no equivalent per-monitor signal.
  - **Linux source:** the process-wide startup query follows effective per-user fontconfig
    `antialias`, horizontal `rgba` and `lcdfilter` values, using trusted absolute
    canonical `fc-match` paths with root-owned, non-group/world-writable parent directories,
    while inheriting the user's environment. Vertical/unknown/none, `antialias: False` and
    malformed values fail closed. A single 750 ms absolute deadline caps stdout at 64 KiB;
    timeout/overflow kills and reaps the child. System-font selection shares that resolver
    and process runner.
  - **Linux per-monitor limitation:** fontconfig exposes one effective order/filter, not
    a proven order for each monitor. Mixed-panel desktops therefore use one process-wide
    choice. Off-grid glyphs use the grayscale fallback, but a compositor that resamples an
    otherwise 1:1 fractional-Wayland surface can still reduce LCD correctness; no
    fontconfig property proves that compositor behavior.
  - **Verification residual:** the implementation/correction workers were explicitly
    forbidden from compiling or running tests. Parent-side integration then passed
    `cargo check` for the app/installer, the normal `egui-wgpu` unit/Naga suite, and the
    manual D3D12/Vulkan hardware suite with
    `TASKMAN_RUN_GPU_SUBPIXEL_TESTS=1`. The final release gate and user-visible visual
    review remain separate checks; portable CI still has no GPU hardware claim.
  - **Color-glyph hardware residual:** current epaint has no COLR/CBDT source, so the
    manual GPU suite cannot create a real color glyph. Synthetic premultiplied color
    texels are covered in software, and renderer policy keeps all non-LCD texels on the
    ordinary blend. Revisit if upstream adds color-glyph rasterization.
- **WGPU hidden-state restore has bounded scope.** eframe's `CreateNew` path releases the
  surfaces, managed textures, renderer and device that it owns while TaskMan is hidden.
  `WgpuSetup::Existing` keeps its configured device handle by design, and an application
  retaining another `RenderState` clone or custom paint-callback GPU resources can keep
  memory resident or require callback recreation. TaskMan uses `CreateNew`, retains no
  clone, and installs no WGPU paint callbacks. Automatic recovery after a hard device-loss
  event and capability drift on a replacement device are also not implemented; normal
  suspend/recreate selects the same surface-compatible adapter.
  - **Generic eframe viewport residuals:** TaskMan uses only the root viewport. A hidden
    immediate viewport currently skips its callback even though egui's immediate-viewport
    contract expects it to run, and the first X11 map can still tear down/recreate the
    renderer once between `Visibility::YesWait`, `Occlusion(false)` and `VisibilityNotify`.
    Neither path is reachable in TaskMan's current single-root UI.
- **Wayland hidden-state signal depends on xdg-shell v6.** The winit fork maps the
  `SUSPENDED` xdg-toplevel configure state to `Window::is_minimized()`, requests a redraw on
  every configure, and clears a pending frame-callback throttle when the suspended bit
  changes. A compositor that does not advertise the state (or omits it when minimizing)
  provides no client-side proof that the window is hidden, so the Vulkan window can remain
  resident on that legacy/non-compliant path.
- **Software rendering performance: SUPERSEDED.** CPU mode directly rasterizes
  the UI. The old WARP path measured about 14 cores at 2.9 fps. D3D12 and Vulkan
  select a surface-compatible GPU.
- **Stack overflow under `TASKMAN_FPS_PROBE=1` on the software renderer.** Forcing
  continuous repaints crashes the process after a few seconds with "thread 'main' has
  overflowed its stack". Diagnostic-only: normal operation is event driven and stable
  (verified over repeated 15-20 s runs), and the GPU backends do not reproduce it.
  What is *known*, so it is not re-investigated from scratch:
  - It is **not** the rasterizer. Replacing the entire paint with a no-op still
    overflows, while routing the same frame through the slower tessellated path does
    not. It tracks frame *rate*, not painted content.
  - It is not re-entrancy of `run_ui_and_paint` -- a thread-local depth guard never
    fired -- and not `softbuffer::present`, which was stubbed out with no effect.
  - Upstream documents a related Windows behaviour: an invisible window burns a whole
    core (emilk/egui#7776), mitigated there by a 10 ms sleep. That mitigation is in
    place here and does not prevent this.
  The remaining suspicion is window-procedure frames accumulating around the paint at
  high repaint rates; confirming it needs a native stack trace, which a Rust stack
  overflow on Windows does not provide. Not chased further because the trigger is a
  diagnostic env var.
- **A translucent strip below the caption is not reachable from here.** The
  caption itself is painted to match what the app draws under it
  (`win/window_chrome.rs`: caption/text/border colours plus
  `IMMERSIVE_DARK_MODE`), and the Windows 11 backdrop is requested when the
  user has "Transparency effects" on — but DWM composes that material BEHIND
  the window, and it can only show where the window is transparent. It is not:
  the default renderer is the CPU one, presenting through `BitBlt` from a DIB
  whose alpha byte is always zero, so the client area is opaque by
  construction. An explicit caption colour also wins over the material on the
  caption, which is the deliberate trade — an exact colour match makes the
  caption and the search strip read as one surface, which a translucent
  caption over an opaque strip would not.
  Doing it properly means a presentation path carrying per-pixel alpha:
  writing 0xFF alpha everywhere except a declared glass region in
  `software_integration.rs`, `DwmExtendFrameIntoClientArea` for that region,
  and `with_transparent(true)` plus an alpha-aware clear on the wgpu/glow
  paths. Not attempted; it is a fork-level change that cannot be verified
  headlessly.

- **CPU compatibility:** current `cpu_load.rs` intentionally matches the
  standardized time-based CPU metric. The legacy frequency-weighted
  **CPU Utility** provider/column/switcher still does not exist; do not mutate
  the current accountant into the old metric.
- **Advanced Windows diagnostics:** live kernel dumps and their settings,
  analyze-wait-chain, processor-group-aware affinity above 64 logical CPUs,
  and an optional Efficiency-mode confirmation preference.
- **Shell/accessibility:** Settings is a resizable scrolling dialog rather
  than the native navigation page; high-contrast tokens, text-scaling
  validation, and multi-monitor-aware position restore remain.
  **Keyboard-only interaction: LANDED** (2026-09-25) — page switching, the
  native-TM table selection model, dialog tab-through and modality, focus
  rings and shortcut help; the deliberately accepted residuals are in
  § Keyboard accessibility accepted limitations below.
  **Screen-reader semantics: PARTIAL** (2026-09-25) — tables, the
  Performance card list, the charts, the splitters and the buttons now build
  a structured accesskit tree (list/list-item with set position and size,
  splitter with its value, the keyboard-selected chart sample as the chart's
  value). Not done: the Process Explorer-style hierarchy, and full
  announcement of live-updating values. The menu/dialog key contracts above
  (first-entry focus, stale-handoff death) are unchanged by this.
  **Menu keyboard navigation: DONE** (2026-09-17) — a keyboard-opened menu
  focuses its first enabled entry; arrows/Enter/Space ride egui's focus
  system. The focus request is stored against the popup id (a bare flag was
  eaten by the popup's sizing pass, where no entry is enabled), and a request
  nothing consumed dies with its menu.

The Modules inspector is deliberately on-demand. Linux provides a read-only
inventory by merging contiguous executable file mappings from
`/proc/<pid>/maps`; module unloading remains Windows-only. The Windows unload
command attempts ANY enumerated module — which module to unload is explicitly
the user's call (product decision 2026-09-06; there is no "protected module"
concept), gated only by the confirmation dialog and its crash warning. What
remains is technical, not policy: same-architecture targets only (a 64-bit
FreeLibrary address cannot be injected into a 32-bit process), process-level
trust rules at the broker (critical/system/requesting-GUI targets refused),
TaskMan refusing to act on itself, and the exact process identity + module
base/path revalidation at action time. The unload repeats FreeLibrary until
the module leaves or the bounded budget is spent, and the honest
`ModuleUnloadOutcome` (still mapped, with how many references were dropped) is
what the user sees; permanently pinned or re-loaded modules stay.

## Process Properties list pages (2026-10-21)

- Linux thread inspection comes from `/proc/<pid>/task`; procfs exposes no
  thread entry routine or cumulative CPU cycles, so those fields remain
  unavailable. Missing context-switch counters are also unknown, not zero.
- Windows thread start addresses are `module+offset`, never symbol names: no
  PDB or symbol-server lookup (privacy, latency), and nearest-export names
  would mislabel the non-exported routines most threads start in.
- Start address and cycles of another user's (service/SYSTEM) threads need a
  thread handle an unelevated GUI does not get. The exact-identity-bound
  `ProcessThreadDetails` request (introduced in v8) supplies them as
  LocalSystem when Windows allows the target's thread handles to be opened.
  Protected-process threads can still refuse even that token; they remain "—"
  and the page states how many records are partial rather than fabricating
  values.
- The connection tables carry only a PID. The identity is re-checked
  against the newest snapshot before each query and before showing it; a
  PID recycled inside that sub-second window is accepted.
- Origin-thread activity is deliberately scoped to threads whose START address
  lies in the module. It is not module-code CPU: a thread can execute code
  from many modules, runtime/JIT threads may not map to the application DLL,
  and a module that starts no thread shows “None started here.” Injected hooks
  are the common case: RTSS's `rtsshooks64.dll` can be loaded and executing on
  D3D render threads while legitimately showing zero threads started inside
  it.
- Sampled code columns close the execution-evidence gap without claiming
  exact CPU attribution. They collect no call stacks and do not symbolize;
  code in JIT or other memory outside the current image ranges is counted as
  unattributed, and ETW loss makes counts lower bounds. Sampling is available
  only while Modules is visible and depends on the installed LocalSystem
  service (or an elevated GUI fallback with `SeSystemProfilePrivilege`). These
  are accepted properties of a low-overhead sampled measurement, not a hidden
  exact-total claim.
- Protected system and security processes can refuse every user-mode module
  read; the protocol-v9 service therefore reconstructs their inventory from
  kernel loader events. Known Image_Load versions 1–3 are decoded, and the
  load-only version-0 layout uses the event-header PID. Unknown versions fail
  closed. If the session cannot start, a target record cannot be decoded, or
  ETW reports unknown/non-zero loss, the page reports unavailable rather than
  substituting a partial list.

## Keyboard accessibility accepted limitations (2026-09-25)

Recorded so the next accessibility pass does not re-derive them; the
interaction model itself is in `current.md`.

Resolved later the same day and moved to `current.md`: dialogs are now real
modals (the fork grew `Window::modal`, see `vendor/egui/TASKMAN-FORK.md`), the
chart hover readout is no longer pointer-only, and the Users column
drag-reorder has a keyboard route like every other reorderable table.

- **A mouse click lights the clicked row one frame late in five of the
  seven tables.** Only App history re-reads the selection inside the row
  closure (plus `repaint_row_selected`) so a click highlights same-frame;
  the other pages paint the new highlight on the next frame. Cosmetic
  latency, no keyboard impact.
- **Performance cards: the card itself is not a focus stop.** Selection is
  still the indicator and the cards stay non-focusable like table rows, so
  the whole list is ONE egui focus target that paints a ring around the card
  column. Screen readers get the position/size through the accesskit list
  node and `set_active_descendant`, but there is no per-card ring, so a
  sighted keyboard user sees the column, not which card inside it is
  selected beyond its own highlight.

## System state this program can leave behind

Found in the 2026-09-21 side-effect audit. Both are understood and accepted;
neither is a defect to "fix" without weighing the same trade-off again.

- **The IFEO registration outlives a deleted app.** With the Task Manager
  replacement enabled, `HKLM\...\Image File Execution Options	askmgr.exe`
  carries a `Debugger` value pointing at taskman. If every copy of the binary
  is then deleted, Windows keeps launching that command for `taskmgr.exe` and
  for Ctrl+Shift+Esc, `CreateProcess` fails, and the user is left with NO Task
  Manager and no in-app way to recover — the app that owns the registration is
  gone. What IS covered: a moved or renamed copy is detected
  (`replacement_target_missing`) and repaired to the running exe on the next
  start, a third party's registration is never overwritten or deleted, and
  `validate_target` rejects registrations that could not launch. What is not,
  and cannot be from inside the process: the user deleting the last copy while
  it is registered. `--core-service=uninstall` deliberately does NOT clear the
  value either — it removes the privileged broker, and the replacement keeps
  working without one, so clearing it there would break a working setup for
  someone who only wanted the service gone. Recovery is manual: delete that
  `Debugger` value. A standalone signed uninstaller is the only place this
  could be handled properly, and it does not exist (see the item below).
- **sysinfo holds a `PROCESS_VM_READ` handle per enumerated process.** That is
  the dependency's own enumeration, not a call this code makes — the hot
  sampler path itself only ever asks for `PROCESS_QUERY_LIMITED_INFORMATION`,
  and the intrusive rights (`PROCESS_CREATE_THREAD | VM_WRITE` for module
  unload, `VM_READ | DUP_HANDLE` for minidumps) are reached only from an
  explicitly confirmed user action. The consequences are real but bounded:
  a long-lived VM_READ handle on a game process is the pattern kernel
  anti-cheat looks at (a protected process simply refuses the open, which is
  the common outcome), and the handle is part of what pins a crashed process's
  `EPROCESS` — already mitigated by `should_release_handles`. Changing it
  means patching or replacing sysinfo's Windows enumeration, which is a much
  larger change than the residual risk justifies.

## Core-service production hardening still outstanding

**Accepted security residual risk (documented in
`hardening/core-service/hardening.json`, do not re-raise as new):** any code
running as the authorized user can inject into the installed GUI process (or
run it under a debugger) and thereby issue broker requests with that GUI's
identity. The broker's client check is image-path based; the pipe DACL is the
authorization boundary and it authorizes the user, not the binary. Mitigating
this needs Authenticode signing plus a code-integrity/process-trust policy
(and possibly a signed service-side allowlist), not another path check. The
2026-09-08 security pass additionally requires the authorized SID to be a
user account (`SidTypeUser`) and decouples file logging from broker startup;
see `core-service.md`.

The service boundary, ACL installer, bounded protocol, identity checks, and
recovery policy are implemented. These release-engineering/operational items
remain follow-up rather than being simulated in headless tests:

- Sign both binaries and add publisher/signature verification to install and
  update policy. Protected paths plus pinned SHA-256 hashes prevent ordinary
  user replacement, but signing is still the right production provenance
  layer.
- Exercise install, upgrade, rollback, reparse/hard-link/pre-opened-handle
  attacks, ACL readback, SCM failure recovery, multi-session denial, and no-service fallback in a
  disposable Windows VM. The automated gate intentionally does not mutate the
  developer machine's Program Files, ProgramData, registry, or SCM.
- The 2026-09-24 upgrade change removes the denied LocalSystem process-handle
  prerequisite and waits for SCM `Stopped`. It still needs a live running-
  service upgrade and injected mid-copy failure test in that VM; source and
  unit tests alone cannot establish rollback behavior.
- The pipe authorizes exactly one installing user SID. Multi-user support must
  add explicit per-user enrollment/revocation and auditability; broadening the
  DACL to `Authenticated Users` is not acceptable.
- Two workers, queue depth 16, 64 KiB requests / 512 KiB responses, and 20
  pipe instances bound broker resource growth, but service-side I/O is
  synchronous. An authenticated client can stall both workers. The GUI's
  telemetry worker has a bounded response wait; slow control operations and
  server-side cancellation still require VM fault injection.
- Service-only removal (`--core-service=uninstall`) removes the service
  registration but deliberately keeps binaries/data and the Task Manager
  replacement. Full setup uninstall restores an owned replacement pointing at
  the installed GUI, stops the service, removes program files and shortcuts,
  then removes the ARP entry. See `installer.md` for self-relocation and failure
  behavior; personal settings survive.

- **Open from the 2026-10-03 audit (verified, not yet fixed; need a VM or a
  design decision rather than a local patch):**
  - `%ProgramData%\TaskMan` / `broker.json`: an existing directory is reused
    after a share-mode pin and a DACL rewrite, but a handle opened earlier with
    `WRITE_DAC`/`WRITE_OWNER` keeps those rights, and `load_manifest` does not
    check the manifest's owner/DACL. Fix direction: refuse or move aside a
    pre-existing directory not owned by SYSTEM/Administrators, verify owner
    and DACL through the open handle, and compare the client token's SID with
    `authorized_user_sid` (`ImpersonateNamedPipeClient`).
  - `instance.rs`: all launches share the auto-reset Show/Shown events, so two
    quick launches against an elevated primary can merge acknowledgements and
    one of them starts a second, unregistered instance after its ~5.5 s wait.
    Needs a per-request sequence in the shared `Primary` section.
  - `instance.rs` labels the coordination objects low integrity
    (`S:(ML;;NW;;;LW)`); a medium label (`ME`) would still serve medium
    launches into an elevated primary while keeping sandboxed low-IL
    processes out. Unverified against the hotkey/IFEO launch path — test
    before changing.
  - When the broker has to wait for a pipe instance (`PIPE_REARM_BACKOFF`),
    the pipe can briefly have no instance at all once every client lets go;
    a squatter could create the name in that window. Re-creating with
    `FILE_FLAG_FIRST_PIPE_INSTANCE` whenever the broker knows it holds no
    instance would close it; instance ownership is not tracked today.

## Deliberate deviations / session-limited fixes

- **Efficiency-mode UI latency** (2026-08-26): after a toggle the UI waits
  for the next sample (plus one forced refresh) to reflect Windows'
  returned EcoQoS state. A spinner/pending affordance could be added if the
  ~1 s gap bothers users; correctness deliberately wins over optimistic UI.
- **Grouped process labels show whole-subtree counts even when collapsed**
  ("Brave Browser (43)" with children hidden). This now MATCHES native TM;
  noted so it isn't "fixed" back to direct-children counts.
- **Always-on-top only reaches the Start menu when enabled at startup**
  (2026-09-09). Native Task Manager creates its window in window band 16
  (`CreateWindowInBand`), above the Start menu's band 6. A band-16 window is
  implicitly `WS_EX_TOPMOST` and cannot be demoted: `SetWindowBand` fails with
  `ERROR_ACCESS_DENIED`, and both `SetWindowPos(HWND_NOTOPMOST)` and clearing
  the exstyle leave the window topmost. The band is therefore chosen at window
  creation from the persisted `always_on_top` setting (vendored `vendor/winit`
  patch; `TASKMAN_WINDOW_BAND=16`). Toggling the setting at runtime still
  applies ordinary band-1 topmost, and the settings dialog shows the existing
  "Takes effect at the next start." hint while the live value differs. Do not
  add polling or shell-fighting workarounds.
- **Windows can drop the Ctrl+Shift+Esc hook without telling anyone**
  (2026-09-23). A `WH_KEYBOARD_LL` callback that misses `LowLevelHooksTimeout`
  even once is silently removed (Windows 7+), and there is no API to detect
  it. The hook thread is kept off every blocking path so that should not
  happen, but a machine thrashing hard enough to stall a HIGHEST-priority,
  non-throttled thread can still cause it; the replacement then keeps working
  everywhere except over fullscreen games. Since 2026-10-21 any launch that
  hands off to the running instance (Start menu, taskbar, Ctrl+Alt+Del screen,
  Explorer's hotkey) re-installs the hook, so the gap lasts only until the
  next such launch rather than until TaskMan restarts. Periodic
  re-installation was rejected as polling on the input path.
- **Hotkey cases the hook cannot cover** (2026-10-21):
  - An ELEVATED foreground window while TaskMan runs unelevated: low-level
    hooks are expected not to see input aimed at higher-integrity windows
    (UIPI; not verified here). Explorer's path still works there, except
    over fullscreen apps. "Always start elevated" covers it.
  - An application that swallows Ctrl or Shift itself: the system never
    sees the modifier, so there is no combo to match.
  - A newer hook that re-installs itself on every keystroke between the
    chord and Escape would still win; nothing known does this.

- **Small accepted residuals from the 2026-10-03 audit:**
  - `format::format_date` renders a UTC calendar date (App history "since",
    Details process start). tm-core cannot query the time zone; shifting it
    needs a platform-provided offset. Documented on the function.
  - `logging.rs` drains the early ring before the file writer is switched on,
    so a line logged by another thread inside that window can be lost.
  - Machines with more than 64 logical CPUs: per-core counters cover the
    current processor group while per-process times span all groups.
  - `gpu::process_gpu_view` rescans every GPU record per process
    (processes x records per tick); fine at desktop scale.
  - macOS has no system thread-count source without Mach FFI, so its
    Performance page still shows 0 threads; Linux/macOS disk byte rates read
    0 B/s on the first sample (the model has no unknown rate).

### 2026-11-23 Linux privileged-action residuals

- **Linux Run no longer interprets shell syntax.** It tokenizes quotes and
  escapes and launches the resulting argv directly, so pipes, redirects,
  command substitution, globbing and `;` remain ordinary arguments. This is
  deliberate: the same action must never become arbitrary text on a future
  privileged surface. A user who needs shell composition should run a shell
  explicitly and remains responsible for that unprivileged launch.
- **Linux Properties uses the containing directory.** `xdg-open` has no
  standardized properties verb, so the action opens the parent directory
  rather than opening the file with an unrelated default application. A
  proper properties dialog needs a FileManager1 implementation or
  file-manager-specific integration.
- **Privileged process actions need a protected helper.** A portable install
  deliberately refuses to elevate the mutable TaskMan image, so kill/suspend/
  resume, priority and affinity remain limited to processes the same user may
  already control. A packaged deployment needs a root-owned helper and,
  optionally, a narrowly scoped TaskMan Polkit action.
- **Linux has no elevated GUI relaunch.** A portable install hides and rejects
  that action instead of executing its own user-writable image as root. The
  current Settings elevation block therefore reports status only.
- **There is no TaskMan-specific service-control policy.** The active Polkit
  path invokes trusted `systemctl`, so authorization follows the desktop's
  normal systemd unit-management policy. No installer or daemon is required.

### 2026-11-22 reliability-review residuals

- **macOS has no application-icon source.** `MacActions` implements neither
  `process_icon_rgba` nor `default_app_icon_rgba`, and the UI deliberately
  draws no icon of its own, so macOS rows keep an empty icon cell. Implementing
  it needs `NSWorkspace`/`objc2-app-kit` FFI that no gate on this host can
  compile or exercise; the 0.1.32 changelog is scoped to Windows/Linux until
  that exists.
- **Icon textures are keyed by image path and never invalidated.** Replacing an
  app at the same path keeps the old icon until TaskMan restarts. Re-reading
  XSettings per lookup or statting every row per frame was rejected as
  UI-thread work; a future fix should re-key on the theme and the file's mtime
  and invalidate explicitly. The resolver's own snapshot (theme chain,
  desktop-file index) is the other half of this rule — see
  § 2026-11-24 Linux CPU-renderer, per-process CPU, and icon residuals.
- **Restore acknowledgement still falls back to a 2 s deadline.** The
  "painted" signal is the app's `ui` pass (main.rs), which runs before the
  software backend's `present_rects`; a failed present therefore still lets
  the ack deadline fire with a warning. A precise signal needs a post-present
  callback in the eframe fork; the minimized-window re-fit now happens in the
  ack path.
- **Startup rows and their derived targets are published under two locks.**
  The worker writes `startup_derived` then `startup_cache`, and the UI reads
  them in that order; a descheduled writer can still present new targets with
  the previous row list for one frame. Visual-only; fixing it means publishing
  both in one `Arc` and touching five call sites.
- **Persistence durability is still rename-based.** `replace_file` fsyncs the
  file but not the parent directory (Unix) and does not use `ReplaceFileW`
  (Windows), so a power loss immediately after a save can lose that one save
  (the previous file remains) and the replacement inherits the directory ACL.
  Stale temp files are now swept on the next save.
- **A persistently `WS_EX_TOPMOST` game still outranks a restored window.**
  `raise_above_topmost` briefly applies topmost and demotes unless strict
  topmost is enabled, so it wins against a fullscreen game that only *looks*
  topmost. Keeping TaskMan above a real topmost game for its active lifetime
  would need the existing strict-topmost hooks; not done without a desktop
  repro.

### 2026-11-23 Linux activation/autostart residuals

- **Linux activation success requires observed X11 focus.** The primary queues
  `Visible(true)`, `Minimized(false)`, `Focus`, and attention, then waits for a
  later UI pass reporting `minimized=false` and `focused=true` before
  acknowledging. A failed or missing focus transition is rejected so the
  waiting launch opens independently. This is winit/egui state, not a direct
  compositor acknowledgement, and Wayland does not attempt the handoff because
  its programmatic focus path is unavailable.
- **Activation shutdown can wait for one in-flight handoff.** The listener's
  eventfd interrupts accept polling, but a peer already inside a bounded
  connection read/write can delay GUI shutdown by up to the five-second
  handoff deadline.
- **The coordinator is per `XDG_RUNTIME_DIR`, not per graphical seat.** Two
  simultaneous graphical sessions for the same Unix user can therefore share
  one activation endpoint. The owner-only directory and same-UID
  `SO_PEERCRED` check prevent cross-user access, but no display/seat parsing
  is performed.
- **A degraded independent window does not usurp a wedged lock owner.** It
  opens normally, but later launches continue probing the old owner until its
  own handoff deadline expires. The lock is deliberately not force-reclaimed;
  the remedy is to terminate the wedged owner or let it exit.
- **Linux autostart starts visibly.** This backend has no tray implementation
  or supported hidden-start mode, so Settings passes `start_minimized=false`
  and the desktop entry omits `--minimized-to-tray`. Adding a tray needs
  desktop-shell/system-package assumptions that are intentionally out of
  scope.
- **The autostart entry pins the current executable path.** Moving an
  unpacked binary after enabling startup leaves the old `Exec` until the user
  toggles the setting again. Startup does not silently rewrite the entry,
  because the Startup page can deliberately set `Hidden=true` on the same
  file and a later launch must not undo that choice. For the same reason, a
  Startup-page toggle of that file does not update the Settings checkbox.
- **Autostart parent ancestry still has a same-user TOCTOU window.** The final
  `autostart` directory must be real, current-user owned and not group/other
  writable, and final publication is no-replace/atomic. Symlinked ancestors and
  a same-user swap between validation and path-based rename/unlink are not
  eliminated with directory-FD `openat`/`renameat`/`unlinkat` operations.

### 2026-11-23 remaining Linux parity contracts

- **Per-core history is positional, not CPU-ID keyed.** The Linux sampler
  detects topology changes and inserts a gap, but labels vectors as CPU 0..N.
  A non-contiguous online set such as CPU 0 and CPU 2 is presented as CPU 0
  and CPU 1 until the model carries logical IDs alongside percentages.
- **GPU history identity is the display index.** Linux derives `GpuInfo::id`
  from sorted card enumeration; if cards appear or disappear, a different
  adapter can inherit the same history slot. A stable PCI/sysfs identity is
  needed for multi-GPU hotplug.
- **Users maps only exact `session-N.scope` cgroups.** That is deliberate and
  avoids guessing from usernames, but some systemd user-manager application
  scopes under `user@UID.service` have no session component and therefore do
  not contribute to session rollups.
- **Native-Wayland App/Background roles are launcher evidence, not compositor
  facts.** Only a standard `app-*` unit or Snap launcher in user `app.slice`
  supplies cgroup app-family identity. It can identify an App only when its
  complete environment carries a non-whitespace Wayland/X11 display variable.
  Arbitrary well-formed app.slice units remain Unknown unless the same
  graphical gate and weak user-session evidence accompany a trusted
  Flatpak/Snap/GIO/BAMF per-process hint; an inherited display variable alone
  is not identity. A complete headless recognized app unit is Background and
  unavailable/partial/oversized evidence stays Unknown. Native Wayland still
  has no portable whole-window inventory, focus signal or exact compositor
  mapping. Applications outside these conventions, malformed/ambiguous cgroups
  and exact X11 no-window evidence can leave a process Unknown or Background
  rather than recovered. GNOME Shell.Introspect (sender-allowlisted) and
  interactive KWin APIs are deliberately not used. WSL without real systemd
  PID 1 stays Unknown for its synthetic `/init.scope` evidence unless X11
  ownership supplies independent evidence.
- **Linux desktop-evidence cache has a bounded same-generation staleness
  window.** Cache hits now revalidate `/proc/<pid>/stat`, so PID reuse removes
  the old entry immediately. `execve` and cgroup movement preserve PID/start
  ticks, however; an environment-changing exec or unit move can retain the
  prior launcher role, session and app group until the jittered 4.5-5.5 second
  expiry. This trade keeps procfs reads bounded and is pinned by deterministic
  same-generation/TTL tests; it is not an immutability claim.
- **`tm_core::classify::ClassifyInput` gained a required public
  `ProcessPlatform` field.** In-tree callers now distinguish Windows from
  Unix name/PID rules, preventing cross-platform spoof promotion and false
  low-PID System results. Downstream exhaustive literals must set the field;
  if `tm-core` is consumed externally, treat this as a deliberate API break or
  add a compatibility constructor/builder in a later contract change.
- **`is_core_os_image` remains a union of Windows and Unix presentation
  boundaries.** Classification itself is platform-aware, but the shared tree
  boundary used by the Processes page and Windows sampler still treats
  cross-platform kernel-looking names as boundaries. This can affect family
  nesting for a hostile or coincidentally named app, not its category; a
  platform-specific boundary helper would close that residual.
- **Native App-root selection is deterministic, not semantic, when no member
  has a window.** The collector prefers any positively windowed app-family
  member and otherwise chooses creation ticks then PID. A short-lived
  native-Wayland wrapper with no observable window can therefore still be the
  representative; a richer selection key would need additional launcher or
  compositor evidence.
- **Linux-only tests are still not executed by the Windows host gate.** The
  final gate cross-checks the non-host targets, which type-checks Linux tests
  only if the test targets themselves are included; parser/live-procfs
  regression tests still need a native Linux CI runner.
- **`tm-platform::Capabilities` remains a public exhaustive struct.** New
  fields preserve the in-tree callers and the deprecated startup method alias,
  but downstream exhaustive literals and old field access still break. If the
  crate is consumed externally, introduce a builder/accessor contract or a
  deliberate compatibility major bump.

### 2026-11-24 Linux CPU-renderer, per-process CPU, and icon residuals

- **A Wayland frame is always painted in full.** The tile tracker may only
  paint incrementally into a buffer whose contents are exactly one frame old,
  and softbuffer's Wayland backend keeps two `wl_shm` buffers and swaps them per
  present, so a leased buffer is 0 or 2 frames old and never 1. Wayland
  therefore full-paints every frame and the 64×64 damage path only saves work on
  X11 and Windows. Restoring tile skipping there needs a damage cache PER buffer
  slot, not a per-frame one; do not "fix" it by reintroducing a local
  "we presented something" flag, which is exactly what left black boxes and
  stale regions on screen.
- **X11 and Wayland have no OS invalid-update region.** Windows merges
  `GetUpdateRect` into the present set, so a frame with zero changed egui tiles
  still blits what the compositor asked for. Neither softbuffer backend offers an
  equivalent, so a zero-damage X11 frame presents nothing. A surface the
  compositor discards is only recovered through the force-full-paint path (a
  resize, a reveal, an unpainted surface), never by a delivered damage rectangle.
- **Linux per-process CPU cannot attribute everything.** A process that exits
  inside a window takes its last ticks with it, and irq/softirq time is not
  attributable to a process at all, so the rows sum to slightly less than the
  machine header (measured ~0.6 % of capacity). A process first seen in a window
  reports `None` — one dash for one window — rather than a fabricated 0 %;
  Windows credits newborn processes their in-window time, which is the honest
  difference between the two accountants, not a bug to be evened out.
- **Icons: PNG only, and a miss costs a chain walk.** The resolver has no SVG
  rasterizer (an offline build cannot link one), so an app whose theme icon
  exists only as SVG falls back to the generic `application-x-executable` glyph.
  A footprint miss walks the whole transitive theme chain and every size
  directory in it before giving up — roughly 3.4k `stat` calls per distinct
  missing image, deduped by the icon cache's 30 s failure retry TTL and, on
  Processes/Details, behind the per-frame icon memo, so neither a repaint nor a
  scroll frame repeats it. A footprint hit is cheap.
- **Icon theme resolution is a process-lifetime snapshot.** The theme chain and
  the desktop-file index are resolved once per process (the texture half of the
  same rule is § 2026-11-22 above), so switching the desktop icon theme keeps the
  chain the process started with. Re-resolving per lookup would mean reading
  XSettings and walking `index.theme` files on the UI thread for every row.
  Cached *answers* are a narrower rule: an icon name's file, its decoded pixels
  and a theme's parsed layout are remembered for the process, but only when they
  resolved (a miss and an empty layout are retried), because the GUI's own
  `FAILURE_RETRY_TTL` retry is the only thing that recovers a transient failure.

## The D3D12 reveal is only as fast as its first frame (2026-11-25)

- The presentation gate means "the window is visible" is "a frame of ours is in
  the compositor", so the reveal can never be *earlier* than one presentable
  frame. The renderer work moved that frame earlier (a cleared primer with no
  pipelines, and a resident device across a minimize), it did not remove the
  rule. `REVEAL_MIN_TICKS` is still 2 and the primer does not shorten it.
- A backend that advertises `DUAL_SOURCE_BLENDING` but still refuses the LCD
  pipeline leaves the app rasterizing per-channel coverage while `egui-wgpu`
  draws it through the ordinary pipeline. `supports_subpixel_text()` fails
  closed for every deterministic cause (device feature and Naga validation of
  the shipped shader), and `Painter::ensure_renderer` logs one `log::error!`
  when it happens, but reconciling after the fact needs an atlas
  re-rasterization path that does not exist. Until then the mitigation is the
  log line, not a repair.
- A taskbar un-minimize bypasses `NativeApp::restore_window`, so that path
  emits no `restored_window_painted` mark; the gate's own `present gate:`
  debug lines and the eframe reveal line are what is left to date it.
- `RenderState::ensure_renderer` is first-call-wins for `RendererOptions` and
  the target format (documented on the slot, with a `debug_assert!` in
  `Painter::ensure_renderer`). A caller that needs different options must build
  a new `RenderState`, as kittest does.

## Platform test coverage: the Linux backend is never executed (2026-11-25)

- The gate type-checks Linux (`cargo check --workspace --target
  x86_64-unknown-linux-gnu`, plus `-p tm-platform --all-targets` when a change
  touches it) but nothing *runs* Linux code: CI is `windows-latest` only, and
  `crates/tm-platform/tests/integration.rs` is `#![cfg(target_os = "windows")]`.
  A `#[cfg(target_os = "linux")]` test therefore never executes anywhere, and
  `build.py`'s cross check omits `--all-targets`, so even `#[cfg(test)]` code
  inside a Linux-only module is only type-checked when someone runs the
  all-targets form by hand.
- Consequence for review: logic that must stay pinned has to be reachable from a
  non-Linux build. `crates/tm-platform/src/linux_telemetry.rs` exists for exactly
  that (meminfo/`nice`/topology/base-frequency parsing, the `/proc/stat` CPU
  count, DRM identity comparison, the demand-to-provider mapping), and
  `xdg_icons.rs` is platform-neutral on purpose. Behaviour that only exists as a
  call site inside `#[cfg(target_os = "linux")]` — the demand gates themselves,
  the identity bracket, `forget()` on a resumed disk window — has no automated
  regression test. Do not "fix" this with an unrunnable Linux test.
- The one wiring hop that does have host coverage is the engine's: a demand
  change sent to a running engine must reach the collector
  (`engine::tests::demand_sent_to_a_running_engine_reaches_the_collector`).

## Row/total audit leftovers (2026-10-21)

Found by the 2026-10-21 row-vs-total audit and deliberately not changed:

- **CPU on >64 logical processors:** `cpu_load::query_cores` uses
  `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` and
  `GetSystemInfo`, both limited to the calling thread's processor group,
  while process CPU times span all groups. On such machines per-process % is
  measured against group 0's capacity (inflated) and the header covers group
  0 only. Fix: `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` and one
  `NtQuerySystemInformationEx` call per group (needs an ntdll FFI
  declaration; the `windows` crate lacks it), keeping per-core indices in
  sync with sysinfo's CPU list. Not done because no >64-LP machine was
  available to validate it.
- **Startup impact is keyed by image path** and uses `IO_COUNTERS` bytes, so a
  shared image (`powershell.exe`, `rundll32.exe`, a browser) is charged for
  every instance in the boot window, including network/pipe bytes.
  Restricting it to the processes a startup entry launched needs launch-tree
  heuristics; the module docs state the current rule.
- **Disk I/O column vs its header:** rows are `IO_COUNTERS` bytes/s (cache
  hits, pipes, sockets), the header is the busiest disk's active %. This is
  the documented 2026-09-11 design (`Disk I/O` = bytes a process asked for;
  `Disk active time` = what reached a disk), not an oversight.
- **Loopback:** only 127.0.0.0/8, ::1 and ::ffff:127/104 are excluded from
  per-process network; traffic to the machine's own adapter address also
  stays inside the stack but is still counted.

## Mouse movement over the window repaints per move — MITIGATED 2026-11-18 (live verification pending)

- The 2026-11-17 raw-input fix (fork divergence #9) removed the cost of mouse movement
  *outside* a focused TaskMan window. Movement *over* the window used to repaint per
  `CursorMoved`, and the initial `c2a9612` / `d4e07dd` fixes left four gaps on the live
  Windows software renderer:
  1. `egui` tooltips defaulted to `show_tooltips_only_when_still = true`, causing
     `tooltip.rs:362` to call `request_repaint()` every frame while moving over any
     tooltipped widget (uncapped `ControlFlow::Poll` on software renderer).
  2. `tablekit::TmTable::row` set the boolean tracking hint for every hovered row,
     disabling the same-widget skip over almost the entire window.
  3. Repainting `CursorMoved` events called `window.current_monitor()` →
     `EnumDisplaySettingsExW` on every move.
  4. Each frame cleared, rasterized, and `BitBlt`-presented the entire window buffer,
     and the taskbar monitor's foreground `LOCATIONCHANGE` hook still hooked TaskMan's
     own UI thread while TaskMan had focus.
- Fixed in code (2026-11-18): `show_tooltips_only_when_still = false` in `tm-ui/theme.rs`,
  `POINTER_STABLE_RECT` + anchored `hover_text_at_fixed_pointer` replacing the row-wide
  hint, 2 s refresh-interval cache + `interact_radius`-aware `hover_target_changed` +
  trailing short-circuit + `PointerMoved` queue coalescing in `egui-winit`, 64×64 tile
  damage tracking + `present_with_damage` in `egui_software` / `software_integration.rs`,
  own-process skip in `taskbar_monitor/hooks.rs` (`sync_foreground_hook`) with app-side
  move/resize refresh, zero-alloc `sync_taskbar_monitor`, and `TASKMAN_INPUT_STATS=1`
  diagnostics. Live `TASKMAN_INPUT_STATS=1` + `typeperf` measurement on the installed
  Windows build is still needed to confirm the live CPU reduction.

## Falsified findings — do not re-raise

- "`process_ops::tests::service_catalog_discovers_active_services` is timing-sensitive"
  — falsified and fixed 2026-10-21. Intermittent failure was not timing but
  HashMap iteration non-determinism selecting per-user svchost services (e.g.
  `UdkUserSvc`, `cbdhsvc`) where the PEB command line has dynamic `-s <service>`
  appended by Windows while SCM's static `lpBinaryPathName` configured only
  `svchost.exe -k <group>`. The test now properly checks that the live command line
  matches or extends the configured service command line across all active svchosts.
- "heat_cells discovers maxima from one row" — fixed 2026-08-26 (P0.2);
  intensities are normalized per column over the full display model before
  virtualization (`tablekit::norm`, `normalize_heat`, users' `HeatMax`).
- "columns can't be resized" (drag delta handling) — root-caused earlier;
  egui `drag_delta()` accumulation onto the LIVE width is correct behavior.

## Per-process disk active time

- Shares are per physical disk (`DiskNumber` of the `DiskIo` record = the
  `PhysicalDrive<n>` number of the PDH `PhysicalDisk` instance). The
  number match is asserted only in the ignored elevated integration test.
- `Disk active time` is measured, so it is only available where an ETW session
  can run: through the LocalSystem broker (the normal install) or in an
  elevated GUI. Without either it renders "—", like the Network column.
- Requests issued by a thread that had already exited when the window was
  drained cannot be attributed and stay in the unattributed remainder, so the
  per-process shares sum to at most 100 %, not exactly 100 %. Charging them to
  a guess would be worse than the gap.
- `Microsoft-Windows-Kernel-Disk` publishes no documented unit for
  `HighResResponseTime`, so it is never shown as a duration — only as a share
  of the window's total. `disk_etw.rs` keeps that rule.
- The payload offsets are hand-written. `disk_etw::tests` pins the decoder
  against synthetic records and rejects implausible ones; only
  `integration::disk_trace_attributes_real_requests_to_the_issuing_process`
  (ignored, needs elevation) proves them against live kernel events. Run it
  after any Windows build that changes the provider. **The system-logger
  provider switch of 2026-09-11 has not yet been proven on live events.**
- `Microsoft-Windows-Kernel-Disk` is NOT usable for attribution: its
  `DiskRead`/`DiskWrite` template ends at `HighResResponseTime` and carries no
  issuing thread. The events come from `SystemIoProviderGuid` on a
  system-logger session instead (Windows 10 2004+). On older builds the
  session cannot start and the column reports "—".
- The per-file trace (`file_etw.rs`) was proven on live events on build 26300
  (2026-11-25): opcodes 64–68/76, template version 3, exact byte counts in the
  ignored elevated test `the_file_trace_measures_exact_bytes_per_file`. Open
  limits: only version 3 is decoded (any other version is `Undecodable`, logged
  with opcode/version/length — re-probe on a new Windows build before adding
  one); a file touched only through a memory-mapped view shows no bytes
  (paging I/O is excluded on purpose); a completion whose request has not
  arrived yet (an `OpEnd` delivered before its own record, cross-CPU delivery
  order) charges nothing and increments nothing, so that one request's bytes
  are never attributed — `orphaned_requests` does not count it either: that
  counter holds the requests retired by a later record on the same `Irp` —
  another request, a Create or a Close — plus the ones dropped when the pending
  map hit `MAX_PENDING_REQUESTS`, so it is not a count of early completions;
  naming objects from the handle table needs `SeDebugPrivilege` for object
  addresses (LocalSystem and an elevated admin have it). A run of the service
  or an elevated GUI that is KILLED — no destructor and no panic hook — while it
  owns the exclusive NT Kernel Logger fallback leaves that session taken until
  the machine reboots: its name is deliberately never reclaimed (another
  application's trace would be stopped), while the per-role `TaskMan-File-*`
  sessions are reclaimed by name on the next start. The always-on session's
  memory is bounded by the maps' caps (worst case tens of MB in the service on
  a machine that fills them); not measured over days of uptime. Per-file
  history before the session started does not exist anywhere in Windows, so
  processes older than the service are counted from its start (stated on the
  page). Linux has no per-file source at all (would need
  eBPF on `vfs_read`/`vfs_write` with root) and keeps the columns
  `Unavailable`.
