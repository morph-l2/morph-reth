//! Error types for Morph RPC

use alloy_primitives::B256;
use morph_evm::MorphEvmConfig;
use reth_errors::ProviderError;
use reth_evm::revm::context::result::EVMError;
use reth_evm::{HaltReasonFor, InvalidTxError};
use reth_rpc_convert::TransactionConversionError;
use reth_rpc_eth_types::{
    EthApiError,
    error::{AsEthApiError, api::FromEvmHalt, api::FromRevert},
};
use std::convert::Infallible;
use thiserror::Error;

/// Extension trait for converting `Result<T, E>` where `E: Into<EthApiError>` to `Result<T, MorphEthApiError>`.
pub trait ToMorphErr<T> {
    /// Convert the error to `MorphEthApiError`.
    fn to_morph_err(self) -> Result<T, MorphEthApiError>;
}

impl<T, E: Into<EthApiError>> ToMorphErr<T> for Result<T, E> {
    fn to_morph_err(self) -> Result<T, MorphEthApiError> {
        self.map_err(|e| MorphEthApiError::Eth(e.into()))
    }
}

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

/// Converts [`MorphEthApiError`] to a JSON-RPC error object.
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

/// Extracts the inner [`EthApiError`] if present.
impl AsEthApiError for MorphEthApiError {
    fn as_err(&self) -> Option<&EthApiError> {
        match self {
            Self::Eth(err) => Some(err),
            _ => None,
        }
    }
}

// Note: `FromEthApiError` is auto-implemented via blanket impl for any `T: From<EthApiError>`.
// We get it for free since we have `#[from] EthApiError` above.

/// Converts EVM halt reasons to [`MorphEthApiError`].
impl FromEvmHalt<HaltReasonFor<MorphEvmConfig>> for MorphEthApiError {
    fn from_evm_halt(halt: HaltReasonFor<MorphEvmConfig>, gas_limit: u64) -> Self {
        Self::Eth(EthApiError::from_evm_halt(halt, gas_limit))
    }
}

/// Converts EVM revert output to [`MorphEthApiError`].
impl FromRevert for MorphEthApiError {
    fn from_revert(output: alloy_primitives::Bytes) -> Self {
        Self::Eth(EthApiError::from_revert(output))
    }
}

/// Converts [`ProviderError`] to [`MorphEthApiError`].
impl From<ProviderError> for MorphEthApiError {
    fn from(err: ProviderError) -> Self {
        Self::Eth(err.into())
    }
}

/// Converts [`EVMError`] to [`MorphEthApiError`].
impl<T, TxError> From<EVMError<T, TxError>> for MorphEthApiError
where
    T: Into<EthApiError>,
    TxError: InvalidTxError,
{
    fn from(err: EVMError<T, TxError>) -> Self {
        Self::Eth(err.into())
    }
}

/// Converts [`TransactionConversionError`] to [`MorphEthApiError`].
impl From<TransactionConversionError> for MorphEthApiError {
    fn from(err: TransactionConversionError) -> Self {
        Self::Eth(err.into())
    }
}

/// Infallible conversion (never fails).
impl From<Infallible> for MorphEthApiError {
    fn from(err: Infallible) -> Self {
        match err {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        assert_eq!(
            MorphEthApiError::BlockNotFound.to_string(),
            "block not found"
        );
        assert_eq!(
            MorphEthApiError::TransactionNotFound(B256::ZERO).to_string(),
            format!("transaction {} not found", B256::ZERO)
        );
        assert_eq!(
            MorphEthApiError::SkippedTransactionNotFound(B256::ZERO).to_string(),
            format!("skipped transaction {} not found", B256::ZERO)
        );
        assert_eq!(
            MorphEthApiError::InvalidBlockNumberOrHash.to_string(),
            "invalid block number or hash"
        );
        assert_eq!(
            MorphEthApiError::StateNotAvailable.to_string(),
            "state not available for block"
        );
        assert_eq!(
            MorphEthApiError::Internal("oops".to_string()).to_string(),
            "internal error: oops"
        );
        assert_eq!(
            MorphEthApiError::Database("db fail".to_string()).to_string(),
            "database error: db fail"
        );
        assert_eq!(
            MorphEthApiError::Provider("provider fail".to_string()).to_string(),
            "provider error: provider fail"
        );
    }

    #[test]
    fn error_to_json_rpc_error_codes() {
        let check = |err: MorphEthApiError, expected_code: i32| {
            let rpc_err: jsonrpsee::types::ErrorObject<'static> = err.into();
            assert_eq!(rpc_err.code(), expected_code);
        };

        check(MorphEthApiError::BlockNotFound, -32001);
        check(MorphEthApiError::TransactionNotFound(B256::ZERO), -32002);
        check(
            MorphEthApiError::SkippedTransactionNotFound(B256::ZERO),
            -32003,
        );
        check(MorphEthApiError::InvalidBlockNumberOrHash, -32004);
        check(MorphEthApiError::StateNotAvailable, -32005);
        check(MorphEthApiError::Internal("x".into()), -32603);
        check(MorphEthApiError::Database("x".into()), -32006);
        check(MorphEthApiError::Provider("x".into()), -32007);
    }

    #[test]
    fn as_eth_api_error_returns_inner_for_eth_variant() {
        let inner = EthApiError::InvalidParams("test".to_string());
        let err = MorphEthApiError::Eth(inner);
        assert!(err.as_err().is_some());
    }

    #[test]
    fn as_eth_api_error_returns_none_for_non_eth_variants() {
        assert!(MorphEthApiError::BlockNotFound.as_err().is_none());
        assert!(MorphEthApiError::StateNotAvailable.as_err().is_none());
        assert!(MorphEthApiError::Internal("x".into()).as_err().is_none());
    }

    #[test]
    fn from_eth_api_error() {
        let inner = EthApiError::InvalidParams("test".to_string());
        let err: MorphEthApiError = inner.into();
        assert!(matches!(err, MorphEthApiError::Eth(_)));
    }

    #[test]
    fn to_morph_err_extension_trait() {
        let ok_result: Result<u32, EthApiError> = Ok(42);
        assert_eq!(ok_result.to_morph_err().unwrap(), 42);

        let err_result: Result<u32, EthApiError> =
            Err(EthApiError::InvalidParams("bad".to_string()));
        let morph_err = err_result.to_morph_err().unwrap_err();
        assert!(matches!(morph_err, MorphEthApiError::Eth(_)));
    }
}
