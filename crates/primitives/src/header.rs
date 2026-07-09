//! Morph block header type.
//!
//! This module defines the Morph-specific header type that includes:
//! - `next_l1_msg_index`: The next L1 message queue index to process
//!
//! Historical headers without the millisecond timestamp remainder retain
//! compatibility with standard Ethereum header hashing.

use alloy_consensus::{BlockHeader, Header, Sealable};
use alloy_primitives::{Address, B64, B256, BlockNumber, Bloom, Bytes, U256, keccak256};
use alloy_rlp::{Encodable, Header as RlpHeader, RlpDecodable, RlpEncodable};
use core::num::NonZeroU64;

/// Morph block header.
///
/// This header extends the standard Ethereum header with Morph-specific fields:
/// - `next_l1_msg_index`: Next L1 message queue index to process
///
/// **Important**: Historical headers without `timestamp_millis_part` hash only the
/// inner Ethereum header. Headers carrying a non-zero millisecond remainder bind
/// that remainder into block identity while `next_l1_msg_index` remains excluded.
///
/// `timestamp_millis_part` is a trailing RLP field so pre-Onyx blocks decode with
/// the default millisecond remainder of zero.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, RlpEncodable, RlpDecodable)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[rlp(trailing)]
pub struct MorphHeader {
    /// Next L1 message queue index to process.
    /// Not part of the header hash calculation.
    #[cfg_attr(feature = "serde", serde(with = "alloy_serde::quantity"))]
    pub next_l1_msg_index: u64,

    /// Standard Ethereum header (flattened in JSON serialization).
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub inner: Header,

    /// Sub-second millisecond portion of the block timestamp.
    ///
    /// The standard Ethereum timestamp remains seconds-based. This field is
    /// interpreted after the Onyx hardfork and must be zero before it. `None`
    /// represents historical headers encoded before this trailing field existed.
    #[cfg_attr(
        feature = "serde",
        serde(
            default,
            skip_serializing_if = "Option::is_none",
            with = "alloy_serde::quantity::opt"
        )
    )]
    pub timestamp_millis_part: Option<NonZeroU64>,
}

impl From<Header> for MorphHeader {
    fn from(inner: Header) -> Self {
        Self {
            inner,
            next_l1_msg_index: 0,
            timestamp_millis_part: None,
        }
    }
}

impl AsRef<Self> for MorphHeader {
    fn as_ref(&self) -> &Self {
        self
    }
}

impl MorphHeader {
    /// Returns the sub-second millisecond portion of the block timestamp.
    pub fn timestamp_millis_part(&self) -> u64 {
        self.timestamp_millis_part.map_or(0, NonZeroU64::get)
    }

    /// Sets the sub-second millisecond portion of the block timestamp.
    pub fn set_timestamp_millis_part(&mut self, timestamp_millis_part: u64) {
        self.timestamp_millis_part = NonZeroU64::new(timestamp_millis_part);
    }

