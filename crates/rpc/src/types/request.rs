//! Morph RPC transaction request type.

use alloy_primitives::{B256, Bytes, U64, U256};
use alloy_rpc_types_eth::TransactionRequest;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction request representation.
///
/// Extends standard Ethereum transaction request with:
/// - `feeTokenID`: Token ID for ERC20 gas payment
/// - `feeLimit`: Maximum token amount willing to pay for fees
/// - `reference`: 32-byte reference key for transaction indexing
/// - `memo`: Arbitrary memo data (up to 64 bytes)
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

    /// Reference key for transaction indexing (32 bytes).
    /// Used for looking up transactions by external systems.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<B256>,

    /// Memo field for arbitrary data (up to 64 bytes).
    /// Can be used for notes, invoice numbers, or other metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memo: Option<Bytes>,
}

/// Returns a reference to the inner [`TransactionRequest`].
impl AsRef<TransactionRequest> for MorphTransactionRequest {
    fn as_ref(&self) -> &TransactionRequest {
        &self.inner
    }
}

/// Returns a mutable reference to the inner [`TransactionRequest`].
impl AsMut<TransactionRequest> for MorphTransactionRequest {
    fn as_mut(&mut self) -> &mut TransactionRequest {
        &mut self.inner
    }
}

/// Creates a [`MorphTransactionRequest`] from a standard [`TransactionRequest`].
///
/// Sets `fee_token_id`, `fee_limit`, `reference`, and `memo` to `None`.
impl From<TransactionRequest> for MorphTransactionRequest {
    fn from(value: TransactionRequest) -> Self {
        Self {
            inner: value,
            fee_token_id: None,
            fee_limit: None,
            reference: None,
            memo: None,
        }
    }
}

/// Extracts the inner [`TransactionRequest`] from a [`MorphTransactionRequest`].
impl From<MorphTransactionRequest> for TransactionRequest {
    fn from(value: MorphTransactionRequest) -> Self {
        value.inner
    }
}
