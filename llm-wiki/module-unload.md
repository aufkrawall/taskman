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

One confirmation performs **one** remote `FreeLibrary` request. TaskMan does not own the target process's loader references and therefore must not drain an unknown reference count automatically. A successful `FreeLibrary` only proves that one release request completed; it does not prove that the image was unmapped. The module inventory is re-read and the result is represented as `ModuleUnloadOutcome { still_mapped: Option<bool> }`: `Some(false)` means the exact selected mapping disappeared, `Some(true)` means it remains, and `None` means the post-action state could not be verified.

A destructive unload requires a positive sampled process creation time. Neither the GUI-side broker wrapper nor the service may replace a missing snapshot identity with the creation time of whichever process currently owns that PID. The action handle verifies creation time before use, and ToolHelp enumeration is followed by another process-generation check.

The selected module is revalidated by exact base address plus case-insensitive path immediately before the remote call. The caller and target must have the same effective PE machine as reported by `IsWow64Process2`; unknown or cross-architecture cases fail closed. The address used for remote `FreeLibrary` is calculated from the module that actually owns the local export (Kernel32 exports can be forwarded), and the target owner must match that image's name, path, and image size before the relative offset is used.

`SeDebugPrivilege` is enabled best-effort when the current token already holds it. This grants no new privilege to an ordinary token, but makes elevated/local and LocalSystem execution reliable when process VM/thread rights require debug privilege.

A remote-thread timeout is an unknown-state result, not a clean failure: closing the thread handle does not stop a remote call that may still finish. The Modules dialog therefore re-enumerates after every attempt, including errors and timeouts. Manual refresh and unload are serialized so an older refresh cannot overwrite the unload's post-action inventory.

## Mixed service generations

Broker protocol v2 predates the one-release semantic and older installed services may still contain the former bounded multi-release loop. Current GUIs append `MODULE_UNLOAD_SINGLE_RELEASE_MARKER`, a NUL-delimited capability marker, to the expected path sent with an unload request. NUL cannot occur in a Windows file name. A current backend strips the marker before normal validation; an older v2 service cannot match the marked string to its ToolHelp path and therefore rejects before its old unload implementation can call `FreeLibrary`.

Current backends continue to accept unmarked paths so an older GUI talking to a newer service gets the safer one-release behavior. The marker is intentionally scoped to this destructive operation instead of bumping the complete broker protocol and disabling unrelated service controls during a mixed-generation upgrade.

The v2 `ModuleUnload` response remains backward-decodable. Older responses contain `still_mapped` and may contain a historical `released` count; current responses add verification state. The historical count is compatibility data only and is not used to infer or drain the loader's private reference count.

## Validation

The Windows integration test explicitly loads `WTSAPI32.dll` three times in a spawned child through remote `LoadLibraryW`. It then proves that three explicit unload requests are required: the first two leave the DLL mapped and the third removes it. The same test asserts that a PID-only unload is refused.

The repository quality gate is `python build.py --check`; it covers formatting, clippy, tests, and the vendored-fork gate. Live behavior against elevated/protected third-party targets still belongs in a disposable Windows test session because unloading arbitrary DLLs can crash or corrupt the target by design.
