#!/usr/bin/env bash
# Two-node TUN data-plane RTT, optionally with netem delay on the underlay.
# Run as root: sudo test/run-netns.sh test/latency_tun_2node.sh
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
OUT=${OUT:-$ROOT/test/results/latency-tun-2node-$(date +%Y%m%d-%H%M%S)}
DELAY=${DELAY:-0ms}
COUNT=${COUNT:-20}
NS1=lat-a-$$
NS2=lat-b-$$
V1=latv1-$$
V2=latv2-$$
A=10.200.0.1
B=10.200.0.2
TA=10.100.0.1
TB=10.100.0.2

[ "$(id -u)" = 0 ] || { echo "must run as root" >&2; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }
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
if [ "$DELAY" != 0 ] && [ "$DELAY" != 0ms ]; then
    ip netns exec "$NS1" tc qdisc add dev "$V1" root netem delay "$DELAY"
    ip netns exec "$NS2" tc qdisc add dev "$V2" root netem delay "$DELAY"
fi

RUST_LOG=warn ip netns exec "$NS1" "$BIN" \
    --interface-prefix cn --listen 0.0.0.0:2333 --ctl-path "$OUT/a.ctl" \
    >"$OUT/node1.log" 2>&1 &
RUST_LOG=warn ip netns exec "$NS2" "$BIN" \
    --interface-prefix cn --servers "ws://$A:2333" \
    --listen 0.0.0.0:2333 --ctl-path "$OUT/b.ctl" >"$OUT/node2.log" 2>&1 &

for _ in $(seq 1 120); do
    ip -n "$NS1" link show cn0 >/dev/null 2>&1 \
        && ip -n "$NS2" link show cn0 >/dev/null 2>&1 && break
    sleep 0.2
done
ip -n "$NS1" addr add "$TA/32" dev cn0
ip -n "$NS1" link set cn0 up mtu 1280
ip -n "$NS1" route add "$TB/32" dev cn0
ip -n "$NS2" addr add "$TB/32" dev cn0
ip -n "$NS2" link set cn0 up mtu 1280
ip -n "$NS2" route add "$TA/32" dev cn0

ok=0
for _ in $(seq 1 120); do
    if ip netns exec "$NS1" ping -c1 -W1 -q "$TB" >/dev/null 2>&1; then
        ok=1
        break
    fi
    sleep 0.5
done
log "data path up=$ok delay=$DELAY"
if [ "$ok" != 1 ]; then
    tail -40 "$OUT/node1.log" "$OUT/node2.log"
    exit 1
fi

ip netns exec "$NS1" ping -c "$COUNT" -i 0.2 -q "$TB" >"$OUT/ping.txt" 2>&1
grep -E 'rtt|packet loss' "$OUT/ping.txt" | tee "$OUT/summary.txt"
log "out=$OUT"
