#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.5"]
# ///
"""Render the Soak charts and tables from a results file.

The chart set is the model's output surface. Each figure answers one
question, states its method in the footer, and is readable without the raw
file:

- `assets/soak-<ver>.png`           the master: per tool, the interactive
  stream's RTT over time (log axis, so the shaped stages and the recovery
  stage are on one scale) with its per-stage p50/p99 and the bulk throughput
  beneath it, over the shared stage schedule
- `assets/soak-<ver>-stages.png`    the same data as small multiples: one
  panel per stage, one lollipop per tool (p50 dot, p99 bar) — the comparison
  the scatter cannot give
- `assets/soak-<ver>-capacity.png`  response time against offered load with
  the SLO line: sustainable load is where a curve crosses it
- `assets/soak-<ver>-udp.png`       the UDP session's RTT and its sliding
  loss rate over time
- `assets/soak-<ver>-drift.png`     the soak's drift axis (handles, RSS, CPU)
  with every fitted slope printed: a leak is a slope, not a level
- `assets/soak-<ver>-cost.png`      CPU-seconds per carried Gbit per stage

Design rules the charts follow (they came from reading the first release's
charts, which were cluttered):

1. **A metric without contrast is not a plot.** A panel whose series is
   constant, or which no test produced, is dropped — not drawn empty.
2. **One scale per axis, decided once.** Log for a response time (a 7000 ms
   outlier must not flatten the 5 ms detail), linear for a rate.
3. **A tool keeps its colour everywhere**, so a reader can follow one line
   from panel to panel without a legend.
4. **Nothing is averaged away silently.** Stage medians are overlaid on the
   scatter, wedges are drawn, and every figure carries its method constants
   and revision in the footer.
5. **Shading means something**: the clean stages (the control) are green,
   the shaped stages grey.

Peers are plotted beside molehill in every panel: the workload is identical,
so the comparison is the point.
"""

import json
import sys
from dataclasses import dataclass
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D
from matplotlib.patches import Patch

sys.path.insert(0, str(Path(__file__).parent))
import lib

DPI = 130
THROUGHPUT_COLOR = "#b26a00"
SLO_COLOR = "#c62828"
CLEAN_FACE = "#e6f2e6"  # the unshaped control stages
SHAPED_FACE = "#ececf2"  # the degraded stages
SHAPED_FACE_ALT = "#e2e2ec"  # ... alternating, so two neighbours divide
TOOL_COLORS = [
    "#1565c0",
    "#2e7d32",
    "#6a1b9a",
    "#ef6c00",
    "#00838f",
    "#ad1457",
    "#4e342e",
    "#37474f",
]
FONT = 8.0
# Where the "no samples in this stage" marker sits on the log RTT axis.
NO_DATA_X = 3.0
# UDP figure: RTT panel, plus a loss panel when a loss series exists.
UDP_PANELS = 2


@dataclass(frozen=True)
class DriftPanel:
    """One drift metric and how to present it.

    A named bundle because the panel needs four things that only make sense
    together (the server metric, the client metric, the axis label and the
    divisor that turns a raw unit into the labelled one).
    """

    label: str
    server: str
    client: str
    scale: float


DRIFT_PANELS = [
    DriftPanel("open fds", "server_fds", "client_fds", 1.0),
    DriftPanel("RSS MiB", "server_rss_kb", "client_rss_kb", 1024.0),
    DriftPanel("CPU %", "server_cpu_pct", "client_cpu_pct", 1.0),
]


def apply_style() -> None:
    """One house style for every figure (readability over decoration)."""
    plt.rcParams.update(
        {
            "font.size": FONT,
            "axes.titlesize": FONT + 1,
            "axes.labelsize": FONT,
            "axes.edgecolor": "0.55",
            "axes.labelcolor": "0.15",
            "axes.grid": True,
            "axes.axisbelow": True,
            "grid.color": "0.0",
            "grid.alpha": 0.07,
            "grid.linewidth": 0.6,
            "legend.frameon": False,
            "legend.fontsize": FONT - 0.5,
            "xtick.labelsize": FONT - 0.5,
            "ytick.labelsize": FONT - 0.5,
            "xtick.color": "0.25",
            "ytick.color": "0.25",
            "figure.facecolor": "white",
            "savefig.facecolor": "white",
        }
    )


