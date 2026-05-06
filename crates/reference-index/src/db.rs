//! Reference index database opening and metadata helpers.

use crate::{
    tables::{IndexedBlockKey, IndexedBlocks, MetaKey, MetaValue, ReferenceIndexTables},
    types::{BackfillState, ReferenceIndexError, SCHEMA_VERSION},
};
use alloy_primitives::B256;
use reth_db::{DatabaseEnv, mdbx::DatabaseArguments};
use reth_db_api::{
    Database,
    transaction::{DbTx, DbTxMut},
};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

// ── meta key discriminants ────────────────────────────────────────────────────

/// Discriminant values for the `IndexMeta` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum IndexMetaKey {
    IndexedFrom = 1,
    IndexedTo = 2,
    BackfillState = 3,
    BackfillCurrent = 4,
    ChainId = 5,
    GenesisHash = 6,
    SchemaVersion = 7,
    JadeFirstBlockNumber = 8,
}

impl From<IndexMetaKey> for MetaKey {
    fn from(k: IndexMetaKey) -> Self {
        Self(k as u8)
    }
}

// ── codec helpers ─────────────────────────────────────────────────────────────

pub fn encode_u64(value: u64) -> MetaValue {
    MetaValue(value.to_be_bytes().to_vec())
}

pub fn decode_u64(value: MetaValue) -> Result<u64, ReferenceIndexError> {
    let bytes: [u8; 8] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::Other(eyre::eyre!("invalid u64 metadata length")))?;
    Ok(u64::from_be_bytes(bytes))
}

pub fn encode_u32(value: u32) -> MetaValue {
    MetaValue(value.to_be_bytes().to_vec())
}

pub fn decode_u32(value: MetaValue) -> Result<u32, ReferenceIndexError> {
    let bytes: [u8; 4] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::Other(eyre::eyre!("invalid u32 metadata length")))?;
    Ok(u32::from_be_bytes(bytes))
}

pub fn encode_b256(value: B256) -> MetaValue {
    MetaValue(value.as_slice().to_vec())
}

pub fn decode_b256(value: MetaValue) -> Result<B256, ReferenceIndexError> {
    let bytes: [u8; 32] = value
        .0
        .as_slice()
        .try_into()
        .map_err(|_| ReferenceIndexError::Other(eyre::eyre!("invalid B256 metadata length")))?;
    Ok(B256::new(bytes))
}

// ── database handle ───────────────────────────────────────────────────────────

/// Handle to the reference index MDBX database.
///
/// Wraps the raw [`DatabaseEnv`] and exposes metadata read helpers and an
/// atomic readiness flag.  All writes go through `tx_mut()`.
#[derive(Debug, Clone)]
pub struct ReferenceIndexDb {
    db: Arc<DatabaseEnv>,
    ready: Arc<AtomicBool>,
}

impl ReferenceIndexDb {
    /// Open (or create) the reference index at `path` and validate chain identity.
    ///
    /// On the first open (empty DB), writes `chain_id`, `genesis_hash`, and
    /// `schema_version` into `IndexMeta`.  On every subsequent open, re-reads
    /// those values and returns an error if any mismatch is detected.
    pub fn open(
        path: impl AsRef<Path>,
        chain_id: u64,
        genesis_hash: B256,
    ) -> Result<Self, ReferenceIndexError> {
        let db = reth_db::mdbx::init_db_for::<_, ReferenceIndexTables>(
            path,
            DatabaseArguments::new(reth_db::models::ClientVersion::default()),
        )
        .map_err(|e| {
            ReferenceIndexError::Other(eyre::eyre!("failed to open reference index DB: {e}"))
        })?;

        let this = Self {
            db: Arc::new(db),
            ready: Arc::new(AtomicBool::new(false)),
        };

        this.validate_or_init_chain_identity(chain_id, genesis_hash)?;
        Ok(this)
    }

    /// Check (or initialise) the persisted chain identity in `IndexMeta`.
    fn validate_or_init_chain_identity(
        &self,
        chain_id: u64,
        genesis_hash: B256,
    ) -> Result<(), ReferenceIndexError> {
        let tx = self.tx()?;
        let stored_chain_id = tx
            .get::<crate::tables::IndexMeta>(IndexMetaKey::ChainId.into())?
            .map(decode_u64)
            .transpose()?;

        // First-ever open: write identity and return.
        if stored_chain_id.is_none() {
            drop(tx);
            let tx = self.tx_mut()?;
            tx.put::<crate::tables::IndexMeta>(IndexMetaKey::ChainId.into(), encode_u64(chain_id))?;
            tx.put::<crate::tables::IndexMeta>(
                IndexMetaKey::GenesisHash.into(),
                encode_b256(genesis_hash),
            )?;
            tx.put::<crate::tables::IndexMeta>(
                IndexMetaKey::SchemaVersion.into(),
                encode_u32(SCHEMA_VERSION),
            )?;
            tx.commit()?;
            return Ok(());
        }

        // Subsequent opens: validate every field.
        if stored_chain_id != Some(chain_id) {
            return Err(ReferenceIndexError::ChainIdentityMismatch("chain_id"));
        }

        let stored_genesis = tx
            .get::<crate::tables::IndexMeta>(IndexMetaKey::GenesisHash.into())?
            .map(decode_b256)
            .transpose()?;
        if stored_genesis != Some(genesis_hash) {
            return Err(ReferenceIndexError::ChainIdentityMismatch("genesis_hash"));
        }

        let stored_schema = tx
            .get::<crate::tables::IndexMeta>(IndexMetaKey::SchemaVersion.into())?
            .map(decode_u32)
            .transpose()?;
        match stored_schema {
            Some(v) if v != SCHEMA_VERSION => {
                return Err(ReferenceIndexError::SchemaMismatch {
                    expected: SCHEMA_VERSION,
                    actual: v,
                });
            }
            _ => {}
        }

        Ok(())
    }

