//! Morph-specific transaction validation errors.

use alloy_evm::error::InvalidTxError;
use alloy_primitives::U256;
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};

/// Morph-specific invalid transaction errors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MorphInvalidTransaction {
    /// Standard Ethereum transaction validation error.
    #[error(transparent)]
    EthInvalidTransaction(#[from] InvalidTransaction),

    /// Token is not registered in the Token Registry.
    #[error("Token with ID {0} is not registered")]
    TokenNotRegistered(u16),

    /// Token ID 0 not supported for gas payment.
    #[error("Token ID 0 is not supported for gas payment")]
    TokenIdZeroNotSupported,

    /// Token is not active for gas payment.
    #[error("Token with ID {0} is not active for gas payment")]
    TokenNotActive(u16),

    #[error("Token transfer failed: {reason}")]
    TokenTransferFailed {
        /// Token transfer failure reason.
        reason: String,
    },

    /// Insufficient token balance for gas payment.
    #[error(
        "Insufficient token balance for gas payment: required {required}, available {available}"
    )]
    InsufficientTokenBalance {
        /// Required token amount.
        required: U256,
        /// Available token balance.
        available: U256,
    },
}

impl InvalidTxError for MorphInvalidTransaction {
    fn is_nonce_too_low(&self) -> bool {
        match self {
            Self::EthInvalidTransaction(err) => err.is_nonce_too_low(),
            _ => false,
        }
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        match self {
            Self::EthInvalidTransaction(err) => Some(err),
            _ => None,
        }
    }
}

impl<DBError> From<MorphInvalidTransaction> for EVMError<DBError, MorphInvalidTransaction> {
    fn from(err: MorphInvalidTransaction) -> Self {
        Self::Transaction(err)
    }
}

/// Morph-specific halt reason.
#[derive(Debug, Clone, PartialEq, Eq, Hash, derive_more::From)]
pub enum MorphHaltReason {
    /// Basic Ethereum halt reason.
    #[from]
    Ethereum(HaltReason),
}

#[cfg(feature = "rpc")]
impl reth_rpc_eth_types::error::api::FromEvmHalt<MorphHaltReason>
    for reth_rpc_eth_types::EthApiError
{
    fn from_evm_halt(halt_reason: MorphHaltReason, gas_limit: u64) -> Self {
        match halt_reason {
            MorphHaltReason::Ethereum(halt_reason) => Self::from_evm_halt(halt_reason, gas_limit),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_messages() {
        let err = MorphInvalidTransaction::TokenNotRegistered(5);
        assert_eq!(err.to_string(), "Token with ID 5 is not registered");

        let err = MorphInvalidTransaction::TokenIdZeroNotSupported;
        assert_eq!(
            err.to_string(),
            "Token ID 0 is not supported for gas payment"
        );

        let err = MorphInvalidTransaction::TokenNotActive(3);
        assert_eq!(
            err.to_string(),
            "Token with ID 3 is not active for gas payment"
        );

        let err = MorphInvalidTransaction::TokenTransferFailed {
            reason: "balance too low".into(),
        };
        assert!(err.to_string().contains("balance too low"));

        let err = MorphInvalidTransaction::InsufficientTokenBalance {
            required: U256::from(100),
            available: U256::from(50),
        };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("50"));
    }

    #[test]
    fn test_is_nonce_too_low() {
        // Morph-specific errors are not nonce-too-low
        assert!(!MorphInvalidTransaction::TokenNotRegistered(1).is_nonce_too_low());
        assert!(!MorphInvalidTransaction::TokenIdZeroNotSupported.is_nonce_too_low());
        assert!(!MorphInvalidTransaction::TokenNotActive(1).is_nonce_too_low());

        // Wrapped Ethereum nonce-too-low should be detected
        let eth_err = InvalidTransaction::NonceTooLow { tx: 5, state: 10 };
        let morph_err = MorphInvalidTransaction::EthInvalidTransaction(eth_err);
        assert!(morph_err.is_nonce_too_low());
    }

    #[test]
    fn test_as_invalid_tx_err() {
        // Morph-specific errors return None
        assert!(
            MorphInvalidTransaction::TokenNotRegistered(1)
                .as_invalid_tx_err()
                .is_none()
        );

        // Wrapped Ethereum errors return Some
        let eth_err = InvalidTransaction::NonceTooLow { tx: 5, state: 10 };
        let morph_err = MorphInvalidTransaction::EthInvalidTransaction(eth_err.clone());
        assert_eq!(morph_err.as_invalid_tx_err(), Some(&eth_err));
    }

    #[test]
    fn test_from_invalid_transaction() {
        let eth_err = InvalidTransaction::NonceTooLow { tx: 5, state: 10 };
        let morph_err: MorphInvalidTransaction = eth_err.into();
        assert!(matches!(
            morph_err,
            MorphInvalidTransaction::EthInvalidTransaction(_)
        ));
    }

    #[test]
    fn test_into_evm_error() {
        let morph_err = MorphInvalidTransaction::TokenNotRegistered(1);
        let evm_err: EVMError<std::convert::Infallible, MorphInvalidTransaction> = morph_err.into();
        assert!(matches!(
            evm_err,
            EVMError::Transaction(MorphInvalidTransaction::TokenNotRegistered(1))
        ));
    }

    #[test]
    fn test_morph_halt_reason_from_ethereum() {
        let halt = HaltReason::OutOfGas(revm::context::result::OutOfGasError::Basic);
        let morph_halt: MorphHaltReason = halt.clone().into();
        assert_eq!(morph_halt, MorphHaltReason::Ethereum(halt));
    }

    #[test]
    fn test_error_equality() {
        let err1 = MorphInvalidTransaction::TokenNotRegistered(5);
        let err2 = MorphInvalidTransaction::TokenNotRegistered(5);
        let err3 = MorphInvalidTransaction::TokenNotRegistered(6);
        assert_eq!(err1, err2);
        assert_ne!(err1, err3);
    }
}
