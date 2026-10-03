# Windows Setup Installer

## Sources

- `crates/tm-installer/` — `taskman-setup` (wizard + silent installer) and
  `taskman-payload` (build-time embedder), sharing `payload.rs` (archive
  format), `options.rs` (CLI), `install.rs` (plan/orchestration), `win.rs`
  (Win32 plumbing), `ui.rs` (eframe wizard).
- `crates/tm-ui/` — the shared theme (see `repo-map.md`).
- `build.py` `package_setup()` — artifact production.
- Contract it reuses: `tm-platform/src/win/core_service.rs`
  (`--core-service=install|uninstall` elevated helper; see `core-service.md`).

Modeled on the green-curve setup installer: a single self-contained
`taskman-setup.exe` per architecture, custom-drawn UI that mirrors the host
application's theming, no NSIS/WiX/Inno and no native helper libraries
("100% Rust": `flate2`/miniz_oxide for compression, `windows` crate for Win32,
eframe for the wizard).

## Shape of the artifact

`taskman-setup.exe` = linked wizard + appended payload:

```text
[ setup executable ][ deflate blob region ][ manifest JSON ]
[ manifest_len: u64 LE ][ MAGIC: u64 LE ("TMSUPAYL") ]
```

- Entry names are flat (`taskman.exe`, `taskman-service.exe`, `LICENSE`);
  the parser rejects separators, drive markers, dot-dot and dotfiles.
- The reader validates the footer magic, manifest shape, offset/length pairs
  and non-overlap, inflates every entry in memory, and verifies each SHA-256
  BEFORE extracting anything: a corrupt payload never leaves a half-written
  install directory.
- Inflation is bounded by the manifest's declared size (`inflate_bounded`
  reads at most `declared + 1` bytes): the manifest is untrusted, and a small
  deflate stream can expand without limit.
- `taskman-payload <setup.exe> <out.exe> <name>=<file>...` appends the
  archive. It runs on the build HOST, so an x64 host can package the ARM64
  installer. Writer and reader live in one crate, so the format cannot drift.

## What install does (and does not) do

Steps (visible in the wizard and in `--dry-run`):

1. Verify the payload and extract to freshly created administrator/System-only
   staging under Program Files (`staging.rs` creates an inheritable protected
   DACL atomically). Its RAII guard cleans up on success and failure. This is FIRST,
   before anything on the machine is touched: a bare build output (no
   embedded payload) or a corrupt artifact must fail here, not after the
   running app has been stopped. The wizard additionally preflights this at
   startup, so a payload-less `taskman-setup.exe` opens straight on the
   failure page with guidance to the packaged installer (uninstall is exempt:
   it needs no payload).
2. Stop running `taskman.exe` instances whose image path is the installed
   GUI (WM_CLOSE first, bounded handle wait, terminate as fallback). Portable
   copies elsewhere are left alone.
3. Program files + service: run the app's own elevated helper
   `taskman.exe --core-service=install --core-service-user=<sid>`. That helper
   owns the pinned copy, ACLs, broker manifest and SCM registration; the
   installer does NOT reimplement any of it. `--no-service` first runs the
   new payload helper's uninstall operation, waiting for an existing service
   to stop, then calls `core_service::install_program_files_without_service`
   for the same protected-directory and pinned-copy rules. This removes an
   existing service during upgrade; later enrollment is available in Settings.
4. Add/Remove Programs entry (whose `UninstallString` points at the
   `taskman-setup.exe` copy placed into the install directory) — directly
   after step 3, so once a LocalSystem service exists the install is always
   removable even if a later step fails. The copy is skipped when setup
   already runs from that file (file identity, not path text).
5. Shortcuts (per-user Start menu by default, desktop opt-in).
6. Optional launch with the desktop user's unelevated token and environment
   (`CreateProcessWithTokenW`); no elevated fallback. Token elevation is checked
   before touching the installation when launch is requested.
   The duplicated primary token needs `TOKEN_ADJUST_DEFAULT` and
   `TOKEN_ADJUST_SESSIONID` in addition to query/duplicate/assign-primary rights:
   the Secondary Logon launch path otherwise returns access denied (`0x80070005`).
   Reproduced with a suspended `cmd.exe` child independently of any running GUI;
   adjustment rights fix it while preserving desktop SID and unelevated status.

Deliberate constraints:

- **Running elevated from Downloads (2026-10-03):** setup starts elevated
  from a folder its unelevated user controls, so nothing trusts that folder:
  - DLL planting: every static import outside the KnownDLLs is delay-loaded
    (`build.rs` `DELAY_LOADED` + `delayimp.lib`), the VC runtime is linked
    statically for this binary only (hybrid CRT: `libvcruntime`/`libcmt`
    static, UCRT through the KnownDLL `ucrtbase.dll`), and `main` calls
    `SetDefaultDllDirectories(LOAD_LIBRARY_SEARCH_SYSTEM32)` before anything
    can trigger a delay-load. `tests/import_table.rs` fails when a new
    non-KnownDLL static import appears.
  - The running image is pinned once at startup (`win::SetupImage`): opened
    through the name the image mapping reports (which follows renames),
    re-verified against it, and held with write/delete sharing denied. The
    payload, the uninstaller copy and the wizard preflight read through that
    handle, so renaming the running exe after UAC and planting another file
    at its path changes nothing. On non-NTFS volumes a name mismatch makes
    setup refuse.
  - Uninstall moves itself out of the tree by handle to a GUID-named file in
    `%ProgramFiles%` (admin-only, same volume) and rolls back the same way,
    so only that exact file can ever return into the install tree.
    `is_within` compares whole path components (`TaskManX` is not inside
    `TaskMan`).
- **Elevation:** `taskman-setup` carries a `requireAdministrator` manifest
  (attached per-bin in `build.rs`, so `taskman-payload` stays unelevated).
  One UAC prompt at start; the wizard runs elevated the whole time. A launch
  from a non-elevated console fails with os error 740
  (`ERROR_ELEVATION_REQUIRED`) instead of prompting — silent/automated
  installs must therefore run from an already elevated context (which is what
  deployment systems do anyway). `taskman-setup.exe` is `test = false` for the
  same reason.
- **Desktop identity:** `user.rs` captures the shell token for broker SID,
  per-user shortcut folders, and optional launch. Credential-based UAC must not
  enroll the administrator instead of the desktop user. Only setup's existing
  debug privilege is enabled to open another account's shell token; no global
  debugger settings change. Without a shell, the calling account owns enrollment
  and shortcuts, and elevated post-install launch is refused. A launch failure
  after installation reports the failure without rolling back installed files.
- **Install directory is fixed** at `%ProgramFiles%\TaskMan` and `/D=` is
  rejected with an explanation: the broker pins the installed GUI/service
  path (see `core-service.md`), so a custom directory would break the service
  contract. `install_dir()` in `win.rs` must stay identical to the helper's.
- **Uninstall** first clears only an owned IFEO replacement naming the installed
  GUI. Other products and portable TaskMan registrations remain; unreadable or
  malformed registrations fail before deletion. It waits for SCM Stopped even
  before using an older helper, then removes the service through the installed helper
  (`--core-service=uninstall`; a `windows-service`-based delete is the
  fallback when `taskman.exe` is already gone), then shortcuts,
  `%ProgramData%\TaskMan` (best-effort) and the install tree. Per-user
  settings are preserved (they live in the user profile). Because the ARP
  entry runs the uninstaller FROM the install tree, a running image cannot
  delete itself: the uninstaller renames itself out of the tree first (a
  running image can be renamed, not deleted) and schedules its own removal at
  reboot. The ARP key is removed last. File-removal failure attempts to restore
  the relocated uninstaller so the retained ARP command can retry; restoration
  and cleanup-scheduling failures are reported rather than silently discarded.
- **Upgrade** is just install again: the helper stops the old service
  generation before replacing files; settings survive because they never live
  in the install tree.
- **Silent/CLI:** `/S`, `--uninstall`, `--no-start-menu`, `--desktop`,
  `--launch`, `--no-service`, `--dry-run`, `--help`. Unknown flags fail
  loudly. Progress is written to `%SystemRoot%\Logs\TaskMan\setup.log`,
  best-effort like the service log. That folder is created with an
  admin-only DACL, and a reparse-point folder or log file is refused rather
  than followed. Deliberately NOT `%TEMP%` (the user's own for consent
  elevation) or `%ProgramData%\TaskMan\logs`: Users can pre-create folders
  in ProgramData before the first install (a junction there turns the
  elevated append into a write primitive), and a foreign `setup.log` in the
  service's log folder makes the service disable its own file logging
  (`verify_owned_log_entries`). Install deletes that legacy file — by handle,
  only at its exact resolved path — before the service starts. Unelevated
  `--help`/`--dry-run` runs need `__COMPAT_LAYER=RunAsInvoker`.

## Theming and wizard structure

