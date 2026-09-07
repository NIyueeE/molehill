# /// script
# requires-python = ">=3.11"
# dependencies = []
# ///
"""HTTP latency comparison (molehill vs frp) via vegeta.

Assumes both proxies are already running and forwarding to a local HTTP
backend: molehill on :5202, frp on :5203. vegeta must be on PATH.

Usage: uv run latency.py [rates...]   (default: 1 1000 2000 3000 4000 qps)
"""
import subprocess
import sys

RATES = sys.argv[1:] or ["1", "1000", "2000", "3000", "4000"]
DURATION = "60s"
TARGETS = {"frp": "http://127.0.0.1:5203", "molehill": "http://127.0.0.1:5202"}

if not (vegeta := subprocess.run(["sh", "-c", "command -v vegeta"],
                                 capture_output=True, text=True).stdout.strip()):
    sys.exit("vegeta required on PATH")


def attack(url: str, rate: str) -> str:
    name = f"{url.split(':')[-1]}-{rate}qps-{DURATION}.bin"
    with open(name, "wb") as out:
        subprocess.run(f"echo GET {url} | vegeta attack -rate {rate} "
                       f"-duration {DURATION}", shell=True, stdout=out,
                       check=True)
    report = subprocess.run(["vegeta", "report", name],
                            capture_output=True, text=True).stdout
    print(report)
    return name


for tool, url in TARGETS.items():
    print(f"warming up {tool}")
    subprocess.run(f"echo GET {url} | vegeta attack -duration 10s",
                   shell=True, stdout=subprocess.DEVNULL, check=True)
    for rate in RATES:
        print(f"{tool}-{rate}qps-{DURATION}")
        attack(url, rate)
