//! Reference index write path helpers.
//!
//! All write functions take an already-open write transaction so that the
//! caller can batch multiple blocks (backfill) or delete+write (reorg) in a
//! single atomic commit.

use crate::{
    db::{IndexMetaKey, encode_u64},
    tables::{
        BlockHashValue, BlockReferenceIndex, BlockReferenceKey, BlockTimestampValue, IndexMeta,
        IndexedBlockKey, IndexedBlocks, ReferenceIndex, ReferenceIndexKey, ReferenceValue,
    },
    types::ReferenceIndexError,
};
use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::B256;
use morph_primitives::MorphTxEnvelope;
use reth_db_api::{cursor::DbCursorRO, transaction::DbTxMut};

/// Index one canonical block into all three data tables.
///
/// Returns the number of reference entries written.
///
/// **Not idempotent on its own**: the caller must call [`delete_block`] for
/// the same block number before re-writing to avoid leaving stale entries
/// (keys contain `tx_hash`, so a re-write with a different tx set would leave
/// old-tx ghost rows).  Reconcile and reorg paths follow this contract.
pub(crate) fn write_block<Tx: DbTxMut>(
    tx: &Tx,
    block_number: u64,
    block_hash: B256,
    block_timestamp: u64,
    transactions: &[MorphTxEnvelope],
) -> Result<u64, ReferenceIndexError> {
    tx.put::<IndexedBlocks>(IndexedBlockKey { block_number }, BlockHashValue(block_hash))?;

    let mut written = 0u64;
    for (idx, transaction) in transactions.iter().enumerate() {
        let Some(reference) = transaction.reference() else {
            continue;
        };
        let transaction_index = idx as u64;
        let transaction_hash = *transaction.tx_hash();

        let reference_key = ReferenceIndexKey {
            reference,
            block_number,
            transaction_index,
            transaction_hash,
        };
        tx.put::<ReferenceIndex>(reference_key, BlockTimestampValue(block_timestamp))?;
        tx.put::<BlockReferenceIndex>(
            BlockReferenceKey {
                block_number,
                transaction_index,
                transaction_hash,
            },
            ReferenceValue(reference),
        )?;
        written += 1;
    }

    Ok(written)
}

/// Delete all reference-index state for a single block number.
///
/// Implements the reverse of [`write_block`]: reads every
/// `BlockReferenceIndex` row for `block_number`, reconstructs each
/// `ReferenceIndex` key, and removes entries from all three tables.
pub(crate) fn delete_block<Tx: DbTxMut>(
    tx: &Tx,
    block_number: u64,
) -> Result<(), ReferenceIndexError> {
    // 1. Collect all BlockReferenceIndex rows for this block.
    let mut entries = Vec::new();
    {
        let mut cursor = tx.cursor_write::<BlockReferenceIndex>()?;
        let start = BlockReferenceKey {
            block_number,
            transaction_index: 0,
            transaction_hash: B256::ZERO,
        };
        let mut next = cursor.seek(start)?;
        while let Some((key, value)) = next {
            if key.block_number != block_number {
                break;
            }
            entries.push((key, value.0));
            next = cursor.next()?;
        }
    }

    // 2. Delete each ReferenceIndex + BlockReferenceIndex row.
    for (key, reference) in entries {
        tx.delete::<ReferenceIndex>(
            ReferenceIndexKey {
                reference,
                block_number: key.block_number,
                transaction_index: key.transaction_index,
                transaction_hash: key.transaction_hash,
            },
            None,
        )?;
        tx.delete::<BlockReferenceIndex>(key, None)?;
    }

    // 3. Remove the IndexedBlocks row.
    tx.delete::<IndexedBlocks>(IndexedBlockKey { block_number }, None)?;
    Ok(())
}

// ── metadata writes ──────────────────────────────────────────────────────────

pub(crate) fn update_indexed_to<Tx: DbTxMut>(
    tx: &Tx,
    block_number: u64,
) -> Result<(), ReferenceIndexError> {
    tx.put::<IndexMeta>(IndexMetaKey::IndexedTo.into(), encode_u64(block_number))?;
    Ok(())
}

pub(crate) fn clear_indexed_to<Tx: DbTxMut>(tx: &Tx) -> Result<(), ReferenceIndexError> {
    tx.delete::<IndexMeta>(IndexMetaKey::IndexedTo.into(), None)?;
    Ok(())
}

pub(crate) fn set_jade_first_block_number<Tx: DbTxMut>(
    tx: &Tx,
    block_number: u64,
) -> Result<(), ReferenceIndexError> {
    tx.put::<IndexMeta>(
        IndexMetaKey::JadeFirstBlockNumber.into(),
        encode_u64(block_number),
    )?;
    Ok(())
}

