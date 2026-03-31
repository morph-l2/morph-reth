//! Morph Engine API error types.

use alloy_primitives::B256;
use jsonrpsee::types::{ErrorObject, ErrorObjectOwned};
use thiserror::Error;

/// Morph Engine API errors.
#[derive(Debug, Error)]
pub enum MorphEngineApiError {
    /// Block number is not continuous with the current chain head.
    #[error("discontinuous block number {actual}, expected {expected}")]
    DiscontinuousBlockNumber {
        /// Expected block number.
        expected: u64,
        /// Actual block number.
        actual: u64,
    },

    /// Wrong parent hash.
    #[error("wrong parent hash {actual}, expected {expected}")]
    WrongParentHash {
        /// Expected parent hash.
        expected: B256,
        /// Actual parent hash.
        actual: B256,
    },

    /// Invalid transaction.
    #[error("transaction at index {index} is invalid: {message}")]
    InvalidTransaction {
        /// Transaction index.
        index: usize,
        /// Error message.
        message: String,
    },

    /// Block build error.
    #[error("failed to build block: {0}")]
    BlockBuildError(String),

    /// Block validation failed.
    #[error("block validation failed: {0}")]
    ValidationFailed(String),

    /// Block execution error.
    #[error("block execution failed: {0}")]
    ExecutionFailed(String),

    /// Database error.
    #[error("database error: {0}")]
    Database(String),

    /// Internal error.
    #[error("internal error: {0}")]
    Internal(String),
}

impl MorphEngineApiError {
    /// Converts this error into a JSON-RPC error object.
    pub fn into_rpc_error(self) -> ErrorObjectOwned {
        // Use custom error codes
        // -32000 to -32099: Server error (reserved for implementation-defined server-errors)
        let code = match &self {
            Self::DiscontinuousBlockNumber { .. } | Self::WrongParentHash { .. } => -32001,
            Self::InvalidTransaction { .. } => -32002,
            Self::BlockBuildError(_) => -32003,
            Self::ValidationFailed(_) => -32004,
            Self::ExecutionFailed(_) => -32005,
            Self::Database(_) => -32010,
            Self::Internal(_) => -32099,
        };

        ErrorObject::owned(code, self.to_string(), None::<()>)
    }
}

impl From<MorphEngineApiError> for ErrorObjectOwned {
    fn from(err: MorphEngineApiError) -> Self {
        err.into_rpc_error()
    }
}

/// A type alias for the result of Engine API methods.
pub type EngineApiResult<T> = Result<T, MorphEngineApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_code_discontinuous_block_number() {
        let err = MorphEngineApiError::DiscontinuousBlockNumber {
            expected: 100,
            actual: 102,
        };
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32001);
    }

    #[test]
    fn test_error_code_wrong_parent_hash() {
        let err = MorphEngineApiError::WrongParentHash {
            expected: B256::from([0x01; 32]),
            actual: B256::from([0x02; 32]),
        };
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32001);
    }

    #[test]
    fn test_error_code_invalid_transaction() {
        let err = MorphEngineApiError::InvalidTransaction {
            index: 0,
            message: "invalid signature".to_string(),
        };
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32002);
    }

    #[test]
    fn test_error_code_block_build_error() {
        let err = MorphEngineApiError::BlockBuildError("out of gas".to_string());
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32003);
    }

    #[test]
    fn test_error_code_validation_failed() {
        let err = MorphEngineApiError::ValidationFailed("invalid state root".to_string());
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32004);
    }

    #[test]
    fn test_error_code_execution_failed() {
        let err = MorphEngineApiError::ExecutionFailed("evm error".to_string());
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32005);
    }

    #[test]
    fn test_error_code_database() {
        let err = MorphEngineApiError::Database("connection lost".to_string());
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32010);
    }

    #[test]
    fn test_error_code_internal() {
        let err = MorphEngineApiError::Internal("unexpected".to_string());
        let rpc_err = err.into_rpc_error();
        assert_eq!(rpc_err.code(), -32099);
    }

    #[test]
    fn test_error_display_messages() {
        let err = MorphEngineApiError::DiscontinuousBlockNumber {
            expected: 100,
            actual: 102,
        };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("102"));

        let err = MorphEngineApiError::InvalidTransaction {
            index: 3,
            message: "nonce too low".to_string(),
        };
        assert!(err.to_string().contains("index 3"));
        assert!(err.to_string().contains("nonce too low"));
    }

    #[test]
    fn test_from_error_to_rpc_error() {
        let err = MorphEngineApiError::Internal("test".to_string());
        let rpc_err: ErrorObjectOwned = err.into();
        assert_eq!(rpc_err.code(), -32099);
        assert!(rpc_err.message().contains("test"));
    }
}
