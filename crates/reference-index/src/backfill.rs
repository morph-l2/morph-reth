//! Bounded historical reference-index backfill helpers.

use crate::{
    db::ReferenceIndexDb,
    source::{CanonicalBlock, CanonicalChain},
    types::ReferenceIndexError,
    writer::{
        prune_indexed_blocks_before, set_jade_first_block_number, update_indexed_to, write_block,
    },
};
use reth_db_api::transaction::DbTx;

/// Find the first canonical block whose timestamp activates Jade.
pub(crate) fn resolve_jade_first_block<C: CanonicalChain>(
    chain: &C,
    head: crate::CanonicalTip,
    jade_timestamp: u64,
) -> Result<Option<u64>, ReferenceIndexError> {
    if head.timestamp < jade_timestamp {
        return Ok(None);
    }

    let mut low = 0u64;
    let mut high = head.number;
    while low < high {
        let middle = low + (high - low) / 2;
        let timestamp = chain.block_timestamp(middle)?.ok_or_else(|| {
            ReferenceIndexError::Other(eyre::eyre!(
                "missing canonical header {middle} while resolving Jade"
            ))
        })?;
        if timestamp < jade_timestamp {
            low = middle + 1;
        } else {
            high = middle;
        }
    }

    Ok(Some(low))
}

/// Commit one already-validated contiguous canonical block batch.
///
/// `finalized` is the L1 finalized canonical block number, if any. Because a
/// finalized block can never be reorged, `IndexedBlocks` breadcrumbs strictly
/// below it are never needed for reorg rewind again and are pruned here — but
/// only once the cursor tip has reached the pruning floor, so a deep backfill
/// that is still below finalized keeps its rows (and the tip's row) intact.
pub(crate) fn commit_canonical_batch(
    db: &ReferenceIndexDb,
    jade_first_block: u64,
    expected_start: u64,
    blocks: &[CanonicalBlock],
    finalized: Option<u64>,
) -> Result<u64, ReferenceIndexError> {
    let Some(last) = blocks.last() else {
        return Ok(0);
    };

    for (offset, block) in blocks.iter().enumerate() {
        let expected = expected_start.saturating_add(offset as u64);
        if block.number != expected {
            return Err(ReferenceIndexError::Other(eyre::eyre!(
                "non-contiguous reference-index batch: expected block {expected}, got {}",
                block.number
            )));
        }
    }

    let tx = db.tx_mut()?;
    let mut references_written = 0u64;
    for block in blocks {
        references_written += write_block(
            &tx,
            block.number,
            block.hash,
            block.timestamp,
            &block.transactions,
        )?;
    }
    set_jade_first_block_number(&tx, jade_first_block)?;
    update_indexed_to(&tx, last.number)?;
    // Prune reorg breadcrumbs below finalized, but never above the cursor tip:
    // during a deep backfill still below finalized the floor is skipped so the
    // tip's own breadcrumb (and everything reconcile may still need) survives.
    if let Some(finalized) = finalized {
        let floor = finalized.max(jade_first_block);
        if floor <= last.number {
            prune_indexed_blocks_before(&tx, floor)?;
        }
    }
    tx.commit()?;
    Ok(references_written)
}
