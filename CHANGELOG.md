# Changelog

## Unreleased

### New

- **Release packages for Arch, Debian/Ubuntu and Fedora, named by distribution.** `python build.py` now writes `dist/taskman-v<version>-arch-x86_64.pkg.tar.zst` (the renamed pacman package), `dist/taskman-v<version>-debian-ubuntu-x86_64.deb` and `dist/taskman-v<version>-fedora-x86_64.rpm` (plus `-arm64` `.deb`/`.rpm` with `--all-targets`), built from the glibc binary with a pure-Python writer (`tools/distpkg.py`) so no `dpkg-deb`/`rpmbuild` is needed. All three install the payload `install-linux.sh --packaged` stages, so they cannot disagree with the tarball. `--no-distro-packages` skips the `.deb`/`.rpm`; the static musl fallback produces none because it cannot open a window.
- **Linux package upgrades now restart the GUI as well as the core service.** `install-linux.sh --post-install` (run by the pacman, `.deb` and `.rpm` scriptlets) and a tarball re-install relaunch every running TaskMan GUI as its own user, with its original arguments and display/session environment, right after `taskman-core.service` restarts, so an update never leaves the old binary on screen against the new broker. A GUI running as root, or one whose process cannot be inspected, is left alone with a warning.
- **Linux: Private working set (`Private_Clean + Private_Dirty`) for processes.** The Linux sampler now reads `/proc/<pid>/smaps_rollup` (`Private_Clean + Private_Dirty`) for non-zero-RSS processes — directly when readable and in batches via `taskman-core.service` (`PROTOCOL_VERSION` bumped to `4` with `TelemetryTarget::need_private_ws` / `ProcessTelemetryEntry::private_working_set_bytes`) for elevated or other-user processes — so the default **Memory** column, **Details → Memory (active private working set)**, **Shared working set** (`Working set − Private working set`), and **Process Properties → Private working set** match Windows semantics. When `smaps_rollup` is unreadable (for example when `taskman-core.service` is absent), `mem_bytes` falls back to RSS while `private_working_set_bytes` and `shared_working_set_bytes()` remain `None` (`—`) rather than fabricating `0 B`.
- **Linux: per-process per-file read and write I/O tracing.** Process Properties → I/O now displays measured per-file `Read`, `Written`, and `Total` bytes on Linux when `taskman-core.service` is active. The service parses `/sys/kernel/btf/vmlinux` at startup and attaches pure-Rust eBPF `fexit` trampolines to `vfs_read`, `vfs_write`, `vfs_readv`, and `vfs_writev` (requiring `CAP_BPF`, `CAP_PERFMON`, and the `bpf` syscall in `taskman-core.service`), plus a best-effort `fentry` probe on `filp_close` (`bpf_d_path`) so short-lived files closed between polls stay in the table as `(closed)` rows. Entries are keyed by `(start_boottime, tgid, s_dev, i_ino)` so a reused PID cannot inherit prior counts, and `PROTOCOL_VERSION` is bumped to `3` to carry the measured accounting state and counters for both the user's own and other users' processes. When the service is absent or eBPF is unavailable, rows fall back to open handles with `—`.
- **`taskman --selfcheck-icons` measures icon resolution headlessly.** Icon cost was the standing suspect behind the page slowdowns and the only part of the app nothing could measure without a GUI, because a row whose icon is still pending paints an empty cell. The new check takes one process snapshot, resolves the distinct image paths the pages ask for (through the same key rule, so it cannot drift), does a cold and a warm pass, and prints one JSON line with `keys`, `resolved`, `missing`, `cold_ms`, `warm_ms` and the p50/p95/max microseconds per key. It opens no window and needs no display, so it runs on a headless Linux box; the same measurement is available to the test suite as the ignored `bench_host_theme_icon_lookup`.
- **`python build.py` now ships the pacman package, on Windows too.** `dist/` carried only tarballs and installers, because the Arch step sat behind `--arch-package` and `makepkg` does not exist on a Windows host at all, so a plain release run could never produce `taskman-<version>-<rel>-x86_64.pkg.tar.zst`. Packaging now tries the Arch package every run: a native `makepkg` where one is installed, otherwise the first Arch WSL distro that has it (`TASKMAN_ARCH_WSL_DISTRO` names a distro instead of probing for one), with `PKGDEST` aimed at `dist/` and both paths translated by the distro's own `wslpath` rather than assuming `/mnt/c`. The `pkgver` and `.SRCINFO` checks run through that same host, so a stale description still fails before anything is packaged; the WSL route builds in `/var/tmp/taskman-arch` inside the distro, so a cold build neither lands gigabytes in the checkout nor depends on it being writable from there. A host with neither `makepkg` nor an Arch distro stays a logged skip and never a failed build, `--no-arch-package` skips the step deliberately, and `--no-package`/`--host-only` skip it with the rest.
- **`python build.py` on Linux and macOS now also builds the Windows installer.** A non-Windows host cross-compiles `x86_64-pc-windows-msvc` with `cargo-xwin` (lld-link plus the Microsoft CRT and SDK import libraries) and drops `taskman-v<version>-windows-x86_64.zip` and `taskman-v<version>-windows-x86_64-setup.exe` into `dist/` beside the Linux archive and pacman package. The binaries carry the same hardening (Control Flow Guard, CET compatibility) and the installer the same link properties (delay-loaded DLLs, static VC runtime, `requireAdministrator` manifest) as a Windows build, and the payload is embedded and verified by the host's own `taskman-payload`. `python tools/setup-windows-cross.py --accept-microsoft-license` prepares everything once inside the checkout (`.cross/`, gitignored: a private rustup with the matching toolchain, `cargo-xwin`, the Microsoft CRT/SDK cache), so nothing is written outside the project (the build's crate downloads land in `.cross/cargo-home` too); the build itself never installs a toolchain or downloads the Microsoft SDK. Without that toolchain the step is a logged skip with the command to run, never a failed build. `--host-only` and the new `--no-windows` leave it out, `--no-package` skips packaging, and `--require-all-targets` makes a skip fatal. `--check` now also type-checks `x86_64-pc-windows-msvc` where its std is installed, and finds installed targets in the sysroot when there is no rustup (a distribution's own `rust`).

### Improved

- **Linux: substantially lower CPU usage and memory footprint when active or hidden in the tray.**
  - `/proc/<pid>/smaps_rollup` private working set reads (and fallback `taskman-core.service` requests) now cache per process generation and rescan only when RSS moves by at least `64 KiB` or on a staggered `8–12 s` TTL, skipping hidden-to-tray ticks while clamping cached estimates against live RSS; `VmHWM` (`/proc/<pid>/status`) caches until RSS exceeds it or on a `5–9 s` TTL, `/proc/<pid>/cmdline` negative-caches transient empties with a short retry window, `/proc/<pid>/fd` `access()` checks only run when `PROCESS_NET` is demanded, kernel threads skip `cmdline`/`io`/ELF checks, disk list refreshes (`Disks::refresh`) match the 5 s topology TTL, DRM/NVML skips NVML probing on non-NVIDIA hosts, and per-PID `/proc` loops reuse stack paths and scratch buffers instead of allocating per process. Measured in headless sway (`1557x1016`) on a 140-process Ryzen desktop: `tm-engine` CPU dropped from ~12.4–13.0 % of a core to 3.2–3.6 %, and `--selfcheck` `sample_duration_ms` dropped from 125 ms to 23 ms.
  - At single-threaded startup on Linux (glibc), TaskMan configures `mallopt` (`M_ARENA_MAX=2`, `M_MMAP_THRESHOLD=64 KiB`, `M_TRIM_THRESHOLD=64 KiB`), disables transparent huge pages for its process (`PR_SET_THP_DISABLE`), and opts out of the implicit MangoHud Vulkan overlay (`DISABLE_MANGOHUD=1` unless `TASKMAN_ALLOW_MANGOHUD` is set) so inherited `MANGOHUD=1` no longer runs a polling thread in the background.
  - When hidden or minimized, the Linux `softbuffer` backend releases its shared-memory surface after the 15 s residency grace window (recreating it cleanly on restore), transient XDG icon directory listings and decoded RGBA buffers are cleared when the icon queue goes idle or enters the tray (`xdg_icons::shrink_transient_caches`), `epaint`'s font atlas width is capped at `2048` px instead of `16384` px (saving ~8–11 MB RSS/PSS on active windows), and `malloc_trim(0)` returns freed heap pages to the kernel.
- **Grayscale text keeps its weight in both directions, and Linux Fontconfig settings are respected.** When sub-pixel rendering is off (Linux `rgba=none`, Remote Desktop, portrait displays, GPUs without dual-source blending), glyph coverage now becomes alpha per text colour rather than per theme: light-on-dark text is lifted and dark-on-light text kept near native weight, so the Light theme's white toast text no longer gets the thinning meant for dark body text (it drew ~17 % lighter than plain coverage). The curve is applied by every renderer -- CPU, D3D12/Vulkan and OpenGL -- from one definition, and is calibrated against native DirectWrite grayscale text: `Sharp` keeps ~0.9 of native ink in both themes, `Standard` ~1.0. On Linux, `Sharp` respects Fontconfig `hintslight`/`hintnone` and an explicit `lcdfilter: default`; `hintmedium` now grid-fits fully, as FreeType's normal target does, instead of acting like `hintslight`. Faded ClearType text no longer has its solid interior boosted by the contrast curve on the CPU renderer, and italic/transformed ClearType text keeps upright weight there.
- **ClearType text weight is measured against native Windows text, and the GPU renderers match the CPU renderer.** A headless DirectWrite reference now pins the weight: `Sharp` keeps 0.89-0.93 of native DirectWrite ink at 100/125/150 % in both themes (thin and crisp by design; the Light theme used to be at ~0.83), `Standard` 0.98-1.13. `Sharp` uses FreeType's light LCD filter by default and no contrast boost, which keeps 1 px stems at 100 % from spilling into neighbours. On D3D12 and Vulkan the ClearType blend is calibrated to the CPU renderer's gamma model on both `Unorm` and sRGB surfaces, including the Light theme's white toast text (previously up to ~16 levels heavier on the GPU); a hardware suite measures it at a mean difference of about 1 level, with isolated pixels up to 7-8 levels where glyphs overlap or italics are sheared, which fixed-function blending cannot see. Instruction-hinted fonts (Segoe UI) are hinted for LCD output; autohinted Linux fonts keep the horizontal grid-fitting `Sharp` asks for.
- **Linux (Wayland): the CPU renderer repaints only what changed again.** Wayland's software surface swaps two buffers, so every buffer handed back is two frames old. The renderer treated that as "contents unknown" and redrew the whole window on every frame, including an idle telemetry tick that changed a single number. It now keeps a short record of which tiles each presented frame changed, and repaints exactly what the older buffer is missing. A test drives simulated 2- and 3-buffer swaps and requires every presented frame to match a full repaint pixel for pixel. Measured at 4K with 1.5× scaling: an idle Processes page went from 1.4 % to 0.3 % of a core on the UI thread, and a scripted scroll from 45 % to 29 %. Fragmented damage is repainted as its bounding box when that is cheaper. An unchanged frame is still committed on Wayland, so redraws stay paced by the compositor instead of spinning.
- **The CPU renderer draws a scrolling frame about 20 % faster.** Profiling a scripted scroll at 1.5× scale showed most of each frame going to wasted rasterizer work. Every triangle tested every pixel of its bounding box, so the dozens of overlapping triangles of a rounded shape tested the same pixels again and again. Every blended pixel and texture sample called the C library's `roundf`, because baseline x86-64 has no rounding instruction. And every anti-aliased edge merged into a batch with one textured shape did a full four-texel fetch of the atlas's white pixel. Each row now fills exactly its covered span, solved in closed form from the same integer edge equations, so the covered pixels are unchanged. Rounding and flooring use exact integer versions, and white-texel triangles are detected per triangle. Measured headlessly at 1650×1080: 8.8 → 7.0 ms of paint per scrolling frame. New tests pin the exactness against the per-pixel test and against libm.
- **Linux: the sampling engine costs half as much.** With the window idle, the once-a-second sample was the largest remaining cost, about 2.7 % of a core on a desktop with ~140 processes, nearly all of it kernel time spent in `/proc`. sysinfo listed and parsed every thread of every process just to count them. The thread count now comes from the process' own stat record, which the sample already reads. The GPU columns walked every descriptor of every process to find GPU device handles, even on the proprietary NVIDIA driver, whose handles carry no per-process statistics (NVML supplies those). That walk now runs only where a driver reports them, and is repeated only for processes whose descriptor count changed. Handle counts come from a single `stat` instead of listing each descriptor, and startup no longer reads every process' environment. Measured: 2.7 % → 1.3 % of a core, with each sample taking 10 ms instead of 24 ms. Handles are now also shown for other users' processes, where the kernel reports the count but refuses the listing.
- **`TASKMAN_SCROLL_PROBE=<seconds>` measures what scrolling and page switching cost.** It scrolls at a fixed 1,200 points per second, independent of frame rate, and walks the row-icon pages in turn. This works in a headless compositor, where synthetic pointer input never reaches the window. Read the per-thread CPU from `/proc/<pid>/task` alongside it. `--selfcheck-icons` now also lists `missing_keys`, the executables that paint the generic icon.
- **`build.py --check` gates the vendored forks on Linux too.** Without PowerShell the egui/winit fork gate was skipped; it now runs the same fmt/clippy/test steps from Python. The fork, workspace and Linux-only tests that had never run on a Linux host are fixed (toolchain lint drift in the fork, a racy socket test, a Windows-path test), and Windows-only dependencies are no longer linked into Linux builds.

### Fixed

- **Linux (Wayland): `--minimized-to-tray` autostart no longer leaves a ghost or brief taskbar entry.** When starting minimized to the tray, `winit`'s `Window::new` already sent the initial `wl_surface.commit()` and received a configure, which registers a configured `xdg_toplevel` on compositors such as KWin even before any buffer is attached; `Window::set_visible(false)` skipped unmapping because no frame had been presented yet, and KWin does not unmap a surface that never had a buffer attached. The bundled `winit` fork now sets `skip_taskbar` / `skip_switcher` via `org_kde_plasma_surface` before `Window::new`'s initial commit when `visible == false` (clearing both on `set_visible(true)`) and issues the null-buffer unmap commit whenever `!needs_initial_commit`.
- **Linux: `taskman-core.service` protocol version mismatch on install/upgrade and missing `.deb`/`.rpm` packages from `build.py` on Linux.** `packaging/linux/install-linux.sh` still hardcoded `PROTOCOL_VERSION=3` after `linux_core_service::PROTOCOL_VERSION` was bumped to `4`, causing `taskman-service` to exit with `protocol 3 is not supported (this build speaks 4)` on startup and hit `start-limit-hit`. `install-linux.sh` now defaults to `4`, queries `taskman-service --version` at install/post-install time when runnable, and a unit test (`install_script_protocol_version_matches_crate`) pins both in sync. `build.py` also now uses the native host glibc build when `target == host_target()` without `cross`/`cargo-zigbuild` (instead of falling back to headless musl), so `python build.py` on Linux produces `.deb` and `.rpm` packages in `dist/`.
- **macOS: memory column no longer mislabels RSS as Private working set or fabricates `0 B` for Shared working set.** `ProcessEntry` now tracks `private_working_set_bytes` separately from `mem_bytes` (`None` when unmeasured, such as on macOS or on Linux when `smaps_rollup` is unavailable), so `shared_working_set_bytes()` returns `None` (`—`) instead of `RSS - RSS = 0 B`, Process Properties omits the unmeasured "Private working set" row on macOS, and the macOS Processes tooltip and Details Memory column label report `Memory (RSS)`.
- **Process Properties → I/O file table sort caret:** replaced the Unicode `▲`/`▼` text characters and `✖` clear button glyph in `process_io.rs` with the vector `tablekit::caret` triangle and `×`, preventing placeholder square ("tofu") boxes on Linux fonts that lack those glyphs.
- **Linux: Chromium/Brave child processes no longer hide under `chrome_crashpad_handler` in Apps.** When a browser split its processes between an `app-*.scope` unit and an `app-*.service` unit whose oldest member was a `systemd --user`-reparented `chrome_crashpad_handler`, `merge_child_app_groups` only walked the parent chain of that single oldest member, failed to merge the `.service` unit into the browser's `.scope`, and left the browser's zygote/GPU/renderer processes grouped under `chrome_crashpad_handler` instead of the main browser row. `merge_child_app_groups` now inspects all `App` members of each group (with `is_plausible_parent` and transitive chain resolution), so the split unit merges into the main browser app group.
- **Linux (Wayland): window size and maximized state are now remembered across restarts.** On Wayland, `winit` does not support querying window coordinates, so `egui_winit` left `viewport().inner_rect` as `None`; `TaskManApp` read `window_size` only from `inner_rect`, and `ui_state::save` skipped writing `window-state.ini` whenever `position` was `None`. `TaskManApp` now falls back to `raw.screen_rect` for the window size when `inner_rect` is absent (while still ignoring minimized/maximized frames), and `ui_state::save` persists `maximized` even without a position. Window position itself cannot be saved or restored by a Wayland client under `xdg-shell` (compositor-managed, e.g. via KWin Window Rules matching `taskman`).
- **Linux: desktop foreground apps with clobbered environments and generic user services no longer fall into Unclassified.** Chromium and Electron apps (Brave, Antigravity, VS Code) overwrite `/proc/<pid>/environ` on startup, which caused the graphical-session check to miss them, split them across `app-*.scope` and `app-*.service` units, and drop their UI/renderer processes into Unclassified while only crashpad/extension helpers stayed in Apps. Clobbered or unreadable `app-*`/`snap.*` units now inherit confirmed unit or user-session graphical status (while keeping readable headless units in Background), child `app-*` units with the same executable merge under their parent app root, visible GUI `.desktop` apps in a graphical user session promote even outside `app-*` scopes, and non-app systemd user services in `app.slice` (`dconf-service`, `gnome-keyring-daemon`, `xdg-desktop-portal-gtk`) and `app-dbus\x2d:...` D-Bus daemon slices classify as Background instead of Unclassified or Apps.
- **Linux: elevated and non-dumpable processes report disk I/O, disk active time, and network stats when the core service is active.** The GUI client also reused a single-request socket after the broker closed it, failing every second request with `EPIPE` and triggering a 4-second cooldown. The client now opens a fresh socket per request matching the server contract, and a batched `ProcessTelemetry` broker request (`PROTOCOL_VERSION = 2`) supplies `/proc/<pid>/io` and `/proc/<pid>/fd` socket ownership for root and `PR_SET_DUMPABLE=0` processes while keeping them unavailable (`—`) when the service is absent or refuses.
- **Linux: Performance Disk and GPU graphs no longer have gaps after closing to tray.** While hidden to tray, `TelemetryDemand::hidden()` turned off `DISK_RATE` and `GPU_ADAPTER`, leaving Disk and GPU utilization/memory graphs unmeasured until restore. `LinuxCollector` now keeps `/proc/diskstats` and device-level DRM/NVML stats active alongside CPU, Memory, and Network while still pausing per-process GPU/disk/network walks. Restoring from tray also skips an immediate refresh when a sample was taken within the last 250 ms, preventing sub-jiffy `/proc/stat` reads that caused a 1-tick gap in per-core CPU graphs.
- **Linux: tray left-click reopens the window, and Wayland restores titlebar name and icon.** `StatusNotifierItem.ItemIsMenu` was `true`, which caused panels to open the context menu on left-click instead of activating/reopening the window; it is now `false` so left-click restores the window and right-click keeps the menu. On Wayland, unmapping the surface via a null buffer resets `xdg_toplevel` state, so reopening from tray left the window without its title or `app_id` (generic Wayland icon); `Window::set_visible(true)` now re-applies `app_id`, `title`, and size hints before the remap commit.
- **Linux: `--minimized-to-tray` no longer flashes a frame on WGPU/OpenGL.** On frame 1, `wgpu_integration` and `glow_integration` painted and presented before processing `ViewportCommand::Visible(false)`, which immediately mapped the Wayland surface for one frame. Both renderers now skip presenting frame 1 when the app explicitly requested `Visible(false)`.
- **Linux: fewer processes fall into Unclassified.** Systemd cgroup paths with `\xHH` hex escapes (such as `app-brave\x2dbrowser@....service` and `app-code\x2doss@....service`) and nested `.slice` units under `app.slice` were previously rejected as malformed, and kernel threads outside a 6-prefix allowlist (`idle_inject/*`, `irq/*`, `kswapd0`, etc.) were left `Unknown`. The cgroup parser now accepts valid `\xHH` escapes and `.slice` units, and `/proc/<pid>/stat` `PF_KTHREAD` flags classify all Linux kernel threads as System.
- **Performance and I/O graphs no longer show stacked transparent layers after a clock step.** When the system clock stepped backward (for example during an NTP correction shortly after boot), snapshots carried decreasing wall-clock timestamps, so the rolling window kept both pre-jump and post-jump samples and painted their translucent area fills and strokes on top of each other over the shared time span. Snapshot timestamps now advance from a monotonic epoch-anchored clock (`CLOCK_BOOTTIME` on Linux) so clock syncs never move history backward while suspend gaps still advance, and both `visible_window` and `ChartGeometry` stop at backward steps and trim overlapping runs so translucent fills never stack.
- **Process icons appear with their rows instead of up to a second later.** The icon workers finished their lookups within milliseconds but never woke the window, so a finished icon waited for the next unrelated repaint. On an idle window that is the next telemetry tick, so a freshly opened page, a new process or a scrolled-in row showed an empty icon cell for up to a second. The workers now request one repaint per batch of answers. A new snapshot also starts the lookups for every executable it contains, plus the platform's default icon, the moment it arrives, so switching pages finds the icons already resolved. A regression test fails without the wake-up.
- **Linux: per-process network, GPU and disk activity.** The Network, GPU, GPU engine, dedicated/shared GPU memory and Disk active time columns read "—" for every Linux process. Network now comes from the kernel's per-socket TCP byte counters (`sock_diag`, no root or tracing; UDP/QUIC traffic is not attributable this way), which also gives App history its network bytes. GPU use comes from NVML on the proprietary NVIDIA driver and from DRM `fdinfo` on amdgpu/i915/xe/nouveau and other open drivers, and the GPU page now draws per-engine graphs (3D, Video Encode/Decode, Copy) on Linux too. Disk active time is the drive's measured busy time shared out by each process' block I/O, against the bytes the drives themselves moved, so I/O of processes TaskMan cannot read is never credited to the ones it can.
- **Linux: close to tray and start minimized work on Wayland.** Closing with "close to tray" quit the program, and the autostart entry opened the window, because Wayland had no way to hide a window. The bundled winit now unmaps the window (and maps it again from the tray, or when TaskMan is launched a second time), so both behave as on Windows and X11.
- **Linux: Details shows Platform, I/O and fault columns.** Platform (32/64-bit from the executable), I/O operations, page faults and hard faults per second are filled; the Windows-only UAC virtualization column is hidden by default.
- **Linux: scrolling and dragging no longer burn a CPU core.** After asking for a redraw the event loop polled instead of waiting, and on Wayland a redraw is held until the compositor's next frame, so every scroll, scrollbar drag or animation spun one core at 100 % on both the CPU and the Vulkan renderer. The CPU renderer also did not tie its presents to the compositor's frame clock, so a drag repainted once per mouse event instead of once per display frame. Measured: continuous repainting went from 100 % to about 20 % of one core at the display rate.
- **Linux: icons for (almost) every process.** Breeze and Adwaita ship their icons, including the generic application icon, only as SVG, which TaskMan could not draw, so most rows on KDE had no icon at all or one that appeared late. SVG/SVGZ icons are now rendered.
- **Linux: process icons appear at once and stop burning CPU while they do.** The icon resolver answered every question with a live filesystem probe: against a stock Arch theme tree one icon name that no theme ships cost about 12,300 `stat`/`open` calls and 1,400 directory reads, and because a miss was never remembered the cache's 30 s retry walked the whole theme chain again for every icon-less process — which is the CPU spike that stopped only once every icon was loaded, and the reason scrolling (which brings new executables into view) kept triggering it. Directories are now read once into a time-bounded listing cache, so a warm lookup performs no filesystem work at all; the ~670 directories `hicolor` declares but does not install are pruned once per theme instead of being walked per lookup; and a miss is remembered for the same 30 s window the cache retries on, so an icon installed later still appears. On top of that the pages hand their icon keys to the cache when their row model is rebuilt instead of waiting for the first paint, so rows show their real icon on the first frame rather than filling in one at a time, the extraction now runs on three threads where the work is pure CPU (Linux decode/rasterize; Windows keeps one, its shell extraction is serialized) so a full page's images arrive together instead of one at a time, the texture cache evicts in one batch instead of re-sorting its whole map per insert, its failure retry backs off from 30 s to 30 min instead of re-extracting every icon-less row forever, and `taskman --selfcheck-icons` prints one JSON line with cold/warm per-key timings so the cost can be measured on a headless machine. With `TASKMAN_INPUT_STATS=1` the frame summary also reports the icon cache's counters.
- **Linux: full process names.** Names were cut to the kernel's 15 characters ("DiscoverNotifie", "plasma-browser-"); they are now taken from the executable or command line.
- **Linux: disks are physical drives, and no longer show bogus activity.** Each mounted partition was listed as its own disk and repeated its whole drive's counters, so one busy NVMe drive looked like several busy SSDs, and RAID members never appeared. The Performance page now lists each physical drive ("Disk 0 (nvme0n1)") with its model, type, mounted volumes and average response time; md/dm volumes count on the drives beneath them, and USB drives are labelled USB.
- **Linux: CPU base speed and virtualization.** Base speed read 0 on AMD (amd-pstate) systems; it now falls back to the firmware's CPPC nominal clock. Virtualization reads Enabled when VT-x/AMD-V is available.
- **Linux: memory page.** Swap now shows usage against its size instead of a bare "0 B", and the memory composition bar (In use / Modified / Standby / Free) is drawn on Linux, counting block-device buffers as cache.
- **Linux: Services show PID and description.** Every service had an empty PID and Description; both are filled, along with the unit's enablement state and account.
- **Linux: one Users row per login.** systemd's per-user service manager was listed as a second session of the same user with no processes, while most of the user's processes (everything started through systemd) counted towards no session. The manager session is hidden and its processes count towards the user's login. Session state and logon time were never read at all, because current `loginctl` ignores a comma-separated property list.
- **Linux: the GPU page no longer shows a permanently empty shared-memory chart** on drivers that expose no shared pool (the proprietary NVIDIA driver); amdgpu/Intel keep their GTT-based reading.
- **Linux: Settings reflects the installed service.** With the TaskMan service running, Settings still said privileged actions "may be limited"; it now names the service and its state.
 across two Processes sections.** A windowless helper of an app that used at least 1 % CPU and ran a different executable was lifted out of the app into Background processes, together with its own children but not its parent — so a `cargo.exe` started from an editor's terminal stayed in the editor's tree while its busy `rustc.exe` children appeared under Background, and rows jumped between the two sections whenever a compiler crossed the threshold. The rule predates the nested app trees and was there to keep busy tools visible; an expanded app row now shows its whole subtree with the same parent links as the Details tree, and the app row's totals already include the tool's load, so the helpers stay where they belong. Background "Name (N)" family rows still keep a busy different-image helper out of the collapsed group.

## 0.1.34 - 2026-11-26

### New

- **Linux elevated background service:** a new `taskman-core.service` systemd unit runs `taskman-service` as root and owns only a fixed allowlist of privileged work: ending a process (optionally its whole tree), suspend/resume, priority, CPU affinity and systemd service control, plus the process evidence `/proc` withholds from other users — the module map, threads, socket endpoints, per-file rows and credential/namespace/cgroup hardening — and the root sensor snapshot. The GUI stays an ordinary user process: it asks the broker only after its own token is denied, an explicit refusal is final and never becomes an elevation prompt, and an action that may already have run (a delivered action, a partially delivered tree kill) is never repeated under another token. Peers are authenticated from the kernel's own socket credentials against a root-owned manifest, and the client binary by its executable's device/inode rather than its path, because a path can be aliased by a bind mount in the caller's mount namespace. One request per connection, bounded frames, two workers and typed refusals keep the control plane finite, and a refusal is written and the request drained before the connection closes, so a client sees an answer instead of a reset.
- **Linux system tray icon:** TaskMan registers a StatusNotifierItem on the session bus (KDE, and GNOME with the AppIndicator extension) with a real menu — Open, Settings, Exit — and a left click that brings the window back, so closing to the tray and starting minimized now exist on Linux as they do on Windows. The window is hidden only once the icon is actually registered; if the panel restarts, the item re-registers and, when it cannot, a window that had been hidden is restored instead of being left unreachable. A Wayland session, where showing and hiding a window is a no-op, refuses to hide at all rather than claiming the window is in the tray.
- **NVIDIA graphics telemetry on Linux:** the GPU page reads NVML for NVIDIA adapters whose driver publishes no utilization, VRAM use or temperature in sysfs, so load, dedicated memory, die and memory temperatures, fan, power, clocks and PCIe link appear instead of unavailable. NVML is loaded dynamically, so the build needs no NVIDIA library, and the root service serves the same readings when the current user may not open the driver's device nodes. Without a usable NVML — including the static musl artifact, which has no dynamic loader — the fields stay unavailable rather than zero.
- **Full Linux sensor polling:** every hwmon chip is polled for temperature, fan, voltage, current, power, energy and humidity inputs with their labels, limits and alarm flags, alongside the thermal zones, the RAPL package and subzone energy counters (with the counter's wrapping range) and the NVML readings above. Every value is range-checked against the driver's sentinels, so a sensor with no valid sample reads unavailable instead of a plausible number, and the service answers from one cached snapshot so a GUI refresh burst cannot become a poll burst.
- **One-command Linux install and an Arch package:** `install-linux.sh` installs the GUI and the daemon into a root-owned directory, the unit, the root-owned authorization manifest (the invoking user, with an existing policy extended rather than replaced), the desktop entry, a scalable icon and AppStream metadata, then enables and starts the service; `--uninstall`, `--dev`, `--root` staging and `--post-install` scriptlet modes are supported. `packaging/arch` produces the same layout with `makepkg` by running that installer into `$pkgdir`, and its scriptlet enables the unit and refreshes the desktop and icon caches after pacman writes the files; the authorization manifest is a `backup` file, so an upgrade keeps the users it already lists. The Linux release artifact now carries the daemon, the unit, the manifest template, the desktop/icon/AppStream files and the installer, and `build.py --arch-package` builds the Arch package when `makepkg` is available.
- **Per-file read and written bytes:** the I/O page of Process Properties shows how many bytes a process read from and wrote to each open file, measured by a kernel file-I/O trace: every read and write request names its process and file object, and its completion reports the bytes it actually moved, which are summed per process and file. Files the process opened before the trace started are named from the system handle table, so a long-running process's database, log or image file gets its numbers too, and a file the process opened, read or wrote and closed again (a config reread every few seconds, an open-append-close log) stays in the table as a "(closed)" row with its bytes instead of vanishing into the total. The counts are logical requests the process completed since the trace started — cache flushes, the lazy writer and page faults are not counted twice — and a file the trace knows to be open with no transfer reads "0 B". The trace runs for the whole life of its host — the elevated service from its start, or an elevated GUI without the service from the GUI's start — instead of only while the page is open, so a process started after it has its entire life counted (the page says "since process start"), and an older one is counted from when tracing began. A process's exit ends its accounting at once, so a reused process ID never inherits another process's files. An unelevated process without the service still reports per-file accounting as unavailable, so without elevation the numbers are absent rather than zero. Linux reports per-file rows from `/proc/<pid>/fdinfo` (size, offset, access) and keeps the per-file byte counts unavailable, because that kernel publishes no per-file byte accounting; another user's process gets the same rows through the core service.
- **`--selfcheck` reports whether the per-file file trace is alive.** The headless JSON summary gained a `file_io` block for the sampled process: the accounting state (`unavailable`, `undelivered`, `undecodable` or `measured`), how long accounting has been running, and the session's loss and cap counters — events ETW reports as dropped, unnamed file creates, paths dropped at a capacity limit and file names cleared from the trace's map. A run without a GUI can now tell a trace that started and delivered nothing from one that measured nothing, instead of comparing two runs that both printed dashes. The check starts the session, creates, writes, flushes, reads back and deletes a temporary file of its own in the data directory, and then polls the sample until it stops saying `undelivered` and shows the check's own bytes, or three seconds pass — so the verdict comes from a session that had I/O to deliver and time to deliver it, not from a sample taken four milliseconds after the start. `waited_ms` states that wait (a short one cannot be read as a verdict), `decoded_bytes` reports the bytes the trace decoded for the process and is `null` rather than `0` unless the state is measured, and `own_io_error` is non-null when the check's own file I/O failed, which makes a silent trace its fault rather than the trace's. A question that could not be asked reports `null`, and an unqueried counter stays `null` rather than becoming a zero.

### Improved

- **The I/O page says which per-file byte state the table is in.** Process Properties → I/O prints one dim line above the file table for every state but "measured", and each state says something different: no trace is running (needs administrator rights or the installed service), a trace is running but has delivered no file event yet, or file events arrived but could not be decoded. Before, only the missing-trace case was explained, so a trace that started and delivered nothing looked exactly like a working session on a quiet process — a column of "—" with no explanation. Non-zero loss and cap counters (events ETW dropped, paths dropped at a capacity limit, file names cleared from the trace's map) are appended to the same line, so a lossy or capped session is visible rather than passing its numbers off as complete. The line still stays off while a row carries a count and on a platform where elevation is not the remedy, so it never contradicts a measurement already on screen. A row without a record still shows "—", and a measured zero still shows "0 B".
- **The I/O page says how much of a process's I/O went to files.** The graphs count every read and write the process makes — files, but also pipes, devices and the network — while the file table can only show files, so a service that talks to its GUI over a pipe showed busy graphs above a table of zeros. A line above the table now compares the two over exactly the traced window, e.g. "Since process start: files got 19 B of 19 B read and 19 B of 38 B written, the rest went to pipes, devices or the network". A file reached through an inherited or duplicated handle (a parent's redirected output written by its child) is now counted for every process that writes to it, and a long-running process keeps counting new files by folding its oldest closed ones into the remainder instead of stopping at its path limit. The always-on trace costs about 0.2% of one core on an idle desktop, measured in the CPU cycles of the threads involved.
- **Process lists stop rebuilding paint inputs they never read.** The Processes list laid out every visible row's name text on every repaint only to answer a tooltip, and cloned each row's tooltip string even when a higher-precedence tip won; the name is measured only under the pointer now and the tip is borrowed. The file-I/O summary moved into one predicate so a measured-zero list, an all-unknown list and a half-measured list each have a test.
- **Table guides are one crisp physical pixel from header to body.** Column dividers and the header separators now snap to a pixel centre and use a 1-physical-pixel stroke, so the guides match at any DPI or UI zoom instead of smearing across two pixels (and falling below one when zoomed out).
- **The settings INI is parsed once per start.** The usability check and the field mapping shared one read and one parse instead of reading a file of up to 4 MiB twice.
- **Linux session listings resolve `loginctl` once per page.** `show-session` reuses the already policy-checked path instead of re-canonicalizing and re-statting every ancestor for each row.
- **Typography hierarchy, variable-font optical sizing, and taskbar monitor digits.** On Windows 11, the single loaded `SegUIVar.ttf` buffer now backs `Proportional` (`wght 400, opsz 10.5`), `semibold` (`wght 600, opsz 10.5`), `display` (`wght 400, opsz 36`), and `small` (`wght 400, opsz 8`) with zero extra disk reads or byte duplication (falling back to static Semibold/Bold faces on Windows 10, Linux, and macOS). Tab titles, dialog headings, Process I/O headers and totals, shortcut keys, and the installer hero now render in real Semibold instead of no-op `.strong()`, Performance KPI readouts use the Display optical size, thread start addresses use monospace, and the taskbar monitor applies OpenType tabular figures (`tnum`) with DirectWrite `NATURAL_SYMMETRIC` rendering and un-clamped user contrast (with `Segoe UI Variable Small` added to Settings).
- **Lower font/atlas RAM and single-pass widget text layout.** `epaint` pools per-face rasterization scratch buffers (`GlyphScratch`) across glyphs; `egui-wgpu` no longer keeps a permanent duplicate CPU copy of the font atlas in `TextureSnapshots` (replaying live from `Context::fonts` on resume); `tm-ui` caches system font bytes in process memory so `fonts::reapply` never re-reads disk, skips loading unused `segoeuib.ttf` when `SegUIVar.ttf` is present, drops a replaced bundled face from the family its system replacement serves (`Ubuntu-Light` from `Proportional`, `Hack` from `Monospace`; `Ubuntu-Light` stays where `epaint` lists it as the monospace maths and symbol fallback). `TmTable`, context menus, chart hover readouts, and custom checkboxes now reuse one `Arc<Galley>` for measurement and painting, and table column dividers snap to 1 physical pixel.
- **Process lists no longer re-derive their paint inputs on every frame.** Processes and Details rebuilt model-derived values on each repaint, which on the CPU renderer is real cost per wheel step: row icons were resolved once per visible row per frame through the icon cache (which drains results, takes two or three locks and does two hash lookups, plus a second full lookup for the default icon), the selectable-row index was rebuilt over the whole model plus two more model scans, every numeric cell was re-formatted into a fresh `String`, and Details laid out every visible text cell only to answer a hover tooltip it then discarded unless the pointer was inside that cell. A per-frame icon memo now resolves each distinct image once (allocation-free on a hit; the memo is rebuilt per frame, so a cached "no icon yet" cannot outlive the frame), the selectable index is built with the row model and invalidated through `view_generation`, numeric cell text is built with the row model, and Details measures only the cell under the pointer through an unmeasured paint path with byte-identical output. `TASKMAN_INPUT_STATS=1` now also reports `model_rebuilds`, `icon_lookups`, `icon_memo_hits` and `text_measures` for the pages that report them, so the effect is measurable instead of assumed.
- **Linux icon resolution stops repeating its disk work for every process row.** Resolving an app's icon re-read and re-parsed the theme's `index.theme`, re-listed its size directories and decoded the same PNG once per executable that used it, so a startup burst of rows paid the same walk again for every Chrome/Electron helper, every launcher entry and every repeated image. A theme's layout is now parsed once, an icon name's file and its decoded pixels are remembered (small bounded caches that store successful answers only, so a failed lookup stays retryable and a miss never becomes a fabricated icon), so each distinct icon costs one lookup and one decode per run.

### Fixed

- **Taskbar stats no longer leave a strip of taskbar clickable above and below them.** The widget's box was its content plus at most 4 DIP of its own padding, capped again at the bar minus 4 DIP and centred in the bar, and its DIP height and its pixel height were rounded separately — so the bar always kept at least 2 DIP of itself on each side of the widget (2 px at 100 % scaling, 2.5 px at 125 %, 5 px at 250 %), and more whenever the content was shorter than the bar. A click within a few pixels of the bar's top or bottom edge therefore went to the taskbar instead of the widget, and a left or right bar had the same dead band along its inner edge. The box now takes its cross-axis extent from the taskbar itself in device pixels — exactly the bar's height on a top or bottom bar, exactly its width on a left or right one — and derives the DIP space it draws in from that same number, so the window, the rows and the near-transparent hit area are one box at every scaling (100–250 %, per monitor) and every bar size from a 24 px compact bar to a 75 px one. A box that does not fill the bar's cross axis is now refused by placement and hidden rather than presented short, with a rate-limited diagnostic, and a bar too short for even the smallest font scales the row rhythm into the box instead of drawing past it. Because the box is now the bar's full height, the taskbar's own drag-resize edge is covered across the widget's span on Windows 10, where the bar can be dragged taller.
- **The value band's column separators are one physical pixel wide, on a pixel centre.** Inside the numeric columns the heat band paints over the row's dividers with its own separators, and those were still a point wide and centred on the fractional boundary a sum of column widths really produces — so at any scaling other than 100 % the numeric block kept the 2 px smudge, and its seams did not line up with the guides above them in the header or beside them in the text columns.
- **A sample stamped outside a graph's window is a gap, and the readout never names one.** The graphs anchor their axis to the newest *arrival*, not to the largest timestamp, so a wall clock that jumps forward and is later corrected (NTP, a resume) leaves samples already buffered stamped past the right edge of the window. The line, the hover marker and the readout now follow one rule: the axis refuses such a sample, so it is a gap and never a point pinned onto the edge. Before, the polyline pinned it — a lone dot of stale data beside the live curve, or a stroke joining the in-window sample before it across time nobody measured — and the hover readout could still select one, reporting a seconds-old value with its age saturated to "Now" and its marker standing on the newest sample it was not describing. The readout now resolves whatever the pointer (or a pin) picked to the nearest sample the chart actually draws, and stays silent when there is none. The keyboard cursor is clamped the same way: `Home` used to walk onto the lead-in sample from before the window, whose marker was painted outside the chart entirely, leaving a value and age with no marker anywhere.
- **The Network column no longer reports a measured zero when its trace starts or restarts.** A request that had to start the broker's network session (or replace one that died) answered from a session that had delivered no event yet, and the GUI cannot tell "this session measured nothing" from "this session has delivered nothing": every process showed a measured 0 B/s and a zeroed byte total for that tick. Such an answer is now unknown, so the tick in which the session starts — including the first tick that shows the column, whose own probe is what starts it — reports the Network column as unavailable: every cell reads `—` instead of an idle machine, exactly as the disk counters already do, and the next request measures the session that is now running.
- **A long file list no longer disappears because of its own remainder.** The broker trims a per-file list to fit one response and charges the dropped rows' bytes to the "omitted" remainder, but it measured the list before those bytes were charged — so the remainder's digits could push the trimmed list back over the limit, and the request failed instead of answering, losing the whole table exactly for the process with the most open files. The trim now gives rows back until the measured length really fits, and only a snapshot that cannot fit even with no rows fails.
- **Disk counters no longer lose the window a size cap refused.** The per-process disk numbers are a drained interval: when a window exceeded the response caps the drained records were discarded, so the next window's shares were computed without that work and the columns undercounted silently once. A refused window is now held and measured together with the next one; the refused tick still reports the columns as unavailable rather than the rows that happen to fit.
- **A broker refusal is no longer answered by measuring locally.** The per-file I/O request was the one call that treated an explicit service rejection as permission to fall back to this process's own token, and that fallback now also starts the per-file trace — so an elevated GUI whose broker declined the request would host a second machine-wide session to work around the decision. The rejection is reported instead, as every other request already did.

- **The kernel's own pseudo-processes stay in the Windows-processes group.** The category rework asked the publisher test for ownership evidence on `Registry`, `Memory Compression` and `Secure System`; their identity is a bare kernel name rather than a path under `%SystemRoot%`, so the test answered "there is a file and it is not ours" and all three silently dropped out of the Windows-processes group into Background. Their ownership is reported as unknown again — which is what the name rule reads as kernel evidence — while a real image that merely borrowed one of those names is still rejected.
- **Linux keeps a machine-wide CPU reading when one core cannot be measured.** A core whose counters reset, or simply did not move inside the window, made the whole `/proc/stat` sample unusable — the Performance CPU card went blank and every process' CPU percentage with it, because that percentage is the process delta divided by the machine capacity carried by the same sample. The machine reading now stands on its own and only the per-core split is reported as unavailable. A `cpuN` line that this build cannot read completely is no longer filled in with zeroes either: a truncated line used to report a machine that spent the whole window in user time, and now leaves the window unmeasured.
- **Linux disk, connection and file-I/O probes report "unavailable" instead of a measurement nobody made.** A `/proc/diskstats` counter that would not parse became a measured `0` and then the next window's baseline, so the following tick reported the device's whole lifetime of traffic; such a row is dropped now. A device that reappeared under the same name with restarted counters (a re-created `dm-N`, a controller reset) no longer publishes one interval of measured zero activity. A process whose `/proc/<pid>/net` tables could not be opened no longer renders as "no connections", and a socket that was created but never connected (`TCP_CLOSE`, peer `0.0.0.0:0`) is listed with an empty peer instead of being dropped. Process Properties → I/O also reads a process that started inside the current sampling tick: its handle walk is bracketed by the same creation-tick check the module, thread and connection reads use, instead of failing with "process creation identity unavailable".
- **The Run dialog keeps a backslash inside double quotes.** The shell-free splitter dropped the backslash before *every* character between `"…"`, so `grep "\."` was launched as `grep .` (matching any byte), `printf "a\n"` as `printf an`, and a `sed "s/\./\//g"` expression lost both escapes. Only `$`, a backtick, `"` and `\` still escape there — the characters POSIX keeps special — and a backslash-newline is the line continuation it is meant to be; everything else is now passed through literally.
- **A failed `fc-match` is no longer memoized as Linux's text policy.** The startup text-policy query cached whatever the helper returned, including the fallback it returns when the helper could not be run at all — fontconfig installed after first use, a stalled autostart mount, or the 750 ms deadline lost to a first-run font cache build — so that fallback became the process-wide answer for good. Only a measured answer is remembered now. The query still runs once per process, so a helper that fails at startup leaves text rendering at its grayscale defaults for that run; the difference is that a failure is no longer recorded as the desktop's setting.
- **Icon names can no longer reach outside the icon theme's directories.** An `Icon=` value or `Inherits=` entry is joined onto every icon dir, and a relative `../..` component resolved outside them, so an unrelated file could be read and painted as an application's icon. A themed name (and a theme name) must now be a single path component, and a theme's declared `Directories=`/`ScaledDirectories=` entry must stay inside its own theme; the absolute-path form the spec allows is unchanged.
- **A mixed-case executable gets its themed icon back.** The basename used to fall back to the icon theme was lowercased, which is right for the case-insensitive `.desktop` index but wrong for the file name it then probed: on Linux's case-sensitive filesystems `MyApp` looked for `myapp.png` and missed the theme's `MyApp.png`. The index lookup still lowercases; the fallback keeps the case it has.
- **An empty `Icon=` no longer hides another entry's icon.** A `.desktop` file declaring `Icon=` with no value was indexed as the empty string for its own id and for every executable it names, and since the first entry inserted wins, a later (or higher-precedence) file with a real icon never applied — the row fell back to the generic glyph. Empty values are not indexed any more, matching what the direct `.desktop` path already did.
- **`Exec=env VAR=/path/bin app` names `app`, not `lib`.** The env-assignment test looked for a slash anywhere in the token, so an assignment whose *value* is a path was read as the executable and the entry claimed an icon for a binary named after the value's last component. Only the assignment's name is examined now.
- **Equal-distance icon sizes prefer the sharper asset.** A 32 px target with both `16x16` and `48x48` available picked the 16 px file and upscaled it 2×, because the last tie-break was the path; the larger pixel size now wins, so the icon is scaled down instead. Sibling `applications/` subdirectories are also scanned in name order, like the files inside one directory, instead of the last one winning.
- **Linux explains and names a USB network adapter.** A USB NIC's `device` link names its `usb_interface`, where no `product`/`manufacturer` file exists, so the description always fell through to the raw id pair (`0b95/1790/100`); the parent `usb_device` is consulted now, and only when `idVendor` proves it is one.
- **Two GPUs on different PCI domains no longer share one name.** The join key for `/proc/driver/nvidia/gpus` dropped the domain, folding `0000:03:00.0` and `0001:03:00.0` onto one key, so one card's model labelled both. Both sides of the join carry the domain, so it stays in the key.
- **macOS refuses to signal TaskMan itself or the init process.** Ending the process selected as TaskMan stopped the app from inside its own action, and `SIGTERM` to `launchd` (pid 1) takes the machine into a restart — both of which the Linux backend already refuses. Signals and priority changes now reject both targets, an unknown priority class is refused instead of being applied as nice 10, and a stale pid is reported as gone rather than as "permission denied".
- **A D3D12 window hidden for more than ten seconds comes back faster.** Past the hidden grace period the renderer releases its device, and the restore rebuilt it by enumerating every graphics adapter again — wgpu opens a D3D12 device on each one, the WARP software rasterizer and any second GPU included — which logged 240–430 ms against ~36 ms for the pipelines, all before the window could show. The release now keeps only the adapter that was already selected, so the restore re-opens that one directly; the wgpu device, descriptor heaps, allocator blocks, pipelines, textures and swapchain are still released as before. An adapter that can no longer open a device (driver update, device removed) or cannot present to the window is dropped and the full selection runs. Each re-create logs `wgpu device re-created in <n> ms from the retained adapter` (or `from the adapter enumeration`).
- **Per-file byte counts now actually arrive.** Every earlier subscription delivered nothing the decoder could use, so the page read "a trace is running but has not delivered a file event yet" forever. Measured live: the kernel's file-I/O flag and the SystemIo FILE keyword deliver only request completions, never a read or write request, and the decoder filtered on the manifest provider's event ids (12–16) where the kernel's records carry opcodes 64–68, so even arriving requests were discarded. The trace now asks for the request family and its completions on a private system logger (the global NT Kernel Logger is only a fallback, and another tool owning it is still reported by name), decodes the version-3 layouts the kernel writes, and counts a transfer when it completes: a write the file-system filter refuses on the fast path and then retries is counted once instead of twice, a read at end of file counts what it got, and paging I/O no longer doubles cached writes. An elevated end-to-end test now checks exact byte counts for a file opened before and one opened after the trace started.
- **Per-file byte counts survive two spellings of one path, 32-bit processes and long file lists.** A file opened as `C:\Windows\...` and again as `c:\windows\...` produced two trace buckets for one row and the second replaced the first, so the row under-reported while nothing else absorbed the difference; the buckets now add into the row. A 32-bit process's records were decoded four bytes narrow because the record header carries the CALLER's pointer width while the payload is written by the kernel, which left every row "—" on such a process while the session reported itself measured. A process that reused a retired pid inherited the previous process's paths. A file-I/O snapshot of very long paths overflowed the response cap and the whole list was reported as unavailable instead of being trimmed into the omitted remainder, and the trace now reports the names it had to drop, the opens it could not name and the paths it could not track so a "—" row explains itself.
- **An empty file table no longer claims the process did no file I/O.** Process Properties → I/O printed "No file activity detected for this process" whenever the file list came back empty, which on an unelevated GUI with no service is the everyday case — no per-file trace exists there, so the sentence was a definite negative about file I/O nothing had measured, and the state line above the table never ran. The empty list now names the state instead (no trace, nothing delivered yet, records that did not decode, or another application owning the NT Kernel Logger) and keeps the plain "no file activity" only where an empty list really is the answer: a handle enumeration that covered the table together with a trace that measured the process, or a platform like Linux, which has no per-file source at all and whose file list is the whole feature. "No matches for the current search" is claimed only over a list that had candidates, never over a process with no rows to filter, and a list that could not be enumerated still reads "File information unavailable" instead — the search box is never blamed for a gap the enumeration caused. A list trimmed to the 4096-row retention cap keeps that same "unavailable" note for as long as the page is open: before, the note was written into the snapshot's accounting, which the next poll replaced, so a trimmed list read as complete again one frame later.
- **Process Properties no longer reports its open-file list as unavailable for every process.** The handle enumeration cleared its completeness flag for any handle Windows would not let us duplicate or describe — and every real process holds protected objects — while the I/O page renders its list only when that flag is set, so the page showed "File information unavailable" everywhere. The flag now describes the enumeration itself (the whole handle table was covered); handles that could not be inspected are counted and logged instead, and files that were found keep their names, sizes, handles and access.
- **Unreported memory no longer renders as a measured zero.** The Processes and Users memory column, the Performance memory card and page header, the memory sparkline, the Committed chart and the Committed/Commit-limit/Cached/Pool rows printed a measured-looking zero when the platform reported no total; they now render `—`, a graph gap or a dropped row, a measured zero still shows, and `--selfcheck` reports `null` for an unmeasured memory figure. In that same state the memory page's In-use chart, its In-use readout and its Total/Available rows follow the header instead of stating `0 B` under a `—` title. The same rule now governs the Process Properties file totals, which printed a partial sum as the process's traffic.
- **A thread whose start time the platform cannot report keeps its measured CPU.** The Threads tracker dropped its history when either side's creation time was unknown, so on a host that never reports one (a protected Windows thread, a Linux kernel without `btime`) every tick started over: per-thread CPU read "—" forever and every row re-entered "observing" instead of settling. Two unknown start times are now treated as an unchanged identity, while a start time that appears or is lost still starts a fresh history.
- **The Services, Startup and Users pages state why they are unavailable.** Their "unavailable" banners ended in a bare colon because the message had no placeholder for the platform's error, so the reason the page had just formatted was thrown away; the reason is now part of the line, and the Services page reads a plain "Unavailable" where the platform has no service concept at all instead of claiming a failure. The German "status of elevated privileges unknown" string was also doubly encoded and read "erhÃ¶hten" instead of "erhöhten".
- **App history and Startup impact no longer let a contentless store replace the recovery copy.** A whitespace-only or NUL-filled `app-history.json` or `startup-impact.json` was renamed over the `.bad` copy an earlier failure had kept; both loaders now remove such a file where it lies and leave that copy byte-identical.
- **Win32 presentation-gate logs report the cloak DWM actually holds.** The install and hold diagnostics logged "holding the window cloaked" or "cloak refused" from a boolean that folds every cloaking party together, so a shell cloak next to a refused `DwmSetWindowAttribute` produced two contradicting lines, and an unreadable state was reported as refused. The gate logs the raw `DWMWA_CLOAKED` mask instead (`None` unreadable, `0` nothing cloaks the window), and it still fails closed.
- **Process Properties no longer hides another process's open files.** The pre-copy `GetFileType` probe asked the *calling* process's handle table about the *target's* handle value, so a foreign process's files were skipped unless a local handle at the same value happened to be a disk file — and the walk still reported itself complete, which zeroed the rows. The probe now runs only when the enumerated process is this one, the snapshot records are read unaligned out of their byte buffer, and completeness stays honest for foreign processes.
- **Taskbar monitor teardown and bitmap failures.** A refused `CreateDIBSection` leaked its device context every tick, a refused `SelectObject` was reported as success and would have composited nothing, and the window's user-data pointer was cleared only after `DestroyWindow` — and, on one path, into a dead handle — while the state was already freed. The clear now happens while the window is alive, the free only after the destroy, and a refused destroy is logged.
- **Always-on-top no longer loses one of its two hooks.** `install_strict_topmost_hooks` returned as soon as either WinEvent hook was stored, so a first success followed by a failure dropped `EVENT_OBJECT_SHOW` or `EVENT_SYSTEM_FOREGROUND` for the rest of the session; each event is installed and retried on its own.
- **A responsive app is no longer labelled "Not responding" when the hung-window scan runs out of probe budget.** A candidate the 600 ms corroboration never reached was inserted into the non-responding set anyway, so four hung-window candidates were enough to hold a responsive app on "Not responding" through the 10 s debounce; an unprobed candidate now stays unresolved, and each scan starts at a different candidate so a genuinely hung app sitting behind others is still confirmed.
- **The presentation gate reports a cloak DWM refused instead of assuming it.** A refused `DwmSetWindowAttribute` was logged as a window held cloaked; the gate now reads `DWMWA_CLOAKED` back and says which of the two happened, and still fails closed.
- **The restored-window startup milestone is no longer dated a frame early.** `restored_window_painted` was logged at the end of the first `ui` pass, which counts a frame before eframe tessellates and presents it; the mark now happens on the pass that begins after the frame was handed to the backend.
- **A corrupt, empty or setting-less `config.ini` no longer discards settings from older builds.** An unusable INI was set aside and defaults returned, skipping the legacy `settings.json` migration, and the next autosave then wrote those defaults over the user's real settings; the loader falls through to the migration whenever the INI holds no settings at all (unreadable, zero-length, zero-filled, BOM-only, or no pair under a section), and a file that says nothing never replaces the last recoverable `.bad` copy. A settings writer thread that fails to start is logged instead of silently dropping every later save.
- **WGPU starts on GL drivers with a small maximum texture size.** `egui-wgpu` required a flat 8192 px texture dimension (2D, then 1D) over wgpu's GL-safe 2048 px base, and wgpu validates required limits against the adapter, so a driver below that failed `request_device` and the app started with no window; both dimensions now request `min(8192, adapter)`.
- **Renderer timing lines describe the compile they measure.** A launch that compiled the pipelines before the first presented frame left that measurement unconsumed, so a compile after a suspend/restore was logged with the first device's cost against the restore's elapsed time; start-up and restore compiles are separate measurements now, and `Painter::suspend` clears the sub-pixel capability together with the device it describes.
- **The software renderer no longer drops a shape whose rect exceeds the 32-bit pixel range.** Floor-and-cast saturated and the 2 px margin then overflowed: a debug build panicked and a release build inverted the rect, so the shape was skipped until a forced full repaint. The scaled coordinate is clamped before the cast, and all three `debug_*` tessellation toggles are part of the damage frame key so a runtime toggle invalidates the frame instead of painting outside the tiles that claim to be current.
- **A shared icon name resolves to the same file everywhere.** An icon present in several contexts of one theme size was decided by raw `read_dir` order; the walk is sorted now, pinned by a test that drives the scan from a fixed order instead of relying on a host that happens to collate names.
- **The installer no longer sizes its inflate reservation from the manifest.** A few hundred hostile payload bytes could reserve 64 MiB before a byte was read; the reservation is capped at 64 KiB and grows geometrically under the same bound, and a missing install-directory parent returns the staging error instead of panicking inside the elevated setup.
- **`loginctl` is resolved under the same trusted-path policy as `systemctl`.** It was accepted as a root-owned file with any execute bit and no ancestor check, so a writable or redirected tool could be executed; it now needs a canonical absolute path whose every ancestor is root-owned and not group/other writable.
- **Linux startup sampling stops re-reading hardware that cannot change.** Each tick re-read `/proc/meminfo` four times, read every process's `/proc/<pid>/stat` a third time only for its priority, walked `/sys` for CPU topology, base frequency and DRM adapter identity (including the NVIDIA model probe), and rebuilt every network adapter's description from sysfs — work that was identical between ticks and ran even for a tray window. Those facts are now derived once (per process for the driver/hardware identity, re-derived when a card or interface is replaced or a DRM card number stops naming the same device), process command lines are remembered per process generation, and the collector honours the UI's telemetry demand, so GPU measurement, disk-rate counters and the per-CPU frequency probe stay off while no page shows them — on a host without `cpufreq` (a VM or a container) the old frequency refresh parsed the whole `/proc/cpuinfo` once per CPU, on every tick, the largest of these costs.
- **No white flash when the window appears — cold start, tray, or taskbar.** The window is now created hidden and cloaked (`DWMWA_CLOAK`) before the desktop can compose it, and a window-procedure hook re-cloaks it on every message that makes it visible — `WM_SHOWWINDOW`, a `SWP_SHOWWINDOW` position change, `SC_RESTORE`/`SC_MAXIMIZE` on a minimized window, and the `WM_SIZE` restore that follows. That hook is the part the app could not do itself: a taskbar restore is performed by Windows before the app's next frame, so a cloak set from `update()` was always one frame too late. `present_gate_release` is the only uncloak in the toolkit and is called only once a frame has really painted into the window, bounded by 8 frames / 2 s so the window can never stay invisible. Three smaller leaks closed with it: a renderer attempt that failed after creating its window could show it unpainted, `ensure_presentable`/`set_window_cloaked` used to uncloak a window a restore was still holding, and the client area had no defined background at all (winit registers its class with `hbrBackground: 0`), so any erase now fills with the app's own background colour instead of the system's white. The native caption and border are applied at creation rather than in the first frame, so a dark-theme app no longer shows a light-mode caption while the window appears.
- **Performance graphs no longer draw strokes through regions they have no data for.** A tick the sampler could not measure — a CPU window that is not ready yet, a mount or adapter that is not in that tick's snapshot, a sequence resumed after a suspend — used to reach the graph as a value: a sentinel that epaint turned into a straight stroke across the plot, typically a diagonal from about mid-tile down to the baseline with no area fill anywhere under it. Non-finite and absent samples are now a gap in the data and break the line and its fill; a lead-in more than one window older than the axis opening (a resume after a suspend) is dropped rather than pinned to the plot edge; a stamp older than its predecessor (a wall-clock step back, a duplicate) starts a new run instead of being bridged; and the per-core kernel band is only drawn where the value it belongs to exists and both vectors describe the same core set. On the CPU page the logical-processor grid also keeps its tiles while a tick has no per-core data instead of flipping to the aggregate chart for one frame, and an unavailable sample reads as "—" instead of a fabricated `0,0 %`. The disk transfer chart and the disk page's Read/Write readouts follow the same rule, and they carry their own measured flag rather than borrowing the activity percentage's: macOS reports `IOBlockStorageDriver` bytes with no per-disk busy-time source at all, so inferring "not measured" from the missing percentage would hide real throughput, while a first sample or a sleeping Windows `PhysicalDisk` counter still reads as a gap and a dash instead of a measured-looking `0 B/s`.
- **WGPU LCD text matches CPU weight, conserves energy on sRGB targets, and stops dithering transparent padding.** The WGPU dual-source shader previously bypassed `text_gamma` calibration on non-sRGB `Unorm` surfaces, double-premultiplied coverage in sRGB space before converting to linear on sRGB targets, and injected interleaved-gradient noise into zero-coverage glyph padding pixels. `egui-wgpu` now forwards `TextOptions::text_gamma` in the 32-byte `Locals` uniform for foreground-luminance gamma compensation on gamma framebuffers, converts straight foreground RGB to linear before premultiplying by channel coverage on sRGB targets, and applies dithering only to covered pixels.
- **Light-mode text weight, default contrast, `TextStyle::Monospace`, and startup smoothing order.** `TextSmoothing::Sharp` previously lowered gamma in both dark and light themes — thinning white-on-dark stems but thickening dark-on-light stems — while `light_visuals()` omitted `override_text_color` (`#1a1a1a`) and fell back to egui's `#505050` gray. `TextStyle::Monospace` was mapped to `FontId::proportional(12.0)` instead of `FontId::monospace(12.0)`, `main.rs` applied startup visuals and started async font loading before publishing saved `text_smoothing`, and the installer rebuilt the atlas on frame 2 by not querying its creation `HWND`. Light-mode `Sharp` now uses polarity-correct gamma and contrast tuning (pinned by a light-mode `text_compare` test), `light_visuals()` sets `LIGHT.text`, `TextStyle::Monospace` uses the monospace family, startup publishes `text_smoothing` before `apply_startup`/`install_async`, and both `tm-app` and `tm-installer` query ClearType at creation time and refresh on focus return (`WindowFocused(true)`).
- **Sub-pixel text fails closed over RDP and on 90°/270° rotated panels, and Linux no longer inherits Windows contrast.** Windows text-rendering queries now disable horizontal LCD over Remote Desktop (`SM_REMOTESESSION`) and on portrait-rotated monitors (`DMDO_90`/`DMDO_270`) even when a driver omits `DWRITE_PIXEL_GEOMETRY_FLAT`. Linux Fontconfig queries now default `contrast` to `0.0` (instead of Windows' `0.5`), parse `hinting` and `hintstyle` for `TextSmoothing::Standard`, and resolve `.ttc` collection face indices (`%{index}`).
- **Linux CPU rendering no longer leaves black boxes, stale glyphs or left-over regions.** The non-Windows softbuffer surface faked `FrameBuffer::age()` from a "presented once" bool, but softbuffer's Wayland backend keeps two `wl_shm` buffers and swaps them per present, so a leased buffer is always 0 or 2 frames old and never 1 — and the 64×64 tile tracker therefore repainted only the changed tiles into zeroed or two-frames-old memory. The surface now reports softbuffer's own age: Wayland full-paints every frame (a ring can never hand out a one-frame-old buffer), X11 keeps the incremental path, and a resize no longer paints incrementally into a reallocated segment. A font-atlas patch that overwrites a LIVE glyph slot (epaint rewrites live cells when the atlas overflows) now bumps the texture generation, so text tiles sampling an evicted glyph are repainted instead of keeping stale pixels, while filling empty atlas space still invalidates nothing. The damage frame key now also covers `bezier_tolerance`, `epsilon` and the remaining pixel-affecting tessellation options (`debug_*` keyed as well, because they are applied after the shape hash and would otherwise repaint nothing), `Shape::Circle` hashes the font atlas because a small filled circle can be tessellated through a prerasterized disc, and `tools/check-fork.ps1` now lints and unit-tests eframe with the shipped software feature set (`accesskit,default_fonts,software,wayland,x11`), which nothing did before.
- **Linux per-process CPU load is measured instead of fabricated, and an unmeasurable share reads "—".** Rows were wrong and partly invented: sysinfo re-baselines `/proc/stat` only every 200 ms while process times are re-read on every tick, so numerator and denominator came from different windows (fast ticks understated every row and could not agree with the header); its per-process delta was discarded while `old_utime == old_stime == 0`, printing a fabricated 0 % for the first window of every process; and there was no availability channel at all, so rows printed 0 % on the very tick the header rendered a dash, while an unreadable `/proc/stat` made every process read 100 %. A new std-only accountant (`tm-platform/src/linux/process_cpu.rs`) measures each process against the SAME `/proc/stat` window the header uses, as a share of total machine capacity in [0, 100] — the Windows contract — with a pid-reuse guard, skipped-reading detection, regressing-counter rejection and a never-exceeds-machine clamp, and the post-refresh `/proc/<pid>/stat` read now feeds both the identity check and the accountant, so the accountant no longer opens the file a second time. `ProcessEntry::cpu_pct` is `Option<f32>`: an unmeasurable share renders as a dash in Processes, Details and the Properties dialog, the Users rollup folds only measured rows, an unmeasured cell ranks below a measured zero in both sort directions (CPU, Network, Disk active time and GPU), the CPU dash gained a localized hover explanation in both places, and `--selfcheck` reports `process_cpu_readings`/`process_cpu_unknown` so a regression that blanks every CPU cell stays visible headlessly. Documented limits: Linux rows can sum slightly below the header (a process that exits inside a window takes its last ticks with it, and irq/softirq time is not attributable — measured ~0.6 % of capacity), and a process seen for the first time reports a dash for its first window. Windows keeps its own accountant but now also renders a dash instead of 0 % on the first tick or a sub-120 ms window, where `cpu_load` deliberately returns `None`.
- **Linux process icons resolve through the whole desktop theme chain.** Theme discovery fell back to `hicolor` alone and never consulted the spec `default` theme, so on a stock Arch box — where `hicolor` holds no PNGs — nearly every lookup missed, for application icons and for the default glyph alike; `Inherits` was followed exactly one level, `Scale=` was ignored when ranking size directories, an `Icon=` that already named a file produced `foo.png.png`, `.desktop` matching missed `TryExec`, versioned interpreters (`python3.12`) and Flatpak exported entries, the desktop index walked `read_dir` order so a shared executable name resolved to whichever file the filesystem listed first, and the GUI icon cache left a key in its `queued` dedup set when no worker channel existed, so that row was never requested again. The resolver now builds the full transitive chain (the configured theme or the spec default, `Inherits` depth-first with a cycle guard, `hicolor` last, Plasma `kdeglobals` as a theme source), parses `ScaledDirectories=` and derives a missing `Scale=` from an `@N` directory suffix, ranks size directories by `Size × Scale` and prefers the native-size directory over an upscaled one on a pixel tie, searches `$HOME/.icons` and every data dir's `pixmaps`, stops doubling `.png` on `Icon=` file names, matches `Exec`/`TryExec`/wrapper/Flatpak `--command=` correctly (`TryExec` only where `Exec` names a launcher or is unusable; `env`/`sudo`/`setsid`/`nohup`/`nice` skip a prefix and its flags; `python3 app.py` never adopts `app.py`), walks `applications/` in sorted order, and the cache releases a stranded key, backs off a failed worker spawn on a monotonic 30 s deadline (one warning per window instead of one per row per frame), releases the whole outstanding set when the worker is gone, and checks one atomic before draining results. Still unsupported, unchanged and documented: SVG-only icons fall back to the generic glyph, and `StartupWMClass` stays unused because it names a window class, not an executable.

- **The Linux release tarball is executable as shipped.** `build.py` wrote the Linux artifact with `tarfile.add()`, and Windows has no POSIX mode bits, so the extracted `taskman` came out `-rw-r--r--` and every user had to `chmod +x` before the first launch. The archive now stamps `0755` on the program and `0644` on the desktop entry, with root ownership instead of the build host's identity.
- **The trusted `fc-match` wait no longer spins, and its executable is resolved once found.** Waiting for the helper now parks the thread — a `pidfd` plus `poll` where the kernel allows it, a capped 100 µs→5 ms backoff elsewhere — under the caller's single 750 ms deadline, replacing a `yield_now` loop that could pin a core between the child's stdout EOF and its reaping; a `poll` the kernel or a seccomp profile refuses, or one that reports an unusable descriptor, falls back to the sleep instead of re-polling a failing call at full speed. `resolve_fc_match` also runs its canonicalize-and-stat chain once per process instead of on every spawn (a hit is remembered; a miss is retried, because a miss can be a transient filesystem or descriptor problem rather than a statement about trust).
- **A broker trace whose idle watchdog could not be started is no longer kept.** The per-process network and disk traces are stopped by nothing but the thread that idles them out, so when that thread could not be spawned the session stayed for the life of the LocalSystem service and kept tracing every process's network and disk traffic machine-wide, with the counters reading as live. Both now drop the session and report the columns as unavailable — the answer a missing source has to give — exactly as the per-file trace already did.
- **Scrolling and hover repaints keep the display cadence on a virtual display.** A monitor that reports Win32's `dmDisplayFrequency == 1` ("use the hardware default" — RDP sessions, virtual displays, some projectors) reached the input throttle as a one-second frame period. The wheel and pointer repaints are paced with that interval, so a scroll advanced one notch per second, and because the same value becomes egui's predicted frame time, every `request_repaint_after` delay was reduced to zero and the UI repainted back-to-back. A monitor rate below 20 Hz is no longer taken as a frame cadence; the throttle falls back to 60 Hz there.
- **Text stays readable when the GPU refuses the dual-source pipeline.** Whether `egui-wgpu` can blend sub-pixel coverage per channel is decided before the pipeline exists (the device feature plus a CPU validation of the shader), and a driver can still reject it. The rasterized atlas was uploaded unchanged in that case, so its zero alpha made glyphs invisible on a dark background and additive on a light one — the app reported sub-pixel text as available and then drew none of it. With no LCD pipeline the atlas is now uploaded as mean-coverage premultiplied gray, so the text is degraded antialiased instead of corrupt, and the refusal is logged once.
- **Linux window ownership is no longer derived from a stale X11 query.** The observation's timestamp now marks when the query started instead of when its reply arrived, so a client list older than the three-second observation window reads as unavailable rather than landing as fresh complete coverage; a window opened during a stalled or overloaded X server can therefore no longer make the process that owns it look windowless.
- **One failed `loginctl` run no longer blanks the Linux Session column.** The 30 s retry cooldown was part of the platform's "user sessions are supported" answer, so a single failed listing reported the whole surface as unsupported and withheld every process's session id for half a minute; the cooldown now only defers the next `loginctl` spawn, and the capability stays true. A request inside the retry window also repeats the last real diagnostic instead of only saying that a failure is cooling down.
- **Linux `systemctl` can no longer occupy an action lane indefinitely.** `systemctl` ran without a deadline while the daemon or its D-Bus socket could be wedged, so one Services refresh or one service start/stop could hold one of the two action lanes forever. Both calls now share the pidfd/`poll` deadline `loginctl` already used — 5 s for the read-only unit list, 30 s for control — and a child that outlives it is killed, reaped, and reported as a failure that names the program.
- **Toggling a Linux startup item no longer rewrites the user's desktop file in place.** The `Hidden` change was a truncating write, so a full disk or a killed process left a truncated entry, and rebuilding the text from `lines()` turned a CRLF entry into LF. The rewrite now keeps every untouched line's own ending, does nothing when the value is already right, and publishes a sibling temporary file with the entry's exact permission bits; an entry that is a symlink is still written through the link rather than replaced.
- **A Linux process whose environment cannot be read is no longer called a background task.** An empty `/proc/<pid>/environ` — a zombie whose memory is already gone, or a process the kernel will not expose — was read as a complete, headless environment, which turned a launcher-managed `app.slice` unit into an authoritative Background role; it is unusable evidence now, and the role stays unknown.
- **TaskMan's Linux autostart entry no longer tightens a foreign file's permissions.** Enabling start-with-system opened the entry at TaskMan's own name and set it to `0600` before checking the ownership marker, so a user-owned file that was then refused ("not owned by TaskMan") had its permission bits changed anyway; the mode is only tightened after the marker proves ownership.
- **`--selfcheck` no longer prints `"ok": true` for a run it fails.** The plausibility gate (fewer than five processes, no logical processors, no measured CPU share) ran *after* the JSON line was printed and the process then exited 1, so a consumer that reads the line — which is what the summary is for — saw a success claim from a failed probe. The gate is now evaluated before the summary is built, `ok` and the exit code come from that one decision, and the reason is published in `error`.
- **`--selfcheck` no longer bills the per-file probe to `startup_to_snapshot_ms`.** The figure was timed at the print, i.e. after the per-file I/O probe added in this release has waited up to three seconds for a trace delivery, so trace latency landed in the number. It is now taken when the usable snapshot arrives. It still covers the deliberate four-tick wait the two-sample GPU, disk-rate and CPU-speed groups need, so read it as time to a usable sample, not as process startup cost.
- **A huge file in a user-writable config directory can no longer stall the app.** The startup-impact store, the saved window placement and the Linux `index.theme` / `.desktop` / `kdeglobals` reads had no size bound, unlike the settings, app-history and icon-image readers, so a multi-gigabyte or hostile file was read whole — in-process on the GUI's deferred startup path for the store, on the window-creation path for the placement, on the icon worker for the theme files. The store now caps at 16 MiB and treats an oversized file like a structurally corrupt one (kept aside for recovery, never loaded), the placement file caps at 64 KiB and falls back to the OS-chosen position, and the theme and desktop-entry parsers cap at 1 MiB and treat an oversized file as no icon at all.
- **`--size=` can no longer create an infinite window.** The argument reached the window rect with only a lower bound checked, so `--size=infxinf` asked the window manager for an infinite rectangle and a finite-but-absurd size was passed through as-is. A non-finite size is rejected (the saved size stands) and a finite one is clamped to the same 16384 px maximum the settings file stores.
- **A Linux helper wait only reports an exit the kernel actually signalled.** The bounded `pidfd` wait behind `systemctl`, `loginctl` and `fc-match` treated *any* readiness of the descriptor as "the child exited", and `poll` reports an unusable descriptor as a positive readiness rather than a failure. A wrong "exited" then skipped the deadline and let the caller collect the child's output with a blocking wait, so a wedged helper could still hold an action lane indefinitely. Only a readable `pidfd` proves the exit now; anything else falls back to the bounded status poll, and the fontconfig runner's park treats a descriptor that cannot be read the same way instead of re-polling it at full speed.
- **A failed `/sys/class/net` read no longer erases the Linux adapter descriptions.** The per-tick interface listing threw the cached adapter identities away and published every adapter as undescribed whenever that directory could not be read, so one transient failure blanked the Network page's model names and made the next tick re-read them from scratch. The identities already measured are kept and published instead — the live link and address fields stay unclaimed, since they cannot be read without the interface directory — which is what the DRM card scan already does for a card list it cannot re-read.
- **A Linux helper's output is read under the same deadline as its exit.** The bounded wait proved that `loginctl`/`systemctl` had exited, but the caller then collected the helper's pipes with a plain read-to-EOF, which has no deadline of its own: a process that inherited those descriptors keeps them open after the helper is gone, so one Services or Users refresh could still block an action lane indefinitely. Both streams are now drained with `poll` plus `read` under the same absolute deadline (1 MiB per stream) before the exit is collected, and a helper that outlives it is killed and reaped as before.
- **A Linux thread row is no longer dropped when only its generation cannot be re-proved.** The walk re-read each `/proc/<pid>/task/<tid>/stat` to prove that the `/status` record belonged to the same generation and discarded the whole row when it could not, so a TID recycled mid-walk — or a thread that exited during it — left the Threads page one row short with nothing saying the list was incomplete, and disagreeing with the Details thread count. Only the status-derived context-switch count is given up now: the stat is one self-consistent record, so its state, priority and CPU times are still reported, while positive evidence that the TID belongs to another task group still omits the row.
- **A Linux GPU location is no longer derived from a non-zero PCI domain.** The slot parser dropped the domain, so on a multi-segment host `0001:03:00.0` and `0000:03:00.0` reported the same bus/device/function and the Performance page could name one slot twice — while the adapter-name join deliberately keeps the domain for exactly that reason. A slot whose domain is not zero now publishes no location rather than an ambiguous one.
- **`/proc/stat` treats a field it cannot read like a field the line does not have.** A `cpuN` line whose counter failed to parse rejected the whole file, while a line with too few fields only dropped that core. Both leave the window unmeasurable, but the first also kept the older baseline, so the following window silently measured a longer interval against a capacity from the same two reads. Both cases now drop the core and let the existing CPU-set check report the window, which is the rule the per-core split already described.
- **The first painted frame really reaches the screen before the window is revealed.** The D3D12/Vulkan renderer reveals the window through the cleared primer frame, but that call reported success whenever it returned, and three of its paths present nothing: a zero-sized surface, a surface the compositor reports as occluded, and one that is outdated or lost at that moment. The window was then revealed over an uninitialised backbuffer — the white flash the primer exists to remove — and because the reveal is a one-time decision, the cleared frame was never retried. The painter now reports whether a frame was actually presented, and a reveal only happens on that `true`; otherwise the fully painted frame later in the same pass reveals with content.
- **Linux's hidden root helper refuses to re-run TaskMan as root.** The `--linux-privileged-*` entry point that Polkit can start as root still implemented the `relaunch` action and executed the portable, user-writable TaskMan image again — the one thing this backend documents as never happening, and what elevated Run, elevated relaunch and start-elevated already report as unsupported. A privileged helper now fails closed on that action with the same typed error the request side uses, so approving a Polkit prompt cannot make this image root's TaskMan. Request-side and root-side refusals share one pair of error constructors, so the two halves cannot drift apart again.
- **Linux service and session probes find the systemd tools on NixOS.** `systemctl` and `loginctl` were resolved only from `/usr/bin` and `/bin`, so a host that keeps them in the NixOS system profile reported the whole Services surface as unavailable and withheld every process's Session id — while `pkexec` and `fc-match` resolution in the same backend already carried those profile paths. Both tools now resolve from one directory list, which the Polkit control path shares instead of keeping its own copy, so the probes cannot disagree about what the host has. Every candidate still has to be an executable regular file whose ancestors are root-owned and not group/other writable.
- **A Polkit prompt can no longer park a Linux action lane forever.** The elevated retry ran `pkexec` with `Command::output()`, which waits without any bound, so an unanswered authorization dialog or a wedged authentication agent held one of the two action lanes — and then its queue — for the rest of the session, defeating the bound the direct `systemctl` path had just gained. The wait now uses the same pidfd/`poll` deadline with a generous 120-second limit for a human prompt, its pipes are drained with `poll` plus `read` under that same absolute deadline — so a chatty `pkexec` cannot block itself and a process that inherited the descriptors cannot outlive the deadline — and a request that outlives it is killed, reaped and reported as cancelled with the deadline named.
- **A transient `accept` failure no longer disables Linux single-instance activation for the rest of the session.** When `accept` failed with anything but `WouldBlock`/`Interrupted` — a temporary descriptor or memory shortage (`EMFILE`, `ENFILE`, `ENOMEM`, `ENOBUFS`), or a pending connection the peer aborted — the listener unlinked its socket and left the loop while the advisory lock stayed held, so every later launch found no endpoint, retried for the whole five-second handoff deadline and opened a duplicate window that the first instance never heard about. Such a failure is now logged once per episode and retried after a short backoff that the shutdown wake-up cuts short, because the lock and the endpoint stay owned by the process for its whole lifetime. The same holds for a failed `poll` of the listener: only an error that names an unusable descriptor (`EBADF`/`EINVAL`/`EFAULT`) still unlinks the socket and ends the loop — `EINTR` retries at once, and `ENOMEM`, `EAGAIN` or an errno this build does not know back off and retry. Shutdown no longer depends on a poll succeeding either: the listener re-checks its stop flag between attempts, so a persistently failing poll cannot keep the window and its listener thread alive after the user closes TaskMan.
- **A just-started Linux process keeps its name-based category for its first tick.** A process that first appeared in the current refresh, or whose `/proc/<pid>/stat` read failed, has no identity bracket, so the collector had no desktop evidence for it — yet it still classified the row with an authoritative `Unknown`. The category itself was computed from evidence nobody observed, and the Processes page treats that marker as a fact and refuses its userspace-name fallback, so `systemd` or `dbus-daemon` rendered as Unknown for one tick and then flipped to System. Both the published role and the role handed to the classifier are now absent when the tick did not bracket the process's generation, which is what re-enables the userspace-name fallback in `classify.rs` for exactly that tick; a bracketed process still carries its authoritative role, `Unknown` included.
- **Disabling TaskMan on the Startup page no longer blocks "start with system".** Turning off a system-wide `io.github.aufkrawall.Taskman.desktop` entry wrote a minimal `Hidden=true` file at the user's own entry name carrying TaskMan's override marker but not the autostart marker, and enabling start-with-system then refused to replace that file as "not owned by TaskMan" — the Settings toggle stayed dead until the file was deleted by hand. A file at TaskMan's own name with either marker is replaceable now; one with neither is still left untouched, permission bits included.

## 0.1.33 - 2026-11-24

### New

- **Linux native-Wayland process roles:** KDE Plasma, GNOME and other systemd desktop sessions can now classify launcher-managed native-Wayland applications without an X11 window. A recognized user `app-*` or Snap-launcher `app.slice` unit becomes an App only when a complete, capped process environment has a non-whitespace `WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `DISPLAY`; arbitrary `app.slice` units need an independently validated Flatpak/Snap/GIO/BAMF identity, so inherited display variables alone cannot turn `sleep.service` or `run-u42.scope` into Apps. A complete headless recognized unit is Background, while unavailable, partial or oversized environment evidence stays Unknown. Explicit `background.slice`, `session.slice`, direct session-scope and user-manager `init.scope` roles are Background, generic `user@UID.service` membership is not guessed, and `system.slice`/`init.scope` are System only when systemd is the real PID 1 (synthetic WSL cgroups stay Unknown). Flatpak, Snap, GIO and BAMF hints may identify an App only on weak user-session evidence and cannot override authoritative Background evidence; app-family keys, hybrid cgroup root fallback, cache-hit generation checks, staggered expiration and windowed app-root preference are handled without merging unrelated scopes. The collector passes its authoritative Unknown marker through classification, and the Processes display never re-promotes a legacy Background process from a cross-platform core-looking name. This identifies launcher evidence, not compositor focus or an exact window inventory, and X11 ownership remains independent evidence.
- **GPU sub-pixel text on D3D12 and Vulkan:** the WGPU renderer now uses a dedicated dual-source font-atlas pipeline when the selected device supports it, blending each RGB channel with its own LCD coverage. Horizontal RGB/BGR order, LCD filtering and the selected text-weight contrast are honored; unsupported adapters, Glow, validation failures and transformed or fractionally aligned text fail closed to grayscale. Non-font textures, all-white untextured geometry and grayscale/color glyphs keep the ordinary blend path, and the managed atlas is switched back to grayscale immediately after a full reset. Shader and pipeline validation are bounded so a broken LCD path cannot block or break normal WGPU startup.
- **Linux desktop LCD text:** Linux now reads the user's effective fontconfig `antialias`, horizontal `rgba` order and `lcdfilter` through a cached, trusted absolute `fc-match` path. RGB/BGR desktops get ClearType-like per-channel text on the CPU renderer and on supported Vulkan devices, with default/light/legacy/no-filter presets mapped to the fork's LCD filters; disabled antialiasing, vertical/unknown/none orders and malformed settings stay grayscale. The same absolute executable, parent-directory policy, 750 ms deadline and 64 KiB output cap are used for system-font selection, and a timed-out helper is killed and reaped.
- **Linux Run without a shell:** Run now tokenizes quoted arguments and starts the selected program directly instead of passing user text to `sh -c`. Shell operators remain literal arguments, elevated Run is hidden and rejected on Linux, and macOS no longer exposes or executes the sudo-shell path.
- **Linux process inspection:** Process Properties now lists loaded file mappings, per-thread scheduler and CPU evidence, and TCP/UDP endpoints from procfs on Linux. PID/TID generation and network-namespace changes are rejected rather than mixing snapshots; socket reuse is filtered across before/after handle walks; malformed rows are omitted instead of rendered as listeners; Linux's `R` state is shown as runnable rather than proven execution; and missing switch/cycle evidence remains unknown rather than becoming "Idle". The module inventory is read-only, and data that procfs cannot provide remains unavailable rather than being fabricated.
- **Linux telemetry and session parity:** Linux now computes aggregate and per-core CPU utilization plus kernel time from `/proc/stat` deltas (the counter-only first sample stays unavailable), reports adapter IPv4/IPv6 and sysfs driver/hardware descriptions, enriches every DRM adapter with available PCI identity, driver, VRAM/GTT and temperature data, and lists systemd login sessions through a validated `loginctl` path when available. Missing CPU/GPU utilization and GPU memory sources remain unavailable rather than idle or zero, history gaps stay aligned with their timestamps, no engine data is invented, and Wi-Fi SSID/signal plus session logoff/disconnect remain unavailable rather than guessed.
- **Linux privileged service actions without an installer or daemon:** systemd start/stop/restart first use direct authorization and otherwise request a one-time Polkit prompt for a trusted `systemctl` path. Portable user-writable TaskMan binaries are never executed as root; privileged per-process actions and elevated GUI relaunch fail closed until an installed protected helper exists. Open file location, Properties, and HTTP(S) links use `xdg-open` without interpolation, and elevated arbitrary Run remains unavailable.
- **Linux single-instance activation and start with the system:** a second Linux launch asks the running process to show, restore and focus its window, then exits. Coordination uses an owner-only runtime lock and Unix socket with same-user peer checks, strict non-executable messages, stale-socket cleanup, and bounded timeouts. Settings offers a capability-driven **Start with the system** option backed by a self-owned, safely quoted XDG autostart entry that can be disabled idempotently without touching unrelated desktop files. The entry starts visibly because this Linux build has no tray/hidden-start support.

### Improved

- **D3D12/Vulkan resident memory:** WGPU now uses memory-oriented allocator blocks instead of performance-sized defaults, bounds the D3D12 descriptor heap, and reclaims oversized vertex/index buffers after sustained low use. The normal 2D UI keeps the ordinary and LCD text pipelines while lowering its resident GPU allocation floor.
- **LCD atlas detection and startup:** full font-atlas replacements determine LCD state from their contents, while partial additions short-circuit once LCD is already known present. The WGPU `Locals` uniform is explicitly padded to 32 bytes, and both LCD fragment paths are parsed and validated with Naga in the normal `egui-wgpu` test suite. The hardware rendering suite is now explicitly ignored/manual and fails if its opt-in flag or a D3D12/Vulkan dual-source adapter is missing.
- **Wheel scrolling no longer drives the UI faster than the display.** Wheel input is coalesced to one full repaint per display refresh, with the first event still immediate and one trailing repaint carrying the final accumulated delta. Egui's wheel smoothing now follows the same minimum cadence instead of scheduling back-to-back frames, and short high-refresh delays are no longer collapsed to zero. Line and pixel units, horizontal/vertical movement, modifiers, click order and zero-delta touchpad lifecycle phases are preserved. `TASKMAN_INPUT_STATS=1` now also reports wheel, coalesced, immediate and trailing counts.
- **A stored renderer preference is no longer silently rewritten.** Startup still coerces the value it actually uses on the current platform, but the user's explicit choice survives for a platform that supports it, and the coercion is logged once instead of being persisted as `cpu`.
- **The restore acknowledgement repairs placement before answering.** A minimized window restored after a monitor unplug is re-fitted after it is unminimized, not only on the cloaked path.

- **Text rendering follows the monitor it is actually on.** The startup ClearType query uses the real window handle, a FLAT pixel geometry (rotated or non-RGB-stripe panels) disables sub-pixel rendering, and the theme refresh key now covers the whole text-rendering state, so a change of the display's parameters re-rasterizes the font atlas instead of reusing glyphs built for the previous monitor.

### Fixed

- **A wheel scroll no longer creeps on after it looks finished.** One notch used to be smoothed by an asymptotic filter that delivered 90 % of it in the first tenth of a second and then trickled the remaining 3–4 px out for another 0.1–0.2 s in ever-smaller discrete steps, and a frame that arrived later than the predicted frame time stretched that trickle further. The smoothing is now a finite burst that lands on exactly zero inside 0.1 s, with every step smaller than the one before it up to a final landing of at most 0.2 pt, a running burst is charged the frame time actually measured instead of the nominal one, the follow-up frames keep coming until the impulse is spent instead of leaving the last sliver to an unrelated frame, and a wheel report whose repaint was dropped can no longer hold a queued notch back until something else happens to wake the window.

- **Tray/minimized GPU memory:** the D3D12/Vulkan renderer releases its swapchain, renderer, managed textures, and device when the last TaskMan surface is hidden, minimized, fully occluded, or sent to the tray, then recreates them lazily when the window returns. Swapchain, depth/MSAA targets and the size-dependent views are still released on the first hidden frame, but the device and its compiled pipelines now stay resident for a ten-second grace period: recreating them was the whole of the delay a restore paid against the CPU renderer. Past the grace the teardown goes all the way down to the device, as before. Font-atlas and icon textures are retained on the CPU and replayed after recreation, so the UI survives a tray round-trip. Explicit hides also suppress the first-frame reveal, and Wayland uses the xdg-shell `SUSPENDED` repaint hint for its unavailable native visibility/minimize query.

- **Sub-pixel text follows monitor moves and fails closed when panels disagree.** Windows now tracks the window's placement/monitor/DPI key and refreshes the DirectWrite policy without recreating the renderer. Same-sized moves between monitors are detected. If a DirectWrite query is unavailable, or a window intersects monitors whose validated text-rendering parameters or RGB/BGR orders conflict, LCD is disabled for that window until a complete agreeing query succeeds; if all intersecting monitors agree, LCD remains enabled. `TASKMAN_SUBPIXEL=0` still forces it off, while `TASKMAN_SUBPIXEL=1` cannot override an unsupported renderer or platform policy.
- **Neutral and mixed-atlas LCD glyphs render correctly.** Non-empty LCD texels now use an explicit zero-alpha tag, so neutral, tinted and full coverage are all recognized and receive the contrast curve instead of falling through as ordinary grayscale. Zero coverage stays invisible and premultiplied color glyphs retain their ordinary blend. The software direct blitter applies that distinction per texel, while transformed software text and the GPU fallback recover mean coverage from RGB rather than the tag alpha.
- **Platform-specific controls now follow backend capabilities:** Settings shows start-at-login and elevation controls only when the platform can act on them, Reset clears TaskMan's own autostart through that same action, and Services/Users no longer offer Windows-only `services.msc` or Settings-app buttons on Linux. Keyboard/context-menu paths now honor Run, affinity, efficiency-mode and end-process capabilities too, and a failed Windows elevation-token query renders Unknown instead of claiming an unelevated process. Process-tree termination continues to use the shared `end_process_tree` capability.
- **Linux service/session/boot queries stay honest and visibly unavailable.** `systemctl` and `loginctl` use trusted absolute paths, loginctl has one absolute deadline with child cleanup and a bounded retry cooldown, and a failed service/startup/session query is retained as an unavailable state instead of becoming a fresh empty table. IPv6 preference rejects tentative/DAD-failed/optimistic addresses and demotes temporary/deprecated ones.
- **Linux X11 apps are classified from their real EWMH windows without guessing from partial X11 metadata.** Non-system processes that own a Normal or Iconic managed client in `_NET_CLIENT_LIST` or `_NET_CLIENT_LIST_STACKING` now become Apps, so minimizing a window or switching desktop no longer moves the app to Background. X-Resource supplies the client PID where available, the lookup runs on a dedicated non-blocking observer thread with bounded work, and only complete, identity-verified coverage can classify a missing window as a real negative. Native Wayland roles come from the independent systemd/launcher evidence above; malformed metadata, unknown window state, and unavailable X11 evidence remain Unknown rather than becoming a false Background result.
- **Per-file I/O no longer invents traffic from seek positions.** Linux and Windows keep per-file read/write byte and operation totals unavailable unless a real per-file source measures them; the file position is exposed only as a position, never as a transfer total. Linux process-wide counters remain separate from per-file accounting, handle enumeration is identity-bracketed and bounded, deleted regular files retain their descriptor facts, incomplete snapshots persistently report their limits, and measured zero is distinct from unavailable.
- **End process tree now works on Linux and respects process identity.** Descendants come from `/proc` parent links paired with creation ticks, are signalled child-first with `SIGTERM`, and are revalidated immediately before delivery (with `pidfd` where the kernel supports it). A recycled PID is never treated as part of the selected tree, a descendant that exits during the action counts as done, and no implicit `SIGKILL` is added.
- **A failed or skipped blit can no longer leave the window blank, stale or half-painted.** The Windows software renderer now presents through its own Win32 DIB surface: every `BitBlt` result is checked, only the rectangles that were actually blitted are validated, and a `WM_PAINT` update region is merged into the present set even when no egui tile changed. A failed resize, buffer access or present warns, marks the surface unpainted and retries in 50 ms instead of aborting (release builds abort on panic) or silently freezing the old pixels in place.
- **The window repaints after a display-mode change.** `WM_DISPLAYCHANGE` and a DWM composition change now invalidate the client area, so a resolution or refresh change that keeps the client size identical no longer leaves whatever was on screen until something else forces a redraw. A minimized, hidden or zero-sized window also stops running the UI pass, which used to keep a "painted" frame counter advancing while nothing could be seen.
- **Ctrl+Shift+Esc, the tray icon and a second launch only report success once the window is really usable.** The restore no longer acknowledges the request before the queued show/unminimize commands ran: it uncloaks after a frame was painted into the visible window (bounded to 8 frames / 2 s), re-checks DWM's cloak state instead of assuming the uncloak worked, repairs a window left off-screen, and raises itself above a lingering fullscreen game that is not itself `WS_EX_TOPMOST` (a persistently topmost game still outranks a non-topmost window; see `llm-wiki/known-debt.md`). A hidden window whose tray icon never appeared is restored automatically instead of disappearing for good, and close-to-tray now requires the icon to exist rather than merely its thread to have started.
- **Saved window placement survives mixed-DPI layouts and unplugged monitors.** The outer position is stored in physical pixels alongside the legacy logical `position=` line (which is still read and written), applied before the first frame, fitted into the desktop (or onto the nearest monitor's work area when the saved spot falls into a hole in a non-rectangular layout), and re-fitted when the monitor layout changes. A window deliberately spanning two monitors is left alone; only geometry that reaches outside every monitor is moved, and only a window larger than the whole desktop is shrunk. The 720×480 minimum is clamped to the largest monitor, and the UI zoom is applied when the window is created so a non-1.0 zoom no longer resizes the window on every restart.
- **Graphics resource failures no longer abort or wait forever.** GDI allocation returns an error instead of asserting, the software surface owns its device context (no allocator thread to wait on), and a D3D12 surface that cannot be recreated keeps its old entry and schedules a retry — previously the entry was dropped before the recreate attempt, so nothing ever retried and a paused window stayed stale. GDI surface sizes are bounded (64 megapixels) rather than trusted.
- **A drag no longer sticks when Windows takes the mouse capture away.** `WM_CAPTURECHANGED` now synthesizes the button releases, focus loss clears the held-button state, and a release that arrives after the pointer left the window clears the button too — without turning that interrupted press into a click that would activate whatever sat under the cursor.
- **The file pickers and shell actions no longer freeze the window.** The dump-destination picker, the Run dialog's **Browse…** and the shell calls behind **Open file location**, **Properties** and **Open link** (including `ms-settings:` jumps) run off the UI thread on the bounded action executor; the system dialogs keep the main window as their owner (so they cannot be buried behind it), the Run dialog stays responsive while a picker is open, and a cancelled pick is remembered correctly.
- **An action with nothing to report no longer shows an empty toast.** A success message that is empty (or only whitespace) is dropped, so **Open file location** and friends no longer draw an empty frame with a close button that could also push a real message out of the toast queue.
- **The Select columns dialog fits a short or high-zoom window.** Its list height is bounded by the viewport instead of a fixed 330 pt, so the dialog's **Close** button cannot end up below the bottom edge where nothing can reach it.
- **Linux icon themes no longer abort the GUI on a 16-bit PNG.** Theme decoding now normalizes every PNG to 8-bit RGBA (`EXPAND | STRIP_16`), bounds the decoder allocation, validates the decoded length before the texture upload, and alpha-weights box downscaling so transparent borders do not darken large icons into black halos. A legal 16-bit theme icon previously yielded 8 bytes per pixel and tripped `ColorImage::from_rgba_unmultiplied`'s length assert on the UI thread (release builds abort).
- **Start-in-tray no longer flashes the window while the tray icon is being created.** Every pending frame now asserts `Visible(false)`, so eframe's first-frame reveal cannot show the window before `TrayShell` is ready; the 3 s fallback still shows it when the icon can never appear.
- **A window restored into a hole in a non-rectangular monitor layout is moved back onto a real monitor.** `ensure_window_on_screen` now enumerates monitors: geometry that intersects no monitor is fitted to the nearest work area, while a window genuinely spanning two monitors keeps its size.
- **Startup impact no longer double-counts a recycled PID with an unknown identity.** The tracker subtracts a previous baseline only when the creation identity matches; an unknown current identity now resets the baseline instead of charging the new process with the old one's counters.
- **A release synthesized by capture loss can no longer activate the control under the cursor.** `WM_CAPTURECHANGED` now sends `CursorLeft` before the synthetic releases, so egui clears the held buttons without a click; this also covers a middle-button drag.
- **Details no longer offers Open file location / Properties for kernel pseudo-processes** (`Registry`, `Memory Compression`, ...). Both menus now use the same "can this name an image" rule as the icon column, so the entries are disabled exactly as the 0.1.32 notes say.
- **Collapsed Linux process groups keep their selection.** The aggregate row now uses `creation_identity()` (boot-relative ticks) instead of the wall-clock `start_epoch_s`, matching what `identity_is_live` compares.
- **`--mock` no longer writes fabricated usage into the real app-history or startup-impact store.** Both use in-memory stores in mock mode.
- **XDG desktop-file indexing visits every subdirectory and skips shared interpreters.** The scan now tracks depth per directory (three sibling subdirs all resolve), and a `python3`/`java`/`wine`/`flatpak` `Exec=` no longer claims that interpreter's process icon.
- **App History resolves bare command keys on Linux**, so a name-keyed row can use the XDG `.desktop` lookup instead of always painting the default icon.
- **The software renderer backs off after a failed reveal re-present** instead of requesting an immediate redraw every frame, and a restored minimized window is re-fitted after it is unminimized.
- **The icon texture upload budget is enforced per frame, not per row.** A cold start with hundreds of ready icons no longer uploads all of them on the first visible frame.
- **Interrupted atomic saves no longer leave temp files forever.** Stale `.<pid>-<seq>.*.tmp` files older than an hour are swept on the next save, while a recent temp (which may belong to a live concurrent save) is left alone.
- **The Run dialog's Browse button recovers from a dead picker worker.** If the worker unwinds without publishing a result, the slot is marked cancelled instead of leaving Browse inert until the dialog is reopened.

### Changed

- **Classification API platform discriminator:** `tm_core::classify::ClassifyInput` now requires a `ProcessPlatform::{Windows, Unix}` field. Windows PID/core-image/ownership rules and Unix kernel/userspace rules no longer leak into each other, which prevents Windows images such as `migration.exe` or a container's low PIDs from becoming System unless platform evidence supports it. Downstream code that constructs `ClassifyInput` must set the field.
- **Broker protocol v12.** The creation-time token now travels as 100-ns ticks instead of seconds. The version bump makes a mixed GUI/service pair fail at the handshake with a clear incompatibility error instead of reporting every protected target as "process exited", and module-ETW revalidation accepts the mixed-precision token.
- **A post-start event-loop failure is reported as such** — "event loop failed after startup" — instead of "failed to start GUI". The original error text is still included.
- **The vendored-fork gate fails instead of silently skipping when PowerShell exists but cannot run**, and the probe has a 10 s timeout.

### Security

- **Patched `url` dependency:** the lockfile now uses `url` 2.5.9 instead of 2.5.8, addressing RUSTSEC-2026-0592 (stack overflow in `Url::origin()` for nested `blob:` URLs).

## 0.1.32 - 2026-11-21

### Improved

- **Graphics backend:** CPU rendering is the default on Windows and Linux. Settings can select D3D12 on Windows or Vulkan on Linux for the next launch. The GPU choices require a compatible hardware adapter. Initialization failures fall back to native CPU rendering, and Settings shows the active backend.

- **Every row icon now comes from the OS icon theme:** rows without an application icon no longer show a glyph of our own. On Windows and Linux, Processes, Details, Users, Startup and App History paint the platform's own default application icon instead — on Windows the shell's generic `.exe` icon (the "no icon defined" window glyph Explorer and Task Manager show), on Linux the desktop theme's `application-x-executable` icon. The accent-colored rounded square that `Memory Compression`, `System Interrupts`, `smss.exe` and other path-less kernel rows used to show is gone; icon-less system images (`lsass.exe`, `winlogon.exe`, `ntoskrnl.exe`) and path-less pseudo-processes now render the same OS glyph. Row icons are also never invented for a row that has no image. macOS has no OS icon source in this backend yet, so its icon cells stay empty (tracked in `llm-wiki/known-debt.md`).

### New

- **Linux application icons from the desktop icon theme:** on Linux, rows on the Processes, Details, Users, Startup and App History pages now resolve each application's icon through the freedesktop icon theme spec — the `.desktop` entry's `Icon=` matched by `Exec=` basename or desktop-file id across the XDG data dirs, then the current GTK icon theme (with its `Inherits` chain), `hicolor`, and the legacy pixmaps dir; PNG icons are decoded and capped at 64 px — instead of always showing the placeholder glyph.

### Fixed

- **Unix process actions:** Linux and macOS confirm that the sampled process generation remained the same across the telemetry refresh. Linux keeps the native boot-relative start ticks stable across system clock corrections.
- **Application icon loading:** Startup icon paths are resolved in the background. Linux resolves bare Startup commands and reads the current X11 session icon theme. Missing icons retain their retry delay when the texture cache is full.

- **App History rows no longer show Explorer's folder icon:** rows whose app had no known executable store a blank icon path, and the shell answers an empty name with the icon of the *current directory* — the yellow folder glyph. Blank history values are dropped during load, and bare names are filtered before Windows shell extraction. Those rows fall back to the running process’s icon or the OS default application icon. The Windows sampler also no longer stores an empty image path in the first place.
- **Process icons no longer randomly revert to the placeholder:** the icon texture cache evicted arbitrary (HashMap-iteration-order) entries once it passed its 512-icon cap, so on-screen rows could lose a just-loaded icon and flash the placeholder; eviction is now true LRU, a repaint is requested whenever textures are uploaded (the last batch no longer waits for an unrelated repaint), and a dead icon worker thread is respawned on the next request.
- **Windows shell icons no longer intermittently come back empty:** `SHGetFileInfoW` returns no icon when several threads extract at the same time, and needs a COM apartment on the calling thread; extraction is now serialized process-wide and initializes COM per thread, so a row does not lose its icon (and fall back to the generic one for 30 s) for no visible reason.
- **Kernel pseudo-process rows and the file actions:** the fabricated `System` → `ntoskrnl.exe` / `Registry` → `regedit.exe` name mapping is gone. It gave `Registry` an icon Windows itself does not show, and — because the same string doubled as the row's executable — enabled **Open file location** / **Properties** for processes that have no file (they failed or opened nothing). Pseudo-processes now show the OS default icon and those menu entries are disabled again, exactly as before.
- **Startup-page icons for env-var/`.lnk` commands on Windows:** the Startup tab's icon lookup now uses the same command resolver as the publisher/impact columns (expands `%VAR%`, resolves `.lnk` targets), so those entries get their real application icon instead of the placeholder.

- **Mock mode OS isolation:** `--mock` now wires an inert `MockActions` backend and skips the Windows Task Manager replacement check and Explorer restart path, preventing simulated UI actions from affecting real host processes or system state.
- **Concurrent settings and history save safety:** atomic file replacement (`replace_file`) now uses unique exclusive temp files (`create_new(true)` per PID/sequence) and cleans them up on failure instead of sharing a fixed `<name>.tmp` path across threads or multiple instances.
- **Installer shutdown PID-reuse TOCTOU:** `stop_running_gui` now opens each target `taskman.exe` process handle with query/terminate/synchronize rights first, verifies its full image path through that pinned handle, and terminates only the verified handle on timeout.
- **Session-scoped Windows Explorer restart:** Explorer restart now fails closed unless the target carries a verified creation timestamp matching the current desktop session's shell window (`GetShellWindow`), preventing cross-session Explorer termination from an elevated instance.
- **Sub-second PID-reuse identity protection:** process creation timestamps now preserve native 100-ns (`FILETIME` on Windows, microsecond `proc_pidinfo` on macOS, clock-tick `/proc/<pid>/stat` on Linux) resolution without ±1 s tolerance, `PlatformActions` checked/unchecked defaults fail closed cleanly, and Linux signals pin targets via `pidfd_open`/`pidfd_send_signal` (with identity-checked fallback when unavailable).
- **Recycled-PID process grouping:** Processes-tab tree parenting, flat same-image family grouping, Details search ancestor reveal, and same-image Windows service account inheritance now verify `is_plausible_parent`, keeping older orphan workers from attaching to a younger unrelated process or collapsing into shared `termination_targets`.
- **Windows service `ControlService` and restart errors:** stopping or restarting a service now passes writable `SERVICE_STATUS` storage instead of a null pointer to `ControlService`, and `Restart` propagates real stop failures rather than silently timing out in `wait_state`.
- **Renderer fallback, `EventLoop` recreation masking, and startup error diagnostics:**
  - `winit::event_loop::EventLoopBuilder::build()` now resets `EVENT_LOOP_CREATED` when backend initialization returns an error instead of leaving the process-wide guard set after no event loop was created.
  - GUI startup (`tm-app`) now aborts renderer fallback immediately on `eframe::Error::WinitEventLoop(_)` instead of retrying other renderers and overwriting the real display-server error with `EventLoop can't be recreated`, flushes early startup logs to disk on fatal failure, and reports a plain failure message naming each attempted renderer instead of `"no usable GPU renderer found: Some(...)"`.
  - Startup records which backend created the window and logs initialization failures before trying the next backend.
- **Linux glibc release cross-build discovery & static-musl startup hint:**
  - `build.py` now locates `zig` across `ZIG`, `PATH`, Python's `ziglang` package, and standard Windows install paths (`WinGet`, `Scoop`, `Chocolatey`, per-user/system Python), supplies `ZIG`/`PATH` to `cargo zigbuild`, cleans stale opposite-flavor (`-musl.tar.gz` vs glibc `.tar.gz`) Linux archives from `dist/`, and falls back from inaccessible WindowsApps `pwsh` aliases to `powershell` in `check_fork()`.
  - `tm-app` now appends an actionable static-musl note directing users to the glibc `linux-x86_64` build (or `--selfcheck`) whenever startup fails with `"Dynamic loading not supported"`.

## 0.1.31 - 2026-11-18

### Improved

- **"Restart" context-menu entry for Windows Explorer:** right-clicking a live `explorer.exe` row on the Processes or Details page now offers **Restart**, matching the top-bar Restart command shown when Explorer is selected — the same entry native Task Manager provides for the shell process.

- **Multi-engine GPU Performance page with Dedicated and Shared memory graphs:**
  - Replaced the single top GPU utilization graph with a 2×2 grid of per-engine charts (`3D`, `Copy`, `Video Encode`, and `Video Decode` by default, collapsing to one column on narrow windows), each with its own engine switcher dropdown, live percentage readout, and right-click menu. Added a persistent `Multiple engines (2×2)` toggle (`gpu_multi_engine`) and per-pane selections (`gpu_graph_engines`) in settings while keeping the single-graph mode available.
  - Added a **Shared GPU memory** rolling chart below **Dedicated GPU memory**, scaling against DXGI's `SharedSystemMemory` capacity on Windows, and updated the bottom GPU summary to show combined **GPU memory**, **Dedicated GPU memory**, **Shared GPU memory**, **Temperature** (via `D3DKMTQueryAdapterInfo` `KMTQAITYPE_ADAPTERPERFDATA`), **Driver date**, **DirectX version**, **Physical location**, **Hardware reserved** memory, and active non-grid engines.

### Fixed

- **"Delete usage history" confirmation dialog no longer opens oversized:** the dialog window now pins its outer size (420×124, stable window id) exactly like the End task confirmation, instead of falling back to egui's default window size (340×420), which made it render roughly three times taller than its content.

- **Background 30 FPS games (such as Fortnite) no longer falsely report "Not responding":**
  - Excluded non-interactive auxiliary top-level windows from application/hung tracking unless `WS_EX_APPWINDOW` is set: `WS_EX_NOACTIVATE` overlays/helpers, owned windows whose owner is already visible (`GW_OWNER`), `Shell_TrayWnd`, and non-desktop shell window bands (retaining UWP `ApplicationFrameWindow` / `CoreWindow` bands).
  - Resolved DWM `"Ghost"` windows back to the underlying hung window via `HungWindowFromGhostWindow` instead of blaming `dwm.exe`, and checked hosted UWP `CoreWindow` child handles directly instead of `ApplicationFrameHost.exe`'s shared frame.
  - Added bounded non-aborting `SendMessageTimeoutW(..., WM_NULL, SMTO_BLOCK, 200 ms)` verification (deduplicated per thread within a 600 ms scan budget) whenever `IsHungAppWindow` fires, so threads actively pumping at 30 FPS that trigger kernel `IsPumpingInputMsgs` false positives (e.g. via filtered `PeekMessage`) clear immediately instead of tripping `NotRespondingHold` while deadlocked/blocked and elevation-gated (`ERROR_ACCESS_DENIED`) hung windows remain detected.

- **In-window mouse movement no longer spins the CPU or repaints the full window:**
  - Stopped `egui`'s tooltip wait loop (`show_tooltips_only_when_still = false` in `tm-ui`) from requesting immediate `ControlFlow::Poll` repaints on every frame while the cursor moves over any tooltipped widget.
  - Replaced the row-wide `POINTER_TRACKING_HINT` with `POINTER_STABLE_RECT` (`egui-winit` #10) so table rows only repaint when crossing into a different cell or status glyph (and anchored row tooltips at the opening pointer position via `hover_text_at_fixed_pointer` instead of chasing cursor jitter), while charts (`Rect::NOTHING`) keep smooth readout tracking.
  - Fixed `hover_target_changed` in `egui-winit` to compare `(contains_pointer, click, drag)` signatures so approaching buttons within `interact_radius` still highlight, cached the previous pass's signature per `(pass_nr, pos)`, short-circuited coalesced moves before hit-testing, coalesced consecutive `PointerMoved` events when no button is down, and cached the display refresh interval for 2 s (invalidated on move/resize/scale/focus) to avoid repeated `EnumDisplaySettingsExW` calls.
  - Added 64×64 tile damage tracking in `egui_software` and `eframe`'s software backend (`present_with_damage` when `buffer.age() == 1`, skipping `present` on zero dirty tiles, and forcing a full present once the window is visible after startup or restore from tray/minimized/occluded) so hover changes and periodic refreshes no longer clear, rasterize, and `BitBlt` all pixels on screen.
  - Skipped installing the taskbar monitor's foreground `EVENT_OBJECT_LOCATIONCHANGE` hook on TaskMan's own process (`sync_foreground_hook` in `tm-platform`), notifying the monitor on TaskMan's own window moves/resizes instead, and avoided per-frame string allocations in `sync_taskbar_monitor`.
  - Added `TASKMAN_INPUT_STATS=1` diagnostics (per-second `CursorMoved` decision counts, software pass/paint/present timings, tile-damage counts, top `repaint_causes()`, and one-shot AccessKit activation logging).

## 0.1.30 - 2026-11-17

### New

- **2 s update speed option:** the Settings dialog's Update speed block now offers a `2 s` choice between Normal (1 s) and Low (4 s), in addition to the existing High, Normal, Low and Paused values. It persists in the config as `update_speed=2s` and makes the engine and update-speed-driven pages sample every two seconds; pages with a longer floor (Services: 5 s) keep that floor, and Paused still freezes updates.

### Fixed

- **Mouse movement outside the TaskMan window no longer raises CPU load while the window has focus:** winit registers all mice and keyboards for raw input with its default `DeviceEvents::WhenFocused`, so on Windows every mouse packet system-wide arrived as `WM_INPUT` while any TaskMan window was in the foreground — even with the cursor 100% outside the window — and each packet was dispatched through the UI thread (and could even trigger a full repaint when the tracked pointer position was stale). The vendored egui fork now unregisters raw device input when the event loop is created (`listen_device_events(DeviceEvents::Never)`); egui receives pointer movement from ordinary window cursor events, so hover, drags and tooltips are unchanged. Measured on the live app with synthetic ~5 kHz mouse movement outside the focused window: ~0.6 cores before, at idle level (~0.04–0.15 cores) after; at a realistic 1 kHz a minimal eframe app went from ~0.14 cores to ~0. Movement over the window itself still repaints per move and is unchanged.

## 0.1.29 - 2026-11-17

### Fixed

- **Process Properties I/O no longer disturbs the inspected process or mislabels its files:** reading each open file's position used to write that position back into the file object the target process shares with TaskMan, so a write the target made at that moment could be rewound; positions are now only queried. Access rights are now read from the handle actually inspected instead of from the earlier handle snapshot, so a handle the target closed and reused in between can no longer make a write-only file show up as read/write (with its byte counts then shown as unavailable).
- **Taskbar stats no longer blink out when a layout check fails:** a timed-out or momentarily empty UI Automation recheck of an unchanged taskbar (common while Explorer is busy or the system is loaded) used to discard the confirmed layout and hide the stats for at least half a second. The widget now stays in place, click-through, on its last confirmed layout while the scanner retries with backoff, and only hides if three consecutive rechecks fail (about 3.5 s). Taskbar geometry, DPI or identity changes still discard the layout immediately.
- **Taskbar stats stay visible with an auto-hide taskbar over maximized windows:** with auto-hide enabled, every maximized window's frame covers the whole monitor, so the widget treated it as a fullscreen app and stayed hidden when the taskbar was revealed. Maximized windows with a standard title bar no longer count as fullscreen, matching how Windows itself keeps revealing the taskbar over them.
- **Taskbar stats values no longer freeze during layout revalidation:** while taskbar button positions are being re-confirmed, new CPU/RAM/network values are now drawn in place at the unchanged footprint instead of waiting for the scan to finish.
- **No single-frame overdraw when the stats move under a menu or topmost window:** a new or growing cut-out (or a hide) is now applied before the moved or resized frame is presented, and a vanishing cut-out is removed only afterwards.
- **Taskbar stats occlusion handling is low-overhead:** occlusion checks reuse the occluder rectangles from the same window enumeration rather than running a second full `EnumWindows` pass, and sub-2-pixel jitter in a popup frame no longer reinstalls the window region, avoiding needless desktop compositor work.
- **Taskbar stats no longer vanish behind overlapping windows:** a partially covered widget now stays visible and is clipped around each overlapping window's frame with a Win32 window region instead of hiding entirely, so the Windows 11 taskbar right-click menu and an always-on-top Task Manager moved a few pixels over the display no longer blank the whole overlay. It still hides when fully covered, when the tray is missing, or when clipping is unavailable, and clears the region as soon as the occluder disappears. The invisible XAML drop-shadow that Windows 11 taskbar flyouts render inside their HWND frame is no longer subtracted from the widget region, so opening the taskbar context menu no longer cuts off the top row of the stats.
- **Taskbar monitor transient disappearances on window switches and tray updates:**
  - Stopped `TrayNotifyWnd` width/position adjustments (such as IME mode indicators, microphone/location privacy icons, or notification tray icons appearing/disappearing when switching windows) from invalidating the UI Automation occupancy cache and hiding the taskbar stats for `120–280 ms`. Only core taskbar geometry changes (`Shell_TrayWnd` identity, `taskbar_rect`, or DPI) now force a hard cache reset; `TrayNotifyWnd`-only changes keep the widget visible, immediately recompute placement against the updated `tray_notify_rect` and cached button rectangles in click-through mode (`WS_EX_TRANSPARENT`), and revalidate via UI Automation in the background.
  - Hardened the background UI Automation scanner against COM timeouts, transient scan failures, and burst starvation: raised `CUIAutomation8` connection/transaction timeouts from `50 ms / 80 ms` to `150 ms / 250 ms`, added immediate one-time session re-initialization on the first scan failure (including any per-element `GetElement` / `CachedBoundingRectangle` failure so partial button lists are never published as complete) before entering progressive backoff, and allowed completed post-invalidation scans superseded by a soft recheck to populate an empty cache (`current = false`) rather than leaving the cache empty across event bursts.
  - Eliminated false fullscreen and occlusion hides: scoped `SHQueryUserNotificationState` (`QUNS_RUNNING_D3D_FULL_SCREEN` / `QUNS_PRESENTATION_MODE`) suppression to foreground windows actually ordered above `Shell_TrayWnd` on the taskbar's monitor, ignored click-through/alpha-0 foreground overlays, verified `saw_tray` (With a mid-scan band-change retry) before trusting a Band-1 occluder in `EnumWindows` so transient `Shell_TrayWnd` promotion to Band 6 (`ZBID_IMMERSIVE_MOGO`) cannot misclassify normal Band-1 windows as above-tray occluders, and armed the 500 ms recovery timer whenever `tray_missing_in_enum` hides the widget.
  - Hooked `EVENT_OBJECT_DESTROY` in the global visibility hook (`LIVE_OCCLUDER_HWND`) so destroyed popup menus and tooltips wake the taskbar monitor immediately even when Windows omits `EVENT_OBJECT_HIDE`, scoped `EVENT_SYSTEM_MOVESIZEEND` layout rechecks to `Shell_TrayWnd`, allowed `TaskbarCreated` / `SHELLHOOK` / theme broadcasts through UIPI when running elevated (`ChangeWindowMessageFilterEx`), and cleared stuck hover highlights when a modal popup steals `WM_MOUSELEAVE`.
  - Filtered non-finite (`NaN` / `±∞`) CPU, RAM, and network rate values to unavailable (`"—"`) instead of fabricating `"0 B/s"`.

### Improved

- **Taskbar stats cost Explorer far less work on window open/close:** taskbar layout rechecks triggered by shell and taskbar notifications are now debounced into a single scan once the burst has been quiet for 300 ms (at most 1 s later), instead of one cross-process UI Automation walk of the taskbar every 80 ms during the burst plus a separate settle rescan. Each walk was measured at roughly 15 ms and about 9 ms of Explorer CPU; startup, invalidation, failure retries and returning from fullscreen still scan immediately, and no scans run while a fullscreen app suppresses the widget.
- **Half the mouse-move wakeups while a fullscreen game is in front:** the foreground-window location hook also receives every cursor move (about 100 M CPU cycles per second of continuous mouse movement). The fullscreen suppressor's thread no longer gets a second, identical location hook when it is the foreground thread (the usual game case); the dedicated hook is installed only when foreground moves to another thread.
- **Fewer cross-process calls per taskbar refresh:** the auto-hide state (`ABM_GETSTATE`, a synchronous message to Explorer that waits on a hung Explorer) is only queried when the taskbar geometry actually looks slid away, and the session-wide fullscreen state query only runs when the foreground window is ordered above the taskbar.
- **Diagnostics for disappearances that are not hides:** a DWM/shell cloak of the taskbar stats window itself (for example during a virtual-desktop switch animation) is now logged at debug level with its cloak reason; the system-wide hooks skip TaskMan's own windows, so these were previously invisible in the logs.
- **Taskbar monitor CPU/GPU overhead and engine sampling isolation:** Unified the per-refresh Z-order, fullscreen, occlusion, and topmost-thread `EnumWindows` passes into a single stack-buffered pass (`ClassNameBuf`, checking `DWMWA_CLOAKED` before `OpenProcess`), cached DirectWrite text formats, static glyph layouts (`↑`, `↓`, `CPU:`, `RAM:`, `C:`, `R:`), sample widths, rendering params, and Direct2D brushes across ticks, skipped redundant DirectWrite layout measurement when metrics and core geometry are unchanged, unhooked topmost-thread WinEvent hooks while disabled or fullscreen-suppressed, and stopped taskbar monitor visibility transitions from re-applying identical sampling intervals or forcing off-schedule full-system engine refreshes while the main TaskMan window is already open.

## 0.1.28 - 2026-11-16

### Improved

- **Faster taskbar monitor reappearance when Alt+Tabbing out of fullscreen:** Alt+Tabbing or switching away from a fullscreen window now restores the taskbar stats display immediately whenever the taskbar geometry and DPI are unchanged, instead of discarding the cached layout and waiting for a fresh UI Automation scan. The widget reappears click-through (`WS_EX_TRANSPARENT`) on its cached placement while UI Automation verifies taskbar button positions in the background, tracks the suppressing fullscreen window across focus handoff, and runs a short bounded transition check if Windows finishes clearing fullscreen Z-order or D3D state slightly after focus changes. Changed taskbar geometry, failed scans, or a full taskbar still wait for fresh UI Automation confirmation.

### Fixed

- **Taskbar monitor hidden during Windows 11 Task View (`Win+Tab`):** kept the taskbar stats display visible when opening Task View / virtual desktop overview by treating Windows 11 Task View overlay canvases (`XamlExplorerHostIslandWindow`, `MultitaskingViewFrame`, and `Windows.UI.Core.CoreWindow`) owned by `%SystemRoot%\explorer.exe` and verified Windows shell host binaries as non-occluding overlays, while still suppressing or hiding behind regular apps, fullscreen File Explorer (`CabinetWClass`), menus, tray overflow, and tooltips.
- **Delayed taskbar monitor reappearance after closing a fullscreen window:** hooked object lifecycle (`EVENT_OBJECT_DESTROY`..`EVENT_OBJECT_HIDE`) and cloaking (`EVENT_OBJECT_CLOAKED`..`EVENT_OBJECT_UNCLOAKED`) events on the tracked fullscreen suppressor thread plus top-level cloak/uncloak events, queued immediate fullscreen wakes on hide/cloak/destroy transitions, and tuned the bounded fullscreen-exit transition recheck (`10 ms × 70`) so slower window teardown or D3D fullscreen exit does not fall back to the `300 ms` settle timer or `5 s` hidden interval.
- **Taskbar monitor UIA failure spin, re-entrancy safety, and background topmost occlusion:**
  - Stopped repeated 80 ms UI Automation retries and message-thread wakes when taskbar button layout is unavailable across consecutive scans (`None` before and after), applying progressive failure backoff (`500` ms -> `1` s -> `2` s -> `5` s) queued directly inside the UIA worker so recovery continues even when sampling is paused while still waking immediately whenever valid button geometry disappears or a geometry change arrives.
  - Eliminated Rust aliasing UB (`&mut WindowCtx` across synchronously re-entrant Win32 calls and `TrackPopupMenuEx`'s modal menu loop) in the taskbar monitor window procedure via `RefCell` interior mutability (`WndProcState`), queuing re-entrant refresh/layout/theme/close flags until the outer borrow drops and guarding against nested context menus.
  - Added a bounded 500 ms topmost-occlusion check (`TOPMOST_RECHECK_TIMER_ID`) active only while the Band-16 widget is visible or occlusion-hidden so background windows promoted to `HWND_TOPMOST` without move, size, activation, WinEvent, or `SHELLHOOK` notifications are detected, and clear occlusion of the blocked placement candidate (`last_occluded_rect` before `last_rect()`) restores visibility cleanly even when paused.
- **Taskbar monitor visibility with Windows 11 Start, Search, and shell flyouts open:** Opening the Windows 11 Start Menu, Search, or tray flyouts no longer hides the taskbar stats. On Windows 11, opening Start or Search promotes `Shell_TrayWnd` into immersive band 6 (`ZBID_IMMERSIVE_MOGO`), which `EnumWindows` omits from non-immersive callers (`tray_missing_in_enum`), and places `Windows.UI.Core.CoreWindow` overlay canvases (`StartMenuExperienceHost.exe`, `SearchHost.exe`, `SearchApp.exe`, `ShellExperienceHost.exe`, `ShellHost.exe`) in band 6 above the taskbar. Occlusion and fullscreen scans now skip lower-band windows (`band < tray_band`), keep Band-16 stats visible while a live `Shell_TrayWnd` resides in an elevated band (> 1) omitted by `EnumWindows`, ignore those shell host `CoreWindow` canvases (while still hiding behind genuine overlapping popup menus such as `#32768` or `Xaml_WindowedPopupClass`), and ignore cross-band higher windows in band-1 fallback ordering.
- **Taskbar monitor focus flicker eliminated via elevated window band:** Clicking the taskbar after another window had focus no longer flashes or briefly hides the taskbar stats text. On Windows hosts supporting `CreateWindowInBand`, the unowned taskbar monitor now runs in window band 16 (`ZBID_GENUINE_WINDOWS`) instead of sharing band 1 with `Shell_TrayWnd`, preventing taskbar activation from momentarily raising `Shell_TrayWnd` over the widget before async hooks run—without Explorer ownership or input-queue attachment. Overlapping popup menus, tooltips, and always-on-top windows above the taskbar hide the widget while overlapping its footprint, its own right-click context menu anchors just outside the taskbar edge so menus stay unobscured, and hosts denying band 16 fall back cleanly to standard band-1 Z-order tracking.
- **Taskbar monitor click flicker:** Opening/restoring TaskMan and taskbar control notifications now keep the existing display visible and stationary while layout is revalidated, temporarily passing clicks through to protect controls underneath. Fresh results restore interaction or reposition/hide the display; failed scans and actual taskbar geometry changes still hide it. Focus-time stacking repair runs before queued layout/render work, ignoring hidden/cloaked bookkeeping windows while staying below visible popups. The monitor remains unowned and keeps its input queue independent of Explorer.
- **Taskbar monitor isolation and background work:** The Windows taskbar monitor no longer shares an input queue with Explorer through cross-process window ownership. It follows taskbar Z-order without modifying Explorer, hides if safe positioning is denied, and restores the five-second background sampling interval whenever both the main window and taskbar monitor are hidden. Showing the monitor requests a fresh sample while preserving Pause.
- **Taskbar button access and visibility checks:** The monitor stays hidden until UI Automation confirms taskbar occupancy, discards failed or stale layout results, and no longer places itself over buttons when layout information is unavailable. Z-order regression checks now handle desktops with more than 128 intervening windows.

## 0.1.27 - 2026-11-15

### Fixed

- **Taskbar monitor vertical, small-bar, and accent-color handling:** The Windows taskbar performance monitor now stacks network and CPU/RAM sections vertically when a left- or right-pinned vertical taskbar is narrower than side-by-side columns, steps down font size (down to `6.5` pt) when needed to fit short horizontal or narrow vertical taskbars without clipping or `clamp` panics, and hides cleanly rather than overflowing if the taskbar remains too narrow. In `Auto` color mode, contrast detection now also evaluates Windows `ColorPrevalence` and `DWM\AccentColor` luminance on `WM_SETTINGCHANGE` and `WM_DWMCOLORIZATIONCOLORCHANGED` so dark or light accent-colored taskbars get matching text contrast without screen pixel reads.
- **Taskbar monitor visibility while holding Alt+Tab in fullscreen apps:** The Windows taskbar performance monitor no longer pops up while holding `Alt+Tab` over a fullscreen game or app when the taskbar itself remains hidden behind the fullscreen window. Visibility detection now checks Z-order for uncloaked, non-transparent fullscreen windows still sitting above `Shell_TrayWnd` even after the task switcher takes foreground focus, and non-topmost demotion keeps the widget anchored right above `Shell_TrayWnd`.

## 0.1.26 - 2026-11-14

### Improved

- **Collision-free Windows taskbar monitor placement:** The taskbar stats overlay now inspects visible `Shell_TrayWnd` UI Automation bounds on a dedicated event-driven MTA thread and places itself into open taskbar space—stepping past XAML Widgets/Copilot controls beside the tray, relocating with hysteresis to an open alternate gap (such as the left side of a crowded centered Windows 11 taskbar), switching to a narrower compact layout (`C`/`R` labels) when tight, and cleanly hiding instead of overlapping taskbar buttons if the taskbar fills end-to-end (with `TrayNotifyWnd`-relative fallback if UIA is unavailable).

## 0.1.25 - 2026-11-14

### New

- **Windows taskbar performance monitor:** Added an optional taskbar overlay in **Settings** that renders live CPU usage, RAM usage, and total network upload/download rates next to the notification tray using software Direct2D/DirectWrite + `UpdateLayeredWindow` (without `GetDC(NULL)`/`GetPixel` screen reads or Explorer `SetParent` hooks). Supports toggling individual metrics, font family/size, network speed units (`Auto`, `KB/s`/`Kbps`, `MB/s`/`Mbps`, Bytes/s vs bits/s), and `Auto` (taskbar theme) or custom RGB text color; automatically hides when the taskbar auto-hides or a fullscreen window covers its monitor, and keeps TaskMan alive in the system tray on window close while enabled.
- **Row spacing option in Processes and Details overflow menus:** Added a **Row spacing** (**Normal** / **Compact**) submenu to the top-right `…` menu on both the Processes and Details pages. Processes defaults to **Normal** (32 px) and Details defaults to **Compact** (20 px), and each page's choice is persisted independently in `config.ini`.

### Changed

- **Installer post-install launch enabled by default:** The "Launch Task Manager when setup completes" checkbox in `taskman-setup.exe` is now ticked by default (and silent CLI installs launch Task Manager unless `--no-launch` is passed).

## 0.1.24 - 2026-11-10

### New

- **Per-file read and written byte tracking in Process Properties I/O dialog:** Added a sortable file activity table below the I/O graphs and summary counters in the Process Properties dialog. The table enumerates open and active files for the inspected process, displaying exact amounts read and written, total transfer size, and on-disk file size. Users can interactively sort by written most, read from most, total transfer, file size, or filename (toggling ascending/descending order with indicators), filter files with a live search box, and access context-menu actions to open the file location in Explorer, copy the path, or open file properties.
- **Process Properties I/O page with inter-process (IPC) telemetry:** Added a dedicated "I/O" tab to the Process Properties dialog featuring Process Hacker / System Informer style rolling history graphs for Read (yellow), Write (purple), and Inter-Process / Other (cyan) I/O, along with a "Disk & Inter-Process I/O" table breaking down cumulative operation counts (Reads, Writes, Other), cumulative transfer bytes, interval deltas, and current transfer rates. Inter-process I/O captures operations not involving physical disks (such as named pipes, ALPC, device controls/IOCTLs, and socket communications) read directly from the Windows kernel process table's `IO_COUNTERS` block without requiring elevated process handles.

### Improved

- **Process Properties title and heading process name alignment:** The title bar of the Process Properties dialog and the program heading on the General tab now display the executable process name (e.g. `explorer.exe`) rather than the localized process description (e.g. `Windows-Explorer`).
- **Process Properties I/O tab cleanup and files table path column:** Removed the redundant "Options" dropdown menu button and the elevation "Protection" label along with the vertical gap below the I/O summary table. The files table now includes a dedicated "Path" column with interactive sorting and truncated path display, and right-clicking any table entry opens a context menu with "Copy" (copying active text selection or cell text), "Select all" (selecting text in that entry), and "Open file location" (opening Windows Explorer with the target file preselected).
- **Parent process name and binary display on Process Properties:** The Parent process row on the General tab of the Process Properties dialog now displays both the friendly process name and the executable/binary name alongside the PID (e.g. `Services and Controller app (services.exe) (1496)` or `Userinit Logon Application (userinit.exe) (5428) (Non-existent process)`), rather than only the friendly description or a raw numeric PID. For elevated processes, system services, and processes in other sessions, parent process resolution routes through the authenticated LocalSystem elevation service (`TaskmanCore`) via protocol v11 `ProcessSecurityInfo` with verified creation identity, resolving the parent process handle, image path, binary name, and FileDescription even when non-elevated GUI tokens are denied access. When the parent process has terminated (such as `userinit.exe` for `explorer.exe`), resolution automatically falls back from standard Win32 format to native NT image paths (`PROCESS_NAME_NATIVE`) on lingering kernel process objects and inspects `GetExitCodeProcess != STILL_ACTIVE`, accurately discovering the parent binary name and marking it as terminated, supported by an in-session bounded process history cache and `CpuLoadSampler` kernel-table parent PID fallbacks.
- **Process Properties I/O graphs theming and pinned hover readout:** Process Properties I/O tab graphs now match the styling and theming of the Performance page for disks: dark/light adaptive sunken inset card surfaces (`pal.card_bg_sunken` to visually distinguish chart cells from the modal dialog window background), subtle gridlines, and coordinated series colors (green for Read, amber for Write, and teal for Inter-Process/Other) with translucent area fills, replacing retro green-on-black canvases. Cursor hover tooltips now appear directly at the pointer position with series swatches, and stay pinned to the inspected sample timestamp across rolling graph updates until the pointer is moved again. In the Disk & Inter-Process I/O table, the values for the trailing "Other (IPC)" column now use measured bounded cell allocations so their spacing matches the clean 16 px gap of the Read and Write columns instead of stretching to the window border.

### Fixed

- **Process Properties I/O handle access for services and system processes:** Resolved `OpenProcess: Zugriff verweigert (0x80070005)` errors when opening the I/O tab for services (such as background services) or system processes. File I/O queries now route through the authenticated LocalSystem elevation service (`TaskmanCore`) via protocol v10 `ProcessFileIo` endpoint, allowing an unelevated GUI to inspect open files and read/write byte positions for services. For processes where handle duplication is restricted by the kernel (such as PID 4 System or protected PPL processes), queries fall back to reading process-level kernel `IO_COUNTERS` via `PROCESS_QUERY_LIMITED_INFORMATION` or `SystemProcessInformation` without failing, rendering file availability as unavailable rather than reporting an error.
- **Process Properties I/O per-file attribution honesty and accuracy:** Eliminated false byte counts in the file list on the Process Properties I/O tab, where write-only file handles on existing files previously fabricated their entire file size as written bytes, and read/write handles duplicated their seek pointer into both columns and double-counted in total. Handle pointer offsets are now strictly separated from cumulative transfer counts, read/write access is accurately differentiated, and an explanatory tooltip and non-file I/O notice clarify why kernel process I/O (network sockets, named pipes, IPC, and closed temporary files) in the graphs above exceeds the open disk file handles below.


## 0.1.23 - 2026-11-03

### New

- **Batch process dump creation for multi-selections:** When multiple processes
  are selected in either the Details or Processes tabs (or when right-clicking an
  aggregate process group), the "Create dump file (N)" command prompts for a target
  directory via a folder picker and creates memory dumps sequentially for all
  selected processes (`<name>_<pid>_<timestamp>.dmp`). A live progress modal
  shows current file progress, overall completion (`[i/N]`), transfer rate, and
  responsive cooperative cancellation.

### Improved

- **Process command-line hover for all processes:** Hovering over the Name or Command Line columns on the Processes and Details tabs now reliably spawns a tooltip for every concrete process, falling back to the executable path or image name if an explicit command line was not recorded or restricted, while keeping uncollapsed process groups quiet and preventing truncated names from suppressing command-line visibility.

### Fixed

- **Battle.net name rendering and Unicode dot leaders:** The friendly process description for Battle.net (`Battle.net.exe`) previously rendered a missing glyph tofu box placeholder (`▢`) instead of a period because Blizzard's PE `FileDescription` metadata uses the Unicode ONE DOT LEADER (`\u{2024}`) rather than an ASCII period, and Windows' `Segoe UI Variable` lacks that glyph. Version string extraction now normalizes Unicode dot leaders (`\u{2024}`) to ASCII periods for reliable rendering, search filtering, online searches, and clipboard copying; in addition, `Segoe UI Symbol` (`seguisym.ttf`) has been added as a system fallback font in `tm-ui` so arbitrary punctuation and mathematical symbols in process names and telemetry render properly.
- **Process properties text selection and context menu:** Selecting text with the left mouse button across labels and property rows in the Process Properties dialog was previously discarded immediately upon right-clicking because pointer interaction unconditionally collapsed the selection. Right-clicking now preserves the active selection and opens a context menu offering "Copy" (copying the marked text, or the row's full value if nothing is selected) and "Select all" (selecting all text in that property row).
- **Foreground and input focus over fullscreen 3D games when restored from minimized:** When TaskMan
  was evoked via Ctrl+Shift+Esc while minimized to the taskbar and a borderless or exclusive
  fullscreen 3D game (such as DOOM Eternal) held focus, the window would visually render on top
  but input and mouse clicks were still captured by the game because Windows denied foreground
  transfer to a target thread without merged input rights and cursor clipping remained active.
  `force_foreground_attached` now merges both the worker and target threads with the foreground
  queue for the activation, waits for the asynchronous transition to complete before detaching,
  and releases Win32 cursor clipping (`ClipCursor(None)`).
- **Search bar focus indication on window restore/activation:** Fixed an issue
  where the search bar intermittently lacked its blue focus ring and blinking text
  caret despite accepting keyboard input when the window was activated or restored
  from the system tray, taskbar, or title bar. Synchronizing Win32 focus on the
  foreground window ensures viewport focus state and caret animation reflect
  active keyboard focus immediately.
- **TCP listener state and remote peer on Windows 11:** Listening sockets
  on modern Windows 11 builds, which report `dwState = 0` via `GetExtendedTcpTable`,
  are now properly recognized as `Listen` rather than raw state `0`, and placeholder
  endpoints (`0.0.0.0:0` or `[::]:0`) are no longer exposed as active remote peers.
- **Table hover tooltips:** Hovering over metric columns (such as CPU, Memory, Disk,
  or Network) in the Processes and Details tabs no longer displays the process command-line
  tooltip over the entire row, restricting command-line tooltips to the Name and Command Line
  columns.
- **Process metadata consistency across timestamp skew:** Base priority, session ID,
  working set, handle count, and thread count in `CpuLoadAccountant` now tolerate the same
  1-second rounding discrepancy between `sysinfo` process creation times and native kernel
  process tables as process lifetime checks, preventing intermittent telemetry drops.
- **App history multi-core scaling:** Fallback CPU usage accumulation when cumulative
  process CPU time is unavailable now scales system-normalized CPU percentages by the
  machine's logical core count, avoiding core-count-factor under-reporting.

## 0.1.22 - 2026-10-26

### New

- **Thread search:** Process Properties now has the same live filter-style
  search box on Threads that Modules already offered. It matches decimal and
  hexadecimal thread IDs, hexadecimal start addresses, `module+offset`, the
  module path, priority, state, activity, CPU percentage, and CPU time.

### Improved

- **Process command-line hover:** Processes and Details show an available
  command line when hovering a process, even when its text is not clipped.
- **Thread start addresses for elevated processes:** Process Properties now
  asks the authenticated LocalSystem core service for thread start addresses
  and cycle counters that the unelevated GUI cannot read itself. This fills
  the previously blank fields for targets such as Windows Task Manager while
  preserving honest "—" values when Windows still refuses access. The page
  reports how many thread records remain partial, sorts Start address by the
  numeric address instead of the displayed module name, and keeps the
  handle-free local read as a fallback when the service is unavailable.
- **Executing-module evidence:** Modules now shows Code samples and Sample
  share from an on-demand privileged system-profile ETW window. Samples are
  attributed to the module ranges of the exact process, so an injected hook or
  overlay such as RTSS's `rtsshooks64.dll` can show execution evidence even
  when it starts no threads of its own. These are sampled execution/share
  evidence, not exact CPU-time attribution; lost or unknown ETW loss renders
  counts as lower bounds and never turns missing evidence into a zero.
- **Protected-process module inventory:** when ToolHelp cannot open a target's
  module list, the LocalSystem service reconstructs it from kernel loader
  load/unload and rundown events, including the legacy load-only, compact, and
  current full Image_Load layouts. This covers protected targets such as
  `csrss.exe`, which deny direct module enumeration. The first request starts
  the rundown and the next refresh exposes the completed list; target decode,
  loss, or completeness failures remain unavailable instead of showing a
  partial list.
- **Core-service profile privilege:** the LocalSystem broker now whitelists
  `SeSystemProfilePrivilege` alongside `SeDebugPrivilege`. Without it, Windows
  denied every profile `StartTrace` with `ERROR_PRIVILEGE_NOT_HELD`, leaving
  Code samples unavailable for every process.
- **Injected overlay module semantics:** The Modules page now labels its count
  as threads started in the module and explains that a loaded hook or overlay
  DLL, such as RTSS's `rtsshooks64.dll`, can correctly show zero. Those DLLs
  execute on existing game/render threads rather than creating threads that
  start inside the injected module; the new code-sample columns provide the
  complementary execution evidence.
- **Thread and module activity reporting:** Process Properties now distinguishes
  threads that are currently running, ran recently, are provably idle, or are
  still being observed. The Threads page shows cumulative CPU time plus
  current and observed-window average CPU, kernel/user time, cycle details,
  sample age, and elapsed idle time. The Modules page labels its verdict as
  origin-thread activity and explicitly distinguishes own threads from no own
  threads, loading, unavailable data, and unknown start addresses.

### Fixed

- **Process row tooltips:** hover information on Processes and Details now
  appears near the pointer instead of at the left edge of the row.

- **False thread and module activity:** Ready, Standby, and DeferredReady
  threads no longer count as having executed. Activity observed across Paused
  mode, another properties tab, sleep, or another long sampling gap is now
  re-observed instead of being reported as recent; unchanged counters across
  such a gap remain valid idle evidence.
- **System and service process account names in CI / domain environments:**
  `sysinfo`'s user enumeration falls back to the LSA logon session user name
  when its internal `LookupAccountSidW` query fails to allocate a domain buffer.
  In elevated Windows Server/Azure runner environments, this reported the computer
  account (e.g. `runnerxyz$`) rather than `SYSTEM`. The sampler now canonicalizes
  well-known SIDs and computer account names from user enumeration to canonical
  account names (`SYSTEM`, `LOCAL SERVICE`, etc.) and pre-seeds well-known accounts.

### Changed

- **Core service protocol v9:** the service gains an identity-bound
  `ProcessModuleCodeSamples` request for sampled module execution and now uses
  the protected loader rundown for module inventory when direct access is
  denied. The app and service must be updated together.

## 0.1.21 - 2026-10-21

### New

- **Threads, Network and Services pages in Process Properties:** like System
  Informer, the properties dialog now lists a process's threads (TID, CPU,
  cycles delta, start address as `module+offset`, symbolic priority, state
  with wait reason), its TCP/UDP endpoints next to its live receive/send
  rates, and the services it hosts (double-click or "Go to service(s)" jumps
  to the Services page). The thread list also works for service and SYSTEM
  processes without administrator rights; their start addresses and cycle
  counts need thread access and read "—" where Windows denies it. All list
  pages refresh themselves while shown, at the update speed (frozen while
  paused), and F5 refreshes the open page.
- **Dormant threads and modules are marked:** a thread that went at least
  5 s without being scheduled once, and without using CPU time or cycles, is
  labelled *Dormant* and dimmed ("Hide dormant" filters them). On the Modules
  page, a module whose own threads are all dormant is marked the same way;
  a module that starts no thread of its own is never called dormant, because
  its code runs on other modules' threads.
- **Priority and affinity on the Processes page:** the context menu gains
  the Priority submenu and "Set affinity…" from Details. On a group row they
  apply to every member; a priority is ticked only when all members share it,
  and "Save priority for this program" is offered only when every member is
  the same program.

### Changed

- **Modules moved into Process Properties:** the module list, including
  unload, is now the Modules page of the properties dialog and gains Threads
  and Activity columns. It now updates itself — DLLs loaded later appear
  without a manual refresh — so its Refresh button is gone, as are the
  separate "View modules" context-menu entry and button. An unload confirmed
  during a refresh waits for it instead of failing.

- **One Performance card per physical disk:** disks are listed like Task
  Manager — "Disk 0 (C: D:)" — instead of one card per drive letter, which
  showed two volumes of one disk as two cards each claiming the whole disk's
  activity. Disks without a drive letter now get a card too (capacity rows
  are omitted for them). The core service protocol moves to v7, so the
  service must be updated together with the app.

### Fixed

- **List lost keyboard focus after any dialog:** after closing the End
  process confirmation, Process Properties or any other dialog, the row
  stayed selected but Delete and the arrow keys did nothing until it was
  clicked again. Focus now returns to the list (or whatever held it) when a
  dialog closes, including dialogs opened from a right-click menu.
- **Blue-filled confirmation buttons:** the action button of confirmation
  dialogs (End process, Unload module, service Stop/Restart, Apply affinity
  and the other two-button dialogs) was painted solid blue with a white ring,
  left over from an earlier theming attempt. It now looks exactly like the
  Cancel button; whichever button Enter would press carries a blue outline.
- **Ctrl+Shift+Esc did nothing in some games:** a game or tool with its own
  low-level keyboard hook could swallow the combo before TaskMan saw it —
  DOOM Eternal blocks Escape whenever Ctrl is held — so neither TaskMan nor
  Windows opened anything. TaskMan's hook now moves itself ahead of such
  hooks the moment Ctrl+Shift is pressed; all other keys still reach them
  unchanged. While TaskMan is the Task Manager replacement, tools that
  deliberately capture Ctrl+Shift+Esc (key remappers, kiosk software) no
  longer receive it.
- **Minimized TaskMan stayed behind fullscreen games:** with TaskMan's
  window minimized (rather than closed to the tray), Ctrl+Shift+Esc in a
  fullscreen game left it hidden behind the game. The window is now restored
  first and brought to the front afterwards.
- **Hotkey lost while TaskMan's window was frozen:** a press caught by the
  hook vanished when the window could not answer it. TaskMan now waits for
  the window to confirm, starts a fresh task manager when it provably hangs,
  and lets further presses reach Windows until the window responds again.
- **Hotkey dead until restart after Windows dropped the hook:** Windows
  silently removes a keyboard hook that answers too slowly even once. Any
  other way of opening the Task Manager (Start menu, taskbar,
  Ctrl+Alt+Del screen) now restores it.
- **GPU rows above the GPU total:** a process rendering on the software
  "Microsoft Basic Render Driver" (WARP/headless renderers, test runners)
  showed up to 100 % GPU while the column header read 1 %. The header and the
  Performance page leave that adapter out when a real GPU is present, but the
  per-process values still counted it. Per-process GPU utilization, engine
  label and GPU memory now cover exactly the adapters the totals come from.
- **GPU total understated under shared load:** an adapter's utilization was
  the busiest single process instead of the busiest engine, so two programs
  sharing the 3D engine at 60 % and 30 % read 60 % instead of 90 %. Engine
  load is now the sum of every process's share of it, matching Windows Task
  Manager; the per-engine graphs on the Performance page follow.
- **Group GPU above the machine total:** app families on the Processes page
  and session/app rollups on the Users page added up their members' GPU
  values, so a game on the 3D engine plus a video on the decoder could read
  130 %. Groups now report their busiest engine like the total does, and
  sorting by a resource column orders app rows by the values they show.
- **Search box lost focus on Left/Right:** moving the caret with the arrow
  keys jumped keyboard focus to the sidebar or a toolbar button, so the box
  dropped its blue frame and caret while still showing the text being
  edited (regression from 2026-09-27). The arrows now move the caret.
- **Search caret missing after keyboard focus:** focusing the search box
  with Ctrl+F, F6 or Tab, or switching back to the window, could leave the
  caret hidden for up to half a second; it now appears immediately.
- **DPC time counted as "Terminated processes":** the "System interrupts"
  row only subtracted interrupt time, so driver DPC load (network, audio,
  storage) showed up as CPU of terminated processes, often with an empty
  exit list. Interrupt and DPC time now come from the same per-processor
  records and window as the CPU residual they are split from.
- **Stale "Terminated processes" value:** a row held visible through quiet
  ticks kept showing the share that armed it (e.g. 35 % for ~4 s after a
  build ended under a 2 % CPU total); it now shows the current share.
- **Network total read too high:** the Processes/Users network header added
  receive + send across all adapters, so a full-duplex link at 60 % each way
  read 100 % and a virtual adapter (Hyper-V vEthernet, VPN) counted its
  physical adapter's traffic twice. It is now the busiest adapter in its
  busiest direction, and reads "—" when no adapter reports a link speed.
- **Loopback traffic in the per-process Network column:** local traffic to
  127.0.0.0/8 or ::1 (a local database or dev server) was counted as network
  traffic although it never reaches an adapter; it is now excluded, also from
  the Users and App history network totals.
- **Disk active time attributed across the wrong disk:** a process's Disk
  active time was its share of the service time summed over every disk,
  scaled by the busiest disk, so a process saturating a slow disk could read
  a few percent next to a busy fast disk, and two processes each saturating
  their own disk read 50 % apiece. Each process now gets its share of the
  disk that served it, scaled by that disk's own active time, and shows its
  busiest disk; app families and Users rows show their busiest disk instead
  of a sum. A reading carried over a very short refresh window now follows
  the disks' current activity instead of keeping its old percentage.
- **GPU engine column named the wrong GPU:** the Details "GPU engine" label
  used the adapter's internal node index, so every GPU read "GPU 0"; it now
  uses the GPU number shown on the Performance page.

## 0.1.20 - 2026-10-06

### Fixed

- **App icons on App History tab:** the App History ("App-Verlauf") page was
  the only table drawing the generic placeholder window icon instead of
  loading real application icons. The table now queries the icon cache using
  the recorded executable path or active process identity, matching the
  Processes, Details, Startup, and Users pages.

## 0.1.19 - 2026-10-06

### Fixed

- **Command line arguments for svchost and Windows services:** `svchost.exe` and
  other Windows background services running in Session 0 (SYSTEM, LocalService,
  NetworkService) frequently showed "—" in the Details Command Line column when
  TaskMan was run unelevated, because interactive processes cannot open those
  service hosts with `PROCESS_QUERY_LIMITED_INFORMATION` to query their process
  parameters. TaskMan now queries the Service Control Manager catalog for
  configured service binary command lines, properly showing arguments (e.g.
  `-k <group> -p -s <service>`) for svchost instances and services across the
  entire system. Search matching on command lines now works for services as well.

### Changed

- **Wi-Fi details are opt-in — no more Windows location prompts:** the SSID
  and signal-strength rows on the Performance network card were read through
  `WlanQueryInterface`, which Windows treats as precise-location data. On
  current Windows 11 that made TaskMan pop the one-time location permission
  prompt (once per build/executable identity) and re-surface in the
  "location in use" tray activity every few seconds, without consent ever
  yielding data. TaskMan now never touches location-gated APIs by default:
  the rows are collected only after enabling "Show Wi-Fi details" in
  Settings (the explicit action at which Windows may show its one-time
  consent prompt), only while the network card is actually on screen — never
  from the tray or another page — and at most once per 30 s. When Windows
  refuses the query, the card says so and links to the Location settings
  instead of silently dropping the rows.

## 0.1.18 - 2026-10-03

### Fixed

- **User name / Elevated for protected processes:** the Details rows of
  boot-critical Windows processes (smss, csrss, wininit, winlogon, services,
  lsass, dwm), kernel pseudo-processes (Registry, Memory Compression, Secure
  System) and the font/session hosts again resolve their owning account
  ("SYSTEM", "DWM-\<session\>", "UMFD-\<session\>") with their SID and
  elevation instead of showing "—" / "Unknown". A spoofed image name still
  earns nothing: only a verified Windows image or the kernel's own
  pseudo-process name is accepted as evidence.
- **Version metadata on localized Windows:** FileDescription / CompanyName are
  read from each file's own version-resource translation table instead of the
  English blocks only. Binaries that ship only a localized table (csrss and
  friends on a German Windows) used to lose their friendly name and company,
  which also disabled the Windows-image evidence behind the system-role
  fallbacks above.
- **Elevated after account inheritance:** a process that inherits a service
  account from a same-image service parent now reports the matching elevation
  and UAC virtualization instead of "Unknown".
- **End process tree killed unrelated processes:** a process whose original
  parent had exited was treated as a child of whatever process later received
  that recycled PID, so ending that process's tree could also end Explorer or
  any other long-running program. Descendants now come from one kernel
  snapshot of PIDs, parent links and creation times, and a "child" older than
  its parent is no longer part of the tree.
- **End process tree reported spurious failures:** children that exited on
  their own while the tree was being ended no longer turn a complete success
  into an error, and a failure to end the selected process itself is now the
  error shown instead of a child's.
- **"Process not found" errors:** actions on a process that has already exited
  report that it no longer exists instead of a raw "The parameter is
  incorrect" platform error.
- **CPU cache sizes:** the Performance CPU page divided every cache by the
  number of threads sharing it, so a Ryzen 7 5700X showed L1 256 KB, L2 2 MB
  and L3 2 MB instead of 512 KB, 4.0 MB and 32.0 MB. Each cache instance now
  counts once at its full size, as in Task Manager.
- **Virtualization row:** reports whether virtualization is enabled in the
  firmware (or a hypervisor is running) instead of whether the CPU merely
  supports it, which showed "Enabled" on machines where it is switched off.
- **GPU, disk and CPU-speed telemetry after hiding the window:** once the
  window had been hidden to the tray (or the consuming page left) for 30 s,
  these counters never came back for the rest of the session and showed "—".
  They now resume when they are needed again.
- **Disk counters for C: on a shared disk:** a volume that shared its physical
  disk with a later drive letter (C: and D: on one SSD) got no active time or
  transfer rates.
- **Process path after PID reuse:** a process whose PID previously belonged to
  an exited process no longer inherits that process's path, description,
  company or saved scheduling rules.
- **Network link speed:** adapters that report an unknown link speed (Wi-Fi
  Direct and some VPN adapters) no longer show an absurd multi-million Tbit/s
  speed or zero out network utilization.
- **Legacy icons on scaled displays:** icons without an alpha channel whose
  width is an odd multiple of 16 px no longer get a sheared transparency mask.
- **Users page "Sign out" froze the window:** choosing Sign out from a user
  row's context menu (or Enter on a row) deadlocked the UI thread.
- **Services page "Start" froze the window:** starting a service from a row's
  context menu deadlocked the UI thread.
- **Suspend/Resume reported success on failure:** suspending or resuming a
  process that refused it (access denied, already exited) now shows the error,
  with the completed count when only part of a selection failed.
- **Efficiency mode and Resume on grouped rows:** on a collapsed group such as
  a browser, the checkmark and "Resume" reflect any member, but the click only
  acted on the group's head process — clearing the checkmark could turn
  efficiency mode ON. The action now applies to every member of the group.
- **Startup impact sorting:** sorting by Startup impact now orders by the
  measured impact the column shows instead of leaving enabled entries in name
  order.
- **Unknown values shown as zero:** the GPU total above the Processes and
  Users tables reads "—" instead of "0 %" while no process has GPU telemetry,
  and an unknown socket or core count on the CPU page reads "—" instead of
  "0" (or a guessed "1").
- **App history inflated by tab switches:** after a tick without per-process
  network or CPU counters (another page shown, or the background service
  missing a tick), the next reading was counted from zero, so every return to
  Processes, Users or App history credited a program's entire traffic and CPU
  time since it started again — and saved it.
- **"Paused" update speed ignored at startup:** with Update speed set to
  Paused, the app came up sampling anyway while the menu said Paused. It now
  starts paused with one initial snapshot.
- **Extra and duplicate samples:** switching pages, hiding or restoring the
  window and similar changes each forced an off-schedule sample, and Refresh
  (F5) sampled twice. Rates from those near-zero windows showed as one-tick
  spikes and dips in graphs and columns.
- **Startup impact undercounted:** an app that started after boot is now
  charged everything it did since it started, not only what happened after
  TaskMan first saw it, so a launcher that does its work in its first second
  is no longer "Low" or "Not measured".
- **Saved GPU graph engine reset on restart:** an engine name containing
  spaces (e.g. "High Priority Compute") is kept instead of reverting to the
  overall graph.
- **Background service stopped under load:** when every connection slot of
  the service's control pipe was in use (a burst of actions, or clients that
  kept their connection open after being refused), the service gave up within
  microseconds and exited until Windows restarted it 5–60 s later. It now
  waits for a slot to free up.
- **Linux priority read back wrong:** a process set to High showed as Above
  normal (so a saved per-program priority stored the wrong class), and
  priority and affinity changes only reached a process's main thread. Both now
  apply to every thread and read back as the class that was set.
- **Linux CPU caches undercounted:** L1 and L2 showed one core's caches (and
  dropped the instruction cache); every distinct cache on every CPU is now
  counted.
- **Linux disks on LVM/LUKS:** volumes mounted through `/dev/mapper` now get
  transfer rates and active time, and the first sample shows active time as
  unknown instead of 0 %.
- **Linux GPUs without telemetry:** cards whose driver publishes no
  utilization (Intel, Nouveau, NVIDIA, simpledrm) are no longer shown as an
  idle GPU at 0 % with 0 B memory.
- **Linux services without systemd:** a system where `systemctl` is unavailable
  reports the error instead of an empty service list, and service
  descriptions are no longer cut short when a unit name contains its state.
- **Linux fonts:** a system font that is not TrueType/OpenType (PCF, Type 1) no
  longer crashes the app at startup; the next candidate is used.
- **macOS suspended processes:** a suspended process now shows as Suspended
  and offers Resume. macOS also reports disk transfer rates, the open file
  handle count, the real host name and each startup item's enabled state.
- **Installed memory with 32 GB modules:** each 32 GB (or larger) RAM module
  was counted as 32 MB, which made "Hardware reserved" read a fabricated 0.
- **Wi-Fi name and signal:** the Performance page's Wi-Fi SSID and signal
  strength never appeared because adapters were matched to WLAN interfaces by
  the wrong GUID.
- **Startup entries stored as expandable strings:** Run entries such as
  Windows Security's showed "N bytes" instead of their command, publisher and
  impact.
- **Commit size of unreadable processes:** shows "—" instead of a fabricated
  "0 K" when no source reports it.
- **GPU after a driver update:** a GPU that got a new adapter identity (driver
  update, device restart, external GPU) is picked up again instead of showing
  0 % until TaskMan restarts.
- **Per-core CPU layout:** on systems where interrupt time is at least DPC
  time, per-core CPU records could be decoded with the wrong layout, and a
  counter running backwards could show a "Terminated processes" row near
  100 %.
- **Services list after a service was installed:** a service registered while
  the list was being read no longer produces an empty Services page.
- **Per-process network and disk frozen at zero:** when the event trace
  behind the Network and Disk activity columns was stopped from outside (by
  another TaskMan instance reclaiming it, or by `logman`), every process kept
  showing 0 as if idle. TaskMan now detects the stopped trace, shows "—", and
  restarts it (immediately in the background service, after a short back-off
  in the app).
- **Background service file logging after setup:** every install made by the
  setup left a `setup.log` in the service's log folder, which made the service
  refuse that folder and silently run without its log file. Setup now logs
  elsewhere and removes the old file.
- **Interrupted installs left no uninstall entry:** setup registers itself in
  Settings > Apps right after installing the background service, so a later
  failure (such as a shortcut that cannot be created) no longer leaves a
  service behind with no way to uninstall it. Running setup from the install
  folder itself also works now.
- **Stale setup artifacts:** a packaging run whose setup embedding or
  verification fails no longer leaves a broken or older `*-setup.exe` in
  `dist/`.
- **Selection jumped to the top after End task:** when the selected process
  exited (End task, or on its own) while the Processes or Details table had
  focus, the selection and the view snapped to the first row. The row that
  takes its place is selected instead, like in Task Manager.

### Improved

- **Per-process identity for unopenable processes:** the LocalSystem service
  answers a bounded, PID+creation-time-bound token identity read (account,
  SID, elevation, UAC virtualization) for the SYSTEM/service processes the
  interactive GUI's own token cannot open at all — the rows that used to be
  the last ones with a blank User name. An unanswered read still renders "—"
  and is retried on a bounded horizon; nothing is ever guessed.
- **Lower sampling cost:** per-process thread counts are read from the kernel
  process table the sampler already queries, instead of a Toolhelp thread
  snapshot that cost about 30 ms of CPU on every tick on a typical desktop.
- **Startup page responsiveness:** resolving each entry's executable and
  reading the last BIOS time now happen when the list is fetched, not for
  every row on every repaint; an entry pointing at an unreachable network path
  no longer stalls the window on mouse movement.
- **Safer settings and history files:** settings, app history and startup
  impact data are flushed to disk before they replace the previous file, so a
  power loss cannot leave an empty file behind. A file that exists but cannot
  be used (corrupt, oversized, unreadable) is kept as `<name>.bad` instead of
  being overwritten by defaults on the next save.
- **Faster Run and native Task Manager launch:** launching a task from the
  Run dialog, and the "open Windows Task Manager" escape hatch, no longer wait
  an extra half second after the program has started.
- **Much lower sampling cost:** each sampling tick re-read the configuration
  of every running Windows service (~36 ms on a typical desktop) and probed
  every process with up to three `OpenProcess` calls. Service configuration
  is now cached and only processes the kernel no longer lists are probed.
- **Lighter repaints on every table page:** Processes, Details, Users,
  Services, Startup and App history measured the text width of every cell of
  the whole list on each repaint, only to support double-clicking a column
  separator. That measurement now runs only on the double-click itself.

### Changed

- **Core service protocol v6:** the identity read above is a wire addition, so
  GUI and service must be upgraded together (setup does exactly that). Against
  an older service, privileged broker calls are refused with "action state is
  unknown" instead of being misapplied, and identity reads answer nothing —
  the affected rows keep "—" until the service is upgraded.

### Security

- **Setup DLL planting:** the elevated setup no longer loads system DLLs (and
  no longer needs the VC++ runtime DLL) from the folder it was started from.
  A DLL planted next to `taskman-setup.exe` in Downloads would previously run
  with administrator rights before the wizard appeared.
- **Setup payload substitution:** setup reads its embedded payload and the
  uninstaller copy through one handle to its own running image, opened and
  verified at startup and locked against changes. Renaming the running setup
  and putting another file at its path after the UAC prompt no longer changes
  what gets installed.
- **Setup log location:** the elevated setup log moved to an
  administrator-only folder (`%SystemRoot%\Logs\TaskMan\setup.log`) and is never
  written through a junction or symbolic link, instead of a folder ordinary
  users can prepare before the first install.
- **Uninstall rollback:** a failed uninstall can only move the uninstaller
  itself back into Program Files; whatever else appears at its temporary
  location is never moved in.
- **Oversized setup payloads:** a manipulated payload entry is rejected as
  soon as it inflates past its declared size instead of being decompressed in
  full first.

## 0.1.17 - 2026-10-02

### Fixed

- **Setup launch access denied:** fixed post-install desktop launch failing with
  `0x80070005` after program files had already been installed. TaskMan still
  launches as the unelevated desktop user.
- **Release artifact names:** host architecture follows the active Rust
  compiler even when the environment omits machine information, preventing
  archives and setup installers with an empty architecture suffix.
- **Windows uninstall:** restores the built-in Task Manager before deleting
  an installed replacement, preserves unrelated and portable replacements,
  waits for the background service to stop, and keeps the uninstall entry
  until program files have been removed.
- **Service-free upgrades:** opting out removes an existing broker before
  updating its binaries, avoiding locked-file failures and an invalid broker
  manifest. Program files use the same protected-copy rules as service installs.
- **Setup user and launch:** broker enrollment and shortcuts use the desktop
  user even when UAC credentials belong to another administrator. Post-install
  launch uses the desktop user's unelevated token and environment; an unavailable
  unelevated token is reported before installation begins.
- **Setup interruption and errors:** window close is blocked while work runs;
  failed steps and worker-start failures are shown and logged. The wizard can
  be resized for longer messages.
- **Disk graphs:** unavailable activity leaves gaps in the graph and resource
  sparkline; hover and keyboard readouts show “—”. Measured idle remains zero.
- **Batch action errors:** bulk End task, efficiency, priority and affinity
  actions preserve failure reasons; partial success reports the completed count
  and first failure instead of success alone.

### Security

- **Installer staging:** elevated helpers are extracted into freshly created,
  administrator/System-only staging under Program Files instead of a predictable
  user temporary directory. Staging is cleaned on both success and failure.

## 0.1.16 - 2026-09-30

### New

- **Windows setup installer (`taskman-setup.exe`):** releases now ship a
  self-contained, 100%-Rust setup executable (modeled on the green-curve
  installer) as `dist/taskman-v<version>-windows-<arch>-setup.exe`. It
  installs Task Manager to the protected `%ProgramFiles%\TaskMan` location,
  optionally registers and starts the background service so the first GUI
  start reaches the broker without a UAC prompt, creates Start menu and
  opt-in desktop shortcuts, registers an uninstall entry in Settings > Apps,
  and upgrades existing installations in place while preserving settings.
  The wizard mirrors the app's Windows 11 dark and light theming (shared
  `tm-ui` theme, product icon, header/footer bands) and reviews the MIT
  license before installing. Running a bare `taskman-setup` build output
  (no embedded payload) is refused up front with guidance to the packaged
  installer, and packaging verifies the produced artifact's embedded payload
  before it reaches `dist/`. Unattended
  use: `/S`, `--uninstall`, `--no-start-menu`, `--desktop`, `--launch`,
  `--no-service`, `--dry-run`. The install directory is fixed (`/D=` is
  rejected with an explanation) because the service is pinned to the
  protected location. The embedded payload is deflate-compressed and SHA-256
  verified before any file is written; uninstall moves the running uninstaller
  out of the install tree instead of leaving files behind.
- **Shared theme crate (`tm-ui`):** the Windows-11-Task-Manager palette and
  OS-native font setup moved out of `tm-app` into a shared crate so the setup
  installer renders from the same theme source as the app. `tm-app` re-exports
  both modules unchanged.

### Changed

- **Core service start type:** the TaskMan Core Service now installs as plain
  "Automatic" instead of "Automatic (Delayed Start)", so the privileged broker
  is ready as soon as the desktop is up and privileged actions work right
  after boot instead of after the delayed-start grace period. Existing
  installations pick this up on the next install/repair: the delayed flag is
  cleared explicitly because updating a service's config never touches it.

### Improved

- **Core service failure visibility:** when the installed TaskMan binaries no
  longer match the pinned broker manifest, the service now reports that under
  its own SCM exit code and logs which binary drifted together with the
  expected/actual hash fingerprints, and Settings › Advanced shows
  "integrity check failed" with the Repair action instead of a bare
  "stopped". Previously this permanent, repairable condition shared one
  generic exit code with every other broker fault — the System event log only
  showed an opaque "service-specific error 1", the real reason sat in an
  admin-only log, and SCM kept restarting the service once per minute
  indefinitely. Ordinary broker failures are unchanged.
- **Setup wizard spacing:** the installer's body content is now inset to match
  the header/footer bands and each option's description is indented under its
  checkbox, so no text hugs the window's rounded edge or reads as truncated /
  overlapping against the surrounding labels.
- **Single launch prompt:** "run Task Manager when setup completes" is now
  offered once, as the Options checkbox. The completion page no longer repeats
  it with a separate "Launch Task Manager" button — the checkbox already starts
  the app at the end of the install.

## 0.1.15 - 2026-09-27

### New

- **Keyboard page switching:** Ctrl+Tab and Ctrl+Shift+Tab cycle through the
  pages (wrapping around), Ctrl+1 through Ctrl+9 jump directly to a page.
  While a dialog is open it keeps keyboard ownership, so the shortcuts stand
  down — the same gate the Delete shortcut uses.
- **Run new task and Settings shortcuts:** Ctrl+N opens the "Run new task"
  dialog and Ctrl+, opens the Settings dialog from anywhere in the application,
  matching native Windows 11 Task Manager shortcuts. Both are documented in
  the F1 shortcut help overlay.
- **F1 shortcut help:** F1 toggles a German/English overlay listing every
  keyboard shortcut; Esc closes it. The same list is documented in a new
  "Keyboard shortcuts" section in the README.
- **Keyboard navigation for the remaining lists:** the Services, Startup,
  Users and App history rows and the Performance resource cards follow the
  arrow keys plus Home/End/PageUp/PageDown to move the selection. Enter
  opens the row menu on Services, Startup and Users, and the Menu key or
  Shift+F10 opens the selection's context menu. App history gains a visible
  row selection.
- **F6 focus regions:** F6 and Shift+F6 step through the search box, the
  sidebar, the current page's command buttons and the page content, so the
  whole window is reachable without hunting for the next Tab stop. The
  Performance tab has no search box and is skipped.
- **Keyboard-scrubbable charts:** a focused Performance graph walks its
  samples with Left/Right and jumps to the oldest or newest with Home/End.
  Up/Down moves between the series of a multi-series graph, and between the
  logical processor charts in the per-core CPU grid. The focused chart paints
  a focus ring and shows the same hover readout the mouse produces, pinned to
  the selected sample.
- **Reorderable columns from the keyboard:** Alt+Left/Alt+Right on a focused
  column header moves a movable column one position, matching the mouse drag.
- **Resizable Performance card column from the keyboard:** the splitter
  between the resource cards and the detail area is a focusable stop that
  takes Left/Right in 8 px steps (Shift for 32 px), reported to assistive
  technology with its current width.
- **Screen reader support:** tables, the Performance card list, the charts and
  the dialogs expose a structured accessibility tree — list/list-item roles
  with set position and size, labelled buttons and splitters, and the
  keyboard-selected chart sample published as the chart's value.

### Improved

- **Tables follow the native Task Manager model:** table rows are no longer
  Tab stops, so the arrow keys move the visible selection from anywhere on
  the page instead of dead-ending on a focused row. Shift+arrows extend the
  multi-selection (previously unreachable), Ctrl+arrows move it without
  changing it, Space toggles the primary row in and out of the selection,
  Enter runs the primary row action (Processes: Go to details; Details:
  Process Properties), and PgUp/PgDn page by the visible table span.
- **Cyclic keyboard navigation:** Pressing Tab from an active list navigates
  forward into enabled toolbar buttons and table column headers, while
  Shift+Tab returns directly to the search bar. In the Performance tab,
  Shift+F10 and the Menu key open graph context menus from the keyboard.
- **Keyboard navigation flow and focus continuity:** Audited and perfected
  keyboard-only navigation across the shell and tables. Tab and Shift+Tab form
  an unbroken cycle across the search bar, sidebar, command toolbar, table
  headers, and table content with no focus traps or dead ends when toolbar
  buttons are empty or disabled. Active vs. inactive row selections match
  Windows 11 Task Manager visuals (vibrant accent fill with vertical accent
  indicator pill when table content holds focus, dimmed fill without pill when
  unfocused).
- **List navigation and type-ahead:** Clamped navigation indices when lists
  shrink to prevent out-of-bounds selection jumps; PageUp without an active
  selection navigates from the bottom of the list. Switching tabs or committing
  search on Users and App History tabs automatically selects the first matching
  row in live display order and scrolls it into view. Added type-ahead search
  support across Services, Startup, Users, and App History.
- **Visible keyboard focus:** sidebar entries, header and command buttons,
  the search box with its clear button, table header cells and the toast
  close buttons paint a focus ring while focused. Disabled command buttons
  leave the Tab order entirely, and the titlebar drag regions no longer sit
  in it as invisible stops.
- **Keyboard column control:** a focused header cell sorts with Enter or
  Space, and the Menu key opens its column menu — the Details column chooser
  is reachable without the mouse. A focused column resize handle adjusts the
  width with Left/Right (Shift for larger steps).
- **Dialog keyboard model:** Settings tabs through its controls with an
  anchored initial focus and scrolls freshly focused controls into view, and
  Enter closes a dialog only while no activatable control is focused.
  Closing a dialog returns focus to the widget that opened it. The affinity
  dialog's CPU checkboxes become keyboard-reachable (its button row follows
  real focus instead of pinning it), the destructive confirms keep their
  safe-button Tab trap, and with a dialog open the pages' table navigation
  (arrows, type-ahead, Enter/Space, context-menu key) stands down.
- **Context menus from the keyboard:** Tab closes an open menu instead of
  stranding it, ArrowRight/ArrowLeft open and close submenus, and the Menu
  key now works while any non-text widget holds focus. Closing a menu returns
  focus to the control that opened it.
- **Dialogs keep focus inside:** an open dialog holds keyboard focus and
  pointer interaction for itself — Tab cycles only its own controls and the
  page behind it no longer reacts to clicks or stray keys while it is up.
- **App History on Linux and macOS:** non-system processes now accumulate
  local usage history even where window ownership cannot be determined.

### Fixed

- **Context menu keyboard focus and arrow navigation:** Fixed context menus opened via the
  keyboard Menu key or Shift+F10 failing to receive keyboard focus or respond to arrow keys.
  The first enabled item now receives focus immediately upon opening, ArrowUp/ArrowDown wrap
  circularly skipping disabled items, Home/End jump to the start/end, and directional keys are
  locked within the popup to prevent focus from escaping into background table headers or rows.
- **Keyboard navigation visual selection flicker:** Eliminated 1-frame visual
  anomalies where unintended UI elements briefly flashed as focused or selected
  before focus/selection moved to the intended element. Patched `set_focus_lock_filter`
  in egui so mid-frame focus handoffs immediately install event filters without
  requiring prior-frame history, processed sidebar Settings navigation keys before
  the tabs loop renders, and installed directional/tab event filters on command
  toolbar buttons, table headers, and the global search box.
- **Sidebar arrow navigation jumping:** Resolved unexpected jumping and overshooting
  when pressing ArrowUp or ArrowDown in the sidebar. Installed strict directional focus
  locks (`EventFilter`) and explicit focus direction clearing on all sidebar items
  (hamburger toggle, tabs, and settings) so egui's default spatial navigation never
  interferes or causes double jumps. ArrowDown and ArrowUp now navigate sequentially
  one item at a time across the entire column with clean boundary stops at Hamburger
  and Settings, while Tab and Shift+Tab smoothly cycle into and out of the sidebar
  preserving the currently active tab.
- **Selection focus indicators:** Removed the whole-section accent border that
  framed the entire table viewport (Processes, Details, Services, etc.) or card
  column (Performance) when focused. Focus and selection are now indicated
  solely by the clicked/focused row or resource card itself, matching native
  Task Manager behavior.
- **Process tree collapse selection warp:** Fixed a defect where pressing
  Left Arrow on an aggregate child row collapsed the parent without moving the
  stored selection identity, leaving an orphaned selection on a hidden child.
  On the subsequent keystroke, the table failed to resolve the selected row and
  warped selection to row 0. Left Arrow on a child row now transfers selection
  directly to the parent.
- **Sidebar and toolbar Tab dead ends:** Fixed focus traps where tabbing from
  Settings or shifting tab from table headers stranded focus when no toolbar
  command buttons were active or enabled. Focus now smoothly falls through to
  headers or content.
- **Search clear button Tab stop:** Changed the clear search button sense to
  prevent it from capturing a redundant Tab stop inside the search box.

- **Tab navigation to sidebar tabs and tab switching on focus:** Pressing Tab from
  an active list or table now reliably navigates into the sidebar navigation pane,
  focusing the active page tab directly. Navigating through the sidebar tabs via
  Tab, Shift+Tab, or ArrowUp/ArrowDown automatically activates and switches to that
  page, and pressing Enter, ArrowRight, or Escape surrenders focus into the active
  page content. When the search field has no text, Tab flows forward into the sidebar
  tabs instead of getting trapped in the list, while non-empty search queries continue
  to commit and jump directly to the matching row on Tab or Down Arrow. Shift+Tab from
  the list now returns to the search bar across all tabs including Performance.
- **Concurrent sidebar and list input conflict:** Keystrokes such as Tab or
  ArrowDown/ArrowUp no longer register simultaneously in the sidebar and the
  process/details table list. List navigation gates strictly stand down when
  any chrome widget (sidebar item, hamburger button, toolbar button, search
  input) holds keyboard focus. Committing search with Tab or Down Arrow
  intercepts the key before text edit focus traversal, preventing egui from
  leaking focus to the sidebar hamburger button. Sidebar items and toolbar
  buttons now reliably surrender focus directly to the table rows upon
  pressing ArrowRight, ArrowDown, or Escape.
- **Search bar Tab jump to list:** Pressing Tab or Down Arrow from the search
  field commits the search match and immediately hands keyboard focus to the
  table rows, parking the selection into view. The clear ('X') button is
  excluded from the Tab cycle so keyboard navigation never gets stranded in
  the search box chrome.
- **Table header focus escape:** Pressing Down Arrow or Escape on a focused
  table column header or resize handle surrenders focus back to table rows,
  preventing keyboard navigation from getting locked in the header.
- **Core-service upgrades:** replacing a running service now waits for SCM to
  report it stopped without requiring access to the LocalSystem process
  handle. A failed repair or switch also re-enables its Settings control.
- **Wrong executable identity:** unreadable Windows processes no longer borrow
  another same-named process's path, publisher, or saved scheduling rules;
  their path stays unknown unless a PID-bound source resolves it.
- **App History loss and overcounting:** a slow database load can no longer be
  overwritten by autosave; new observations merge after it completes. The
  first sighting of a busy process no longer adds an invented interval of CPU
  or network usage.
- **Misidentified service helpers:** Session 0 or a familiar process name
  alone no longer labels an unreadable process as SYSTEM or assigns it
  SYSTEM's SID.
- **Stalled or crowded telemetry:** a core-service telemetry request has a
  bounded wait, so a silent broker cannot freeze the sampling engine. A
  response with more active processes than its frame can hold reports
  unavailable rather than false zero readings.
- **Linux release fallback:** an installed `cargo-zigbuild` without the `zig`
  executable now selects the available self-contained musl build.
- **Search bar held focus hostage:** committing a search with Enter left the
  field focused, so the arrow keys could not move the row selection it had
  just set up. Enter now commits the jump to the first match and releases
  the field (without also firing the row action on the same keypress), a
  click outside the field releases it, and Esc clears the search globally
  whenever no dialog or menu is open — not only while the field is focused.

## 0.1.14 - 2026-09-23

### Fixed

- **Ctrl+Shift+Esc dead while TaskMan was unresponsive:** if TaskMan's window
  stopped responding, the next Ctrl+Shift+Esc left the thread that handles
  the hotkey waiting on it indefinitely. From then on every press was
  swallowed without effect — Explorer never got to start a fresh task
  manager — and turning the Task Manager replacement off no longer removed
  the keyboard hook. The hotkey thread no longer waits on the window, and the
  combo now passes through to Windows whenever TaskMan cannot answer it
  (still starting up, or still busy with the previous press), so a new
  instance opens instead.
- **Game input stalled while TaskMan took the foreground:** raising TaskMan
  over a fullscreen game briefly shared the game's input queue with
  TaskMan's own UI thread, so a slow TaskMan frame delayed the game's input
  until the switch completed. Only the foreground thread is shared now, and
  only for the single activation call.
- **Escape stuck after releasing Ctrl+Shift first:** holding Escape after
  letting go of Ctrl+Shift sent auto-repeat Escape presses to the focused app
  but still swallowed the release, leaving Escape held down there.
- **White flash when Ctrl+Shift+Esc restored TaskMan from the tray:** the
  hotkey path showed the window before it had painted, bypassing the cloak
  the tray restore uses; the window is now shown only by its own UI thread.
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
- **Empty StartupApproved registry value rendered as disabled:** `list_startup`
  treated an empty `StartupApproved` value as disabled because `data.first()`
  was `None`, even though Windows considers an empty entry enabled and runs it
  at logon. Empty entries now correctly report enabled.
- **Kernel ETW sessions outliving an application crash:** ETW disk and network
  trace sessions are kernel objects that survived application aborts
  (`panic = "abort"`) because cleanup was `Drop`-only. An unhandled abort now
  runs emergency teardown (`stop_live_sessions`) before the process exits so
  orphaned kernel traces do not continue tracing machine-wide I/O.
- **Log files truncated to a single line and crash diagnostics lost:** the
  file logging `WorkerGuard` was dropped immediately after startup check in
  `main.rs`, silently discarding subsequent logs, and normal GUI launches did
  not attach file logging. The logging worker guard now lives for the process
  lifetime, file logging is attached on the engine frame, and fatal panics
  synchronously flush a `.crash` log with thread backtraces.

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
- **Live-kernel verification tool:** added `python build.py --live-tests` to
  validate elevated NT kernel structure offsets and ETW payload decoders
  against live events.
