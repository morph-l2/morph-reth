# Maximum TPS Benchmark Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a comprehensive benchmark suite that finds the absolute maximum TPS of morph-reth and morph-geth across three test dimensions (pure execution, end-to-end pipeline, sustained production), three workloads, and multiple sender configurations.

**Architecture:** Extends the existing `bench-block-exec` binary with three new run modes (`exec`, `e2e`, `sustained`), a `sweep` subcommand for automatic inflection-point discovery, a new `tx_factory` module for multi-sender/multi-workload transaction generation, and extended genesis generation. A Python script generates charts from JSON-line output. A rewritten shell script orchestrates the full 4-phase test matrix.

**Tech Stack:** Rust (alloy-*, clap, tokio, serde, reqwest, jsonrpsee), Solidity/Foundry, Python (matplotlib, numpy), Bash/PM2

**Spec:** `docs/superpowers/specs/2026-04-07-max-tps-benchmark-design.md`

---

## File Map

### New Files
| File | Responsibility |
|------|---------------|
| `local-test/bench-contracts/src/BenchSwap.sol` | Simplified constant-product AMM contract |
| `local-test/bench-contracts/test/BenchSwap.t.sol` | Gas verification test for BenchSwap |
| `bin/bench-block-exec/src/tx_factory.rs` | Multi-sender account generation + all transaction builders |
| `bin/bench-block-exec/src/mode_exec.rs` | Mode A: pure execution benchmark |
| `bin/bench-block-exec/src/mode_e2e.rs` | Mode B: end-to-end pipeline benchmark |
| `bin/bench-block-exec/src/mode_sustained.rs` | Mode C: sustained block production benchmark |
| `bin/bench-block-exec/src/sweep.rs` | Automatic inflection point discovery |
| `local-test/bench-plot.py` | Chart generation from JSON-line results |

### Modified Files
| File | Changes |
|------|---------|
| `local-test/erc20-bench-contracts/` | Rename to `local-test/bench-contracts/`, add BenchSwap |
| `bin/bench-block-exec/src/main.rs` | Add `Run`, `Sweep` subcommands |
| `bin/bench-block-exec/src/engine.rs` | Extend `BlockTiming` → `BlockTimingV2` with new fields |
| `bin/bench-block-exec/src/genesis.rs` | Multi-sender, contract pre-deploy, limits removed |
| `bin/bench-block-exec/src/report.rs` | New summary columns (MGas/s, degradation_pct) |
| `local-test/bench-block-exec.sh` | Rewrite: 4-phase orchestration |

### Unchanged Files
| File | Note |
|------|------|
| `bin/bench-block-exec/src/workload.rs` | Existing `run-workload` kept working as-is |
| `bin/bench-block-exec/src/verify.rs` | No changes needed |

---

## Task 1: BenchSwap Solidity Contract

**Files:**
- Rename: `local-test/erc20-bench-contracts/` → `local-test/bench-contracts/`
- Create: `local-test/bench-contracts/src/BenchSwap.sol`
- Create: `local-test/bench-contracts/test/BenchSwap.t.sol`

- [ ] **Step 1: Rename the contracts directory**

```bash
cd /Users/panos/workspace/morph-reth
mv local-test/erc20-bench-contracts local-test/bench-contracts
```

- [ ] **Step 2: Write BenchSwap.sol**

Create `local-test/bench-contracts/src/BenchSwap.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

/// @title BenchSwap - Simplified constant-product AMM for benchmarking
/// @notice Mirrors Uniswap V2 storage access pattern: 4 SLOAD + 4 SSTORE + arithmetic + LOG
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

- [ ] **Step 3: Write gas verification test**

Create `local-test/bench-contracts/test/BenchSwap.t.sol`:

```solidity
// SPDX-License-Identifier: MIT
pragma solidity ^0.8.20;

import "forge-std/Test.sol";
import "../src/BenchSwap.sol";

contract BenchSwapTest is Test {
    BenchSwap swap;
    address alice = address(0xA11CE);

    function setUp() public {
        swap = new BenchSwap();
        // Set reserves via storage manipulation
        vm.store(address(swap), bytes32(uint256(0)), bytes32(uint256(1e24))); // reserve0
        vm.store(address(swap), bytes32(uint256(1)), bytes32(uint256(1e24))); // reserve1
        // Set alice balance0
        bytes32 slot = keccak256(abi.encode(alice, uint256(2))); // balance0 mapping at slot 2
        vm.store(address(swap), slot, bytes32(uint256(1e24)));
    }

    function test_swap_gas() public {
        vm.prank(alice);
        uint256 gasBefore = gasleft();
        swap.swap0For1(1000);
        uint256 gasUsed = gasBefore - gasleft();
        // Should be in the 120k-150k range for warm slots
        assertGt(gasUsed, 50_000, "gas too low");
        assertLt(gasUsed, 200_000, "gas too high");
    }

    function test_swap_updates_state() public {
        vm.prank(alice);
        swap.swap0For1(1000);
        assertEq(swap.reserve0(), 1e24 + 1000);
        assertLt(swap.reserve1(), 1e24);
        assertEq(swap.balance0(alice), 1e24 - 1000);
        assertGt(swap.balance1(alice), 0);
    }

    function test_swap_insufficient_balance_reverts() public {
        vm.prank(address(0xDEAD)); // no balance
        vm.expectRevert("insufficient balance");
        swap.swap0For1(1);
    }
}
```

- [ ] **Step 4: Add forge-std dependency and compile**

```bash
cd /Users/panos/workspace/morph-reth/local-test/bench-contracts
forge install foundry-rs/forge-std --no-commit
```

Update `foundry.toml`:
```toml
[profile.default]
src = "src"
out = "out"
libs = ["lib"]
optimizer = true
optimizer_runs = 200
evm_version = "shanghai"
```

- [ ] **Step 5: Run tests, verify gas range**

```bash
cd /Users/panos/workspace/morph-reth/local-test/bench-contracts
forge test -vvv
```

Expected: all 3 tests pass, gas in 50k-200k range.

- [ ] **Step 6: Record deployed bytecode for genesis**

```bash
forge inspect BenchSwap deployedBytecode
forge inspect BenchToken deployedBytecode
```

Save both hex strings — they will be used in genesis.rs Task 5.

- [ ] **Step 7: Commit**

```bash
git add -f local-test/bench-contracts/
git commit -m "feat(bench): add BenchSwap contract for uniswap-style workload"
```

---

## Task 2: Extend BlockTiming with New Metrics

**Files:**
- Modify: `bin/bench-block-exec/src/engine.rs`

- [ ] **Step 1: Add BlockTimingV2 struct**

Add below the existing `BlockTiming` struct in `engine.rs`:

```rust
/// Extended timing record for new benchmark modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockTimingV2 {
    pub block_number: u64,
    pub tx_count: u64,
    pub expected_tx_count: u64,
    pub engine: String,
    pub mode: String,
    pub workload: String,
    pub senders: u64,
    pub warmup_blocks: u64,

    // Timing breakdown (milliseconds)
    pub submit_ms: f64,
    pub pool_wait_ms: f64,
    pub assemble_ms: f64,
    pub import_ms: f64,
    pub total_ms: f64,

    // Derived metrics
    pub gas_used: u64,
    pub tps: f64,
    pub mgas_per_sec: f64,
    pub inclusion_rate: f64,

    // Cumulative (for sustained mode)
    pub cumulative_blocks: u64,
    pub cumulative_txs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolling_avg_tps_100: Option<f64>,

    // Error flag
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub error: bool,
}

impl BlockTimingV2 {
    /// Compute derived fields after timing is recorded.
    pub fn finalize(&mut self) {
        self.total_ms = self.submit_ms + self.pool_wait_ms + self.assemble_ms + self.import_ms;
        if self.total_ms > 0.0 {
            let secs = self.total_ms / 1000.0;
            self.tps = self.tx_count as f64 / secs;
            self.mgas_per_sec = self.gas_used as f64 / secs / 1_000_000.0;
        }
        self.inclusion_rate = if self.expected_tx_count > 0 {
            self.tx_count as f64 / self.expected_tx_count as f64
        } else {
            1.0
        };
    }
}
```

- [ ] **Step 2: Verify compilation**

```bash
cd /Users/panos/workspace/morph-reth
cargo check -p bench-block-exec
```

Expected: compiles with no errors.

- [ ] **Step 3: Commit**

```bash
git add bin/bench-block-exec/src/engine.rs
git commit -m "feat(bench): add BlockTimingV2 with extended metrics and MGas/s"
```

---

## Task 3: Transaction Factory — Account Generation

**Files:**
- Create: `bin/bench-block-exec/src/tx_factory.rs`
- Modify: `bin/bench-block-exec/src/main.rs` (add `pub mod tx_factory;`)

- [ ] **Step 1: Create tx_factory.rs with sender generation**

Create `bin/bench-block-exec/src/tx_factory.rs`:

```rust
use alloy_primitives::{Address, Bytes, B256, U256, keccak256};
use alloy_signer_local::PrivateKeySigner;
use std::str::FromStr;

/// A benchmark sender with its own key, address, and nonce tracker.
#[derive(Debug, Clone)]
pub struct BenchSender {
    pub signer: PrivateKeySigner,
    pub address: Address,
    pub nonce: u64,
}

/// Deterministic key derivation for reproducible benchmarks.
///
/// master_key = keccak256("bench-sender-0")
/// sender[i].private_key = keccak256(master_key ++ i.to_be_bytes())
pub fn generate_senders(count: u64) -> Vec<BenchSender> {
    let master = keccak256(b"bench-sender-0");
    (0..count)
        .map(|i| {
            let mut preimage = [0u8; 40];
            preimage[..32].copy_from_slice(master.as_slice());
            preimage[32..].copy_from_slice(&i.to_be_bytes());
            let key_bytes = keccak256(&preimage);
            let signer = PrivateKeySigner::from_bytes(&key_bytes)
                .expect("valid private key from keccak256");
            let address = signer.address();
            BenchSender { signer, address, nonce: 0 }
        })
        .collect()
}

