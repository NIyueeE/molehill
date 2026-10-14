#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.5"]
# ///
"""The bench model's output surface: the charts and tables of one results file.

Each figure answers one question the model asks, and a figure whose data does
not exist is not drawn at all — a `smoke` run has no stage schedule, so it has
no timeline:

- `bench-<fp>-timeline.png`  the master: per arm, the interactive stream's
  per-stage p50 (solid step) and p99 (dashed step) over the shaded stage
  bands, with the per-stage bulk spine beside it. It answers what a stage table
  cannot: where the tail bends, and whether it comes back when the path does.
- `bench-<fp>-stages.png`    the same reading as small multiples, one lollipop
  per arm per stage — the comparison a per-arm timeline cannot give.
- `bench-<fp>-capacity.png`  throughput and tail latency against the offered
  stream level, with the SLO line and each arm's last sustainable level: how
  much load the path carries while a fresh visitor still meets the SLO.
- `bench-<fp>-udp.png`       the datagram ladder: received rate and loss
  against the offered rate, so the rate where a path starts shedding shows.
- `bench-<fp>-drift.png`     the footprint over time with its fitted slope: a
  leak is a slope, not a level, and a high flat line is not a leak.
- `bench-<fp>-cost.png`      CPU-seconds per carried Gbit, the price of a bit.

Two rules bind every reader here:

* **The summary is the only source of numbers.** A median, a percentile or a
  range comes from `analysis`'s summary; this module never recomputes one, so
  a chart cannot disagree with the report rendered from the same file.
* **An absence is reported, not zeroed.** A cell that carried no reading is
  `- (reason)` in the tables and an `x` on the chart, carrying the instrument's
  own typed reason — a zero is a measurement and an absence is not.

Every figure states its method in the footer (profile, fingerprint, revision,
condition, date): a chart without its method is not publishable.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from dataclasses import dataclass
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import analysis
import matplotlib.pyplot as plt
import model
from matplotlib.lines import Line2D
from matplotlib.patches import Patch

#: One house style, decided once: readability over decoration.
DPI = 130
FONT = 8.0
FIGURE_WIDTH = 12.5
STAGE_WIDTH = 2.1
#: Half a stage's width on the shared stage axis: a stage is one slot, so its
#: step and its band span slot +/- this.
STAGE_HALF = 0.5
#: A log axis gets at most this many ticks: enough to read a range, few enough
#: that a nine-decade axis does not turn its labels into a smear.
MAX_LOG_TICKS = 12
CLEAN_FACE = "#e6f2e6"
DEGRADED_FACE = "#ececf2"
DEGRADED_FACE_ALT = "#e2e2ec"
SLO_COLOR = "#c62828"
REFERENCE_COLOR = "#b26a00"
#: A tool keeps its colour everywhere, so a reader can follow one line from
#: figure to figure without a legend.
ARM_COLORS = (
    "#1565c0",
    "#2e7d32",
    "#6a1b9a",
    "#ef6c00",
    "#00838f",
    "#ad1457",
    "#4e342e",
    "#37474f",
)


@dataclass(frozen=True)
class DriftPanel:
    """One drift series and how to present it.

    A named bundle because four things only make sense together: the evidence
    key, the divisor that turns it into the labelled unit, the label, and the
    summary metric that carries its fitted slope.
    """

    key: str
    scale: float
    label: str
    slope_metric: str


DRIFT_PANELS = (
    DriftPanel("rss_kib", 1.0 / 1024.0, "RSS MiB", "drift_rss_mib_per_min"),
    DriftPanel("fds", 1.0, "open fds", "drift_fds_per_min"),
    DriftPanel("threads", 1.0, "threads", "drift_threads_per_min"),
)

#: Which panel label and axis each capacity metric gets, in panel order.
CAPACITY_PANELS = (
    ("throughput_gbps", "bulk Gbit/s"),
    ("rtt_p99_ms", "interactive p99, ms (log)"),
)


@dataclass(frozen=True)
class Run:
    """A loaded results file, indexed the way every figure and table reads it.

    The wrapper exists so all readers share one definition of "this cell's
    median" and one of "this cell carried nothing": the summary is the source
    of numbers, the samples are the source of the typed reasons. A chart that
    went looking for either itself would need the analysis' rules a second
    time, and the two copies would drift.
    """

    results: dict
    meta: dict
    summary: dict

    @property
    def arms(self) -> list:
        """The arm ids, in the order the run declared them."""
        return [arm["id"] for arm in self.meta.get("arms") or ()]

    def spec(self, scenario: str) -> dict:
        """A scenario's declaration: the file's own, else the model's."""
        for spec in self.meta.get("scenarios") or ():
            if spec.get("id") == scenario:
                return spec
        known = model.SCENARIOS.get(scenario)
        if known is None:
            return {"id": scenario, "kind": "", "headline": "", "params": {}}
        return {
            "id": scenario,
            "kind": known.kind,
            "headline": known.headline,
            "params": known.params,
        }

    def kind(self, scenario: str) -> str:
        return self.spec(scenario).get("kind", "")

    def headline(self, scenario: str) -> str:
        return self.spec(scenario).get("headline", "")

    def scenario_ids(self) -> list:
        """Every scenario the summary measured, in the file's own order."""
        seen: list = []
        for cell in self.summary.get("cells") or ():
            if cell["scenario"] not in seen:
                seen.append(cell["scenario"])
        return seen

    def scenarios_of(self, kind: str) -> list:
        """Every measured scenario of one workload kind, in file order."""
        return [sid for sid in self.scenario_ids() if self.kind(sid) == kind]

    def cells(self, scenario: str) -> list:
        """One scenario's summary cells, in the order the run wrote them.

        That order is the schedule's: the runner appends a staged cell as it
        finishes the stage, so the chart reads the schedule it measured rather
        than a timeline constant that could drift away from it.
        """
        return [c for c in self.summary.get("cells") or () if c["scenario"] == scenario]

    def arm_entry(self, scenario: str, cell: str, arm: str) -> dict:
        """One arm's entry inside one summary cell (empty when it has none)."""
        for entry in self.cells(scenario):
            if entry["cell"] == cell:
                return (entry.get("arms") or {}).get(arm) or {}
        return {}

    def stat(self, scenario: str, cell: str, arm: str, metric: str):
        """The arm's summary statistic for one metric, or None."""
        return (self.arm_entry(scenario, cell, arm).get("metrics") or {}).get(metric)

    def median(self, scenario: str, cell: str, arm: str, metric: str):
        """The arm's median for one metric in one cell, or None."""
        stat = self.stat(scenario, cell, arm, metric)
        return stat.get("median") if stat else None

    def absence(self, scenario: str, cell: str, arm: str, metric: str) -> str:
        """The typed reason a reading is missing, or an empty string.

        Three places can hold it, in order of specificity: the metric's own
        `unavailable` record in a measured round, the cell's failure reason
        when no round was measured at all, and the sample's record when the
        summary has no entry for the arm.
        """
        entry = self.arm_entry(scenario, cell, arm)
        if entry.get("rounds"):
            reason = (entry.get("unavailable") or {}).get(metric)
            if reason:
                return str(reason)
        for failure in self.summary.get("failures") or ():
            key = (failure.get("arm"), failure.get("scenario"), failure.get("cell"))
            if key == (arm, scenario, cell):
                return str(failure.get("reason") or "no reading")
        return self._sample_absence(scenario, cell, arm, metric)

    def _sample_absence(self, scenario: str, cell: str, arm: str, metric: str) -> str:
        """The reason a sample recorded, for a cell the summary skipped."""
        for sample in self.results.get("samples") or ():
            key = (sample.get("arm"), sample.get("scenario"), sample.get("cell"))
            if key != (arm, scenario, cell):
                continue
            info = (sample.get("unavailable") or {}).get(metric)
            if info:
                return str(info if isinstance(info, str) else info.get("reason", ""))
        return ""

    def condition(self, scenario: str, cell: str) -> str:
        """The path condition a cell ran under, from its own evidence.

        A staged cell records the class it held; a cell that recorded none
        falls back to its label (`clean#2` is the second `clean`), because a
        band drawn from a guess would put a condition on the chart that the
        run never applied.
        """
        for sample in self.results.get("samples") or ():
            if sample.get("scenario") != scenario or sample.get("cell") != cell:
                continue
            found = (sample.get("evidence") or {}).get("condition")
            if found:
                return str(found)
        return cell.split("#", 1)[0]

    def color(self, arm: str) -> str:
        """The arm's colour, the same one in every figure."""
        arms = self.arms
        return ARM_COLORS[arms.index(arm) % len(ARM_COLORS)] if arm in arms else "0.3"


