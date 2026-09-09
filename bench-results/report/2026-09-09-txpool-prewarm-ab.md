# txpool prewarming（`--engine.txpool-prewarming`）A/B 压测

日期：2026-09-09（Asia/Shanghai）

## 背景

reth v2.5.0 发布说明：opt-in 的 transaction-pool prewarming 让以太坊主网平均出块处理延迟降低 7%，
用 `--engine.txpool-prewarming` 打开，只有 nightly Docker 镜像默认开启（上游 #26378、#26493）。
morph-reth 当前 pin 的 reth v2.5.2 已经包含这部分代码，`MorphTreeEngineValidatorBuilder`
（`crates/node/src/validator.rs`）在 flag 打开时会接上 `MorphTxPoolPrewarmSource`，默认关闭。

本次回答一个问题：这个开关能不能提高 morph-reth 的最大 TPS。

## 机制与 Morph 流程的关系

- worker 是一个常驻线程：在 canonical head 的状态上投机执行 txpool 里的 best transactions，
  把读到的 account/storage/code 写进 `CachedReads`，最多每 100 ms 发布一次不可变 snapshot；
  head 变化时缓存清零重来；迭代器暂时取不到交易时休眠 100 ms。
- snapshot 只在**块导入**（`insert_block`）时被消费：作为 `CachedStateProvider` 的第一层查找；
  导入期间 worker 被暂停。
- Morph 的 `engine_assembleL2Block`（`crates/engine-api/src/builder.rs`）以
  `BuildNewPayload { resources: Default::default() }` 触发 builder：builder 既拿不到 snapshot，
  也不会像上游 FCU 路径那样持有暂停 worker 的 lease。于是 assemble 阶段无法受益，
  而且 worker 会和 builder 同时跑。
- 在本压测的块周期里 assemble 约占 88%，import 约占 11%。

## 受控条件

- Morph commit `aaa48e765`（压测分支）+ 本次加入的 `RETH_EXTRA_ARGS`/`NODE_LOG_FILTER` 开关，
  release profile，`dirty_files = 1`（只有脚本改动，Rust 代码未变）。
- 机器：Apple M4 Pro，14 核，48 GiB，macOS 25.6.0。后台有一个闲置的 debug morph-reth
  （hoodi dev + firehose，约 0.3% CPU）全程存在，off/on 两侧相同。
- 两侧节点 flag 完全一致，只差 `--engine.txpool-prewarming`；日志过滤都用
  `info,engine::tree::txpool_prewarm=debug`（off 侧该 target 不产生日志）。
- 3 对交替运行：off、on、off、on、off、on；每个 mode 都是全新节点 + 全新 datadir。
- 2,000 个发送账户，receiver mode `unique`，workload `eth-transfer`，genesis 上限 1 GiB。
- exec：10k/50k/100k 笔/块 × 10 块；sustained：50k × 20 块（+5 预热）；
  openloop：目标 100k TPS × 30 s，drain 300 s。

## 结果（3 次运行均值）

| 模式 | txs/块 | prewarm | asm ms | imp ms | total ms | p95 ms | avg TPS | realized TPS | realized Δ |
|---|---:|---|---:|---:|---:|---:|---:|---:|---:|
| exec | 10,000 | off | 68.2 | 8.9 | 77.2 | 88.6 | 131,852 | 129,575 | |
| exec | 10,000 | on | 68.5 | 9.1 | 77.6 | 89.0 | 131,181 | 128,886 | -0.5% |
| exec | 50,000 | off | 350.7 | 46.5 | 397.2 | 467.4 | 128,412 | 125,891 | |
| exec | 50,000 | on | 352.2 | 46.8 | 399.1 | 466.6 | 128,019 | 125,309 | -0.5% |
| exec | 100,000 | off | 738.4 | 92.7 | 831.1 | 985.6 | 123,765 | 120,331 | |
| exec | 100,000 | on | 741.8 | 93.6 | 835.4 | 989.4 | 123,177 | 119,705 | -0.5% |
| sustained | 50,000 | off | 471.8 | 47.7 | 1,254.6 | 1,328.5 | 39,955 | 39,856 | |
| sustained | 50,000 | on | 470.6 | 47.6 | 1,248.8 | 1,319.2 | 40,126 | 40,039 | +0.5% |
| openloop（active） | ~35k | off | 398.4 | 36.9 | 438.4 | 2,133.1 | 84,947 | 77,402 | |
| openloop（active） | ~35k | on | 414.1 | 37.8 | 454.8 | 2,175.5 | 83,753 | 76,089 | -1.7% |