# The master figure's encoding legend (built once: every panel repeats the
# same encoding, and a per-panel legend covered the data it explained).
MASTER_LEGEND = [
    Line2D(
        [],
        [],
        marker=".",
        ls="",
        ms=4,
        color="0.35",
        label="interactive sample (one fresh connection per ping)",
    ),
    Line2D([], [], lw=1.4, color="0.35", label="stage p50"),
    Line2D([], [], lw=1.0, ls="--", color="0.35", label="stage p99"),
    Line2D([], [], color=SLO_COLOR, ls=":", lw=1.1, label="SLO (interactive p99)"),
    Line2D(
        [], [], color=THROUGHPUT_COLOR, lw=1.2, label="bulk throughput (right axis)"
    ),
    Patch(color=CLEAN_FACE, label="clean stage (unshaped)"),
    Patch(color=SHAPED_FACE, label="shaped stage"),
    Line2D([], [], color=SLO_COLOR, lw=3, label="no samples (wedge)"),
]


def tool_colors(tests: list) -> dict:
    """A stable colour per tool label, reused by every figure."""
    return {t["tool"]: TOOL_COLORS[i % len(TOOL_COLORS)] for i, t in enumerate(tests)}


def load(path: Path) -> dict:
    return json.loads(Path(path).read_text())


def series(test: dict, metric: str) -> list:
    """`(seconds since the test's first sample, value)` for one metric."""
    pts = lib.points(test.get("series", []), metric)
    if not pts:
        return []
    t0 = test["series"][0]["t"]
    return [(t - t0, v) for t, v in pts]


def stage_windows(test: dict) -> list:
    """`(absolute t_start, absolute t_end, stage)` per stage.

    Absolute (epoch) times: the series carries absolute timestamps, so every
    window that has to *select* samples must stay in that base. The relative
    view used for plotting is `stage_spans`, which subtracts the first
    sample's time — mixing the two silently produced empty stage windows and
    therefore charts with no per-stage statistics on them.
    """
    last = test["series"][-1]["t"] if test["series"] else 0.0
    starts = [s["t_start"] for s in test.get("stages", [])]
    windows = []
    for i, stage in enumerate(test.get("stages", [])):
        end = starts[i + 1] if i + 1 < len(starts) else last
        windows.append((stage["t_start"], end, stage["stage"]))
    return windows


def stage_spans(test: dict) -> list:
    """`(t_start, t_end, stage)` in seconds since the first sample."""
    base = test["series"][0]["t"] if test["series"] else 0.0
    return [(t0 - base, t1 - base, name) for t0, t1, name in stage_windows(test)]


def shade_stages(ax, spans: list, labels: bool = True) -> None:
    """Shade each stage's span and label it above the axes.

    Labels sit above the axes (in the panel gap) rather than inside: a label
    drawn at the top of the data area collides with the data it describes.
    """
    for i, (t0, t1, name) in enumerate(spans):
        clean = name == "clean"
        face = CLEAN_FACE if clean else (SHAPED_FACE if i % 2 == 0 else SHAPED_FACE_ALT)
        ax.axvspan(t0, t1, color=face, zorder=0, lw=0)
        if labels and t1 > t0:
            ax.text(
                (t0 + t1) / 2,
                1.015,
                name,
                ha="center",
                va="bottom",
                fontsize=FONT - 1,
                color="0.35",
                transform=ax.get_xaxis_transform(),
                clip_on=False,
            )


def stage_rows(test: dict, metric: str) -> list:
    """`(stage, (relative t0, t1), stats)` per stage, in schedule order.

    The statistics come from the absolute window (`stage_windows`); the span
    returned is the relative one, so a caller can draw without re-deriving
    times.
    """
    base = test["series"][0]["t"] if test["series"] else 0.0
    rows = []
    for t0, t1, name in stage_windows(test):
        window = [r for r in test["series"] if t0 <= r["t"] <= t1]
        rows.append((name, (t0 - base, t1 - base), lib.series_stats(window, metric)))
    return rows


