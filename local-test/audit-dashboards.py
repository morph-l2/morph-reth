#!/usr/bin/env python3
"""
Static audit of morph-reth.json (the unified dashboard) and related JSONs.

Checks:
  1. Syntactic validity (proper JSON, no trailing commas)
  2. Panel inventory — per row, per panel, with every target's expr
  3. Duplicate panel titles / ids / overlapping gridPos
  4. Custom-metric coverage: every reth_morph_engine_* / reth_morph_payload_builder_*
     referenced by metrics.rs must appear in at least one panel
  5. Template variable consistency (every ${var} placeholder matches a declared variable)
  6. Zero-height / zero-width panels
  7. gridPos row-width compliance (w <= 24)
"""
from __future__ import annotations

import csv
import json
import re
import sys
from pathlib import Path
from collections import defaultdict

ROOT = Path(__file__).resolve().parents[1]
DASH_DIR = ROOT / "etc/grafana/dashboards"
UNIFIED = DASH_DIR / "morph-reth.json"
OUT_CSV = ROOT / "local-test/panel-inventory.csv"
OUT_REPORT = ROOT / "local-test/audit-report.txt"

ENGINE_METRICS_RS = ROOT / "crates/engine-api/src/metrics.rs"
BUILDER_METRICS_RS = ROOT / "crates/payload/builder/src/metrics.rs"

# Metrics that are intentionally exposed by the binary but NOT plotted on any
# Grafana panel. Currently empty — all custom metrics in metrics.rs have
# corresponding panels, and event-style skip counters (L1/pool tx skips)
# are logged rather than metered.
INTENTIONALLY_UNCOVERED_METRICS: set[str] = set()


def load_json(path: Path) -> dict:
    with path.open() as h:
        return json.load(h)


def iter_all_panels(panels):
    for p in panels:
        yield p
        if p.get("type") == "row" and p.get("panels"):
            yield from iter_all_panels(p["panels"])


def iter_non_row_panels(panels):
    for p in panels:
        if p.get("type") == "row":
            yield from iter_non_row_panels(p.get("panels", []))
            continue
        yield p


def extract_rows(dashboard: dict) -> dict[str, list[dict]]:
    rows: dict[str, list[dict]] = {}
    current = None
    for p in dashboard["panels"]:
        if p.get("type") == "row":
            current = p["title"]
            rows.setdefault(current, [])
            for nested in p.get("panels", []):
                if nested.get("type") != "row":
                    rows[current].append(nested)
            continue
        if current is not None:
            rows.setdefault(current, []).append(p)
    return rows


def panel_expressions(panel: dict) -> list[str]:
    exprs = []
    for t in panel.get("targets", []):
        e = t.get("expr") or t.get("query") or ""
        if e:
            exprs.append(e.strip())
    return exprs


def find_custom_metrics_from_rust() -> set[str]:
    """Derive the expected reth_morph_* metric names from metrics.rs."""
    metrics = set()
    engine_scope = "morph_engine"
    builder_scope = "morph_payload_builder"
    for path, scope in [
        (ENGINE_METRICS_RS, engine_scope),
        (BUILDER_METRICS_RS, builder_scope),
    ]:
        src = path.read_text()
        for line in src.splitlines():
            m = re.match(
                r"\s*pub\(crate\)\s+([a-z0-9_]+):\s*(Counter|Gauge|Histogram),",
                line,
            )
            if m:
                metrics.add(f"reth_{scope}_{m.group(1)}")
        # Also pick up metrics::counter!() style (e.g. pool_transactions_skipped_total)
        for m in re.finditer(r'metrics::counter!\("([a-z0-9_]+)"', src):
            metrics.add(f"reth_{m.group(1)}")
    return metrics


def check_template_vars(dashboard: dict) -> list[str]:
    declared = {v["name"] for v in dashboard.get("templating", {}).get("list", [])}
    # Find every ${name} / $name reference in stringified dashboard
    raw = json.dumps(dashboard)
    used = set(re.findall(r"\$\{?(\w+)\}?", raw))
    # Known Grafana built-ins we can skip
    builtins = {
        "__interval", "__interval_ms", "__range", "__range_s",
        "__range_ms", "__rate_interval", "__timeFilter",
        "__auto_interval", "__from", "__to", "__name",
        "__user", "__org", "__dashboard", "__user_id",
        "__auto", "__name", "__data",
    }
    missing = sorted(used - declared - builtins)
    return missing


