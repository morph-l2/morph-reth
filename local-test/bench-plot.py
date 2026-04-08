#!/usr/bin/env python3
"""Generate benchmark charts from JSON-lines output.

Requires: matplotlib, numpy (no other dependencies).

Usage:
    python3 bench-plot.py --input <results-dir> --output <charts-dir> [--type all]
"""

import argparse
import json
import sys
from collections import defaultdict
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DPI = 150
COLOR_RETH = "#2196F3"
COLOR_GETH = "#FF9800"
ENGINE_COLORS = {"reth": COLOR_RETH, "geth": COLOR_GETH}
FINAL_MODE_DIRS = ("exec", "e2e", "sustained", "openloop")


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _load_jsonl(path: Path) -> list[dict]:
    """Load a JSON-lines file, skipping malformed lines."""
    records = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    return records


def _load_all_jsonl(input_dir: Path) -> list[dict]:
    """Load only final matrix records from final mode directories."""
    records = []

    mode_dirs = [input_dir / mode for mode in FINAL_MODE_DIRS if (input_dir / mode).is_dir()]
    if not mode_dirs and input_dir.name in FINAL_MODE_DIRS:
        mode_dirs = [input_dir]

    for mode_dir in mode_dirs:
        for p in sorted(mode_dir.rglob("*.jsonl")):
            records.extend(_load_jsonl(p))
    return records


def _engine_color(engine: str) -> str:
    return ENGINE_COLORS.get(engine, "#888888")


def _save(fig, output_dir: Path, name: str):
    fig.tight_layout()
    dest = output_dir / name
    fig.savefig(str(dest), dpi=DPI)
    plt.close(fig)
    print(f"  saved {dest}")


# ---------------------------------------------------------------------------
# Chart 1: Sweep TPS / MGas curve
# ---------------------------------------------------------------------------

def chart_sweep(input_dir: Path, output_dir: Path):
    """Sweep TPS / MGas curve from sweep summary JSON files."""
    summaries = list(input_dir.rglob("*-sweep-summary.json"))
    if not summaries:
        print("  [skip] no *-sweep-summary.json files found")
        return

    # Parse summaries: keyed by (workload, engine)
    data: dict[str, dict[str, dict]] = defaultdict(dict)  # workload -> engine -> summary
    for p in summaries:
        try:
            s = json.loads(p.read_text())
        except (json.JSONDecodeError, OSError):
            continue
        engine = s.get("engine", "unknown")
        workload = s.get("workload", "unknown")
        data[workload][engine] = s

    workloads = sorted(data.keys())
    if not workloads:
        print("  [skip] no valid sweep summaries")
        return

    n = len(workloads)
    fig, axes = plt.subplots(1, n, figsize=(7 * n, 5), squeeze=False)

    for idx, wl in enumerate(workloads):
        ax_left = axes[0][idx]
        ax_right = ax_left.twinx()

        for engine in ("reth", "geth"):
            if engine not in data[wl]:
                continue
            points = data[wl][engine].get("points", [])
            if not points:
                continue

            xs = [p["txs_per_block"] for p in points]
            tps = [p.get("median_tps", 0) for p in points]
            mgas = [p.get("median_mgas_s", 0) for p in points]
            color = _engine_color(engine)

            ax_left.plot(xs, tps, "o-", color=color, label=f"{engine} TPS")
            ax_right.plot(xs, mgas, "s--", color=color, alpha=0.6, label=f"{engine} MGas/s")

            # Annotate peak TPS
            if tps:
                peak_idx = int(np.argmax(tps))
                ax_left.annotate(
                    f"{tps[peak_idx]:.0f}",
                    xy=(xs[peak_idx], tps[peak_idx]),
                    textcoords="offset points",
                    xytext=(0, 10),
                    ha="center",
                    fontsize=8,
                    fontweight="bold",
                    color=color,
                )

        ax_left.set_xlabel("txs_per_block")
        ax_left.set_ylabel("TPS")
        ax_right.set_ylabel("MGas/s")
        ax_left.set_title(wl)
        # Merge legends from both axes
        lines_l, labels_l = ax_left.get_legend_handles_labels()
        lines_r, labels_r = ax_right.get_legend_handles_labels()
        ax_left.legend(lines_l + lines_r, labels_l + labels_r, loc="best", fontsize=8)

    fig.suptitle("Sweep: TPS & MGas/s vs txs_per_block", fontsize=14)
    _save(fig, output_dir, "sweep-tps-curve.png")


# ---------------------------------------------------------------------------
# Chart 2: Latency CDF
# ---------------------------------------------------------------------------

