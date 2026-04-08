#!/usr/bin/env python3
"""Analyze openloop benchmark results with proper statistical methodology."""

import json
import sys
import statistics
from pathlib import Path


def load_blocks(path: str) -> list[dict]:
    return [json.loads(line) for line in open(path)]


def analyze(path: str, warmup_pct: float = 0.10) -> dict:
    blocks = load_blocks(path)
    if not blocks:
        return {}

    nonempty = [b for b in blocks if b["tx_count"] > 0]
    if not nonempty:
        return {}

    # Discard warmup (first N% of non-empty blocks)
    warmup_count = int(len(nonempty) * warmup_pct)
    measured = nonempty[warmup_count:]
    if len(measured) < 10:
        measured = nonempty  # fallback if too few blocks

    total_txs = sum(b["tx_count"] for b in measured)
    total_gas = sum(b["gas_used"] for b in measured)
    # Wall-clock: sum of total_ms across measured blocks
    wall_ms = sum(b["total_ms"] for b in measured)
    wall_s = wall_ms / 1000.0

    # Sustained throughput (the key metric)
    sustained_tps = total_txs / wall_s if wall_s > 0 else 0
    sustained_mgas = total_gas / wall_s / 1e6 if wall_s > 0 else 0

    # Per-block TPS for percentiles (only blocks with >= 100 txs to avoid noise)
    big_block_tps = [b["tps"] for b in measured if b["tx_count"] >= 100]
    if not big_block_tps:
        big_block_tps = [b["tps"] for b in measured if b["tx_count"] > 0]

    # Assemble time analysis
    asm_times = [b["assemble_ms"] for b in measured if b["tx_count"] > 0]
    import_times = [b["import_ms"] for b in measured if b["tx_count"] > 0]

    # Per-tx cost (from large blocks only)
    large = [b for b in measured if b["tx_count"] >= 1000]
    us_per_tx = (
        [b["assemble_ms"] / b["tx_count"] * 1000 for b in large] if large else []
    )

    result = {
        "file": Path(path).name,
        "engine": measured[0].get("engine", "unknown"),
        "workload": measured[0].get("workload", "unknown"),
        "total_blocks": len(blocks),
        "measured_blocks": len(measured),
        "warmup_discarded": warmup_count,
        "total_txs": total_txs,
        "total_gas": total_gas,
        "wall_s": wall_s,
        # Key metrics
        "sustained_tps": sustained_tps,
        "sustained_mgas_s": sustained_mgas,
        # Block TPS percentiles
        "peak_tps": max(big_block_tps) if big_block_tps else 0,
        "p50_tps": statistics.median(big_block_tps) if big_block_tps else 0,
        "p95_tps": (
            sorted(big_block_tps)[int(len(big_block_tps) * 0.95)]
            if big_block_tps
            else 0
        ),
        "p99_tps": (
            sorted(big_block_tps)[int(len(big_block_tps) * 0.99)]
            if big_block_tps
            else 0
        ),
        # Block size stats
        "avg_txs_block": total_txs / len(measured) if measured else 0,
        "max_txs_block": max(b["tx_count"] for b in measured),
        # Timing
        "avg_assemble_ms": statistics.mean(asm_times) if asm_times else 0,
        "avg_import_ms": statistics.mean(import_times) if import_times else 0,
        # Per-tx cost
        "avg_us_per_tx": statistics.mean(us_per_tx) if us_per_tx else 0,
    }
    return result


def print_report(results: list[dict]):
    print("=" * 100)
    print("OPENLOOP BENCHMARK REPORT")
    print("=" * 100)
    print()

    # Group by engine+workload for multi-run aggregation
    groups: dict[str, list[dict]] = {}
    for r in results:
        key = f"{r['engine']}|{r['workload']}"
        groups.setdefault(key, []).append(r)

    header = f"{'Engine':>6} {'Workload':>15} {'Runs':>4} {'Sustained TPS':>15} {'Mgas/s':>10} {'Peak TPS':>10} {'P50 TPS':>10} {'us/tx':>7}"
    print(header)
    print("-" * len(header))

    summary_rows = []
    for key, runs in sorted(groups.items()):
        engine, workload = key.split("|")
        n = len(runs)
        tps_vals = [r["sustained_tps"] for r in runs]
        mgas_vals = [r["sustained_mgas_s"] for r in runs]
        peak_vals = [r["peak_tps"] for r in runs]
        p50_vals = [r["p50_tps"] for r in runs]
        us_vals = [r["avg_us_per_tx"] for r in runs]

        mean_tps = statistics.mean(tps_vals)
        std_tps = statistics.stdev(tps_vals) if n > 1 else 0
        mean_mgas = statistics.mean(mgas_vals)
        mean_peak = statistics.mean(peak_vals)
        mean_p50 = statistics.mean(p50_vals)
        mean_us = statistics.mean(us_vals) if us_vals[0] > 0 else 0

        tps_str = f"{mean_tps:,.0f}" + (f" +/-{std_tps:,.0f}" if n > 1 else "")
        print(
            f"{engine:>6} {workload:>15} {n:>4} {tps_str:>15} {mean_mgas:>10,.0f} {mean_peak:>10,.0f} {mean_p50:>10,.0f} {mean_us:>6.1f}"
        )
        summary_rows.append(
            (engine, workload, mean_tps, std_tps, mean_mgas, mean_peak, mean_us)
        )

    # Cross-engine comparison
    print()
    print("=== Cross-engine comparison ===")
    workloads = set(r["workload"] for r in results)
    for wl in sorted(workloads):
        reth_runs = [r for r in results if r["engine"] == "reth" and r["workload"] == wl]
        geth_runs = [r for r in results if r["engine"] == "geth" and r["workload"] == wl]
        if reth_runs and geth_runs:
            reth_tps = statistics.mean(r["sustained_tps"] for r in reth_runs)
            geth_tps = statistics.mean(r["sustained_tps"] for r in geth_runs)
            ratio = reth_tps / geth_tps if geth_tps > 0 else float("inf")
            print(f"  {wl}: reth {reth_tps:,.0f} vs geth {geth_tps:,.0f} → reth {ratio:.1f}x faster")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <result1.jsonl> [result2.jsonl ...]")
        sys.exit(1)

    results = []
    for path in sys.argv[1:]:
        r = analyze(path)
        if r:
            results.append(r)

    print_report(results)
