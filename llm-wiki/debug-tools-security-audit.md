<!--
SPDX-License-Identifier: MIT
Copyright (c) 2026 aufkrawall
-->

## Windows debugging and binary analysis tools

- When analyzing crash dumps, use the correct symbol path that includes both the Microsoft symbol server AND the local PDB directory:
```
cdb -z crash.dmp -y "srv*;%USERPROFILE%\Programme\build\captureproject\installed\captureengine" -c ".ecxr; k; q"
```
The `srv*`-only path misses CE's local PDBs and produces incomplete stack traces.

- Installed Windows tools for `.dmp`, symbol, PE/COFF, Sysinternals, and media/capture analysis:

| Tool | Purpose | Installed/default path |
| --- | --- | --- |
| `cdb.exe` | Command-line `.dmp` debugging and stack inspection | `C:\Program Files\Windows Kits\10\Debuggers\x64\cdb.exe` |
| `windbg.exe` | Interactive `.dmp` debugging | `C:\Program Files\Windows Kits\10\Debuggers\x64\windbg.exe` |
| `WinDbgX.exe` | Interactive WinDbg Preview `.dmp` debugging | `%LOCALAPPDATA%\Microsoft\WindowsApps\WinDbgX.exe` |
| `dumpchk.exe` | Validate dump readability and basic dump metadata | `C:\Program Files\Windows Kits\10\Debuggers\x64\dumpchk.exe` |
| `symchk.exe` | Verify/download symbols for binaries and dumps | `C:\Program Files\Windows Kits\10\Debuggers\x64\symchk.exe` |
| `dbh.exe` | Inspect symbols and PDB contents | `C:\Program Files\Windows Kits\10\Debuggers\x64\dbh.exe` |
| `pdbcopy.exe` | Copy/strip PDBs for symbol handling | `C:\Program Files\Windows Kits\10\Debuggers\x64\pdbcopy.exe` |
| `symstore.exe` | Add/query files in a symbol store | `C:\Program Files\Windows Kits\10\Debuggers\x64\symstore.exe` |
| `gflags.exe` | Configure debug/runtime flags; use only with explicit intent | `C:\Program Files\Windows Kits\10\Debuggers\x64\gflags.exe` |
| `umdh.exe` | Heap snapshot and leak investigation | `C:\Program Files\Windows Kits\10\Debuggers\x64\umdh.exe` |
| `dumpbin.exe` | Inspect PE/COFF headers, imports, exports, sections, symbols, and disassembly | `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\dumpbin.exe` |
| `undname.exe` | Undecorate MSVC C++ symbols | `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\undname.exe` |
| `link.exe /dump` | `dumpbin`-style fallback inspection | `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\link.exe` |
| `lib.exe /list` | List static library contents | `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\lib.exe` |
| `editbin.exe` | PE/COFF mutation; do not use unless explicitly requested | `C:\Program Files\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\Hostx64\x64\editbin.exe` |
| `procdump.exe` | Capture process dumps | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\procdump.exe` |
| `procmon.exe` | Trace process, registry, file, and network activity | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\procmon.exe` |
| `procexp.exe` | Inspect processes, handles, DLLs, and threads | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\procexp.exe` |
| `vmmap.exe` | Inspect process virtual memory layout | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\vmmap.exe` |
| `handle.exe` | Find open handles | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\handle.exe` |
| `listdlls.exe` | List loaded DLLs for a process | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\listdlls.exe` |
| `sigcheck.exe` | Inspect signatures, versions, hashes, and VirusTotal metadata | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\sigcheck.exe` |
| `strings.exe` | Extract printable strings from binaries or dumps | `%LOCALAPPDATA%\Microsoft\WinGet\Packages\Microsoft.Sysinternals.Suite_Microsoft.Winget.Source_8wekyb3d8bbwe\strings.exe` |
| `ffmpeg.exe` | Media conversion/inspection helper for captures | `%USERPROFILE%\Programme\build\captureproject\build\msys64\clang64\bin\ffmpeg.exe` |
| `ffprobe.exe` | Media metadata/probing helper for captures | `%USERPROFILE%\Programme\build\captureproject\build\msys64\clang64\bin\ffprobe.exe` |
| `llvm-strings.exe` | Extract printable strings from COFF objects / DLLs (reliable on `.o`/`.dll` where `grep -a` mis-parses) | `%USERPROFILE%\Programme\build\captureproject\build\msys64\clang64\bin\llvm-strings.exe` |
| `llvm-objdump.exe` | Disassemble / inspect sections of the hook DLL/objects | `%USERPROFILE%\Programme\build\captureproject\build\msys64\clang64\bin\llvm-objdump.exe` |

---

## Conditional applicability of project-specific diagnostics

This document includes both general security-audit tooling and project-specific diagnostic knowledge.

Project-specific sections, including DX12 DRED, D3D12 debug-layer diagnostics, `DX12 DIAG:` log interpretation, media/capture helpers, hook DLL inspection, overlay diagnostics, and GPU device-removal analysis, apply only when the audited project contains the corresponding subsystem or when the audit question concerns that subsystem.

Rules:

- Do not treat DX12/DRED/debug-layer diagnostics as generic security-audit requirements.
- Do not penalize unrelated projects for missing DX12, DRED, GPU, capture, hook, or overlay artifacts.
- Mark these sections `N/A — not applicable` when the project does not contain the corresponding subsystem.
- If the subsystem is in scope and relevant diagnostics are unavailable, report a warning and lower only affected categories such as runtime stability, crash diagnostics, binary inspection, logging/privacy, domain-specific safety, or platform coverage.
- Diagnosis-only modes such as full DRED or debug-layer validation may change timing or behavior; distinguish diagnostic-induced behavior from production behavior.
- Treat diagnostic logs, DRED output, debug-layer output, dumps, captures, and hook logs as sensitive artifacts.


> Applicability: this section is project-specific and applies only to DX12/D3D12/GPU/capture/hook/overlay audits. Mark N/A for unrelated projects.

## DX12 DRED GPU-fault diagnostics (device-hung / `0x887A0006`)

- The hook arms **DRED** (Device Removed Extended Data) auto-breadcrumbs + page-fault in `Wrapped_D3D12CreateDevice` before the game's device is created (`hook/common/dx12_dred.cpp`). It is the primary tool for any `DXGI_ERROR_DEVICE_HUNG/REMOVED` (e.g. the x86 DX12 focus/mode-switch freeze): a bare HRESULT is not actionable, DRED names the hung command list and the faulting GPU VA.
- **Default OFF (opt-in)**; enable page-fault-only mode with an empty `ce_dx12_dred` file or env `CE_DX12_DRED=pf`, and full breadcrumbs with `CE_DX12_DRED=1` / `full` only while actively diagnosing a real device-removal. Auto-breadcrumbs (`SetAutoBreadcrumbsEnablement(FORCED_ON)`) make the APPLICATION's every `ID3D12GraphicsCommandList::Reset()` allocate a breadcrumb buffer via a KERNEL GPU allocation (`NtGdiDdDDICreateAllocation/DestroyAllocation`); during the Alt+Tab iflip<->composited mode switch that stalls the present thread for seconds and itself trips the 2 s TDR — i.e. leaving full DRED on caused the very freeze it was meant to capture (`logs/20260606_145929`: `CGraphicsCommandList::Reset -> Dred::AllocateBreadcrumbBuffer -> NtGdiDdDDIDestroyAllocation2`, gap=2646ms). Decision via `ce::dx12_overlay_policy::DecideDredArmMode`. Freeze dumps still capture the CPU-side present-thread stack without DRED.
- On device removal (the two `ProcessFrame` device-removed sites and the freeze watchdog dump) the hook log (`installed/captureengine/logs/<ts>/hook_debug.log`) gets a block:
  - `DX12 DRED: ===== device-removed extended data (<reason>) =====`
  - `DX12 DRED:  node#N queue='...' list='...' completedOps=X/Y  <-- INCOMPLETE (GPU hung in this list)` plus the breadcrumb op window and `ctx@opN=` context strings.
  - `DX12 DRED:  pageFaultVA=0x...` then `[existing]` and `[recently-freed]` allocation names. A faulting VA that matches a **recently-freed** allocation is the smoking gun for stale-resource access (e.g. a backbuffer reallocated during the iflip<->composited mode switch). CE overlay objects are named `CE_OverlayFence` / `CE_OverlayCmdList` / `CE_OverlayAlloc[i]` / `CE_OverlayQueue` / `CE_OverlayOffscreenRT`.
