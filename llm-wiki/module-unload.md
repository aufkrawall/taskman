# Module Unload

Last cross-checked: 2026-09-07

Primary sources:

- `crates/tm-app/src/tabs/modules.rs`
- `crates/tm-platform/src/actions.rs`
- `crates/tm-platform/src/win/process_ops.rs`
- `crates/tm-platform/src/win/core_service.rs`
- `crates/tm-platform/tests/integration.rs`

## Contract

Module unload is an explicitly destructive diagnostic action. Which module to attempt is the user's decision; TaskMan does not maintain a policy list of protected DLLs. Technical safety checks remain mandatory.

The Windows platform primitive performs **one** remote `FreeLibrary` request and then verifies whether the selected mapping remains. The Modules UI intentionally turns a confirmed unload into a **force unload** by chaining up to 64 of those one-release requests. Between every release it re-enumerates the module inventory and only continues while the same sampled process still has the same selected base address and path mapped. This preserves action-time identity validation for every release and avoids the former blind multi-call loop against a stale `HMODULE`.

A successful `FreeLibrary` only proves that one release request completed; it does not prove that the image was unmapped. The platform result is represented as `ModuleUnloadOutcome { still_mapped: Option<bool> }`: `Some(false)` means the exact selected mapping disappeared, `Some(true)` means it remains, and `None` means the post-action state could not be verified. Force mode stops immediately on an unverified refresh rather than issuing another release without fresh inventory.

A destructive unload requires a positive sampled process creation time. Neither the GUI-side broker wrapper nor the service may replace a missing snapshot identity with the creation time of whichever process currently owns that PID. The action handle verifies creation time before use, and ToolHelp enumeration is followed by another process-generation check.

The selected module is revalidated by exact base address plus case-insensitive path immediately before every remote call. The caller and target must have the same effective PE machine as reported by `IsWow64Process2`; unknown or cross-architecture cases fail closed. The address used for remote `FreeLibrary` is calculated from the module that actually owns the local export (Kernel32 exports can be forwarded), and the target owner must match that image's name, path, and image size before the relative offset is used.

`SeDebugPrivilege` is enabled best-effort when the current token already holds it. This grants no new privilege to an ordinary token, but makes elevated/local and LocalSystem execution reliable when process VM/thread rights require debug privilege.

A remote-thread timeout is an unknown-state result, not a clean failure: closing the thread handle does not stop a remote call that may still finish. The Modules dialog therefore re-enumerates after every attempt, including errors and timeouts. Manual refresh and unload are serialized so an older refresh cannot overwrite the unload's post-action inventory.

TaskMan deliberately does not use `NtUnmapViewOfSection` as a fallback for normal DLL modules. Raw unmapping would bypass the loader's dependency graph, TLS cleanup and detach notifications and can leave loader bookkeeping pointing at unmapped memory. Force mode remains loader-mediated: repeated, revalidated `FreeLibrary` requests only.

## Mixed service generations

Broker protocol v2 predates the current hardened primitive and older installed services may still contain the former bounded blind multi-release loop. Current GUIs append `MODULE_UNLOAD_SINGLE_RELEASE_MARKER`, a NUL-delimited capability marker, to the expected path sent with each unload request. NUL cannot occur in a Windows file name. A current backend strips the marker before normal validation; an older v2 service cannot match the marked string to its ToolHelp path and therefore rejects before its old unload implementation can call `FreeLibrary`.

Force mode is intentionally implemented by the GUI issuing multiple **marked single-release broker requests** with a fresh module enumeration between them. This means a new GUI can force-unload through the current service without reintroducing an unsafe multi-release broker primitive, while an older incompatible service still fails closed on the first marked request.

Current backends continue to accept unmarked paths so an older GUI talking to a newer service gets the one-release platform behavior. The marker is intentionally scoped to this destructive operation instead of bumping the complete broker protocol and disabling unrelated service controls during a mixed-generation upgrade.

The v2 `ModuleUnload` response remains backward-decodable. Older responses contain `still_mapped` and may contain a historical `released` count; current responses add verification state. The historical count is compatibility data only and is not used to infer the loader's private reference count.

## Validation

The Windows integration test explicitly loads `WTSAPI32.dll` three times in a spawned child through remote `LoadLibraryW`. It pins the **platform primitive** by proving that three explicit one-release calls are required: the first two leave the DLL mapped and the third removes it. The same test asserts that a PID-only unload is refused. The Modules UI force policy is layered above this primitive and repeatedly invokes it only after revalidation.

The repository quality gate is `python build.py --check`; it covers formatting, clippy, tests, and the vendored-fork gate. Live behavior against elevated/protected third-party targets still belongs in a disposable Windows test session because unloading arbitrary DLLs can crash or corrupt the target by design.
