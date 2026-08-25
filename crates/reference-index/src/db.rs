//! Reference index database opening and metadata helpers.

use crate::{
    tables::{
        BlockReferenceIndex, IndexMeta, IndexedBlockKey, IndexedBlocks, MetaKey, MetaValue,
        ReferenceIndex, ReferenceIndexTables,
    },
    types::{ReferenceIndexError, SCHEMA_VERSION},
};
use alloy_primitives::B256;
use reth_db::{DatabaseEnv, mdbx::DatabaseArguments};
use reth_db_api::{
    Database,
    transaction::{DbTx, DbTxMut},
};
use std::{path::Path, sync::Arc};

/// Discriminant values for the `IndexMeta` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum IndexMetaKey {
    IndexedTo = 1,
    ChainId = 2,
    GenesisHash = 3,
    SchemaVersion = 4,
    JadeFirstBlockNumber = 5,
}

impl From<IndexMetaKey> for MetaKey {
    fn from(key: IndexMetaKey) -> Self {
        Self(key as u8)
    }
}

pub(crate) fn encode_u64(value: u64) -> MetaValue {
    MetaValue(value.to_be_bytes().to_vec())
}

pub(crate) fn decode_u64(value: MetaValue) -> Result<u64, ReferenceIndexError> {
    let bytes: [u8; 8] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::CorruptMetadata("invalid u64 value"))?;
    Ok(u64::from_be_bytes(bytes))
}

pub(crate) fn encode_u32(value: u32) -> MetaValue {
    MetaValue(value.to_be_bytes().to_vec())
}

pub(crate) fn decode_u32(value: MetaValue) -> Result<u32, ReferenceIndexError> {
    let bytes: [u8; 4] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::CorruptMetadata("invalid u32 value"))?;
    Ok(u32::from_be_bytes(bytes))
}

pub(crate) fn encode_b256(value: B256) -> MetaValue {
    MetaValue(value.as_slice().to_vec())
}

pub(crate) fn decode_b256(value: MetaValue) -> Result<B256, ReferenceIndexError> {
    let bytes: [u8; 32] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::CorruptMetadata("invalid B256 value"))?;
    Ok(B256::new(bytes))
}

/// Cloneable handle to the reference-index MDBX environment.
#[derive(Debug, Clone)]
pub(crate) struct ReferenceIndexDb {
    db: Arc<DatabaseEnv>,
}

impl ReferenceIndexDb {
    /// Open (or create) the index and validate its complete chain identity.
    pub(crate) fn open(
        path: impl AsRef<Path>,
        chain_id: u64,
        genesis_hash: B256,
    ) -> Result<Self, ReferenceIndexError> {
        let db = reth_db::mdbx::init_db_for::<_, ReferenceIndexTables>(
            path,
            DatabaseArguments::new(reth_db::models::ClientVersion::default()),
        )
        .map_err(|error| {
            ReferenceIndexError::Other(eyre::eyre!("failed to open reference index DB: {error}"))
        })?;
        let this = Self { db: Arc::new(db) };
        this.validate_or_init_chain_identity(chain_id, genesis_hash)?;
        Ok(this)
    }

    fn validate_or_init_chain_identity(
        &self,
        chain_id: u64,
        genesis_hash: B256,
    ) -> Result<(), ReferenceIndexError> {
        let tx = self.tx()?;
        let stored_chain_id = tx
            .get::<IndexMeta>(IndexMetaKey::ChainId.into())?
            .map(decode_u64)
            .transpose()?;
        let stored_genesis = tx
            .get::<IndexMeta>(IndexMetaKey::GenesisHash.into())?
            .map(decode_b256)
            .transpose()?;
        let stored_schema = tx
            .get::<IndexMeta>(IndexMetaKey::SchemaVersion.into())?
            .map(decode_u32)
            .transpose()?;
        let has_runtime_meta = tx
            .get::<IndexMeta>(IndexMetaKey::IndexedTo.into())?
            .is_some()
            || tx
                .get::<IndexMeta>(IndexMetaKey::JadeFirstBlockNumber.into())?
                .is_some();
        let has_index_data = tx.entries::<ReferenceIndex>()? != 0
            || tx.entries::<BlockReferenceIndex>()? != 0
            || tx.entries::<IndexedBlocks>()? != 0;

        match (stored_chain_id, stored_genesis, stored_schema) {
            (None, None, None) if !has_runtime_meta && !has_index_data => {
                drop(tx);
                let tx = self.tx_mut()?;
                tx.put::<IndexMeta>(IndexMetaKey::ChainId.into(), encode_u64(chain_id))?;
                tx.put::<IndexMeta>(IndexMetaKey::GenesisHash.into(), encode_b256(genesis_hash))?;
                tx.put::<IndexMeta>(
                    IndexMetaKey::SchemaVersion.into(),
                    encode_u32(SCHEMA_VERSION),
                )?;
                tx.commit()?;
                return Ok(());
            }
            (Some(stored_chain_id), Some(stored_genesis), Some(stored_schema)) => {
                if stored_chain_id != chain_id {
                    return Err(ReferenceIndexError::ChainIdentityMismatch("chain_id"));
                }
                if stored_genesis != genesis_hash {
                    return Err(ReferenceIndexError::ChainIdentityMismatch("genesis_hash"));
                }
                if stored_schema != SCHEMA_VERSION {
                    return Err(ReferenceIndexError::SchemaMismatch {
                        expected: SCHEMA_VERSION,
                        actual: stored_schema,
                    });
                }
            }
            _ => {
                return Err(ReferenceIndexError::CorruptMetadata(
                    "chain identity fields must be all present or all absent",
                ));
            }
        }

        Ok(())
    }