# --- reading the file -------------------------------------------------------
def load_run(path: Path) -> Run:
    """Read one results file through the analysis, or exit with the reason."""
    try:
        results = analysis.load(path)
    except (OSError, json.JSONDecodeError) as exc:
        sys.exit(f"cannot read {path}: {exc}")
    meta = results.get("meta") or {}
    if meta.get("model") != "bench":
        sys.exit(f"{path} is not a bench results file (meta.model is not 'bench')")
    return Run(results=results, meta=meta, summary=results.get("summary") or {})


def revision_text(meta: dict) -> str:
    """The revision the run recorded, as one string (a list is a pair)."""
    revision = (meta.get("provenance") or {}).get("revision", "")
    if isinstance(revision, (list, tuple)):
        revision = revision[0] if revision else ""
    return str(revision) or "unrecorded"


def condition_text(run: Run) -> str:
    """The condition the run applied, and the classes its stages walked.

    The method's own `shape` names the baseline; the staged and ramp cells name
    what they actually held, and printing both is what makes a chart's bands
    auditable rather than decorative. Only names the model knows as conditions
    count, so the `run` and `ramp` cells — which hold no path class — cannot
    invent one.
    """
    baseline = (run.meta.get("method") or {}).get("condition") or "?"
    kinds = (model.Kind.STAGED, model.Kind.CAPACITY)
    walked = sorted(
        {
            name
            for kind in kinds
            for scenario in run.scenarios_of(kind)
            for cell in run.cells(scenario)
            if (name := run.condition(scenario, cell["cell"])) in model.CONDITIONS
        }
    )
    if walked and walked != [baseline]:
        return f"{baseline}; stages {', '.join(walked)}"
    return baseline


def reading(run: Run, scenario: str, cell: str, arm: str, metric: str) -> str:
    """One table cell: the median, or `- (reason)` when nothing was measured."""
    stat = run.stat(scenario, cell, arm, metric)
    if stat:
        return model.METRICS[metric].format(stat["median"])
    reason = run.absence(scenario, cell, arm, metric)
    return f"- ({reason})" if reason else "-"


def stage_cells(run: Run, scenario: str) -> list:
    """A staged scenario's cells in schedule order, without the run cell.

    The run cell carries the drift and wedge axis rather than a stage, so it is
    not a column of a per-stage chart.
    """
    return [cell for cell in run.cells(scenario) if cell["cell"] != "run"]


def richest_staged(run: Run) -> str:
    """The staged scenario the master draws: the one with the most stages.

    A run may carry the sweep and a single-stage cost point; the schedule is
    the one with more than one stage, so the master shows a shape rather than a
    lone operating point.
    """
    best, count = "", 0
    for scenario in run.scenarios_of(model.Kind.STAGED):
        measured = len(stage_cells(run, scenario))
        if measured > count:
            best, count = scenario, measured
    return best


