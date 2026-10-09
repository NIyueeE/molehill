#!/usr/bin/env bash
# Transparent-L3 acceptance harness: the REAL molehill binaries in a
# three-namespace topology, plus the evidence that the CLIENT owns the public
# ip:port and the server holds no connection state for the visitor's flow.
#
#   sudo -n bash benches/scripts/l3/run.sh
#
# Root-only and Linux-only: it creates veth pairs, two TUN devices, routes and
# an ip rule. It is deliberately outside the check chain -- see docs/checks.md,
# "Outside the chain". LOG overrides the artifact directory (/tmp/l3-accept).
#
# What a green run proves, end to end:
#   1. a visitor reaches a service behind NAT, with no NAT anywhere;
#   2. the service sees the VISITOR's real address (transparency);
#   3. the server namespace holds NO connection state for the visitor's flow;
#   4. the client namespace owns the accepted connection;
#   5. 200 KB round-trips byte-exactly (no PMTU black hole), and so do 2000
#      small request/response round trips;
#   6. the run reports what each arm costs on the wire and what share of it a
#      header compressor could reach (benches/scripts/l3/wire_report.py);
#   7. a server whose config has no `[server.transparent]` refuses the
#      registration BY POLICY, before it looks at a device (its device is
#      deleted for that run, so the order is proven rather than asserted).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../.." && pwd)"
LOG="${LOG:-/tmp/l3-accept}"
BIN="$ROOT/target/debug/molehill"

# A silent pass without root would be worse than a failure: exit 77 is the
# "skipped" status, and the exact command to run is part of the message.
if [ "$(id -u)" -ne 0 ]; then
    echo "SKIP l3-accept: it needs root for namespaces, TUN devices and routes." >&2
    echo "     re-run it as: sudo -n bash $HERE/run.sh" >&2
    exit 77
fi

case "$LOG" in
"" | "/")
    echo "refusing LOG=$LOG" >&2
    exit 2
    ;;
esac

for tool in ip ss sysctl uv cargo; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "missing required tool: $tool" >&2
        exit 1
    fi
done
if [ ! -c /dev/net/tun ]; then
    echo "/dev/net/tun is missing: the harness and the daemons need the TUN driver" >&2
    exit 1
fi

# The topology under test. The daemon configures no network itself, so all of
# this is the operator side of the contract (src/transparent/check.rs verifies
# the parts it depends on).
NS_VIS=l3vis
NS_SRV=l3srv
NS_CLI=l3cli
VIS_IP=10.10.0.2
SRV_VIS_IP=10.10.0.254
SRV_CLI_IP=10.30.0.1
CLI_IP=10.30.0.2
PUBLIC_IP=10.99.0.1
PUBLIC_PORT=8443
CONTROL_PORT=2333
TUN_SRV=l3srv0
TUN_CLI=l3cli0
ROUTE_TABLE=100
TUN_MTU=1400
BULK_BYTES=200000
# The small-packet arm's instrument: strict round trips of this size. Both are
# recorded with the numbers, because they are what the numbers mean.
SMALL_REQUESTS=2000
SMALL_BYTES=64
# Optional override of the claim's data-channel mode. Empty writes nothing,
# which is how the run measures the default an operator would get; the axis
# exists because a claim has exactly one channel, so its mode is a cost
# decision rather than a topology one.
CLAIM_MODE="${CLAIM_MODE:-}"
# Long enough that the socket and conntrack snapshots are taken while the
# visitor's connection is unmistakably live.
HOLD_SECONDS=5

PIDS=()

# Deleting a namespace removes everything the harness installed inside it: the
# veth end, the TUN device, and every route and rule on top of them.
topology_down() {
    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do
        ip netns del "$ns" 2>/dev/null || true
    done
}

# Kill whatever is still inside a namespace. PID-based on purpose: a pattern
# kill can match the calling shell.
kill_namespace() {
    local ns="$1" pids
    pids="$(ip netns pids "$ns" 2>/dev/null || true)"
    if [ -n "$pids" ]; then
        kill $pids 2>/dev/null || true
        sleep 0.2
        pids="$(ip netns pids "$ns" 2>/dev/null || true)"
        if [ -n "$pids" ]; then
            kill -9 $pids 2>/dev/null || true
        fi
    fi
}