- Confirm DRED is actually armed in a build with: `llvm-strings installed/captureengine/capture_hook_x64.dll | grep "DX12 DRED: armed"`. If absent, the arming was dead-stripped (ThinLTO + `--gc-sections`) — the DRED entry points must stay `__declspec(dllexport)` (`CE_DRED_API`; plain `used` was honored on x64 but stripped on x86).
- DRED auto-breadcrumbs require arming BEFORE device creation. CE arms in `DX12Hook::Init()` (early, on a worker thread); the `Wrapped_D3D12CreateDevice` site is dead in normal builds (`#ifdef ENABLE_D3D12_WRAPPER`, no `d3d12_wrappers.dll`). If injection happens after the game's device is already created, DRED can't arm and `GetAutoBreadcrumbsOutput1` returns failure.
- **Historical v12 upload-ring signature**: DRED reports an INCOMPLETE CE overlay command list (`DRAWINDEXEDINSTANCED`) with `pageFaultVA=0` (pure hang), and the freeze dump's render thread is parked in `...DetourExecuteCommandLists -> D3D12Core!CCommandQueue::ExecuteCommandLists -> nvwgf2um (AllocateCB) -> win32u!NtGdiDdDDICreateAllocation`. That older hazard was fixed by the per-slot overlay fence and remains an invariant: never reuse an overlay upload slot until the GPU has completed the frame that used it.
- **Current x86 DX12 no-vsync fixed signature (v13)**: healthy 32-bit `dx12_test.exe` logs show `DX12 focus-loss sync policy=v13 draw-every-frame + x86 solid-span text + upload-slot per-frame fence`, `DX12 Overlay: x86 solid-span text path enabled`, and `DX12 DIAG: Texture2D command ... textured=0`. A reappearance of `textured=1` in the x86 no-FG path is a regression.