def level_cells(run: Run, scenario: str) -> list:
    """A ramp's level cells in ascending order (the `ramp` summary is not one)."""
    named = [
        cell
        for cell in run.cells(scenario)
        if cell["cell"].startswith("L") and cell["cell"][1:].isdigit()
    ]
    return sorted(named, key=lambda cell: int(cell["cell"][1:]))


def drift_series(run: Run, scenario: str, arm: str) -> list:
    """Every drift series one arm recorded, one per measured round.

    Read from the run cell's evidence, which the sampler wrote; the slopes
    printed beside the lines come from the summary, so the chart and the table
    cannot disagree.
    """
    series = []
    for sample in run.results.get("samples") or ():
        if sample.get("arm") != arm or sample.get("scenario") != scenario:
            continue
        if sample.get("cell") != "run" or not sample.get("ok"):
            continue
        points = (sample.get("evidence") or {}).get("drift_series") or []
        if points:
            series.append(points)
    return series


def drift_scenario(run: Run) -> str:
    """The staged scenario whose run cell carries the drift series."""
    for scenario in run.scenarios_of(model.Kind.STAGED):
        if any(drift_series(run, scenario, arm) for arm in run.arms):
            return scenario
    return ""


def drift_base(run: Run, scenario: str) -> float:
    """The run's first drift sample: the shared time origin of every line."""
    starts = [
        points[0]["t"]
        for arm in run.arms
        for points in drift_series(run, scenario, arm)
        if points
    ]
    return min(starts) if starts else 0.0


def cost_cells(run: Run) -> list:
    """The (scenario, cell) pairs the cost chart draws, in reading order.

    The `cost` scenario is the model's own operating point, so it wins; with no
    such scenario, every staged cell that measured the metric is the same
    question asked at a different condition.
    """
    staged = [
        (scenario, cell["cell"])
        for scenario in run.scenarios_of(model.Kind.STAGED)
        for cell in stage_cells(run, scenario)
    ]
    dedicated = [pair for pair in staged if pair[0] == "cost"]
    return [
        pair
        for pair in dedicated or staged
        if any(
            run.median(pair[0], pair[1], arm, "cpu_s_per_gbit") is not None
            for arm in run.arms
        )
    ]


def ladder_rate(run: Run, scenario: str, cell: str) -> float | None:
    """The rate a ladder cell offers, in Mbit/s.

    The cell name is the scenario's own offered rate (`1000M`) and the rate the
    question is asked in; the measured visitor-link egress is the fallback for
    a file whose cells were named differently.
    """
    if cell.endswith("M") and cell[:-1].replace(".", "", 1).isdigit():
        return float(cell[:-1])
    for arm in run.arms:
        measured = run.median(scenario, cell, arm, "offered_gbps")
        if measured:
            return measured * 1000.0
    return None


def udp_points(run: Run) -> list:
    """Every datagram-ladder cell with its offered rate, cheapest first."""
    points = [
        (scenario, cell["cell"], ladder_rate(run, scenario, cell["cell"]))
        for scenario in run.scenarios_of(model.Kind.UDP_LADDER)
        for cell in run.cells(scenario)
    ]
    return sorted(
        (point for point in points if point[2] is not None),
        key=lambda point: point[2],
    )


# --- figure plumbing --------------------------------------------------------
def apply_style() -> None:
    """Set the one house style every figure is drawn in."""
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


def caption(title: str, text: str) -> str:
    """A figure's caption, with the caller's own title in front when given."""
    return f"{title} — {text}" if title else text


def footer_reserve(fig) -> float:
    """The fraction of the figure the two footer lines occupy.

    The lines sit a fixed number of inches from the bottom, so a short figure
    does not print its method over an axis label.
    """
    return min(0.3, 0.52 / fig.get_figheight())


def footer(fig, run: Run, extra: str = "") -> None:
    """The method record under every figure: what was measured, and when.

    Profile, fingerprint, revision, condition and date are the five things a
    reader needs to know whether two figures may be compared at all, so they
    are stated on every one of them.
    """
    meta = run.meta
    head = "  |  ".join(
        (
            f"profile {meta.get('profile', '?')}",
            f"fingerprint {meta.get('fingerprint', '?')}",
            f"revision {revision_text(meta)}",
            f"date {str(meta.get('started', '?'))[:19]}",
        )
    )
    method = [f"condition {condition_text(run)}"]
    if extra:
        method.append(extra)
    height = fig.get_figheight()
    for offset, line in ((0.30, head), (0.06, "  |  ".join(method))):
        fig.text(
            0.5,
            offset / height,
            line,
            ha="center",
            va="bottom",
            fontsize=FONT - 1.5,
            color="0.4",
        )


def save(fig, out: Path) -> None:
    """Write one figure and say where (a silent render is a lost figure)."""
    fig.savefig(out, dpi=DPI, bbox_inches="tight", pad_inches=0.28, facecolor="white")
    plt.close(fig)
    print(f"wrote {out}")


def decade_ticks(low: float, high: float) -> list:
    """Readable 1-2-5 ticks inside a log range.

    A fixed decade list is empty on a fast path whose whole range is below one
    millisecond, and a log axis with no ticks is unreadable.
    """
    out: list = []
    decade = 10.0 ** math.floor(math.log10(max(low, 1e-9)))
    while decade <= high and len(out) < MAX_LOG_TICKS:
        out += [
            factor * decade for factor in (1, 2, 5) if low <= factor * decade <= high
        ]
        decade *= 10
    return out or [low, high]


def step_curve(values: list) -> tuple:
    """A step curve's x/y for per-stage values over the shared stage axis.

    A stage with no reading contributes a break rather than a bridge: the gap
    is the finding, and a straight segment across it would hide a wedge.
    """
    xs: list = []
    ys: list = []
    for index, value in enumerate(values):
        xs += [index - STAGE_HALF, index + STAGE_HALF]
        ys += [value, value]
    return xs, ys


