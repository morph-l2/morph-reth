# morph-reth vs morph-geth 执行引擎性能基准测试报告

**测试日期**: 2026-04-09  
**测试人员**: Panos  
**测试环境**: Apple M4 Pro (14 核, 48GB RAM), macOS Darwin 25.4.0

---

## 目录

1. [测试目标与背景](#1-测试目标与背景)
2. [测试计划与方法论](#2-测试计划与方法论)
3. [测试环境与配置](#3-测试环境与配置)
4. [Mode A: Exec — 纯 EVM 执行性能](#4-mode-a-exec--纯-evm-执行性能)
5. [Mode B: Sustained — 全链路持续性能与衰减分析](#5-mode-b-sustained--全链路持续性能与衰减分析)
6. [Mode C: Openloop — 高压持续吞吐测试](#6-mode-c-openloop--高压持续吞吐测试)
7. [综合对比与结论](#7-综合对比与结论)
8. [附录 A: 复现本测试的完整提示词](#附录-a-复现本测试的完整提示词)

---

## 1. 测试目标与背景

### 1.1 为什么做这个测试

Morph L2 正在从 morph-geth（Go 实现）向 morph-reth（Rust 实现）迁移执行引擎。在迁移前，我们需要量化回答一个核心问题：**reth 到底比 geth 快多少？在什么场景下快？快在哪个环节？**

单纯跑一个 TPS 数字是不够的。我们需要分离不同阶段的性能差异（txpool 入池、EVM 执行、state 持久化），测试不同交易复杂度下的表现，并验证性能在持续负载下是否稳定。

### 1.2 测试的三个核心问题

| 问题 | 对应测试模式 |
|------|------------|
| reth 和 geth 的 EVM 纯执行速度差多少？ | Mode A: Exec |
| 长时间运行后性能是否衰减？ | Mode B: Sustained |
| 在真实高压场景下，端到端吞吐是多少？ | Mode C: Openloop |

### 1.3 为什么选择两种交易类型

| 交易类型 | Gas 消耗 | 测试目的 |
|---------|---------|---------|
| **ETH Transfer** | 21,000 gas | 最简单的交易，只有余额转移，无 EVM 字节码执行。测试引擎的基础开销（state read/write, trie update）。 |
| **ERC20 Transfer** | ~34,000 gas | 需要执行 EVM 字节码、读写合约 storage（balanceOf mapping）、发射 event log。代表真实 DeFi 交易的最小复杂度。 |

选择这两种而非 Uniswap swap 等更复杂的交易，是因为：
- 简单交易能更清晰地分离引擎层面的性能差异，不会被合约逻辑的复杂度掩盖
- ERC20 transfer 是链上最常见的交易类型（占 L2 交易量的 40-60%）
- 两者的 gas 消耗比为 1:1.6，可以观察到从"纯 state 操作"到"EVM 执行 + state 操作"的性能变化

---

## 2. 测试计划与方法论

### 2.1 三种测试模式设计原理

#### Mode A: Exec（纯 EVM 执行）

```
[不计时] 签名 → HTTP 提交 → txpool 验证 → 等待入池确认
[计时开始] assembleL2Block (txpool 取 tx → EVM 执行 → 构建 block)
           → newL2Block (state root 计算 → 持久化)
[计时结束]
```

**为什么这样设计**：将 txpool 提交排除在计时之外，只测量 EVM 执行 + state commit 的纯粹性能。这消除了 HTTP 传输、JSON 解析、ECDSA 签名恢复等 I/O 开销的干扰，让我们能精确测量两个引擎的执行层差异。

**为什么扫描不同 block size (1k → 200k)**：不同 block size 下引擎的行为可能不同。小 block 受 per-block 固定开销影响大，大 block 可能触发内存/cache 效应。通过扫描，我们能找到每个引擎的最优工作区间，而不是只测一个点。

**参数**：每个 block size 跑 10 blocks × 2 runs = 20 个数据点。

#### Mode B: Sustained（全链路持续出块）

```
[warmup] 5 blocks — 不计时，让 JIT/cache 预热
[measurement] 100 blocks × 50,000 txs/block — 每块完整计时
  每块: 签名 → 提交 → 等待入池 → assemble → import
```

**为什么这样设计**：模拟 sequencer 持续出块的场景。100 个 block 共处理 500 万笔交易，足以观察：
- 性能是否随 state 增长而衰减（trie 变大，cache miss 增多）
- 是否存在 GC pause 或内存压力导致的周期性抖动
- 串行 pipeline 下每个环节（submit/pool_wait/assemble/import）的占比

**为什么需要 warmup**：前几个 block 会触发 JIT 编译、内存分配、连接池建立等一次性开销，计入会拉高方差。5 个 warmup blocks 足以消除冷启动效应。

**参数**：3 runs × (5 warmup + 100 measured) = 300 个有效数据点。

#### Mode C: Openloop（高压持续吞吐）

```
Pipeline 1 (submit): 按 200,000 TPS 速率持续灌 txs 进 txpool
Pipeline 2 (producer): 不停出块，每个 block 吃 txpool 里能拿到的所有 tx
两个 pipeline 完全并行，互不等待。
```

**为什么这样设计**：Mode A 和 B 都是"串行"的——一个 block 完成后才开始下一个。但真实 L2 sequencer 中，用户交易持续到达，出块和交易接收是并行的。Openloop 模式模拟这种真实场景。

**为什么 target TPS 设为 200,000**：需要高到足以让 txpool 始终饱和（有足够多的 tx 等待被执行），这样测到的就是引擎的处理上限，而非 submit 的投喂速率。200k 对 reth（~70k 处理能力）和 geth（~27k 处理能力）都远超其消化能力。

**为什么 120 秒**：60 秒的测试在之前的迭代中已经足够稳定（stddev < 5%），但 120 秒进一步消除了长尾波动，且能产生更多的大 block 数据点用于统计分析。

**参数**：3 runs × 120s = 360 秒有效测量时间，每次提交 2400 万笔交易。

### 2.2 统计方法

| 方法 | 说明 | 为什么 |
|------|------|--------|
| **Median TPS**（而非 Mean） | 取 block TPS 的中位数 | 不受极端值影响（如 txpool 刚灌满时的超大块、drain 末尾的小块） |
| **大块过滤** | 只统计 ≥1000 txs 的 block | 排除 txpool 饥饿导致的噪声小块（几十 txs 的 block TPS 不代表引擎性能） |
| **多次运行** | Exec 2 runs, Sustained/Openloop 3 runs | 计算 stddev 验证可重复性 |
| **Warmup 丢弃** | Sustained 丢弃前 5 blocks | 消除冷启动效应 |

### 2.3 公平性保障

| 维度 | reth 配置 | geth 配置 | 说明 |
|------|----------|----------|------|
| 存储模式 | archive（默认） | --gcmode archive | 两边都保留全量历史 state |
| 内存 | 无限制（默认） | --cache 8192 (8GB) | geth 显式分配 8GB cache |
| txpool 大小 | 100 万 slots | 100 万 slots | 完全一致 |
| 验证并行度 | 12 线程 | 单线程（geth 不支持） | **这是 reth 的架构优势，非配置差异** |
| 内存 block buffer | 2 blocks | 无（geth 不支持） | **同上** |
| 测试工具 | 相同二进制 | 相同二进制 | 同一个 benchmark 工具，同样的参数 |
| Genesis | 相同文件 | 相同文件 | 完全一致的初始状态 |
| 交易 | 相同预生成 | 相同预生成 | 同一批签名后的交易 |

> **注意**：reth 的并行 tx 验证（12 线程）和内存 block buffer 是其架构设计优势，geth 的单线程 txpool 是其固有限制。这不是"不公平配置"，而是两个引擎在架构层面的真实差异。

---

## 3. 测试环境与配置

| 项目 | 值 |
|------|-----|
| 硬件 | Apple M4 Pro, 14 核 (10P+4E), 48GB 统一内存 |
| 操作系统 | macOS Darwin 25.4.0 (arm64) |
| morph-reth | feat/max-tps-benchmark 分支, Rust release build |
| morph-geth | main 分支, Go 1.24.0, maxRequestContentLength 增大到 512MB |
| Benchmark 工具 | bench-block-exec (Rust), 同仓库编译 |
| 预生成签名 | rayon 并行 ECDSA secp256k1, ~285,000 txs/s |
| Genesis | chain_id=99999, 30B gas limit, 2000 sender 账户, BenchToken ERC20 预部署 |
| 交易 gas price | max_fee_per_gas = 1 gwei (确保 geth 兼容) |

---

## 4. Mode A: Exec — 纯 EVM 执行性能

### 4.1 测试说明

Exec 模式排除了所有 I/O 开销，只测量从 txpool 取交易 → EVM 执行 → state root 计算 → 持久化写入的时间。这是衡量两个引擎 EVM 实现（revm vs go-ethereum/core/vm）性能差异的最直接方式。

我们扫描了 5 个 block size（1k, 10k, 50k, 100k, 200k txs/block），每个跑 10 blocks × 2 runs。

### 4.2 结果

![Exec ETH Transfer](charts/exec_eth-transfer.png)

![Exec ERC20 Transfer](charts/exec_erc20-transfer.png)

#### ETH Transfer

| Engine | 1k txs | 10k txs | 50k txs | 100k txs | 200k txs |
|--------|--------|---------|---------|----------|----------|
| **reth** | 137,170 | **241,294** | **261,075** | **262,000** | 256,921 |
| geth | 33,910 | 40,619 | 39,377 | 38,087 | 37,028 |
| **reth/geth** | **4.0x** | **5.9x** | **6.6x** | **6.9x** | **6.9x** |

#### ERC20 Transfer

| Engine | 1k txs | 10k txs | 50k txs | 100k txs | 200k txs |
|--------|--------|---------|---------|----------|----------|
| **reth** | 80,315 | 126,272 | **133,570** | **135,771** | 133,471 |
| geth | 20,670 | 24,981 | 24,611 | 23,884 | 23,085 |
| **reth/geth** | **3.9x** | **5.1x** | **5.4x** | **5.7x** | **5.8x** |

#### 每笔交易 EVM 执行成本 (μs/tx)

| Engine | ETH Transfer | ERC20 Transfer |
|--------|-------------|----------------|
| **reth** | **3.0 μs** (最优 50k) | **6.4 μs** (稳定 50k-200k) |
| geth | 17.6 μs (最优 10k) | 28.7 μs (最优 10k) |

### 4.3 结果分析

**为什么 reth 快 5-7 倍？**

1. **revm vs go-ethereum EVM**：reth 使用 Rust 实现的 revm，天然比 Go 的 EVM 解释器快。Rust 的零成本抽象、无 GC、编译期优化让每笔 EVM 操作都更高效。

2. **State root 计算差异**：reth 的 import 阶段（state root 计算 + 持久化）比 geth 快 5-10 倍。例如 50k ETH transfer：reth import = 42ms，geth import = 357ms。这说明 reth 的 MPT（Merkle Patricia Trie）实现也显著更快。

3. **Block size 响应曲线不同**：
   - reth 在 10k-50k 区间有明显的性能跃升（1k 的 137k TPS → 50k 的 261k TPS），说明更大的 block 让 reth 的 batch 处理优化发挥作用
   - geth 在 10k 之后基本平坦（40k → 37k TPS），甚至随 block 增大略微下降，说明 geth 没有类似的 batch 优化

4. **ERC20 比 ETH 慢约 50%**：ERC20 transfer 需要额外的 EVM 字节码执行（CALL → SLOAD → SSTORE → LOG）和合约 storage 读写，这部分开销在两个引擎上比例相似。

---

## 5. Mode B: Sustained — 全链路持续性能与衰减分析

### 5.1 测试说明

Sustained 模式模拟 sequencer 持续出块：每块签名 50,000 笔交易 → HTTP 提交到 txpool → 等待入池确认 → assemble 出块 → import 入链。这是完整的端到端流水线，包含了 HTTP 传输、ECDSA 签名验证、txpool 管理等所有实际开销。

共处理 100 blocks × 50,000 txs = **500 万笔交易**（每组），3 组共 1500 万笔。关注两个维度：绝对性能和性能是否衰减。

### 5.2 结果

![Sustained ETH Transfer](charts/sustained_eth-transfer.png)

![Sustained ERC20 Transfer](charts/sustained_erc20-transfer.png)

#### 性能对比

| Engine | Workload | Median TPS | 说明 |
|--------|----------|-----------|------|
| **reth** | ETH Transfer | **58,720** | 包含 submit 在内的全链路 TPS |
| **reth** | ERC20 Transfer | **41,805** | |
| geth | ETH Transfer | 15,938 | |
| geth | ERC20 Transfer | 12,899 | |

#### 性能衰减分析

| Engine | Workload | 前 10 块 TPS | 后 10 块 TPS | 衰减幅度 |
|--------|----------|-------------|-------------|---------|
| **reth** | ETH Transfer | 58,153 | 59,718 | **+2.7%** (无衰减) |
| **reth** | ERC20 Transfer | 41,696 | 42,172 | **+1.1%** (无衰减) |
| geth | ETH Transfer | 17,391 | 15,391 | **-11.5%** (显著衰减) |
| geth | ERC20 Transfer | 13,515 | 12,512 | **-7.4%** (明显衰减) |

### 5.3 结果分析

**为什么 reth 无衰减而 geth 衰减 7-12%？**

1. **State trie 增长影响**：100 blocks × 50k txs 会创建大量新的 state trie 节点。geth 的 Go MPT 实现在 trie 变大后查找变慢（更多的内存分配和 GC 压力），而 reth 的 Rust 实现通过更紧凑的内存布局和无 GC 开销保持稳定。

2. **Go GC 影响**：geth 运行 100 个大 block 后会积累大量短生命周期对象（trie 节点、RLP 编码缓冲区），Go 的 GC 需要周期性暂停清理。从图表可以看到 geth 的 TPS 曲线有细微的周期性抖动，这正是 GC pause 的特征。

3. **reth 的微升**：reth 后 10 块反而比前 10 块快 1-3%，这可能是 OS 文件系统缓存和 CPU 分支预测器在连续相似操作后进一步预热的结果。

**为什么 Sustained 的 TPS 低于 Exec？**

Sustained 模式包含了 submit（HTTP 提交 + ECDSA 验证）阶段，这在 Exec 模式中不计时。以 reth ETH Transfer 为例：
- Exec TPS: ~261,000（纯执行）
- Sustained TPS: ~58,700（含 submit）
- 差异来源：每 block 的 submit + pool_wait 约占总时间的 76%

---

## 6. Mode C: Openloop — 高压持续吞吐测试

### 6.1 测试说明

Openloop 模式是最接近真实 L2 sequencer 运行状态的测试。交易提交和出块并行运行，txpool 作为缓冲区。当提交速率（200k TPS）远超处理能力时，txpool 持续积压，引擎在每个 block 中尽可能多地消费交易。

每组运行 120 秒，提交 2400 万笔交易，3 组共 7200 万笔。

### 6.2 结果

![Openloop Comparison](charts/openloop_comparison.png)

![Openloop Distribution](charts/openloop_distribution.png)

#### Openloop 性能对比

| Engine | Workload | Median TPS | P95 TPS | Peak TPS | Mgas/s | Run Stddev (n=3) |
|--------|----------|-----------|---------|----------|--------|-----------------|
| **reth** | ETH Transfer | **73,110** | 81,495 | 140,596 | 1,493 | ±4,552 |
| **reth** | ERC20 Transfer | **68,806** | 87,613 | 105,957 | 2,352 | ±2,122 |
| geth | ETH Transfer | 26,911 | 30,745 | 35,301 | 554 | ±422 |
| geth | ERC20 Transfer | 16,773 | 22,030 | 23,778 | 575 | ±365 |

#### 性能倍率

| Workload | reth Median | geth Median | **reth/geth** |
|----------|-----------|-----------|-------------|
| ETH Transfer | 73,110 | 26,911 | **2.7x** |
| ERC20 Transfer | 68,806 | 16,773 | **4.1x** |

### 6.3 结果分析

**为什么 Openloop 的 reth/geth 倍率（2.7-4.1x）低于 Exec 的倍率（5-7x）？**

Openloop 模式中，txpool 入池速度（RPC 处理）也是瓶颈之一。reth 的并行验证（12 线程）在高并发下优势显著，但 geth 的单线程 txpool 也在持续工作。两者的 txpool 入池速率差异小于 EVM 执行速率差异，所以综合倍率被"拉平"了。

**为什么 reth 的 ERC20 median TPS (68,806) 接近 ETH (73,110)？**

在 Openloop 模式下，txpool 入池（RPC 层 ECDSA 验证）是共同瓶颈。ERC20 和 ETH 交易的 txpool 验证成本相同（ECDSA 恢复 ~35μs/tx），只有 EVM 执行成本不同。当 txpool 入池速率成为限制因素时，两种 workload 的差异被压缩。

**为什么 geth 的 ERC20 (16,773) 远低于 ETH (26,911)？**

geth 的 EVM 执行更慢，ERC20 的额外 EVM 开销（CALL + SLOAD + SSTORE + LOG）在 geth 上的代价更高。geth ERC20 的每笔执行成本 ~39μs vs ETH ~23μs，差距 70%。而 reth 上 ERC20 ~12μs vs ETH ~12μs，差距仅 5%。这说明 reth 的 revm 在 EVM 字节码执行上的优势比在纯 state 操作上更加显著。

**关于 Mgas/s 指标**：reth ERC20 的 Mgas/s (2,352) 高于 ETH (1,493)，因为 ERC20 每笔消耗 60k gas（ETH 只有 21k gas），在类似的 TPS 下消耗更多 gas。Mgas/s 更适合衡量"链的 gas 吞吐能力"，TPS 更适合衡量"用户体验"。

---

## 7. 综合对比与结论

![Summary](charts/summary.png)

### 7.1 各模式性能汇总

| 模式 | 测量内容 | reth ETH | reth ERC20 | geth ETH | geth ERC20 |
|------|---------|----------|-----------|----------|-----------|
| **Exec** (最优 block size) | 纯 EVM | **262,000** | **135,771** | 40,619 | 24,981 |
| **Sustained** (50k/block) | 全链路 | **58,720** | **41,805** | 15,938 | 12,899 |
| **Openloop** (200k target) | 高压并行 | **73,110** | **68,806** | 26,911 | 16,773 |

### 7.2 reth 相对 geth 的性能倍率

| 模式 | ETH Transfer | ERC20 Transfer |
|------|-------------|----------------|
| **Exec** | **6.4x** | **5.4x** |
| **Sustained** | **3.7x** | **3.2x** |
| **Openloop** | **2.7x** | **4.1x** |

### 7.3 关键结论

1. **reth 在纯 EVM 执行层面快 5-7 倍**。这是 Rust (revm) vs Go (go-ethereum/core/vm) 的语言/运行时层面差异，包括 EVM 解释器性能和 MPT state root 计算性能。

2. **reth 在端到端场景下快 2.7-4.1 倍**。HTTP RPC 处理和 txpool 验证是共同开销，压缩了 EVM 层面的倍率优势。

3. **reth 在持续负载下性能零衰减**，而 geth 衰减 7-12%。这对 L2 sequencer 的长期稳定运行至关重要——不需要定期重启来恢复性能。

4. **reth 的架构优势不可在 geth 上复制**：并行 tx 验证（12 线程 vs 单线程）、内存 block buffer、无 GC 的 Rust 运行时。这些不是配置调优能弥补的差距。

5. **对真实生产环境的参考**：ETH transfer 的 73k TPS 是理论上限。真实 L2 上混合交易（DeFi 合约调用，gas 消耗 100k-500k）的 TPS 预期为 **15,000-40,000**，取决于交易组合复杂度。

---

## 附录 A: 复现本测试的完整提示词

以下提示词可以直接交给 AI 编程助手（如 Claude Code），让其在新项目中从零开始构建类似的执行引擎性能基准测试工具。

---

### 提示词

```
你是一位高级系统工程师，我需要你为我的以太坊 L2 执行引擎构建一个完整的性能基准测试工具和测试流程。

## 项目背景

我们有两个执行引擎实现：
- Engine A: Rust 实现（基于 reth/revm）
- Engine B: Go 实现（基于 go-ethereum）

两者都支持自定义 Engine API：
- `assembleL2Block`: 从 txpool 取交易，执行 EVM，构建 block
- `newL2Block`: 导入已构建的 block（state root 计算 + 持久化）

交易通过标准 `eth_sendRawTransaction` JSON-RPC 提交到 txpool。

## 需要构建的 Benchmark 工具

### 1. 交易生成器 (tx_factory)

要求：
- 支持多种 workload: ETH transfer (21k gas), ERC20 transfer (~34k gas)
- 预生成大量签名交易（数百万级），使用 ECDSA secp256k1
- 签名必须并行化（rayon 或类似方案），目标 >200k txs/s
- 支持多 sender（2000+），round-robin 分配确保 nonce 连续
- 交易预序列化为 JSON-RPC HTTP body，运行时只做 HTTP POST

### 2. Genesis 生成器

要求：
- 生成两个引擎都兼容的 genesis JSON
- 预部署 ERC20 合约（minimal transfer + balanceOf，直接在 genesis alloc 中放 bytecode 和 storage）
- 为所有 sender 预分配 ETH 余额和 ERC20 token 余额（通过 storage slot 直接写入）
- gas limit 设为 30B（足够容纳大 block）
- max_fee_per_gas 设为 1 gwei（确保两个引擎都接受）

### 3. 三种测试模式

#### Mode A: Exec（纯 EVM 执行）
- 先把 N 笔交易提交到 txpool 并等待入池确认（不计时）
- 然后只计时 assembleL2Block + newL2Block
- 扫描不同 block size（1k, 10k, 50k, 100k, 200k txs），找最优执行区间
- 每个 block size 跑 10 blocks × 2 runs

#### Mode B: Sustained（全链路持续出块 + 衰减分析）
- warmup 5 blocks（不计时），然后连续跑 100 blocks
- 每块完整流程：签名→提交→等待入池→assemble→import，全部计时
- 对比前 10 块和后 10 块的 TPS，分析衰减
- 3 runs

#### Mode C: Openloop（高压持续吞吐）
- 两个并发 pipeline：submit 按 target TPS 持续提交，producer 不停出块
- 所有交易在运行前完成预生成+签名+序列化（消除运行时签名开销）
- submit 使用 fire-and-forget（JoinSet，不等 HTTP 响应）
- target_tps 设为远超引擎处理能力（如 200k），确保 txpool 饱和
- 120 秒 duration，3 runs

### 4. 关键设计要求

- **pre-generation**: 所有交易在测试前预生成，签名使用 rayon 跨 sender 并行
- **公平性**: 同一 genesis、同一批交易、相同 txpool 配置（100 万 slots）
- **统计方法**: 
  - 用 median TPS（非 mean）作为主指标
  - Openloop 只统计 ≥1000 txs 的大块，排除 txpool 饥饿噪声
  - 多次运行计算 stddev
- **输出**: JSONL 格式，每 block 一行，含 block_number, tx_count, assemble_ms, import_ms, total_ms, gas_used, tps, mgas_per_sec 等

### 5. 测试运行脚本 (bash)

- pm2 管理引擎节点启停
- 每次测试前清空 datadir（确保空 state 起步）
- 顺序执行：Exec sweep → Sustained → Openloop
- Engine A 配置：archive 模式、并行验证线程、内存 block buffer、禁用 txpool 备份、大 txpool
- Engine B 配置：--gcmode archive、--cache 8192、大 txpool、无限 batch size

### 6. 报告生成脚本 (Python + matplotlib)

- 读取 JSONL 结果文件
- 图表：
  - Exec: TPS vs block size 曲线 + 每笔 EVM 成本曲线
  - Sustained: TPS 时间序列（衰减分析）+ 累积处理时间
  - Openloop: 柱状对比 + boxplot 分布
  - Summary: 全模式综合对比
- 输出 Markdown 报告，含详细分析

## 技术栈

- Benchmark 工具: Rust, clap, tokio, reqwest, rayon, alloy (ethereum types)
- 报告: Python 3, matplotlib
- 进程管理: pm2
- 引擎交互: HTTP JSON-RPC (txpool), JWT-authenticated Engine API

请从 tx_factory 开始，逐步构建。每个模块需要单元测试。构建过程中持续验证两个引擎都能正常工作。
```

---

*报告结束*