def stage_wedges(tests: list) -> dict:
    """`stage -> number of tools that recorded a wedge in it`."""
    counts: dict = {}
    for test in tests:
        for stage in test.get("stages", []):
            if stage.get("flat_segments"):
                name = stage["stage"]
                counts[name] = counts.get(name, 0) + 1
    return counts


def step_curve(rows: list, key: str) -> tuple:
    """A step curve of one per-stage statistic, drawn across its window."""
    xs, ys = [], []
    for _name, (t0, t1), stats in rows:
        if stats.get(key) is None:
            continue
        xs += [t0, t1]
        ys += [stats[key], stats[key]]
    return xs, ys


def footer_reserve(fig) -> float:
    """The fraction of the figure the two footer lines occupy.

    The lines are placed a fixed number of inches from the bottom, so a short
    figure (a single UDP panel) does not print its footer over the axis label.
    """
    return min(0.3, 0.46 / fig.get_figheight())


def footer(fig, meta: dict, tests: list, extra: str = "") -> None:
    """The method record under every figure: what was measured, and on what."""
    slo = (meta.get("slo") or {}).get("rtt_p99_ms")
    parts = [
        f"host {meta.get('hostname', '?')}",
        f"kernel {meta.get('kernel', '?')}",
        f"{meta.get('batch', '?')} of {meta.get('nproc', '?')} cores",
        f"SLO interactive p99 <= {slo} ms",
        f"revision {meta.get('revision', 'unrecorded')}",
        f"date {meta.get('date', '?')}",
    ]
    if extra:
        parts.append(extra)
    tools = ", ".join(f"{t['tool']} {t.get('version', '?')}" for t in tests)
    height = fig.get_figheight()
    fig.text(
        0.5,
        0.28 / height,
        "  |  ".join(parts),
        ha="center",
        va="bottom",
        fontsize=FONT - 1.5,
        color="0.4",
    )
    fig.text(
        0.5,
        0.04 / height,
        f"measured: {tools}",
        ha="center",
        va="bottom",
        fontsize=FONT - 1.5,
        color="0.4",
    )


def save(fig, out: Path, bottom: float = 0.07) -> None:
    fig.savefig(out, dpi=DPI, bbox_inches="tight", pad_inches=0.28, facecolor="white")
    plt.close(fig)
    print(f"wrote {out}")


def draw_wedges(ax, test: dict) -> None:
    """Mark the silent segments a stage recorded, as a bar on the bottom edge.

    A full-height wash would hide the very data the wedge explains; a thin
    bar in a reserved strip states "no samples here" without competing with
    the samples that did land.
    """
    base = test["series"][0]["t"] if test["series"] else 0.0
    for seg in test.get("metrics", {}).get("flat_segments") or []:
        t0, t1 = seg["start"] - base, seg["end"] - base
        ax.axvspan(
            t0, t1, ymin=0.0, ymax=0.04, color=SLO_COLOR, alpha=0.85, lw=0, zorder=5
        )
    wedges = test.get("metrics", {}).get("flat_segments") or []
    if wedges:
        # One summary instead of a label per bar: a stage schedule with ten
        # short wedges produced ten overlapping labels and hid the panel.
        total = sum(w["duration_s"] for w in wedges)
        longest = max(w["duration_s"] for w in wedges)
        ax.annotate(
            f"silent {total:.0f}s in {len(wedges)} segment(s), longest {longest:.0f}s",
            xy=(0.995, 0.05),
            xycoords="axes fraction",
            ha="right",
            va="bottom",
            fontsize=FONT - 2,
            color=SLO_COLOR,
        )


