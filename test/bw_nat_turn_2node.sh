#!/usr/bin/env bash
# Two-node TUN data plane behind NAT, forced through a local coturn/TURN server.
# Run as root: sudo test/run-netns.sh test/bw_nat_turn_2node.sh
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
IPERF3=${IPERF3:-$(command -v iperf3 2>/dev/null || ls -d /nix/store/*-iperf-3*/bin/iperf3 2>/dev/null | head -1)}
TURNBIN=${TURNBIN:-$(command -v turnserver 2>/dev/null || ls -d /nix/store/*coturn-*/bin/turnserver 2>/dev/null | head -1)}
IPTABLES=${IPTABLES:-iptables}
OUT=${OUT:-$ROOT/test/results/bw-nat-turn-2node-$(date +%Y%m%d-%H%M%S)}
DURATION=${DURATION:-5}
TURN_PORT=${TURN_PORT:-3478}
TURN_USER=${TURN_USER:-test}
TURN_PASS=${TURN_PASS:-test}
TURN_REALM=${TURN_REALM:-test}
TURN_HOST=10.10.0.2
TA=10.100.0.1
TB=10.100.0.2
NS_SRV=srv-$$
NS_RTR=rtr-$$
NS_CLI=cli-$$
V_SRV=nvs-$$
V_RTR1=nvr1-$$
V_CLI=nvc-$$
V_RTR2=nvr2-$$

