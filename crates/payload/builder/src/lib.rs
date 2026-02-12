//! Morph payload builder.
//!
//! This crate provides the payload building logic for Morph L2.
//!
//! The [`MorphPayloadBuilder`] implements reth's `PayloadBuilder` trait
//! to construct L2 blocks with:
//! - L1 message transactions (prioritized, must be at the beginning)
//! - Sequencer forced transactions
//! - Pool transactions (optional, controlled by `no_tx_pool` flag)
//!
//! # Transaction Ordering
//!
//! Transactions are included in the following order:
//! 1. L1 messages from payload attributes (must have sequential queue indices)
//! 2. Other forced transactions from payload attributes
//! 3. Pool transactions (if `no_tx_pool` is false)
//!
//! # L1 Message Rules
//!
//! - L1 messages must appear at the beginning of the block
//! - Queue indices must be strictly sequential
//! - Gas is prepaid on L1, so no refunds for unused gas

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod builder;
mod config;
mod error;

pub use builder::{MorphPayloadBuilder, MorphPayloadTransactions};
pub use config::{MorphBuilderConfig, PayloadBuildingBreaker};
pub use error::MorphPayloadBuilderError;