def chart_cdf(input_dir: Path, output_dir: Path):
    """Latency CDF of assemble_ms, one subplot per mode (exec / e2e)."""
    records = _load_all_jsonl(input_dir)
    modes_of_interest = ("exec", "e2e")

    # Group assemble_ms by mode
    by_mode: dict[str, list[float]] = defaultdict(list)
    for r in records:
        mode = r.get("mode", "exec")  # default to exec for older files
        if mode not in modes_of_interest:
            continue
        val = r.get("assemble_ms")
        if val is not None:
            by_mode[mode].append(float(val))

    active_modes = [m for m in modes_of_interest if by_mode.get(m)]
    if not active_modes:
        print("  [skip] no assemble_ms data for exec/e2e modes")
        return

    n = len(active_modes)
    fig, axes = plt.subplots(1, n, figsize=(7 * n, 5), squeeze=False)

    for idx, mode in enumerate(active_modes):
        ax = axes[0][idx]
        values = np.sort(by_mode[mode])
        cdf = np.arange(1, len(values) + 1) / len(values)

        ax.plot(values, cdf, linewidth=1.5)

        # Percentile lines
        for pct, style in [(50, ":"), (95, "--"), (99, "-.")]:
            v = np.percentile(values, pct)
            ax.axvline(v, color="grey", linestyle=style, linewidth=0.8)
            ax.text(v, 0.5, f"p{pct}={v:.1f}ms", rotation=90, fontsize=7,
                    va="center", ha="right", color="grey")

        ax.set_xlabel("assemble_ms")
        ax.set_ylabel("CDF")
        ax.set_title(f"Latency CDF — {mode}")
        ax.set_ylim(0, 1.05)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Latency CDF (assemble_ms)", fontsize=14)
    _save(fig, output_dir, "latency-cdf.png")


# ---------------------------------------------------------------------------
# Chart 3: Sustained time-series
# ---------------------------------------------------------------------------

