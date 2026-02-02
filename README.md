# Morph Reth

## Overview

Morph Reth is the next-generation execution client for [Morph](https://www.morph.network/), a decentralized Layer 2 scaling solution for Ethereum. Built on the modular [Reth SDK](https://github.com/paradigmxyz/reth), it provides high-performance block execution with Morph-specific features.

### Key Features

- **L1 Message Support**: Seamless bridging of assets and messages from Ethereum L1 to Morph L2
- **Morph Transaction**: Morph EVM+ transaction enabling alternative token fees, reference key indexing, and memo attachment
- **Morph Hardforks**: Full support for Morph's upgrade schedule (Bernoulli, Curie, Morph203, Viridian, Emerald)

## Architecture

Morph Reth is designed as a modular extension of Reth, following the SDK pattern:

```
morph-reth/
├── crates/
│   ├── chainspec/       # Morph chain specification and hardfork definitions
│   ├── consensus/       # L2 block validation (header, body, L1 messages)
│   ├── evm/             # EVM configuration and block execution
│   ├── payload/
│   │   ├── builder/     # Block building logic
│   │   └── types/       # Engine API types (ExecutableL2Data, etc.)
│   ├── primitives/      # Core types (transactions, receipts)
│   └── revm/            # L1 fee calculation, token fee logic
```

### Crates

| Crate | Description |
|-------|-------------|
| `morph-chainspec` | Chain specification with Morph hardfork timestamps |
| `morph-consensus` | Consensus validation for L2 blocks |
| `morph-evm` | EVM configuration and receipt builder |
| `morph-payload-types` | Engine API payload types |
| `morph-payload-builder` | Block building implementation |
| `morph-primitives` | Transaction and receipt types |
| `morph-revm` | L1 fee and token fee calculations |

## Getting Started

### Prerequisites

- Rust 1.82.0 or later
- Cargo

### Building from Source

```bash
git clone https://github.com/morph-l2/morph-reth.git
cd morph-reth
cargo build --release
```

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
| EIP-7702 | `0x04` | Account abstraction transactions |
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

| Hardfork | Description |
|----------|-------------|
| Bernoulli | Initial L2 launch |
| Curie | EIP-1559 fee market activation |
| Morph203 | Various improvements |
| Viridian | Fee vault and alt fee support |
| Emerald | Block time 300ms |

### Development Setup

1. Fork and clone the repository
2. Create a new branch for your feature
3. Make your changes with tests
4. Ensure all checks pass: `cargo fmt`, `cargo clippy`, `cargo test`
5. Submit a pull request

## Links

- [Morph Website](https://morph.network/)
- [Morph Documentation](https://docs.morph.network/)
- [Morph GitHub](https://github.com/morph-l2)
- [Reth Documentation](https://reth.rs/)
