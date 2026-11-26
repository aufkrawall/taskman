# Linux Elevated Core Service and Desktop Integration

Last cross-checked: 2026-11-26

Primary sources:

- `crates/tm-platform/src/linux_core_service.rs` (wire protocol, manifest, peer policy)
- `crates/tm-platform/src/linux_sensors.rs` (sensor reading types and conversions)
- `crates/tm-platform/src/linux/core_service/` (root-side server, GUI-side client)
- `crates/tm-platform/src/linux/sensors.rs` (sysfs + NVML collection)
- `crates/tm-service/src/main.rs` (the `taskman-service` binary)
- `crates/tm-app/src/main.rs` (tray lifecycle), `crates/tm-app/src/tray/`
- `packaging/linux/`, `build.py`

## Why a service on Linux

The Windows backend already splits the product in two: an interactive GUI
renders and samples, and a LocalSystem service owns a narrow allowlist of
privileged actions and root-only evidence (`core-service.md`). The Linux
backend had only per-action `pkexec` elevation for `systemctl`, and every
process tab read `/proc/<pid>` under the GUI's own token, so another user's
process showed unavailable I/O, modules, connections and threads.

The Linux core service closes that gap with the same shape:

- `taskman-service` runs as a systemd system unit (`taskman-core.service`) as
  root. It serves one Unix socket, authorizes each peer from kernel-reported
  credentials plus a root-owned manifest, and dispatches a fixed allowlist.
- The GUI stays unelevated. When a request needs more privilege than the GUI
  token has, it asks the broker first; if the broker is absent it may try the
  operation under its own token. An explicit broker rejection NEVER falls back
  (a safety decision must not become permission), exactly like Windows.
- The service never samples the machine on its own schedule and never keeps
  GUI state. Its only shared state is the cached sensor snapshot the GUI asks
  for.

## Trust boundary

1. The socket is `/run/taskman/core.sock` in a root-owned directory; the file
   is root-owned, mode `0666` (ordinary users must be able to connect), and
   the broker rejects a socket that is writable by a non-root user.
2. Peer identity is `SO_PEERCRED` — the kernel's pid/uid/gid of the connected
   socket — never a client-sent field.
3. `/etc/taskman/core-service.json` is a root-owned, not-group/world-writable
   manifest pinning the protocol version, the authorized uids and the
   installed client executables. The service validates it before listening and
   refuses to start when it cannot be proven safe.
4. Unless `allow_user_writable_client` is set (development only; the packaged
   installer never sets it), the peer's `/proc/<pid>/exe` must be a manifest
   client path, owned by root and not writable by group or others.
5. uid 0 is always accepted: it already possesses everything the broker can do.
6. The service binary must live below `/usr/lib/taskman` and be root-owned and
   non-writable. A user-writable image is never executed as root.

## Protocol

`linux_core_service.rs` owns the contract; it compiles on every host and its
authorization decisions are unit-tested there.

- Frame: big-endian `u32` body length + JSON body. Requests are capped at
  64 KiB, responses at 2 MiB. `RequestEnvelope { protocol, request }` and
  `ResponseEnvelope { protocol, response }` make a version mismatch an explicit
  error instead of a misread field.
- Requests (`Request`): `Status`, `Sensors`, `ProcessModules`,
  `ProcessThreads`, `ProcessConnections`, `ProcessEvidence`, `Action`.
- Actions (`ActionRequest`): `kill` (optionally the descendant tree),
  `suspend`, `resume`, `set-priority`, `set-affinity`, `service` (`start`,
  `stop`, `restart`). Every process action carries `start_ticks`, the kernel
  creation identity, and the service re-reads `/proc/<pid>/stat` before acting
  so a recycled pid cannot address a different process.
- Refusals are typed (`Refusal`): `unauthorized-user`, `missing-client-image`,
  `client-not-listed`, `unprotected-client`, `protocol-mismatch`,
  `malformed-request`, `unsupported`, `not-found`, `rejected`, `failed`,
  `busy`. Only transport failure (no socket, timeout) allows a fallback.

## Sensors

`linux_sensors.rs` holds the reading model and every conversion; sysfs and
NVML access lives in `linux/sensors.rs`.

- hwmon: every `/sys/class/hwmon/hwmonN` chip, every `temp*/fan*/in*/curr*/
  power*/energy*/humidity*` input plus its `_max`/`_crit`/`_alarm` siblings.
  Values are scaled to °C/RPM/V/A/W/J and range-checked, so a driver sentinel
  reads as unavailable, never as a plausible lie.
- thermal zones and the RAPL powercap tree (energy counters are root-only on
  current kernels; the unelevated sampler reports them absent instead of 0).
