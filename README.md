# Morph Reth

[![Build](https://github.com/morph-l2/morph-reth/actions/workflows/build.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/build.yml)
[![Test](https://github.com/morph-l2/morph-reth/actions/workflows/test.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/test.yml)
[![Lint](https://github.com/morph-l2/morph-reth/actions/workflows/lint.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/lint.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#license)

## Overview

Morph Reth is the next-generation execution client for [Morph](https://www.morph.network/), a decentralized Layer 2 scaling solution for Ethereum. Built on the modular [Reth SDK](https://github.com/paradigmxyz/reth), it provides high-performance block execution with Morph-specific features.

### Key Features

- **L1 Message Support**: Seamless bridging of assets and messages from Ethereum L1 to Morph L2
- **Morph Transaction**: Versioned Morph EVM+ transaction with alternative fee-token support and Jade-era reference/memo fields
- **Morph Hardforks**: Implements Morph hardfork logic through Jade, with bundled Mainnet and Hoodi chainspecs scheduled through Jade
- **Custom Engine API**: L2-specific Engine API for sequencer block building and validation
- **L1 Fee Validation**: Transaction pool with L1 data fee affordability checks

### Supported Networks

| Network | Chain ID | Type |
|---------|----------|------|
| Morph Mainnet | 2818 | Production |
| Morph Hoodi | 2910 | Testnet |

## Architecture

Morph Reth is designed as a modular extension of Reth, following the SDK pattern:

```text
morph-reth/
├── bin/
│   └── morph-reth/      # Main CLI binary
└── crates/
    ├── chainspec/        # Morph chain specification and hardfork definitions
    ├── consensus/        # L2 block validation (header, body, L1 messages)
    ├── engine-api/       # Custom L2 Engine API (assemble/validate/import blocks)
    ├── evm/              # EVM configuration and block execution
    ├── node/             # Node assembly with component builders
    ├── payload/
    │   ├── builder/      # Block building logic
    │   └── types/        # Engine API types (MorphExecutionData, etc.)
    ├── primitives/       # Core types (transactions, receipts)
    ├── revm/             # L1 fee calculation, token fee logic
    ├── rpc/              # RPC implementation and type conversions
    └── txpool/           # Transaction pool with L1 fee validation
```

### Crates

| Crate | Description |
|-------|-------------|
| `morph-reth` | Main CLI binary — Morph L2 Execution Layer Client |
| `morph-chainspec` | Chain specification with Morph hardfork definitions |
| `morph-consensus` | Consensus validation for L2 blocks |
| `morph-engine-api` | Custom L2 Engine API for sequencer interaction |
| `morph-evm` | EVM configuration and receipt builder |
| `morph-node` | Node implementation with modular component builders |
| `morph-payload-types` | Engine API payload types |
| `morph-payload-builder` | Block building implementation |
| `morph-primitives` | Transaction and receipt types |
| `morph-revm` | L1 fee and token fee calculations |
| `morph-rpc` | RPC implementation and type conversions |
| `morph-txpool` | Transaction pool with L1 fee and MorphTx validation |

## Getting Started

### Prerequisites

- Rust 1.88 or later
- Cargo

### Building from Source

```bash
git clone https://github.com/morph-l2/morph-reth.git
cd morph-reth
cargo build --release
```

### Running the Node

Morph Reth is a sequencer-driven L2 execution client. Block production and import are driven through the custom L2 Engine API by an external sequencer or derivation pipeline, and the execution layer must stay aligned with the Morph consensus node state.

```bash
# Generate a JWT secret for Engine API authentication
openssl rand -hex 32 > jwt.hex

# Run on Morph mainnet
./target/release/morph-reth node \
  --chain mainnet \
  --http \
  --authrpc.jwtsecret jwt.hex

# Run on Hoodi testnet
./target/release/morph-reth node \
  --chain hoodi \
  --http \
  --authrpc.jwtsecret jwt.hex

# Run with a custom genesis file
./target/release/morph-reth node \
  --chain /path/to/genesis.json \
  --http \
  --authrpc.jwtsecret jwt.hex
```

> **Note:** The commands above only start the Morph execution client. In production, bootstrap with a paired `reth` + `node` snapshot at the same height, because Morph EL state must stay aligned with the consensus node's `node-data`. The node still requires a sequencer or derivation pipeline to drive the custom Engine API (`engine_assembleL2Block`, `engine_newL2Block`, etc.) for block production and import. See [Morph Documentation](https://docs.morph.network/) for deployment guides.

#### Morph-Specific CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--morph.max-tx-payload-bytes` | 122880 (120KB) | Maximum transaction payload bytes per block |
| `--morph.max-tx-per-block` | None (unlimited) | Maximum number of transactions per block |
| `--rpc.eth-proof-window` | 0 (disabled) | Max historical blocks for `eth_getProof` (up to 1209600) |

### Running Tests

```bash
# Run all tests
cargo test --all

# Run tests for a specific crate
cargo test -p morph-consensus
```

### Development

```bash
# Format code
cargo fmt --all

# Run clippy
cargo clippy --all --all-targets -- -D warnings

# Run doc tests
cargo test --doc --all --verbose
```

## Morph L2 Specifics

### Transaction Types

Morph supports the following transaction types:

| Type | ID | Description |
|------|-----|-------------|
| Legacy | `0x00` | Standard legacy transactions |
| EIP-2930 | `0x01` | Access list transactions |
| EIP-1559 | `0x02` | Dynamic fee transactions |
| EIP-7702 | `0x04` | Delegate EOA execution to smart contract code |
| L1 Message | `0x7e` | L1-to-L2 deposit messages |
| Morph Transaction | `0x7f` | Morph EVM+ transaction with enhanced features |

### L1 Messages

L1 messages are special deposit transactions that originate from Ethereum L1:

- Must appear at the beginning of each block
- Must have strictly sequential `queue_index` values
- Gas is prepaid on L1, so no L2 gas fee is charged
- Cannot be sent via the mempool (sequencer only)

### Morph Transaction

Morph Transaction (`0x7f`) is a versioned custom transaction type that extends EIP-1559-style transactions with alternative fee payment and, from Jade onward, optional metadata fields:

| Version | Availability | Description |
|---------|--------------|-------------|
| V0 | Always | Requires `fee_token_id > 0`, uses an active fee token from the L2 Token Registry, and does not support `reference` or `memo` |
| V1 | Jade+ | Adds optional `reference` (32 bytes) and `memo` (max 64 bytes); `fee_token_id == 0` uses the normal ETH-fee path, while `fee_token_id > 0` uses an active registry token |

### Hardforks

Bernoulli and Curie use block-based activation; Morph203, Viridian, Emerald, and Jade use timestamp-based activation.

The codebase implements hardfork logic through Jade, and the bundled Mainnet and Hoodi chainspecs include activation timestamps through Jade.

| Hardfork | Activation | Description |
|----------|------------|-------------|
| Bernoulli | Block | Initial L2 launch with disabled ripemd160 and blake2f precompiles |
| Curie | Block | Introduces blob-based L1 data-fee calculation and initializes the Curie L1 Gas Price Oracle fields |
| Morph203 | Timestamp | Re-enable ripemd160 and blake2f precompiles |
| Viridian | Timestamp | EIP-7702 EOA delegation support |
| Emerald | Timestamp | BLS12-381 and P256verify precompiles |
| Jade | Timestamp | MPT state root validation, MorphTx V1 with reference and memo fields |

Before Jade, Morph uses ZK-trie (Poseidon hash) state roots. morph-reth skips ZK-trie state-root validation pre-Jade and enables MPT state-root validation from Jade onward.

### Engine API

Morph provides a custom L2 Engine API (different from the standard Ethereum Engine API) for sequencer interaction:

| Method | Description |
|--------|-------------|
| `engine_assembleL2Block` | Build executable L2 block data for the next height; the sequencer supplies L1-message transactions via the `transactions` parameter, and L2 transactions are pulled from the txpool |
| `engine_validateL2Block` | Validate executable block data without importing it |
| `engine_newL2Block` | Import a new L2 block via `newPayload` + `forkchoiceUpdated` and advance the canonical head |
| `engine_newSafeL2Block` | Rebuild and import a safe L2 block from derivation inputs |
| `engine_setBlockTags` | Update safe/finalized block tags without importing a block |

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines on how to contribute.

## License

Licensed under either of:

- [Apache License, Version 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

at your option.

## Links

- [Morph Website](https://morph.network/)
- [Morph Documentation](https://docs.morph.network/)
- [Morph GitHub](https://github.com/morph-l2)
- [Reth Documentation](https://reth.rs/)
