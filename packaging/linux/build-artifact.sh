#!/usr/bin/env bash
#
# Build the GUI-capable Linux install tarball on a Linux host.
#
# `build.py` cross-builds the Linux artifact from Windows, but a glibc link
# needs `cross` or `cargo-zigbuild` plus `zig`. Without either, build.py can
# only produce the static musl fallback, which has no dynamic loader and is
# therefore headless (winit cannot open Wayland/X11). This script is that
# missing half on the host where it is trivial: any glibc Linux box, a WSL2
# distro, or a CI container. It produces exactly the layout `build.py`
# produces, so `install-linux.sh` cannot tell the two apart:
#
#   bin/taskman, bin/taskman-service, install-linux.sh, uninstall-linux.sh,
#   taskman-core.service, core-service.json.in, the desktop entry, the icon,
#   the AppStream metadata and README.md — mode 0755 for the binaries and the
#   two scripts, 0644 for the data files, owner root/root.
#
# Usage: packaging/linux/build-artifact.sh [--out DIR] [--verify]
#
#   --out DIR   write the tarball there (default: <repo>/dist)
#   --verify    run the freshly built GUI's headless `--selfcheck --mock` and
#               the broker's `--selfcheck`, and fail when either does not pass

set -euo pipefail

# Resolve the toolchain before pinning PATH: rustup installs live under
# ~/.cargo/bin, which is not a system directory, and a tampered PATH must not
# choose the `tar` or `install` that assemble what an installer will run.
cargo_bin=$(command -v cargo) || {
  printf 'build-artifact: cargo is not on PATH\n' >&2
  exit 1
}
rustc_bin=$(command -v rustc) || {
  printf 'build-artifact: rustc is not on PATH\n' >&2
  exit 1
}
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
readonly SCRIPT_DIR
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd -P)
readonly REPO_ROOT
readonly PACKAGING=$SCRIPT_DIR
readonly SERVICE_BINARY=taskman-service
readonly PAYLOAD_FILES=(
  install-linux.sh
  uninstall-linux.sh
  taskman-core.service
  core-service.json.in
  io.github.aufkrawall.Taskman.desktop
  io.github.aufkrawall.Taskman.svg
  io.github.aufkrawall.Taskman.metainfo.xml
  README.md
)

die() {
  printf 'build-artifact: %s\n' "$*" >&2
  exit 1
}
note() {
  printf 'build-artifact: %s\n' "$*"
}

out_dir=$REPO_ROOT/dist
verify=0
while (($#)); do
  case $1 in
    --out)
      [[ $# -ge 2 ]] || die "--out needs a directory"
      out_dir=$2
      shift 2
      ;;
    --verify)
      verify=1
      shift
      ;;
    -h | --help)
      sed -n '3,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

# A static build would produce exactly the headless artifact this script exists
# to replace, so refuse early instead of shipping a second useless tarball.
dynamic_target_supported() {
  local target=${1:-}
  case $target in
    *musl*) return 1 ;;
  esac
  return 0
}

cd -- "$REPO_ROOT"
[[ -f Cargo.toml ]] || die "no Cargo.toml at $REPO_ROOT"

host_target=$("$rustc_bin" -vV | sed -n 's/^host: //p')
[[ -n $host_target ]] || die "rustc did not report a host triple"
dynamic_target_supported "$host_target" ||
  die "host target $host_target is static; run this on a glibc Linux host"

version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[[ -n $version ]] || die "could not read the workspace version"

# Native build: no cross toolchain, no target flag, so the resulting binary is
# linked against the host's glibc and can load Wayland/X11 at runtime.
note "building taskman and $SERVICE_BINARY for $host_target"
"$cargo_bin" build --release --locked --bin taskman --bin "$SERVICE_BINARY"

readonly GUI_EXE=$REPO_ROOT/target/release/taskman
readonly SERVICE_EXE=$REPO_ROOT/target/release/$SERVICE_BINARY
[[ -x $GUI_EXE ]] || die "$GUI_EXE was not produced"
[[ -x $SERVICE_EXE ]] || die "$SERVICE_EXE was not produced; the tarball must carry the broker"

# A glibc GUI is the whole point: a statically linked one silently cannot open
# a window, and that is exactly the mistake this script must not repeat.
if file "$GUI_EXE" 2>/dev/null | grep -q 'statically linked'; then
  die "$GUI_EXE is statically linked; this artifact would be headless"
fi

if ((verify)); then
  note "verifying the GUI's headless self-check"
  data_dir=$(mktemp -d)
  TASKMAN_DATA_DIR=$data_dir TASKMAN_CONFIG_DIR=$data_dir "$GUI_EXE" --selfcheck --mock >/dev/null ||
    die "the GUI's --selfcheck --mock failed"
  rm -rf -- "$data_dir"
  # The broker's own --selfcheck verifies the protected install path, so it can
  # only pass on an installed copy as root. That belongs to the packaging test
  # (`makepkg` + `pacman -U`), not to this build.
fi

stage=$(mktemp -d)
trap 'rm -rf -- "$stage"' EXIT
mkdir -p -- "$stage/bin"
install -m755 "$GUI_EXE" "$stage/bin/taskman"
install -m755 "$SERVICE_EXE" "$stage/bin/$SERVICE_BINARY"
for name in "${PAYLOAD_FILES[@]}"; do
  [[ -f $PACKAGING/$name ]] || die "packaging file $name is missing"
  case $name in
    *.sh) install -m755 "$PACKAGING/$name" "$stage/$name" ;;
    *) install -m644 "$PACKAGING/$name" "$stage/$name" ;;
  esac
done

mkdir -p -- "$out_dir"
artifact=$out_dir/taskman-v$version-linux-x86_64.tar.gz
# Deterministic archive: sorted names, fixed owner and mtime-independent order,
# so two builds of one commit produce one file.
# Named entries rather than `.`: the archive must list `bin/taskman` exactly as
# `build.py`'s tarball does, with no `./` prefix to trip a directory comparison
# between the two producers.
tar --create --file - \
  --directory "$stage" \
  --owner=0 --group=0 --numeric-owner \
  --sort=name \
  bin/taskman "bin/$SERVICE_BINARY" "${PAYLOAD_FILES[@]}" | gzip -n -9 >"$artifact"

note "artifact: $artifact"
tar -tzf "$artifact" | sed 's/^/  /'
