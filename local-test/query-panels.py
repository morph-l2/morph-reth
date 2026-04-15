#!/usr/bin/env python3
"""
Layer 2 verification: iterate every panel's PromQL expression and query
Prometheus. Classify results as:
  OK           — at least one series with a non-NaN numeric sample
  EMPTY        — query succeeds, zero series
  ERROR        — PromQL parse/execute error
  SKIP         — trivially non-numeric (annotations, constants)

Writes /tmp/panel-query-report.csv and prints a summary table.
"""
from __future__ import annotations

import csv
import json
import os
import sys
import urllib.request
import urllib.error
import urllib.parse
from pathlib import Path

# Bypass system proxy for localhost
os.environ.setdefault("NO_PROXY", "127.0.0.1,localhost")
os.environ.setdefault("no_proxy", "127.0.0.1,localhost")

PROM = os.environ.get("PROMETHEUS", "http://127.0.0.1:9090")
ROOT = Path(__file__).resolve().parents[1]
INVENTORY_JSON = ROOT / "etc/grafana/dashboards/morph-reth.json"
REPORT = ROOT / "local-test/panel-query-report.csv"
SUMMARY = ROOT / "local-test/panel-query-summary.txt"

VAR_REPLACEMENTS = {
    "$chain": "morph-devnet",
    "${chain}": "morph-devnet",
    "$service": "sequencer0-reth|full0-reth|full1-reth",
    "${service}": "sequencer0-reth|full0-reth|full1-reth",
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

# Panels use two styles:
#   exact:    {service="$instance"}    → becomes {service="sequencer0-reth"}
#   regex:    {service=~"$service"}    → becomes {service=~"sequencer0-reth|..."}
# The regex forms are handled by VAR_REPLACEMENTS. The exact forms need a
# concrete service value, so we substitute $instance AFTER checking context.


def substitute_vars(expr: str) -> str:
    out = expr
    for k, v in VAR_REPLACEMENTS.items():
        out = out.replace(k, v)
    # Substitute $instance last — it's only used in exact-match panels.
    # Use sequencer0-reth because that's the node producing blocks.
    out = out.replace("${instance}", "sequencer0-reth")
    out = out.replace("$instance", "sequencer0-reth")
    return out


def iter_non_row_panels(panels):
    for p in panels:
        if p.get("type") == "row":
            yield from iter_non_row_panels(p.get("panels", []))
            continue
        yield p


def extract_row_of_panel(dashboard):
    """Yield (row_title, panel_dict) for every non-row panel in order."""
    rows = []
    current = None
    for p in dashboard["panels"]:
        if p.get("type") == "row":
            current = p["title"]
            for nested in p.get("panels", []):
                if nested.get("type") != "row":
                    yield (current, nested)
            continue
        if current is not None:
            yield (current, p)


def query_prometheus(expr: str) -> dict:
    url = f"{PROM}/api/v1/query?query={urllib.parse.quote(expr)}"
    try:
        resp = urllib.request.urlopen(url, timeout=10)
        return json.loads(resp.read())
    except urllib.error.HTTPError as e:
        body = e.read().decode()[:300]
        return {"status": "http_error", "code": e.code, "body": body}
    except Exception as e:  # noqa: BLE001
        return {"status": "exception", "message": str(e)[:200]}


def classify(expr: str, raw: str) -> tuple[str, int, str]:
    """Return (status, n_series, note)."""
    sub = substitute_vars(raw)
    res = query_prometheus(sub)

    if res.get("status") == "http_error":
        return ("ERROR", 0, f"HTTP {res.get('code')} {res.get('body','')[:100]}")
    if res.get("status") == "exception":
        return ("ERROR", 0, f"exception: {res.get('message', '')}")
    if res.get("status") != "success":
        err = res.get("error") or res.get("errorType") or "unknown"
        return ("ERROR", 0, f"prom error: {str(err)[:150]}")

    data = res.get("data", {})
    result = data.get("result", [])
    if not result:
        return ("EMPTY", 0, "no series")

    n = len(result)
    # Check for non-NaN numeric sample
    for r in result:
        v = r.get("value")
        if v and len(v) >= 2:
            try:
                float(v[1])
                return ("OK", n, f"{n} series")
            except ValueError:
                continue
    return ("EMPTY", n, f"{n} series but all NaN")


def main():
    dash = json.loads(INVENTORY_JSON.read_text())
    rows_out = []
    stats = {"OK": 0, "EMPTY": 0, "ERROR": 0, "SKIP": 0}

    for row_title, panel in extract_row_of_panel(dash):
        pid = panel.get("id")
        ptitle = panel.get("title", "")
        ptype = panel.get("type", "")
        targets = panel.get("targets", [])
        if not targets:
            rows_out.append([row_title, ptitle, pid, ptype, "", "SKIP", 0, "no targets"])
            stats["SKIP"] += 1
            continue

        target_results = []
        aggregate_status = None
        aggregate_n = 0
        notes = []

        for t in targets:
            expr = t.get("expr") or t.get("query") or ""
            if not expr.strip():
                target_results.append(("SKIP", 0, "empty expr"))
                continue
            st, n, note = classify(expr, expr)
            target_results.append((st, n, note))
            if st == "OK":
                aggregate_status = "OK"
                aggregate_n += n
            elif st == "ERROR" and aggregate_status is None:
                aggregate_status = "ERROR"
            elif st == "EMPTY" and aggregate_status not in ("OK", "ERROR"):
                aggregate_status = "EMPTY"
            notes.append(note[:120])

        if aggregate_status is None:
            aggregate_status = "SKIP"

        stats[aggregate_status] += 1
        rows_out.append([
            row_title, ptitle, pid, ptype, len(targets),
            aggregate_status, aggregate_n, " | ".join(notes)[:400],
        ])

    # Write CSV
    with REPORT.open("w") as h:
        w = csv.writer(h)
        w.writerow(["row", "panel", "id", "type", "n_targets", "status", "n_series", "notes"])
        w.writerows(rows_out)

    # Summary
    lines = []
    lines.append(f"Total panels queried: {sum(stats.values())}")
    for s in ["OK", "EMPTY", "ERROR", "SKIP"]:
        lines.append(f"  {s:6s}: {stats[s]}")
    lines.append("")
    lines.append("## Errors")
    for r in rows_out:
        if r[5] == "ERROR":
            lines.append(f"  [{r[2]:>4}] {r[0]!r} / {r[1]!r}: {r[7][:200]}")
    lines.append("")
    lines.append("## Empty panels by row")
    by_row = {}
    for r in rows_out:
        if r[5] == "EMPTY":
            by_row.setdefault(r[0], []).append(f"{r[1]!r}")
    for row, panels in by_row.items():
        lines.append(f"  {row}: {len(panels)} empty")
        for p in panels[:10]:
            lines.append(f"    - {p}")
        if len(panels) > 10:
            lines.append(f"    ... and {len(panels) - 10} more")
    lines.append("")
    lines.append("## OK breakdown by row")
    ok_by_row = {}
    total_by_row = {}
    for r in rows_out:
        total_by_row[r[0]] = total_by_row.get(r[0], 0) + 1
        if r[5] == "OK":
            ok_by_row[r[0]] = ok_by_row.get(r[0], 0) + 1
    for row in total_by_row:
        ok = ok_by_row.get(row, 0)
        tot = total_by_row[row]
        lines.append(f"  {row}: {ok}/{tot}")

    summary = "\n".join(lines)
    SUMMARY.write_text(summary + "\n")
    print(summary)
    print(f"\nReport: {REPORT}")


if __name__ == "__main__":
    main()
