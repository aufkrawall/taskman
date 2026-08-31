# Security Hardening Proposal: Own the Privileged Action Boundary

## Decision

Choose how TaskMan should provide reliable process and service controls while
keeping its normal GUI unelevated. The user selected Option 2, the protected
action broker, for implementation in this work.

## Executive Recommendation

There are three credible designs. **Option 1: Elevated UI** keeps the existing
direct API shape and uses UAC or a scheduled task. **Option 2: Protected action
broker** installs an always-running LocalSystem service with a small,
authenticated command protocol while telemetry stays local to the GUI.
**Option 3: Privileged sampler and broker** moves both collection and controls
into the service.

I recommend Option 2 under the current requirements. It removes routine GUI
elevation without moving renderer, settings, search, shell, and file-dialog
inputs into the administrative boundary. Option 3 becomes preferable only if
TaskMan later needs a carefully specified, multi-user privileged telemetry
product and can fund per-field authorization and redaction.

## Evidence

I inspected the source files listed below. The most important observation is
that the current `PlatformActions` trait mixes harmless shell operations,
read-only enumeration, and destructive controls in one object owned by the GUI.
That makes a direct “run everything elevated” solution mechanically easy but
structurally broad.

| Evidence | Finding or document | What it establishes |
| --- | --- | --- |
| `E001` | Workspace manifest (`Cargo.toml`) | The product is split into core, platform, and GUI crates; no service executable currently exists. |
| `E002` | Per-user settings (`crates/tm-core/src/settings.rs`) | Preferences and saved policies belong to the interactive user, not to a machine service. |
| `E003` | Platform action surface (`crates/tm-platform/src/actions.rs`) | Process termination, priority, affinity, module unload, service control, arbitrary task launch, and file-oriented operations currently share one trait. |
| `E004` | Windows dispatch (`crates/tm-platform/src/win/mod.rs`) | The GUI calls Win32 operations directly and uses short-lived elevation for machine-wide integration. |
| `E005` | GUI startup (`crates/tm-app/src/main.rs`) | Persisted “start elevated” can re-exec the entire renderer/UI process through UAC. |
| `E006` | App ownership (`crates/tm-app/src/app.rs`) | The GUI owns both its sampling engine and action executor; collection can remain outside the service. |

**Observed:** no long-lived privileged service or IPC boundary exists in the
inventoried source. The whole GUI can be elevated, and a helper path exists for
one narrow machine-wide change.

**Inferred:** moving the existing trait wholesale into a service would expose
dangerous generic primitives. In particular, “run this command elevated” and
“write a dump to this path” would let any accepted client turn LocalSystem into
an arbitrary execution or file-write oracle. It is therefore not enough to add
a pipe; the protocol must own a smaller semantic allowlist.

## Current Design And Failure Mode

The current design is not evidence of an exploitable defect by itself. Its
failure mode is architectural breadth under the new requirement: if we use a
scheduled task or automatic UAC to make every action reliable, all GUI parsing,
renderer, clipboard, shell, configuration, and file-dialog behavior executes
with administrator rights. If we instead put the same broad action interface
behind unauthenticated IPC, a normal same-machine process could ask a SYSTEM
service to perform actions it could not perform itself.

The structural condition is mixed authority. Read-only per-user presentation,
machine-wide destructive controls, and arbitrary launch/file operations do not
share the same least-privilege contract, yet the current object boundary groups
them together. A service design must separate them before privilege is added.

## Desired Invariants

- The GUI normally runs unelevated and retains per-user settings and telemetry.
- The service starts only through SCM from an administrator-protected absolute
  path and fails closed if its protected manifest or binary identity is wrong.
- Only the installing/authorized Windows account, administrators, and SYSTEM
  can connect; remote pipe clients are rejected at creation time.
- Each request is bounded, versioned, parsed once, and mapped to a fixed command.
- The service derives the client PID from the pipe, verifies its installed GUI
  image path, and never trusts a caller-supplied PID as client identity.
- Process mutations require target PID plus expected creation time; recycled
  PIDs fail closed.
