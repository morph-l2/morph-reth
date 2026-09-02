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

    /// A supplied transaction did not fit the block gas limit during a deterministic build.
    ///
    /// Only raised when `no_tx_pool` is set. Sequencer assembly instead stops packing and
    /// seals the block with whatever already fits, leaving the rest for the next block.
    #[error(
        "transaction {tx_index} exceeds block gas limit during deterministic build: \
         tx_gas={tx_gas}, cumulative_gas_used={cumulative_gas_used}, block_gas_limit={block_gas_limit}"
    )]
    BlockGasLimitExceeded {
        /// Index of the offending transaction in the supplied list.
        tx_index: usize,
        /// Gas limit of the offending transaction.
        tx_gas: u64,
        /// Gas already consumed by preceding transactions.
        cumulative_gas_used: u64,
        /// Gas limit of the block being built.
        block_gas_limit: u64,
    },

    /// Generic storage error (e.g. from revm EvmDatabaseError, ProviderError).
    #[error("storage error: {0}")]
    Storage(String),
}
