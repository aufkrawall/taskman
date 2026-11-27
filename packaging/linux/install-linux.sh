#!/usr/bin/env bash
#
# TaskMan Linux installer, uninstaller and package scriptlet.
#
# The install layout is documented in llm-wiki/linux-core-service.md:
#
#   /usr/bin/taskman                                         GUI, 0755
#   /usr/lib/taskman/taskman-service                         broker, 0755
#   /usr/lib/taskman/install-linux.sh                        this script, 0755
#   /usr/lib/taskman/core-service.json.in                    manifest template, 0644
#   /usr/lib/systemd/system/taskman-core.service             unit, 0644
#   /etc/taskman/core-service.json                           manifest, 0644
#   /usr/share/applications/io.github.aufkrawall.Taskman.desktop
#   /usr/share/icons/hicolor/scalable/apps/io.github.aufkrawall.Taskman.svg
#   /usr/share/metainfo/io.github.aufkrawall.Taskman.metainfo.xml
#   /run/taskman                                             by the unit's RuntimeDirectory=
#
# Why one script instead of a file list in each package: the tarball, the Arch
# package and a --dev install must put the same bytes in the same places with
# the same modes. --packaged is exactly the same install into a staging root,
# so the `pacman` payload and a manual tarball install cannot drift apart.
#
# The service image and the manifest are only ever written root-owned and not
# group/world-writable; every check runs before the first file is written, so a
# refusal leaves the system untouched.

set -euo pipefail

readonly GUI_BINARY=taskman
readonly SERVICE_BINARY=taskman-service
readonly UNIT_NAME=taskman-core.service
readonly DESKTOP_ID=io.github.aufkrawall.Taskman
readonly MANIFEST_NAME=core-service.json

# Absolute paths on the target system; --root prefixes them for package
# staging, which is the only place a prefix may appear.
readonly BIN_DIR=/usr/bin
readonly LIB_DIR=/usr/lib/taskman
readonly UNIT_DIR=/usr/lib/systemd/system
readonly CONF_DIR=/etc/taskman
readonly DESKTOP_DIR=/usr/share/applications
readonly ICON_DIR=/usr/share/icons/hicolor/scalable/apps
readonly METAINFO_DIR=/usr/share/metainfo
readonly RUNTIME_DIR=/run/taskman

# Manifest constants mirrored from crates/tm-platform/src/linux_core_service.rs.
# The service rejects a manifest that disagrees with these, so they are fixed
# here rather than configurable.
readonly PROTOCOL_VERSION=3
readonly SOCKET_PATH=/run/taskman/core.sock
readonly SENSOR_CACHE_MS=500

# Largest uid a manifest can carry (u32). Checked here because a shorter read
# of "4294967296" would authorize a different account entirely.
readonly MAX_UID=4294967295

# Every tool below is invoked by bare name while this script runs as root, and
# both a sudo environment and pacman's scriptlet environment can carry a PATH an
# unprivileged user prepared. Search the system set instead.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

ROOT=
SOURCE_DIR=
TARGET_USER=
MODE=install
PACKAGED=0
DEV=0
QUIET=0

SCRIPT_PATH=
SCRIPT_DIR=
GUI_SOURCE=
SERVICE_SOURCE=
UNIT_SOURCE=
TEMPLATE=
MODE_LABEL=install
CAN_CHOWN=0
RESOLVED_UID=
RESOLVED_USER_NAME=
# Set when the installed manifest is not one this installer wrote and has to be
# replaced: the old file is kept as <manifest>.bak before the new one lands.
MANIFEST_NEEDS_BACKUP=0

# Every uid the manifest will authorize, in the order they were found: an
# existing manifest's list first, then the user of this run.
AUTHORIZED_UIDS=()
# Paths of installed clients recorded in the manifest.
CLIENT_PATHS=()
TEMP_FILES=()

usage() {
  cat <<'EOF'
TaskMan Linux installer.

Usage: install-linux.sh [options]

Installs the GUI, the protected core service (taskman-core.service) and the
desktop integration, or removes exactly the files this installer owns.

Options:
  --user <name|uid>    account allowed to use the broker (default: SUDO_USER,
                       or the invoking user). Repeat the install for each
                       desktop user on a multi-user machine; existing uids are
                       kept.
  --source-dir <dir>   directory holding bin/taskman and bin/taskman-service
                       (default: next to this script, i.e. the tarball layout)
  --root <dir>         install into a staging root instead of / (package
                       builds: DESTDIR). No systemd action is taken.
  --packaged           staging mode for packages: stage the files, write the
                       manifest placeholder and leave authorization plus
                       enablement to --post-install
  --dev                development install: the manifest accepts any binary of
                       the authorized user, and the client path actually used
                       is recorded
  --post-install       scriptlet mode: reload systemd, enable and start the
                       unit, refresh desktop/icon caches and (re)write the
                       manifest for the installing user
  --uninstall          stop and disable the unit and remove exactly the files
                       this installer owns; user settings are left alone
  --quiet              only print warnings and errors
  --help               show this help

Environment:
  SUDO_USER            used to pick the authorized user when --user is absent
EOF
}

