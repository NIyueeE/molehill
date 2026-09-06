#!/usr/bin/env python3
"""Render the README benchmark chart from run_bench.sh results (schema v2).

Usage: plot_bench.py [results.json] [out.png]
Defaults: newest results-v*.json in this directory -> assets/benchmark-v<ver>.png

Panels (top row: loopback cell; bottom row: all cells incl. weak-network):
  1 throughput (loopback, 1/8 streams)   4 throughput per cell (log scale)
  2 connection-path RTT (loopback)       5 echo RTT per cell (log scale)
  3 memory (avg RSS)
"""
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

script_dir = Path(__file__).parent

def pick_results():
    if len(sys.argv) > 1:
        return Path(sys.argv[1])
    files = sorted(script_dir.glob("results-v*.json"))
    if not files:
        sys.exit("no results-v*.json found; run run_bench.sh first")
    return files[-1]

def pick_out(results_path):
    if len(sys.argv) > 2:
        return Path(sys.argv[2])
    ver = results_path.stem.removeprefix("results-")
    return script_dir.parents[2] / "assets" / f"benchmark-{ver}.png"

results_path = pick_results()
out_path = pick_out(results_path)
data = json.loads(results_path.read_text())
meta, results = data["meta"], data["results"]

cells = [c["name"] for c in meta.get("cells", [])] or ["loopback"]
loopback = "loopback" if "loopback" in cells else cells[0]

# molehill variants first, then peers, in stable alphabetical order
tools = sorted(results.keys(),
               key=lambda t: (0 if t.startswith("molehill") else 1, t))
palette = ["#d95f02", "#e7a17a", "#66c2a5", "#8da0cb", "#a6d854", "#e78ac3"]
colors = {t: palette[i % len(palette)] for i, t in enumerate(tools)}

fig, axes = plt.subplots(2, 3, figsize=(15.5, 8.0))
fig.subplots_adjust(left=0.06, right=0.98, bottom=0.09, top=0.90,
                    wspace=0.30, hspace=0.42)
(ax_thr, ax_rtt, ax_mem), (ax_cthr, ax_crtt, ax_note) = axes

def short(t):  # "molehill 0.7.2 (mux)" -> "molehill\n(mux)"
    parts = t.split()
    name = parts[0]
    rest = " ".join(p for p in parts[1:] if not p[0].isdigit())
    return f"{name}\n{rest}" if rest else name

# ---- 1: loopback throughput -------------------------------------------------
xs = range(len(tools))
width = 0.38
t1 = [results[t].get(loopback, {}).get("throughput_1stream_gbps", 0) for t in tools]
t8 = [results[t].get(loopback, {}).get("throughput_8streams_gbps", 0) for t in tools]
for off, vals, alpha, label in ((-width/2, t1, 0.55, "1 stream"),
                                (+width/2, t8, 1.0, "8 streams")):
    bars = ax_thr.bar([x + off for x in xs], vals, width, alpha=alpha,
                      color=[colors[t] for t in tools], label=label)
    for r in bars:
        ax_thr.annotate(f"{r.get_height():.1f}",
                        (r.get_x() + r.get_width()/2, r.get_height()),
                        ha="center", va="bottom", fontsize=7)
ax_thr.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
ax_thr.set_ylabel("TCP throughput (Gbit/s)")
ax_thr.set_ylim(0, max(t8) * 1.2 if t8 else 1)
ax_thr.set_title(f"Throughput ({loopback}-saturated)", fontsize=10)
ax_thr.legend(fontsize=8, loc="lower right")
ax_thr.grid(axis="y", alpha=0.3)

# ---- 2: loopback connection-path RTT ---------------------------------------
p50 = [results[t].get(loopback, {}).get("echo_rtt_ms", {}).get("p50", 0) for t in tools]
p99 = [results[t].get(loopback, {}).get("echo_rtt_ms", {}).get("p99", 0) for t in tools]
ax_rtt.bar(xs, p50, width * 1.4, color=[colors[t] for t in tools])
ax_rtt.errorbar(xs, p50,
                yerr=[[0.0] * len(p99), [hi - lo for lo, hi in zip(p50, p99)]],
                fmt="none", ecolor="black", elinewidth=1.1, capsize=4)
for x, v in zip(xs, p50):
    ax_rtt.annotate(f"{v:.3f}", (x, v), ha="center", va="bottom", fontsize=7,
                    xytext=(0, 5), textcoords="offset points")
ax_rtt.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
ax_rtt.set_ylabel("echo RTT (ms)")
ax_rtt.set_ylim(0, max(p99) * 1.3 if p99 else 1)
ax_rtt.set_title("Connection-path latency, p50 (whisker: p99)", fontsize=10)
ax_rtt.grid(axis="y", alpha=0.3)

# ---- 3: loopback memory -----------------------------------------------------
mem = [results[t].get(loopback, {}).get("memory_rss_kb", {}).get("total_avg_kb", 0)
       for t in tools]
