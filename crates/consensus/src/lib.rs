//! Morph L2 consensus validation.
//!
//! This crate provides consensus validation for Morph L2 blocks.
//!
//! # Main Components
//!
//! - [`MorphConsensus`]: The main consensus engine implementing reth's `Consensus`,
//!   `HeaderValidator`, and `FullConsensus` traits.
//! - [`MorphConsensusError`]: Error types for consensus validation failures.
//!
//! # L1 Message Rules
//!
//! Morph L2 blocks must follow these rules for L1 messages:
//!
//! 1. All L1 messages must be at the beginning of the block
//! 2. L1 messages must be in ascending `queue_index` order
//! 3. No gaps in the `queue_index` sequence

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod error;
mod validation;

// Re-export main types
pub use error::MorphConsensusError;
pub use validation::MorphConsensus;