# --- the master chart -------------------------------------------------------
def render_master(tests: list, meta: dict, out: Path) -> None:
    """Per tool: the interactive RTT over time, with the bulk behind it."""
    if not tests:
        return
    colors = tool_colors(tests)
    fig, axes = plt.subplots(
        len(tests), 1, figsize=(12.5, 2.9 * len(tests)), sharex=True, squeeze=False
    )
    for row, test in zip(axes, tests):
        color = colors[test["tool"]]
        ax = row[0]
        spans = stage_spans(test)
        shade_stages(ax, spans)
        rows = stage_rows(test, "rtt_interactive_ms")
        rtt = series(test, "rtt_interactive_ms")
        if rtt:
            ax.plot(
                [p[0] for p in rtt],
                [p[1] for p in rtt],
                ".",
                ms=1.6,
                alpha=0.22,
                color=color,
                zorder=2,
                label="interactive sample (a fresh connection per ping)",
            )
            x, y = step_curve(rows, "p50")
            ax.plot(x, y, lw=1.4, color=color, zorder=4, label="stage p50")
            x, y = step_curve(rows, "p99")
            ax.plot(x, y, lw=1.0, ls="--", color=color, zorder=4, label="stage p99")
        ax.axhline(
            meta.get("slo", {}).get("rtt_p99_ms", 50.0),
            color=SLO_COLOR,
            ls=":",
            lw=1.1,
            zorder=3,
            label="SLO (interactive p99)",
        )
        draw_wedges(ax, test)
        ax.set_yscale("log")
        ax.set_ylim(bottom=0.4)
        ax.set_ylabel(f"{test['tool']}\nRTT ms (log)", fontsize=FONT)
        ax.set_yticks([1, 10, 100, 1000])
        ax.set_yticklabels(["1", "10", "100", "1000"])
        ax.minorticks_off()
        ax2 = ax.twinx()
        bulk = series(test, "throughput_bulk_gbps")
        if bulk:
            ax2.plot(
                [p[0] for p in bulk],
                [p[1] for p in bulk],
                lw=0.9,
                color=THROUGHPUT_COLOR,
                alpha=0.9,
                zorder=2,
            )
        ax2.set_ylabel("bulk Gbit/s", color=THROUGHPUT_COLOR, fontsize=FONT)
        ax2.tick_params(axis="y", colors=THROUGHPUT_COLOR)
        ax2.set_ylim(bottom=0)
        ax2.grid(False)
    axes[-1][0].set_xlabel("seconds since the test started")
    # One encoding legend for the whole figure: repeated inside every panel it
    # covered the data it was explaining (and the tool is named on the axis).
    fig.legend(
        handles=MASTER_LEGEND,
        loc="upper center",
        ncol=4,
        bbox_to_anchor=(0.5, 0.965),
        fontsize=FONT - 0.5,
    )
    fig.suptitle(
        "Interactive stream through the stage schedule "
        "(log RTT; shaded bands are path classes)",
        y=1.0,
        fontsize=FONT + 3,
    )
    footer(fig, meta, tests)
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.955))
    save(fig, out)


# --- small multiples: one panel per stage ----------------------------------
def render_stages(tests: list, meta: dict, out: Path) -> None:
    """The comparison the time series cannot give: tool vs tool, per stage.

    One panel per stage, a lollipop per tool: the dot is the stage's p50, the
    bar reaches its p99, the faint tick is its worst single second. A reader
    sees which tool wins which condition without reading a table.
    """
    stages = []
    for test in tests:
        for name, _span, _stats in stage_rows(test, "rtt_interactive_ms"):
            if name not in stages:
                stages.append(name)
    if not stages:
        return
    colors = tool_colors(tests)
    fig, axes = plt.subplots(
        1,
        len(stages),
        figsize=(2.0 * len(stages) + 2.0, 0.5 * len(tests) + 3.4),
        sharey=True,
        squeeze=False,
    )
    slo = meta.get("slo", {}).get("rtt_p99_ms", 50.0)
    ypos = {t["tool"]: i for i, t in enumerate(tests)}
    wedges = stage_wedges(tests)
    for ax, stage in zip(axes[0], stages):
        ax.axvspan(0.05, slo, color=CLEAN_FACE, zorder=0, lw=0)
        ax.axvline(slo, color=SLO_COLOR, ls=":", lw=1.0, zorder=1)
        for test in tests:
            y = ypos[test["tool"]]
            stats = next(
                (
                    s
                    for n, _sp, s in stage_rows(test, "rtt_interactive_ms")
                    if n == stage
                ),
                None,
            )
            if not stats or stats.get("p50") is None:
                # "no samples" and "not measured" are different facts, and a
                # blank row would leave the reader guessing which one it is.
                ax.plot([NO_DATA_X], [y], "x", ms=4, color="0.6", zorder=3)
                continue
            color = colors[test["tool"]]
            p99 = stats.get("p99") or stats["p50"]
            ax.plot(
                [stats["p50"], max(p99, stats["p50"])],
                [y, y],
                lw=2.2,
                color=color,
                alpha=0.55,
                solid_capstyle="butt",
                zorder=2,
            )
            ax.plot([stats["p50"]], [y], "o", ms=4.5, color=color, zorder=3)
            if stats.get("max") is not None:
                ax.plot(
                    [stats["max"]], [y], "|", ms=6, color=color, alpha=0.6, zorder=3
                )
        ax.set_xscale("log")
        count = wedges.get(stage, 0)
        note = f"\n{count} wedge(s)" if count else ""
        ax.set_title(f"{stage}\n(p50 → p99){note}", fontsize=FONT)
        ax.set_xlim(left=0.4)
        ax.set_xticks([1, 10, 100, 1000, 10000])
        ax.set_xticklabels(["1", "10", "100", "1000", ""])
        ax.minorticks_off()
        ax.grid(axis="x", alpha=0.1)
    axes[0][0].set_yticks(list(ypos.values()))
    axes[0][0].set_yticklabels(list(ypos.keys()))
    axes[0][0].set_ylim(-0.6, len(tests) - 0.4)
    fig.suptitle(
        f"Interactive RTT per stage, ms (log) — p50 (dot), p99 (bar), worst "
        f"second (tick); green band is inside the SLO p99 = {slo} ms, "
        "x = no samples",
        y=0.98,
        fontsize=FONT + 3,
    )
    footer(fig, meta, tests)
    fig.tight_layout(rect=(0, footer_reserve(fig) + 0.04, 1, 0.93))
    save(fig, out)