    // ── readiness ─────────────────────────────────────────────────────────────

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn set_ready(&self, ready: bool) {
        self.ready.store(ready, Ordering::Release);
    }

    // ── transaction factory ───────────────────────────────────────────────────

    pub fn tx(&self) -> Result<<DatabaseEnv as Database>::TX, ReferenceIndexError> {
        Ok(self.db.tx()?)
    }

    pub fn tx_mut(&self) -> Result<<DatabaseEnv as Database>::TXMut, ReferenceIndexError> {
        Ok(self.db.tx_mut()?)
    }

    // ── metadata reads ────────────────────────────────────────────────────────

    pub fn backfill_state(&self) -> Result<BackfillState, ReferenceIndexError> {
        let tx = self.tx()?;
        match tx.get::<crate::tables::IndexMeta>(IndexMetaKey::BackfillState.into())? {
            Some(v) => BackfillState::try_from(*v.0.first().unwrap_or(&0)),
            None => Ok(BackfillState::NotStarted),
        }
    }

    pub fn indexed_to(&self) -> Result<u64, ReferenceIndexError> {
        let tx = self.tx()?;
        tx.get::<crate::tables::IndexMeta>(IndexMetaKey::IndexedTo.into())?
            .map(decode_u64)
            .transpose()
            .map(|v| v.unwrap_or(0))
    }

    pub fn indexed_from(&self) -> Result<Option<u64>, ReferenceIndexError> {
        let tx = self.tx()?;
        tx.get::<crate::tables::IndexMeta>(IndexMetaKey::IndexedFrom.into())?
            .map(decode_u64)
            .transpose()
    }

    pub fn jade_first_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
        let tx = self.tx()?;
        tx.get::<crate::tables::IndexMeta>(IndexMetaKey::JadeFirstBlockNumber.into())?
            .map(decode_u64)
            .transpose()
    }

    /// Returns the canonical block hash stored in `IndexedBlocks` for `block_number`.
    pub fn indexed_block_hash(
        &self,
        block_number: u64,
    ) -> Result<Option<B256>, ReferenceIndexError> {
        let tx = self.tx()?;
        Ok(tx
            .get::<IndexedBlocks>(IndexedBlockKey { block_number })?
            .map(|v| v.0))
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;
    use tempfile::TempDir;

    fn open_temp_db() -> (TempDir, ReferenceIndexDb) {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        (dir, db)
    }

    #[test]
    fn open_writes_chain_identity_on_first_open() {
        let (_dir, db) = open_temp_db();
        assert_eq!(db.backfill_state().unwrap(), BackfillState::NotStarted);
        assert_eq!(db.indexed_to().unwrap(), 0);
    }

    #[test]
    fn open_rejects_mismatched_chain_id() {
        let dir = TempDir::new().unwrap();
        ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        // Re-open with wrong chain_id.
        let err = ReferenceIndexDb::open(dir.path(), 9999, B256::ZERO).unwrap_err();
        assert!(matches!(
            err,
            ReferenceIndexError::ChainIdentityMismatch("chain_id")
        ));
    }

    #[test]
    fn open_rejects_mismatched_genesis_hash() {
        let dir = TempDir::new().unwrap();
        ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let err = ReferenceIndexDb::open(dir.path(), 2818, B256::repeat_byte(0xff)).unwrap_err();
        assert!(matches!(
            err,
            ReferenceIndexError::ChainIdentityMismatch("genesis_hash")
        ));
    }

    #[test]
    fn encode_decode_u64_roundtrip() {
        let v = 0xDEAD_BEEF_CAFE_1234u64;
        assert_eq!(decode_u64(encode_u64(v)).unwrap(), v);
    }

    #[test]
    fn encode_decode_b256_roundtrip() {
        let v = B256::repeat_byte(0xab);
        assert_eq!(decode_b256(encode_b256(v)).unwrap(), v);
    }
}