cleanup() {
    local rc=$?
    trap - EXIT
    set +e
    for p in "${PIDS[@]}"; do
        kill "$p" 2>/dev/null
    done
    sleep 0.3
    for p in "${PIDS[@]}"; do
        kill -9 "$p" 2>/dev/null
    done
    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do
        kill_namespace "$ns"
        ip netns del "$ns" 2>/dev/null
    done
    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do
        if ip netns list 2>/dev/null | grep -q "^$ns"; then
            echo "WARNING: $ns survived teardown; remove it with: ip netns del $ns" >&2
        fi
    done
    exit "$rc"
}
trap cleanup EXIT

# A missing log is itself diagnostic: the process died before writing one.
dump_log() {
    local file="$1"
    if [ -f "$file" ]; then
        cat "$file" >&2
    else
        echo "(no log at $file)" >&2
    fi
}

# Packet and byte counters for the interfaces a measurement arm cares about,
# read inside the namespaces that own them. For a tun device the host's view is
# the L3 path's: `rx` is what userspace wrote into the kernel, `tx` is what the
# kernel routed out to userspace. The client's veth is the tunnel's wire.
dev_snapshot() {
    local out="$1"
    : >"$out"
    for ns in "$NS_CLI" "$NS_SRV"; do
        ip netns exec "$ns" cat /proc/net/dev \
            | awk -v ns="$ns" '/:/ { print "iface " ns " " $1, $2, $3, $10, $11 }' \
            | sed 's/://' >>"$out"
    done
    # CPU the two daemons spend on the arm, in clock ticks (100/s): the
    # difference between "this path is cheap" and "this path is a core".
    cpu_ticks "$CLI_PID" client >>"$out"
    cpu_ticks "$SRV_PID" server >>"$out"
}

# utime + stime of one process, in ticks. The tokio runtime's threads are all
# in this process, so one number covers the whole data path.
cpu_ticks() {
    local pid="$1" who="$2"
    if [ -r "/proc/$pid/stat" ]; then
        awk -v who="$who" '{ print "cpu", who, $14 + $15 }' "/proc/$pid/stat"
    fi
}

# One arm's counters, turned into carried packet sizes and the ceiling any
# header compressor could reach (the reasoning is wire_report.py's docstring).
wire_report() {
    uv run "$HERE/wire_report.py" --label "$1" --before "$2" --after "$3" \
        --client-tun "$TUN_CLI" --server-tun "$TUN_SRV" --tunnel v-cli | tee "$4"
}

# Poll a log for a readiness line instead of sleeping blind.
wait_for() {
    local file="$1" pattern="$2"
    for _ in $(seq 1 100); do
        if grep -qE "$pattern" "$file" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
    done
    echo "TIMEOUT waiting for /$pattern/ in $file" >&2
    tail -n 20 "$file" >&2 || true
    return 1
}

# The server's own readiness signal: its control listener.
wait_port() {
    local ns="$1" port="$2"
    for _ in $(seq 1 100); do
        if ip netns exec "$ns" ss -tln 2>/dev/null | grep -qE ":$port[[:space:]]"; then
            return 0
        fi
        sleep 0.1
    done
    echo "TIMEOUT waiting for $ns to listen on :$port" >&2
    return 1
}

require_alive() {
    local pid="$1" name="$2" file="$3"
    if ! kill -0 "$pid" 2>/dev/null; then
        echo "$name exited before the service was ready; its log:" >&2
        dump_log "$file"
        exit 1
    fi
}

# One ESTAB row holds both literals: how the socket verdicts pin the live flow.
# The state is part of the match because a closed connection lingers as
# TIME-WAIT with the same local address, which would otherwise look alive.
estab_line_has_both() {
    awk -v first="$2" -v second="$3" \
        '$1 ~ /^ESTAB/ && index($0, first) && index($0, second) { found = 1 }
         END { exit(found ? 0 : 1) }' "$1"
}

