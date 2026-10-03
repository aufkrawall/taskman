# Changelog

## Unreleased

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

### Changed

- **Core service protocol v6:** the identity read above is a wire addition, so
  GUI and service must be upgraded together (setup does exactly that). Against
  an older service, privileged broker calls are refused with "action state is
  unknown" instead of being misapplied, and identity reads answer nothing —
  the affected rows keep "—" until the service is upgraded.

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