    /// Returns the block timestamp in milliseconds.
    pub fn timestamp_millis(&self) -> u64 {
        self.inner
            .timestamp()
            .saturating_mul(1000)
            .saturating_add(self.timestamp_millis_part())
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

    fn block_access_list_hash(&self) -> Option<B256> {
        // EIP-7928 / Amsterdam fork field. Pre-Amsterdam Morph blocks do not carry it.
        None
    }

    fn slot_number(&self) -> Option<u64> {
        // PoS slot number, not part of Morph's L2 header model.
        None
    }
}

/// Sealable implementation for MorphHeader.
///
/// **Critical**: Historical headers without the trailing millisecond field keep the
/// old inner-header hash. New headers with a non-zero millisecond remainder bind
/// that remainder into the hash while preserving Morph's existing exclusion of
/// `next_l1_msg_index`.
impl Sealable for MorphHeader {
    fn hash_slow(&self) -> B256 {
        if let Some(timestamp_millis_part) = self.timestamp_millis_part {
            morph_header_hash_with_millis(&self.inner, timestamp_millis_part.get())
        } else {
            // Historical headers were hashed by the inner Ethereum header only.
            self.inner.hash_slow()
        }
    }
}

fn morph_header_hash_with_millis(inner: &Header, timestamp_millis_part: u64) -> B256 {
    // alloy's `Header` RLP is exactly geth's flat field list; the Onyx hash simply
    // appends the millisecond remainder as a trailing element. We re-encode `inner`
    // and splice the remainder in rather than enumerating its fields by hand, so new
    // upstream header fields flow through automatically instead of silently drifting
    // the hash whenever alloy's `Header` grows a field.
    let inner_rlp = alloy_rlp::encode(inner);
    let mut payload = inner_rlp.as_slice();
    let inner_header = RlpHeader::decode(&mut payload).expect("inner header is valid RLP");
    let payload = &payload[..inner_header.payload_length];

    let payload_length = inner_header.payload_length + timestamp_millis_part.length();
    // length_of_length already counts the list header's lead byte, so this is the
    // exact encoded size: header prefix + payload.
    let mut out = Vec::with_capacity(payload_length + alloy_rlp::length_of_length(payload_length));
    RlpHeader {
        list: true,
        payload_length,
    }
    .encode(&mut out);
    out.extend_from_slice(payload);
    timestamp_millis_part.encode(&mut out);
    keccak256(out)
}

impl reth_primitives_traits::InMemorySize for MorphHeader {
    fn size(&self) -> usize {
        reth_primitives_traits::InMemorySize::size(&self.inner)
            + core::mem::size_of::<u64>() // next_l1_msg_index
            + core::mem::size_of::<Option<u64>>() // timestamp_millis_part
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

    fn set_mix_hash(&mut self, mix_hash: B256) {
        self.inner.set_mix_hash(mix_hash);
    }

    fn set_extra_data(&mut self, extra_data: Bytes) {
        self.inner.set_extra_data(extra_data);
    }

    fn set_parent_beacon_block_root(&mut self, parent_beacon_block_root: Option<B256>) {
        self.inner
            .set_parent_beacon_block_root(parent_beacon_block_root);
    }
}

#[cfg(feature = "reth-codec")]
mod codec {
    use crate::MorphHeader;
    use alloy_consensus::Header;
    use alloy_rlp::Decodable;

    const COMPACT_RLP_MARKER: &[u8; 8] = b"MORPHMS1";

    #[derive(Clone, Debug, Default, Eq, Hash, PartialEq, reth_codecs::Compact)]
    struct OldMorphHeaderCompact {
        pub next_l1_msg_index: u64,
        pub inner: Header,
    }

    impl reth_codecs::Compact for MorphHeader {
        fn to_compact<B>(&self, buf: &mut B) -> usize
        where
            B: alloy_rlp::bytes::BufMut + AsMut<[u8]>,
        {
            if self.timestamp_millis_part.is_none() {
                let header = OldMorphHeaderCompact {
                    next_l1_msg_index: self.next_l1_msg_index,
                    inner: self.inner.clone(),
                };
                return header.to_compact(buf);
            }

            let encoded = alloy_rlp::encode(self);
            buf.put_slice(COMPACT_RLP_MARKER);
            buf.put_slice(encoded.as_ref());
            COMPACT_RLP_MARKER.len() + encoded.len()
        }

        fn from_compact(buf: &[u8], len: usize) -> (Self, &[u8]) {
            let value = &buf[..len];
            if let Some(encoded) = value.strip_prefix(COMPACT_RLP_MARKER) {
                let mut rlp = encoded;
                if let Ok(header) = Self::decode(&mut rlp)
                    && rlp.is_empty()
                {
                    return (header, &buf[len..]);
                }
            }

            let (header_compat, buf) = OldMorphHeaderCompact::from_compact(buf, len);
            let header = Self {
                next_l1_msg_index: header_compat.next_l1_msg_index,
                inner: header_compat.inner,
                timestamp_millis_part: None,
            };

            (header, buf)
        }
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
    fn decompress(value: &[u8]) -> Result<Self, reth_codecs::DecompressError> {
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
            // Pre-Amsterdam morph headers carry no block-access-list hash and
            // no PoS slot number. Both fields were introduced upstream in
            // alloy 2.0 / EIP-7928.
            block_access_list_hash: None,
            slot_number: None,
        }
    }

    #[test]
    fn test_morph_header_from_header() {
        let inner = create_test_header();
        let header: MorphHeader = inner.clone().into();

        assert_eq!(header.inner, inner);
        assert_eq!(header.next_l1_msg_index, 0);
        assert_eq!(header.timestamp_millis_part(), 0);
    }