def shade_stages(ax, run: Run, scenario: str, cells: list) -> None:
    """Shade each stage's band: clean green, degraded grey, alternating.

    Two neighbouring degraded stages alternate between two greys so the
    boundary between them stays visible; the alternation counts only degraded
    stages, because a clean band already divides them.
    """
    degraded = 0
    for index, cell in enumerate(cells):
        if run.condition(scenario, cell["cell"]) == "clean":
            face = CLEAN_FACE
        else:
            face = DEGRADED_FACE if degraded % 2 == 0 else DEGRADED_FACE_ALT
            degraded += 1
        ax.axvspan(index - STAGE_HALF, index + STAGE_HALF, color=face, zorder=0, lw=0)


def missing_stages(run: Run, scenario: str, cells: list, arm: str, metric: str) -> list:
    """The stage indices an arm carried no reading for.

    A broken step and an `x` are different findings ("this arm went quiet" vs
    "this stage measured nothing"), and a reader must be able to tell them
    apart on the chart.
    """
    return [
        index
        for index, cell in enumerate(cells)
        if run.median(scenario, cell["cell"], arm, metric) is None
    ]


def log_limits(ax, values: list, floor: float) -> None:
    """A log y range that fits the data and always shows the SLO line."""
    seen = [value for value in values if value and value > 0] or [floor]
    bottom = min([*seen, floor]) / 2.0
    top = max([*seen, floor]) * 2.0
    ax.set_ylim(bottom, top)
    ax.set_yticks(decade_ticks(bottom, top))
    ax.minorticks_off()


# --- the master: the stage timeline ----------------------------------------
def timeline_legend() -> list:
    """The master's encoding, stated once (a per-panel legend covers data)."""
    return [
        Line2D([], [], lw=1.4, color="0.35", label="stage p50 (interactive)"),
        Line2D([], [], lw=1.0, ls="--", color="0.35", label="stage p99"),
        Line2D(
            [],
            [],
            ls="--",
            lw=1.1,
            color=SLO_COLOR,
            label=f"SLO p99 {model.SLO_RTT_P99_MS:g} ms",
        ),
        Patch(color=CLEAN_FACE, label="clean stage"),
        Patch(color=DEGRADED_FACE, label="degraded stage"),
        Line2D([], [], ls="", marker="x", color="0.55", label="no reading"),
    ]


def rtt_panel(ax, run: Run, scenario: str, cells: list, arm: str) -> None:
    """One arm's interactive tail per stage: p50 solid, p99 dashed, log scale."""
    p50 = [run.median(scenario, cell["cell"], arm, "rtt_p50_ms") for cell in cells]
    p99 = [run.median(scenario, cell["cell"], arm, "rtt_p99_ms") for cell in cells]
    color = run.color(arm)
    shade_stages(ax, run, scenario, cells)
    xs, ys = step_curve(p50)
    ax.plot(xs, ys, lw=1.4, color=color, zorder=4)
    xs, ys = step_curve(p99)
    ax.plot(xs, ys, lw=1.0, ls="--", color=color, zorder=4)
    ax.axhline(model.SLO_RTT_P99_MS, color=SLO_COLOR, ls="--", lw=1.1, zorder=3)
    for index in missing_stages(run, scenario, cells, arm, "rtt_p99_ms"):
        ax.plot(
            [index],
            [0.03],
            marker="x",
            ms=4,
            color="0.55",
            transform=ax.get_xaxis_transform(),
            clip_on=False,
            zorder=5,
        )
    ax.set_yscale("log")
    log_limits(ax, [value for value in (*p50, *p99) if value], model.SLO_RTT_P99_MS)
    ax.set_ylabel(f"{arm}\nRTT ms (log)", fontsize=FONT)
    note = wedge_note(run, scenario, arm)
    if note:
        ax.text(
            0.995,
            0.04,
            note,
            transform=ax.transAxes,
            ha="right",
            va="bottom",
            fontsize=FONT - 2,
            color=SLO_COLOR,
        )


def wedge_note(run: Run, scenario: str, arm: str) -> str:
    """The run's wedge record, from the run cell's own metrics.

    A wedge is a silent stretch; printing the count and the longest one is what
    keeps a chart of medians from hiding it.
    """
    count = run.median(scenario, "run", arm, "wedge_count")
    if not count:
        return ""
    longest = run.median(scenario, "run", arm, "wedge_max_s")
    note = f"wedges: {count:.0f}"
    return note if longest is None else f"{note}, longest {longest:.1f} s"


def bulk_panel(ax, run: Run, scenario: str, cells: list, arm: str) -> None:
    """The per-stage bulk spine: what the interactive reading was bought with."""
    values = [
        run.median(scenario, cell["cell"], arm, "throughput_gbps") for cell in cells
    ]
    shade_stages(ax, run, scenario, cells)
    xs, ys = step_curve(values)
    ax.plot(xs, ys, lw=1.2, color=run.color(arm), marker="o", ms=3, zorder=4)
    ax.set_ylim(bottom=0)
    ax.set_ylabel("bulk Gbit/s", fontsize=FONT)


