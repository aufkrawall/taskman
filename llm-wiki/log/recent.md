- 2026-09-08: Linux parity + release validation: the collector no longer lists threads as processes (sysinfo 0.39 keeps `tasks` enabled by default) and thread counts include the leader; Linux sub-pixel AA follows fontconfig `rgba` (`rgb`/`bgr` only); `build.py` warns that the static-musl fallback is headless-only; v0.1.3 ships a GUI-capable glibc Linux artifact validated under WSLg.
- 2026-09-08: Published TaskMan v0.1.2 for Windows x86_64 and Linux x86_64 (static musl PIE); both archives and their SHA-256 checksums are on GitHub. Release publication steps are now in `build.md`.
- 2026-09-08: Processes/Details scroll stability: `tablekit::scrolled_rows` anchors the viewport to the top visible row identity across model rebuilds (new/removed processes, tree expand/collapse), and Processes ordering is deterministic (creation-order snapshot sort plus pid tie-breaks) so equal-valued or same-named rows no longer reshuffle every sample.
- 2026-09-08: Security audit pass: the broker's installing SID must resolve to a user account (group/alias SIDs rejected), file-log verification no longer gates broker startup (a planted `ProgramData\TaskMan\logs` entry could disable the privileged control plane), Win32 scratch buffers are pointer-aligned (`aligned.rs`), Windows release binaries are CET-compatible, `build.py --audit` runs cargo-audit/gitleaks, and `tm-core` denies `unsafe_code` while `tm-platform`/`tm-app` deny `unsafe_op_in_unsafe_fn`.
- 2026-09-08: `python build.py` now always builds the Linux x86_64 release too: glibc via `cross`/`cargo-zigbuild` when present, otherwise a self-contained static musl PIE linked by the bundled `rust-lld` (rustup std only, no zig/Docker). Fixed `crates/tm-app/build.rs` using HOST `cfg`, which attached the Windows `.res` to Linux links. Linux artifact verified by `--selfcheck` under WSL.
- 2026-09-08: Full-repository audit fixes: non-Windows backends compile again (`cargo check --target x86_64-unknown-linux-gnu` / `aarch64-apple-darwin`), and `build.py --check` now runs those cross-target checks (CI installs both targets). MSRV corrected to 1.88 (let-chains/`as_chunks`), release `--remap-path-prefix` also strips the checkout root, config/history reads are capped, and the ETW properties buffer is word-aligned.
- 2026-09-08: Agent/template alignment: generalized the security-audit tool inventory (removed captureengine-era DX12/hook content), merged missing upstream agent rules into AGENTS.md/CLAUDE.md, filled the codestyle page, and completed the wiki catalog.
- 2026-09-08: Run dialog focus state now has an explicit Command default, satisfying egui IdTypeMap temporary-state removal requirements while keeping initial keyboard focus semantics centralized.
- Run-new-task dialog now owns a native-style keyboard focus loop: Tab/Shift+Tab cycle command, elevation checkbox, Cancel, Browse and OK; Enter invokes the focused push button or default OK; Space toggles/activates focused controls; Escape cancels; the command edit is focused only while it is the active stop.
- 2026-09-08: Details gained optional Network / Network receive / Network send columns. PROCESS_NET demand now follows those visible columns and stays active while Process Properties is open, fixing blank live network statistics there without running the ETW session continuously on Details.
- 2026-09-08: Process Properties now summarizes mitigations with System Informer-style qualifiers (permanent DEP, high-entropy ASLR, prohibited/disabled wording, CF Guard and stack protection), and module inventory uses the authenticated LocalSystem broker for identity-bound SYSTEM/service inspection with bounded responses.
# Recent Activity

## 2026-09-11 — The Disk column was zero for half the process list

User report: `BackgroundDownload.exe` showed `0 MB/s` in the Disk column while
native Task Manager showed about 200 KB/s for it. Measured with
`\Prozess(*)\E/A-Datenbytes/s`: 205 KB/s, real.

Same root cause as the image-path bug earlier today. `disk_read_bps` /
`disk_write_bps` came from sysinfo's `disk_usage()`, which is
`GetProcessIoCounters` and needs a process HANDLE — so every SYSTEM or
elevated process reported a flat, measured-looking ZERO. The
`SYSTEM_PROCESS_INFORMATION` table `cpu_load.rs` already reads every tick
carries `ReadTransferCount`/`WriteTransferCount` for every process; the Disk
column now takes them from there and keeps sysinfo only as a fallback.
Coverage went from roughly half the list to 191/241. `OtherTransferCount` is
deliberately excluded — native Task Manager leaves ioctl payloads out of its
Disk column too.

`format_rate_mb` also rounded anything under 51 KB/s to "0,0 MB/s", which is
indistinguishable from idle — the exact thing the column exists to tell apart.
It now prints "<0,1 MB/s", the convention the Network column already used.

What the two disk columns mean, for the record: **Disk** is I/O BYTES the
process requested (cache hits, pipes and sockets included — this is what
native TM shows), **Disk activity** is its share of the time the disks were
really busy (ETW). A process can top one and not the other; that is the point
of having both.

## 2026-09-11 — Draggable Processes columns (and the crash that came with them)

The Processes page's numeric columns can now be dragged into any order in the
header, persisted under `[columns.processes.order]`. `tablekit` gained an
opt-in gesture (`TmTable::reorderable(range)` + `take_reorder`) so the page
owns its ordering; the drag source lives in egui context memory because the
table is rebuilt every frame while the pointer is down, and the drop slot is
CLAMPED into the reorderable range rather than rejected, so dragging left past
Name parks at the first movable slot.

Name and Status are pinned deliberately: `heat_cells` paints the blue band as
one contiguous span and the Name cell owns the tree chevron.

The important internal rule: `RowData::values`/`heat` stay in a fixed LOGICAL
order and `State::sort_col` is a logical index. Only the UI layer knows the
user's permutation (`State::value_order`), so dragging a column never
re-sorts the table and never disturbs the aggregation, caching or tests.

Two defects surfaced while wiring it:

* `sort_entries` matched columns 1..=5 by hand, so sorting by Disk activity or
  GPU — added after that match was written — silently fell through to sorting
  by NAME. Replaced by an arm over every value column.
* Widening `Aggregates::strings` to six machine totals CRASHED the Users page:
  its auto-fit loop wrote `fit[i + 2]` per total, and Users has four numeric
  columns in a six-column table. Both tabs now zip against
  `TmTable::numeric_indices`, which cannot overrun in either direction.

## 2026-09-11 — Identity of the processes that will not open

User report: Process Properties showed `Path: —` for `BackgroundDownload.exe`,
the SYSTEM process they had just identified as the disk hog. Session ID and
Platform were blank too, while Image type and mitigations were filled in — the
tell that the broker's identity-bound reads worked and the local ones did not.

Every ordinary path source needs a HANDLE: sysinfo, `QueryFullProcessImageNameW`,
the PEB. An unelevated session cannot open a SYSTEM or elevated process at all,
which is roughly half a Windows process list — measured here, only 50 % of
processes had a path. `NtQuerySystemInformation(SystemProcessIdInformation)`
(class 88) answers for any PID without a handle; it returns an NT device path,
translated back to a drive letter via `QueryDosDeviceW`. Coverage went to
270/273, the remainder being kernel pseudo-processes that genuinely have no
image. Pinned by two live tests (our own path must match `current_exe`, and
`wininit.exe`/`services.exe` must resolve) plus an integration threshold that
catches a regression to the handle-only era.

Session ID had the same shape and a sillier cause: the kernel-table fallback
`cpu_load::session_id_of` already existed and was only ever used to infer a
user name, never assigned to `ProcessEntry::session_id`. Platform (`wow64`)
followed for free once the path resolves, since it reads the PE header.

Command line still requires a handle and stays "—" for those processes.

## 2026-09-11 — Disk attribution, cursor-anchored graph readout, byte units

User report, five parts: only the CPU core tiles showed a hover value and its
position was unreliable; the CPU graphs looked blurry; Ethernet was quoted in
kbps; the Ethernet readout did not say which series was send and which
receive; and nothing on Processes/Details identified the process behind the
Performance page's disk Active time.

**Readout.** `Response::on_hover_text` anchors its tooltip to the WIDGET rect,
so on a 700 px chart it lands near a corner — and at a different corner per
chart, which is what made it read as "only some graphs have this".
`chart.rs::readout` paints on the tooltip layer at the pointer instead, with a
colour swatch + name per series, the hovered sample's age and a vertical
marker. Pinned headlessly by
`chart::tests::hovering_paints_a_readout_next_to_the_cursor`, which asserts a
new shape appears within 40 px of the cursor.

**Blur.** Chart rects come out of egui layout at fractional point
coordinates — the per-core grid's cell size is `(width - gaps) / columns` — so
a 1 pt border straddled two device-pixel rows and `painter_at` clipped it at a
fractional boundary. Frames are now `round_to_pixels`, hairlines
`round_to_pixel_center`, and every hairline is exactly `1 / pixels_per_point`
wide.

**Disk attribution.** `IO_COUNTERS` cannot answer it (cache hits, sockets,
named pipes in; paging I/O out), so `win/disk_etw.rs` enables
`Microsoft-Windows-Kernel-Disk` and sums `HighResResponseTime` per process.
Attribution is by `IssuingThreadId`, because disk events complete in whatever
context the DPC ran in and their header PID is usually System; the session
therefore also enables `Microsoft-Windows-Kernel-Process` thread events
(keyword `0x20`, verified with `wevtutil gp`) and seeds a tid->pid map from a
ToolHelp snapshot. Unmappable requests go to an unattributed remainder that
stays in the share denominator. The provider's time unit is undocumented, so
only ratios are ever published. Hosted by the broker at protocol v4, exactly
like the network trace; `win/etw.rs` now owns the session plumbing both use.

Free alongside it: `IO_COUNTERS` operation counts and `HardFaultCount` from
the kernel table `cpu_load.rs` already reads every tick, exposed as
`I/O operations/s` and `Hard faults/s` on Details. Offsets pinned against the
`windows` crate's declared layout AND against a live table
(`live_kernel_table_yields_plausible_io_counters`).

