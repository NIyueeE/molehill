#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.5"]
# ///
"""Render the README benchmark charts from the results file (schema v3).

Three figures plus markdown tables — every comparison is a SINGLE
variable (no confounding):
- main chart (`assets/benchmark-vX.Y.Z.png`): molehill's default (mux, plain
  TCP) row vs the plain-TCP peers (frp, rathole, bore) — same-transport
  competition. Encrypted tools (e.g. chisel's SSH tunnel) are deliberately
  absent: their numbers are not comparable on the plain-TCP axis.
- mux chart (`assets/benchmark-mux-vX.Y.Z.png`): mux vs mux-off — the one
  variable is multiplexing on/off; loopback plus weak cells (under loss the
  single tunnel shares one loss/retransmit domain, mux-off does not).
- transport chart (`assets/benchmark-transport-vX.Y.Z.png`): mux vs noise vs
  tls — the one variable is the encrypted transport, multiplexing on for
  all three, with mux as the shared control.

Usage: plot_bench.py [results.json]
Default: newest results-v*.json in this directory.
"""
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

script_dir = Path(__file__).parent
PEERS = ("frp", "rathole", "bore")  # plain-TCP peers, no encryption

# molehill family gets its own palette so it reads at a glance; peers share
# a pastel cool palette that recedes behind it
FAMILY_COLORS = {"(mux)": "#d95f02", "(mux-off)": "#f2a25c",
                 "(noise)": "#8c2d04", "(tls)": "#4e79a7"}
PEER_PALETTE = ["#a3c6e8", "#b9dcb9", "#d5c2e0"]


def pick_results():
    if len(sys.argv) > 1:
        return Path(sys.argv[1])
    files = sorted(script_dir.glob("results-v*.json"))
    if not files:
        sys.exit("no results-v*.json found; run `just bench` first")
    return files[-1]


def molehill_family(results: dict) -> list:
    return [t for t in sorted(results) if t.startswith("molehill")]


def mux_row(results: dict) -> str:
    cands = [t for t in molehill_family(results)
             if "(mux)" in t and "mux-off" not in t]
    return min(cands) if cands else molehill_family(results)[0]


def peers_rows(results: dict) -> list:
    return [t for t in sorted(results) if t.split()[0] in PEERS]


def short(t: str) -> str:  # "molehill 0.7.2 (mux)" -> "molehill\n(mux)"
    parts = t.split()
    rest = " ".join(p for p in parts[1:] if not p[0].isdigit())
    return f"{parts[0]}\n{rest}" if rest else parts[0]


def tool_colors(tools: list) -> dict:
    colors, pi = {}, 0
    for t in tools:
        if t.startswith("molehill"):
            variant = t[t.find("("):] if "(" in t else ""
            colors[t] = FAMILY_COLORS.get(variant, "#d95f02")
        else:
            colors[t] = PEER_PALETTE[pi % len(PEER_PALETTE)]
            pi += 1
    return colors


# --- small plotting helpers --------------------------------------------------
def bar_group(ax, tools, colors, cells, get, log=False, floor=1e-4,
              fmt=None, fontsize=6):
    """Grouped bars over cells (or a single synthetic group); get(t, cell)
    returns the value or None."""
    ngroups = len(tools)
    band = 0.8
    for ci, cell in enumerate(cells):
        for ti, t in enumerate(tools):
            v = get(t, cell)
            if v is None:
                continue
            x = ci + (ti - (ngroups - 1) / 2) * (band / ngroups)
            ax.bar(x, max(v, floor) if log else v, width=band / ngroups,
                   color=colors[t])
            if fmt:
                ax.annotate(fmt(v), (x, v), ha="center", va="bottom",
                            fontsize=fontsize, rotation=90,
                            xytext=(0, 2), textcoords="offset points")
    if log:
        ax.set_yscale("log")
    ax.set_xticks(range(len(cells)), cells, fontsize=8, rotation=12)
    ax.grid(axis="y", alpha=0.3)


def read(results, t, cell, key, *path):
    v = results.get(t, {}).get(cell, {}).get(key)
    for p in path:
        if not isinstance(v, dict):
            return None
        v = v.get(p)
    return v


def fmt_table(v, nd=1):
    return f"{v:.{nd}f}" if isinstance(v, (int, float)) else "-"


