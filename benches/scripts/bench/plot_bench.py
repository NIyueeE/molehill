#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.5"]
# ///
"""Render the README benchmark charts from the results file (schema v3).

Five figures plus markdown tables — every comparison is a SINGLE
variable (no confounding):
- main chart (`assets/benchmark-vX.Y.Z.png`): molehill's default (mux, plain
  TCP) row vs the plain-TCP peers (frp, rathole, bore) — same-transport
  competition. Encrypted tools (e.g. chisel's SSH tunnel) are deliberately
  absent: their numbers are not comparable on the plain-TCP axis.
- mux chart (`assets/benchmark-mux-vX.Y.Z.png`): mux vs mux-off — the one
  variable is multiplexing on/off. mux-off runs the loopback cell only by
  design (bench.py appends the variant just to loopback): the architecture
  cost is a clean-path question, and the weak-cell multiplexing behavior is
  told by the count axis (count = 4 vs count = 1) instead.
- transport chart (`assets/benchmark-transport-vX.Y.Z.png`): plain vs noise —
  the one variable is the transport; multiplexing on and count = 4 for both.
- count chart (`assets/benchmark-count-vX.Y.Z.png`): count = 4 vs count = 1 —
  the one variable is the number of parallel tunnels; plain transport.
- carrier chart (`assets/benchmark-carrier-vX.Y.Z.png`): noise (TCP tunnels)
  vs kcp4 (KCP-over-UDP sessions) — ONE variable: what carries the data
  channels; noise control channel, mux on, count = 4 for both.
- cost chart (`assets/benchmark-cost-vX.Y.Z.png`): the configuration
  tradeoffs — CPU% (noise/KCP), connection churn, sustained UDP capacity,
  64-stream scale, mixed workload; all loopback panels.

Usage: plot_bench.py [results.json]
Default: newest results-v*.json in this directory.
"""
import json
import sys
from pathlib import Path

import matplotlib
from bench_lib import cell_sort_key

matplotlib.use("Agg")
import matplotlib.pyplot as plt

script_dir = Path(__file__).parent
PEERS = ("frp", "rathole", "bore")  # plain-TCP peers, no encryption

