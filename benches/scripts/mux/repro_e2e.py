# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""End-to-end smoke test for the multiplexed data path (0.7.0+).

Not part of the Soak model: this is the scenario check that the tunnel still
carries a visitor round trip at all (it began as the reproduction script for
the 0.7.0 stall). Requires target/release/molehill built with the default
features (which include `multiplex`). Starts a local echo backend plus a
molehill server+client pair using the default `mode = "multiplex"`, waits for
the registration, then requires three visitor echo round-trips.

Usage: uv run repro_e2e.py
"""

import atexit
import contextlib
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

BIN = os.environ.get(
    "MOLEHILL_BIN", str(Path(__file__).parents[3] / "target/release/molehill")
)
WORK = Path(tempfile.mkdtemp(prefix="molehill-mux."))
PING = b"ping"
CONTROL_PORT = 23332
VISITOR_PORT = 52021
ECHO_BACKEND_PORT = 60002
VISITS = 3


def wait_registration(log: Path, timeout_s: float = 10.0) -> bool:
    end = time.time() + timeout_s
    while time.time() < end:
        if "Control channel established" in log.read_text(errors="ignore"):
            return True
        time.sleep(0.25)
    return False


def visitor_ping(i: int) -> None:
    """One visitor round trip; raises when the echo does not come back."""
    s = socket.create_connection(("127.0.0.1", VISITOR_PORT), timeout=3)
    s.settimeout(3)
    try:
        s.sendall(PING)
        got = b""
        while len(got) < len(PING):  # the echo may arrive in several segments
            chunk = s.recv(len(PING) - len(got))
            if not chunk:
                break
            got += chunk
        if got != PING:
            raise AssertionError(f"echo mismatch: sent {PING!r}, got {got!r}")
        print(f"echo{i}={got!r}")
    finally:
        s.close()


def write_configs() -> None:
    """The scenario's configs: the multiplex default, one echo service."""
    (WORK / "server.toml").write_text(f"""[server]
default_token = "bench"
allow_ports = ["{VISITOR_PORT}"]
[server.control]
bind_addr = "0.0.0.0:{CONTROL_PORT}"
""")
    # `[client.data]` is omitted on purpose: the multiplex default applies.
    (WORK / "client.toml").write_text(f"""[client]
default_token = "bench"
[client.control]
default_remote_addr = "127.0.0.1:{CONTROL_PORT}"
[client.transport]
type = "plain"
[client.services.echo]
local_addr = "127.0.0.1:{ECHO_BACKEND_PORT}"
remote_bind_addr = "0.0.0.0:{VISITOR_PORT}"
pool_size = 8
""")


def start_echo_backend() -> socket.socket:
    """The local echo server the tunnel forwards to (one thread per conn)."""
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", ECHO_BACKEND_PORT))
    srv.listen(64)

    def serve(conn: socket.socket) -> None:
        try:
            while True:
                data = conn.recv(65536)
                if not data:
                    break
                conn.sendall(data)
        except OSError:
            pass
        finally:
            conn.close()

    def accept_loop() -> None:
        srv.settimeout(0.5)
        while True:
            try:
                conn, _ = srv.accept()
            except TimeoutError:
                continue
            except OSError:
                break  # the socket was closed; the scenario is over
            threading.Thread(target=serve, args=(conn,), daemon=True).start()

    threading.Thread(target=accept_loop, daemon=True).start()
    return srv


def start_pair() -> list:
    """Start the molehill server/client pair; the client logs where we read."""
    with (WORK / "client.log").open("w") as client_log:
        procs = [
            subprocess.Popen(
                [BIN, "--server", str(WORK / "server.toml")],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            ),
            subprocess.Popen(
                [BIN, "--client", str(WORK / "client.toml")],
                stdout=client_log,
                stderr=subprocess.STDOUT,
            ),
        ]

    def cleanup() -> None:
        for p in procs:
            with contextlib.suppress(OSError):
                p.send_signal(signal.SIGKILL)

    atexit.register(cleanup)
    return procs


def tail_client_log() -> str:
    return (WORK / "client.log").read_text(errors="ignore")[-500:]


def main() -> None:
    if not os.access(BIN, os.X_OK):
        sys.exit(f"missing {BIN} (build with: cargo build --release)")
    # Only reap molehill leftovers of THIS scenario (configs under a
    # molehill-mux.* workdir) — never a bench run in progress.
    subprocess.run(
        ["pkill", "-9", "-f", r"molehill.*molehill-mux\."],
        capture_output=True,
        check=False,
    )
    time.sleep(0.3)

    write_configs()
    start_echo_backend()
    start_pair()

    if not wait_registration(WORK / "client.log"):
        print("REG FAILED")
        print(tail_client_log())
        sys.exit(1)
    time.sleep(0.5)

    for i in range(1, VISITS + 1):
        try:
            visitor_ping(i)
        except Exception as e:  # noqa: BLE001 — report any failure, not a few
            print(f"visitor{i} failed: {e!r}")
            print("MUX E2E FAILED")
            print(tail_client_log())
            sys.exit(1)

    print("MUX E2E OK")


if __name__ == "__main__":
    main()
