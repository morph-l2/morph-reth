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
    /// Transaction does not contain valid MorphTx fee fields.
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

    /// MorphTx format validation failed (version, memo length, gas fee ordering).
    InvalidFormat {
        /// Reason for the validation failure.
        reason: String,
    },
}

impl fmt::Display for MorphTxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTokenId => {
                write!(f, "invalid MorphTx fee fields")
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
            Self::InvalidFormat { reason } => {
                write!(f, "invalid MorphTx format: {reason}")
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
            // Missing/invalid MorphTx fee fields indicate malformed transaction input.
            Self::InvalidTokenId => true,
            // Format violations (bad version, memo too long, etc.) are malformed input.
            Self::InvalidFormat { .. } => true,
            // Token not found or not active - could be due to temporary state, not penalizable
            Self::TokenNotFound { .. } | Self::TokenNotActive { .. } => false,
            // Invalid price ratio - configuration issue, not penalizable
            Self::InvalidPriceRatio { .. } => false,
            // Insufficient balance or fee limit - normal validation failure
            Self::InsufficientTokenBalance { .. } | Self::InsufficientEthForValue { .. } => false,
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
        match err {
            MorphTxError::InsufficientEthForValue { balance, value } => Self::Overdraft {
                cost: value,
                balance,
            },
            MorphTxError::InsufficientTokenBalance {
                balance, required, ..
            } => Self::Overdraft {
                cost: required,
                balance,
            },
            _ => Self::Other(Box::new(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    #[test]
    fn test_error_display() {
        let err = MorphTxError::InvalidTokenId;
        assert!(err.to_string().contains("invalid MorphTx fee fields"));

        let err = MorphTxError::TokenNotFound { token_id: 1 };
        assert!(err.to_string().contains("token ID 1"));
        assert!(err.to_string().contains("not registered"));

        let err = MorphTxError::TokenNotActive { token_id: 2 };
        assert!(err.to_string().contains("token ID 2"));
        assert!(err.to_string().contains("not active"));

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

    #[test]
    fn test_error_conversion_insufficient_eth() {
        let err = MorphTxError::InsufficientEthForValue {
            balance: U256::from(50),
            value: U256::from(100),
        };
        let pool_err: InvalidPoolTransactionError = err.into();
        assert!(matches!(
            pool_err,
            InvalidPoolTransactionError::Overdraft { .. }
        ));
    }

    #[test]
    fn test_error_conversion_insufficient_token() {
        let err = MorphTxError::InsufficientTokenBalance {
            token_id: 1,
            token_address: address!("1234567890123456789012345678901234567890"),
            balance: U256::from(30),
            required: U256::from(60),
        };
        let pool_err: InvalidPoolTransactionError = err.into();
        assert!(matches!(
            pool_err,
            InvalidPoolTransactionError::Overdraft { .. }
        ));
    }

    #[test]
    fn test_is_bad_transaction() {
        // Malformed = bad
        assert!(MorphTxError::InvalidTokenId.is_bad_transaction());
        assert!(
            MorphTxError::InvalidFormat {
                reason: "test".into()
            }
            .is_bad_transaction()
        );

        // Insufficient funds = not bad (shouldn't penalize peer)
        assert!(
            !MorphTxError::InsufficientTokenBalance {
                token_id: 1,
                token_address: Address::ZERO,
                balance: U256::ZERO,
                required: U256::from(1u64),
            }
            .is_bad_transaction()
        );

        assert!(
            !MorphTxError::InsufficientEthForValue {
                balance: U256::ZERO,
                value: U256::from(1u64),
            }
            .is_bad_transaction()
        );

        // Token state issues = not bad
        assert!(!MorphTxError::TokenNotFound { token_id: 1 }.is_bad_transaction());
        assert!(!MorphTxError::TokenNotActive { token_id: 1 }.is_bad_transaction());
        assert!(!MorphTxError::InvalidPriceRatio { token_id: 1 }.is_bad_transaction());
        assert!(
            !MorphTxError::TokenInfoFetchFailed {
                token_id: 1,
                message: "error".into()
            }
            .is_bad_transaction()
        );
    }

    #[test]
    fn test_error_display_all_variants() {
        // Verify all variants produce non-empty display strings
        let variants: Vec<MorphTxError> = vec![
            MorphTxError::InvalidTokenId,
            MorphTxError::TokenNotFound { token_id: 1 },
            MorphTxError::TokenNotActive { token_id: 2 },
            MorphTxError::InvalidPriceRatio { token_id: 3 },
            MorphTxError::InsufficientTokenBalance {
                token_id: 4,
                token_address: Address::ZERO,
                balance: U256::from(10u64),
                required: U256::from(20u64),
            },
            MorphTxError::InsufficientEthForValue {
                balance: U256::from(5u64),
                value: U256::from(10u64),
            },
            MorphTxError::TokenInfoFetchFailed {
                token_id: 5,
                message: "db error".into(),
            },
            MorphTxError::InvalidFormat {
                reason: "bad version".into(),
            },
        ];

        for err in variants {
            let display = err.to_string();
            assert!(
                !display.is_empty(),
                "Display for {err:?} should not be empty"
            );
        }
    }
}
