#!/usr/bin/env bash
# Benchmark matrix: peer comparison × network cells × service types, loopback.
#
# Topology (every run, 127.0.0.1):
#   visitor -> proxy-server -> proxy-client -> backend (iperf3 / echo / udp-echo)
#
# Network cells (CELLS):
#   "0/0"       loopback
#   "0/10"      rtt only                 (netem, else userspace weakproxy)
#   "1%/10"     loss% / rtt_ms           (netem; skipped without CAP_NET_ADMIN)
#   "2:25/10"   loss% : burst% / rtt_ms  (netem correlated loss)
#   "r10/50"    rate 10 mbit + rtt_ms    (netem; congestion-policy cells)
#   netem runs on `lo` (needs CAP_NET_ADMIN; shapes the whole loopback path,
#   backend leg included — known limitation, netns isolation is future work),
#   otherwise the userspace weakproxy adds rtt/2 per direction on the
#   client<->server leg only; loss cells are skipped without netem, since a
#   userspace proxy cannot drop packets before the kernel ACKs them (UDP
#   excepted: weakproxy --udp can drop datagrams, for visitor-side scenarios).
#
# Per tool per cell:
#   - iperf3 TCP throughput, 1 and 8 streams (median of REPS; retransmits kept)
#   - echo connection-path RTT, 300 sequential connections (ms percentiles)
#   - TCP data-path RTT: steady ping over ONE established connection
#   - UDP session quality: steady ping over ONE established UDP session —
#       RTT percentiles, end-to-end loss %, jitter, max inter-packet gap
#   - HoL probe: saturating bulk flow + game-like pinger concurrently over the
#     same service (head-of-line visibility; per-stream isolation check)
#   - resident memory (RSS) of the tool's server+client, sampled during the run
#
# Tools (pin versions in fetch_peers.sh): molehill (current tree; variants via
# MOLEHILL_VARIANTS="mux,noise" — mux-off runs on the loopback cell), frp,
# rathole (upstream), bore, chisel. UDP arms: molehill / frp / rathole /
# chisel forward UDP; bore is TCP-only (its UDP metrics are omitted and the
# regression gate skips them). Full matrix runtime is roughly an hour — trim
# with TOOLS / CELLS / MOLEHILL_VARIANTS.
#
# Output: results-vX.Y.Z.json (version read from Cargo.toml), schema v3:
#   meta{date, hostname, kernel, cpu, reps, secs, cells[], tool_versions{}}
#   results[tool][cell]{throughput_1stream_gbps, throughput_8streams_gbps,
#                       retransmits_1stream, retransmits_8streams,
#                       echo_rtt_ms{p50,p95,p99,mean}, tcp_steady_rtt_ms{...},
#                       udp_rtt_ms{...}, udp_loss_pct, udp_jitter_ms,
#                       udp_max_gap_ms, hol{bulk_gbps, ping_rtt_ms{...},
#                       ping_max_gap_ms}, hol_udp{...}, memory_rss_kb{...}}
# Regression gate: check_regression.sh compares against the previous tag's file
# (metrics absent from an older baseline are skipped).
set -u

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/../../.." && pwd)

MOLEHILL_BIN=${MOLEHILL_BIN:-$REPO_ROOT/target/release/molehill}
PEER_DIR=${PEER_DIR:-/tmp/bench-peers}
REPS=${REPS:-3}
SECS=${SECS:-8}
SECS_WEAK=${SECS_WEAK:-15}
HOL_SECS=${HOL_SECS:-8}
# HoL bulk pacing: a paced bulk keeps the pinger alive so the metric stays
# comparable across arms; 0 = saturate (stress mode). Loopback has huge spare
# capacity — the paced numbers differentiate arms primarily in weak cells.
HOL_BULK_RATE_TCP=${HOL_BULK_RATE_TCP:-200}
HOL_BULK_RATE_UDP=${HOL_BULK_RATE_UDP:-50}
CELLS=${CELLS:-"0/0 0/10 0/100 1%/10 5%/100 2:25/10"}
MOLEHILL_VARIANTS=${MOLEHILL_VARIANTS:-"mux noise"}
POOL_SIZE=${POOL_SIZE:-8}
TOOLS=${TOOLS:-"molehill frp rathole bore chisel"}
OUT=${OUT:-}
[ -n "$OUT" ] || OUT="$SCRIPT_DIR/results-v$(grep -m1 '^version' "$REPO_ROOT/Cargo.toml" | sed 's/.*"\(.*\)".*/\1/').json"

