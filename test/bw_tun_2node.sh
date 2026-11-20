#!/usr/bin/env bash
# Two-node data-plane bandwidth test across a direct veth pair.
#   baseline: iperf3 straight over the veth (no cryonet)
#   cryonet:  iperf3 over the per-peer TUN devices
# Run as root:  sudo test/run-netns.sh test/bw_tun_2node.sh
#
# Env:
#   BIN CRYOC IPERF3 OUT TUN_MTU=1280 DURATION=10
#   WORKERS (tokio worker threads, empty = default)
#   MSS=1240            baseline TCP MSS, matched to TUN_MTU-40 by default
#   ENCRYPT=0           pass --encrypt-local-packets
#   CPU1=2 CPU2=3 CPIC=4 CPIS=5   process pinning
#   SKIP_BASE=0         skip the no-cryonet baseline
#   UDP=0               also run an unlimited-rate UDP iperf3 test
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
CRYOC=${CRYOC:-$ROOT/target/release/cryoc}
IPERF3=${IPERF3:-$(command -v iperf3 2>/dev/null || ls -d /nix/store/*-iperf-3*/bin/iperf3 2>/dev/null | head -1)}
TUN_MTU=${TUN_MTU:-1280}
TUN_QLEN=${TUN_QLEN:-}
SET_MTU=${SET_MTU:-1}
CRYONET_ARGS=${CRYONET_ARGS:-}
DURATION=${DURATION:-10}
MODE=${MODE:-tun}
WORKERS=${WORKERS:-}
MSS=${MSS:-$((TUN_MTU - 40))}
ENCRYPT=${ENCRYPT:-0}
SKIP_BASE=${SKIP_BASE:-0}
BASE_UDP=${BASE_UDP:-0}
UDP=${UDP:-0}
CPU1=${CPU1:-2}
CPU2=${CPU2:-3}
CPIC=${CPIC:-4}
CPIS=${CPIS:-5}
OUT=${OUT:-$ROOT/test/results/bw-tun-2node-$(date +%Y%m%d-%H%M%S)}
NS1=bw2a-$$
NS2=bw2b-$$
V1=bwv1-$$
V2=bwv2-$$
A=10.200.0.1
B=10.200.0.2
TA=10.100.0.1
TB=10.100.0.2

