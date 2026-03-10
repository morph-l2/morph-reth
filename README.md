# Morph Reth

[![Build](https://github.com/morph-l2/morph-reth/actions/workflows/build.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/build.yml)
[![Test](https://github.com/morph-l2/morph-reth/actions/workflows/test.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/test.yml)
[![Lint](https://github.com/morph-l2/morph-reth/actions/workflows/lint.yml/badge.svg)](https://github.com/morph-l2/morph-reth/actions/workflows/lint.yml)
[![License](https://img.shields.io/badge/license-MIT%2FApache--2.0-blue)](#license)

## Overview

Morph Reth is the next-generation execution client for [Morph](https://www.morph.network/), a decentralized Layer 2 scaling solution for Ethereum. Built on the modular [Reth SDK](https://github.com/paradigmxyz/reth), it provides high-performance block execution with Morph-specific features.

### Key Features

- **L1 Message Support**: Seamless bridging of assets and messages from Ethereum L1 to Morph L2
- **Morph Transaction**: Morph EVM+ transaction enabling alternative token fees, reference key indexing, and memo attachment
- **Morph Hardforks**: Full support for Morph's upgrade schedule (Bernoulli, Curie, Morph203, Viridian, Emerald, Jade)
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

Morph Reth is a sequencer-driven L2 execution client. It does **not** sync blocks via P2P — blocks are delivered through the custom L2 Engine API by an external sequencer or derivation pipeline.

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

> **Note:** The node requires a sequencer or derivation pipeline to call the Engine API (`engine_assembleL2Block`, `engine_newL2Block`, etc.) for block production and import. See [Morph Documentation](https://docs.morph.network/) for full deployment guides.

#### Morph-Specific CLI Flags

| Flag | Default | Description |
|------|---------|-------------|
| `--morph.max-tx-payload-bytes` | 122880 (120KB) | Maximum transaction payload bytes per block |
| `--morph.max-tx-per-block` | None (unlimited) | Maximum number of transactions per block |
| `--morph.geth-rpc-url` | None | Geth RPC URL for cross-validating MPT state root via `morph_diskRoot` |

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
cargo +nightly fmt --all

# Run clippy
cargo clippy --all --all-targets -- -D warnings

# Check documentation
cargo doc --no-deps --document-private-items
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

Morph Transaction (`0x7f`) is Morph's EVM+ transaction type, extending standard EVM transactions for better user experience and enterprise integration:

| Feature | Description |
|---------|-------------|
| **Alternative Fee Tokens** | Pay gas in stablecoins (USDT, USDC) or other ERC-20 tokens — no ETH required |
| **Transaction Reference** | Tag transactions with a 32-byte key for order tracking and payment reconciliation |
| **Memo Field** | Attach notes or invoice numbers (up to 64 bytes) for auditing and record-keeping |

### Hardforks

Bernoulli and Curie use block-based activation; Morph203, Viridian, Emerald, and Jade use timestamp-based activation.

| Hardfork | Activation | Description |
|----------|------------|-------------|
| Bernoulli | Block | Initial L2 launch with disabled ripemd160 and blake2f precompiles |
| Curie | Block | EIP-1559 fee market activation with blob-based L1 data fee |
| Morph203 | Timestamp | Re-enable ripemd160 and blake2f precompiles |
| Viridian | Timestamp | EIP-7702 EOA delegation support |
| Emerald | Timestamp | BLS12-381 and P256verify precompiles |
| Jade | Timestamp | MPT state root validation, MorphTx V1 with reference and memo fields |

### Engine API

Morph provides a custom L2 Engine API (different from the standard Ethereum Engine API) for sequencer interaction:

| Method | Description |
|--------|-------------|
| `engine_assembleL2Block` | Build a new block with given transactions |
| `engine_validateL2Block` | Validate a block without importing |
| `engine_newL2Block` | Import and finalize a block |
| `engine_newSafeL2Block` | Import a safe block from derivation |

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
