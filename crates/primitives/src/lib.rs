//! Morph primitive types
//!
//! Re-exports standard Ethereum types for use in the Morph EVM.

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

use reth_primitives_traits::NodePrimitives;

/// Morph block.
pub type Block = alloy_consensus::Block<MorphTxEnvelope, MorphHeader>;

/// Morph block body.
pub type BlockBody = alloy_consensus::BlockBody<MorphTxEnvelope, MorphHeader>;

// Re-export receipt types
pub use receipt::{MorphReceipt, MorphReceiptWithBloom, MorphTransactionReceipt};

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