mkdir -p "$OUT"
exec > >(tee -a "$OUT/run.log") 2>&1
log() { echo "[$(date +%H:%M:%S)] $*"; }
now() { date +%s%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.3f", (a-b)/1e9}'; }
CLK=$(getconf CLK_TCK)

cleanup() {
    log "cleanup (rc=$? last=$BASH_COMMAND)"
    if [ -n "${SUDO_USER:-}" ] && [ "$SUDO_USER" != root ]; then
        chown -R "$SUDO_USER": "$OUT" 2>/dev/null || true
    fi
    for ns in "$NS1" "$NS2"; do
        ip netns pids "$ns" 2>/dev/null | xargs -r kill -9 2>/dev/null
    done
    sleep 0.2
    for ns in "$NS1" "$NS2"; do ip netns del "$ns" 2>/dev/null; done
}
trap cleanup EXIT INT TERM

[ "$(id -u)" = 0 ] || { echo "must run as root"; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }
[ -x "$CRYOC" ] || { echo "missing $CRYOC"; exit 1; }
[ -n "$IPERF3" ] || { echo "missing iperf3"; exit 1; }

proc_cpu() { # pid -> utime+stime ticks
    local p=$1
    [ -r "/proc/$p/stat" ] || { echo 0; return; }
    awk '{print $14+$15}' "/proc/$p/stat" 2>/dev/null || echo 0
}
proc_rss() { local p=$1; awk '/VmRSS/{print $2}' "/proc/$p/status" 2>/dev/null; }
proc_threads() { local p=$1; awk '/Threads/{print $2}' "/proc/$p/status" 2>/dev/null; }
snapshot_cpu() { # tag
    local tag=$1 i
    : >"$OUT/cpu-$tag.before"
    : >"$OUT/cores-$tag.before"
    for i in 1 2; do
        local pid=${NODE_PID[$i]:-}
        [ -n "$pid" ] && echo "$i $pid $(( $(proc_cpu "$pid") ))" >>"$OUT/cpu-$tag.before"
    done
    while read -r cpu user nice system idle iowait irq softirq steal guest guest_nice; do
        [[ "$cpu" =~ ^cpu[0-9]+$ ]] && echo "${cpu#cpu} $((user+nice+system+irq+softirq+steal)) $((idle+iowait))" >>"$OUT/cores-$tag.before"
    done </proc/stat
}
snapshot_cpu_after() { # tag elapsed_ns
    local tag=$1 elapsed_ns=$2 i
    : >"$OUT/cpu-$tag.after"
    for i in 1 2; do
        local pid=${NODE_PID[$i]:-}
        [ -n "$pid" ] && echo "$i $pid $(( $(proc_cpu "$pid") ))" >>"$OUT/cpu-$tag.after"
    done
    : >"$OUT/cores-$tag.after"
    while read -r cpu user nice system idle iowait irq softirq steal guest guest_nice; do
        [[ "$cpu" =~ ^cpu[0-9]+$ ]] && echo "${cpu#cpu} $((user+nice+system+irq+softirq+steal)) $((idle+iowait))" >>"$OUT/cores-$tag.after"
    done </proc/stat
    awk -v e="$elapsed_ns" -v c="$CLK" '
        NR==FNR { b[$1]=$3; next }
        { d=$3-b[$1]; printf "node%s_cpu_pct=%.2f\n", $1, d/c/(e/1e9)*100 }
    ' "$OUT/cpu-$tag.before" "$OUT/cpu-$tag.after" >>"$OUT/timings.txt"
    awk -v e="$elapsed_ns" '
        NR==FNR { bbusy[$1]=$2; bidle[$1]=$3; next }
        { dt=($2-bbusy[$1])+($3-bidle[$1]); if (dt>0) busy=($2-bbusy[$1])/dt*100; else busy=0;
          printf "cpu%s %.1f\n", $1, busy }
    ' "$OUT/cores-$tag.before" "$OUT/cores-$tag.after" | sort -k2 -nr | head -6 | sed 's/^/  /' >>"$OUT/timings.txt"
}

start_iperf_server() { # ns cpu tag
    local ns=$1 cpu=$2 tag=$3
    ip netns exec "$ns" taskset -c "$cpu" "$IPERF3" -s -1 >"$OUT/iperf-server-$tag.log" 2>&1 &
    SERV_PID=$!
    for _ in $(seq 1 50); do
        kill -0 "$SERV_PID" 2>/dev/null || break
        ip netns exec "$ns" ss -ltn 2>/dev/null | grep -q ':5201' && break
        sleep 0.1
    done
    sleep 0.2
}
stop_iperf_server() { kill "${SERV_PID:-}" 2>/dev/null; wait "${SERV_PID:-}" 2>/dev/null; }

parse_iperf_udp() { # file -> "sent_bps recv_bps"
    awk '
        /sender$/   { s=$(NF-6) " " $(NF-5) }
        /receiver$/ { r=$(NF-6) " " $(NF-5) }
        END {
            split(s,a," "); split(r,b," ");
            f=(a[2]=="Gbits/sec")?1e9:((a[2]=="Mbits/sec")?1e6:((a[2]=="Kbits/sec")?1e3:1));
            g=(b[2]=="Gbits/sec")?1e9:((b[2]=="Mbits/sec")?1e6:((b[2]=="Kbits/sec")?1e3:1));
            printf "%.0f %.0f", a[1]*f, b[1]*g
        }' "$1"
}

parse_iperf_text() { # file -> "sent_bps recv_bps"
    awk '
        /sender$/   { s=$(NF-3) " " $(NF-2) }
        /receiver$/ { r=$(NF-2) " " $(NF-1) }
        END {
            split(s,a," "); split(r,b," ");
            f=(a[2]=="Gbits/sec")?1e9:((a[2]=="Mbits/sec")?1e6:((a[2]=="Kbits/sec")?1e3:1));
            g=(b[2]=="Gbits/sec")?1e9:((b[2]=="Mbits/sec")?1e6:((b[2]=="Kbits/sec")?1e3:1));
            printf "%.0f %.0f", a[1]*f, b[1]*g
        }' "$1"
}

run_iperf() { # tag ns_client ns_server cpu_client cpu_server target extra...
    local tag=$1 cns=$2 sns=$3 cpcpu=$4 spcpu=$5 target=$6
    shift 6
    start_iperf_server "$sns" "$spcpu" "$tag"
    local t0 t1
    snapshot_cpu "iperf-$tag"
    t0=$(now)
    ip netns exec "$cns" taskset -c "$cpcpu" "$IPERF3" -c "$target" -t "$DURATION" -O 1 -i 0 "$@" >"$OUT/iperf-$tag.log" 2>&1 || log "WARN iperf $tag failed"
    t1=$(now)
    snapshot_cpu_after "iperf-$tag" "$((t1 - t0))"
    stop_iperf_server
    local r
    case " $* " in *" -u "*) r=$(parse_iperf_udp "$OUT/iperf-$tag.log");; *) r=$(parse_iperf_text "$OUT/iperf-$tag.log");; esac
    echo "$tag $(secs "$t1" "$t0") $r" >>"$OUT/throughput.txt"
    log "iperf $tag: sent=$(awk -v v="${r%% *}" 'BEGIN{printf "%.2f Gbit/s",v/1e9}') recv=$(awk -v v="${r##* }" 'BEGIN{printf "%.2f Gbit/s",v/1e9}') in $(secs "$t1" "$t0")s"
}

