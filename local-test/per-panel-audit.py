#!/usr/bin/env python3
"""
Per-panel audit: for each of the 121 panels, classify
  layout   — gridPos position / alignment within its row
  data     — does Prometheus return non-empty for the (substituted) expr
  curve    — qualitative shape (has data / flat zero / "No data" expected / noisy)
Produces a markdown report at local-test/per-panel-audit.md.
"""
from __future__ import annotations

import json
import os
import urllib.parse
import urllib.request
from collections import defaultdict
from pathlib import Path

os.environ.setdefault("NO_PROXY", "127.0.0.1,localhost")
os.environ.setdefault("no_proxy", "127.0.0.1,localhost")

PROM = "http://127.0.0.1:9090"
ROOT = Path(__file__).resolve().parents[1]
DASHBOARD = ROOT / "etc/grafana/dashboards/morph-reth.json"
OUT = ROOT / "local-test/per-panel-audit.md"

ROWS_ORDER = [
    "Overview", "Morph Engine API", "Morph Payload Builder",
    "Execution & State Root", "Database & Storage", "RPC",
    "Sync & Downloaders", "TxPool & P2P", "State & History",
    "Process & Maintenance",
]

VAR_SUBS = {
    "$chain": "morph-devnet",
    "${chain}": "morph-devnet",
    "$service": "sequencer0-reth|full0-reth",
    "${service}": "sequencer0-reth|full0-reth",
    "$role": "sequencer|full",
    "${role}": "sequencer|full",
    "$quantile": "0.95",
    "${quantile}": "0.95",
    "$__rate_interval": "30s",
    "${__rate_interval}": "30s",
    "$__interval": "30s",
    "${__interval}": "30s",
    "$instance_label": "service",
    "${instance_label}": "service",
    "$interval": "10m",
    "${interval}": "10m",
}


def substitute(expr: str) -> str:
    out = expr
    for k, v in VAR_SUBS.items():
        out = out.replace(k, v)
    # $instance with All-mode expansion (regex `.+`)
    out = out.replace("${instance}", ".+").replace("$instance", ".+")
    return out


def prom_query(expr: str) -> dict:
    url = f"{PROM}/api/v1/query?query={urllib.parse.quote(expr)}"
    try:
        return json.loads(urllib.request.urlopen(url, timeout=10).read())
    except Exception as e:  # noqa: BLE001
        return {"status": "error", "error": str(e)[:100]}


def classify_panel(panel: dict) -> tuple[str, int, str]:
    """Return (status, n_series, note)."""
    targets = panel.get("targets", [])
    if not targets:
        return ("NO_TARGET", 0, "panel has no targets")
    seen_series = 0
    any_ok = False
    errs = []
    for t in targets:
        expr = t.get("expr") or t.get("query") or ""
        if not expr.strip():
            continue
        sub = substitute(expr)
        r = prom_query(sub)
        if r.get("status") != "success":
            errs.append(r.get("error") or r.get("errorType") or "unknown")
            continue
        result = r.get("data", {}).get("result", [])
        if result:
            any_ok = True
            seen_series += len(result)
    if errs and not any_ok:
        return ("ERROR", 0, " ".join(errs)[:80])
    if any_ok:
        return ("OK", seen_series, f"{seen_series} series")
    return ("EMPTY", 0, "no series")


def iter_panels_with_row(dashboard: dict):
    current = None
    for p in dashboard["panels"]:
        if p.get("type") == "row":
            current = p.get("title")
            for c in p.get("panels", []):
                if c.get("type") != "row":
                    yield (current, c)
            continue
        if current is not None:
            yield (current, p)


def check_row_layout(panels: list[dict]) -> list[str]:
    """Return layout issues for a given row's panels."""
    issues = []
    # Group by y
    rows_of_panels = defaultdict(list)
    for p in panels:
        g = p["gridPos"]
        rows_of_panels[g["y"]].append(p)
    for y in sorted(rows_of_panels):
        ps = sorted(rows_of_panels[y], key=lambda p: p["gridPos"]["x"])
        widths = [p["gridPos"]["w"] for p in ps]
        total_w = sum(widths)
        if total_w != 24:
            titles = [p["title"] for p in ps]
            issues.append(f"y={y}: {'+'.join(str(w) for w in widths)}={total_w} (missing {24-total_w}), panels={titles}")
    return issues


def main():
    dash = json.loads(DASHBOARD.read_text())
    panels_by_row = defaultdict(list)
    for row, p in iter_panels_with_row(dash):
        panels_by_row[row].append(p)

    out = []
    out.append("# Per-Panel Audit Report")
    out.append(f"\nTotal panels: {sum(len(v) for v in panels_by_row.values())}\n")

    summary_ok = 0
    summary_empty = 0
    summary_error = 0
    all_layout_issues = {}

    for row in ROWS_ORDER:
        panels = panels_by_row.get(row, [])
        panels.sort(key=lambda p: (p["gridPos"]["y"], p["gridPos"]["x"]))
        out.append(f"\n## {row} ({len(panels)})")

        layout_issues = check_row_layout(panels)
        if layout_issues:
            all_layout_issues[row] = layout_issues
            out.append("\n**Layout issues**:")
            for issue in layout_issues:
                out.append(f"- ⚠️ {issue}")
        else:
            out.append("\nLayout: ✅ all rows fill 24 columns cleanly")

        out.append("\n| ID | Title | Type | Pos | Size | Status | Note |")
        out.append("|---:|---|---|---|---|---|---|")
        for p in panels:
            g = p["gridPos"]
            status, n, note = classify_panel(p)
            if status == "OK":
                summary_ok += 1
                icon = "✅"
            elif status == "EMPTY":
                summary_empty += 1
                icon = "⭕"
            else:
                summary_error += 1
                icon = "❌"
            out.append(
                f"| {p['id']} | {p['title']} | {p['type']} | "
                f"x={g['x']},y={g['y']} | {g['w']}×{g['h']} | "
                f"{icon} {status} | {note} |"
            )

    out.insert(2, f"\n**Data status**: OK {summary_ok} / Empty {summary_empty} / Error {summary_error}\n")

    if all_layout_issues:
        out.insert(3, "\n**Rows with layout gaps**: " + ", ".join(all_layout_issues.keys()) + "\n")

    OUT.write_text("\n".join(out) + "\n")
    print("\n".join(out))
    print(f"\n→ written to {OUT}")


if __name__ == "__main__":
    main()