# molehill family gets its own palette so it reads at a glance; peers share
# a pastel cool palette that recedes behind it
FAMILY_COLORS = {"(mux)": "#d95f02", "(mux-off)": "#f2a25c",
                 "(mux1)": "#4e79a7", "(noise)": "#8c2d04",
                 "(kcp4)": "#17becf"}
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
    returns the value or None. A None slot (not measured / measurement
    failed) is marked with a grey 'x'; a measured 0 is labelled '0' (a
    zero-height bar is otherwise indistinguishable from an absent slot),
    so the charts separate absent, zero and non-zero data."""
    ngroups = len(tools)
    band = 0.8
    for ci, cell in enumerate(cells):
        for ti, t in enumerate(tools):
            v = get(t, cell)
            x = ci + (ti - (ngroups - 1) / 2) * (band / ngroups)
            if v is None:
                ax.annotate("x", (x, floor * 1.6 if log else 0.0),
                            ha="center", va="center", fontsize=8,
                            color="#999999")
                continue
            ax.bar(x, max(v, floor) if log else v, width=band / ngroups,
                   color=colors[t])
            label = fmt(v) if fmt else ("0" if v == 0 else None)
            if label is not None:
                ax.annotate(label, (x, v), ha="center", va="bottom",
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
    cells = stable_cells(meta)
    loopback = "loopback" if "loopback" in cells else cells[0]
    colors = tool_colors(tools)
    # The per-cell panels are a molehill-vs-peers comparison: a cell the
    # peers do not run is not a comparison at all, so it is not plotted
    # (its molehill-only story lives in the count/carrier charts). A metric
    # that is null inside a cell that IS compared still gets the grey 'x'.
    compare_cells = [c for c in cells
                     if sum(1 for t in tools
                            if read(results, t, c,
                                    "throughput_1stream_gbps") is not None)
                     >= 2] or cells

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
    bar_group(ax_hol, tools, colors, compare_cells, hol_gap, log=True,
              floor=1e-1, fmt=lambda v: f"{v:.0f}")
    ax_hol.set_ylabel("pinger max gap (ms, log)")
    ax_hol.set_title("HoL probe: pinger stall under bulk load", fontsize=10)

    def thr1(t, cell):
        return read(results, t, cell, "throughput_1stream_gbps")
    bar_group(ax_cthr, tools, colors, compare_cells, thr1, log=True,
              fmt=lambda v: f"{v:.2f}" if v < 10 else f"{v:.0f}")
    ax_cthr.set_ylabel("1-stream TCP (Gbit/s, log)")
    ax_cthr.set_title("Throughput per network cell", fontsize=10)

    def rtt50(t, cell):
        return (read(results, t, cell, "echo_rtt_ms") or {}).get("p50")
    bar_group(ax_crtt, tools, colors, compare_cells, rtt50, log=True,
              floor=1e-1)
    ax_crtt.set_ylabel("echo RTT p50 (ms, log)")
    ax_crtt.set_title("Connection-path latency per cell", fontsize=10)

    # bore is TCP-only and carries no UDP metrics: drop it from the UDP
    # panels so it does not occupy an empty slot
    udp_tools = [t for t in tools
                 if read(results, t, loopback, "udp_rtt_ms") is not None]

    def udp_rtt99(t, cell):
        return (read(results, t, cell, "udp_rtt_ms") or {}).get("p99")
    bar_group(ax_udprtt, udp_tools, colors, compare_cells, udp_rtt99,
              log=True, floor=1e-1)
    ax_udprtt.set_ylabel("UDP RTT p99 (ms, log)")
    ax_udprtt.set_title("UDP session RTT p99 per cell", fontsize=10)

    def udp_loss(t, cell):
        return read(results, t, cell, "udp_loss_pct")
    bar_group(ax_udploss, udp_tools, colors, compare_cells, udp_loss)
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
    panels below when the comparison has any. `tools` share the mux control
    (one variable only). A weak cell is only plotted when at least two of the
    tools actually run it: mux-off is loopback-only by design, so the mux
    chart is a single loopback row instead of half-empty weak panels."""
    cells = stable_cells(meta)
    loopback = "loopback" if "loopback" in cells else cells[0]
    colors = tool_colors(tools)
    weak_cells = [c for c in cells if c != loopback
                  and sum(1 for t in tools
                          if read(results, t, c,
                                  "throughput_1stream_gbps") is not None)
                  >= 2]

    if weak_cells:
        fig, axes = plt.subplots(2, 3, figsize=(15, 8.5))
        fig.subplots_adjust(left=0.06, right=0.98, bottom=0.15, top=0.90,
                            wspace=0.3, hspace=0.45)
        loopback_panels(axes[0], tools, colors, results, loopback)
        weak = axes[1]
    else:
        fig, axes = plt.subplots(1, 3, figsize=(15, 4.8))
        fig.subplots_adjust(left=0.06, right=0.98, bottom=0.24, top=0.80,
                            wspace=0.3)
        loopback_panels(axes, tools, colors, results, loopback)
        weak = []

    if weak_cells:
        (ax_cthr, ax_crtt, ax_udp) = weak

        def thr1(t, cell):
            return read(results, t, cell, "throughput_1stream_gbps")
        bar_group(ax_cthr, tools, colors, weak_cells, thr1, log=True,
                  fmt=lambda v: f"{v:.2f}" if v < 10 else f"{v:.0f}")
        ax_cthr.set_ylabel("1-stream TCP (Gbit/s, log)")
        ax_cthr.set_title("Throughput per network cell", fontsize=10)

        def rtt50(t, cell):
            return (read(results, t, cell, "echo_rtt_ms") or {}).get("p50")
        bar_group(ax_crtt, tools, colors, weak_cells, rtt50, log=True,
                  floor=1e-1)
        ax_crtt.set_ylabel("echo RTT p50 (ms, log)")
        ax_crtt.set_title("Connection-path latency per cell", fontsize=10)

        def udp_rtt99(t, cell):
            return (read(results, t, cell, "udp_rtt_ms") or {}).get("p99")
        bar_group(ax_udp, tools, colors, weak_cells, udp_rtt99, log=True,
                  floor=1e-1)
        ax_udp.set_ylabel("UDP RTT p99 (ms, log)")
        ax_udp.set_title("UDP session RTT p99 per cell", fontsize=10)

    footer(meta, "reproduce: just bench && just bench-plot")
    fig.suptitle(suptitle, fontsize=12)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_path, dpi=150)
    plt.close(fig)
    print(f"wrote {out_path}")


