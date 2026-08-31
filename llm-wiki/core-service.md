# Protected Core Service

Last cross-checked: 2026-08-31

Primary sources:

- `crates/tm-platform/src/win/core_service.rs`
- `crates/tm-platform/src/win/process_ops.rs`
- `crates/tm-service/src/main.rs`
- `crates/tm-app/src/main.rs`
- `build.py`

## Boundary

TaskMan uses a split-process design on Windows:

- `taskman.exe` remains an ordinary interactive process. It owns sampling,
  settings, rendering, dialogs, tray state, user-selected output paths, and
  module inventory.
- `taskman-service.exe` is a delayed-auto SCM service running as LocalSystem.
  It owns only a small allowlist of privileged control operations. It does not
  render, parse arbitrary command lines, collect cross-user telemetry, choose
  output paths, or create dumps.
- If the service is absent or genuinely unavailable, the GUI tries the same
  identity-checked operation under its own token. An explicit broker rejection
  never falls back, because that would turn a safety decision into permission.

The service exists for reliable protected actions without elevating the whole
GUI. Task Scheduler remains suitable for an occasional trusted launch, but is
not the primary boundary: it would still elevate the complete GUI/parser and
does not provide the service's narrow request schema or readiness/recovery
contract.

## IPC and authorization

- Protocol v1 uses `\\.\pipe\Taskman.Core.v1` with fixed 12-byte framed JSON.
  Requests and responses are independently capped at 64 KiB; unknown request
  fields are rejected.
- The pipe rejects remote clients and uses first-instance creation to prevent
  pre-creation/squatting. Its protected DACL denies Network, grants full access
  to System/Administrators, and grants the one installing user
  `FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE`
  (`USER_PIPE_ACCESS`). npfs silently requires FILE_READ_ATTRIBUTES beyond the
  requested data rights for pipe client ends, so an ACE with only the data
  bits denies every non-elevated client — elevated clients only worked
  through the generic Administrators ACE. Do not "simplify" either side of
  that mask/request pair.
- The broker gets the kernel-reported client PID, resolves its executable, and
  requires the protected installed GUI path. The client likewise gets the
  kernel-reported server PID and requires the protected installed service
  path. Resolving the server image needs PROCESS_QUERY_LIMITED_INFORMATION on
  a LocalSystem process, which non-elevated GUI tokens are denied, so the
  client falls back to the SCM view: the pipe server PID must be the PID the
  SCM reports for `TaskmanCore` and the configured image (quotes stripped;
  windows-service keeps them) must be the protected path.
  `live_service_identity_verifies_without_elevation` covers this against a
  running service and self-skips when the pipe is unreachable.
- A protected manifest pins protocol/schema, authorized user SID, exact GUI and
  service paths, and SHA-256 hashes. The service validates it before listening.
- Two workers, a bounded queue of 16, and a matching 19-instance pipe cap keep
  request load finite. Queue/auth refusals return framed errors instead of
  looking like a missing service; repeated rejection logging is rate-limited.

Every process-changing broker request needs a positive sampled creation time.
The service rejects PID 0–4, itself, the requesting GUI, and critical Windows
processes. The action handle rechecks the exact PID+creation-time identity and
critical state before use. Tree termination refuses descendants whose creation
identity could not be captured; it never falls back to an unverified child PID.

Allowlisted operations are process/tree termination, suspend/resume, priority,
affinity, efficiency mode, UAC virtualization, guarded module unload, service
control, user-session control, and Task Manager replacement integration.
There is deliberately no generic Win32 call, shell command, file write, dump
path, or telemetry endpoint.

## Filesystem and SCM installation

Installation is the only UAC-requiring step and is handled by a short-lived
headless helper before GUI startup. The helper uses memory/console-only early
logging, never opens the interactive user's file log with an elevated token,
and does not mutate per-user redirect state (important for over-the-shoulder
UAC credentials).

- `%ProgramFiles%\TaskMan` has a protected inherited DACL: System and
  Administrators full control; Built-in Users read/execute only.
- `%ProgramData%\TaskMan` has a protected DACL granting only System and
  Administrators full control, with protected Administrator owner/group. The
  installer/service pins each directory against conflicting write/delete
  handles while assigning owner, group, and DACL. The manifest and service
  logs live here. Log startup accepts only TaskMan's exact daily service-log
  filename shape and rejects reparse/nested/non-file entries. Each existing
  file is likewise pinned and checked by handle, rejected if hard-linked, then
  assigned the complete protected security descriptor. At most 14 daily files
  are retained.
- Existing/install-source reparse points are rejected. Source binaries are
  opened without write/delete sharing and hashed through that pinned handle.
  Existing installed binaries are also pinned, rejected if hard-linked, and
  fully secured before their hash is trusted. Replacement uses synchronized/
  write-through staging files, protected security, atomic move, and a final
  destination hash check.
