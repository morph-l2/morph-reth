//! Morph-specific transaction validation errors.

use alloy_primitives::U256;
use evm2::registry::HandlerError;

/// Morph-specific invalid transaction errors.
///
/// These travel through evm2 as [`HandlerError::External`]; read them back with
/// [`HandlerError::external_ref`]. Standard Ethereum rejections stay plain [`HandlerError`]
/// variants, so generic consumers keep classifying them without knowing about Morph.
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MorphInvalidTransaction {
    /// Token is not registered in the Token Registry.
    #[error("Token with ID {0} is not registered")]
    TokenNotRegistered(u16),

    /// Token ID 0 not supported for gas payment.
    #[error("Token ID 0 is not supported for gas payment")]
    TokenIdZeroNotSupported,

    /// Token is not active for gas payment.
    #[error("Token with ID {0} is not active for gas payment")]
    TokenNotActive(u16),

    /// Token has an invalid price ratio or scale.
    #[error("Token with ID {0} has invalid fee configuration")]
    InvalidTokenConfig(u16),

    /// The token balance call reverted, violated static execution, or returned malformed data.
    #[error("Token balance query failed")]
    TokenBalanceQueryFailed,

    /// The transfer failed or its return value/balance delta was invalid.
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

    /// A MorphTx below version 2 carries an EIP-7702 authorization list.
    ///
    /// Only MorphTx V2 (Celadon onwards) may carry authorizations; the RLP decoders
    /// never produce this shape, so it only surfaces for malformed simulation
    /// requests.
    #[error("MorphTx version {version} does not support an authorization list")]
    AuthorizationListNotSupported {
        /// The transaction's MorphTx version.
        version: u8,
    },

    /// A MorphTx carrying an EIP-7702 authorization list is a contract creation.
    ///
    /// Same rule as `ErrSetCodeTxCreate` for `0x04` transactions.
    #[error("MorphTx with an authorization list cannot create a contract")]
    AuthorizationListCreate,

    /// A MorphTx carries an authorization list before EIP-7702 is active.
    #[error("EIP-7702 is not supported by the active hardfork")]
    Eip7702NotSupported,

    /// The maximum fee of the transaction overflows the balance arithmetic.
    #[error("overflow payment in transaction")]
    OverflowPaymentInTransaction,
}

impl From<MorphInvalidTransaction> for HandlerError {
    fn from(err: MorphInvalidTransaction) -> Self {
        Self::external(err)
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

        let err = MorphInvalidTransaction::InvalidTokenConfig(3);
        assert_eq!(
            err.to_string(),
            "Token with ID 3 has invalid fee configuration"
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
    fn morph_error_round_trips_through_handler_error() {
        let handler_err: HandlerError = MorphInvalidTransaction::TokenNotRegistered(1).into();
        assert_eq!(
            handler_err.external_ref::<MorphInvalidTransaction>(),
            Some(&MorphInvalidTransaction::TokenNotRegistered(1))
        );
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
