#!/usr/bin/env bash
# Reproducible loopback benchmark: molehill v0.7.0 (mux on/off) vs v0.6.4 vs frp.
#
# Topology (all on 127.0.0.1):
#   visitor -> proxy-server -> proxy-client -> backend (iperf3 / echo)
#
# Metrics:
#   - iperf3 TCP throughput, 1 stream and 8 streams (median of N reps, Gbit/s)
#   - echo request-response RTT over 300 sequential connections (ms percentiles)
#   - resident memory (RSS) of server+client sampled during the run (KB, avg/peak)
#
# Environment overrides: M070, M064, FRP_DIR, OUT, REPS, SECS
set -u
cd "$(dirname "$0")"

M070=${M070:-$PWD/../../../target/release/molehill}
M064=${M064:-/tmp/molehill-064/target/release/molehill}
FRP_DIR=${FRP_DIR:-/tmp/frp_0.71.0_linux_amd64}
OUT=${OUT:-results-v0.7.0.json}
REPS=${REPS:-3}
SECS=${SECS:-8}

command -v iperf3 >/dev/null || { echo "iperf3 required" >&2; exit 1; }
[ -x "$M070" ] || { echo "missing $M070" >&2; exit 1; }
[ -x "$M064" ] || { echo "missing $M064" >&2; exit 1; }
[ -x "$FRP_DIR/frps" ] || { echo "missing $FRP_DIR/frps" >&2; exit 1; }

WORK=$(mktemp -d /tmp/molehill-bench.XXXXXX)
IPERF_PORT=5201
ECHO_PORT=5211
SRV_PID=""
CLI_PID=""
MEM_PID=""
MEM_STOP=""