# --- capacity ---------------------------------------------------------------
def render_capacity(tests: list, meta: dict, out: Path) -> None:
    """Response time against offered load, with the SLO crossing marked."""
    colors = tool_colors(tests)
    fig, ax = plt.subplots(figsize=(8.5, 5.0))
    slo = meta.get("slo", {}).get("rtt_p99_ms", 50.0)
    ax.axhspan(0.01, slo, color=CLEAN_FACE, zorder=0, lw=0)
    ax.axhline(slo, color=SLO_COLOR, ls=":", lw=1.1, label="SLO (p99)")
    for test in tests:
        curve = test.get("metrics", {}).get("curve") or []
        pts = [
            (c["streams"], c["rtt_p99"]) for c in curve if c.get("rtt_p99") is not None
        ]
        if not pts:
            continue
        color = colors[test["tool"]]
        ax.plot(
            [p[0] for p in pts],
            [p[1] for p in pts],
            "o-",
            ms=4,
            lw=1.2,
            color=color,
            label=test["tool"],
        )
        crossing = next((c for c in curve if c.get("slo_broken")), None)
        if crossing:
            ax.annotate(
                f"{test['tool']}: SLO broken at {crossing['streams']} streams",
                xy=(crossing["streams"], crossing["rtt_p99"] or slo),
                xytext=(6, 6),
                textcoords="offset points",
                fontsize=FONT - 1,
                color=color,
            )
            ax.axvline(crossing["streams"], color=color, ls=":", lw=0.9, alpha=0.7)
    ax.set_yscale("log")
    ax.set_ylim(bottom=0.01)
    ax.set_yticks([0.01, 0.1, 1, 10, 50, 100, 1000])
    ax.set_yticklabels(["0.01", "0.1", "1", "10", "50", "100", "1000"])
    ax.minorticks_off()
    ax.set_xlabel("bulk streams offered (load)")
    ax.set_ylabel("interactive RTT p99, ms (log)")
    ax.set_title("Capacity: the interactive stream's p99 against offered load")
    ax.legend(loc="upper left")
    footer(fig, meta, tests)
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 1))
    save(fig, out)


