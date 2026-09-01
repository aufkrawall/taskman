# Recent Activity

## 2026-09-02 — the Task Manager hotkey has to work when nothing else does

Audit of the Ctrl+Shift+Esc path (IFEO `Debugger` on taskmgr.exe → our exe).
Six ways a press could produce nothing, and what each needed:

### A minimized instance stayed minimized

`restore_window` sent `Visible(true)` + `Focus`. winit's `focus_window` is a
documented no-op while `is_minimized`, and `ShowWindow(SW_SHOW)` does not
un-minimize either, so a hotkey press on a minimized task manager did exactly
nothing. The restore now sends `Minimized(false)` between the two. The cloak
that hides the first unpainted frame is now applied only when the window was
genuinely hidden to the tray — a minimized window still holds its last frame,
so cloaking it only added a blink.

### The window came back behind everything

The restore ran in the OLD process, which has no foreground right; Windows
answers `SetForegroundWindow` from a background process with a taskbar flash.
The right belongs to the process the shell just launched, so the LAUNCH now
calls `AllowSetForegroundWindow(primary_pid)` before it signals, and the
instance it wakes can take the foreground.

### A wedged instance swallowed every press

The second launch set the show event and exited unconditionally. If the
running instance was hung — the exact situation one reaches for a task
manager in — the press produced nothing at all, forever. There is now an
acknowledgement event, set by the UI thread that actually processed the
restore. No acknowledgement inside the deadline means the launch opens its
own window instead of exiting. `Local\TaskMan.Primary.v2` publishes the
primary's pid and HWND so the launch can tell the cases apart: a dead pid
gives up immediately, a window that times out on `WM_NULL`
(`SMTO_ABORTIFHUNG`, the shell's own "not responding" question) gives up
after ~2 s, anything else keeps its request for the extended deadline. A
denied `SendMessageTimeout` is inconclusive, not wedged: UIPI refuses the
message when an unelevated launch probes an elevated instance, and only
`ERROR_TIMEOUT` may cost an instance its request.

Verified live: a second launch defers in ~96 ms; with the first instance
suspended (`NtSuspendProcess`), the second one has its own window in ~2.1 s.

### An elevated instance could not be reached at all

The coordination objects were created with default security. An instance
started elevated labels them high integrity, so the medium-integrity launch
the shell starts on Ctrl+Shift+Esc was denied every access and silently
started a SECOND task manager. All four objects now carry `S:(ML;;NW;;;LW)`
plus a DACL for this user, Administrators and SYSTEM. The names moved to
`.v2`: an old instance would take the show event without ever acknowledging
it, and mixed generations only exist across an upgrade.

### Raising an open window asked for consent

With "always start elevated" on, every press re-execed through UAC before it
discovered that the window it wanted was already open. The handoff now runs
FIRST in `run_gui` — before locale, settings and the elevation policy — so
showing an existing window costs nothing.

### Registrations that cannot be launched

Windows runs the `Debugger` command INSTEAD of taskmgr.exe; if that command
cannot start, the hotkey opens nothing at all — not even the built-in Task
Manager. `set_direct_for_exe` therefore refuses to register a path that is
not a file, cannot be quoted, or is itself named taskmgr.exe (which would
re-enter its own registration forever). A registration that has since gone
dangling is detected at startup and repaired wherever that needs no consent
prompt (elevated session, or the core service broker, which re-points at the
protected GUI); otherwise the settings dialog says the hotkey currently opens
nothing and offers a one-click Repair — the old advice was to toggle the
checkbox off and on, which costs two UAC prompts.

Single-instance coordination moved out of `tm-app/src/main.rs` into
`tm-platform/src/win/instance.rs`; the app now only supplies the wake
callback, publishes its HWND, and acknowledges. `build.py --check` passed.

## 2026-09-01 — process owners, multi-select, GPU engines, window chrome

Ten reported gaps, fixed together. The ones with a root cause worth keeping:

### The User column was empty for most of the process list

sysinfo resolves a process owner through its own token query, which asks for
more access than an identity read needs and fails for roughly half the list —
all of session 0 unelevated. The old fallback ("session id 0 → SYSTEM") needed
`session_id_of`, which needs `OpenProcess`, so it did not fire for exactly the
processes that had triggered it.

`process_ops::token_user` opens the token through
`PROCESS_QUERY_LIMITED_INFORMATION` (granted for almost every process,
protected ones included) and resolves `TokenUser` → `LookupAccountSidW`.
Answers are memoized per SID string for the life of the process: a
domain-joined machine runs hundreds of processes under a handful of accounts,
and `LookupAccountSidW` can reach a domain controller. The resolved name is
also carried forward across the attribute cache's TTL refresh — a process's
owner cannot change while it lives.

The kernel process table now also yields `session_id` (offset 100 on 64-bit,
80 on 32-bit, pinned against the `windows` crate's struct), so the session-0
fallback works even for the processes no handle opens at all. The domain is
dropped for the NT AUTHORITY pseudo-domain **in every locale spelling** —
matching only the English one left "NT-AUTORITÄT\SYSTEM" in a 120 px column
on a German system.

### Dumps were not debuggable

`MiniDumpWriteDump` was called with `MINIDUMP_TYPE(0)` — `MiniDumpNormal`:
stacks and module headers, no memory. WinDbg opens it and then answers almost
every question with "memory access error". Worse, without
`IgnoreInaccessibleMemory` a single unreadable region (guard page, driver-locked
memory) fails the whole write.

`DUMP_TYPE_FULL` is now the `procdump -ma` set minus token information (SIDs
and privileges are not needed to debug, and a dump is user data). A target that
refuses its whole address space is retried once at `DUMP_TYPE_REDUCED` — after
rewinding the file, because `MiniDumpWriteDump` writes from the current
position and the failed attempt left bytes behind.

### Grouping rejected a family for one foreign child

`same_image_family` returned `None` the moment any descendant had a different
image, so a browser with twenty renderers and one `crashpad_handler` rendered
as twenty-one flat rows. `application_family` keeps the members that belong and
leaves the rest where they are — which is why the group aggregate had to become
the members' own values (`family_values`) rather than the subtree's: a
descendant shown as its own row must not also be counted inside the group.

Two rules group now, and the difference in strength between them is the design:
same image joins unconditionally (that is what keeps a 40 % renderer inside its
browser row), same PUBLISHER under a different image joins only while windowless
and idle and never across a system or launch boundary. Plus repeat runs of one
image under one parent (`sibling_run_key`), which the family walk cannot see
because the only process connecting them has a different image. `svchost.exe` is
excluded from run grouping on purpose — see `known-debt.md`.

### Dragging by the strip below the caption

`StartDrag` was sent from egui's `drag_started()`, which only fires once the
pointer has passed egui's drag threshold. Every pixel before that was movement
the window did not follow, so it jumped to catch up. It goes on the button
press now, like the real caption. Double-click-to-maximize had to be detected
by hand (time + distance since the last press in the region): handing the press
to the window manager ends egui's view of it, so its own click bookkeeping
never completes.

### The white flash restoring from the tray

`software_integration.rs` skips painting entirely while the window is hidden
(upstream documents an invisible window burning a whole core, emilk/egui#7776,
and forced repaints into one grow the stack until the process dies). A hidden
window also receives no `WM_PAINT`, so there is no way to have a frame ready
before it appears: `ShowWindow` composes the empty window and the app's first
frame lands a beat later.

Restores go through `NativeApp::restore_window` now, which sets `DWMWA_CLOAK`
BEFORE `Visible(true)`. A cloaked window is "visible" to Windows —
`window.is_visible()` returns true, so painting resumes — but DWM does not
display it. `finish_restore` uncloaks two frames later.

Two frames, and the countdown runs at the START of a frame, not the end: the
`Visible(true)` viewport command is applied AFTER the frame that issued it, so
the first frame that can present into a visible window is the next one, and
the one after that is safe to reveal. Counting at the end of a frame reaches
zero before anything has been presented, which is the flash again. It also
runs unconditionally every frame rather than from a one-shot callback, so no
path can leave the window invisible.

Cloaking is deliberately NOT the tray-hide mechanism: a cloaked window keeps
its taskbar button.

### The rest

- **Search** matched name/display/publisher/PID only. It now also covers
  description, owning user, service name, image path and command line,
  cheapest field first so the kilobyte-long command line is only scanned when
  nothing else matched. Startup and Services route through the same `Query`.
- **Multi-select** (`selection.rs`) backs both process tables with the native
  list-view gesture set. Rows are held as identities, never indexes — the list
  re-sorts on every tick. Fan-out is limited to the repeatable commands; the
  single-target ones follow a separate `primary`. More than one target always
  goes through the confirmation, which lists every one by name and pid.
- **GPU engines**: `HistoryPoint.gpu_engines` plus a "change graph to" menu
  (Overall / All engines / each engine). The menu's engine list comes from the
  history window, not the snapshot — the snapshot's list is sorted by
  utilization, so an engine that just went idle would drop out exactly when the
  user wanted to look at what it had been doing. The collector no longer
  truncates to the six busiest for the same reason.
- **Window chrome** (`win/window_chrome.rs`): caption/text/border colours plus
  `IMMERSIVE_DARK_MODE`, pushed only when the theme changes (each attribute
  recomposes the frame). What is NOT reachable, and why, is in `known-debt.md`.
- **Search box** gained a clear button; Escape clears it too.
- **Row names split by page.** Details is the diagnostic page and shows the
  IMAGE NAME only (`svchost.exe`); the description is available there as its
  own optional column. Processes is the app list and shows
  `Microsoft Edge (msedge.exe)` — description first, image name behind it,
  bracket dropped when it would only repeat the name. `ColumnId::Name` on
  Details sorts by image name, matching what it renders.
- **Process counts moved to square brackets**: `Microsoft Edge (msedge.exe)
  [24]`, and the group headers with them (`Apps [7]`). Round brackets now mean
  an image name on that page, so a count in round brackets would read as one.
- **Scroll fades off.** egui paints a 20 px background-coloured ramp at the
  top and bottom of every scroll area (`ScrollFadeStyle`, strength 0.5). Over
  a dense table it reads as a shadow lying on the list — a second, moving edge
  beside the header and the window frame. `strength: 0.0` in `theme.rs`.
- `ROW_H_DENSE` 22 → 20 px. That is the floor: 13 px row text needs a ~17 px
  line box and `icon_cell` derives its glyph side from `row_h - 6`.

## 2026-08-31 — tray polish, and the fabricated start time behind "no priority"

### Half the process list had no identity

Reported as "the priority tick is missing for some processes". It was 130 of
261, and the priority was only the visible half of it.

sysinfo reads a process's creation time through a process HANDLE and returns
**0** when it cannot open one — which unelevated is every svchost, every
protected service, System, Registry, Secure System. `sampler.rs` stored that
verbatim as `start_epoch_s = Some(0)`: a fabricated identity that no
`creation_epoch_from_handle` check can ever match, quietly poisoning every
PID-reuse guard for those processes. `GetPriorityClass` failed for exactly the
same set, leaving `PriorityClass::Unknown`.

Both now come from the kernel process table, which reports a creation time AND
a base priority for every process without opening anything:

- `cpu_load::start_epoch_of` fills a missing start time (and leaves it `None`
  when even the kernel table has none — never `Some(0)`).
- `cpu_load::base_priority` + `process_ops::priority_class_from_base` resolve
  the class from the base priority (4/6/8/10/13/24 → Idle…Realtime, compared
  as RANGES because a foreground normal process commonly reads 9).
  `GetPriorityClass` still wins where a handle opens; this is the fallback.

Both accessors read the retained raw table, NOT `LoadSample`: a load sample
needs two ticks to exist, and the first snapshot is the one the UI shows.
Result: unknown priorities went 130 → 1 (pid 0, which the kernel table
excludes by design). `hand_written_offsets_match_the_declared_layout` pins the
`BasePriority` offset against the `windows` crate's own struct, and
`unopenable_processes_still_report_an_identity_and_a_priority` pins the
end-to-end behavior including "no process may carry `Some(0)`".

### The Details tree is now a sort state, not a mode

The flat/tree switch is gone from settings and from the overflow menu's View
submenu. The tree IS `SortOrder::Hierarchical`, reachable only from the Name
column: click it a third time. Clicking any other column lands on a plain
direction, which is what leaves the tree — the user asked for an ordering by
THAT column, and a tree would keep children pinned under their parents. The
overflow menu keeps one "Process tree (Name column)" tick as the discoverable
way in and out.

`details_tree_view` (and the older `process_tree_view`) migrate into
`details_tree_hierarchical`, and the restore path runs even when no `[sort]`
entry exists — a migrated config has exactly that shape.

### Tray icon

- A single left click restores the window; double click still works.
- The notification-area menu follows the app's effective light/dark theme.
  There is no supported API: Windows themes popup menus from a process-wide
  preference set by two unnamed uxtheme exports (ordinal 135
  `SetPreferredAppMode`, 136 `FlushMenuThemes`). muda's `MenuTheme` is not a
  substitute — it documents itself as affecting a window's menu BAR only.
  Both lookups fail soft.

## 2026-08-31 — menus, scroll bars, stale process attributes, hierarchical tree

Four user-reported UI defects, three of which had a shared shape: the fix was
never in the tab that showed the symptom.

### Context menus were not menus

egui's `menu_style` sets `button_padding = (2, 0)` and then lets the global
`item_spacing.y` (6 px) separate the entries. The result was a column of
text-height labels with dead gaps between them — the gaps neither highlight
nor activate the entry under the cursor.

`widgets/menu.rs` now paints every entry itself: one uniform 28 px full-width
row, `item_spacing.y = 0`, a left check gutter so ticked and unticked labels
share a left edge, and a right gutter for submenu arrows. `menu::context_menu`
installs the style on the popup, and submenus inherit it through
`MenuConfig`. Two details that are easy to get wrong:

- Use `allocate_at_least`, NOT `allocate_exact_size`. Menus lay out
  top-down-JUSTIFIED and the "exact" variant re-aligns the desired size back
  inside the justified frame, leaving each row only as wide as its own label.
- The tick is a hand-painted polyline. `✓` is not in every installed UI font
  and a missing glyph renders as a tofu BOX — which is what the user was
  seeing and reported as "the checkbox looks ugly". The same reasoning
  retires `controls::checkbox` from inside menus (it drew a real 18×18 box
  next to menu text).

`entries_are_uniform_full_width_and_gapless` pins height, width and the
absence of gaps.

### Priority changes took up to ten seconds to appear

`win/sampler.rs` caches slow-changing per-PID attributes (priority, EcoQoS,
UAC virtualization) behind `ATTR_REFRESH_TTL` (10 s). Nothing invalidated
that cache when WE changed one of those attributes, so the menu kept ticking
the old priority until the TTL expired.

Two halves, both needed:

- `process_ops::note_attrs_changed` / `take_changed_attrs` — a process-global
  invalidation list drained by the sampler at the top of every tick. The
  broker path bypasses `process_ops`, so `core_service.rs` records it too
  (`noting_attrs`).
- `TaskManApp::run_action_refreshing` — refresh AFTER the action completes.
  `toggle_efficiency_mode` used to call `engine.request_refresh()` beside the
  dispatch, which races the executor's worker thread and samples the
  still-unchanged process.

No optimistic UI echo, deliberately: Windows silently downgrades a Realtime
request to High without `SeIncreaseBasePriority`, so echoing the requested
class would tick a value the OS never applied.

### Scroll bars painted over content

`ScrollStyle.floating_allocated_width` was 0, so the bars covered the last
~14 px of every table row, Performance card and dialog. It is now 14
(`bar_width` + `bar_outer_margin`), which moves the bar just outside the
content rect while keeping the thin-idle/expand-on-hover look.

`floating` stays TRUE on purpose. egui measures "is the content too large"
against the OUTER rect for floating bars and against the shrunken INNER rect
for solid ones; with solid bars, content whose height depends on its width
can flip the bar on and off every frame.

Consequence for `tablekit`: header and body are two independent scroll areas
sharing one horizontal offset. The body now reserves a bar lane and the
header (bars hidden) does not, so the header is fed the body's previous-frame
reservation via `prev_bar_use`/`store_bar_use`. Without it the two clamp the
shared offset at different maxima. `BODY_PAD_RIGHT`/`BODY_PAD_BOTTOM` dropped
to 6/4 — they are breathing room now, not bar clearance.

### The Details tree was a mix of hierarchy and alphabet

The tree sorted siblings by the active column, so it was never a literal
hierarchy. `details::SortOrder` adds a third state: clicking the sorted
column a third time (tree only) reaches `Hierarchical`, which ignores the
sort column entirely and orders siblings by CREATION time — System
Informer's cycle. It is also reachable from View ▸ Strict hierarchy.

Creation order, not snapshot order: `sys.processes()` is a hash map, so
"unsorted" would reshuffle the whole tree every tick. `start_epoch_s` then
PID is stable and is the order the OS actually created the children in.

Persistence: the shared `[sort]` line format is `column,ascending` and has no
room for a third state, so the flag rides in `[general]` as
`details_tree_hierarchical` next to `details_tree_view`.

## 2026-08-31 — per-process network without elevating the GUI (broker v2)

The GUI needed administrator rights to show the Network column, which defeats
the point of having a LocalSystem broker. Protocol v2 adds
`ProcessNetworkCounters`: the service hosts the ETW session and answers a
read-only, bounded query, so an ordinary unelevated GUI gets real numbers.
This deliberately reverses the "no telemetry endpoint" invariant — the
reasoning is recorded in `core-service.md`.

Version bumped rather than adding a variant to v1 on purpose: an older service
then fails the HANDSHAKE, which the client can tell apart from a REJECTION and
therefore fall back on. Slipping the variant into v1 would have made
"unsupported" indistinguishable from "denied", and the no-fallback-on-rejection
rule depends on that distinction. Cost: a one-time service reinstall.

### The bug this uncovered: leaked ETW sessions

First end-to-end run showed 0 KB/s everywhere despite a live 50 MB download.
The service HAD started its trace (log confirmed) and the GUI's requests were
accepted, so the counters had to be arriving empty. `network_trace_raw_vs_pruned`
separated the two candidate causes and reported `raw entries=0, live pids=273`
— the trace was receiving no events at all, so pruning was innocent.

Root cause: the session name included the pid. `Drop` stops the session, but a
force-killed or crashed process never runs `Drop`, and the next start picked a
DIFFERENT name — so orphans accumulated instead of being reclaimed. Four had
piled up from processes killed during development, and once enough sessions
have one provider enabled Windows stops delivering events to them. The module
doc even claimed stale sessions were reclaimed by name; that was never true
with a pid in it.

Fix: fixed, role-scoped names (`TaskMan-Net-Service`, `TaskMan-Net-App`) so
each host reclaims its own orphan through the existing
`ERROR_ALREADY_EXISTS` → stop → restart path, and never the other's.
`session_names_are_fixed_per_role_and_never_contain_the_pid` pins it. Also
hardened `totals_pruned`: an EMPTY live-pid set now means "the caller could not
enumerate", not "nothing is alive" — pruning against it would have wiped every
counter and produced the same silent all-zero symptom.

Verified: after cleanup, `raw entries=9, busiest = pid 23092 recv 50,075,955`
(exactly the 50 MB download), and the unelevated installed GUI sorted by
Network shows `codex.exe 633,1 KB/s` at the top. One session exists, and
force-killing plus restarting the GUI no longer adds any.

## 2026-08-31 — engine dropped pre-start demand (why the Network column stayed empty)

User report: the Network column still showed "—" after the ETW work landed.
The ETW session, the sampler wiring and the display model were all correct;
the bug was one line in `engine::run_loop`.

Root cause: while a lazily spawned engine parks waiting for `Start`, the
command loop matched `Ok(_) => {}` and **silently discarded every other
command**. The UI computes its telemetry demand on its FIRST frame — which is
deliberately before the engine is started — and `update_demand` only re-sends
when the bitmask changes. So `PROCESS_NET` was requested exactly once, into
the void, and the collector never learned it was wanted. The ETW session was
therefore never started, and `net_*` stayed `None` forever.

This was latent before: every other demand bit is only added when leaving the
default start page, and a tab switch re-sends. `PROCESS_NET` is the first bit
set on the DEFAULT page, so it is the first one that could be lost for good.
`SetInterval` had the same hole (an interval set before start was ignored).

Fix: remember demand while parked and apply it right after the factory runs;
apply `SetInterval` immediately, since the sampling loop reads the interval
from shared state anyway. Pause/Resume/Refresh remain meaningless before
`Start`. Two regression tests
(`demand_sent_before_start_reaches_the_collector`,
`interval_sent_before_start_is_honored`) fail against the old code.

Diagnosis path worth remembering: an engine-level integration test that
reproduces the UI's exact call order (`spawn_lazy` → `set_demand` → `start`)
separated "platform layer broken" from "app layer broken" in one run — the
platform test passed 235/235 while the GUI showed nothing, which pointed
straight at the plumbing between them.

### Network column now reads in bytes

Native TM fixes this column to Mbit/s, where realistic per-process traffic (a
few KB/s) renders as "0,0" and the column looks broken. It now uses the same
1024-based KB/s / MB/s units and one decimal as the neighbouring Disk column
(`format::format_process_net_rate`), so a browser at 3 KB/s is visible and the
two rate columns read consistently. Deliberate deviation from TM parity.

## 2026-08-31 — per-process network via ETW

Implemented on request, replacing the honest-but-empty "—" column.
`win/net_etw.rs` runs a private real-time ETW session on
`Microsoft-Windows-Kernel-Network` and accumulates the `size` field of the
TCP/UDP data events per PID.

Details that matter:
- **The PID comes from the event PAYLOAD, not `EventHeader.ProcessId`.**
  Kernel network events fire in arbitrary (usually System) context, so the
  header PID is not the traffic owner. All eight data events — TCP/UDP ×
  v4/v6 × sent/received — begin with `PID: u32, size: u32`, and that prefix is
  the only thing parsed.
- Only the *data* event ids (10/26/42/58 sent, 11/27/43/59 received) are
  counted. Connect/disconnect/retransmit/ACK events are ignored, or
  retransmitted bytes would be billed twice.
- **Administrator rights are required** to start an ETW session. Without them
  the monitor stays inactive and every process keeps `None`, which renders as
  "—" — never a fabricated zero. Availability is all-or-nothing and pinned by
  `per_process_network_is_unknown_or_measured_never_fabricated`. A failed
  start is remembered so an unelevated session does not retry a
  permanently-denied API every tick.
- Demand-gated on `TelemetryDemand::PROCESS_NET` (the bit was already
  reserved): the session runs only while Processes or App History is on
  screen, and `Drop` stops it and joins the consumer thread.
- Rates are deltas of cumulative counters over the tick interval, keyed by
  `(pid, start_epoch_s)` so a recycled PID cannot inherit a dead process's
  totals; the map is pruned to live PIDs each tick to bound memory.

Verified elevated against live traffic — EpicGamesLauncher/python/claude all
attributed with sane recv/sent, and the two ends of a localhost pair showing
mirrored counters. Measured overhead with the session running: 4.0 % of one
core vs ~3 % without it. The Network cells now explain themselves on hover
when unavailable ("needs administrator rights") instead of showing a bare
dash.

## 2026-08-31 — text rendering limits, graphics mode, and two measured findings

### ClearType is not reachable from this side (measured, not assumed)

User report: "fonts don't seem to have coloured sub-pixels, they look blurry
and fat". Confirmed by measurement: across a text region of our own window the
maximum per-pixel RGB channel spread is **0** — pure grayscale coverage. The
same measurement on the reference Windows Task Manager capture
(`taskmanpngs/1.png`) shows orange/blue fringes, i.e. real sub-pixel AA.

Why it cannot be fixed here: epaint stores every glyph in a SINGLE-channel
coverage atlas and tints it in the shader, and the renderers blend with one
scalar alpha. Per-channel coverage would need a 3-channel atlas plus either
dual-source blending or per-channel colour write masks in BOTH the wgpu and
glow backends — a fork of epaint + egui-wgpu + egui_glow. Upstream tracks it
as emilk/egui#2639. Recorded in `known-debt.md`.

What DID improve, and why:
- **Segoe UI Variable** (`SegUIVar.ttf`, the real Win11 UI face) is now
  preferred over the Win10 static `segoeui.ttf`, pinned to `wght=400`,
  `opsz=10.5` through `FontTweak::coords`.
- **Horizontal grid-fitting.** epaint's default `SmoothHinting` sets
  `preserve_linear_metrics: true` AND `symmetric_rendering: true`, both of
  which switch x-direction grid-fitting OFF, so vertical stems straddle two
  grey columns. The `Sharp` profile turns both off. Measured on a stem:
  `237 252 187 92` → `220 255 220 114` (a fully-lit column appears).
- **Coverage ramp.** egui's dark-mode default `2c − c²` lifts every partially
  covered pixel (0.5 → 0.75) — that is the "fat" look. `Sharp` uses raw
  coverage; measured ink over a fixed text region drops 277 → 225 (−19 %).
- All three are one user setting, `text_smoothing = sharp|standard|smooth`
  (default `sharp`), applied live; `TASKMAN_TEXT_SMOOTHING` overrides it for
  A/B comparisons.

### Software rendering is not viable on this stack (measured)

`render_mode` replaces the short-lived `gpu_acceleration` bool with
`auto | compatibility | software`. Continuous-repaint measurements on this
machine (Ryzen 7 5700X, 16 logical CPUs, 2400x1350 window):

| mode | backend | cost |
| --- | --- | --- |
| auto | wgpu D3D12, GPU | **0.2–0.3 cores** |
| compatibility | glow/OpenGL, GPU | **1.0–1.1 cores** |
| software | wgpu D3D12, WARP | **14 cores at 2.9 fps** |

WARP's cost is FIXED per frame, not fill rate: identical at 500x320 and
2000x1200 (10.5 vs 14.0 cores), identical for a near-empty paused window and a
full process list, and unchanged by present mode or frame latency. Nothing
this app draws can bring it down. Even at the normal 1 Hz sampling tick,
software mode sits at ~10 cores. It is therefore shipped with an explicit
warning in the settings dialog, and "no GPU trouble" is served by
`compatibility` (OpenGL — a different driver stack, still real-time).

### Per-process network is still not implemented

Checked on request: no Windows collector ever writes `net_recv_bps` /
`net_sent_bps` / `net_recv_total` / `net_sent_total` (the only writers are in
an `app_history` unit test), so the Processes and App History network columns
render "—" for every row. That is the intended honest-unavailable behavior,
not a regression; the ETW work is listed in `known-debt.md`.

## 2026-08-31 — Processes/Details table parity: heat map, status glyphs, type-ahead, tree

Seven user-reported gaps against native Task Manager and System Informer,
plus the root cause behind two of them.

Root cause found (efficiency mode never showed): `GetProcessInformation`
with `ProcessPowerThrottling` treats `PROCESS_POWER_THROTTLING_STATE.Version`
as an INPUT field. `efficiency_mode_state` passed a zeroed struct, so the
call failed with `ERROR_INVALID_PARAMETER` (87) for EVERY pid and
`power_throttled` was `None` system-wide — the leaf could never appear.
Verified out-of-band with a P/Invoke probe before and after (Version 0: 87
for all 235 processes; Version 1: Brave/Edge renderers report
ControlMask/StateMask `EXECUTION_SPEED`). Regression test:
`efficiency_mode_state_is_known_for_own_process`. Efficiency mode also moved
to its own 2 s sub-TTL (`POWER_THROTTLE_REFRESH_TTL`) inside the 10 s
attribute cache: it is a live status column, and one
OpenProcess/GetProcessInformation pair per process is cheap.

UI changes:
- **Heat map** (`theme::heat_blue` + `TmTable::heat_cells`): every numeric
  cell is now filled from a continuous gradient whose FLOOR is `heat_base`;
  the old model painted a flat band only for rows with a non-zero value and
  a single binary top-consumer highlight, so idle processes had unpainted
  holes. Curve is ease-OUT (`sqrt`) — intensities are normalized against the
  column maximum, so the old ease-in curve collapsed everything but the top
  consumer onto the base tint. `heat_low`/`heat_top` are gone.
- **Hover reaches the value columns**: the heat band is opaque and is painted
  after the row fill, so hovering only lit the name area. `TmTable::row`
  records its selection/hover fill in `row_overlay`; `heat_cells` re-applies
  it over the band. Light mode gets a dark wash (`row_hover_fill`) — a white
  one over a light background was invisible.
- **Status column glyphs** (Processes): orange pause for suspended, green
  leaf for efficiency mode, words moved into the row tooltip; only
  "not responding" stays spelled out. The glyphs deliberately do NOT create
  their own hover widgets — that would steal the row's hover state and make
  the highlight flicker.
- **Group rows summarize efficiency mode** (`Subtree::efficiency`): a
  collapsed `Brave Browser (24)` row already aggregates CPU/memory, so it now
  aggregates the power state too. That is where native TM shows the leaf, and
  it is what the user actually reported missing.
- **Type-ahead** (`search::list_type_ahead`): typed characters accumulate
  into a word for 1 s, so "svc" lands on svchost.exe instead of jumping to
  whatever starts with "c". One letter — or the same letter repeated — still
  cycles. All text events in a frame are consumed in order (fast typing
  delivers several per frame); buffers are keyed per list.
- **Dense rows** (`ROW_H_DENSE = 22`, `TmTable::row_h`): Details, Services and
  Modules pack their rows like native TM's Details tab. Processes/Users/
  Startup/App history keep the airy 32 px app-list spacing. `scrolled_rows`
  virtualizes on the table's own height; page-up/down counts match.
- **Details tree is expanded by default**: `State.collapsed` replaces
  `State.expanded`, so the tree always shows the COMPLETE hierarchy —
  including subtrees whose parents started after the page was opened, which
  the one-shot `ensure_tree_initialized` left collapsed. Parent links whose
  parent started AFTER the child are rejected (`is_plausible_parent`),
  matching System Informer's PID-reuse guard; unknown timestamps never
  reject. Indent tightened to 18 px.

Verified visually against live captures (leaf on Brave/Claude/Codex, pause on
a deliberately suspended process, sampled cell fills monotone
`#2E6FC4 > #2962AB > #2960A9 > #1C3D68 > #162C4A`).

## 2026-08-31 — foreign-session UX: repair now hands over to the installed copy

User report: the state "somewhat randomly" showed ForeignClient. Root cause:
not a bug in the state machine — the installed generation and the dev-tree
build had diverged (the final fmt/gate rebuild changed the hash after the
install), so dev-tree launches stopped hash-matching the installed copy and
the startup redirect declined by design; the service log confirmed the
rejected client paths were all `target\release\taskman.exe`. Launching the
installed copy showed Active — hence the perceived randomness across
launches.

UX fixes: repair from a ForeignClient session now installs this build AND
hands the session over to the installed copy (`dispatch_core_service_repair_and_switch`,
reusing `switch_to_installed_gui`), closing the loop in one click; the
ForeignClient text explains the rebuild/portable scenario and names both
buttons. The installed generation was refreshed to the current release build.

## 2026-08-31 — response-delivery race: state display flapped Running/Degraded

User report: the Advanced state alternated between "Active" and "broker
authentication failed". `handle_client` called `DisconnectNamedPipe`
immediately after writing the response; npfs discards queued outbound bytes
on disconnect, so the client's read raced the teardown — measured 24/40
delivered (drop: 40/40, flush+disconnect: 40/40). Lost pings surfaced as the
generic degraded message while the SCM still reported Running. Became visible
only after the DACL/identity fixes let non-elevated pings reach the response
stage at all.

Fix: broker drops its handle instead of disconnecting, in `handle_client` and
`reject_client` alike; the client drains the response and releases the
instance when its end closes. Also empirically validated
`USER_PIPE_ACCESS`/npfs requirements with a self-made-pipe bisection
(see tools history): grants without FILE_READ_ATTRIBUTES deny pipe client
opens regardless of requested mask.

## 2026-08-31 — real root cause: pipe DACL missing FILE_READ_ATTRIBUTES

Follow-up to the ForeignClient fix: the installed (non-elevated) GUI still
reported "broker authentication failed". Empirical ACL bisection on self-made
pipes proved npfs requires `DesiredAccess | SYNCHRONIZE |
FILE_READ_ATTRIBUTES` for pipe client ends; the broker's user ACE granted
only `0x00100083`'s predecessor `0x00100003` (no attributes), so every
non-elevated client was denied at open and elevated ones only worked via the
Administrators generic-all ACE. Second bug: the client's server-identity
check opened the LocalSystem service process, which non-elevated tokens
cannot do, so non-elevated GUIs could never complete the handshake.

Fixes: user ACE + client request now share `USER_PIPE_ACCESS = 0x00100083`
(data + attributes + synchronize); `verify_pipe_server` falls back to the SCM
view (pipe server PID == `ServiceStatus.process_id`, configured image ==
protected path, quotes stripped because windows-service keeps them) when the
direct image check is denied; the switch action is allowed from elevated
sessions (user-initiated, elevation inherited intentionally); ForeignClient
state keeps a secondary Repair button so a newer dev build can still upgrade
the protected generation. Verified live: non-elevated open + identity + ping
delivery against the reinstalled service; live test
`live_service_identity_verifies_without_elevation` added.

## 2026-08-31 — ForeignClient state: honest GUI↔service connection reporting

User report: the GUI said the core service "does not work" and repair "fails".
Diagnosis against the live machine showed the service, manifest, and pipe DACL
all healthy; the service log proved the running GUI had been launched from
`target\release\taskman.exe`, which the broker's path-based client
authorization rejects by design. The install behind the Repair button had
actually succeeded (fresh manifest + service restart), but the foreign session
kept being rejected forever and the generic "broker authentication failed"
detail made a successful repair look broken.

Fix: `service_state` classifies a healthy service + non-installed client image
as `CoreServiceState::ForeignClient` (new enum variant, unit-tested helper
`foreign_client_session`); the Advanced section shows an explanatory text and
a "Switch to installed copy" button backed by
`core_service::relaunch_into_installed_gui` (spawns the installed GUI with
`--single-instance-handoff`, then programmatic-exits the foreign session;
elevated sessions refuse). i18n keys: `CoreServiceForeignClient`,
`CoreServiceSwitchRequested`, `SwitchToInstalledCoreService`.

## 2026-08-31 — protected core service, persistent Details controls, overload resilience

Implemented the requested split architecture: the ordinary GUI owns all
telemetry/UI/user-path work, while a new delayed-auto LocalSystem
`taskman-service.exe` owns only versioned allowlisted control requests.

- **Service boundary:** local/remote-rejecting first-instance named pipe;
  one-user protected DACL; kernel client/server PID plus protected image-path
  binding; 64 KiB framed request/response caps; unknown-field rejection; two
  workers, queue cap 16, and pipe-instance cap 19. A protected ProgramData
  manifest pins schema, protocol, SID, paths, and SHA-256 hashes. Explicit
  broker rejection—and any post-authentication transport failure with an
  unknown outcome—never falls back to the GUI token.
- **Action safety:** brokered process actions require a positive sampled
  creation time, reject system/service/requesting-GUI/critical targets, then
  repeat exact identity and critical checks on the handle used by the action.
  Tree kill refuses unidentified descendants. Module unload stays narrowly
  allowlisted and re-enumerates exact base/path; module inventory and dumps
  remain local. UAC virtualization is a similarly narrow token operation with
  current-state menu marker, allowed-state check, and warning dialog.
- **Filesystem/SCM:** Program Files binaries are protected System/Admin-full,
  Users-read/execute; ProgramData manifest/logs are System/Admin-only with
  protected Administrator ownership. Directory/file handles are pinned against
  conflicting mutation while owner/group/DACL are assigned. Log startup
  accepts only the exact daily service-log name shape and rejects
  reparse/nested/non-file/hard-linked entries,
  and retains at most 14 daily files. The elevated helper never opens the
  interactive user's file log or mutates per-user redirect state. Install
  rejects reparse points, pins source and existing-destination handles against
  mutation, rejects hard-linked installed binaries, hashes pinned content,
  uses synchronized/write-through staging + atomic move, and verifies the
  destination. Upgrade waits for the old service process. SCM is
  delayed-auto LocalSystem with service SID, only `SeDebugPrivilege`, and
  restart actions at 5/15/60 seconds.
- **GUI handoff:** the successful one-time UAC install writes a per-user marker
  only after the helper's SCM start request succeeds; SCM reports readiness
  independently once the pipe is listening. Matching package/portable launches
  redirect before any window to the protected GUI, with Windows-safe argument
  quoting; a differing package hash remains local for repair/upgrade. Existing
  autostart and owned Task Manager replacement entries are migrated. Uninstall
  removes SCM capability and the marker while leaving protected files.
- **Parity/state:** literal PPID tree is on Details (not Processes); current
  priority and UAC states are marked; priority/affinity can be persisted per
  executable; sort state persists across all process/list tables; Process
  status reports suspended/not-responding/efficiency; modules can be unloaded;
  Delete confirms termination; close-to-tray and HKCU autostart are optional.
- **Resilience/startup:** two independent bounded 32-job GUI action lanes,
  no renderer-thread fallback, explicit overload feedback, above-normal (not high/realtime) control-plane
  priority, single-instance restore signaling, bounded ownership handoff for
  explicit elevation, condition-variable-coordinated SCM listener wakeup, lazy
  tray, background affinity and advanced-settings probes. Autostart migration
  rewrites only commands proven to be TaskMan-owned; settings input is capped
  at 4 MiB before parsing. A broker-worker panic aborts service mode so SCM
  recovery restarts a clean process rather than retaining degraded capacity.
  Windows WGPU remains D3D12-only with Glow fallback.

Final headless verification passed format, Clippy with warnings denied, all
166 workspace tests (71 app, 50 core, 40 platform library, 5 Windows
integration), the host release/package build, and the service protocol
self-check. The ZIP contains the 13,873,664-byte GUI and 1,378,816-byte service.
Linux cross packaging was skipped because neither supported cross toolchain is
installed. No GUI, UAC helper, ACL mutation, or service install/start occurred.

The earlier 2026-08-28 kill-path note below is historical: the current code
uses exact same-second creation identity (no ±2-second tolerance) and refuses
tree descendants whose creation identity could not be captured (no unverified
fallback).

## 2026-08-31 — GUI parity, literal process tree, Modules inspector, D3D12 trim

Broad parity/ergonomics pass against current Windows 11 Task Manager plus
selected System Informer diagnostics. No GUI/capture executable was launched
during verification so the active desktop remained undisturbed.

- **Process interaction:** native grouped ownership stays on Processes, while
  Details offers a persisted literal PPID tree; hierarchy-preserving search
  and sibling sorting; expand/collapse controls;
  arrow/Home/End/Page navigation; Delete opens an identity-bound end-process
  confirmation. Processes/Details menus now consistently expose copy, online
  search, file location, properties, dumps, and modules where supported.
- **Modules:** new async on-demand name/path/base/size inspector. Unload is a
  confirmation-gated diagnostic action restricted to same-architecture
  third-party DLLs. ToolHelp enumeration handles transient `ERROR_BAD_LENGTH`,
  surfaces unexpected iteration errors, and revalidates the exact process
  creation FILETIME plus module base/path before remote `FreeLibrary`; main,
  Windows, critical-loader, cross-bitness, and self targets fail closed.
- **Tables/settings:** Details gains description, publisher, parent PID,
  session ID, image path, page faults/sec, and I/O read/write columns;
  Startup/App History/Users/Services sort by headers; quiet column guides,
  keyboard list movement, and a Reset column widths setting were added.
  Settings is now resizable/scrollable and exposes the default page/process
  presentation. Registry startup publishers are resolved best-effort.
- **Performance/data:** lighter/resource-colored charts, graph-context CPU
  controls, responsive logical-CPU grids, combined network throughput, and
  cached native IPv4/IPv6/SSID/signal details. Unsupported App History network
  is `—` rather than a fabricated zero; the UI labels history as local-only.
  History disk writes are coalesced to 30 seconds plus shutdown.
- **Renderer/size:** WGPU is built with one native backend per target (D3D12
  on Windows, Vulkan on Linux, Metal on macOS), low-power adapter preference,
  FIFO/one-frame latency; Glow remains the fallback. This removes unused WGPU
  backend dependencies without removing recovery for problematic drivers.

Headless verification: `python build.py --check` passed format, clippy with
warnings denied, and 148 tests (69 app, 47 core, 27 platform, 5 Windows
integration); `python build.py --host-only` refreshed/package-checked the final
artifact. The EXE is 13,470,208 bytes versus the 14,656,000-byte baseline:
1,185,792 bytes / 8.09% smaller despite the added diagnostics. The feature
graph and binary string check contain DX12 but no Vulkan loader/API on Windows.
Linux packaging was skipped because neither `cross` nor `cargo-zigbuild` is
installed, as permitted by the non-`--require-all-targets` gate.

## 2026-08-28 — Security audit: 6 fixes (parser OOB, kill-path identity, hardening)

Full security audit of the workspace (source, deps, secrets, binary
hardening, runtime). Findings implemented:

- **SMBIOS Type-17 OOB panic** (`tm-platform/win/memory_info.rs`): the old
  guard `< 0x15` admitted spec-legal 0x15–0x1A-byte records but indexed up
  to 0x1A — index panic in `Sampler::lazy_init`, fatal because release uses
  `panic = "abort"`. Now every field read is length-guarded via the new
  pure `ram_static_from_table(table)` (probe is a thin wrapper), with
  regression tests for short (0x15) and full (0x22) Type-17 records.
- **Kill-path PID-reuse TOCTOU** (`tm-platform/win/process_ops.rs`):
  identity was only checked UI-side against a possibly-stale snapshot; the
  platform killed a bare pid (tree-kill children had NO identity check).
  `PlatformActions::kill_process` now takes
  `expected_start_epoch_s: Option<i64>`; `open_process_verified` reads the
  creation time THROUGH the freshly opened handle (handle-bound → immune to
  later pid reuse), ±2 s tolerance, fail-closed when the expected time is
  set but unverifiable. Tree-kill captures each child's birth at
  enumeration and re-verifies via the terminate handle; children without a
  captured birth fall back to the old unverified kill (their identity has
  nothing to do with the root's). Linux/macOS use the trait default and
  ignore the hint (documented).
- **Details context-menu identity gates** (`tm-app/tabs/details.rs`):
  priority/suspend/affinity now run the same start-time check against the
  live snapshot that End Task and Efficiency mode already had.
- **Elevated relaunch quoting** (`tm-platform/win/mod.rs`):
  `quote_win_arg` implements the MSVCRT/CommandLineToArgvW quoting rules
  (escaped quotes, doubled backslashes) for
  `relaunch_elevated_with_args`; round-trip tested against the real
  `CommandLineToArgvW`.
- **Sign-out confirmation** (`tm-app/tabs/users.rs` + `app.rs`):
  session Logoff parks in `TaskManApp::pending_session_logoff` and shows a
  confirm dialog (`session_logoff_dialog`, new `K::SignOutConfirm` key);
  Disconnect stays immediate.
- **Binary hardening**: `.cargo/config.toml` adds
  `-C control-flow-guard=yes` for windows targets (verified: Guard CF
  function table 0 → 1391 entries); `build.py` release builds add
  `--remap-path-prefix=<home>=` (strips the build-machine user name from
  panic locations; verified: zero "REDACTED" strings in the shipped exe).
  RUSTFLAGS overrides config rustflags, so build.py restates the CFG flag.
- macOS `launchctl kickstart` target now interpolates `libc::getuid()`
  (the literal `$(id -u)` never resolved — launchctl gets no shell).

Audit evidence (2026-08-28, commit 93c460d): cargo-audit clean (456
deps), gitleaks clean (tree + 75-commit history), clippy -D warnings +
fmt clean, 134+ tests pass, `--selfcheck` green on the hardened release
binary. Offline app (no networking crates at all). Known coverage gap: no
cross/zigbuild on this machine → Linux x86_64 build/ELF inspection not
verifiable locally; ARM64/macOS targets are not built by build.py.

## 2026-08-28 — Settings: always-start-elevated policy + one-shot restart

Windows-only "Administratorrechte" / "Administrator privileges" section in
the settings dialog (inside the existing `Advanced` block, below the Task
Manager replacement):

- **Status line** — whether THIS process is elevated. `TaskManApp::new`
  queries `actions.is_elevated()` exactly once (`is_elevated` field);
  elevation is fixed at process creation, never per frame.
- **"Always start with administrator privileges"** (`start_elevated`,
  `[general] start_elevated` in config.ini): at startup, `run_gui` checks
  the setting BEFORE any window exists and, when this launch is unelevated,
  re-execs via `tm_platform::win::relaunch_elevated_with_args(args)`
  (ShellExecuteExW "runas", CLI args forwarded) and `std::process::exit(0)`.
  The elevated child re-reads the setting, is elevated, and proceeds — no
  loop. A declined UAC prompt logs a warning and degrades to a normal
  unelevated start (retried next launch). Guards: `TASKMAN_CONFIG_DIR`
  override (test/isolated context) never auto-elevates; `--selfcheck` and
  the `--taskmgr-integration` helper exit before the check. Every launch
  therefore shows a UAC consent prompt — inherent for third-party exes
  (Task Manager itself auto-elevates via its system-binary status).
- **"Restart as administrator" button** — one-shot elevation of the
  current session: `PlatformActions::relaunch_elevated()` (no args) on the
  action executor; on success the job sends `ViewportCommand::Close` from
  the executor thread — safe because `Context` is Send+Sync and commands
  queue into the next frame; eframe 0.36 exits the event loop gracefully
  (`should_close`), so `on_exit` still flushes settings + app history. A
  declined prompt surfaces the standard error toast; the single executor
  worker serializes repeat clicks, so two prompts cannot race.

Tooling note: `tools/capture.ps1` now seeds an isolated temp config dir
(`TASKMAN_CONFIG_DIR` + `config.ini`, ASCII on purpose — a UTF-8 BOM would
break the INI parser's first `[general]` header) instead of writing legacy
`settings.json` into the REAL config dir; this also keeps captures working
when `start_elevated` is on (auto-elevation is skipped under the override).

Verified visually via `tools/capture.ps1` with `TASKMAN_DIALOG=settings`;
UAC consent paths not exercisable headlessly. `build.py --check`
(fmt + clippy `-D warnings` + workspace tests, incl. release build)
passed.

## 2026-08-27 — Details column prefs persist; last resize handle grabbable

Two user reports, both on tables:

1. **Column visibility/order was session-only.** The Select-columns dialog
   state (`details::State.visible`/`order`) never reached `config.ini`.
   Fixed with two new settings fields: `col_visible` (`table -> id -> on`,
   ONLY entries differing from the built-in default, so future builds'
   new columns keep their designed default) and `col_order` (`table ->
   [ids]`, stored only while it differs from the built-in order). INI
   schema: `[columns.<table>.visible] <id>=0|1` and
   `[columns.<table>.order] order=<id>,<id>,...` — parsed under the
   existing `columns.*` prefix logic via `rsplit_once('.')`. Applied at
   startup in `TaskManApp::new` (`details::State::apply_saved_prefs`, with
   guards: never empty the table, sort column always visible, unknown ids
   skipped, missing ids keep built-in position); written back by
   `details::persist_column_prefs` on every dialog mutation through the
   usual debounced `save_settings()` path. Hidden GPU columns also lower
   telemetry demand correctly from startup.

2. **Last column's resize handle ungrabbable when the table is wider than
   the window.** egui hit-testing clips widget rects to the scroll area's
   clip rect; the header scroll area had NO content margin while the body
   reserves `BODY_PAD_RIGHT` (10 px), so fully scrolled right the last
   boundary sat flush at the viewport edge — only the inner ~6 px of the
   ±6 px handle were clickable, effectively unreachable. Fix: the header
   gets the same right `content_margin` as the body (both in
   `scrolled_table` and `scrolled_rows`), which also aligns header/body
   far-right geometry. Regression test
   `last_boundary_is_grabbable_when_scrolled_fully_right` drives the real
   `scrolled_rows` path (priming the stored body offset BETWEEN passes —
   egui pass memory starts lazily, and `insert_temp` is type-generic:
   an untyped `10_000.0` literal silently stored as f64 and was never read
   back by the f32 reader). Verified the test fails without the fix.

Tests: settings roundtrip + new-section parsing; details prefs
roundtrip/reorder/guards; tablekit last-handle drag. `build.py --check`
(fmt + clippy `-D warnings` + workspace tests) passed; release build
packaged.

## 2026-08-27 — Performance chart freeze (VecDeque ring wrap)

User report: sometimes the Performance graphs / card sparkline previews
stop updating while the rest of the app stays live. Root cause was NOT the
engine/wakeup path (engine → `request_repaint` → eframe event loop was
verified sound) but `TaskManApp.history`: a `VecDeque` with capacity
`history_cap + 8` that `poll_engine` keeps at `len == history_cap` via
pop-front/push-back. Once the ring wraps (after ~cap ticks — 2 min at
Normal speed), `as_slices()` returns TWO runs and the newest points live in
the SECOND one — which `performance::window()` discarded (`let (full, _)`).
Every frame then rendered a stale front-run: frozen for 119 of every 120
ticks (verified with a standalone ring probe). The one-tick catch-up blip
per cycle explains the "sometimes" flavor.

Fix: history is a plain contiguous `Vec<HistoryPoint>` (always
append-ordered; `push_history_point` extracted for the regression test
`history_retention_keeps_newest_point_visible`). Sibling hardening in the
same symptom class: `visible_slice` now scans backward from the newest
sample instead of `partition_point` (robust against a backward wall-clock
step leaving future-stamped older points), and `chart_multi` computes its
x-span with `saturating_sub` (the old `last - first` wrapped/panicked on
such data). If history ever becomes a deque again, windowing must handle
both slices or call `make_contiguous` — see the field doc in `app.rs`.

Tests: retention-through-wrap (app.rs), backward-clock-step window
(performance.rs); `cargo test -p tm-app` 55 passed, clippy clean, release
build packaged.

## 2026-08-27 — TM-parity resource sorting (flat list, no group sections)

Follow-up to the attribution fix, from a side-by-side screenshot: native
Win11 Task Manager keeps the Apps/Background/Windows sections ONLY when
sorted by Name; any resource sort (CPU/memory/disk/network) flattens the
whole list into ONE globally sorted sequence (family groups like
"Brave (29)" stay collapsible). We kept the sections, burying top
consumers.

Fix in `build_display_rows` (tm-app/tabs/processes.rs): group headers and
collapse are applied only for `sort_col < 2` (Name/Status); for resource
sorts `sort_blocks_globally` reorders the per-section emission: the
emitters produce self-contained BLOCKS (depth-0 head row + its expanded,
nested children), blocks are sorted by the head's representative value
(subtree aggregate for family/tree heads, own values otherwise) and
concatenated. Expanded families therefore stay attached while heads
compete globally. Group-collapse state is ignored in the flat view
(native TM offers no group toggles there either). Do NOT "fix" this back
to per-section grouping for resource sorts.

Also: the "Terminated processes" pseudo-row shows its count only when > 0
(a residual without observed exits — born-and-dead-within-one-window
churn is never sampled alive — must not read "(0)"); new i18n key
`TerminatedProcessesPlain`. NOTE for tests: a busy DIFFERENT-image child
of an app (≥ 1 % cpu) is intentionally promoted to a Background top-level
row by `promote_busy_external_tasks`, so it competes globally in the flat
view — block-attachment tests need same-image children.

Tests: flat ordering across categories (top consumer first, no headers),
name sort keeps sections, expanded same-image family stays attached,
pseudo-row label/tooltip.

## 2026-08-27 — CPU attribution completeness (terminated processes, interrupts)

User report: while compiling in a terminal, the Processes page showed NO
process owning the CPU load even when sorted by CPU. Root cause was in the
time-based accountant (`win/cpu_load.rs::build_sample`), not in grouping:

- **New processes got a fabricated 0 %** on their first sample
  (`.map_or(0.0, ...)` when absent from the previous sample).
- **Processes that terminated during the sampling window contributed their
  whole in-window CPU time to NO row**: the per-process loop iterates the
  CURRENT `SystemProcessInformation` table only, so a `rustc.exe` born and
  dead inside one ~1 s window (typical for small crates) was never seen at
  all. The global number (per-core accumulators) sees everything — hence
  "high load, no responsible process".

Fix (accounting completeness, all in `cpu_load.rs` + `win/sampler.rs`):
1. Processes born inside the window are credited their accumulated time
   since creation (which for them is exactly in-window time); reused pids
   get the same treatment (create_time guard).
2. `LoadSample` now carries `unattributed_pct` (global busy − Σ live-process
   in-window time) plus `exited_count`/`exited_images` (image names parsed
   from the NT table's `ImageName`, remembered from the previous sample).
3. `sampler.rs` splits the residual: measured `% Interrupt Time` (new PDH
   group `interrupt`, gated on CORE_PROCESS, counter path
   `\Processor Information(_Total)\% Interrupt Time`) → "System Interrupts"
   row (System/Windows group, TM parity); the rest → "Terminated processes
   (N)" row (Background) with exited image names as a localized hover
   tooltip. Both are SYNTHETIC `ProcessEntry` rows (`synthetic: bool` on the
   model, sentinel pids `u32::MAX`/`u32::MAX-1`), appended AFTER
   `refine_categories_and_group_apps` so the classifier never touches them;
   they sort/heat-map/search like any row. Rows show only above 0.5 % with
   a 5-tick hold-decay (`HeldPseudoRow`/`PseudoRowHold`) so bursty churn
   does not flicker; a measured-low interrupt value hides immediately (only
   UNKNOWN measurement decays — never read missing as zero).
4. Actions are withheld: no context menu on synthetic rows; the header
   aggregate comes from `snap.cpu.utilization_pct`, so no double counting;
   users tab skips them (no session); details shows them like native TM
   shows "System interrupts" (Del/kill guarded by `identity_is_live`).

KEY EMPIRICAL FINDINGS (pinned by a live-kernel unit test
`live_kernel_table_yields_sane_image_names`):
- `SYSTEM_PROCESS_INFORMATION.ImageName.Buffer` is an **absolute pointer
  into the output buffer** on this Windows build (NT writes the caller's
  buffer in place — matches ReactOS `SpiCurrent->ImageName.Buffer =
  (void*)(Current + CurrentSize)`); the Process-Hacker-style record-relative
  interpretation decoded 0/285 names here. `parse_image_name` therefore
  tries absolute / table-relative / record-relative candidates, all bounds-
  and control-character-validated, empty name on any doubt. The i18n
  `keys!` macro CANNOT take multi-line array entries (`expr` fragment
  matcher breaks on the newline before `,`) — keep entries single-line.

Tests: accountant unit tests (new-process credit, residual → exited
names, buffer conventions, live table), sampler hold/decay + append tests,
Processes-tab presentation tests (pseudo rows in the right groups, sorted
by CPU, tooltip only for synthetic rows).

## 2026-08-27 — command lines, real CPU speed, high-CPU background visibility

Three user-reported bugs fixed:

1. **Details command line always "—" on Windows**: `ProcessEntry.command_line`
   was never populated (only the Linux backend did). Fix:
   `process_ops::command_line_of(pid)` via
   `NtQueryInformationProcess(ProcessCommandLineInformation)`. IMPORTANT
   finding: on this Windows build the correct PROCESSINFOCLASS value is **60**
   (matching windows-rs 0.62's `Wdk_System_Threading` binding) — the older
   "class 92" reference does not work here (STATUS_INFO_LENGTH_MISMATCH with
   any buffer). Works with only `PROCESS_QUERY_LIMITED_INFORMATION` (no
   VM_READ); elevated/protected processes fail to open → None → "—". Wired
   through the 10 s TTL `PidAttrs` cache in `sampler.rs` (new field
   `command_line`); integration + unit tests spawn a child and assert the
   args are retrieved.
2. **Performance CPU speed stuck at base clock**: sysinfo's frequency comes
   from `CallNtPowerInformation(ProcessorInformation)` `CurrentMhz`, which
   reports the *nominal* clock constantly on modern Windows (verified: static
   3401 MHz on a 5700X even under load; WMI CurrentClockSpeed identical).
   Fix, Task-Manager-style: new demand-gated PDH group `cpu` with single
   counter `\Processor Information(_Total)\% Processor Performance`
   (`perfcounters.rs`; new `TelemetryDemand::CPU_SPEED` bit 8, set for
   Tab::Performance). `sampler.rs` computes `freq_mhz = base × pct/100`.
   Fallback ladder: counter warming → 0 (UI renders "—", never fakes data);
   counter permanently unavailable (`cpu_counter_failed`) → sysinfo value.
   Counter needs 2 PDH collections before formatting succeeds (matches
   existing `QueryGroup` warm-up). Verified live: idle 4.2 GHz, under load
   4.4 GHz (base 3.4).
3. **High-CPU background/CLI tasks invisible on Processes page**: TWO root
   causes, both fixed:
   - **Background/Windows groups render as TREES** — a busy build tool under
     a console shell (cmd → cargo → rustc, all Background) was hidden as a
     child row under the unexpanded shell row; only the aggregate leaked
     into the parent. Fix: Task Manager parity — **Background/Windows groups
     are FLAT lists** (every process its own depth-0 row with its OWN values,
     sorted by the current column, no expand handles). Only the Apps group
     keeps the family tree. Verified with a live-system repro test: busy
     powershell under cmd becomes a visible flat row.
   - App-absorbed external tasks: promotion pass in `derive_display_groups`
     (`promote_busy_external_tasks`, `is_external_family_member`,
     `PROMOTE_CPU_PCT = 1.0`): an absorbed non-root process with cpu share
     ≥ 1 % whose image differs from every family ancestor is reclassified to
     Background (with its absorbed descendants, wholesale) and appears as an
     ordinary flat Background row; same-image helpers (Chrome renderers
     etc.) stay folded like TM app children. Two-phase decisions (against
     pre-promotion categories) keep the result iteration-order independent.
     Guards learned from the HitmanPro report: **windowed processes are
     never demoted** (they are foreground apps), and the wholesale descent
     skips windowed children (they surface as their own app roots).
   - **Windowed absorption refined (`plausibly_same_application`)**: a
     windowed process folds into a windowless ancestor's family only when
     they share the image or the publisher (company from version metadata;
     unknown publisher falls back to permissive). Start-menu/COM launches
     are brokered by windowless shell-session processes (sihost,
     RuntimeBroker, dllhost — NOT explorer), which would otherwise adopt
     the launched app (HitmanPro case); those brokers plus browsers are
     launch boundaries now. Boundary check precedes the company check, so
     same-image secondary browser windows/PWAs start their own rows
     (TM shows PWAs separately); non-boundary same-company families
     (steam.exe/steamwebhelper) still absorb.
   - **Background/Windows family collapse (TM parity per user's TM
     screenshot)**: connected same-image families render as one expandable
     `Name (N)` row with the family aggregate ("Dropbox (7)"), expanding to
     member rows; unrelated same-name processes and mixed-image trees stay
     flat (`emit_flat_with_family_groups`, `same_image_family`).
   Existing test fixtures set explicit low `cpu_pct` where promotion would
   otherwise trigger (proc() helper defaults cpu = 1.0×pid). NOTE:
   `cpu_pct` is share of TOTAL machine capacity — a full core on a 16-thread
   machine shows as 6.25, not 100; don't key logic off raw "100%".

## 2026-08-27 — window placement UX, type-ahead scroll fixes, dialog chevrons

Three user-reported issues fixed:

1. **Window size/position not persisted**: root cause on the affected
   machine was `remember_window=false` in `config.ini` — a setting with NO
   settings-dialog UI, so it could not be re-enabled. The Settings dialog
   now has a "Remember window size and position" checkbox (i18n key
   `RememberWindow`, persists via autosave). Additionally, maximized state
   is now part of placement: while maximized, neither the restore size nor
   the position is clobbered with monitor geometry; `window-state.ini`
   gains a `maximized=` key and startup re-maximizes via
   `ViewportBuilder::with_maximized`. See `ui_state.rs` (Placement struct)
   and `NativeApp::ui` in main.rs.
2. **Select-columns dialog arrows**: the →/← text buttons sat under the
   floating vertical scrollbar (right edge) and could not be clicked. They
   are now painted chevron icons (`controls::icon_button`, new
   `icons::Icon::ChevronUp`), moved 16 px left of the scrollbar strip, and
   reordered to ↑/↓ (up = earlier position, down = later).
3. **Type-ahead scroll**: plain-letter navigation (Processes/Details)
   used `Response::scroll_to_me(Some(Center))` on a virtualized row, which
   (a) never fired for rows outside the rendered window (no vertical
   scroll at all) and (b) when it fired, targeted BOTH scroll axes,
   yanking the table horizontally. `tablekit::scrolled_rows` now takes a
   one-shot `focus_row: Option<usize>` and computes a vertical-only,
   minimal-move offset from the last frame's y-offset (`tm-rowsy` temp)
   applied via `ScrollArea::vertical_scroll_offset` on the request frame
   only. Callers pass the index from `scroll_to_pid.take()`; the per-row
   `scroll_to_me` mechanism is gone. Regression test:
   `focus_row_scrolls_vertically_only_even_for_unrendered_rows`.
   `search::cycle_process_initial` was genericized to `cycle_match<T:
   PartialEq + Clone>` and the Performance card column gained the same
   type-ahead (jump + vertical scroll via `scroll_to_me(None)`, which for
   full-width items can never move horizontally).

Also fixed (found by the heavy gate): `cpu_info::base_mhz_from_smbios_table`
returned `max(current, max)` while its tests document current-speed-
preferred-with-max-fallback — pre-existing failing test
`smbios_type4_current_speed_is_preferred`, now green.

Note: HEAD was not fmt-clean under the local rustfmt
(1.10.0-nightly 2026-08-25); `python build.py --check` failed on the
pristine tree. The formatting drift in previously untouched files
(fonts.rs, chart.rs, linux/*, win/mod.rs, taskmgr_replacement.rs) is
mechanical rustfmt output required to keep this machine's gate green.

Validation: `python build.py --check` (fmt, clippy -D warnings, all
workspace tests) + release build/packaging + `--selfcheck --mock` pass.

## 2026-08-27 — Processes app-grouping parity

The Processes page now builds a presentation topology instead of treating
raw PPID as UI ownership. Explorer and common shell launchers are boundaries,
so programs the user starts from Explorer/cmd/PowerShell/Terminal appear as
independent app groups while helpers remain under their app family. Raw
`ProcessEntry.ppid` is unchanged.

- App membership is rebuilt from visible-window ownership while preserving
  System classification; no-window backends retain collector categories.
- Display parent edges are cut across category boundaries and at app roots,
  so Explorer no longer inherits CPU/memory/subtree counts from launched apps.
- `Apps (N)` now counts top-level app groups (matching native `Apps (9)`);
  Background/Windows keep unflattened process counts.
- Cyclic/malformed PPID components stay visible instead of disappearing.
- Regression coverage covers Explorer launches, shell-launched GUI apps,
  app-group totals, aggregate boundaries, and cycle visibility.
- CPU load/accounting code was not changed.

Validation: code was statically reviewed through the GitHub connector. This
environment has no Rust toolchain/Windows runtime, so the requested local
build remains the final compile/runtime/UI check.

## 2026-08-26 — audit.md Phase 1 (correctness) implemented

All 11 Phase-1 items from the 2026 parity audit landed, each with
regression tests:

1. **Table width architecture (P0.1)**: `TmColumn::elastic` and the
   index-0/"width==0 sentinel" fill behavior REMOVED. Every column uses its
   configured/persisted width; unused viewport space stays blank on the
   right like native TM. `TmTable::new`/`make_table` lost the `name_min`
   param; `col_width(i)`/`total_width()` no longer take `avail`; layout is
   width-driven (rebuilt only on mutation). Double-click restores
   `default_w`.
2. **Per-column heat normalization (P0.2)**: `heat_cells` now paints given
   `HeatCell { intensity, text }` values; callers normalize per COLUMN over
   the whole display model BEFORE virtualization (`tablekit::norm`,
   processes' `normalize_heat`, users' `HeatMax`, apphistory maxima).
   "value>0 ⇒ 1.0" binary intensities are gone.
3. **Details GPU demand (P0.3)**: `show_gpu_columns` bool removed;
   `State.visible: BTreeSet<ColumnId>` is the single source of truth, both
   for rendering (dynamic column list) and `requires_gpu_telemetry()`
   demand derivation. Minimal Select-columns dialog added ("…" overflow),
   session-only persistence (see known-debt.md).
4. **Users search fixed**: old condition kept every active user visible;
   now query matches user display name OR any aggregated app name.
5. **Global search + shortcuts**: new `tm-app/src/search.rs` `Query`
   matcher (binary name/display/PID/publisher) used by Processes, Details,
   Startup (+publisher) and App History. Alt+F and Ctrl+F focus the global
   search field (`egui::Id::new("global-search")`).
6. **Performance Refresh now**: actually calls `refresh_all()`.
7. **Startup impact**: disabled items report `None` (win/startup.rs);
   enabled-without-data stays Unknown; real thresholds = later SRUM work.
8. **Group counters (P0.4/P0.5)**: `DisplayRow::GroupHeader(gi, total)`
   carries unflattened classification counts; grouped labels use O(n)
   whole-subtree process counts (`subtree_values_and_counts`). RowData lost
   its now-unread `group` field.
9. **Selection identity (§7)**: `selected_pid` replaced by
   `selected_process: Option<ProcessIdentity>`; `end_selected` validates
   start-time identity against the live snapshot before dispatch
   (`TaskManApp::identity_is_live`); Efficiency toggles validate too.
10. **Efficiency mode from OS state (§8)**: leaf icon/menu derive from
    `ProcessEntry.power_throttled`; `efficiency_pids` HashSet deleted;
    toggle issues one forced refresh so paused mode updates as well.
11. **History capacity (§10)**: `history_cap_for()` recomputes whenever
    `graph_seconds` changes (logic pass), truncating overflow.

Also fixed in passing (§23): services fetch + service-control workers wake
the UI via `Context::request_repaint` (Services page could stick on
"Gathering data" while paused). Stale cpu_load.rs doc rewritten to the 2026
metric split (current pages = time-based; utility survives as Details
"CPU Utility" column; legacy provider = future work).

Gates: fmt+clippy(-D warnings)+workspace tests+release build all green on
Windows (`build.py --check`, then `--host-only`); selfcheck --mock ok.

## 2026-08-26 — Visual parity pass vs real Win11 TM (taskmanpngs/ reference)

Measured the real Task Manager screenshots (taskmanpngs/1..7.png, captured at
133% scaling) pixel-by-pixel and re-derived the design tokens. Reference
logical sizes (Segoe cap≈0.75·font): rows 13, header labels 12, header
aggregates 17, group headers 20–21, sidebar/search/tab-title 15, titlebar 13,
kv rows 13 (pitch 22.5), captions 11, stat values 23, card titles 17,
page title 31. Colors (dark): content 0x191919, sidebar/chrome 0x202020
(sidebar LIGHTER than content), header separators 0x2D2D2D, heat base
(17,36,62), top-consumer cell (8,51,110), heat cell separators (41,50,63).

Changes:
- theme.rs: new palette (window 0x191919, sidebar 0x202020, stroke 0x2D2D2D,
  heat_top/heat_sep added), Body/Button text style 13.
- tablekit.rs: ROW_H 32, HEADER_H 57; `ui.spacing_mut().item_spacing.y = 0`
  before show_rows so rows TOUCH (the 6px default gap striped the heat
  bands); header agg font 17 at top+19, label font 12 at bottom−13;
  heat_cells now draws the flat TM style: base fill + brighter `heat_top`
  cell for each column's top consumer (max intensity) + 1px separators
  between cells. `heat_blue` gradient kept (dead_code) for future use.
- processes.rs: group headers 20px, NO background band (ref has none);
  heat intensities are binary (value>0 ⇒ 1.0) — top consumer per column wins.
- users.rs/apphistory.rs: same binary intensity model.
- app_ui.rs: nav items 15px text, h=38, accent bar 3×18; search box 495px
  wide with 15px font; cmd buttons 13px; toasts 13px.
- performance.rs: page title 31, right detail 17, card titles 17, card value
  lines 13, captions 11.5, big/med stat values 23 with 13px labels
  (56/48px blocks), kv rows 13px pitch 23.
- chart.rs: clippy `chunks_exact(3)` → `as_chunks::<3>()`.

Verified against ref by downscaling our 200%-DPI captures by 2/3 and
comparing crops side by side (shots/ui3-*.png).

## 2026-08-26 — Column/splitter resize ACTUALLY fixed (egui delta semantics)

**Root cause of "columns can't be resized" (persisted across the 2026-08-25
fix):** implement.md §8.1 claimed `Response::drag_delta()` is cumulative from
drag start — FALSE in egui 0.36. It is `pointer.delta()` = movement since the
LAST FRAME; only `total_drag_delta()` is cumulative. The shipped math froze a
drag-start width (`start_w + drag_delta().x`), so every frame reset the width
to ~its starting value: boundaries jiggled sub-pixel and snapped back when
the pointer paused. The 2026-08-25 fix (materializing the elastic Name
column) addressed a real but DIFFERENT failure layered on top; neither made
resizing work.

Fix (tablekit.rs + performance.rs splitter): accumulate each frame's delta
onto the LIVE width, `width = (width + dx).clamp(min,max)`; drop the
drag-start temp-data machinery entirely.

Also fixed in the same pass:
- Resize handles were registered DURING the cell loop, so each next header
  cell covered the right half (±6 px) of its neighbor's handle and won hit
  testing there — grabs landed on the cell and quick clicks even toggled
  sorting. Handles are now ALL created after the cell loop (topmost), full
  ±6 px grabbable; the last column keeps its right-edge handle.
- Double-click-to-default never worked: a `Sense::drag()` widget never
  receives egui click flags, so `double_clicked()` was always false. Now
  detected via `pointer.button_double_clicked(Primary)` while hovered.
- Regression tests drive REAL pointer events through an egui `Context`
  (`ctx.run_ui` + RawInput events; clear `out.textures_delta` or egui panics
  headlessly). Verified the test FAILS against the old math (730 ≠ 760).
  Test-authoring gotchas: egui counts multi-clicks ACROSS sequences within
  0.3 s (space sequences >0.3 s apart); boundary x positions must be derived
  from CURRENT column widths after any mutation.

implement.md §8.1/§14.1 corrected in place.

## 2026-08-25 — UI polish pass + Linux backend repaired & verified under WSLg

**Column resize root cause (tablekit.rs):** the elastic name column absorbed
viewport slack EVERY frame, so a manual drag of any other boundary was
cancelled in the same frame (dragged separator stayed put, Name/Status
divider shifted instead, regime flips around `spare == stored` read as
wobbling). Fix: `name_effective` is elastic ONLY while `width == 0.0`
(virgin sentinel); the first `drag_started` on any table materializes the
name width (value-preserving), after which all columns are explicitly
sized and boundaries track the cursor 1:1. Double-click on the name
separator restores fill mode via the 0.0 sentinel. Also: last column got a
right-edge resize handle; removed the extra stroke box around the Name
header cell; `table_avail` margin 6→16 px; body ScrollAreas got
`.content_margin(right:10, bottom:8)` so FLOATING scroll bars never paint
over the last column/bottom row (floating bars never reserve layout space;
reserving would desync header/body widths).

**Graph rendering root cause (chart.rs):** all area fills used
`Shape::convex_polygon`, but epaint fan-triangulates fills from vertex 0
(`fill_closed_path`) — only valid for CONVEX polygons. Concave series got
straight fan edges cutting across dips: fake linear ramps, cliffs, fills
floating above the line, white gaps (see graphpngs/1-3.png). Fix:
`fill_area_to_baseline()` builds an explicit x-monotone triangle strip
(2 verts + 6 indices per segment) that hugs the polyline exactly; used by
sparkline, core_chart (incl. kernel band) and chart_multi. Also: series
extractors (`disk/net/gpu_series`) now return one value PER window point
(0.0 when the device is absent; no more zero-filtering in net_series) so
series indices stay aligned with `timestamps_ms` — shortened series used
to plot at wrong x positions; chart_multi hover maps pointer time back to
the nearest sample via partition_point.

**Other UI:** checkbox hover theme-aware (accent border over faint tint,
no white flood in dark mode); chart strokes/fills strengthened, kernel
times = darker accent shade (`kernel_color`), per-theme secondary colors
(`pal.ok_green` not hardcoded dark-theme green).

**Linux backend (tm-platform/linux):** was NOT compiling — model additions
from Windows work (`CpuInfo::kernel_pct/per_core_kernel_pct`,
`GpuInfo::luid/shared_used_bytes`) never landed there, plus missing
`use std::sync::Mutex` and 5 clippy lints. Filled with documented "unknown"
values (empty vec / 0 / None) matching model docs & mock. Verified:
- `--selfcheck` real backend ok:true (CPU/mem/disks/networks/processes;
  GPU honestly empty — WSL2 exposes no /dev/dri for DRM).
- GUI runs under WSLg Wayland: wgpu+Vulkan at locked ~60 fps (Fifo), surface
  1100×720; glow renderer starts too. Note: wgpu hides "Microsoft Direct3D12
  (NVIDIA ...)" as not-Vulkan-compliant → renders via llvmpipe software path,
  still vsync'd 60 fps.
- `goto_services_for_pid`: cfg_attr allow(dead_code) on non-Windows (callers
  are cfg(windows)); keep compiling everywhere.

**WSL/Windows dual-host gotcha:** WSL builds produce `target/release/taskman`
(ELF) while the launched binary is `taskman.exe` — a WSL-side
`build.py --host-only` does NOT refresh the exe. Refresh it from WSL via
interop: `cmd.exe /c "... && set PATH=C:\Users\REDACTED\.cargo\bin;%PATH% &&
py.exe build.py --host-only"` (works; selfcheck via powershell
Start-Process -RedirectStandardOutput since GUI-subsystem console attach
fails through the interop pipe). Consider separate CARGO_TARGET_DIR per
host to avoid cache thrash.

## <YYYY-MM-DD> — Template scaffold created

Initial generic `AGENTS.md` / `llm-wiki` template. No project-specific
history recorded yet.
