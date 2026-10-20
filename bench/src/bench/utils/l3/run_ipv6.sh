#!/usr/bin/env bash
# Transparent-L3 IPv6 acceptance harness.
#
#   sudo -n just l3-accept-ipv6
#   sudo -n env MOLEHILL_L3_WIRE=ip just l3-accept-ipv6
#
# A focused companion to run.sh, not a replacement: the full harness covers
# lanes, lending, valves and policy refusal on an IPv4 claim. This one proves
# the IPv6 data path end to end — visitor -> server -> TUN -> client -> the
# client's kernel — including a 60 KiB stream that exercises GSO splitting and
# checksums on the plain-IP wire. The transport between server and client is
# IPv4; only the claimed public address and the visitor path are IPv6, which is
# exactly the family boundary this test exists to cover.
#
# Root-only and Linux-only: it creates veth pairs, two TUN devices, a route and
# an IPv6 policy rule. LOG overrides the artifact directory (/tmp/l3-accept-ipv6).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../../../../.." && pwd)"
LOG="${LOG:-/tmp/l3-accept-ipv6}"
BIN="$ROOT/target/debug/molehill"

if [ "$(id -u)" -ne 0 ]; then
    echo "SKIP l3-accept-ipv6: it needs root for namespaces, TUN devices and routes." >&2
    echo "     re-run it as: sudo -n just l3-accept-ipv6" >&2
    exit 77
fi
case "$LOG" in
"" | "/") echo "refusing LOG=$LOG" >&2; exit 2 ;;
esac

for tool in ip sysctl cargo python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "missing required tool: $tool" >&2; exit 1; }
done
if [ ! -c /dev/net/tun ]; then
    echo "/dev/net/tun is missing: the harness and the daemons need the TUN driver" >&2
    exit 1
fi

NS_VIS=l3vis6
NS_SRV=l3srv6
NS_CLI=l3cli6
VIS4=10.60.0.2
SRV4=10.60.0.254
SRVCLI4=10.61.0.1
CLI4=10.61.0.2
VIS6=2001:db8:10::2
SRV6=2001:db8:10::254
PUB6=2001:db8:99::1
CONTROL_PORT=2433
PUBLIC_PORT=9443
TUN_SRV=l3srv6
TUN_CLI=l3cli6
ROUTE_TABLE=106
NOISE_SERVER_PRIVATE="kQiSRtS3bs8BoGCJYgFnl1FLrTG1lV53Dj8jSjmg8tE="
NOISE_SERVER_PUBLIC="mEnUEACy9UrTBmwoCJb6fcKWBRdvfD9XzuBVsroOLFg="
NOISE_CLIENT_PRIVATE="8JlJQXXN0lFqz5ghIjJRq3bfRiCDKJHuXxbfu5jIxWE="
NOISE_CLIENT_PUBLIC="zDdpcSQP+7Y5Mr+7UdEZMkz3gO6NGbQ1hko8/yt5knE="

PIDS=()
cleanup() {
    for pid in "${PIDS[@]:-}"; do kill "$pid" 2>/dev/null || true; done
    for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do ip netns del "$ns" 2>/dev/null || true; done
}
trap cleanup EXIT

rm -rf "$LOG"; mkdir -p "$LOG"
cleanup

cargo build --quiet --manifest-path "$ROOT/Cargo.toml"

ip netns add "$NS_VIS"
ip netns add "$NS_SRV"
ip netns add "$NS_CLI"
ip link add v-vis type veth peer name v-srv
ip link set v-vis netns "$NS_VIS"
ip link set v-srv netns "$NS_SRV"
ip link add v-cli type veth peer name v-srv2
ip link set v-cli netns "$NS_CLI"
ip link set v-srv2 netns "$NS_SRV"
ip -n "$NS_VIS" addr add "$VIS4/24" dev v-vis
ip -n "$NS_VIS" -6 addr add "$VIS6/64" dev v-vis
ip -n "$NS_SRV" addr add "$SRV4/24" dev v-srv
ip -n "$NS_SRV" -6 addr add "$SRV6/64" dev v-srv
ip -n "$NS_SRV" addr add "$SRVCLI4/24" dev v-srv2
ip -n "$NS_CLI" addr add "$CLI4/24" dev v-cli
for ns in "$NS_VIS" "$NS_SRV" "$NS_CLI"; do ip -n "$ns" link set lo up; done
ip -n "$NS_VIS" link set v-vis up
ip -n "$NS_SRV" link set v-srv up
ip -n "$NS_SRV" link set v-srv2 up
ip -n "$NS_CLI" link set v-cli up
ip -n "$NS_VIS" route add default via "$SRV4"
ip -n "$NS_VIS" -6 route add default via "$SRV6"
ip -n "$NS_CLI" route add default via "$SRVCLI4"

