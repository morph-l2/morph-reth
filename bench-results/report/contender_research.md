# Flashbots Contender 压测工具研究与 Morph 基准对照建议

> 研究日期：2026-08-25
> 一手资料范围：Flashbots 官方 `flashbots/contender` 仓库的 README、docs、源码、提交与 release。没有使用第三方文章。
> 源码基线：[`main@56825d79cafaa94af0377e7fd619eb650348c1a6`](https://github.com/flashbots/contender/commit/56825d79cafaa94af0377e7fd619eb650348c1a6)，提交日期 2026-07-31。
> 最新稳定标签：[`v0.10.3@596766b129aa458fc83fd755edba475f091239b7`](https://github.com/flashbots/contender/releases/tag/v0.10.3)，2026-06-23；本次检查的 `main` 只比该标签多一个 OpenSSL 依赖升级提交。
> Morph 对照基线：[`morph-reth@e6db9852d4f6f299825f56dcce5e1a04c81a51ad`](https://github.com/morph-l2/morph-reth/commit/e6db9852d4f6f299825f56dcce5e1a04c81a51ad)。

## 1. 结论先行

Contender 的核心定位是“通过标准 JSON-RPC 向一个正在出块的以太坊网络发送可配置交易，并追踪其后续落块情况”。它很适合：

- 做外部 RPC/txpool 压力、目标发送速率扫描、真实区块中的 inclusion/TTI 观察；
- 用 TOML 表达合约部署、setup、spam、fuzz 和多场景 campaign；
- 在固定 seed 下重复生成相同账户和 fuzz 参数；
- 对一个外部运行的节点做黑盒负载测试。

但它不能直接替代本仓库 `bench-block-exec` 的 `exec` 模式，也不能把命令行 `--tps` 或 Contender 报告里的任意 “TPS” 字段直接解释成 Morph 的真实执行 TPS。源码审计确认了以下关键事实：

1. **`--tps` 是目标发送量，不是实际成功提交、更不是实际落块 TPS。** TimedSpammer 每秒触发一次；触发后客户端还要顺序完成该批 request 的 nonce/gas 补全与本地签名，再把 N 笔作为突发批次并发发送。下一 tick 又必须先 collect 上一批 send tasks；发生延迟时采用 `MissedTickBehavior::Delay`，不会补发追赶。因此目标 100k TPS 实际是“每秒最多一次 100k 突发”，并且可能先被 Contender 客户端 CPU/签名/JSON-RPC 能力卡住。[TimedSpammer 源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/timed.rs#L35-L63)、[顺序准备与签名](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1060-L1162)、[批次执行源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1561-L1629)
2. **默认 ETH `transfers` 不是“唯一接收地址”负载。** 未指定 recipient 时，接收者是 `{_sender}`，即每个发送者给自己转账；状态热点数量约等于发送者数量。默认 ERC20 则相反：未指定 recipient 时会对 `transfer` 的接收地址做 seed 驱动 fuzz，接近“不断触达新 token balance slot”；指定 `--recipient` 后又变成单热点地址。[ETH transfers 源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/default_scenarios/transfers.rs#L11-L72)、[ERC20 源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/default_scenarios/erc20.rs#L19-L137)
3. **单次 report 的“成功交易数”存在严重口径风险。** 未在等待窗口内确认的交易会被 `dump_cache` 写入 DB，`block_number=None`、`gas_used=None`，但如果之前没有明确 RPC/执行错误，`error=None`；report 随后用 `successful = total - error_count`，会把这些“未确认且无明确错误”的交易计为成功。[dump_cache 源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_actor.rs#L210-L232)、[report 成败统计源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/command.rs#L322-L373)
4. **campaign 在日志不完整时会回退到计划交易数，并把错误数设为 0。** 这会令 campaign 的 `avg_tps` 接近配置目标、`error_rate` 接近 0，即便实际落块记录不足。官方文档承认“不完整时错误数可能低估”，源码明确展示了 `planned tx_count + errors=0` 的回退。[campaign 文档](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/campaigns.md#L94-L103)、[campaign 回退源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/command.rs#L538-L572)
5. **JSON-RPC batch 路径有测量完整性风险。** 整批 HTTP 失败或响应 JSON 解析失败时函数直接返回，没有给该批每笔交易写失败记录；响应处理按数组下标对齐交易而不按 JSON-RPC `id` 匹配，响应缺项会被当作 `error=None` 的发送成功。若服务端重排响应或返回部分响应，失败会错配或漏记。这是根据源码控制流得到的推论。[batch 发送与响应处理](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1439-L1555)
6. **Contender 单次 HTML/JSON report 本身没有一个严格的端到端 realized TPS。** 它主要报告 DB 交易数、失败数、gas、区块时间、RPC latency 和 TTI。进度日志中的 `current_tps` 是“从运行开始到当前的累计已确认成功数 ÷ elapsed”，不是瞬时 TPS；`txs_sent` 的当前实现实际上退化为 `confirmed + failed`，不是 RPC 提交计数。[进度统计源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/util.rs#L129-L188)
7. **因此，Contender 可以加入我们的交叉验证，但只能公平对照 `local-test openloop`，不能拿来反驳或替换 `exec` 结果。** 最稳妥做法是把 Contender 仅作为负载发生器，用独立的链上区块/receipt 统计计算成功 inclusion、active/drain realized TPS，再与本仓库 openloop 的同口径数据比较。

## 2. 无法复原同事之前的 Contender 测试

已搜索当前工作区以及仓库全部 Git 历史，没有找到以下任何材料：

- `contender` 命令或脚本；
- Contender scenario/campaign TOML；
- Contender SQLite DB、CSV、HTML/JSON report；
- seed、发送账户、recipient 模式、RPC batch、receipt 等参数记录；
- README、issue 或提交信息中的 Contender 引用。

所以目前**不能复原同事当时的测试口径，也不能把同事的 Contender 数字与本轮 `bench-block-exec` 数字直接相减**。至少需要向同事索取：

1. Contender 精确 tag/commit 或容器 digest；
2. 完整命令行和所有环境变量；
3. scenario/campaign TOML 原文件及哈希；
4. `CONTENDER_SEED`、`--accounts-per-agent`、私钥/账户集合说明；
5. `--tps/--tpb`、`--duration`、`--rpc-batch-size`、`--ignore-receipts`、`--optimistic-nonces`、`--pending-timeout`；
6. ETH/ERC20 recipient 的生成方式；
7. Contender SQLite DB、每轮 CSV、HTML/JSON report 和原始 stdout/stderr；
8. Morph 节点 commit、genesis、启动参数、payload/gas/txpool 限制和节点日志；
9. 测试是否共享链、是否存在其他流量，以及墙钟开始/结束时间。

尤其需要 SQLite DB/CSV，而不能只要截图或最终 TPS；否则无法区分 confirmed、reverted、RPC rejected、pending timeout 和完全漏记的 batch。

## 3. 架构和完整压测流程

官方架构把系统拆成 Generator、Spammer、Callback、Database、CLI 和 Report Generator。[官方架构图](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/architecture.md#L1-L25)

实际源码流程如下：

```text
scenario TOML / builtin
  -> TestConfig([env], [[create]], [[setup]], [[spam]])
  -> 根据 seed 和 from_pool 生成 signer pools
  -> 账户 funding、合约 deploy、setup、gas estimate
  -> 预生成 tx request/fuzz 值，并按 TPS/TPB × duration 切块
  -> 每个 trigger 时补 nonce/gas、签名
  -> eth_sendRawTransaction / JSON-RPC batch / sendRawTransactionSync
  -> callback 将 hash 与发送时间放进 TxActor cache
  -> TxActor 每秒观察新块并批量取 receipts
  -> confirmed/reverted 写 SQLite；结束时等待或 dump pending
  -> report 再从 SQLite 和节点读取 block/trace，生成 CSV/HTML/JSON
```

关键实现位置：

- TOML 顶层结构是 `[env]`、`[[create]]`、`[[setup]]`、`[[spam]]`。[TestConfig](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/testfile/src/test_config.rs#L12-L27)
- 未声明 sender 时，spam 默认使用 `from_pool="spammers"`，setup/create 默认 `admin`。[默认 pool 注入](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/testfile/src/test_config.rs#L154-L215)
- 默认 spam pool 有 10 个账户，CLI `-a/--accounts-per-agent` 可以覆盖。[AgentSpec](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/generator/agent_pools.rs#L9-L43)、[CLI 参数](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/commands/common.rs#L113-L131)
- seed 加 pool name 派生私钥，因此固定 seed + pool name + 账户数可稳定复现 signer 集合。[SignerStore 生成](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/agent_controller.rs#L128-L158)
- spam 总 request 数预生成后按 `txs_per_period` 切块。[预生成与切块](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1865-L1879)
- 默认 receipt actor 通过 `eth_getBlockReceipts`，不支持时回退为块内逐笔并发取 receipt，然后按 hash 匹配。[receipt 采集](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_actor.rs#L597-L700)
- 报告会额外取整个相关区块范围并执行 trace；所以 report 阶段本身也会给节点带来较重的读取/trace 负载，不能与 spam 热区间重叠运行。[block/trace 采集](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/block_trace.rs#L69-L206)

### TPS 与 TPB 两种触发模式

| 模式 | 实际行为 | 重要含义 |
|---|---|---|
| `--tps N` | 每 1 秒产生一个 trigger；一次性并发发送 N 笔 | 是 1 Hz burst，不是均匀泊松/平滑流量 |
| `--tpb N` | 订阅新区块；每观察到一个新区块后发送 N 笔 | 通常是为后续区块制造 pending，不表示本区块必含 N 笔 |
| `--duration D` | TPS 模式取 D 个秒 trigger；TPB 模式取 D 个 block trigger | 如果发送批次超过周期，TPS 模式墙钟时长可超过 D 秒 |
| `--forever` | 重复运行上述 D-period spam batch | 默认 nonce 同步可能在后续批次报错，源码会提示使用 optimistic nonces |

Timed 模式细节见 [timed.rs](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/timed.rs#L35-L63)，TPB 监听见 [blockwise.rs](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/blockwise.rs#L31-L61)。

### 标准发送路径的并发

每个 tick 触发后，`prepare_spam` 在单个 async 控制流中逐笔完成 request、nonce/gas 和本地签名，然后才进入发送。无 JSON-RPC batching 时，每个 period 会给每笔交易 spawn 一个任务，没有单独的 spam-send semaphore；例如 `--tps 100000` 会瞬时创建约 100,000 个发送任务。开启 `--rpc-batch-size 64` 后，每 64 笔组成一个 HTTP POST，但每个 POST 仍作为任务并发启动。Contender 会在下一 period 开始前收集上一批发送任务结果；如果准备+发送超过 1 秒，后续 tick 被 delay，而不是补齐原目标。[顺序准备/签名](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1060-L1162)、[非 batch 任务创建](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1213-L1357)、[batch 任务创建](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1439-L1558)、[跨 tick 收集](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1561-L1631)

因此在高目标 TPS 下，结果可能先受 Contender 的单线程批次准备/签名、进程调度、socket/FD、HTTP 连接池或 RPC batch 实现限制，而非节点执行能力。正式测试必须独立监控 Contender 客户端 CPU、RSS、签名/prepare 墙钟、实际 sending 墙钟、RPC latency 和 unique RPC accepted 数；`rpc_batch_size` 不同的两组测试不能直接比较 RPC latency 或极限负载。

## 4. 交易、发送地址和接收地址语义

### 4.1 Sender

- `from_pool` 指定一个逻辑 signer pool；同一 pool 内按生成索引取 `idx % signer_count`，从而轮转账户。[sender 选择](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/generator/trait.rs#L211-L247)
- 未指定 `from/from_pool` 的 spam 自动进入 `spammers` pool；默认 10 个账户。
- `--override-senders` 会把全部 sender 换成单一主账户，显著改变 nonce 串行度和状态热点；campaign 多 mix 下官方直接禁止此组合以避免 nonce 冲突。[override 实现](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/testfile/src/test_config.rs#L82-L140)、[campaign 限制](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/campaigns.md#L94-L103)
- nonce 在客户端本地递增；发送错误只会调整下一批的 nonce/gas bookkeeping，不会自动重发当前失败交易。[nonce 准备](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L979-L1038)、[错误调整](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1991-L2060)

### 4.2 Recipient：不能只写“ETH transfer/ERC20 transfer”而不写模式

| Contender workload | 默认 recipient | 状态语义 | 与本仓库对应关系 |
|---|---|---|---|
| builtin `transfers` | `{_sender}` | sender 给自己转 ETH；固定 sender 状态集，不创建新接收账户 | 既不等于 `unique`，也不等于 April `legacy-small-set` |
| builtin `transfers --recipient X` | 单一固定 X | 极端热点接收地址 | 比 `legacy-small-set` 更热 |
| builtin `erc20` | seed 驱动 fuzz `transfer(guy, wad)` 的 `guy` | 大量新 ERC20 balance storage slots，接近 unique state growth | 可作为 `unique` ERC20 的近似，但必须验证实际唯一数 |
| builtin `erc20 --recipient X` | 单一固定 X | 单一 token balance 热点 | 比本仓库 legacy 5/25 地址更热 |
| 自定义 TOML | 由 `to/args/fuzz/env` 决定 | 完全取决于配置 | 必须保存 TOML 和解析后的 tx 样本 |

值得注意的是，Contender 的 fuzz 只覆盖函数参数、tx value 和 priority fee；直接 ETH transfer 的 `to` 字段没有一个等价于本仓库 `receiver_mode=unique` 的内置逐笔地址 fuzzer。[FuzzParam 定义](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/generator/function_def.rs#L199-L215)

另外，自定义 scenario 有多个 `[[spam]]` step 时，生成器按 step 外循环、样本索引内循环生成请求，不应未经验证就假设多个 step 在每个 1 秒 batch 内均匀交错。[spam 生成循环](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/generator/trait.rs#L590-L678)

### 4.3 可复现性

固定 seed 会稳定派生 sender 和 fuzz 序列，但“报告文件”没有完整记录下面这些实验变量：

- Contender Git commit/容器 digest；
- scenario 文件内容或哈希、`-e KEY=VALUE` overrides；
- seed、accounts-per-agent、具体 sender/recipient 唯一数；
- tx type、gas price、min balance；
- RPC batch size、ignore receipts、optimistic nonces、专用 `--txs-url`；
- 节点版本、genesis、节点 flags、数据库初始状态和硬件环境。

JSON report 的 metadata 有 Contender crate version 和有限 runtime params，但 `RuntimeParams` 只有每 period 交易数、duration 和 timeout。[JSON export](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/gen_html.rs#L74-L125)、[RuntimeParams](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/command.rs#L806-L826)

所以正式实验必须额外生成自己的 `metadata.json`，不能只保留 Contender HTML。

## 5. 指标到底如何采集

### 5.1 发送、确认、失败

| 字段/概念 | 源码实际定义 | 不应解释为 |
|---|---|---|
| CLI `--tps` | 每秒计划生成并发送的请求数 | 实际提交 TPS、落块 TPS、节点容量 |
| send success | `eth_sendRawTransaction` RPC 没返回 error | 最终落块成功 |
| confirmed | DB row 有 `block_number` 且 `error=None` | 经过多确认/finality |
| failed | DB row 的 `error` 非空，包括明确 RPC error 或 receipt revert | 所有未成功交易；漏记和 pending timeout 可能不在其中 |
| pending dumped | 结束等待后仍在 cache，写 DB 但无 block/gas | 成功交易 |
| report successful | `all DB rows - rows with error` | 严格的链上成功数 |

默认路径不是逐笔 `await receipt` 后再发下一笔。它先发送并缓存 hash，独立 TxActor 按新区块扫描 receipt。发送完成后 `dump_tx_cache` 等 cache 缩小；若超过 pending timeout 没有进展，则把剩余项直接 dump 到 DB。[等待逻辑](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1882-L1916)

receipt 一旦在一个区块中出现就被当作 confirmed，没有额外 N-block confirmation 或 finalized-head 校验。因此这个“confirmed”严格说是“观察到一次 inclusion”，不等于抗 reorg finality。

receipt actor 优先按块调用 `eth_getBlockReceipts`；若失败则对该块全部交易并发逐笔调用 `eth_getTransactionReceipt`，没有单独 semaphore。逐笔 fallback 中的瞬时错误会被过滤掉，但 actor 仍会把 target block 向前推进，旧块不会在下一轮自动重扫；对应压测 tx 可能最终变成 pending timeout 记录。[flush loop](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_actor.rs#L531-L595)、[receipt fallback](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_actor.rs#L611-L641)

`--pending-timeout` 的 CLI 文案单位是“块”，实现却用链上第 1、2 块时间差估算 block time，再换算成墙钟秒数；若测试链早期两个块的间隔不代表当前出块节奏，真实等待窗口会偏离配置意图。[block time 推导](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/util.rs#L9-L31)、[换算使用](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/commands/spam.rs#L816-L858)

`--ignore-receipts` 会关闭成功交易的 receipt/cache 记录，但明确发送错误仍会入 DB；该模式不能拿来生成 inclusion 成功率。[NilCallback](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_callback.rs#L208-L238)

### 5.2 重试

- 普通交易发送和整批 HTTP POST 在失败时会**重试一次**。[retry_once](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L95-L109)
- scenario 初始化对“可恢复错误”最多尝试 3 次，退避 5 秒、10 秒。[初始化重试](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/commands/spam.rs#L991-L1030)
- 当前交易 RPC error 后不会再次生成 replacement；只按错误字符串调整后续 nonce 或 gas。重试一次如果第一次其实已被服务端接受但响应丢失，第二次返回 `already imported`，该交易仍可能被记录为 error。这会使“RPC failure rate”与真实链上 inclusion 不完全一致。
- CLI 的 `--timeout` 已明确 deprecated 且 currently does nothing，不能把它当作总测试超时。[参数源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/commands/spam.rs#L220-L227)

### 5.3 TTI（time to inclusion）

普通 `eth_sendRawTransaction`：

- start：发送任务调用 RPC 前的本机 `SystemTime`；
- end：receipt 所在 block header 的 timestamp × 1000；
- TTI：`end - start`。

[发送时间捕获](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1230-L1233)、[block timestamp 写入](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/tx_actor.rs#L677-L692)

如果本机时钟比 block timestamp 超前，chart 代码把该样本平移成 **0ms** 防下溢，而不是标记 clock skew 或丢弃；这会在人为出块、秒级 block timestamp 或主机时钟不一致时制造虚假的 0ms TTI。[TTI chart](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/chart/time_to_inclusion.rs#L18-L37)

`--send-raw-tx-sync` 则以同步 RPC 返回时刻作为 end；它与 `--rpc-batch-size` 不兼容，CLI 会关闭 batching。这个 TTI 的精度依赖目标节点对非标准 `eth_sendRawTransactionSync` 的语义。[CLI 参数](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/commands/spam.rs#L211-L218)、[sync 发送路径](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/test_scenario.rs#L1233-L1313)

TTI histogram 只检查 `end_timestamp_ms`，不要求 `block_number` 存在，也不排除 `error`。普通 async 路径的 pending/send failure 通常没有 end，因而被排除并形成 survivor bias；但 sync 路径在解析 RPC error 之前已经写入 end，因此 sync RPC error 也可能进入名为 “Time to Inclusion” 的图。正式报告必须同时给出 TTI 样本数、其中 confirmed/error/pending 的拆分、计划数和发送记录数。[TTI 筛选源码](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/chart/time_to_inclusion.rs#L18-L31)

### 5.4 RPC latency

Tower middleware 从发起 RPC 到 transport future 返回计时，并按 RPC method 写 Prometheus histogram；transport `Err` 不进入 histogram。默认边界只有 0.1ms、1ms、10ms、50ms、100ms、250ms、500ms。[middleware](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/provider.rs#L68-L109)、[bucket 配置](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/provider.rs#L113-L126)

report 的 p50/p90/p99 是对 histogram bucket 做线性插值估计，不是原始 latency 样本分位数。[分位估计](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/buckets.rs#L62-L89)

需要额外验证慢于 500ms 的样本是否通过采集路径保留；当前收集器只读取 histogram buckets，并以最后一个 bucket 的累计数作为 quantile denominator。若 `+Inf` 没进入该 vector，>500ms 尾部将不参与 p99。这是依据当前 collector 实现得到的风险判断。[collector](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/buckets.rs#L30-L59)

JSON-RPC batching 下，一个 HTTP POST 只有一个 latency observation，而不是每笔交易一个；所以 batch size 改变后，RPC latency 图的样本单位也改变。

还有一个进程内累计风险：CLI 的 Prometheus registry/histogram 是全局 `OnceCell`，每次 run 结束会把当前 registry 的累计 bucket 写到该 run_id；同一进程中的 campaign/`--forever` 多轮如果 registry 不重置，后轮可能再次包含前轮样本，随后跨 run report 又把 buckets 相加。[全局 registry](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/cli/src/lib.rs#L9-L12)、[每轮采集写入](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/core/src/spammer/spammer_trait.rs#L119-L123)、[SQLite 插入](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/sqlite_db/src/db.rs#L565-L583)。这是需要用单进程多轮小实验验证的重复计数风险。

### 5.5 区块 gas、peak tx count 和 TPS

report 会读取从第一笔 landed tx 到最后一笔 landed tx、前后各 padding 3 个区块的**完整区块**；`peak_tx_count` 和 `gas_per_block` 统计完整区块中的全部交易/全部 gas，而不是只统计 Contender 交易。[block range](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/block_trace.rs#L107-L134)、[peak 指标](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/command.rs#L250-L277)

因此在共享网络上，其他用户流量会抬高 peak tx count/gas；只有在隔离、fresh chain 上才能把它们近似归因于本次压测。

campaign 的 `avg_tps` 是 `total_tx / (max end - min start)`。其中 total 可能包含 error/pending；日志不完整时还会使用 planned count。它不是“成功落块 TPS”。[campaign avg_tps](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/crates/report/src/command.rs#L659-L704)

## 6. 可能产生误导结果的风险清单

这里的“漏洞”指压测测量完整性/口径漏洞，不是链上安全漏洞。

| 严重性 | 风险 | 可能产生的误导 |
|---|---|---|
| 严重 | pending timeout 后 `error=None` 的未确认交易被 report 计为 successful | 成功率虚高，甚至把未落块交易当成功 |
| 严重 | campaign 日志不完整时用 planned count、errors=0 | avg TPS 回到目标值，error rate 归零 |
| 严重 | batch 整体 HTTP/JSON 失败直接 return，不逐笔写失败 | DB 分母缺失，失败率虚低 |
| 高 | batch response 按数组位置而非 id 配对；缺项默认无 error | 部分响应/重排时错误错配或误判成功 |
| 高 | `--tps` 每秒一次 burst，慢时 Delay 不追赶 | 目标 TPS 与真实 offered rate 分离；RPC 突发特征不真实 |
| 高 | ETH builtin 默认 self-transfer，ERC20 默认 unique-like fuzz | 同名 “transfer” 比较的是完全不同状态增长模型 |
| 高 | report 的 total/success/failure 分母只覆盖已写 DB row | 漏记的请求不进入失败率 |
| 中 | TTI 混用本机毫秒与 block 秒时间；负值修正为 0 | 0ms TTI 尖峰、分位数偏低 |
| 中 | TTI 只看有 end 的样本 | 慢/失败/pending 样本被排除，survivor bias |
| 中 | progress `current_tps` 是累计 confirmed/elapsed，`txs_sent` 不是真实 send counter | 被误读为瞬时 TPS/实际提交 TPS |
| 中 | report 区块指标含同区块所有外部交易 | 共享网络的 peak/gas 被外部流量污染 |
| 中 | RPC latency 是 bucket 估计，transport error 不采样，慢尾需验证 | p99 偏乐观，跨 batch 配置不可比 |
| 中 | 进程级全局 histogram 可能在 campaign/forever 后轮重复携带前轮累计值 | 跨 run 聚合 latency 重复计数 |
| 中 | receipt fallback 瞬时失败后仍推进 target block | 已落块 tx 可能被留成未确认记录 |
| 中 | 一次 transport retry 后的 already-imported 仍可记录为 error | RPC failure 与真实 inclusion 不一致 |
| 中 | confirmed 无 N-block/finality 校验 | reorg 网络中 inclusion 被当作最终成功 |
| 低 | report 元数据没有记录关键配置 | 实验无法独立复现 |

## 7. 与本仓库 local-test 做公平对照

本仓库当前支持的三种模式定义见 [`local-test/BENCHMARK.md`](https://github.com/morph-l2/morph-reth/blob/e6db9852d4f6f299825f56dcce5e1a04c81a51ad/local-test/BENCHMARK.md#L12-L18)：

- `exec`：交易已经进入 txpool 后，只测 assemble + import；
- `sustained`：每块串行完成 submit、pool acceptance、assemble、import；
- `openloop`：提交与 assemble/import 两条 pipeline 并行。

### 7.1 哪些能比，哪些不能比

| 对比 | 是否公平 | 原因 |
|---|---|---|
| Contender vs local `exec` | 否 | Contender 包含 RPC、签名/发送、txpool、出块节奏和 receipt；exec 故意排除这些 |
| Contender vs local `sustained` | 只能做辅助 | 都含 submit，但 local 是受控逐块闭环，Contender 是外部 timed/block trigger |
| Contender vs local `openloop` | 可以，但要重算统一指标 | 都是外部持续提交 + 并行出块；driver pacing 和 accounting 仍不同 |

local openloop 默认每 100ms 一个 tick、JSON-RPC batch 64、HTTP concurrency 64，并在截止时间停止调度；Contender timed 是每 1 秒一个 tick且标准路径无等价 bounded concurrency。[local openloop pacing](https://github.com/morph-l2/morph-reth/blob/e6db9852d4f6f299825f56dcce5e1a04c81a51ad/bin/bench-block-exec/src/mode_openloop.rs#L24-L81)、[local bounded submit](https://github.com/morph-l2/morph-reth/blob/e6db9852d4f6f299825f56dcce5e1a04c81a51ad/bin/bench-block-exec/src/mode_openloop.rs#L269-L389)

因此即使都写 `--tps 200000`，两者也不是相同到达过程。公平结论应建立在独立链上 realized TPS 上，而不是 target TPS 或两端工具自己的 “avg TPS”。

### 7.2 必须锁定的变量

1. 同一台机器、同一 CPU/电源状态，正式测量时不同时跑两个 driver；
2. 同一个 `morph-reth` commit、release profile、genesis、fresh data dir；
3. 同样的 block gas limit、txpool limit、payload-limit bypass、builder deadline、engine persistence flags；
4. 相同交易类型、chain id、fee、gas limit、calldata、合约 bytecode；
5. 相同 sender 数（当前 local 默认 2,000）、每 sender nonce 分布和余额；
6. 相同 recipient 唯一数/复用规律，明确标为 `unique` 或 `legacy-small-set`；
7. 相同 active duration、drain deadline、warmup 和 fresh-node run 数；
8. 相同 RPC 拓扑，不能一边单 endpoint、一边多 endpoint；
9. 明确 JSON-RPC batch size 和最大并发；
10. 每组至少 3 个 fresh-node run，报告每轮结果、均值/中位数、CV 和失败数。

本分支按用户要求关闭了 Morph DA/payload size 限制；若使用 Contender 对照，也必须启动同一个已经 bypass builder/import payload cap 的节点。Contender 自己不会关闭该限制。

### 7.3 交易模型对齐方案

#### ETH

不要直接使用 builtin `transfers` 默认值来声称与 local unique/legacy 相同：默认 self-transfer 明显不同。

- 对照 `legacy-small-set`：需要定制 Contender scenario/generator，严格按本仓库每个 100ms tick 的 batch-local index 复用 recipient；仅指定一个 `--recipient` 只能得到 single-hot，不等于 5/25 地址。
- 对照 `unique`：需要逐笔产生 sender+nonce 唯一 recipient。标准 scenario 的 ETH `to` 没有逐笔 fuzz，可能要用 `spam-stream` 输入精确 tx spec 或做小补丁；但 `spam-stream` 是 prototype，发送/重试/batching 语义又不同，必须单独标注。[stream mode 状态与限制](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/stream-mode.md#L1-L15)、[stream mode out-of-scope](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/stream-mode.md#L142-L165)

#### ERC20

- builtin 默认 fuzz recipient 可近似 local unique，但必须导出样本确认 `unique_recipient_count / tx_count`；
- 显式单 recipient 只是 single-hot；要复现 local `legacy-small-set` 的每 tick 5 地址，需要定制生成顺序，不能只写 5 个 `[[spam]]` 就假设每 batch 均匀轮转。

### 7.4 统一、可靠的结果口径

每轮至少输出以下 5 个互不混淆的数字：

1. `planned_txs = target_tps × nominal_duration`；
2. `rpc_accepted_txs`：RPC 明确成功响应的唯一 tx hash 数；
3. `included_success_txs`：active + drain 窗口内 receipt status=1 的唯一压测 tx 数；
4. `reverted_txs`：receipt status=0；
5. `unresolved_txs = planned - accepted error - included - reverted`，并单列 duplicate/nonce/fee errors。

TPS 至少同时给：

- `offered_actual_tps = actual RPC attempts / active wall time`；
- `accepted_tps = unique RPC accepted / active wall time`；
- `active_realized_tps = active 期间 included success / active wall time`；
- `end_to_end_realized_tps = active+drain included success / (active + drain wall time)`；
- `block_execution_tps = tx_count / assemble+import time`，仅 local tool 能直接、严格提供，不能从 Contender TTI 反推。

最终守恒式必须成立或解释差额：

```text
planned = not_scheduled + rpc_rejected + accepted
accepted = included_success + included_revert + pending_at_deadline + dropped/reorged
```

Contender 自带 report 不足以证明这个守恒式，需要用 SQLite raw rows、RPC 原始响应、节点区块/receipt 和节点日志独立核对。

## 8. 推荐的 Contender 复测步骤

1. 先拿到同事旧材料；没有材料时，将旧结果标为“不可复现历史数据”，不要纳入回归百分比。
2. 固定 `v0.10.3` 或当前 `56825d7`，记录 source SHA；不要使用浮动 `cargo install --git ...`。
3. 新建一个明确记录 recipient 语义的 scenario，并在小样本预生成后导出前 100/1000 笔的 `(from,to,nonce,calldata)` 核验。
4. fresh node + fresh Contender DB；setup/report 阶段不得混入 active 测量窗口。
5. 先跑 offered-load 曲线，而不是只跑一个 200k：例如 40k、60k、80k、100k、120k、160k、200k，每档 120 秒 + 足够 drain，3 轮。
6. 同时跑两种 receiver workload：strict unique 与 strict legacy-small-set；不要拿 self/single-hot 代替。
7. 固定 2,000 sender、fee、RPC endpoint、batch size。若无法让 Contender pacing/concurrency 与 local 一致，就明确把它当作“不同 driver 的交叉验证”。
8. 每轮保存 Contender DB export、CSV/JSON、stdout/stderr、节点日志、metadata、区块/receipt 独立导出。
9. 只有当 `rpc_accepted == included + reverted + pending` 可闭合，且 `pending=0`，才报告成功 realized TPS。
10. 把 Contender 结果与 local openloop 的 `active_realized_tps` / total realized 对比；不要与 local exec TPS 直接对比。

## 9. 对同事历史结果的暂定判断框架

拿到历史命令后，首先按下面顺序分类：

1. 如果用的是 `transfers` 默认 recipient，则这是 self-transfer 热点 sender 集，不是当前 local unique；性能偏高是预期。
2. 如果用的是 ERC20 默认 recipient fuzz，则更接近 unique state growth；如果显式 `--recipient`，则是 single-hot，性能会明显偏高。
3. 如果数字直接取自 `--tps` 参数，不能视为测量结果。
4. 如果数字取自 campaign `avg_tps`，先检查 `logs_incomplete`；为 true 时不能用于性能结论。
5. 如果成功率来自 single report，必须重新按 `block_number.is_some() && error.is_none()` 计算；不能使用 `successful=total-errors`。
6. 如果开启 RPC batching，检查 batch 原始响应和 DB 数量守恒；否则失败可能漏记。
7. 如果没有 drain 后的 receipt/accounting，数字最多代表 load generation，不代表节点吞吐。
8. 如果测试是在共享链上，peak tx/gas 必须按压测 sender/hash 过滤，不能使用完整区块总量。

## 10. 一手来源索引

- [官方 README（定位、安装、基本命令）](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/README.md)
- [官方 Architecture](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/architecture.md)
- [官方 CLI Reference](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/cli.md)
- [官方 Scenarios](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/scenarios.md)
- [官方 Campaigns](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/campaigns.md)
- [官方 Reports/DB](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/reports-db-admin.md)
- [官方 Engine API](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/engine-api.md)
- [官方 Stream mode](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/docs/stream-mode.md)
- [v0.10.3 release](https://github.com/flashbots/contender/releases/tag/v0.10.3)
- [官方 CHANGELOG](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/CHANGELOG.md)

### 版本敏感性

Contender 的发送与报告语义近几版变化很快：v0.7.3 才把 on-chain revert 写为 error，v0.8.1 修改 TimedSpammer 规律性和 ERC20 默认参数，v0.9.x 增加 sync raw tx、live progress 与 RPC latency，v0.10.0 才增加 total/success/failure report，并改善 ignore-receipts 下错误记录。因此历史结果如果没有精确版本，不能用当前源码倒推其口径。[官方 CHANGELOG](https://github.com/flashbots/contender/blob/56825d79cafaa94af0377e7fd619eb650348c1a6/CHANGELOG.md#L1-L100)