topology_up() {
    # A namespace left over from an aborted run would fail every add below.
    topology_down

    ip netns add "$NS_VIS"
    ip netns add "$NS_SRV"
    ip netns add "$NS_CLI"

    # visitor <-> server, and server <-> client (the client dials out).
    ip link add v-vis type veth peer name v-srv
    ip link set v-vis netns "$NS_VIS"
    ip link set v-srv netns "$NS_SRV"
    ip link add v-cli type veth peer name v-srv2
    ip link set v-srv2 netns "$NS_SRV"
    ip link set v-cli netns "$NS_CLI"

    ip -n "$NS_VIS" addr add "$VIS_IP/24" dev v-vis
    ip -n "$NS_SRV" addr add "$SRV_VIS_IP/24" dev v-srv
    ip -n "$NS_SRV" addr add "$SRV_CLI_IP/24" dev v-srv2
    ip -n "$NS_CLI" addr add "$CLI_IP/24" dev v-cli

    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do
        ip -n "$ns" link set lo up
    done
    ip -n "$NS_VIS" link set v-vis up
    ip -n "$NS_SRV" link set v-srv up
    ip -n "$NS_SRV" link set v-srv2 up
    ip -n "$NS_CLI" link set v-cli up

    ip -n "$NS_VIS" route add default via "$SRV_VIS_IP"

    # The TUN devices exist before the daemons start: they attach to the name
    # and refuse to create a device themselves (src/transparent/check.rs).
    ip -n "$NS_SRV" tuntap add dev "$TUN_SRV" mode tun
    ip -n "$NS_SRV" link set "$TUN_SRV" up mtu "$TUN_MTU"
    ip -n "$NS_SRV" route add "$PUBLIC_IP/32" dev "$TUN_SRV"

    ip -n "$NS_CLI" tuntap add dev "$TUN_CLI" mode tun
    ip -n "$NS_CLI" link set "$TUN_CLI" up mtu "$TUN_MTU"
    ip -n "$NS_CLI" addr add "$PUBLIC_IP/32" dev "$TUN_CLI"
    # Source policy, not a destination route: every reply from the owned
    # address goes back into the tunnel, whatever it is addressed to.
    ip -n "$NS_CLI" rule add from "$PUBLIC_IP" lookup "$ROUTE_TABLE"
    ip -n "$NS_CLI" route add default dev "$TUN_CLI" table "$ROUTE_TABLE"

    # The client's control channel dials the server on the visitor link, so it
    # needs a way out through the server. The rule above stays ahead of it
    # (pref 32765 < main's 32766), so only the owned address is diverted.
    ip -n "$NS_CLI" route add default via "$SRV_CLI_IP"

    # Forwarding, and no reverse-path filtering: the tunnel injects packets
    # whose source is the visitor's, which a strict rp_filter would drop.
    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do
        ip netns exec "$ns" sysctl -qw net.ipv4.ip_forward=1
        ip netns exec "$ns" sysctl -qw net.ipv4.conf.all.rp_filter=0
        ip netns exec "$ns" sysctl -qw net.ipv4.conf.default.rp_filter=0
    done
    ip netns exec "$NS_SRV" sysctl -qw "net.ipv4.conf.$TUN_SRV.rp_filter=0"
    ip netns exec "$NS_CLI" sysctl -qw "net.ipv4.conf.$TUN_CLI.rp_filter=0"
}

write_configs() {
    cat >"$LOG/server.toml" <<TOML
[server]
default_token = "bench"
allow_ports = ["$PUBLIC_PORT"]

[server.control]
bind_addr = "$SRV_VIS_IP:$CONTROL_PORT"

[server.transparent]
tun = "$TUN_SRV"
TOML

    cat >"$LOG/client.toml" <<TOML
[transparent]
default_token = "bench"
tun = "$TUN_CLI"

[transparent.control]
default_remote_addr = "$SRV_VIS_IP:$CONTROL_PORT"

[transparent.claims.web]
remote_bind_addr = "$PUBLIC_IP:$PUBLIC_PORT"
TOML
    # The claim's mode is the product's default unless the run names one.
    if [ -n "$CLAIM_MODE" ]; then
        printf 'mode = "%s"\n' "$CLAIM_MODE" >>"$LOG/client.toml"
    fi
}

# The same server without `[server.transparent]`: the negative half of the
# policy. Serving L3 is the server operator's decision, so this server must
# refuse the registration *by policy* — and before it looks at a device.
write_no_l3_config() {
    cat >"$LOG/server-no-l3.toml" <<TOML
[server]
default_token = "bench"
allow_ports = ["$PUBLIC_PORT"]

[server.control]
bind_addr = "$SRV_VIS_IP:$CONTROL_PORT"
TOML
}

echo "================= BUILD ================="
mkdir -p "$LOG"
if ! (cd "$ROOT" && cargo build) >"$LOG/cargo-build.log" 2>&1; then
    echo "cargo build failed; refusing to run against a stale or missing binary:" >&2
    tail -n 30 "$LOG/cargo-build.log" >&2
    exit 1
fi
if [ ! -x "$BIN" ]; then
    echo "no executable at $BIN after the build" >&2
    exit 1
fi
echo "binary: $BIN"
sha256sum "$BIN" | tee "$LOG/binary.sha256"
"$BIN" --version 2>&1 | tee "$LOG/binary.version" || true

