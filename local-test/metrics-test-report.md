# Morph-Reth Metrics Dashboard — Full Panel Test Report

**Branch**: `worktree-feat+engine-api-metrics`
**HEAD**: `13fb789 feat(metrics): add unified Grafana dashboard and local devnet test infra`
**Test env**: local 3-node devnet (sequencer0-reth + full0-reth + full1-reth) on chain `morph-devnet`,
Prometheus scrape_interval=5s, Grafana 13.0.0
**Workload**: 44 blocks produced via `produce-blocks.py`, covering success + failure paths for
every Morph Engine API method + every custom payload-builder metric.

---

## 1. Static audit (Layer 1 — JSON structure)

`local-test/audit-dashboards.py` runs a full re-generation through `merge-dashboards.py`
and lints the resulting `etc/grafana/dashboards/morph-reth.json`.

| Check | Result |
|---|---|
| `merge-dashboards.py` reproducible (diff vs committed) | ✅ clean |
| Row count / order | ✅ 10 rows, order matches `TARGET_ROWS` |
| Panel count | 131 non-row panels |
| Duplicate panel IDs | ✅ 0 |
| Zero-size or overflowing `gridPos` | ✅ 0 |
| 16 custom metrics from `metrics.rs` → panel coverage | ✅ 16/16 referenced |
| Template vars declared (chain, service, role, quantile, instance, interval, ...) | ✅ |
| Source dashboards (`morph-engine.json`, `overview.json`, `reth-*.json`) JSON-valid | ✅ all 5 |

**Warning only**: dashboard JSON contains literal `${A}`/`${B}`/`${DS_EXPRESSION}` strings —
these are Grafana `refId` / datasource-expression placeholders, not template variables, so it's
not a real issue but the audit flags them for awareness.

Generated artifacts:
- `local-test/audit-report.txt` — full audit output
- `local-test/panel-inventory.csv` — `row, panel, id, gridPos, type, exprs` for every panel

## 2. Prometheus query coverage (Layer 2)

`local-test/query-panels.py` iterates every panel's PromQL expression, substitutes the template
variables with real devnet values, and classifies the result.

| Row | OK | Empty | Total |
|---|---:|---:|---:|
| **Overview** | 15 | 0 | 15 |
| **Morph Engine API** | **6** | **0** | **6** |
| **Morph Payload Builder** | 8 | 1 | 9 |
| **Execution & State Root** | 9 | 5 | 14 |
| **Database & Storage** | 17 | 4 | 21 |
| **RPC** | 2 | 4 | 6 |
| **Sync & Downloaders** | 11 | 1 | 12 |
| **TxPool & P2P** | 25 | 3 | 28 |
| **State & History** | 10 | 0 | 10 |
| **Process & Maintenance** | 7 | 3 | 10 |
| **TOTAL** | **110** | **21** | **131** |

- **0 PromQL errors** (no malformed queries)
- Every `reth_morph_engine_*` and `reth_morph_payload_builder_*` metric verified present in Prometheus
- Full per-panel breakdown: `local-test/panel-query-report.csv`

### The 21 "empty" panels — all are expected-for-environment, not bugs:

| Panel | Reason empty | Expected? |
|---|---|---|
| Morph Pool Tx Skipped By Role | No pool-tx drops in happy-path workload | ✅ expected |
| Execution cache hitrate / Precompile cache hitrate | No precompile/cache traffic | ✅ expected |
| Total / Redundant multiproof account+storage nodes | Multi-proof only runs on mainnet-scale workloads | ✅ expected |
| DB Average/Max commit/insert time | Expr filters `>= 0`; commit time rounds to 0 at devnet scale | ✅ expected |
| Static Files Max Writer Commit Time | Same — 0 writes at this scale | ✅ expected |
| RPC Request Latency / Call Latency / Throughput / Max per-method | `rpc_server_calls_*` needs sustained HTTP RPC; our traffic is engine API | ✅ expected |
| Block body response sizes | No pipeline sync traffic on local devnet | ✅ expected |
| Discv5 Peers / Peer Churn / Advertised Stacks | Devnet uses `--trusted-peers`, Discv5 disabled | ✅ expected |
| Jemalloc Memory | Binary not built with `jemalloc` feature | ✅ expected |
| Pruner duration per segment / Highest pruned per segment | Only one pruner run so far at devnet scale | ✅ expected |

