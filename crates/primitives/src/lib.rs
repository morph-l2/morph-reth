//! Morph primitive types
//!
//! Re-exports standard Ethereum types for use in the Morph EVM.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg), allow(unexpected_cfgs))]

// Re-export standard Ethereum types
pub use alloy_consensus::Header;
pub use reth_ethereum_primitives::{
    Block, EthPrimitives as MorphPrimitives, Receipt as MorphReceipt,
    TransactionSigned as MorphTxEnvelope,
};

/// Header alias for backwards compatibility.
pub type MorphHeader = Header;
