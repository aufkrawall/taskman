# Security Hardening Review: TaskMan Core Service

## Evidence Basis

I inspected the current action, startup, settings, and application ownership
boundaries recorded in [`context.md`](context.md). The source shows a GUI that
samples locally and invokes Windows controls directly, with UAC helpers for the
few machine-wide writes. The user has explicitly selected an always-running
service so the normal GUI can stay unelevated.

This is a source-derived architecture review, not a vulnerability scan. Its
original evidence collection records the pre-service boundary; the selected
design has now been implemented and the implementation plan records the final
source/diff evidence separately so the architectural baseline remains auditable.

## Constraints

- Windows 11 is the primary host; the other platform builds must remain valid.
- Normal GUI startup must not require UAC and must remain fast.
- The service must never run from a user-writable build or download directory.
- The service must not turn arbitrary GUI input into SYSTEM file writes or
  arbitrary command execution.
- Exact process identity, bounded work, fail-closed authentication, and clean
  uninstall/fallback behavior are non-negotiable.
- Full SYSTEM telemetry is intentionally kept out of the broker protocol.

## Opportunity Portfolio

| Opportunity | Evidence | Options | Recommendation | Proposal |
| --- | --- | --- | --- | --- |
| Put privileged controls behind one owned boundary | Current direct GUI action and elevation paths (`E003`–`E006`) | Elevated UI; protected broker; privileged sampler | Implement the protected broker now; retain local unelevated telemetry | [Privileged action boundary](proposals/privileged-action-boundary.md) |

## Recommendation Summary

I recommend the protected broker because it confines long-lived privilege to a
small allowlisted component while preserving the GUI's existing local telemetry
path. The service must authenticate the kernel-reported client process, run only
from protected Program Files storage, keep its manifest under a SYSTEM/Admin
DACL, reject remote pipe clients, cap every frame, and require PID plus creation
time for process mutations. A scheduled task is reasonable for occasional
elevation but leaves the entire GUI and its broad input surface elevated; a
SYSTEM sampler would add a new cross-user confidentiality problem.

The selected service still grants a durable privileged capability to one
authorized Windows account. We cannot cryptographically distinguish benign
TaskMan UI actions from code injected into that same authorized GUI process.
Protected binaries, narrow commands, exact identities, and no generic
command/file primitives materially reduce that residual risk without claiming
to eliminate same-user process injection.

## Next Decisions

The protected broker option is implemented, including stop-and-wait in-place
upgrade. The remaining decisions are operational: code-signing policy before
public distribution, whether multi-user authorization is ever needed, and the
disposable-VM install/ACL/recovery validation matrix.

## Implementation status

The implemented source is committed at
`fefaeb9376428128f7bee952f957c10559eb9813`; its final evidence digest is
`5803bebd8726401a016092fdb3846f2b75866d9058885e60ffae87f1fc4141d6`.
The automated workspace and release gates passed without launching the GUI,
installing the service, or changing machine ACLs. See
[`implementation/protected-action-broker.md`](implementation/protected-action-broker.md)
for the acceptance result and the boundaries that still require disposable-VM
validation.
