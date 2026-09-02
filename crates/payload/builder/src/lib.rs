//! Morph payload builder.
//!
//! This crate provides the payload building logic for Morph L2.
//!
//! The [`MorphPayloadBuilder`] implements reth's `PayloadBuilder` trait
//! to construct L2 blocks with:
//! - transactions supplied in the payload attributes (executed first, in order)
//! - pool transactions, unless the caller requested a deterministic build
//!
//! # Build Policies
//!
//! The `no_tx_pool` attribute selects between two behaviours:
//!
//! **Sequencer assembly** (`no_tx_pool == false`, `engine_assembleL2Block`):
//! 1. L1 messages from payload attributes (must have sequential queue indices)
//! 2. Best pool transactions (L2 transactions from mempool)
//!
//! This matches go-ethereum's `AssembleL2Block`, where L1 messages arrive via payload
//! attributes and L2 transactions are pulled from the txpool.
//!
//! **Deterministic build** (`no_tx_pool == true`, `engine_newSafeL2Block`): the attributes
//! carry the complete ordered transaction list of an L1-committed block, and nothing is taken
//! from the pool. This mirrors go-ethereum, where `NewSafeL2Block` executes the decoded block
//! through `BlockChain.ProcessBlock` and never involves the miner at all. Reading the pool
//! here would let a follower absorb gossiped transactions belonging to *later* blocks and
//! fork off the sequencer chain.
//!
//! # L1 Message Rules
//!
//! - L1 messages must appear at the beginning of the block
//! - Queue indices must be strictly sequential
//! - Gas is prepaid on L1, so no refunds for unused gas
//! - L1 messages are never in the transaction pool
//! - If a transaction does not fit remaining block gas: under assembly, packing stops and
//!   leftovers are retried on the next block via `next_l1_msg_index`; under a deterministic
//!   build it is a hard error, as in go-ethereum's `ErrGasLimitReached`

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod builder;
mod config;
mod error;
mod metrics;

pub use builder::{MorphPayloadBuilder, MorphPayloadTransactions};
pub use config::{MorphBuilderConfig, PayloadBuildingBreaker};
pub use error::MorphPayloadBuilderError;
