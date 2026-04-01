//! Morph RPC receipt type.

use alloy_network::ReceiptResponse;
use alloy_primitives::{Address, B256, BlockHash, Bytes, U64, U256};
use alloy_rpc_types_eth::{Log, TransactionReceipt};
use morph_primitives::MorphReceiptEnvelope;
use serde::{Deserialize, Serialize};

/// Morph RPC transaction receipt representation.
///
/// Wraps the standard RPC transaction receipt and adds Morph-specific fields:
/// - L1 fee and fee token metadata
/// - Version, reference, and memo for V1 MorphTx
/// - Custom tx type for L1 message / Morph tx receipts
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MorphRpcReceipt {
    /// Standard RPC receipt fields.
    #[serde(flatten)]
    pub inner: TransactionReceipt<MorphReceiptEnvelope<Log>>,

    /// L1 data fee paid (in wei).
    #[serde(rename = "l1Fee")]
    pub l1_fee: U256,

    /// MorphTx version (only for MorphTx type 0x7F).
    /// 0 = legacy format, 1 = with reference/memo support.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<u8>,

    /// Token ID used for fee payment.
    #[serde(rename = "feeTokenID", skip_serializing_if = "Option::is_none")]
    pub fee_token_id: Option<U64>,

    /// Fee rate used for token fee calculation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_rate: Option<U256>,

    /// Token scale factor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_scale: Option<U256>,

    /// Fee limit specified in the transaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_limit: Option<U256>,

    /// Reference key for transaction indexing (only for MorphTx type 0x7F).
    /// 32-byte key used for looking up transactions by external systems.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<B256>,

    /// Memo field for arbitrary data (only for MorphTx type 0x7F).
    /// Up to 64 bytes for notes, invoice numbers, or other metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<Bytes>,
}

/// Implementation of [`ReceiptResponse`] for Morph receipts.
///
/// Delegates all methods to the inner receipt.
impl ReceiptResponse for MorphRpcReceipt {
    fn contract_address(&self) -> Option<Address> {
        self.inner.contract_address
    }

    fn status(&self) -> bool {
        self.inner.inner.status()
    }

    fn block_hash(&self) -> Option<BlockHash> {
        self.inner.block_hash
    }

    fn block_number(&self) -> Option<u64> {
        self.inner.block_number
    }

    fn transaction_hash(&self) -> alloy_primitives::B256 {
        self.inner.transaction_hash
    }

    fn transaction_index(&self) -> Option<u64> {
        self.inner.transaction_index()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn effective_gas_price(&self) -> u128 {
        self.inner.effective_gas_price()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn blob_gas_price(&self) -> Option<u128> {
        self.inner.blob_gas_price()
    }

    fn from(&self) -> Address {
        self.inner.from()
    }

    fn to(&self) -> Option<Address> {
        self.inner.to()
    }

    fn cumulative_gas_used(&self) -> u64 {
        self.inner.cumulative_gas_used()
    }

    fn state_root(&self) -> Option<alloy_primitives::B256> {
        self.inner.state_root()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Eip658Value, Receipt, ReceiptWithBloom};
    use alloy_primitives::{Bloom, address, b256};

    /// Helper to build a minimal TransactionReceipt with a MorphReceiptEnvelope.
    fn make_rpc_receipt(
        l1_fee: U256,
        fee_token_id: Option<U64>,
        version: Option<u8>,
    ) -> MorphRpcReceipt {
        let inner_receipt = Receipt {
            status: Eip658Value::Eip658(true),
            cumulative_gas_used: 50_000,
            logs: vec![],
        };
        let envelope = MorphReceiptEnvelope::Eip1559(ReceiptWithBloom {
            receipt: inner_receipt,
            logs_bloom: Bloom::ZERO,
        });
        let tx_receipt = TransactionReceipt {
            inner: envelope,
            transaction_hash: b256!(
                "0000000000000000000000000000000000000000000000000000000000000001"
            ),
            transaction_index: Some(0),
            block_hash: Some(b256!(
                "0000000000000000000000000000000000000000000000000000000000000002"
            )),
            block_number: Some(42),
            gas_used: 21_000,
            effective_gas_price: 1_000_000_000,
            blob_gas_used: None,
            blob_gas_price: None,
            from: address!("0000000000000000000000000000000000000001"),
            to: Some(address!("0000000000000000000000000000000000000002")),
            contract_address: None,
        };

        MorphRpcReceipt {
            inner: tx_receipt,
            l1_fee,
            version,
            fee_token_id,
            fee_rate: None,
            token_scale: None,
            fee_limit: None,
            reference: None,
            memo: None,
        }
    }

    #[test]
    fn receipt_response_delegates_to_inner() {
        let receipt = make_rpc_receipt(U256::from(100), None, None);

        assert!(receipt.status());
        assert_eq!(receipt.block_number(), Some(42));
        assert_eq!(receipt.gas_used(), 21_000);
        assert_eq!(receipt.effective_gas_price(), 1_000_000_000);
        assert_eq!(receipt.blob_gas_used(), None);
        assert_eq!(receipt.blob_gas_price(), None);
        assert_eq!(
            receipt.from(),
            address!("0000000000000000000000000000000000000001")
        );
        assert_eq!(
            receipt.to(),
            Some(address!("0000000000000000000000000000000000000002"))
        );
        assert_eq!(receipt.contract_address(), None);
        assert_eq!(receipt.transaction_index(), Some(0));
        assert_eq!(receipt.cumulative_gas_used(), 50_000);
    }

    #[test]
    fn receipt_serde_roundtrip_standard() {
        let receipt = make_rpc_receipt(U256::from(500), None, None);
        let json = serde_json::to_string(&receipt).unwrap();
        let deserialized: MorphRpcReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(receipt, deserialized);
    }

    #[test]
    fn receipt_serde_roundtrip_with_morph_fields() {
        let mut receipt = make_rpc_receipt(U256::from(1000), Some(U64::from(1)), Some(1));
        receipt.fee_rate = Some(U256::from(2_000_000));
        receipt.token_scale = Some(U256::from(1_000_000));
        receipt.fee_limit = Some(U256::from(500_000));
        receipt.reference = Some(b256!(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        receipt.memo = Some(Bytes::from("hello"));

        let json = serde_json::to_string(&receipt).unwrap();
        let deserialized: MorphRpcReceipt = serde_json::from_str(&json).unwrap();
        assert_eq!(receipt, deserialized);
    }

    #[test]
    fn receipt_serde_skips_none_fields() {
        let receipt = make_rpc_receipt(U256::from(100), None, None);
        let json = serde_json::to_string(&receipt).unwrap();

        // Optional fields should not appear in JSON when None
        assert!(!json.contains("version"));
        assert!(!json.contains("feeTokenID"));
        assert!(!json.contains("feeRate"));
        assert!(!json.contains("tokenScale"));
        assert!(!json.contains("feeLimit"));
        assert!(!json.contains("reference"));
        assert!(!json.contains("memo"));
    }

    #[test]
    fn receipt_serde_l1_fee_field_name() {
        let receipt = make_rpc_receipt(U256::from(12345), None, None);
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.contains("\"l1Fee\""));
    }

    #[test]
    fn receipt_serde_fee_token_id_field_name() {
        let receipt = make_rpc_receipt(U256::ZERO, Some(U64::from(42)), Some(1));
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.contains("\"feeTokenID\""));
    }
}
