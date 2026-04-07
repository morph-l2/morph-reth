# Maximum TPS Benchmark Design

**Date:** 2026-04-07

**Goal**

Determine the absolute maximum TPS of morph-reth and morph-geth by testing three dimensions of performance: pure EVM execution throughput, end-to-end pipeline throughput, and sustained block production under state growth. Remove all artificial limits and squeeze every bit of performance from both engines.

**Why this benchmark exists**

The previous benchmark (2026-04-03) validated whether reth/geth can meet the 3,000 tx/block @ 300ms target. This benchmark goes further: find the breaking point of each engine under unconstrained conditions, understand where bottlenecks lie (EVM, state commit, txpool, IO), and quantify performance degradation under realistic state sizes and multi-sender contention.

**Relation to previous benchmark**

Extends `bin/bench-block-exec/` with new modes, workloads, and automation. The existing `run-workload` subcommand continues to work unchanged. New functionality is additive.

---

## 1. Test Modes

### Mode A: Pure Execution (`--mode exec`)

Measures raw EVM execution + state commit speed, with all external overhead eliminated.

**Data path:**
- Pre-generate all transactions in memory before timing starts
- Inject transactions directly via `assembleL2Block`'s `transactions` field (bypass txpool entirely)
- Measure `assemble_ms` and `import_ms` separately

**Configuration:**
- Single sender only (nonce-sequential, no sorting overhead)
- Sweep range: 1k, 2k, 5k, 10k, 20k, 50k, 100k txs/block
- 50 blocks per data point

**Answers:** What is the raw EVM throughput ceiling? Is the bottleneck in EVM execution or state commit?

### Mode B: End-to-End (`--mode e2e`)

Measures full pipeline throughput including txpool acceptance, sorting, block assembly, and import.

**Data path:**
- N senders submit transactions concurrently via `eth_sendRawTransaction` (batched, async)
- Wait for txpool to accept all (pending nonce polling)
- `assembleL2Block` with empty transactions array (pulls from txpool)
- `newL2Block` to import

**Per-block timing breakdown:**
- `submit_ms`: time to send all batches to txpool
- `pool_wait_ms`: time for pending nonce to reach expected value
- `assemble_ms`: assembleL2Block RPC latency
- `import_ms`: newL2Block RPC latency

**Configuration:**
- Sender counts: 1 (baseline), 100 (default), 1000 (stress)
- 200 blocks per test
- Batched submission: chunks of 500 txs per JSON-RPC batch call

**Answers:** What is the end-to-end sequencer TPS? Is the bottleneck in txpool or execution?

### Mode C: Sustained (`--mode sustained`)

Measures long-running block production stability and performance degradation under state growth.

**Two-phase operation:**
1. **Warmup phase** (`--warmup-blocks N`): Produce N blocks that are NOT timed. Fills up the state trie to simulate a mature chain.
2. **Measurement phase** (`--blocks N`): Produce N blocks with full timing. Default 1000.

**Tracked over time:**
- Per-block: all timing fields from Mode B
- Rolling average TPS (100-block window)
- Cumulative block/tx counters

**Configuration:**
- Warmup: 0 (empty state) or 500 blocks (populated state)
- Sender counts: 1, 100
- 1000 measurement blocks

**Answers:** Does TPS degrade over time? How much does state size impact performance?

---

## 2. Workload Types

### eth-transfer (existing)
- EIP-1559 value transfer, 1 wei, gas limit 21,000
- Minimal EVM work: balance debit/credit, nonce increment
- Purpose: baseline throughput ceiling

### erc20-transfer (existing)
- BenchToken.transfer(to, 1), gas limit 60,000
- 2 SSTORE (sender balance down, receiver balance up) + 1 LOG
- Purpose: typical contract call performance

### uniswap-swap (new)
- BenchSwap.swap0For1(amountIn), gas limit 150,000
- Constant-product AMM: 4 SLOAD + 4 SSTORE + arithmetic + 1 LOG
- Purpose: heavy compute + heavy storage workload

---

## 3. BenchSwap Contract

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.0;

