//! Morph consensus error types.
//!
//! This module defines Morph-specific consensus errors that don't have
//! equivalents in reth's `ConsensusError`.
//!
//! For common errors (difficulty, nonce, ommers, gas, timestamp, base fee),
//! use the standard `reth_consensus::ConsensusError` variants directly.

use alloy_primitives::{Address, B256};

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

    /// Malformed L1 message - missing required fields (e.g., queue_index).
    #[error("Malformed L1 message: missing required field")]
    MalformedL1Message,

    /// Block base fee over limit.
    #[error("Block base fee is over limit: {0}")]
    BaseFeeOverLimit(u64),

    /// Invalid next L1 message index in header.
    #[error("invalid block.NextL1MsgIndex: expected {expected}, got {actual}")]
    InvalidNextL1MessageIndex {
        /// Expected next L1 message index.
        expected: u64,
        /// Actual next L1 message index.
        actual: u64,
    },

    /// The withdraw trie root committed by the engine payload does not match execution.
    #[error("withdraw trie root mismatch: expected {expected}, got {actual}")]
    WithdrawTrieRootMismatch {
        /// Withdraw trie root the payload committed to.
        expected: B256,
        /// Withdraw trie root produced by execution.
        actual: B256,
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

    /// L2 transaction payload exceeds the per-block DA cap.
    ///
    /// Matches go-ethereum `ErrInvalidBlockPayloadSize`. L1 messages are
    /// excluded from `size`.
    #[error("invalid block payload size: {size} exceeds limit {limit}")]
    InvalidBlockPayloadSize {
        /// Encoded L2 payload bytes (EIP-2718, L1 messages excluded).
        size: u64,
        /// Maximum allowed payload bytes.
        limit: u64,
    },
}

impl MorphConsensusError {
    /// Whether this rejection can be caused solely by a payload field that the block
    /// hash does not commit to.
    ///
    /// `NextL1MsgIndex` is excluded from the header hash, and the withdraw trie root
    /// travels in the engine payload rather than the header, so neither is covered by
    /// the block hash or the sequencer signature: a peer can relay a validly signed
    /// block with either field corrupted. reth caches rejected blocks by hash, and the
    /// corrupted copy shares its hash with the honest block, so these rejections must
    /// not be cached (see `MorphConsensus::is_transient_error`).
    pub const fn is_unhashed_field_error(&self) -> bool {
        matches!(
            self,
            Self::InvalidNextL1MessageIndex { .. } | Self::WithdrawTrieRootMismatch { .. }
        )
    }
}

impl From<alloy_rlp::Error> for MorphConsensusError {
    fn from(err: alloy_rlp::Error) -> Self {
        Self::TransactionDecodeError(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_next_l1_message_index_error_matches_node_retry_classifier() {
        let error = MorphConsensusError::InvalidNextL1MessageIndex {
            expected: 2,
            actual: 3,
        }
        .to_string();

        assert!(
            error.contains("invalid block.NextL1MsgIndex"),
            "node treats this substring as a non-retryable error: {error}"
        );
    }

    #[test]
    fn only_unhashed_field_errors_are_classified_as_such() {
        assert!(
            MorphConsensusError::InvalidNextL1MessageIndex {
                expected: 2,
                actual: 3,
            }
            .is_unhashed_field_error()
        );
        assert!(
            MorphConsensusError::WithdrawTrieRootMismatch {
                expected: B256::ZERO,
                actual: B256::with_last_byte(1),
            }
            .is_unhashed_field_error()
        );
        assert!(!MorphConsensusError::InvalidL1MessageOrder.is_unhashed_field_error());
        assert!(
            !MorphConsensusError::L1MessagesNotInOrder {
                expected: 1,
                actual: 2,
            }
            .is_unhashed_field_error()
        );
    }
}
