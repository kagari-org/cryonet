#!/usr/bin/env bash
# Run a netns test inside a private mount+network namespace.
# Usage: sudo test/run-netns.sh test/control_plane_netns.sh [args...]
set -euo pipefail

if [ "$#" -eq 0 ]; then
    echo "usage: sudo $0 <test-script> [args...]" >&2
    exit 2
fi

if [ "$(id -u)" != 0 ]; then
    exec sudo -- "$0" "$@"
fi

if [ "${CRYONET_NETNS_WRAPPED:-0}" = 1 ]; then
    exec "$@"
fi

BASH_BIN=$(readlink -f "$(command -v bash)")
CURRENT=$(readlink -f /run/current-system 2>/dev/null || true)

exec unshare -nm env \
    CRYONET_NETNS_WRAPPED=1 \
    BASH_BIN="$BASH_BIN" \
    CURRENT="$CURRENT" \
    "$BASH_BIN" -c '
        set -euo pipefail
        mount -t tmpfs tmpfs /run
        if [ -n "${CURRENT:-}" ]; then
            ln -s "$CURRENT" /run/current-system
        fi
        mkdir -p /run/netns
        exec "$BASH_BIN" "$@"
    ' _ "$@"