/// Known contract addresses for genesis pre-deploy.
pub const BENCH_TOKEN_ADDR: Address = Address::new([
    0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x01,
]);

pub const BENCH_SWAP_ADDR: Address = Address::new([
    0x55, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x02,
]);

/// Max fee per gas for all benchmark transactions (Morph base fee).
pub const BENCH_MAX_FEE_PER_GAS: u128 = 1_000_000;

/// Compute the storage slot for a Solidity mapping(address => uint256).
/// slot_of(mapping[key]) = keccak256(abi.encode(key, mapping_slot))
pub fn mapping_slot(key: Address, mapping_slot: u64) -> B256 {
    let mut buf = [0u8; 64];
    // Left-pad address to 32 bytes
    buf[12..32].copy_from_slice(key.as_slice());
    // Left-pad slot number to 32 bytes
    buf[56..64].copy_from_slice(&mapping_slot.to_be_bytes());
    keccak256(&buf)
}

/// Workload type for the new benchmark modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    EthTransfer,
    Erc20Transfer,
    UniswapSwap,
}

impl Workload {
    pub fn as_str(&self) -> &'static str {
        match self {
            Workload::EthTransfer => "eth-transfer",
            Workload::Erc20Transfer => "erc20-transfer",
            Workload::UniswapSwap => "uniswap-swap",
        }
    }

    pub fn gas_per_tx(&self) -> u64 {
        match self {
            Workload::EthTransfer => 21_000,
            Workload::Erc20Transfer => 60_000,
            Workload::UniswapSwap => 150_000,
        }
    }
}

impl std::str::FromStr for Workload {
    type Err = eyre::Report;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "eth-transfer" => Ok(Workload::EthTransfer),
            "erc20-transfer" => Ok(Workload::Erc20Transfer),
            "uniswap-swap" => Ok(Workload::UniswapSwap),
            _ => Err(eyre::eyre!("unknown workload: {s}")),
        }
    }
}
```

- [ ] **Step 2: Register module in main.rs**

Add to `bin/bench-block-exec/src/main.rs` after the existing `mod` declarations:

```rust
pub mod tx_factory;
```

- [ ] **Step 3: Verify compilation**

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 4: Commit**

```bash
git add bin/bench-block-exec/src/tx_factory.rs bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): add tx_factory module with multi-sender generation and workload types"
```

---

## Task 4: Transaction Factory — Transaction Builders

**Files:**
- Modify: `bin/bench-block-exec/src/tx_factory.rs`

- [ ] **Step 1: Add transaction building functions**

Append to `tx_factory.rs`:

```rust
use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip1559, TxEip4844};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::TxKind;
use alloy_signer::SignerSync;
use alloy_sol_types::SolValue;

type TxEnvelope = EthereumTxEnvelope<TxEip4844>;

/// Receiver address for transfers. Deterministic: 0xBB...{index}.
pub fn receiver_address(index: u64) -> Address {
    let mut addr = [0u8; 20];
    addr[0] = 0xBB;
    addr[12..20].copy_from_slice(&index.to_be_bytes());
    Address::new(addr)
}

fn sign_and_encode(signer: &PrivateKeySigner, mut tx: TxEip1559, chain_id: u64) -> eyre::Result<Bytes> {
    tx.chain_id = chain_id;
    let sig = signer.sign_transaction_sync(&mut tx)?;
    let envelope = TxEnvelope::Eip1559(tx.into_signed(sig));
    let mut buf = Vec::new();
    envelope.encode_2718(&mut buf);
    Ok(Bytes::from(buf))
}

/// Build a batch of ETH transfer transactions.
pub fn build_eth_transfers(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    let mut txs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let tx = TxEip1559 {
            nonce: sender.nonce,
            gas_limit: 21_000,
            max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(receiver_address(sender.nonce)),
            value: U256::from(1),
            ..Default::default()
        };
        txs.push(sign_and_encode(&sender.signer, tx, chain_id)?);
        sender.nonce += 1;
    }
    Ok(txs)
}

/// Build a batch of ERC20 transfer transactions.
pub fn build_erc20_transfers(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    // transfer(address,uint256) selector: 0xa9059cbb
    let mut txs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let to = receiver_address(sender.nonce);
        let mut calldata = vec![0xa9, 0x05, 0x9c, 0xbb]; // selector
        calldata.extend_from_slice(&[0u8; 12]); // left-pad address
        calldata.extend_from_slice(to.as_slice());
        calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>());

        let tx = TxEip1559 {
            nonce: sender.nonce,
            gas_limit: 60_000,
            max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(BENCH_TOKEN_ADDR),
            input: Bytes::from(calldata),
            ..Default::default()
        };
        txs.push(sign_and_encode(&sender.signer, tx, chain_id)?);
        sender.nonce += 1;
    }
    Ok(txs)
}

/// Build a batch of BenchSwap swap0For1 transactions.
pub fn build_swap_txs(
    sender: &mut BenchSender,
    count: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    // swap0For1(uint256) selector: first 4 bytes of keccak256("swap0For1(uint256)")
    let selector = &keccak256(b"swap0For1(uint256)")[..4];
    let mut txs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let mut calldata = selector.to_vec();
        calldata.extend_from_slice(&U256::from(1).to_be_bytes::<32>());

        let tx = TxEip1559 {
            nonce: sender.nonce,
            gas_limit: 150_000,
            max_fee_per_gas: BENCH_MAX_FEE_PER_GAS,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(BENCH_SWAP_ADDR),
            input: Bytes::from(calldata),
            ..Default::default()
        };
        txs.push(sign_and_encode(&sender.signer, tx, chain_id)?);
        sender.nonce += 1;
    }
    Ok(txs)
}

/// Build transactions for a block using multiple senders, round-robin interleaved.
pub fn build_block_txs(
    senders: &mut [BenchSender],
    workload: Workload,
    total_txs: u64,
    chain_id: u64,
) -> eyre::Result<Vec<Bytes>> {
    let n_senders = senders.len() as u64;
    let per_sender = total_txs / n_senders;
    let remainder = total_txs % n_senders;

    // Build per-sender batches
    let mut per_sender_txs: Vec<Vec<Bytes>> = Vec::with_capacity(senders.len());
    for (i, sender) in senders.iter_mut().enumerate() {
        let count = per_sender + if (i as u64) < remainder { 1 } else { 0 };
        let txs = match workload {
            Workload::EthTransfer => build_eth_transfers(sender, count, chain_id)?,
            Workload::Erc20Transfer => build_erc20_transfers(sender, count, chain_id)?,
            Workload::UniswapSwap => build_swap_txs(sender, count, chain_id)?,
        };
        per_sender_txs.push(txs);
    }

    // Round-robin interleave
    let mut result = Vec::with_capacity(total_txs as usize);
    let max_len = per_sender_txs.iter().map(|v| v.len()).max().unwrap_or(0);
    for tx_idx in 0..max_len {
        for sender_txs in &per_sender_txs {
            if tx_idx < sender_txs.len() {
                result.push(sender_txs[tx_idx].clone());
            }
        }
    }
    Ok(result)
}
```

- [ ] **Step 2: Verify compilation**

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 3: Commit**

```bash
git add bin/bench-block-exec/src/tx_factory.rs
git commit -m "feat(bench): add transaction builders for eth/erc20/swap with multi-sender support"
```

---

## Task 5: Extend Genesis Generation

**Files:**
- Modify: `bin/bench-block-exec/src/genesis.rs`

- [ ] **Step 1: Add multi-sender and contract pre-deploy support**

Replace the contents of `genesis.rs` with an extended version. Keep the existing `WriteGenesisArgs` and `run` function signatures, but add new args and extend `build_genesis`:

```rust
use alloy_primitives::{Address, B256, U256};
use clap::Args;
use eyre::ensure;
use serde_json::{json, Map, Value};
use crate::tx_factory::{
    generate_senders, mapping_slot, BENCH_TOKEN_ADDR, BENCH_SWAP_ADDR,
};

#[derive(Args)]
pub struct WriteGenesisArgs {
    #[arg(long)]
    pub output: String,
    /// Legacy single-sender address (hex). Ignored if --senders > 0.
    #[arg(long)]
    pub sender: Option<String>,
    #[arg(long, default_value = "1000000000000000000000000000")]
    pub sender_balance: String,
    /// Number of benchmark senders to generate (deterministic keys).
    #[arg(long, default_value = "0")]
    pub senders: u64,
    /// Gas limit for the genesis block (hex or decimal).
    #[arg(long, default_value = "0x2540BE400")]
    pub gas_limit: String,
    /// Max transactions per block. 0 = use a very large value (10,000,000).
    #[arg(long, default_value = "10000")]
    pub max_tx_per_block: u64,
    /// Pre-deploy BenchToken bytecode (hex). If set, deploys at BENCH_TOKEN_ADDR.
    #[arg(long)]
    pub bench_token_code: Option<String>,
    /// Pre-deploy BenchSwap bytecode (hex). If set, deploys at BENCH_SWAP_ADDR.
    #[arg(long)]
    pub bench_swap_code: Option<String>,
}