def render_mux(results, meta, out_path):
    """Multiplexing cost: mux vs mux-off. ONE variable (multiplexing on/off).
    The mux-off arm runs the loopback cell only (bench.py appends the
    variant just to loopback): the clean path shows the architecture cost
    (single-stream mux overhead, 8-stream ceiling parity), while the
    weak-cell multiplexing behavior is the count axis's story (count=4 vs
    count=1 share one loss/retransmit domain per tunnel)."""
    mux = mux_row(results)
    off = [t for t in molehill_family(results) if "(mux-off)" in t]
    render_dimension(results, meta, out_path, [mux, *off],
                     "Multiplexing cost — mux vs mux-off, one variable "
                     f"({meta.get('date', '')})")


def render_transport(results, meta, out_path):
    """Encryption cost: mux (plain) vs noise. ONE variable — the transport;
    multiplexing on and count = 4 for both. Loopback panels plus the
    weak-cell behavior."""
    mux = mux_row(results)
    tools = [mux] + [t for t in molehill_family(results) if "(noise)" in t]
    render_dimension(results, meta, out_path, tools,
                     "Encryption cost — plain vs noise, one variable "
                     f"({meta.get('date', '')})")


def render_cost(results, meta, out_path):
    """The new tradeoff metrics (0.8): CPU% (noise/KCP cost), connection
    churn (mux-vs-direct and pool guidance), sustained UDP capacity,
    64-stream scale and the mixed bulk+interactive workload. All are
    loopback panels: they answer "which configuration do I pick", the
    network cells answer "how does it behave when the path is bad"."""
    arms = [t for t in molehill_family(results)
            if (results[t].get("loopback") or {}).get("cpu") is not None]
    if not arms:
        print("skipping cost chart: no CPU/churn data in results")
        return
    colors = tool_colors(arms)
    cells = stable_cells(meta)
    loopback = "loopback" if "loopback" in cells else cells[0]

    fig, axes = plt.subplots(2, 3, figsize=(16, 8.5))
    fig.subplots_adjust(left=0.06, right=0.98, bottom=0.15, top=0.90,
                        wspace=0.32, hspace=0.5)
    width = 0.5

    def bars(ax, key, *path, fmt=lambda v: f"{v:.1f}", ylabel="",
             title="", ylim_factor=1.25):
        # only arms that actually have this metric are drawn: an arm that
        # cannot run the probe (mux1 is over the yamux ceiling for the
        # 64-stream scale point) is not a bar — an empty slot would read as
        # "not comparable" clutter, and a 0.0 bar would read as a measurement
        vals = [read(results, t, loopback, key, *path) for t in arms]
        present = [t for t, v in zip(arms, vals) if v is not None]
        drawn = [v for v in vals if v is not None]
        ax.bar(range(len(present)), drawn, width,
               color=[colors[t] for t in present])
        for x, v in enumerate(drawn):
            ax.annotate(fmt(v), (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 2),
                        textcoords="offset points")
        ax.set_xticks(range(len(present)),
                      [short(t) for t in present], fontsize=8)
        ax.set_ylabel(ylabel)
        if drawn:
            ax.set_ylim(0, max(drawn) * ylim_factor)
        ax.set_title(title, fontsize=10)
        ax.grid(axis="y", alpha=0.3)

    bars(axes[0][0], "cpu", "total_avg_pct", ylabel="% of one core",
         title="CPU (server+client avg) — noise/KCP tradeoff", fmt=lambda v: f"{v:.0f}")
    bars(axes[0][1], "churn", "connects_per_s", ylabel="connects/s",
         title="Connection churn (64 concurrent short connectors)",
         fmt=lambda v: f"{v:.0f}")
    bars(axes[0][2], "churn", "setup_first_byte_ms_p99", ylabel="ms",
         title="Churn setup-to-first-byte p99", fmt=lambda v: f"{v:.2f}")

    udp_arms = [t for t in arms
                if read(results, t, loopback, "udp_capacity") is not None]
    if udp_arms:
        uc = [read(results, t, loopback, "udp_capacity", "pps") or 0
              for t in udp_arms]
        ax = axes[1][0]
        ax.bar(list(range(len(udp_arms))), uc, width,
               color=[colors[t] for t in udp_arms])
        for x, v in zip(range(len(udp_arms)), uc):
            ax.annotate(f"{v:.0f}", (x, v), ha="center", va="bottom",
                        fontsize=6, rotation=90, xytext=(0, 2),
                        textcoords="offset points")
        ax.set_xticks(list(range(len(udp_arms))),
                      [short(t) for t in udp_arms], fontsize=8)
        ax.set_ylabel("datagrams/s")
        ax.set_ylim(0, max(uc) * 1.2 if uc else 1)
        ax.set_title("Sustained UDP capacity (paced, loss in table)",
                     fontsize=10)
        ax.grid(axis="y", alpha=0.3)
    else:
        axes[1][0].set_visible(False)

    bars(axes[1][1], "throughput_64streams_gbps", ylabel="Gbit/s",
         title="Scale: 64 concurrent streams (loopback)",
         fmt=fmt_auto)
    bars(axes[1][2], "mixed_bulk_latency", "bulk_gbps", ylabel="Gbit/s",
         title="Mixed workload: bulk transfer with interactive latency "
               "on the same client", fmt=fmt_auto)

    footer(meta, "reproduce: just bench && just bench-plot")
    fig.suptitle(f"Configuration tradeoffs — {meta.get('date', '')}",
                 fontsize=12)
    out_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out_path, dpi=150)
    plt.close(fig)
    print(f"wrote {out_path}")


