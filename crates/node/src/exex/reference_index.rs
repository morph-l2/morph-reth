//! Reference index ExEx: drains canonical chain notifications and keeps the
//! index incrementally up to date.
//!
//! ## Lifecycle
//!
//! **Task A** (`run_startup_indexing`): runs in a spawned task, executes
//! backfill → reconcile, then sets `is_ready = true` and signals the ExEx with
//! `FinishedHeight(indexed_to)`.
//!
//! **Task B** (`reference_index_exex`): registered via `install_exex` and
//! started by reth's framework at node launch.  It drains all notifications
//! immediately to avoid backpressure; writes are gated behind `is_ready`.

use alloy_consensus::BlockHeader;
use alloy_eips::BlockNumHash;
use futures::FutureExt;
use morph_chainspec::spec::MorphChainSpec;
use morph_primitives::MorphPrimitives;
use morph_reference_index::{
    DEFAULT_BACKFILL_BATCH_BLOCKS, DEFAULT_MAX_REORG_DEPTH, ReferenceIndexDb,
    backfill::{maybe_reset_jade_sentinel, run_backfill},
    reconcile::run_startup_reconcile,
    writer::{delete_block, update_indexed_to, write_block},
};
use reth_db_api::transaction::{DbTx, DbTxMut};
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, ChainSpecProvider, HeaderProvider,
};
use reth_storage_api::TransactionVariant;
use tokio::sync::watch;
use tokio_stream::{Stream, StreamExt};
use tracing::{debug, error, info};

const TARGET: &str = "morph::reference_index";
const MAX_LIVE_NOTIFICATION_BATCH: usize = 1024;

// ── shared control ────────────────────────────────────────────────────────────

/// Shared handle that connects Task A (startup indexing) with Task B (ExEx).
///
/// Task A completes, sets `is_ready`, then sends the startup
/// `FinishedHeight` through the watch channel.  Task B receives this and
/// forwards it to reth's ExEx event bus.
#[derive(Clone, Debug)]
pub struct ReferenceIndexControl {
    pub db: ReferenceIndexDb,
    startup_tx: watch::Sender<Option<BlockNumHash>>,
}

impl ReferenceIndexControl {
    /// Create a new control pair.
    ///
    /// Returns `(control, receiver)`.  The receiver must be passed to
    /// [`reference_index_exex`] so the ExEx knows when startup has finished.
    pub fn new(db: ReferenceIndexDb) -> (Self, watch::Receiver<Option<BlockNumHash>>) {
        let (startup_tx, startup_rx) = watch::channel(None);
        (Self { db, startup_tx }, startup_rx)
    }

    /// Called by Task A after backfill + reconcile complete.
    pub fn mark_startup_finished(&self, block: BlockNumHash) -> eyre::Result<()> {
        self.startup_tx.send(Some(block))?;
        Ok(())
    }
}

// ── Task B: ExEx ──────────────────────────────────────────────────────────────