def render_timeline(run: Run, out: Path, title: str) -> None:
    """The master: how the interactive tail moves as the path degrades.

    One row per arm, the per-stage p50 and p99 as steps over the shaded stage
    bands, with the per-stage bulk throughput in a second panel when the
    scenario produced any.
    """
    scenario = richest_staged(run)
    cells = stage_cells(run, scenario) if scenario else []
    if not cells or not run.arms:
        return
    bulk = any(
        run.median(scenario, cell["cell"], arm, "throughput_gbps") is not None
        for cell in cells
        for arm in run.arms
    )
    fig, axes = plt.subplots(
        len(run.arms),
        2 if bulk else 1,
        figsize=(FIGURE_WIDTH if bulk else 8.6, 2.0 * len(run.arms) + 2.6),
        sharex=True,
        squeeze=False,
    )
    for row, arm in enumerate(run.arms):
        rtt_panel(axes[row][0], run, scenario, cells, arm)
        if bulk:
            bulk_panel(axes[row][1], run, scenario, cells, arm)
    axes[-1][0].set_xticks(range(len(cells)), labels=[cell["cell"] for cell in cells])
    for label in axes[-1][0].get_xticklabels():
        label.set_rotation(30)
        label.set_ha("right")
    fig.legend(
        handles=timeline_legend(),
        loc="upper center",
        ncol=6,
        bbox_to_anchor=(0.5, 0.965),
        fontsize=FONT - 0.5,
    )
    fig.suptitle(
        caption(
            title,
            f"interactive stream through {scenario} — per-stage p50 (solid) "
            "and p99 (dashed)",
        ),
        y=1.0,
        fontsize=FONT + 3,
    )
    footer(fig, run, extra=f"SLO interactive p99 <= {model.SLO_RTT_P99_MS:g} ms")
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.92))
    save(fig, out)


# --- small multiples: one panel per stage ----------------------------------
def stage_panel(ax, run: Run, scenario: str, cell: str, rows: dict) -> None:
    """One stage: every arm's p50 dot and p99 bar on a shared log axis."""
    measured: list = []
    for arm, y in rows.items():
        p50 = run.median(scenario, cell, arm, "rtt_p50_ms")
        if p50 is None:
            ax.plot(
                [0.02],
                [y],
                marker="x",
                ms=4,
                color="0.6",
                transform=ax.get_yaxis_transform(),
                clip_on=False,
            )
            continue
        p99 = max(run.median(scenario, cell, arm, "rtt_p99_ms") or p50, p50)
        worst = run.median(scenario, cell, arm, "rtt_max_ms")
        color = run.color(arm)
        ax.plot(
            [p50, p99],
            [y, y],
            lw=2.2,
            color=color,
            alpha=0.55,
            solid_capstyle="butt",
            zorder=2,
        )
        ax.plot([p50], [y], "o", ms=4.5, color=color, zorder=3)
        if worst is not None:
            ax.plot([worst], [y], "|", ms=6, color=color, alpha=0.6, zorder=3)
        measured += [p50, p99, *([worst] if worst is not None else [])]
    low, high = axis_range(measured, model.SLO_RTT_P99_MS)
    ax.axvspan(low, min(model.SLO_RTT_P99_MS, high), color=CLEAN_FACE, zorder=0, lw=0)
    ax.axvline(model.SLO_RTT_P99_MS, color=SLO_COLOR, ls="--", lw=1.0, zorder=1)
    ax.set_xscale("log")
    ax.set_xlim(low, high)
    ticks = decade_ticks(low, high)
    ax.set_xticks(ticks, labels=[f"{tick:g}" for tick in ticks])
    ax.minorticks_off()
    ax.grid(axis="x", alpha=0.1)


def axis_range(values: list, floor: float) -> tuple:
    """A log x range that fits the data and always shows the SLO line."""
    seen = [value for value in values if value and value > 0] or [floor]
    return max(min([*seen, floor]) / 2.0, 1e-3), max([*seen, floor]) * 2.0


def render_stages(run: Run, out: Path, title: str) -> None:
    """Small multiples: one panel per staged cell, one lollipop per arm.

    The dot is the stage's p50 median, the bar reaches its p99, the tick is the
    worst single round trip, and an `x` is an arm this cell measured nothing
    for: which arm holds the tail, per condition, at a glance.
    """
    panels = [
        (scenario, cell["cell"])
        for scenario in run.scenarios_of(model.Kind.STAGED)
        for cell in stage_cells(run, scenario)
    ]
    if not panels or not run.arms:
        return
    scenarios = len({scenario for scenario, _ in panels})
    fig, axes = plt.subplots(
        1,
        len(panels),
        figsize=(STAGE_WIDTH * len(panels) + 1.8, 0.42 * len(run.arms) + 3.2),
        sharey=True,
        squeeze=False,
    )
    rows = {arm: index for index, arm in enumerate(run.arms)}
    for ax, (scenario, cell) in zip(axes[0], panels, strict=True):
        ax.set_title(f"{scenario}\n{cell}" if scenarios > 1 else cell, fontsize=FONT)
        stage_panel(ax, run, scenario, cell, rows)
    first = axes[0][0]
    first.set_yticks(list(rows.values()), labels=list(rows))
    first.set_ylim(-0.6, len(run.arms) - 0.4)
    first.invert_yaxis()
    fig.suptitle(
        caption(
            title,
            "interactive RTT per stage, ms (log) — p50 dot, p99 bar, worst "
            "round-trip tick; x = no reading",
        ),
        y=0.99,
        fontsize=FONT + 3,
    )
    footer(fig, run, extra=f"SLO p99 {model.SLO_RTT_P99_MS:g} ms")
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.9))
    save(fig, out)


# --- capacity ---------------------------------------------------------------
def capacity_panel(ax, run: Run, scenario: str, levels: list, metric: str) -> list:
    """One capacity reading against the offered level, one line per arm.

    Returns the (arm, level, colour) marks so the caller can print them once,
    beside the panel the reader looks at first.
    """
    xs = [int(cell["cell"][1:]) for cell in levels]
    marks: list = []
    measured: list = []
    for arm in run.arms:
        ys = [run.median(scenario, cell["cell"], arm, metric) for cell in levels]
        color = run.color(arm)
        ax.plot(xs, ys, "o-", ms=4, lw=1.2, color=color, label=arm)
        measured += [value for value in ys if value is not None]
        level = run.median(scenario, "ramp", arm, "capacity_streams")
        if level is not None:
            ax.axvline(level, color=color, ls=":", lw=0.9, alpha=0.7)
            marks.append((arm, level, color))
    label = dict(CAPACITY_PANELS)[metric]
    if metric == "rtt_p99_ms":
        ax.set_yscale("log")
        log_limits(ax, measured, model.SLO_RTT_P99_MS)
        low, high = ax.get_ylim()
        ax.axhspan(
            low, min(model.SLO_RTT_P99_MS, high), color=CLEAN_FACE, zorder=0, lw=0
        )
        ax.axhline(
            model.SLO_RTT_P99_MS,
            color=SLO_COLOR,
            ls="--",
            lw=1.1,
            zorder=3,
            label=f"SLO p99 {model.SLO_RTT_P99_MS:g} ms",
        )
    else:
        ax.set_ylim(bottom=0)
    ax.set_ylabel(label, fontsize=FONT)
    return marks


