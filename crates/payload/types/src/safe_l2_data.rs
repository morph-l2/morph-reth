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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct SafeL2Data {
    /// Block number.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub number: u64,

    /// Gas limit.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub gas_limit: u64,

    /// Base fee per gas (EIP-1559).
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )
    )]
    pub base_fee_per_gas: Option<u128>,

    /// Block timestamp.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub timestamp: u64,

    /// RLP-encoded transactions.
    #[cfg_attr(feature = "serde", serde(default))]
    pub transactions: Vec<Bytes>,

    /// Optional batch hash for batch association.
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub batch_hash: Option<B256>,
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

    /// Returns true if this block is associated with a batch.
    pub fn has_batch(&self) -> bool {
        self.batch_hash.is_some()
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
        assert!(!data.has_batch());
    }

    #[test]
    fn test_serde_roundtrip() {
        let data = SafeL2Data {
            number: 12345,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            timestamp: 1234567890,
            transactions: vec![Bytes::from(vec![0x01, 0x02])],
            batch_hash: Some(B256::random()),
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
            transactions: vec![],
            batch_hash: None,
        };

        let json = serde_json::to_string(&data).expect("serialize");
        // Optional fields should not appear in JSON when None
        assert!(!json.contains("baseFeePerGas"));
        assert!(!json.contains("batchHash"));

        let decoded: SafeL2Data = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, decoded);
    }
}
