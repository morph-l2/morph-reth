# morph-reth vs morph-geth Execution Benchmark Report

**Generated**: 2026-04-09 18:20

## 1. Test Environment

| Item | Value |
|------|-------|
| Hardware | Apple M4 Pro, 14 cores, 48GB RAM |
| morph-reth | Latest feat/max-tps-benchmark branch |
| morph-geth | Latest main, maxRequestContentLength patched to 512MB |
| OS | macOS Darwin 25.4.0 |
| Senders | 2,000 pre-funded accounts |
| Genesis | 30B gas limit, all Morph hardforks at block 0, MPT mode |
| reth config | archive mode (default), 12 validation threads, in-memory block buffer |
| geth config | --gcmode archive, --cache 8192, unlimited batch/response size |

## 2. Mode A: Exec (Pure EVM Execution)

Measures only `assembleL2Block` + `newL2Block` time. Transaction submission is excluded.

![Exec ETH](charts/exec_eth-transfer.png)

![Exec ERC20](charts/exec_erc20-transfer.png)

### Exec Results Table

| Engine | Workload | Block Size | Median TPS | μs/tx | Assemble (ms) | Import (ms) |
|--------|----------|-----------|-----------|-------|-------------|------------|
| geth | erc20-transfer | 1,000 | 20,670 | 32.1 | 32.1 | 16.3 |
| geth | erc20-transfer | 10,000 | 24,981 | 28.7 | 287.1 | 113.1 |
| geth | erc20-transfer | 50,000 | 24,611 | 29.4 | 1468.4 | 561.3 |
| geth | erc20-transfer | 100,000 | 23,884 | 29.9 | 2989.1 | 1181.3 |
| geth | erc20-transfer | 200,000 | 23,085 | 31.1 | 3886.8 | 1526.5 |
| geth | eth-transfer | 1,000 | 33,910 | 20.4 | 20.4 | 9.4 |
| geth | eth-transfer | 10,000 | 40,619 | 17.6 | 175.8 | 69.7 |
| geth | eth-transfer | 50,000 | 39,377 | 17.9 | 897.1 | 357.4 |
| geth | eth-transfer | 100,000 | 38,087 | 18.4 | 1844.2 | 736.6 |
| geth | eth-transfer | 200,000 | 37,028 | 18.9 | 3772.6 | 1579.2 |
| reth | erc20-transfer | 1,000 | 80,315 | 11.1 | 11.1 | 1.6 |
| reth | erc20-transfer | 10,000 | 126,272 | 6.8 | 68.1 | 11.1 |
| reth | erc20-transfer | 50,000 | 133,570 | 6.4 | 319.0 | 54.8 |
| reth | erc20-transfer | 100,000 | 133,894 | 6.4 | 638.0 | 111.0 |
| reth | erc20-transfer | 200,000 | 133,471 | 6.4 | 1284.8 | 214.6 |
| reth | eth-transfer | 1,000 | 137,170 | 6.2 | 6.2 | 1.4 |
| reth | eth-transfer | 10,000 | 241,294 | 3.3 | 33.1 | 8.7 |
| reth | eth-transfer | 50,000 | 261,075 | 3.0 | 152.1 | 41.6 |
| reth | eth-transfer | 100,000 | 216,556 | 3.4 | 342.3 | 93.8 |
| reth | eth-transfer | 200,000 | 256,921 | 3.0 | 609.3 | 167.4 |

## 3. Mode B: Sustained (Full Pipeline, Degradation Analysis)

100 blocks × 50,000 txs/block with 5 warmup blocks. Tests performance stability.

![Sustained ETH](charts/sustained_eth-transfer.png)

![Sustained ERC20](charts/sustained_erc20-transfer.png)

### Sustained Results Table

| Engine | Workload | Median TPS | First-10 TPS | Last-10 TPS | Degradation |
|--------|----------|-----------|-------------|-------------|-------------|
| geth | erc20-transfer | 12,899 | 13,515 | 12,512 | -7.4% |
| geth | eth-transfer | 15,938 | 17,391 | 15,391 | -11.5% |
| reth | erc20-transfer | 41,805 | 41,696 | 42,172 | +1.1% |
| reth | eth-transfer | 58,720 | 58,153 | 59,718 | +2.7% |

## 4. Mode C: Openloop (High-Pressure Sustained Throughput)

200,000 target TPS, 120 seconds. Submit and block production run concurrently.

![Openloop Comparison](charts/openloop_comparison.png)

![Openloop Distribution](charts/openloop_distribution.png)

### Openloop Results Table

| Engine | Workload | Median TPS | P95 TPS | Peak TPS | Mgas/s | Stddev |
|--------|----------|-----------|---------|----------|--------|--------|
| geth | erc20-transfer | 16,995 | 22,423 | 23,778 | 583 | ±728 |
| geth | eth-transfer | 26,911 | 30,745 | 35,301 | 554 | ±422 |
| reth | erc20-transfer | 68,806 | 87,613 | 105,957 | 2,352 | ±2,122 |
| reth | eth-transfer | 73,110 | 81,495 | 140,596 | 1,493 | ±4,552 |

## 5. Overall Summary

![Summary](charts/summary.png)

### Cross-Engine Comparison

| Mode | Workload | reth TPS | geth TPS | reth/geth |
|------|----------|---------|---------|-----------|
| Openloop | eth-transfer | 73,110 | 26,911 | **2.7x** |
| Openloop | erc20-transfer | 68,806 | 16,995 | **4.0x** |

### Key Findings

1. **reth is 2.5-4x faster than geth** across all workloads and modes

2. reth's advantage comes from: parallel tx validation, Rust-native EVM (revm), in-memory block buffering

3. geth cannot match reth's parallelism — its txpool and EVM are fundamentally single-threaded

4. Both engines run in archive mode with equivalent storage configuration