# --- UDP --------------------------------------------------------------------
def render_udp(tests: list, meta: dict, out: Path) -> None:
    """The UDP session: response time and the sliding loss rate."""
    colors = tool_colors(tests)
    loss = [(t, list(lib.loss_rate_series(t.get("series", [])))) for t in tests]
    panels = UDP_PANELS if any(rows for _t, rows in loss) else 1
    fig, axes = plt.subplots(
        panels, 1, figsize=(12.5, 2.6 * panels + 1.7), sharex=True, squeeze=False
    )
    for test in tests:
        color = colors[test["tool"]]
        rtt = series(test, "rtt_udp_ms")
        if rtt:
            axes[0][0].plot(
                [p[0] for p in rtt],
                [p[1] for p in rtt],
                ".",
                ms=1.6,
                alpha=0.3,
                color=color,
                label=test["tool"],
            )
    axes[0][0].axhline(
        meta.get("slo", {}).get("rtt_p99_ms", 50.0), color=SLO_COLOR, ls=":", lw=1.0
    )
    axes[0][0].set_yscale("log")
    axes[0][0].set_ylim(bottom=0.5)
    axes[0][0].set_yticks([1, 10, 100, 1000])
    axes[0][0].set_yticklabels(["1", "10", "100", "1000"])
    axes[0][0].minorticks_off()
    axes[0][0].set_ylabel("UDP RTT ms (log)")
    if panels == UDP_PANELS:  # the loss panel exists only with a loss series
        for test, (_t, rows) in zip(tests, loss):
            if not rows:
                continue
            axes[1][0].plot(
                [r["t"] - test["series"][0]["t"] for r in rows],
                [r["v"] for r in rows],
                lw=1.0,
                color=colors[test["tool"]],
            )
        axes[1][0].set_ylabel("loss % (sliding)")
        axes[1][0].set_ylim(bottom=0)
    if tests:
        for ax in axes[:, 0]:
            shade_stages(ax, stage_spans(tests[0]), labels=(ax is axes[0][0]))
    axes[-1][0].set_xlabel("seconds since the test started")
    # The legend gets its own strip under the title: above the axes it
    # collided with the stage labels, inside them with the data.
    fig.legend(
        handles=[
            Line2D(
                [],
                [],
                marker=".",
                ls="",
                ms=4,
                color=colors[t["tool"]],
                label=t["tool"],
            )
            for t in tests
        ],
        loc="upper center",
        ncol=max(1, len(tests)),
        bbox_to_anchor=(0.5, 1.0),
        fontsize=FONT,
    )
    fig.suptitle(
        "UDP session quality through the stage schedule", y=1.07, fontsize=FONT + 3
    )
    footer(fig, meta, tests, extra=f"loss over a {lib.LOSS_WINDOW_S:.0f} s window")
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.9))
    save(fig, out)


# --- drift ------------------------------------------------------------------
def _drift_panel(ax, tests: list, labels: "DriftPanel", colors: dict) -> None:
    """One drift metric: server solid, client dashed, slopes in the corner."""
    metric_1, metric_2, ylabel, scale = (
        labels.server,
        labels.client,
        labels.label,
        labels.scale,
    )
    lines = []
    for test in tests:
        color = colors[test["tool"]]
        for metric, style in zip((metric_1, metric_2), ("-", "--")):
            pts = series(test, metric)
            if not pts:
                continue
            ax.plot(
                [p[0] for p in pts],
                [v / scale for _, v in pts],
                style,
                lw=0.9,
                color=color,
                alpha=0.85,
            )
        slopes = [
            test.get("metrics", {}).get(f"{m}_slope_per_min")
            for m in (metric_1, metric_2)
        ]
        if any(s is not None for s in slopes):
            lines.append(
                f"{test['tool']}: "
                + " / ".join(
                    f"{s / scale:+.2f}" if s is not None else "—" for s in slopes
                )
            )
    if lines:
        ax.text(
            0.995,
            0.04,
            "\n".join(lines),
            transform=ax.transAxes,
            ha="right",
            va="bottom",
            fontsize=FONT - 2,
            color="0.3",
            family="monospace",
            bbox={"facecolor": "white", "alpha": 0.75, "lw": 0, "pad": 2.0},
        )
    ax.set_ylabel(ylabel)


