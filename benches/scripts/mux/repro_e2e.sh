#!/usr/bin/env bash
# End-to-end smoke test for the multiplexed data path (0.7.0+).
#
# Requires target/release/molehill built with the default features (which
# include `multiplex`). Starts a local echo backend plus a molehill
# server+client pair using the default `mux = true`, registers the service, then requires
# three visitor echo round-trips through the yamux tunnel. This used to be
# the reproduction script for the 0.7.0 stall; the same flow now asserts the
# fixed behavior.
set -u

cd "$(dirname "$0")/../../.." || exit 1
BIN=${MOLEHILL_BIN:-$PWD/target/release/molehill}
[ -x "$BIN" ] || { echo "missing $BIN (build with: cargo build --release)" >&2; exit 1; }

pkill -9 -x molehill 2>/dev/null; sleep 0.3

cat > /tmp/molehill_mux_server.toml <<'TOML'
[server]
bind_addr = "0.0.0.0:23332"
default_token = "bench"
allow_ports = ["52021"]
[server.transport]
type = "tcp"
TOML

cat > /tmp/molehill_mux_client.toml <<'TOML'
# `mux` is omitted on purpose: true is the 0.7.0 default.
[client]
remote_addr = "127.0.0.1:23332"
default_token = "bench"
[client.transport]
type = "tcp"
[client.services.echo]
local_addr = "127.0.0.1:60002"
remote_bind_addr = "0.0.0.0:52021"
pool_size = 8
TOML

python3 - <<'PY' &
import socket, threading
srv = socket.socket(); srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", 60002)); srv.listen(64)
def serve(c):
    try:
        while True:
            d = c.recv(65536)
            if not d: break
            c.sendall(d)
    except OSError: pass
    finally: c.close()
while True:
    c, _ = srv.accept()
    threading.Thread(target=serve, args=(c,), daemon=True).start()
PY
ECHO=$!

cleanup() { kill -9 "$ECHO" "$SRV" "$CLI" 2>/dev/null; wait 2>/dev/null; }
trap cleanup EXIT

"$BIN" --server /tmp/molehill_mux_server.toml > /tmp/molehill_mux_server.log 2>&1 &
SRV=$!
"$BIN" --client /tmp/molehill_mux_client.toml > /tmp/molehill_mux_client.log 2>&1 &
CLI=$!

ok=0
for _ in $(seq 1 40); do
    grep -q "Control channel established" /tmp/molehill_mux_client.log 2>/dev/null && { ok=1; break; }
    sleep 0.25
done
[ "$ok" -eq 1 ] || { echo "REG FAILED"; tail -5 /tmp/molehill_mux_client.log; exit 1; }
sleep 0.5

failed=0
for i in 1 2 3; do
    if ! python3 - "$i" <<'PY'
import socket, sys
i = sys.argv[1]
s = socket.create_connection(("127.0.0.1", 52021)); s.settimeout(3)
try:
    s.sendall(b"ping"); d = s.recv(4)
    assert d == b"ping", d
    print(f"echo{i}={d!r}")
except Exception as e:
    print(f"visitor{i} failed: {e!r}")
    sys.exit(1)
finally:
    s.close()
PY
    then failed=1; break; fi
done

[ "$failed" -eq 0 ] || { echo "MUX E2E FAILED"; tail -20 /tmp/molehill_mux_client.log; exit 1; }
echo "MUX E2E OK"