echo
echo "================= CONFIG ================="
write_configs
echo "--- server.toml ---"
cat "$LOG/server.toml"
echo "--- client.toml ---"
cat "$LOG/client.toml"

echo
echo "================= TOPOLOGY ================="
topology_up
echo "$NS_VIS $VIS_IP/24 -> $SRV_VIS_IP | $NS_SRV $SRV_CLI_IP/24 | $NS_CLI $CLI_IP/24 owns $PUBLIC_IP"

echo
echo "================= SERVICES ================="
# The service, inside the client namespace, bound to the public address the
# client owns. Its log is the transparency proof.
ip netns exec "$NS_CLI" uv run "$HERE/echo_service.py" --bind "$PUBLIC_IP:$PUBLIC_PORT" \
    >"$LOG/echo.log" 2>&1 &
ECHO_PID=$!
PIDS+=("$ECHO_PID")
wait_for "$LOG/echo.log" "^ECHO READY" || exit 1
echo "echo service ready (pid $ECHO_PID)"

ip netns exec "$NS_SRV" "$BIN" --server "$LOG/server.toml" >"$LOG/server.log" 2>&1 &
SRV_PID=$!
PIDS+=("$SRV_PID")
if ! wait_port "$NS_SRV" "$CONTROL_PORT"; then
    echo "the server never listened on :$CONTROL_PORT" >&2
    dump_log "$LOG/server.log"
    exit 1
fi
echo "server listening on :$CONTROL_PORT (pid $SRV_PID)"

ip netns exec "$NS_CLI" "$BIN" --transparent "$LOG/client.toml" >"$LOG/client.log" 2>&1 &
CLI_PID=$!
PIDS+=("$CLI_PID")
echo "client started (pid $CLI_PID)"

echo
echo "--- waiting for the transparent path (visitor -> server -> TUN -> client) ---"
READY=0
for _ in $(seq 1 60); do
    require_alive "$SRV_PID" "the server" "$LOG/server.log"
    require_alive "$CLI_PID" "the client" "$LOG/client.log"
    if ip netns exec "$NS_VIS" uv run "$HERE/visitor.py" --target "$PUBLIC_IP:$PUBLIC_PORT" \
        --payload probe --timeout 1 >"$LOG/visitor-probe.log" 2>&1; then
        READY=1
        break
    fi
    sleep 0.5
done
if [ "$READY" -ne 1 ]; then
    echo "FAIL the service never became reachable from the visitor" >&2
    for f in visitor-probe server client; do
        echo "--- $f log ---" >&2
        dump_log "$LOG/$f.log"
    done
    exit 1
fi
cat "$LOG/visitor-probe.log"

echo
echo "================= VISITOR ================="
# The visitor holds its connection open (--hold) so the snapshots below are
# taken while the flow is live; the verdict reads its exit code after.
ip netns exec "$NS_VIS" uv run "$HERE/visitor.py" --target "$PUBLIC_IP:$PUBLIC_PORT" \
    --hold "$HOLD_SECONDS" >"$LOG/visitor.log" 2>&1 &
VISITOR_PID=$!
wait_for "$LOG/visitor.log" "^VISITOR " || exit 1
cat "$LOG/visitor.log"

VIS_PORT="$(sed -n 's/^CONNECTED [0-9.]*://p' "$LOG/visitor.log" | head -n 1)"
PEER_LITERAL="$VIS_IP"
if [ -n "$VIS_PORT" ]; then
    PEER_LITERAL="$VIS_IP:$VIS_PORT"
fi

echo
echo "================= EVIDENCE ================="
echo "--- echo service log (PEER is the transparency proof) ---"
cat "$LOG/echo.log"
echo "--- TCP sockets in the SERVER namespace (expect no :$PUBLIC_PORT) ---"
ip netns exec "$NS_SRV" ss -tan | tee "$LOG/srv-sockets.txt"
echo "--- TCP sockets in the CLIENT namespace (expect $PUBLIC_IP:$PUBLIC_PORT <- $VIS_IP) ---"
ip netns exec "$NS_CLI" ss -tan | tee "$LOG/cli-sockets.txt"
# Liveness is judged from the captured evidence, not from the visitor process:
# a process can outlive its connection, and a closed flow lingers as TIME-WAIT
# with the same local address, which a bare "is the PID alive" would miss.
SNAPSHOT_LIVE=0
if estab_line_has_both "$LOG/cli-sockets.txt" "$PUBLIC_IP:$PUBLIC_PORT" "$PEER_LITERAL"; then
    SNAPSHOT_LIVE=1