The wizard uses `tm-ui` (moved out of `tm-app/src/theme.rs` + `fonts.rs`), so
palette, text-weight tuning and fonts are literally the app's: proper
Windows 11 dark and light mode via `theme::apply_startup`
(`ThemePreference::System`). Renderer: eframe `software` only — short-lived
window, no GPU dependency, and the only backend that does sub-pixel text.
The product icon is shared with the app (`app.res` is linked into
`taskman-setup.exe`, `icon_64.raw` paints the header logo and the window
icon).

Layout mirrors the green-curve setup: header band (product icon + name +
accent-colored version), content area (`pal.window_bg`), footer button row
(Back / Next / Cancel, right-aligned). The license review page shows the
repository `LICENSE` and gates Next behind "I accept the terms of the MIT
license".

The header and footer MUST stay `Panel::top` / `Panel::bottom` chrome: the
first version stacked them vertically with a content `ScrollArea
::auto_shrink(false)`, which consumes the entire remaining height and pushed
the footer out of the clip rect (invisible buttons).

The wizard is resizable. X/Alt+F4 receive `CancelClose` while work runs, matching
the disabled Cancel button. Worker-start errors and failed running steps are
surfaced; wizard and silent progress both reach the setup log.

## Testing and verification

- `cargo test -p tm-installer`: payload round trip, corruption/traversal
  rejection, `embed`/`Archive::open`/`verify_all` filesystem round trips, CLI
  parsing, step-plan shape (including "service install is in the default
  plan" and "payload verification precedes touching the machine").
- Wizard layout regression tests drive the real `draw()` through a windowless
  `egui::Context::run_ui` and assert: every footer button lies fully inside
  the viewport on every page (the first wizard version pushed the row out of
  the window), Next is disabled until the license is accepted, and a
  payload-less exe is refused up front. `eframe::App::ui` is a thin wrapper
  around `draw()` exactly so the tests exercise the shipped layout. They also
  walk the painted `Shape::Text` galleys (`painted_texts`/`collect_texts`) to
  pin spacing that widget responses can't express: body content stays inset
  from both window edges (the content panel's `Frame` inner margin must match
  the header/footer bands' 18px), each option's description is indented under
  its checkbox label, and the Done page offers only "Close" — the "run Task
  Manager when setup completes" Options checkbox is the single launch prompt
  (it drives `install.rs`'s post-install launch), never a second button.
- `taskman-payload verify <setup.exe>` re-parses a finished artifact and
  hash-checks every embedded entry. `build.py` embeds into
  `target/<profile>/<name>.partial`, verifies that, and only then moves it
  into `dist/` with one `os.replace`; any failure removes the partial file
  AND an existing `dist/` setup of the same name, so neither a broken nor a
  stale artifact can pass as this run's output
  (`tools/tests/test_build_setup.py`; `--check` runs every
  `tools/tests/test_*.py`).
- `win.rs` tests stage renames and substitutions against a private mapped
  copy of a system DLL: a file substituted at the launch path is not the
  image, relocation moves only the pinned image and never overwrites, setup
  log appends never follow a junction, and the legacy log is deleted only at
  its exact path.
- The ignored `user::tests::desktop_launch_creates_an_unelevated_process_for_the_shell_user`
  regression requires an elevated test harness and an interactive shell. It uses
  the production launch path to create a suspended system `cmd.exe`, never runs
  commands, terminates it, and checks its token's SID and elevation. It reproduced
  the launch access-denied error before the token-rights fix and passed afterward.
- `taskman-setup.exe` cannot run as an unelevated test harness (the manifest
  makes Windows return os error 740), so the bin target is `test = false`;
  the behavior beyond the unit tests is exercised via `--dry-run` and a real
  install/uninstall pass on a Windows machine.
- GUI appearance cannot be verified headlessly beyond the layout invariants
  above; confirm dark/light manually.

## Open questions / accepted limits

- Regression tests cover service opt-out stop/remove-before-copy and failure
  short-circuiting, helper ownership of service installs, selective IFEO cleanup,
  native-close rejection during work, failed-step propagation, and refusal of
  elevated launch tokens. A read-only token/folder smoke runs when a shell
  exists; it does not establish credential-based UAC behavior.
- Disposable-VM checks remain: a standard user supplying another account's UAC
  credentials, launched process token/environment, no-service upgrade with a
  running/stopped broker, full uninstall with IFEO enabled, locked-file retry,
  staging ACL readback, and forced-close handling. Automated tests do not mutate
  the developer machine's installation, registry or SCM.

- Wizard strings are English-only for now; the app itself is DE/EN
  (i18n follow-up candidate).
- No custom install directory (`/D=`), by the security constraint above.
- No code signing; same posture as the rest of the release artifacts.
