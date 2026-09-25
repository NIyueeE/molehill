# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""Sample RSS of frp and molehill server/client processes under load.

A quick side-by-side memory probe — not part of the Soak model, whose own RSS
axis is sampled per test in `benches/scripts/soak/`. Assumes both stacks are
already running and forwarding HTTP to a local backend, drives vegeta load
through each, and writes one RSS log (KiB) per process next to the working
directory for analysis.

Usage: uv run mem.py
"""

import os
import subprocess
import threading
import time
from pathlib import Path

LOAD_DURATION = "30s"
LOAD_RATE = "1000"
SAMPLE_INTERVAL_S = 1.0
SETTLE_AFTER_LOAD_S = 10.0
FRP_URL = "http://127.0.0.1:5203"
MOLEHILL_URL = "http://127.0.0.1:5202"


def sample(pid: int, log: Path, stop: threading.Event) -> None:
    """Write the process's RSS in KiB every interval until `stop` is set."""
    page_kib = os.sysconf("SC_PAGE_SIZE") // 1024
    with open(log, "w") as f:
        while not stop.is_set():
            try:
                with open(f"/proc/{pid}/statm") as fh:
                    rss_pages = int(fh.read().split()[1])
                f.write(f"{rss_pages * page_kib}\n")
                f.flush()
            except (OSError, ValueError, IndexError):
                break  # the process is gone; the log ends where it ended
            stop.wait(SAMPLE_INTERVAL_S)


def pidof(pattern: str) -> int:
    out = subprocess.run(
        ["pgrep", "-f", pattern], capture_output=True, text=True, check=False
    ).stdout.split()
    return int(out[0]) if out else 0


def run_load(url: str) -> None:
    shell_cmd = (
        f"echo GET {url} | vegeta attack -duration {LOAD_DURATION} -rate {LOAD_RATE}"
    )
    subprocess.run(["sh", "-c", shell_cmd], stdout=subprocess.DEVNULL, check=True)


class RssWatch:
    """The samplers started so far, so one call can stop them all."""

    def __init__(self) -> None:
        self.stop = threading.Event()
        self.logs: list[Path] = []
        self.threads: list[threading.Thread] = []

    def watch(self, pattern: str, logname: str) -> None:
        pid = pidof(pattern)
        if not pid:
            print(f"  {pattern}: not running")
            return
        log = Path(logname)
        self.logs.append(log)
        thread = threading.Thread(
            target=sample, args=(pid, log, self.stop), daemon=True
        )
        thread.start()
        self.threads.append(thread)

    def join(self) -> None:
        self.stop.set()
        for t in self.threads:
            t.join(timeout=2)


def main() -> None:
    watch = RssWatch()
    print("frp")
    watch.watch("frpc", "frpc-mem.log")
    watch.watch("frps", "frps-mem.log")
    run_load(FRP_URL)
    time.sleep(SETTLE_AFTER_LOAD_S)

    print("molehill")
    watch.watch(r"molehill.*--server", "molehills-mem.log")
    watch.watch(r"molehill.*--client", "molehillc-mem.log")
    run_load(MOLEHILL_URL)
    time.sleep(SETTLE_AFTER_LOAD_S)

    watch.join()
    print("logs:", ", ".join(str(path) for path in watch.logs))


if __name__ == "__main__":
    main()
