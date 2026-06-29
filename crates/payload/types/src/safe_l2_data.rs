//! SafeL2Data type definition.
//!
//! This type is used for NewSafeL2Block in the derivation pipeline.

use alloy_primitives::{B256, Bytes};

/// Safe L2 block data, used for NewSafeL2Block (derivation).
///
/// This is a subset of [`ExecutableL2Data`] that contains only the data
/// needed to reconstruct a block that has been finalized on L1.
/// The execution results (state_root, gas_used, etc.) are computed
/// during execution rather than provided upfront.
///
/// [`ExecutableL2Data`]: super::ExecutableL2Data
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafeL2Data {
    /// Block number.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,

    /// Gas limit.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_limit: u64,

    /// Base fee per gas (EIP-1559).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "alloy_serde::quantity::opt"
    )]
    pub base_fee_per_gas: Option<u128>,

    /// Block timestamp.
    #[serde(with = "alloy_serde::quantity")]
    pub timestamp: u64,

    /// Sub-second millisecond portion of the block timestamp.
    #[serde(default, with = "alloy_serde::quantity")]
    pub timestamp_millis_part: u64,

    /// RLP-encoded transactions.
    #[serde(default)]
    pub transactions: Vec<Bytes>,

    /// Optional parent hash for the derivation reorg path.
    ///
    /// When set, the block is executed on top of this parent (looked up by hash)
    /// and the engine reorganizes the canonical chain onto it via forkchoice
    /// update — used by `derivation.deriveForce` to apply the L1-canonical chain
    /// on top of a non-head parent. When `None`, the legacy "extend the current
    /// head" semantics apply. Mirrors go-ethereum's `SafeL2Data.ParentHash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_hash: Option<B256>,
}

impl SafeL2Data {
    /// Create a new empty [`SafeL2Data`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns true if this block contains any transactions.
    pub fn has_transactions(&self) -> bool {
        !self.transactions.is_empty()
    }

    /// Returns the number of transactions in this block.
    pub fn transaction_count(&self) -> usize {
        self.transactions.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_l2_data_default() {
        let data = SafeL2Data::default();
        assert_eq!(data.number, 0);
        assert_eq!(data.gas_limit, 0);
        assert!(data.base_fee_per_gas.is_none());
        assert!(!data.has_transactions());
    }

    #[test]
    fn test_serde_roundtrip() {
        let data = SafeL2Data {
            number: 12345,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            timestamp: 1234567890,
            timestamp_millis_part: 0,
            transactions: vec![Bytes::from(vec![0x01, 0x02])],
            parent_hash: None,
        };

        let json = serde_json::to_string(&data).expect("serialize");
        let decoded: SafeL2Data = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(data, decoded);
    }

    #[test]
    fn test_serde_without_optional_fields() {
        let data = SafeL2Data {
            number: 100,
            gas_limit: 30_000_000,
            base_fee_per_gas: None,
            timestamp: 1234567890,
            timestamp_millis_part: 0,
            transactions: vec![],
            parent_hash: None,
        };

        let json = serde_json::to_string(&data).expect("serialize");
        // Optional fields should not appear in JSON when None
        assert!(!json.contains("baseFeePerGas"));

        let decoded: SafeL2Data = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_safe_l2_data_new() {
        let data = SafeL2Data::new();
        assert_eq!(data, SafeL2Data::default());
    }

    #[test]
    fn test_transaction_helpers() {
        let mut data = SafeL2Data::default();
        assert!(!data.has_transactions());
        assert_eq!(data.transaction_count(), 0);

        data.transactions.push(Bytes::from(vec![0x01]));
        data.transactions.push(Bytes::from(vec![0x02]));
        assert!(data.has_transactions());
        assert_eq!(data.transaction_count(), 2);
    }

    #[test]
    fn test_serde_camel_case() {
        let json = r#"{
            "number": "0x64",
            "gasLimit": "0x1c9c380",
            "baseFeePerGas": "0x3b9aca00",
            "timestamp": "0x499602d2",
            "transactions": ["0xdead"]
        }"#;

        let data: SafeL2Data = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.number, 100);
        assert_eq!(data.gas_limit, 30_000_000);
        assert_eq!(data.base_fee_per_gas, Some(1_000_000_000));
        assert_eq!(data.timestamp, 1234567890);
        assert_eq!(data.transaction_count(), 1);
    }

    #[test]
    fn test_serde_parent_hash_roundtrip() {
        let hash = alloy_primitives::B256::repeat_byte(0xab);
        let data = SafeL2Data {
            number: 7,
            gas_limit: 30_000_000,
            base_fee_per_gas: None,
            timestamp: 100,
            timestamp_millis_part: 0,
            transactions: vec![],
            parent_hash: Some(hash),
        };

        let json = serde_json::to_string(&data).expect("serialize");
        assert!(
            json.contains("parentHash"),
            "parentHash must be serialized when set: {json}"
        );

        let decoded: SafeL2Data = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.parent_hash, Some(hash));
    }

    #[test]
    fn test_serde_without_parent_hash_omits_field() {
        let data = SafeL2Data {
            parent_hash: None,
            ..Default::default()
        };

        let json = serde_json::to_string(&data).expect("serialize");
        assert!(
            !json.contains("parentHash"),
            "parentHash must be omitted when None: {json}"
        );

        let decoded: SafeL2Data = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded.parent_hash, None);
    }

    #[test]
    fn test_serde_parent_hash_from_camel_case_json() {
        let json = r#"{
            "number": "0x64",
            "gasLimit": "0x1c9c380",
            "timestamp": "0x499602d2",
            "transactions": [],
            "parentHash": "0xabababababababababababababababababababababababababababababababab"
        }"#;

        let data: SafeL2Data = serde_json::from_str(json).expect("deserialize");
        assert_eq!(
            data.parent_hash,
            Some(alloy_primitives::B256::repeat_byte(0xab))
        );
    }

    #[test]
    fn test_clone_and_equality() {
        let data = SafeL2Data {
            number: 42,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(100),
            timestamp: 999,
            timestamp_millis_part: 0,
            transactions: vec![Bytes::from(vec![0x01, 0x02])],
            parent_hash: None,
        };

        let cloned = data.clone();
        assert_eq!(data, cloned);
    }
}