[ "$(id -u)" = 0 ] || { echo "must run as root" >&2; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN" >&2; exit 1; }
[ -n "$IPERF3" ] || { echo "missing iperf3" >&2; exit 1; }
[ -n "$TURNBIN" ] || { echo "missing turnserver; set TURNBIN" >&2; exit 1; }
mkdir -p "$OUT"

log() { echo "[$(date +%H:%M:%S)] $*"; }
cleanup() {
    for ns in "$NS_SRV" "$NS_RTR" "$NS_CLI"; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
    done
    for ns in "$NS_SRV" "$NS_RTR" "$NS_CLI"; do
        ip netns del "$ns" 2>/dev/null
    done
    ip link del "$V_SRV" 2>/dev/null
    ip link del "$V_CLI" 2>/dev/null
}
trap cleanup EXIT INT TERM

cleanup
ip netns add "$NS_SRV"
ip netns add "$NS_RTR"
ip netns add "$NS_CLI"
ip link add "$V_SRV" type veth peer name "$V_RTR1"
ip link add "$V_CLI" type veth peer name "$V_RTR2"
ip link set "$V_SRV" netns "$NS_SRV"
ip link set "$V_RTR1" netns "$NS_RTR"
ip link set "$V_CLI" netns "$NS_CLI"
ip link set "$V_RTR2" netns "$NS_RTR"
for ns in "$NS_SRV" "$NS_RTR" "$NS_CLI"; do
    ip -n "$ns" link set lo up
done
ip -n "$NS_SRV" addr add "$TURN_HOST/24" dev "$V_SRV"
ip -n "$NS_SRV" link set "$V_SRV" up
ip -n "$NS_RTR" addr add 10.10.0.1/24 dev "$V_RTR1"
ip -n "$NS_RTR" link set "$V_RTR1" up
ip -n "$NS_CLI" addr add 10.20.0.2/24 dev "$V_CLI"
ip -n "$NS_CLI" link set "$V_CLI" up
ip -n "$NS_RTR" addr add 10.20.0.1/24 dev "$V_RTR2"
ip -n "$NS_RTR" link set "$V_RTR2" up
ip -n "$NS_CLI" route add default via 10.20.0.1
ip -n "$NS_SRV" route add 10.20.0.0/24 via 10.10.0.1
ip netns exec "$NS_RTR" sh -c 'echo 1 > /proc/sys/net/ipv4/ip_forward'
ip netns exec "$NS_RTR" "$IPTABLES" -t nat -A POSTROUTING \
    -s 10.20.0.0/24 -o "$V_RTR1" -j MASQUERADE
ip netns exec "$NS_RTR" "$IPTABLES" -A FORWARD -m conntrack \
    --ctstate ESTABLISHED,RELATED -j ACCEPT
ip netns exec "$NS_RTR" "$IPTABLES" -A FORWARD -p udp \
    -s 10.20.0.0/24 -d "$TURN_HOST" --dport "$TURN_PORT" -j ACCEPT
ip netns exec "$NS_RTR" "$IPTABLES" -A FORWARD -p udp \
    -s 10.20.0.0/24 -d 10.10.0.0/24 -j DROP
ip netns exec "$NS_RTR" "$IPTABLES" -A FORWARD -p udp \
    -d 10.20.0.0/24 -j DROP
ip netns exec "$NS_RTR" "$IPTABLES" -A FORWARD -j ACCEPT

log "start coturn"
ip netns exec "$NS_SRV" "$TURNBIN" \
    -a -u "$TURN_USER:$TURN_PASS" -r "$TURN_REALM" --allow-loopback-peers \
    -L "$TURN_HOST" -E "$TURN_HOST" -p "$TURN_PORT" \
    --no-tcp --no-tls --no-dtls --no-stdout-log \
    --log-file "$OUT/turn.log" --pidfile "$OUT/turn.pid" -n \
    >"$OUT/turn-stdout.log" 2>&1 &
TURN_PID=$!
sleep 1
kill -0 "$TURN_PID" 2>/dev/null || {
    log "coturn failed to start"
    tail -40 "$OUT/turn-stdout.log" "$OUT/turn.log" 2>/dev/null
    exit 1
}
TURN_URL="turn:$TURN_HOST:$TURN_PORT|$TURN_USER|$TURN_PASS"

log "start cryonet nodes"
RUST_LOG=warn ip netns exec "$NS_SRV" "$BIN" \
    --interface-prefix cn --listen 0.0.0.0:2333 --ctl-path "$OUT/a.ctl" \
    --ice-servers "$TURN_URL" >"$OUT/node1.log" 2>&1 &
RUST_LOG=warn ip netns exec "$NS_CLI" "$BIN" \
    --interface-prefix cn --servers "ws://$TURN_HOST:2333" \
    --listen 0.0.0.0:2333 --ctl-path "$OUT/b.ctl" \
    --ice-servers "$TURN_URL" >"$OUT/node2.log" 2>&1 &

ok=0
for _ in $(seq 1 180); do
    if ip -n "$NS_SRV" link show cn0 >/dev/null 2>&1 \
        && ip -n "$NS_CLI" link show cn0 >/dev/null 2>&1; then
        ok=1
        break
    fi
    sleep 0.5
done
log "tun devices up=$ok"
if [ "$ok" != 1 ]; then
    tail -40 "$OUT/node1.log" "$OUT/node2.log"
    exit 1
fi
ip -n "$NS_SRV" addr add "$TA/24" dev cn0
ip -n "$NS_SRV" link set cn0 up mtu 1280
ip -n "$NS_CLI" addr add "$TB/24" dev cn0
ip -n "$NS_CLI" link set cn0 up mtu 1280

ok=0
for _ in $(seq 1 60); do
    if ip netns exec "$NS_SRV" ping -c1 -W1 -q "$TB" >/dev/null 2>&1; then
        ok=1
        break
    fi
    sleep 0.5
done
log "data path up=$ok"
if [ "$ok" != 1 ]; then
    tail -40 "$OUT/node1.log" "$OUT/node2.log"
    exit 1
fi

run_iperf() {
    local tag=$1
    shift
    ip netns exec "$NS_CLI" "$IPERF3" -s -1 >"$OUT/server-$tag.log" 2>&1 &
    local server_pid=$!
    for _ in $(seq 1 30); do
        kill -0 "$server_pid" 2>/dev/null || break
        ip netns exec "$NS_CLI" ss -ltn 2>/dev/null | grep -q ':5201' && break
        sleep 0.1
    done
    ip netns exec "$NS_SRV" "$IPERF3" -c "$TB" -t "$DURATION" -O 1 -i 0 "$@" \
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