pub fn build_genesis(args: &WriteGenesisArgs) -> eyre::Result<Value> {
    let max_tx = if args.max_tx_per_block == 0 { 10_000_000 } else { args.max_tx_per_block };

    let mut alloc = Map::new();

    // Fee vault
    alloc.insert(
        "530000000000000000000000000000000000000a".to_string(),
        json!({ "balance": "0x0" }),
    );

    // Legacy single sender (backward compat)
    if let Some(sender) = &args.sender {
        let addr = sender.strip_prefix("0x").unwrap_or(sender);
        alloc.insert(addr.to_lowercase(), json!({ "balance": format!("0x{:x}", U256::from_str_radix(&args.sender_balance, 10).unwrap_or(U256::from(10).pow(U256::from(27)))) }));
    }

    // Multi-sender accounts
    let bench_senders = if args.senders > 0 {
        let senders = generate_senders(args.senders);
        let balance_hex = format!("0x{:x}", U256::from(10).pow(U256::from(27)));
        for s in &senders {
            alloc.insert(
                format!("{:x}", s.address),
                json!({ "balance": &balance_hex }),
            );
        }
        senders
    } else {
        vec![]
    };

    // Pre-deploy BenchToken
    if let Some(code) = &args.bench_token_code {
        let code_hex = if code.starts_with("0x") { code.clone() } else { format!("0x{code}") };
        let mut storage = Map::new();
        // totalSupply at slot 0
        let supply = U256::from(10).pow(U256::from(30));
        storage.insert(
            format!("{:066x}", 0u64),
            json!(format!("0x{:064x}", supply)),
        );
        // balanceOf[sender] at slot 1 for each bench sender
        for s in &bench_senders {
            let slot = mapping_slot(s.address, 1);
            storage.insert(
                format!("0x{:x}", slot),
                json!(format!("0x{:064x}", U256::from(10).pow(U256::from(27)))),
            );
        }
        alloc.insert(
            format!("{:x}", BENCH_TOKEN_ADDR),
            json!({
                "code": code_hex,
                "balance": "0x0",
                "storage": storage,
            }),
        );
    }

    // Pre-deploy BenchSwap
    if let Some(code) = &args.bench_swap_code {
        let code_hex = if code.starts_with("0x") { code.clone() } else { format!("0x{code}") };
        let mut storage = Map::new();
        let reserve = U256::from(10).pow(U256::from(24));
        // reserve0 at slot 0
        storage.insert(format!("{:066x}", 0u64), json!(format!("0x{:064x}", reserve)));
        // reserve1 at slot 1
        storage.insert(format!("{:066x}", 1u64), json!(format!("0x{:064x}", reserve)));
        // balance0[sender] at slot 2 for each bench sender
        for s in &bench_senders {
            let slot = mapping_slot(s.address, 2);
            storage.insert(
                format!("0x{:x}", slot),
                json!(format!("0x{:064x}", U256::from(10).pow(U256::from(24)))),
            );
        }
        alloc.insert(
            format!("{:x}", BENCH_SWAP_ADDR),
            json!({
                "code": code_hex,
                "balance": "0x0",
                "storage": storage,
            }),
        );
    }

    let genesis = json!({
        "config": {
            "chainId": 99999,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "muirGlacierBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0,
            "shanghaiTime": 0,
            "morph": {
                "useZktrie": false,
                "maxTxPerBlock": max_tx,
                "maxTxPayloadBytesPerBlock": 1_073_741_824u64,
                "feeVaultAddress": "0x530000000000000000000000000000000000000a",
                "viridianBlock": 0,
                "emeraldBlock": 0,
                "jadeForkTime": 0
            }
        },
        "difficulty": "0x0",
        "gasLimit": &args.gas_limit,
        "alloc": alloc
    });

    Ok(genesis)
}

pub fn run(args: WriteGenesisArgs) -> eyre::Result<()> {
    let genesis = build_genesis(&args)?;
    let json = serde_json::to_string_pretty(&genesis)?;
    std::fs::write(&args.output, json)?;
    eprintln!("Genesis written to {}", args.output);
    Ok(())
}
```

- [ ] **Step 2: Verify compilation**

```bash
cargo check -p bench-block-exec
```

Note: The existing `WriteGenesis` variant in `main.rs` uses `genesis::WriteGenesisArgs`. The old `--sender` field is now `Option<String>` instead of required `String`. If the old CLI tests call `write-genesis --sender ...` it still works. New usage: `write-genesis --senders 100 --gas-limit 0x2540BE400 --max-tx-per-block 0 --bench-token-code 0x... --bench-swap-code 0x...`.

- [ ] **Step 3: Commit**

```bash
git add bin/bench-block-exec/src/genesis.rs
git commit -m "feat(bench): extend genesis with multi-sender, contract pre-deploy, configurable limits"
```

---

## Task 6: Mode A — Pure Execution

**Files:**
- Create: `bin/bench-block-exec/src/mode_exec.rs`

- [ ] **Step 1: Write mode_exec.rs**

```rust
use clap::Args;
use std::fs::OpenOptions;
use std::io::Write;

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::tx_factory::{self, BenchSender, Workload};

#[derive(Args)]
pub struct ExecArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    #[arg(long)]
    pub jwt_secret: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long)]
    pub txs_per_block: u64,
    #[arg(long, default_value = "50")]
    pub blocks: u64,
    #[arg(long)]
    pub output: String,
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

pub async fn run(args: ExecArgs) -> eyre::Result<()> {
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)?.trim().to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;

    // Single sender for pure execution mode
    let mut senders = tx_factory::generate_senders(1);

    let mut file = OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&args.output)?;

    // Pre-generate ALL transactions for ALL blocks upfront
    eprintln!("Pre-generating {} blocks × {} txs...", args.blocks, args.txs_per_block);
    let mut all_block_txs: Vec<Vec<alloy_primitives::Bytes>> = Vec::with_capacity(args.blocks as usize);
    for _ in 0..args.blocks {
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;
        all_block_txs.push(txs);
    }
    eprintln!("Pre-generation complete. Starting benchmark...");

    let mut cumulative_txs: u64 = 0;

    let mut consecutive_errors = 0u32;

    for (i, txs) in all_block_txs.into_iter().enumerate() {
        let block_number = (i + 1) as u64;
        let expected_count = txs.len() as u64;

        // Assemble: inject transactions directly (bypass txpool)
        let params = AssembleL2BlockParams {
            number: block_number,
            transactions: txs,
            timestamp: Some(block_number),
        };
        let result = client.assemble_l2_block(&args.engine_rpc, params).await;
        let (data, assemble_ms, tx_count, gas_used, import_ms, is_error) = match result {
            Ok((data, asm_ms)) => {
                let tc = data.transactions.len() as u64;
                let gu = data.gas_used;
                match client.new_l2_block(&args.engine_rpc, data).await {
                    Ok(imp_ms) => { consecutive_errors = 0; (None, asm_ms, tc, gu, imp_ms, false) }
                    Err(e) => {
                        eprintln!("Import error block {}: {e}", block_number);
                        consecutive_errors += 1;
                        (None, asm_ms, tc, gu, 0.0, true)
                    }
                }
            }
            Err(e) => {
                eprintln!("Assemble error block {}: {e}", block_number);
                consecutive_errors += 1;
                (None, 0.0, 0, 0, 0.0, true)
            }
        };

        if consecutive_errors >= 5 {
            eprintln!("5 consecutive errors, terminating.");
            break;
        }

        cumulative_txs += tx_count;

        let mut timing = BlockTimingV2 {
            block_number,
            tx_count,
            expected_tx_count: expected_count,
            engine: args.engine_name.clone(),
            mode: "exec".to_string(),
            workload: workload.as_str().to_string(),
            senders: 1,
            warmup_blocks: 0,
            submit_ms: 0.0,
            pool_wait_ms: 0.0,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            error: is_error,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: block_number,
            cumulative_txs,
            rolling_avg_tps_100: None,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(file, "{}", line)?;

        if block_number % 10 == 0 || block_number == args.blocks {
            eprintln!(
                "Block {}/{}: {} txs, asm={:.1}ms, imp={:.1}ms, {:.0} TPS, {:.0} MGas/s",
                block_number, args.blocks, tx_count,
                timing.assemble_ms, timing.import_ms,
                timing.tps, timing.mgas_per_sec,
            );
        }
    }

    eprintln!("Mode exec complete. Results: {}", args.output);
    Ok(())
}
```

- [ ] **Step 2: Register module in main.rs**

Add `pub mod mode_exec;` to `main.rs`.

- [ ] **Step 3: Verify compilation**

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 4: Commit**

```bash
git add bin/bench-block-exec/src/mode_exec.rs bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): add Mode A pure execution benchmark (mode_exec)"
```

---

## Task 7: Mode B — End-to-End

**Files:**
- Create: `bin/bench-block-exec/src/mode_e2e.rs`

- [ ] **Step 1: Write mode_e2e.rs**

```rust
use alloy_primitives::Bytes;
use clap::Args;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Instant;

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::tx_factory::{self, BenchSender, Workload};

#[derive(Args)]
pub struct E2eArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    #[arg(long)]
    pub jwt_secret: String,
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long)]
    pub txs_per_block: u64,
    #[arg(long, default_value = "200")]
    pub blocks: u64,
    #[arg(long, default_value = "100")]
    pub senders: u64,
    #[arg(long)]
    pub output: String,
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

/// Send raw transactions to txpool in batches, using concurrent tasks.
async fn submit_to_txpool(http_rpc: &str, txs: &[Bytes], concurrency: usize) -> eyre::Result<f64> {
    let start = Instant::now();
    let client = reqwest::Client::new();
    let chunk_size = 500;

    // Split into concurrent groups
    let chunks: Vec<&[Bytes]> = txs.chunks(chunk_size).collect();
    let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));

    let mut handles = Vec::new();
    for chunk in chunks {
        let client = client.clone();
        let url = http_rpc.to_string();
        let sem = sem.clone();
        let batch: Vec<serde_json::Value> = chunk.iter().enumerate().map(|(i, tx)| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_sendRawTransaction",
                "params": [format!("0x{}", hex::encode(tx))],
                "id": i + 1
            })
        }).collect();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let resp = client.post(&url).json(&batch).send().await?;
            resp.error_for_status()?;
            Ok::<_, eyre::Report>(())
        }));
    }

    for h in handles {
        h.await??;
    }

    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Wait for all senders' pending nonces to reach expected values.