# --- figures ---------------------------------------------------------------
def render_main(results, meta, out_path):
    """molehill default (mux) vs plain-TCP peers — the release chart."""
    tools = [mux_row(results), *peers_rows(results)]
    cells = [c["name"] for c in meta.get("cells", [])] or ["loopback"]
    loopback = "loopback" if "loopback" in cells else cells[0]
    colors = tool_colors(tools)

    fig, axes = plt.subplots(2, 4, figsize=(19.5, 8.5))
    fig.subplots_adjust(left=0.05, right=0.98, bottom=0.15, top=0.90,
                        wspace=0.28, hspace=0.45)
    (ax_thr, ax_rtt, ax_mem, ax_hol), (ax_cthr, ax_crtt, ax_udprtt,
                                       ax_udploss) = axes
    xs = range(len(tools))
    width = 0.38

    t1 = [read(results, t, loopback, "throughput_1stream_gbps") or 0
          for t in tools]
    t8 = [read(results, t, loopback, "throughput_8streams_gbps") or 0
          for t in tools]
    for off, vals, alpha, label in ((-width / 2, t1, 0.55, "1 stream"),
                                    (+width / 2, t8, 1.0, "8 streams")):
        bars = ax_thr.bar([x + off for x in xs], vals, width, alpha=alpha,
                          color=[colors[t] for t in tools], label=label)
        for r in bars:
            ax_thr.annotate(f"{r.get_height():.1f}",
                            (r.get_x() + r.get_width() / 2, r.get_height()),
                            ha="center", va="bottom", fontsize=6, rotation=90)
    ax_thr.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_thr.set_ylabel("TCP throughput (Gbit/s)")
    ax_thr.set_ylim(0, max(t8) * 1.2 if t8 else 1)
    ax_thr.set_title(f"Throughput ({loopback}, through the tunnel)",
                     fontsize=10)
    ax_thr.legend(fontsize=8, loc="lower right")
    ax_thr.grid(axis="y", alpha=0.3)

    p50 = [(read(results, t, loopback, "echo_rtt_ms") or {}).get("p50") or 0
           for t in tools]
    p99 = [(read(results, t, loopback, "echo_rtt_ms") or {}).get("p99") or 0
           for t in tools]
    ax_rtt.bar(xs, p50, width * 1.4, color=[colors[t] for t in tools])
    ax_rtt.errorbar(
        xs, p50,
        yerr=[[0.0] * len(p99), [hi - lo for lo, hi in zip(p50, p99)]],
        fmt="none", ecolor="black", elinewidth=1.1, capsize=4)
    for x, v in zip(xs, p50):
        ax_rtt.annotate(f"{v:.3f}", (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 5),
                        textcoords="offset points")
    ax_rtt.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_rtt.set_ylabel("echo RTT (ms)")
    ax_rtt.set_ylim(0, max(p99) * 1.3 if p99 else 1)
    ax_rtt.set_title("Connection-path latency, p50 (whisker: p99)",
                     fontsize=10)
    ax_rtt.grid(axis="y", alpha=0.3)

    mem = [(read(results, t, loopback, "memory_rss_kb") or {})
           .get("total_avg_kb", 0) for t in tools]
    mem_mib = [v / 1024 for v in mem]
    ax_mem.bar(xs, mem_mib, width * 1.4, color=[colors[t] for t in tools])
    for x, v in zip(xs, mem_mib):
        ax_mem.annotate(f"{v:.1f}", (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 3),
                        textcoords="offset points")
    ax_mem.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_mem.set_ylabel("avg RSS (MiB, server+client)")
    ax_mem.set_ylim(0, max(mem_mib) * 1.25 if mem_mib else 1)
    ax_mem.set_title("Memory (avg RSS)", fontsize=10)
    ax_mem.grid(axis="y", alpha=0.3)

    def hol_gap(t, cell):
        return (read(results, t, cell, "hol") or {}).get("ping_max_gap_ms")
    bar_group(ax_hol, tools, colors, cells, hol_gap, log=True, floor=1e-1,
              fmt=lambda v: f"{v:.0f}")
    ax_hol.set_ylabel("pinger max gap (ms, log)")
    ax_hol.set_title("HoL probe: pinger stall under bulk load", fontsize=10)

    def thr1(t, cell):
        return read(results, t, cell, "throughput_1stream_gbps")
    bar_group(ax_cthr, tools, colors, cells, thr1, log=True,
              fmt=lambda v: f"{v:.2f}" if v < 10 else f"{v:.0f}")
    ax_cthr.set_ylabel("1-stream TCP (Gbit/s, log)")
    ax_cthr.set_title("Throughput per network cell", fontsize=10)

    def rtt50(t, cell):
        return (read(results, t, cell, "echo_rtt_ms") or {}).get("p50")
    bar_group(ax_crtt, tools, colors, cells, rtt50, log=True, floor=1e-1)
    ax_crtt.set_ylabel("echo RTT p50 (ms, log)")
    ax_crtt.set_title("Connection-path latency per cell", fontsize=10)

    def udp_rtt99(t, cell):
        return (read(results, t, cell, "udp_rtt_ms") or {}).get("p99")
    bar_group(ax_udprtt, tools, colors, cells, udp_rtt99, log=True,
              floor=1e-1)
    ax_udprtt.set_ylabel("UDP RTT p99 (ms, log)")
    ax_udprtt.set_title("UDP session RTT p99 per cell", fontsize=10)

    def udp_loss(t, cell):
        return read(results, t, cell, "udp_loss_pct")
    bar_group(ax_udploss, tools, colors, cells, udp_loss)
    ax_udploss.set_ylabel("UDP loss (%)")
    ax_udploss.set_title("UDP session loss per cell", fontsize=10)

    footer(meta, "reproduce: just bench && just bench-plot")
    fig.suptitle(
        f"molehill (default mux) vs plain-TCP peers — "
        f"{meta.get('topology', '')}, {meta.get('transport', '')} "
        f"({meta.get('date', '')})", fontsize=12)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_path, dpi=150)
    plt.close(fig)
    print(f"wrote {out_path}")


