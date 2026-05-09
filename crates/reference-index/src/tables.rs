//! Reference index table declarations.

use alloy_primitives::B256;
use reth_db_api::{
    DatabaseError, TableSet, TableType, TableViewer,
    table::{Compress, Decode, Decompress, Encode, TableInfo},
    tables,
};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Key for looking up transactions by Morph transaction reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReferenceIndexKey {
    pub reference: B256,
    pub block_number: u64,
    pub transaction_index: u64,
    pub transaction_hash: B256,
}

impl ReferenceIndexKey {
    /// Encoded length: reference(32) + block number(8) + transaction index(8) + hash(32).
    pub const ENCODED_LEN: usize = 80;
}

impl Encode for ReferenceIndexKey {
    type Encoded = [u8; Self::ENCODED_LEN];

    fn encode(self) -> Self::Encoded {
        let mut encoded = [0u8; Self::ENCODED_LEN];
        encoded[..32].copy_from_slice(self.reference.as_ref());
        encoded[32..40].copy_from_slice(&self.block_number.to_be_bytes());
        encoded[40..48].copy_from_slice(&self.transaction_index.to_be_bytes());
        encoded[48..].copy_from_slice(self.transaction_hash.as_ref());
        encoded
    }
}

impl Decode for ReferenceIndexKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != Self::ENCODED_LEN {
            return Err(DatabaseError::Decode);
        }

        Ok(Self {
            reference: B256::new(value[..32].try_into().map_err(|_| DatabaseError::Decode)?),
            block_number: u64::from_be_bytes(
                value[32..40]
                    .try_into()
                    .map_err(|_| DatabaseError::Decode)?,
            ),
            transaction_index: u64::from_be_bytes(
                value[40..48]
                    .try_into()
                    .map_err(|_| DatabaseError::Decode)?,
            ),
            transaction_hash: B256::new(value[48..].try_into().map_err(|_| DatabaseError::Decode)?),
        })
    }
}

/// Key for listing references seen in a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BlockReferenceKey {
    pub block_number: u64,
    pub transaction_index: u64,
    pub transaction_hash: B256,
}

impl BlockReferenceKey {
    /// Encoded length: block number(8) + transaction index(8) + hash(32).
    pub const ENCODED_LEN: usize = 48;
}

impl Encode for BlockReferenceKey {
    type Encoded = [u8; Self::ENCODED_LEN];

    fn encode(self) -> Self::Encoded {
        let mut encoded = [0u8; Self::ENCODED_LEN];
        encoded[..8].copy_from_slice(&self.block_number.to_be_bytes());
        encoded[8..16].copy_from_slice(&self.transaction_index.to_be_bytes());
        encoded[16..].copy_from_slice(self.transaction_hash.as_ref());
        encoded
    }
}

impl Decode for BlockReferenceKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != Self::ENCODED_LEN {
            return Err(DatabaseError::Decode);
        }

        Ok(Self {
            block_number: u64::from_be_bytes(
                value[..8].try_into().map_err(|_| DatabaseError::Decode)?,
            ),
            transaction_index: u64::from_be_bytes(
                value[8..16].try_into().map_err(|_| DatabaseError::Decode)?,
            ),
            transaction_hash: B256::new(value[16..].try_into().map_err(|_| DatabaseError::Decode)?),
        })
    }
}

/// Key for tracking indexed canonical blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct IndexedBlockKey {
    pub block_number: u64,
}

impl IndexedBlockKey {
    /// Encoded length: block number(8).
    pub const ENCODED_LEN: usize = 8;
}

impl Encode for IndexedBlockKey {
    type Encoded = [u8; Self::ENCODED_LEN];

    fn encode(self) -> Self::Encoded {
        self.block_number.to_be_bytes()
    }
}

impl Decode for IndexedBlockKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        if value.len() != Self::ENCODED_LEN {
            return Err(DatabaseError::Decode);
        }

        Ok(Self {
            block_number: u64::from_be_bytes(value.try_into().map_err(|_| DatabaseError::Decode)?),
        })
    }
}

/// Key for reference index metadata entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct MetaKey(pub u8);

impl MetaKey {
    /// Encoded length: metadata discriminator(1).
    pub const ENCODED_LEN: usize = 1;
}

impl Encode for MetaKey {
    type Encoded = [u8; Self::ENCODED_LEN];

    fn encode(self) -> Self::Encoded {
        [self.0]
    }
}

impl Decode for MetaKey {
    fn decode(value: &[u8]) -> Result<Self, DatabaseError> {
        match value {
            [key] => Ok(Self(*key)),
            _ => Err(DatabaseError::Decode),
        }
    }
}

/// Block timestamp stored for a reference index hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTimestampValue(pub u64);

/// Morph transaction reference stored for a block-scoped key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceValue(pub B256);

/// Canonical block hash for an indexed block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockHashValue(pub B256);

/// Arbitrary small metadata payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetaValue(pub Vec<u8>);

impl Compress for BlockTimestampValue {
    type Compressed = Vec<u8>;

    fn compress_to_buf<B: reth_codecs::__private::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        reth_codecs::__private::bytes::BufMut::put_slice(buf, &self.0.to_be_bytes());
    }
}

impl Decompress for BlockTimestampValue {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self(u64::from_be_bytes(
            value.try_into().map_err(|_| DatabaseError::Decode)?,
        )))
    }
}

