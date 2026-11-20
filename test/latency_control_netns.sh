#!/usr/bin/env bash
# Two-node control-plane RTT: WebSocket mesh + IGP/registry, no TUN/ICE.
# Run as root: sudo test/run-netns.sh test/latency_control_netns.sh
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
CRYOC=${CRYOC:-$ROOT/target/release/cryoc}
OUT=${OUT:-$ROOT/test/results/latency-control-2node-$(date +%Y%m%d-%H%M%S)}
DELAY=${DELAY:-0ms}
COUNT=${COUNT:-20}
BR=latbr-$$
NET=10.200.0
NS1=latctl-a-$$
NS2=latctl-b-$$
V1=lath1-$$
V2=lath2-$$
C1=lath2c1-$$
C2=lath2c2-$$

[ "$(id -u)" = 0 ] || { echo "must run as root" >&2; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }
[ -x "$CRYOC" ] || { echo "missing $CRYOC" >&2; exit 1; }
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
    ip link del "$V2" 2>/dev/null
    ip link del "$BR" 2>/dev/null
}
trap cleanup EXIT INT TERM

cleanup
ip link add "$BR" type bridge
ip link set "$BR" up
namespaces=("$NS1" "$NS2")
host_links=("$V1" "$V2")
ns_links=("$C1" "$C2")
for i in 0 1; do
    ns=${namespaces[$i]}
    host_link=${host_links[$i]}
    ns_link=${ns_links[$i]}
    ip netns add "$ns"
    ip link add "$host_link" type veth peer name "$ns_link"
    ip link set "$host_link" master "$BR"
    ip link set "$host_link" up
    ip link set "$ns_link" netns "$ns"
    ip -n "$ns" addr add "$NET.$((i + 1))/24" dev "$ns_link"
    ip -n "$ns" link set "$ns_link" up
    ip -n "$ns" link set lo up
    if [ "$DELAY" != 0 ] && [ "$DELAY" != 0ms ]; then
        ip netns exec "$ns" tc qdisc add dev "$ns_link" root netem delay "$DELAY"
    fi
done

RUST_LOG=warn ip netns exec "$NS1" "$BIN" --id 1 \
    --listen 0.0.0.0:2333 --ctl-path "$OUT/a.ctl" \
    --candidate-filter-prefixes 0.0.0.0/0,::/0 >"$OUT/node1.log" 2>&1 &
RUST_LOG=warn ip netns exec "$NS2" "$BIN" --id 2 \
    --servers "ws://$NET.1:2333" --listen 0.0.0.0:2333 \
    --ctl-path "$OUT/b.ctl" \
    --candidate-filter-prefixes 0.0.0.0/0,::/0 >"$OUT/node2.log" 2>&1 &

r1=0
r2=0
for _ in $(seq 1 60); do
    r1=$(timeout 3 "$CRYOC" -c "$OUT/a.ctl" stats 2>/dev/null \
        | sed -n 's/^  MESH .*routes \([0-9][0-9]*\).*/\1/p')
    r2=$(timeout 3 "$CRYOC" -c "$OUT/b.ctl" stats 2>/dev/null \
        | sed -n 's/^  MESH .*routes \([0-9][0-9]*\).*/\1/p')
    if [ "${r1:-0}" -ge 1 ] && [ "${r2:-0}" -ge 1 ]; then
        break
    fi
    sleep 0.5
done
if [ "${r1:-0}" -lt 1 ] || [ "${r2:-0}" -lt 1 ]; then
    log "no routes"
    tail -40 "$OUT/node1.log" "$OUT/node2.log"
    exit 1
fi

ok=0
fail=0
: >"$OUT/pings.txt"
for _ in $(seq 1 "$COUNT"); do
    t0=$(date +%s%N)
    if timeout 5 "$CRYOC" -c "$OUT/a.ctl" ping 2 >/dev/null 2>&1; then
        ok=$((ok + 1))
    else
        fail=$((fail + 1))
    fi
    t1=$(date +%s%N)
    echo $(((t1 - t0) / 1000)) >>"$OUT/pings.txt"
done
awk -v d="$DELAY" -v ok="$ok" -v fail="$fail" \
    'BEGIN { min = 1e18; max = 0; sum = 0; n = 0 }
     { v = $1; if (v < min) min = v; if (v > max) max = v; sum += v; n++ }
     END {
         printf "delay=%s ok=%d fail=%d n=%d min=%.2fms avg=%.2fms max=%.2fms\n",
             d, ok, fail, n, min / 1000, (n ? sum / n / 1000 : 0), max / 1000
     }' "$OUT/pings.txt" | tee "$OUT/summary.txt"
log "out=$OUT"