def render_drift(tests: list, meta: dict, out: Path) -> None:
    """The soak's drift axis, with the fitted slope of every line printed."""
    colors = tool_colors(tests)
    fig, axes = plt.subplots(
        len(DRIFT_PANELS), 1, figsize=(12.5, 8.0), sharex=True, squeeze=False
    )
    for panel, ax in zip(DRIFT_PANELS, axes[:, 0]):
        _drift_panel(ax, tests, panel, colors)
    handles = [
        Line2D([], [], color=colors[t["tool"]], lw=1.4, label=t["tool"]) for t in tests
    ]
    handles += [
        Line2D([], [], color="0.3", lw=1.0, ls="-", label="server"),
        Line2D([], [], color="0.3", lw=1.0, ls="--", label="client"),
    ]
    axes[0][0].legend(handles=handles, loc="upper left", ncol=len(handles))
    axes[-1][0].set_xlabel("seconds since the test started")
    fig.suptitle(
        "Drift: handles, memory and CPU — the boxed numbers are the fitted "
        "slopes per minute (server / client); a leak is a slope, not a level",
        y=0.995,
        fontsize=FONT + 3,
    )
    footer(fig, meta, tests, extra="slopes fitted over the run except the first stage")
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.97))
    save(fig, out)


# --- cost -------------------------------------------------------------------
def render_cost(tests: list, meta: dict, out: Path) -> None:
    """CPU-seconds per carried Gbit, per stage and per tool."""
    colors = tool_colors(tests)
    stages = []
    for test in tests:
        for stage in test.get("stages", []):
            if (
                stage.get("cost_cpu_per_gbit") is not None
                and stage["stage"] not in stages
            ):
                stages.append(stage["stage"])
    if not stages:
        return
    fig, ax = plt.subplots(figsize=(1.4 * len(stages) + 3.0, 4.6))
    width = 0.8 / max(1, len(tests))
    for i, test in enumerate(tests):
        xs, ys = [], []
        for j, stage_name in enumerate(stages):
            stage = next(
                (s for s in test.get("stages", []) if s["stage"] == stage_name), None
            )
            if not stage or stage.get("cost_cpu_per_gbit") is None:
                continue
            xs.append(j + i * width - 0.4 + width / 2)
            ys.append(stage["cost_cpu_per_gbit"])
        ax.bar(
            xs, ys, width=width * 0.9, color=colors[test["tool"]], label=test["tool"]
        )
    ax.set_xticks(range(len(stages)))
    ax.set_xticklabels(stages)
    ax.set_ylabel("CPU-seconds per carried Gbit")
    ax.set_title("Cost at the operating point (lower is better)")
    ax.legend(loc="upper left")
    footer(fig, meta, tests)
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 1))
    save(fig, out)


# --- tables -----------------------------------------------------------------
def _fmt(value, unit: str = "") -> str:
    return "—" if value is None else f"{value}{unit}"


def tables(tests: list, meta: dict) -> None:
    """The markdown view of the same numbers, for the README and the notes."""
    slo = (meta.get("slo") or {}).get("rtt_p99_ms")
    print("\n## Soak results\n")
    print(
        f"host `{meta.get('hostname')}` | kernel {meta.get('kernel')} | "
        f"batch {meta.get('batch')} of {meta.get('nproc')} cores | "
        f"SLO interactive p99 <= {slo} ms | "
        f"revision {meta.get('revision', 'unrecorded')} | "
        f"{meta.get('date')}"
    )
    print("\n### Per tool\n")
    print(
        "| tool | version | test | sustainable streams | bulk Gbit/s | "
        "interactive p99 | worst 1 s | errors % | churn/s | churn p99 | "
        "UDP p99 | UDP loss % | RSS MiB | RSS slope/min | fds slope/min |"
    )
    print("|" + "---|" * 15)
    for t in tests:
        m = t.get("metrics", {})
        stages = t.get("stages", [])
        bulk = (m.get("bulk_throughput_stats") or {}).get("mean")
        rtt = (m.get("interactive_rtt_stats") or {}).get("p99")
        worst = (m.get("interactive_rtt_worst_1s") or {}).get("mean")
        udp = (m.get("udp_rtt_stats") or {}).get("p99")
        rss = lib.points(t.get("series", []), "server_rss_kb")
        rss_mib = round(sum(v for _, v in rss) / len(rss) / 1024) if rss else None
        churn = [s.get("churn_per_s") or 0 for s in stages]
        churn_p99 = lib.pct(
            [s["churn_p99"] for s in stages if s.get("churn_p99") is not None], 0.5
        )
        loss = lib.pct(
            [s["udp_loss_pct"] for s in stages if s.get("udp_loss_pct") is not None],
            0.5,
        )
        print(
            f"| {t['tool']} | {t.get('version')} | {t['test']} | "
            f"{_fmt(m.get('max_sustainable_streams'))} | "
            f"{_fmt(bulk)} | {_fmt(rtt, ' ms')} | {_fmt(worst, ' ms')} | "
            f"{100 * (m.get('interactive_error_rate') or 0):.2f} | "
            f"{round(sum(churn) / max(1, len(churn)))} | "
            f"{_fmt(churn_p99, ' ms')} | {_fmt(udp, ' ms')} | "
            f"{_fmt(loss, '%')} | {_fmt(rss_mib, ' MiB')} | "
            f"{_fmt(m.get('server_rss_kb_slope_per_min'), ' KiB/min')} | "
            f"{_fmt(m.get('server_fds_slope_per_min'), '/min')} |"
        )
    print_stage_table(tests, "rtt_p99", "interactive RTT p99 (ms)")
    print_stage_table(tests, "udp_p99", "UDP RTT p99 (ms)", note_wedges=False)
    print_wedges(tests)