macro_rules! impl_b256_value_codec {
    ($name:ident) => {
        impl Compress for $name {
            type Compressed = Vec<u8>;

            fn uncompressable_ref(&self) -> Option<&[u8]> {
                Some(self.0.as_ref())
            }

            fn compress_to_buf<B: reth_codecs::__private::bytes::BufMut + AsMut<[u8]>>(
                &self,
                buf: &mut B,
            ) {
                reth_codecs::__private::bytes::BufMut::put_slice(buf, self.0.as_ref());
            }
        }

        impl Decompress for $name {
            fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
                Ok(Self(B256::new(
                    value.try_into().map_err(|_| DatabaseError::Decode)?,
                )))
            }
        }
    };
}

impl_b256_value_codec!(ReferenceValue);
impl_b256_value_codec!(BlockHashValue);

impl Compress for MetaValue {
    type Compressed = Vec<u8>;

    fn uncompressable_ref(&self) -> Option<&[u8]> {
        Some(&self.0)
    }

    fn compress_to_buf<B: reth_codecs::__private::bytes::BufMut + AsMut<[u8]>>(&self, buf: &mut B) {
        reth_codecs::__private::bytes::BufMut::put_slice(buf, &self.0);
    }
}

impl Decompress for MetaValue {
    fn decompress(value: &[u8]) -> Result<Self, DatabaseError> {
        Ok(Self(value.to_vec()))
    }

    fn decompress_owned(value: Vec<u8>) -> Result<Self, DatabaseError> {
        Ok(Self(value))
    }
}

tables! {
    /// Maps reference query keys to their block timestamp.
    table ReferenceIndex {
        type Key = ReferenceIndexKey;
        type Value = BlockTimestampValue;
    }

    /// Maps block-scoped transaction keys back to Morph transaction references.
    table BlockReferenceIndex {
        type Key = BlockReferenceKey;
        type Value = ReferenceValue;
    }

    /// Tracks canonical blocks that have been indexed.
    table IndexedBlocks {
        type Key = IndexedBlockKey;
        type Value = BlockHashValue;
    }

    /// Stores reference index metadata.
    table IndexMeta {
        type Key = MetaKey;
        type Value = MetaValue;
    }
}

/// Table set for the Morph transaction reference index database.
pub use Tables as ReferenceIndexTables;

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use reth_db_api::{
        DatabaseError,
        table::{Decode, Encode},
    };

    fn b256(byte: u8) -> B256 {
        B256::repeat_byte(byte)
    }

    #[test]
    fn reference_index_key_sorts_by_reference_then_block_then_tx_index() {
        let by_reference = ReferenceIndexKey {
            reference: b256(1),
            block_number: 10,
            transaction_index: 0,
            transaction_hash: b256(1),
        };
        let by_block = ReferenceIndexKey {
            reference: b256(2),
            block_number: 9,
            transaction_index: 9,
            transaction_hash: b256(1),
        };
        let by_tx_index = ReferenceIndexKey {
            reference: b256(2),
            block_number: 10,
            transaction_index: 0,
            transaction_hash: b256(1),
        };
        let later_tx_index = ReferenceIndexKey {
            reference: b256(2),
            block_number: 10,
            transaction_index: 1,
            transaction_hash: b256(0),
        };

        let mut encoded = [
            by_block.encode(),
            later_tx_index.encode(),
            by_reference.encode(),
            by_tx_index.encode(),
        ];
        encoded.sort();

        assert_eq!(
            encoded,
            [
                by_reference.encode(),
                by_block.encode(),
                by_tx_index.encode(),
                later_tx_index.encode(),
            ]
        );
    }

    #[test]
    fn reference_index_key_roundtrip() {
        let key = ReferenceIndexKey {
            reference: b256(0x11),
            block_number: 0x0102_0304_0506_0708,
            transaction_index: 0x1112_1314_1516_1718,
            transaction_hash: b256(0x22),
        };

        let encoded = key.encode();

        assert_eq!(encoded.len(), ReferenceIndexKey::ENCODED_LEN);
        assert_eq!(ReferenceIndexKey::decode(encoded.as_ref()).unwrap(), key);
    }

    #[test]
    fn reference_index_key_decode_rejects_wrong_length() {
        assert!(matches!(
            ReferenceIndexKey::decode(&[0u8; ReferenceIndexKey::ENCODED_LEN - 1]),
            Err(DatabaseError::Decode)
        ));
        assert!(matches!(
            ReferenceIndexKey::decode(&[0u8; ReferenceIndexKey::ENCODED_LEN + 1]),
            Err(DatabaseError::Decode)
        ));
    }

    #[test]
    fn block_reference_key_sorts_and_roundtrips() {
        let by_block = BlockReferenceKey {
            block_number: 7,
            transaction_index: 3,
            transaction_hash: b256(1),
        };
        let by_tx_index = BlockReferenceKey {
            block_number: 8,
            transaction_index: 1,
            transaction_hash: b256(1),
        };
        let later_tx_index = BlockReferenceKey {
            block_number: 8,
            transaction_index: 2,
            transaction_hash: b256(0),
        };

        let mut encoded = [
            later_tx_index.encode(),
            by_tx_index.encode(),
            by_block.encode(),
        ];
        encoded.sort();

        assert_eq!(
            encoded,
            [
                by_block.encode(),
                by_tx_index.encode(),
                later_tx_index.encode(),
            ]
        );
        assert_eq!(
            BlockReferenceKey::decode(by_tx_index.encode().as_ref()).unwrap(),
            by_tx_index
        );
    }
}