async fn wait_for_pool(
    http_rpc: &str,
    senders: &[BenchSender],
    timeout_secs: u64,
) -> eyre::Result<f64> {
    let start = Instant::now();
    let client = reqwest::Client::new();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    for sender in senders {
        loop {
            if Instant::now() > deadline {
                eyre::bail!("txpool wait timeout for sender {:?}", sender.address);
            }
            let resp: serde_json::Value = client.post(http_rpc)
                .json(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_getTransactionCount",
                    "params": [format!("0x{:x}", sender.address), "pending"],
                    "id": 1
                }))
                .send().await?
                .json().await?;

            let nonce_hex = resp["result"].as_str().unwrap_or("0x0");
            let nonce = u64::from_str_radix(nonce_hex.strip_prefix("0x").unwrap_or("0"), 16)?;
            if nonce >= sender.nonce {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

mod hex {
    pub fn encode(data: &[u8]) -> String {
        data.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

pub async fn run(args: E2eArgs) -> eyre::Result<()> {
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)?.trim().to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;

    let mut senders = tx_factory::generate_senders(args.senders);
    let concurrency = std::cmp::min(args.senders as usize, 16);

    let mut file = OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&args.output)?;

    let mut cumulative_txs: u64 = 0;

    for block_idx in 0..args.blocks {
        let block_number = (block_idx + 1) as u64;

        // Build transactions for this block
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;
        let expected_count = txs.len() as u64;

        // Submit to txpool (timed)
        let submit_ms = submit_to_txpool(&args.http_rpc, &txs, concurrency).await?;

        // Wait for txpool acceptance (timed)
        let pool_wait_ms = wait_for_pool(&args.http_rpc, &senders, 60).await?;

        // Assemble block (pull from txpool, empty transactions array)
        let params = AssembleL2BlockParams {
            number: block_number,
            transactions: vec![],
            timestamp: Some(block_number),
        };
        let (data, assemble_ms) = client.assemble_l2_block(&args.engine_rpc, params).await?;
        let tx_count = data.transactions.len() as u64;
        let gas_used = data.gas_used;

        // Import
        let import_ms = client.new_l2_block(&args.engine_rpc, data).await?;

        cumulative_txs += tx_count;

        let mut timing = BlockTimingV2 {
            block_number,
            tx_count,
            expected_tx_count: expected_count,
            engine: args.engine_name.clone(),
            mode: "e2e".to_string(),
            workload: workload.as_str().to_string(),
            senders: args.senders,
            warmup_blocks: 0,
            submit_ms,
            pool_wait_ms,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: block_number,
            cumulative_txs,
            rolling_avg_tps_100: None,
            error: false,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(file, "{}", line)?;

        if block_number % 10 == 0 || block_number == args.blocks {
            eprintln!(
                "Block {}/{}: {} txs (incl {:.0}%), sub={:.0}ms pool={:.0}ms asm={:.1}ms imp={:.1}ms | {:.0} TPS",
                block_number, args.blocks, tx_count,
                timing.inclusion_rate * 100.0,
                submit_ms, pool_wait_ms,
                timing.assemble_ms, timing.import_ms,
                timing.tps,
            );
        }
    }

    eprintln!("Mode e2e complete. Results: {}", args.output);
    Ok(())
}
```

- [ ] **Step 2: Register module, verify compilation**

Add `pub mod mode_e2e;` to `main.rs`.

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 3: Commit**

```bash
git add bin/bench-block-exec/src/mode_e2e.rs bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): add Mode B end-to-end benchmark with concurrent txpool submission"
```

---

## Task 8: Mode C — Sustained Block Production

**Files:**
- Create: `bin/bench-block-exec/src/mode_sustained.rs`

- [ ] **Step 1: Write mode_sustained.rs**

```rust
use alloy_primitives::Bytes;
use clap::Args;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::time::Instant;

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::mode_e2e::{submit_to_txpool, wait_for_pool};
use crate::tx_factory::{self, Workload};

#[derive(Args)]
pub struct SustainedArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    #[arg(long)]
    pub jwt_secret: String,
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long)]
    pub txs_per_block: u64,
    #[arg(long, default_value = "1000")]
    pub blocks: u64,
    #[arg(long, default_value = "0")]
    pub warmup_blocks: u64,
    #[arg(long, default_value = "100")]
    pub senders: u64,
    #[arg(long)]
    pub output: String,
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

pub async fn run(args: SustainedArgs) -> eyre::Result<()> {
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)?.trim().to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;

    let mut senders = tx_factory::generate_senders(args.senders);
    let concurrency = std::cmp::min(args.senders as usize, 16);
    let total_blocks = args.warmup_blocks + args.blocks;

    let mut file = OpenOptions::new()
        .create(true).write(true).truncate(true)
        .open(&args.output)?;

    let mut cumulative_txs: u64 = 0;
    let mut cumulative_measured_blocks: u64 = 0;
    let mut rolling_tps: VecDeque<f64> = VecDeque::with_capacity(100);

    for block_idx in 0..total_blocks {
        let block_number = (block_idx + 1) as u64;
        let is_warmup = block_idx < args.warmup_blocks;

        // Build transactions
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;
        let expected_count = txs.len() as u64;

        // Submit to txpool
        let submit_ms = submit_to_txpool(&args.http_rpc, &txs, concurrency).await?;
        let pool_wait_ms = wait_for_pool(&args.http_rpc, &senders, 60).await?;

        // Assemble
        let params = AssembleL2BlockParams {
            number: block_number,
            transactions: vec![],
            timestamp: Some(block_number),
        };
        let (data, assemble_ms) = client.assemble_l2_block(&args.engine_rpc, params).await?;
        let tx_count = data.transactions.len() as u64;
        let gas_used = data.gas_used;

        // Import
        let import_ms = client.new_l2_block(&args.engine_rpc, data).await?;

        if is_warmup {
            if block_number % 50 == 0 {
                eprintln!("Warmup block {}/{}", block_number, args.warmup_blocks);
            }
            continue;
        }

        cumulative_measured_blocks += 1;
        cumulative_txs += tx_count;

        let total_ms = submit_ms + pool_wait_ms + assemble_ms + import_ms;
        let tps = if total_ms > 0.0 { tx_count as f64 / (total_ms / 1000.0) } else { 0.0 };

        // Rolling average
        rolling_tps.push_back(tps);
        if rolling_tps.len() > 100 {
            rolling_tps.pop_front();
        }
        let rolling_avg = if rolling_tps.len() >= 100 {
            Some(rolling_tps.iter().sum::<f64>() / rolling_tps.len() as f64)
        } else {
            None
        };

        let mut timing = BlockTimingV2 {
            block_number,
            tx_count,
            expected_tx_count: expected_count,
            engine: args.engine_name.clone(),
            mode: "sustained".to_string(),
            workload: workload.as_str().to_string(),
            senders: args.senders,
            warmup_blocks: args.warmup_blocks,
            submit_ms,
            pool_wait_ms,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: cumulative_measured_blocks,
            cumulative_txs,
            rolling_avg_tps_100: rolling_avg,
            error: false,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(file, "{}", line)?;

        if cumulative_measured_blocks % 100 == 0 {
            eprintln!(
                "Sustained block {}/{} (abs {}): {:.0} TPS, rolling100={:.0} TPS, {:.0} MGas/s",
                cumulative_measured_blocks, args.blocks, block_number,
                timing.tps,
                rolling_avg.unwrap_or(0.0),
                timing.mgas_per_sec,
            );
        }
    }

    eprintln!("Mode sustained complete. Results: {}", args.output);
    Ok(())
}
```

- [ ] **Step 2: Make submit_to_txpool and wait_for_pool public in mode_e2e.rs**

In `mode_e2e.rs`, change these functions from module-private to `pub`:

```rust
pub async fn submit_to_txpool(...) -> eyre::Result<f64> {
pub async fn wait_for_pool(...) -> eyre::Result<f64> {
```

Also make the `hex` module `pub(crate)`.

- [ ] **Step 3: Register module, verify compilation**

Add `pub mod mode_sustained;` to `main.rs`.

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 4: Commit**

```bash
git add bin/bench-block-exec/src/mode_sustained.rs bin/bench-block-exec/src/mode_e2e.rs bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): add Mode C sustained block production with warmup and rolling TPS"
```

---

## Task 9: Sweep — Automatic Inflection Point Discovery

**Files:**
- Create: `bin/bench-block-exec/src/sweep.rs`

- [ ] **Step 1: Write sweep.rs**

```rust
use clap::Args;
use crate::mode_exec;
use crate::report::percentile;
use crate::engine::BlockTimingV2;
use std::io::BufRead;

#[derive(Args)]
pub struct SweepArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    #[arg(long)]
    pub jwt_secret: String,
    #[arg(long)]
    pub workload: String,
    #[arg(long, default_value = "30")]
    pub blocks_per_step: u64,
    #[arg(long)]
    pub output_dir: String,
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

/// Coarse scan points (exponential).
const COARSE_POINTS: &[u64] = &[1000, 2000, 5000, 10_000, 20_000, 50_000, 100_000];

#[derive(Debug, serde::Serialize)]
pub struct SweepResult {
    pub engine: String,
    pub workload: String,
    pub peak_tps: f64,
    pub peak_mgas_s: f64,
    pub inflection_txs: u64,
    pub points: Vec<SweepPoint>,
}

#[derive(Debug, serde::Serialize)]
pub struct SweepPoint {
    pub txs_per_block: u64,
    pub median_assemble_ms: f64,
    pub median_total_ms: f64,
    pub p95_total_ms: f64,
    pub median_tps: f64,
    pub median_mgas_s: f64,
    pub per_tx_assemble_us: f64,
}

fn read_timings(path: &str) -> eyre::Result<Vec<BlockTimingV2>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut timings = Vec::new();
    for line in reader.lines() {
        let t: BlockTimingV2 = serde_json::from_str(&line?)?;
        if !t.error {
            timings.push(t);
        }
    }
    Ok(timings)
}

