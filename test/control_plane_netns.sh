#!/usr/bin/env bash
# Control-plane-only scale test: no TUN, no full mesh, no ICE.
# Nodes talk only over the WebSocket mesh (IGP + registry).
# Run as root:  sudo env N=50 DELAY=50ms LOSS=1% test/run-netns.sh test/control_plane_netns.sh
#
# Env: N, TOPO=star|tree|hubs, HUBS (hub count), MODE=all|one (leaf attachment),
#      DELAY (e.g. 50ms), LOSS (e.g. 1%), HOLD, STABLE,
#      MAX_WAIT, POLL, OUT, BIN, CRYOC, TOKIO_WORKER_THREADS
set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
BIN=${BIN:-$ROOT/target/release/cryonet}
CRYOC=${CRYOC:-$ROOT/target/release/cryoc}
N=${N:-10}
TOPO=${TOPO:-star}
HUBS=${HUBS:-3}
MODE=${MODE:-all}
REGIONS=${REGIONS:-4}
EDGE=${EDGE:-2}
BORDER=${BORDER:-leaf}
DELAY=${DELAY:-}
LOSS=${LOSS:-}
HOLD=${HOLD:-30}
STABLE=${STABLE:-15}
MAX_WAIT=${MAX_WAIT:-300}
POLL=${POLL:-2}
POLL_NS=$(awk -v p="$POLL" 'BEGIN{printf "%d", p*1e9}')
WORKERS=${TOKIO_WORKER_THREADS:-1}
OUT=${OUT:-$ROOT/test/results/control-plane-n$N-$(date +%H%M%S)}

BR=crbr0
NET=10.200.0

# Node address: /16, 200 addresses per third octet so N can exceed 254.
naddr() { echo "10.200.$(( ($1 - 1) / 200 )).$(( ($1 - 1) % 200 + 1 ))"; }

mkdir -p "$OUT"
exec > >(tee -a "$OUT/run.log") 2>&1
log() { echo "[$(date +%H:%M:%S)] $*"; }
now() { date +%s%N; }
secs() { awk -v a="$1" -v b="$2" 'BEGIN{printf "%.2f", (a-b)/1e9}'; }

export TOKIO_WORKER_THREADS="$WORKERS"
export RUST_LOG="${RUST_LOG:-info,rustrtc=off}"

declare -A T0 TRT NEXT
cleanup() {
    log "cleanup"
    for i in $(seq 1 "$N"); do
        ip netns pids "cr$i" 2>/dev/null | xargs -r kill -9 2>/dev/null
    done
    sleep 0.3
    for i in $(seq 1 "$N"); do
        ip netns del "cr$i" 2>/dev/null
    done
    ip link del "$BR" 2>/dev/null
}
trap cleanup EXIT INT TERM

[ "$(id -u)" = 0 ] || { echo "must run as root"; exit 1; }
[ -x "$BIN" ] || { echo "missing $BIN"; exit 1; }
[ -x "$CRYOC" ] || { echo "missing $CRYOC"; exit 1; }

cleanup
log "N=$N topo=$TOPO hubs=$HUBS mode=$MODE regions=$REGIONS edge=$EDGE border=$BORDER delay=${DELAY:-none} loss=${LOSS:-none} workers=$WORKERS out=$OUT"

ip link add "$BR" type bridge
ip link set "$BR" up
for i in $(seq 1 "$N"); do
    ip netns add "cr$i"
    ip link add "vh$i" type veth peer name "vc$i"
    ip link set "vh$i" master "$BR"
    ip link set "vh$i" up
    ip link set "vc$i" netns "cr$i"
    ip -n "cr$i" addr add "$(naddr "$i")/16" dev "vc$i"
    ip -n "cr$i" link set "vc$i" up
    ip -n "cr$i" link set lo up
    if [ -n "$DELAY" ] || [ -n "$LOSS" ]; then
        tc_args=()
        [ -n "$DELAY" ] && tc_args+=(delay "$DELAY")
        [ -n "$LOSS" ] && tc_args+=(loss "$LOSS")
        ip netns exec "cr$i" tc qdisc add dev "vc$i" root netem "${tc_args[@]}" || log "WARN netem failed on cr$i"
    fi
