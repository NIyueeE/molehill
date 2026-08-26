#!/usr/bin/env python3
"""Render the README benchmark chart from run_bench.sh JSON output."""
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

results_path = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parent / "results-v0.7.0.json"
out_path = Path(sys.argv[2]) if len(sys.argv) > 2 else Path(__file__).parents[3] / "assets" / "benchmark-v0.7.0.png"

data = json.loads(results_path.read_text())["results"]

ORDER = ["molehill 0.7.0 (mux)", "molehill 0.7.0 (mux=off)", "molehill 0.6.4", "frp 0.71.0"]
labels = ["v0.7.0\nmux", "v0.7.0\nmux=off", "v0.6.4", "frp\n0.71.0"]
colors = ["#d95f02", "#e7a17a", "#8da0cb", "#66c2a5"]  # highlight current release

tools = [t for t in ORDER if t in data]
idx = {t: ORDER.index(t) for t in tools}

fig, (ax1, ax2, ax3) = plt.subplots(1, 3, figsize=(14.5, 4.2))
fig.subplots_adjust(left=0.06, right=0.98, bottom=0.18, top=0.86, wspace=0.32)

# ---- throughput ------------------------------------------------------------
width = 0.38
xs = range(len(tools))
t1 = [data[t]["throughput_1stream_gbps"] for t in tools]
t8 = [data[t]["throughput_8streams_gbps"] for t in tools]
b1 = ax1.bar([x - width / 2 for x in xs], t1, width, color=[colors[i] for i in idx.values()],
             alpha=0.55, label="1 stream")
b8 = ax1.bar([x + width / 2 for x in xs], t8, width, color=[colors[i] for i in idx.values()],
             label="8 streams")
for bars in (b1, b8):
    for r in bars:
        ax1.annotate(f"{r.get_height():.1f}", (r.get_x() + r.get_width() / 2, r.get_height()),
                     ha="center", va="bottom", fontsize=8)
ax1.set_xticks(list(xs), labels)
ax1.set_ylabel("TCP throughput (Gbit/s)")
ax1.set_ylim(0, max(t8) * 1.15)
ax1.set_title("Throughput (loopback-saturated)")
ax1.legend(fontsize=8, loc="lower right")
ax1.grid(axis="y", alpha=0.3)

# ---- echo RTT --------------------------------------------------------------
p50 = [data[t]["echo_rtt_ms"]["p50"] for t in tools]
p99 = [data[t]["echo_rtt_ms"]["p99"] for t in tools]
bars = ax2.bar(xs, p50, width * 1.4, color=[colors[i] for i in idx.values()])
ax2.errorbar(xs, p50, yerr=[[0.0] * len(p99), [hi - lo for lo, hi in zip(p50, p99)]],
             fmt="none", ecolor="black", elinewidth=1.1, capsize=4)
for x, v in zip(xs, p50):
    ax2.annotate(f"{v:.3f}", (x, v), ha="center", va="bottom", fontsize=8,
                 xytext=(0, 5), textcoords="offset points")
ax2.set_xticks(list(xs), labels)
ax2.set_ylabel("echo RTT (ms)")
ax2.set_ylim(0, max(p99) * 1.25)
ax2.set_title("Connection-path latency, p50 (whisker: p99)")
ax2.grid(axis="y", alpha=0.3)

# ---- memory ----------------------------------------------------------------
mem_kb = [data[t].get("memory_rss_kb", {}).get("total_avg_kb", 0) for t in tools]
mem_mib = [v / 1024 for v in mem_kb]
pct = [data[t].get("memory_rss_kb", {}).get("pct_of_frp") for t in tools]
bars = ax3.bar(xs, mem_mib, width * 1.4, color=[colors[i] for i in idx.values()])
for x, v, p in zip(xs, mem_mib, pct):
    label = f"{v:.1f}"
    if p is not None:
        label += f"\n({p:.0f}%)"
    ax3.annotate(label, (x, v), ha="center", va="bottom", fontsize=8,
                 xytext=(0, 3), textcoords="offset points")
ax3.set_xticks(list(xs), labels)
ax3.set_ylabel("avg RSS (MiB, server+client)")
ax3.set_ylim(0, (max(mem_mib) if mem_mib else 1) * 1.25)
ax3.set_title("Memory (avg RSS)")
ax3.grid(axis="y", alpha=0.3)

meta_note = ""
try:
    meta_note = json.loads(results_path.read_text())["meta"]["date"]
except Exception:
    pass
fig.suptitle(f"molehill v0.7.0 vs v0.6.4 vs frp v0.71.0 — loopback, plain TCP ({meta_note})",
             fontsize=12)

out_path.parent.mkdir(parents=True, exist_ok=True)
fig.savefig(out_path, dpi=150)
print(f"wrote {out_path}")