contract BenchSwap {
    uint256 public reserve0;
    uint256 public reserve1;

    mapping(address => uint256) public balance0;
    mapping(address => uint256) public balance1;

    event Swap(
        address indexed sender,
        uint256 amountIn,
        uint256 amountOut,
        uint256 reserve0After,
        uint256 reserve1After
    );

    function swap0For1(uint256 amountIn) external {
        uint256 r0 = reserve0;
        uint256 r1 = reserve1;
        uint256 bal = balance0[msg.sender];

        require(bal >= amountIn, "insufficient balance");
        require(r0 > 0 && r1 > 0, "no liquidity");

        uint256 amountInWithFee = amountIn * 997;
        uint256 amountOut = (amountInWithFee * r1) / (r0 * 1000 + amountInWithFee);

        reserve0 = r0 + amountIn;
        reserve1 = r1 - amountOut;

        balance0[msg.sender] = bal - amountIn;
        balance1[msg.sender] = balance1[msg.sender] + amountOut;

        emit Swap(msg.sender, amountIn, amountOut, r0 + amountIn, r1 - amountOut);
    }
}
```

**Gas profile:** ~120-150k per call (warm slots). 4 SLOAD + 4 SSTORE + mul/div + LOG3.

**Genesis pre-deployment:**
- Contract deployed at deterministic address in genesis alloc
- `reserve0` and `reserve1` set to 10^24 (enough for millions of swaps)
- Each sender gets pre-allocated `balance0` sufficient for all test transactions

---

## 4. Multi-Sender Design

### Account Generation

Deterministic derivation for reproducibility:

```
master_key = keccak256("bench-sender-0")
sender[i].private_key = keccak256(master_key ++ i.to_be_bytes())
```

### Genesis Pre-population

Each sender account receives:
- ETH balance: 10^27 wei (gas funding)
- BenchToken balance: 10^24 tokens (in contract storage)
- BenchSwap balance0: 10^24 tokens (in contract storage)

### Transaction Distribution

For `txs_per_block = T` and `senders = S`:
- Each sender generates `T / S` transactions per block
- Round-robin interleaving: `[sender0_tx0, sender1_tx0, ..., senderS_tx0, sender0_tx1, ...]`
- Each sender tracks its own nonce independently

### Concurrent Submission (Mode B/C)

- Spawn `min(S, 16)` tokio tasks for parallel `eth_sendRawTransaction` submission
- Each task handles a subset of senders
- All tasks must complete before proceeding to assembly

---

## 5. Genesis Configuration Changes

### Limits Removed

```json
{
  "config": {
    "morph": {
      "maxTxPerBlock": 0,
      "maxTxPayloadBytesPerBlock": 1073741824
    }
  },
  "gasLimit": "0x2540BE400"
}
```

- `maxTxPerBlock`: Set to 0 (if 0 means unlimited in both reth and geth). If not supported, use 10,000,000. **Must verify during implementation.**
- `maxTxPayloadBytesPerBlock`: 1 GB (effectively unlimited)
- `gasLimit`: 10,000,000,000 (10B gas). Allows ~476k eth-transfers or ~166k erc20-transfers or ~66k swaps per block.

### Node Startup Tuning

**Reth:**
```bash
morph-reth node \
  --chain genesis.json \
  --morph.max-tx-payload-bytes 1073741824 \
  --engine.persistence-threshold 4096 \
  --engine.memory-block-buffer-target 4096
```

**Geth:**
```bash
geth \
  --gcmode archive \
  --cache 8192 \
  --txpool.globalslots 100000 \
  --txpool.accountslots 1000