def render_count(results, meta, out_path):
    """Tunnel count: mux (count = 4, the default) vs mux1 (count = 1). ONE
    variable — the number of parallel tunnel connections; plain transport,
    multiplexing on for both."""
    mux = mux_row(results)
    one = [t for t in molehill_family(results) if "(mux1)" in t]
    if not one:
        print("skipping count chart: no mux1 arm in results")
        return
    render_dimension(results, meta, out_path, [mux, *one],
                     "Tunnel count — count=4 vs count=1, one variable "
                     f"({meta.get('date', '')})")


CARRIER_ORDER = {"(noise)": 0, "(kcp4)": 1}


def render_carrier(results, meta, out_path):
    """Data-plane carrier: noise (TCP tunnels) vs kcp4 (KCP-over-UDP
    sessions). ONE variable — what carries the data channels; noise control
    channel, multiplexing on, count = 4 for both. The HoL and retransmit
    metrics are the panels that separate the two."""
    arms = [t for t in molehill_family(results)
            if any(v in t for v in CARRIER_ORDER)]
    if not arms:
        print("skipping carrier chart: no noise/kcp4 rows in results")
        return
    arms.sort(key=lambda t: CARRIER_ORDER.get(
        next(v for v in CARRIER_ORDER if v in t), 9))
    render_dimension(results, meta, out_path, arms,
                     "Data-plane carrier — TCP vs KCP, one variable "
                     f"({meta.get('date', '')})")
    print_tables_arms(results, meta, arms)