done
log "netns + bridge ready"

start_node() {
    local i=$1 h r start prev pstart j size args=()
    case "$TOPO" in
        tree)
            [ "$i" -gt 1 ] && args=(--servers "ws://$(naddr $((i / 2))):2333")
            ;;
        hubs)
            # Nodes 1..HUBS are meshed together; leaves attach either to every
            # hub (MODE=all) or to a single hub (MODE=one, round-robin).
            if [ "$i" -le "$HUBS" ]; then
                for h in $(seq 1 $((i - 1))); do
                    args+=(--servers "ws://$(naddr "$h"):2333")
                done
            elif [ "$MODE" = one ]; then
                h=$(( (i - HUBS - 1) % HUBS + 1 ))
                args=(--servers "ws://$(naddr "$h"):2333")
            else
                for h in $(seq 1 "$HUBS"); do
                    args+=(--servers "ws://$(naddr "$h"):2333")
                done
            fi
            ;;
        regions)
            # REGIONS regions of ceil(N/REGIONS) nodes. The first HUBS nodes of
            # each region are hubs (meshed); the first EDGE hubs also connect to
            # the same-index hub of the previous region (ring). Leaves attach to
            # one local hub (MODE=one) or all local hubs (MODE=all).
            size=$(( (N + REGIONS - 1) / REGIONS ))
            r=$(( (i - 1) / size ))
            start=$(( r * size + 1 ))
            h=$(( i - start ))
            pend=$(( (r * size) + size ))
            [ "$pend" -gt "$N" ] && pend=$N
            if [ "$h" -lt "$HUBS" ]; then
                for j in $(seq 0 $((h - 1))); do
                    args+=(--servers "ws://$(naddr $((start + j))):2333")
                done
                if [ "$BORDER" = hub ] && [ "$h" -lt "$EDGE" ]; then
                    prev=$(( (r + REGIONS - 1) % REGIONS ))
                    pstart=$(( prev * size + 1 ))
                    if [ $((pstart + h)) -le "$N" ]; then
                        args+=(--servers "ws://$(naddr $((pstart + h))):2333")
                    fi
                fi
            else
                if [ "$MODE" = one ]; then
                    args=(--servers "ws://$(naddr $((start + (h - HUBS) % HUBS))):2333")
                else
                    for j in $(seq 0 $((HUBS - 1))); do
                        args+=(--servers "ws://$(naddr $((start + j))):2333")
                    done
                fi
                # Border leaves also peer with the previous region's border leaf.
                if [ "$BORDER" = leaf ] && [ $((h - HUBS)) -lt "$EDGE" ]; then
                    prev=$(( (r + REGIONS - 1) % REGIONS ))
                    pstart=$(( prev * size + 1 ))
                    ppend=$(( (prev + 1) * size ))
                    [ "$ppend" -gt "$N" ] && ppend=$N
                    remote=$(( pstart + HUBS + (h - HUBS) ))
                    if [ "$remote" -le "$ppend" ]; then
                        args+=(--servers "ws://$(naddr "$remote"):2333")
                    fi
                fi
            fi
            ;;
        *)
            [ "$i" -gt 1 ] && args=(--servers "ws://$(naddr 1):2333")
            ;;
    esac
    # The daemon no longer exposes --disable-fullmesh / --disable-registry:
    # filtering out every ICE candidate keeps the data plane from forming, so
    # only the WebSocket control plane is exercised.
    ip netns exec "cr$i" "$BIN" \
        --candidate-filter-prefixes 0.0.0.0/0,::/0 --listen 0.0.0.0:2333 --ctl-path "$OUT/node$i.ctl" "${args[@]}" \
        >"$OUT/node$i.log" 2>&1 &
    T0[$i]=$(now)
    NEXT[$i]=0
}

