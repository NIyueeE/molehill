# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""End-to-end smoke test for the multiplexed data path (0.7.0+).

Requires target/release/molehill built with the default features (which
include `multiplex`). Starts a local echo backend plus a molehill
server+client pair using the default `mode = "multiplex"`, registers the service,
then requires three visitor echo round-trips through the yamux tunnel. This
used to be the reproduction script for the 0.7.0 stall; the same flow now
asserts the fixed behavior.

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

BIN = os.environ.get("MOLEHILL_BIN",
                     str(Path(__file__).parents[3] / "target/release/molehill"))
WORK = Path(tempfile.mkdtemp(prefix="molehill-mux."))


def wait_registration(log: Path, timeout_s: float = 10.0) -> bool:
    end = time.time() + timeout_s
    while time.time() < end:
        if "Control channel established" in log.read_text(errors="ignore"):
            return True
        time.sleep(0.25)
    return False


def visitor_ping(i: int) -> None:
    s = socket.create_connection(("127.0.0.1", 52021))
    s.settimeout(3)
    try:
        s.sendall(b"ping")
        d = b""
        while len(d) < 4:  # echo may arrive in several segments
            chunk = s.recv(4 - len(d))
            if not chunk:
                break
            d += chunk
        assert d == b"ping", d
        print(f"echo{i}={d!r}")
    finally:
        s.close()


def main() -> None:
    if not os.access(BIN, os.X_OK):
        sys.exit(f"missing {BIN} (build with: cargo build --release)")
    # only reap molehill leftovers of THIS scenario (configs under a
    # molehill-mux.* workdir) — never a running benchmark matrix
    subprocess.run(["pkill", "-9", "-f", r"molehill.*molehill-mux\."],
                   capture_output=True, check=False)
    time.sleep(0.3)

    (WORK / "server.toml").write_text("""[server]
default_token = "bench"
allow_ports = ["52021"]
[server.control]
bind_addr = "0.0.0.0:23332"
""")
    # `[client.data]` is omitted on purpose: the multiplex default applies.
    (WORK / "client.toml").write_text("""[client]
default_token = "bench"
[client.control]
default_remote_addr = "127.0.0.1:23332"
[client.transport]
type = "plain"
[client.services.echo]
local_addr = "127.0.0.1:60002"
remote_bind_addr = "0.0.0.0:52021"
pool_size = 8
""")

    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 60002))
    srv.listen(64)

    def echo():
        srv.settimeout(0.5)
        while True:
            try:
                conn, _ = srv.accept()
            except TimeoutError:
                continue
            except OSError:
                break

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

            threading.Thread(target=serve, args=(conn,), daemon=True).start()

    threading.Thread(target=echo, daemon=True).start()

    procs = [
        subprocess.Popen([BIN, "--server", str(WORK / "server.toml")],
                         stdout=subprocess.DEVNULL,
                         stderr=subprocess.DEVNULL),
        subprocess.Popen([BIN, "--client", str(WORK / "client.toml")],
                         stdout=(WORK / "client.log").open("w"),
                         stderr=subprocess.STDOUT),
    ]

    def cleanup():
        for p in procs:
            with contextlib.suppress(OSError):
                p.send_signal(signal.SIGKILL)

    atexit.register(cleanup)

    if not wait_registration(WORK / "client.log"):
        print("REG FAILED")
        print((WORK / "client.log").read_text(errors="ignore")[-500:])
        sys.exit(1)
    time.sleep(0.5)

    for i in range(1, 4):
        try:
            visitor_ping(i)
        except Exception as e:
            print(f"visitor{i} failed: {e!r}")
            print("MUX E2E FAILED")
            print((WORK / "client.log").read_text(errors="ignore")[-500:])
            sys.exit(1)

    print("MUX E2E OK")


if __name__ == "__main__":
    main()
