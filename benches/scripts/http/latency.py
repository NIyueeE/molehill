# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""HTTP latency comparison (molehill vs frp) via vegeta.

A quick side-by-side latency probe — not part of the Soak model, whose own
interactive-stream instrument lives in `benches/scripts/soak/`. Assumes both
proxies are already running and forwarding to a local HTTP backend: molehill
on :5202, frp on :5203. vegeta must be on PATH.

Usage: uv run latency.py [rates...]   (default: 1 1000 2000 3000 4000 qps)
"""

import shutil
import subprocess
import sys
from pathlib import Path

DEFAULT_RATES = ["1", "1000", "2000", "3000", "4000"]
DURATION = "60s"
WARMUP = "10s"
TARGETS = {"frp": "http://127.0.0.1:5203", "molehill": "http://127.0.0.1:5202"}
OUT_DIR = Path("vegeta-results")


def vegeta(
    args: list, url: str, check: bool = True, **kwargs
) -> subprocess.CompletedProcess:
    """Run vegeta with a request line piped in (its only input format)."""
    shell_cmd = f"echo GET {url} | {' '.join(['vegeta', *args])}"
    return subprocess.run(["sh", "-c", shell_cmd], check=check, **kwargs)


def attack(url: str, rate: str) -> Path:
    """One vegeta attack at `rate`, plus its printed report."""
    name = OUT_DIR / f"{url.rsplit(':', maxsplit=1)[-1]}-{rate}qps-{DURATION}.bin"
    with open(name, "wb") as out:
        vegeta(["attack", "-rate", rate, "-duration", DURATION], url=url, stdout=out)
    report = subprocess.run(
        ["vegeta", "report", str(name)], capture_output=True, text=True, check=False
    ).stdout
    print(report)
    return name


def main() -> None:
    if shutil.which("vegeta") is None:
        sys.exit("vegeta required on PATH")
    OUT_DIR.mkdir(exist_ok=True)
    rates = sys.argv[1:] or DEFAULT_RATES
    for tool, url in TARGETS.items():
        print(f"warming up {tool}")
        vegeta(["attack", "-duration", WARMUP], url=url, stdout=subprocess.DEVNULL)
        for rate in rates:
            print(f"{tool}-{rate}qps-{DURATION}")
            attack(url, rate)


if __name__ == "__main__":
    main()