    pub(crate) fn tx(&self) -> Result<<DatabaseEnv as Database>::TX, ReferenceIndexError> {
        Ok(self.db.tx()?)
    }

    pub(crate) fn tx_mut(&self) -> Result<<DatabaseEnv as Database>::TXMut, ReferenceIndexError> {
        Ok(self.db.tx_mut()?)
    }

    pub(crate) fn indexed_to(&self) -> Result<Option<u64>, ReferenceIndexError> {
        let tx = self.tx()?;
        tx.get::<IndexMeta>(IndexMetaKey::IndexedTo.into())?
            .map(decode_u64)
            .transpose()
    }

    pub(crate) fn jade_first_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
        let tx = self.tx()?;
        tx.get::<IndexMeta>(IndexMetaKey::JadeFirstBlockNumber.into())?
            .map(decode_u64)
            .transpose()
    }

    pub(crate) fn indexed_block_hash(
        &self,
        block_number: u64,
    ) -> Result<Option<B256>, ReferenceIndexError> {
        let tx = self.tx()?;
        Ok(tx
            .get::<IndexedBlocks>(IndexedBlockKey { block_number })?
            .map(|value| value.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp_db() -> (TempDir, ReferenceIndexDb) {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        (dir, db)
    }

    #[test]
    fn new_database_has_no_durable_cursor() {
        let (_dir, db) = open_temp_db();
        assert_eq!(db.indexed_to().unwrap(), None);
    }

    #[test]
    fn open_rejects_mismatched_chain_id() {
        let dir = TempDir::new().unwrap();
        ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let error = ReferenceIndexDb::open(dir.path(), 9999, B256::ZERO).unwrap_err();
        assert!(matches!(
            error,
            ReferenceIndexError::ChainIdentityMismatch("chain_id")
        ));
    }

    #[test]
    fn open_rejects_mismatched_genesis_hash() {
        let dir = TempDir::new().unwrap();
        ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let error = ReferenceIndexDb::open(dir.path(), 2818, B256::repeat_byte(0xff)).unwrap_err();
        assert!(matches!(
            error,
            ReferenceIndexError::ChainIdentityMismatch("genesis_hash")
        ));
    }

    #[test]
    fn open_rejects_incomplete_identity_metadata() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        tx.delete::<IndexMeta>(IndexMetaKey::SchemaVersion.into(), None)
            .unwrap();
        tx.commit().unwrap();
        drop(db);

        let error = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap_err();
        assert!(matches!(error, ReferenceIndexError::CorruptMetadata(_)));
    }

    #[test]
    fn open_rejects_the_pre_release_schema() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        tx.put::<IndexMeta>(IndexMetaKey::SchemaVersion.into(), encode_u32(1))
            .unwrap();
        tx.commit().unwrap();
        drop(db);

        let error = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap_err();
        assert!(matches!(
            error,
            ReferenceIndexError::SchemaMismatch {
                expected: SCHEMA_VERSION,
                actual: 1
            }
        ));
    }

    #[test]
    fn encode_decode_u64_roundtrip() {
        let value = 0xDEAD_BEEF_CAFE_1234u64;
        assert_eq!(decode_u64(encode_u64(value)).unwrap(), value);
    }

    #[test]
    fn encode_decode_b256_roundtrip() {
        let value = B256::repeat_byte(0xab);
        assert_eq!(decode_b256(encode_b256(value)).unwrap(), value);
    }
}
