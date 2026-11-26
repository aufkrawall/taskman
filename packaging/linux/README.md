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

No PNG is shipped: the icon is vector-only, so every icon theme size is derived
from the one SVG. `*.png` is gitignored repo-wide, and a generated raster would
be a committed artifact nobody rebuilds.

## Installing from the release tarball

```sh
tar xzf taskman-v0.1.34-linux-x86_64.tar.gz -C taskman-0.1.34
sudo taskman-0.1.34/install-linux.sh
```

The tarball keeps the two binaries under `bin/` and this packaging's files next
to the script, which is the layout the installer expects by default.

The installer refuses to change anything unless `systemctl` exists **and**
`/run/systemd/system` is a directory (containers without systemd), the binaries
exist and are executable, `ExecStart=` in the unit matches the binary path it
installs, and the account named by `--user`/`SUDO_USER` exists. It never writes
a service image or a manifest that a non-root user could modify.

## How the service is authorized

`/etc/taskman/core-service.json` is root-owned (0644) and lists:

- `protocol` — the wire version (`1`); a GUI and a service from different
  releases fail at the handshake instead of misreading each other.
- `authorized_uids` — the local accounts allowed to connect. The installer
  records the user that ran it. On a machine with several desktop users, run
  the installer once per user (or `--user <name>`): an existing list is
  **extended**, never replaced, and the installer says which uids it kept.
- `clients` — `/usr/bin/taskman`. The broker resolves the peer's
  `/proc/<pid>/exe` and requires that exact root-owned, non-writable image, so
  an authorized user cannot hand the broker's capabilities to a binary they can
  write.
- `socket` — `/run/taskman/core.sock`, and `sensor_cache_ms` — 500, the minimum
  age of the cached sensor snapshot a burst of GUI requests shares.

`--dev` is the one exception: it sets `allow_user_writable_client` and records
the image the developer actually built. A package never ships it.

The service runs as root under the unit's hardening (`ProtectSystem=strict`,
`NoNewPrivileges=yes`, a `CapabilityBoundingSet` of exactly the five
capabilities the action allowlist needs) and only holds the socket plus an
in-memory sensor cache. Stopping it costs privilege, not correctness: the GUI
keeps working and reports the affected columns as unavailable.

## Checking and removing

```sh
systemctl is-active taskman-core.service
journalctl -u taskman-core.service -n 50
sudo /usr/lib/taskman/install-linux.sh --uninstall   # or uninstall-linux.sh
```

`--uninstall` stops and disables the unit, refreshes the caches and removes
exactly the files in the table above. The GUI's own settings
(`~/.config/taskman`) are left alone.

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
files.

`python build.py --arch-package` runs that `makepkg` for you and copies the
result into `dist/`; it skips with a note when `makepkg` is not installed
(pacman is Arch-only, so a missing build tool is never a build failure).

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