fn analyze_point(timings: &[BlockTimingV2]) -> SweepPoint {
    let n = timings.len();
    let txs_per_block = if n > 0 { timings[0].expected_tx_count } else { 0 };

    let mut asm: Vec<f64> = timings.iter().map(|t| t.assemble_ms).collect();
    let mut tot: Vec<f64> = timings.iter().map(|t| t.total_ms).collect();
    let mut tps: Vec<f64> = timings.iter().map(|t| t.tps).collect();
    let mut mgas: Vec<f64> = timings.iter().map(|t| t.mgas_per_sec).collect();

    asm.sort_by(|a, b| a.partial_cmp(b).unwrap());
    tot.sort_by(|a, b| a.partial_cmp(b).unwrap());
    tps.sort_by(|a, b| a.partial_cmp(b).unwrap());
    mgas.sort_by(|a, b| a.partial_cmp(b).unwrap());

    let median_asm = percentile(&asm, 50.0);
    let per_tx_us = if txs_per_block > 0 { median_asm * 1000.0 / txs_per_block as f64 } else { 0.0 };

    SweepPoint {
        txs_per_block,
        median_assemble_ms: median_asm,
        median_total_ms: percentile(&tot, 50.0),
        p95_total_ms: percentile(&tot, 95.0),
        median_tps: percentile(&tps, 50.0),
        median_mgas_s: percentile(&mgas, 50.0),
        per_tx_assemble_us: per_tx_us,
    }
}

pub async fn run(args: SweepArgs) -> eyre::Result<()> {
    std::fs::create_dir_all(&args.output_dir)?;

    let mut points: Vec<SweepPoint> = Vec::new();
    let mut min_per_tx_us = f64::MAX;

    // Coarse scan
    eprintln!("=== Sweep coarse scan: {:?}", COARSE_POINTS);
    for &txs in COARSE_POINTS {
        let output = format!("{}/{}-{}-{}.jsonl",
            args.output_dir, args.engine_name, args.workload, txs);

        let exec_args = mode_exec::ExecArgs {
            engine_rpc: args.engine_rpc.clone(),
            jwt_secret: args.jwt_secret.clone(),
            workload: args.workload.clone(),
            txs_per_block: txs,
            blocks: args.blocks_per_step,
            output: output.clone(),
            engine_name: args.engine_name.clone(),
            chain_id: args.chain_id,
        };

        match mode_exec::run(exec_args).await {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Sweep failed at txs={}: {e}. Treating as hard limit.", txs);
                break;
            }
        }

        let timings = read_timings(&output)?;
        let point = analyze_point(&timings);
        eprintln!(
            "  txs={}: asm_p50={:.1}ms, per_tx={:.2}us, tps={:.0}, mgas={:.0}",
            txs, point.median_assemble_ms, point.per_tx_assemble_us,
            point.median_tps, point.median_mgas_s,
        );

        if point.per_tx_assemble_us < min_per_tx_us {
            min_per_tx_us = point.per_tx_assemble_us;
        }

        points.push(point);

        // Check for degradation: per-tx time > 1.5× minimum
        if point.per_tx_assemble_us > min_per_tx_us * 1.5 {
            eprintln!("  Degradation detected at txs={}. Stopping coarse scan.", txs);
            break;
        }
    }

    // Find inflection point
    let (inflection_txs, peak_tps, peak_mgas_s) = if let Some(best) = points.iter()
        .max_by(|a, b| a.median_tps.partial_cmp(&b.median_tps).unwrap())
    {
        (best.txs_per_block, best.median_tps, best.median_mgas_s)
    } else {
        (0, 0.0, 0.0)
    };

    let result = SweepResult {
        engine: args.engine_name.clone(),
        workload: args.workload.clone(),
        peak_tps,
        peak_mgas_s,
        inflection_txs,
        points,
    };

    let summary_path = format!("{}/{}-{}-sweep-summary.json",
        args.output_dir, args.engine_name, args.workload);
    std::fs::write(&summary_path, serde_json::to_string_pretty(&result)?)?;

    eprintln!("\n=== Sweep complete: peak_tps={:.0}, peak_mgas={:.0}, inflection={}txs",
        peak_tps, peak_mgas_s, inflection_txs);
    eprintln!("Summary: {}", summary_path);

    Ok(())
}
```

- [ ] **Step 2: Make `percentile` function public in report.rs**

In `report.rs`, ensure `percentile` is `pub`:

```rust
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
```

- [ ] **Step 3: Register module, verify compilation**

Add `pub mod sweep;` to `main.rs`.

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 4: Commit**

```bash
git add bin/bench-block-exec/src/sweep.rs bin/bench-block-exec/src/report.rs bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): add sweep subcommand for automatic inflection point discovery"
```

---

## Task 10: CLI Integration

**Files:**
- Modify: `bin/bench-block-exec/src/main.rs`

- [ ] **Step 1: Add new subcommands to CLI**

Replace the content of `main.rs` with:

```rust
use clap::{Parser, Subcommand};

pub mod engine;
pub mod genesis;
pub mod mode_e2e;
pub mod mode_exec;
pub mod mode_sustained;
pub mod report;
pub mod sweep;
pub mod tx_factory;
pub mod verify;
pub mod workload;

#[derive(Parser)]
#[command(name = "bench-block-exec", about = "Morph block execution benchmark")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Generate a benchmark genesis file.
    WriteGenesis(genesis::WriteGenesisArgs),
    /// Run the legacy workload benchmark (backward compat).
    RunWorkload(workload::RunWorkloadArgs),
    /// Run a benchmark in the specified mode (exec, e2e, sustained).
    Run {
        #[command(subcommand)]
        mode: RunMode,
    },
    /// Automatically find the TPS inflection point.
    Sweep(sweep::SweepArgs),
    /// Verify state consistency between two nodes.
    VerifyState(verify::VerifyStateArgs),
    /// Summarize benchmark results into TSV.
    Summarize(report::SummarizeArgs),
}

#[derive(Subcommand)]
pub enum RunMode {
    /// Mode A: Pure execution (bypass txpool).
    Exec(mode_exec::ExecArgs),
    /// Mode B: End-to-end (txpool → assembly → import).
    E2e(mode_e2e::E2eArgs),
    /// Mode C: Sustained block production with optional warmup.
    Sustained(mode_sustained::SustainedArgs),
}

#[tokio::main]
pub async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::WriteGenesis(args) => genesis::run(args),
        Command::RunWorkload(args) => workload::run(args).await,
        Command::Run { mode } => match mode {
            RunMode::Exec(args) => mode_exec::run(args).await,
            RunMode::E2e(args) => mode_e2e::run(args).await,
            RunMode::Sustained(args) => mode_sustained::run(args).await,
        },
        Command::Sweep(args) => sweep::run(args).await,
        Command::VerifyState(args) => verify::run(args).await,
        Command::Summarize(args) => report::summarize(args),
    }
}
```

- [ ] **Step 2: Verify compilation and CLI help**

```bash
cargo check -p bench-block-exec
cargo run -p bench-block-exec -- --help
cargo run -p bench-block-exec -- run --help
cargo run -p bench-block-exec -- run exec --help
cargo run -p bench-block-exec -- sweep --help
```

Expected: all help texts display correctly with all arguments documented.

- [ ] **Step 3: Commit**

```bash
git add bin/bench-block-exec/src/main.rs
git commit -m "feat(bench): integrate all modes and sweep into CLI"
```

---

## Task 11: Extend Report Summarization

**Files:**
- Modify: `bin/bench-block-exec/src/report.rs`

- [ ] **Step 1: Add V2 summary support**

Add a new function to `report.rs` that handles `BlockTimingV2` files alongside the existing `BlockTiming` summarizer. Append to the file:

```rust
use crate::engine::BlockTimingV2;

