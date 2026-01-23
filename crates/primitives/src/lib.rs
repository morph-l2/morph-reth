//! Morph primitive types.
//!
//! This crate provides core data types for Morph L2, including custom transaction
//! types, receipt types, and type aliases for blocks and headers.
//!
//! # Transaction Types
//!
//! Morph L2 extends Ethereum's transaction types with:
//!
//! - [`TxL1Msg`]: L1 message transactions (type `0x7E`) - deposits from L1
//! - [`TxAltFee`]: Alternative fee transactions (type `0x7F`) - pay gas with ERC20 tokens
//! - [`MorphTxEnvelope`]: Transaction envelope containing all supported transaction types
//!
//! # Receipt Types
//!
//! - [`MorphReceipt`]: Receipt enum for all transaction types
//! - [`MorphTransactionReceipt`]: Extended receipt with L1 fee and AltFee fields
//!
//! # Block Types
//!
//! - [`Block`]: Morph block type alias
//! - [`BlockBody`]: Morph block body type alias
//! - [`MorphHeader`]: Header type alias (same as Ethereum)
//!
//! # Node Primitives
//!
//! [`MorphPrimitives`] implements reth's `NodePrimitives` trait, providing
//! all the type bindings needed for a Morph node.
//!
//! Note: `NodePrimitives` implementation requires the `reth-codec` feature.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

// Suppress unused_crate_dependencies warnings for dependencies used in submodules
use alloy_consensus as _;
use alloy_eips as _;
use alloy_primitives as _;
use alloy_rlp as _;
use bytes as _;
#[cfg(feature = "reth")]
use reth_ethereum_primitives as _;
#[cfg(feature = "reth-codec")]
use reth_zstd_compressors as _;

pub mod receipt;
pub mod transaction;

// Re-export standard Ethereum types
pub use alloy_consensus::Header;
/// Header alias for backwards compatibility.
pub type MorphHeader = Header;

#[cfg(feature = "reth-codec")]
use reth_primitives_traits::NodePrimitives;

/// Morph block.
pub type Block = alloy_consensus::Block<MorphTxEnvelope, MorphHeader>;

/// Morph block body.
pub type BlockBody = alloy_consensus::BlockBody<MorphTxEnvelope, MorphHeader>;

// Re-export receipt types
pub use receipt::{
    MorphReceipt, MorphReceiptWithBloom, MorphTransactionReceipt, calculate_receipt_root_no_memo,
};

// Re-export transaction types
pub use transaction::{
    ALT_FEE_TX_TYPE_ID, L1_TX_TYPE_ID, MorphTxEnvelope, MorphTxType, TxAltFee, TxAltFeeExt, TxL1Msg,
};

/// A [`NodePrimitives`] implementation for Morph.
#[derive(Debug, Clone, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct MorphPrimitives;

#[cfg(feature = "reth-codec")]
impl NodePrimitives for MorphPrimitives {
    type Block = Block;
    type BlockHeader = MorphHeader;
    type BlockBody = BlockBody;
    type SignedTx = MorphTxEnvelope;
    type Receipt = MorphReceipt;
}
