//! ExecutableL2Data type definition.
//!
//! This type is compatible with go-ethereum's ExecutableL2Data struct.

use alloy_primitives::{Address, B256, Bytes};
use serde::{Deserialize, Serialize};

/// L2 block data used for AssembleL2Block/ValidateL2Block/NewL2Block.
///
/// This struct contains all the data needed to construct and validate an L2 block.
/// It is designed to be compatible with go-ethereum's ExecutableL2Data type.
///
/// # Fields
///
/// The struct is divided into three sections:
/// 1. **BLS message fields**: Fields that affect state calculation and need BLS signing
/// 2. **Execution results**: Fields computed after block execution
/// 3. **Metadata**: Additional L2-specific fields
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableL2Data {
    // === BLS message fields (need to be signed) ===
    /// Parent block hash.
    pub parent_hash: B256,

    /// Block miner/coinbase address.
    pub miner: Address,

    /// Block number.
    #[serde(with = "alloy_serde::quantity")]
    pub number: u64,

    /// Gas limit for this block.
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

    /// RLP-encoded transactions.
    #[serde(default)]
    pub transactions: Vec<Bytes>,

    // === Execution results ===
    /// State root after execution.
    pub state_root: B256,

    /// Gas used by all transactions.
    #[serde(with = "alloy_serde::quantity")]
    pub gas_used: u64,

    /// Receipts root.
    pub receipts_root: B256,

    /// Bloom filter for logs.
    pub logs_bloom: Bytes,

    /// Withdraw trie root (Morph-specific).
    pub withdraw_trie_root: B256,

    // === Metadata ===
    /// Next L1 message queue index after this block.
    #[serde(with = "alloy_serde::quantity")]
    pub next_l1_message_index: u64,

    /// Cached block hash.
    pub hash: B256,
}

impl ExecutableL2Data {
    /// Create a new empty [`ExecutableL2Data`].
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
    fn test_executable_l2_data_default() {
        let data = ExecutableL2Data::default();
        assert_eq!(data.number, 0);
        assert_eq!(data.gas_used, 0);
        assert_eq!(data.gas_limit, 0);
        assert!(!data.has_transactions());
        assert_eq!(data.transaction_count(), 0);
    }

    #[test]
    fn test_executable_l2_data_new() {
        let data = ExecutableL2Data::new();
        assert_eq!(data, ExecutableL2Data::default());
    }

    #[test]
    fn test_has_transactions() {
        let mut data = ExecutableL2Data::default();
        assert!(!data.has_transactions());

        data.transactions.push(Bytes::from(vec![0x01, 0x02]));
        assert!(data.has_transactions());
        assert_eq!(data.transaction_count(), 1);
    }

    #[test]
    fn test_serde_roundtrip() {
        let data = ExecutableL2Data {
            parent_hash: B256::random(),
            miner: Address::random(),
            number: 12345,
            gas_limit: 30_000_000,
            base_fee_per_gas: Some(1_000_000_000),
            timestamp: 1234567890,
            transactions: vec![Bytes::from(vec![0x01, 0x02, 0x03])],
            state_root: B256::random(),
            gas_used: 21000,
            receipts_root: B256::random(),
            logs_bloom: Bytes::from(vec![0; 256]),
            withdraw_trie_root: B256::random(),
            next_l1_message_index: 100,
            hash: B256::random(),
        };

        let json = serde_json::to_string(&data).expect("serialize");
        let decoded: ExecutableL2Data = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(data, decoded);
    }

    #[test]
    fn test_serde_without_base_fee() {
        let data = ExecutableL2Data {
            number: 100,
            base_fee_per_gas: None,
            ..Default::default()
        };

        let json = serde_json::to_string(&data).expect("serialize");
        // base_fee should not appear in JSON when None
        assert!(!json.contains("baseFeePerGas"));

        let decoded: ExecutableL2Data = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_serde_camel_case() {
        let json = r#"{
            "parentHash": "0x0000000000000000000000000000000000000000000000000000000000000001",
            "miner": "0x0000000000000000000000000000000000000002",
            "number": "0x64",
            "gasLimit": "0x1c9c380",
            "timestamp": "0x499602d2",
            "transactions": [],
            "stateRoot": "0x0000000000000000000000000000000000000000000000000000000000000003",
            "gasUsed": "0x5208",
            "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000004",
            "logsBloom": "0x",
            "withdrawTrieRoot": "0x0000000000000000000000000000000000000000000000000000000000000005",
            "nextL1MessageIndex": "0xa",
            "hash": "0x0000000000000000000000000000000000000000000000000000000000000006"
        }"#;

        let data: ExecutableL2Data = serde_json::from_str(json).expect("deserialize");
        assert_eq!(data.number, 100);
        assert_eq!(data.gas_limit, 30_000_000);
        assert_eq!(data.gas_used, 21000);
        assert_eq!(data.next_l1_message_index, 10);
    }
}
