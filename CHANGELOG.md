# Changelog

## Unreleased

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
  key now works while any non-text widget holds focus.
- **App History on Linux and macOS:** non-system processes now accumulate
  local usage history even where window ownership cannot be determined.

### Fixed

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
