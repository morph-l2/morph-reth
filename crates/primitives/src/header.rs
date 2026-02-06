//! Morph block header type.
//!
//! This module defines the Morph-specific header type that includes:
//! - `next_l1_msg_index`: The next L1 message queue index to process
//! - `batch_hash`: The batch hash (non-zero if this block is a batch point)
//!
//! These fields are NOT included in the block hash calculation to maintain
//! compatibility with standard Ethereum header hashing (matching go-ethereum's behavior).

use alloy_consensus::{BlockHeader, Header, Sealable};
use alloy_primitives::{Address, B64, B256, BlockNumber, Bloom, Bytes, U256};
use alloy_rlp::{RlpDecodable, RlpEncodable};

/// Morph block header.
///
/// This header extends the standard Ethereum header with Morph-specific fields:
/// - `next_l1_msg_index`: Next L1 message queue index to process
/// - `batch_hash`: Non-zero if this block is a batch point
///
/// **Important**: The `hash_slow()` method only hashes the inner Ethereum header,
/// excluding `next_l1_msg_index` and `batch_hash`. This matches go-ethereum's
/// `Header.Hash()` behavior where these L2-specific fields are not part of the
/// block hash calculation.
///
/// Note: The `inner` field must be placed last for the `Compact` derive macro,
/// as fields with unknown size must come last in the struct definition.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "reth-codec", derive(reth_codecs::Compact))]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct MorphHeader {
    /// Next L1 message queue index to process.
    /// Not part of the header hash calculation.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub next_l1_msg_index: u64,

    /// Batch hash - non-zero if this block is a batch point.
    /// Not part of the header hash calculation.
    pub batch_hash: B256,

    /// Standard Ethereum header (flattened in JSON serialization).
    /// Must be placed last due to Compact derive requirements.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Header,
}

impl MorphHeader {
    /// Returns true if this block is a batch point (batch_hash is non-zero).
    pub fn is_batch_point(&self) -> bool {
        self.batch_hash != B256::ZERO
    }
}

impl From<Header> for MorphHeader {
    fn from(inner: Header) -> Self {
        Self {
            inner,
            next_l1_msg_index: 0,
            batch_hash: B256::ZERO,
        }
    }
}

impl AsRef<Self> for MorphHeader {
    fn as_ref(&self) -> &Self {
        self
    }
}

// Implement BlockHeader trait by delegating to inner header
impl BlockHeader for MorphHeader {
    fn parent_hash(&self) -> B256 {
        self.inner.parent_hash()
    }