def footer(meta, reproduce):
    tv = meta.get("tool_versions", {})
    lines = [f"tools: {', '.join(f'{k} {v}' for k, v in tv.items())}",
             (f"cells: {', '.join(stable_cells(meta))}"
              f"  |  reps: {meta.get('reps')} / "
              f"{meta.get('secs_per_rep_loopback')}s "
              f"(weak: {meta.get('secs_per_rep_weak')}s)  |  "
              f"host: {meta.get('hostname')}"),
             reproduce]
    plt.gcf().text(0.05, 0.015, "\n".join(lines), fontsize=7,
                   family="monospace")


# --- markdown tables --------------------------------------------------------
def print_tables(results, meta):
    cells = stable_cells(meta)
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

    print("\n### Encryption cost (plain vs noise)")
    print("| Tool | thr 1-str | thr 8-str | RTT p50 | RTT p99 | "
          "steady p99 | RSS |",
          "|---|---|---|---|---|---|---|", sep="\n")
    for t in [mux_row(results)] + \
            [t for t in molehill_family(results) if "(noise)" in t]:
        print(family_row(t))

    def tradeoff_row(t):
        lb = results[t].get(loopback, {})
        cpu = lb.get("cpu") or {}
        ch = lb.get("churn") or {}
        uc = lb.get("udp_capacity") or {}
        mx = lb.get("mixed_bulk_latency") or {}
        return (f"| {t} | {fmt_table(cpu.get('total_avg_pct'), 1)} | "
                f"{fmt_table(ch.get('connects_per_s'), 1)} | "
                f"{fmt_table(ch.get('setup_first_byte_ms_p99'), 2)} | "
                f"{fmt_table(uc.get('pps'), 1)} | "
                f"{fmt_table(lb.get('throughput_64streams_gbps'), 2)} | "
                f"{fmt_table(mx.get('bulk_gbps'), 2)} |")

    if any((results[t].get(loopback) or {}).get("cpu") is not None
           for t in [mux_row(results), *molehill_family(results)]):
        print("\n### Configuration tradeoffs (loopback)")
        print("| Tool | CPU% | churn/s | churn p99 ms | UDP pps | "
              "thr64 | mixed bulk |",
              "|---|---|---|---|---|---|---|", sep="\n")
        seen = []
        for t in [mux_row(results), *molehill_family(results)]:
            if t in seen:
                continue
            seen.append(t)
            if (results[t].get(loopback) or {}).get("cpu") is not None:
                print(tradeoff_row(t))

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


