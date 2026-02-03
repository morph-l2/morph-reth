//! Morph primitive types
//!
//! This crate provides core data types for Morph L2, including custom transaction
//! types, receipt types, and type aliases for blocks and headers.
//!
//! # Transaction Types
//!
//! Morph L2 extends Ethereum's transaction types with:
//!
//! - [`TxL1Msg`]: L1 message transactions (type `0x7E`) - deposits from L1
//! - [`TxMorph`]: Morph transactions (type `0x7F`) - pay gas with ERC20 tokens
//! - [`MorphTxEnvelope`]: Transaction envelope containing all supported transaction types
//!
//! # Receipt Types
//!
//! - [`MorphReceipt`]: Receipt enum for all transaction types
//! - [`MorphTransactionReceipt`]: Extended receipt with L1 fee and Morph transaction fields
//!
//! # Block Types
//!
//! - [`Block`]: Morph block type alias
//! - [`BlockBody`]: Morph block body type alias
//! - [`MorphHeader`]: Morph header type alias
//!
//! # Node Primitives
//!
//! [`MorphPrimitives`] implements reth's `NodePrimitives` trait, providing
//! all the type bindings needed for a Morph node.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

#[cfg(feature = "serde-bincode-compat")]
use reth_ethereum_primitives as _;

pub mod header;
pub mod receipt;
pub mod transaction;

// Re-export header type
pub use header::MorphHeader;

/// Morph block.
pub type Block = alloy_consensus::Block<MorphTxEnvelope, MorphHeader>;

/// Morph block body.
pub type BlockBody = alloy_consensus::BlockBody<MorphTxEnvelope, MorphHeader>;

// Re-export receipt types
pub use receipt::{MorphReceipt, MorphReceiptEnvelope, MorphReceiptWithBloom, MorphTransactionReceipt};

// Re-export transaction types
pub use transaction::{
    L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, MorphTxEnvelope, MorphTxType, TxL1Msg, TxMorph, TxMorphExt,
};

/// A [`NodePrimitives`] implementation for Morph.
///
/// This implementation is only available when the `serde-bincode-compat` feature is enabled.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct MorphPrimitives;

#[cfg(feature = "serde-bincode-compat")]
impl reth_primitives_traits::NodePrimitives for MorphPrimitives {
    type Block = Block;
    type BlockHeader = MorphHeader;
    type BlockBody = BlockBody;
    type SignedTx = MorphTxEnvelope;
    type Receipt = MorphReceipt;
}