## 3. Visual verification (Layer 3 — Chrome DevTools MCP)

Opened `http://localhost:3000/d/morph-reth-unified/morph-reth` in Chrome, expanded every row,
scrolled through all 131 panels, took 25 viewport screenshots (`/tmp/morph-dashboard-screenshots/`).

### 3.1 Row-by-row visual status

| Row | Layout | Panel render | Notes |
|---|---|---|---|
| Overview | ✅ | 14/15 render | ⚠️ **Cargo Features** shows raw `reth_info{...}` JSON (see issue #1) |
| **Morph Engine API** | ✅ clean 2-col grid | ✅ 6/6 render with real data | avg + p{quantile} both plot; quantile switch confirmed works (tested p0.5 → p0.99) |
| **Morph Payload Builder** | ⚠️ **last row has 1 wide gap** (Reth Builder Failed Jobs occupies left 12 cols, right 12 cols empty) | 8/9 render | `Morph Pool Tx Skipped By Role` = "No data" (expected); ⚠️ **Morph Block Transactions** timeseries shows periodic 0↔4 spikes — see issue #3 |
| Execution & State Root | ✅ | 9/14 render, 5 "No data" | y-axis auto-scales to `1.67 mins` when all values are 0 → see issue #4 |
| Database & Storage | ✅ | 17/21 render (pie charts, heatmap, tables all ok) | |
| RPC | ✅ | 2/6 render | Most empty because engine API path doesn't touch HTTP RPC (expected) |
| Sync & Downloaders | ✅ | 11/12 render | Headers/Bodies I/O are flat-zero (expected — no pipeline sync) |
| TxPool & P2P | ✅ | 25/28 render | Discovery tracked peers rising, P2P Connections/Errors visible |
| State & History | ✅ | 10/10 render | First couple of "growth over 10m" panels empty because devnet is <10m old |
| Process & Maintenance | ✅ | 7/10 render | Memory/CPU/FD curves look healthy; pruner panels empty |

### 3.2 Interactive-behavior smoke tests

| Test | Result |
|---|---|
| Expand/collapse every row | ✅ |
| Quantile variable switch (`0.5` → `0.99`) propagates to Morph Engine API latency legends | ✅ |
| Instance variable switch (`full0-reth` → `sequencer0-reth`) refreshes panels | ✅ |
| Grafana console errors | ✅ 0 |
| Grafana XHR/fetch 4xx or 5xx | ✅ 0 |

---

## 4. Issues found

### Issue #1 — **Cargo Features panel displays raw `reth_info` metric** [BUG]

**Location**: Overview row, panel "Cargo Features" (panel id 7)

**Root cause**:
```jsonc
{
  "expr": "reth_info{$instance_label=\"$instance\"}",
  "legendFormat": "{{cargo_features}}"
}
```
The `reth_info` metric exposed by morph-reth has labels
`{build_profile, build_timestamp, chain, git_sha, instance, job, role, service, target_triple, version}` —
**there is no `cargo_features` label**. When Grafana's stat panel can't resolve the legendFormat
placeholder it falls back to the raw `{k="v",…}` metric, which is what we see on the dashboard.

**Fix options**:
1. **Add `cargo_features` label to `reth_info`** in reth metrics setup (cleanest — matches pattern of the other build-info labels).
2. **Change the panel** to a different metric or drop the "Cargo Features" tile entirely (the other Build-timestamp / Git-SHA / Build-Profile / Target-Triple tiles cover everything useful).

Recommendation: (2) is simpler — the tile adds no value without a dedicated label.

**Evidence**: `/tmp/morph-dashboard-screenshots/02-row-overview.png` (top right tile).

---

### Issue #2 — **Morph Payload Builder last row has unused half** [COSMETIC]

**Location**: Morph Payload Builder row, "Reth Builder Failed Jobs" panel (panel id 33).

The panel occupies columns 0–12 at the bottom of the row; columns 12–24 are empty. All other
rows tile cleanly. This is a `merge-dashboards.py` layout issue — when the panel count isn't
a multiple of 2 the last row bleeds.

**Fix**: Either widen `Reth Builder Failed Jobs` to `w:24`, or pair it with another metric (e.g. a
`reth_payloads_*_jobs{rate}` stat).

**Evidence**: `/tmp/morph-dashboard-screenshots/06-row-morph-payload-builder-bottom.png`.

---

### Issue #3 — **"Morph Block Transactions" timeseries has visual 0↔N spikes** [UX]

**Location**: Morph Payload Builder row, panel id 28.

**Cause**: `block_transactions` is a gauge that is set on every successful payload build (`BuildOutcomeKind::Better`). Because the sequencer also builds some empty blocks in between the tx-producing ones (or between polling cycles), the gauge alternates between `4` and `0`. The timeseries plot renders these as sharp spikes, which *looks* like a bug but is the intended semantic.

**Isolated reproduction**: `/tmp/morph-dashboard-screenshots/25-morph-block-transactions-solo.png` — single series filtered to `sequencer0-reth` still shows the spikes.

**Fix options** (nice-to-have, not blocking):
1. Change panel query to `max_over_time(reth_morph_payload_builder_block_transactions[$__rate_interval])` — smooths out the zeros so the visual shows "peak tx per build in that window".
2. Or add a second panel "Avg tx per built block" = `rate(..._count)/rate(..._count)` (requires making it a histogram, which is a code change).
3. Or leave as-is and accept that gauge on a timeseries looks spiky.

---

### Issue #4 — **Empty histogram panels auto-scale y-axis to `1.67 mins`** [UX]

**Panels affected**: several reth-native histogram panels in Execution & State Root row
(e.g. `State root latency`, `Proof fetching total duration`, `Block validation overhead`).

When all observations are `0s` Grafana's auto y-axis extends to 1.67 minutes to match the raw
summary quantile max, which makes the signal line hug `0s` and look like "no data". This is
Grafana's default behaviour — not a bug of our dashboard, but we could override `min=0`, `max=auto`
or set a soft-max (e.g. `soft-max=100ms`) in field overrides to make the panel look saner when
the signal is tiny.

**Evidence**: `/tmp/morph-dashboard-screenshots/07-row-execution-state-top.png`.

---

### Issue #5 — **Reth Builder Active/Initiated/Failed Jobs are always 0** [EXPECTED-BUT-CONFUSING]

**Panels**: `Reth Builder Active Jobs`, `Reth Builder Initiated Jobs`, `Reth Builder Failed Jobs`
(panel ids 31, 32, 33) all hit `reth_payloads_*_jobs` metrics exposed by the generic reth payload
builder manager. These metrics are only incremented by the `PayloadBuilderManager` path, which
Morph **does not use** — our engine API drives block production directly through
`morph_build_block`. So they will be 0 on every Morph node in every environment.

**Recommendation**: Either remove these three panels, or rename the row to make the split clear
("Morph Payload Builder" first 6 panels + "Reth-native builder (unused on Morph)" last 3
— and/or hide them behind a template toggle). As-is they add confusion.

---

### Issue #6 — **Overview / Stage checkpoints + Sync progress panels are near-useless on Morph** [ARCHITECTURAL]

**Panels**: `Stage checkpoints`, `Sync progress (stage progress in %)`, `Sync progress (stage progress as highest block number reached)` — all in the Overview row.

**Full empirical breakdown** (from the running 3-node devnet at `sequencer0-reth` block 1928,
`full0-reth`/`full1-reth` block 0):

| Metric | sequencer0-reth (driver) | full0-reth / full1-reth (passive) |
|---|---|---|
| `reth_sync_checkpoint{stage=…}` | ✅ 16 series, each = current head | ❌ not exposed at all |
| `reth_sync_entities_processed` | `0` | `0` |
| `reth_sync_entities_total` | `0` | `0` |
| `reth_storage_providers_database_save_blocks_update_pipeline_stages_count` | `635` (== block count) | `0` |
| `reth_storage_providers_database_insert_block_count` | `0` | `0` |
| `eth_blockNumber` via HTTP | `1928` | `0` |

**What's actually going on** (root cause):

1. **Morph-reth does not use reth-native P2P pipeline sync.** The reth "staged sync" path
   (`Headers → Bodies → SenderRecovery → Execution → AccountHashing → StorageHashing →
   MerkleExecute → …`) is driven by the `Pipeline` running over P2P-downloaded headers/bodies.
   Because Morph L2 blocks are not wire-compatible with reth's block gossip format, **block
   gossip over P2P is disabled / never triggers the pipeline**. The built-in metric
   `reth_sync_entities_processed / _total` is only updated while the pipeline is actively
   running a stage; on Morph it stays at 0 on every node.

2. **Sequencer path — `engine_newL2Block` → `new_payload` + `fork_choice_updated`** (see
   `crates/engine-api/src/builder.rs:700`). The standard engine API tree commits each block
   through the background write pipeline, which calls
   `save_blocks → update_pipeline_stages`. That *does* update `reth_sync_checkpoint{stage=…}`
   — but because `save_blocks` is a single write, **every stage gets bumped to the current
   head number immediately**. So on the sequencer the panel is a flat heatmap where every
   row is identical to `eth_blockNumber`. It confirms "the driver is at block N" but tells
   you nothing about pipeline stage progress (because there are no separate stages
   progressing).

3. **Full-node path — `engine_newSafeL2Block`** is the intended mechanism by which
   morph-node (the external consensus layer) pushes derived blocks to sync-mode reth nodes.
   The local devnet doesn't run morph-node, so full0/full1 receive **no blocks**, never
   exercise `save_blocks`, and therefore **never expose the `reth_sync_checkpoint` metric at
   all** (derive-label metrics only appear after first increment). That's why the panel
   renders "No data" on every full node in every environment that uses reth-only P2P.

**Visual evidence**:
- `/tmp/morph-dashboard-screenshots/02-row-overview.png` — Instance=`full0-reth`, "Stage checkpoints" = No data
- `/tmp/morph-dashboard-screenshots/26-overview-instance-sequencer0.png` — Instance=`sequencer0-reth`, heatmap shows
  16 stages all pegged to the same block number

**So the answer to "does morph-reth support stage sync?"**:
- **reth-native staged sync: no.** Morph replaces it with the engine-API-driven path above.
- **Stage-checkpoint metrics: half-yes** — only on the node that imports blocks
  (sequencer with produce-blocks, or a full node that morph-node is actively pushing to),
  and even then the per-stage breakdown is uninformative because it's effectively a single
  atomic write.
- **Pipeline stage progress (`entities_processed / _total`): never** on any Morph node.

**Recommendation** (pick one):

1. **Remove** the three panels from the Overview row — they add "No data" clutter for any
   architecturally honest usage of morph-reth.
2. **Replace** them with a single Morph-aware panel: e.g. "Imported blocks per second by
   role" = `rate(reth_storage_providers_database_save_blocks_update_pipeline_stages_count[$__rate_interval])` —
   this actually distinguishes "driver advancing" vs "driver stuck" and works on any
   instance that's actively importing.
3. **Keep** them but add a dashboard description / markdown panel explaining "these panels
   show the engine-API write path, not a staged P2P sync; full nodes only populate them
   once `engine_newSafeL2Block` has been called at least once by morph-node".

Recommendation: (2) — it replaces three noisy panels with one informative one and aligns
with how Morph actually ingests blocks.

Note: my original report assumed the "No data" here was "expected because devnet has no
pipeline sync". That was too generous — the deeper finding is that these reth-native panels
are **architecturally mismatched to morph-reth** and will behave the same way in
production (full-node monitoring will always show "No data" on these tiles unless morph-node
has pushed at least one block).

---

## 5. Dashboard elegance assessment (the `.json`)

`merge-dashboards.py` is the right approach — it takes 6 smaller source dashboards and composes
them into a single 131-panel unified dashboard with hard validation. The validator already catches
the most important structural problems (`assert_unique_panel_ids`, `assert_no_panel_overlaps`,
`assert_safe_instance_variable`, `assert_no_legacy_datasource_refs`, etc.).

Things that could make it even better:

1. **Add a panel-coverage assertion to the validator** — fail the merge if any `reth_morph_*`
   metric declared in `metrics.rs` does not appear in at least one panel `expr`. My
   `audit-dashboards.py` does exactly this after the fact; pulling it into `merge-dashboards.py`
   would prevent accidental drift.

2. **Reth Builder * panels should go away or be clearly marked**, as per Issue #5.

3. **The last-row layout gap** (Issue #2) is symptomatic of `layout_panels` not snapping
   single-panel rows to `w=24`. A small patch to `layout_panels` in `merge-dashboards.py` would
   fix that and make every row fully tiled.

4. `morph-engine.json` (the hand-written source row dashboard) is not exposed anywhere through
   provisioning — it's only read by the merge script. Worth a comment at the top of the file,
   or moving it into a `sources/` subfolder so future editors aren't confused about which file
   Grafana actually serves.

5. `etc/grafana/dashboards/dashboard.yml` configures the provisioner, but
   `local-test/docker-compose.monitoring.yml` only copies `morph-reth.json` into
   `/var/lib/grafana/dashboards` via `sed`. This works but is slightly fragile — the `sed` replaces
   `${VAR_INSTANCE_LABEL}` / `${datasource}` / `${DS_PROMETHEUS}` inline, meaning those
   placeholders don't exist in the final JSON. Would be cleaner to either keep them as Grafana
   templating variables, or do the substitution in `merge-dashboards.py` at build time.

---

## 6. Test artifacts & how to reproduce

```sh
# 0. Check out branch
git worktree add .worktrees/engine-api-metrics-test origin/worktree-feat+engine-api-metrics

# 1. Build binary
CARGO_TARGET_DIR=$PWD/target cargo build --release --bin morph-reth

# 2. Start 3-node devnet + Prometheus + Grafana
./local-test/start-devnet.sh

# 3. Produce ~40 blocks to exercise every Morph metric
cd local-test
NO_PROXY=127.0.0.1,localhost python3 produce-blocks.py --blocks 40 --interval 0.3

# 4. Layer 1 — static audit
python3 audit-dashboards.py            # writes audit-report.txt + panel-inventory.csv

# 5. Layer 2 — PromQL panel verification
NO_PROXY=127.0.0.1,localhost python3 query-panels.py  # writes panel-query-report.csv

# 6. Layer 3 — open http://localhost:3000/d/morph-reth-unified/morph-reth (admin/admin)
```

Artifacts from this run:

| File | Description |
|---|---|
| `local-test/audit-dashboards.py` | Layer-1 static audit script (new) |
| `local-test/query-panels.py` | Layer-2 PromQL batch-query script (new) |
| `local-test/audit-report.txt` | Layer-1 audit output (50 lines) |
| `local-test/panel-inventory.csv` | 131 panels × exprs |
| `local-test/panel-query-report.csv` | 131 panels × (status, series count, notes) |
| `local-test/panel-query-summary.txt` | Pretty summary of Layer-2 results |
| `/tmp/morph-dashboard-screenshots/*.png` | 25 screenshots covering every row |

## 7. Summary

- **The monitoring implementation is functionally sound.** Every one of the 16 custom metrics
  declared in `engine-api/metrics.rs` and `payload/builder/metrics.rs` is present at the
  `/metrics` endpoint, scraped into Prometheus, and referenced by at least one panel in
  `morph-reth.json`. All 6 Morph Engine API panels and 8 of 9 Morph Payload Builder panels
  plot real series in the Grafana dashboard under the test workload.

- **JSON is elegant-enough**, with a strong reproducible build via `merge-dashboards.py` and
  multiple structural assertions. Suggested improvements in §5 above.

- **6 issues found**, none blocking:
  1. Cargo Features panel renders raw metric (fix: drop the tile) — **real bug, one-line fix**
  2. Layout gap in last Morph Payload Builder sub-row — cosmetic
  3. Block-transactions gauge spikes — UX
  4. Empty histogram y-axis auto-scale — UX (Grafana default)
  5. Reth-native payload builder panels are permanently 0 on Morph — clarity
  6. **Stage checkpoints / Sync progress panels architecturally mismatched** — reth-native pipeline sync doesn't apply to Morph's engine-API-driven import path; recommend replacing with a "save_blocks rate by role" panel
