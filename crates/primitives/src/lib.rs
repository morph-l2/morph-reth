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

pub mod transaction;

// Re-export standard Ethereum types
pub use alloy_consensus::Header;
pub use reth_ethereum_primitives::{
    Block, EthPrimitives as MorphPrimitives, Receipt as MorphReceipt,
    TransactionSigned as MorphTxEnvelope,
};

// Re-export transaction types
pub use transaction::{L1Transaction, L1_TX_TYPE_ID};

/// Header alias for backwards compatibility.
pub type MorphHeader = Header;