- Upgrade stops the old service and waits on its process handle before pinning
  retained logs or replacing binaries, so the old appender cannot conflict
  with security repair. The manifest is replaced atomically only after both
  protected binaries are ready.
- SCM configuration is own-process, automatic delayed start, LocalSystem,
  unrestricted service SID, required privilege list limited to
  `SeDebugPrivilege`, and restart failure actions at 5/15/60 seconds.
- The service reports `RUNNING` only after manifest validation, ACL creation,
  workers, and the first listening pipe are ready.

Successful installation writes a per-user HKCU marker only after the elevated
helper has submitted the SCM start successfully. Readiness is reported
independently by SCM only after the broker is listening. A later matching
portable/package launch redirects before any window is created to the
protected Program Files GUI and safely forwards arguments. A different package
hash stays in place so it can repair/upgrade the protected generation. An
already elevated launch does not redirect, so elevation is never silently
preserved. Existing autostart and owned Task Manager-replacement registrations
are retargeted to the protected GUI during migration.

Uninstall stops and deletes the SCM registration; the original interactive GUI
then clears its own per-user marker. Protected binaries/data intentionally remain: deleting a running GUI
or security-sensitive tree in place is not required to remove privileged
capability and would make rollback less reliable.

## Operational behavior

- The GUI and service use above-normal process/control-thread priority. High or
  realtime priority is intentionally avoided because it can starve input,
  storage, or recovery work during overload.
- Broker client authorization is image-PATH based: any session running outside
  the protected install location (dev tree, portable copy) is rejected with
  "client is not the protected TaskMan GUI" even when the service is healthy.
  `service_state` classifies this as `CoreServiceState::ForeignClient` instead
  of a generic auth failure, and the Advanced settings surface offers a
  "switch to installed copy" action
  (`core_service::relaunch_into_installed_gui`) that spawns the installed GUI
  with `--single-instance-handoff` and closes the foreign session. A successful
  repair/upgrade from a foreign session therefore needs this switch (or an app
  restart, which the startup redirect handles) before the service is usable;
  repair itself does not fail. Elevated sessions refuse the switch so
  elevation is never silently preserved.
- GUI actions run in two independent bounded executor lanes (32 each), so one
  wedged OS API cannot stall every action and overload is surfaced explicitly.
  Platform calls are never run inline on the renderer thread if worker startup
  or bounded dispatch fails.
- SCM stop and the synchronous accept loop coordinate through a condition
  variable: the stop thread waits eventfully for the current listener before
  connecting to wake it, so a stop racing listener creation cannot strand
  service shutdown.
- Service-mode panics abort the whole broker process rather than silently
  losing one worker; SCM then applies the configured clean-image recovery.
- GUI startup is lazy and single-instance. Explicit elevation performs a
  bounded mutex-ownership handoff so the old instance cannot reject or strand
  the replacement, and programmatic exit bypasses close-to-tray. Tray objects
  are created only when needed; close-to-tray and per-user autostart are
  optional settings. Autostart migration only rewrites command lines TaskMan
  can prove it generated; conflicting third-party values remain untouched.
- Renderer policy is independent: Windows WGPU is D3D12-only and low-power by
  default, with Glow as the compatibility fallback. Vulkan is not shipped in
  the Windows WGPU feature set.

## Validation and manual checks

Headless checks:

- `cargo test -p tm-platform --lib`
- `target/release/taskman-service.exe --selfcheck`
- final `python build.py --check`, then `python build.py --host-only`

The self-check does not install/start the service or mutate ACL/registry/SCM
state. Installation, UAC, tray behavior, visual tree interaction, ACL readback,
service recovery, multi-user denial, and upgrade/rollback need a disposable
Windows VM/manual session. Do not exercise those on an active desktop when the
user requested window-free testing.

The final 2026-08-31 headless run passed all 166 workspace tests and returned
protocol v1 with 65,536-byte request/response caps from the release service
self-check. The packaged service is 1,378,816 bytes.

## Open questions / stale risk

- Production signing and an installer/update chain should verify publisher
  signatures in addition to protected paths/hashes.
- Authorization intentionally binds one installing user SID. A future
  multi-user design needs explicit per-user enrollment/revocation, not a broad
  `Authenticated Users` pipe ACE.
- Pipe workers use bounded synchronous I/O. An authenticated client can occupy
  both workers by connecting and stalling, although it cannot create unbounded
  threads, buffers, or queues. Overlapped I/O with per-request deadlines is a
  candidate after installed-service fault injection establishes safe timeout
  values for slow control operations.
- Processor-group-aware affinity (>64 logical CPUs) needs a versioned protocol
  extension; the current `u64` mask must not silently truncate.
