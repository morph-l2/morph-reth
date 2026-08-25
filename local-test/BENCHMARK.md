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
generates `summary.tsv`.

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
| `OPENLOOP_TARGET_TPS` | `200000` | Requested open-loop submission rate |
| `P2P_PORT` | `30313` | Isolated P2P listener port for the benchmark node |

## Interpreting results

Treat the first full run after a cold build as a smoke test, not a baseline. For comparable numbers,
keep the commit, build profile, hardware power mode, sender count, workload, and node flags fixed.
`metadata.json` records the commit and main run configuration. A non-zero `dirty_files` value means
the result was produced from an uncommitted worktree and should be labeled accordingly.

The benchmark uses the sequential V1 Morph Engine methods because each run only extends the current
head. The V2 methods add explicit-parent/reorg behavior, which this workload does not exercise.

Use `bench-block-exec verify-state --rpc-a <url> --rpc-b <url>` when comparing two nodes. It checks
their latest block number, state root, receipts root, and deterministic funded-sender balances.

## Limitations

- This is a synthetic execution-engine ceiling test, not a production TPS forecast. It disables
  discovery and transaction backup and raises the block, payload, RPC, and txpool limits far above
  normal deployment values; it does not include consensus, networking, proving, or L1 data costs.
- The supported runner is reth-only. The archived April 2026 reth/geth report was produced by an
  older runner and older binaries and must not be presented as a current-`main` comparison.
- Open-loop mode pre-generates `target_tps * duration_secs` signed requests. The defaults create
  24 million transactions per run and therefore require substantial memory. `target_tps` is offered
  load; use `realized_tps`, the active/drain split, and the completion log to judge delivered load.
- Three runs provide descriptive repeatability, not a confidence interval or statistical
  significance test. Preserve the per-run JSONL and logs and inspect run-to-run spread before making
  regression or capacity claims.