def chart_sustained(input_dir: Path, output_dir: Path):
    """Sustained mode time-series: rolling_avg_tps_100 vs cumulative_blocks."""
    records = _load_all_jsonl(input_dir)
    sustained = [r for r in records if r.get("mode") == "sustained"]
    if not sustained:
        print("  [skip] no sustained-mode records found")
        return

    # Group by workload, then by (engine, warmup_blocks)
    by_wl: dict[str, list[dict]] = defaultdict(list)
    for r in sustained:
        wl = r.get("workload", r.get("layer", "unknown"))
        by_wl[wl].append(r)

    workloads = sorted(by_wl.keys())
    n = len(workloads)
    fig, axes = plt.subplots(1, max(n, 1), figsize=(7 * max(n, 1), 5), squeeze=False)

    for idx, wl in enumerate(workloads):
        ax = axes[0][idx]
        # Group by (engine, warmup_blocks)
        series: dict[tuple, list[dict]] = defaultdict(list)
        for r in by_wl[wl]:
            key = (r.get("engine", "unknown"), r.get("warmup_blocks", 0))
            series[key].append(r)

        for (engine, warmup), recs in sorted(series.items()):
            recs_sorted = sorted(recs, key=lambda r: r.get("cumulative_blocks", 0))
            xs, ys = [], []
            for r in recs_sorted:
                x = r.get("cumulative_blocks")
                y = r.get("rolling_avg_tps_100")
                if x is not None and y is not None:
                    xs.append(x)
                    ys.append(y)

            if not xs:
                continue

            linestyle = "-" if warmup == 0 else "--"
            label = f"{engine} (warmup={warmup})"
            ax.plot(xs, ys, linestyle=linestyle, color=_engine_color(engine),
                    label=label, linewidth=1.2)

        ax.set_xlabel("cumulative_blocks")
        ax.set_ylabel("rolling_avg_tps_100")
        ax.set_title(wl)
        ax.legend(fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Sustained: Rolling Average TPS", fontsize=14)
    _save(fig, output_dir, "sustained-timeseries.png")


# ---------------------------------------------------------------------------
# Chart 4: Reth vs Geth comparison (grouped bar)
# ---------------------------------------------------------------------------

def chart_comparison(input_dir: Path, output_dir: Path):
    """Grouped bar chart: average TPS per engine, grouped by (mode, workload)."""
    records = _load_all_jsonl(input_dir)
    if not records:
        print("  [skip] no records found")
        return

    # Compute TPS per record: tx_count / (total_ms / 1000) if available
    # Group by (mode, workload, engine)
    agg: dict[tuple, list[float]] = defaultdict(list)
    for r in records:
        mode = r.get("mode", "exec")
        wl = r.get("workload", r.get("layer", "unknown"))
        engine = r.get("engine", "unknown")
        tps = r.get("tps")
        if tps is None:
            # Derive TPS from tx_count and total_ms
            tx_count = r.get("tx_count")
            total_ms = r.get("total_ms")
            if tx_count and total_ms and total_ms > 0:
                tps = tx_count / (total_ms / 1000.0)
        if tps is not None:
            agg[(mode, wl, engine)].append(float(tps))

    if not agg:
        print("  [skip] no TPS data for comparison chart")
        return

    # Build groups: (mode, workload) -> {engine: avg_tps}
    groups: dict[tuple, dict[str, float]] = defaultdict(dict)
    for (mode, wl, engine), vals in agg.items():
        groups[(mode, wl)][engine] = float(np.mean(vals))

    group_keys = sorted(groups.keys())
    engines = sorted({e for g in groups.values() for e in g})

    x = np.arange(len(group_keys))
    width = 0.35
    n_engines = len(engines)

    fig, ax = plt.subplots(figsize=(max(8, len(group_keys) * 1.5), 5))

    for i, engine in enumerate(engines):
        offsets = x + (i - (n_engines - 1) / 2) * width
        vals = [groups[k].get(engine, 0) for k in group_keys]
        ax.bar(offsets, vals, width * 0.9, label=engine, color=_engine_color(engine))

    ax.set_xticks(x)
    ax.set_xticklabels([f"{m}\n{wl}" for m, wl in group_keys], fontsize=8)
    ax.set_ylabel("Average TPS")
    ax.set_title("Reth vs Geth — Average TPS by (mode, workload)")
    ax.legend()
    ax.grid(axis="y", alpha=0.3)

    _save(fig, output_dir, "reth-vs-geth-comparison.png")


# ---------------------------------------------------------------------------
# Chart 5: Multi-sender impact
# ---------------------------------------------------------------------------

def chart_sender(input_dir: Path, output_dir: Path):
    """Multi-sender impact: TPS vs sender count for e2e mode."""
    records = _load_all_jsonl(input_dir)
    e2e = [r for r in records if r.get("mode") == "e2e"]
    if not e2e:
        print("  [skip] no e2e-mode records for sender chart")
        return

    # Group by (workload, engine, senders)
    agg: dict[tuple, list[float]] = defaultdict(list)
    for r in e2e:
        wl = r.get("workload", r.get("layer", "unknown"))
        engine = r.get("engine", "unknown")
        senders = r.get("senders")
        if senders is None:
            continue
        tps = r.get("tps")
        if tps is None:
            tx_count = r.get("tx_count")
            total_ms = r.get("total_ms")
            if tx_count and total_ms and total_ms > 0:
                tps = tx_count / (total_ms / 1000.0)
        if tps is not None:
            agg[(wl, engine, int(senders))].append(float(tps))

    if not agg:
        print("  [skip] no sender data in e2e records")
        return

    # Group by workload
    workloads: dict[str, dict[str, dict[int, float]]] = defaultdict(lambda: defaultdict(dict))
    for (wl, engine, senders), vals in agg.items():
        workloads[wl][engine][senders] = float(np.mean(vals))

    wl_keys = sorted(workloads.keys())
    n = len(wl_keys)
    fig, axes = plt.subplots(1, max(n, 1), figsize=(7 * max(n, 1), 5), squeeze=False)

    for idx, wl in enumerate(wl_keys):
        ax = axes[0][idx]
        for engine in ("reth", "geth"):
            if engine not in workloads[wl]:
                continue
            sender_data = workloads[wl][engine]
            xs = sorted(sender_data.keys())
            ys = [sender_data[s] for s in xs]
            ax.plot(xs, ys, "o-", color=_engine_color(engine), label=engine)

        ax.set_xscale("log")
        ax.set_xlabel("Sender count")
        ax.set_ylabel("Average TPS")
        ax.set_title(wl)
        ax.legend(fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Multi-Sender Impact on TPS (e2e mode)", fontsize=14)
    _save(fig, output_dir, "multi-sender-impact.png")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

CHART_FUNCS = {
    "sweep": chart_sweep,
    "cdf": chart_cdf,
    "sustained": chart_sustained,
    "comparison": chart_comparison,
    "sender": chart_sender,
}


def main():
    parser = argparse.ArgumentParser(description="Generate benchmark charts")
    parser.add_argument("--input", required=True, help="Results directory")
    parser.add_argument("--output", required=True, help="Output directory for PNG files")
    parser.add_argument(
        "--type",
        choices=["sweep", "cdf", "sustained", "comparison", "sender", "all"],
        default="all",
    )
    args = parser.parse_args()

    input_dir = Path(args.input)
    output_dir = Path(args.output)

    if not input_dir.is_dir():
        print(f"Error: input directory does not exist: {input_dir}", file=sys.stderr)
        sys.exit(1)

    output_dir.mkdir(parents=True, exist_ok=True)

    chart_type = args.type
    if chart_type == "all":
        targets = list(CHART_FUNCS.keys())
    else:
        targets = [chart_type]

    for name in targets:
        print(f"[{name}]")
        CHART_FUNCS[name](input_dir, output_dir)

    print("Done.")


if __name__ == "__main__":
    main()