- No broker command accepts an arbitrary executable command line.
- No broker command accepts an arbitrary privileged output path.
- Worker count, queue depth, frame size, and per-operation work are bounded.
- A stopped or absent service degrades to existing local behavior rather than
  preventing TaskMan from starting.

## Constraints And Non-Goals

The binaries are not currently Authenticode-signed, so publisher-signature
verification cannot be the first client-authentication control. Protected path
plus install-time hash verification is the selected interim mechanism. This
does not claim resistance to code injection into an already authorized GUI
process. Kernel drivers, Protected Process Light, remote administration,
cross-machine IPC, and exporting SYSTEM-level command lines are non-goals.

## Before Architecture

[`privileged-action-boundary-before.mmd`](../diagrams/privileged-action-boundary-before.mmd)
shows the present source-derived design. The important edge is that elevation
widens the boundary around the entire GUI rather than around a semantic command.

| Change | Before | After | Security consequence | Cost |
| --- | --- | --- | --- | --- |
| Privileged owner | GUI or short helper | Undecided | Current elevation can encompass broad UI input | No service operational burden today |
| Filesystem trust | Current launch location | Undecided | A future service could accidentally execute from writable storage | Packaging is simple today |
| IPC authorization | None | Undecided | No broker attack surface yet | No protocol/version maintenance |

## Options

### Option 1: Elevated UI

This baseline keeps all direct calls and either launches TaskMan through UAC or
registers a highest-run-level scheduled task. Its strongest case is simplicity:
there is no IPC serialization, second process, SCM lifecycle, or split failure
mode. It also preserves every existing action, including file dumps and elevated
task launch, without designing new protocols.

What gives me pause is the size of the elevated input surface. Renderer and GPU
drivers, window messages, search text, clipboard content, config parsing, URLs,
shell launch, and file dialogs all remain in the same privileged process. A
scheduled task can avoid repeated prompts after an administrator registers it,
but it does not shrink this boundary. Rollback is easy—remove the task and keep
normal UAC—but the security architecture remains broad.

The after view is
[`privileged-action-boundary-elevated-ui-after.mmd`](../diagrams/privileged-action-boundary-elevated-ui-after.mmd).

| Change | Before | After | Security consequence | Cost |
| --- | --- | --- | --- | --- |
| Elevation timing | Optional/manual | Startup or scheduled task | Reliable privileged actions, but all GUI inputs are elevated | Minimal code |
| Authorization | UAC per helper/relaunch | Task registration plus task ACL | Registration must be protected; task still starts broad code | Scheduler lifecycle |
| Failure isolation | One GUI process | One elevated GUI process | GUI crash removes all capability | No extra service memory |

Performance and memory remain closest to today because there is no IPC or extra
resident process. Reliability improves for access-denied operations but remains
tied to the GUI lifecycle. Operability is attractive for a personal build, yet
task ACL drift and opaque scheduler state complicate support. Migration and
rollback are low-cost.

### Option 2: Protected action broker

This option installs a small service and keeps the existing collector in the
interactive session. The GUI attempts a bounded request for only those actions
that benefit from privilege—exact process controls and guarded module unload,
service control, and the Task Manager replacement write. Harmless shell and
per-user operations, module inventory, and dumps remain local. Generic elevated
task execution and arbitrary dump paths never enter the protocol.

The service is useful only if installation is part of the boundary. The
installer copies a dedicated service binary and the GUI into Program Files,
applies a protected DACL that gives Users read/execute but not write, places the
broker manifest under ProgramData with SYSTEM/Administrators access only, and
registers an absolute quoted binary path with SCM. The manifest pins protocol
version, authorized user SID, GUI path/hash, and service path/hash. At startup,
the service verifies those values before creating the pipe.

For every connection, Windows supplies the client PID. The service resolves the
actual process image and compares it with the protected installed GUI path. The
pipe itself rejects remote clients and has a DACL for SYSTEM, administrators,
and the authorized SID. This layered check is valuable because a permissive
default named-pipe descriptor is not an authentication policy. Requests then
pass through a fixed allowlist and exact target-identity checks.