    fn ommers_hash(&self) -> B256 {
        self.inner.ommers_hash()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn state_root(&self) -> B256 {
        self.inner.state_root()
    }

    fn transactions_root(&self) -> B256 {
        self.inner.transactions_root()
    }

    fn receipts_root(&self) -> B256 {
        self.inner.receipts_root()
    }

    fn withdrawals_root(&self) -> Option<B256> {
        self.inner.withdrawals_root()
    }

    fn logs_bloom(&self) -> Bloom {
        self.inner.logs_bloom()
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn number(&self) -> BlockNumber {
        self.inner.number()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn gas_used(&self) -> u64 {
        self.inner.gas_used()
    }

    fn timestamp(&self) -> u64 {
        self.inner.timestamp()
    }

    fn mix_hash(&self) -> Option<B256> {
        self.inner.mix_hash()
    }

    fn nonce(&self) -> Option<B64> {
        self.inner.nonce()
    }

    fn base_fee_per_gas(&self) -> Option<u64> {
        self.inner.base_fee_per_gas()
    }

    fn blob_gas_used(&self) -> Option<u64> {
        self.inner.blob_gas_used()
    }

    fn excess_blob_gas(&self) -> Option<u64> {
        self.inner.excess_blob_gas()
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.inner.parent_beacon_block_root()
    }

    fn requests_hash(&self) -> Option<B256> {
        self.inner.requests_hash()
    }

    fn extra_data(&self) -> &Bytes {
        self.inner.extra_data()
    }
}

/// Sealable implementation for MorphHeader.
///
/// **Critical**: The `hash_slow()` method only hashes the inner Ethereum header,
/// NOT the `next_l1_msg_index` and `batch_hash` fields. This matches go-ethereum's
/// `Header.Hash()` behavior which explicitly excludes these L2-specific fields
/// from the hash calculation.
impl Sealable for MorphHeader {
    fn hash_slow(&self) -> B256 {
        // Only hash the inner header to match go-ethereum behavior.
        // next_l1_msg_index and batch_hash are NOT part of the hash.
        self.inner.hash_slow()
    }
}

#[cfg(feature = "serde-bincode-compat")]
impl reth_primitives_traits::serde_bincode_compat::RlpBincode for MorphHeader {}

impl reth_primitives_traits::InMemorySize for MorphHeader {
    fn size(&self) -> usize {
        reth_primitives_traits::InMemorySize::size(&self.inner)
            + core::mem::size_of::<u64>() // next_l1_msg_index
            + core::mem::size_of::<B256>() // batch_hash
    }
}

impl reth_primitives_traits::BlockHeader for MorphHeader {}

impl reth_primitives_traits::header::HeaderMut for MorphHeader {
    fn set_parent_hash(&mut self, hash: B256) {
        self.inner.set_parent_hash(hash);
    }

    fn set_block_number(&mut self, number: BlockNumber) {
        self.inner.set_block_number(number);
    }

    fn set_timestamp(&mut self, timestamp: u64) {
        self.inner.set_timestamp(timestamp);
    }

    fn set_state_root(&mut self, state_root: B256) {
        self.inner.set_state_root(state_root);
    }

    fn set_difficulty(&mut self, difficulty: U256) {
        self.inner.set_difficulty(difficulty);
    }
}

#[cfg(feature = "reth-codec")]
impl reth_db_api::table::Compress for MorphHeader {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: alloy_primitives::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        let _ = reth_codecs::Compact::to_compact(self, buf);
    }
}

#[cfg(feature = "reth-codec")]
impl reth_db_api::table::Decompress for MorphHeader {
    fn decompress(value: &[u8]) -> Result<Self, reth_db_api::DatabaseError> {
        let (obj, _) = reth_codecs::Compact::from_compact(value, value.len());
        Ok(obj)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, b256};

    fn create_test_header() -> Header {
        Header {
            parent_hash: b256!("0000000000000000000000000000000000000000000000000000000000000001"),
            ommers_hash: b256!("0000000000000000000000000000000000000000000000000000000000000002"),
            beneficiary: address!("0000000000000000000000000000000000000011"),
            state_root: b256!("0000000000000000000000000000000000000000000000000000000000000003"),
            transactions_root: b256!(
                "0000000000000000000000000000000000000000000000000000000000000004"
            ),
            receipts_root: b256!(
                "0000000000000000000000000000000000000000000000000000000000000005"
            ),
            logs_bloom: Bloom::default(),
            difficulty: U256::from(1u64),
            number: 100,
            gas_limit: 30_000_000,
            gas_used: 21_000,
            timestamp: 1234567890,
            extra_data: Bytes::default(),
            mix_hash: B256::ZERO,
            nonce: B64::ZERO,
            base_fee_per_gas: Some(1000000000),
            withdrawals_root: None,
            blob_gas_used: None,
            excess_blob_gas: None,
            parent_beacon_block_root: None,
            requests_hash: None,
        }
    }

    #[test]
    fn test_morph_header_from_header() {
        let inner = create_test_header();
        let header: MorphHeader = inner.clone().into();

        assert_eq!(header.inner, inner);
        assert_eq!(header.next_l1_msg_index, 0);
        assert_eq!(header.batch_hash, B256::ZERO);
        assert!(!header.is_batch_point());
    }

    #[test]
    fn test_morph_header_with_fields() {
        let inner = create_test_header();
        let batch_hash = b256!("0000000000000000000000000000000000000000000000000000000000000abc");
        let header = MorphHeader {
            inner,
            next_l1_msg_index: 100,
            batch_hash,
        };

        assert_eq!(header.next_l1_msg_index, 100);
        assert_eq!(header.batch_hash, batch_hash);
        assert!(header.is_batch_point());
    }

    #[test]
    fn test_morph_header_hash_excludes_l2_fields() {
        let inner = create_test_header();

        // Create two headers with different L2 fields
        let header1: MorphHeader = inner.clone().into();
        let header2 = MorphHeader {
            inner: inner.clone(),
            next_l1_msg_index: 999,
            batch_hash: b256!("1111111111111111111111111111111111111111111111111111111111111111"),
        };

        // Both should have the same hash since L2 fields are excluded
        assert_eq!(header1.hash_slow(), header2.hash_slow());

        // And they should match the inner header's hash
        assert_eq!(header1.hash_slow(), inner.hash_slow());
    }

    #[test]
    fn test_morph_header_field_mutation() {
        let inner = create_test_header();
        let mut header: MorphHeader = inner.into();

        header.next_l1_msg_index = 50;
        assert_eq!(header.next_l1_msg_index, 50);

        let batch_hash = b256!("2222222222222222222222222222222222222222222222222222222222222222");
        header.batch_hash = batch_hash;
        assert_eq!(header.batch_hash, batch_hash);
        assert!(header.is_batch_point());
    }

    #[test]
    fn test_morph_header_block_header_delegation() {
        let inner = create_test_header();
        let header: MorphHeader = inner.clone().into();

        // Test that all BlockHeader methods delegate correctly
        assert_eq!(header.parent_hash(), inner.parent_hash());
        assert_eq!(header.beneficiary(), inner.beneficiary());
        assert_eq!(header.state_root(), inner.state_root());
        assert_eq!(header.number(), inner.number());
        assert_eq!(header.gas_limit(), inner.gas_limit());
        assert_eq!(header.gas_used(), inner.gas_used());
        assert_eq!(header.timestamp(), inner.timestamp());
        assert_eq!(header.base_fee_per_gas(), inner.base_fee_per_gas());
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_morph_header_serde() {
        let inner = create_test_header();
        let header = MorphHeader {
            inner,
            next_l1_msg_index: 42,
            batch_hash: b256!("3333333333333333333333333333333333333333333333333333333333333333"),
        };

        let json = serde_json::to_string(&header).expect("serialization failed");
        let deserialized: MorphHeader =
            serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(header, deserialized);
    }
}