command -v iperf3 >/dev/null || { echo "iperf3 required (see 'just bench-deps')" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 required" >&2; exit 1; }
[ -x "$MOLEHILL_BIN" ] || { echo "missing $MOLEHILL_BIN (cargo build --release)" >&2; exit 1; }

WORK=$(mktemp -d /tmp/molehill-bench.XXXXXX)
SRV_PID=""; CLI_PID=""; MEM_PID=""; MEM_STOP=""

cleanup() {
    for pid in "${MEM_PID:-}" "${CLI_PID:-}" "${SRV_PID:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    done
    pkill -x molehill 2>/dev/null; pkill -x frps 2>/dev/null; pkill -x frpc 2>/dev/null
    pkill -x rathole 2>/dev/null; pkill -x bore 2>/dev/null; pkill -x chisel 2>/dev/null
    pkill -x iperf3 2>/dev/null
    pkill -f "weakproxy.py" 2>/dev/null
    sudo tc qdisc del dev lo root 2>/dev/null || true
}
trap cleanup EXIT

wait_port() { # port timeout_s
    python3 - "$1" "$2" <<'PYWAIT'
import socket, sys, time
port, tmo = int(sys.argv[1]), float(sys.argv[2])
end = time.time() + tmo
while time.time() < end:
    try:
        s = socket.create_connection(("127.0.0.1", port), timeout=0.5)
        s.close()
        sys.exit(0)
    except OSError:
        time.sleep(0.15)
sys.exit(1)
PYWAIT
}

throughput() { # streams secs -> "gbps retransmits"
    local streams=$1 secs=$2 vals=() retrs=()
    local j
    for _ in $(seq 1 "$REPS"); do
        j=$(iperf3 -J -c 127.0.0.1 -p "$BACKEND_IPERF" -t "$secs" -O 2 -P "$streams" 2>/dev/null)
        python3 - "$j" <<'PYPARSE' > "$WORK/.tp"
import json, sys
try:
    d = json.loads(sys.argv[1])
    print(f"{d['end']['sum_received']['bits_per_second'] / 1e9:.3f} "
          f"{d['end']['sum_sent'].get('retransmits', 0)}")
except Exception:
    print("0 0")
PYPARSE
        read -r g r <<< "$(cat "$WORK/.tp")"
        [ "$g" != "0" ] && { vals+=("$g"); retrs+=("$r"); }
    done
    [ "${#vals[@]}" -gt 0 ] || { echo "0 0"; return 1; }
    local gmed
    gmed=$(printf '%s\n' "${vals[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}')
    echo "$gmed ${retrs[${#retrs[@]}-1]}"
}

latency() { # exposed_port -> json percentiles (ms)
    python3 - "$1" <<'PYLAT'
import json, socket, sys, time
port = int(sys.argv[1])
def once():
    t0 = time.perf_counter()
    s = socket.socket()
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    s.connect(("127.0.0.1", port))
    s.sendall(b"p")
    s.recv(1)
    dt = (time.perf_counter() - t0) * 1000.0
    s.close()
    return dt
for _ in range(30):
    once()                      # warmup: fill pools / caches
xs = sorted(once() for _ in range(300))  # timed
out = {"p50": round(xs[len(xs)//2], 3),
       "p95": round(xs[int(len(xs)*0.95)], 3),
       "p99": round(xs[int(len(xs)*0.99)-1], 3),
       "mean": round(sum(xs)/len(xs), 3)}
print(json.dumps(out))
PYLAT
}

sample_mem() { # tag -> samples RSS of SRV_PID/CLI_PID while the tool runs
    MEM_STOP="$WORK/$1.mem.stop"
    : > "$WORK/$1.mem"
    (
        while :; do
            [ -f "$MEM_STOP" ] && break
            local_s=$(ps -o rss= -p "${SRV_PID:-0}" 2>/dev/null | tr -d ' ')
            local_c=$(ps -o rss= -p "${CLI_PID:-0}" 2>/dev/null | tr -d ' ')
            if [ -n "$local_s" ] && [ -n "$local_c" ]; then
                echo "$local_s $local_c" >> "$WORK/$1.mem"
            fi
            sleep 0.5
        done
    ) &
    MEM_PID=$!
}

mem_stats() { # file -> "server_avg_kb client_avg_kb total_avg_kb total_peak_kb samples"
    awk '{
        s=$1; c=$2; t=s+c; sum_s+=s; sum_c+=c; sum_t+=t; if (t>peak) peak=t; n++
    } END {
        if (n>0) printf "%.0f %.0f %.0f %.0f %d", sum_s/n, sum_c/n, sum_t/n, peak, n
        else printf "0 0 0 0 0"
    }' "$1"
}

stop_mem() {
    [ -n "${MEM_STOP:-}" ] && touch "$MEM_STOP"
    [ -n "${MEM_PID:-}" ] && wait "$MEM_PID" 2>/dev/null
    MEM_PID=""
}

# --- weak-network simulation -------------------------------------------------
NETEM_OK=0
if command -v tc >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
    if sudo tc qdisc replace dev lo root netem loss 0% delay 0ms 2>/dev/null; then
        NETEM_OK=1
        sudo tc qdisc del dev lo root 2>/dev/null || true
    fi
fi
[ "$NETEM_OK" = 1 ] || echo "NOTE: netem unavailable (no CAP_NET_ADMIN) -> rtt cells run via userspace weakproxy; loss cells are skipped" >&2

netem_on() { # loss_pct burst_pct rate_mbit rtt_ms
    local args=()
    if [ "$1" != 0 ]; then
        args+=(loss "$1%")
        [ "$2" != 0 ] && args+=("$2%")
    fi
    [ "$4" != 0 ] && args+=(delay "$4ms")
    [ "$3" != 0 ] && args+=(rate "$3mbit")
    sudo tc qdisc replace dev lo root netem "${args[@]}" >/dev/null
}
netem_off() {
    sudo tc qdisc del dev lo root >/dev/null 2>&1 || true
}

# --- tool setups -------------------------------------------------------------
# Each setup_* writes configs under $WORK, starts the tool's server+client and
# sets SRV_PID/CLI_PID (plus EXTRA_PID where a tool needs a third process).
# Port globals are provided by the caller: SERVER_PORT / CLIENT_PORT (client
# dials CLIENT_PORT — the weakproxy when one is active), IPERF_EXPOSED,
# ECHO_EXPOSED, BACKEND_IPERF, BACKEND_ECHO.

# Molehill variant spec: "mux" | "mux-off" | "noise". The UDP echo service is
# part of every variant — the UDP scenario runs for every arm that forwards
# UDP. Noise keys are generated once per run via `molehill --genkey`.
ensure_noise_keys() {
    [ -s "$WORK/noise.pub" ] && return 0
    "$MOLEHILL_BIN" --genkey > "$WORK/noise.gen" 2>&1
    awk '/^Private Key:/{getline; print; exit}' "$WORK/noise.gen" > "$WORK/noise.priv"
    awk '/^Public Key:/{getline; print; exit}' "$WORK/noise.gen" > "$WORK/noise.pub"
    [ -s "$WORK/noise.priv" ] && [ -s "$WORK/noise.pub" ]
}

setup_molehill() { # $1: variant ("mux" | "mux-off" | "noise")
    local d="$WORK/molehill"; mkdir -p "$d"
    local mux=true transport="tcp" noise_server="" noise_client=""
    case "${1:-mux}" in
        mux-off) mux=false ;;
        noise)   transport="noise"
                 ensure_noise_keys || { echo "noise keygen failed" >&2; return 1; }
                 noise_server=$(printf '[server.transport.noise]\nlocal_private_key = "%s"\n' "$(cat "$WORK/noise.priv")")
                 noise_client=$(printf '[client.transport.noise]\nremote_public_key = "%s"\n' "$(cat "$WORK/noise.pub")") ;;
    esac
    cat > "$d/server.toml" <<TOML
[server]
bind_addr = "127.0.0.1:$SERVER_PORT"
default_token = "bench"
allow_ports = ["25100-25999"]
[server.transport]
type = "$transport"
${noise_server}
TOML
    cat > "$d/client.toml" <<TOML
[client]
remote_addr = "127.0.0.1:$CLIENT_PORT"
default_token = "bench"
mux = $mux
[client.transport]
type = "$transport"
${noise_client}

[client.services.iperf]
local_addr = "127.0.0.1:$BACKEND_IPERF"
remote_bind_addr = "127.0.0.1:$IPERF_EXPOSED"
pool_size = $POOL_SIZE

[client.services.echo]
local_addr = "127.0.0.1:$BACKEND_ECHO"
remote_bind_addr = "127.0.0.1:$ECHO_EXPOSED"
pool_size = $POOL_SIZE

[client.services.udpecho]
type = "udp"
local_addr = "127.0.0.1:$BACKEND_UDP"
remote_bind_addr = "127.0.0.1:$UDP_EXPOSED"
pool_size = 2
udp_buffer_size = 2048
udp_idle_timeout = 60
udp_sendq_size = 1024
TOML
    "$MOLEHILL_BIN" --server "$d/server.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$MOLEHILL_BIN" --client "$d/client.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

setup_frp() {
    local d="$WORK/frp"; mkdir -p "$d"
    cat > "$d/frps.toml" <<TOML
bindAddr = "127.0.0.1"
bindPort = $SERVER_PORT
auth.token = "bench"
TOML
    cat > "$d/frpc.toml" <<TOML
serverAddr = "127.0.0.1"
serverPort = $CLIENT_PORT
auth.token = "bench"
loginFailExit = false

[[proxies]]
name = "iperf"
type = "tcp"
localIP = "127.0.0.1"
localPort = $BACKEND_IPERF
remotePort = $IPERF_EXPOSED

[[proxies]]
name = "echo"
type = "tcp"
localIP = "127.0.0.1"
localPort = $BACKEND_ECHO
remotePort = $ECHO_EXPOSED

[[proxies]]
name = "udpecho"
type = "udp"
localIP = "127.0.0.1"
localPort = $BACKEND_UDP
remotePort = $UDP_EXPOSED
TOML
    "$PEER_DIR/frp/frps" -c "$d/frps.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$PEER_DIR/frp/frpc" -c "$d/frpc.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

setup_rathole() { # upstream v0.5.0, v1 protocol (static server-side services)
    local d="$WORK/rathole"; mkdir -p "$d"
    cat > "$d/server.toml" <<TOML
[server]
bind_addr = "127.0.0.1:$SERVER_PORT"
[server.transport]
type = "tcp"

[server.services.iperf]
bind_addr = "127.0.0.1:$IPERF_EXPOSED"
token = "bench"

[server.services.echo]
bind_addr = "127.0.0.1:$ECHO_EXPOSED"
token = "bench"

[server.services.udpecho]
bind_addr = "127.0.0.1:$UDP_EXPOSED"
token = "bench"
TOML
    cat > "$d/client.toml" <<TOML
[client]
remote_addr = "127.0.0.1:$CLIENT_PORT"
[client.transport]
type = "tcp"

[client.services.iperf]
local_addr = "127.0.0.1:$BACKEND_IPERF"
token = "bench"

[client.services.echo]
local_addr = "127.0.0.1:$BACKEND_ECHO"
token = "bench"

[client.services.udpecho]
type = "udp"
local_addr = "127.0.0.1:$BACKEND_UDP"
token = "bench"
TOML
    "$PEER_DIR/rathole" --server "$d/server.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$PEER_DIR/rathole" --client "$d/client.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

start_bore_local() { # exposed_port local_port log -> echoes client pid; retries
    local exposed=$1 localp=$2 logf=$3 pid
    for _ in 1 2 3; do
        # --to takes a bare host: port 7835 is implied (fixed control port);
        # in weakproxy cells the proxy holds 127.0.0.1:7835
        "$PEER_DIR/bore" local "$localp" --to 127.0.0.1 \
            --port "$exposed" >"$logf" 2>&1 &
        pid=$!
        sleep 1.2
        if kill -0 "$pid" 2>/dev/null && wait_port "$exposed" 3; then
            echo "$pid"
            return 0
        fi
        kill "$pid" 2>/dev/null
        sleep 0.5
    done
    return 1
}

setup_bore() { # control port is fixed at 7835; one `bore local` per exposed port
    # bore is TCP-only: no UDP service arm (its UDP metrics stay absent and
    # the regression gate skips them)
    local d="$WORK/bore"; mkdir -p "$d"
    if [ "$MECH" = "weakproxy" ]; then
        # control on a second loopback IP so the weakproxy can hold
        # 127.0.0.1:7835 (see start_bore_local); tunnels stay on 127.0.0.1
        "$PEER_DIR/bore" server --bind-addr 127.0.0.2 --bind-tunnels 127.0.0.1 \
            --min-port "$((IPERF_EXPOSED - 2))" --max-port "$((ECHO_EXPOSED + 2))" \
            >"$d/s.log" 2>&1 &
    else
        "$PEER_DIR/bore" server --bind-addr 127.0.0.1 \
            --min-port "$((IPERF_EXPOSED - 2))" --max-port "$((ECHO_EXPOSED + 2))" \
            >"$d/s.log" 2>&1 &
    fi
    SRV_PID=$!
    wait_port 7835 25 || { echo "bore control port not ready" >&2; return 1; }
    # the first control handshake occasionally dies with "unexpected EOF";
    # retry until the tunnel port is actually bound
    CLI_PID=$(start_bore_local "$IPERF_EXPOSED" "$BACKEND_IPERF" "$d/c1.log") \
        || { echo "bore iperf tunnel failed" >&2; kill_tool; return 1; }
    EXTRA_PID=$(start_bore_local "$ECHO_EXPOSED" "$BACKEND_ECHO" "$d/c2.log") \
        || { echo "bore echo tunnel failed" >&2; kill_tool; return 1; }
}

setup_chisel() {
    local d="$WORK/chisel"; mkdir -p "$d"
    "$PEER_DIR/chisel" server --host 0.0.0.0 --port "$SERVER_PORT" --reverse >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$PEER_DIR/chisel" client "http://127.0.0.1:$CLIENT_PORT" \
        "R:$IPERF_EXPOSED:127.0.0.1:$BACKEND_IPERF" \
        "R:$ECHO_EXPOSED:127.0.0.1:$BACKEND_ECHO" \
        "R:$UDP_EXPOSED:127.0.0.1:$BACKEND_UDP/udp" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

kill_tool() {
    # NOTE: the weakproxy is cell-scoped (started once per tool, shared by the
    # molehill variants) — it is killed at the end of the cell, not here.
    for pid in "$SRV_PID" "$CLI_PID" "${EXTRA_PID:-}"; do
        [ -n "$pid" ] && kill "$pid" 2>/dev/null
    done
    EXTRA_PID=""
    sleep 0.4
    pkill -x molehill 2>/dev/null; pkill -x frps 2>/dev/null; pkill -x frpc 2>/dev/null
    pkill -x rathole 2>/dev/null; pkill -x bore 2>/dev/null; pkill -x chisel 2>/dev/null
    sleep 0.3
}

# --- run matrix --------------------------------------------------------------

# Port offsets inside each cell's 100-port band: 0-9 = molehill (mux),
# 10-19 = molehill (mux=off, loopback only), then one 20-port band per peer.
declare -A TOFF=( [molehill]=0 [frp]=20 [rathole]=40 [bore]=60 [chisel]=80 )
declare -A TSETUP=( [molehill]=setup_molehill [frp]=setup_frp [rathole]=setup_rathole [bore]=setup_bore [chisel]=setup_chisel )

ver_of() { "$@" 2>/dev/null | head -1 | sed 's/^[^0-9]*//' | awk '{print $1}'; }
# molehill --version is multi-line build info; the semver lives on the
# "Build Version:" line (the first line prints an empty version)
MOLEHILL_V=$("$MOLEHILL_BIN" --version 2>/dev/null | awk '/^Build Version:/ {print $3}')
[ -n "$MOLEHILL_V" ] || MOLEHILL_V=dev
FRP_V=$(ver_of "$PEER_DIR/frp/frps" --version)
RATHOLE_V=$(ver_of "$PEER_DIR/rathole" --version)
BORE_V=$(ver_of "$PEER_DIR/bore" --version)
CHISEL_V=$(ver_of "$PEER_DIR/chisel" --version)
# prebuilt releases may print empty versions (no git metadata at build time);
# fall back to the pinned versions in fetch_peers.sh
FRP_V=${FRP_V:-0.71.0}; RATHOLE_V=${RATHOLE_V:-0.5.0}
BORE_V=${BORE_V:-0.6.0}; CHISEL_V=${CHISEL_V:-1.10.1}

tool_bin_ok() {
    case "$1" in
        molehill) [ -x "$MOLEHILL_BIN" ] ;;
        frp)      [ -x "$PEER_DIR/frp/frps" ] ;;
        rathole)  [ -x "$PEER_DIR/rathole" ] ;;
        bore)     [ -x "$PEER_DIR/bore" ] ;;
        chisel)   [ -x "$PEER_DIR/chisel" ] ;;
    esac
}