else
    echo "the client snapshot has no live ESTAB connection for $PEER_LITERAL" >&2
fi
echo "--- conntrack in the SERVER namespace (expect no entry for the flow) ---"
if ip netns exec "$NS_SRV" cat /proc/net/nf_conntrack >"$LOG/srv-conntrack.txt" 2>/dev/null; then
    if [ -s "$LOG/srv-conntrack.txt" ]; then
        cat "$LOG/srv-conntrack.txt"
    else
        echo "(table present and empty)"
    fi
else
    echo "(no conntrack table in $NS_SRV: nothing to track with)"
fi
echo "--- server routing decision for the public address ---"
ip netns exec "$NS_SRV" ip route get "$PUBLIC_IP" | tee "$LOG/srv-route.txt"
echo "--- client routing decision for the visitor, sourced from the public address ---"
ip netns exec "$NS_CLI" ip route get "$VIS_IP" from "$PUBLIC_IP" | tee "$LOG/cli-route.txt"

VISITOR_RC=0
wait "$VISITOR_PID" || VISITOR_RC=$?

echo
echo "================= BULK / MTU ================="
# 200 KB through the tunnel: many segments. The client's TUN MTU (1400) is the
# smallest on the path, so the MSS it advertises keeps every segment inside
# it -- the PMTU black hole is avoided by construction, not by clamping.
dev_snapshot "$LOG/bulk.before"
BULK_START=$(date +%s.%N)
BULK_RC=0
ip netns exec "$NS_VIS" uv run "$HERE/visitor.py" --target "$PUBLIC_IP:$PUBLIC_PORT" \
    --size "$BULK_BYTES" >"$LOG/visitor-bulk.log" 2>&1 || BULK_RC=$?
BULK_END=$(date +%s.%N)
dev_snapshot "$LOG/bulk.after"
cat "$LOG/visitor-bulk.log"
awk -v s="$BULK_START" -v e="$BULK_END" -v n="$BULK_BYTES" 'BEGIN {
    d = e - s
    if (d <= 0) d = 0.000001
    printf "elapsed %.3fs, %.2f Mbit/s round-trip (diagnostic, not a benchmark)\n",
        d, 2 * n * 8 / d / 1e6
}'
wire_report "bulk ${BULK_BYTES}B" "$LOG/bulk.before" "$LOG/bulk.after" "$LOG/bulk.report"

echo
echo "================= SMALL PACKETS ================="
# The workload a header compressor lives or dies by: one strict round trip at a
# time, so both directions carry many small segments instead of a few full ones.
# $SMALL_REQUESTS x $SMALL_BYTES is the instrument; it is recorded with the
# numbers because it decides them (§10, "instrument parameters are part of the
# method").
dev_snapshot "$LOG/small.before"
SMALL_START=$(date +%s.%N)
SMALL_RC=0
ip netns exec "$NS_VIS" uv run "$HERE/visitor.py" --target "$PUBLIC_IP:$PUBLIC_PORT" \
    --requests "$SMALL_REQUESTS" --size "$SMALL_BYTES" >"$LOG/visitor-small.log" 2>&1 \
    || SMALL_RC=$?
SMALL_END=$(date +%s.%N)
dev_snapshot "$LOG/small.after"
cat "$LOG/visitor-small.log"
awk -v s="$SMALL_START" -v e="$SMALL_END" -v n="$SMALL_REQUESTS" -v b="$SMALL_BYTES" 'BEGIN {
    d = e - s
    if (d <= 0) d = 0.000001
    printf "%d round trips of %d B in %.3fs, %.0f round trips/s (diagnostic, not a benchmark)\n",
        n, b, d, n / d
}'
wire_report "small ${SMALL_REQUESTS}x${SMALL_BYTES}B" "$LOG/small.before" "$LOG/small.after" \
    "$LOG/small.report"

echo
echo "================= NEGATIVE: NO SERVER SWITCH ================="
# The switch is the *server's*: a server whose config has no
# `[server.transparent]` must refuse a transparent registration by policy, and
# must do so before it looks at a device. The order is proven, not asserted:
# the device is DELETED from the server namespace first, so an implementation
# that checked the interface first would answer with the "does not exist"
# recipe instead of the policy refusal.
kill "$SRV_PID" "$CLI_PID" 2>/dev/null || true
sleep 0.5
ip netns exec "$NS_SRV" ip link del "$TUN_SRV"
write_no_l3_config
echo "--- server-no-l3.toml ---"
cat "$LOG/server-no-l3.toml"

