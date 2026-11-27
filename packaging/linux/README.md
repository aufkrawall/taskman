# Linux packaging

Everything the Linux artifact and the Arch package install, and the one
installer that owns the layout. The design contract is
[`llm-wiki/linux-core-service.md`](../../llm-wiki/linux-core-service.md).

## What gets installed where

| Path | Mode | Owner | Purpose |
| --- | --- | --- | --- |
| `/usr/bin/taskman` | 0755 | root:root | the GUI; runs as an ordinary user |
| `/usr/lib/taskman/taskman-service` | 0755 | root:root | the protected core service image |
| `/usr/lib/taskman/install-linux.sh` | 0755 | root:root | this packaging's installer, kept for `--post-install` / `--uninstall` |
| `/usr/lib/taskman/core-service.json.in` | 0644 | root:root | manifest template the scriptlet renders |
| `/usr/lib/systemd/system/taskman-core.service` | 0644 | root:root | the system unit |
| `/etc/taskman/core-service.json` | 0644 | root:root | authorization policy the service validates before it listens |
| `/usr/share/applications/io.github.aufkrawall.Taskman.desktop` | 0644 | root:root | application menu entry |
| `/usr/share/icons/hicolor/scalable/apps/io.github.aufkrawall.Taskman.svg` | 0644 | root:root | scalable icon |
| `/usr/share/metainfo/io.github.aufkrawall.Taskman.metainfo.xml` | 0644 | root:root | AppStream metadata |
| `/run/taskman/core.sock` | 0666 | root:root | socket, created by the service in the unit's `RuntimeDirectory=` |
| `/var/log/taskman/` | 0755 | root:root | the broker's log directory, created by the unit's `LogsDirectory=` |

No PNG is shipped: the icon is vector-only, so every icon theme size is derived
from the one SVG. `*.png` is gitignored repo-wide, and a generated raster would
be a committed artifact nobody rebuilds.

## The artifact needs the broker to build

Every Linux artifact ships `bin/taskman-service`, and `build.py` refuses to
package one without it: an installer that stages a missing broker would leave a
GUI talking to nothing. The shipped binary is the real service — it serves the
socket the unit starts it for — and the packaging test (build, `pacman -U`,
`systemctl is-active`, `taskman-service --selfcheck`) is what proves it.

The static musl fallback artifact is different on purpose: it has no `dlopen`, so
winit cannot load Wayland/X11 and the GUI cannot start on a glibc desktop. It
therefore carries only `bin/taskman`, `bin/taskman-service` and this README — no
installer, unit, desktop entry or icon — because an install surface for a GUI
that cannot run would be a promise the artifact does not keep. `build.py` keeps
an existing GUI-capable archive in `dist/` when it rebuilds only that fallback:
a host without a linker for glibc cannot produce the better artifact, so it must
not delete it.

## Building a GUI-capable artifact on Linux

`build.py` cross-builds from Windows and needs `cross` or `cargo-zigbuild` plus
`zig` to link a glibc binary; without them it can only produce the musl
fallback. On a glibc Linux host (a desktop, a WSL2 distro, a CI container) the
same artifact is a plain native build, and
[`build-artifact.sh`](build-artifact.sh) produces exactly the layout `build.py`
does:

```sh
packaging/linux/build-artifact.sh --verify   # writes dist/taskman-v<version>-linux-x86_64.tar.gz
```

It refuses to run on a static (musl) target, refuses to package a statically
linked GUI, writes the archive deterministically (sorted names, owner 0/0), and
with `--verify` runs the freshly built GUI's headless `--selfcheck --mock`
first. The Arch package below is the other GUI-capable path, and the two are
interchangeable: both end in the same `install-linux.sh` layout.


## Installing from the release tarball

The archives have no top-level directory, so extract into one:

```sh
mkdir -p taskman-0.1.36
tar xzf taskman-v0.1.36-linux-x86_64.tar.gz -C taskman-0.1.36
sudo taskman-0.1.36/install-linux.sh
```

The tarball keeps the two binaries under `bin/` and this packaging's files next
to the script, which is the layout the installer expects by default.

The installer refuses to change anything unless `systemctl` exists **and**
`/run/systemd/system` is a directory (containers without systemd), the binaries
exist and are executable, every packaging file it installs is next to it,
`ExecStart=` in the unit matches the binary path it installs, and the account
named by `--user`/`SUDO_USER` exists. It never writes a service image or a
manifest that a non-root user could modify. `--post-install` (the pacman
scriptlet) never fails a transaction: it returns 0 without touching the unit
when systemd is not the running init system (pacstrap, `arch-chroot`, container
builds) or when it cannot determine who to authorize, and in the latter case it
prints the command that authorizes a user, enables and starts the unit.

## How the service is authorized

`/etc/taskman/core-service.json` is root-owned (0644) and lists:

- `protocol` — the wire version (`1`); a GUI and a service from different
  releases fail at the handshake instead of misreading each other.
- `authorized_uids` — the local accounts allowed to connect. The installer
  records the user that ran it. On a machine with several desktop users, run
  the installer once per user (or `--user <name>`): an existing list is
  **extended**, never replaced, and the installer says which uids it kept.
  The list is read by structure, not by a fixed layout, so a hand-formatted
  multi-line array is understood; every entry must be a plain decimal uid in
  `1`..`4294967295` (a `-1`, a `0` or a value the service's `Vec<u32>` would
  reject makes the installer treat the file as foreign instead of guessing).