cat > "$WORK/echo_srv.py" <<'PYECHO'
import socket, threading, sys
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", int(sys.argv[1])))
srv.listen(512)
def serve(c):
    try:
        while True:
            d = c.recv(65536)
            if not d:
                break
            c.sendall(d)
    except OSError:
        pass
    finally:
        c.close()
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PYECHO

cat > "$WORK/udp_echo_srv.py" <<'PYUDPECHO'
import socket, sys
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.bind(("127.0.0.1", int(sys.argv[1])))
while True:
    data, addr = s.recvfrom(65535)
    s.sendto(data, addr)
PYUDPECHO

declare -A R      # "cell|tool" -> per-run json fragment
CELLS_META=""
IDX=0

for cell in $CELLS; do
    # formats: "loss%/rtt", "loss%:burst%/rtt" (correlated loss), "r<mbit>/rtt"
    loss=0; burst=0; rate=0; rtt=0
    case "$cell" in
        r[0-9]*)  rate=${cell%%/*}; rate=${rate#r}; rtt=${cell##*/}
                  cname="rate${rate}_rtt${rtt}" ;;
        *:*)      lb=${cell%%/*}; loss=${lb%%:*}; burst=${lb#*:}; rtt=${cell##*/}
                  cname="loss${loss}b${burst}_rtt${rtt}" ;;
        *)        loss=${cell%%/*}; loss=${loss%\%}   # tolerate "1%/10" style
                  rtt=${cell##*/}
                  if [ "$loss" = 0 ] && [ "$rtt" = 0 ]; then cname="loopback"
                  elif [ "$loss" = 0 ]; then cname="rtt${rtt}"
                  else cname="loss${loss}_rtt${rtt}"; fi ;;
    esac
    secs=$SECS
    MECH="direct"
    if [ "$loss" != 0 ] || [ "$rtt" != 0 ] || [ "$rate" != 0 ] || [ "$burst" != 0 ]; then
        secs=$SECS_WEAK
        if [ "$NETEM_OK" = 1 ]; then
            netem_on "$loss" "$burst" "$rate" "$rtt"
            MECH="netem"
        elif [ "$loss" != 0 ]; then
            echo "skip cell $cname: loss simulation needs netem (CAP_NET_ADMIN)" >&2
            continue
        else
            MECH="weakproxy"
        fi
    fi

    BASE=$((25100 + IDX * 100))
    BACKEND_IPERF=$((BASE + 90)); BACKEND_ECHO=$((BASE + 91)); BACKEND_UDP=$((BASE + 92))
    iperf3 -s -B 127.0.0.1 -p "$BACKEND_IPERF" >/dev/null 2>&1 &
    IPERF_SRV_PID=$!
    python3 "$WORK/echo_srv.py" "$BACKEND_ECHO" >/dev/null 2>&1 &
    ECHO_SRV_PID=$!
    python3 "$WORK/udp_echo_srv.py" "$BACKEND_UDP" >/dev/null 2>&1 &
    UDP_SRV_PID=$!
    sleep 0.4

    run_one() { # label setup_fn setup_arg has_udp
        local label=$1 setup=$2 sarg=$3 has_udp=${4:-1}
        echo "=== $label [$cname] ===" >&2
        "$setup" "$sarg" || { echo "$label: setup failed, skipped" >&2; kill_tool; return; }
        wait_port "$IPERF_EXPOSED" 25 || { echo "$label: exposed port not ready, skipped" >&2; kill_tool; return; }
        wait_port "$ECHO_EXPOSED" 25 || { echo "$label: echo port not ready, skipped" >&2; kill_tool; return; }
        sleep 0.7
        sample_mem "$cname.$label"
        read -r t1 r1 <<< "$(throughput 1 "$secs")"
        read -r t8 r8 <<< "$(throughput 8 "$secs")"
        lat=$(latency "$ECHO_EXPOSED")
        steady=$(python3 "$SCRIPT_DIR/tcp_steady_ping.py" 127.0.0.1 "$ECHO_EXPOSED" 200 20 \
            || echo '{"p50":0,"p95":0,"p99":0,"mean":0}')
        udp_json="null"; hol_udp_json="null"
        if [ "$has_udp" = 1 ]; then
            udp_json=$(python3 "$SCRIPT_DIR/udp_ping.py" 127.0.0.1 "$UDP_EXPOSED" 200 20 $((secs + 3)) \
                || echo '{"sent":0,"received":0,"loss_pct":100,"rtt_ms":{"p50":0,"p95":0,"p99":0,"mean":0},"jitter_ms":0,"max_gap_ms":0}')
            hol_udp_json=$(python3 "$SCRIPT_DIR/hol_probe.py" --mode udp --host 127.0.0.1 --port "$UDP_EXPOSED" \
                --duration "$HOL_SECS" --bulk-rate-mbps "$HOL_BULK_RATE_UDP" || echo "null")
        fi
        hol_json=$(python3 "$SCRIPT_DIR/hol_probe.py" --mode tcp --host 127.0.0.1 --port "$ECHO_EXPOSED" \
            --duration "$HOL_SECS" --bulk-rate-mbps "$HOL_BULK_RATE_TCP" || echo "null")
        stop_mem
        read -r ms mc mt mp mn <<< "$(mem_stats "$WORK/$cname.$label.mem")"
        kill_tool
        R["$cname|$label"]="{\"thr1\": $t1, \"retr1\": $r1, \"thr8\": $t8, \"retr8\": $r8, \"lat\": $lat, \"steady\": ${steady:-null}, \"udp\": ${udp_json}, \"hol\": ${hol_json}, \"hol_udp\": ${hol_udp_json}, \"mem\": {\"server_avg_kb\": $ms, \"client_avg_kb\": $mc, \"total_avg_kb\": $mt, \"total_peak_kb\": $mp, \"samples\": $mn}}"
    }

    for tool in $TOOLS; do
        if ! tool_bin_ok "$tool"; then echo "skip $tool: binary missing (run fetch_peers.sh)" >&2; continue; fi
        off=${TOFF[$tool]}
        CONTROL=$((BASE + off + 1)); IPERF_EXPOSED=$((BASE + off + 2)); ECHO_EXPOSED=$((BASE + off + 3))
        UDP_EXPOSED=$((BASE + off + 4))
        SERVER_PORT=$CONTROL
        if [ "$MECH" = "weakproxy" ]; then
            # one userspace proxy per tool: client dials the proxy, which adds
            # rtt/2 per direction towards the tool's server control port
            CLIENT_PORT=$((BASE + off + 19))
            target="127.0.0.1:$CONTROL"
            if [ "$tool" = bore ]; then
                # bore's control port is fixed at 7835 and --to takes a bare
                # host (port implied); in proxied cells the server control
                # moves to 127.0.0.2 so the proxy can hold 127.0.0.1:7835
                CLIENT_PORT=7835
                target="127.0.0.2:7835"
            fi
            python3 "$SCRIPT_DIR/weakproxy.py" "$CLIENT_PORT" "$target" "$rtt" \
                >"$WORK/weakproxy.$cname.$tool.log" 2>&1 &
            PROXY_PID=$!
            sleep 0.3
        else
            CLIENT_PORT=$CONTROL
            PROXY_PID=""
        fi
        case "$tool" in
            molehill)
                for variant in $MOLEHILL_VARIANTS; do
                    run_one "molehill $MOLEHILL_V ($variant)" setup_molehill "$variant" 1
                done
                if [ "$cname" = loopback ] && [[ " $MOLEHILL_VARIANTS " != *"mux-off"* ]]; then
                    run_one "molehill $MOLEHILL_V (mux=off)" setup_molehill "mux-off" 1
                fi ;;
            frp)      run_one "frp $FRP_V" setup_frp "" 1 ;;
            rathole)  run_one "rathole $RATHOLE_V" setup_rathole "" 1 ;;
            bore)     run_one "bore $BORE_V" setup_bore "" 0 ;;
            chisel)   run_one "chisel $CHISEL_V" setup_chisel "" 1 ;;
        esac
    done

    # loopback extra: molehill with mux disabled is handled inside the
    # molehill case (runs after the mux/noise variants on the loopback cell)

    kill "$IPERF_SRV_PID" "$ECHO_SRV_PID" "$UDP_SRV_PID" 2>/dev/null
    pkill -x iperf3 2>/dev/null
    pkill -f "weakproxy.py" 2>/dev/null
    PROXY_PID=""
    [ "$MECH" = "netem" ] && netem_off
    CELLS_META="$CELLS_META{\"name\": \"$cname\", \"loss_pct\": $loss, \"rtt_ms\": $rtt, \"mech\": \"$MECH\"},"
    IDX=$((IDX + 1))
