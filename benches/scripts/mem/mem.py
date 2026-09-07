# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Sample RSS of frp and molehill server/client processes under load.

Assumes both stacks are already running (frpc/frps, molehill server/client)
and forwards HTTP load via vegeta. Writes per-process RSS logs (bytes) next
to the working directory for analysis.

Usage: uv run mem.py
"""
import subprocess
import threading
import time
from pathlib import Path


def sample(pid: int, log: Path, stop: threading.Event) -> None:
    with open(log, "w") as f:
        while not stop.is_set():
            try:
                with open(f"/proc/{pid}/statm") as fh:
                    rss_kb = fh.read().split()[1]
                f.write(f"{int(rss_kb) * 4}\n")  # pages -> KB
                f.flush()
            except (OSError, ValueError, IndexError):
                break
            stop.wait(1.0)


def pidof(pattern: str) -> int:
    out = subprocess.run(["pgrep", "-f", pattern],
                         capture_output=True, text=True,
                         check=False).stdout.split()
    return int(out[0]) if out else 0


def run_load(url: str) -> None:
    subprocess.run(f"echo GET {url} | vegeta attack -duration 30s -rate 1000",
                   shell=True, stdout=subprocess.DEVNULL, check=True)


stop = threading.Event()
logs: list = []
samplers: list = []


def watch(pattern: str, logname: str) -> None:
    pid = pidof(pattern)
    if pid:
        log = Path(logname)
        logs.append(log)
        samplers.append(threading.Thread(target=sample, args=(pid, log, stop),
                                         daemon=True))
        samplers[-1].start()


print("frp")
watch("frpc", "frpc-mem.log")
watch("frps", "frps-mem.log")
run_load("http://127.0.0.1:5203")
time.sleep(10)

print("molehill")
watch(r"molehill.*--server", "molehills-mem.log")
watch(r"molehill.*--client", "molehillc-mem.log")
run_load("http://127.0.0.1:5202")
time.sleep(10)

stop.set()
for t in samplers:
    t.join(timeout=2)
print("logs:", ", ".join(str(l) for l in logs))