def render_capacity(run: Run, out: Path, title: str) -> None:
    """How much load the path carries while a fresh visitor still meets the SLO.

    Throughput and the interactive tail against the offered stream level, with
    the SLO line and each arm's last sustainable level marked: the level is the
    answer, and the curve shows how close the next one came to losing it.
    """
    scenarios = run.scenarios_of(model.Kind.CAPACITY)
    if not scenarios or not run.arms:
        return
    scenario = scenarios[0]
    levels = level_cells(run, scenario)
    if not levels:
        return
    panels = [
        metric
        for metric, _label in CAPACITY_PANELS
        if any(
            run.median(scenario, cell["cell"], arm, metric) is not None
            for cell in levels
            for arm in run.arms
        )
    ]
    if not panels:
        return
    fig, axes = plt.subplots(
        len(panels),
        1,
        figsize=(9.0, 3.2 * len(panels) + 1.8),
        sharex=True,
        squeeze=False,
    )
    marks: list = []
    for axis, metric in zip(axes[:, 0], panels, strict=True):
        marks = capacity_panel(axis, run, scenario, levels, metric) or marks
    top = axes[0][0]
    for index, (arm, level, color) in enumerate(marks):
        top.text(
            0.995,
            0.04 + 0.05 * (len(marks) - 1 - index),
            f"{arm}: {level:.0f} streams",
            transform=top.transAxes,
            ha="right",
            va="bottom",
            fontsize=FONT - 1,
            color=color,
            bbox={"facecolor": "white", "alpha": 0.75, "lw": 0, "pad": 1.5},
        )
    axes[-1][0].set_xlabel("bulk streams offered (load)")
    axes[-1][0].set_xticks(
        [int(cell["cell"][1:]) for cell in levels],
        labels=[cell["cell"] for cell in levels],
    )
    top.legend(loc="upper left", ncol=min(len(run.arms), 3))
    fig.suptitle(
        caption(
            title,
            f"capacity ramp ({scenario}) — throughput and interactive p99 "
            "against offered load",
        ),
        y=0.99,
        fontsize=FONT + 3,
    )
    footer(fig, run)
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.93))
    save(fig, out)


# --- UDP ladder -------------------------------------------------------------
def udp_panel(ax, run: Run, points: list, metric: str) -> None:
    """One datagram reading against the offered rate, one line per arm."""
    for arm in run.arms:
        xs = [rate for _scenario, _cell, rate in points]
        ys = [
            run.median(scenario, cell, arm, metric) for scenario, cell, _rate in points
        ]
        ax.plot(xs, ys, "o-", ms=4, lw=1.2, color=run.color(arm), label=arm)
    rates = sorted({rate for _scenario, _cell, rate in points})
    if metric == "udp_recv_mbit":
        ax.plot(
            rates,
            rates,
            ls="--",
            lw=1.0,
            color=REFERENCE_COLOR,
            label="offered (no loss)",
        )
        ax.set_ylabel("received Mbit/s", fontsize=FONT)
    else:
        ax.set_ylim(bottom=0)
        ax.set_ylabel("datagram loss %", fontsize=FONT)
    ax.set_xticks(rates, labels=[f"{rate:g}" for rate in rates])


def render_udp(run: Run, out: Path, title: str) -> None:
    """The datagram ladder: where a UDP path starts shedding.

    Received rate (with the y = x line a lossless path would follow) and loss
    against the offered rate, one line per arm: the rate where a curve leaves
    the diagonal is the answer.
    """
    points = udp_points(run)
    if not points or not run.arms:
        return
    panels = [
        metric
        for metric in ("udp_recv_mbit", "udp_loss_pct")
        if any(
            run.median(scenario, cell, arm, metric) is not None
            for scenario, cell, _rate in points
            for arm in run.arms
        )
    ]
    if not panels:
        return
    fig, axes = plt.subplots(
        len(panels),
        1,
        figsize=(9.5, 2.9 * len(panels) + 1.8),
        sharex=True,
        squeeze=False,
    )
    for axis, metric in zip(axes[:, 0], panels, strict=True):
        udp_panel(axis, run, points, metric)
    axes[-1][0].set_xlabel("offered rate (Mbit/s)")
    axes[0][0].legend(loc="upper left", ncol=min(len(run.arms) + 1, 4))
    fig.suptitle(
        caption(title, "datagram ladder — received rate and loss against offered rate"),
        y=0.99,
        fontsize=FONT + 3,
    )
    footer(fig, run)
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.93))
    save(fig, out)


# --- drift ------------------------------------------------------------------
def drift_panel(ax, run: Run, scenario: str, panel: DriftPanel, base: float) -> None:
    """One drift series over time, one line per arm and round.

    The fitted slope rides in the legend, next to the arm it belongs to: it is
    the summary's own number, and a level without its slope cannot say whether
    anything leaked.
    """
    unit = model.METRICS[panel.slope_metric].unit
    handles = []
    for arm in run.arms:
        color = run.color(arm)
        for points in drift_series(run, scenario, arm):
            stamps = [point for point in points if point.get(panel.key) is not None]
            ax.plot(
                [(point["t"] - base) / 60.0 for point in stamps],
                [point[panel.key] * panel.scale for point in stamps],
                lw=0.9,
                color=color,
                alpha=0.85,
            )
        slope = run.median(scenario, "run", arm, panel.slope_metric)
        label = f"{arm}: no slope" if slope is None else f"{arm}: {slope:+.3f} {unit}"
        handles.append(Line2D([], [], color=color, lw=1.4, label=label))
    ax.set_ylabel(panel.label, fontsize=FONT)
    ax.margins(y=0.25)  # headroom, so the legend does not sit on the lines
    ax.legend(handles=handles, loc="upper left", ncol=min(len(handles), 3))