def print_tables_arms(results, meta, arms):
    """Per-cell tables for the 3-arm data-path comparison. The HoL-ping
    stall and the TCP retransmit count are the discriminating rows."""
    cells = stable_cells(meta)
    loopback = "loopback" if "loopback" in cells else cells[0]

    def row(t, cell):
        d = results[t].get(cell, {})
        if not d:
            return None
        echo = d.get("echo_rtt_ms") or {}
        hol = d.get("hol") or {}
        hol_rtt = (hol.get("ping_rtt_ms") or {}).get("p50")
        mem_kb = (d.get("memory_rss_kb") or {}).get("total_avg_kb")
        steady = d.get("tcp_steady_rtt_ms") or {}
        return (f"| {t.split('(')[1].rstrip(')')} | "
                f"{fmt_table(d.get('throughput_1stream_gbps'), 3)} | "
                f"{fmt_table(d.get('throughput_8streams_gbps'), 3)} | "
                f"{fmt_table(echo.get('p50'), 3)} | "
                f"{fmt_table(steady.get('p99'), 3)} | "
                f"{d.get('retransmits_1stream') or '-'} | "
                f"{fmt_table(d.get('udp_loss_pct'), 2)}% | "
                f"{fmt_table(hol_rtt, 1)} | "
                f"{fmt_table(hol.get('ping_max_gap_ms'), 1)} | "
                f"{fmt_table((mem_kb / 1024) if mem_kb else None)} |")

    print("\n### Data-path transport arms (noise on all three, mux on)")
    print(f"Cell `{loopback}`:",
          "| Arm | thr 1-str | thr 8-str | RTT p50 | steady p99 | "
          "retr | UDP loss | HoL p50 | HoL max | RSS MiB |",
          "|---|---|---|---|---|---|---|---|---|---|", sep="\n")
    for t in arms:
        if row(t, loopback):
            print(row(t, loopback))

    def tradeoff_row(t):
        lb = results[t].get(loopback, {})
        cpu = lb.get("cpu") or {}
        ch = lb.get("churn") or {}
        uc = lb.get("udp_capacity") or {}
        mx = lb.get("mixed_bulk_latency") or {}
        return (f"| {t} | {fmt_table(cpu.get('total_avg_pct'), 1)} | "
                f"{fmt_table(ch.get('connects_per_s'), 1)} | "
                f"{fmt_table(ch.get('setup_first_byte_ms_p99'), 2)} | "
                f"{fmt_table(uc.get('pps'), 1)} | "
                f"{fmt_table(lb.get('throughput_64streams_gbps'), 2)} | "
                f"{fmt_table(mx.get('bulk_gbps'), 2)} |")

    if any((results[t].get(loopback) or {}).get("cpu") is not None
           for t in [mux_row(results), *molehill_family(results)]):
        print("\n### Configuration tradeoffs (loopback)")
        print("| Tool | CPU% | churn/s | churn p99 ms | UDP pps | "
              "thr64 | mixed bulk |",
              "|---|---|---|---|---|---|---|", sep="\n")
        seen = []
        for t in [mux_row(results), *molehill_family(results)]:
            if t in seen:
                continue
            seen.append(t)
            if (results[t].get(loopback) or {}).get("cpu") is not None:
                print(tradeoff_row(t))

    for cell in cells:
        if cell == loopback:
            continue
        print(f"\nCell `{cell}`:",
              "| Arm | thr 1-str | thr 8-str | RTT p50 | steady p99 | "
              "retr | UDP loss | HoL p50 | HoL max | RSS MiB |",
              "|---|---|---|---|---|---|---|---|---|---|", sep="\n")
        for t in arms:
            if row(t, cell):
                print(row(t, cell))


def fmt_auto(v):
    """Adaptive decimals: rate cells produce 0.00x Gbps values that a fixed
    one-decimal format would round into '0.0'; 3 significant digits below 1
    keeps 0.65, 0.04 and 0.0004 all truthful."""
    if not isinstance(v, (int, float)):
        return "-"
    if abs(v) < 1:
        return f"{v:.3g}"
    return f"{v:.1f}"


def stable_cells(meta):
    """Cells in the canonical order (shared with the runner via bench_lib)
    — the merged results file keeps an arbitrary meta order."""
    cells = [c["name"] for c in meta.get("cells", [])] or ["loopback"]
    by_name = {c["name"]: c for c in meta.get("cells", [])}
    return sorted(cells,
                  key=lambda n: cell_sort_key(by_name.get(n, {"name": n})))