link_stats() { # tag
    local i ns v d
    {
        for i in 1 2; do
            ns="NS$i"; v="V$i"
            echo "--- ns$i veth ---"; ip -n "${!ns}" -s link show "${!v}"
            for d in cn0; do
                ip -n "${!ns}" link show "$d" >/dev/null 2>&1 && { echo "--- ns$i $d ---"; ip -n "${!ns}" -s link show "$d"; }
            done
        done
    } >>"$OUT/link-stats-$1.txt" 2>&1
}

# ---------------------------------------------------------------- setup
cleanup
log "OUT=$OUT mode=$MODE tun_mtu=$TUN_MTU duration=$DURATION workers=${WORKERS:-default} encrypt=$ENCRYPT"
ip netns add "$NS1"; ip netns add "$NS2"
ip link add "$V1" type veth peer name "$V2"
ip link set "$V1" netns "$NS1"; ip link set "$V2" netns "$NS2"
ip -n "$NS1" addr add "$A/24" dev "$V1"; ip -n "$NS1" link set lo up; ip -n "$NS1" link set "$V1" up mtu 1500
ip -n "$NS2" addr add "$B/24" dev "$V2"; ip -n "$NS2" link set lo up; ip -n "$NS2" link set "$V2" up mtu 1500
sleep 0.3
ip netns exec "$NS1" ping -c1 -W2 -q "$B" >/dev/null || { log "ERROR underlay ping failed"; exit 1; }
log "underlay up: $A <-> $B"

: >"$OUT/throughput.txt"
echo "test wall_s sent_bps recv_bps" >"$OUT/throughput.txt"

declare -A NODE_PID
if [ "$SKIP_BASE" != 1 ]; then
    # Baseline without cryonet. -M matches the TUN-era TCP MSS.
    run_iperf base-veth-fwd "$NS1" "$NS2" "$CPIC" "$CPIS" "$B" -M "$MSS"
    run_iperf base-veth-rev "$NS1" "$NS2" "$CPIC" "$CPIS" "$B" -M "$MSS" -R
    link_stats base
fi
if [ "$BASE_UDP" = 1 ]; then
    run_iperf base-veth-udp "$NS1" "$NS2" "$CPIC" "$CPIS" "$B" -u -b 0 -l 1200
fi