**Wrong provider, found the same day.** The first cut enabled
`Microsoft-Windows-Kernel-Disk` and decoded 52-byte records ending in
`IssuingThreadId`. That field belongs to the CLASSIC kernel `DiskIo` record;
the manifest provider's template is `DiskNumber, IrpFlags, TransferSize,
Reserved, ByteOffset, FileObject, IORequestPacket, HighResResponseTime` and
stops there — confirmed by reading the `WEVT_TEMPLATE` resource out of
`Microsoft-Windows-System-Events.dll`, since `wevtutil` does not print
templates. Every record was therefore rejected, nothing was attributed, and
the column reported 0 % for every process while C: sat at 100 % — a fabricated
measurement. Two fixes: the events now come from `SystemIoProviderGuid` +
`SystemProcessProviderGuid` on a SYSTEM-LOGGER session (the documented way to
reach the kernel providers without seizing the global "NT Kernel Logger"),
where the classic layout the decoder already implements is the right one; and
a window with records but zero decodes is reported as UNKNOWN, never as zero,
with a rate-limited warning naming the rejected payload length. `DiskWindow`
carries `events_seen`/`events_decoded` through the broker (protocol v5) and
`--selfcheck` prints them, so this failure mode is diagnosable from one
headless run instead of a debugger.

Open: the `Kernel-Disk` payload offsets are proven only by
`integration::disk_trace_attributes_real_requests_to_the_issuing_process`,
which needs elevation. `--selfcheck` reports `process_disk_readings` and
`process_disk_busiest_pct` so an elevated headless run shows whether
attribution worked.

**Hover index.** Drawing a marker on the sample the readout describes exposed
a second bug: the index came from `t0 as f32 + frac * span as f32`, and
timestamps are epoch milliseconds (~1.8e12) where one f32 step is ~131 s. The
query time rounded to a fixed point regardless of the cursor, so the readout
described a sample far from the pointer and its age just grew as newer samples
arrived. `nearest_sample` does it in u64/f64 and rounds to the closer
neighbour; verified on the real window (left edge of a 40 s history "vor 40 s",
right edge "vor 8 s").

**Link speed was 8x too high.** `NetworkInfo::link_bps` is BITS per second
(`TransmitLinkSpeed`, Linux `sysfs` `speed`), but `format_mbit` multiplied by
eight to convert bytes, so a gigabit adapter read "8000 MBit/s".
`format_link_speed` takes bits.

Also in this pass: Ethernet throughput moved to KB/s / MB/s everywhere except
the negotiated link speed (now Gbps-aware); byte-rate charts scale to a
rounded maximum instead of the raw peak, which used to rescale the whole curve
every tick; and the Performance captions stopped hardcoding "60 seconds" while
the graph window is a setting.

## 2026-09-09 — Hidden UWP hosts left Apps for Background

User report: `SystemSettings.exe` with no visible window and
`TextInputHost.exe` showed under Apps. Root cause in `windows_enum.rs`: both
UWP window classes were exempt from the `DWMWA_CLOAKED` filter, so ANY visible
`Windows.UI.Core.CoreWindow` counted as an App window. Measured on this
machine (`EnumWindows` + `DwmGetWindowAttribute` probes):

| state | frame | app's CoreWindow |
| --- | --- | --- |
| open | uncloaked, hosts the CoreWindow as a child | child of the frame |
| minimized | uncloaked, iconic, no child | top-level, cloaked (2 = shell) |
| closed but resident | cloaked (2), no child | top-level, cloaked (2) |
| `TextInputHost.exe` | none | top-level, cloaked (2), permanently |

So a cloaked `CoreWindow` proves nothing on its own, and the minimized case
cannot be recovered from the frame alone (it belongs to the shared
`ApplicationFrameHost.exe`, and attributing it there put the broker into Apps).
Both halves do carry the same application user model id — the frame's through
`SHGetPropertyStoreForWindow` + `PKEY_AppUserModel_ID` (~43 µs measured), the
process's through `GetApplicationUserModelId` — so the enumeration now sorts
each window into a `WindowRole` (unit-tested, no Win32) and pairs detached
frames with parked CoreWindows by that id. The shell property store needs a COM
apartment, which the sampling thread now opens once per thread.

Verified live after the change: `TextInputHost.exe` → Background,
`ApplicationFrameHost.exe` → Background, Settings open or minimized →
`SystemSettings.exe` in Apps.

## 2026-09-09 — TaskMan v0.1.4 published

GitHub release `v0.1.4` (tag `906cdd9`) ships Windows x86_64, Windows ARM64,
Linux x86_64 and Linux ARM64 archives with SHA-256 checksums. Highlights:
native-aligned process classification and service-host names, exact user
identity (SID column, protected-service account inheritance), UWP app
attribution, and always-on-top above the Start menu via window band 16.
`python build.py --audit` (cargo-audit + gitleaks) was clean before publishing.
ARM64 artifacts are cross-built and build-verified (PE/ELF machine headers),
not run on ARM64 hardware here. `--all-targets` builds all four and is
documented in `build.md`.

## 2026-09-09 — Always-on-top vs the Start menu: fixed with window band 16

User report: the Start menu still appears above an always-on-top TaskMan
window. Measured on this machine with `GetWindowBand`:

- TaskMan (egui/winit `Window Class`): band 1 (`ZBID_DESKTOP`), `WS_EX_TOPMOST`.
- Taskbar (`Shell_TrayWnd`): band 1, `WS_EX_TOPMOST`.
- Start menu/search (`Windows.UI.Core.CoreWindow`, "Suche", SearchHost): band 6
  (`ZBID_IMMERSIVE_MOBILE`), `WS_EX_TOPMOST`.
- Native Task Manager (`TaskManagerWindow`): **band 16**, `WS_EX_TOPMOST`.

A controlled A/B capture (magenta topmost window, with and without the menu)
proved the acrylic is above band-1 topmost windows, and that a late
`SetWindowPos(HWND_TOPMOST)` does not help. The answer is what Task Manager
does: `Taskmgr.exe` contains the `CreateWindowInBand` string and resolves it
dynamically (it is not in the user32 import library).

Band behaviour measured by creating windows in each band: bands 0/1 are
normal, bands 2–14/17/18 are denied (`ERROR_ACCESS_DENIED`), 15/19/20 are
invalid, and **band 16 is allowed, implicitly `WS_EX_TOPMOST`, and cannot be
demoted** (`SetWindowBand` fails, `SetWindowPos(HWND_NOTOPMOST)` and clearing
the exstyle are both ignored). `SetWindowBand` also cannot promote an existing
window into band 16.

Fix: a one-change vendored winit patch (`vendor/winit`, `TASKMAN_WINDOW_BAND`)
creates the window with `CreateWindowInBand` when the persisted
`always_on_top` is enabled at startup. Because band 16 is permanently topmost,
the band is startup-only; runtime toggling keeps ordinary band-1 topmost and
the settings dialog shows the existing "Takes effect at the next start." hint.
Verified live: `always_on_top=on` → band 16 and the app stays above the open
Start menu (pixel-identical overlap before/after); `always_on_top=off` → band
1, normal stacking. `tools/check-fork.ps1` now also gates `vendor/winit`
(clippy + tests; fmt is skipped because upstream's tree is not formatted with
this nightly rustfmt).

## 2026-09-09 — Native-aligned classification, service-host names, exact user identity

A side-by-side comparison with native Windows 11 Task Manager (screenshot)
showed the classifier was over-broad and several native conveniences were
missing.

1. **"Microsoft-signed under %SystemRoot%" was the wrong entry ticket.** The
   2026-09-07 positive-ownership rule fixed third-party services but pushed
   `WmiPrvSE`, SmartScreen, `fontdrvhost`, `TextInputHost`, `WUDFHost`, the
   shell brokers and windowless PowerShell/cmd into the Windows group; native
   keeps all of them in Background. `classify.rs` now mirrors the documented
   native rule: kernel pseudo-names, core OS images or `IsProcessCritical` →
   System, visible window → App, else Background. The Microsoft/path test
   survives only as a spoof guard (`windows_owned_evidence`: wrong location →
   `Some(false)`, missing metadata → `None` so protected core images are not
   rejected). The core-image list moved into
   `tm_core::classify::is_core_os_image`; the sampler's tree boundaries and
   the Processes page share it, removing three drifting copies. The sampler
   queries `IsProcessCritical` in the 10 s attribute cache, and the platform
   no longer marks a System subtree as App during propagation.
2. **Service hosts are named without spamming the list.** `ServiceCatalog`
   gained `names_by_pid` (hosted service display names) and `accounts_by_pid`;
   `ProcessEntry.service_name` is finally populated. `svchost.exe` rows render
   `Service Host: <service>`, the collapsed run row is `Service Host [N]` with
   every hosted service in its tooltip, and expanded children stay
   distinguishable. Search and Process Properties already consumed the field.
3. **Exact user identity.** `token_identity` returns the string SID with the
   account name; `well_known_sid_name` canonicalizes S-1-5-18/19/20 to English
   names on localized systems, and `well_known_sid_for_account` supplies the
   SID for session-0 hosts whose token cannot be opened. The fallback chain
   now uses the per-PID service account BEFORE the blanket session-0 →
   "SYSTEM" guess, so NETWORK SERVICE/LOCAL SERVICE hosts no longer all read
   "SYSTEM". Details has an optional User SID column and Process Properties a
   User SID row. Protected processes (`csrss`, `dwm`, `winlogon`, NVIDIA's
   session container) refuse `OpenProcess` and even `WTSEnumerateProcessesEx`
   returns a null SID for them; `inherit_same_image_service_accounts` fills a
   windowless same-image child from its catalogued service parent, which is
   how the session-1 `NVDisplay.Container.exe` now reports SYSTEM/S-1-5-18.
4. **UWP apps are attributed to the app, not the broker.** A visible
   `ApplicationFrameWindow` is attributed to the process owning its hosted
   `Windows.UI.Core.CoreWindow`; a cloaked frame without a hosted child is a
   ghost of a suspended app and is ignored, while the suspended app's own
   top-level cloaked CoreWindow keeps it in Apps with status Suspended.
   `ApplicationFrameHost.exe` no longer shows as an App row.
5. **Services → Go to details** (reverse of the existing Processes → Go to
   service(s)).
6. Fixed pre-existing rustfmt drift in `app_ui.rs`/`explorer_restart.rs` that
   would have failed `build.py --check`.

Tests: classifier unit tests including the screenshot-derived Background set
and spoof/critical cases; the sampler classification test now expects WmiPrvSE
and windowless PowerShell in Background; live integration invariants (core
images System, Microsoft background machinery never System unless critical, at
least one service name present); service catalog asserts per-PID names and
accounts; svchost display-name regression test; Details column sort test
covers UserSid. Verified live: `svchost` hosts show SYSTEM/LOCAL SERVICE/
NETWORK SERVICE/user accounts with SIDs, suspended Settings appears as an App
with status Suspended, and ApplicationFrameHost/RuntimeBroker/taskhostw are
Background. `python build.py --check` green (fmt, clippy, workspace tests,
fork gate, release + Linux packaging); `taskman --selfcheck` sane.

## 2026-09-08 — Linux parity fixes and the first GUI-capable Linux release

Validating the v0.1.2 Linux artifact under WSLg surfaced three issues:

1. **Threads were listed as processes.** sysinfo 0.39 keeps `tasks` enabled
   even in `ProcessRefreshKind::nothing()`, so every Linux task was inserted
   into the process map. The Processes page showed `tm-engine`,
   `dconf worker`, `gmain`, ... each repeating the parent's memory, and the
   selfcheck counted 61 "processes" on a 39-process / 54-thread system. The
   collector now skips `thread_kind().is_some()` entries; `p.tasks()` still
   feeds the Details thread count, and the leader is added back so it matches
   Windows.
2. **The musl artifact could not run the GUI.** A fully static musl binary
   has no `dlopen` at all (`Dynamic loading not supported`), and winit needs
   it to load Wayland/X11. `build.py` now warns when it falls back to that
   path; `cargo-zigbuild` produces a GUI-capable glibc artifact.
3. **Sub-pixel text on Linux.** The fork's LCD path was already there behind
   `TASKMAN_SUBPIXEL=1`; `text_rendering::query` now reads fontconfig's
   `rgba` (rgb/bgr only, grayscale for unknown/none/vertical orders), so the
   app follows the desktop's own setting. Verified under WSLg at scale 1 and
   at app-level 150% (`WINIT_X11_SCALE_FACTOR=1.5`): text chroma 0 -> ~156
   with balanced red/blue fringes. WSLg itself is Weston/RDP, not
   Plasma/GNOME; its limits (no fractional scaling, `[WARN:COPY MODE]`
   shared-memory bug, no GPU) are documented in `debug-tools.md`.

v0.1.3 ships the glibc artifact and these fixes; the v0.1.2 Linux asset was
musl and GUI-incapable.

## 2026-09-08 — Scroll anchoring and deterministic process ordering

Two independent jumpiness sources on the Processes/Details pages:

1. **Raw pixel scroll offsets across model rebuilds.** The display model is
   rebuilt on every sample; a process spawning/exiting above the viewport (or
   a tree node expanding) shifted every visible row while the offset stayed
   put. `tablekit::scrolled_rows` now takes an optional `ScrollAnchor`
   (`model_changed`, `prefer_key`, stable per-row `key_of`) and re-derives
   the vertical offset from the identities of the previously visible rows,
   preferring the selected row while it is on screen (a spawn between the
   viewport top and the selection must not push the row the user is tracking
   out of view), then falling back to the top row and the next surviving one.
   A row that moved further than a screenful is treated as a reorder
   (sort/search) and left alone, so anchoring can never teleport the
   viewport. `focus_row` still wins.
   Wired for Processes (`pid`, `start_epoch_s`, aggregate flag, group-header
   section) and Details (`pid`, `start_epoch_s`); other tabs pass `None`.
   The Processes key includes the aggregate flag because an expanded family
   renders its virtual head and the concrete representative with the same
   pid/start; without it anchoring could pin the wrong row and shift the
   list by one.
2. **Snapshot-order-dependent sorting.** sysinfo hands the sampler a hash-map
   order that changes between ticks. Processes' stable sorts had no
   tie-breakers, so equal-valued (usually 0% CPU) and same-named rows swapped
   places every sample. `build_display_rows` now sorts the snapshot by
   `(start_epoch_s, pid)` once, and `sort_entries`/`sort_blocks_globally`
   tie-break by pid.

Tests: `insertion/removal_above_the_viewport_keeps_the_same_row_pinned`,
`vanished_top_row_falls_back_to_the_next_visible_row`,
`insertion_between_top_and_selection_keeps_the_selection_in_view`,
`large_reorder_is_not_followed`, `fully_replaced_model_leaves_the_offset_alone`,
`equal_value_rows_keep_a_stable_order_across_snapshot_permutations`,
`expanded_family_aggregate_and_concrete_rows_have_distinct_anchor_keys`.

## 2026-09-08 — Security audit pass (broker, FFI alignment, hardening gate)

Full-repository security review. Fixed and verified in this pass:

1. **Broker SID authorization (medium).** `--core-service-user=<sid>` accepted
   any well-formed SID, so a crafted elevated helper invocation could name a
   group/alias (`S-1-5-32-545`, `S-1-1-0`) and widen the pipe ACE. `install`
   now requires `LookupAccountSidW` to classify the SID as `SidTypeUser`; the
   GUI's own token SID is unaffected. Test: `only_user_account_sids_may_be_authorized`.
2. **Service log-directory DoS (medium).** `prepare_service_log_dir` returned
   `Err` for any unexpected entry under `%ProgramData%\TaskMan\logs`, and the
   service refused to run the broker on that error. A standard user can plant
   such an entry before the first install (ProgramData is Users-writable), so
   the LocalSystem control plane could be disabled until an admin cleaned it.
   Verification is now a `None` result that disables file logging only; the
   broker always starts. Tests: `foreign_log_entries_disable_file_logging_without_failing`.
3. **Misaligned Win32 scratch buffers (low, UB).** `Vec<u8>` (align 1) was
   cast to `ENUM_SERVICE_STATUS_PROCESSW`, `QUERY_SERVICE_CONFIGW`,
   `SERVICE_DESCRIPTIONW`, `SERVICE_STATUS_PROCESS`,
   `PDH_FMT_COUNTERVALUE_ITEM_W` and `SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX`.
   New `win/aligned.rs` backs every such buffer; `net_info` already did this
   with `Vec<u64>` and now uses the shared type.
4. **Binary hardening.** Windows release builds are now CET-compatible
   (`/CETCOMPAT`) in addition to CFG/GS/ASLR/NX; verified with `dumpbin`.
   Linux musl artifact remains static PIE + full RELRO/BIND_NOW + NX stack.
5. **Gate/tooling.** `python build.py --audit` runs `cargo audit` and
   `gitleaks detect` (findings fail; missing scanners warn). `tm-core` now
   denies `unsafe_code`; `tm-platform`/`tm-app` deny `unsafe_op_in_unsafe_fn`.
   Added deterministic no-panic tests for the broker frame/JSON parser, the
   SMBIOS walker and the PDH instance parser.

Not changed (documented residual/coverage items): same-user GUI injection can
borrow broker authority (needs signing + CIG), synchronous broker I/O can be
stalled by an already-authenticated client, no fuzzing harness, and the
Windows ARM64 / macOS / Linux ARM64 targets are not built on this host.

## 2026-09-08 — Dual-target release build (Windows + Linux) out of the box

1. **`build.py` no longer skips Linux.** Resolution order: `cross`, then
   `cargo-zigbuild` (glibc, existing behavior), then a self-contained
   `x86_64-unknown-linux-musl` build linked by the toolchain's `rust-lld`.
   The fallback needs only the rustup std component (installed on demand) and
   produces a static PIE; artifact name gains a `-musl` suffix.
2. **Root cause of the never-working cross link found:**
   `crates/tm-app/build.rs` gated the Windows `app.res` link argument on the
   HOST (`cfg!(target_os = "windows")`), so a Windows host always attached it
   to Linux links. Now keyed on `CARGO_CFG_TARGET_OS` (the real target).
3. **`libloading` emits `-ldl` on musl**, where dlopen lives in libc;
   build.py creates an empty `target/cross-stubs/libdl.a` to satisfy the
   linker lookup without defining symbols.
4. **Verified:** `python build.py` produces both archives; the Linux binary
   reports `ok:true` from `--selfcheck` under WSL2 (72 processes, linux
   backend) and exits cleanly when no display is present.

## 2026-09-08 — Full-repository audit: non-Windows build fix and gate hardening

1. **Both non-Windows backends were broken and invisible to the gate.**
   `cargo check --target x86_64-unknown-linux-gnu` failed in `tm-app`
   (`sync_title_bar` used the Windows-only `raw_window_handle` dep;
   `dispatch_core_service_*` referenced a Windows-only field) and
   `--target aarch64-apple-darwin` failed in `tm-platform` (missing
   `kernel_pct` / `per_core_kernel_pct` in the macOS `CpuInfo`). Fixed by
   gating the Windows-only code and supplying the honest unknown defaults.
2. **The gate can now see platform code.** `build.py --check` runs
   `cargo check --workspace --target <non-host>` for both targets when their
   std is installed, and `.github/workflows/ci.yml` installs both targets.
3. **MSRV was wrong.** `let`-chains, `slice::as_chunks` and
   `is_multiple_of` require Rust 1.88, not the declared 1.85; Cargo.toml,
   README and codestyle now say 1.88.
4. **Release artifacts leaked the checkout path.** `--remap-path-prefix`
   stripped `C:\Users\<user>` but left `Programme\build\tmproject\target\...`
   strings from generated bindings; the checkout root is now remapped too.
5. **Bounded config/history reads** (`config.ini`, legacy `settings.json`,
   app-history JSON) and **word-aligned ETW `EVENT_TRACE_PROPERTIES`**
   (a `Vec<u8>` cast to an 8-byte-aligned struct was technically UB).

## 2026-09-08 — Agent instructions and audit-tooling template alignment

1. **Security-audit inventory generalized.** `llm-wiki/debug-tools-security-audit.md`
   now matches the current upstream template's generic core rules, tool/path
   precedence, crash-dump/symbol guidance, runtime-tracing rules, coverage-gap
   reporting, and project-specific-additions guidance. The captureengine-era
   DX12/hook-DLL content and cross-project paths are gone.
2. **TaskMan audit facts added.** Verified local tool paths, the release-symbol
   policy (`strip = "symbols"`; dev/test keep line tables), broker invariants,
   sensitive artifacts, and the non-mutating `taskman-service.exe --selfcheck`.
3. **Agent rules merged.** `AGENTS.md`/`CLAUDE.md` gained the missing upstream
   rules: large-output context hygiene, exit-status-is-not-success,
   compatibility/contract preservation, focused behavioral diffs, low-value-test
   guidance, binary-analysis mutation rules, and the fuller llm-wiki workflow.
   `CLAUDE.md` is now byte-identical (LF) to `AGENTS.md`.
4. **Wiki hygiene.** `codestyle.md` placeholders were replaced with tool-backed
   rules, and `index.md` now lists `module-unload.md`, `render-pipeline.md`, and
   `debug-tools-security-audit.md`.

## 2026-09-08 — CI gate red on main; blind-spot fix

Main CI (quality gate) failed on several consecutive bot-merge commits:
missing `cargo fmt` across four files, a test still calling the removed
`Subtree::values()` accessor (E0599 in
`crates/tm-app/src/tabs/processes.rs`), and clippy
`redundant_closure_call` in `crates/tm-platform/src/win/process_ops.rs`. Fixed by
formatting, switching the test to direct map indexing (`st.values[&1][0]`;
asserted pids always have rollups, so no defaulting is lost), and replacing the
IIFE with a plain block (no early returns). Root cause of the landings:
branch CI runs were seconds-long no-ops that still reported success —
see build.md § CI. Commit `c218a97`; CI green after.

## 2026-09-08 — Process groups use virtual aggregate parents

1. **Group totals are no longer disguised as a real process.** Every expandable
   Apps/Background/Windows family has a presentation-only parent row whose CPU,
   memory, disk and network values are the sum of its represented members.
2. **Expansion reveals every concrete process.** The process that previously
   doubled as the group header is now the first real child; all real rows show
   only their own resource values. App descendants retain hierarchy indentation,
   while flat Background/Windows families remain one level deep.
3. **Search is unambiguously per-process.** Search results are flat concrete rows
   and therefore sort/render by each process's own metrics rather than a hidden
   subtree aggregate.
4. Regression coverage pins aggregate-vs-root resource values, full expanded
   membership (including svchost groups), hierarchy depth, resource-sorted block
   attachment and collapsed End-task membership.

## 2026-09-08 — Strict topmost and native Task Manager escape hatch

1. **Always-on-top now wins against shell topmost surfaces.** The toolkit's
   one-shot window level is still set, but Windows also installs out-of-context
   WinEvent hooks for foreground, top-level show and z-order reorder events.
   While enabled and visible, TaskMan reinserts itself at `HWND_TOPMOST` with
   `SWP_NOACTIVATE`, preventing band-1 shell surfaces (the taskbar) from
   remaining above it without stealing input focus. No polling or timing retry
   is involved. Correction (2026-09-09): the Start menu/search lives in window
   band 6 and is always above every normal window regardless of topmost; see
   the 2026-09-09 entry.
2. **The built-in Task Manager remains explicitly reachable.** Every tab's top
   command bar has a Windows Task Manager action. It starts the System32
   `Taskmgr.exe` with `DEBUG_ONLY_THIS_PROCESS`, which deliberately bypasses
   that image's IFEO Debugger registration, and immediately calls
   `DebugActiveProcessStop`; TaskMan's replacement registration never has to be
   removed or temporarily weakened.
3. **Detach failure is fail-closed.** If Windows cannot detach the debug-created
   Task Manager, the just-created process is terminated instead of being left
   suspended on a long-lived action-executor debugger connection.

## 2026-09-07 — Minimized windows cannot overwrite restore placement

1. **Ignore iconic placement samples.** While `remember_window` is enabled, the
   native shell no longer updates the saved position or maximized flag from a
   minimized viewport. Windows may report iconic/off-screen geometry for a
   minimized window while `maximized` is false; persisting that value can make a
   later taskbar restore appear broken because the window reopens off-screen.
2. **Normal/maximized semantics are unchanged.** Normal windows still update the
   remembered desktop position; maximized windows still preserve the last normal
   position while updating only the maximized flag.

## 2026-09-07 — Windows-process classification requires OS ownership

1. **Session 0 is not Windows ownership.** The sampler no longer classifies every
   Session-0 process or every descendant of `services.exe`/another system process
   as a Windows process. Third-party services such as Battle.net Update Agent and
   Steam helpers now remain Background unless they independently qualify as Apps.
2. **Positive first-party evidence.** Core system image names remain System. Other
   Windows components require Microsoft file-version metadata plus a Windows-owned
   executable path (`%SystemRoot%`, with Defender's first-party ProgramData/Program
   Files locations covered explicitly). A vendor binary in System32 is not enough.
3. **Foreground visibility is preserved.** A visible non-Windows executable launched
   below a system/service process is classified as an App rather than being hidden in
   the Windows section. Regression coverage includes Battle.net, Steam WebHelper,
   TaskMan itself, WMI Provider Host, Defender/path ownership, and vendor-in-System32.

    ## 2026-09-07 — Processes grouping favors visibility and groups service hosts

    1. **Service-host clutter is collapsible.** Repeated `svchost.exe` siblings under
       `services.exe` now use the existing repeat-run grouping path, yielding one
       expandable aggregate row instead of dozens of indistinguishable top-level rows.
    2. **Publisher is evidence, not ownership.** Different-image background processes
       only fold on a same-publisher match when the child also looks like an actual
       helper/renderer/service component. Arbitrary idle programs from the same vendor
       remain visible.
    3. **Foreground apps fail open to visibility.** Different visible executables no
       longer merge when publisher metadata is missing. Common app/game launchers keep
       their UI/helper processes but launched titles (including a same-publisher game
       such as Fortnite under Epic Games Launcher) become independent top-level App rows.
    4. Regression coverage pins service-host collapse/expansion, same-publisher
       non-helper separation, launcher/game separation, missing-publisher separation,
       and the existing Steam helper behavior.

    ## 2026-09-07 — Bulk process suspend/resume, toolbar controls, and visual indicators

1. **Bulk suspend/resume execution:** Previously in `details.rs`, the context menu
   suspend action only targeted `p.pid`, ignoring any active multi-selection. Added
   `TaskManApp::set_suspended_batch` and updated `details.rs` and `processes.rs`
   context menus to route all selected processes through a single refreshing action,
   displaying dynamic counts (e.g. `Suspend (N)` / `Resume (N)`).
2. **Dedicated Resume / Suspend toolbar controls:** Added command buttons in both
   the Processes and Details tab headers next to "End task", dynamically toggling
   between `Icon::Play` ("Resume") and `Icon::Pause` ("Suspend") according to the
   primary selected process's state (`primary_suspended`).
3. **Suspended process detection root-cause fix:** Unelevated sessions often receive
   `p.start_time() == 0` from sysinfo. In `crates/tm-platform/src/win/sampler.rs`,
   `is_suspended` was called before `entry.start_epoch_s` was resolved against the
   kernel process table fallback (`start_epoch_of`), passing `Some(0)` which always
   failed identity verification against native start times. Fixed by resolving
   `entry.start_epoch_s` first and tolerating 1-second second-boundary truncation
   skew in `CpuLoadAccountant::is_suspended`.
4. **Visual indicators on Details and Processes pages:**
   - On the Details page, the Status column now renders `Icon::Pause` with
     `pal.warn_orange` (and `Icon::Leaf` for efficiency mode) with status text styled
     in `pal.warn_orange`, making suspended processes instantly recognizable.
   - On the Processes page, group headers (application families and repeat runs) now
     roll up child process status (`Subtree` and `emit_flat_with_family_groups`),
     ensuring collapsed rows display the orange pause glyph whenever descendants
     are suspended.

## 2026-09-07 — End-task dialog spatial layout and Tab focus navigation fix

1. **End-task dialog Tab focus navigation:** The dialog previously compared
   `ctx.memory(|m| m.focused())` against manually generated IDs (`cancel_id` and
   `end_id`) that never matched the button responses' internal widget IDs.
   Consequently, every frame without an active Tab press evaluated focus to `None`,
   instantly resetting selection back to `Some(true)` (End task) and making Tab
   appear broken/frozen. The dialog now persists selection in `ctx.data` temp
   storage (`Id::new("end_task_dialog_focus_end")`), evaluates `Shift+Tab` before
   `Tab` (per egui's modifier matching rules), supports Left/Right arrow
   directionality, and clears temp state when the dialog closes.
2. **End-task dialog space utilization and layout polish:**
   - Previously `ScrollArea::vertical()` used default `auto_shrink([true, true])`,
     shrinking the scroll area to the length of the shortest process text and
     leaving the scrollbar stranded at x ~ 125px with ~280px of blank space to its
     right.
   - The process list is now housed in a framed container (`auto_shrink([false, true])`)
     with a distinct sunken background and border. Process names are left-aligned
     and PIDs are right-aligned in muted text (`pal.text_dim`), with the scrollbar
     cleanly docked at the container's right edge.
   - Max height adjusted to 168px to prevent half-clipped rows at default line height.
   - Buttons standardized to 85x24 min-size and right-aligned at the bottom right
     with high-contrast focus strokes (crisp white focus border on the accent End
     task button, accent-colored stroke on Cancel).
3. **Diagnostics:** Added `TASKMAN_DIALOG=end_task` for headless UI test captures,
   and added `--single-instance-handoff` to `tools/capture_exact.ps1`.

## 2026-09-07 — Keyboard context menu fix (`PopupAnchor::Position` memory persistence)

1. **Context menu via Menu key / Shift+F10 opens and stays open.** The previous
   implementation opened the popup on frame 1 using `Popup::at_position(...)`, but
   `Popup::show` only recorded the position in egui memory (`open_popup_at`) for
   `PopupAnchor::PointerFixed`. For `PopupAnchor::Position(pos)`, it fell back to
   `Popup::open_id` which saved `None` as the position. On frame 2, when
   `keyboard_open` was false and `context_menu_kb` defaulted back to
   `Popup::context_menu(resp)` (`PointerFixed`), `Popup::position_of_id` returned
   `None`, causing the popup to drop its anchor rect and fail to render, closing
   instantly. `Popup::show` now stores `Some(pos)` for `PopupAnchor::Position(pos)`,
   allowing keyboard-anchored menus to remain open and render properly.
2. **Gate keyboard menu requests:** `menu::keyboard_menu_requested` now guards
   with `!ctx.egui_wants_keyboard_input()`, ensuring text inputs (e.g. search fields)
   do not inadvertently trigger table row context menus.

## 2026-09-07 — CPU renderer and application pipeline optimizations for lowest CPU load

1. **`egui_software` rasterizer optimizations:**
   - **Gamma table lookups hoisted:** added `linear_u16` and `blend_channel_fast`
     so text blitting computes source linear luminance once per glyph instead of
     per pixel.
   - **Text blitter scanline slicing:** hoisted horizontal bounds clamping
     once per scanline and replaced per-pixel `get()` and option unwraps with
     direct slice zipping (`src_slice.iter().zip(dst_slice.iter_mut())`).
   - **Solid-glyph interior fast-bypass:** when glyph coverage channels are all 255
     and glyph alpha is 255, writes packed RGB directly, skipping blending and LUT lookups.
   - **Uniform triangle rasterization fast paths:** uniform untextured geometry
     (chart area fills, line strokes, polygons) skips barycentrics and float shading;
     opaque triangles write directly, and translucent triangles hoist color math out
     of the pixel loop. Edge testing evaluates all three edge signs with a single
     branch via `(w[0] | w[1] | w[2]) >= 0`.
   - **Recursive `Shape::Vec` flattening:** container frames (`egui::Frame`) wrap
     backgrounds in `Shape::Vec`; flattening them enables `span::fast_rect` rather
     than tessellating into Gouraud meshes.
   - **Target memset clear:** `Target::fill_rect` delegates full-target fills to
     `slice::fill` (vectorized memset).
   - **Buffer reuse:** reused batch vectors across flushes in `paint_shapes` with
     `.drain(..)` instead of allocating new vectors.

2. **Taskman application pipeline optimizations:**
   - **Processes tab row borrow:** borrowed cached rows (`&cache.rows`) instead of
     cloning hundreds of `DisplayRow` instances with owned strings on every frame.
   - **History deque bulk drain:** replaced O(N^2) `history.remove(0)` loop with
     O(N) `drain(..excess)`. Removed duplicate `poll_engine` call in `ui()`.
   - **IconCache O(1) lookup:** replaced per-lookup hashmap `retain` scan with lazy
     TTL check on failed entries; moved general retain to `drain_results`. Removed
     redundant manual premultiplication loop before `from_rgba_unmultiplied`.

## 2026-09-06 (later still yet) — the Menu key works; Tab cycles the End-task dialog; advapi32's 64 references are real

1. **The keyboard Menu/Application key now opens the selected row's context
   menu.** It never reached the app at all: egui's `Key` enum has no entry
   for it and egui-winit dropped it in BOTH key translations, so the press
   died in the backend. Fork divergence #5: `Key::ContextMenu` in egui
   (variant + `Key::ALL` + name mapping) and both `NamedKey::ContextMenu`
   and `KeyCode::ContextMenu` mapped in egui-winit. In the app,
   `menu::keyboard_menu_requested` (Menu key, or Shift+F10) plus
   `menu::context_menu_kb` force the selected row's OWN popup open, anchored
   to the row (`Popup::at_position`) — the keyboard counterpart of a right
   click, wired into Processes, Details, Users, Services, Startup and the
   Modules dialog. Because row popups are identity-keyed, the keyboard menu
   is bound to the selection, not a slot.
2. **Tab in the End-task dialog moved egui's focus, not our selection.** The
   dialog toggled its own focus variable on Tab, but the button ALSO held
   real egui focus (request_focus), so pressing Tab moved egui's built-in
   navigation to a different widget entirely; the memory-derived selection
   read `None`, defaulted back to End task, and the ring looked frozen. The
   dialog now CONSUMES Tab/Shift+Tab/ArrowLeft/ArrowRight/Enter/Space/Escape
   (`InputState::consume_key`) and decides inside the window with its own
   focus state — egui's navigation and focused-button Enter activation can
   no longer race it.
3. **"Released 64 references, but advapi32.dll is still in use" is the
   feature working, correctly bounded.** Every statically-importing module
   holds one loader reference; mspaint's module graph references advapi32
   from far more than 64 modules, so the refcount never reaches zero — and
   should not: the DLL is load-bearing. The budget stays at 64 on purpose;
   pushing further turns a protected process into a guaranteed crash.
   Repeated attempts are harmless while references remain (nothing unmaps
   below zero), so the only cost of the bound is honesty about futility.

## 2026-09-06 (later yet) — unload repeats FreeLibrary until the module actually leaves

Live test showed the honest-outcome change was not enough: unloading
`AcGenral.DLL` (the AppCompat shim DLL Windows injects) reported "reference
released, but still in use". That is the correct description of ONE
FreeLibrary call — it drops exactly one loader reference, and injected or
statically-imported DLLs are referenced several times, so a single call
almost never unmaps anything. "Forced unload" now means what it says: the
platform repeats the remote FreeLibrary — re-validating process identity and
the exact module base/path on every attempt — until the module is gone or
`MAX_FREE_LIBRARY_CALLS` (64) is spent. The outcome carries the honest count
(`ModuleUnloadOutcome { still_mapped, released }`, over the broker as
`BrokerValue::ModuleUnload { still_mapped, released }`), and the still-mapped
toast says how many references were dropped ("Released 12 references, but X
is still in use…") so a futile attempt is distinguishable from a pinned
module the target refuses to let go (FreeLibrary returning FALSE still fails
loudly).

`module_unload_releases_every_reference_and_reports_the_count` pins the
semantics end-to-end on the real loader: a spawned child is made to load
`WTSAPI32.dll` three times through remote `LoadLibraryW` calls (the exact
mirror of the unload's remote FreeLibrary), and one unload request must
release exactly 3 references and remove the module. The test drives the
LOCAL `WinActions` surface on purpose — it tests loader semantics, not the
broker transport (and the installed service generation lags the workspace
build; its `ModuleUnload` answer lacks `released`, which the new client
reports as an undecidable error until the service is upgraded through the
normal repair/upgrade flow).

## 2026-09-06 (later still) — row context menus are bound to the row's owner, not its slot

User report: right-clicking a process on Details opened a menu that changed
to a DIFFERENT process when the list re-sorted (a start brought the menu's
"process" out from under it). Root cause: `TmTable::row` derived its response
id from egui's auto-id stream — effectively the row's position — and
`egui::Popup::context_menu` keys the open popup to the response id. While a
menu is open, the closure is re-run every frame for the row now occupying
that id; in a re-sorting list that is a different process. The same defect
existed on Processes, Users, Services, Startup, Modules and App history.

Fix at the root: `TmTable::row` now takes a `key` identifying the row's
OWNER — `(pid, start_epoch_s)` on the process tables, module base address,
service name, startup item id, session id (users' per-user app rows key as
`(session, name)`, which is why `URow::App` now carries the session) — and
allocates the row response through `ui.interact(rect, Id::new(table.id).with(key), …)`.
The id follows the process through re-sorts and re-insertions, so an open
menu keeps showing the process it was opened on (native menus freeze their
contents; ours keeps live values for the same process — both read as
"sticking with the click"). If the row's owner exits or scrolls out of the
virtualization window, the menu goes away with it instead of describing a
stranger. Table ids are prefixed per table, so keys only need to be unique
WITHIN one table.

## 2026-09-06 (later) — unload is the user's call; the outcome crosses the pipe as data; tray menu opens above the taskbar

Follow-up to the same-day unload/tray work, from live-testing feedback:

1. **No protected-module concept (product decision).** The user rejected the
   remaining gate outright: WHICH module to unload is their choice. The
   `unloadable` flag is gone from `ProcessModule`, `module_is_unloadable` and
   `is_windows_owned_path(_under)` are deleted, the unload button is enabled
   for every selected module (only the platform capability and a busy action
   grey it out), and `ModuleProtected` is retired from i18n. What deliberately
   REMAINS is technical or process-level, not module policy: cross-architecture
   refusal (a 64-bit FreeLibrary address cannot enter a 32-bit process),
   the broker's critical/system/requesting-GUI target rules, TaskMan not
   acting on itself, exact identity + base/path revalidation at action time,
   and the confirmation dialog with its crash warning. Attempting advapi32
   now runs — and will honestly report StillMapped while mspaint holds its
   static imports.
2. **The still-mapped outcome became data, not an error.** Reporting it as a
   red "platform API failure: core service: platform API failure: FreeLibrary:
   …" read as a malfunction, but a released reference with the module still
   loaded is the EXPECTED result for implicitly-linked DLLs. The platform
   trait now returns `Result<ModuleUnloadOutcome>` (`Unmapped` |
   `StillMapped`); the broker answers the new `BrokerValue::ModuleUnload
   { still_mapped }` variant and the client decodes it (an older service's
   `Unit` decodes to an honest "undecidable" error — no fabricated success;
   PROTOCOL_VERSION stays 2, so the mixed pair still handshakes). The UI
   words the states apart: "Module unloaded: X" (then refreshes the list)
   vs. the new neutral `ModuleStillMappedMsg` (list untouched — it is
   already accurate).
3. **Tray menu position.** `TPM_BOTTOMALIGN` anchors the popup's bottom edge
   at the cursor — which sits inside the taskbar — so the menu grows upward
   over the taskbar instead of overlapping it (topmost/focus contention the
   user reported).

## 2026-09-06 — the tray owns a thread; unload stops lying; two tofu glyphs

1. **Tray menu freezes — the modal loop left the UI thread.** The tray icon's
   window was created on the UI thread (that is where `TrayShell::new` ran),
   so the right-click handler — and with it `TrackPopupMenuEx` — executed
   inside the egui/winit dispatch. A popup menu pumps a nested modal message
   loop; running that loop inside the frame pipeline re-entered winit/egui
   with repaints, input and tray messages in orders the framework is not
   built for: the "cursed" freezes. The tray now lives on its own thread
   (`tray_thread_main`): icon creation, the event handler, the menu and a
   plain `GetMessageW` loop all run there; the UI side (`TrayShell`) is only
   atomics and `PostThreadMessageW`. Visibility handoff protocol: the UI
   stores the desired state BEFORE reading the thread id; the thread
   publishes its id BEFORE its first read; both SeqCst — the two overlap
   windows are mutually exclusive, and a lost post (queue not up yet) is
   covered by the thread's initial apply. `TrayShell::shutdown` posts
   `WM_QUIT` so the thread drops the icon on its own thread — the only
   thread allowed to `DestroyWindow` it — instead of leaving a ghost icon.
   `MAIN_HWND` (used only as the old menu's hwnd fallback) is gone. Note for
   live testing: menu behavior is GUI-only and cannot be verified headlessly.
2. **Module unload stopped claiming false success.** `FreeLibrary` releases
   ONE loader reference and returns TRUE even when remaining references keep
   the module mapped — which is the norm for implicitly-linked DLLs. The old
   code toasted "module unloaded" anyway, then re-listed and showed the
   module still sitting there: the feature read as broken. Now the platform
   (`unload_process_module`) re-enumerates after the call and returns an
   honest error ("still in use and remains loaded") when it is; the UI keeps
   its existing list in that case instead of refreshing. Two related UI
   fixes: a failed re-list after a SUCCESSFUL unload no longer throws the
   dialog into its error state (unelevated GUIs cannot list an elevated
   target's modules at all — the list just stays), and a selection whose
   module vanished falls back to the first unloadable so the button does not
   dead-end right after a success.
3. **Module unload refuses Windows-owned locations again — precisely.**
   Commit 607c9a3 had removed the `\windows\` path check (case-sensitive
   substring) to unblock non-core DLLs; the side effect was that any
   `C:\Windows\System32\*.dll` passed the unloadability gate, and FreeLibrary
   actually unmapping one can tear the target down. The replacement
   (`is_windows_owned_path_under`) refuses the Windows root directory and
   the `system32\` (incl. DriverStore), `syswow64\` and `winsxs\` trees —
   case-insensitive, separator-normalized, and not fooled by a prefix string
   (`C:\Windows-esque\` stays allowed). `ext-ms-*` proxies are refused by
   name next to `api-ms-win-*`. Everything outside those roots (app plugins,
   hooks, overlays) remains unloadable, keeping the intent of the removal.
4. **The GPU caption's dropdown marker was a tofu box.** `"{title} ▾, {window}"`
   typed U+25BE, which Segoe UI Variable has no glyph for — the placeholder
   square in the user's screenshot. The marker is now hand-painted
   (`caption_dropdown`), the same policy as `menu.rs`'s tick: never rely on
   font coverage for an affordance. The Details filter chip's `✕` (U+2715)
   had the same latent problem and became `×` (U+00D7, Latin-1, in every
   font).

Verified: `python build.py --check` (fmt, clippy `-D warnings`, all
workspace tests incl. Windows integration, fork gate) plus release build and
`--selfcheck` (`"ok":true`). Menu, restore and unload outcome are GUI-runtime
behavior and await user confirmation.

## 2026-09-06 — ServiceCatalog, DriverStore paths, multi-session process user/elevation & platform resolution

Comprehensive resolution of platform (32-bit vs 64-bit), user name, elevation, and UAC virtualization for protected processes and service helpers across sessions (Session 0 and interactive sessions):

1. **Service catalog & DriverStore path resolution**:
   - Replaced PID-only service paths with `ServiceCatalog` (`paths_by_pid`, `paths_by_name`, `accounts_by_name`).
   - SCM queries for active services retrieve full binary paths (including driver repositories like `System32\DriverStore\FileRepository\...\NVDisplay.Container.exe`) and service accounts (`LocalSystem` -> `SYSTEM`, `LocalService`, `NetworkService`).
   - Seeded `known_paths_by_name` with `service_catalog` and all paths discovered by `sysinfo`. Secondary / session-helper instances (such as `NVDisplay.Container.exe` in Session 1 spawned by Session 0) now resolve their exact executable path on disk, enabling `pe_is_wow64` and `version::query`.
2. **Multi-session process user name attribution**:
   - `csrss.exe` and `winlogon.exe` always run as `SYSTEM` in all sessions.
   - `dwm.exe` runs as `DWM-<session_id>` (e.g. `DWM-1`).
   - `fontdrvhost.exe` runs as `UMFD-<session_id>` (e.g. `UMFD-0`, `UMFD-1`).
   - Service helpers in non-zero sessions (`GameInputSvc.exe`, `NVDisplay.Container.exe`) resolve account names via `service_catalog.accounts_by_name`.
3. **Elevation & UAC virtualization inference**:
   - Service accounts (`SYSTEM`, `LOCAL SERVICE`, `NETWORK SERVICE`, `DWM-*`, `UMFD-*`) and kernel/session subsystem processes are reliably recognized as elevated (`Some(true)`) with `UacVirtualization::NotAllowed`, removing "Unknown" fields in Details.
4. **Platform (wow64) detection**:
   - Inspects PE header of resolved executables on disk via `pe_is_wow64(exe)`. Falls back to known 64-bit subsystem rules (`is_kernel_or_system`), ensuring both 32-bit services (e.g. `steamservice.exe`, `MicrosoftEdgeUpdate.exe`) and 64-bit drivers (`NVDisplay.Container.exe`, `GameInputSvc.exe`) report correctly.

## 2026-09-06 — Tray focus & dismiss, GPU graph right-click, dialog keyboard nav, service platforms & module unloading

Five follow-up fixes addressing tray menu focus, GPU engine switching, dialog keyboard navigation, platform detection for protected/service processes, and module unloading:

1. **Tray context menu stuck / no hover highlights**:
   - `TrackPopupMenuEx` was previously passing `MAIN_HWND`. When minimized to tray, `MAIN_HWND` is hidden (`IsWindowVisible == false`), causing `SetForegroundWindow` to fail. Explorer only grants foreground rights to the window registered with the tray icon.
   - Stored `TRAY_HWND` (`icon.window_handle() as isize`). Updated `show_native_tray_menu()` to call `ReleaseCapture()` before `TrackPopupMenuEx`, pass `TRAY_HWND` to `SetForegroundWindow` and `TrackPopupMenuEx`, omit `TPM_NONOTIFY` so notifications route correctly to the tray window, and finalize with `PostMessageW(TRAY_HWND, WM_NULL)`. Menu now closes properly when clicking outside and highlights entries on hover.
2. **GPU usage graph context menu**:
   - `chart_multi` and `core_chart` allocated their canvas using `egui::Sense::hover()`, which silently ignores click events in egui. Changed allocation to `Sense::click()`.
   - Exposed context menu both on secondary-click over the GPU chart and via the caption dropdown indicator (`"{title} ▾, {window}"`), allowing selection of Total GPU load, 3D, Video Encode, Video Decode, Compute, etc.
3. **Delete confirmation dialog keyboard & mouse navigation**:
   - Added explicit keyboard navigation in `end_task_dialog`: `Tab` / `Shift+Tab` / `Left` / `Right` cycles selection between "End task" and "Cancel"; `Escape` cancels; `Enter` or `Space` executes the currently focused button.
   - Added mouse drag selection: holding the primary mouse button down while hovering over a button updates visual focus without triggering early; only click releases confirm.
   - Added distinct visual focus rings around the selected button.
4. **Details page platform resolution (32-bit vs 64-bit)**:
   - Protected processes and Session 0 services (`WmiPrvSE.exe`, `MicrosoftEdgeUpdate.exe`, `NVDisplay.Container.exe`, `GameInputSvc.exe`, `csrss.exe`, `winlogon.exe`, `fontdrvhost.exe`, `dwm.exe`) returned Access Denied on `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`.
   - Added `service_exe_paths()` querying SCM for service binary paths and `resolve_candidate_path()` searching `System32`, `System32\wbem`, `SysWOW64`, `Windows`, EdgeUpdate, and GameInput directories.
   - Normalizes process names by stripping `.exe` and matching stems. Hardcoded native 64-bit (`Some(false)`) attribution for known 64-bit OS core subsystem binaries (`csrss`, `winlogon`, `fontdrvhost`, `dwm`, etc.).
5. **Details page module unloading**:
   - `LoadState::Ready` pre-selects the first unloadable module so the "Unload Module" button is not grayed out on dialog open.
   - Broadened `module_is_unloadable` to allow `.mui`, `.ocx`, `.cpl`, `.ime`, `.node` while rejecting main image binaries (`.exe`) and core NT subsystem binaries (`ntdll`, `kernel32`, `kernelbase`, `wow64*`, `api-ms-win-*`).
   - `core_service.rs` uses `creation_epoch_of(pid)` dynamically when start epoch is unknown so requests route to the elevated broker instead of failing locally with Access Denied.
   - Treated `FreeLibrary` reference decrements where remaining references keep the module in memory as non-fatal success.

## 2026-09-06 — UI polish, system process telemetry, tray responsiveness & app icon

Seven targeted fixes and polish items across `tm-app` and `tm-platform`:

1. **Tray context menu responsiveness**: Replaced `tray-icon`'s internal menu and 15ms `Shell_NotifyIconGetRect` timer with native Win32 `TrackPopupMenuEx(TPM_RETURNCMD | TPM_RIGHTBUTTON)` and `PostMessageW(WM_NULL)`, eliminating cursor hover lag and slow opening.
2. **Delete confirmation dialog**: Styled "End task" button with accent fill and requested default focus so it is both visually and interactively preselected.
3. **Protected/system process telemetry on Details page**:
   - `cpu_load.rs`: Extracted `working_set`, `peak_working_set`, `commit`, `handle_count`, and `thread_count` from `SYSTEM_PROCESS_INFORMATION`, which queries all processes including PID 0, PID 4, and protected processes without opening handles.
   - `sampler.rs`: Falls back to kernel process table when `sysinfo` reports 0 for memory, commit, handles, and threads.
   - `process_ops.rs`: Added `pe_is_wow64` inspecting executable PE `Machine` header for 32 vs 64-bit detection when handles cannot be opened; native 64-bit kernel processes (PID 0, 4, etc.) report `Some(false)`.
   - Enabled `SeDebugPrivilege` at startup and attributed kernel/Session 0/SYSTEM service processes as elevated.
4. **Performance graph separation**: Styled performance charts and core charts with `pal.card_bg`, 4px corner radius, outer border stroke, and clipped painters.
5. **GPU engine selection**: Standard engines (`3D`, `Copy`, `VideoEncode`, `VideoDecode`, `Compute`) are always included in the GPU graph dropdown and context menu, allowing monitoring of idle/spiking encoders.
6. **Module unload button**: Removed overly restrictive `\windows\` path check from `module_is_unloadable`, allowing non-core DLLs to be unloaded with proper user confirmation.
7. **Application icon**: Generated Fluent-style Windows 11 task manager icon and embedded multi-resolution `.ico` and `app.res` into `taskman.exe`.

## 2026-09-02 — publish preparation: untracked captures, scrubbed history

Preparing the repository for publication. `shots/` and `taskmanpngs/`
(development, final, and native-reference window captures) are no longer
tracked — they show the developer's real user name and installed software,
and the `.gitignore` `*.png` rule now actually covers them. Personal-path
and hostname strings in tracked docs were reworded
(`hardening/core-service/context.md` and two entries below), README,
LICENSE and CI were added, and the full git history was rewritten with
git-filter-repo to purge every historical capture and personal-path/
hostname string. Commit SHAs cited in docs written before this entry are
pre-rewrite provenance and no longer resolve.

## 2026-09-02 — full-repository audit: what the smoke test never looked at

A whole-tree audit (source, tests, gates, release artifacts). The workspace
was already clean under `python build.py --check` and the shipped binaries
already carry ASLR/DEP/CFG with no build-machine paths in them, so the
findings are about coverage and lifetime, not about broken features.

### `--selfcheck` was sampling with the Processes page's demand

`selfcheck::run` built a collector and never called `set_demand`, so it ran at
the sampler's default `TelemetryDemand::core()` — CORE_PROCESS,
NET_ADAPTER_RATE, TOKEN_SECURITY. Everything else is demand-gated, and DXGI
enumeration is skipped outright until `demand.any_gpu()` (`sampler.rs`), so the
documented headless smoke test never touched the GPU, disk-rate, per-process
network or CPU-speed providers — the four most fragile collectors in the tree.
Worse, it *printed* the result: `"gpus":[]` on a machine with an RTX 5070,
which reads as "no GPU found" rather than "never asked". That is the
fabricate-nothing invariant leaking out of the UI and into the diagnostics.

Now `TelemetryDemand::all()` (new, with a test pinning that it covers every
declared bit, so a future provider cannot silently drop out of it), four ticks
instead of three because the PDH groups are two-sample counters that also open
lazily, and `freq_mhz` + `process_net_readings` in the summary so the CPU-speed
and ETW paths are visible rather than merely exercised. Verified on this
machine: `"gpus":["NVIDIA GeForce RTX 5070","Microsoft Basic Render Driver"]`,
`freq_mhz: 4136.3`.

### ShellExecuteEx handles were leaked on every non-waiting launch

`process_ops::shell_execute` always sets `SEE_MASK_NOCLOSEPROCESS`, but closed
`info.hProcess` only inside `if wait`. Four of the five call sites pass
`wait = false`: "Run new task", "Open file location", "Open URL" and
`relaunch_elevated`. Each one leaked a process handle for the app's lifetime,
which also keeps the launched process's kernel object alive after it exits and
pins its pid. Closed unconditionally now.

### The engine dropped every non-`Unsupported` sampling error on the floor

`run_loop` matched only `Err(TmError::Unsupported(_))`. Any other collector
failure was discarded with no log and no state change, so a collector failing
every tick presented as a frozen UI with a silent log. Now counted and reported
through a 30 s rate-limited `warn!`. While there: `sample_and_publish`'s
`count_tick` parameter had two byte-identical branches — removed, with the
reason the counter always advances written down instead.

### chart_multi indexed its timestamps by series index

`timestamps_ms.unwrap()[i]`, where `i` runs over a *series*. Every caller
happens to build both from the same window, so it is safe today — but that
exact desynchronization already happened once
(`performance::tests::series_extractors_stay_aligned_with_window`), and in the
paint path the consequence is not a shifted curve, it is a panic in the middle
of a frame. Now `.get(i)` with an even-spacing fallback, pinned by
`a_series_longer_than_its_timestamps_degrades_instead_of_panicking`.

### `bench/logs/.gitignore` never matched anything

The file contained `bench/logs/*.log`. A pattern with a slash is anchored to
the directory holding the `.gitignore`, so it resolved to
`bench/logs/bench/logs/*.log`. That is how four cargo build logs carrying the
build machine's home path came to be tracked, in a repo whose build driver goes
out of its way to `--remap-path-prefix` that same string out of the shipped
binaries. Pattern fixed to `*.log`, logs untracked (kept on disk).

### `implement.md` is gone but 30 comments still cite it

Deleted in `fc4ecb7c`; `repo-map.md` still listed it as a live path and
`AGENTS.md` still cited `§31`. The `§` numbers in source comments are now
documented as historical provenance rather than a resolvable reference.

Also: four tests waited on a spawned child with a fixed `sleep` — the thing
AGENTS.md forbids — replaced with bounded `poll_for`/`wait_until_visible`
loops; a stray `eprintln!` on the GUI startup path became `tracing::info!`;
and the `TextSmoothing`/`RenderMode` doc comments no longer claim that
sub-pixel text is impossible and that `Software` means WARP.

## 2026-09-02 — the Task Manager hotkey has to work when nothing else does

Audit of the Ctrl+Shift+Esc path (IFEO `Debugger` on taskmgr.exe → our exe).
Six ways a press could produce nothing, and what each needed:

### A minimized instance stayed minimized

`restore_window` sent `Visible(true)` + `Focus`. winit's `focus_window` is a
documented no-op while `is_minimized`, and `ShowWindow(SW_SHOW)` does not
un-minimize either, so a hotkey press on a minimized task manager did exactly
nothing. The restore now sends `Minimized(false)` between the two. The cloak
that hides the first unpainted frame is now applied only when the window was
genuinely hidden to the tray — a minimized window still holds its last frame,
so cloaking it only added a blink.

### The window came back behind everything

The restore ran in the OLD process, which has no foreground right; Windows
answers `SetForegroundWindow` from a background process with a taskbar flash.
The right belongs to the process the shell just launched, so the LAUNCH now
calls `AllowSetForegroundWindow(primary_pid)` before it signals, and the
instance it wakes can take the foreground.

### A wedged instance swallowed every press

The second launch set the show event and exited unconditionally. If the
running instance was hung — the exact situation one reaches for a task
manager in — the press produced nothing at all, forever. There is now an
acknowledgement event, set by the UI thread that actually processed the
restore. No acknowledgement inside the deadline means the launch opens its
own window instead of exiting. `Local\TaskMan.Primary.v2` publishes the
primary's pid and HWND so the launch can tell the cases apart: a dead pid
gives up immediately, a window that times out on `WM_NULL`
(`SMTO_ABORTIFHUNG`, the shell's own "not responding" question) gives up
after ~2 s, anything else keeps its request for the extended deadline. A
denied `SendMessageTimeout` is inconclusive, not wedged: UIPI refuses the
message when an unelevated launch probes an elevated instance, and only
`ERROR_TIMEOUT` may cost an instance its request.

Verified live: a second launch defers in ~96 ms; with the first instance
suspended (`NtSuspendProcess`), the second one has its own window in ~2.1 s.

### An elevated instance could not be reached at all

The coordination objects were created with default security. An instance
started elevated labels them high integrity, so the medium-integrity launch
the shell starts on Ctrl+Shift+Esc was denied every access and silently
started a SECOND task manager. All four objects now carry `S:(ML;;NW;;;LW)`
plus a DACL for this user, Administrators and SYSTEM. The names moved to
`.v2`: an old instance would take the show event without ever acknowledging
it, and mixed generations only exist across an upgrade.

### Raising an open window asked for consent

With "always start elevated" on, every press re-execed through UAC before it
discovered that the window it wanted was already open. The handoff now runs
FIRST in `run_gui` — before locale, settings and the elevation policy — so
showing an existing window costs nothing.

### Registrations that cannot be launched

Windows runs the `Debugger` command INSTEAD of taskmgr.exe; if that command
cannot start, the hotkey opens nothing at all — not even the built-in Task
Manager. `set_direct_for_exe` therefore refuses to register a path that is
not a file, cannot be quoted, or is itself named taskmgr.exe (which would
re-enter its own registration forever). A registration that has since gone
dangling is detected at startup and repaired wherever that needs no consent
prompt (elevated session, or the core service broker, which re-points at the
protected GUI); otherwise the settings dialog says the hotkey currently opens
nothing and offers a one-click Repair — the old advice was to toggle the
checkbox off and on, which costs two UAC prompts.

Single-instance coordination moved out of `tm-app/src/main.rs` into
`tm-platform/src/win/instance.rs`; the app now only supplies the wake
callback, publishes its HWND, and acknowledges. `build.py --check` passed.

## 2026-09-01 — process owners, multi-select, GPU engines, window chrome

Ten reported gaps, fixed together. The ones with a root cause worth keeping:

### The User column was empty for most of the process list

sysinfo resolves a process owner through its own token query, which asks for
more access than an identity read needs and fails for roughly half the list —
all of session 0 unelevated. The old fallback ("session id 0 → SYSTEM") needed
`session_id_of`, which needs `OpenProcess`, so it did not fire for exactly the
processes that had triggered it.

`process_ops::token_user` opens the token through
`PROCESS_QUERY_LIMITED_INFORMATION` (granted for almost every process,
protected ones included) and resolves `TokenUser` → `LookupAccountSidW`.
Answers are memoized per SID string for the life of the process: a
domain-joined machine runs hundreds of processes under a handful of accounts,
and `LookupAccountSidW` can reach a domain controller. The resolved name is
also carried forward across the attribute cache's TTL refresh — a process's
owner cannot change while it lives.

The kernel process table now also yields `session_id` (offset 100 on 64-bit,
80 on 32-bit, pinned against the `windows` crate's struct), so the session-0
fallback works even for the processes no handle opens at all. The domain is
dropped for the NT AUTHORITY pseudo-domain **in every locale spelling** —
matching only the English one left "NT-AUTORITÄT\SYSTEM" in a 120 px column
on a German system.

### Dumps were not debuggable

`MiniDumpWriteDump` was called with `MINIDUMP_TYPE(0)` — `MiniDumpNormal`:
stacks and module headers, no memory. WinDbg opens it and then answers almost
every question with "memory access error". Worse, without
`IgnoreInaccessibleMemory` a single unreadable region (guard page, driver-locked
memory) fails the whole write.

`DUMP_TYPE_FULL` is now the `procdump -ma` set minus token information (SIDs
and privileges are not needed to debug, and a dump is user data). A target that
refuses its whole address space is retried once at `DUMP_TYPE_REDUCED` — after
rewinding the file, because `MiniDumpWriteDump` writes from the current
position and the failed attempt left bytes behind.

### Grouping rejected a family for one foreign child

`same_image_family` returned `None` the moment any descendant had a different
image, so a browser with twenty renderers and one `crashpad_handler` rendered
as twenty-one flat rows. `application_family` keeps the members that belong and
leaves the rest where they are — which is why the group aggregate had to become
the members' own values (`family_values`) rather than the subtree's: a
descendant shown as its own row must not also be counted inside the group.

Two rules group now, and the difference in strength between them is the design:
same image joins unconditionally (that is what keeps a 40 % renderer inside its
browser row), same PUBLISHER under a different image joins only while windowless
and idle and never across a system or launch boundary. Plus repeat runs of one
image under one parent (`sibling_run_key`), which the family walk cannot see
because the only process connecting them has a different image. `svchost.exe` is
excluded from run grouping on purpose — see `known-debt.md`.

### Dragging by the strip below the caption

`StartDrag` was sent from egui's `drag_started()`, which only fires once the
pointer has passed egui's drag threshold. Every pixel before that was movement
the window did not follow, so it jumped to catch up. It goes on the button
press now, like the real caption. Double-click-to-maximize had to be detected
by hand (time + distance since the last press in the region): handing the press
to the window manager ends egui's view of it, so its own click bookkeeping
never completes.

### The white flash restoring from the tray

`software_integration.rs` skips painting entirely while the window is hidden
(upstream documents an invisible window burning a whole core, emilk/egui#7776,
and forced repaints into one grow the stack until the process dies). A hidden
window also receives no `WM_PAINT`, so there is no way to have a frame ready
before it appears: `ShowWindow` composes the empty window and the app's first
frame lands a beat later.

Restores go through `NativeApp::restore_window` now, which sets `DWMWA_CLOAK`
BEFORE `Visible(true)`. A cloaked window is "visible" to Windows —
`window.is_visible()` returns true, so painting resumes — but DWM does not
display it. `finish_restore` uncloaks two frames later.

Two frames, and the countdown runs at the START of a frame, not the end: the
`Visible(true)` viewport command is applied AFTER the frame that issued it, so
the first frame that can present into a visible window is the next one, and
the one after that is safe to reveal. Counting at the end of a frame reaches
zero before anything has been presented, which is the flash again. It also
runs unconditionally every frame rather than from a one-shot callback, so no
path can leave the window invisible.

Cloaking is deliberately NOT the tray-hide mechanism: a cloaked window keeps
its taskbar button.

### The rest

- **Search** matched name/display/publisher/PID only. It now also covers
  description, owning user, service name, image path and command line,
  cheapest field first so the kilobyte-long command line is only scanned when
  nothing else matched. Startup and Services route through the same `Query`.
- **Multi-select** (`selection.rs`) backs both process tables with the native
  list-view gesture set. Rows are held as identities, never indexes — the list
  re-sorts on every tick. Fan-out is limited to the repeatable commands; the
  single-target ones follow a separate `primary`. More than one target always
  goes through the confirmation, which lists every one by name and pid.
- **GPU engines**: `HistoryPoint.gpu_engines` plus a "change graph to" menu
  (Overall / All engines / each engine). The menu's engine list comes from the
  history window, not the snapshot — the snapshot's list is sorted by
  utilization, so an engine that just went idle would drop out exactly when the
  user wanted to look at what it had been doing. The collector no longer
  truncates to the six busiest for the same reason.
- **Window chrome** (`win/window_chrome.rs`): caption/text/border colours plus
  `IMMERSIVE_DARK_MODE`, pushed only when the theme changes (each attribute
  recomposes the frame). What is NOT reachable, and why, is in `known-debt.md`.
- **Search box** gained a clear button; Escape clears it too.
- **Row names split by page.** Details is the diagnostic page and shows the
  IMAGE NAME only (`svchost.exe`); the description is available there as its
  own optional column. Processes is the app list and shows
  `Microsoft Edge (msedge.exe)` — description first, image name behind it,
  bracket dropped when it would only repeat the name. `ColumnId::Name` on
  Details sorts by image name, matching what it renders.
- **Process counts moved to square brackets**: `Microsoft Edge (msedge.exe)
  [24]`, and the group headers with them (`Apps [7]`). Round brackets now mean
  an image name on that page, so a count in round brackets would read as one.
- **Scroll fades off.** egui paints a 20 px background-coloured ramp at the
  top and bottom of every scroll area (`ScrollFadeStyle`, strength 0.5). Over
  a dense table it reads as a shadow lying on the list — a second, moving edge
  beside the header and the window frame. `strength: 0.0` in `theme.rs`.
- `ROW_H_DENSE` 22 → 20 px. That is the floor: 13 px row text needs a ~17 px
  line box and `icon_cell` derives its glyph side from `row_h - 6`.

## 2026-08-31 — tray polish, and the fabricated start time behind "no priority"

### Half the process list had no identity

Reported as "the priority tick is missing for some processes". It was 130 of
261, and the priority was only the visible half of it.

sysinfo reads a process's creation time through a process HANDLE and returns
**0** when it cannot open one — which unelevated is every svchost, every
protected service, System, Registry, Secure System. `sampler.rs` stored that
verbatim as `start_epoch_s = Some(0)`: a fabricated identity that no
`creation_epoch_from_handle` check can ever match, quietly poisoning every
PID-reuse guard for those processes. `GetPriorityClass` failed for exactly the
same set, leaving `PriorityClass::Unknown`.

Both now come from the kernel process table, which reports a creation time AND
a base priority for every process without opening anything:

- `cpu_load::start_epoch_of` fills a missing start time (and leaves it `None`
  when even the kernel table has none — never `Some(0)`).
- `cpu_load::base_priority` + `process_ops::priority_class_from_base` resolve
  the class from the base priority (4/6/8/10/13/24 → Idle…Realtime, compared
  as RANGES because a foreground normal process commonly reads 9).
  `GetPriorityClass` still wins where a handle opens; this is the fallback.

Both accessors read the retained raw table, NOT `LoadSample`: a load sample
needs two ticks to exist, and the first snapshot is the one the UI shows.
Result: unknown priorities went 130 → 1 (pid 0, which the kernel table
excludes by design). `hand_written_offsets_match_the_declared_layout` pins the
`BasePriority` offset against the `windows` crate's own struct, and
`unopenable_processes_still_report_an_identity_and_a_priority` pins the
end-to-end behavior including "no process may carry `Some(0)`".

### The Details tree is now a sort state, not a mode

The flat/tree switch is gone from settings and from the overflow menu's View
submenu. The tree IS `SortOrder::Hierarchical`, reachable only from the Name
column: click it a third time. Clicking any other column lands on a plain
direction, which is what leaves the tree — the user asked for an ordering by
THAT column, and a tree would keep children pinned under their parents. The
overflow menu keeps one "Process tree (Name column)" tick as the discoverable
way in and out.

`details_tree_view` (and the older `process_tree_view`) migrate into
`details_tree_hierarchical`, and the restore path runs even when no `[sort]`
entry exists — a migrated config has exactly that shape.

### Tray icon

- A single left click restores the window; double click still works.
- The notification-area menu follows the app's effective light/dark theme.
  There is no supported API: Windows themes popup menus from a process-wide
  preference set by two unnamed uxtheme exports (ordinal 135
  `SetPreferredAppMode`, 136 `FlushMenuThemes`). muda's `MenuTheme` is not a
  substitute — it documents itself as affecting a window's menu BAR only.
  Both lookups fail soft.

## 2026-08-31 — menus, scroll bars, stale process attributes, hierarchical tree

Four user-reported UI defects, three of which had a shared shape: the fix was
never in the tab that showed the symptom.

### Context menus were not menus

egui's `menu_style` sets `button_padding = (2, 0)` and then lets the global
`item_spacing.y` (6 px) separate the entries. The result was a column of
text-height labels with dead gaps between them — the gaps neither highlight
nor activate the entry under the cursor.

`widgets/menu.rs` now paints every entry itself: one uniform 28 px full-width
row, `item_spacing.y = 0`, a left check gutter so ticked and unticked labels
share a left edge, and a right gutter for submenu arrows. `menu::context_menu`
installs the style on the popup, and submenus inherit it through
`MenuConfig`. Two details that are easy to get wrong:

- Use `allocate_at_least`, NOT `allocate_exact_size`. Menus lay out
  top-down-JUSTIFIED and the "exact" variant re-aligns the desired size back
  inside the justified frame, leaving each row only as wide as its own label.
- The tick is a hand-painted polyline. `✓` is not in every installed UI font
  and a missing glyph renders as a tofu BOX — which is what the user was
  seeing and reported as "the checkbox looks ugly". The same reasoning
  retires `controls::checkbox` from inside menus (it drew a real 18×18 box
  next to menu text).

`entries_are_uniform_full_width_and_gapless` pins height, width and the
absence of gaps.

### Priority changes took up to ten seconds to appear

`win/sampler.rs` caches slow-changing per-PID attributes (priority, EcoQoS,
UAC virtualization) behind `ATTR_REFRESH_TTL` (10 s). Nothing invalidated
that cache when WE changed one of those attributes, so the menu kept ticking
the old priority until the TTL expired.

Two halves, both needed:

- `process_ops::note_attrs_changed` / `take_changed_attrs` — a process-global
  invalidation list drained by the sampler at the top of every tick. The
  broker path bypasses `process_ops`, so `core_service.rs` records it too
  (`noting_attrs`).
- `TaskManApp::run_action_refreshing` — refresh AFTER the action completes.
  `toggle_efficiency_mode` used to call `engine.request_refresh()` beside the
  dispatch, which races the executor's worker thread and samples the
  still-unchanged process.

No optimistic UI echo, deliberately: Windows silently downgrades a Realtime
request to High without `SeIncreaseBasePriority`, so echoing the requested
class would tick a value the OS never applied.

### Scroll bars painted over content

`ScrollStyle.floating_allocated_width` was 0, so the bars covered the last
~14 px of every table row, Performance card and dialog. It is now 14
(`bar_width` + `bar_outer_margin`), which moves the bar just outside the
content rect while keeping the thin-idle/expand-on-hover look.

`floating` stays TRUE on purpose. egui measures "is the content too large"
against the OUTER rect for floating bars and against the shrunken INNER rect
for solid ones; with solid bars, content whose height depends on its width
can flip the bar on and off every frame.

Consequence for `tablekit`: header and body are two independent scroll areas
sharing one horizontal offset. The body now reserves a bar lane and the
header (bars hidden) does not, so the header is fed the body's previous-frame
reservation via `prev_bar_use`/`store_bar_use`. Without it the two clamp the
shared offset at different maxima. `BODY_PAD_RIGHT`/`BODY_PAD_BOTTOM` dropped
to 6/4 — they are breathing room now, not bar clearance.

### The Details tree was a mix of hierarchy and alphabet

The tree sorted siblings by the active column, so it was never a literal
hierarchy. `details::SortOrder` adds a third state: clicking the sorted
column a third time (tree only) reaches `Hierarchical`, which ignores the
sort column entirely and orders siblings by CREATION time — System
Informer's cycle. It is also reachable from View ▸ Strict hierarchy.

Creation order, not snapshot order: `sys.processes()` is a hash map, so
"unsorted" would reshuffle the whole tree every tick. `start_epoch_s` then
PID is stable and is the order the OS actually created the children in.

Persistence: the shared `[sort]` line format is `column,ascending` and has no
room for a third state, so the flag rides in `[general]` as
`details_tree_hierarchical` next to `details_tree_view`.

## 2026-08-31 — per-process network without elevating the GUI (broker v2)

The GUI needed administrator rights to show the Network column, which defeats
the point of having a LocalSystem broker. Protocol v2 adds
`ProcessNetworkCounters`: the service hosts the ETW session and answers a
read-only, bounded query, so an ordinary unelevated GUI gets real numbers.
This deliberately reverses the "no telemetry endpoint" invariant — the
reasoning is recorded in `core-service.md`.

Version bumped rather than adding a variant to v1 on purpose: an older service
then fails the HANDSHAKE, which the client can tell apart from a REJECTION and
therefore fall back on. Slipping the variant into v1 would have made
"unsupported" indistinguishable from "denied", and the no-fallback-on-rejection
rule depends on that distinction. Cost: a one-time service reinstall.

### The bug this uncovered: leaked ETW sessions

First end-to-end run showed 0 KB/s everywhere despite a live 50 MB download.
The service HAD started its trace (log confirmed) and the GUI's requests were
accepted, so the counters had to be arriving empty. `network_trace_raw_vs_pruned`
separated the two candidate causes and reported `raw entries=0, live pids=273`
— the trace was receiving no events at all, so pruning was innocent.

Root cause: the session name included the pid. `Drop` stops the session, but a
force-killed or crashed process never runs `Drop`, and the next start picked a
DIFFERENT name — so orphans accumulated instead of being reclaimed. Four had
piled up from processes killed during development, and once enough sessions
have one provider enabled Windows stops delivering events to them. The module
doc even claimed stale sessions were reclaimed by name; that was never true
with a pid in it.

Fix: fixed, role-scoped names (`TaskMan-Net-Service`, `TaskMan-Net-App`) so
each host reclaims its own orphan through the existing
`ERROR_ALREADY_EXISTS` → stop → restart path, and never the other's.
`session_names_are_fixed_per_role_and_never_contain_the_pid` pins it. Also
hardened `totals_pruned`: an EMPTY live-pid set now means "the caller could not
enumerate", not "nothing is alive" — pruning against it would have wiped every
counter and produced the same silent all-zero symptom.

Verified: after cleanup, `raw entries=9, busiest = pid 23092 recv 50,075,955`
(exactly the 50 MB download), and the unelevated installed GUI sorted by
Network shows `codex.exe 633,1 KB/s` at the top. One session exists, and
force-killing plus restarting the GUI no longer adds any.

## 2026-08-31 — engine dropped pre-start demand (why the Network column stayed empty)

User report: the Network column still showed "—" after the ETW work landed.
The ETW session, the sampler wiring and the display model were all correct;
the bug was one line in `engine::run_loop`.

Root cause: while a lazily spawned engine parks waiting for `Start`, the
command loop matched `Ok(_) => {}` and **silently discarded every other
command**. The UI computes its telemetry demand on its FIRST frame — which is
deliberately before the engine is started — and `update_demand` only re-sends
when the bitmask changes. So `PROCESS_NET` was requested exactly once, into
the void, and the collector never learned it was wanted. The ETW session was
therefore never started, and `net_*` stayed `None` forever.

This was latent before: every other demand bit is only added when leaving the
default start page, and a tab switch re-sends. `PROCESS_NET` is the first bit
set on the DEFAULT page, so it is the first one that could be lost for good.
`SetInterval` had the same hole (an interval set before start was ignored).

Fix: remember demand while parked and apply it right after the factory runs;
apply `SetInterval` immediately, since the sampling loop reads the interval
from shared state anyway. Pause/Resume/Refresh remain meaningless before
`Start`. Two regression tests
(`demand_sent_before_start_reaches_the_collector`,
`interval_sent_before_start_is_honored`) fail against the old code.

Diagnosis path worth remembering: an engine-level integration test that
reproduces the UI's exact call order (`spawn_lazy` → `set_demand` → `start`)
separated "platform layer broken" from "app layer broken" in one run — the
platform test passed 235/235 while the GUI showed nothing, which pointed
straight at the plumbing between them.

### Network column now reads in bytes

Native TM fixes this column to Mbit/s, where realistic per-process traffic (a
few KB/s) renders as "0,0" and the column looks broken. It now uses the same
1024-based KB/s / MB/s units and one decimal as the neighbouring Disk column
(`format::format_process_net_rate`), so a browser at 3 KB/s is visible and the
two rate columns read consistently. Deliberate deviation from TM parity.

## 2026-08-31 — per-process network via ETW

Implemented on request, replacing the honest-but-empty "—" column.
`win/net_etw.rs` runs a private real-time ETW session on
`Microsoft-Windows-Kernel-Network` and accumulates the `size` field of the
TCP/UDP data events per PID.

Details that matter:
- **The PID comes from the event PAYLOAD, not `EventHeader.ProcessId`.**
  Kernel network events fire in arbitrary (usually System) context, so the
  header PID is not the traffic owner. All eight data events — TCP/UDP ×
  v4/v6 × sent/received — begin with `PID: u32, size: u32`, and that prefix is
  the only thing parsed.
- Only the *data* event ids (10/26/42/58 sent, 11/27/43/59 received) are
  counted. Connect/disconnect/retransmit/ACK events are ignored, or
  retransmitted bytes would be billed twice.
- **Administrator rights are required** to start an ETW session. Without them
  the monitor stays inactive and every process keeps `None`, which renders as
  "—" — never a fabricated zero. Availability is all-or-nothing and pinned by
  `per_process_network_is_unknown_or_measured_never_fabricated`. A failed
  start is remembered so an unelevated session does not retry a
  permanently-denied API every tick.
- Demand-gated on `TelemetryDemand::PROCESS_NET` (the bit was already
  reserved): the session runs only while Processes or App History is on
  screen, and `Drop` stops it and joins the consumer thread.
- Rates are deltas of cumulative counters over the tick interval, keyed by
  `(pid, start_epoch_s)` so a recycled PID cannot inherit a dead process's
  totals; the map is pruned to live PIDs each tick to bound memory.

Verified elevated against live traffic — EpicGamesLauncher/python/claude all
attributed with sane recv/sent, and the two ends of a localhost pair showing
mirrored counters. Measured overhead with the session running: 4.0 % of one
core vs ~3 % without it. The Network cells now explain themselves on hover
when unavailable ("needs administrator rights") instead of showing a bare
dash.

## 2026-08-31 — text rendering limits, graphics mode, and two measured findings

### ClearType is not reachable from this side (measured, not assumed)

User report: "fonts don't seem to have coloured sub-pixels, they look blurry
and fat". Confirmed by measurement: across a text region of our own window the
maximum per-pixel RGB channel spread is **0** — pure grayscale coverage. The
same measurement on the reference Windows Task Manager capture
(`taskmanpngs/1.png`) shows orange/blue fringes, i.e. real sub-pixel AA.

Why it cannot be fixed here: epaint stores every glyph in a SINGLE-channel
coverage atlas and tints it in the shader, and the renderers blend with one
scalar alpha. Per-channel coverage would need a 3-channel atlas plus either
dual-source blending or per-channel colour write masks in BOTH the wgpu and
glow backends — a fork of epaint + egui-wgpu + egui_glow. Upstream tracks it
as emilk/egui#2639. Recorded in `known-debt.md`.

What DID improve, and why:
- **Segoe UI Variable** (`SegUIVar.ttf`, the real Win11 UI face) is now
  preferred over the Win10 static `segoeui.ttf`, pinned to `wght=400`,
  `opsz=10.5` through `FontTweak::coords`.
- **Horizontal grid-fitting.** epaint's default `SmoothHinting` sets
  `preserve_linear_metrics: true` AND `symmetric_rendering: true`, both of
  which switch x-direction grid-fitting OFF, so vertical stems straddle two
  grey columns. The `Sharp` profile turns both off. Measured on a stem:
  `237 252 187 92` → `220 255 220 114` (a fully-lit column appears).
- **Coverage ramp.** egui's dark-mode default `2c − c²` lifts every partially
  covered pixel (0.5 → 0.75) — that is the "fat" look. `Sharp` uses raw
  coverage; measured ink over a fixed text region drops 277 → 225 (−19 %).
- All three are one user setting, `text_smoothing = sharp|standard|smooth`
  (default `sharp`), applied live; `TASKMAN_TEXT_SMOOTHING` overrides it for
  A/B comparisons.

### Software rendering is not viable on this stack (measured)

`render_mode` replaces the short-lived `gpu_acceleration` bool with
`auto | compatibility | software`. Continuous-repaint measurements on this
machine (Ryzen 7 5700X, 16 logical CPUs, 2400x1350 window):

| mode | backend | cost |
| --- | --- | --- |
| auto | wgpu D3D12, GPU | **0.2–0.3 cores** |
| compatibility | glow/OpenGL, GPU | **1.0–1.1 cores** |
| software | wgpu D3D12, WARP | **14 cores at 2.9 fps** |

WARP's cost is FIXED per frame, not fill rate: identical at 500x320 and
2000x1200 (10.5 vs 14.0 cores), identical for a near-empty paused window and a
full process list, and unchanged by present mode or frame latency. Nothing
this app draws can bring it down. Even at the normal 1 Hz sampling tick,
software mode sits at ~10 cores. It is therefore shipped with an explicit
warning in the settings dialog, and "no GPU trouble" is served by
`compatibility` (OpenGL — a different driver stack, still real-time).

### Per-process network is still not implemented

Checked on request: no Windows collector ever writes `net_recv_bps` /
`net_sent_bps` / `net_recv_total` / `net_sent_total` (the only writers are in
an `app_history` unit test), so the Processes and App History network columns
render "—" for every row. That is the intended honest-unavailable behavior,
not a regression; the ETW work is listed in `known-debt.md`.

## 2026-08-31 — Processes/Details table parity: heat map, status glyphs, type-ahead, tree

Seven user-reported gaps against native Task Manager and System Informer,
plus the root cause behind two of them.

Root cause found (efficiency mode never showed): `GetProcessInformation`
with `ProcessPowerThrottling` treats `PROCESS_POWER_THROTTLING_STATE.Version`
as an INPUT field. `efficiency_mode_state` passed a zeroed struct, so the
call failed with `ERROR_INVALID_PARAMETER` (87) for EVERY pid and
`power_throttled` was `None` system-wide — the leaf could never appear.
Verified out-of-band with a P/Invoke probe before and after (Version 0: 87
for all 235 processes; Version 1: Brave/Edge renderers report
ControlMask/StateMask `EXECUTION_SPEED`). Regression test:
`efficiency_mode_state_is_known_for_own_process`. Efficiency mode also moved
to its own 2 s sub-TTL (`POWER_THROTTLE_REFRESH_TTL`) inside the 10 s
attribute cache: it is a live status column, and one
OpenProcess/GetProcessInformation pair per process is cheap.

UI changes:
- **Heat map** (`theme::heat_blue` + `TmTable::heat_cells`): every numeric
  cell is now filled from a continuous gradient whose FLOOR is `heat_base`;
  the old model painted a flat band only for rows with a non-zero value and
  a single binary top-consumer highlight, so idle processes had unpainted
  holes. Curve is ease-OUT (`sqrt`) — intensities are normalized against the
  column maximum, so the old ease-in curve collapsed everything but the top
  consumer onto the base tint. `heat_low`/`heat_top` are gone.
- **Hover reaches the value columns**: the heat band is opaque and is painted
  after the row fill, so hovering only lit the name area. `TmTable::row`
  records its selection/hover fill in `row_overlay`; `heat_cells` re-applies
  it over the band. Light mode gets a dark wash (`row_hover_fill`) — a white
  one over a light background was invisible.
- **Status column glyphs** (Processes): orange pause for suspended, green
  leaf for efficiency mode, words moved into the row tooltip; only
  "not responding" stays spelled out. The glyphs deliberately do NOT create
  their own hover widgets — that would steal the row's hover state and make
  the highlight flicker.
- **Group rows summarize efficiency mode** (`Subtree::efficiency`): a
  collapsed `Brave Browser (24)` row already aggregates CPU/memory, so it now
  aggregates the power state too. That is where native TM shows the leaf, and
  it is what the user actually reported missing.
- **Type-ahead** (`search::list_type_ahead`): typed characters accumulate
  into a word for 1 s, so "svc" lands on svchost.exe instead of jumping to
  whatever starts with "c". One letter — or the same letter repeated — still
  cycles. All text events in a frame are consumed in order (fast typing
  delivers several per frame); buffers are keyed per list.
- **Dense rows** (`ROW_H_DENSE = 22`, `TmTable::row_h`): Details, Services and
  Modules pack their rows like native TM's Details tab. Processes/Users/
  Startup/App history keep the airy 32 px app-list spacing. `scrolled_rows`
  virtualizes on the table's own height; page-up/down counts match.
- **Details tree is expanded by default**: `State.collapsed` replaces
  `State.expanded`, so the tree always shows the COMPLETE hierarchy —
  including subtrees whose parents started after the page was opened, which
  the one-shot `ensure_tree_initialized` left collapsed. Parent links whose
  parent started AFTER the child are rejected (`is_plausible_parent`),
  matching System Informer's PID-reuse guard; unknown timestamps never
  reject. Indent tightened to 18 px.

Verified visually against live captures (leaf on Brave/Claude/Codex, pause on
a deliberately suspended process, sampled cell fills monotone
`#2E6FC4 > #2962AB > #2960A9 > #1C3D68 > #162C4A`).

## 2026-08-31 — foreign-session UX: repair now hands over to the installed copy

User report: the state "somewhat randomly" showed ForeignClient. Root cause:
not a bug in the state machine — the installed generation and the dev-tree
build had diverged (the final fmt/gate rebuild changed the hash after the
install), so dev-tree launches stopped hash-matching the installed copy and
the startup redirect declined by design; the service log confirmed the
rejected client paths were all `target\release\taskman.exe`. Launching the
installed copy showed Active — hence the perceived randomness across
launches.

UX fixes: repair from a ForeignClient session now installs this build AND
hands the session over to the installed copy (`dispatch_core_service_repair_and_switch`,
reusing `switch_to_installed_gui`), closing the loop in one click; the
ForeignClient text explains the rebuild/portable scenario and names both
buttons. The installed generation was refreshed to the current release build.

## 2026-08-31 — response-delivery race: state display flapped Running/Degraded

User report: the Advanced state alternated between "Active" and "broker
authentication failed". `handle_client` called `DisconnectNamedPipe`
immediately after writing the response; npfs discards queued outbound bytes
on disconnect, so the client's read raced the teardown — measured 24/40
delivered (drop: 40/40, flush+disconnect: 40/40). Lost pings surfaced as the
generic degraded message while the SCM still reported Running. Became visible
only after the DACL/identity fixes let non-elevated pings reach the response
stage at all.

Fix: broker drops its handle instead of disconnecting, in `handle_client` and
`reject_client` alike; the client drains the response and releases the
instance when its end closes. Also empirically validated
`USER_PIPE_ACCESS`/npfs requirements with a self-made-pipe bisection
(see tools history): grants without FILE_READ_ATTRIBUTES deny pipe client
opens regardless of requested mask.

## 2026-08-31 — real root cause: pipe DACL missing FILE_READ_ATTRIBUTES

Follow-up to the ForeignClient fix: the installed (non-elevated) GUI still
reported "broker authentication failed". Empirical ACL bisection on self-made
pipes proved npfs requires `DesiredAccess | SYNCHRONIZE |
FILE_READ_ATTRIBUTES` for pipe client ends; the broker's user ACE granted
only `0x00100083`'s predecessor `0x00100003` (no attributes), so every
non-elevated client was denied at open and elevated ones only worked via the
Administrators generic-all ACE. Second bug: the client's server-identity
check opened the LocalSystem service process, which non-elevated tokens
cannot do, so non-elevated GUIs could never complete the handshake.

Fixes: user ACE + client request now share `USER_PIPE_ACCESS = 0x00100083`
(data + attributes + synchronize); `verify_pipe_server` falls back to the SCM
view (pipe server PID == `ServiceStatus.process_id`, configured image ==
protected path, quotes stripped because windows-service keeps them) when the
direct image check is denied; the switch action is allowed from elevated
sessions (user-initiated, elevation inherited intentionally); ForeignClient
state keeps a secondary Repair button so a newer dev build can still upgrade
the protected generation. Verified live: non-elevated open + identity + ping
delivery against the reinstalled service; live test
`live_service_identity_verifies_without_elevation` added.

## 2026-08-31 — ForeignClient state: honest GUI↔service connection reporting

User report: the GUI said the core service "does not work" and repair "fails".
Diagnosis against the live machine showed the service, manifest, and pipe DACL
all healthy; the service log proved the running GUI had been launched from
`target\release\taskman.exe`, which the broker's path-based client
authorization rejects by design. The install behind the Repair button had
actually succeeded (fresh manifest + service restart), but the foreign session
kept being rejected forever and the generic "broker authentication failed"
detail made a successful repair look broken.

Fix: `service_state` classifies a healthy service + non-installed client image
as `CoreServiceState::ForeignClient` (new enum variant, unit-tested helper
`foreign_client_session`); the Advanced section shows an explanatory text and
a "Switch to installed copy" button backed by
`core_service::relaunch_into_installed_gui` (spawns the installed GUI with
`--single-instance-handoff`, then programmatic-exits the foreign session;
elevated sessions refuse). i18n keys: `CoreServiceForeignClient`,
`CoreServiceSwitchRequested`, `SwitchToInstalledCoreService`.

## 2026-08-31 — protected core service, persistent Details controls, overload resilience

Implemented the requested split architecture: the ordinary GUI owns all
telemetry/UI/user-path work, while a new delayed-auto LocalSystem
`taskman-service.exe` owns only versioned allowlisted control requests.

- **Service boundary:** local/remote-rejecting first-instance named pipe;
  one-user protected DACL; kernel client/server PID plus protected image-path
  binding; 64 KiB framed request/response caps; unknown-field rejection; two
  workers, queue cap 16, and pipe-instance cap 19. A protected ProgramData
  manifest pins schema, protocol, SID, paths, and SHA-256 hashes. Explicit
  broker rejection—and any post-authentication transport failure with an
  unknown outcome—never falls back to the GUI token.
- **Action safety:** brokered process actions require a positive sampled
  creation time, reject system/service/requesting-GUI/critical targets, then
  repeat exact identity and critical checks on the handle used by the action.
  Tree kill refuses unidentified descendants. Module unload stays narrowly
  allowlisted and re-enumerates exact base/path; module inventory and dumps
  remain local. UAC virtualization is a similarly narrow token operation with
  current-state menu marker, allowed-state check, and warning dialog.
- **Filesystem/SCM:** Program Files binaries are protected System/Admin-full,
  Users-read/execute; ProgramData manifest/logs are System/Admin-only with
  protected Administrator ownership. Directory/file handles are pinned against
  conflicting mutation while owner/group/DACL are assigned. Log startup
  accepts only the exact daily service-log name shape and rejects
  reparse/nested/non-file/hard-linked entries,
  and retains at most 14 daily files. The elevated helper never opens the
  interactive user's file log or mutates per-user redirect state. Install
  rejects reparse points, pins source and existing-destination handles against
  mutation, rejects hard-linked installed binaries, hashes pinned content,
  uses synchronized/write-through staging + atomic move, and verifies the
  destination. Upgrade waits for the old service process. SCM is
  delayed-auto LocalSystem with service SID, only `SeDebugPrivilege`, and
  restart actions at 5/15/60 seconds.
- **GUI handoff:** the successful one-time UAC install writes a per-user marker
  only after the helper's SCM start request succeeds; SCM reports readiness
  independently once the pipe is listening. Matching package/portable launches
  redirect before any window to the protected GUI, with Windows-safe argument
  quoting; a differing package hash remains local for repair/upgrade. Existing
  autostart and owned Task Manager replacement entries are migrated. Uninstall
  removes SCM capability and the marker while leaving protected files.
- **Parity/state:** literal PPID tree is on Details (not Processes); current
  priority and UAC states are marked; priority/affinity can be persisted per
  executable; sort state persists across all process/list tables; Process
  status reports suspended/not-responding/efficiency; modules can be unloaded;
  Delete confirms termination; close-to-tray and HKCU autostart are optional.
- **Resilience/startup:** two independent bounded 32-job GUI action lanes,
  no renderer-thread fallback, explicit overload feedback, above-normal (not high/realtime) control-plane
  priority, single-instance restore signaling, bounded ownership handoff for
  explicit elevation, condition-variable-coordinated SCM listener wakeup, lazy
  tray, background affinity and advanced-settings probes. Autostart migration
  rewrites only commands proven to be TaskMan-owned; settings input is capped
  at 4 MiB before parsing. A broker-worker panic aborts service mode so SCM
  recovery restarts a clean process rather than retaining degraded capacity.
  Windows WGPU remains D3D12-only with Glow fallback.

Final headless verification passed format, Clippy with warnings denied, all
166 workspace tests (71 app, 50 core, 40 platform library, 5 Windows
integration), the host release/package build, and the service protocol
self-check. The ZIP contains the 13,873,664-byte GUI and 1,378,816-byte service.
Linux cross packaging was skipped because neither supported cross toolchain is
installed. No GUI, UAC helper, ACL mutation, or service install/start occurred.

The earlier 2026-08-28 kill-path note below is historical: the current code
uses exact same-second creation identity (no ±2-second tolerance) and refuses
tree descendants whose creation identity could not be captured (no unverified
fallback).

## 2026-08-31 — GUI parity, literal process tree, Modules inspector, D3D12 trim

Broad parity/ergonomics pass against current Windows 11 Task Manager plus
selected System Informer diagnostics. No GUI/capture executable was launched
during verification so the active desktop remained undisturbed.

- **Process interaction:** native grouped ownership stays on Processes, while
  Details offers a persisted literal PPID tree; hierarchy-preserving search
  and sibling sorting; expand/collapse controls;
  arrow/Home/End/Page navigation; Delete opens an identity-bound end-process
  confirmation. Processes/Details menus now consistently expose copy, online
  search, file location, properties, dumps, and modules where supported.
- **Modules:** new async on-demand name/path/base/size inspector. Unload is a
  confirmation-gated diagnostic action restricted to same-architecture
  third-party DLLs. ToolHelp enumeration handles transient `ERROR_BAD_LENGTH`,
  surfaces unexpected iteration errors, and revalidates the exact process
  creation FILETIME plus module base/path before remote `FreeLibrary`; main,
  Windows, critical-loader, cross-bitness, and self targets fail closed.
- **Tables/settings:** Details gains description, publisher, parent PID,
  session ID, image path, page faults/sec, and I/O read/write columns;
  Startup/App History/Users/Services sort by headers; quiet column guides,
  keyboard list movement, and a Reset column widths setting were added.
  Settings is now resizable/scrollable and exposes the default page/process
  presentation. Registry startup publishers are resolved best-effort.
- **Performance/data:** lighter/resource-colored charts, graph-context CPU
  controls, responsive logical-CPU grids, combined network throughput, and
  cached native IPv4/IPv6/SSID/signal details. Unsupported App History network
  is `—` rather than a fabricated zero; the UI labels history as local-only.
  History disk writes are coalesced to 30 seconds plus shutdown.
- **Renderer/size:** WGPU is built with one native backend per target (D3D12
  on Windows, Vulkan on Linux, Metal on macOS), low-power adapter preference,
  FIFO/one-frame latency; Glow remains the fallback. This removes unused WGPU
  backend dependencies without removing recovery for problematic drivers.

Headless verification: `python build.py --check` passed format, clippy with
warnings denied, and 148 tests (69 app, 47 core, 27 platform, 5 Windows
integration); `python build.py --host-only` refreshed/package-checked the final
artifact. The EXE is 13,470,208 bytes versus the 14,656,000-byte baseline:
1,185,792 bytes / 8.09% smaller despite the added diagnostics. The feature
graph and binary string check contain DX12 but no Vulkan loader/API on Windows.
Linux packaging was skipped because neither `cross` nor `cargo-zigbuild` is
installed, as permitted by the non-`--require-all-targets` gate.

## 2026-08-28 — Security audit: 6 fixes (parser OOB, kill-path identity, hardening)

Full security audit of the workspace (source, deps, secrets, binary
hardening, runtime). Findings implemented:

- **SMBIOS Type-17 OOB panic** (`tm-platform/win/memory_info.rs`): the old
  guard `< 0x15` admitted spec-legal 0x15–0x1A-byte records but indexed up
  to 0x1A — index panic in `Sampler::lazy_init`, fatal because release uses
  `panic = "abort"`. Now every field read is length-guarded via the new
  pure `ram_static_from_table(table)` (probe is a thin wrapper), with
  regression tests for short (0x15) and full (0x22) Type-17 records.
- **Kill-path PID-reuse TOCTOU** (`tm-platform/win/process_ops.rs`):
  identity was only checked UI-side against a possibly-stale snapshot; the
  platform killed a bare pid (tree-kill children had NO identity check).
  `PlatformActions::kill_process` now takes
  `expected_start_epoch_s: Option<i64>`; `open_process_verified` reads the
  creation time THROUGH the freshly opened handle (handle-bound → immune to
  later pid reuse), ±2 s tolerance, fail-closed when the expected time is
  set but unverifiable. Tree-kill captures each child's birth at
  enumeration and re-verifies via the terminate handle; children without a
  captured birth fall back to the old unverified kill (their identity has
  nothing to do with the root's). Linux/macOS use the trait default and
  ignore the hint (documented).
- **Details context-menu identity gates** (`tm-app/tabs/details.rs`):
  priority/suspend/affinity now run the same start-time check against the
  live snapshot that End Task and Efficiency mode already had.
- **Elevated relaunch quoting** (`tm-platform/win/mod.rs`):
  `quote_win_arg` implements the MSVCRT/CommandLineToArgvW quoting rules
  (escaped quotes, doubled backslashes) for
  `relaunch_elevated_with_args`; round-trip tested against the real
  `CommandLineToArgvW`.
- **Sign-out confirmation** (`tm-app/tabs/users.rs` + `app.rs`):
  session Logoff parks in `TaskManApp::pending_session_logoff` and shows a
  confirm dialog (`session_logoff_dialog`, new `K::SignOutConfirm` key);
  Disconnect stays immediate.
- **Binary hardening**: `.cargo/config.toml` adds
  `-C control-flow-guard=yes` for windows targets (verified: Guard CF
  function table 0 → 1391 entries); `build.py` release builds add
  `--remap-path-prefix=<home>=` (strips the build-machine user name from
  panic locations; verified: zero personal-name strings in the shipped exe).
  RUSTFLAGS overrides config rustflags, so build.py restates the CFG flag.
- macOS `launchctl kickstart` target now interpolates `libc::getuid()`
  (the literal `$(id -u)` never resolved — launchctl gets no shell).

Audit evidence (2026-08-28, commit 93c460d): cargo-audit clean (456
deps), gitleaks clean (tree + 75-commit history), clippy -D warnings +
fmt clean, 134+ tests pass, `--selfcheck` green on the hardened release
binary. Offline app (no networking crates at all). Known coverage gap: no
cross/zigbuild on this machine → Linux x86_64 build/ELF inspection not
verifiable locally; ARM64/macOS targets are not built by build.py.

## 2026-08-28 — Settings: always-start-elevated policy + one-shot restart

Windows-only "Administratorrechte" / "Administrator privileges" section in
the settings dialog (inside the existing `Advanced` block, below the Task
Manager replacement):

- **Status line** — whether THIS process is elevated. `TaskManApp::new`
  queries `actions.is_elevated()` exactly once (`is_elevated` field);
  elevation is fixed at process creation, never per frame.
- **"Always start with administrator privileges"** (`start_elevated`,
  `[general] start_elevated` in config.ini): at startup, `run_gui` checks
  the setting BEFORE any window exists and, when this launch is unelevated,
  re-execs via `tm_platform::win::relaunch_elevated_with_args(args)`
  (ShellExecuteExW "runas", CLI args forwarded) and `std::process::exit(0)`.
  The elevated child re-reads the setting, is elevated, and proceeds — no
  loop. A declined UAC prompt logs a warning and degrades to a normal
  unelevated start (retried next launch). Guards: `TASKMAN_CONFIG_DIR`
  override (test/isolated context) never auto-elevates; `--selfcheck` and
  the `--taskmgr-integration` helper exit before the check. Every launch
  therefore shows a UAC consent prompt — inherent for third-party exes
  (Task Manager itself auto-elevates via its system-binary status).
- **"Restart as administrator" button** — one-shot elevation of the
  current session: `PlatformActions::relaunch_elevated()` (no args) on the
  action executor; on success the job sends `ViewportCommand::Close` from
  the executor thread — safe because `Context` is Send+Sync and commands
  queue into the next frame; eframe 0.36 exits the event loop gracefully
  (`should_close`), so `on_exit` still flushes settings + app history. A
  declined prompt surfaces the standard error toast; the single executor
  worker serializes repeat clicks, so two prompts cannot race.

Tooling note: `tools/capture.ps1` now seeds an isolated temp config dir
(`TASKMAN_CONFIG_DIR` + `config.ini`, ASCII on purpose — a UTF-8 BOM would
break the INI parser's first `[general]` header) instead of writing legacy
`settings.json` into the REAL config dir; this also keeps captures working
when `start_elevated` is on (auto-elevation is skipped under the override).

Verified visually via `tools/capture.ps1` with `TASKMAN_DIALOG=settings`;
UAC consent paths not exercisable headlessly. `build.py --check`
(fmt + clippy `-D warnings` + workspace tests, incl. release build)
passed.

## 2026-08-27 — Details column prefs persist; last resize handle grabbable

Two user reports, both on tables:

1. **Column visibility/order was session-only.** The Select-columns dialog
   state (`details::State.visible`/`order`) never reached `config.ini`.
   Fixed with two new settings fields: `col_visible` (`table -> id -> on`,
   ONLY entries differing from the built-in default, so future builds'
   new columns keep their designed default) and `col_order` (`table ->
   [ids]`, stored only while it differs from the built-in order). INI
   schema: `[columns.<table>.visible] <id>=0|1` and
   `[columns.<table>.order] order=<id>,<id>,...` — parsed under the
   existing `columns.*` prefix logic via `rsplit_once('.')`. Applied at
   startup in `TaskManApp::new` (`details::State::apply_saved_prefs`, with
   guards: never empty the table, sort column always visible, unknown ids
   skipped, missing ids keep built-in position); written back by
   `details::persist_column_prefs` on every dialog mutation through the
   usual debounced `save_settings()` path. Hidden GPU columns also lower
   telemetry demand correctly from startup.

2. **Last column's resize handle ungrabbable when the table is wider than
   the window.** egui hit-testing clips widget rects to the scroll area's
   clip rect; the header scroll area had NO content margin while the body
   reserves `BODY_PAD_RIGHT` (10 px), so fully scrolled right the last
   boundary sat flush at the viewport edge — only the inner ~6 px of the
   ±6 px handle were clickable, effectively unreachable. Fix: the header
   gets the same right `content_margin` as the body (both in
   `scrolled_table` and `scrolled_rows`), which also aligns header/body
   far-right geometry. Regression test
   `last_boundary_is_grabbable_when_scrolled_fully_right` drives the real
   `scrolled_rows` path (priming the stored body offset BETWEEN passes —
   egui pass memory starts lazily, and `insert_temp` is type-generic:
   an untyped `10_000.0` literal silently stored as f64 and was never read
   back by the f32 reader). Verified the test fails without the fix.

Tests: settings roundtrip + new-section parsing; details prefs
roundtrip/reorder/guards; tablekit last-handle drag. `build.py --check`
(fmt + clippy `-D warnings` + workspace tests) passed; release build
packaged.

## 2026-08-27 — Performance chart freeze (VecDeque ring wrap)

User report: sometimes the Performance graphs / card sparkline previews
stop updating while the rest of the app stays live. Root cause was NOT the
engine/wakeup path (engine → `request_repaint` → eframe event loop was
verified sound) but `TaskManApp.history`: a `VecDeque` with capacity
`history_cap + 8` that `poll_engine` keeps at `len == history_cap` via
pop-front/push-back. Once the ring wraps (after ~cap ticks — 2 min at
Normal speed), `as_slices()` returns TWO runs and the newest points live in
the SECOND one — which `performance::window()` discarded (`let (full, _)`).
Every frame then rendered a stale front-run: frozen for 119 of every 120
ticks (verified with a standalone ring probe). The one-tick catch-up blip
per cycle explains the "sometimes" flavor.

Fix: history is a plain contiguous `Vec<HistoryPoint>` (always
append-ordered; `push_history_point` extracted for the regression test
`history_retention_keeps_newest_point_visible`). Sibling hardening in the
same symptom class: `visible_slice` now scans backward from the newest
sample instead of `partition_point` (robust against a backward wall-clock
step leaving future-stamped older points), and `chart_multi` computes its
x-span with `saturating_sub` (the old `last - first` wrapped/panicked on
such data). If history ever becomes a deque again, windowing must handle
both slices or call `make_contiguous` — see the field doc in `app.rs`.

Tests: retention-through-wrap (app.rs), backward-clock-step window
(performance.rs); `cargo test -p tm-app` 55 passed, clippy clean, release
build packaged.

## 2026-08-27 — TM-parity resource sorting (flat list, no group sections)

Follow-up to the attribution fix, from a side-by-side screenshot: native
Win11 Task Manager keeps the Apps/Background/Windows sections ONLY when
sorted by Name; any resource sort (CPU/memory/disk/network) flattens the
whole list into ONE globally sorted sequence (family groups like
"Brave (29)" stay collapsible). We kept the sections, burying top
consumers.

Fix in `build_display_rows` (tm-app/tabs/processes.rs): group headers and
collapse are applied only for `sort_col < 2` (Name/Status); for resource
sorts `sort_blocks_globally` reorders the per-section emission: the
emitters produce self-contained BLOCKS (depth-0 head row + its expanded,
nested children), blocks are sorted by the head's representative value
(subtree aggregate for family/tree heads, own values otherwise) and
concatenated. Expanded families therefore stay attached while heads
compete globally. Group-collapse state is ignored in the flat view
(native TM offers no group toggles there either). Do NOT "fix" this back
to per-section grouping for resource sorts.

Also: the "Terminated processes" pseudo-row shows its count only when > 0
(a residual without observed exits — born-and-dead-within-one-window
churn is never sampled alive — must not read "(0)"); new i18n key
`TerminatedProcessesPlain`. NOTE for tests: a busy DIFFERENT-image child
of an app (≥ 1 % cpu) is intentionally promoted to a Background top-level
row by `promote_busy_external_tasks`, so it competes globally in the flat
view — block-attachment tests need same-image children.

Tests: flat ordering across categories (top consumer first, no headers),
name sort keeps sections, expanded same-image family stays attached,
pseudo-row label/tooltip.

## 2026-08-27 — CPU attribution completeness (terminated processes, interrupts)

User report: while compiling in a terminal, the Processes page showed NO
process owning the CPU load even when sorted by CPU. Root cause was in the
time-based accountant (`win/cpu_load.rs::build_sample`), not in grouping:

- **New processes got a fabricated 0 %** on their first sample
  (`.map_or(0.0, ...)` when absent from the previous sample).
- **Processes that terminated during the sampling window contributed their
  whole in-window CPU time to NO row**: the per-process loop iterates the
  CURRENT `SystemProcessInformation` table only, so a `rustc.exe` born and
  dead inside one ~1 s window (typical for small crates) was never seen at
  all. The global number (per-core accumulators) sees everything — hence
  "high load, no responsible process".

Fix (accounting completeness, all in `cpu_load.rs` + `win/sampler.rs`):
1. Processes born inside the window are credited their accumulated time
   since creation (which for them is exactly in-window time); reused pids
   get the same treatment (create_time guard).
2. `LoadSample` now carries `unattributed_pct` (global busy − Σ live-process
   in-window time) plus `exited_count`/`exited_images` (image names parsed
   from the NT table's `ImageName`, remembered from the previous sample).
3. `sampler.rs` splits the residual: measured `% Interrupt Time` (new PDH
   group `interrupt`, gated on CORE_PROCESS, counter path
   `\Processor Information(_Total)\% Interrupt Time`) → "System Interrupts"
   row (System/Windows group, TM parity); the rest → "Terminated processes
   (N)" row (Background) with exited image names as a localized hover
   tooltip. Both are SYNTHETIC `ProcessEntry` rows (`synthetic: bool` on the
   model, sentinel pids `u32::MAX`/`u32::MAX-1`), appended AFTER
   `refine_categories_and_group_apps` so the classifier never touches them;
   they sort/heat-map/search like any row. Rows show only above 0.5 % with
   a 5-tick hold-decay (`HeldPseudoRow`/`PseudoRowHold`) so bursty churn
   does not flicker; a measured-low interrupt value hides immediately (only
   UNKNOWN measurement decays — never read missing as zero).
4. Actions are withheld: no context menu on synthetic rows; the header
   aggregate comes from `snap.cpu.utilization_pct`, so no double counting;
   users tab skips them (no session); details shows them like native TM
   shows "System interrupts" (Del/kill guarded by `identity_is_live`).

KEY EMPIRICAL FINDINGS (pinned by a live-kernel unit test
`live_kernel_table_yields_sane_image_names`):
- `SYSTEM_PROCESS_INFORMATION.ImageName.Buffer` is an **absolute pointer
  into the output buffer** on this Windows build (NT writes the caller's
  buffer in place — matches ReactOS `SpiCurrent->ImageName.Buffer =
  (void*)(Current + CurrentSize)`); the Process-Hacker-style record-relative
  interpretation decoded 0/285 names here. `parse_image_name` therefore
  tries absolute / table-relative / record-relative candidates, all bounds-
  and control-character-validated, empty name on any doubt. The i18n
  `keys!` macro CANNOT take multi-line array entries (`expr` fragment
  matcher breaks on the newline before `,`) — keep entries single-line.

Tests: accountant unit tests (new-process credit, residual → exited
names, buffer conventions, live table), sampler hold/decay + append tests,
Processes-tab presentation tests (pseudo rows in the right groups, sorted
by CPU, tooltip only for synthetic rows).

## 2026-08-27 — command lines, real CPU speed, high-CPU background visibility

Three user-reported bugs fixed:

1. **Details command line always "—" on Windows**: `ProcessEntry.command_line`
   was never populated (only the Linux backend did). Fix:
   `process_ops::command_line_of(pid)` via
   `NtQueryInformationProcess(ProcessCommandLineInformation)`. IMPORTANT
   finding: on this Windows build the correct PROCESSINFOCLASS value is **60**
   (matching windows-rs 0.62's `Wdk_System_Threading` binding) — the older
   "class 92" reference does not work here (STATUS_INFO_LENGTH_MISMATCH with
   any buffer). Works with only `PROCESS_QUERY_LIMITED_INFORMATION` (no
   VM_READ); elevated/protected processes fail to open → None → "—". Wired
   through the 10 s TTL `PidAttrs` cache in `sampler.rs` (new field
   `command_line`); integration + unit tests spawn a child and assert the
   args are retrieved.
2. **Performance CPU speed stuck at base clock**: sysinfo's frequency comes
   from `CallNtPowerInformation(ProcessorInformation)` `CurrentMhz`, which
   reports the *nominal* clock constantly on modern Windows (verified: static
   3401 MHz on a 5700X even under load; WMI CurrentClockSpeed identical).
   Fix, Task-Manager-style: new demand-gated PDH group `cpu` with single
   counter `\Processor Information(_Total)\% Processor Performance`
   (`perfcounters.rs`; new `TelemetryDemand::CPU_SPEED` bit 8, set for
   Tab::Performance). `sampler.rs` computes `freq_mhz = base × pct/100`.
   Fallback ladder: counter warming → 0 (UI renders "—", never fakes data);
   counter permanently unavailable (`cpu_counter_failed`) → sysinfo value.
   Counter needs 2 PDH collections before formatting succeeds (matches
   existing `QueryGroup` warm-up). Verified live: idle 4.2 GHz, under load
   4.4 GHz (base 3.4).
3. **High-CPU background/CLI tasks invisible on Processes page**: TWO root
   causes, both fixed:
   - **Background/Windows groups render as TREES** — a busy build tool under
     a console shell (cmd → cargo → rustc, all Background) was hidden as a
     child row under the unexpanded shell row; only the aggregate leaked
     into the parent. Fix: Task Manager parity — **Background/Windows groups
     are FLAT lists** (every process its own depth-0 row with its OWN values,
     sorted by the current column, no expand handles). Only the Apps group
     keeps the family tree. Verified with a live-system repro test: busy
     powershell under cmd becomes a visible flat row.
   - App-absorbed external tasks: promotion pass in `derive_display_groups`
     (`promote_busy_external_tasks`, `is_external_family_member`,
     `PROMOTE_CPU_PCT = 1.0`): an absorbed non-root process with cpu share
     ≥ 1 % whose image differs from every family ancestor is reclassified to
     Background (with its absorbed descendants, wholesale) and appears as an
     ordinary flat Background row; same-image helpers (Chrome renderers
     etc.) stay folded like TM app children. Two-phase decisions (against
     pre-promotion categories) keep the result iteration-order independent.
     Guards learned from the HitmanPro report: **windowed processes are
     never demoted** (they are foreground apps), and the wholesale descent
     skips windowed children (they surface as their own app roots).
   - **Windowed absorption refined (`plausibly_same_application`)**: a
     windowed process folds into a windowless ancestor's family only when
     they share the image or the publisher (company from version metadata;
     unknown publisher falls back to permissive). Start-menu/COM launches
     are brokered by windowless shell-session processes (sihost,
     RuntimeBroker, dllhost — NOT explorer), which would otherwise adopt
     the launched app (HitmanPro case); those brokers plus browsers are
     launch boundaries now. Boundary check precedes the company check, so
     same-image secondary browser windows/PWAs start their own rows
     (TM shows PWAs separately); non-boundary same-company families
     (steam.exe/steamwebhelper) still absorb.
   - **Background/Windows family collapse (TM parity per user's TM
     screenshot)**: connected same-image families render as one expandable
     `Name (N)` row with the family aggregate ("Dropbox (7)"), expanding to
     member rows; unrelated same-name processes and mixed-image trees stay
     flat (`emit_flat_with_family_groups`, `same_image_family`).
   Existing test fixtures set explicit low `cpu_pct` where promotion would
   otherwise trigger (proc() helper defaults cpu = 1.0×pid). NOTE:
   `cpu_pct` is share of TOTAL machine capacity — a full core on a 16-thread
   machine shows as 6.25, not 100; don't key logic off raw "100%".

## 2026-08-27 — window placement UX, type-ahead scroll fixes, dialog chevrons

Three user-reported issues fixed:

1. **Window size/position not persisted**: root cause on the affected
   machine was `remember_window=false` in `config.ini` — a setting with NO
   settings-dialog UI, so it could not be re-enabled. The Settings dialog
   now has a "Remember window size and position" checkbox (i18n key
   `RememberWindow`, persists via autosave). Additionally, maximized state
   is now part of placement: while maximized, neither the restore size nor
   the position is clobbered with monitor geometry; `window-state.ini`
   gains a `maximized=` key and startup re-maximizes via
   `ViewportBuilder::with_maximized`. See `ui_state.rs` (Placement struct)
   and `NativeApp::ui` in main.rs.
2. **Select-columns dialog arrows**: the →/← text buttons sat under the
   floating vertical scrollbar (right edge) and could not be clicked. They
   are now painted chevron icons (`controls::icon_button`, new
   `icons::Icon::ChevronUp`), moved 16 px left of the scrollbar strip, and
   reordered to ↑/↓ (up = earlier position, down = later).
3. **Type-ahead scroll**: plain-letter navigation (Processes/Details)
   used `Response::scroll_to_me(Some(Center))` on a virtualized row, which
   (a) never fired for rows outside the rendered window (no vertical
   scroll at all) and (b) when it fired, targeted BOTH scroll axes,
   yanking the table horizontally. `tablekit::scrolled_rows` now takes a
   one-shot `focus_row: Option<usize>` and computes a vertical-only,
   minimal-move offset from the last frame's y-offset (`tm-rowsy` temp)
   applied via `ScrollArea::vertical_scroll_offset` on the request frame
   only. Callers pass the index from `scroll_to_pid.take()`; the per-row
   `scroll_to_me` mechanism is gone. Regression test:
   `focus_row_scrolls_vertically_only_even_for_unrendered_rows`.
   `search::cycle_process_initial` was genericized to `cycle_match<T:
   PartialEq + Clone>` and the Performance card column gained the same
   type-ahead (jump + vertical scroll via `scroll_to_me(None)`, which for
   full-width items can never move horizontally).

Also fixed (found by the heavy gate): `cpu_info::base_mhz_from_smbios_table`
returned `max(current, max)` while its tests document current-speed-
preferred-with-max-fallback — pre-existing failing test
`smbios_type4_current_speed_is_preferred`, now green.

Note: HEAD was not fmt-clean under the local rustfmt
(1.10.0-nightly 2026-08-25); `python build.py --check` failed on the
pristine tree. The formatting drift in previously untouched files
(fonts.rs, chart.rs, linux/*, win/mod.rs, taskmgr_replacement.rs) is
mechanical rustfmt output required to keep this machine's gate green.

Validation: `python build.py --check` (fmt, clippy -D warnings, all
workspace tests) + release build/packaging + `--selfcheck --mock` pass.

## 2026-08-27 — Processes app-grouping parity

The Processes page now builds a presentation topology instead of treating
raw PPID as UI ownership. Explorer and common shell launchers are boundaries,
so programs the user starts from Explorer/cmd/PowerShell/Terminal appear as
independent app groups while helpers remain under their app family. Raw
`ProcessEntry.ppid` is unchanged.

- App membership is rebuilt from visible-window ownership while preserving
  System classification; no-window backends retain collector categories.
- Display parent edges are cut across category boundaries and at app roots,
  so Explorer no longer inherits CPU/memory/subtree counts from launched apps.
- `Apps (N)` now counts top-level app groups (matching native `Apps (9)`);
  Background/Windows keep unflattened process counts.
- Cyclic/malformed PPID components stay visible instead of disappearing.
- Regression coverage covers Explorer launches, shell-launched GUI apps,
  app-group totals, aggregate boundaries, and cycle visibility.
- CPU load/accounting code was not changed.

Validation: code was statically reviewed through the GitHub connector. This
environment has no Rust toolchain/Windows runtime, so the requested local
build remains the final compile/runtime/UI check.

## 2026-08-26 — audit.md Phase 1 (correctness) implemented

All 11 Phase-1 items from the 2026 parity audit landed, each with
regression tests:

1. **Table width architecture (P0.1)**: `TmColumn::elastic` and the
   index-0/"width==0 sentinel" fill behavior REMOVED. Every column uses its
   configured/persisted width; unused viewport space stays blank on the
   right like native TM. `TmTable::new`/`make_table` lost the `name_min`
   param; `col_width(i)`/`total_width()` no longer take `avail`; layout is
   width-driven (rebuilt only on mutation). Double-click restores
   `default_w`.
2. **Per-column heat normalization (P0.2)**: `heat_cells` now paints given
   `HeatCell { intensity, text }` values; callers normalize per COLUMN over
   the whole display model BEFORE virtualization (`tablekit::norm`,
   processes' `normalize_heat`, users' `HeatMax`, apphistory maxima).
   "value>0 ⇒ 1.0" binary intensities are gone.
3. **Details GPU demand (P0.3)**: `show_gpu_columns` bool removed;
   `State.visible: BTreeSet<ColumnId>` is the single source of truth, both
   for rendering (dynamic column list) and `requires_gpu_telemetry()`
   demand derivation. Minimal Select-columns dialog added ("…" overflow),
   session-only persistence (see known-debt.md).
4. **Users search fixed**: old condition kept every active user visible;
   now query matches user display name OR any aggregated app name.
5. **Global search + shortcuts**: new `tm-app/src/search.rs` `Query`
   matcher (binary name/display/PID/publisher) used by Processes, Details,
   Startup (+publisher) and App History. Alt+F and Ctrl+F focus the global
   search field (`egui::Id::new("global-search")`).
6. **Performance Refresh now**: actually calls `refresh_all()`.
7. **Startup impact**: disabled items report `None` (win/startup.rs);
   enabled-without-data stays Unknown; real thresholds = later SRUM work.
8. **Group counters (P0.4/P0.5)**: `DisplayRow::GroupHeader(gi, total)`
   carries unflattened classification counts; grouped labels use O(n)
   whole-subtree process counts (`subtree_values_and_counts`). RowData lost
   its now-unread `group` field.
9. **Selection identity (§7)**: `selected_pid` replaced by
   `selected_process: Option<ProcessIdentity>`; `end_selected` validates
   start-time identity against the live snapshot before dispatch
   (`TaskManApp::identity_is_live`); Efficiency toggles validate too.
10. **Efficiency mode from OS state (§8)**: leaf icon/menu derive from
    `ProcessEntry.power_throttled`; `efficiency_pids` HashSet deleted;
    toggle issues one forced refresh so paused mode updates as well.
11. **History capacity (§10)**: `history_cap_for()` recomputes whenever
    `graph_seconds` changes (logic pass), truncating overflow.

Also fixed in passing (§23): services fetch + service-control workers wake
the UI via `Context::request_repaint` (Services page could stick on
"Gathering data" while paused). Stale cpu_load.rs doc rewritten to the 2026
metric split (current pages = time-based; utility survives as Details
"CPU Utility" column; legacy provider = future work).

Gates: fmt+clippy(-D warnings)+workspace tests+release build all green on
Windows (`build.py --check`, then `--host-only`); selfcheck --mock ok.

## 2026-08-26 — Visual parity pass vs real Win11 TM (taskmanpngs/ reference)

Measured the real Task Manager screenshots (taskmanpngs/1..7.png, captured at
133% scaling) pixel-by-pixel and re-derived the design tokens. Reference
logical sizes (Segoe cap≈0.75·font): rows 13, header labels 12, header
aggregates 17, group headers 20–21, sidebar/search/tab-title 15, titlebar 13,
kv rows 13 (pitch 22.5), captions 11, stat values 23, card titles 17,
page title 31. Colors (dark): content 0x191919, sidebar/chrome 0x202020
(sidebar LIGHTER than content), header separators 0x2D2D2D, heat base
(17,36,62), top-consumer cell (8,51,110), heat cell separators (41,50,63).

Changes:
- theme.rs: new palette (window 0x191919, sidebar 0x202020, stroke 0x2D2D2D,
  heat_top/heat_sep added), Body/Button text style 13.
- tablekit.rs: ROW_H 32, HEADER_H 57; `ui.spacing_mut().item_spacing.y = 0`
  before show_rows so rows TOUCH (the 6px default gap striped the heat
  bands); header agg font 17 at top+19, label font 12 at bottom−13;
  heat_cells now draws the flat TM style: base fill + brighter `heat_top`
  cell for each column's top consumer (max intensity) + 1px separators
  between cells. `heat_blue` gradient kept (dead_code) for future use.
- processes.rs: group headers 20px, NO background band (ref has none);
  heat intensities are binary (value>0 ⇒ 1.0) — top consumer per column wins.
- users.rs/apphistory.rs: same binary intensity model.
- app_ui.rs: nav items 15px text, h=38, accent bar 3×18; search box 495px
  wide with 15px font; cmd buttons 13px; toasts 13px.
- performance.rs: page title 31, right detail 17, card titles 17, card value
  lines 13, captions 11.5, big/med stat values 23 with 13px labels
  (56/48px blocks), kv rows 13px pitch 23.
- chart.rs: clippy `chunks_exact(3)` → `as_chunks::<3>()`.

Verified against ref by downscaling our 200%-DPI captures by 2/3 and
comparing crops side by side (shots/ui3-*.png).

## 2026-08-26 — Column/splitter resize ACTUALLY fixed (egui delta semantics)

**Root cause of "columns can't be resized" (persisted across the 2026-08-25
fix):** implement.md §8.1 claimed `Response::drag_delta()` is cumulative from
drag start — FALSE in egui 0.36. It is `pointer.delta()` = movement since the
LAST FRAME; only `total_drag_delta()` is cumulative. The shipped math froze a
drag-start width (`start_w + drag_delta().x`), so every frame reset the width
to ~its starting value: boundaries jiggled sub-pixel and snapped back when
the pointer paused. The 2026-08-25 fix (materializing the elastic Name
column) addressed a real but DIFFERENT failure layered on top; neither made
resizing work.

Fix (tablekit.rs + performance.rs splitter): accumulate each frame's delta
onto the LIVE width, `width = (width + dx).clamp(min,max)`; drop the
drag-start temp-data machinery entirely.

Also fixed in the same pass:
- Resize handles were registered DURING the cell loop, so each next header
  cell covered the right half (±6 px) of its neighbor's handle and won hit
  testing there — grabs landed on the cell and quick clicks even toggled
  sorting. Handles are now ALL created after the cell loop (topmost), full
  ±6 px grabbable; the last column keeps its right-edge handle.
- Double-click-to-default never worked: a `Sense::drag()` widget never
  receives egui click flags, so `double_clicked()` was always false. Now
  detected via `pointer.button_double_clicked(Primary)` while hovered.
- Regression tests drive REAL pointer events through an egui `Context`
  (`ctx.run_ui` + RawInput events; clear `out.textures_delta` or egui panics
  headlessly). Verified the test FAILS against the old math (730 ≠ 760).
  Test-authoring gotchas: egui counts multi-clicks ACROSS sequences within
  0.3 s (space sequences >0.3 s apart); boundary x positions must be derived
  from CURRENT column widths after any mutation.

implement.md §8.1/§14.1 corrected in place.

## 2026-08-25 — UI polish pass + Linux backend repaired & verified under WSLg

**Column resize root cause (tablekit.rs):** the elastic name column absorbed
viewport slack EVERY frame, so a manual drag of any other boundary was
cancelled in the same frame (dragged separator stayed put, Name/Status
divider shifted instead, regime flips around `spare == stored` read as
wobbling). Fix: `name_effective` is elastic ONLY while `width == 0.0`
(virgin sentinel); the first `drag_started` on any table materializes the
name width (value-preserving), after which all columns are explicitly
sized and boundaries track the cursor 1:1. Double-click on the name
separator restores fill mode via the 0.0 sentinel. Also: last column got a
right-edge resize handle; removed the extra stroke box around the Name
header cell; `table_avail` margin 6→16 px; body ScrollAreas got
`.content_margin(right:10, bottom:8)` so FLOATING scroll bars never paint
over the last column/bottom row (floating bars never reserve layout space;
reserving would desync header/body widths).

**Graph rendering root cause (chart.rs):** all area fills used
`Shape::convex_polygon`, but epaint fan-triangulates fills from vertex 0
(`fill_closed_path`) — only valid for CONVEX polygons. Concave series got
straight fan edges cutting across dips: fake linear ramps, cliffs, fills
floating above the line, white gaps (see graphpngs/1-3.png). Fix:
`fill_area_to_baseline()` builds an explicit x-monotone triangle strip
(2 verts + 6 indices per segment) that hugs the polyline exactly; used by
sparkline, core_chart (incl. kernel band) and chart_multi. Also: series
extractors (`disk/net/gpu_series`) now return one value PER window point
(0.0 when the device is absent; no more zero-filtering in net_series) so
series indices stay aligned with `timestamps_ms` — shortened series used
to plot at wrong x positions; chart_multi hover maps pointer time back to
the nearest sample via partition_point.

**Other UI:** checkbox hover theme-aware (accent border over faint tint,
no white flood in dark mode); chart strokes/fills strengthened, kernel
times = darker accent shade (`kernel_color`), per-theme secondary colors
(`pal.ok_green` not hardcoded dark-theme green).

**Linux backend (tm-platform/linux):** was NOT compiling — model additions
from Windows work (`CpuInfo::kernel_pct/per_core_kernel_pct`,
`GpuInfo::luid/shared_used_bytes`) never landed there, plus missing
`use std::sync::Mutex` and 5 clippy lints. Filled with documented "unknown"
values (empty vec / 0 / None) matching model docs & mock. Verified:
- `--selfcheck` real backend ok:true (CPU/mem/disks/networks/processes;
  GPU honestly empty — WSL2 exposes no /dev/dri for DRM).
- GUI runs under WSLg Wayland: wgpu+Vulkan at locked ~60 fps (Fifo), surface
  1100×720; glow renderer starts too. Note: wgpu hides "Microsoft Direct3D12
  (NVIDIA ...)" as not-Vulkan-compliant → renders via llvmpipe software path,
  still vsync'd 60 fps.
- `goto_services_for_pid`: cfg_attr allow(dead_code) on non-Windows (callers
  are cfg(windows)); keep compiling everywhere.

**WSL/Windows dual-host gotcha:** WSL builds produce `target/release/taskman`
(ELF) while the launched binary is `taskman.exe` — a WSL-side
`build.py --host-only` does NOT refresh the exe. Refresh it from WSL via
interop: `cmd.exe /c "... && set PATH=%USERPROFILE%\.cargo\bin;%PATH% &&
py.exe build.py --host-only"` (works; selfcheck via powershell
Start-Process -RedirectStandardOutput since GUI-subsystem console attach
fails through the interop pipe). Consider separate CARGO_TARGET_DIR per
host to avoid cache thrash.

## <YYYY-MM-DD> — Template scaffold created

Initial generic `AGENTS.md` / `llm-wiki` template. No project-specific
history recorded yet.

- 2026-09-08: Process Properties now uses a bounded scroll viewport (fixing the frame-over-frame vertical growth regression) and adds an on-demand Windows Security page. Security telemetry is identity-checked on the queried handle and includes process protection/PPL, critical state, exact image machine and active mitigation policies (DEP, ASLR, dynamic code, strict handles, CFG, Win32k lockdown, signature/font/image-load/child-process restrictions, CET, SEHOP, side-channel and payload restrictions).

- 2026-09-08: Process Properties vertical sizing now follows the user's actual resizable window height while keeping only the inner scroll body bounded. Windows security inspection moved onto PlatformActions and can use the authenticated LocalSystem core-service broker for SYSTEM/service processes; the broker request remains identity-bound and read-only, with local fallback for absent/older services.

- 2026-09-11: Set-affinity dialog gained Select all / Deselect all buttons above the CPU grid. Making the context-menu subject caption read as non-clickable took three rejected attempts: an outlined recessed chip (looked like a text input), a filled band (looked like a selected row), and dimmed caption text (hard to read). What shipped: the caption stays plain full-strength text and the menu separator was made visible — `pal.stroke` is mixed against the window background and lands two values off the popup fill on dark (0x2d on 0x2b), so rules on a popup now derive from the popup surface instead.

- 2026-09-11: Users page Network column was a hardcoded "—" — the tab aggregated CPU/memory/disk per session but never network. It now sums `net_recv_bps + net_sent_bps` per session and per app, tracks whether ANY member measured it, and keeps unknown out of the heat band and at the bottom of the sort. Unknown still renders "—" (with the Processes page's ETW explanation on hover), so a machine without the per-process network trace looks unchanged.

- 2026-09-11: Users page Network stayed "—" even after it started aggregating: the tab never requested `PROCESS_NET`, so the per-process ETW session was not running while it was on screen (demand is derived from the visible tab in `app.rs::update_demand`). Users now requests the same sources Processes does and mirrors its whole numeric block — six columns, draggable order persisted per table, per-column known/unknown, heat and sorting. The shared catalogue moved to `tabs/value_columns.rs`. Two related fixes fell out: the Processes hover explanation looked up the cell by LOGICAL index (wrong cell once columns were dragged), and the Disk activity column explained itself with the NETWORK string.

- 2026-09-11: Released v0.1.5 (tag on `7755950`, bump commit). Followed `build.md` §Publishing: `--check` + `--audit` green, version bump pushed on main, `build.py --all-targets` for all four archives, `sha256` beside each, `gh release create --latest`. Gotcha worth remembering: `gh release create --target <short-sha>` is rejected with `HTTP 422 Release.target_commitish is invalid` — pass the FULL sha. Published archives verified by re-downloading: checksum matches and both binaries pass `--selfcheck`.

- 2026-09-11: Both disk headers on Processes/Users read a flat "0 %" while a row showed a process at 51 % disk activity. `DiskInfo::active_pct` was `f32` and the Windows sampler published `perf.map_or(0.0, …)`, so "the PhysicalDisk PDH group is not collecting" became a measured-looking zero — and only the Performance tab ever requested `DISK_RATE`, so 30 s (the PDH keep-alive) after leaving it the counters shut down and the machine total the two disk columns are read against decayed to zero. Fixed at both ends: `active_pct` is `Option<f32>` on every backend (macOS has no source and says so; Linux keeps its diskstats value), `Aggregates::disk_pct` is `Option<f32>` and renders "—", and `update_demand` was split into the pure `app::demand_for` so a test can pin that every page showing the columns also requests the counters behind them. `--selfcheck` reports `disk_busiest_active_pct` (null = unmeasured). Renamed with it: `Disk` → `Disk I/O` / `Datenträger-E/A` (I/O bytes the process asked for, cache hits included) and `Disk activity` → `Disk active time` / `Aktive Datenträgerzeit` (its share of the time the disks were really busy). Header label widths were measured against the app's own font before shipping the longer German strings — the header clips, it does not ellipsize.

- 2026-09-11: Chart area fills were built with `Color32::from_rgba_premultiplied(r, g, b, 34)` — full-strength components handed over with an alpha claiming they had already been scaled by it, which blends ADDITIVELY. Over the dark card fill a CPU series composited to roughly `#71e7ff`, i.e. the "translucent" infill came out brighter than the `#4cc2ff` line bounding it, so every graph read as a solid slab with no outline. `widgets/chart.rs` now derives fills through `area_fill()` (`from_rgba_unmultiplied`, `FILL_ALPHA = 72`) on all three shapes (sparkline, core tile, `chart_multi`), and the kernel-time band through `kernel_fill()`/`kernel_shade()` — the readout swatch uses the opaque shade, a translucent one was a smudge on the panel fill. Second fix in the same pass: a Performance card lifts onto `pal.card_bg` when selected or hovered, and the mini graph kept painting `card_bg` too, so the cell dissolved into the row exactly when pointed at. `paint_sparkline` now takes the cell background and the card passes the new `Palette::card_bg_sunken` while raised.

- 2026-09-11: Paired chart series were told apart only by transparency. `gamma_multiply(0.62)` scales a `Color32`'s ALPHA along with its components, so disk write, network send, CPU kernel time, memory committed and the GPU engine ramp were all "the primary colour with more background showing through" — and `widgets::chart`'s own translucent area fill then thinned that again. Two fixes: the three genuinely overlaid pairs got opaque partner colours a HUE apart (`Palette::cpu_kernel_graph` indigo, `disk_write_graph` gold, `network_send_graph` teal, each tuned per theme), and the merely subordinate series (memory committed, GPU dedicated memory, the `all engines` brightness ramp) go through the new `theme::toned(pal, color, factor)`, which mixes towards `card_bg` and stays opaque. `core_chart` now takes `kernels: Option<(&[f64], Color32)>` instead of deriving the band colour as `color / 2` internally, so a core tile and the aggregate CPU chart paint kernel time identically. Pinned by `theme::tests::paired_series_are_opaque_and_far_apart`.
