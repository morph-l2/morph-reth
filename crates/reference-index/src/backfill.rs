//! Historical reference index backfill.
//!
//! Runs once at startup to index all MorphTx transactions from
//! `jade_first_block_number` up to the chain head at startup time.

use crate::{
    JADE_NOT_ACTIVE_SENTINEL, ReferenceIndexDb,
    types::{BackfillState, ReferenceIndexError},
    writer::{
        set_backfill_state, set_jade_first_block_number, update_backfill_current,
        update_indexed_from, update_indexed_to, write_block,
    },
};
use alloy_consensus::BlockHeader;
use morph_chainspec::hardfork::{MorphHardfork, MorphHardforks};
use morph_primitives::MorphHeader;
use reth_chainspec::ForkCondition;
use reth_db_api::transaction::DbTx;
use reth_provider::{BlockReader, HeaderProvider};
use reth_storage_api::{BlockNumReader, TransactionVariant};
use tracing::{debug, info};

/// Determine the first block number at which the Jade hardfork is active.
///
/// Returns `JADE_NOT_ACTIVE_SENTINEL` (`u64::MAX`) when:
/// - The chain spec has no Jade timestamp condition, or
/// - The current canonical head has not yet reached Jade activation.
///
/// The dual-condition check (`block(N).timestamp >= jade_ts` AND
/// `block(N-1).timestamp < jade_ts`) guarantees we return the **first**
/// Jade block, not just any block after activation.
pub fn resolve_jade_first_block<P, CS>(
    provider: &P,
    chain_spec: &CS,
    head: u64,
) -> Result<u64, ReferenceIndexError>
where
    P: HeaderProvider<Header = MorphHeader>,
    CS: MorphHardforks,
{
    let jade_ts = match chain_spec.morph_fork_activation(MorphHardfork::Jade) {
        ForkCondition::Timestamp(ts) => ts,
        _ => return Ok(JADE_NOT_ACTIVE_SENTINEL),
    };

    let head_header = provider
        .header_by_number(head)?
        .ok_or_else(|| ReferenceIndexError::Other(eyre::eyre!("missing canonical head header")))?;

    if head_header.timestamp() < jade_ts {
        return Ok(JADE_NOT_ACTIVE_SENTINEL);
    }

    // Binary-search over [0, head] to find the lowest block whose timestamp
    // is >= jade_ts.  We still verify the prev block to guard against unusual
    // timestamp monotonicity violations.
    let mut lo = 0u64;
    let mut hi = head;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mid_header = provider
            .header_by_number(mid)?
            .ok_or_else(|| ReferenceIndexError::Other(eyre::eyre!("missing header at {mid}")))?;
        if mid_header.timestamp() < jade_ts {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }

    // `lo` is now the first block whose timestamp >= jade_ts.
    // Verify that the previous block (if any) has timestamp < jade_ts.
    if lo > 0 {
        let prev = provider.header_by_number(lo - 1)?.ok_or_else(|| {
            ReferenceIndexError::Other(eyre::eyre!("missing header at {}", lo - 1))
        })?;
        if prev.timestamp() >= jade_ts {
            // Shouldn't happen on a well-formed chain, but fall back to sentinel.
            return Ok(JADE_NOT_ACTIVE_SENTINEL);
        }
    }

    Ok(lo)
}

