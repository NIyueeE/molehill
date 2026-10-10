#!/usr/bin/env bash
# Attribute the multiplexing overhead: where does one flow's CPU time go when it
# rides a multiplexed tunnel instead of a connection of its own?
#
# What this explains (it does not measure it — the bench model owns numbers and
# their noise floor): `HANDOFF.md`, "The mux×1 gap, measured before touching it",
# and the L4 rows of `docs/benchmarks.md`. The model's finding was ~9 Gbit/s for
# a single flow through a pinned pool against ~19 Gbit/s for the same flow on its
# own connection, at +87 % CPU per byte, with a 15.35 % A/A floor. A number says
# a gap exists; a profile says which functions own it, which is what a change
# needs before it is written.
#
# The topology is deliberately minimal — iperf3 as the backend, one molehill
# server and one client on loopback, one TCP service — because the profile is
# read by function name, not by absolute rate. `--tunnels 1` is the purest shape:
# one stream on one tunnel, so every sample is multiplexing's own cost and no
# placement decision. Compare `--tunnels 2` to see what a second tunnel adds.
#
# Requires `perf` and a paranoid level that lets it sample
# (`sysctl kernel.perf_event_paranoid` <= 1, or run it as root). It does not
# need TUN, namespaces or root otherwise.
#
# Usage:
#   bash benches/scripts/mux/gap_profile.sh [--tunnels N] [--seconds S] [--binary PATH]
#
# Output: an iperf3 rate, then the top symbols per daemon under
# $LOG (default /tmp/mux-gap). The daemons' own report is one-sided on purpose:
# the client's CPU is where framing and window updates live, the server's is
# where de-framing and the visitor socket live, and a change has to say which
# end it is helping.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"

TUNNELS=1
SECONDS_TO_PROFILE=8
TRANSFER_SECONDS=12
# The release profile strips symbols (`[profile.release] strip = true`), and a
# profile of raw addresses attributes nothing. The `bench` profile keeps debug
# info, built into its own target directory so the bench model's binary — the one
# measurements are quoted from — is left exactly as it was.
PROFILE_TARGET_DIR="${PROFILE_TARGET_DIR:-$ROOT/target/profile}"
BINARY="$PROFILE_TARGET_DIR/release/molehill"
while [ $# -gt 0 ]; do
    case "$1" in
    --tunnels)
        TUNNELS="$2"
        shift 2
        ;;
    --seconds)
        SECONDS_TO_PROFILE="$2"
        TRANSFER_SECONDS=$((SECONDS_TO_PROFILE + 4))
        shift 2
        ;;
    --binary)
        BINARY="$2"
        shift 2
        ;;
    -h | --help)
        sed -n '2,30p' "$0"
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        exit 2
        ;;
    esac
done

LOG="${LOG:-/tmp/mux-gap}"
case "$LOG" in
"" | "/")
    echo "refusing LOG=$LOG" >&2
    exit 2
    ;;
esac

for tool in perf iperf3 cargo; do
    command -v "$tool" >/dev/null 2>&1 || {
        echo "$tool is required" >&2
        exit 1
    }
done
if [ ! -x "$BINARY" ]; then
    echo "building $BINARY (bench profile, symbols kept)"
    (cd "$ROOT" && CARGO_PROFILE_BENCH_STRIP=false CARGO_TARGET_DIR="$PROFILE_TARGET_DIR" \
        cargo build --profile bench --quiet)
fi
# `grep -q` closes the pipe, and `pipefail` would read nm's SIGPIPE as a failure:
# the symbols are captured whole instead.
SYMBOLS="$(nm -C "$BINARY" 2>/dev/null || true)"
case "$SYMBOLS" in
*molehill_rathole*) ;;
*)
    echo "$BINARY carries no symbols; rebuild it with the bench profile" >&2
    exit 1
    ;;
esac
# Provenance: a profile of a binary nobody can identify is not evidence.
"$BINARY" --version 2>/dev/null | head -n 1 || true
if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$BINARY" | awk '{print "binary sha256", substr($1, 1, 16)}'
fi

BACKEND_PORT=5201
CONTROL_PORT=24338
PUBLIC_PORT=24334

rm -rf "$LOG"
mkdir -p "$LOG"

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT

cat >"$LOG/server.toml" <<TOML
[server]
default_token = "profile-only" # security-scan:allow loopback profiling fixture
allow_ports = ["$PUBLIC_PORT"]

[server.control]
bind_addr = "127.0.0.1:$CONTROL_PORT"
TOML

