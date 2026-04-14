#!/usr/bin/env python3
"""
Build a single Morph-Reth unified dashboard using domain-first grouping.

Rules:
- no row-level instance repetition
- unique top-level rows only
- preserve source panel queries where possible
- rename same-row panels for clarity
- remove only true duplicates
"""

from __future__ import annotations

import copy
import json
from dataclasses import dataclass
from pathlib import Path

GRID_WIDTH = 24
ROW_HEIGHT = 1
UNIFIED_DATASOURCE_UID = "${datasource}"
LEGACY_DATASOURCE_UID = "${DS_PROMETHEUS}"

WORKTREE_ROOT = Path(__file__).resolve().parents[1]
DASHBOARDS_DIR = WORKTREE_ROOT / "etc/grafana/dashboards"
OUTPUT_PATH = DASHBOARDS_DIR / "morph-reth.json"

TARGET_ROWS = [
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

EXPANDED_ROWS = {
    "Overview",
    "Morph Engine API",
    "Morph Payload Builder",
    "Execution & State Root",
}


@dataclass(frozen=True)
class RowSource:
    dashboard: str
    row_title: str
    target_row: str


ROW_SOURCES = [
    RowSource("overview.json", "Overview", "Overview"),
    RowSource("morph-engine.json", "Overview", "Overview"),
    RowSource("morph-engine.json", "Engine API Latency", "Morph Engine API"),
    RowSource("morph-engine.json", "Engine API Failures", "Morph Engine API"),
    RowSource("morph-engine.json", "Payload Builder", "Morph Payload Builder"),
    RowSource(
        "morph-engine.json",
        "L1 Message & Pool Tx Processing",
        "Morph Payload Builder",
    ),
    RowSource("overview.json", "Payload Builder", "Morph Payload Builder"),
    RowSource("overview.json", "Execution", "Execution & State Root"),
    RowSource("overview.json", "State Root Task", "Execution & State Root"),
    RowSource("overview.json", "Database", "Database & Storage"),
    RowSource("overview.json", "Static Files", "Database & Storage"),
    RowSource("overview.json", "Blockchain Tree", "Database & Storage"),
    RowSource("overview.json", "RPC server", "RPC"),
    RowSource("overview.json", "Downloader: Headers", "Sync & Downloaders"),
    RowSource("overview.json", "Downloader: Bodies", "Sync & Downloaders"),
    RowSource("overview.json", "Eth Requests", "Sync & Downloaders"),
    RowSource("reth-mempool.json", "Transaction Pool", "TxPool & P2P"),
    RowSource("reth-mempool.json", "Networking", "TxPool & P2P"),
    RowSource("reth-discovery.json", "Discv5", "TxPool & P2P"),
    RowSource("reth-state-growth.json", "State", "State & History"),
    RowSource("reth-state-growth.json", "History", "State & History"),
    RowSource("overview.json", "Process", "Process & Maintenance"),
    RowSource("overview.json", "Pruning", "Process & Maintenance"),
]

PANEL_RENAMES = {
    ("morph-engine.json", "Payload Builder", "Payload Build Duration"): (
        "Morph Payload Build Duration"
    ),
    (
        "morph-engine.json",
        "Payload Builder",
        "Tx Execution Duration (commit_txs_all)",
    ): "Morph Tx Execution Duration",
    ("morph-engine.json", "Payload Builder", "Per-Tx EVM Apply Duration"): (
        "Morph Per-Tx Apply Duration"
    ),
    ("morph-engine.json", "Payload Builder", "Block Transactions (last built)"): (
        "Morph Block Transactions"
    ),
    (
        "morph-engine.json",
        "L1 Message & Pool Tx Processing",
        "L1 Tx Skip Counters",
    ): "Morph L1 Tx Skip Counters",
    (
        "morph-engine.json",
        "L1 Message & Pool Tx Processing",
        "Pool Tx Skipped By Role",
    ): "Morph Pool Tx Skipped By Role",
    ("overview.json", "Payload Builder", "Active Jobs"): "Reth Builder Active Jobs",
    ("overview.json", "Payload Builder", "Initiated Jobs"): (
        "Reth Builder Initiated Jobs"
    ),
    ("overview.json", "Payload Builder", "Failed Jobs"): "Reth Builder Failed Jobs",
    ("overview.json", "Downloader: Headers", "I/O"): "Headers I/O",
    ("overview.json", "Downloader: Headers", "Errors"): "Headers Errors",
    ("overview.json", "Downloader: Headers", "Requests"): "Headers Requests",
    ("overview.json", "Downloader: Bodies", "I/O"): "Bodies I/O",
    ("overview.json", "Downloader: Bodies", "Errors"): "Bodies Errors",
    ("overview.json", "Downloader: Bodies", "Requests"): "Bodies Requests",
    ("overview.json", "Database", "Average commit time"): "DB Average Commit Time",
    ("overview.json", "Database", "Commit time heatmap"): "DB Commit Time Heatmap",
    ("overview.json", "Database", "Average transaction open time"): (
        "DB Average Transaction Open Time"
    ),
    ("overview.json", "Database", "Max transaction open time"): (
        "DB Max Transaction Open Time"
    ),
    ("overview.json", "Database", "Number of read-write transactions"): (
        "DB Read-Write Transactions"
    ),
    ("overview.json", "Database", "Number of read-only transactions"): (
        "DB Read-Only Transactions"
    ),
    ("overview.json", "Database", "Database tables"): "DB Tables",
    ("overview.json", "Database", "Max insertion operation time"): (
        "DB Max Insertion Operation Time"
    ),
    ("overview.json", "Database", "Database pages"): "DB Pages",
    ("overview.json", "Database", "Database growth"): "DB Growth",
    ("overview.json", "Database", "Freelist"): "DB Freelist",
    ("overview.json", "Database", "Overflow pages by table"): (
        "DB Overflow Pages By Table"
    ),
    ("overview.json", "Static Files", "Segments size"): "Static Files Segment Size",
    ("overview.json", "Static Files", "Entries per segment"): (
        "Static Files Entries Per Segment"
    ),
    ("overview.json", "Static Files", "Files per segment"): (
        "Static Files File Count Per Segment"
    ),
    ("overview.json", "Static Files", "Static Files growth"): (
        "Static Files Growth"
    ),
    ("overview.json", "Static Files", "Max writer commit time"): (
        "Static Files Max Writer Commit Time"
    ),
    ("overview.json", "Blockchain Tree", "Canonical chain height"): (
        "Chain Tree Canonical Height"
    ),
    ("overview.json", "Blockchain Tree", "Block buffer blocks"): (
        "Chain Tree Block Buffer"
    ),
    ("overview.json", "Blockchain Tree", "Reorgs"): "Chain Tree Reorgs",
    ("overview.json", "Blockchain Tree", "Latest Reorg Depth"): (
        "Chain Tree Latest Reorg Depth"
    ),
    ("reth-mempool.json", "Transaction Pool", "Blob store"): "TxPool Blob Store",
    ("reth-mempool.json", "Networking", "Connections"): "P2P Connections",
    ("reth-mempool.json", "Networking", "Peer Disconnect Reasons"): (
        "P2P Peer Disconnect Reasons"
    ),
    ("reth-mempool.json", "Networking", "Total Dial Success"): (
        "P2P Total Dial Success"
    ),
    ("reth-discovery.json", "Discv5", "Peers"): "Discv5 Peers",
    ("reth-discovery.json", "Discv5", "Peer Churn"): "Discv5 Peer Churn",
    ("reth-discovery.json", "Discv5", "Advertised Network Stacks"): (
        "Discv5 Advertised Network Stacks"
    ),
}

TRUE_DUPLICATE_PANELS = {
    ("overview.json", "Overview", "Block Processing Latency"),
}

PANEL_ALLOWLISTS = {
    # The unified Overview row keeps only the approved reth overview summary panels.
    (
        "overview.json",
        "Overview",
    ): {
        "Version",
        "Build Timestamp",
        "Git SHA",
        "Build Profile",
        "Target Triple",
        "Cargo Features",
        "Connected peers",
        "Stage checkpoints",
        "Storage size",
        "Sync progress (stage progress in %)",
        "Sync progress (stage progress as highest block number reached)",
        "Execution throughput",
    },
}

PANEL_REPEAT_KEYS = {
    "maxPerRow",
    "repeat",
    "repeatDirection",
    "repeatIteration",
    "repeatPanelId",
}


def load_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as handle:
        return json.load(handle)


def load_dashboard_template() -> dict:
    for candidate in (OUTPUT_PATH, DASHBOARDS_DIR / "overview.json"):
        if candidate.exists():
            dashboard = load_json(candidate)
            dashboard["title"] = "Morph Reth"
            dashboard["uid"] = "morph-reth-unified"
            dashboard["description"] = (
                "Morph-Reth unified: Engine API, Payload Builder, DB, P2P, TxPool, "
                "Sync, System"
            )
            normalize_dashboard_variables(dashboard)
            ensure_interval_variable(dashboard)
            dashboard["panels"] = []
            return dashboard
    raise SystemExit("no dashboard template found")


def extract_rows(dashboard: dict) -> dict[str, list[dict]]:
    rows: dict[str, list[dict]] = {}
    current_row: str | None = None
    for panel in dashboard["panels"]:
        if panel.get("type") == "row":
            current_row = panel["title"]
            rows.setdefault(current_row, [])
            for nested_panel in panel.get("panels", []):
                if nested_panel.get("type") != "row":
                    rows[current_row].append(nested_panel)
            continue

        if current_row is not None:
            rows.setdefault(current_row, []).append(panel)

    return rows


def normalize_dashboard_variables(dashboard: dict) -> None:
    templating = dashboard.get("templating", {}).get("list", [])
    for variable in templating:
        if variable.get("name") != "instance":
            continue

        # Most overview/reth panels use exact `label="$instance"` matching, so
        # allowing multi-select or All produces empty queries like `service=".*"`.
        variable["includeAll"] = False
        variable["multi"] = False
        variable.pop("allValue", None)

        current = variable.get("current")
        if current and current.get("value") in {"$__all", ".*"}:
            variable.pop("current", None)
        return

    raise SystemExit("dashboard template is missing the 'instance' variable")


_INTERVAL_VARIABLE = {
    "current": {"selected": True, "text": "10m", "value": "10m"},
    "hide": 0,
    "includeAll": False,
    "label": "Interval",
    "multi": False,
    "name": "interval",
    "options": [
        {"selected": False, "text": "5m",  "value": "5m"},
        {"selected": True,  "text": "10m", "value": "10m"},
        {"selected": False, "text": "30m", "value": "30m"},
        {"selected": False, "text": "1h",  "value": "1h"},
        {"selected": False, "text": "6h",  "value": "6h"},
        {"selected": False, "text": "12h", "value": "12h"},
        {"selected": False, "text": "1d",  "value": "1d"},
        {"selected": False, "text": "7d",  "value": "7d"},
        {"selected": False, "text": "14d", "value": "14d"},
        {"selected": False, "text": "30d", "value": "30d"},
    ],
    "query": "5m,10m,30m,1h,6h,12h,1d,7d,14d,30d",
    "type": "custom",
}


def ensure_interval_variable(dashboard: dict) -> None:
    """Add $interval variable used by state-growth panels if not present."""
    templating = dashboard.setdefault("templating", {}).setdefault("list", [])
    if any(v.get("name") == "interval" for v in templating):
        return
    templating.append(copy.deepcopy(_INTERVAL_VARIABLE))


def rename_panel(source: RowSource, title: str) -> str:
    return PANEL_RENAMES.get((source.dashboard, source.row_title, title), title)


def normalize_panel_string(value: str) -> str:
    return (
        value.replace(LEGACY_DATASOURCE_UID, UNIFIED_DATASOURCE_UID)
        .replace('instance=~"$instance"', '$instance_label=~"$instance"')
        .replace('instance="$instance"', '$instance_label="$instance"')
    )


def normalize_panel_references(value):
    if isinstance(value, dict):
        for key, child in value.items():
            if isinstance(child, str):
                value[key] = normalize_panel_string(child)
            else:
                normalize_panel_references(child)
        return

    if isinstance(value, list):
        for index, child in enumerate(value):
            if isinstance(child, str):
                value[index] = normalize_panel_string(child)
            else:
                normalize_panel_references(child)


def prepare_panel(source: RowSource, panel: dict) -> dict:
    panel = copy.deepcopy(panel)
    normalize_panel_references(panel)
    panel["title"] = rename_panel(source, panel["title"])
    panel.pop("collapsed", None)
    panel.pop("panels", None)
    for key in PANEL_REPEAT_KEYS:
        panel.pop(key, None)
    return panel


def should_drop_panel(source: RowSource, title: str) -> bool:
    allowlist = PANEL_ALLOWLISTS.get((source.dashboard, source.row_title))
    if allowlist is not None and title not in allowlist:
        return True

    return (source.dashboard, source.row_title, title) in TRUE_DUPLICATE_PANELS


def collect_panels() -> dict[str, list[dict]]:
    dashboards = {}
    for source in ROW_SOURCES:
        if source.dashboard not in dashboards:
            dashboard = load_json(DASHBOARDS_DIR / source.dashboard)
            dashboards[source.dashboard] = extract_rows(dashboard)

    grouped = {row_title: [] for row_title in TARGET_ROWS}
    for source in ROW_SOURCES:
        row_panels = dashboards[source.dashboard].get(source.row_title)
        if row_panels is None:
            raise SystemExit(
                f"missing row {source.row_title!r} in source dashboard {source.dashboard}"
            )

        for panel in row_panels:
            if should_drop_panel(source, panel["title"]):
                continue
            grouped[source.target_row].append(prepare_panel(source, panel))

    for row_title, panels in grouped.items():
        if not panels:
            raise SystemExit(f"target row {row_title!r} has no panels")

    return grouped


def make_row(title: str, collapsed: bool, panel_id: int, y: int) -> dict:
    return {
        "collapsed": collapsed,
        "gridPos": {"h": ROW_HEIGHT, "w": GRID_WIDTH, "x": 0, "y": y},
        "id": panel_id,
        "panels": [],
        "title": title,
        "type": "row",
    }


def layout_panels(panels: list[dict], start_y: int) -> int:
    x = 0
    y = start_y
    row_bottom = start_y

    for panel in panels:
        width = int(panel["gridPos"]["w"])
        height = int(panel["gridPos"]["h"])
        width = min(max(width, 1), GRID_WIDTH)
        height = max(height, 1)

        if x > 0 and x + width > GRID_WIDTH:
            x = 0
            y = row_bottom

        panel["gridPos"]["x"] = x
        panel["gridPos"]["y"] = y
        panel["gridPos"]["w"] = width
        panel["gridPos"]["h"] = height

        x += width
        row_bottom = max(row_bottom, y + height)

        if x >= GRID_WIDTH:
            x = 0
            y = row_bottom

    return row_bottom


def assign_ids(panels: list[dict], next_id: int) -> int:
    for panel in panels:
        panel["id"] = next_id
        next_id += 1
    return next_id


def build_panels(grouped: dict[str, list[dict]]) -> list[dict]:
    dashboard_panels: list[dict] = []
    next_id = 1
    next_y = 0

    for row_title in TARGET_ROWS:
        collapsed = row_title not in EXPANDED_ROWS
        row = make_row(row_title, collapsed=collapsed, panel_id=next_id, y=next_y)
        next_id += 1

        row_panels = copy.deepcopy(grouped[row_title])
        next_id = assign_ids(row_panels, next_id)
        bottom_y = layout_panels(row_panels, start_y=next_y + ROW_HEIGHT)

        if collapsed:
            row["panels"] = row_panels
            dashboard_panels.append(row)
        else:
            dashboard_panels.append(row)
            dashboard_panels.extend(row_panels)

        next_y = bottom_y

    return dashboard_panels


def iter_all_panels(panels: list[dict]):
    for panel in panels:
        yield panel
        if panel.get("type") == "row":
            yield from iter_all_panels(panel.get("panels", []))


def iter_non_row_panels(panels: list[dict]):
    for panel in panels:
        if panel.get("type") == "row":
            yield from iter_non_row_panels(panel.get("panels", []))
            continue
        yield panel


def assert_row_order(panels: list[dict]) -> None:
    rows = [panel["title"] for panel in panels if panel.get("type") == "row"]
    if rows != TARGET_ROWS:
        raise SystemExit(f"row order mismatch: {rows}")


def assert_unique_row_titles(panels: list[dict]) -> None:
    titles = [panel["title"] for panel in panels if panel.get("type") == "row"]
    duplicates = sorted({title for title in titles if titles.count(title) > 1})
    if duplicates:
        raise SystemExit(f"duplicate row titles: {duplicates}")


def assert_no_repeat_rows(panels: list[dict]) -> None:
    repeated = sorted(
        panel["title"]
        for panel in panels
        if panel.get("type") == "row" and panel.get("repeat")
    )
    if repeated:
        raise SystemExit(f"repeat rows still present: {repeated}")


def assert_unique_panel_ids(panels: list[dict]) -> None:
    ids = []
    for panel in iter_all_panels(panels):
        panel_id = panel.get("id")
        if panel_id is None:
            raise SystemExit(f"panel without id: {panel.get('title')!r}")
        ids.append(panel_id)

    duplicates = sorted({panel_id for panel_id in ids if ids.count(panel_id) > 1})
    if duplicates:
        raise SystemExit(f"duplicate panel ids: {duplicates}")


def overlaps(left: dict, right: dict) -> bool:
    left_x1 = left["gridPos"]["x"]
    left_y1 = left["gridPos"]["y"]
    left_x2 = left_x1 + left["gridPos"]["w"]
    left_y2 = left_y1 + left["gridPos"]["h"]

    right_x1 = right["gridPos"]["x"]
    right_y1 = right["gridPos"]["y"]
    right_x2 = right_x1 + right["gridPos"]["w"]
    right_y2 = right_y1 + right["gridPos"]["h"]

    return (
        left_x1 < right_x2
        and left_x2 > right_x1
        and left_y1 < right_y2
        and left_y2 > right_y1
    )


def assert_no_panel_overlaps(panels: list[dict]) -> None:
    flattened = list(iter_non_row_panels(panels))
    for index, panel in enumerate(flattened):
        for other in flattened[index + 1 :]:
            if overlaps(panel, other):
                raise SystemExit(
                    "overlapping panels: "
                    f"{panel['title']!r} (id={panel['id']}) and "
                    f"{other['title']!r} (id={other['id']})"
                )


def validate_dashboard(dashboard: dict) -> None:
    panels = dashboard["panels"]
    assert_row_order(panels)
    assert_unique_row_titles(panels)
    assert_no_repeat_rows(panels)
    assert_unique_panel_ids(panels)
    assert_no_panel_overlaps(panels)
    assert_safe_instance_variable(dashboard)
    assert_no_legacy_datasource_refs(dashboard)
    assert_no_raw_instance_filters_when_instance_is_service(dashboard)


def assert_safe_instance_variable(dashboard: dict) -> None:
    variables = {
        variable.get("name"): variable
        for variable in dashboard.get("templating", {}).get("list", [])
        if variable.get("name")
    }
    instance = variables.get("instance")
    if instance is None:
        raise SystemExit("dashboard is missing the 'instance' variable")

    exact_instance_matches = 0
    for panel in iter_non_row_panels(dashboard["panels"]):
        for target in panel.get("targets", []):
            expr = target.get("expr", "")
            if '="$instance"' in expr:
                exact_instance_matches += 1

    if exact_instance_matches and (
        instance.get("multi") or instance.get("includeAll")
    ):
        raise SystemExit(
            "instance variable cannot allow multi-select or All while panels "
            "still use exact `label=\"$instance\"` matching"
        )


def assert_no_legacy_datasource_refs(dashboard: dict) -> None:
    if LEGACY_DATASOURCE_UID in json.dumps(dashboard):
        raise SystemExit(
            "dashboard still contains legacy datasource placeholder "
            f"{LEGACY_DATASOURCE_UID}"
        )


def assert_no_raw_instance_filters_when_instance_is_service(dashboard: dict) -> None:
    variables = {
        variable.get("name"): variable
        for variable in dashboard.get("templating", {}).get("list", [])
        if variable.get("name")
    }
    instance = variables.get("instance")
    if instance is None:
        raise SystemExit("dashboard is missing the 'instance' variable")

    instance_query = instance.get("query", "")
    if "service" not in instance_query:
        return

    raw_instance_filters = 0
    for panel in iter_non_row_panels(dashboard["panels"]):
        for target in panel.get("targets", []):
            expr = target.get("expr", "")
            raw_instance_filters += expr.count('instance=~"$instance"')
            raw_instance_filters += expr.count('instance="$instance"')

    if raw_instance_filters:
        raise SystemExit(
            "dashboard still contains raw instance filters even though the "
            "'instance' variable is populated from service labels"
        )


def main() -> None:
    dashboard = load_dashboard_template()
    grouped_panels = collect_panels()
    dashboard["panels"] = build_panels(grouped_panels)
    validate_dashboard(dashboard)

    with OUTPUT_PATH.open("w") as handle:
        json.dump(dashboard, handle, indent=2)
        handle.write("\n")

    row_titles = [panel["title"] for panel in dashboard["panels"] if panel.get("type") == "row"]
    total_panels = sum(1 for _ in iter_non_row_panels(dashboard["panels"]))
    print(
        f"wrote {OUTPUT_PATH.relative_to(WORKTREE_ROOT)} "
        f"with {len(row_titles)} rows and {total_panels} panels"
    )
    print(f"rows: {row_titles}")


if __name__ == "__main__":
    main()