# The operator's side: both TUNs exist and are up; the server routes the claim
# into its device; the client owns the claim and policy-routes its replies.
ip netns exec "$NS_SRV" ip tuntap add dev "$TUN_SRV" mode tun
ip netns exec "$NS_CLI" ip tuntap add dev "$TUN_CLI" mode tun
ip -n "$NS_SRV" link set "$TUN_SRV" up mtu 1400
ip -n "$NS_CLI" link set "$TUN_CLI" up mtu 1400
ip -n "$NS_SRV" -6 route add "$PUB6/128" dev "$TUN_SRV"
ip -n "$NS_CLI" -6 addr add "$PUB6/128" dev "$TUN_CLI"
ip -n "$NS_CLI" -6 rule add from "$PUB6" lookup "$ROUTE_TABLE"
ip -n "$NS_CLI" -6 route add default dev "$TUN_CLI" table "$ROUTE_TABLE"
ip netns exec "$NS_SRV" sysctl -qw net.ipv4.ip_forward=1
ip netns exec "$NS_SRV" sysctl -qw net.ipv6.conf.all.forwarding=1
ip netns exec "$NS_SRV" sysctl -qw "net.ipv6.conf.$TUN_SRV.forwarding=1"
ip netns exec "$NS_CLI" sysctl -qw "net.ipv6.conf.$TUN_CLI.forwarding=1"

cat >"$LOG/server.toml" <<EOF
[server]
default_token = "bench"
allow_ports = ["$PUBLIC_PORT"]

[server.control]
bind_addr = "$SRV4:$CONTROL_PORT"

[server.transparent]
tun = "$TUN_SRV"

[server.noise]
private_key = "$NOISE_SERVER_PRIVATE"
public_key = "$NOISE_CLIENT_PUBLIC"
EOF

cat >"$LOG/client.toml" <<EOF
[transparent]
default_token = "bench"
tun = "$TUN_CLI"

[transparent.control]
default_remote_addr = "$SRV4:$CONTROL_PORT"

[transparent.noise]
private_key = "$NOISE_CLIENT_PRIVATE"
public_key = "$NOISE_SERVER_PUBLIC"

[transparent.claims.web]
remote_bind_addr = "[$PUB6]:$PUBLIC_PORT"
EOF

cat >"$LOG/echo.py" <<'PY'
import socket
import struct

s = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("2001:db8:99::1", 9443, 0, 0))
s.listen(8)
print("ECHO READY", flush=True)


def readn(c, n):
    b = b""
    while len(b) < n:
        x = c.recv(n - len(b))
        if not x:
            return b
        b += x
    return b


while True:
    c, a = s.accept()
    h = readn(c, 4)
    if len(h) < 4:
        c.close()
        continue
    n = struct.unpack("!I", h)[0]
    d = readn(c, n)
    print(f"PEER {a[0]}:{a[1]} RECV {len(d)}", flush=True)
    c.sendall(b"echo:" + d)
    c.close()
PY

ip netns exec "$NS_CLI" python3 "$LOG/echo.py" >"$LOG/echo.log" 2>&1 &
PIDS+=("$!")
ip netns exec "$NS_SRV" "$BIN" --server "$LOG/server.toml" >"$LOG/server.log" 2>&1 &
PIDS+=("$!")
sleep 1
ip netns exec "$NS_CLI" "$BIN" --transparent "$LOG/client.toml" >"$LOG/client.log" 2>&1 &
PIDS+=("$!")

echo "waiting for the IPv6 transparent path..."
ready=0
for _ in $(seq 1 100); do
    if ip netns exec "$NS_VIS" python3 - "$PUB6" "$PUBLIC_PORT" >/tmp/l3v6-probe 2>&1 <<'PY'
import socket
import struct
import sys

host, port = sys.argv[1], int(sys.argv[2])
s = socket.create_connection((host, port), 2)
msg = b"hi"
s.sendall(struct.pack("!I", len(msg)) + msg)
b = b""
while len(b) < len(msg) + 5:
    x = s.recv(64)
    if not x:
        break
    b += x
assert b == b"echo:hi", b
print("VISITOR OK")
s.close()
PY
    then ready=1; break; fi
    sleep 0.2
done
if [ "$ready" -ne 1 ]; then
    echo "FAIL the IPv6 visitor path never came up" >&2
    tail -40 "$LOG/server.log" >&2 || true
    tail -40 "$LOG/client.log" >&2 || true
    exit 1
fi

ip netns exec "$NS_VIS" python3 - "$PUB6" "$PUBLIC_PORT" <<'PY'
import socket
import struct
import sys

host, port = sys.argv[1], int(sys.argv[2])
blob = bytes(range(256)) * 235  # 60 KiB, enough for a GSO super-frame
s = socket.create_connection((host, port), 5)
s.sendall(struct.pack("!I", len(blob)) + blob)
want = len(blob) + 5
got = bytearray()
while len(got) < want:
    x = s.recv(65536)
    if not x:
        break
    got += x
assert bytes(got[5:]) == blob, (len(got), len(blob))
print("BULK OK", len(blob))
s.close()
PY

if grep -q "PEER $VIS6:" "$LOG/echo.log"; then
    echo "PASS the backend saw the visitor's IPv6 address"
else
    echo "FAIL the backend did not see the visitor's IPv6 address" >&2
    cat "$LOG/echo.log" >&2
    exit 1
fi
if ip netns exec "$NS_SRV" ss -tan | grep -q ":$PUBLIC_PORT"; then
    echo "FAIL the server namespace has a socket on the claimed port" >&2
    exit 1
fi
echo "PASS the server holds no socket for the IPv6 flow"
echo "PASS 60 KiB IPv6 stream round-trips byte-exactly"
echo "PASS IPv6 transparent acceptance"
