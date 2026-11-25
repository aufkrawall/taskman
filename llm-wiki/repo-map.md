# Repo Map (code map)

Last cross-checked: 2026-11-24 (Linux per-process CPU accountant, icon theme chain, and the table repaint caches)

Primary sources:
- workspace tree (verified against working tree)
- `build.py`, `Cargo.toml`
- `crates/*/src/lib.rs`

## Core Tree

Rust workspace with six crates. The GUI and Windows service both reuse the
platform boundary (`tm-core` ← `tm-platform` ← `tm-app` / `tm-service`), and
`tm-ui` (theme/fonts) is shared between `tm-app` and `tm-installer`:

- `crates/tm-core`
  - Platform-agnostic heart. `model.rs` (Snapshot data model — CPU, memory,
    disks, networks incl. optional IP metadata (SSID/signal only behind the
    opt-in `wifi_details` setting), GPU incl.
    `AdapterLuid`, processes incl.
    elevation/UAC/power-throttle fields and `cpu_pct: Option<f32>` — `None`
    means the backend could not attribute CPU time to that process in the
    sample window, and it renders as "—", never as 0 %), `engine.rs` (sampling engine:
    lazy start via collector factory, event notifier, Refresh-while-paused),
    `settings.rs` (INI config + persisted Details tree view, per-table sort +
    per-image priority/affinity rules, tray/autostart options +
    bounded 4 MiB startup input +
    ID-keyed column prefs: widths, visibility overrides and user order under
    `[columns.<table>]` / `.visible` / `.order` sections + debounced
    SettingsWriter), `app_history.rs` (per-app usage db, single serialized
    writer thread with generations, late-load merge, Unix non-system
    eligibility; identity is the exe path, so an app first seen without one
    keeps a name-keyed row; icon paths are stored per key, blank ones are
    dropped on load and never written, and bare names are filtered before
    shell extraction — on Linux a bare name is a legitimate XDG lookup key),
    `demand.rs` (TelemetryDemand bitmask),
    `persist.rs` (crate-private: synced temp-file-then-rename replacement
    for every persisted file, plus `set_aside`/`discard_contentless` — a file
    that cannot be used is kept as `<name>.bad` instead of being overwritten
    by defaults, except a file that says nothing, which is removed so it can
    never take that name from a real copy),
    `startup_impact.rs` (measured startup cost per image: per-tick CPU-time
    and I/O deltas folded across the boot window, Microsoft's documented
    Low/Medium/High thresholds, and the persisted store — see the module doc
    for why this is measured here rather than read from SRUM),
    `logging.rs` (early ring sink → deferred file attach, wired from
    `app.rs` on the frame that starts the engine; the appender's `WorkerGuard`
    is owned by a process-lifetime static and returned to NOBODY —
    `tracing_appender`'s non-blocking writer is lossy and drops every record
    after the guard falls, which is how each shipped log file came to hold one
    single line. `shutdown()` is the controlled flush; the panic hook writes a
    crash record with a backtrace SYNCHRONOUSLY to `<log>.crash` because
    release builds are `panic = "abort"` and the async worker is never
    scheduled again, and it runs `set_panic_cleanup` first for resources that
    outlive the process. Elevated installer helper remains
    memory/console-only), `classify.rs`
    (conservative Apps/Background/System classification: required
    `ProcessPlatform::{Windows,Unix}` discriminator, then platform-specific
    kernel/core-image/PID rules, observed-window or explicit platform
    desktop-role evidence in precedence order; `is_core_os_image` remains the
    shared tree-boundary name list but never overrides a collector category),
    `i18n.rs` (DE/EN keys macro),
    `format.rs`, `mock.rs`.
