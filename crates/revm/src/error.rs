//! Morph-specific transaction validation errors.

use alloy_evm::error::InvalidTxError;
use revm::context::result::{EVMError, HaltReason, InvalidTransaction};

/// Morph-specific invalid transaction errors.
#[derive(Debug, Clone, PartialEq, Eq, Hash, thiserror::Error)]
pub enum MorphInvalidTransaction {
    /// Standard Ethereum transaction validation error.
    #[error(transparent)]
    EthInvalidTransaction(#[from] InvalidTransaction),
}

impl InvalidTxError for MorphInvalidTransaction {
    fn is_nonce_too_low(&self) -> bool {
        match self {
            Self::EthInvalidTransaction(err) => err.is_nonce_too_low(),
        }
    }

    fn as_invalid_tx_err(&self) -> Option<&InvalidTransaction> {
        match self {
            Self::EthInvalidTransaction(err) => Some(err),
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