    #[test]
    fn test_morph_header_with_fields() {
        let inner = create_test_header();
        let header = MorphHeader {
            inner,
            next_l1_msg_index: 100,
            timestamp_millis_part: None,
        };

        assert_eq!(header.next_l1_msg_index, 100);
    }

    #[test]
    fn test_morph_header_timestamp_millis_combines_seconds_and_remainder() {
        let inner = Header {
            timestamp: 1_700_000_000,
            ..create_test_header()
        };
        let header = MorphHeader {
            inner,
            next_l1_msg_index: 0,
            timestamp_millis_part: NonZeroU64::new(987),
        };

        assert_eq!(header.timestamp(), 1_700_000_000);
        assert_eq!(header.timestamp_millis_part(), 987);
        assert_eq!(header.timestamp_millis(), 1_700_000_000_987);
    }

    #[test]
    fn test_morph_header_hash_excludes_l2_fields() {
        let inner = create_test_header();

        // A non-zero millisecond remainder is bound into the block hash.
        let header1: MorphHeader = inner.clone().into();
        let header2 = MorphHeader {
            inner: inner.clone(),
            next_l1_msg_index: 999,
            timestamp_millis_part: NonZeroU64::new(999),
        };

        assert_ne!(header1.hash_slow(), header2.hash_slow());

        // Historical headers without the trailing timestamp field retain the old hash.
        assert_eq!(header1.hash_slow(), inner.hash_slow());

        // Morph's existing L1 message index remains excluded from the hash.
        let header3 = MorphHeader {
            inner,
            next_l1_msg_index: 0,
            timestamp_millis_part: NonZeroU64::new(999),
        };
        assert_eq!(header2.hash_slow(), header3.hash_slow());
    }

    #[test]
    fn test_morph_header_hash_uses_flat_geth_field_order_with_millis() {
        use alloy_rlp::{Encodable, Header as RlpHeader};

        let inner = create_test_header();
        let header = MorphHeader {
            inner: inner.clone(),
            next_l1_msg_index: 999,
            timestamp_millis_part: NonZeroU64::new(987),
        };

        let millis = 987u64;
        let payload_len = inner.parent_hash.length()
            + inner.ommers_hash.length()
            + inner.beneficiary.length()
            + inner.state_root.length()
            + inner.transactions_root.length()
            + inner.receipts_root.length()
            + inner.logs_bloom.length()
            + inner.difficulty.length()
            + U256::from(inner.number).length()
            + U256::from(inner.gas_limit).length()
            + U256::from(inner.gas_used).length()
            + inner.timestamp.length()
            + inner.extra_data.length()
            + inner.mix_hash.length()
            + inner.nonce.length()
            + U256::from(inner.base_fee_per_gas.unwrap()).length()
            + millis.length();
        let mut expected_rlp = Vec::new();
        RlpHeader {
            list: true,
            payload_length: payload_len,
        }
        .encode(&mut expected_rlp);
        inner.parent_hash.encode(&mut expected_rlp);
        inner.ommers_hash.encode(&mut expected_rlp);
        inner.beneficiary.encode(&mut expected_rlp);
        inner.state_root.encode(&mut expected_rlp);
        inner.transactions_root.encode(&mut expected_rlp);
        inner.receipts_root.encode(&mut expected_rlp);
        inner.logs_bloom.encode(&mut expected_rlp);
        inner.difficulty.encode(&mut expected_rlp);
        U256::from(inner.number).encode(&mut expected_rlp);
        U256::from(inner.gas_limit).encode(&mut expected_rlp);
        U256::from(inner.gas_used).encode(&mut expected_rlp);
        inner.timestamp.encode(&mut expected_rlp);
        inner.extra_data.encode(&mut expected_rlp);
        inner.mix_hash.encode(&mut expected_rlp);
        inner.nonce.encode(&mut expected_rlp);
        U256::from(inner.base_fee_per_gas.unwrap()).encode(&mut expected_rlp);
        millis.encode(&mut expected_rlp);

        assert_eq!(header.hash_slow(), keccak256(expected_rlp));
    }

    #[test]
    fn test_morph_header_field_mutation() {
        let inner = create_test_header();
        let mut header: MorphHeader = inner.into();

        header.next_l1_msg_index = 50;
        assert_eq!(header.next_l1_msg_index, 50);
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
            timestamp_millis_part: None,
        };

