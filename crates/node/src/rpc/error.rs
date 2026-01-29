//! Error types for Morph RPC

use alloy_primitives::B256;
use reth_rpc_eth_types::EthApiError;
use thiserror::Error;

/// Morph Eth API errors
#[derive(Debug, Error)]
pub enum MorphEthApiError {
    /// Inner eth API error
    #[error(transparent)]
    Eth(#[from] EthApiError),

    /// Block not found
    #[error("block not found")]
    BlockNotFound,

    /// Transaction not found
    #[error("transaction {0} not found")]
    TransactionNotFound(B256),

    /// Skipped transaction not found
    #[error("skipped transaction {0} not found")]
    SkippedTransactionNotFound(B256),

    /// Invalid block number or hash
    #[error("invalid block number or hash")]
    InvalidBlockNumberOrHash,

    /// State not available for block
    #[error("state not available for block")]
    StateNotAvailable,

    /// Internal error
    #[error("internal error: {0}")]
    Internal(String),

    /// Database error
    #[error("database error: {0}")]
    Database(String),

    /// Provider error
    #[error("provider error: {0}")]
    Provider(String),
}

impl From<MorphEthApiError> for jsonrpsee::types::ErrorObject<'static> {
    fn from(err: MorphEthApiError) -> Self {
        match err {
            MorphEthApiError::Eth(e) => e.into(),
            MorphEthApiError::BlockNotFound => {
                jsonrpsee::types::ErrorObject::owned(-32001, "Block not found", None::<()>)
            }
            MorphEthApiError::TransactionNotFound(hash) => jsonrpsee::types::ErrorObject::owned(
                -32002,
                format!("Transaction {hash} not found"),
                None::<()>,
            ),
            MorphEthApiError::SkippedTransactionNotFound(hash) => {
                jsonrpsee::types::ErrorObject::owned(
                    -32003,
                    format!("Skipped transaction {hash} not found"),
                    None::<()>,
                )
            }
            MorphEthApiError::InvalidBlockNumberOrHash => jsonrpsee::types::ErrorObject::owned(
                -32004,
                "Invalid block number or hash",
                None::<()>,
            ),
            MorphEthApiError::StateNotAvailable => jsonrpsee::types::ErrorObject::owned(
                -32005,
                "State not available for block",
                None::<()>,
            ),
            MorphEthApiError::Internal(msg) => jsonrpsee::types::ErrorObject::owned(
                -32603,
                format!("Internal error: {msg}"),
                None::<()>,
            ),
            MorphEthApiError::Database(msg) => jsonrpsee::types::ErrorObject::owned(
                -32006,
                format!("Database error: {msg}"),
                None::<()>,
            ),
            MorphEthApiError::Provider(msg) => jsonrpsee::types::ErrorObject::owned(
                -32007,
                format!("Provider error: {msg}"),
                None::<()>,
            ),
        }
    }
}