cat >"$LOG/client.toml" <<TOML
[client]
default_token = "profile-only" # security-scan:allow loopback profiling fixture

[client.control]
default_remote_addr = "127.0.0.1:$CONTROL_PORT"

[client.data.tcp]
tunnels = $TUNNELS

[client.services.svc]
local_addr = "127.0.0.1:$BACKEND_PORT"
remote_bind_addr = "127.0.0.1:$PUBLIC_PORT"
TOML

# iperf3 is single-test by design: a fresh one per run, and its own log kept.
iperf3 -s -p "$BACKEND_PORT" -1 >"$LOG/iperf3-server.log" 2>&1 &
PIDS+=("$!")
"$BINARY" --server "$LOG/server.toml" >"$LOG/server.log" 2>&1 &
SRV_PID=$!
PIDS+=("$SRV_PID")
"$BINARY" --client "$LOG/client.toml" >"$LOG/client.log" 2>&1 &
CLI_PID=$!
PIDS+=("$CLI_PID")

# Wait for the service to be *forwarding* rather than for a fixed delay or a
# connect: the backend is `iperf3 -s -1` (one test, then it exits), so a probe
# connection through the tunnel would consume the run's only test.
for _ in $(seq 1 100); do
    if grep -q "Registered, exposed at" "$LOG/client.log" 2>/dev/null &&
        grep -q "Listening at 127.0.0.1:$PUBLIC_PORT" "$LOG/server.log" 2>/dev/null; then
        break
    fi
    sleep 0.1
done
if ! grep -q "Registered, exposed at" "$LOG/client.log"; then
    echo "the client never registered" >&2
    tail -n 20 "$LOG/client.log" "$LOG/server.log" >&2
    exit 1
fi

echo "profiling both daemons for ${SECONDS_TO_PROFILE}s (tunnels=$TUNNELS)"
# Frame pointers are not something a Rust release build promises, so the stacks
# come from DWARF — which is also why the binary has to carry debug info (see
# PROFILE_TARGET_DIR above). Self time names the leaf; the callers below name the
# code that asks for it, and a change needs the second.
perf record -o "$LOG/perf.data" -F 999 --call-graph dwarf,8192 \
    -p "$SRV_PID" -p "$CLI_PID" -- sleep "$SECONDS_TO_PROFILE" \
    >"$LOG/perf-record.log" 2>&1 &
PERF_PID=$!
PIDS+=("$PERF_PID")
sleep 0.3

iperf3 -c 127.0.0.1 -p "$PUBLIC_PORT" -t "$TRANSFER_SECONDS" -f g \
    >"$LOG/iperf3-client.log" 2>&1 || true
wait "$PERF_PID" || true

echo "--- iperf3 through the tunnel ---"
grep -E "sender|receiver" "$LOG/iperf3-client.log" || cat "$LOG/iperf3-client.log"

echo
echo "================= WHERE THE CPU WENT ================="
# `comm` first, so a symbol's owner is never a guess; Rust's mangled names carry
# their module path, which is what the reader is here for.
perf report -i "$LOG/perf.data" --stdio --no-children --sort=comm,overhead,symbol \
    --percent-limit 0.4 2>/dev/null | grep -E "^\s+[0-9]+\.[0-9]+%" | head -n 30

echo
echo "================= WHO ASKS FOR IT (callers of the top leaves) ================="
# The leaf names the cost, the caller names the code that pays it. Kernel samples
# have no symbols here when the container hides kallsyms from perf (common: the
# addresses are still counted, just not named), which is why this section reads
# the *user*-space leaves.
perf report -i "$LOG/perf.data" --stdio --children --sort=overhead,symbol \
    --percent-limit 0.4 2>/dev/null | grep -E "^\s+[0-9]+\.[0-9]+%" | head -n 25

echo
echo "--- the same, split by daemon ---"
for name in molehill; do
    for pid in "$SRV_PID" "$CLI_PID"; do
        role="server"
        [ "$pid" = "$CLI_PID" ] && role="client"
        echo "--- $role (pid $pid) ---"
        perf report -i "$LOG/perf.data" --stdio --no-children --sort=overhead,symbol \
            --pid "$pid" --percent-limit 0.4 2>/dev/null |
            grep -E "^\s+[0-9]+\.[0-9]+%" | head -n 15
    done
done

echo
echo "artifacts in $LOG: perf.data, server.log, client.log, iperf3-*.log"
echo "per-daemon: perf report -i $LOG/perf.data --stdio --sort=overhead,symbol,pid"