## DX12 always-on present/ECL timing diagnostics (`DX12 DIAG:` in hook_debug.log)
Built-in, ALWAYS-ON (no env/flag/install), written via `HookLogImportant` to `hook_debug.log`. Added 2026-06-06 to localize x86 DX12 present/ECL stalls; see `dx12-overlay-third-party-coexistence.md` and `handoff-dx12-32bit-crash.md` for the current v13 fixed state. Source: `hook/apis/dx12_hook.cpp`, `hook/common/custom_overlay_dx12.cpp`, and `hook/common/dxgi_shared.cpp`.

- `DX12 DIAG: ExecuteCommandLists SLOW Xms (queue=.. overlayQueue=.. lists=.. devRemoved=0x..)` — a single ECL ≥2 ms (the call includes the real forward where the NV driver's `AllocateCB → NtGdiDdDDICreateAllocation` happens). `devRemoved` non-zero = post-removal spinning, ignore. On the 32-bit freeze this maxed at 9.5 ms → the stall is NOT the ECL.
- `DX12 DIAG: ECL timing/1s: count=.. maxMs=.. avgMs=..` — per-second ECL stats for steady-state 32-bit vs 64-bit comparison (note: count/avg inflate AFTER a freeze because the app spins on the dead device).
- `DX12 DIAG: overlay footprint draws=.. vbBytes=.. ibBytes=..` — CE's per-frame overlay GPU work (was identical 32-bit vs 64-bit: draws=4 vb=13760 ib=2064 → not a code-path difference).
- `DX12 DIAG: DetourPresent TOTAL SLOW Xms` / `ProcessFrame (overlay) SLOW Xms` / `overlay-completion wait SLOW Xms` — present-phase split. **Interpretation**: slow TOTAL with NO slow ProcessFrame/wait ⇒ the stall is the real `Present` blocking on the hung GPU (CE overlay backbuffer draw wedged the GPU mid mode-switch); slow `wait` ⇒ CE overlay GPU work hung; slow `ProcessFrame` ⇒ CE record/submit path.

## DX12 debug-layer diagnostic (env `CE_DX12_DEBUG_LAYER`)

- For cases DRED can only report as a "pure hang" (`pageFaultVA=0`, e.g. the x86 DX12 Alt+Tab overlay-draw hang), CE can enable the D3D12 debug layer to surface the exact resource-state/hazard at the API call. Requires the Graphics Tools optional feature (`C:\Windows\System32\d3d12SDKLayers.dll` — present on this machine). Armed in `DX12Hook::Init()` before device creation (`ce::dx12_dred::ArmDebugLayerBeforeDeviceCreation`).
- Levels: `CE_DX12_DEBUG_LAYER=1` enables the debug layer (lighter); `=2` also enables GPU-based validation (heavier, serializes — can mask timing hangs but catches GPU-side hazards). Unset/`0` = off (default; the debug layer changes timing so it is diagnosis-only).
- The device's `ID3D12InfoQueue` is drained to the hook log every `ProcessFrame` and on device-removal, tagged `DX12 DBGLAYER [<context>] sev=.. cat=.. id=..: <description>`. Run the repro with the env set, then read `hook_debug.log` for those lines around the freeze.
---

## Security audit additions

Use this section during security audits to verify binary hardening, DLL loading, signatures, embedded secrets, dependency exposure, crash-dump sensitivity, runtime mitigations, filesystem/registry behavior, network behavior, and security-relevant logs.

If a tool listed here is unavailable, print a warning in the audit report, state what coverage was lost, and reduce confidence/scoring for affected categories.

### General rules for security use of these tools

- Treat this file as a local tool inventory and project-specific diagnostic guide, not as proof that tools are installed or usable.
- Before relying on a tool, verify that it exists at the documented path and can run in the current environment.
- Prefer project-documented symbol paths, binary paths, PDB paths, logs, and diagnostic flags when they are applicable.
- If a documented tool, PDB, symbol directory, dump, binary, log, capture, or platform target is unavailable, report a warning and reduce confidence for affected audit areas.
- Do not mutate PE/COFF files, PDBs, registry settings, global debug flags, runtime mitigations, or project configuration unless explicitly requested.
- Treat crash dumps, logs, capture files, generated diagnostics, and string-extraction outputs as sensitive artifacts.

### Windows PE/COFF hardening checks

Use these checks for shipped `.exe`, `.dll`, `.sys`, `.lib`, and relevant object files.

Recommended commands:

```bat
dumpbin /headers <binary>
dumpbin /loadconfig <binary>
dumpbin /dependents <binary>
dumpbin /imports <binary>
sigcheck.exe -m -i -h <binary>
```

Assess where applicable:

- ASLR / `/DYNAMICBASE`
- high-entropy VA
- DEP / NX compatibility
- Control Flow Guard / `/guard:cf`
- exception-continuation protection where available
- stack cookies / `/GS`
- SafeSEH for legacy 32-bit builds where applicable
- CET / shadow-stack or related platform mitigation metadata where applicable
- writable-executable sections
- executable stack or unusual section permissions
- debug/release differences
- unexpected exported symbols
- suspicious imports, such as shell execution, process injection, unsafe temp-file APIs, dynamic loading, credential APIs, registry persistence, or network APIs

Warnings to emit:

```text
WARNING: PE hardening metadata was not inspected for <binary>; binary-hardening confidence is reduced.
WARNING: <binary> lacks expected mitigation metadata: <mitigation>; assess whether this is justified for the target platform and build mode.
```

### DLL search-order and sideloading audit

For hook DLLs, plugins, launchers, services, helper binaries, and injected components, assess DLL loading behavior.

Recommended tools:

```bat
dumpbin /imports <binary>
dumpbin /dependents <binary>
listdlls.exe <pid>
procmon.exe
sigcheck.exe -m -i -h <dll-or-exe>
```

Review:

- relative `LoadLibrary` or `LoadLibraryEx` calls
- current-directory DLL loading
- missing `SetDefaultDllDirectories` / `AddDllDirectory` where applicable
- unsafe plugin search paths
- unexpected DLLs loaded from writable directories
- unsigned or unexpectedly signed DLLs
- PATH-dependent runtime behavior
- side-by-side/runtime redistributable assumptions
- architecture mismatches, especially x86/x64/ARM64
- user-writable directories in DLL search paths
- update/download flows that place executable files in loadable locations

Warnings to emit:

```text
WARNING: DLL search behavior could not be validated for <binary>; sideloading confidence is reduced.
WARNING: <process> loaded <dll> from a user-writable or unexpected path.
```

### Embedded secrets and sensitive strings

Use `strings.exe` and `llvm-strings.exe` for security review, not only general binary inspection.

Recommended commands:

```bat
strings.exe -n 8 <binary> > strings.txt
llvm-strings.exe <binary> > llvm-strings.txt
findstr /i "token secret password passwd api_key apikey bearer private key localhost http:// https:// pdb users temp credential auth session cookie webhook" strings.txt
```

Look for:

- API keys
- bearer tokens
- passwords
- private keys
- certificates
- internal URLs
- localhost-only assumptions
- usernames
- local build paths
- PDB paths
- temp directories
- crash/log paths
- internal hostnames
- debug-only flags
- feature flags that weaken security
- telemetry endpoints
- webhook URLs
- command-line templates
- suspicious shell snippets

Warnings to emit:

```text
WARNING: Embedded-string scan was not performed for <binary>; confidence in secrets/path leakage is reduced.
WARNING: Potential sensitive string found in <binary>: <redacted-summary>.
```

Never paste full secrets into the audit report. Redact values and include only enough context to identify the location and risk.

### Authenticode, signer, hash, and trust validation

Use `sigcheck.exe` for shipped binaries and third-party redistributables.

Recommended commands:

```bat
sigcheck.exe -m -i -h <binary>
sigcheck.exe -q -m -i -h -e <release-folder>
```

Assess:

- unsigned shipped binaries
- unexpected signer
- expired certificate
- revoked or unverifiable signature
- inconsistent product/version metadata
- unexpected hashes between inspected and shipped artifacts
- unexpected third-party binaries
- binaries downloaded or generated outside the expected build path

Warnings to emit:

```text
WARNING: Signature and hash validation was not performed for <binary>; file-trust confidence is reduced.
WARNING: <binary> is unsigned or signed by an unexpected signer.
```

### Local dependency and bundled-library inspection

Even when SBOM/provenance is out of scope, inspect local bundled dependencies for security risk.

Recommended commands:

```bat
dumpbin /dependents <binary>
sigcheck.exe -m -i -h -e <release-folder>
strings.exe <third-party-dll>
```

Assess:

- bundled DLL inventory
- duplicate or conflicting DLL versions
- old or vulnerable native libraries
- OpenSSL, zlib, curl, ffmpeg, media codec, compression, crypto, XML, JSON, archive, and networking library versions
- unexpected runtime redistributables
- dependency version strings visible in metadata or binary strings
- architecture-specific dependency drift
- libraries loaded from user-writable locations

Warnings to emit:

```text
WARNING: Bundled dependency inventory was not inspected; dependency confidence is reduced.
WARNING: Potentially outdated or vulnerable bundled library detected: <library/version>.
```

### Crash dump sensitivity and privacy

Crash dumps can contain highly sensitive data. Treat them as confidential audit artifacts.

Dumps may contain:

- tokens
- credentials
- session data
- URLs
- usernames
- local file paths
- environment variables
- command-line arguments
- process memory
- frame/capture buffers
- device/application state
- loaded module paths
- proprietary code/data fragments

Rules:

- Do not upload, attach, or copy dumps outside the local audit environment unless explicitly approved.
- Prefer local symbol resolution.
- Redact sensitive values before quoting dump-derived evidence.
- If a dump is unavailable, inaccessible, or lacks required symbols/PDBs, report the coverage loss.
- If local PDBs are expected, do not rely on Microsoft-symbol-server-only stack traces.

Warnings to emit:

```text
WARNING: Crash dump analysis was skipped because <dump> was unavailable; crash/root-cause confidence is reduced.
WARNING: Local PDB directory was unavailable; stack traces may be incomplete.
WARNING: Dump-derived evidence may include sensitive process memory and was redacted.
```

### Windows runtime mitigation policy

Use PowerShell to inspect process mitigation policy where applicable.

Recommended command:

```powershell
Get-ProcessMitigation -Name <exe>
```

Assess:

- DEP
- ASLR
- CFG
- dynamic code restrictions
- binary signature policy
- extension-point disablement
- child-process restrictions
- image-load restrictions
- strict handle checks
- SEHOP where relevant
- audit-only versus enforce mode

Warnings to emit:

```text
WARNING: Runtime mitigation policy was not inspected for <exe>; exploit-mitigation confidence is reduced.
WARNING: <exe> does not enforce expected mitigation <mitigation>; assess whether this is justified.
```

### Filesystem and registry tracing for security

Use `procmon.exe`, `handle.exe`, and related tools to inspect behavior under realistic runtime scenarios.

Review:

- unsafe temp files
- writes outside expected directories
- weak file permissions
- symlink/hardlink-sensitive file operations
- unsafe overwrite/delete behavior
- registry autorun or persistence behavior
- unexpected credential-store access
- unexpected config reads
- unexpected network/config writes
- DLL search path behavior
- log/capture output locations
- cleanup on crash, cancellation, and restart

Useful tools:

```bat
procmon.exe
handle.exe <name-or-pid>
procexp.exe
```

Warnings to emit:

```text
WARNING: Filesystem/registry runtime tracing was not performed for high-risk write paths; storage safety confidence is reduced.
WARNING: Process wrote security-relevant data to an unexpected or weakly protected location: <path>.
```

### Network behavior inspection

If the product opens sockets or makes outbound requests, inspect network behavior.

Potential tools, if available:

```bat
netstat -ano
powershell -Command "Get-NetTCPConnection"
pktmon
netsh trace start capture=yes tracefile=<path>
netsh trace stop
```

If Wireshark or tshark is installed, it may be used where appropriate and permitted.

Assess:

- listening ports
- outbound connections
- plaintext HTTP
- unexpected telemetry
- TLS endpoints
- certificate validation behavior
- webhook/callback behavior
- localhost-only trust assumptions
- retry storms
- excessive connection attempts
- network behavior during crash/restart/update flows

Warnings to emit:

```text
WARNING: Network behavior was not inspected despite network-capable code; network exposure confidence is reduced.
WARNING: Unexpected outbound connection observed: <host-or-endpoint-summary>.
```

### Windows event logs and reliability/security evidence

Use Windows event logs to correlate crashes, blocked loads, exploit mitigations, and security-relevant runtime events.

Recommended commands:

```powershell
Get-WinEvent -LogName Application -MaxEvents 200
Get-WinEvent -LogName System -MaxEvents 200
wevtutil qe Application /c:200 /f:text
wevtutil qe System /c:200 /f:text
```

Check for:

- application crashes
- service failures
- driver/device errors
- blocked DLL loads
- exploit mitigation events
- Windows Defender events
- SmartScreen events
- AppLocker / WDAC events where applicable
- repeated failure loops
- update/install errors that affect security posture

Warnings to emit:

```text
WARNING: Windows event logs were not checked for crash/security correlation; runtime evidence confidence is reduced.
```

### DX12 diagnostics security notes

DX12 DRED, DX12 debug-layer diagnostics, and `DX12 DIAG:` logs are diagnosis tools, not production security controls.

Security audit notes:

- Full DRED and debug-layer modes can change timing or behavior; distinguish diagnostic-induced behavior from production behavior.
- DRED/debug-layer output may expose object names, paths, GPU state, app/game names, user directories, capture context, and internal diagnostics.
- Treat `hook_debug.log`, DRED blocks, debug-layer messages, freeze dumps, capture logs, and media captures as sensitive artifacts.
- When debug-layer or DRED output is unavailable, state whether root-cause confidence is reduced.
- When full DRED is enabled, document whether it could have affected the reproduced behavior.

Warnings to emit:

```text
WARNING: DRED/debug-layer diagnostics were unavailable for a device-removal issue; GPU fault root-cause confidence is reduced.
WARNING: Full DRED/debug-layer diagnostics may alter timing; distinguish diagnostic-induced behavior from production behavior.
```

### Tool discovery fallbacks

When documented absolute paths fail, use discovery only as a fallback and record the result.

Recommended commands:

```bat
where cdb
where dumpbin
where sigcheck
where strings
where llvm-strings
```

```powershell
Get-Command cdb.exe -ErrorAction SilentlyContinue
Get-Command dumpbin.exe -ErrorAction SilentlyContinue
Get-Command sigcheck.exe -ErrorAction SilentlyContinue
```

If a fallback tool is used, record:

- documented path
- fallback path
- version, if available
- reason fallback was needed
- coverage difference

### Evidence capture conventions

For security audits, record enough evidence to make results reproducible without leaking secrets.

Capture:

- exact command
- target binary/log/dump path
- tool path
- tool version where practical
- architecture of the target
- build configuration
- timestamp of inspected artifact
- hash of inspected binary where practical
- redacted output excerpts
- reason output is trusted or incomplete

Do not include:

- full secrets
- full crash dumps
- full process memory
- private keys
- unredacted tokens
- unnecessary user paths
- unrelated personal data
---

## Installer-created paths and source-of-truth rule

When `install-security-audit-tools.ps1` is used with default settings, it installs or detects tools under:

```text
%LOCALAPPDATA%\SecurityAuditTools
```

Default generated evidence files:

```text
%LOCALAPPDATA%\SecurityAuditTools\security-audit-tool-manifest.json
%LOCALAPPDATA%\SecurityAuditTools\security-audit-tool-warnings.txt
%LOCALAPPDATA%\SecurityAuditTools\security-audit-tool-availability.md
```

Default portable tool locations created by the PowerShell installer:

```text
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\procdump.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\sigcheck.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\strings.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\handle.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\listdlls.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\vmmap.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\vswhere\vswhere.exe
```

Optional paths created only when corresponding installer flags are used:

```text
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\Procmon.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\procexp.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\ffmpeg\extract\...\bin\ffmpeg.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\ffmpeg\extract\...\bin\ffprobe.exe
```

The installer does **not** install these large/non-portable toolsets by default:

```text
Windows SDK Debugging Tools: cdb.exe, windbg.exe, dumpchk.exe, symchk.exe, dbh.exe, pdbcopy.exe, symstore.exe, gflags.exe, umdh.exe
Visual Studio / MSVC tools: dumpbin.exe, link.exe, lib.exe, editbin.exe, undname.exe
WinDbg Preview: WinDbgX.exe
LLVM: llvm-strings.exe, llvm-objdump.exe
FFmpeg: ffmpeg.exe, ffprobe.exe
```

The paths in the earlier Windows debugging table are known-good examples or common installed/default paths. They are not guaranteed to match a fresh environment after running the installer.

For audits, use this precedence order:

1. `security-audit-tool-manifest.json` generated by the installer
2. explicit paths from `tool-paths.env`
3. `Get-Command` / `where` discovery
4. known-good paths listed in this document
5. fallback tools, if safe and appropriate

Do not report an example hardcoded path as missing if the tool exists elsewhere and is recorded in the manifest.

Do report a warning when a relevant tool is missing from all sources:

```text
WARNING: dumpbin.exe was not found in the installer manifest, PATH, Visual Studio discovery, or documented fallback paths; PE/COFF inspection confidence is reduced.
```

---

## Path portability and local overrides

Hardcoded paths in this document are examples from one local development environment. For portable security audits, prefer environment variables and relative discovery before treating a path as missing.

Recommended variables:

| Variable | Meaning |
|---|---|
| `CE_PROJECT_ROOT` | Repository root |
| `CE_BUILD_ROOT` | Build tree root |
| `CE_INSTALL_ROOT` | Installed artifact root |
| `CE_PDB_ROOT` | Local PDB/symbol directory |
| `CE_LOG_ROOT` | Local logs directory |
| `CE_DUMP_ROOT` | Crash dump directory |
| `CE_CAPTURE_ROOT` | Capture/media artifact directory |
| `SECURITY_AUDIT_TOOL_ROOT` | Portable audit tools root |

Example crash-dump command:

```bat
cdb -z "%CE_DUMP_ROOT%\crash.dmp" -y "srv*;%CE_PDB_ROOT%" -c ".ecxr; k; q"
```

If `CE_PDB_ROOT` is unset, try documented relative locations before warning:

- `%CE_INSTALL_ROOT%`
- `%CE_BUILD_ROOT%`
- `%CE_PROJECT_ROOT%\installed\captureengine`
- `%CE_PROJECT_ROOT%\build\captureengine`
- artifact/symbol directories documented by the current build or release process

Warning policy:

- Do not warn only because an example path from another machine does not exist.
- Warn when the current audit target requires the path or artifact and no equivalent was found.
- State what was unavailable, what fallback was used, and how confidence/scoring changed.
---

## Linux and macOS security audit tools

Use this section when auditing Linux or macOS targets. Verify tool availability before relying on results.

### Linux x64 / ARM64 binary and runtime inspection

Preferred tools:

| Tool | Purpose |
|---|---|
| `readelf` | ELF headers, dynamic section, symbols, RELRO/NX/PIE evidence |
| `objdump` | ELF program headers, imports, disassembly, dynamic deps |
| `checksec` | Summary of ELF hardening, where available |
| `patchelf` | RPATH/RUNPATH inspection, where available |
| `file` | Architecture, ABI, linkage metadata |
| `nm` | Symbols |
| `strings` | Embedded strings and secrets/path review |
| `strace` | Syscall tracing for file/network/process behavior |
| `ltrace` | Library-call tracing, where useful |
| `ldd` | Dependency inspection only for trusted local build artifacts |
| `gdb` / `lldb` | Crash/debug inspection |
| `coredumpctl` | systemd core dump lookup where available |

Safer dependency/hardening commands:

```sh
file ./binary
readelf -h ./binary
readelf -l ./binary
readelf -d ./binary
readelf -s ./binary
objdump -p ./binary
readelf -d ./binary | grep -E 'RPATH|RUNPATH|NEEDED|BIND_NOW'
checksec --file=./binary
strings -a ./binary | grep -Ei 'token|secret|password|passwd|api[_-]?key|bearer|private|credential|cookie|webhook|http://|https://'
```

Do not use `ldd` on untrusted binaries. Prefer `readelf -d` or `objdump -p`.

Runtime tracing examples:

```sh
strace -f -e trace=file,process,network ./binary
```

### macOS x64 / ARM64 binary and runtime inspection

Preferred tools:

| Tool | Purpose |
|---|---|
| `codesign` | Signature, hardened runtime, entitlements |
| `otool` | Mach-O load commands and dynamic libraries |
| `lipo` | Universal binary slice inspection |
| `file` | Architecture and Mach-O metadata |
| `nm` | Symbols |
| `strings` | Embedded strings and secrets/path review |
| `dwarfdump` | dSYM/debug info inspection |
| `lldb` | Crash/debug inspection |
| `spctl` | Gatekeeper assessment where relevant |
| `log` | Unified logging inspection |
| `fs_usage` | Filesystem runtime tracing |
| `dtruss` | Syscall tracing where permitted |

Useful commands:

```sh
file ./binary
codesign -dvv ./binary
codesign -d --entitlements - ./binary
otool -L ./binary
otool -l ./binary | grep -A3 -E 'LC_RPATH|LC_LOAD_DYLIB'
lipo -info ./binary
strings -a ./binary | grep -Ei 'token|secret|password|passwd|api[_-]?key|bearer|private|credential|cookie|webhook|http://|https://'
```

For universal binaries, inspect each slice independently.

Warnings to emit:

```text
WARNING: Linux/macOS binary hardening tools were unavailable; platform binary-inspection confidence is reduced.
WARNING: macOS universal binary was shipped but individual slices were not inspected independently.
WARNING: Linux dependency inspection used ldd only on a trusted local build artifact; do not use ldd on untrusted binaries.
```


---

## Exact tool path lookup after installer runs

After running `install-security-audit-tools.ps1`, use the generated manifest as the first source of truth for paths.

PowerShell examples:

```powershell
$manifestPath = Join-Path $env:LOCALAPPDATA 'SecurityAuditTools\security-audit-tool-manifest.json'
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
$manifest.results | Sort-Object category, name | Format-Table name, category, status, path -AutoSize
```

Resolve one tool:

```powershell
$sigcheck = ($manifest.results | Where-Object { $_.name -eq 'sigcheck.exe' -and $_.path } | Select-Object -First 1).path
& $sigcheck -m -i -h .\some-binary.exe
```

Default installed paths from `install-security-audit-tools.ps1`:

```text
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\procdump.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\sigcheck.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\strings.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\handle.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\listdlls.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sysinternals\vmmap.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\vswhere\vswhere.exe
%LOCALAPPDATA%\SecurityAuditTools\bin\sast\...
```

Do not assume these directories are on `PATH` unless `-AddToUserPath` was used.

### Required-tool gate

The PowerShell installer supports a strict required-tool gate:

```powershell
.\install-security-audit-tools.ps1 -RequireTools semgrep,gitleaks,osv-scanner -StrictRequiredTools
```

Exit codes:

```text
0 = no warnings
2 = completed with warnings
3 = strict required-tool gate failed
```

Use strict mode only when the audit scope really requires those tools. Do not gate on irrelevant project-specific tools.

### Windows SAST/secrets/dependency optional installs

Default behavior installs portable, low-side-effect scanners where possible:


```powershell
.\install-security-audit-tools.ps1
```

Default install attempts:

```text
gitleaks
osv-scanner
```

Python/pip-based tools are detected but not installed unless explicitly requested:

```text
semgrep
flawfinder
pip-audit
```

Install Python/pip-based tools explicitly:

```powershell
.\install-security-audit-tools.ps1 -IncludePythonSast
```

Conservative detector-only mode:

```powershell
.\install-security-audit-tools.ps1 -Minimal
```

Targeted opt-outs:

```powershell
.\install-security-audit-tools.ps1 -SkipSastInstall
.\install-security-audit-tools.ps1 -SkipSecretsInstall
.\install-security-audit-tools.ps1 -SkipDependencyScannerInstall
```

Additional opt-in tools:


Individual opt-ins:

```powershell
.\install-security-audit-tools.ps1 -IncludeSemgrep
.\install-security-audit-tools.ps1 -IncludeFlawfinder
.\install-security-audit-tools.ps1 -IncludeGitleaks
.\install-security-audit-tools.ps1 -IncludeTruffleHog
.\install-security-audit-tools.ps1 -IncludeOSVScanner
.\install-security-audit-tools.ps1 -IncludePipAudit
.\install-security-audit-tools.ps1 -IncludeCodeQL
```

`CodeQL` is large and should remain explicitly opt-in.

### Full and uninstall modes

Full install mode:

```powershell
.\install-security-audit-tools.ps1 -Full
```

This opts into large, noisy, package-manager, and Python-based tools. It is intended for dedicated audit environments.

Managed uninstall:

```powershell
.\install-security-audit-tools.ps1 -Uninstall
```

Full supported uninstall preview:

```powershell
.\install-security-audit-tools.ps1 -Uninstall -RemoveSharedPackages -RemovePythonPackages -WhatIfOnly
```

Full supported uninstall execution:

```powershell
.\install-security-audit-tools.ps1 -Uninstall -RemoveSharedPackages -RemovePythonPackages
```

Shared package removal can uninstall tools the user may have installed before this audit setup. Use it only when that is acceptable.

### Full-mode path reliability

When full mode installs winget packages, the current shell may not immediately see PATH changes. The installer checks known install directories after installation.

Important known directories:

```text
C:\Program Files\LLVM\bin
C:\Program Files (x86)\LLVM\bin
%LOCALAPPDATA%\Programs\LLVM\bin
%LOCALAPPDATA%\Microsoft\WindowsApps\WinDbgX.exe
```

If winget reports no upgrade or already-installed status, verify availability using `winget list --id <PackageId> -e` before treating it as a failure.
