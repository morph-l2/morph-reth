# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Development Commands

```bash
# Build
cargo build --release

# Test
cargo test --all                              # all tests
cargo test -p morph-consensus                 # single crate
cargo test -p morph-primitives test_name      # single test
cargo test -p morph-consensus -- --nocapture  # with stdout

# Lint (CI runs these)
cargo fmt --all -- --check                    # format check
cargo clippy --all --all-targets -- -D warnings

# Quick compile check (faster iteration than full build)
cargo check -p <crate-name>
```

Nightly is **not** required. CI uses stable toolchain for fmt, clippy, and tests.

## Architecture

Morph Reth is an L2 execution client built as a **Reth SDK extension** — it extends reth via its node-builder/SDK extension points rather than forking reth source. All reth crates are pinned to a fork at `morph-l2/reth` rev `b8c8a9411bb1b4668119559e1b752bae18a76c5a` (v1.10.2-based patch branch). This fork temporarily carries the `PayloadValidator` state-root validation hook needed for Morph pre-MPTFork semantics until an upstream reth equivalent is available.

### Crate Dependency Flow

```
bin/morph-reth
  └── morph-node          ← Node assembly, add-ons, engine validator
        ├── morph-engine-api      ← Custom L2 Engine API (assembleL2Block, newL2Block, etc.)
        ├── morph-payload-builder ← Block building (L1 messages + pool txs)
        ├── morph-rpc             ← eth_ namespace customizations
        ├── morph-txpool          ← Transaction pool validation
        ├── morph-evm             ← EVM config, receipt builder
        ├── morph-consensus       ← Header/body/L1-message validation
        └── morph-payload-types   ← ExecutableL2Data, SafeL2Data, payload attributes
              └── morph-primitives    ← MorphTxEnvelope, MorphReceipt, MorphHeader, Block
                    morph-chainspec   ← Chain specs (mainnet/hoodi), hardforks, system contract constants
                          morph-revm  ← L1 fee calc, token fee logic
```

### Key Design Decisions

**Engine API integration**: The custom L2 Engine API (`morph-engine-api`) does **not** bypass reth's engine tree. All block imports flow through `ConsensusEngineHandle.new_payload()` + `fork_choice_updated()`, which routes to reth's `EngineApiTreeHandler`. The custom API is a thin adapter layer registered on the authenticated RPC namespace.

**State root before MPTFork**: Morph uses ZK-trie (Poseidon hash) before the MPTFork hardfork. morph-reth has no zktrie implementation — it computes MPT roots but **skips state root validation** pre-MPTFork via `MorphEngineValidator::validate_computed_state_root()`. Genesis state roots are hardcoded ZK-trie values from go-ethereum.

**PayloadValidator hook**: `MorphEngineValidator` (in `morph-node`) implements reth's `PayloadValidator` trait. It handles withdraw-trie-root verification via a cache of expectations set during `convert_payload_to_block()` and checked in `validate_block_post_execution_with_hashed_state()`.

### Custom Transaction Types

| Type | ID | Key trait method |
|------|-----|-----------------|
| L1 Message | `0x7E` | `is_l1_msg()`, `queue_index()` |
| Morph Tx | `0x7F` | `fee_token()`, `reference_key()`, `memo()` |

L1 messages must appear at the start of each block with sequential `queue_index` values. Gas is prepaid on L1.

### Hardfork Order

Bernoulli (block) → Curie (block) → Morph203 (ts) → Viridian (ts) → Emerald (ts) → MPTFork (ts)

Check activation: `chain_spec.is_bernoulli_active_at_block(n)`, `chain_spec.is_emerald_active_at_timestamp(t)`, `chain_spec.is_mpt_fork_active_at_timestamp(t)`.

### System Contract Constants

`L2_MESSAGE_QUEUE_ADDRESS` and `L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT` live in `morph-chainspec/src/constants.rs` (following scroll-reth convention).

## Node Synchronization & State

