# Changelog

## Unreleased

### Fixed

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