route_count() {
    local count
    count=$(timeout 5 "$CRYOC" -c "$OUT/node$1.ctl" stats 2>/dev/null \
        | sed -n 's/^  MESH .*routes \([0-9][0-9]*\).*/\1/p')
    echo "${count:-0}"
}

counters() { # rx_b rx_p tx_b tx_p
    ip -n "cr$1" -s link show "vc$1" | awk '
        /RX:/ {getline; rb=$1; rp=$2}
        /TX:/ {getline; tb=$1; tp=$2}
        END {print rb+0, rp+0, tb+0, tp+0}'
}

log "starting $N nodes"
for i in $(seq 1 "$N"); do
    start_node "$i"
done
T_LAST=$(now)
log "all nodes started"

: >"$OUT/timeseries.csv"
echo "t_s,node,routes" >>"$OUT/timeseries.csv"
DEADLINE=$(( $(now) / 1000000000 + MAX_WAIT ))
while :; do
    tns=$(now)
    pending=0
    for i in $(seq 1 "$N"); do
        [ -n "${TRT[$i]:-}" ] && continue
        if [ "$tns" -lt "${NEXT[$i]}" ]; then pending=$((pending + 1)); continue; fi
        NEXT[$i]=$(( tns + POLL_NS ))
        [ -S "$OUT/node$i.ctl" ] || { pending=$((pending + 1)); continue; }
        c=$(route_count "$i")
        echo "$(secs "$tns" "${T0[$i]}"),$i,$c" >>"$OUT/timeseries.csv"
        if [ "${c:-0}" -ge $((N - 1)) ]; then
            TRT[$i]=$(now)
            log "node $i converged ($(secs "${TRT[$i]}" "${T0[$i]}")s after start)"
        else
            pending=$((pending + 1))
        fi
    done
    [ "$pending" = 0 ] && break
    if [ $(( $(now) / 1000000000 )) -ge "$DEADLINE" ]; then
        log "TIMEOUT: $pending node(s) did not converge"
        break
    fi
    sleep 0.2
done
T_CONV=$(now)
CONV_DONE=0
[ -z "$(for i in $(seq 1 "$N"); do [ -z "${TRT[$i]:-}" ] && echo x; done)" ] && CONV_DONE=1
log "converged all=$CONV_DONE wall=$(secs "$T_CONV" "$T_LAST")s"

# Route stability: minimum route count over the window (drop == flap).
declare -A RMIN
for i in $(seq 1 "$N"); do RMIN[$i]=999999; done
: >"$OUT/stability.csv"
echo "t_s,node,routes" >>"$OUT/stability.csv"
for _ in $(seq 1 $((STABLE / 2))); do
    for i in $(seq 1 "$N"); do
        c=$(route_count "$i")
        echo "$(secs "$(now)" "$T_LAST"),$i,$c" >>"$OUT/stability.csv"
        [ "${c:-0}" -lt "${RMIN[$i]}" ] && RMIN[$i]=$c
    done
    sleep 2
done

# Steady-state counters + CPU.
declare -A PID RB0 RP0 TB0 TP0 UT0
for i in $(seq 1 "$N"); do
    PID[$i]=$(ip netns pids "cr$i" 2>/dev/null | head -1)
    read -r rb rp tb tp < <(counters "$i")
    RB0[$i]=$rb; RP0[$i]=$rp; TB0[$i]=$tb; TP0[$i]=$tp
    [ -n "${PID[$i]:-}" ] && UT0[$i]=$(awk '{print $14+$15}' "/proc/${PID[$i]}/stat" 2>/dev/null || echo 0)