### Architecture Constraint (EL/Node State Coupling)
The morph execution layer (reth) should **not** be synced completely independently of the consensus node (e.g., via reth's pipeline staged sync `import`). The Morph node maintains its own strict databases (Tendermint state and L1 message pointers in `node-data/`). If the EL jumps ahead by thousands of blocks without the node processing them, the node's Tendermint state and L1 sync pointers will become completely out of sync, breaking consensus.

### Recommended Bootstrap Approach
The proper way to bootstrap a new morph-reth node from scratch in production is to download a **complete paired snapshot tarball** containing both the `reth` database and the `node` (consensus) databases at the exact same block height. Once extracted, both components start at the snapshot height and continue "live syncing" via the Engine API together.

### Performance Caveat for Engine API Sync
When syncing historical blocks via the custom Engine API (`newL2Block`), reth will by default use a parallel `StateRootTask` that takes ~500ms+ per block (designed for large L1 blocks). For fast L2 historical sync, you MUST pass `--engine.legacy-state-root` to the reth node to force serial incremental state root computation, which drops the overhead to ~1ms per block.

## Workspace Configuration

- Rust edition **2024**, minimum rust-version **1.88**, resolver **3**
- Clippy lints configured in root `Cargo.toml` under `[workspace.lints]` — `uninlined-format-args`, `use-self`, `redundant-clone` are warn-level
- Each crate uses `#![cfg_attr(not(test), warn(unused_crate_dependencies))]` — add `use some_crate as _` for deps only used transitively or behind feature gates

## Reference Projects

All located under `~/workspace/`. Use these for understanding patterns, API design, and cross-referencing implementations:

| Directory | Description | When to consult |
|-----------|-------------|-----------------|
| `scroll-reth` | Scroll's reth implementation. Morph's go-ethereum was originally based on Scroll's, so their reth code is the closest architectural reference. | Crate structure, system contract constants layout, L1 message handling, custom tx types |
| `morph` | Morph's official go-ethereum (geth) implementation. The canonical reference for all L2 logic. | Verifying engine API behavior, block building logic, consensus rules, hardfork semantics |
| `reth` | Morph fork of upstream reth by Paradigm. The SDK/framework morph-reth extends. | Understanding reth traits (`PayloadValidator`, `PayloadBuilder`, `ConsensusEngineHandle`), node-builder patterns, engine tree internals |
| `tempo` | Tempo's reth-based L2 with stablecoin payment as a highlight. | Alternative fee token patterns, EVM+ transaction design |

## Code Style & Design Patterns

Guidelines derived from upstream reth conventions (see `paradigmxyz-reth/CLAUDE.md` and `scroll-reth/CLAUDE.md`).

### Type Ordering in Files

The file's primary type (matching the filename) comes first, followed by supporting public types, then private types and helpers.

```rust
use ...;

/// Primary type of this file (matches filename).
pub struct MorphPayloadBuilder { ... }

impl MorphPayloadBuilder { ... }

// Public auxiliary types that support the primary type
pub struct MorphBuilderConfig { ... }

// Public traits related to the primary type
pub trait MorphPayloadTransactions { ... }

// Private helper types
struct ExecutionInfo { ... }

// Private helper functions
fn build_payload_inner() { ... }
```

### Commenting Guidelines

Write comments that explain **why**, not **what**. Future readers won't have PR context.

```rust
// GOOD - explains non-obvious behavior
// L1 message gas is prepaid on L1, so no fees are collected here.
let gas_used = if recovered_tx.is_l1_msg() { ... }

// GOOD - documents constraints
// Must be done before finish() consumes the builder
let withdraw_trie_root = read_withdraw_trie_root(builder.evm_mut().db_mut())?;

// BAD - restates the code
// Increment transaction count
info.transaction_count += 1;
```

### Logging

Use `tracing` with structured fields and appropriate target namespaces:

```rust
tracing::debug!(
    target: "payload_builder",
    tx_index = tx_idx,
    %error,
    ?recovered_tx,
    "invalid L1 message transaction in payload attributes"
);
```

Target naming convention: `"morph::engine"` for engine API, `"payload_builder"` for block building, `"morph::node"` for node-level events.

### Error Handling

- Use `thiserror` for error enums with structured variants (not just `String`).
- Prefer specific error variants over generic `String` wrapping where the caller needs to distinguish error types.
- Use `PayloadBuilderError::other(MorphPayloadBuilderError::...)` to wrap domain errors into reth framework error types.

### Performance

- Avoid allocations in hot paths — use references and borrowing.
- Use `spawn_blocking` for CPU-intensive work inside async contexts.
- Use `Arc<SealedBlock>` to share sealed blocks without cloning.

### Making Components Generic

Follow reth's pattern of replacing concrete types with trait-bounded generics to enable reuse:

```rust
// Concrete → Generic
impl<Pool, Client, Txs> PayloadBuilder for MorphPayloadBuilder<Pool, Client, Txs>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Txs: MorphPayloadTransactions<Pool::Transaction>,
{ ... }
```

## Networks

| Network | Chain ID | Spec constant |
|---------|----------|---------------|
| Mainnet | 2818 | `MORPH_MAINNET` |
| Hoodi (testnet) | 2910 | `MORPH_HOODI` |
