//! Morph consensus error types.
//!
//! This module defines Morph-specific consensus errors that don't have
//! equivalents in reth's `ConsensusError`.
//!
//! For common errors (difficulty, nonce, ommers, gas, timestamp, base fee),
//! use the standard `reth_consensus::ConsensusError` variants directly.

use alloy_primitives::Address;

/// Morph consensus validation error.
///
/// These are Morph L2-specific errors that have no direct equivalent
/// in the standard reth `ConsensusError`.
#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum MorphConsensusError {
    /// Invalid L1 message order - either L1 messages are not at the start of the block
    /// or queue indices are not strictly sequential.
    #[error("Invalid L1 message order")]
    InvalidL1MessageOrder,

    /// L1 messages queue indices are not sequential.
    #[error("L1 messages are not in queue order: expected {expected}, got {actual}")]
    L1MessagesNotInOrder {
        /// Expected queue index.
        expected: u64,
        /// Actual queue index.
        actual: u64,
    },

    /// Block base fee over limit.
    #[error("Block base fee is over limit: {0}")]
    BaseFeeOverLimit(u64),

    /// Invalid next L1 message index in header.
    #[error("Invalid next L1 message index: expected {expected}, got {actual}")]
    InvalidNextL1MessageIndex {
        /// Expected next L1 message index.
        expected: u64,
        /// Actual next L1 message index.
        actual: u64,
    },

    /// Invalid coinbase (must be empty when FeeVault is enabled).
    #[error("Invalid coinbase: expected zero address, got {0}")]
    InvalidCoinbase(Address),

    /// Invalid header field.
    #[error("Invalid header: {0}")]
    InvalidHeader(String),

    /// Invalid block body.
    #[error("Invalid body: {0}")]
    InvalidBody(String),

    /// Transaction decode error.
    #[error("Failed to decode transaction: {0}")]
    TransactionDecodeError(String),

    /// Withdrawals are not empty.
    #[error("Withdrawals are not empty")]
    WithdrawalsNonEmpty,
}

impl From<alloy_rlp::Error> for MorphConsensusError {
    fn from(err: alloy_rlp::Error) -> Self {
        Self::TransactionDecodeError(err.to_string())
    }
}