done

netem_off
tc qdisc show dev lo | grep -q netem && echo "WARNING: netem still active on lo!" >&2

# --- dump --------------------------------------------------------------------
BENCH_RAW=""
for k in "${!R[@]}"; do BENCH_RAW+="$k=${R[$k]};"; done
BENCH_RAW=${BENCH_RAW%;} BENCH_CELLS="$CELLS_META" \
BENCH_TOOLV="molehill=$MOLEHILL_V;frp=$FRP_V;rathole=$RATHOLE_V;bore=$BORE_V;chisel=$CHISEL_V" \
REPS="$REPS" SECS="$SECS" SECS_WEAK="$SECS_WEAK" HOL_SECS="$HOL_SECS" \
python3 - "$OUT" <<'PYDUMP'
import datetime, json, os, platform, socket, sys

raw = os.environ.get("BENCH_RAW", "")
results = {}
for item in filter(None, raw.split(";")):
    cell, rest = item.split("|", 1)
    tool, blob = rest.rsplit("=", 1)  # labels may contain "=" (e.g. mux=off)
    frag = json.loads(blob)
    # normalize to the v3 schema consumed by plot_bench.py / check_regression.sh
    udp = frag.get("udp") or {}
    results.setdefault(tool, {})[cell] = {
        "throughput_1stream_gbps": frag["thr1"],
        "throughput_8streams_gbps": frag["thr8"],
        "retransmits_1stream": frag.get("retr1"),
        "retransmits_8streams": frag.get("retr8"),
        "echo_rtt_ms": frag["lat"],
        "tcp_steady_rtt_ms": frag.get("steady"),
        "udp_rtt_ms": udp.get("rtt_ms"),
        "udp_loss_pct": udp.get("loss_pct"),
        "udp_jitter_ms": udp.get("jitter_ms"),
        "udp_max_gap_ms": udp.get("max_gap_ms"),
        "hol": frag.get("hol"),
        "hol_udp": frag.get("hol_udp"),
        "memory_rss_kb": frag["mem"],
    }