def loopback_panels(axes, tools, colors, results, loopback):
    """Three loopback panels: throughput (1/8 streams), RTT, memory."""
    ax_thr, ax_rtt, ax_mem = axes
    xs = range(len(tools))
    width = 0.34

    t1 = [read(results, t, loopback, "throughput_1stream_gbps") or 0
          for t in tools]
    t8 = [read(results, t, loopback, "throughput_8streams_gbps") or 0
          for t in tools]
    for off, vals, alpha, label in ((-width / 2, t1, 0.55, "1 stream"),
                                    (+width / 2, t8, 1.0, "8 streams")):
        bars = ax_thr.bar([x + off for x in xs], vals, width, alpha=alpha,
                          color=[colors[t] for t in tools], label=label)
        for r in bars:
            ax_thr.annotate(f"{r.get_height():.1f}",
                            (r.get_x() + r.get_width() / 2, r.get_height()),
                            ha="center", va="bottom", fontsize=6, rotation=90)
    ax_thr.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_thr.set_ylabel("TCP throughput (Gbit/s)")
    ax_thr.set_ylim(0, max(t8) * 1.2 if t8 else 1)
    ax_thr.set_title(f"Throughput ({loopback})", fontsize=10)
    ax_thr.legend(fontsize=8, loc="lower right")
    ax_thr.grid(axis="y", alpha=0.3)

    p50 = [(read(results, t, loopback, "echo_rtt_ms") or {}).get("p50") or 0
           for t in tools]
    p99 = [(read(results, t, loopback, "echo_rtt_ms") or {}).get("p99") or 0
           for t in tools]
    ax_rtt.bar(xs, p50, width * 1.4, color=[colors[t] for t in tools])
    ax_rtt.errorbar(
        xs, p50,
        yerr=[[0.0] * len(p99), [hi - lo for lo, hi in zip(p50, p99)]],
        fmt="none", ecolor="black", elinewidth=1.1, capsize=4)
    for x, v in zip(xs, p50):
        ax_rtt.annotate(f"{v:.3f}", (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 5),
                        textcoords="offset points")
    ax_rtt.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_rtt.set_ylabel("echo RTT (ms)")
    ax_rtt.set_ylim(0, max(p99) * 1.3 if p99 else 1)
    ax_rtt.set_title("Connection-path latency, p50 (whisker: p99)",
                     fontsize=10)
    ax_rtt.grid(axis="y", alpha=0.3)

    mem = [(read(results, t, loopback, "memory_rss_kb") or {})
           .get("total_avg_kb", 0) for t in tools]
    mem_mib = [v / 1024 for v in mem]
    ax_mem.bar(xs, mem_mib, width * 1.4, color=[colors[t] for t in tools])
    for x, v in zip(xs, mem_mib):
        ax_mem.annotate(f"{v:.1f}", (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 3),
                        textcoords="offset points")
    ax_mem.set_xticks(list(xs), [short(t) for t in tools], fontsize=8)
    ax_mem.set_ylabel("avg RSS (MiB, server+client)")
    ax_mem.set_ylim(0, max(mem_mib) * 1.25 if mem_mib else 1)
    ax_mem.set_title("Memory (avg RSS)", fontsize=10)
    ax_mem.grid(axis="y", alpha=0.3)