- NVML: `libnvidia-ml.so.1` is loaded dynamically (no build-time dependency),
  giving the fields sysfs does not publish for the proprietary driver —
  utilization, VRAM use, die/memory temperature, fan, power, clocks, PCIe
  link. This is what makes NVIDIA telemetry work at all; it works unelevated
  when `/dev/nvidiactl` is readable (the `video` group), and the root service
  supplies the same readings when it is not.
- `drm.rs` joins NVML readings to DRM cards by normalized PCI slot and keeps
  `util_pct_available`/`dedicated_mem_available` honest when no source exists.

## Desktop integration

- Tray: a `StatusNotifierItem` at `/StatusNotifierItem` plus
  `com.canonical.dbusmenu` at `/MenuBar` on the session bus (pure-Rust `zbus`,
  no GTK/libappindicator at build time). Three items — Open / Settings / Exit —
  and a left click that activates Open. The served `Status` is `Active` or
  `Passive` following the requested visibility, and the UI thread only stores
  the desire and wakes the tray thread, which owns the connection and emits
  `NewStatus`.
- The tray lifecycle is the Windows one: the window is hidden only once the
  icon is registered, `--minimized-to-tray` falls back to showing the window
  when there is no icon, and a tray click restores it.
- Wayland cannot hide a window (winit ignores `ViewportCommand::Visible(false)`
  and refuses to un-minimize), so the backend is detected once from the
  window's display handle with `WAYLAND_DISPLAY` as the fallback, and a Wayland
  session refuses to close-to-tray rather than leaving the window on screen
  while the app believes it is hidden.
- A panel restart replaces the watcher; the tray thread records the watcher's
  unique owner and re-registers on every 30 s tick when it changed or vanished.
  A failed re-registration stores `FAILED` and wakes the UI, so an already
  hidden window is restored instead of becoming unreachable.
- GNOME without the AppIndicator extension has no watcher. Registration fails
  once with a single explanatory log line, the state becomes `FAILED`, and the
  window stays visible.
- Install layout (`packaging/linux/install-linux.sh`):
  - `/usr/bin/taskman` (the GUI)
  - `/usr/lib/taskman/taskman-service` (root:root 0755) and the two support
    files the scriptlet needs: `install-linux.sh` and `core-service.json.in`
  - `/usr/lib/systemd/system/taskman-core.service`
  - `/etc/taskman/core-service.json` (root:root 0644) — a pacman `backup`
    file, so an upgrade keeps the live policy instead of replacing it; the
    new placeholder arrives as `.pacnew` and is removed after the merge
  - `/usr/share/applications/io.github.aufkrawall.Taskman.desktop`
  - `/usr/share/icons/hicolor/scalable/apps/io.github.aufkrawall.Taskman.svg`
    (one scalable icon: both KDE and GNOME's icon themes load SVG, and it
    needs no raster size table; `*.png` stays gitignored in this repository)
  - `/usr/share/metainfo/io.github.aufkrawall.Taskman.metainfo.xml`
  - `/run/taskman` via the unit's `RuntimeDirectory=`, and `/var/log/taskman`
    via `LogsDirectory=` (the unit runs with `ProtectSystem=strict`, so the
    service cannot create that directory itself)
- Arch package: `packaging/arch/PKGBUILD` + `taskman.install`. `package()`
  runs the installer into `$pkgdir`, so the package contents and the tarball
  install cannot drift; the `.install` scriptlet refreshes the desktop/icon
  caches and enables the unit. With no known invoker (a root login, a chroot)
  it warns, leaves the unit disabled and names the one command that fixes it —
  a scriptlet must never fail the pacman transaction, and enabling a unit with
  an empty policy would only burn its start limit.
- `build.py` ships the packaging tree with the Linux artifact and can build
  the Arch package when the packaging tools exist; a missing tool is a skip,
  not a failure. The musl artifact stays headless: it carries the two binaries
  and the README, no desktop entry and no unit.
- A Windows host without `cross`/`zig` can only produce that headless
  fallback, so `packaging/linux/build-artifact.sh` builds the GUI-capable
  glibc tarball natively on any Linux host (desktop, WSL2, CI container) with
  the same layout, and `build.py` keeps such an archive in `dist/` when it
  rebuilds only musl. The Arch package is the other GUI-capable path.
- CI runs the Linux half in two extra jobs (`.github/workflows/ci.yml`): the
  tests/clippy/scripts/artifact job, and an `archlinux:latest` job that diffs
  `makepkg --printsrcinfo` against `.SRCINFO` and validates the unit and the
  desktop entry. The Windows gate can type-check Linux code but cannot execute
  any of it — a broker that refused every start and two Linux-only test
  fixtures all passed it.

## Invariants

- Missing telemetry renders as unavailable, never as a fabricated zero.
- Normal GUI startup never elevates and never requires UAC/polkit.
- The service never runs from a user-writable directory.
- A broker rejection never degrades into an unelevated retry.
- One connection-level error never stops the broker.
- No sleeps as race fixes; readiness is event-driven with deadlines.