/// Summarize BlockTimingV2 JSON-lines files into extended TSV.
pub fn summarize_v2(results_dir: &str, output: Option<&str>) -> eyre::Result<()> {
    let dir = Path::new(results_dir);
    let files = walkdir(dir)?;

    let header = "engine\tmode\tworkload\tsenders\twarmup\tblocks\t\
        avg_txs\tinclusion%\t\
        avg_asm_ms\tavg_imp_ms\tavg_tot_ms\t\
        p50_ms\tp95_ms\tp99_ms\t\
        peak_tps\tavg_tps\tavg_mgas_s\t\
        degradation%\terrors";

    let mut rows: Vec<String> = Vec::new();

    for file_path in &files {
        let ext = file_path.extension().and_then(|e| e.to_str());
        if ext != Some("jsonl") {
            continue;
        }

        let reader = BufReader::new(fs::File::open(file_path)?);
        let mut entries: Vec<BlockTimingV2> = Vec::new();
        for line in reader.lines() {
            if let Ok(t) = serde_json::from_str::<BlockTimingV2>(&line?) {
                entries.push(t);
            }
        }

        if entries.len() <= 10 {
            continue;
        }

        // Skip first 10 as warmup
        let data = &entries[10..];
        let n = data.len();
        let errors = data.iter().filter(|t| t.error).count();

        let meta = &data[0]; // for grouping fields

        let avg_txs = data.iter().map(|t| t.tx_count as f64).sum::<f64>() / n as f64;
        let avg_incl = data.iter().map(|t| t.inclusion_rate).sum::<f64>() / n as f64 * 100.0;
        let avg_asm = data.iter().map(|t| t.assemble_ms).sum::<f64>() / n as f64;
        let avg_imp = data.iter().map(|t| t.import_ms).sum::<f64>() / n as f64;
        let avg_tot = data.iter().map(|t| t.total_ms).sum::<f64>() / n as f64;

        let mut tots: Vec<f64> = data.iter().map(|t| t.total_ms).collect();
        tots.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p50 = percentile(&tots, 50.0);
        let p95 = percentile(&tots, 95.0);
        let p99 = percentile(&tots, 99.0);

        let tps_values: Vec<f64> = data.iter().map(|t| t.tps).collect();
        let peak_tps = tps_values.iter().cloned().fold(0.0_f64, f64::max);
        let avg_tps = tps_values.iter().sum::<f64>() / n as f64;
        let avg_mgas = data.iter().map(|t| t.mgas_per_sec).sum::<f64>() / n as f64;

        // Degradation: last 100 blocks avg TPS vs first 100 blocks avg TPS
        let degradation = if n >= 200 {
            let first100: f64 = tps_values[..100].iter().sum::<f64>() / 100.0;
            let last100: f64 = tps_values[n-100..].iter().sum::<f64>() / 100.0;
            if first100 > 0.0 { (last100 / first100 - 1.0) * 100.0 } else { 0.0 }
        } else {
            0.0
        };

        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t\
            {:.0}\t{:.1}\t\
            {:.1}\t{:.1}\t{:.1}\t\
            {:.1}\t{:.1}\t{:.1}\t\
            {:.0}\t{:.0}\t{:.0}\t\
            {:.1}\t{}",
            meta.engine, meta.mode, meta.workload, meta.senders, meta.warmup_blocks, n,
            avg_txs, avg_incl,
            avg_asm, avg_imp, avg_tot,
            p50, p95, p99,
            peak_tps, avg_tps, avg_mgas,
            degradation, errors,
        ));
    }

    let mut out: Box<dyn Write> = if let Some(path) = output {
        Box::new(fs::File::create(path)?)
    } else {
        Box::new(std::io::stdout())
    };

    writeln!(out, "{}", header)?;
    for row in &rows {
        writeln!(out, "{}", row)?;
    }

    Ok(())
}
```

- [ ] **Step 2: Update SummarizeArgs to support V2 mode**

Add a `--v2` flag to `SummarizeArgs`:

```rust
#[derive(Args)]
pub struct SummarizeArgs {
    #[arg(long)]
    pub results_dir: String,
    #[arg(long)]
    pub output: Option<String>,
    /// Use V2 format (for new benchmark modes).
    #[arg(long, default_value = "false")]
    pub v2: bool,
}
```

Update the `summarize` function dispatch:

```rust
pub fn summarize(args: SummarizeArgs) -> eyre::Result<()> {
    if args.v2 {
        return summarize_v2(&args.results_dir, args.output.as_deref());
    }
    // ... existing logic unchanged ...
}
```

- [ ] **Step 3: Verify compilation**

```bash
cargo check -p bench-block-exec
```

- [ ] **Step 4: Commit**

```bash
git add bin/bench-block-exec/src/report.rs
git commit -m "feat(bench): add V2 summarization with MGas/s, degradation, inclusion rate"
```

---

## Task 12: Chart Generation Script

**Files:**
- Create: `local-test/bench-plot.py`

- [ ] **Step 1: Write bench-plot.py**

```python
#!/usr/bin/env python3
"""Generate benchmark charts from JSON-line results."""

import argparse
import json
import os
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def read_jsonl(path):
    """Read a JSON-lines file into a list of dicts."""
    records = []
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                records.append(json.loads(line))
    return records


def read_all_jsonl(directory, pattern="*.jsonl"):
    """Read all .jsonl files in a directory tree."""
    records = []
    for p in Path(directory).rglob(pattern):
        records.extend(read_jsonl(str(p)))
    return records


def read_sweep_summaries(directory):
    """Read sweep summary JSON files."""
    results = []
    for p in Path(directory).rglob("*-sweep-summary.json"):
        with open(p) as f:
            results.append(json.load(f))
    return results


# ── Chart 1: Sweep TPS / MGas Curve ──────────────────────────────────────────

