//! Reference index read path helpers.

use crate::{
    db::ReferenceIndexDb,
    tables::{ReferenceIndex, ReferenceIndexKey},
    types::{ReferenceIndexError, ReferenceQuery, ReferenceTransactionResult},
};
use alloy_primitives::{B256, U64};
use reth_db_api::{cursor::DbCursorRO, transaction::DbTx};

/// Read-only facade for reference index queries.
///
/// `canonical_tip` must be the current chain head block number so the reader
/// can detect excessive lag between the index and the live chain.
#[derive(Clone, Debug)]
pub struct ReferenceIndexReader {
    db: ReferenceIndexDb,
    lag_threshold: u64,
}

impl ReferenceIndexReader {
    pub const fn new(db: ReferenceIndexDb, lag_threshold: u64) -> Self {
        Self { db, lag_threshold }
    }

    pub fn db(&self) -> &ReferenceIndexDb {
        &self.db
    }

    /// Execute a paginated reference query.
    ///
    /// `canonical_tip` is the current best block number, used to compute lag.
    pub fn query(
        &self,
        query: ReferenceQuery,
        canonical_tip: u64,
    ) -> Result<Vec<ReferenceTransactionResult>, ReferenceIndexError> {
        if !self.db.is_ready() {
            return Err(ReferenceIndexError::Initializing);
        }

        let indexed_to = self.db.indexed_to()?;
        if canonical_tip.saturating_sub(indexed_to) > self.lag_threshold {
            crate::metrics::set_lag_blocks(canonical_tip.saturating_sub(indexed_to));
            return Err(ReferenceIndexError::IndexBehind);
        }

        let tx = self.db.tx()?;
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
            if key.reference != query.reference {
                break;
            }
            if results.len() as u64 >= query.limit {
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

        crate::metrics::set_lag_blocks(canonical_tip.saturating_sub(indexed_to));
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ReferenceIndexDb,
        writer::{update_indexed_to, write_block},
    };
    use alloy_primitives::B256;
    use tempfile::TempDir;

    fn open_ready_db() -> (TempDir, ReferenceIndexDb) {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        db.set_ready(true);
        (dir, db)
    }

    #[test]
    fn query_returns_initializing_when_not_ready() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();
        // is_ready stays false
        let reader = ReferenceIndexReader::new(db, 16);
        let q = ReferenceQuery::new(B256::ZERO, None, None).unwrap();
        assert!(matches!(
            reader.query(q, 100),
            Err(ReferenceIndexError::Initializing)
        ));
    }

    #[test]
    fn query_returns_index_behind_when_lag_exceeds_threshold() {
        let (_dir, db) = open_ready_db();

        let tx = db.tx_mut().unwrap();
        update_indexed_to(&tx, 10).unwrap();
        tx.commit().unwrap();

        let reader = ReferenceIndexReader::new(db, 16);
        let q = ReferenceQuery::new(B256::with_last_byte(1), None, None).unwrap();
        // canonical_tip=27 → lag=17 > threshold=16
        assert!(matches!(
            reader.query(q, 27),
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn query_returns_empty_when_no_reference_txs() {
        let (_dir, db) = open_ready_db();

        let tx = db.tx_mut().unwrap();
        write_block(&tx, 1, B256::repeat_byte(0x01), 100, &[]).unwrap();
        update_indexed_to(&tx, 1).unwrap();
        tx.commit().unwrap();

        let reader = ReferenceIndexReader::new(db, 16);
        let q = ReferenceQuery::new(B256::with_last_byte(0x42), None, None).unwrap();
        let results = reader.query(q, 1).unwrap();
        assert!(results.is_empty());
    }
}