toolv = {}
for pair in filter(None, os.environ.get("BENCH_TOOLV", "").split(";")):
    k, v = pair.split("=", 1)
    if v:
        toolv[k] = v

cells = []
for item in filter(None, os.environ.get("BENCH_CELLS", "").split("},")):
    if not item.endswith("}"):
        item += "}"
    cells.append(json.loads(item))

cpu = "?"
for line in open("/proc/cpuinfo"):
    if line.startswith("model name"):
        cpu = line.split(":", 1)[1].strip()
        break

meta = {
    "date": datetime.date.today().isoformat(),
    "hostname": socket.gethostname(),
    "kernel": platform.release(),
    "cpu": cpu,
    "schema": 3,
    "reps": int(os.environ.get("REPS", "3")),
    "secs_per_rep_loopback": int(os.environ.get("SECS", "8")),
    "secs_per_rep_weak": int(os.environ.get("SECS_WEAK", "15")),
    "hol_probe_seconds": int(os.environ.get("HOL_SECS", "8")),
    "latency_samples": 300,
    "memory_samples_interval_s": 0.5,
    "topology": "loopback visitor->server->client->backend",
    "transport": "plain tcp + noise (molehill variants)",
    "cells": cells,
    "tool_versions": toolv,
}
json.dump({"meta": meta, "results": results}, open(sys.argv[1], "w"), indent=2)
print(f"saved -> {sys.argv[1]}")
print(json.dumps(results, indent=2))
PYDUMP