def render_drift(run: Run, out: Path, title: str) -> None:
    """Does anything leak? The footprint over time, with its fitted slope.

    RSS, open descriptors and threads from the run cell's own samples, with the
    summary's slope per minute in the legend: a leak is a slope, not a level.
    """
    scenario = drift_scenario(run)
    panels = [
        panel
        for panel in DRIFT_PANELS
        if scenario
        and any(
            any(point.get(panel.key) is not None for point in points)
            for arm in run.arms
            for points in drift_series(run, scenario, arm)
        )
    ]
    if not panels or not run.arms:
        return
    base = drift_base(run, scenario)
    fig, axes = plt.subplots(
        len(panels),
        1,
        figsize=(11.0, 2.3 * len(panels) + 2.2),
        sharex=True,
        squeeze=False,
    )
    for axis, panel in zip(axes[:, 0], panels, strict=True):
        drift_panel(axis, run, scenario, panel, base)
    axes[-1][0].set_xlabel("minutes since the run's first drift sample")
    fig.suptitle(
        caption(
            title,
            f"footprint over the {scenario} run — the legend carries each "
            "arm's fitted slope per minute",
        ),
        y=0.99,
        fontsize=FONT + 3,
    )
    footer(fig, run, extra="slopes are the run cell's own least-squares fits")
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.93))
    save(fig, out)


# --- cost -------------------------------------------------------------------
def render_cost(run: Run, out: Path, title: str) -> None:
    """What a carried bit costs: CPU-seconds per Gbit, per arm.

    One bar per arm at the run's cost operating point (the `cost` scenario when
    the run has one, otherwise the staged cells that measured the metric). A
    run with several such cells groups the bars by cell, because one average
    across the schedule would hide which condition the CPU went to.
    """
    cells = cost_cells(run)
    if not cells or not run.arms:
        return
    fig, ax = plt.subplots(figsize=(1.5 * len(cells) + 2.6, 4.8))
    width = 0.8 / len(run.arms)
    for index, (scenario, cell) in enumerate(cells):
        for slot, arm in enumerate(run.arms):
            value = run.median(scenario, cell, arm, "cpu_s_per_gbit")
            if value is None:
                # A missing bar is a finding, not an empty slot: the control
                # arm runs no tool, so there is nothing to charge CPU to.
                ax.plot(
                    [index - 0.4 + width * (slot + 0.5)],
                    [0.0],
                    marker="x",
                    ms=4,
                    color="0.55",
                    clip_on=False,
                )
                continue
            ax.bar(
                index - 0.4 + width * (slot + 0.5),
                value,
                width=width * 0.9,
                color=run.color(arm),
            )
    scenarios = {scenario for scenario, _ in cells}
    ax.set_xticks(
        range(len(cells)),
        labels=[
            cell if len(scenarios) == 1 else f"{scenario}\n{cell}"
            for scenario, cell in cells
        ],
    )
    ax.set_ylabel("CPU-seconds per carried Gbit (lower is better)")
    ax.legend(
        handles=[Patch(color=run.color(arm), label=arm) for arm in run.arms],
        loc="upper left",
        ncol=min(len(run.arms), 3),
    )
    fig.suptitle(
        caption(
            title,
            "cost at the operating point — CPU-seconds per carried Gbit; "
            "x = no reading",
        ),
        y=0.97,
        fontsize=FONT + 3,
    )
    footer(fig, run, extra=_cost_note(run, cells))
    fig.tight_layout(rect=(0, footer_reserve(fig), 1, 0.93))
    save(fig, out)


def _cost_note(run: Run, cells: list) -> str:
    """Why an arm may have no bar: the control runs no tool to charge CPU to."""
    measured = {
        arm
        for scenario, cell in cells
        for arm in run.arms
        if run.median(scenario, cell, arm, "cpu_s_per_gbit") is not None
    }
    if "control" in run.arms and "control" not in measured:
        return "the control arm runs no tool, so it has no CPU cost"
    return ""


# --- tables -----------------------------------------------------------------
def row_line(cells: list) -> str:
    """One markdown table row from its cells."""
    return "| " + " | ".join(cells) + " |"


def table_header(labels: list) -> list:
    """A markdown table's header and separator rows."""
    return [row_line(["arm", *labels]), "|---" * (len(labels) + 1) + "|"]


def headline_section(run: Run) -> list:
    """One table per scenario: the metric its claim rests on, per cell and arm."""
    sections: list = []
    for scenario in run.scenario_ids():
        metric = run.headline(scenario)
        cells = run.cells(scenario)
        if metric not in model.METRICS or not cells:
            continue
        unit = model.METRICS[metric].unit
        lines = [f"#### {scenario} — {metric} ({unit})", ""]
        lines += table_header([cell["cell"] or scenario for cell in cells])
        for arm in run.arms:
            lines.append(
                row_line(
                    [
                        arm,
                        *[
                            reading(run, scenario, cell["cell"], arm, metric)
                            for cell in cells
                        ],
                    ]
                )
            )
        sections += [*lines, ""]
    return ["### Headline metric per cell", "", *sections] if sections else []


