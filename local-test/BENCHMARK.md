# morph-reth benchmark

`run_full_benchmark.sh` is the supported benchmark interface. It benchmarks one freshly started
morph-reth node per run; geth and cross-binary comparisons are intentionally out of scope.

The April 2026 reth/geth comparison and its charts remain under `bench-results/report/`. The
original design and implementation plan remain under `docs/superpowers/` as historical context;
they describe the earlier runner and should not be treated as the current command reference.
Its report generator is preserved at `local-test/legacy/generate_report.py` and only understands
the archived runner's filename layout.

## What it measures

- `exec`: block assembly and import time after transactions are already accepted by the txpool.
- `sustained`: transaction submission, txpool acceptance, block assembly, and import over many blocks.
- `openloop`: continuously submits at a requested rate while morph-reth continuously assembles and
  imports blocks. Use `realized_tps` and the active/drain split in `summary.tsv`; `target_tps` is load,
  not an expected number of transactions in each block.

The runner rebuilds morph-reth and the benchmark binary, compiles the Solidity fixtures, generates a
fresh genesis, waits for RPC readiness, captures node and benchmark logs, writes run metadata, and
generates `summary.tsv`. Node stdout is fixed at info level and background log files are disabled so
all diagnostic output stays with the run artifacts.

## Prerequisites

- Rust/Cargo
- Foundry (`forge`)
- `curl`, `jq`, and `openssl`

## Run

```bash
./local-test/run_full_benchmark.sh
```

The default suite is deliberately long: three runs of `exec`, `sustained`, and `openloop` for ETH and
ERC20 transfers. Results default to a timestamped directory under `/tmp/morph-reth-bench/`.

Use a small smoke run before a full measurement:

```bash
MODES=exec WORKLOADS=eth-transfer RUNS=1 EXEC_BLOCK_SIZES=10 EXEC_BLOCKS=2 \
  ./local-test/run_full_benchmark.sh
```

Common overrides:

| Variable | Default | Meaning |
| --- | --- | --- |
| `PROFILE` | `release` | Cargo profile used for both binaries |
| `BUILD` | `1` | Set to `0` to reuse existing binaries |
| `RESULTS_DIR` | timestamped `/tmp` path | Output, logs, genesis, and metadata |
| `MODES` | `exec sustained openloop` | Space-separated modes |
| `WORKLOADS` | `eth-transfer erc20-transfer` | Space-separated workloads; `uniswap-swap` is also supported |
| `RUNS` | `3` | Independent fresh-node runs per configuration |
| `SENDERS` | `2000` | Deterministic funded sender accounts |
| `EXEC_BLOCK_SIZES` | `1000 10000 50000 100000` | Requested txs/block for the execution scaling curve |
| `SUSTAINED_TXS_PER_BLOCK` | `50000` | Requested txs/block for the long state-growth run |
| `OPENLOOP_TARGET_TPS` | `200000` | Offered load; use the submitted and realized rates to judge what was delivered |
| `OPENLOOP_DURATION_SECS` | `120` | Active submission duration |
| `OPENLOOP_DRAIN_SECS` | `600` | Maximum time to import all accepted transactions after submission ends |
| `BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT` | `1` | Disable both builder and import-side DA payload bounds; benchmark only |
| `BENCHMARK_GENESIS_MAX_TX_PAYLOAD_BYTES` | `1073741824` | Compatibility value recorded in generated genesis; not enforced when the bypass is enabled |
| `BENCHMARK_BUILDER_DEADLINE_SECS` | `12` | Fixed payload-building deadline for very large synthetic blocks |
| `BENCHMARK_TXPOOL_MAX_COUNT` | `30000000` | Per-subpool count ceiling for high-rate open-loop runs |
| `P2P_PORT` | `30313` | Isolated P2P listener port for the benchmark node |

## Interpreting results

Treat the first full run after a cold build as a smoke test, not a baseline. For comparable numbers,
keep the commit, build profile, hardware power mode, sender count, workload, and node flags fixed.
`metadata.json` records the commit and main run configuration. A non-zero `dirty_files` value means
the result was produced from an uncommitted worktree and should be labeled accordingly.

Fixed-size modes require 100% inclusion. Requests that do not fit under the configured gas limit or
builder deadline fail the run instead of reporting the smaller assembled block as though it
represented the requested block size. The default runner explicitly disables the DA-derived payload
limit on both block building and import, so its 50k/100k results measure the execution path rather
than production-valid block capacity. Set `BENCHMARK_DISABLE_TX_PAYLOAD_LIMIT=0` to test the normal
720 KiB behavior.

The benchmark uses the sequential V1 Morph Engine methods because each run only extends the current
head. The V2 methods add explicit-parent/reorg behavior, which this workload does not exercise.

Use `bench-block-exec verify-state --rpc-a <url> --rpc-b <url>` when comparing two nodes. It checks
their latest block number, state root, receipts root, and deterministic funded-sender balances.

## Limitations

- This is a synthetic execution-engine ceiling test, not a production TPS forecast. It disables
  discovery and transaction backup and raises the block gas, RPC, and txpool limits far above normal
  deployment values. By default it also disables the 720 KiB builder and import payload checks;
  blocks produced in this mode can exceed the Morph DA envelope and are not production-valid. It
  does not include consensus networking, proving, or L1 data costs.
- The supported runner is reth-only. The archived April 2026 reth/geth report was produced by an
  older runner and older binaries and must not be presented as a current-`main` comparison.
- Open-loop mode pre-generates `target_tps * duration_secs` signed requests. The defaults create
  24 million transactions per run and therefore require substantial memory. A target that fills the
  txpool is a failed run, not a throughput result. `target_tps` is offered load, not guaranteed
  delivered load. Submission uses bounded concurrency and stops scheduling at the deadline; use
  `realized_tps`, the active/drain split, and the completion log's submitted count to judge the
  achieved load.
- Three runs provide descriptive repeatability, not a confidence interval or statistical
  significance test. Preserve the per-run JSONL and logs and inspect run-to-run spread before making
  regression or capacity claims.
