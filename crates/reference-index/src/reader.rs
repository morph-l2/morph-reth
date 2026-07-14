//! Reference index read path helpers.

use crate::{
    db::{IndexMetaKey, ReferenceIndexDb, decode_u64},
    metrics::ReferenceIndexMetrics,
    tables::{IndexMeta, IndexedBlockKey, IndexedBlocks, ReferenceIndex, ReferenceIndexKey},
    types::{CanonicalTip, ReferenceIndexError, ReferenceQuery, ReferenceTransactionResult},
};
use alloy_primitives::{B256, U64};
use parking_lot::RwLock;
use reth_db_api::{cursor::DbCursorRO, transaction::DbTx};
use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

/// Runtime phase that controls whether reference queries can be served.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReferenceIndexPhase {
    /// Runtime has not opened and validated its derived database yet.
    Opening = 0,
    /// Canonical head predates Jade, so the complete result is empty.
    PreJade = 1,
    /// Historical chain sync is active and index writes are intentionally paused.
    Deferred = 2,
    /// A bounded historical batch is being reconciled or committed.
    Backfill = 3,
    /// Durable cursor and hash matched the canonical head after the latest turn.
    Live = 4,
    /// A non-canonical indexed suffix is being removed.
    Repairing = 5,
    /// Queries cannot be served until retry or a manual rebuild succeeds.
    Unavailable = 6,
}

impl ReferenceIndexPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Opening,
            1 => Self::PreJade,
            2 => Self::Deferred,
            3 => Self::Backfill,
            4 => Self::Live,
            5 => Self::Repairing,
            6 => Self::Unavailable,
            _ => unreachable!("reference index phase is only written by this crate"),
        }
    }
}

#[derive(Debug)]
struct ReferenceIndexShared {
    db: RwLock<Option<ReferenceIndexDb>>,
    phase: AtomicU8,
    /// Even values are stable snapshots; odd values mean a transition is in progress.
    generation: AtomicU64,
    metrics: ReferenceIndexMetrics,
}

/// Cloneable query handle shared by the runtime and RPC layer.
///
/// The runtime installs the database and advances the phase. Queries only
/// succeed in [`ReferenceIndexPhase::Live`] when the durable cursor exactly
/// matches the supplied canonical tip number and hash.
#[derive(Clone, Debug)]
pub struct ReferenceIndexHandle {
    shared: Arc<ReferenceIndexShared>,
    jade_timestamp: u64,
}

impl ReferenceIndexHandle {
    /// Create a handle before the background runtime has opened the database.
    pub fn new(jade_timestamp: u64) -> Self {
        Self {
            shared: Arc::new(ReferenceIndexShared {
                db: RwLock::new(None),
                phase: AtomicU8::new(ReferenceIndexPhase::Opening as u8),
                generation: AtomicU64::new(0),
                metrics: ReferenceIndexMetrics::default(),
            }),
            jade_timestamp,
        }
    }

    /// Current in-memory runtime phase, exposed for logs and metrics.
    pub fn phase(&self) -> ReferenceIndexPhase {
        ReferenceIndexPhase::from_u8(self.shared.phase.load(Ordering::Acquire))
    }

    pub(crate) fn install_db(&self, db: ReferenceIndexDb) {
        *self.shared.db.write() = Some(db);
    }

    pub(crate) fn set_phase(&self, phase: ReferenceIndexPhase) {
        if self.phase() == phase {
            return;
        }
        self.shared.generation.fetch_add(1, Ordering::AcqRel);
        self.shared.phase.store(phase as u8, Ordering::Release);
        self.shared.generation.fetch_add(1, Ordering::Release);
        self.shared.metrics.phase.set(phase as u8 as f64);
        self.shared
            .metrics
            .rpc_ready
            .set(f64::from(phase == ReferenceIndexPhase::Live));
    }

    pub(crate) fn db(&self) -> Option<ReferenceIndexDb> {
        self.shared.db.read().clone()
    }

