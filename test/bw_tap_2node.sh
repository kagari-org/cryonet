#!/usr/bin/env bash
# Two-node TAP data-plane bandwidth test over a direct veth pair.
# Run as root: sudo test/run-netns.sh test/bw_tap_2node.sh
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
IPERF3=${IPERF3:-$(command -v iperf3 2>/dev/null || ls -d /nix/store/*-iperf-3*/bin/iperf3 2>/dev/null | head -1)}
OUT=${OUT:-$ROOT/test/results/bw-tap-2node-$(date +%Y%m%d-%H%M%S)}
DURATION=${DURATION:-5}
NS1=tap-a-$$
NS2=tap-b-$$
V1=tapv1-$$
V2=tapv2-$$
A=10.200.0.1
B=10.200.0.2
TA=10.100.0.1
TB=10.100.0.2

[ "$(id -u)" = 0 ] || { echo "must run as root" >&2; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }
[ -n "$IPERF3" ] || { echo "missing iperf3" >&2; exit 1; }
mkdir -p "$OUT"

log() { echo "[$(date +%H:%M:%S)] $*"; }
cleanup() {
    for ns in "$NS1" "$NS2"; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
    done
    for ns in "$NS1" "$NS2"; do
        ip netns del "$ns" 2>/dev/null
    done
    ip link del "$V1" 2>/dev/null
}
trap cleanup EXIT INT TERM

cleanup
ip netns add "$NS1"
ip netns add "$NS2"
ip link add "$V1" type veth peer name "$V2"
ip link set "$V1" netns "$NS1"
ip link set "$V2" netns "$NS2"
ip -n "$NS1" addr add "$A/24" dev "$V1"
ip -n "$NS1" link set lo up
ip -n "$NS1" link set "$V1" up mtu 1500
ip -n "$NS2" addr add "$B/24" dev "$V2"
ip -n "$NS2" link set lo up
ip -n "$NS2" link set "$V2" up mtu 1500

RUST_LOG=warn ip netns exec "$NS1" "$BIN" \
    --tap-mode --tap-interface-name cn0 \
    --listen 0.0.0.0:2333 --ctl-path "$OUT/a.ctl" >"$OUT/node1.log" 2>&1 &
RUST_LOG=warn ip netns exec "$NS2" "$BIN" \
    --tap-mode --tap-interface-name cn0 \
    --servers "ws://$A:2333" \
    --listen 0.0.0.0:2333 --ctl-path "$OUT/b.ctl" >"$OUT/node2.log" 2>&1 &

for _ in $(seq 1 100); do
    ip -n "$NS1" link show cn0 >/dev/null 2>&1 \
        && ip -n "$NS2" link show cn0 >/dev/null 2>&1 && break
    sleep 0.2
done
ip -n "$NS1" addr add "$TA/24" dev cn0
ip -n "$NS1" link set cn0 up mtu 1280
ip -n "$NS2" addr add "$TB/24" dev cn0
ip -n "$NS2" link set cn0 up mtu 1280

ok=0
for _ in $(seq 1 60); do
    if ip netns exec "$NS1" ping -c1 -W1 -q "$TB" >/dev/null 2>&1; then
        ok=1
        break
    fi
    sleep 0.5
done
log "tap data path up=$ok"
if [ "$ok" != 1 ]; then
    tail -40 "$OUT/node1.log" "$OUT/node2.log"
    exit 1
fi

run_iperf() {
    local tag=$1
    shift
    ip netns exec "$NS2" "$IPERF3" -s -1 >"$OUT/server-$tag.log" 2>&1 &
    local server_pid=$!
    for _ in $(seq 1 30); do
        kill -0 "$server_pid" 2>/dev/null || break
        ip netns exec "$NS2" ss -ltn 2>/dev/null | grep -q ':5201' && break
        sleep 0.1
    done
    ip netns exec "$NS1" "$IPERF3" -c "$TB" -t "$DURATION" -O 1 -i 0 "$@" \
        >"$OUT/iperf-$tag.log" 2>&1
    local rc=$?
    kill "$server_pid" 2>/dev/null
    wait "$server_pid" 2>/dev/null
    log "iperf $tag rc=$rc"
    grep -E 'sender|receiver' "$OUT/iperf-$tag.log" | tail -2
}

run_iperf fwd
run_iperf rev -R
log "out=$OUT"