def print_count_table(results, meta):
    """Compact per-cell table for the tunnel-count section, in the exact
    README format — emitted mechanically so the README can never drift from
    the raw results again."""
    cells = stable_cells(meta)
    mux = mux_row(results)
    one = [t for t in molehill_family(results) if "(mux1)" in t]
    if not one:
        return
    one = one[0]

    def hol_max(t, cell):
        return (results.get(t, {}).get(cell, {}).get("hol") or {}) \
            .get("ping_max_gap_ms")

    def cell_row(t, cell, key):
        return fmt_auto(results.get(t, {}).get(cell, {}).get(key))

    print("\n### Tunnel count (count = 4 vs count = 1)")
    print("| Cell | c4 1-str | c1 1-str | c4 8-str | c1 8-str | "
          "c4 HoL max | c1 HoL max |",
          "|---|---|---|---|---|---|---|", sep="\n")
    for cell in cells:
        print(f"| {cell} | {cell_row(mux, cell, 'throughput_1stream_gbps')} | "
              f"{cell_row(one, cell, 'throughput_1stream_gbps')} | "
              f"{cell_row(mux, cell, 'throughput_8streams_gbps')} | "
              f"{cell_row(one, cell, 'throughput_8streams_gbps')} | "
              f"{fmt_auto(hol_max(mux, cell))} | "
              f"{fmt_auto(hol_max(one, cell))} |")


def print_carrier_table(results, meta, arms):
    """Compact per-cell table for the data-plane-carrier section, in the
    exact README format (tcp = noise control channel, kcp = kcp4)."""
    cells = stable_cells(meta)
    by_col = {}
    for t in arms:
        if "(noise)" in t:
            by_col["tcp"] = t
        elif "(kcp4)" in t:
            by_col["kcp"] = t

    def get(col, cell, key):
        t = by_col.get(col)
        if not t:
            return "-"
        return fmt_auto(results.get(t, {}).get(cell, {}).get(key))

    def hol(col, cell):
        t = by_col.get(col)
        if not t:
            return "-"
        return fmt_auto((results.get(t, {}).get(cell, {}).get("hol") or {})
                        .get("ping_max_gap_ms"))

    def rss(col, cell):
        t = by_col.get(col)
        if not t:
            return "-"
        kb = (results.get(t, {}).get(cell, {}).get("memory_rss_kb") or {}) \
            .get("total_avg_kb")
        return fmt_auto(kb / 1024 if kb else None)

    print("\n### Data-plane carrier (tcp vs kcp)")
    print("| Cell | tcp 1-str | kcp 1-str | tcp 8-str | kcp 8-str | "
          "tcp HoL max | kcp HoL max | tcp RSS | kcp RSS |",
          "|---|---|---|---|---|---|---|---|---|", sep="\n")
    for cell in cells:
        print(f"| {cell} | {get('tcp', cell, 'throughput_1stream_gbps')} | "
              f"{get('kcp', cell, 'throughput_1stream_gbps')} | "
              f"{get('tcp', cell, 'throughput_8streams_gbps')} | "
              f"{get('kcp', cell, 'throughput_8streams_gbps')} | "
              f"{hol('tcp', cell)} | {hol('kcp', cell)} | "
              f"{rss('tcp', cell)} | {rss('kcp', cell)} |")


def main():
    path = pick_results()
    data = json.loads(path.read_text())
    meta, results = data["meta"], data["results"]
    ver = path.stem.removeprefix("results-")
    assets = script_dir.parents[2] / "assets"
    family = molehill_family(results)
    has_mux = any("(mux)" in t for t in family)
    if has_mux:
        render_main(results, meta, assets / f"benchmark-{ver}.png")
        render_mux(results, meta, assets / f"benchmark-mux-{ver}.png")
        if any("(noise)" in t for t in family):
            render_transport(results, meta,
                             assets / f"benchmark-transport-{ver}.png")
        if any("(mux1)" in t for t in family):
            render_count(results, meta,
                         assets / f"benchmark-count-{ver}.png")
            print_count_table(results, meta)
        render_cost(results, meta, assets / f"benchmark-cost-{ver}.png")
        print_tables(results, meta)
    render_carrier(results, meta, assets / f"benchmark-carrier-{ver}.png")
    print_carrier_table(results, meta, [t for t in molehill_family(results)
                                        if "(noise)" in t or "(kcp4)" in t])


if __name__ == "__main__":
    main()