ip netns exec "$NS_SRV" "$BIN" --server "$LOG/server-no-l3.toml" >"$LOG/no-l3-server.log" 2>&1 &
NO_L3_SRV=$!
PIDS+=("$NO_L3_SRV")
if ! wait_port "$NS_SRV" "$CONTROL_PORT"; then
    echo "the no-L3 server never listened on :$CONTROL_PORT" >&2
    dump_log "$LOG/no-l3-server.log"
    exit 1
fi
ip netns exec "$NS_CLI" "$BIN" --transparent "$LOG/client.toml" >"$LOG/no-l3-client.log" 2>&1 &
NO_L3_CLI=$!
PIDS+=("$NO_L3_CLI")

# Wait for the refusal itself rather than for a fixed delay.
if ! wait_for "$LOG/no-l3-server.log" "does not serve transparent"; then
    echo "the no-L3 server never logged a refusal" >&2
    dump_log "$LOG/no-l3-server.log"
fi
echo "--- no-L3 server log ---"
cat "$LOG/no-l3-server.log"
echo "--- no-L3 client log ---"
cat "$LOG/no-l3-client.log" 2>/dev/null || true

echo
echo "================= VERDICT ================="
FAILURES=0
pass() {
    echo "PASS $1"
}
fail() {
    echo "FAIL $1"
    FAILURES=$((FAILURES + 1))
}

if [ "$VISITOR_RC" -eq 0 ]; then
    pass "echo round-trip"
else
    fail "echo round-trip (visitor exit $VISITOR_RC)"
fi

if grep -qF "PEER $VIS_IP:" "$LOG/echo.log" \
    && { [ -z "$VIS_PORT" ] || grep -qF "PEER $PEER_LITERAL" "$LOG/echo.log"; }; then
    pass "backend saw the visitor's real address"
else
    fail "backend saw the visitor's real address (expected PEER $PEER_LITERAL in the echo log)"
fi

if [ "$SNAPSHOT_LIVE" -ne 1 ]; then
    fail "server holds no connection state for the visitor flow (no live ESTAB row for $PEER_LITERAL)"
elif grep -qF ":$PUBLIC_PORT" "$LOG/srv-sockets.txt"; then
    fail "server holds no connection state for the visitor flow (a socket on $PUBLIC_PORT is in $NS_SRV)"
elif grep -qE "dst=$PUBLIC_IP|dport=$PUBLIC_PORT" "$LOG/srv-conntrack.txt"; then
    fail "server holds no connection state for the visitor flow (conntrack has an entry)"
else
    pass "server holds no connection state for the visitor flow"
fi

if [ "$SNAPSHOT_LIVE" -ne 1 ]; then
    fail "the client namespace owns the accepted connection (no live ESTAB row for $PEER_LITERAL)"
elif ! estab_line_has_both "$LOG/cli-sockets.txt" "$PUBLIC_IP:$PUBLIC_PORT" "$VIS_IP"; then
    fail "the client namespace owns the accepted connection (no $PUBLIC_IP:$PUBLIC_PORT <- $VIS_IP row)"
else
    pass "the client namespace owns the accepted connection"
fi

if [ "$BULK_RC" -eq 0 ] && grep -q "^VISITOR OK" "$LOG/visitor-bulk.log"; then
    pass "200 KB round-trip (no MTU black hole)"
else
    fail "200 KB round-trip (no MTU black hole)"
fi

if [ "$SMALL_RC" -eq 0 ] && grep -q "^VISITOR OK" "$LOG/visitor-small.log"; then
    pass "$SMALL_REQUESTS small round trips stay byte-exact"
else
    fail "$SMALL_REQUESTS small round trips stay byte-exact (exit $SMALL_RC)"
fi

if grep -qF "does not serve transparent (L3) services" "$LOG/no-l3-server.log"; then
    pass "a server without [server.transparent] refuses L3 by policy"
else
    fail "a server without [server.transparent] refuses L3 by policy"
fi

# The device is gone in that run, so a missing-interface recipe in the log would
# mean the policy check ran second — the exact ordering this milestone exists
# for.
if grep -qF "does not exist" "$LOG/no-l3-server.log"; then
    fail "the policy refusal comes before the device is looked at (the log carries the missing-interface recipe)"
else
    pass "the policy refusal comes before the device is looked at"
fi

exit "$FAILURES"