done
# Sample the hub (node 1) once per second during HOLD to capture announce bursts.
PEAK_TX_PPS=0 PEAK_RX_PPS=0 PEAK_TX_BPS=0 PEAK_RX_BPS=0
read -r prb prp ptb ptp < <(counters 1)
for _ in $(seq 1 "$HOLD"); do
    sleep 1
    read -r rb rp tb tp < <(counters 1)
    d=$((tp - ptp)); [ "$d" -gt "$PEAK_TX_PPS" ] && PEAK_TX_PPS=$d
    d=$((rp - prp)); [ "$d" -gt "$PEAK_RX_PPS" ] && PEAK_RX_PPS=$d
    d=$((tb - ptb)); [ "$d" -gt "$PEAK_TX_BPS" ] && PEAK_TX_BPS=$d
    d=$((rb - prb)); [ "$d" -gt "$PEAK_RX_BPS" ] && PEAK_RX_BPS=$d
    prb=$rb prp=$rp ptb=$tb ptp=$tp
done
CLK=$(getconf CLK_TCK)
echo "node,conv_s,routes,routes_min,rss_kb,fds,threads,cpu_pct,rx_bps,rx_pps,tx_bps,tx_pps" >"$OUT/summary.csv"
for i in $(seq 1 "$N"); do
    p=${PID[$i]:-}
    rss=0 fds=0 thr=0 cpu=0
    if [ -n "$p" ] && [ -d "/proc/$p" ]; then
        rss=$(awk '/VmRSS/{print $2}' "/proc/$p/status")
        thr=$(awk '/Threads/{print $2}' "/proc/$p/status")
        fds=$(ls "/proc/$p/fd" | wc -l)
        u1=$(awk '{print $14+$15}' "/proc/$p/stat")
        cpu=$(awk -v d=$((u1 - ${UT0[$i]:-u1})) -v h="$HOLD" -v c="$CLK" 'BEGIN{printf "%.2f", d/h/c*100}')
    fi
    read -r rb rp tb tp < <(counters "$i")
    conv="-"
    [ -n "${TRT[$i]:-}" ] && conv=$(secs "${TRT[$i]}" "${T0[$i]}")
    echo "$i,$conv,$(route_count "$i"),${RMIN[$i]},$rss,$fds,$thr,$cpu,$(awk -v d=$((rb - ${RB0[$i]})) -v h="$HOLD" 'BEGIN{printf "%.0f", d/h}'),$(awk -v d=$((rp - ${RP0[$i]})) -v h="$HOLD" 'BEGIN{printf "%.1f", d/h}'),$(awk -v d=$((tb - ${TB0[$i]})) -v h="$HOLD" 'BEGIN{printf "%.0f", d/h}'),$(awk -v d=$((tp - ${TP0[$i]})) -v h="$HOLD" 'BEGIN{printf "%.1f", d/h}')" >>"$OUT/summary.csv"
done

log "=== summary (N=$N topo=$TOPO hubs=$HUBS mode=$MODE regions=$REGIONS edge=$EDGE border=$BORDER delay=${DELAY:-none} loss=${LOSS:-none}) ==="
head -1 "$OUT/summary.csv"
awk -F, -v rmin=1e9 -v p_txp="$PEAK_TX_PPS" -v p_rxp="$PEAK_RX_PPS" -v p_txb="$PEAK_TX_BPS" -v p_rxb="$PEAK_RX_BPS" 'NR>1 {
    n++; if ($2!="-") { j++; if ($2>jm) jm=$2 }
    rss+=$5; fd+=$6; cpu+=$8; tx+=$11; rx+=$9
    if ($8>cpu_max) cpu_max=$8
    if ($4<rmin) rmin=$4
} END {
    printf "conv_max=%.1fs rss_avg=%.1fMB fd_avg=%.0f cpu_avg=%.2f%% cpu_max=%.2f%% tx_avg=%.0fB/s rx_avg=%.0fB/s routes_min=%d hub_peak_tx=%.0fB/s(%dpps) hub_peak_rx=%.0fB/s(%dpps) (n=%d)\n",
        jm, rss/n/1024, fd/n, cpu/n, cpu_max, tx/n, rx/n, rmin, p_txb, p_txp, p_rxb, p_rxp, n
}' "$OUT/summary.csv" | tee -a "$OUT/final.txt"
log "converged=$CONV_DONE wall=$(secs "$T_CONV" "$T_LAST")s out=$OUT"