log() {
  if [ "$QUIET" -eq 0 ]; then
    printf '[install-linux] %s\n' "$*"
  fi
}

warn() {
  printf '[install-linux] warning: %s\n' "$*" >&2
}

die() {
  printf '[install-linux] error: %s\n' "$*" >&2
  exit 1
}

cleanup() {
  local file
  for file in "${TEMP_FILES[@]}"; do
    if [ -e "$file" ]; then
      rm -f -- "$file" 2>/dev/null || true
    fi
  done
}

trap cleanup EXIT

parse_args() {
  while [ "$#" -gt 0 ]; do
    case $1 in
      --user)
        [ "$#" -ge 2 ] || die "--user needs a value"
        TARGET_USER=$2
        shift 2
        ;;
      --source-dir)
        [ "$#" -ge 2 ] || die "--source-dir needs a value"
        SOURCE_DIR=$2
        shift 2
        ;;
      --root)
        [ "$#" -ge 2 ] || die "--root needs a value"
        ROOT=$2
        shift 2
        ;;
      --packaged)
        PACKAGED=1
        shift
        ;;
      --dev)
        DEV=1
        shift
        ;;
      --post-install)
        MODE=post-install
        shift
        ;;
      --uninstall)
        MODE=uninstall
        shift
        ;;
      --quiet)
        QUIET=1
        shift
        ;;
      --help | -h)
        usage
        exit 0
        ;;
      *)
        die "unknown argument: $1 (see --help)"
        ;;
    esac
  done
}

