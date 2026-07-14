//! Bounded historical reference-index backfill helpers.

use crate::{
    db::ReferenceIndexDb,
    source::{CanonicalBlock, CanonicalChain},
    types::ReferenceIndexError,
    writer::{set_jade_first_block_number, update_indexed_to, write_block},
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
pub(crate) fn commit_canonical_batch(
    db: &ReferenceIndexDb,
    jade_first_block: u64,
    expected_start: u64,
    blocks: &[CanonicalBlock],
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
    tx.commit()?;
    Ok(references_written)
}