- `crates/tm-platform`
  - OS collectors/actions behind traits (`actions.rs`, including explicit
    native-shell, elevation, login-start, and process-tree capabilities;
    `command_line.rs` provides shell-free argv parsing for Linux Run). Windows
    stack under `win/`: `sampler.rs` (sysinfo + NtQuerySystemInformation CPU accountant,
    time-based attr TTL cache with token security + command-line queries and a
    brokered token-identity fallback (bounded retry per process) for the
    SYSTEM/service processes no local token read reaches, plus role fallbacks
    with explicit identity evidence — verified Windows image or the kernel's
    own bare pseudo-process name — never a name match),
    measured CPU speed from PDH `% Processor Performance`, interrupt-time
    counter, synthetic CPU pseudo-rows "System Interrupts"/"Terminated
    processes"), `cpu_load.rs`
    (time-based CPU accounting incl. kernel/user split, new-process credit,
    unattributed-residual + exited-image attribution), `perfcounters.rs`
    (PDH split GpuPdh/DiskPdh groups with demand gating + LUID-preserving
    GPU instance parser), `gpu.rs` (DXGI discovery + LUID-keyed merge,
    busiest-engine semantics with per-engine sums across processes; the
    per-process view is restricted to the merged adapter set), `process_ops.rs` (kill/suspend/priority/
    affinity/EcoQoS/UAC virtualization/token security, with `refuse_self`
    rejecting the scheduling changes that would stall the keyboard hook thread, `token_identity`
    (account + string SID) and `IsProcessCritical`, SCM service catalog
    mapping PID → executable path, hosted service display name(s) and
    configured account, identity-safe minidumps, ToolHelp module
    enumeration, guarded same-architecture DLL unload, non-blocking launch),
    `process_threads.rs` (one process's threads from ONE handle-free
    `SystemExtendedProcessInformation` snapshot, identity-checked against that
    record; start address + cycles through a per-thread handle proven by
    owning PID + creation time; the broker retries missing handle evidence as
    LocalSystem for an exact process and merges only creation-matched TIDs),
    `process_connections.rs` (IP Helper
    owner-PID TCP/UDP tables, v4 + v6),
    `startup.rs` (Run keys + Startup folders + StartupApproved incl. the
    folder-subkey fix and best-effort publisher resolution), `services.rs`,
    `users.rs`, `image_path.rs` (`NtQuerySystemInformation(SystemProcessIdInformation)`:
    the image path of a process no handle can be opened for, plus NT-device to
    drive-letter translation; both answers cached because neither changes while
    a process lives), `icons.rs` (shell icon extraction via `SHGetFileInfoW`,
    plus `default_app_icon_rgba` — the shell's generic application icon from
    `SHGFI_USEFILEATTRIBUTES`, i.e. what Explorer shows for an icon-less `.exe`;
    extraction is serialized process-wide, initializes COM per thread (the
    shell icon cache is not safe to hit concurrently), and refuses blank or
    bare names — the shell answers an empty name with the CURRENT DIRECTORY's
    folder icon),
    `net_info.rs` (cached adapter/link/IP metadata plus
    `wifi_details()`, the ONLY location-gated call — SSID/signal, opt-in via
    `Settings.wifi_details`),
    `etw.rs` (shared real-time-session plumbing: fixed per-role session names,
    orphan reclamation, provider enabling, the `ProcessTrace` worker thread and
    the handle-table id sanity check, plus normal/panic session teardown),
    `net_etw.rs` (per-process network bytes
    from `Microsoft-Windows-Kernel-Network`; the session name MUST stay fixed
    per role — a pid in it leaks an orphaned session on every kill until the
    provider stops delivering events), `disk_etw.rs` (per-process DISK service
    time per physical disk (`DiskNumber`) from `Microsoft-Windows-Kernel-Disk`, attributed by `IssuingThreadId`
    through a thread map seeded from ToolHelp and maintained by the
    `Microsoft-Windows-Kernel-Process` thread events; a request whose thread
    cannot be mapped is never charged to a row),
    `file_etw.rs` (per-file read/written bytes from the kernel's file I/O
    hooks: the session is a system logger that enables the SystemIo provider
    with `SYSTEM_IO_KW_FILE`, a provider call rather than the legacy
    `EVENT_TRACE_FLAG_FILE_IO` flag, and the records are classic `FileIo` ones —
    class GUID in `ProviderId`, operation in the opcode 12–16 — because a
    manifest `EnableTraceEx2` for `Microsoft-Windows-Kernel-File` is accepted
    and delivers nothing; Create correlates a `FileObject` with its
    name, Read/Write add `IOSize` to that (pid, path) pair, both template
    versions are decoded at native pointer width, device paths are translated
    to the DOS paths the handle walk reports, the open-file map and both the
    per-process and session-wide path budgets drop with a counter, and buckets
    are bound to the created time that prevents pid reuse from inheriting; the
    sample becomes rows in `attach_file_io`, the one mapping both the service's
    session and the GUI's own elevated one share),
    `profile_etw.rs` (demand-only system-profile sampling for one exact
    process; MOF sample type 46, profile-privilege enablement, thread
    ownership, page aggregation, module attribution, ETW-loss/lower-bound
    evidence, and fixed profile sessions),
    `module_etw.rs` (protected-process fallback: known compact/full kernel
    image load/unload/DCStart/DCEnd versions, strict PID filtering, ETW-loss
    rejection, exact creation-time revalidation, and a
    settled-complete-or-unavailable contract),
    `core_service.rs` (versioned authenticated named-pipe broker, including
    bounded identity-bound process-security/token-identity/module/sample/thread
    evidence reads for protected targets, plus secure
    SCM/Program Files/ProgramData install lifecycle, including pinned
    reparse/hard-link-resistant owner/group/DACL repair), `autostart.rs` (owned-command-
    only HKCU startup migration), `taskmgr_replacement.rs` (owned IFEO
    `Debugger` registration for taskmgr.exe plus the guards that keep a
    registration launchable, and the explicit built-in-Task-Manager escape
    hatch which debug-creates taskmgr and immediately detaches so IFEO stays
    installed), `instance.rs` (session-local instance
    coordination: mutex + show/acknowledge events + a published pid/HWND
    section, low-integrity-labelled so an unelevated hotkey launch reaches
    an elevated instance; an unacknowledged request starts its own
    instance), `windows_enum.rs` (one-pass top-window/hung-state inventory;
    UWP `ApplicationFrameWindow`s are attributed to their hosted
    `Windows.UI.Core.CoreWindow` process, cloaked ghost frames are ignored,
    and a cloaked top-level `CoreWindow` counts only when an on-screen frame
    with the same application user model id claims it — the minimized-app
    case),
    `window_chrome.rs` (DWM caption colour / dark mode / backdrop / cloaking,
    `force_foreground` (never merges input queues; safe from any thread) and
    `force_foreground_attached` (`AttachThreadInput` with the foreground thread
    only + `SetForegroundWindow` over fullscreen games; activation only, hotkey
    worker only because it is the only caller that pumps its own queue; refused
    for a hung target) — plus an event-driven strict-topmost
    keeper that reasserts the root HWND on foreground/show events and UNHOOKS itself
    when always-on-top is turned off),
    `taskbar_monitor/` (module directory: `render`, `placement`, `occlusion`,
    `identity`, `fullscreen`, `uia`, `zorder`, `hooks`, `visibility`
    (explicit `Hidden(reason)`/`Revalidating`/`Shown` state machine),
    `window` (`WindowCtx` refresh pipeline), `thread` (`wnd_proc`, message
    loop), `test_support`; see `current.md` § Windows taskbar monitor —
    optional taskbar CPU/RAM/network monitor on a
    dedicated `"tm-taskbar"` thread: `WS_EX_LAYERED | WS_EX_TOOLWINDOW |
    WS_EX_NOACTIVATE` unowned popup in window band 16 via `CreateWindowInBand`
    with band-1 fallback, software Direct2D + DirectWrite into
    a cached 32bpp DIB and `UpdateLayeredWindow`; never `GetDC(NULL)`/`GetPixel`
    or cross-process parenting/ownership; hides when overlapping higher/peer
    popups/topmost windows occlude it, UIA occupancy is unknown, on taskbar
    auto-hide, and under fullscreen windows; context menu anchors outside the
    taskbar edge; visibility notifications restore background cadence when both
    surfaces hide),
    `hotkey_hook.rs` (dedicated `WH_KEYBOARD_LL` thread intercepting Ctrl+Shift+Esc,
    installed only while the IFEO replacement is registered and driven by a
    `RegNotifyChangeKeyValue` watch rather than polling; bypasses shell hotkey
    suppression in exclusive/borderless fullscreen games. The hook thread is
    exempted from EcoQoS — it carries every keystroke on the desktop, and a tray
    icon is exactly what Windows throttles — and nothing blocking, `tracing`
    included, runs on it while a hook is installed),
    `version.rs` (cached PE metadata, read through each file's own
    `\VarFileInfo\Translation` table with a UI-language preference — hardcoded
    English codepages silently blanked every binary shipping only a localized
    table). `xdg_icons.rs` (freedesktop icon-theme resolution used by the
    Linux backend's `process_icon_rgba`: an executable basename is matched
    against `Exec=`, `TryExec=` or the desktop file's own id across the XDG
    data dirs, walking `applications/` in sorted order because `read_dir` order
    let the filesystem decide which app owns a shared executable name.
    `TryExec=` is only a fallback — the spec means "is this program installed",
    not "another name for the app" — so it counts only where the entry's
    `Exec=` names a launcher (`env`/`sudo`/`setsid`/`nohup`/`nice` prefixes
    skipped with their flags, `python3 app.py` never adopts `app.py`) or is
    unusable, and a Flatpak launcher contributes its `--command=` instead of
    `flatpak` itself. The `Icon=` value is then searched in the full transitive
    theme chain: the configured theme (X11 XSettings, then GTK settings, then
    Plasma `kdeglobals`) or the spec `default` theme, `Inherits` expanded
    depth-first with a cycle guard, and `hicolor` last. `hicolor`-only discovery
    was the bug: on a stock Arch box it holds no PNGs, so nearly every row
    missed. Size directories come from `index.theme`'s `Directories=` plus
    `ScaledDirectories=`, rank by `Size × Scale` (a missing `Scale=` derived
    from an `@N` directory suffix) and prefer the directory native at the target
    size over an upscaled tie; search roots are `$XDG_DATA_HOME/icons`,
    `$XDG_DATA_DIRS/icons`, `$HOME/.icons` and every data dir's `pixmaps`. PNG
    only — SVG/XPM candidates are skipped per size directory, so an SVG-only app
    falls back to the generic glyph. PNG decode is capped at 64 px
    (`EXPAND | STRIP_16`); plus `default_app_icon_rgba`, the theme's
    `application-x-executable` / `application-default-icon` / `image-missing`
    chain, which is what a row without an icon of its own paints;
    platform-neutral so its tests run on the Windows host too).
    `linux/cpu_stat.rs` owns the machine `/proc/stat` window: aggregate and
    per-core busy/kernel shares plus `window_total_ticks`, the CPU capacity
    those two ticks covered. `linux/process_cpu.rs` is the per-process
    accountant over that SAME window — `/proc/<pid>/stat` fields 14+15 against
    `window_total_ticks`, a share of total machine capacity in [0, 100] (the
    Windows contract), cumulative `cpu_time_s` since process start, a
    boot-relative start-tick guard against PID reuse, skipped-reading
    detection, regressing-counter rejection and a never-exceeds-machine clamp.
    Anything unmeasurable is `None`, never 0 % — including the first window of
    a process. One `/proc/<pid>/stat` read feeds both the identity bracket and
    the accountant. Linux/macOS backends exist and are built by default
    (`build.py`). The Linux
    window observer uses a dedicated non-blocking `linux/x11_windows.rs`
    thread: EWMH client lists are merged, X-Resource supplies local client
    PIDs where available, machine-verified `_NET_WM_PID` is the fallback, and
    only fresh complete observations may prove a process windowless. Native
    Wayland has no portable whole-window inventory, so XWayland clients are
    the available X11 subset. `linux/desktop_session.rs` supplies the
    independent native-Wayland fallback: it prefers a useful unified cgroup v2
    path, falls back from root `0::/` to a useful `name=systemd` path, and
    classifies standard `app-*` or Snap-launcher user `app.slice` units as Apps
    only when a complete capped process environment has a non-whitespace
    `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY`. Arbitrary well-formed
    app.slice units require a validated per-process Flatpak/Snap/GIO/BAMF
    identity, so inherited display variables cannot promote generic services.
    Complete headless recognized app units are Background; unavailable,
    partial or oversized environments stay Unknown. Exact session scopes,
    user-manager `init.scope`, `background.slice` and `session.slice` are
    explicit Background roles; generic user-manager membership stays weak.
    `system.slice`/`init.scope` become System only with `/run/systemd/system`
    and PID 1 named `systemd`, so synthetic WSL cgroups stay Unknown.
    Flatpak/Snap/GIO/BAMF hints require the graphical gate and weak
    user-session evidence and never override explicit Background. The collector
    passes the same authoritative role marker (including Unknown) used by
    `ProcessEntry` into `ClassifyInput`; cache hits revalidate `/proc/<pid>/stat`,
    entries expire on a deterministic 4.5-5.5 s stagger, and dead PIDs are
    pruned. Exec/cgroup same-generation changes can trail for that TTL. Session
    and unit paths remain stable family keys. This is launcher metadata, not
    compositor focus or an exact window inventory.
    Linux privileged execution adds `linux/polkit.rs` (direct-first escalation,
    trusted `systemctl` execution through Polkit, and explicit fail-closed
    portable-install behavior),
    `linux/launch.rs` (process-group launch, trusted `pkexec` path checks),
    `linux/shell.rs` (`xdg-open` file/location/URL actions), plus the
    platform-neutral `linux_action_support.rs` parser/security contract and
    `command_line.rs` shell-free Run tokenizer.
    `linux/activation.rs` adds a per-user advisory lock and Unix-socket handoff
    checked before renderer creation; the primary listener wakes the UI to
    show/restore/focus, and a timed-out or hostile owner leaves the new launch
    independent. `linux/autostart.rs` writes/removes only its marked, quoted
    `$XDG_CONFIG_HOME/autostart` entry. `fontconfig_text.rs` is the shared
    root-level Linux Fontconfig process layer: cached `fc-match -v`, canonical
    trusted absolute executable and parent-directory validation, strict
    `antialias`/horizontal-`rgba`/`lcdfilter` parsing, and one bounded
    750 ms/64 KiB capture shared with `tm-ui` system-font selection.
- `crates/tm-app`
  - eframe GUI. `main.rs` (startup sequence: args → console only for CLI →
    early logging → hand a duplicate launch to the running instance → lazy
    engine factory; StartupTrace markers; the tray thread — icon, event
    handler and the native popup menu all live on a dedicated thread because
    `TrackPopupMenuEx` pumps a modal loop that must never re-enter the
    UI thread's egui/winit dispatch; the UI side is atomics +
    `PostThreadMessageW` — and the cloak-restore dance), `app.rs`
    (TaskManApp: engine starts AFTER first frame, event-driven repaints,
    action executor, toast ids, demand updates per tab; the global keyboard
    layer lives here — Ctrl+Tab/Ctrl+1..9 page switching via
    `page_switch_target`, the F1 shortcut overlay, `modal_open` gating and
    the search-commit `commit_search_selection`), `app_ui.rs`
    (chrome + dialogs incl. scrolling settings and Delete confirmation;
    the shared dialog keyboard contract lives in `app_ui/original.rs`:
    `consume_dialog_keys` / `dialog_key_decision` / `dialog_button_row` —
    consume BEFORE `Window::show`, and the SAFE button owns the default.
    The 2026-09-25 keyboard pass added the variants: tab-through dialogs
    (`consume_dialog_keys_tab_through`, Tab left to egui's focus system,
    Enter/Space consumed only while nothing is focused — Settings, Select
    columns, Process properties), `dialog_button_row_mirror` (the row's
    focus flag follows REAL egui focus, so the affinity checkboxes stay
    Tab-reachable), `capture_focus`/`restore_focus` (hand focus back to the
    invoking widget on close — for a dialog opened from a context menu, the
    menu's recorded owner, never the clicked entry; otherwise focus as of
    FRAME START, because the page is drawn disabled under a dialog and egui
    drops focus from disabled widgets. The hand-back is PARKED and
    `begin_frame_focus` (top of `TaskManApp::ui`) applies it once the
    target's layer may interact: egui judges that by the previous frame's
    modal layer and surrenders focus from blocked widgets, so an immediate
    request after a modal closed was always thrown away), the F1 help overlay, and hand-painted focus
    rings for the custom shell widgets),
     `search.rs` (list-navigation primitives shared by every page:
     `content_focus_id` — the ONE focus target a page's content region owns,
     with `set_active_content`/`content_has_focus` publishing which page is
     active; `nav_gate` — arrows/type-ahead only while THAT content region
     holds focus and no popup or dialog owns the input; `row_action_gate` —
     the same gate for the Enter/Space row bindings (page-level call sites
     additionally thread `TaskManApp::modal_open()`); `list_nav`
     reports Shift
     (range extension) and Ctrl (plain movement) instead of swallowing them,
     plus the 1 s accumulated type-ahead),
    `tabs/*` (processes/details/modules/users/services/startup/app_history/
    performance; Processes keeps native grouped presentation, Details can
    switch between flat and literal raw-PPID tree, offers optional combined/receive/send
    per-process network-rate columns backed by the same on-demand ETW source as Processes,
    and owns a bounded, identity-safe Process Properties inspector
    (General/Statistics/Threads/Modules/Network/Services/Security);
    its Windows Security page lazily queries PPL/protection, critical state,
    image machine and DEP/ASLR/CFG/CET/Win32k/code-integrity/image-load and
    related mitigation policies instead of adding them to the hot sampler.
    The list pages live in their own modules — `threads.rs` (thread list +
    the gap-aware activity `Tracker` and live TID/address/module/state filter),
    `modules.rs` (module list with guarded unload, origin-thread activity, and
    sampled executing-module evidence),
    `connections.rs`,
    `hosted_services.rs` — and query through `inspector::Poll`/`Pace` ONLY
    while their page is on screen, at `page_interval` (the update speed,
    floored per page; Paused = frozen) plus F5 inside the dialog; a failed
    refresh never blanks a shown list (`inspector::merge`). Threads and
    Modules keep each other's list current because each needs the other),
    `selection.rs` (multi-row process selection shared by Processes and
    Details: native click gestures, identity-keyed, primary vs. full set),
    `widgets/tablekit.rs` (TM-style
    tables: drag-start-width resize math, O(1) layout, `scrolled_rows`
    virtualization with identity-keyed scroll anchoring across model
    rebuilds, opt-in header drag-to-reorder via `reorderable`/`take_reorder`,
    and `numeric_indices` — the ONLY correct way to lay out the shared
    aggregate row, which tables of different column counts disagree on.
     Rows are click-sensing WITHOUT the focusable bit (`Sense::CLICK |
     hover`, chevrons likewise): the visible selection IS the keyboard
     focus, native-TM style — a focusable row hands every keypress to
     invisible spatial focus. Page keyboard layers use the one-shot
     owner-keyed `request_row_scroll`, `header_has_focus` (so page-level
     arrow handlers never double-fire with egui's spatial header movement),
     keyboard resize steps on a focused handle (8 px, Shift 32 px),
     Alt+Left/Right column reorder inside `reorderable`, and
     `body_viewport_h`/`page_rows` for the PgUp/PgDn span. Rows, cards and
     the table body itself build accesskit nodes, the non-focusable ones
     reparented onto the collection via `Context::register_accesskit_parent`),
    `widgets/menu.rs` (classic full-width Windows-style
    context menus: uniform 28 px gapless rows, painted check gutter,
    submenus; keyboard contract: Tab closes an open menu,
    ArrowRight/ArrowLeft open and close submenus, and the Menu/Application
    key triggers while any non-text widget holds focus — gate is
    `text_edit_focused`, not `egui_wants_keyboard_input`),
     `widgets/chart.rs` (timestamp-aware charts, kernel
     overlay, pixel-snapped frames and the cursor-anchored hover readout,
     plus `keyboard_sample`/`chart_accessible_value` — a keyboard cursor
     pinned to a sample TIMESTAMP so live telemetry cannot slide it, and the
     selected sample published as the chart's accesskit value),
    `icon_cache.rs` (lazy worker, per-egui-pass upload budget, true-LRU cap; single entry
    point `icon_or_default` — the row's executable icon, else the platform's
    `default_app_icon_rgba` when the row has no path or the OS has no icon for
    that image, else nothing while extraction is in flight; the UI draws no
    icon of its own, and `usable_icon_path` keeps blank/bare strings from ever
    becoming a cache key or a shell lookup; Linux accepts a bare command as an
    XDG lookup key),
    `action_executor.rs`. Theme and font setup moved to `crates/tm-ui`;
    `tm-app` re-exports them as `crate::theme` / `crate::fonts`.
- `crates/tm-ui`
  - Shared UI foundation used by the GUI and the Windows setup installer so
    both render the exact same Windows 11 look: `theme.rs` (dark/light
    palette, widget visuals, renderer capability interlock, text-weight and
    sub-pixel/filter tuning) and `fonts.rs` (async OS-native font load).
    egui-only; it never depends on eframe.
- `crates/tm-service`
  - Windows service executable. Starts under SCM as plain auto-start
    LocalSystem,
    raises only the control plane to above-normal priority, attaches protected
    ProgramData logging, reports `RUNNING` only after the broker is ready, and
    owns no telemetry/UI state. Non-Windows builds are an explicit stub.
- `crates/tm-installer`
  - The self-contained Windows setup installer (`taskman-setup.exe`) plus the
    build-time payload embedder (`taskman-payload`): self-extracting payload
    archive (`payload.rs`), CLI (`options.rs`), install/uninstall plan
    (`install.rs`), Win32 plumbing (`win.rs`), themed eframe wizard (`ui.rs`).
    Service install/uninstall runs through the app's elevated
    `--core-service` helper and is not reimplemented here — see
    `installer.md`.

## Important Support and Output Paths

- `target/` — build artifacts (gitignored, safe to delete)
- `dist/` — packaged releases from `build.py` (gitignored)
- `bench/` — framework comparison apps + timing scripts
- `tools/` — capture/UI-test PowerShell helpers
- `implement.md` — **REMOVED** (commit `fc4ecb7c`). The `implement.md §N`
  citations still in ~30 source comments and in `AGENTS.md` are historical
  provenance, not a live cross-reference: the sections they name no longer
  exist anywhere in the tree. Treat them as "this decision came from the
  original plan"; the current behavior is the code and, for accepted
  trade-offs, `known-debt.md`. Do not add new `§` citations.

## High-Risk / High-Value Files

- `crates/tm-core/src/engine.rs` — startup/lazy-start/notifier semantics;
  breaking these regresses the core architecture goals. While a lazy engine is
  parked before `Start`, configuration commands MUST be remembered rather than
  dropped: the UI ships its telemetry demand on its first frame (before the
  engine starts) and only re-sends on change, so a discarded `SetDemand`
  disables that provider for the whole session — this is exactly how
  per-process network stayed dark. See `log/recent.md` 2026-08-31.
- `crates/tm-platform/src/win/perfcounters.rs` — PDH group lifecycle +
  GPU instance parsing (unit-tested real-world strings), and
  `query_windows_memory_lists`: the hand-written
  `SystemMemoryListInformation` class + `MEMORY_LIST_INFORMATION` struct behind
  the memory composition bar. The layout is pinned by an `#[ignore]` live
  kernel test (`memory_list_layout_decodes_plausible_page_counts`) — run it
  after any Windows build that changes the page lists.
- `crates/tm-platform/src/win/sampler.rs` — `sample_inner` is the whole
  snapshot for one tick. `is_zombie` is the eviction rule split out of it and
  unit-tested: a process the kernel table lists with running threads is
  alive without any probe; only an unlisted one reaches the `terminated` /
  `live_in_kernel_table` CLOSURES, because `is_process_terminated` opens up to
  three handles. Per-process thread counts come from the `cpu_load` kernel
  table (a Toolhelp thread snapshot cost ~30 ms per tick), and the service
  catalog caches each service's configuration for 60 s (the SCM round trips
  cost ~36 ms per tick). `collect_gpus` / `collect_disks` / `collect_networks`
  are the self-contained tail phases; `apply_process_gpu` folds the per-process
  GPU view in. The process loop itself stays inline: every invariant it
  carries is commented where it is enforced.
- `crates/tm-platform/src/win/etw.rs` — `SESSION_NAMES` is exhaustive by
  design and `stop_live_sessions` is registered as the panic teardown the
  first time a session starts. An ETW session is a KERNEL object that outlives
  its host: `Drop` covers ordinary owned handles and the GUI/service shutdown
  paths explicitly call teardown for process-static trace owners;
  `panic = "abort"` and a kill run no destructor, so an orphan remains until
  the next run reclaims it by name or the user reboots. Only sessions THIS
  process started are stopped — a crashing GUI must not stop the still-healthy
  service's. `net_etw`/`disk_etw`/`file_etw`/`profile_etw`/`module_etw` pin
  their names against that list.
- The GUI's `--selfcheck-code-samples` diagnostic runs the installed-path
  client through the real broker, workload, system-profile provider, and
  page-to-module attribution. It is the end-to-end coverage that a unit test
  or a dev-tree client cannot provide because the broker rejects foreign
  images.
- `crates/tm-platform/src/win/cpu_load.rs` — also the source of per-process
  I/O operation counts and hard page faults (the same kernel table already
  carries `IO_COUNTERS` and `HardFaultCount`, so they cost no extra query), and
  the ONLY source of process
  identity and priority for the ~half of the process list that refuses
  `OpenProcess`: `start_epoch_of` and `base_priority` read the retained raw
  kernel table (never `LoadSample`, which needs two ticks to exist).
  `current_creation_epoch` performs a fresh handle-free table query for the
  protected-module fallback and deliberately does not reuse first-tick
  `sample()`, which has no delta window and returns `None`.
  sysinfo's `start_time()` is 0 for those processes and must never be stored
  as `Some(0)` — no handle check can ever match it. Hand-rolled NT structure
  offsets; verify against Process Hacker definitions before "fixing" — but
  note the live-kernel test proved `ImageName.Buffer` is an ABSOLUTE
  pointer into the output buffer on this build (PH's record-relative
  convention decoded 0/285 names); `parse_image_name` tries all
  conventions, validated, and the live test pins it.
- `crates/tm-app/src/widgets/tablekit.rs` — the blue value band is OPAQUE and
  is painted after the row fill, so `row()` records its selection/hover color
  in `row_overlay` and `heat_cells` re-applies it; dropping that makes hover
  stop dead at the first value column. `HeatCell::text` is a `Cow`, so a page
  whose model already formatted the cell passes the borrowed `&str` and a
  repaint allocates nothing. `row_icon` answers a row's icon from a per-frame
  memo keyed by the row's image path (a linear VECTOR, not a map: a table shows
  tens of DISTINCT images per frame however many rows it has, and the hit path
  must not build or copy a key): one cache lookup per distinct image per
  repaint instead of one per visible row plus a second default-icon lookup for
  every path-less row. The table is built per frame, which is what makes the
  memo sound — extraction is asynchronous and only turns a pending image into
  a ready one BETWEEN frames — and what stops a cached "no icon yet" from
  outliving a frame. `text_cell_unmeasured` paints the identical cell without
  the clipped-text layout, for callers that only consume the answer while the
  pointer is inside that cell (Details); `icon_memo_counts` and
  `text_measure_count` feed `TASKMAN_INPUT_STATS=1`. Row height is per-table
  (`row_h`, `ROW_H_DENSE`) and `scrolled_rows` must virtualize on it, never on `ROW_H`.
  `scrolled_rows` also anchors the vertical offset to the identities of the
  previously visible rows (`ScrollAnchor` + `stable_key`) on model-rebuild
  frames, so insertions/removals around the viewport cannot shift the content.
  It follows the MAJORITY displacement of those rows, never a single row's:
  re-ranking under a live sort key (CPU load) must leave the viewport alone,
  and a viewport parked at the top stays there. `prefer_key` only overrides
  that shared shift when it would push the selection out of view;
  `model_changed` must only be set when the caller actually rebuilt the model.
  Resize math MUST accumulate
  each frame's `drag_delta()` onto the LIVE width; `drag_delta()` is
  per-frame movement (NOT cumulative) in egui 0.36, so frozen drag-start
  widths pin boundaries at their start position. Regression-tested with
  real pointer events (`dragging_name_boundary_tracks_cursor_across_frames`).
  Resize handles are registered after ALL header cells so they win hit
  testing across their full ±6 px; double-click restore is detected via
  input state because drag-only widgets never receive click flags.
  `TmTable::row` takes a row-OWNER key (pid+start epoch, base address, …):
  the response id is keyed by it because egui binds an open context-menu
  popup to the response id, and an index-derived id hands the open menu to
  whichever row next occupies the slot in a re-sorting list.
- `crates/tm-app/src/app.rs` — `TaskManApp.history` (Performance-chart data)
  MUST stay a contiguous, append-ordered `Vec<HistoryPoint>`; it was a
  `VecDeque` whose ring wrap once froze all Performance charts (see
  `log/recent.md` 2026-08-27). `performance::window` slices it directly.
- Row naming is deliberately different per page and is NOT a candidate for
  unification: `details::detail_name` is the image name alone (the diagnostic
  page; the description is an optional column there), while
  `processes::process_display_name` is `Description (image.exe)` (the app
  list). Process counts on Processes use SQUARE brackets — `Microsoft Edge
  (msedge.exe) [24]`, `Apps [7]` — because round ones mean an image name.
- `crates/tm-app/src/tabs/value_columns.rs` — the numeric column catalogue
  (CPU/Memory/Disk I/O/Network/Disk active time/GPU) shared by the Processes and
  Users tables: ids and widths, the logical↔display order translation for
  dragged columns and its persistence, one-cell formatting, and the
  explanation each optional column shows when it reads "—". Rows everywhere
  index values in LOGICAL order; display order exists only at paint time.
- `crates/tm-app/src/tabs/processes.rs` — Processes UI intentionally uses a
  presentation-only app tree for Apps (Explorer/common shells, shell-session
  brokers and browsers are launch boundaries; a windowed process folds into
  a windowless ancestor only with positive ownership evidence — the same
  image, or a helper-like image with a matching publisher; missing publisher
  metadata never merges different visible executables. Common app/game
  launchers keep their helper UI but launched titles become peer App rows.
  Background/Windows groups are flat lists where a process's own application
  collapses into an expandable `Name (N)` row and busy absorbed external tasks
  (≥ 1 % CPU, different image, windowless) are promoted into Background.
  `joins_family` lets the same image join unconditionally; different images
  require same-publisher + helper-like evidence while windowless, idle and away
  from a system/launch boundary. Repeat runs of one image under one
  parent group separately (`sibling_run_key`), including `svchost.exe` under `services.exe`,
  which collapse into one `Service Host [N]` row whose expanded children are
  named `Service Host: <hosted service>` (tooltip lists them all; no
  per-service row spam) and whose Process Properties show the full list.
  Windows-process membership is NOT inherited from Session 0 or system ancestry:
  it requires a core OS image name or `IsProcessCritical`, so third-party SCM
  services and Microsoft background machinery (WMI Provider Host, SmartScreen,
  shell brokers, windowless user tools) remain Background. Positive native
  desktop roles are carried by `ProcessEntry::app_group`; when any real window
  exists, the page seeds App roots from both windows and those roles, keeps
  same-group members together even without PPID edges, and never merges two
  unrelated scopes merely because both use `app.slice`. Core-looking names are
  tree boundaries only: they must not re-promote a legacy Background process
  (for example Windows `migration.exe`/`kworker.exe`) or an authoritative
  Linux Background/Unknown process.
  Expandable application/family rows are virtual presentation parents: the
  virtual row owns the aggregate, while expansion reveals every concrete member
  (including the former head/root) as a child with only its own metrics. Flat
  Background/Windows family aggregates use `family_cells` (returns
  `CellValues`: `display` plus `known` per LOGICAL column) — the MEMBERS' own
  values, not the subtree's, because a
  foreign descendant left outside the group is rendered as its own row and must
  not be counted twice. Do not collapse this back to a literal PPID tree.
  `State::cache` (`Cache`) now holds what the repaint path used to re-derive
  every frame: the display rows and the selectable-row index (`selectable`,
  display indices in display order, which selection movement, its Shift range
  and the scroll request all walk). It is invalidated through `view_generation`
  rather than through the `expanded`/`group_collapsed` sets — the index is only
  correct for the expansion state that built it, so every mutation must bump
  that generation (`toggle_expanded`, `toggle_group`, and Expand/Collapse all).
  `RowData::cells` carries the per-row numeric cell TEXT, formatted with the
  model by `normalize_heat`, which already finalizes the per-column heat; a
  repaint paints it instead of re-formatting every visible cell. An unmeasured
  cell is `None` in `CellValues::key`, so it ranks strictly below a measured
  zero in both sort directions rather than tying with it. Raw `ProcessEntry.ppid` remains OS truth and is exposed by the
  persisted System Informer-style tree on the Details page, which is expanded
  by DEFAULT (`details::State.collapsed` tracks the exceptions) so newly
  appearing subtrees are never silently hidden. That tree has THREE sort
  states (`details::SortOrder`), not two, and the tree is not a mode: it IS
  the Name column's third state. Clicking any OTHER column must leave it and
  sort purely by that column.
- `crates/tm-ui/src/theme.rs` — `ScrollStyle.fade.strength` must stay 0:
  egui's edge fades read as a shadow lying on a dense table rather than as
  depth. `ScrollStyle.floating` must stay TRUE. egui
  decides whether a bar is needed against the OUTER rect for floating bars
  and against the shrunken INNER rect for solid ones, so a solid bar can
  oscillate on width-dependent content. Space reservation comes from
  `floating_allocated_width`, not from turning `floating` off; `tablekit`'s
  header must mirror the body's reservation (`prev_bar_use`). The sub-pixel
  interlock must stay tied to the actual renderer/device capability; the
  environment override may force an otherwise valid mode but may never bypass
  that capability.
- `crates/tm-platform/src/win/core_service.rs` — privileged trust boundary.
  Do not add arbitrary commands, output paths, or unbounded/generic telemetry to its protocol.
  Preserve exact PID+creation-time checks, client/server executable binding,
  bounded framing/queues, protected ACLs, reparse rejection, and pinned-copy
  upgrade semantics. See `core-service.md`.
- `crates/tm-platform/src/win/process_ops.rs` module unload — remote
  `FreeLibrary` is intentionally guarded by exact process creation timestamp,
  exact module base/path re-enumeration, same-architecture checks, and
  self/critical-process refusals. WHICH module to unload is the user's call
  (product decision 2026-09-06): there is no protected-module gate. One
  FreeLibrary drops ONE loader reference, so the unload REPEATS the remote
  call (bounded by `MAX_FREE_LIBRARY_CALLS`, identity + base/path
  revalidated per attempt) until the module leaves; the honest
  `ModuleUnloadOutcome { still_mapped, released }` crosses the broker as
  `BrokerValue::ModuleUnload` — never as an error string. Keep it off the
  sampler and UI thread, and never weaken the second confirmation in
  `tabs/modules.rs`.
- `crates/tm-installer/src/win.rs` — `install_dir()` must stay identical to
  `core_service.rs`'s protected install path, and install/uninstall must keep
  running service registration through the elevated `--core-service` helper
  rather than reimplementing it. See `installer.md`.

## Test Matrix

- `cargo test -p tm-core` — model/settings/history/engine/classify units.
- `cargo test -p tm-platform` — pure logic (GPU parsing, merges) + Windows
  integration tests (spawn/kill children, live sample sanity).
- `cargo test -p tm-app` — table/process/performance/detail logic tests.
- `cargo test -p tm-ui` — palette/gradient invariants.
- `cargo test -p tm-installer` — payload archive round trip + corruption and
  traversal rejection, setup CLI parsing, install step plans.
- `cargo test -p tm-service` — service entry/build surface (broker unit tests
  live in `tm-platform`).
- Headless smoke: `taskman --selfcheck [--mock]`.
- Service headless smoke: `taskman-service --selfcheck` (does not install or
  start the service).

## Rendering

`vendor/egui/` is a vendored fork of egui (subtree, tag 0.36.1). It exists for a
few things that stock egui cannot do: sub-pixel (ClearType-like) text on both
the native CPU renderer and capability-gated D3D12/Vulkan WGPU, a native CPU
renderer, the keyboard Menu/Application key (`Key::ContextMenu`, fork
divergence #5), and genuinely modal `Window`s plus public accesskit
reparenting (divergence #6 — the keyboard/screen-reader model depends on both).
The smaller later divergences are the text-cursor blink restart (#7), text-selection
preservation on secondary click (#8), no raw device-input registration (#9, pinned by
a Windows test in `eframe`'s `native/run.rs`), wheel coalescing (#14), the
dual-source LCD font pipeline plus transformed-text grayscale fallback (#15),
releasing resident GPU state while hidden (#16), and the wheel smoothing burst
(#17).
Linux capability parameters come from `tm-platform/src/fontconfig_text.rs`, which
caches a trusted absolute `fc-match` query, maps `antialias`/`rgba`/`lcdfilter`,
and bounds/reaps the process. LCD glyph texels use `(cov_r,cov_g,cov_b,0)`; WGPU
selects its second pipeline only for font-atlas meshes with LCD content and
non-white UVs. Windows `NativeApp` refreshes the monitor policy from a
placement/monitor/DPI key and fails closed across conflicting monitors; Linux
remains one process-wide fontconfig policy. `egui-wgpu` has a normal Naga
parse/validation test, and hardware rendering tests are ignored/manual rather
than portable CI.
`vendor/egui/TASKMAN-FORK.md` is the divergence inventory and rebase runbook;
`llm-wiki/render-pipeline.md` is the design. The fork has its own quality gate,
`tools/check-fork.ps1`, because `cargo clippy --workspace` does not reach an excluded
workspace; it also lints and unit-tests `eframe` with the shipped software feature
set (`accesskit,default_fonts,software,wayland,x11`, never its defaults + `software`),
which no workspace clippy or test run reached before. The script is host-only, so the
Linux-only present surface still depends on `build.py`'s target cross-check to be
compiled at all.

`vendor/winit/` is a patch of winit 0.30.13: `TASKMAN_WINDOW_BAND` makes window
creation use `CreateWindowInBand` so an always-on-top TaskMan window can be
created in native Task Manager's band 16, above the Start menu; other patches
reset failed event-loop creation, harden Windows painting/capture/DPI paths, and
expose xdg-shell `SUSPENDED` as Wayland's minimized/hidden signal.
`vendor/winit/TASKMAN-FORK.md` documents the changes and rebase steps;
`tools/check-fork.ps1` gates it too (clippy + tests, no fmt — upstream's tree
is not formatted with this nightly rustfmt).
