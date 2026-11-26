#!/usr/bin/env bash
#
# Remove TaskMan from this system.
#
# The file list lives in install-linux.sh because one script has to own the
# layout: a second copy of it here would be the first thing to drift, and a
# removal that misses a root-owned service image is worse than no removal.
#
# This wrapper only forwards to `--uninstall`; pass --quiet or --root through as
# usual.

set -euo pipefail

# Same reasoning as install-linux.sh: `readlink` and `bash` are resolved by name
# while this runs as root, so search the system set instead of whatever PATH a
# sudo or scriptlet environment carried.
export PATH=/usr/sbin:/usr/bin:/sbin:/bin

self=$(readlink -f -- "${BASH_SOURCE[0]}")
here=${self%/*}
if [ -z "$here" ]; then
  here=/
fi

exec bash "$here/install-linux.sh" --uninstall "$@"
