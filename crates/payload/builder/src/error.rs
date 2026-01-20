//! Morph payload builder error types.

use alloy_primitives::B256;
use reth_evm::execute::ProviderError;

/// Errors that can occur during Morph payload building.
#[derive(Debug, thiserror::Error)]
pub enum MorphPayloadBuilderError {
    /// Blob transactions are not supported on Morph L2.
    #[error("blob transactions are not supported")]
    BlobTransactionRejected,

    /// Failed to recover transaction signer.
    #[error("failed to recover transaction signer")]
    TransactionEcRecoverFailed,

    /// Block gas limit exceeded by sequencer transactions.
    #[error(
        "block gas limit {gas} exceeded by sequencer transactions, gas spent by tx: {gas_spent_by_tx:?}"
    )]
    BlockGasLimitExceededBySequencerTransactions {
        /// Gas spent by each transaction.
        gas_spent_by_tx: Vec<u64>,
        /// Block gas limit.
        gas: u64,
    },

    /// Failed to decode transaction from payload attributes.
    #[error("failed to decode transaction: {0}")]
    TransactionDecodeError(#[from] alloy_rlp::Error),

    /// Invalid L1 message queue index.
    #[error("invalid L1 message queue index: expected {expected}, got {actual}")]
    InvalidL1MessageQueueIndex {
        /// Expected queue index.
        expected: u64,
        /// Actual queue index.
        actual: u64,
    },

    /// L1 message appears after regular transaction.
    #[error("L1 message appears after regular transaction")]
    L1MessageAfterRegularTx,

    /// Invalid transaction hash.
    #[error("invalid transaction hash: expected {expected}, got {actual}")]
    InvalidTransactionHash {
        /// Expected hash.
        expected: B256,
        /// Actual hash.
        actual: B256,
    },

    /// Database error when reading contract storage.
    #[error("database error: {0}")]
    Database(#[from] ProviderError),
}