The residual risk is candidly important: a process owned by the authorized user
may be able to inject into that user's GUI and borrow its accepted connection.
Code signing and process mitigations can improve assurance later, but no shared
secret readable by an unelevated GUI solves same-user injection. The allowlist
therefore remains the primary damage limiter.

The after view is
[`privileged-action-boundary-protected-broker-after.mmd`](../diagrams/privileged-action-boundary-protected-broker-after.mmd).

| Change | Before | After | Security consequence | Cost |
| --- | --- | --- | --- | --- |
| Privilege lifetime | GUI/helper | Always-running small service | Broad GUI inputs leave the admin boundary | One resident process |
| IPC | None | Local-only framed pipe | New parser surface, constrained by DACL, PID/path checks, size caps, and allowlist | Serialization and one local hop |
| Files | Launch location may be writable | Protected Program Files and ProgramData | Blocks ordinary binary/manifest replacement | Elevated installer and ACL maintenance |
| Target identity | Some exact checks in platform code | Required for every process mutation | PID recycling is refused at final action boundary | Creation-time query per action |
| Recovery | Relaunch GUI/helper | SCM delayed auto-start plus bounded restart actions | Broker can recover independently | Service status/diagnostics needed |

The extra hop is likely small relative to Win32 control calls, but that is a
source-derived expectation rather than a measurement. We should benchmark 100
ping and affinity queries and require p95 local round-trip below 10 ms on the
development host. The service adds a few MiB of resident code/data; its protocol
does not retain snapshots. Reliability improves through failure isolation and
SCM restart, while bounded workers prevent client load from creating unbounded
threads. Migration is moderate because actions need explicit request mappings,
but rollback is clean: stop/remove the service and use current local behavior.

### Option 3: Privileged sampler and broker

This design also moves the sampling engine to LocalSystem and streams snapshots
to the GUI. Its strongest case is continuity: sampling survives GUI restarts,
and one service could serve multiple front ends. It could also observe protected
processes more completely.

Those benefits create the main security concern. A SYSTEM collector can see
other sessions and potentially sensitive command lines that an unelevated user
cannot. The service would need field-level authorization and redaction keyed to
the authenticated user's token and session, plus a policy for administrators,
multiple concurrent users, and cached data captured before authentication.
Snapshot serialization also makes the IPC and memory boundary far larger.

The after view is
[`privileged-action-boundary-system-sampler-after.mmd`](../diagrams/privileged-action-boundary-system-sampler-after.mmd).

| Change | Before | After | Security consequence | Cost |
| --- | --- | --- | --- | --- |
| Telemetry owner | User-session collector | SYSTEM service | Can improve coverage but creates cross-user disclosure paths | Filtering policy and tests |
| IPC payload | None | Large periodic snapshots | Much larger parser/copy surface | CPU, memory, latency, versioning |
| Lifetime | GUI-bound | Boot-to-shutdown | Continuous history possible | Always-on sampling overhead |
| Multi-user state | One GUI | Shared service caches | Requires explicit isolation | Complex cache ownership |

Performance would regress through continuous collection, serialization, and
copies; memory would grow with retained snapshots and per-client queues.
Reliability could improve for historical sampling but would gain cache and
backpressure failure modes. Migration is foundational and rollback requires a
dual collector path. I would choose it only with a concrete privileged-telemetry
requirement and a reviewed per-user data policy.

## Comparison

| Dimension | Option 1: Elevated UI | Option 2: Protected broker | Option 3: Privileged sampler |
| --- | --- | --- | --- |
| Security | Broad elevated input surface | Narrow privileged allowlist; same-user injection remains | Narrow controls but major telemetry confidentiality burden |
| Performance | No IPC hop | Small action-only local hop; benchmark needed | Continuous sampling and snapshot serialization |
| Memory | No extra process | Small resident service, no snapshot cache | Service caches plus client buffers |
| Reliability | Coupled to GUI | SCM recovery and GUI fallback | Independent sampling, more distributed state |
| Operability | UAC/task management | Installer, ACL, service status and logs | All broker costs plus schema/filter operations |
| Migration | Low | Moderate and incremental | High/foundational |