def chart_sweep(input_dir, output_dir):
    summaries = read_sweep_summaries(input_dir)
    if not summaries:
        print("No sweep summaries found, skipping chart_sweep")
        return

    workloads = sorted(set(s["workload"] for s in summaries))
    engines = sorted(set(s["engine"] for s in summaries))
    colors = {"reth": "#2196F3", "geth": "#FF9800"}

    fig, axes = plt.subplots(1, len(workloads), figsize=(6 * len(workloads), 5), squeeze=False)

    for col, wl in enumerate(workloads):
        ax = axes[0][col]
        ax2 = ax.twinx()
        for eng in engines:
            pts = [s for s in summaries if s["engine"] == eng and s["workload"] == wl]
            if not pts:
                continue
            points = pts[0]["points"]
            x = [p["txs_per_block"] for p in points]
            tps = [p["median_tps"] for p in points]
            mgas = [p["median_mgas_s"] for p in points]

            ax.plot(x, tps, "o-", color=colors.get(eng, "gray"), label=f"{eng} TPS")
            ax2.plot(x, mgas, "s--", color=colors.get(eng, "gray"), alpha=0.6, label=f"{eng} MGas/s")

            # Mark inflection
            peak_idx = tps.index(max(tps))
            ax.annotate(f"{tps[peak_idx]:.0f}", (x[peak_idx], tps[peak_idx]),
                        textcoords="offset points", xytext=(0, 10), ha="center", fontsize=8)

        ax.set_title(wl)
        ax.set_xlabel("txs / block")
        ax.set_ylabel("TPS")
        ax2.set_ylabel("MGas/s")
        ax.legend(loc="upper left", fontsize=8)
        ax2.legend(loc="upper right", fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Sweep: TPS vs Block Size", fontsize=14)
    fig.tight_layout()
    fig.savefig(os.path.join(output_dir, "sweep-tps-curve.png"), dpi=150)
    plt.close(fig)
    print("  Generated sweep-tps-curve.png")


# ── Chart 2: Latency CDF ─────────────────────────────────────────────────────

def chart_latency_cdf(input_dir, output_dir):
    records = read_all_jsonl(input_dir)
    if not records:
        print("No records found, skipping chart_latency_cdf")
        return

    modes = ["exec", "e2e"]
    colors = {"reth": "#2196F3", "geth": "#FF9800"}

    fig, axes = plt.subplots(1, len(modes), figsize=(12, 5), squeeze=False)

    for col, mode in enumerate(modes):
        ax = axes[0][col]
        subset = [r for r in records if r.get("mode") == mode]
        if not subset:
            continue

        for eng in sorted(set(r["engine"] for r in subset)):
            vals = sorted(r["assemble_ms"] for r in subset if r["engine"] == eng)
            if not vals:
                continue
            y = np.linspace(0, 100, len(vals))
            ax.plot(vals, y, "-", color=colors.get(eng, "gray"), label=f"{eng} assemble")

            # P50, P95, P99 lines
            for pct in [50, 95, 99]:
                idx = int(len(vals) * pct / 100)
                idx = min(idx, len(vals) - 1)
                ax.axvline(vals[idx], color=colors.get(eng, "gray"), alpha=0.3, linestyle=":")

        ax.set_title(f"Mode: {mode}")
        ax.set_xlabel("Assemble Latency (ms)")
        ax.set_ylabel("Percentile (%)")
        ax.legend(fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Latency CDF", fontsize=14)
    fig.tight_layout()
    fig.savefig(os.path.join(output_dir, "latency-cdf.png"), dpi=150)
    plt.close(fig)
    print("  Generated latency-cdf.png")


# ── Chart 3: Sustained Time Series ───────────────────────────────────────────

def chart_sustained(input_dir, output_dir):
    records = [r for r in read_all_jsonl(input_dir) if r.get("mode") == "sustained"]
    if not records:
        print("No sustained records, skipping chart_sustained")
        return

    colors = {"reth": "#2196F3", "geth": "#FF9800"}
    warmups = sorted(set(r.get("warmup_blocks", 0) for r in records))
    engines = sorted(set(r["engine"] for r in records))
    workloads = sorted(set(r["workload"] for r in records))

    fig, axes = plt.subplots(len(workloads), 1, figsize=(12, 4 * len(workloads)), squeeze=False)

    for row, wl in enumerate(workloads):
        ax = axes[row][0]
        for eng in engines:
            for wu in warmups:
                subset = [r for r in records
                          if r["engine"] == eng and r["workload"] == wl
                          and r.get("warmup_blocks", 0) == wu
                          and r.get("rolling_avg_tps_100") is not None]
                if not subset:
                    continue
                x = [r["cumulative_blocks"] for r in subset]
                y = [r["rolling_avg_tps_100"] for r in subset]
                label = f"{eng} (warmup={wu})"
                ls = "-" if wu == 0 else "--"
                ax.plot(x, y, ls, color=colors.get(eng, "gray"), label=label, alpha=0.8)

        ax.set_title(wl)
        ax.set_xlabel("Block #")
        ax.set_ylabel("Rolling Avg TPS (100-block)")
        ax.legend(fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Sustained Block Production", fontsize=14)
    fig.tight_layout()
    fig.savefig(os.path.join(output_dir, "sustained-timeseries.png"), dpi=150)
    plt.close(fig)
    print("  Generated sustained-timeseries.png")


# ── Chart 4: Reth vs Geth Comparison ─────────────────────────────────────────

def chart_comparison(input_dir, output_dir):
    records = read_all_jsonl(input_dir)
    if not records:
        return

    # Group by (mode, workload) → avg TPS per engine
    from collections import defaultdict
    groups = defaultdict(lambda: defaultdict(list))
    for r in records:
        key = (r.get("mode", ""), r.get("workload", ""))
        groups[key][r["engine"]].append(r.get("tps", 0))

    labels = []
    reth_vals = []
    geth_vals = []
    for (mode, wl), eng_data in sorted(groups.items()):
        labels.append(f"{mode}\n{wl}")
        reth_vals.append(np.mean(eng_data.get("reth", [0])))
        geth_vals.append(np.mean(eng_data.get("geth", [0])))

    if not labels:
        return

    x = np.arange(len(labels))
    w = 0.35

    fig, ax = plt.subplots(figsize=(max(8, len(labels) * 1.5), 5))
    ax.bar(x - w/2, reth_vals, w, label="reth", color="#2196F3")
    ax.bar(x + w/2, geth_vals, w, label="geth", color="#FF9800")
    ax.set_xticks(x)
    ax.set_xticklabels(labels, fontsize=8)
    ax.set_ylabel("Average TPS")
    ax.set_title("Reth vs Geth Comparison")
    ax.legend()
    ax.grid(True, alpha=0.3, axis="y")

    fig.tight_layout()
    fig.savefig(os.path.join(output_dir, "reth-vs-geth-comparison.png"), dpi=150)
    plt.close(fig)
    print("  Generated reth-vs-geth-comparison.png")


# ── Chart 5: Multi-Sender Impact ─────────────────────────────────────────────

def chart_multi_sender(input_dir, output_dir):
    records = [r for r in read_all_jsonl(input_dir) if r.get("mode") == "e2e"]
    if not records:
        print("No e2e records, skipping chart_multi_sender")
        return

    from collections import defaultdict
    groups = defaultdict(lambda: defaultdict(list))
    for r in records:
        groups[r["workload"]][(r["engine"], r.get("senders", 1))].append(r.get("tps", 0))

    workloads = sorted(groups.keys())
    colors = {"reth": "#2196F3", "geth": "#FF9800"}

    fig, axes = plt.subplots(1, len(workloads), figsize=(6 * len(workloads), 5), squeeze=False)

    for col, wl in enumerate(workloads):
        ax = axes[0][col]
        data = groups[wl]
        sender_counts = sorted(set(s for (_, s) in data.keys()))

        for eng in ["reth", "geth"]:
            means = []
            for sc in sender_counts:
                vals = data.get((eng, sc), [])
                means.append(np.mean(vals) if vals else 0)
            ax.plot(sender_counts, means, "o-", color=colors.get(eng, "gray"), label=eng)

        ax.set_title(wl)
        ax.set_xlabel("Sender Count")
        ax.set_ylabel("Average TPS")
        ax.set_xscale("log")
        ax.legend(fontsize=8)
        ax.grid(True, alpha=0.3)

    fig.suptitle("Multi-Sender Impact on TPS", fontsize=14)
    fig.tight_layout()
    fig.savefig(os.path.join(output_dir, "multi-sender-impact.png"), dpi=150)
    plt.close(fig)
    print("  Generated multi-sender-impact.png")


# ── Main ──────────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Generate benchmark charts")
    parser.add_argument("--input", required=True, help="Results directory")
    parser.add_argument("--output", required=True, help="Output directory for PNG files")
    parser.add_argument("--type", choices=["sweep", "cdf", "sustained", "comparison", "sender", "all"],
                        default="all", help="Chart type to generate")
    args = parser.parse_args()

    os.makedirs(args.output, exist_ok=True)

    chart_funcs = {
        "sweep": chart_sweep,
        "cdf": chart_latency_cdf,
        "sustained": chart_sustained,
        "comparison": chart_comparison,
        "sender": chart_multi_sender,
    }

    if args.type == "all":
        for name, func in chart_funcs.items():
            print(f"Generating {name} chart...")
            func(args.input, args.output)
    else:
        chart_funcs[args.type](args.input, args.output)

    print(f"\nAll charts saved to {args.output}/")


if __name__ == "__main__":
    main()
```

- [ ] **Step 2: Verify script syntax**

```bash
python3 -c "import ast; ast.parse(open('local-test/bench-plot.py').read()); print('OK')"
```

- [ ] **Step 3: Commit**

```bash
git add -f local-test/bench-plot.py
git commit -m "feat(bench): add bench-plot.py for chart generation (5 chart types)"
```

---

## Task 13: Orchestration Script Rewrite

**Files:**
- Modify: `local-test/bench-block-exec.sh`

- [ ] **Step 1: Rewrite bench-block-exec.sh with 4-phase orchestration**

The script is large (~400 lines). The key changes from the existing script:

```bash
#!/usr/bin/env bash
set -euo pipefail

# ── Configuration ─────────────────────────────────────────────────────────────
RETH_BIN="${RETH_BIN:-./target/release/morph-reth}"
GETH_BIN="${GETH_BIN:-../go-ethereum/build/bin/geth}"
BENCH_BIN="${BENCH_BIN:-./target/release/bench-block-exec}"
JWT_SECRET="./local-test/jwt-secret.txt"
CHAIN_ID=99999
RESULTS_DIR="bench-results/$(date +%Y%m%d-%H%M%S)"
FORCE=0

# Contract bytecodes (from forge inspect)
BENCH_TOKEN_CODE="$(cat local-test/bench-contracts/out/BenchToken.sol/BenchToken.json | python3 -c 'import sys,json; print(json.load(sys.stdin)["deployedBytecode"]["object"])')"
BENCH_SWAP_CODE="$(cat local-test/bench-contracts/out/BenchSwap.sol/BenchSwap.json | python3 -c 'import sys,json; print(json.load(sys.stdin)["deployedBytecode"]["object"])')"

# Ports
HTTP_PORT=8545
AUTH_PORT=8551

# ── Helpers ───────────────────────────────────────────────────────────────────
generate_genesis() {
    local senders=$1 gas_limit=$2 max_tx=$3 output=$4
    $BENCH_BIN write-genesis \
        --output "$output" \
        --senders "$senders" \
        --gas-limit "$gas_limit" \
        --max-tx-per-block "$max_tx" \
        --bench-token-code "$BENCH_TOKEN_CODE" \
        --bench-swap-code "$BENCH_SWAP_CODE"
}

start_reth() {
    local datadir=$1 genesis=$2
    pm2 start "$RETH_BIN" --name "bench-node" -- node \
        --chain "$genesis" \
        --datadir "$datadir" \
        --http --http.addr 127.0.0.1 --http.port $HTTP_PORT \
        --http.api "web3,debug,eth,txpool,net" \
        --authrpc.addr 127.0.0.1 --authrpc.port $AUTH_PORT \
        --authrpc.jwtsecret "$JWT_SECRET" \
        --morph.max-tx-payload-bytes 1073741824 \
        --engine.persistence-threshold 4096 \
        --engine.memory-block-buffer-target 4096
    wait_for_rpc
}

start_geth() {
    local datadir=$1 genesis=$2
    $GETH_BIN init --datadir "$datadir" "$genesis"
    pm2 start "$GETH_BIN" --name "bench-node" -- \
        --datadir "$datadir" \
        --gcmode archive --syncmode full \
        --http --http.addr 127.0.0.1 --http.port $HTTP_PORT \
        --http.api "web3,eth,debug,txpool,net,morph,engine" \
        --authrpc.addr 127.0.0.1 --authrpc.port $AUTH_PORT \
        --authrpc.jwtsecret "$JWT_SECRET" \
        --maxpeers 0 \
        --cache 8192 \
        --txpool.globalslots 100000 \
        --txpool.accountslots 1000
    wait_for_rpc
}

stop_node() {
    pm2 delete bench-node 2>/dev/null || true
    sleep 1
}

wait_for_rpc() {
    local timeout=120
    for i in $(seq 1 $timeout); do
        if curl -s -X POST -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","method":"web3_clientVersion","params":[],"id":1}' \
            "http://127.0.0.1:$HTTP_PORT" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    echo "ERROR: RPC not ready after ${timeout}s"
    return 1
}

run_test() {
    local engine=$1 mode=$2 workload=$3 senders=$4 txs=$5 blocks=$6 warmup=$7
    local tag="${engine}-${workload}-s${senders}-w${warmup}"
    local output_file="$RESULTS_DIR/${mode}/${tag}.jsonl"

    # Skip if exists and not forced
    if [[ -f "$output_file" && "$FORCE" == "0" ]]; then
        echo "SKIP (exists): $output_file"
        return 0
    fi

    mkdir -p "$(dirname "$output_file")"
    local datadir="bench-data/${tag}-$$"
    local genesis="/tmp/bench-genesis-$$.json"

    generate_genesis "$senders" "0x2540BE400" 0 "$genesis"

    if [[ "$engine" == "reth" ]]; then
        start_reth "$datadir" "$genesis"
    else
        start_geth "$datadir" "$genesis"
    fi

    local mode_args=""
    case "$mode" in
        exec)
            $BENCH_BIN run exec \
                --engine-rpc "http://127.0.0.1:$AUTH_PORT" \
                --jwt-secret "$JWT_SECRET" \
                --workload "$workload" \
                --txs-per-block "$txs" \
                --blocks "$blocks" \
                --output "$output_file" \
                --engine-name "$engine" \
                --chain-id $CHAIN_ID
            ;;
        e2e)
            $BENCH_BIN run e2e \
                --engine-rpc "http://127.0.0.1:$AUTH_PORT" \
                --jwt-secret "$JWT_SECRET" \
                --http-rpc "http://127.0.0.1:$HTTP_PORT" \
                --workload "$workload" \
                --txs-per-block "$txs" \
                --blocks "$blocks" \
                --senders "$senders" \
                --output "$output_file" \
                --engine-name "$engine" \
                --chain-id $CHAIN_ID
            ;;
        sustained)
            $BENCH_BIN run sustained \
                --engine-rpc "http://127.0.0.1:$AUTH_PORT" \
                --jwt-secret "$JWT_SECRET" \
                --http-rpc "http://127.0.0.1:$HTTP_PORT" \
                --workload "$workload" \
                --txs-per-block "$txs" \
                --blocks "$blocks" \
                --warmup-blocks "$warmup" \
                --senders "$senders" \
                --output "$output_file" \
                --engine-name "$engine" \
                --chain-id $CHAIN_ID
            ;;
    esac

    stop_node
    rm -rf "$datadir" "$genesis"
}

