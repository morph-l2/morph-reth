//! Bounded rollback helpers for canonical-chain reconciliation.

use crate::{
    db::ReferenceIndexDb,
    types::ReferenceIndexError,
    writer::{clear_indexed_to, clear_jade_first_block_number, delete_block, update_indexed_to},
};
use reth_db_api::transaction::DbTx;

/// Delete the indexed suffix after `ancestor` using bounded write transactions.
///
/// When `ancestor` is `None`, deletes all indexed blocks from `min_block`
/// through the durable cursor and clears both `IndexedTo` and
/// `JadeFirstBlockNumber` atomically on the final batch.
pub(crate) fn rollback_suffix(
    db: &ReferenceIndexDb,
    ancestor: Option<u64>,
    min_block: u64,
    batch_size: u64,
) -> Result<(), ReferenceIndexError> {
    if batch_size == 0 {
        return Err(ReferenceIndexError::InvalidBackfillBatchSize);
    }

    loop {
        let Some(cursor) = db.indexed_to()? else {
            return Ok(());
        };
        if ancestor.is_some_and(|ancestor| cursor <= ancestor) {
            return Ok(());
        }

        let lower_bound = ancestor.map_or(min_block, |ancestor| ancestor.saturating_add(1));
        if cursor < lower_bound {
            let tx = db.tx_mut()?;
            clear_indexed_to(&tx)?;
            clear_jade_first_block_number(&tx)?;
            tx.commit()?;
            return Ok(());
        }
        let batch_start = cursor
            .saturating_sub(batch_size.saturating_sub(1))
            .max(lower_bound);
        let tx = db.tx_mut()?;
        for number in batch_start..=cursor {
            delete_block(&tx, number)?;
        }

        if batch_start == lower_bound {
            if let Some(ancestor) = ancestor {
                update_indexed_to(&tx, ancestor)?;
            } else {
                clear_indexed_to(&tx)?;
                clear_jade_first_block_number(&tx)?;
            }
        } else {
            update_indexed_to(&tx, batch_start - 1)?;
        }
        tx.commit()?;
    }
}