/// Prune `IndexedBlocks` breadcrumbs strictly below `floor`.
///
/// Only the `block_number -> block_hash` breadcrumbs consumed by reorg rewind
/// are removed; the `ReferenceIndex` / `BlockReferenceIndex` query data is never
/// touched, so pruned history stays fully queryable. Callers pass a floor at or
/// below both the durable cursor tip and the reorg rewind lower bound, so reads
/// and reconciliation are unaffected.
pub(crate) fn prune_indexed_blocks_before<Tx: DbTxMut>(
    tx: &Tx,
    floor: u64,
) -> Result<(), ReferenceIndexError> {
    if floor == 0 {
        return Ok(());
    }

    let mut stale = Vec::new();
    {
        let mut cursor = tx.cursor_write::<IndexedBlocks>()?;
        let mut next = cursor.first()?;
        while let Some((key, _)) = next {
            if key.block_number >= floor {
                break;
            }
            stale.push(key);
            next = cursor.next()?;
        }
    }
    for key in stale {
        tx.delete::<IndexedBlocks>(key, None)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ReferenceIndexDb;
    use alloy_consensus::transaction::TxEip1559;
    use alloy_primitives::{Address, Signature, TxKind, U256};
    use morph_primitives::MorphTxEnvelope;
    use reth_db_api::transaction::DbTx;
    use tempfile::TempDir;

    fn unsigned_sig() -> Signature {
        Signature::new(U256::from(1u64), U256::from(1u64), false)
    }

    /// A MorphTx wrapper that returns `Some(reference)` when queried.  We
    /// don't have a simple TxMorph factory in this crate, so we fabricate
    /// a test envelope by using the Eip1559 variant and pairing the expected
    /// hash via the trait impl.
    ///
    /// For now we test write_block/delete_block indirectly by checking that
    /// `write_block` writes the `IndexedBlocks` row for blocks without any
    /// reference-carrying tx, and `delete_block` removes it.
    fn tx_without_reference() -> MorphTxEnvelope {
        let tx = TxEip1559 {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 21_000,
            max_fee_per_gas: 1,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(Address::ZERO),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Default::default(),
        };
        let signed = alloy_consensus::Signed::new_unchecked(tx, unsigned_sig(), B256::ZERO);
        MorphTxEnvelope::Eip1559(signed)
    }

    #[test]
    fn write_block_without_reference_tx_writes_indexed_blocks_row() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();

        let txs = vec![tx_without_reference()];
        let tx = db.tx_mut().unwrap();
        let written = write_block(&tx, 42, B256::repeat_byte(0xaa), 1_000, &txs).unwrap();
        update_indexed_to(&tx, 42).unwrap();
        tx.commit().unwrap();

        assert_eq!(written, 0);
        assert_eq!(db.indexed_to().unwrap(), Some(42));
        assert_eq!(
            db.indexed_block_hash(42).unwrap(),
            Some(B256::repeat_byte(0xaa))
        );
    }

    #[test]
    fn delete_block_removes_indexed_blocks_row() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();

        let tx = db.tx_mut().unwrap();
        write_block(&tx, 7, B256::repeat_byte(0x11), 100, &[]).unwrap();
        update_indexed_to(&tx, 7).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            db.indexed_block_hash(7).unwrap(),
            Some(B256::repeat_byte(0x11))
        );

        let tx = db.tx_mut().unwrap();
        delete_block(&tx, 7).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.indexed_block_hash(7).unwrap(), None);
    }

    #[test]
    fn cursor_and_jade_activation_are_visible_after_commit() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();

        let tx = db.tx_mut().unwrap();
        update_indexed_to(&tx, 200).unwrap();
        set_jade_first_block_number(&tx, 100).unwrap();
        tx.commit().unwrap();

        assert_eq!(db.indexed_to().unwrap(), Some(200));
        assert_eq!(db.jade_first_block_number().unwrap(), Some(100));
    }

    #[test]
    fn clearing_cursor_restores_no_progress_state() {
        let dir = TempDir::new().unwrap();
        let db = ReferenceIndexDb::open(dir.path(), 2818, B256::ZERO).unwrap();

        let tx = db.tx_mut().unwrap();
        update_indexed_to(&tx, 7).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.indexed_to().unwrap(), Some(7));

        let tx = db.tx_mut().unwrap();
        clear_indexed_to(&tx).unwrap();
        tx.commit().unwrap();
        assert_eq!(db.indexed_to().unwrap(), None);
    }
}
