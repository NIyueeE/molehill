#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["matplotlib>=3.5"]
# ///
"""Render the Soak benchmark charts from a results file.

The chart set is the model's output surface — every panel is a time axis
or a curve, never a single averaged number:

- `assets/soak-<ver>.png`  the master: per stage, the interactive stream's
  RTT distribution over time (the queueing-under-load detector), with the
  bulk throughput and the path schedule on the same axis
- `soak-<ver>-capacity.png`  the response-time-vs-load curve per path class
  with the SLO line — the sustainable load is where the curve crosses it
- `soak-<ver>-udp.png`  the UDP session's RTT/loss over time
- `soak-<ver>-cost.png`  CPU-seconds per carried Gbit at the operating point
- `soak-<ver>-drift.png`  the soak's drift axis (handles, RSS, CPU slopes)

Peers are plotted beside molehill in every panel: the workload is
identical, so the comparison is the point.
"""
import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt


def load(path: Path) -> dict:
    return json.loads(Path(path).read_text())


def series(test: dict, metric: str) -> list:
    return [(r["t"], r["v"]) for r in test["series"]
            if r.get("metric") == metric and isinstance(r.get("v"), (int, float))]


def stage_spans(test: dict) -> list:
    starts = [s["t_start"] for s in test["stages"]]
    spans = []
    for i, s in enumerate(test["stages"]):
        end = starts[i + 1] if i + 1 < len(starts) else (
            test["series"][-1]["t"] if test["series"] else s["t_start"] + s["secs"])
        spans.append((s["t_start"], end, s["stage"]))
    return spans


def shade_stages(ax, test: dict, label: bool = True) -> None:
    base = t0_of(test)
    for i, (s0, s1, name) in enumerate(stage_spans(test)):
        t0, t1 = s0 - base, s1 - base
        if i % 2 == 0:
            ax.axvspan(t0, t1, color="0.92", zorder=0)
        if label:
            ax.text((t0 + t1) / 2, ax.get_ylim()[1], name, ha="center",
                    va="top", fontsize=7, color="0.35")


def t0_of(test: dict) -> float:
    return test["series"][0]["t"] if test["series"] else 0.0


