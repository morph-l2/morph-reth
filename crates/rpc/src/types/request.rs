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
///
/// All MorphTx transactions are constructed as Version 1 (the latest format).
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, b256};

    fn basic_inner_request() -> TransactionRequest {
        TransactionRequest {
            from: Some(address!("0000000000000000000000000000000000000001")),
            to: Some(address!("0000000000000000000000000000000000000002").into()),
            gas: Some(21_000),
            gas_price: Some(1_000_000_000),
            nonce: Some(0),
            ..Default::default()
        }
    }

    #[test]
    fn from_transaction_request_sets_none_fields() {
        let inner = basic_inner_request();
        let morph_req: MorphTransactionRequest = inner.clone().into();
        assert_eq!(morph_req.inner, inner);
        assert!(morph_req.fee_token_id.is_none());
        assert!(morph_req.fee_limit.is_none());
        assert!(morph_req.reference.is_none());
        assert!(morph_req.memo.is_none());
    }

    #[test]
    fn into_transaction_request_strips_morph_fields() {
        let morph_req = MorphTransactionRequest {
            inner: basic_inner_request(),
            fee_token_id: Some(U64::from(1)),
            fee_limit: Some(U256::from(500)),
            reference: Some(b256!(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            )),
            memo: Some(Bytes::from("test")),
        };
        let inner: TransactionRequest = morph_req.into();
        assert_eq!(inner, basic_inner_request());
    }

    #[test]
    fn as_ref_and_as_mut() {
        let mut morph_req = MorphTransactionRequest {
            inner: basic_inner_request(),
            ..Default::default()
        };

        // AsRef
        let inner_ref: &TransactionRequest = morph_req.as_ref();
        assert_eq!(inner_ref.gas, Some(21_000));

        // AsMut
        let inner_mut: &mut TransactionRequest = morph_req.as_mut();
        inner_mut.gas = Some(42_000);
        assert_eq!(morph_req.inner.gas, Some(42_000));
    }

    #[test]
    fn deref_delegates_to_inner() {
        let morph_req = MorphTransactionRequest {
            inner: basic_inner_request(),
            ..Default::default()
        };
        // Deref should allow accessing inner fields directly
        assert_eq!(morph_req.gas, Some(21_000));
        assert_eq!(morph_req.nonce, Some(0));
    }

    #[test]
    fn serde_roundtrip_without_morph_fields() {
        let req = MorphTransactionRequest {
            inner: basic_inner_request(),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: MorphTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, deserialized);
    }

    #[test]
    fn serde_roundtrip_with_morph_fields() {
        let req = MorphTransactionRequest {
            inner: basic_inner_request(),
            fee_token_id: Some(U64::from(5)),
            fee_limit: Some(U256::from(999)),
            reference: Some(b256!(
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            )),
            memo: Some(Bytes::from("memo data")),
        };
        let json = serde_json::to_string(&req).unwrap();
        let deserialized: MorphTransactionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, deserialized);
    }

    #[test]
    fn serde_field_names_camel_case() {
        let req = MorphTransactionRequest {
            inner: basic_inner_request(),
            fee_token_id: Some(U64::from(1)),
            fee_limit: Some(U256::from(100)),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"feeTokenID\""));
        assert!(json.contains("\"feeLimit\""));
    }

    #[test]
    fn serde_skips_none_morph_fields() {
        let req = MorphTransactionRequest {
            inner: basic_inner_request(),
            ..Default::default()
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("feeTokenID"));
        assert!(!json.contains("feeLimit"));
        assert!(!json.contains("reference"));
        assert!(!json.contains("memo"));
    }

    #[test]
    fn default_creates_empty_request() {
        let req = MorphTransactionRequest::default();
        assert_eq!(req.inner, TransactionRequest::default());
        assert!(req.fee_token_id.is_none());
        assert!(req.fee_limit.is_none());
        assert!(req.reference.is_none());
        assert!(req.memo.is_none());
    }
}