mem_mib = [v / 1024 for v in mem]
ax_mem.bar(xs, mem_mib, width * 1.4, color=[colors[t] for t in tools])
for x, v in zip(xs, mem_mib):
    ax_mem.annotate(f"{v:.1f}", (x, v), ha="center", va="bottom", fontsize=7,
                    xytext=(0, 3), textcoords="offset points")
ax_mem.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
ax_mem.set_ylabel("avg RSS (MiB, server+client)")
ax_mem.set_ylim(0, max(mem_mib) * 1.25 if mem_mib else 1)
ax_mem.set_title("Memory (avg RSS)", fontsize=10)
ax_mem.grid(axis="y", alpha=0.3)

# ---- 4: throughput per cell (log scale — weak cells span decades) -----------
import numpy as np  # noqa: E402

ngroups = len(tools)
band = 0.8
for ci, cell in enumerate(cells):
    for ti, t in enumerate(tools):
        v = results[t].get(cell, {}).get("throughput_1stream_gbps", 0)
        x = ci + (ti - (ngroups - 1) / 2) * (band / ngroups)
        ax_cthr.bar(x, max(v, 1e-4), width=band / ngroups, color=colors[t])
        if v:
            ax_cthr.annotate(f"{v:.2f}" if v < 10 else f"{v:.0f}",
                             (x, v), ha="center", va="bottom", fontsize=6,
                             xytext=(0, 2), textcoords="offset points")
ax_cthr.set_yscale("log")
ax_cthr.set_xticks(range(len(cells)), cells, fontsize=8)
ax_cthr.set_ylabel("1-stream TCP (Gbit/s, log)")
ax_cthr.set_title("Throughput per network cell", fontsize=10)
ax_cthr.grid(axis="y", alpha=0.3)

# ---- 5: RTT per cell --------------------------------------------------------
for ti, t in enumerate(tools):
    vals = [results[t].get(c, {}).get("echo_rtt_ms", {}).get("p50", 0)
            for c in cells]
    xs2 = [ci + (ti - (ngroups - 1) / 2) * (band / ngroups)
           for ci in range(len(cells))]
    ax_crtt.bar(xs2, vals, width=band / ngroups, color=colors[t], label=t)
ax_crtt.set_yscale("log")
ax_crtt.set_xticks(range(len(cells)), cells, fontsize=8)
ax_crtt.set_ylabel("echo RTT p50 (ms, log)")
ax_crtt.set_title("Connection-path latency per cell", fontsize=10)
ax_crtt.legend(fontsize=7, loc="upper left", ncol=2)
ax_crtt.grid(axis="y", alpha=0.3)

# ---- 6: notes ---------------------------------------------------------------
ax_note.axis("off")
tv = meta.get("tool_versions", {})
lines = ["tool versions:"] + [f"  {k}: {v}" for k, v in tv.items()]
lines += ["", f"cells: {', '.join(cells)}",
          f"reps: {meta.get('reps')} / {meta.get('secs_per_rep_loopback')}s "
          f"(weak: {meta.get('secs_per_rep_weak')}s)",
          f"host: {meta.get('hostname')} / {meta.get('cpu')}",
          "", "reproduce: just bench && just bench-plot"]
ax_note.text(0.0, 0.98, "\n".join(lines), va="top", fontsize=8,
             family="monospace")

suptitle = (f"molehill vs peers — {meta.get('topology', '')}, "
            f"{meta.get('transport', '')} ({meta.get('date', '')})")
fig.suptitle(suptitle, fontsize=12)

out_path.parent.mkdir(parents=True, exist_ok=True)
fig.savefig(out_path, dpi=150)
print(f"wrote {out_path}")

# ---- markdown table for the README ------------------------------------------
def fmt(v, nd=1):
    return f"{v:.{nd}f}" if isinstance(v, (int, float)) else "-"

print("\n| Tool | thr 1-str | thr 8-str | RTT p50 | RTT p99 | RSS |",
      "|---|---|---|---|---|---|", sep="\n")
for t in tools:
    lb = results[t].get(loopback, {})
    print(f"| {t} | {fmt(lb.get('throughput_1stream_gbps'))} | "
          f"{fmt(lb.get('throughput_8streams_gbps'))} | "
          f"{fmt(lb.get('echo_rtt_ms', {}).get('p50'), 3)} | "
          f"{fmt(lb.get('echo_rtt_ms', {}).get('p99'), 3)} | "
          f"{fmt(lb.get('memory_rss_kb', {}).get('total_avg_kb', 0) / 1024)} MiB |")
for cell in cells:
    if cell == loopback:
        continue
    print(f"\nCell `{cell}`:", "| Tool | thr 1-str | RTT p50 | retransmits |",
          "|---|---|---|---|", sep="\n")
    for t in tools:
        d = results[t].get(cell, {})
        print(f"| {t} | {fmt(d.get('throughput_1stream_gbps'), 3)} | "
              f"{fmt(d.get('echo_rtt_ms', {}).get('p50'), 3)} | "
              f"{d.get('retransmits_1stream', '-')} |")
