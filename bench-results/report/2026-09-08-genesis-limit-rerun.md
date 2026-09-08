# Genesis 驱动 payload 上限后的压测复核

日期：2026-09-08（Asia/Shanghai）

## 目的

压测分支合并了 `feat/genesis-payload-limit`（main v1.3.0 + PR #198）并删除了
benchmark-only 的 payload bypass（`--morph.benchmark-disable-tx-payload-limit`、
`MorphConsensus::without_tx_payload_size_limit`）。本次复核两件事：

1. 共识上限完全由 genesis `config.morph.maxTxPayloadBytesPerBlock` 决定，builder 与导入两侧一致；
2. 1 GiB genesis 上限下的执行吞吐与此前结论量级一致，去掉 bypass 没有改变执行路径。

## 受控条件

- Morph commit：`9ad7886d8`，release profile，`dirty_files = 0`
- 机器：Apple M4 Pro，14 核，48 GiB，macOS 25.6.0；同机压测，每个配置单次运行
- 节点 flag：`--builder.deadline 12 --morph.builder-use-reth-deadline`，无 payload bypass
- 发送账户 2,000，receiver mode `unique`，workload `eth-transfer`

## 上限验证（genesis = 737280）

| 用例 | 请求 txs/块 | 结果 |
|---|---:|---|
| exec 5000 | 5000 | 100% 打包，平均 30.6 ms/块，约 163k TPS |
| exec 10000 | 10000 | builder 在 6,827 笔（约 737 KB）处停下，fixed-size 模式按设计判失败 |

节点日志：`max_da_block_size=737280 chain_max_tx_payload_bytes=737280`，没有任何额外 flag。

## 正式结果（genesis = 1 GiB）

| 模式 | txs/块 | 块数 | 打包率 | asm ms | imp ms | total ms | p95 ms | avg TPS | realized TPS |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| exec | 10,000 | 5 | 100% | 59.4 | 8.7 | 68.1 | 77.5 | 148,681 | 146,837 |
| exec | 50,000 | 5 | 100% | 295.2 | 44.5 | 339.6 | 385.9 | 149,269 | 147,220 |
| exec | 100,000 | 5 | 100% | 619.6 | 89.1 | 708.7 | 825.1 | 143,828 | 141,106 |
| sustained | 50,000 | 20 | 100% | 472.1 | 47.1 | 1,262.4 | 1,334.0 | 39,690 | 39,609 |
| openloop | 100k target / 30 s | 70 | 100% | 414.0 | 37.2 | 454.6 | 1,940.6 | 81,983 | 76,056 |

openloop 细节：30 s 内提交 2,420,000 笔（计划 3,000,000），active 68 块
`active_avg_tps = 83,248`、`active_realized_tps = 76,501`，drain 2 块，0 错误。

## 解读

- exec 曲线在 1 万到 10 万笔/块之间基本平坦，块内执行速率约 14 万到 15 万 TPS，
  与 bypass 时代的量级一致，说明上限改由 genesis 提供后执行路径没有变化。
- sustained 的 total 包含提交与 txpool 接收（约 540 ms + 200 ms），其 TPS 列是端到端速率，不是执行速率。
- openloop 的发送端在同机上先于节点饱和（30 s 只送出计划量的 81%），realized 7.6 万反映发送侧受限，
  不代表节点上限。
- 单次运行，无重复性统计；与 8 月报告的 `legacy-small-set` 结果不可直接比较（receiver mode 不同）。

## 复现

```bash
# 720 KiB 生产上限（预期 5000 通过、10000 被顶住）
BENCHMARK_GENESIS_MAX_TX_PAYLOAD_BYTES=737280 MODES=exec WORKLOADS=eth-transfer RUNS=1 \
  EXEC_BLOCK_SIZES=5000 EXEC_BLOCKS=2 ./local-test/run_full_benchmark.sh

# 本次正式配置（默认 1 GiB 上限）
MODES="exec sustained openloop" WORKLOADS=eth-transfer RUNS=1 \
  EXEC_BLOCK_SIZES="10000 50000 100000" EXEC_BLOCKS=5 \
  SUSTAINED_TXS_PER_BLOCK=50000 SUSTAINED_BLOCKS=20 \
  OPENLOOP_TARGET_TPS=100000 OPENLOOP_DURATION_SECS=30 OPENLOOP_DRAIN_SECS=300 \
  ./local-test/run_full_benchmark.sh
```
