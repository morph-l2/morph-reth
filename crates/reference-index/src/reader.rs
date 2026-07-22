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
    atomic::{AtomicBool, AtomicU8, Ordering},
};

/// Runtime phase, exported purely for metrics and logs.
///
/// The read path no longer branches on this. Read correctness comes from the
/// MDBX read-transaction snapshot plus the `(indexed_to, indexed_hash) == tip`
/// comparison in [`ReferenceIndexHandle::query_at`], and from the before/after
/// `chain_info()` bracketing in the RPC layer. The only read-gate bit derived
/// from phase is [`ReferenceIndexShared::unavailable`].
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
    /// Latest runtime phase. Kept for metrics/logs only; not read on the query
    /// path (see [`ReferenceIndexPhase`]).
    phase: AtomicU8,
    /// Set while the runtime is in [`ReferenceIndexPhase::Unavailable`] (manual
    /// rebuild required / persistent failure). The read path returns
    /// `IndexUnavailable` so clients stop retrying; in every other phase the
    /// tip comparison decides on its own (`IndexBehind` when the cursor lags).
    unavailable: AtomicBool,
    metrics: ReferenceIndexMetrics,
}

/// Cloneable query handle shared by the runtime and RPC layer.
///
/// The runtime installs the database and advances the phase. A query succeeds
/// only when the durable cursor exactly matches the supplied canonical tip
/// (number and hash) within a single MDBX snapshot; otherwise it is
/// `IndexBehind`. The `Unavailable` phase is the one runtime state that
/// short-circuits to `IndexUnavailable`.
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
                unavailable: AtomicBool::new(false),
                metrics: ReferenceIndexMetrics::default(),
            }),
            jade_timestamp,
        }
    }

    /// Current in-memory runtime phase, exposed for logs and metrics.
    pub fn phase(&self) -> ReferenceIndexPhase {
        ReferenceIndexPhase::from_u8(self.shared.phase.load(Ordering::Acquire))
    }

    /// Whether a canonical tip at `timestamp` predates Jade.
    ///
    /// Before Jade no reference-carrying transaction can exist, so the complete
    /// result is empty. The RPC layer short-circuits on this so pre-Jade
    /// queries never reach [`Self::query_at`] — a pre-Jade answer is a terminal
    /// empty result, not `IndexBehind`, and clients must not retry it.
    pub fn is_pre_jade(&self, timestamp: u64) -> bool {
        timestamp < self.jade_timestamp
    }

    pub(crate) fn install_db(&self, db: ReferenceIndexDb) {
        *self.shared.db.write() = Some(db);
    }

    pub(crate) fn set_phase(&self, phase: ReferenceIndexPhase) {
        if self.phase() == phase {
            return;
        }
        self.shared.phase.store(phase as u8, Ordering::Release);
        self.shared
            .unavailable
            .store(phase == ReferenceIndexPhase::Unavailable, Ordering::Release);
        self.shared.metrics.phase.set(phase as u8 as f64);
    }

    pub(crate) fn db(&self) -> Option<ReferenceIndexDb> {
        self.shared.db.read().clone()
    }

    pub(crate) fn metrics(&self) -> &ReferenceIndexMetrics {
        &self.shared.metrics
    }

    /// Record the outcome of a bounded server-side catch-up wait performed by
    /// the RPC layer. Counts only waits whose budget elapsed and still returned
    /// `IndexBehind`; a wait that resolves to data (or fails fast) is not counted.
    pub fn observe_rpc_wait(&self, timed_out: bool) {
        if timed_out {
            self.shared.metrics.rpc_wait_timeouts_total.increment(1);
        }
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
        if self.shared.unavailable.load(Ordering::Acquire) {
            return Err(ReferenceIndexError::IndexUnavailable);
        }

        let db = self.db().ok_or(ReferenceIndexError::Initializing)?;
        let tx = db.tx()?;
        let indexed_to = tx
            .get::<IndexMeta>(IndexMetaKey::IndexedTo.into())?
            .map(decode_u64)
            .transpose()?;
        let Some(indexed_to) = indexed_to else {
            return Err(ReferenceIndexError::IndexBehind);
        };

        let indexed_hash = tx
            .get::<IndexedBlocks>(IndexedBlockKey {
                block_number: indexed_to,
            })?
            .map(|value| value.0);
        let Some(indexed_hash) = indexed_hash else {
            return Err(ReferenceIndexError::IndexBehind);
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
    fn unavailable_phase_blocks_reads_and_recovers() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        let tx = db.tx_mut().unwrap();
        write_block(&tx, 10, B256::repeat_byte(0xaa), 100, &[]).unwrap();
        update_indexed_to(&tx, 10).unwrap();
        tx.commit().unwrap();
        let handle = handle_with_phase(db, 0, ReferenceIndexPhase::Unavailable);
        let query = ReferenceQuery::new(B256::with_last_byte(1), None, None).unwrap();
        let tip = CanonicalTip {
            number: 10,
            hash: B256::repeat_byte(0xaa),
            timestamp: 100,
        };

        // Unavailable short-circuits even when the cursor matches the tip.
        assert!(matches!(
            handle.query_at(query, tip),
            Err(ReferenceIndexError::IndexUnavailable)
        ));

        // Recovering out of Unavailable clears the gate; the matching tip serves.
        handle.set_phase(ReferenceIndexPhase::Live);
        assert!(handle.query_at(query, tip).unwrap().is_empty());
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
    fn query_with_a_missing_cursor_is_behind() {
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
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn query_with_a_missing_cursor_hash_is_behind() {
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
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn is_pre_jade_reflects_the_jade_timestamp() {
        let handle = ReferenceIndexHandle::new(200);
        assert!(handle.is_pre_jade(199));
        assert!(!handle.is_pre_jade(200));
        assert!(!handle.is_pre_jade(201));
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
