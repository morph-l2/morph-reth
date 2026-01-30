//! Error types for Morph transaction pool validation.
//!
//! This module defines error types specific to MorphTx (0x7F) validation,
//! which allows users to pay gas fees using ERC20 tokens.

use alloy_primitives::{Address, U256};
use reth_transaction_pool::error::{InvalidPoolTransactionError, PoolTransactionError};
use std::fmt;

/// Errors that can occur during MorphTx validation.
///
/// These errors are specific to transactions that use ERC20 tokens for gas payment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MorphTxError {
    /// Token ID 0 is not allowed (reserved for ETH).
    InvalidTokenId,

    /// Token is not registered in the L2TokenRegistry.
    TokenNotFound {
        /// The requested token ID.
        token_id: u16,
    },

    /// Token is registered but not active for gas payment.
    TokenNotActive {
        /// The token ID that is not active.
        token_id: u16,
    },

    /// Token price ratio is zero or invalid.
    InvalidPriceRatio {
        /// The token ID with invalid price.
        token_id: u16,
    },

    /// The fee_limit is lower than the required token amount.
    FeeLimitTooLow {
        /// The fee_limit specified in the transaction.
        fee_limit: U256,
        /// The required token amount for the transaction.
        required: U256,
    },

    /// Insufficient ERC20 token balance to pay for gas.
    InsufficientTokenBalance {
        /// The token ID.
        token_id: u16,
        /// The token address.
        token_address: Address,
        /// The actual token balance.
        balance: U256,
        /// The required token amount.
        required: U256,
    },

    /// Insufficient ETH balance to pay for transaction value.
    /// MorphTx still requires ETH for the `value` field.
    InsufficientEthForValue {
        /// The ETH balance.
        balance: U256,
        /// The transaction value.
        value: U256,
    },

    /// Failed to fetch token information from state.
    TokenInfoFetchFailed {
        /// The token ID.
        token_id: u16,
        /// Error message.
        message: String,
    },
}

impl fmt::Display for MorphTxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTokenId => {
                write!(f, "invalid token ID: token ID 0 is reserved for ETH")
            }
            Self::TokenNotFound { token_id } => {
                write!(
                    f,
                    "token ID {token_id} is not registered in L2TokenRegistry"
                )
            }
            Self::TokenNotActive { token_id } => {
                write!(f, "token ID {token_id} is not active for gas payment")
            }
            Self::InvalidPriceRatio { token_id } => {
                write!(f, "token ID {token_id} has invalid price ratio (zero)")
            }
            Self::FeeLimitTooLow {
                fee_limit,
                required,
            } => {
                write!(
                    f,
                    "fee_limit ({fee_limit}) is lower than required token amount ({required})"
                )
            }
            Self::InsufficientTokenBalance {
                token_id,
                token_address,
                balance,
                required,
            } => {
                write!(
                    f,
                    "insufficient token balance for token ID {token_id} ({token_address}): \
                     balance {balance}, required {required}"
                )
            }
            Self::InsufficientEthForValue { balance, value } => {
                write!(
                    f,
                    "insufficient ETH balance for transaction value: balance {balance}, value {value}"
                )
            }
            Self::TokenInfoFetchFailed { token_id, message } => {
                write!(f, "failed to fetch token info for ID {token_id}: {message}")
            }
        }
    }
}

impl std::error::Error for MorphTxError {}

impl PoolTransactionError for MorphTxError {
    fn is_bad_transaction(&self) -> bool {
        // MorphTx validation errors are not necessarily "bad" transactions that warrant
        // peer penalization. They are often just insufficient balance or inactive tokens.
        match self {
            // Invalid token ID is a bad transaction (should never be submitted)
            Self::InvalidTokenId => true,
            // Token not found or not active - could be due to temporary state, not penalizable
            Self::TokenNotFound { .. } | Self::TokenNotActive { .. } => false,
            // Invalid price ratio - configuration issue, not penalizable
            Self::InvalidPriceRatio { .. } => false,
            // Insufficient balance or fee limit - normal validation failure
            Self::FeeLimitTooLow { .. }
            | Self::InsufficientTokenBalance { .. }
            | Self::InsufficientEthForValue { .. } => false,
            // Fetch failures - infrastructure issue, not penalizable
            Self::TokenInfoFetchFailed { .. } => false,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl From<MorphTxError> for InvalidPoolTransactionError {
    fn from(err: MorphTxError) -> Self {
        Self::Other(Box::new(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_error_display() {
        let err = MorphTxError::InvalidTokenId;
        assert!(err.to_string().contains("token ID 0"));

        let err = MorphTxError::TokenNotFound { token_id: 1 };
        assert!(err.to_string().contains("token ID 1"));
        assert!(err.to_string().contains("not registered"));

        let err = MorphTxError::TokenNotActive { token_id: 2 };
        assert!(err.to_string().contains("token ID 2"));
        assert!(err.to_string().contains("not active"));

        let err = MorphTxError::FeeLimitTooLow {
            fee_limit: U256::from(100),
            required: U256::from(200),
        };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("200"));

        let err = MorphTxError::InsufficientTokenBalance {
            token_id: 1,
            token_address: address!("1234567890123456789012345678901234567890"),
            balance: U256::from(50),
            required: U256::from(100),
        };
        assert!(err.to_string().contains("token ID 1"));
        assert!(err.to_string().contains("50"));
        assert!(err.to_string().contains("100"));
    }

    #[test]
    fn test_error_conversion() {
        let err = MorphTxError::InvalidTokenId;
        let pool_err: InvalidPoolTransactionError = err.into();
        assert!(matches!(pool_err, InvalidPoolTransactionError::Other(_)));
    }
}