The table makes the central tradeoff visible: Option 2 pays operational
complexity to shrink privilege, whereas Option 3 pays additional confidentiality
and data-lifecycle complexity that the current product does not need.

## Recommendation

I recommend Option 2 and the user has selected it. Its strongest advantage is
not prompt avoidance; it is authority reduction. We can keep the mature local
collector, GUI settings, and renderer behavior while moving a finite set of
destructive operations behind one inspectable boundary.

Option 1 should win only if this remains an unpackaged developer utility where
service installation cannot be supported. Option 3 should win only if protected
telemetry becomes a stated product requirement and the project accepts a
separate privacy threat model. Authenticode signing would strengthen Option 2's
client validation but is not required to begin with protected path/hash pinning.

## Evidence Coverage And Residual Risk

| Evidence | Effect of selected option | Tactical protection still required |
| --- | --- | --- |
| `E001` — workspace has no service | Addressed by a dedicated service binary and packaging | Keep non-Windows stub builds valid |
| `E002` — settings are per-user | Addressed by keeping rules/preferences out of service state | Protect machine manifest separately |
| `E003` — mixed action trait | Mitigated by mapping only a narrow subset into the protocol | Never add generic command or arbitrary-path requests |
| `E004` — direct Windows actions | Addressed for allowlisted privileged operations | Preserve exact identity checks in final Win32 call |
| `E005` — whole-GUI auto elevation | Mitigated; service becomes preferred normal path | Retain manual elevation only as explicit fallback |
| `E006` — GUI owns sampler/actions | Addressed by splitting action transport while retaining sampler | Bound GUI and broker queues independently |

Residual risks include same-user injection into the authorized GUI, trusted
administrator replacement of protected files, mistakes in new unsafe Win32 IPC
or ACL code, denial of service by a local authorized account, and unsigned
binary provenance before distribution.

## Migration And Rollout

Introduce the protocol and brokered action wrapper first while preserving local
fallback. Add the service executable and headless protocol tests next. Then add
the elevated installer, protected paths, manifest validation, SCM recovery
configuration, and GUI status controls. Packaging must contain both binaries.
The service should remain opt-in until install/readback diagnostics pass.

Rollback stops and deletes the SCM registration but intentionally leaves the
administrator-protected files in place so an executing GUI is never deleted
under itself. TaskMan continues with local actions and explicit UAC helpers.

## Validation Plan

- Unit-test protocol version, frame length, unknown command, truncation, and
  oversized payload rejection without opening a GUI.
- Unit-test manifest validation, path equality, SDDL construction, and command
  allowlist mapping.
- Run the workspace quality gate and host release build.
- Run `taskman-service.exe --selfcheck` as a normal process; it must validate
  protocol logic without creating a service or window.
- In an explicitly approved manual follow-up, install on a disposable Windows
  VM, inspect file/service/pipe ACLs, attempt connections from the wrong user
  and wrong image, exercise PID-reuse rejection, stop/crash/restart the service,
  and uninstall. This work session will not install or start the service on the
  user's active desktop.
- Benchmark 100 local ping and affinity reads; target p95 below 10 ms and no
  unbounded allocation growth. Measure service private working set after idle.

## Implementation Work Packages

- Define a versioned action-only protocol with strict framing and serde limits.
- Add fixed-size broker workers and local-only authenticated named-pipe setup.
- Add installed GUI path/hash and authorized SID validation.
- Add Program Files/ProgramData discovery, protected DACL application, pinned
  file copy, SCM install/config/start, query, and uninstall helpers.
- Add brokered Windows action dispatch with local fallback.
- Add GUI service status/install/remove controls and remove automatic elevation
  as the recommended normal mode.
- Package the service binary, document operations, and retain headless checks.

## Open Questions

- Which Authenticode publisher and certificate lifecycle will be used for
  public releases?
- Should a future installer authorize more than one local user SID?
- Which disposable-VM matrix will gate installed ACL, recovery, upgrade, and
  rollback behavior before public distribution?
