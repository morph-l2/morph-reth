//! Morph RPC transaction request type.

use alloy_primitives::{U64, U256};
use alloy_rpc_types_eth::TransactionRequest;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction request representation.
///
/// Extends standard Ethereum transaction request with:
/// - `feeTokenID`: Token ID for ERC20 gas payment
/// - `feeLimit`: Maximum token amount willing to pay for fees
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    derive_more::Deref,
    derive_more::DerefMut,
)]
#[serde(rename_all = "camelCase")]
pub struct MorphTransactionRequest {
    /// Inner [`TransactionRequest`].
    #[serde(flatten)]
    #[deref]
    #[deref_mut]
    pub inner: TransactionRequest,

    /// Token ID for fee payment (only for MorphTx type 0x7F).
    #[serde(
        rename = "feeTokenID",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub fee_token_id: Option<U64>,

    /// Maximum token amount willing to pay for fees (only for MorphTx type 0x7F).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,
}

impl AsRef<TransactionRequest> for MorphTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        &self.inner
    }
}

impl AsMut<TransactionRequest> for MorphTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        &mut self.inner
    }
}

impl From<TransactionRequest> for MorphTransactionRequest {
    fn from(value: TransactionRequest) -> Self {
        Self {
            inner: value,
            fee_token_id: None,
            fee_limit: None,
        }
    }
}

impl From<MorphTransactionRequest> for TransactionRequest {
    fn from(value: MorphTransactionRequest) -> Self {
        value.inner
    }
}