/// Run (or resume) the historical backfill.
///
/// This function is synchronous and blocks the calling task until backfill is
/// complete.  Batches are committed atomically so a crash mid-run can be
/// safely resumed from the last persisted checkpoint.
pub fn run_backfill<P, CS>(
    db: &ReferenceIndexDb,
    provider: &P,
    chain_spec: &CS,
    head_at_startup: u64,
    batch_size: u64,
) -> Result<(), ReferenceIndexError>
where
    P: BlockReader<Block = morph_primitives::Block>
        + BlockNumReader
        + HeaderProvider<Header = MorphHeader>,
    CS: MorphHardforks,
{
    let state = db.backfill_state()?;

    // `jade_first_block_number` is our canonical lower bound; it's written as the
    // very first step on the NotStarted path.  InProgress/Complete resumes read
    // it back from IndexMeta.
    let start = match state {
        BackfillState::Complete => return Ok(()),
        BackfillState::InProgress => {
            // Resume from `max(indexed_to + 1, jade_first_block_number)`: if the
            // crash happened between InProgress write and the first batch commit,
            // `indexed_to` is still 0 and must not be used as-is.
            let jade_first_stored = db
                .jade_first_block_number()?
                .unwrap_or(JADE_NOT_ACTIVE_SENTINEL);
            if jade_first_stored == JADE_NOT_ACTIVE_SENTINEL {
                // Previously sentinel; crash may have happened before the follow-up
                // txn that would have marked Complete.  Re-resolve against the current
                // head: Jade might have activated in the meantime.
                let new_jade = resolve_jade_first_block(provider, chain_spec, head_at_startup)?;
                if new_jade == JADE_NOT_ACTIVE_SENTINEL {
                    // Still not active; safe to mark complete immediately.
                    let tx = db.tx_mut()?;
                    update_indexed_from(&tx, head_at_startup)?;
                    update_indexed_to(&tx, head_at_startup)?;
                    set_backfill_state(&tx, BackfillState::Complete)?;
                    tx.commit()?;
                    return Ok(());
                }
                // Jade now active; persist the real first block and continue backfill.
                let tx = db.tx_mut()?;
                set_jade_first_block_number(&tx, new_jade)?;
                tx.commit()?;
                db.indexed_to()?.saturating_add(1).max(new_jade)
            } else {
                db.indexed_to()?.saturating_add(1).max(jade_first_stored)
            }
        }
        BackfillState::NotStarted => {
            let jade_first = resolve_jade_first_block(provider, chain_spec, head_at_startup)?;

            let tx = db.tx_mut()?;
            set_jade_first_block_number(&tx, jade_first)?;
            set_backfill_state(&tx, BackfillState::InProgress)?;
            tx.commit()?;

            if jade_first == JADE_NOT_ACTIVE_SENTINEL || jade_first > head_at_startup {
                // Nothing to backfill; mark complete immediately.
                let tx = db.tx_mut()?;
                update_indexed_from(&tx, head_at_startup)?;
                update_indexed_to(&tx, head_at_startup)?;
                set_backfill_state(&tx, BackfillState::Complete)?;
                tx.commit()?;
                return Ok(());
            }

            jade_first
        }
    };

    if start > head_at_startup {
        // Already up to date.
        let tx = db.tx_mut()?;
        set_backfill_state(&tx, BackfillState::Complete)?;
        tx.commit()?;
        return Ok(());
    }

    let jade_first = db.jade_first_block_number()?.unwrap_or(start);

    info!(
        target: "morph::reference_index",
        start, head_at_startup,
        "starting reference index backfill"
    );

    let mut current = start;

    while current <= head_at_startup {
        let batch_end = current.saturating_add(batch_size - 1).min(head_at_startup);
        let is_last_batch = batch_end == head_at_startup;

        let tx = db.tx_mut()?;
        for number in current..=batch_end {
            // `WithHash` is required because write_block stores transaction hashes in the
            // reference index keys.  `NoHash` would leave tx hashes uninitialised per
            // reth's BlockReader contract (see blockchain_provider.rs:309).
            let block = provider
                .sealed_block_with_senders(number.into(), TransactionVariant::WithHash)?
                .ok_or_else(|| {
                    ReferenceIndexError::Other(eyre::eyre!(
                        "missing block {number} during backfill"
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

        if is_last_batch {
            // Atomic: data + Complete + indexed_from + indexed_to in one commit.
            update_indexed_from(&tx, jade_first)?;
            update_indexed_to(&tx, head_at_startup)?;
            set_backfill_state(&tx, BackfillState::Complete)?;
        } else {
            update_backfill_current(&tx, batch_end)?;
            update_indexed_to(&tx, batch_end)?;
            set_backfill_state(&tx, BackfillState::InProgress)?;
        }
        tx.commit()?;

        debug!(
            target: "morph::reference_index",
            batch_start = current,
            batch_end,
            is_last_batch,
            "backfill batch committed"
        );

        current = batch_end + 1;

        if !is_last_batch {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    info!(target: "morph::reference_index", "reference index backfill complete");
    Ok(())
}

/// Re-validate and (if needed) reset `jade_first_block_number` when the DB
/// was opened with a sentinel value (`u64::MAX`) from a previous run where
/// Jade had not yet activated.
///
/// Call this once at startup before `run_backfill`.
pub fn maybe_reset_jade_sentinel<P, CS>(
    db: &ReferenceIndexDb,
    provider: &P,
    chain_spec: &CS,
    head: u64,
) -> Result<(), ReferenceIndexError>
where
    P: HeaderProvider<Header = MorphHeader>,
    CS: MorphHardforks,
{
    if db.backfill_state()? != BackfillState::Complete {
        return Ok(());
    }
    match db.jade_first_block_number()? {
        Some(n) if n == JADE_NOT_ACTIVE_SENTINEL => {
            // Jade was not active when last resolved.  Re-try now.
            let new = resolve_jade_first_block(provider, chain_spec, head)?;
            if new != JADE_NOT_ACTIVE_SENTINEL {
                // Jade has since activated; reset backfill state so the
                // next startup picks it up.
                let tx = db.tx_mut()?;
                set_jade_first_block_number(&tx, new)?;
                set_backfill_state(&tx, BackfillState::NotStarted)?;
                tx.commit()?;
                info!(
                    target: "morph::reference_index",
                    jade_first_block = new,
                    "Jade has activated; resetting backfill to index from first Jade block"
                );
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jade_not_active_sentinel_is_u64_max() {
        assert_eq!(JADE_NOT_ACTIVE_SENTINEL, u64::MAX);
    }

    #[test]
    fn backfill_state_try_from_roundtrip() {
        assert_eq!(
            BackfillState::try_from(0u8).unwrap(),
            BackfillState::NotStarted
        );
        assert_eq!(
            BackfillState::try_from(1u8).unwrap(),
            BackfillState::InProgress
        );
        assert_eq!(
            BackfillState::try_from(2u8).unwrap(),
            BackfillState::Complete
        );
        assert!(BackfillState::try_from(3u8).is_err());
    }
}