run_sweep() {
    local engine=$1 workload=$2
    local datadir="bench-data/sweep-${engine}-${workload}-$$"
    local genesis="/tmp/bench-genesis-sweep-$$.json"

    generate_genesis 1 "0x2540BE400" 0 "$genesis"

    if [[ "$engine" == "reth" ]]; then
        start_reth "$datadir" "$genesis"
    else
        start_geth "$datadir" "$genesis"
    fi

    $BENCH_BIN sweep \
        --engine-rpc "http://127.0.0.1:$AUTH_PORT" \
        --jwt-secret "$JWT_SECRET" \
        --workload "$workload" \
        --blocks-per-step 30 \
        --output-dir "$RESULTS_DIR/sweep" \
        --engine-name "$engine" \
        --chain-id $CHAIN_ID

    stop_node
    rm -rf "$datadir" "$genesis"
}

# ── Main ──────────────────────────────────────────────────────────────────────
echo "=== Max TPS Benchmark ==="
echo "Results: $RESULTS_DIR"
mkdir -p "$RESULTS_DIR"

WORKLOADS="eth-transfer erc20-transfer uniswap-swap"
ENGINES="reth geth"

# ── Phase 1: Sweep ────────────────────────────────────────────────────────────
echo ""
echo "════ Phase 1: Sweep (find inflection points) ════"
for engine in $ENGINES; do
    for wl in $WORKLOADS; do
        echo "── Sweep: $engine / $wl"
        run_sweep "$engine" "$wl"
    done
done

# Read sweep results to determine optimal txs/block per engine+workload
# Default fallback: 5000
get_inflection_txs() {
    local engine=$1 workload=$2
    local file="$RESULTS_DIR/sweep/${engine}-${workload}-sweep-summary.json"
    if [[ -f "$file" ]]; then
        python3 -c "import json; d=json.load(open('$file')); print(d.get('inflection_txs', 5000))"
    else
        echo 5000
    fi
}

# ── Phase 2: Precise Matrix ──────────────────────────────────────────────────
echo ""
echo "════ Phase 2: Precise matrix tests ════"
for engine in $ENGINES; do
    for wl in $WORKLOADS; do
        PEAK=$(get_inflection_txs "$engine" "$wl")

        # Mode A: exec (single sender)
        echo "── Exec: $engine / $wl / txs=$PEAK"
        run_test "$engine" exec "$wl" 1 "$PEAK" 50 0

        # Mode B: e2e (1, 100, 1000 senders)
        E2E_TXS=$(python3 -c "print(int($PEAK * 0.8))")
        for senders in 1 100 1000; do
            echo "── E2E: $engine / $wl / senders=$senders / txs=$E2E_TXS"
            run_test "$engine" e2e "$wl" "$senders" "$E2E_TXS" 200 0
        done

        # Mode C: sustained (1, 100 senders)
        SUST_TXS=$(python3 -c "print(int($PEAK * 0.5))")
        for senders in 1 100; do
            echo "── Sustained: $engine / $wl / senders=$senders / txs=$SUST_TXS"
            run_test "$engine" sustained "$wl" "$senders" "$SUST_TXS" 1000 0
        done
    done
done

# ── Phase 3: State Degradation ────────────────────────────────────────────────
echo ""
echo "════ Phase 3: State degradation tests ════"
for engine in $ENGINES; do
    for wl in $WORKLOADS; do
        PEAK=$(get_inflection_txs "$engine" "$wl")
        SUST_TXS=$(python3 -c "print(int($PEAK * 0.5))")
        echo "── Degradation: $engine / $wl / warmup=500"
        run_test "$engine" sustained "$wl" 100 "$SUST_TXS" 1000 500
    done
done

# ── Phase 4: Summarize + Plot ─────────────────────────────────────────────────
echo ""
echo "════ Phase 4: Summarize + Plot ════"
$BENCH_BIN summarize --results-dir "$RESULTS_DIR" --output "$RESULTS_DIR/summary.tsv" --v2
python3 local-test/bench-plot.py --all --input "$RESULTS_DIR" --output "$RESULTS_DIR/charts"

echo ""
echo "=== COMPLETE ==="
echo "Summary: $RESULTS_DIR/summary.tsv"
echo "Charts:  $RESULTS_DIR/charts/"
```

- [ ] **Step 2: Make script executable**

```bash
chmod +x local-test/bench-block-exec.sh
```

- [ ] **Step 3: Verify syntax**

```bash
bash -n local-test/bench-block-exec.sh
```

Expected: no syntax errors.

- [ ] **Step 4: Commit**

```bash
git add local-test/bench-block-exec.sh
git commit -m "feat(bench): rewrite orchestration script with 4-phase max TPS testing"
```

---

## Task 14: Integration Smoke Test

**Files:** None (testing only)

- [ ] **Step 1: Build the benchmark binary**

```bash
cd /Users/panos/workspace/morph-reth
cargo build -p bench-block-exec --release
```

- [ ] **Step 2: Test genesis generation**

```bash
./target/release/bench-block-exec write-genesis \
    --output /tmp/test-genesis.json \
    --senders 10 \
    --gas-limit 0x2540BE400 \
    --max-tx-per-block 0
```

Verify the output contains 10 sender accounts and gas limit is 10B.

```bash
python3 -c "
import json
g = json.load(open('/tmp/test-genesis.json'))
print('gasLimit:', g['gasLimit'])
print('maxTxPerBlock:', g['config']['morph']['maxTxPerBlock'])
accts = [k for k in g['alloc'] if k != '530000000000000000000000000000000000000a']
print('accounts:', len(accts))
"
```

Expected:
```
gasLimit: 0x2540BE400
maxTxPerBlock: 10000000
accounts: 10
```

- [ ] **Step 3: Test CLI help for all new subcommands**

```bash
./target/release/bench-block-exec run exec --help
./target/release/bench-block-exec run e2e --help
./target/release/bench-block-exec run sustained --help
./target/release/bench-block-exec sweep --help
```

Expected: all display correctly.

- [ ] **Step 4: Quick smoke test with reth (exec mode, 5 blocks, 100 txs)**

This requires a running reth node. Start one manually:

```bash
./target/release/bench-block-exec write-genesis \
    --output /tmp/smoke-genesis.json \
    --senders 1 \
    --gas-limit 0x2540BE400 \
    --max-tx-per-block 0

pm2 start ./target/release/morph-reth --name smoke-reth -- node \
    --chain /tmp/smoke-genesis.json \
    --datadir /tmp/smoke-reth-data \
    --http --http.addr 127.0.0.1 --http.port 8545 \
    --http.api "web3,debug,eth,txpool,net" \
    --authrpc.addr 127.0.0.1 --authrpc.port 8551 \
    --authrpc.jwtsecret ./local-test/jwt-secret.txt \
    --engine.persistence-threshold 4096 \
    --engine.memory-block-buffer-target 4096

# Wait for RPC
sleep 5

./target/release/bench-block-exec run exec \
    --engine-rpc http://127.0.0.1:8551 \
    --jwt-secret ./local-test/jwt-secret.txt \
    --workload eth-transfer \
    --txs-per-block 100 \
    --blocks 5 \
    --output /tmp/smoke-results.jsonl \
    --engine-name reth

# Check output
cat /tmp/smoke-results.jsonl | python3 -c "
import sys, json
for line in sys.stdin:
    r = json.loads(line)
    print(f\"Block {r['block_number']}: {r['tx_count']} txs, {r['tps']:.0f} TPS, {r['mgas_per_sec']:.0f} MGas/s\")
"

pm2 delete smoke-reth
rm -rf /tmp/smoke-reth-data /tmp/smoke-genesis.json
```

Expected: 5 lines of output with non-zero TPS and MGas/s values.

- [ ] **Step 5: Commit (if any fixes were needed)**

```bash
git add -A
git commit -m "fix(bench): integration fixes from smoke test"
```

---

## Dependency Graph

```
Task 1 (BenchSwap contract)
  │
  ├─→ Task 5 (genesis.rs: needs contract bytecodes)
  │
Task 2 (BlockTimingV2) ─→ Task 6, 7, 8, 9 (all modes use BlockTimingV2)
  │
Task 3 (tx_factory: accounts) ─→ Task 4 (tx_factory: builders)
  │                                 │
  │                                 ├─→ Task 5 (genesis.rs: uses generate_senders)
  │                                 ├─→ Task 6 (mode_exec)
  │                                 ├─→ Task 7 (mode_e2e)
  │                                 └─→ Task 8 (mode_sustained: reuses mode_e2e helpers)
  │
Task 9 (sweep) ─→ needs Task 6 (mode_exec)
  │
Task 10 (CLI integration) ─→ needs Tasks 6, 7, 8, 9
  │
Task 11 (report extensions) ─→ needs Task 2 (BlockTimingV2)
  │
Task 12 (bench-plot.py) ─→ independent
  │
Task 13 (shell script) ─→ needs all above
  │
Task 14 (smoke test) ─→ needs all above
```

**Recommended execution order:** 1 → 2 → 3 → 4 → 5 → 6 → 7 → 8 → 9 → 10 → 11 → 12 → 13 → 14

Tasks 12 (Python script) can be done in parallel with Tasks 6-11.