normalize_root() {
  if [ -n "$ROOT" ]; then
    # A trailing slash would double up in every path below; `/` alone means
    # the real system, which is what the no-prefix path already does.
    while [ "${ROOT%/}" != "$ROOT" ]; do
      ROOT=${ROOT%/}
    done
    case $ROOT in
      /*) ;;
      *) die "--root needs an absolute path, got $ROOT" ;;
    esac
  fi
}

validate_mode() {
  if [ "$PACKAGED" -eq 1 ] && [ -z "$ROOT" ]; then
    die "--packaged needs --root <staging dir>: it stages a package payload and never changes the running system"
  fi
  if [ "$PACKAGED" -eq 1 ] && [ "$DEV" -eq 1 ]; then
    die "--dev and --packaged cannot be combined: a package never ships the user-writable client escape hatch"
  fi
  if [ "$PACKAGED" -eq 1 ] && [ "$MODE" = uninstall ]; then
    die "--uninstall is not a package operation: pacman removes the payload"
  fi
  case $MODE in
    install)
      if [ "$DEV" -eq 1 ]; then MODE_LABEL=dev; else MODE_LABEL=install; fi
      ;;
    post-install)
      MODE_LABEL=post-install
      ;;
    uninstall)
      MODE_LABEL=uninstall
      ;;
  esac
  if [ "$PACKAGED" -eq 1 ]; then
    MODE_LABEL=packaged
  fi
}

resolve_self() {
  local source=${BASH_SOURCE[0]:-}
  if [ -z "$source" ]; then
    die "cannot locate this script (BASH_SOURCE is empty)"
  fi
  SCRIPT_PATH=$(readlink -f -- "$source") || die "cannot resolve $source"
  SCRIPT_DIR=${SCRIPT_PATH%/*}
  if [ -z "$SCRIPT_DIR" ]; then
    SCRIPT_DIR=/
  fi
  UNIT_SOURCE=$SCRIPT_DIR/$UNIT_NAME
  TEMPLATE=$SCRIPT_DIR/$MANIFEST_NAME.in
}

# Pick the directory the two binaries come from. --source-dir may point at the
# tarball layout (bin/ below it) or directly at the directory holding them.
# A directory that holds only one of them is a broken payload, so it is named
# instead of silently sliding on to the next candidate.
resolve_source_dir() {
  local given=$1 candidate name missing
  local -a candidates=()
  if [ -n "$given" ]; then
    candidates=("$given/bin" "$given")
  else
    candidates=("$SCRIPT_DIR/bin" "$SCRIPT_DIR")
  fi
  for candidate in "${candidates[@]}"; do
    if [ ! -d "$candidate" ]; then
      continue
    fi
    missing=
    for name in "$GUI_BINARY" "$SERVICE_BINARY"; do
      if [ ! -f "$candidate/$name" ]; then
        missing=$candidate/$name
      fi
    done
    if [ -z "$missing" ]; then
      SOURCE_DIR=$(readlink -f -- "$candidate")
      return 0
    fi
    if [ -f "$candidate/$GUI_BINARY" ] || [ -f "$candidate/$SERVICE_BINARY" ]; then
      die "missing binary: $missing (the GUI and the service are one payload, build both)"
    fi
  done
  if [ -n "$given" ]; then
    die "--source-dir $given holds neither bin/$GUI_BINARY + bin/$SERVICE_BINARY nor the two binaries themselves"
  fi
  die "no $GUI_BINARY and $SERVICE_BINARY found next to $SCRIPT_DIR (expected bin/); pass --source-dir <dir>"
}

validate_sources() {
  local binary
  for binary in "$SOURCE_DIR/$GUI_BINARY" "$SOURCE_DIR/$SERVICE_BINARY"; do
    if [ ! -f "$binary" ]; then
      die "missing binary: $binary (build it with 'python build.py' and pass --source-dir)"
    fi
    if [ ! -x "$binary" ]; then
      die "$binary is not executable"
    fi
  done
  GUI_SOURCE=$SOURCE_DIR/$GUI_BINARY
  SERVICE_SOURCE=$SOURCE_DIR/$SERVICE_BINARY
}

# The unit names the installed broker, so the installer verifies that the two
# descriptions agree before it writes either of them: an ExecStart pointing at
# a path this installer does not create would enable a unit that cannot start.
validate_unit() {
  local expected actual lines
  if [ ! -f "$UNIT_SOURCE" ]; then
    die "missing unit file next to this script: $UNIT_SOURCE"
  fi
  lines=$(grep -c '^ExecStart=' -- "$UNIT_SOURCE" || true)
  if [ "$lines" != 1 ]; then
    die "$UNIT_SOURCE must carry exactly one ExecStart= line, found $lines"
  fi
  actual=$(grep '^ExecStart=' -- "$UNIT_SOURCE")
  expected="ExecStart=$LIB_DIR/$SERVICE_BINARY --run"
  if [ "$actual" != "$expected" ]; then
    die "$UNIT_SOURCE starts '$actual' but this installer installs '$expected'"
  fi
}

# Whether systemd is the running init system. Split from require_systemd so the
# package scriptlet can skip instead of failing inside a chroot or container.
systemd_is_running() {
  command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]
}

require_systemd() {
  if ! command -v systemctl >/dev/null 2>&1; then
    die "systemctl is not installed: $UNIT_NAME is a systemd unit, nothing was changed"
  fi
  if [ ! -d /run/systemd/system ]; then
    die "systemd is not running as the init system (/run/systemd/system is missing), nothing was changed; use --root for a staging build"
  fi
}

require_root() {
  if [ "$EUID" -ne 0 ]; then
    die "a system install needs root: re-run with sudo, or pass --root <dir> for a package build"
  fi
}

# Whether the account that would be authorized can be resolved, without dying.
# --post-install uses this so a SUDO_USER that no longer exists cannot fail a
# package transaction; an explicit --user still fails loudly, because that is an
# operator typo rather than a guess.
can_resolve_user() {
  local spec=${TARGET_USER:-${SUDO_USER:-}}
  [ -n "$spec" ] && id -u -- "$spec" >/dev/null 2>&1
}

resolve_user() {
  local spec=${TARGET_USER:-} uid name
  if [ -z "$spec" ]; then
    spec=${SUDO_USER:-}
  fi
  if [ -z "$spec" ] && [ "$EUID" -ne 0 ]; then
    spec=$(id -un)
  fi
  if [ -z "$spec" ]; then
    die "cannot tell which user may use the broker: re-run through sudo or pass --user <name>"
  fi
  if ! uid=$(id -u -- "$spec" 2>/dev/null); then
    die "no such user: $spec (pass an existing account with --user)"
  fi
  case $uid in
    '' | *[!0-9]*) die "could not resolve a numeric uid for $spec" ;;
  esac
  if [ "$uid" -eq 0 ]; then
    die "--user $spec is uid 0: root is accepted by the broker anyway, so name the unprivileged account that runs the GUI"
  fi
  if ! name=$(id -nu -- "$uid" 2>/dev/null); then
    name=$spec
  fi
  RESOLVED_UID=$uid
  RESOLVED_USER_NAME=$name
  if [ -n "${SUDO_USER:-}" ] && [ "$name" != "$SUDO_USER" ]; then
    log "authorizing $name (uid $uid), not the invoking user $SUDO_USER"
  fi
}

add_uid() {
  local uid=$1 known
  for known in "${AUTHORIZED_UIDS[@]}"; do
    if [ "$known" = "$uid" ]; then
      return 0
    fi
  done
  AUTHORIZED_UIDS+=("$uid")
}

add_client() {
  local path=$1 known
  case $path in
    /*) ;;
    *) die "internal error: client path $path is not absolute" ;;
  esac
  case $path in
    *'"'* | *'\'*) die "internal error: client path $path cannot be represented in JSON" ;;
  esac
  for known in "${CLIENT_PATHS[@]}"; do
    if [ "$known" = "$path" ]; then
      return 0
    fi
  done
  CLIENT_PATHS+=("$path")
}

# Read the uids an existing manifest authorizes.
#
# Returns 0 when the file is absent (nothing to merge), 0 with the parsed uids
# appended to AUTHORIZED_UIDS when it holds a readable list, and 1 when the file
# exists but is not a manifest this installer may extend.
#
# The array body is read as text between the brackets rather than as a
# single-line match, so a hand-formatted multi-line list is understood. Every
# entry must be a plain decimal uid in 1..=4294967295: a truncated read of
# "4294967296" would authorize a different account, and a value the service's
# Vec<u32> rejects would leave an install that claims success and a broker that
# refuses to start.
read_manifest_uids() {
  local file=$1 text rest body token
  local -a found=()
  if [ ! -f "$file" ]; then
    return 0
  fi
  text=$(<"$file")
  case $text in
    *'"authorized_uids"'*) ;;
    *) return 1 ;;
  esac
  rest=${text#*'"authorized_uids"'}
  case $rest in
    *'['*) ;;
    *) return 1 ;;
  esac
  body=${rest#*'['}
  case $body in
    *']'*) ;;
    *) return 1 ;;
  esac
  body=${body%%']'*}
  body=${body//[$'\r\n\t']/ }
  IFS=', ' read -r -a found <<<"$body"
  for token in "${found[@]}"; do
    case $token in
      '' | *[!0-9]*) return 1 ;;
    esac
    if [ "${#token}" -gt 10 ] || [ "$token" -gt "$MAX_UID" ] || [ "$token" -eq 0 ]; then
      return 1
    fi
    add_uid "$token"
  done
  return 0
}

render_manifest() {
  local out=$1 text uids= clients= uid client
  if [ ! -f "$TEMPLATE" ]; then
    die "missing manifest template: $TEMPLATE"
  fi
  for uid in "${AUTHORIZED_UIDS[@]}"; do
    case $uid in
      '' | *[!0-9]*) die "internal error: authorized uid is not numeric: $uid" ;;
    esac
    uids=${uids:+$uids, }$uid
  done
  for client in "${CLIENT_PATHS[@]}"; do
    clients=${clients:+$clients, }\"$client\"
  done
  text=$(<"$TEMPLATE")
  text=${text//@PROTOCOL@/$PROTOCOL_VERSION}
  text=${text//@UIDS@/$uids}
  text=${text//@CLIENTS@/$clients}
  if [ "$DEV" -eq 1 ]; then
    text=${text//@ALLOW_USER_WRITABLE@/true}
  else
    text=${text//@ALLOW_USER_WRITABLE@/false}
  fi
  text=${text//@SOCKET@/$SOCKET_PATH}
  text=${text//@SENSOR_CACHE_MS@/$SENSOR_CACHE_MS}
  text=${text//@INSTALLED_UNIX_S@/$(date +%s)}
  text=${text//@NOTE@/install-linux.sh mode=$MODE_LABEL}
  case $text in
    *@*@*)
      rm -f -- "$out"
      die "the manifest template $TEMPLATE still contains an unexpanded placeholder"
      ;;
  esac
  printf '%s\n' "$text" >"$out"
}

# The rendered manifest is the authorization policy, and the service refuses to
# start on a file it cannot parse. Parsing it here turns "the broker silently
# never comes up" into an install that fails in the terminal, and it also
# catches a client path that would have broken the JSON string.
validate_manifest_json() {
  local file=$1
  if ! command -v python3 >/dev/null 2>&1; then
    log "python3 not installed: the rendered manifest is not JSON-checked"
    return 0
  fi
  if ! python3 -c 'import json,sys; json.load(open(sys.argv[1], encoding="utf-8"))' "$file"; then
    die "$file is not valid JSON: the manifest template or a client path is broken"
  fi
}

# Drop pacman's copy of the packaged placeholder. `/etc/taskman/core-service.json`
# is in the package's `backup=()`, so an upgrade never touches the live file and
# instead writes the placeholder beside it as `.pacnew`. This scriptlet is the
# merge, and the placeholder carries no policy this system should adopt: it is
# superseded both when the policy was regenerated and when the installed policy
# was deliberately kept.
remove_superseded_placeholder() {
  local stale=$1
  if [ -e "$stale" ]; then
    rm -f -- "$stale"
    log "removed the superseded placeholder $stale"
  fi
}

# Write the manifest atomically. A partly written policy file would make the
# broker refuse to start (it validates the manifest before listening), so the
# final rename is what makes a crash mid-install harmless.
#
# This is the only place the manifest is written, and it runs after the
# directory has been created and refused as a symlink: a root-owned write must
# never follow a link an ordinary user could have planted.
install_manifest() {
  local dest=$ROOT$CONF_DIR/$MANIFEST_NAME staged
  ensure_dir "$ROOT$CONF_DIR" 0755
  if [ "$MANIFEST_NEEDS_BACKUP" -eq 1 ]; then
    if [ "$CAN_CHOWN" -eq 1 ]; then
      install -o 0 -g 0 -m 0600 -- "$dest" "$dest.bak"
    else
      install -m 0600 -- "$dest" "$dest.bak"
    fi
    log "kept the previous policy as $dest.bak"
  fi
  staged=$(mktemp -- "$ROOT$CONF_DIR/.$MANIFEST_NAME.XXXXXX")
  TEMP_FILES+=("$staged")
  render_manifest "$staged"
  validate_manifest_json "$staged"
  chmod 0644 -- "$staged"
  if [ "$CAN_CHOWN" -eq 1 ]; then
    chown 0:0 -- "$staged"
  fi
  if [ -L "$dest" ]; then
    warn "$dest was a symbolic link; replacing it with a regular file"
    rm -f -- "$dest"
  fi
  mv -f -- "$staged" "$dest"
  # pacman keeps the packaged placeholder as .pacnew when the live file differs.
  # This run just generated the policy that file was meant to be merged into, so
  # leaving a second, unconfigured copy behind would only hide the real state.
  remove_superseded_placeholder "$dest.pacnew"
  log "manifest written: $dest"
}

ensure_dir() {
  local dir=$1 mode=$2
  if [ -L "$dir" ]; then
    die "$dir is a symbolic link; refusing to install below it"
  fi
  if [ "$CAN_CHOWN" -eq 1 ]; then
    install -d -o 0 -g 0 -m "$mode" -- "$dir"
  else
    install -d -m "$mode" -- "$dir"
  fi
}

# Copy one owned file with an exact mode, never through a symlink (install(1)
# would write through one, which is how a user-writable target could end up
# owning the service image).
install_owned() {
  local src=$1 dest=$2 mode=$3
  if [ ! -f "$src" ]; then
    die "missing source file: $src"
  fi
  if [ "$(readlink -f -- "$src")" = "$(readlink -f -- "$dest" 2>/dev/null || true)" ]; then
    log "unchanged: $dest"
    return 0
  fi
  if [ -L "$dest" ]; then
    warn "$dest was a symbolic link; replacing it with a regular file"
    rm -f -- "$dest"
  fi
  if [ "$CAN_CHOWN" -eq 1 ]; then
    install -o 0 -g 0 -m "$mode" -- "$src" "$dest"
  else
    install -m "$mode" -- "$src" "$dest"
  fi
  log "installed: $dest ($mode root:root)"
}

# The service image and the manifest are the trust boundary, so a real install
# re-reads what it just wrote instead of assuming install(1) did what it was
# told. Modes are also what a package payload carries, so the staging path
# verifies them too; ownership only exists where chown is permitted.
verify_protected() {
  local path=$1 label=$2 expected_mode=$3 owner mode
  if [ ! -f "$path" ]; then
    die "$label was not installed: $path"
  fi
  mode=$(stat -c %a -- "$path")
  if [ "$mode" != "$expected_mode" ]; then
    die "$label has mode $mode, expected $expected_mode: $path"
  fi
  if [ -z "$ROOT" ]; then
    owner=$(stat -c %u -- "$path")
    if [ "$owner" != 0 ]; then
      die "$label must be owned by root, found uid $owner: $path"
    fi
    if [ "$(stat -c %g -- "$path")" != 0 ]; then
      die "$label must belong to group root: $path"
    fi
  fi
}

# Every file this script installs has to be next to it. Checking them up front
# keeps a truncated payload (a hand-copied binary set, an archive that lost a
# file) from failing halfway through an install, and it is what makes a payload
# without the desktop/unit files a clear refusal instead of a stray install.
validate_packaging_files() {
  local name
  for name in \
    "$UNIT_NAME" \
    "$MANIFEST_NAME.in" \
    "$DESKTOP_ID.desktop" \
    "$DESKTOP_ID.svg" \
    "$DESKTOP_ID.metainfo.xml"; do
    if [ ! -f "$SCRIPT_DIR/$name" ]; then
      die "missing packaging file: $SCRIPT_DIR/$name (this payload is incomplete and cannot be installed)"
    fi
  done
}

stage_files() {
  ensure_dir "$ROOT$BIN_DIR" 0755
  ensure_dir "$ROOT$LIB_DIR" 0755
  ensure_dir "$ROOT$UNIT_DIR" 0755
  ensure_dir "$ROOT$CONF_DIR" 0755
  ensure_dir "$ROOT$DESKTOP_DIR" 0755
  ensure_dir "$ROOT$ICON_DIR" 0755
  ensure_dir "$ROOT$METAINFO_DIR" 0755

  # The service image goes first and is verified before any user-visible path is
  # touched: a failed install must not leave a new GUI talking to an old broker.
  install_owned "$SERVICE_SOURCE" "$ROOT$LIB_DIR/$SERVICE_BINARY" 0755
  verify_protected "$ROOT$LIB_DIR/$SERVICE_BINARY" "the service image" 755

  install_owned "$GUI_SOURCE" "$ROOT$BIN_DIR/$GUI_BINARY" 0755
  install_owned "$SCRIPT_PATH" "$ROOT$LIB_DIR/install-linux.sh" 0755
  install_owned "$TEMPLATE" "$ROOT$LIB_DIR/$MANIFEST_NAME.in" 0644
  install_owned "$UNIT_SOURCE" "$ROOT$UNIT_DIR/$UNIT_NAME" 0644
  install_owned "$SCRIPT_DIR/$DESKTOP_ID.desktop" \
    "$ROOT$DESKTOP_DIR/$DESKTOP_ID.desktop" 0644
  install_owned "$SCRIPT_DIR/$DESKTOP_ID.svg" \
    "$ROOT$ICON_DIR/$DESKTOP_ID.svg" 0644
  install_owned "$SCRIPT_DIR/$DESKTOP_ID.metainfo.xml" \
    "$ROOT$METAINFO_DIR/$DESKTOP_ID.metainfo.xml" 0644
}

owned_files() {
  OWNED_FILES=(
    "$ROOT$LIB_DIR/$SERVICE_BINARY"
    "$ROOT$LIB_DIR/install-linux.sh"
    "$ROOT$LIB_DIR/$MANIFEST_NAME.in"
    "$ROOT$BIN_DIR/$GUI_BINARY"
    "$ROOT$UNIT_DIR/$UNIT_NAME"
    "$ROOT$DESKTOP_DIR/$DESKTOP_ID.desktop"
    "$ROOT$ICON_DIR/$DESKTOP_ID.svg"
    "$ROOT$METAINFO_DIR/$DESKTOP_ID.metainfo.xml"
    "$ROOT$CONF_DIR/$MANIFEST_NAME"
    "$ROOT$CONF_DIR/$MANIFEST_NAME.bak"
    "$ROOT$CONF_DIR/$MANIFEST_NAME.pacnew"
  )
}

owned_dirs() {
  OWNED_DIRS=(
    "$ROOT$LIB_DIR"
    "$ROOT$CONF_DIR"
    "$ROOT$RUNTIME_DIR"
  )
}

refresh_caches() {
  if [ -n "$ROOT" ]; then
    log "staging root: the desktop and icon caches of this machine are not touched"
    return 0
  fi
  if command -v update-desktop-database >/dev/null 2>&1; then
    if ! update-desktop-database -q "$DESKTOP_DIR"; then
      warn "update-desktop-database failed; the desktop entry is still installed"
    fi
  else
    log "update-desktop-database not installed; the desktop entry cache is rebuilt at next login"
  fi
  if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    if ! gtk-update-icon-cache -q -t -f /usr/share/icons/hicolor; then
      warn "gtk-update-icon-cache failed; the icon is still installed"
    fi
  else
    log "gtk-update-icon-cache not installed; the icon cache is rebuilt at next login"
  fi
}

reload_units() {
  if ! systemctl daemon-reload; then
    warn "systemctl daemon-reload failed"
  fi
}

enable_and_start() {
  if ! systemctl enable -- "$UNIT_NAME"; then
    warn "systemctl enable $UNIT_NAME failed"
  fi
  # A unit that crash-looped before (bad manifest, missing image) keeps a
  # "failed" start-limit state that swallows the next start, so a reinstall
  # would look successful while the broker stayed down.
  if ! systemctl reset-failed -- "$UNIT_NAME"; then
    log "systemctl reset-failed $UNIT_NAME had no failed state to clear"
  fi
  # `restart` starts a stopped unit too, and a reinstall must make the running
  # broker read the manifest this run just wrote.
  if ! systemctl restart -- "$UNIT_NAME"; then
    warn "systemctl restart $UNIT_NAME failed"
  fi
}

report_service_state() {
  local state
  state=$(systemctl is-active -- "$UNIT_NAME" 2>/dev/null || true)
  if [ -z "$state" ]; then
    state=unknown
  fi
  log "service $UNIT_NAME: $state"
  if [ "$state" != active ]; then
    warn "the core service is not active; the GUI still runs, but privileged actions and elevated process evidence stay unavailable"
    systemctl --no-pager --lines=10 --full status -- "$UNIT_NAME" >&2 || true
  fi
}

# Decide the policy the manifest will carry. Read-only on purpose: it runs
# before the first file is written, so a refusal here cannot leave anything
# behind, and the backup of a foreign manifest is taken by install_manifest once
# the manifest directory itself has been refused as a symlink.
collect_manifest_policy() {
  local manifest=$ROOT$CONF_DIR/$MANIFEST_NAME previous
  AUTHORIZED_UIDS=()
  CLIENT_PATHS=()
  MANIFEST_NEEDS_BACKUP=0
  if ! read_manifest_uids "$manifest"; then
    MANIFEST_NEEDS_BACKUP=1
    warn "the existing $manifest is not a manifest this installer can extend (foreign or unreadable authorized_uids); it will be replaced and kept as $manifest.bak"
    AUTHORIZED_UIDS=()
  else
    previous=$(printf '%s ' "${AUTHORIZED_UIDS[@]}")
    previous=${previous% }
    if [ -n "$previous" ] && [ -n "$RESOLVED_UID" ]; then
      case " $previous " in
        *" $RESOLVED_UID "*)
          log "manifest already authorizes uid $RESOLVED_UID"
          ;;
        *)
          log "manifest authorizes uid(s) $previous; adding uid $RESOLVED_UID ($RESOLVED_USER_NAME) instead of replacing them"
          ;;
      esac
    fi
  fi
  if [ -n "$RESOLVED_UID" ]; then
    add_uid "$RESOLVED_UID"
  fi
  # The installed GUI is the one client the packaged policy trusts. A dev
  # install records the image that was actually built, because that is the
  # binary the developer runs. Both are runtime paths: --root is a build-time
  # prefix and never appears inside the manifest's contents.
  add_client "$BIN_DIR/$GUI_BINARY"
  if [ "$DEV" -eq 1 ] && [ -n "$GUI_SOURCE" ]; then
    add_client "$GUI_SOURCE"
  fi
  if [ "${#AUTHORIZED_UIDS[@]}" -eq 0 ]; then
    log "manifest placeholder: no authorized uid yet, --post-install fills it in for the installing user"
  fi
  if [ "$DEV" -eq 1 ]; then
    warn "development mode: the manifest accepts any binary of the authorized user (allow_user_writable_client)"
  fi
}

do_install() {
  local headline=TaskMan
  resolve_self
  resolve_source_dir "${SOURCE_DIR:-}"
  validate_sources
  validate_packaging_files
  validate_unit
  if [ -z "$ROOT" ]; then
    require_root
    require_systemd
  fi
  if [ -n "$TARGET_USER" ] || [ -z "$ROOT" ]; then
    resolve_user
  fi
  collect_manifest_policy

  if [ -n "$ROOT" ]; then
    headline="TaskMan into $ROOT"
  fi
  if [ -n "$RESOLVED_UID" ]; then
    headline="$headline for uid $RESOLVED_UID ($RESOLVED_USER_NAME)"
  fi
  log "installing $headline"
  stage_files
  install_manifest
  verify_protected "$ROOT$CONF_DIR/$MANIFEST_NAME" "the manifest" 644

  if [ -z "$ROOT" ]; then
    reload_units
    enable_and_start
    refresh_caches
    report_service_state
    log "done: start the GUI from the application menu, or remove this install with '$LIB_DIR/install-linux.sh --uninstall'"
  else
    log "staged into $ROOT; the package scriptlet runs '--post-install' after the files are written"
  fi
}

# --post-install is the pacman scriptlet. It must never fail the transaction for
# a documented manual step, so two cases are a loud warning plus a return 0
# instead of a die: a chroot/pacstrap where systemd is not the init system, and
# an install nobody can be authorized for. Neither leaves a unit that start-loops.
do_post_install() {
  local keep_existing=0 unconfigured=0
  resolve_self
  if [ -n "$ROOT" ]; then
    die "--post-install runs on the installed system and does not take --root"
  fi
  require_root
  if [ ! -f "$LIB_DIR/$SERVICE_BINARY" ] || [ ! -f "$UNIT_DIR/$UNIT_NAME" ]; then
    die "$UNIT_DIR/$UNIT_NAME or $LIB_DIR/$SERVICE_BINARY is missing: the package payload is incomplete"
  fi
  if ! systemd_is_running; then
    # pacstrap, arch-chroot and container builds run scriptlets without systemd;
    # there is nothing to reload, enable or start, and the booted system re-runs
    # this scriptlet when the unit is first needed.
    warn "systemd is not running here (/run/systemd/system is missing): skipping the unit setup"
    warn "from the booted system, run: $LIB_DIR/install-linux.sh --post-install"
    return 0
  fi
  if [ -n "${TARGET_USER:-}" ]; then
    resolve_user
  elif [ -n "${SUDO_USER:-}" ] && can_resolve_user; then
    resolve_user
  elif read_manifest_uids "$CONF_DIR/$MANIFEST_NAME" && [ "${#AUTHORIZED_UIDS[@]}" -gt 0 ]; then
    # A root login ran pacman. The live manifest still holds the policy (it is in
    # backup=), so regenerating it would only rewrite the same uids; keep it
    # when the protocol version still matches, or rewrite it with the preserved
    # uids when a package upgrade bumped the protocol version.
    if grep -Eq "\"protocol\"[[:space:]]*:[[:space:]]*$PROTOCOL_VERSION([^0-9]|$)" "$CONF_DIR/$MANIFEST_NAME"; then
      keep_existing=1
      warn "no invoking user to authorize; keeping the manifest's uid(s) ${AUTHORIZED_UIDS[*]} - run '$LIB_DIR/install-linux.sh --user <name>' to change them"
      remove_superseded_placeholder "$CONF_DIR/$MANIFEST_NAME.pacnew"
    else
      keep_existing=1
      add_client "$BIN_DIR/$GUI_BINARY"
      install_manifest
      verify_protected "$CONF_DIR/$MANIFEST_NAME" "the manifest" 644
      warn "updated manifest protocol to $PROTOCOL_VERSION while preserving uid(s) ${AUTHORIZED_UIDS[*]}"
    fi
  else
    unconfigured=1
  fi
  if [ "$unconfigured" -eq 1 ]; then
    if [ -n "${SUDO_USER:-}" ]; then
      warn "SUDO_USER names $SUDO_USER, which is not an account on this system"
    fi
    remove_superseded_placeholder "$CONF_DIR/$MANIFEST_NAME.pacnew"
    warn "no invoking user could be determined and $CONF_DIR/$MANIFEST_NAME authorizes nobody yet"
    warn "the core service is installed but NOT enabled: a unit whose manifest lists no uid fails on every start"
    warn "authorize a user (this also enables and starts the unit):"
    warn "  $LIB_DIR/install-linux.sh --user <name>"
    warn "or do it by hand afterwards:"
    warn "  systemctl enable $UNIT_NAME"
    warn "  systemctl start $UNIT_NAME"
  elif [ "$keep_existing" -eq 0 ]; then
    collect_manifest_policy
    install_manifest
    verify_protected "$CONF_DIR/$MANIFEST_NAME" "the manifest" 644
  fi
  reload_units
  if [ "$unconfigured" -eq 0 ]; then
    enable_and_start
  fi
  refresh_caches
  if [ "$unconfigured" -eq 0 ]; then
    report_service_state
  fi
  return 0
}

do_uninstall() {
  resolve_self
  if [ -z "$ROOT" ]; then
    require_root
    if command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
      if ! systemctl stop -- "$UNIT_NAME"; then
        warn "systemctl stop $UNIT_NAME failed"
      fi
      if ! systemctl disable -- "$UNIT_NAME"; then
        warn "systemctl disable $UNIT_NAME failed"
      fi
    else
      log "systemd is not running: skipping stop and disable"
    fi
  fi
  owned_files
  local file
  for file in "${OWNED_FILES[@]}"; do
    if [ -e "$file" ] || [ -L "$file" ]; then
      rm -f -- "$file"
      log "removed: $file"
    fi
  done
  owned_dirs
  local dir
  for dir in "${OWNED_DIRS[@]}"; do
    rmdir --ignore-fail-on-non-empty -- "$dir" 2>/dev/null || true
  done
  if [ -z "$ROOT" ]; then
    reload_units
    refresh_caches
    log "removed TaskMan and $UNIT_NAME; the GUI's own settings under ~/.config/taskman are untouched"
  else
    log "removed the staged files below $ROOT"
  fi
}

main() {
  parse_args "$@"
  normalize_root
  validate_mode
  if [ "$EUID" -eq 0 ] || [ -n "${FAKEROOTKEY:-}" ]; then
    CAN_CHOWN=1
  else
    CAN_CHOWN=0
  fi
  case $MODE in
    install)
      do_install
      ;;
    post-install)
      do_post_install
      ;;
    uninstall)
      do_uninstall
      ;;
    *)
      die "internal error: unknown mode $MODE"
      ;;
  esac
}

main "$@"
