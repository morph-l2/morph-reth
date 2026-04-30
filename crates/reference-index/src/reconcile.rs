//! Startup canonical chain reconciliation.
//!
//! After backfill completes, reconcile checks the last `max_reorg_depth`
//! indexed blocks against the current canonical chain to detect any reorgs
//! that occurred while the node was offline, then fills any suffix gap between
//! `indexed_to` and `head_at_startup`.

use crate::{
    ReferenceIndexDb,
    types::ReferenceIndexError,
    writer::{delete_block, update_indexed_to, write_block},
};
use alloy_consensus::BlockHeader;
use morph_primitives::MorphHeader;
use reth_db_api::transaction::DbTx;
use reth_provider::{BlockHashReader, BlockReader, HeaderProvider};
use reth_storage_api::TransactionVariant;
use tracing::{debug, info};

/// Run the startup reconciliation pass.
///
/// Steps:
/// A. Canonical hash check over the last `max_reorg_depth` indexed blocks.
///    On mismatch, find the lowest diverging height, delete forward entries,
///    reset `indexed_to`, and continue as if filling a gap.
/// B. Suffix gap fill: write every block from `indexed_to + 1` to
///    `current_canonical_head` into the three index tables.
pub fn run_startup_reconcile<P>(
    db: &ReferenceIndexDb,
    provider: &P,
    current_head: u64,
    max_reorg_depth: u64,
) -> Result<(), ReferenceIndexError>
where
    P: BlockReader<Block = morph_primitives::Block>
        + HeaderProvider<Header = MorphHeader>
        + BlockHashReader,
{
    let indexed_to = db.indexed_to()?;

    // ── Step A: canonical hash check ────────────────────────────────────────
    // Use indexed_from as lower bound to avoid scanning blocks that were never
    // indexed (e.g. pre-Jade blocks on the sentinel path where backfill sets
    // indexed_to but writes no IndexedBlocks entries).
    let indexed_from = db.indexed_from()?.unwrap_or(indexed_to);
    let depth_start = indexed_to.saturating_sub(max_reorg_depth.saturating_sub(1));
    let check_start = indexed_from.max(depth_start);
    let mut fork_height: Option<u64> = None;

    for number in check_start..=indexed_to {
        let indexed_hash = db.indexed_block_hash(number)?;

        // `None` means the block was never written to IndexedBlocks (e.g. the
        // sentinel path where backfill marks indexed_to = head but writes no
        // IndexedBlocks entries for pre-Jade blocks).  Treat as "not indexed,
        // skip" rather than a hash mismatch to avoid spurious reorg detection.
        let Some(indexed) = indexed_hash else {
            continue;
        };

        let canonical_hash = provider.block_hash(number)?;
        if Some(indexed) != canonical_hash {
            debug!(
                target: "morph::reference_index",
                number,
                ?indexed,
                ?canonical_hash,
                "canonical hash mismatch during reconcile"
            );
            fork_height = Some(number);
            break;
        }
    }

    // ── Step B: apply reorg if detected ──────────────────────────────────────
    let rebuild_start = if let Some(fh) = fork_height {
        info!(
            target: "morph::reference_index",
            fork_height = fh,
            old_indexed_to = indexed_to,
            "offline reorg detected; rolling back index"
        );

        let tx = db.tx_mut()?;
        for number in fh..=indexed_to {
            delete_block(&tx, number)?;
        }
        let new_indexed_to = fh.saturating_sub(1);
        update_indexed_to(&tx, new_indexed_to)?;
        tx.commit()?;

        fh
    } else {
        indexed_to.saturating_add(1)
    };

    // ── Step C: suffix gap fill ───────────────────────────────────────────────
    if rebuild_start <= current_head {
        info!(
            target: "morph::reference_index",
            rebuild_start,
            current_head,
            "filling reference index suffix gap"
        );

        let tx = db.tx_mut()?;
        for number in rebuild_start..=current_head {
            // `WithHash` is required: we persist tx hashes as part of the index keys.
            let block = provider
                .sealed_block_with_senders(number.into(), TransactionVariant::WithHash)?
                .ok_or_else(|| {
                    ReferenceIndexError::Other(eyre::eyre!(
                        "missing block {number} during reconcile"
                    ))
                })?;

            write_block(
                &tx,
                block.number(),
                block.hash(),
                block.timestamp(),
                &block.body().transactions,
            )?;
        }
        update_indexed_to(&tx, current_head)?;
        tx.commit()?;
    }

    Ok(())
}