EXTRA=()
[ "$ENCRYPT" = 1 ] && EXTRA+=(--encrypt-local-packets)
MODE_ARGS=(--interface-prefix cn)

start_node() { # ns id ctl cpu [server]
    local ns=$1 id=$2 ctl=$3 cpu=$4 srv=${5:-}
    local args=() extra_args=()
    [ -n "$srv" ] && args=(--servers "$srv")
    [ -n "$CRYONET_ARGS" ] && read -ra extra_args <<<"$CRYONET_ARGS"
    ip netns exec "$ns" env ${WORKERS:+TOKIO_WORKER_THREADS=$WORKERS} RUST_LOG="${RUST_LOG:-info,rustrtc=off}" \
        taskset -c "$cpu" "$BIN" "${MODE_ARGS[@]}" \
        --listen 0.0.0.0:2333 --ctl-path "$ctl" "${args[@]}" "${EXTRA[@]}" "${extra_args[@]}" \
        >"$OUT/node$id.log" 2>&1 &
    NODE_PID[$id]=$!
}

link_up() { # ns dev
    local mtu_args=() q_args=()
    [ "$SET_MTU" = 1 ] && mtu_args=(mtu "$TUN_MTU")
    [ -n "$TUN_QLEN" ] && q_args=(txqueuelen "$TUN_QLEN")
    ip -n "$1" link set "$2" up "${mtu_args[@]}" "${q_args[@]}"
}

log "starting node 1 and 2 (mode=$MODE)"
start_node "$NS1" 1 "$OUT/a.ctl" "$CPU1"
start_node "$NS2" 2 "$OUT/b.ctl" "$CPU2" "ws://$A:2333"
for _ in $(seq 1 300); do
    ip -n "$NS1" link show cn0 >/dev/null 2>&1 && ip -n "$NS2" link show cn0 >/dev/null 2>&1 && break
    sleep 0.2
done
ip -n "$NS1" addr add "$TA/32" dev cn0; link_up "$NS1" cn0
ip -n "$NS2" addr add "$TB/32" dev cn0; link_up "$NS2" cn0
ip -n "$NS1" route add "$TB/32" dev cn0
ip -n "$NS2" route add "$TA/32" dev cn0

log "waiting for data path"
ok=0
for i in $(seq 1 180); do
    if ip netns exec "$NS1" ping -c1 -W1 -q "$TB" >/dev/null 2>&1; then ok=1; break; fi
    sleep 1
done
[ "$ok" = 1 ] || { log "ERROR data path down"; tail -40 "$OUT/node1.log"; tail -40 "$OUT/node2.log"; exit 1; }
log "data path up after ${i}s"
"$CRYOC" -c "$OUT/a.ctl" fmp >"$OUT/fmp-a.txt" 2>&1 || true
"$CRYOC" -c "$OUT/a.ctl" stats >"$OUT/stats-a-before.txt" 2>&1 || true

log "MTU/mss: cn0=$(ip -n "$NS1" link show cn0 | sed -n 's/.*mtu \([0-9]*\).*/\1/p')"
ip netns exec "$NS1" "$IPERF3" -c "$TB" -t 2 -O 1 -i 0 >/dev/null 2>&1 || true

run_iperf tun-fwd "$NS1" "$NS2" "$CPIC" "$CPIS" "$TB"
run_iperf tun-rev "$NS1" "$NS2" "$CPIC" "$CPIS" "$TB" -R

if [ "$UDP" = 1 ]; then
    run_iperf tun-udp-fwd "$NS1" "$NS2" "$CPIC" "$CPIS" "$TB" -u -b 0 -l 1200
fi

"$CRYOC" -c "$OUT/a.ctl" stats >"$OUT/stats-a-after.txt" 2>&1 || true
"$CRYOC" -c "$OUT/b.ctl" stats >"$OUT/stats-b-after.txt" 2>&1 || true
link_stats cryonet
log "MARK-after-link-stats"

log "=== throughput ==="
cat "$OUT/throughput.txt"
log "=== cpu (per test window) ==="
cat "$OUT/timings.txt"
log "out=$OUT"