    pub(crate) fn metrics(&self) -> &ReferenceIndexMetrics {
        &self.shared.metrics
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Execute a paginated query at an exact canonical chain snapshot.
    ///
    /// Cursor validation and result iteration share one MDBX read transaction,
    /// so callers never observe a cursor from one index commit and rows from
    /// another. The RPC layer must additionally verify that its canonical tip
    /// did not change while this method was running.
    pub fn query_at(
        &self,
        query: ReferenceQuery,
        canonical_tip: CanonicalTip,
    ) -> Result<Vec<ReferenceTransactionResult>, ReferenceIndexError> {
        let generation = loop {
            let generation = self.shared.generation.load(Ordering::Acquire);
            if generation.is_multiple_of(2) {
                break generation;
            }
            std::hint::spin_loop();
        };
        match self.phase() {
            ReferenceIndexPhase::Opening => return Err(ReferenceIndexError::Initializing),
            ReferenceIndexPhase::PreJade => {
                return if canonical_tip.timestamp < self.jade_timestamp {
                    Ok(Vec::new())
                } else {
                    Err(ReferenceIndexError::IndexBehind)
                };
            }
            ReferenceIndexPhase::Deferred | ReferenceIndexPhase::Backfill => {
                return Err(ReferenceIndexError::IndexBehind);
            }
            ReferenceIndexPhase::Repairing | ReferenceIndexPhase::Unavailable => {
                return Err(ReferenceIndexError::IndexUnavailable);
            }
            ReferenceIndexPhase::Live => {}
        }

        let db = self.db().ok_or(ReferenceIndexError::IndexUnavailable)?;
        let tx = db.tx()?;
        let indexed_to = tx
            .get::<IndexMeta>(IndexMetaKey::IndexedTo.into())?
            .map(decode_u64)
            .transpose()?;
        let Some(indexed_to) = indexed_to else {
            return Err(ReferenceIndexError::IndexUnavailable);
        };

        let indexed_hash = tx
            .get::<IndexedBlocks>(IndexedBlockKey {
                block_number: indexed_to,
            })?
            .map(|value| value.0);
        let Some(indexed_hash) = indexed_hash else {
            return Err(ReferenceIndexError::IndexUnavailable);
        };
        if indexed_to != canonical_tip.number || indexed_hash != canonical_tip.hash {
            return Err(ReferenceIndexError::IndexBehind);
        }

        let mut cursor = tx.cursor_read::<ReferenceIndex>()?;
        let seek_key = ReferenceIndexKey {
            reference: query.reference,
            block_number: 0,
            transaction_index: 0,
            transaction_hash: B256::ZERO,
        };

        let mut skipped = 0u64;
        let mut results = Vec::new();
        let mut next = cursor.seek(seek_key)?;
        while let Some((key, value)) = next {
            if key.reference != query.reference || results.len() as u64 >= query.limit {
                break;
            }
            if skipped < query.offset {
                skipped += 1;
            } else {
                results.push(ReferenceTransactionResult {
                    transaction_hash: key.transaction_hash,
                    block_number: U64::from(key.block_number),
                    block_timestamp: U64::from(value.0),
                    transaction_index: U64::from(key.transaction_index),
                });
            }
            next = cursor.next()?;
        }

        if self.shared.generation.load(Ordering::Acquire) != generation
            || self.phase() != ReferenceIndexPhase::Live
        {
            return Err(ReferenceIndexError::IndexUnavailable);
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::ReferenceIndexDb,
        writer::{update_indexed_to, write_block},
    };
    use alloy_primitives::B256;
    use tempfile::TempDir;

    fn handle_with_phase(
        db: ReferenceIndexDb,
        jade_timestamp: u64,
        phase: ReferenceIndexPhase,
    ) -> ReferenceIndexHandle {
        let handle = ReferenceIndexHandle::new(jade_timestamp);
        handle.install_db(db);
        handle.set_phase(phase);
        handle
    }

    #[test]
    fn query_returns_initializing_while_runtime_is_opening() {
        let handle = ReferenceIndexHandle::new(200);
        let query = ReferenceQuery::new(B256::ZERO, None, None).unwrap();
        let tip = CanonicalTip {
            number: 0,
            hash: B256::ZERO,
            timestamp: 0,
        };

        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::Initializing)
        ));
    }

    #[test]
    fn setting_the_same_phase_does_not_invalidate_readers() {
        let handle = ReferenceIndexHandle::new(200);
        let generation = handle.shared.generation.load(Ordering::Acquire);

        handle.set_phase(ReferenceIndexPhase::Opening);

        assert_eq!(handle.shared.generation.load(Ordering::Acquire), generation);
    }

    #[test]
    fn query_returns_index_behind_when_tip_is_one_block_ahead() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        write_block(&tx, 10, B256::repeat_byte(0xaa), 100, &[]).unwrap();
        update_indexed_to(&tx, 10).unwrap();
        tx.commit().unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::with_last_byte(1), None, None).unwrap();
        let tip = CanonicalTip {
            number: 11,
            hash: B256::repeat_byte(0xbb),
            timestamp: 101,
        };

        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn query_at_rejects_a_non_canonical_cursor_hash() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        write_block(&tx, 10, B256::repeat_byte(0xaa), 100, &[]).unwrap();
        update_indexed_to(&tx, 10).unwrap();
        tx.commit().unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::with_last_byte(1), None, None).unwrap();
        let tip = CanonicalTip {
            number: 10,
            hash: B256::repeat_byte(0xbb),
            timestamp: 100,
        };

        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn live_query_treats_a_missing_cursor_as_unavailable() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::ZERO, None, None).unwrap();
        let tip = CanonicalTip {
            number: 1,
            hash: B256::repeat_byte(0x11),
            timestamp: 100,
        };

        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::IndexUnavailable)
        ));
    }

    #[test]
    fn live_query_treats_a_missing_cursor_hash_as_unavailable() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        update_indexed_to(&tx, 1).unwrap();
        tx.commit().unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::ZERO, None, None).unwrap();
        let tip = CanonicalTip {
            number: 1,
            hash: B256::repeat_byte(0x11),
            timestamp: 100,
        };

        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::IndexUnavailable)
        ));
    }

    #[test]
    fn pre_jade_query_is_complete_without_a_cursor() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let handle = handle_with_phase(db, 200, ReferenceIndexPhase::PreJade);
        let query = ReferenceQuery::new(B256::with_last_byte(1), None, None).unwrap();
        let tip = CanonicalTip {
            number: 100,
            hash: B256::repeat_byte(0xaa),
            timestamp: 199,
        };

        assert!(handle.query_at(query, tip).unwrap().is_empty());
    }

    #[test]
    fn live_query_returns_empty_when_no_reference_txs_exist() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let block_hash = B256::repeat_byte(0x01);
        let tx = db.tx_mut().unwrap();
        write_block(&tx, 1, block_hash, 100, &[]).unwrap();
        update_indexed_to(&tx, 1).unwrap();
        tx.commit().unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::with_last_byte(0x42), None, None).unwrap();
        let tip = CanonicalTip {
            number: 1,
            hash: block_hash,
            timestamp: 100,
        };

        assert!(handle.query_at(query, tip).unwrap().is_empty());
    }
}