```

Both engines configured to maximize memory usage so that caches and buffers are not the bottleneck.

---

## 6. Sweep: Automatic Inflection Point Discovery

New subcommand: `bench-block-exec sweep`

### Algorithm

**Step 1: Coarse scan (exponential)**
- Test points: 1k, 2k, 5k, 10k, 20k, 50k, 100k txs/block
- 30 blocks per point, take median `assemble_ms`
- Find first point where `assemble_ms > previous × 1.5` → `rough_peak`

**Step 2: Fine scan (linear)**
- Range: `[rough_peak / 2, rough_peak × 1.5]`
- Step size: `rough_peak / 10`
- 50 blocks per point, record p50/p95

**Step 3: Output**
- `peak_tps`: highest TPS before degradation
- `peak_mgas_s`: corresponding MGas/s
- `inflection_txs`: txs/block where latency begins rising sharply
- Full data points written to `sweep/*.jsonl`

### Degradation Definition
- Mode A: per-tx assemble time (`assemble_ms / tx_count`) increases >50% from minimum
- Mode B: total latency exceeds 1 second
- Mode C: TPS drops >10% from the first 100-block average

---

## 7. Metrics & Output Format

### Per-Block JSON Line

```json
{
  "block_number": 42,
  "tx_count": 5000,
  "expected_tx_count": 5000,
  "engine": "reth",
  "mode": "exec",
  "workload": "erc20-transfer",
  "senders": 100,
  "warmup_blocks": 0,

  "submit_ms": 0,
  "pool_wait_ms": 0,
  "assemble_ms": 85.3,
  "import_ms": 12.1,
  "total_ms": 97.4,

  "gas_used": 300000000,
  "tps": 51334.7,
  "mgas_per_sec": 3082.0,
  "inclusion_rate": 1.0,

  "cumulative_blocks": 42,
  "cumulative_txs": 210000,
  "rolling_avg_tps_100": null
}
```

**Key derived metrics:**
- `tps`: `tx_count / (total_ms / 1000)`
- `mgas_per_sec`: `gas_used / (total_ms / 1000) / 1_000_000`
- `inclusion_rate`: `tx_count / expected_tx_count`
- `rolling_avg_tps_100`: mean TPS of last 100 blocks (Mode C only, null if < 100 blocks)

### Summary TSV Columns

```
engine | mode | workload | senders | warmup_blocks | blocks |
avg_txs_per_block | inclusion_rate |
avg_assemble_ms | avg_import_ms | avg_total_ms |
p50_total_ms | p95_total_ms | p99_total_ms |
peak_tps | avg_tps | avg_mgas_s |
degradation_pct | error_count
```

- `degradation_pct`: (last 100 blocks avg TPS / first 100 blocks avg TPS - 1) × 100. Only for Mode C, 0 otherwise.
- First 10 blocks skipped as warmup in summary statistics (consistent with existing behavior).

---

## 8. Chart Generation

### `bench-plot.py`

Dependencies: `matplotlib`, `numpy` (standard, no heavy frameworks).

### Chart 1: Sweep TPS / MGas Curve
- Source: `sweep/*.jsonl`
- X: txs_per_block, Y-left: TPS, Y-right: MGas/s
- Series: reth vs geth, one subplot per workload
- Annotation: inflection point marker

### Chart 2: Latency CDF
- Source: `exec/*.jsonl` and `e2e/*.jsonl`
- X: latency (ms), Y: percentile (0-100%)
- Series: assemble_ms vs import_ms, reth vs geth
- Vertical lines at p50 / p95 / p99

### Chart 3: Sustained Time Series
- Source: `sustained/*.jsonl`
- X: block_number, Y: rolling_avg_tps_100
- Series: empty state vs pre-populated, reth vs geth
- Gray mask over warmup region

### Chart 4: Reth vs Geth Comparison Bar Chart
- Grouped bars for each workload × mode
- Metrics: peak_tps, avg_assemble_ms, p95_total_ms

### Chart 5: Multi-Sender Impact
- Source: `e2e/*.jsonl`
- X: sender count (1, 100, 1000), Y: TPS
- One subplot per workload

**Usage:**
```bash
python3 bench-plot.py --all --input bench-results/latest/ --output charts/
```

---

## 9. Orchestration Script

### `bench-block-exec.sh` Four Phases

**Phase 1: Sweep (~30 min)**
```
for engine in [reth, geth]:
  for workload in [eth-transfer, erc20-transfer, uniswap-swap]:
    sweep --mode exec --senders 1
```
6 sweep tasks. Output: peak txs/block per combination.

**Phase 2: Precise Matrix (~2 hours)**
```
Mode A (exec):  engine(2) × workload(3) × senders(1)      = 6 runs × 50 blocks
Mode B (e2e):   engine(2) × workload(3) × senders(1,100,1000) = 18 runs × 200 blocks
Mode C (sustained): engine(2) × workload(3) × senders(1,100)  = 12 runs × 1000 blocks
```
36 test cases. txs/block selection per mode:
- Mode A: uses sweep's `inflection_txs` (the point just before degradation)
- Mode B: uses `inflection_txs × 0.8` (conservative, accounts for txpool overhead)
- Mode C: uses `inflection_txs × 0.5` (must be sustainable over 1000 blocks)

**Phase 3: State Degradation (~1 hour)**
```
Mode C only: engine(2) × workload(3) × senders(100) × warmup(500)
```
6 test cases × (500 warmup + 1000 measurement) blocks.

**Phase 4: Summarize + Plot (<1 min)**
```
bench-block-exec summarize ...
python3 bench-plot.py --all ...
```

**Total: 42 test cases, ~26,000 blocks, ~3.5 hours**

### Per-Test Lifecycle

1. Generate genesis (multi-sender, contracts pre-deployed, limits removed)
2. Initialize datadir (geth: `geth init`, reth: `--chain` flag)
3. Start node via PM2
4. Wait for RPC readiness (HTTP poll, 120s timeout)
5. Run benchmark (`bench-block-exec run --mode ... --workload ...`)
6. Stop node
7. Clean datadir

Each test is fully isolated: own genesis, own datadir, own process.

### Resume Support

Script checks for existing result files before each test case. Completed cases are skipped. Use `--force` to re-run all.

---

## 10. Error Handling

### RPC Failures
- 60s timeout per call
- On assemble/import failure: log error in JSON output with `"error": true`, continue to next block
- 5 consecutive failures → terminate current test case, mark as failed

### Txpool Rejection (Mode B/C)
- Exponential backoff retry: 100ms → 200ms → 400ms, max 3 attempts
- On persistent failure: reduce batch size and retry
- Record rejection count in metrics

### Tx Count Mismatch
- Track `expected_tx_count` vs `actual_tx_count`
- Compute `inclusion_rate`
- Sub-100% inclusion rate is valid data (indicates txpool/gas bottleneck)

### OOM / Node Crash
- Detected via PM2 process status
- Sweep: record as hard upper limit, stop increasing txs/block
- Matrix test: mark as failed, continue to next case

### Data Integrity
- JSON lines ordered by block_number
- Summarize detects missing block numbers
- >10% missing → mark test case as unreliable

---

## 11. File Structure Changes

```
bin/bench-block-exec/
├── Cargo.toml              (add: no new heavy deps)
├── src/
│   ├── main.rs             (extend: new subcommands)
│   ├── engine.rs           (existing: minor extensions)
│   ├── genesis.rs          (extend: multi-sender, contract pre-deploy)
│   ├── workload.rs         (extend: multi-sender, uniswap workload)
│   ├── tx_factory.rs       (new: unified transaction generation)
│   ├── mode_exec.rs        (new: Mode A pure execution)
│   ├── mode_e2e.rs         (new: Mode B end-to-end)
│   ├── mode_sustained.rs   (new: Mode C sustained)
│   ├── sweep.rs            (new: automatic inflection finder)
│   ├── report.rs           (extend: new metrics, MGas/s)
│   └── verify.rs           (existing: unchanged)

local-test/
├── bench-block-exec.sh     (rewrite: 4-phase orchestration)
├── bench-plot.py           (new: chart generation)
└── bench-contracts/        (rename from erc20-bench-contracts/)
    ├── foundry.toml
    ├── src/
    │   ├── BenchToken.sol  (existing)
    │   └── BenchSwap.sol   (new)
    └── test/
        └── BenchSwap.t.sol (new: gas verification)
```

No new Rust crate dependencies beyond what already exists in Cargo.toml. Python script requires only `matplotlib` and `numpy`.
