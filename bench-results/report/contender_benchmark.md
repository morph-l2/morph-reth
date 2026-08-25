# Contender 与 openloop 对比压测报告

日期：2026-08-25（Asia/Shanghai）

## 结论

Contender 可以复现 openloop 测到的 **Morph 节点峰值执行能力**，但在当前同机环境中不能持续生成与 openloop 相同强度的负载。

- 相同热地址 ETH 语义下，Contender 单次 200k 大突发的三轮链上均值为 **96,167.8 TPS**；openloop 的 `active_realized_tps` 为 **99,446.3 TPS**，差异 **-3.30%**。
- Contender 三轮每块平均执行能力为 **119,446.3 TPS**；openloop 的 `active_avg_tps` 为 **119,755.0 TPS**，差异仅 **-0.26%**。
- 因此节点执行能力基本一致。持续测试差异主要来自 Contender 的实时生成、签名、receipt 扫描和 SQLite 持久化，而不是 Morph 因交易来自 Contender 就变慢。

## 受控条件

- Morph commit：`e6db9852d4f6f299825f56dcce5e1a04c81a51ad`
- Contender：`0.10.3`，源码 commit `56825d79cafaa94af0377e7fd619eb650348c1a6`
- 发送账户：2,000
- JSON-RPC batch：500
- 交易：EIP-1559、1 wei ETH transfer
- 接收地址：10 个固定热地址，与 200k TPS、100 ms tick、2,000 senders 的 openloop `legacy-small-set` 地址数量一致
- Morph 交易 payload/block-size 限制：压测模式禁用
- Morph 出块：新增 `bench-block-exec run produce`，只调用 Morph 的 `engine_assembleL2Block` / `engine_newL2Block`，不自行发送交易
- 成功条件：Contender DB 行数、确认数、全局唯一 hash 数、出块区间交易数四者一致

Contender 内置 Engine API 使用标准 Engine 方法，不能直接驱动 Morph 的自定义 L2 Engine 方法，因此必须使用独立的 Morph producer。

## 结果

| 测试 | 配置 | 三轮链上 TPS | 均值 | CV | 对 openloop 99,446.3 |
|---|---:|---|---:|---:|---:|
| openloop 历史热地址基线 | 200k target，120 s | 汇总值 | 99,446.3 | — | 基线 |
| Contender 单次大突发 | 200k target，1 s，1 shard | 95,301.5 / 97,104.4 / 96,097.5 | **96,167.8** | 0.77% | **-3.30%** |
| Contender 单进程持续 | 25k target，15 s，1 shard | 25,380.7 / 24,887.2 / 25,316.7 | **25,194.9** | 0.87% | -74.66% |
| Contender 四进程持续 | 100k aggregate target，10 s，4 shards | 50,762.7 / 51,537.2 / 47,964.1 | **50,088.0** | 3.06% | -49.63% |

大突发测试每轮 200,000 笔，三轮合计 600,000 笔；单进程持续测试三轮合计 1,125,000 笔；四进程持续测试三轮合计 3,000,000 笔。所有有效轮次均为 0 失败、0 pending，并且四项计数完全一致。

### 为什么 200k/1s 能接近 openloop，持续测试却不行

Contender 的 `--tps` 是每秒计划生成的批次数量，并不保证每秒实际送达同样数量。其每秒批次需要实时准备和签名；当目标升高后，下一批开始时间会持续后移。

- 20k target：实际能够按 10 秒窗口完成。
- 25k target：接近边界，三轮估算实际发送均值 24,131.0 TPS。
- 30k target：300,000 笔的发送时间跨 10.702 秒，估算实际仅 25,636.6 TPS。
- 50k target：500,000 笔的发送时间跨 17.869 秒，估算实际仅 26,498.5 TPS。

所以直接用 `planned txs / configured duration` 会把 50k 档误报为 50k；实际链上窗口只有约 27.2k。

单次 200k/1s 测试只生成一个大批次，节点随后以约 96k TPS 排空，因此能验证峰值执行能力；它不能证明 Contender 可以持续提供 200k TPS。

## 已发现的风险和不严谨点

1. **配置 TPS 不是 achieved TPS。** 必须同时报告实际发送时间和链上区块窗口吞吐。
2. **一秒突发与 openloop 100 ms cadence 不同。** 同样平均 target 会产生不同的 txpool 波形和尾延迟。
3. **同机压测存在资源争用。** 四个 Contender 进程实时签名、扫 receipt、写 SQLite，降低了留给 Morph 的 CPU；openloop 在计时前预签名。
4. **相邻 seed 会生成重叠账户。** Contender 的派生近似为 `keccak(seed + i)`；4 个 shard 使用相邻 seed 时，相邻 500-account 池重叠 499 个账户，出现 `already known` 和 nonce gap。runner 已改为 seed 间隔 1,000,000，并跨所有 DB 检查全局唯一 hash。
5. **`db reset` 在 0.10.3 中不安全。** 命令先打开连接池，再删除自身正在使用的 DB，实测产生 `database is locked` / `disk I/O error`。runner 改用全新目录加 `db export` 完成初始化。
6. **长测试的 receipt 持久化会成为瓶颈。** 60 秒、150 万笔单实例测试中，链上已打包 1,502,000 笔（含 2,000 笔充值）、txpool 已空，但近 3 分钟后 SQLite 只写入 383,983 条；这段等待不能算节点性能。
7. **`end_timestamp` 不能直接与本测试的 wall-clock `start_timestamp` 相减。** Morph producer 使用基准链的递增 block timestamp；Contender DB 的确认端时间取自区块时间，直接相减会得到无意义负数。
8. **内置 workload 不完全等价。** 内置 ETH transfer 默认是 self-transfer；内置 ERC20 默认 fuzz recipient 且部署 Contender 自己的 TestToken。此次使用自定义 10-recipient ETH scenario，避免用不等价语义冒充 openloop 对比。
9. **Contender 的标准 Engine producer 与 Morph 不兼容。** `--fcu/--auth` 不能替代 Morph 自定义 L2 Engine 调用。

## 建议的使用方式

- 验证 Morph 峰值执行能力：保留 200k 单批突发，多轮报告链上窗口 TPS；本次已与 openloop 在 3.3% 内一致。
- 验证持续吞吐：优先使用 openloop，或把 Contender 放到独立压测机并横向分片；每个 shard 必须使用不重叠 seed 和独立 funder。
- 不以 Contender 的 `target TPS` 或 `planned/duration` 作为最终结果；最终结果以链上实际打包数、实际时间窗口为准。
- 大规模长测如使用 `--ignore-receipts`，必须完全由外部链上采集器补足成功率、pending、nonce gap 和 inclusion latency，不能只看 Contender 结束状态。

## 结果目录

- openloop 热地址基线：`/tmp/morph-reth-bench/e6db985-legacy-openloop-3runs-20260825`
- Contender 200k 单批三轮：`/tmp/morph-reth-bench/contender-hot-200k-1s-3runs-20260825`
- Contender 25k 单进程三轮：`/tmp/morph-reth-bench/contender-hot-25k-15s-3runs-20260825`
- Contender 100k/4 shard 有效样本 1：`/tmp/morph-reth-bench/contender-hot-100k-4shards-10s-valid-20260825`
- Contender 100k/4 shard 有效样本 2–3：`/tmp/morph-reth-bench/contender-hot-100k-4shards-10s-r2r3-20260825`
- Contender 项目研究：`bench-results/report/contender_research.md`
