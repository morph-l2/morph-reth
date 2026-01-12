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