- `clients` — `/usr/bin/taskman`. The broker resolves the peer's
  `/proc/<pid>/exe` to a device and inode at startup and requires that exact
  root-owned, non-writable image, so an authorized user cannot hand the broker's
  capabilities to a binary they can write.
- `socket` — `/run/taskman/core.sock`, and `sensor_cache_ms` — 500, the minimum
  age of the cached sensor snapshot a burst of GUI requests shares.

The uid list is **replaced instead of extended** in exactly two cases, and the
installer says so each time:

1. the installed file is not a manifest this installer wrote (no
   `authorized_uids` key, an unparsable one, or an entry outside the uid range
   above) — the old file is kept as `core-service.json.bak`;
2. `--uninstall` removed the policy, so the next install starts from scratch.

A package upgrade is not one of them. `/etc/taskman/core-service.json` is in the
package's `backup=()`, so pacman leaves the live file alone and writes the
packaged placeholder beside it as `core-service.json.pacnew`; `--post-install`
then merges from the live file and deletes that placeholder, so nothing is
rewritten and no unconfigured copy is left behind. The same deletion happens
when the scriptlet decides the installed policy must stand because no invoking
user could be determined.

`--dev` is the one exception: it sets `allow_user_writable_client` and records
the image the developer actually built. A package never ships it.

The service runs as root under the unit's hardening (`ProtectSystem=strict`,
`NoNewPrivileges=yes`, a `CapabilityBoundingSet` of exactly the five
capabilities the action allowlist needs) and only holds the socket plus an
in-memory sensor cache. `ProtectSystem=strict` makes `/var/log` read-only, so
the unit's `LogsDirectory=taskman` is what keeps the broker's file logger from
degrading to nothing. Stopping the service costs privilege, not correctness: the
GUI keeps working and reports the affected columns as unavailable.

## Checking and removing

```sh
systemctl is-active taskman-core.service
journalctl -u taskman-core.service -n 50
sudo /usr/lib/taskman/install-linux.sh --uninstall   # or uninstall-linux.sh
```

`--uninstall` stops and disables the unit, refreshes the caches and removes
exactly the files in the table above, including the `core-service.json.bak` a
replacement may have left. The GUI's own settings (`~/.config/taskman`) are left
alone.

A reinstall clears a failed start limit first (`systemctl reset-failed`), so a
unit that crash-looped on a broken manifest actually comes back instead of
staying failed while the install reports success.

## Arch package

`packaging/arch/` builds the same payload as a pacman package. `makepkg` runs
**from `packaging/arch` against this repository checkout** — the PKGBUILD has no
`sources` array and no network access; `build()` compiles the two release
binaries from the checkout, and `package()` invokes this packaging's
`install-linux.sh --packaged --root "$pkgdir"` so package contents cannot drift
from the tarball install.

```sh
cd packaging/arch
makepkg -si
```

`taskman.install` is the scriptlet: `post_install`/`post_upgrade` call
`/usr/lib/taskman/install-linux.sh --post-install` (systemd reload, enable and
start, desktop/icon cache refresh, manifest written for the user who ran
pacman), and `pre_remove` stops and disables the unit before pacman deletes the
files. The scriptlet cannot fail a transaction. In a chroot, `pacstrap` or
container install, where systemd is not the running init system, it says so and
returns without touching the unit. With no invoking user to authorize it warns,
leaves the unit **disabled** (a unit whose manifest lists no uid would fail on
every start), and prints
`/usr/lib/taskman/install-linux.sh --user <name>` plus the `systemctl enable` /
`systemctl start` pair for doing it by hand.

`/etc/taskman/core-service.json` is listed in `backup=`, so pacman leaves a
working policy alone and writes the packaged placeholder beside it as `.pacnew`;
`--post-install` merges from the live file and deletes that placeholder, also
when it decided the installed policy should stand. `options` carries `!strip`
because the release profile deliberately keeps the symbol table for crash
records.

`python build.py` builds that package as part of every run and points
`makepkg`'s `PKGDEST` at `dist/`. On an Arch host it is the installed
`makepkg`. Windows has no pacman at all, so there it uses the first Arch WSL
distro that has `makepkg` — set `TASKMAN_ARCH_WSL_DISTRO` to name one instead
of searching. The build happens inside that distro, so a first run compiles
the Linux binaries a second time there; its build tree lives in
`/var/tmp/taskman-arch` inside the distro rather than in `packaging/arch/src`,
which keeps gigabytes off the checkout, survives a distro restart so a rebuild
only relinks, and leaves the checkout free to be mounted read-only there.
`--no-arch-package` skips the step, and
`--no-package`/`--host-only` skip it with the rest. A host with neither
`makepkg` nor an Arch WSL distro is a logged skip, never a failed build
(pacman is Arch-only). Before building it checks that the PKGBUILD's `pkgver`
is the version this run built and that `.SRCINFO` is exactly what
`makepkg --printsrcinfo` generates; the fix for a mismatch is printed as
`cd packaging/arch && makepkg --printsrcinfo > .SRCINFO`.

## Development install

```sh
python build.py --host-only          # or --linux-only
sudo bash ./packaging/linux/install-linux.sh --dev --source-dir target/release
sudo bash ./packaging/linux/install-linux.sh --uninstall
```

`bash` because the checkout keeps the script at the repository's 0644 script
mode; the installed copy and the tarball entry are 0755.

`--dev` accepts any binary of the authorized user and records the image it
installed, so a rebuild does not need a manifest change. Never use it on a
machine you care about: it lets any binary that user can write act with the
broker's privileges.