def render_dimension(results, meta, out_path, tools, suptitle):
    """Single-variable comparison chart: loopback panels on top, weak-cell
    panels below. `tools` share the mux control (one variable only)."""
    cells = [c["name"] for c in meta.get("cells", [])] or ["loopback"]
    loopback = "loopback" if "loopback" in cells else cells[0]
    colors = tool_colors(tools)

    fig, axes = plt.subplots(2, 3, figsize=(15, 8.5))
    fig.subplots_adjust(left=0.06, right=0.98, bottom=0.15, top=0.90,
                        wspace=0.3, hspace=0.45)
    loopback_panels(axes[0], tools, colors, results, loopback)

    (ax_cthr, ax_crtt, ax_udp) = axes[1]
    def thr1(t, cell):
        return read(results, t, cell, "throughput_1stream_gbps")
    bar_group(ax_cthr, tools, colors, cells, thr1, log=True,
              fmt=lambda v: f"{v:.2f}" if v < 10 else f"{v:.0f}")
    ax_cthr.set_ylabel("1-stream TCP (Gbit/s, log)")
    ax_cthr.set_title("Throughput per network cell", fontsize=10)

    def rtt50(t, cell):
        return (read(results, t, cell, "echo_rtt_ms") or {}).get("p50")
    bar_group(ax_crtt, tools, colors, cells, rtt50, log=True, floor=1e-1)
    ax_crtt.set_ylabel("echo RTT p50 (ms, log)")
    ax_crtt.set_title("Connection-path latency per cell", fontsize=10)

    def udp_rtt99(t, cell):
        return (read(results, t, cell, "udp_rtt_ms") or {}).get("p99")
    bar_group(ax_udp, tools, colors, cells, udp_rtt99, log=True, floor=1e-1)
    ax_udp.set_ylabel("UDP RTT p99 (ms, log)")
    ax_udp.set_title("UDP session RTT p99 per cell", fontsize=10)

    footer(meta, "reproduce: just bench && just bench-plot")
    fig.suptitle(suptitle, fontsize=12)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_path, dpi=150)
    plt.close(fig)
    print(f"wrote {out_path}")


def render_mux(results, meta, out_path):
    """Multiplexing cost: mux vs mux-off. ONE variable (multiplexing on/off),
    loopback plus the weak cells — under loss the single-tunnel (mux) shares
    one loss/retransmit domain while mux-off does not, which is exactly the
    behavior worth measuring."""
    mux = mux_row(results)
    off = [t for t in molehill_family(results) if "(mux-off)" in t]
    render_dimension(results, meta, out_path, [mux, *off],
                     "Multiplexing cost — mux vs mux-off, one variable "
                     f"({meta.get('date', '')})")


def render_transport(results, meta, out_path):
    """Transport cost: mux vs noise vs tls. ONE variable (the encrypted
    transport), multiplexing on for all three; mux is the shared control.
    Loopback panels plus the weak-cell behavior."""
    mux = mux_row(results)
    tools = [mux] + [t for t in molehill_family(results)
                     if "(noise)" in t or "(tls)" in t]
    render_dimension(results, meta, out_path, tools,
                     "Transport cost — mux vs noise vs tls, one variable, "
                     f"mux on ({meta.get('date', '')})")


def footer(meta, reproduce):
    tv = meta.get("tool_versions", {})
    lines = [f"tools: {', '.join(f'{k} {v}' for k, v in tv.items())}",
             (f"cells: {', '.join(c['name'] for c in meta.get('cells', []))}"
              f"  |  reps: {meta.get('reps')} / "
              f"{meta.get('secs_per_rep_loopback')}s "
              f"(weak: {meta.get('secs_per_rep_weak')}s)  |  "
              f"host: {meta.get('hostname')}"),
             reproduce]
    plt.gcf().text(0.05, 0.015, "\n".join(lines), fontsize=7,
                   family="monospace")