def stage_section(run: Run) -> list:
    """Per stage: the interactive p99, the statistic the SLO is stated in."""
    rows = [
        (scenario, cell["cell"])
        for scenario in run.scenarios_of(model.Kind.STAGED)
        for cell in stage_cells(run, scenario)
    ]
    if not rows:
        return []
    scenarios = {scenario for scenario, _ in rows}
    labels = [
        cell if len(scenarios) == 1 else f"{scenario} {cell}" for scenario, cell in rows
    ]
    lines = ["### Per stage: interactive p99 (ms)", "", *table_header(labels)]
    lines += [
        row_line(
            [
                arm,
                *[
                    reading(run, scenario, cell, arm, "rtt_p99_ms")
                    for scenario, cell in rows
                ],
            ]
        )
        for arm in run.arms
    ]
    return [*lines, ""]


def udp_section(run: Run) -> list:
    """The datagram cells: what arrived and what was lost, per arm."""
    rows = [
        (scenario, cell["cell"])
        for scenario in run.scenario_ids()
        if run.kind(scenario) in (model.Kind.UDP, model.Kind.UDP_LADDER)
        for cell in run.cells(scenario)
    ]
    if not rows:
        return []
    labels = [
        label
        for _scenario, cell in rows
        for label in (f"{cell} recv (Mbit/s)", f"{cell} loss (%)")
    ]
    metrics = ("udp_recv_mbit", "udp_loss_pct")
    lines = ["### UDP: received rate and loss", "", *table_header(labels)]
    for arm in run.arms:
        values = [
            reading(run, scenario, cell, arm, metric)
            for scenario, cell in rows
            for metric in metrics
        ]
        lines.append(row_line([arm, *values]))
    return [*lines, ""]


def drift_section(run: Run) -> list:
    """The fitted slopes of the staged run: a leak is a slope, not a level."""
    scenario = drift_scenario(run)
    if not scenario:
        return []
    columns = (
        ("drift_rss_mib_per_min", "RSS (MiB/min)"),
        ("drift_fds_per_min", "fds (/min)"),
        ("drift_threads_per_min", "threads (/min)"),
        ("wedge_count", "wedges"),
        ("wedge_max_s", "longest wedge (s)"),
    )
    lines = [
        f"### Drift ({scenario}, run cell)",
        "",
        *table_header([label for _metric, label in columns]),
    ]
    lines += [
        row_line(
            [
                arm,
                *[
                    reading(run, scenario, "run", arm, metric)
                    for metric, _label in columns
                ],
            ]
        )
        for arm in run.arms
    ]
    return [*lines, ""]


def cost_section(run: Run) -> list:
    """CPU-seconds per carried Gbit, at the operating point the run measured."""
    cells = cost_cells(run)
    if not cells:
        return []
    scenarios = {scenario for scenario, _ in cells}
    labels = [
        cell if len(scenarios) == 1 else f"{scenario} {cell}"
        for scenario, cell in cells
    ]
    lines = ["### Cost: CPU-seconds per carried Gbit", "", *table_header(labels)]
    lines += [
        row_line(
            [
                arm,
                *[
                    reading(run, scenario, cell, arm, "cpu_s_per_gbit")
                    for scenario, cell in cells
                ],
            ]
        )
        for arm in run.arms
    ]
    return [*lines, ""]


def tables(run: Run, title: str) -> str:
    """The markdown view of the run: the numbers behind every figure.

    It is the same summary the charts read, so a table and its figure cannot
    disagree; an absent reading is printed with the reason it is absent, and
    never as a zero.
    """
    meta = run.meta
    head = "  |  ".join(
        (
            f"profile `{meta.get('profile', '?')}`",
            f"fingerprint `{meta.get('fingerprint', '?')}`",
            f"revision `{revision_text(meta)}`",
            f"condition `{condition_text(run)}`",
            f"date {str(meta.get('started', '?'))[:19]}",
            f"elapsed {meta.get('elapsed_s', '?')} s of {meta.get('budget_s', '?')} s",
        )
    )
    lines = [
        f"## {title}"
        if title
        else f"## Bench results `{meta.get('fingerprint', '?')}`",
        "",
        head,
        "",
    ]
    lines += [
        (
            "A reading shown as `- (reason)` carried nothing, and the reason is "
            "the instrument's own;"
        ),
        (
            f"the SLO is an interactive p99 <= {model.SLO_RTT_P99_MS:g} ms and an "
            f"error rate <= {model.SLO_ERROR_RATE_PCT:g} %."
        ),
        "",
    ]
    lines += headline_section(run)
    lines += stage_section(run)
    lines += udp_section(run)
    lines += drift_section(run)
    lines += cost_section(run)
    return "\n".join(lines)


# --- entry point ------------------------------------------------------------
#: Every figure, in the order the argument list documents them.
FIGURES = (
    ("timeline", render_timeline),
    ("stages", render_stages),
    ("capacity", render_capacity),
    ("udp", render_udp),
    ("drift", render_drift),
    ("cost", render_cost),
)


def parse_args(argv) -> argparse.Namespace:
    """The command line: one results file in, PNGs (and tables) out."""
    parser = argparse.ArgumentParser(
        prog="plot.py",
        description="Render a bench results file's charts and tables.",
    )
    parser.add_argument("results", type=Path, help="the results file to render")
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=None,
        help="where the PNGs go (default: the results file's own directory)",
    )
    parser.add_argument(
        "--markdown", action="store_true", help="print the tables to stdout"
    )
    parser.add_argument(
        "--title", default="", help="a title prefixed to every figure and table"
    )
    return parser.parse_args(argv)


def main(argv=None) -> int:
    """Render every figure the file supports, and the tables on request."""
    args = parse_args(argv)
    run = load_run(args.results)
    apply_style()
    out_dir = args.out_dir or args.results.parent
    out_dir.mkdir(parents=True, exist_ok=True)
    stem = f"bench-{run.meta.get('fingerprint') or 'unfingerprinted'}"
    for name, render in FIGURES:
        render(run, out_dir / f"{stem}-{name}.png", args.title)
    if args.markdown:
        print(tables(run, args.title))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