/// Main ExEx loop.
///
/// Drains notifications from node launch to avoid backpressure.  While
/// `is_ready = false` each notification is discarded.  After `is_ready`
/// the first notification triggers a gap check before normal processing.
pub async fn reference_index_exex<Node>(
    mut ctx: ExExContext<Node>,
    control: ReferenceIndexControl,
    mut startup_rx: watch::Receiver<Option<BlockNumHash>>,
) -> eyre::Result<()>
where
    Node: FullNodeComponents<
        Types: NodeTypes<Primitives = MorphPrimitives, ChainSpec = MorphChainSpec>,
    >,
    Node::Provider: BlockReader<Block = morph_primitives::Block>
        + BlockNumReader
        + HeaderProvider<Header = morph_primitives::MorphHeader>
        + BlockHashReader,
{
    let mut first_ready = true;
    // Track last sent FinishedHeight to keep progress monotonic.  Without this,
    // the `tokio::select!` arms can race: a live commit may send `FinishedHeight(H+k)`
    // before the startup watch channel is observed, which would otherwise
    // regress reth's pruning marker back to `H`.
    let mut last_finished: Option<BlockNumHash> = None;

    loop {
        tokio::select! {
            // Forward the startup FinishedHeight when Task A finishes.
            changed = startup_rx.changed() => {
                if changed.is_ok() && let Some(block) = *startup_rx.borrow_and_update() {
                    debug!(
                        target: TARGET,
                        block_number = block.number,
                        "startup complete; forwarding initial FinishedHeight"
                    );
                    send_finished_height_monotonic(&ctx.events, block, &mut last_finished)?;
                }
            }

            maybe_notification = ctx.notifications.try_next() => {
                let Some(notification) = maybe_notification? else { break; };

                if !control.db.is_ready() {
                    // Drain without writing to avoid backpressure.
                    if let Some(chain) = notification.committed_chain() {
                        debug!(
                            target: TARGET,
                            tip = chain.tip().number(),
                            "drained notification while index initializing"
                        );
                    }
                    continue;
                }

                // On the first is_ready notification: fill any gap that opened
                // between when Task A finished reconcile and when this notification
                // arrived.  The goal is that after this branch runs, the index
                // covers exactly the canonical range that is not going to be
                // touched by the upcoming handle_notification call.
                //
                // fill_gap_idempotent delete-then-writes so stale entries from
                // possible mini-reorgs during the drain window are cleaned up.
                if first_ready {
                    first_ready = false;
                    let indexed_to = control.db.indexed_to()?;
                    if let Some(chain) = notification.committed_chain() {
                        // ChainCommitted / ChainReorged: blocks to index start at
                        // chain.first().  Anything in (indexed_to, notif_start-1]
                        // must be backfilled from main DB so handle_notification
                        // can pick up from notif_start.
                        let notif_start = chain.first().number();
                        if notif_start > indexed_to + 1 {
                            fill_gap_idempotent(
                                &control.db,
                                &ctx.components.provider().clone(),
                                indexed_to + 1,
                                notif_start - 1,
                            )?;
                        }
                    } else if let Some(old) = notification.reverted_chain() {
                        // ChainReverted: handle_notification will delete old.blocks
                        // and set indexed_to = parent (= revert_start - 1).
                        // If parent > indexed_to, the drain window committed and
                        // never wrote canonical blocks (indexed_to+1..=parent) that
                        // will SURVIVE this revert.  We must fill them now so the
                        // post-revert indexed_to = parent is consistent.
                        let revert_start = old.first().number();
                        let parent = revert_start.saturating_sub(1);
                        if parent > indexed_to {
                            fill_gap_idempotent(
                                &control.db,
                                &ctx.components.provider().clone(),
                                indexed_to + 1,
                                parent,
                            )?;
                        }
                        // parent <= indexed_to: revert overlaps already-indexed
                        // range; handle_notification's delete_block + rollback of
                        // indexed_to takes care of it, no backfill needed.
                    }
                }

                let notifications = drain_ready_notifications(&mut ctx.notifications, notification)?;
                match handle_notifications_batch(&ctx.events, &control.db, notifications, &mut last_finished) {
                    Ok(()) => {}
                    Err(e) => {
                        error!(target: TARGET, ?e, "error processing notification");
                        return Err(e);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Fill (or repair) index entries for blocks `[from, to]` using canonical main DB data.
///
/// Uses delete-then-write per block to stay idempotent even if some entries
/// already exist (e.g. partial prior write or mini-reorg during drain window).
fn fill_gap_idempotent<Provider>(
    db: &ReferenceIndexDb,
    provider: &Provider,
    from: u64,
    to: u64,
) -> eyre::Result<()>
where
    Provider: BlockReader<Block = morph_primitives::Block>,
{
    if from > to {
        return Ok(());
    }
    info!(
        target: TARGET,
        from, to,
        "idempotent gap fill between startup reconcile and first ExEx notification"
    );
    let tx = db.tx_mut()?;
    for number in from..=to {
        // Delete any stale entries before writing canonical ones.
        delete_block(&tx, number)?;
        // `WithHash` is required: write_block stores tx hashes in the index keys.
        let block = provider
            .sealed_block_with_senders(number.into(), TransactionVariant::WithHash)?
            .ok_or_else(|| eyre::eyre!("missing block {number} during gap fill"))?;
        write_block(
            &tx,
            block.number(),
            block.hash(),
            block.timestamp(),
            &block.body().transactions,
        )?;
    }
    update_indexed_to(&tx, to)?;
    tx.commit()?;
    Ok(())
}

/// Drain already-ready notifications so fast catch-up can be indexed in one DB transaction.
fn drain_ready_notifications<S>(
    notifications: &mut S,
    first: ExExNotification<MorphPrimitives>,
) -> eyre::Result<Vec<ExExNotification<MorphPrimitives>>>
where
    S: Stream<Item = eyre::Result<ExExNotification<MorphPrimitives>>> + Unpin,
{
    let mut batch = Vec::with_capacity(32);
    batch.push(first);

    while batch.len() < MAX_LIVE_NOTIFICATION_BATCH {
        match notifications.try_next().now_or_never() {
            Some(Ok(Some(notification))) => batch.push(notification),
            Some(Ok(None)) | None => break,
            Some(Err(err)) => return Err(err),
        }
    }

    Ok(batch)
}

/// Process ready ExEx notifications in one atomic commit.
fn handle_notifications_batch(
    events: &tokio::sync::mpsc::UnboundedSender<ExExEvent>,
    db: &ReferenceIndexDb,
    notifications: Vec<ExExNotification<MorphPrimitives>>,
    last_finished: &mut Option<BlockNumHash>,
) -> eyre::Result<()> {
    if notifications.is_empty() {
        return Ok(());
    }

    let tx = db.tx_mut()?;
    let mut finished_height = None;
    let mut allow_regression = false;
    for notification in notifications {
        let effect = apply_notification_to_tx(&tx, notification)?;
        allow_regression |= effect.allow_regression;
        finished_height = effect.finished_height;
    }
    tx.commit()?;

    if let Some(block) = finished_height {
        if allow_regression {
            events.send(ExExEvent::FinishedHeight(block))?;
            *last_finished = Some(block);
        } else {
            send_finished_height_monotonic(events, block, last_finished)?;
        }
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NotificationEffect {
    finished_height: Option<BlockNumHash>,
    allow_regression: bool,
}

fn apply_notification_to_tx<Tx: DbTxMut>(
    tx: &Tx,
    notification: ExExNotification<MorphPrimitives>,
) -> eyre::Result<NotificationEffect> {
    match notification {
        ExExNotification::ChainCommitted { new } => {
            for block in new.blocks_iter() {
                write_block(
                    tx,
                    block.number(),
                    block.hash(),
                    block.timestamp(),
                    &block.body().transactions,
                )?;
            }
            update_indexed_to(tx, new.tip().number())?;
            Ok(NotificationEffect {
                finished_height: Some(new.tip().num_hash()),
                allow_regression: false,
            })
        }
        ExExNotification::ChainReverted { old } => {
            let parent = old.first().number().saturating_sub(1);
            for block in old.blocks_iter() {
                delete_block(tx, block.number())?;
            }
            update_indexed_to(tx, parent)?;
            // FinishedHeight not sent on revert per spec.
            Ok(NotificationEffect {
                finished_height: None,
                allow_regression: true,
            })
        }
        ExExNotification::ChainReorged { old, new } => {
            for block in old.blocks_iter() {
                delete_block(tx, block.number())?;
            }
            for block in new.blocks_iter() {
                write_block(
                    tx,
                    block.number(),
                    block.hash(),
                    block.timestamp(),
                    &block.body().transactions,
                )?;
            }
            update_indexed_to(tx, new.tip().number())?;
            // On reorg to a shorter chain the tip may go down; that is the one
            // case we ALLOW FinishedHeight to regress (reth's ExExEvent doc
            // explicitly permits "on reorgs, height may go down").
            Ok(NotificationEffect {
                finished_height: Some(new.tip().num_hash()),
                allow_regression: true,
            })
        }
    }
}

/// Send a FinishedHeight only if it strictly advances `last_finished`.
///
/// Used for ChainCommitted and the startup watch channel, where progress
/// must only go forward.  ChainReorged has its own allow-regress path.
fn send_finished_height_monotonic(
    events: &tokio::sync::mpsc::UnboundedSender<ExExEvent>,
    block: BlockNumHash,
    last_finished: &mut Option<BlockNumHash>,
) -> eyre::Result<()> {
    if last_finished.is_none_or(|last| block.number > last.number) {
        events.send(ExExEvent::FinishedHeight(block))?;
        *last_finished = Some(block);
    }
    Ok(())
}

// ── Task A: startup indexing ──────────────────────────────────────────────────

/// Execute backfill → reconcile, set `is_ready = true`, then send the startup
/// `FinishedHeight` through `control`.
///
/// Call once from a spawned task after the node's provider is available.
pub fn run_startup_indexing<Node>(node: &Node, control: &ReferenceIndexControl) -> eyre::Result<()>
where
    Node: FullNodeComponents<
        Types: NodeTypes<Primitives = MorphPrimitives, ChainSpec = MorphChainSpec>,
    >,
    Node::Provider: BlockReader<Block = morph_primitives::Block>
        + BlockNumReader
        + HeaderProvider<Header = morph_primitives::MorphHeader>
        + BlockHashReader
        + ChainSpecProvider<ChainSpec = MorphChainSpec>,
{
    let provider = node.provider().clone();
    let chain_spec = provider.chain_spec();
    let head = provider.best_block_number()?;

    // Re-resolve jade sentinel if Jade has since activated.
    maybe_reset_jade_sentinel(&control.db, &provider, chain_spec.as_ref(), head)?;

    // Run backfill (no-op if already Complete).
    run_backfill(
        &control.db,
        &provider,
        chain_spec.as_ref(),
        head,
        DEFAULT_BACKFILL_BATCH_BLOCKS,
    )?;

    // Re-read head in case new blocks arrived during backfill.
    let current_head = provider.best_block_number()?;

    // Startup reconcile: canonical hash check + suffix gap.
    run_startup_reconcile(
        &control.db,
        &provider,
        current_head,
        DEFAULT_MAX_REORG_DEPTH,
    )?;

    // Atomically mark ready and signal the ExEx.
    control.db.set_ready(true);

    let indexed_to = control.db.indexed_to()?;
    let hash = provider
        .block_hash(indexed_to)?
        .ok_or_else(|| eyre::eyre!("missing canonical hash for block {indexed_to}"))?;

    control.mark_startup_finished(BlockNumHash {
        number: indexed_to,
        hash,
    })?;

    info!(
        target: TARGET,
        indexed_to,
        "reference index ready"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use morph_primitives::{Block, BlockBody, MorphHeader, MorphPrimitives};
    use reth_primitives_traits::SealedBlock;
    use reth_provider::{Chain, ExecutionOutcome};
    use std::{collections::BTreeMap, sync::Arc};

    #[test]
    fn batched_commits_emit_finished_height_once_at_tip() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = ReferenceIndexDb::open(dir.path(), 2910, B256::ZERO)?;
        let (events, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut last_finished = None;

        let block1 = recovered_block(1, B256::ZERO);
        let block1_hash = block1.hash();
        let block2 = recovered_block(2, block1_hash);
        let block2_num_hash = block2.num_hash();

        handle_notifications_batch(
            &events,
            &db,
            vec![
                committed_notification(vec![block1]),
                committed_notification(vec![block2]),
            ],
            &mut last_finished,
        )?;

        assert_eq!(db.indexed_to()?, 2);
        assert!(db.indexed_block_hash(1)?.is_some());
        assert!(db.indexed_block_hash(2)?.is_some());
        assert_eq!(last_finished, Some(block2_num_hash));
        assert_eq!(
            events_rx.try_recv()?,
            ExExEvent::FinishedHeight(block2_num_hash)
        );
        assert!(events_rx.try_recv().is_err());

        Ok(())
    }

    #[test]
    fn batch_with_reorg_allows_final_finished_height_to_regress() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let db = ReferenceIndexDb::open(dir.path(), 2910, B256::ZERO)?;
        let (events, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut last_finished = Some(BlockNumHash {
            number: 12,
            hash: B256::repeat_byte(0x12),
        });

        let old10 = recovered_block(10, B256::ZERO);
        let old11 = recovered_block(11, old10.hash());
        let old12 = recovered_block(12, old11.hash());
        let new10 = recovered_block(10, B256::repeat_byte(0x99));
        let new11 = recovered_block(11, new10.hash());
        let new11_num_hash = new11.num_hash();

        handle_notifications_batch(
            &events,
            &db,
            vec![
                reorg_notification(vec![old10, old11, old12], vec![new10]),
                committed_notification(vec![new11]),
            ],
            &mut last_finished,
        )?;

        assert_eq!(db.indexed_to()?, 11);
        assert_eq!(last_finished, Some(new11_num_hash));
        assert_eq!(
            events_rx.try_recv()?,
            ExExEvent::FinishedHeight(new11_num_hash)
        );
        assert!(events_rx.try_recv().is_err());

        Ok(())
    }

    fn committed_notification(
        blocks: Vec<reth_primitives_traits::RecoveredBlock<Block>>,
    ) -> ExExNotification<MorphPrimitives> {
        ExExNotification::ChainCommitted {
            new: Arc::new(Chain::new(
                blocks,
                ExecutionOutcome::default(),
                BTreeMap::new(),
            )),
        }
    }

    fn reorg_notification(
        old: Vec<reth_primitives_traits::RecoveredBlock<Block>>,
        new: Vec<reth_primitives_traits::RecoveredBlock<Block>>,
    ) -> ExExNotification<MorphPrimitives> {
        ExExNotification::ChainReorged {
            old: Arc::new(Chain::new(
                old,
                ExecutionOutcome::default(),
                BTreeMap::new(),
            )),
            new: Arc::new(Chain::new(
                new,
                ExecutionOutcome::default(),
                BTreeMap::new(),
            )),
        }
    }

    fn recovered_block(
        number: u64,
        parent_hash: B256,
    ) -> reth_primitives_traits::RecoveredBlock<Block> {
        let header = MorphHeader {
            inner: Header {
                number,
                parent_hash,
                timestamp: 1_000 + number,
                gas_limit: 30_000_000,
                parent_beacon_block_root: Some(B256::ZERO),
                ..Default::default()
            },
            next_l1_msg_index: 0,
            timestamp_millis_part: None,
        };
        let body = BlockBody {
            transactions: vec![],
            ommers: vec![],
            withdrawals: None,
        };
        SealedBlock::seal_slow(Block { header, body })
            .try_recover()
            .expect("empty test block should recover")
    }
}