def render_master(tests: list, out: Path) -> None:
    """Per tool: the interactive RTT over time with the bulk throughput."""
    fig, axes = plt.subplots(len(tests), 1, figsize=(11, 3.2 * len(tests)),
                             sharex=True, squeeze=False)
    for ax_row, test in zip(axes, tests):
        ax = ax_row[0]
        t0 = t0_of(test)
        rtt = [(t - t0, v) for t, v in series(test, "rtt_interactive_ms")]
        bulk = [(t - t0, v) for t, v in series(test, "throughput_bulk_gbps")]
        if rtt:
            ax.plot([p[0] for p in rtt], [p[1] for p in rtt], ".", ms=1.5,
                    color="tab:blue", label="interactive RTT")
        if bulk:
            ax2 = ax.twinx()
            ax2.plot([p[0] for p in bulk], [p[1] for p in bulk], "-", lw=1.0,
                     color="tab:orange", alpha=0.8, label="bulk Gbit/s")
            ax2.set_ylabel("bulk Gbit/s", color="tab:orange", fontsize=8)
            ax2.tick_params(axis="y", labelcolor="tab:orange", labelsize=7)
        ax.axhline(50, color="tab:red", ls="--", lw=1,
                   label="SLO p99 = 50 ms")
        shade_stages(ax, test)
        ax.set_ylabel(f"{test['tool']}\nRTT ms", fontsize=8)
        ax.set_ylim(bottom=0)
        ax.tick_params(labelsize=7)
        ax.legend(fontsize=7, loc="upper right")
    axes[-1][0].set_xlabel("seconds since test start", fontsize=8)
    fig.suptitle("Soak: interactive stream under the stage schedule "
                 "(shaded = path class, line = SLO)", fontsize=10)
    fig.tight_layout(rect=(0, 0, 1, 0.97))
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def render_capacity(tests: list, out: Path) -> None:
    """The response-time-vs-load curve with the SLO line."""
    fig, ax = plt.subplots(figsize=(9, 5))
    for test in tests:
        curve = test["metrics"].get("curve") or []
        if not curve:
            continue
        xs = [c["streams"] for c in curve]
        ys = [c["rtt_p99"] or 0 for c in curve]
        ax.plot(xs, ys, "o-", ms=4, label=test["tool"])
        broken = next((c for c in curve if c["slo_broken"]), None)
        if broken:
            ax.axvline(broken["streams"], ls=":", lw=1,
                       color=ax.lines[-1].get_color())
    ax.axhline(50, color="tab:red", ls="--", lw=1, label="SLO p99 = 50 ms")
    ax.set_xlabel("bulk streams offered (load)", fontsize=9)
    ax.set_ylabel("interactive RTT p99 (ms)", fontsize=9)
    ax.set_title("Capacity: sustainable load per tool "
                 "(where the curve crosses the SLO)", fontsize=10)
    ax.legend(fontsize=8)
    ax.tick_params(labelsize=8)
    fig.tight_layout()
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def render_udp(tests: list, out: Path) -> None:
    fig, axes = plt.subplots(2, 1, figsize=(11, 6), sharex=True,
                             squeeze=False)
    for test in tests:
        t0 = t0_of(test)
        rtt = [(t - t0, v) for t, v in series(test, "rtt_udp_ms")]
        loss = [(t - t0, v) for t, v in series(test, "udp_loss")]
        if rtt:
            axes[0][0].plot([p[0] for p in rtt], [p[1] for p in rtt], ".",
                            ms=2, label=test["tool"])
        if loss:
            axes[1][0].plot([p[0] for p in loss], [p[1] for p in loss], "|",
                            ms=6, label=test["tool"])
    if tests:
        shade_stages(axes[0][0], tests[0], label=False)
        shade_stages(axes[1][0], tests[0], label=False)
    axes[0][0].set_ylabel("UDP RTT ms", fontsize=9)
    axes[0][0].legend(fontsize=7)
    axes[1][0].set_ylabel("loss events", fontsize=9)
    axes[1][0].set_xlabel("seconds since test start", fontsize=8)
    for ax in axes[:, 0]:
        ax.tick_params(labelsize=7)
    fig.suptitle("UDP session quality over the stage schedule", fontsize=10)
    fig.tight_layout(rect=(0, 0, 1, 0.96))
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def render_drift(tests: list, out: Path) -> None:
    """The soak's drift axis: handles, RSS, CPU over time."""
    fig, axes = plt.subplots(3, 1, figsize=(11, 8), sharex=True, squeeze=False)
    panels = [("fds", ["server_fds", "client_fds"], "open fds"),
              ("rss", ["server_rss_kb", "client_rss_kb"], "RSS KiB"),
              ("cpu", ["server_cpu_pct", "client_cpu_pct"], "CPU %")]
    for (name, metrics, ylabel), ax in zip(panels, axes[:, 0]):
        for test in tests:
            t0 = t0_of(test)
            for metric, style in zip(metrics, ("-", "--")):
                pts = [(t - t0, v) for t, v in series(test, metric)]
                if pts:
                    ax.plot([p[0] for p in pts], [p[1] for p in pts], style,
                            lw=0.8, label=f"{test['tool']} {metric}")
        ax.set_ylabel(ylabel, fontsize=9)
        ax.tick_params(labelsize=7)
        ax.legend(fontsize=6, ncol=2)
    axes[-1][0].set_xlabel("seconds since test start", fontsize=8)
    fig.suptitle("Drift: handles, memory and CPU over the soak "
                 "(a leak is a slope, not a level)", fontsize=10)
    fig.tight_layout(rect=(0, 0, 1, 0.96))
    fig.savefig(out, dpi=130)
    plt.close(fig)
    print(f"wrote {out}")