# --- markdown tables --------------------------------------------------------
def print_tables(results, meta):
    cells = [c["name"] for c in meta.get("cells", [])] or ["loopback"]
    loopback = "loopback" if "loopback" in cells else cells[0]

    print("\n### Loopback (plain-TCP comparison)")
    print("| Tool | thr 1-str | thr 8-str | RTT p50 | RTT p99 | "
          "steady p99 | UDP loss | RSS |",
          "|---|---|---|---|---|---|---|---|", sep="\n")
    for t in [mux_row(results), *peers_rows(results)]:
        lb = results[t].get(loopback, {})
        echo = lb.get("echo_rtt_ms") or {}
        steady = lb.get("tcp_steady_rtt_ms") or {}
        mem_kb = (lb.get("memory_rss_kb") or {}).get("total_avg_kb")
        print(f"| {t} | {fmt_table(lb.get('throughput_1stream_gbps'))} | "
              f"{fmt_table(lb.get('throughput_8streams_gbps'))} | "
              f"{fmt_table(echo.get('p50'), 3)} | "
              f"{fmt_table(echo.get('p99'), 3)} | "
              f"{fmt_table(steady.get('p99'), 3)} | "
              f"{fmt_table(lb.get('udp_loss_pct'), 2)}% | "
              f"{fmt_table(mem_kb / 1024 if mem_kb else None)} MiB |")

    def family_row(t):
        lb = results[t].get(loopback, {})
        echo = lb.get("echo_rtt_ms") or {}
        steady = lb.get("tcp_steady_rtt_ms") or {}
        mem_kb = (lb.get("memory_rss_kb") or {}).get("total_avg_kb")
        return (f"| {t} | {fmt_table(lb.get('throughput_1stream_gbps'))} | "
                f"{fmt_table(lb.get('throughput_8streams_gbps'))} | "
                f"{fmt_table(echo.get('p50'), 3)} | "
                f"{fmt_table(echo.get('p99'), 3)} | "
                f"{fmt_table(steady.get('p99'), 3)} | "
                f"{fmt_table(mem_kb / 1024 if mem_kb else None)} MiB |")

    print("\n### Multiplexing cost (mux vs mux-off)")
    print("| Tool | thr 1-str | thr 8-str | RTT p50 | RTT p99 | "
          "steady p99 | RSS |",
          "|---|---|---|---|---|---|---|", sep="\n")
    for t in [mux_row(results)] + \
            [t for t in molehill_family(results) if "(mux-off)" in t]:
        print(family_row(t))

    print("\n### Transport cost (mux vs noise vs tls)")
    print("| Tool | thr 1-str | thr 8-str | RTT p50 | RTT p99 | "
          "steady p99 | RSS |",
          "|---|---|---|---|---|---|---|", sep="\n")
    for t in [mux_row(results)] + \
            [t for t in molehill_family(results)
             if "(noise)" in t or "(tls)" in t]:
        print(family_row(t))

    for cell in cells:
        if cell == loopback:
            continue
        print(f"\nCell `{cell}`:",
              "| Tool | thr 1-str | RTT p50 | retransmits | UDP loss | "
              "UDP max gap |",
              "|---|---|---|---|---|---|", sep="\n")
        for t in dict.fromkeys([mux_row(results), *peers_rows(results),
                               *molehill_family(results)]):
            d = results[t].get(cell, {})
            if not d:
                continue
            print(f"| {t} | "
                  f"{fmt_table(d.get('throughput_1stream_gbps'), 3)} | "
                  f"{fmt_table((d.get('echo_rtt_ms') or {}).get('p50'), 3)} | "
                  f"{d.get('retransmits_1stream') or '-'} | "
                  f"{fmt_table(d.get('udp_loss_pct'), 2)}% | "
                  f"{fmt_table(d.get('udp_max_gap_ms'), 1)} |")


def main():
    path = pick_results()
    data = json.loads(path.read_text())
    meta, results = data["meta"], data["results"]
    ver = path.stem.removeprefix("results-")
    assets = script_dir.parents[2] / "assets"
    render_main(results, meta, assets / f"benchmark-{ver}.png")
    render_mux(results, meta, assets / f"benchmark-mux-{ver}.png")
    render_transport(results, meta,
                     assets / f"benchmark-transport-{ver}.png")
    print_tables(results, meta)


if __name__ == "__main__":
    main()