- exec avg TPS 在 3 次运行间的标准差约 1,100 到 1,600（约 1%），所以 ±0.5% 的差异是噪声。
- 逐对 realized TPS 差异（on 相对 off）：exec 10k +1.6 / -1.3 / -1.8；exec 50k +0.3 / -1.1 / -0.6；
  exec 100k -0.8 / -0.5 / -0.2；sustained +0.6 / -0.5 / +1.3；openloop -1.3 / -1.8 / -2.0。
  只有 openloop 三对方向一致。
- openloop 的块大小分布两侧相近（均值 34,455 对 35,112 笔/块）。按每笔成本看：
  assemble 10.18 → 10.35 µs/tx（+1.7%），import 1.01 → 1.02 µs/tx。
- 所有运行打包率 100%，0 错误。绝对值低于 09-08 报告（当天 exec 10k 约 148.7k），
  跨日的机器状态不同，只看同对内的 off/on 对比。

## snapshot 覆盖率（导入前最后一次发布的 snapshot 账户数 / 块内交易数）

| 模式 | 中位账户数 | 覆盖率 |
|---|---:|---:|
| exec 10k | 314 到 623 | 3% 到 5% |
| exec 50k | 约 2,100 | 约 4% |
| exec 100k | 3,800 到 4,100 | 约 3.5% |
| sustained 50k | 1,800 到 2,000 | 4% 到 8% |
| openloop | 12,700 到 16,500（最大约 200k） | 89% 到 106% |

## 解读

- exec / sustained 覆盖率极低：每块之后池子是空的，worker 只赶上下一波提交的头几百到几千笔，
  然后迭代器取空、休眠 100 ms，接着 bench 立即 assemble；每次 head 变化又把缓存清零。
- openloop 有积压，覆盖率接近整块，但 import 没有变快（36.9 → 37.8 ms）。
  导入路径本来就有块内并行 prewarm，snapshot 只替换了本来已经并行化的读，省不出时间。
  同时 worker 在 assemble 期间没有被暂停，和 builder 争 CPU，每笔 assemble 成本 +1.7%，
  最终 realized TPS -1.7%。
- 上游的 7% 来自以太坊主网：12 s 一个 slot，池里的交易在 newPayload 之前有好几秒可以预热，
  而且 newPayload 就是全部延迟。Morph sequencer 的周期由 builder 主导，而 builder 用不上 snapshot。

## 结论

- 对 morph-reth 的最大 TPS 没有提升；sequencer 和同步节点都保持 `--engine.txpool-prewarming`
  默认关闭，代码和部署都不需要动。
- 若将来要重新评估，前提是先给 `engine_assembleL2Block` 的 build 路径接上 engine 的
  pause lease 和 cache（上游 `payload_builder_resources` 那套），否则 assemble 既拿不到收益又要付争抢成本；
  即使接上，收益上限也只在 import 那 11% 里。

## 复现

```bash
# 压测分支 worktree，二进制已编译（BUILD=0 复用）
for pair in 1 2 3; do
  for cfg in off on; do
    extra=""; [ "$cfg" = on ] && extra="--engine.txpool-prewarming"
    RETH_EXTRA_ARGS="$extra" NODE_LOG_FILTER="info,engine::tree::txpool_prewarm=debug" \
    RESULTS_DIR="/tmp/morph-reth-bench/prewarm-ab/full-pair${pair}-${cfg}" BUILD=0 \
    MODES="exec sustained openloop" WORKLOADS=eth-transfer RUNS=1 \
    EXEC_BLOCK_SIZES="10000 50000 100000" EXEC_BLOCKS=10 \
    SUSTAINED_TXS_PER_BLOCK=50000 SUSTAINED_BLOCKS=20 \
    OPENLOOP_TARGET_TPS=100000 OPENLOOP_DURATION_SECS=30 OPENLOOP_DRAIN_SECS=300 \
    ./local-test/run_full_benchmark.sh
  done
done
```

覆盖率来自 on 侧节点日志里 `published txpool prewarming snapshot` 的 `accounts=` 字段，
取每次 `Block added to canonical chain` 之前针对其父块的最后一次发布。