def tables(tests: list, meta: dict) -> None:
    print("\n## Soak results\n")
    print(f"host: {meta.get('hostname')} | kernel: {meta.get('kernel')} | "
          f"batch: {meta.get('batch')} of {meta.get('nproc')} cores | "
          f"SLO: interactive p99 <= {meta.get('slo', {}).get('rtt_p99_ms')} ms")
    print()
    print("| tool | version | test | sustainable streams | bulk Gbit/s | "
          "interactive p99 | worst 1s | interactive err% | churn/s | "
          "churn p99 | udp p99 | RSS server | RSS slope/min | "
          "fds slope/min |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
    for t in tests:
        m = t["metrics"]
        bulk = (m.get("bulk_throughput_stats") or {}).get("mean")
        rtt = (m.get("interactive_rtt_stats") or {}).get("p99")
        worst = (m.get("interactive_rtt_worst_1s") or {}).get("mean")
        udp = (m.get("udp_rtt_stats") or {}).get("p99")
        rss_v = series(t, "server_rss_kb")
        rss_v = round(sum(v for _, v in rss_v) / len(rss_v) / 1024) if rss_v else None
        print(f"| {t['tool']} | {t.get('version')} | {t['test']} | "
              f"{m.get('max_sustainable_streams', '-')} | "
              f"{bulk if bulk is not None else '-'} | "
              f"{rtt if rtt is not None else '-'} | "
              f"{worst if worst is not None else '-'} | "
              f"{100 * m.get('interactive_error_rate', 0):.1f} | "
              f"{sum(s.get('churn_per_s') or 0 for s in t['stages']) // max(1, len(t['stages']))} | "
              f"- | "
              f"{udp if udp is not None else '-'} | "
              f"{rss_v if rss_v is not None else '-'} MiB | "
              f"{m.get('server_rss_kb_slope_per_min', '-')} | "
              f"{m.get('server_fds_slope_per_min', '-')} |")
    # the per-stage view: the shape of the run, not its average
    stage_order: list = []
    for t in tests:
        for s in t["stages"]:
            if s["stage"] not in stage_order:
                stage_order.append(s["stage"])
    print()
    print("### Per stage (interactive stream RTT p99)")
    print()
    print("| tool | " + " | ".join(stage_order) + " |")
    print("|" + "---|" * (len(stage_order) + 1))
    for t in tests:
        cells = []
        for stage in stage_order:
            s = next((x for x in t["stages"] if x["stage"] == stage), None)
            if s is None:
                cells.append("-")
            elif s.get("flat_segments"):
                cells.append("wedge")
            elif s.get("rtt_p99") is not None:
                cells.append(f"{s['rtt_p99']} ms")
            else:
                cells.append("no data")
        print(f"| {t['tool']} | " + " | ".join(cells) + " |")
    for t in tests:
        for s in t.get("stages", []):
            if s.get("flat_segments"):
                print(f"\n**{t['tool']}** stage `{s['stage']}` wedged: "
                      f"{len(s['flat_segments'])} flat segment(s), "
                      f"longest {max(x['duration_s'] for x in s['flat_segments'])}s")


def main() -> None:
    if len(sys.argv) > 1:
        path = Path(sys.argv[1])
    else:
        files = sorted(Path(__file__).parent.glob("results-soak-*.json"),
                       key=lambda p: p.name)
        if not files:
            sys.exit("no results-soak-*.json found")
        path = files[-1]
    data = load(path)
    ver = path.stem.replace("results-soak-", "") or "dev"
    assets = Path(__file__).parents[3] / "assets"
    assets.mkdir(parents=True, exist_ok=True)
    tests = [t for t in data["tests"] if not t.get("error")]
    render_master(tests, assets / f"soak-{ver}.png")
    cap = [t for t in tests if t["test"] == "capacity"]
    if cap:
        render_capacity(cap, assets / f"soak-{ver}-capacity.png")
    if any(series(t, "rtt_udp_ms") for t in tests):
        render_udp(tests, assets / f"soak-{ver}-udp.png")
    if any("drift_from_t" in t["metrics"] for t in tests):
        render_drift(tests, assets / f"soak-{ver}-drift.png")
    tables(tests, data["meta"])


if __name__ == "__main__":
    main()