        let json = serde_json::to_string(&header).expect("serialization failed");
        let deserialized: MorphHeader =
            serde_json::from_str(&json).expect("deserialization failed");

        assert_eq!(header, deserialized);
    }

    #[test]
    fn test_morph_header_rlp_roundtrip() {
        let inner = create_test_header();
        let header = MorphHeader {
            inner,
            next_l1_msg_index: 42,
            timestamp_millis_part: NonZeroU64::new(123),
        };

        let mut buf = Vec::new();
        alloy_rlp::Encodable::encode(&header, &mut buf);

        let decoded = <MorphHeader as alloy_rlp::Decodable>::decode(&mut buf.as_slice())
            .expect("RLP decode should succeed");

        assert_eq!(header, decoded);
    }

    #[test]
    fn test_morph_header_rlp_decodes_without_trailing_timestamp_millis_part() {
        #[derive(RlpEncodable)]
        struct OldMorphHeader {
            next_l1_msg_index: u64,
            inner: Header,
        }

        let old_header = OldMorphHeader {
            next_l1_msg_index: 42,
            inner: create_test_header(),
        };

        let mut buf = Vec::new();
        alloy_rlp::Encodable::encode(&old_header, &mut buf);
        let decoded = <MorphHeader as alloy_rlp::Decodable>::decode(&mut buf.as_slice())
            .expect("old header shape should decode");

        assert_eq!(decoded.next_l1_msg_index, 42);
        assert_eq!(decoded.timestamp_millis_part, None);
        assert_eq!(decoded.timestamp_millis_part(), 0);
    }

    #[test]
    fn test_morph_header_size() {
        let inner = create_test_header();
        let header = MorphHeader {
            inner: inner.clone(),
            next_l1_msg_index: 0,
            timestamp_millis_part: None,
        };

        let inner_size = reth_primitives_traits::InMemorySize::size(&inner);
        let header_size = reth_primitives_traits::InMemorySize::size(&header);

        assert_eq!(header_size - inner_size, 24);
    }

    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_morph_header_compact_decodes_old_layout_without_timestamp_millis_part() {
        #[derive(Clone, Debug, Default, Eq, Hash, PartialEq, reth_codecs::Compact)]
        struct OldMorphHeaderCompact {
            pub next_l1_msg_index: u64,
            pub inner: Header,
        }

        let old_header = OldMorphHeaderCompact {
            next_l1_msg_index: 42,
            inner: create_test_header(),
        };

        let mut buf = Vec::new();
        let len = reth_codecs::Compact::to_compact(&old_header, &mut buf);
        let (decoded, remaining) = <MorphHeader as reth_codecs::Compact>::from_compact(&buf, len);

        assert!(remaining.is_empty());
        assert_eq!(decoded.next_l1_msg_index, 42);
        assert_eq!(decoded.timestamp_millis_part, None);
        assert_eq!(decoded.timestamp_millis_part(), 0);
    }

    #[cfg(feature = "reth-codec")]
    #[test]
    fn test_morph_header_compact_roundtrip_with_timestamp_millis_part() {
        let header = MorphHeader {
            inner: create_test_header(),
            next_l1_msg_index: 42,
            timestamp_millis_part: NonZeroU64::new(789),
        };

        let mut buf = Vec::new();
        let len = reth_codecs::Compact::to_compact(&header, &mut buf);
        let (decoded, remaining) = <MorphHeader as reth_codecs::Compact>::from_compact(&buf, len);

        assert!(remaining.is_empty());
        assert_eq!(decoded, header);
    }

    #[test]
    fn test_morph_header_mut_trait() {
        use reth_primitives_traits::header::HeaderMut;

        let inner = create_test_header();
        let mut header: MorphHeader = inner.into();

        let new_hash = b256!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        header.set_parent_hash(new_hash);
        assert_eq!(header.parent_hash(), new_hash);

        header.set_block_number(999);
        assert_eq!(header.number(), 999);

        header.set_timestamp(12345);
        assert_eq!(header.timestamp(), 12345);

        let new_root = b256!("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        header.set_state_root(new_root);
        assert_eq!(header.state_root(), new_root);

        header.set_difficulty(U256::from(42u64));
        assert_eq!(header.difficulty(), U256::from(42u64));
    }
}