cleanup() {
    if [ -n "${MEM_PID:-}" ]; then kill "$MEM_PID" 2>/dev/null; wait "$MEM_PID" 2>/dev/null; fi
    if [ -n "${SRV_PID:-}" ]; then kill "$SRV_PID" 2>/dev/null; fi
    if [ -n "${CLI_PID:-}" ]; then kill "$CLI_PID" 2>/dev/null; fi
    pkill -x molehill 2>/dev/null; pkill -x frps 2>/dev/null; pkill -x frpc 2>/dev/null
    pkill -x iperf3 2>/dev/null
    if [ -n "${ECHO_PID:-}" ]; then kill "$ECHO_PID" 2>/dev/null; fi
    sleep 0.3
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

start_backends() {
    iperf3 -s -B 127.0.0.1 -p $IPERF_PORT >/dev/null 2>&1 &
    python3 "$WORK/echo_srv.py" "$ECHO_PORT" >/dev/null 2>&1 &
    sleep 0.4
}

latency() { # exposed_port -> prints json percentiles (ms)
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

throughput() { # streams -> median gbps across REPS
    local streams=$1 vals=()
    for _ in $(seq 1 "$REPS"); do
        local j bits
        j=$(iperf3 -J -c 127.0.0.1 -p $IPERF_PORT -t "$SECS" -O 2 -P "$streams" 2>/dev/null)
        bits=$(python3 -c "import json,sys;print(json.loads(sys.stdin.read())['end']['sum_received']['bits_per_second'])" <<<"$j" 2>/dev/null)
        [ -n "$bits" ] || { echo "throughput parse failed" >&2; continue; }
        vals+=("$(python3 -c "print(round($bits/1e9, 3))")")
    done
    [ "${#vals[@]}" -gt 0 ] || { echo "no successful reps" >&2; return 1; }
    printf '%s\n' "${vals[@]}" | sort -n | awk '{a[NR]=$1} END{print a[int((NR+1)/2)]}'
}

sample_mem() { # name -> samples RSS (KB) of server/client in $WORK/$name.mem
    local name=$1
    MEM_STOP="$WORK/$name.mem.stop"
    : > "$WORK/$name.mem"
    (
        while :; do
            [ -f "$MEM_STOP" ] && break
            local_s=$(ps -o rss= -p "${SRV_PID:-0}" 2>/dev/null | tr -d ' ')
            local_c=$(ps -o rss= -p "${CLI_PID:-0}" 2>/dev/null | tr -d ' ')
            if [ -n "$local_s" ] && [ -n "$local_c" ]; then
                echo "$local_s $local_c" >> "$WORK/$name.mem"
            fi
            sleep 0.5
        done
    ) &
    MEM_PID=$!
}

stop_sample_mem() {
    if [ -n "${MEM_PID:-}" ]; then
        [ -n "$MEM_STOP" ] && touch "$MEM_STOP"
        wait "$MEM_PID" 2>/dev/null || true
        MEM_PID=""
    fi
}

mem_stats() { # log -> prints "server_avg_kb client_avg_kb total_avg_kb total_peak_kb samples"
    awk '{
        s=$1; c=$2; t=s+c; sum_s+=s; sum_c+=c; sum_t+=t; if (t>peak) peak=t; n++
    } END {
        if (n>0) printf "%.0f %.0f %.0f %d %d", sum_s/n, sum_c/n, sum_t/n, peak, n
        else printf "0 0 0 0 0"
    }' "$1"
}

run_tool() { # name iperf_exposed echo_exposed setup_cmd
    local name=$1 iex=$2 eex=$3
    echo "=== $name ===" >&2
    eval "$4"
    start_backends
    wait_port "$iex" 20 || { echo "$name: iperf port not ready" >&2; cleanup; sleep 0.5; return 1; }
    wait_port "$eex" 20 || { echo "$name: echo port not ready" >&2; cleanup; sleep 0.5; return 1; }
    sleep 0.7
    local t1 tp l
    local ms mc mt mp mn
    sample_mem "$name"
    t1=$(throughput 1)
    tp=$(throughput 8)
    l=$(latency "$eex")
    stop_sample_mem
    read -r ms mc mt mp mn <<< "$(mem_stats "$WORK/$name.mem")"
    cleanup
    sleep 0.5
    echo "{\"thr1\": $t1, \"thrP8\": $tp, \"lat\": $l, \"mem\": {\"server_avg_kb\": $ms, \"client_avg_kb\": $mc, \"total_avg_kb\": $mt, \"total_peak_kb\": $mp, \"samples\": $mn}}"
}

# ---- tool setups -----------------------------------------------------------

setup_m070_mux() {
    local d=$WORK/mux; mkdir -p "$d"
    cat > "$d/server.toml" <<TOML
[server]
bind_addr = "127.0.0.1:23341"
default_token = "bench"
allow_ports = ["25101-25102"]
[server.transport]
type = "tcp"
TOML
    cat > "$d/client.toml" <<TOML
[client]
remote_addr = "127.0.0.1:23341"
default_token = "bench"
mux = true
[client.transport]
type = "tcp"
[client.services.iperf]
local_addr = "127.0.0.1:$IPERF_PORT"
remote_bind_addr = "127.0.0.1:25101"
pool_size = 8
[client.services.echo]
local_addr = "127.0.0.1:$ECHO_PORT"
remote_bind_addr = "127.0.0.1:25102"
pool_size = 8
TOML
    "$M070" --server "$d/server.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$M070" --client "$d/client.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

setup_m070_nomux() {
    local d=$WORK/nomux; mkdir -p "$d"
    cat > "$d/server.toml" <<TOML
[server]
bind_addr = "127.0.0.1:23342"
default_token = "bench"
allow_ports = ["25103-25104"]
[server.transport]
type = "tcp"
TOML
    cat > "$d/client.toml" <<TOML
[client]
remote_addr = "127.0.0.1:23342"
default_token = "bench"
mux = false
[client.transport]
type = "tcp"
[client.services.iperf]
local_addr = "127.0.0.1:$IPERF_PORT"
remote_bind_addr = "127.0.0.1:25103"
pool_size = 8
[client.services.echo]
local_addr = "127.0.0.1:$ECHO_PORT"
remote_bind_addr = "127.0.0.1:25104"
pool_size = 8
TOML
    "$M070" --server "$d/server.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$M070" --client "$d/client.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

setup_m064() {
    local d=$WORK/v064; mkdir -p "$d"
    cat > "$d/server.toml" <<TOML
[server]
bind_addr = "127.0.0.1:23343"

[server.services.iperf]
token = "bench"
bind_addr = "127.0.0.1:25105"

[server.services.echo]
token = "bench"
bind_addr = "127.0.0.1:25106"

[server.transport]
type = "tcp"
TOML
    cat > "$d/client.toml" <<TOML
[client]
remote_addr = "127.0.0.1:23343"

[client.services.iperf]
token = "bench"
local_addr = "127.0.0.1:$IPERF_PORT"

[client.services.echo]
token = "bench"
local_addr = "127.0.0.1:$ECHO_PORT"

[client.transport]
type = "tcp"
TOML
    "$M064" --server "$d/server.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$M064" --client "$d/client.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

setup_frp() {
    local d=$WORK/frp; mkdir -p "$d"
    cat > "$d/frps.toml" <<TOML
bindAddr = "127.0.0.1"
bindPort = 7400
auth.token = "bench"
TOML
    cat > "$d/frpc.toml" <<TOML
serverAddr = "127.0.0.1"
serverPort = 7400
auth.token = "bench"
loginFailExit = false

[[proxies]]
name = "iperf"
type = "tcp"
localIP = "127.0.0.1"
localPort = $IPERF_PORT
remotePort = 25107

[[proxies]]
name = "echo"
type = "tcp"
localIP = "127.0.0.1"
localPort = $ECHO_PORT
remotePort = 25108
TOML
    "$FRP_DIR/frps" -c "$d/frps.toml" >"$d/s.log" 2>&1 &
    SRV_PID=$!
    "$FRP_DIR/frpc" -c "$d/frpc.toml" >"$d/c.log" 2>&1 &
    CLI_PID=$!
}

# ---------------------------------------------------------------------------

mkdir -p "$WORK"
cat > "$WORK/echo_srv.py" <<'PYECHOFILE'
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
PYECHOFILE

declare -A R
r=$(run_tool m070-mux 25101 25102 setup_m070_mux)      && R[m070_mux]=$r
r=$(run_tool m070-nomux 25103 25104 setup_m070_nomux)  && R[m070_nomux]=$r
r=$(run_tool m064 25105 25106 setup_m064)              && R[m064]=$r
r=$(run_tool frp071 25107 25108 setup_frp)             && R[frp071]=$r

BENCH_RAW="m070_mux=${R[m070_mux]:-};m070_nomux=${R[m070_nomux]:-};m064=${R[m064]:-};frp071=${R[frp071]:-}" \
python3 - "$OUT" <<'PYDUMP'
import datetime, json, os, sys
raw = os.environ.get("BENCH_RAW", "")
pairs = []
for item in raw.split(";"):
    if "=" in item:
        k, v = item.split("=", 1)
        if v.strip():
            pairs.append((k, v))
names = {"m070_mux": "molehill 0.7.0 (mux)",
         "m070_nomux": "molehill 0.7.0 (mux=off)",
         "m064": "molehill 0.6.4",
         "frp071": "frp 0.71.0"}
res = {}
for k, v in pairs:
    d = json.loads(v)
    res[names[k]] = {"throughput_1stream_gbps": d["thr1"],
                     "throughput_8streams_gbps": d["thrP8"],
                     "echo_rtt_ms": d["lat"],
                     "memory_rss_kb": d["mem"]}
frp_mem = res.get("frp 0.71.0", {}).get("memory_rss_kb", {}).get("total_avg_kb")
if frp_mem:
    for tool, item in res.items():
        mem = item.setdefault("memory_rss_kb", {})
        total = mem.get("total_avg_kb", 0)
        mem["pct_of_frp"] = round(total / frp_mem * 100.0, 1) if frp_mem else None
meta = {"date": datetime.date.today().isoformat(),
        "reps": int(os.environ.get("REPS", "3")),
        "seconds_per_rep": int(os.environ.get("SECS", "8")),
        "latency_samples": 300,
        "memory_samples_interval_s": 0.5,
        "topology": "loopback visitor->server->client->backend",
        "transport": "plain tcp",
        "frp_version": "0.71.0"}
json.dump({"meta": meta, "results": res}, open(sys.argv[1], "w"), indent=2)
print(json.dumps(res, indent=2))
PYDUMP
echo "saved -> $OUT"
