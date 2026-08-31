# Implementation Plan: Protected Action-Only Service

## Selected Design And Constraints

The user selected the action-only LocalSystem broker. The GUI remains
unelevated, keeps its local per-user collector and settings, and uses the
service only for an explicit set of protected process/service operations.
Generic elevated command launch, arbitrary privileged output paths, shell
navigation, and per-user autostart remain outside the protocol.

## Source Revision And Drift Check

The design was anchored to evidence digest
`6588135e9f18606e014663a5fe5779209ae9d9575cbe4a7c48c19fba413a9b7a`
at revision `13696d08f17e03754cb842c74c52ceb59c06991a`. Source drift was already
present because related GUI work was in progress. Final implementation evidence
is recorded separately after the source commit so the original architecture
baseline remains reproducible.

## Affected Components

- Workspace/build packaging: `Cargo.toml`, `build.py`, new `crates/tm-service`.
- Shared action/protocol types: `crates/tm-platform/src/actions.rs` and a new
  Windows broker module.
- Windows installation and service lifecycle: new files under
  `crates/tm-platform/src/win/`.
- GUI action selection and service controls: `crates/tm-app/src/app.rs`,
  `crates/tm-app/src/app_ui.rs`, `crates/tm-app/src/main.rs`.
- Settings/i18n and durable architecture documentation.

## Ordered Work Packages

1. Finish the already-started GUI identity, sort, tree, saved-policy, status,
   module, tray, and autostart changes so protocol work builds on stable APIs.
2. Define versioned request/reply enums and a strict length-prefixed frame with
   request and response caps. Add parser and round-trip tests.
3. Add a fixed-size service worker pool. Create a local-only named pipe with an
   explicit DACL, use the kernel-reported client PID, resolve its image path,
   and reject anything except the protected installed GUI.
4. Dispatch only exact process controls (including the narrow UAC-token toggle),
   guarded module unload, service/session control, and Task Manager replacement
   state/change. Keep module inventory, dumps, shell launch, and output paths
   local. Preserve final PID/creation-time checks.
5. Add known-folder discovery, protected Program Files and ProgramData DACLs,
   pinned source copying, SHA-256 manifest generation, service/self/client hash
   validation, and fail-closed startup.
6. Register an absolute own-process LocalSystem service with delayed automatic
   start, a service SID, minimal required privileges, bounded restart actions,
   description, status readback, start, and removal.
7. Add a brokered Windows action wrapper with local fallback only when the
   broker is absent/unavailable—not when it explicitly denies a request.
8. Add GUI service status/install/remove controls. Installation/removal is an
   explicit UAC helper path and never happens during tests.
9. Package both executables and add headless service/protocol self-checks.

## Compatibility And Migration

The service is additive and Windows-only. Linux and macOS keep their current
collectors/actions and compile a stub service binary only if the workspace
requires it. Existing settings continue to load. The GUI starts and operates
without the service, allowing gradual opt-in and clean rollback.

## Tactical Protections During Migration

- Keep exact process identity verification in the final Win32 implementation,
  not only in GUI or protocol code.
- Keep dump creation local/UAC-gated until a safe user-created-handle transfer
  is designed.
- Keep elevated task launch outside the service.
- Refuse service execution from any path other than the protected manifest path.
- Do not add a development bypass to release service authentication.

## Tests And Security Validation

- Protocol unit tests: good frame, truncated header/body, wrong magic/version,
  wrong kind, oversized payload/reply, unknown operation/fields, and exact
  identity requirement.
- Authorization unit tests for SID/SDDL generation, service-name filtering,
  protected GUI argument quoting, and allowlist framing. Installed path/hash/
  ACL behavior remains a disposable-VM validation item.
- Platform tests for exact creation-time rejection on each brokered process
  mutation.
- Headless service self-check must create no window, install nothing, and make
  no machine-wide change.
- Workspace fmt/clippy/tests and a release host build are the final gate.
- Manual VM validation remains explicitly separate because this active desktop
  must not receive UAC prompts, service installation, or GUI windows during the
  automated work session.

## Performance And Resource Benchmarks

On a disposable installed build, compare direct and brokered runs for 100 ping,
priority-query-equivalent, and affinity-read calls. Record median/p95/p99 and
require p95 broker overhead below 10 ms. Measure idle private working set after
five minutes and verify the worker queue and frame buffers stay at their fixed
caps under a stalled or flooding client.

## Rollout And Rollback

Ship the service opt-in first. Installation copies both version-matched binaries
to protected storage, writes the protected manifest, registers/configures SCM,
then submits an SCM start request. The service reports `RUNNING` only after the
manifest, ACL, workers, and listening pipe are ready; the GUI polls this state
without blocking its renderer. Any validation failure leaves the GUI on local
actions and reports a diagnostic. Removal stops/deletes the SCM registration;
protected files may remain because deleting the currently running installed GUI
is unsafe. Rollback requires no settings migration.

## Acceptance Criteria

- Normal GUI launch has no UAC prompt and works with the service absent.
- Installed binaries are read/execute-only to normal Users; the broker manifest
  is writable only by SYSTEM/Administrators.
- The service rejects wrong paths, wrong hashes, wrong users, remote clients,
  protocol mismatches, unknown operations, oversized frames, and recycled PIDs.
- No request provides generic SYSTEM command execution or arbitrary SYSTEM file
  output.
- Worker/thread/memory growth is bounded, SCM restart is configured, and broker
  failure cannot freeze the GUI.
- Final documentation distinguishes automated source/test verification from
  manual installed-service validation that was not run on the active desktop.

## Open Decisions

- Authenticode publisher/signing pipeline for public packages.
- Multi-user authorization beyond the installing account.
- A future safe dump mechanism based on a user-created file handle rather than
  a service-trusted path.

## Final implementation evidence

The implementation is committed at
`fefaeb9376428128f7bee952f957c10559eb9813`. Its 16-file evidence collection is
recorded in [`../context.md`](../context.md) with digest
`5803bebd8726401a016092fdb3846f2b75866d9058885e60ffae87f1fc4141d6`;
the original source drift is resolved for this implementation.

Automated verification completed with `python build.py --check`, followed by
`python build.py --host-only`. All 166 workspace tests passed, Clippy passed
with warnings denied, release packaging contains both executables, and the
service self-check completed without installing a service or opening a window.

The implementation meets the source-verifiable acceptance criteria. Installed
ACL/SCM recovery, hostile-client, reparse/hardlink, upgrade, and uninstall
behavior still requires the documented disposable-VM matrix. Authenticode and
deadline-based overlapped service I/O remain explicit follow-up hardening work,
not silently accepted implementation claims.
