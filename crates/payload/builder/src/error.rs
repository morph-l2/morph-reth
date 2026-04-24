//! Morph payload builder error types.

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

    /// Invalid sequencer transaction in forced transaction list.
    #[error("invalid sequencer transaction: {error}")]
    InvalidSequencerTransaction {
        /// Human-readable validation error.
        error: String,
    },

    /// Failed to decode transaction from payload attributes.
    #[error("failed to decode transaction: {0}")]
    TransactionDecodeError(#[from] alloy_rlp::Error),

    /// L1 message appears after regular transaction.
    #[error("L1 message appears after regular transaction")]
    L1MessageAfterRegularTx,

    /// Generic storage error (e.g. from revm EvmDatabaseError, ProviderError).
    #[error("storage error: {0}")]
    Storage(String),
}