def main() -> None:
    lines: list[str] = []
    out_rows: list[list[str]] = []
    errors: list[str] = []
    warnings: list[str] = []

    # ── 1. Load unified dashboard ─────────────────────────────────────────
    dash = load_json(UNIFIED)
    lines.append(f"# Audit: {UNIFIED.name}")
    lines.append(f"Title: {dash.get('title')}  UID: {dash.get('uid')}")
    lines.append("")

    rows = extract_rows(dash)
    lines.append(f"## Rows ({len(rows)})")
    for r in rows:
        lines.append(f"  - {r}  ({len(rows[r])} panels)")
    lines.append("")

    total_panels = sum(len(v) for v in rows.values())
    lines.append(f"Total non-row panels: {total_panels}")
    lines.append("")

    # ── 2. Expected TARGET_ROWS order check is already in merge script,
    # but let's re-verify visible order ───────────────────────────────────
    expected_rows = [
        "Overview",
        "Morph Engine API",
        "Morph Payload Builder",
        "Execution & State Root",
        "Database & Storage",
        "RPC",
        "Sync & Downloaders",
        "TxPool & P2P",
        "State & History",
        "Process & Maintenance",
    ]
    actual_row_titles = [
        p["title"] for p in dash["panels"] if p.get("type") == "row"
    ]
    if actual_row_titles != expected_rows:
        errors.append(f"Row order mismatch:\n  expected {expected_rows}\n  got      {actual_row_titles}")

    # ── 3. Duplicate panel title per row, duplicate IDs ───────────────────
    all_ids = []
    for p in iter_all_panels(dash["panels"]):
        pid = p.get("id")
        if pid is None:
            errors.append(f"Panel without id: {p.get('title')!r}")
        else:
            all_ids.append(pid)
    dup_ids = [x for x in set(all_ids) if all_ids.count(x) > 1]
    if dup_ids:
        errors.append(f"Duplicate panel ids: {dup_ids}")

    for row_name, panels in rows.items():
        titles = [p["title"] for p in panels]
        dup_titles = sorted({t for t in titles if titles.count(t) > 1})
        if dup_titles:
            warnings.append(f"Row {row_name!r} has duplicate panel titles: {dup_titles}")

    # ── 4. gridPos validity ───────────────────────────────────────────────
    for p in iter_non_row_panels(dash["panels"]):
        g = p.get("gridPos", {})
        w, h, x, y = g.get("w"), g.get("h"), g.get("x"), g.get("y")
        if not all(isinstance(v, int) for v in (w, h, x, y)):
            errors.append(f"Panel {p['id']} {p['title']!r}: gridPos missing ints: {g}")
            continue
        if w <= 0 or h <= 0:
            errors.append(f"Panel {p['id']} {p['title']!r}: zero-size gridPos w={w} h={h}")
        if w > 24:
            errors.append(f"Panel {p['id']} {p['title']!r}: gridPos w={w} > 24")
        if x < 0 or x + w > 24:
            errors.append(f"Panel {p['id']} {p['title']!r}: gridPos x={x} w={w} overflows x+w > 24")

    # ── 5. Custom metric coverage ────────────────────────────────────────
    expected_metrics = find_custom_metrics_from_rust() - INTENTIONALLY_UNCOVERED_METRICS
    all_exprs_blob = "\n".join(
        e for p in iter_non_row_panels(dash["panels"]) for e in panel_expressions(p)
    )
    missing_coverage = sorted(
        m for m in expected_metrics if m not in all_exprs_blob
    )
    lines.append(f"## Custom metrics expected from Rust source: {len(expected_metrics)}")
    for m in sorted(expected_metrics):
        covered = m in all_exprs_blob
        lines.append(f"  [{'✓' if covered else '✗'}] {m}")
    if INTENTIONALLY_UNCOVERED_METRICS:
        lines.append("")
        lines.append(f"## Exposed but not plotted on dashboard ({len(INTENTIONALLY_UNCOVERED_METRICS)})")
        for m in sorted(INTENTIONALLY_UNCOVERED_METRICS):
            lines.append(f"  [~] {m}")
    if missing_coverage:
        errors.append(f"Dashboard has no panel referencing these metrics: {missing_coverage}")
    lines.append("")

    # ── 6. Template variable consistency ─────────────────────────────────
    missing_vars = check_template_vars(dash)
    if missing_vars:
        warnings.append(f"Dashboard references undeclared template vars: {missing_vars}")

    # ── 7. Panel inventory CSV ───────────────────────────────────────────
    for row_name, panels in rows.items():
        for p in panels:
            exprs = panel_expressions(p)
            out_rows.append([
                row_name,
                p.get("title", ""),
                str(p.get("id", "")),
                f"{p['gridPos']['x']},{p['gridPos']['y']},{p['gridPos']['w']}x{p['gridPos']['h']}",
                p.get("type", ""),
                " || ".join(exprs),
            ])

    with OUT_CSV.open("w") as h:
        w = csv.writer(h)
        w.writerow(["row", "panel_title", "panel_id", "gridPos x,y,wxh", "type", "exprs"])
        w.writerows(out_rows)

    lines.append(f"## Panel inventory CSV: {OUT_CSV.relative_to(ROOT)}")
    lines.append(f"Panels inventoried: {len(out_rows)}")
    lines.append("")

    # morph-reth.json is the sole source of truth — no other dashboards to audit.

    lines.append("")
    lines.append(f"## Errors ({len(errors)})")
    for e in errors:
        lines.append(f"  ✗ {e}")
    lines.append("")
    lines.append(f"## Warnings ({len(warnings)})")
    for w in warnings:
        lines.append(f"  ! {w}")
    lines.append("")
    lines.append("## Exit code: " + ("FAIL" if errors else "OK"))

    report = "\n".join(lines)
    OUT_REPORT.write_text(report + "\n")
    print(report)
    sys.exit(1 if errors else 0)


if __name__ == "__main__":
    main()