def print_stage_table(
    tests: list, key: str, title: str, note_wedges: bool = True
) -> None:
    """The per-stage matrix: the shape of the run, not its average."""
    stages = []
    for t in tests:
        for s in t.get("stages", []):
            if s["stage"] not in stages:
                stages.append(s["stage"])
    if not stages:
        return
    print(f"\n### Per stage: {title}\n")
    print("| tool | " + " | ".join(stages) + " |")
    print("|" + "---|" * (len(stages) + 1))
    for t in tests:
        cells = []
        for stage in stages:
            s = next((x for x in t.get("stages", []) if x["stage"] == stage), None)
            if s is None:
                cells.append("—")
            elif note_wedges and s.get("flat_segments"):
                cells.append("wedge")
            elif s.get(key) is not None:
                cells.append(f"{s[key]}")
            else:
                cells.append("no data")
        print(f"| {t['tool']} | " + " | ".join(cells) + " |")


def print_wedges(tests: list) -> None:
    """Wedge durations, which a cell average cannot express."""
    for t in tests:
        for s in t.get("stages", []):
            if s.get("flat_segments"):
                longest = max(x["duration_s"] for x in s["flat_segments"])
                print(
                    f"\n**{t['tool']}** stage `{s['stage']}` wedged: "
                    f"{len(s['flat_segments'])} flat segment(s), longest "
                    f"{longest}s"
                )


def main() -> None:
    if len(sys.argv) > 1:
        path = Path(sys.argv[1])
    else:
        found = sorted(
            Path(__file__).parent.glob("results-soak-*.json"), key=lambda p: p.name
        )
        if not found:
            sys.exit("no results-soak-*.json found")
        path = found[-1]
    data = load(path)
    ver = path.stem.replace("results-soak-", "") or "dev"
    assets = Path(__file__).parents[3] / "assets"
    assets.mkdir(parents=True, exist_ok=True)
    tests = [t for t in data["tests"] if not t.get("error")]
    meta = data["meta"]
    apply_style()
    render_master(tests, meta, assets / f"soak-{ver}.png")
    if any(stage_rows(t, "rtt_interactive_ms") for t in tests):
        render_stages(tests, meta, assets / f"soak-{ver}-stages.png")
    capacity = [t for t in tests if t["test"] == "capacity"]
    if capacity:
        render_capacity(capacity, meta, assets / f"soak-{ver}-capacity.png")
    if any(series(t, "rtt_udp_ms") for t in tests):
        render_udp(tests, meta, assets / f"soak-{ver}-udp.png")
    if any("drift_from_t" in t.get("metrics", {}) for t in tests):
        render_drift(tests, meta, assets / f"soak-{ver}-drift.png")
    if any(
        s.get("cost_cpu_per_gbit") is not None
        for t in tests
        for s in t.get("stages", [])
    ):
        render_cost(tests, meta, assets / f"soak-{ver}-cost.png")
    tables(tests, meta)


if __name__ == "__main__":
    main()
