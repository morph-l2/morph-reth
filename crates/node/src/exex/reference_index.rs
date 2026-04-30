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
use morph_chainspec::spec::MorphChainSpec;
use morph_primitives::MorphPrimitives;
use morph_reference_index::{
    DEFAULT_BACKFILL_BATCH_BLOCKS, DEFAULT_MAX_REORG_DEPTH, ReferenceIndexDb,
    backfill::{maybe_reset_jade_sentinel, run_backfill},
    reconcile::run_startup_reconcile,
    writer::{delete_block, update_indexed_to, write_block},
};
use reth_db_api::transaction::DbTx;
use reth_exex::{ExExContext, ExExEvent, ExExNotification};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, ChainSpecProvider, HeaderProvider,
};
use reth_storage_api::TransactionVariant;
use tokio::sync::watch;
use tokio_stream::StreamExt;
use tracing::{debug, error, info};

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

    loop {
        tokio::select! {
            // Forward the startup FinishedHeight when Task A finishes.
            changed = startup_rx.changed() => {
                if changed.is_ok() && let Some(block) = *startup_rx.borrow_and_update() {
                    debug!(
                        target: "morph::reference_index",
                        block_number = block.number,
                        "startup complete; sending initial FinishedHeight"
                    );
                    ctx.events.send(ExExEvent::FinishedHeight(block))?;
                }
            }

            maybe_notification = ctx.notifications.try_next() => {
                let Some(notification) = maybe_notification? else { break; };

                if !control.db.is_ready() {
                    // Drain without writing to avoid backpressure.
                    if let Some(chain) = notification.committed_chain() {
                        debug!(
                            target: "morph::reference_index",
                            tip = chain.tip().number(),
                            "drained notification while index initializing"
                        );
                    }
                    continue;
                }

                // On the first is_ready notification: fill any gap that opened
                // between when Task A finished reconcile and when this notification
                // arrived.  We delete-then-write to avoid stale entries in case a
                // mini-reorg happened during that window.
                if first_ready {
                    first_ready = false;
                    let indexed_to = control.db.indexed_to()?;
                    if let Some(chain) = notification.committed_chain() {
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
                        // Reorg during drain window: roll back below the revert start.
                        let revert_start = old.first().number();
                        if revert_start <= indexed_to {
                            fill_gap_idempotent(
                                &control.db,
                                &ctx.components.provider().clone(),
                                revert_start,
                                indexed_to,
                            )?;
                        }
                    }
                }

                if let Err(e) =
                    handle_notification(&ctx.events, &control.db, notification)
                {
                    error!(target: "morph::reference_index", ?e, "error processing notification");
                    return Err(e);
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
        target: "morph::reference_index",
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

/// Process one ExEx notification: commit or revert three tables atomically.
fn handle_notification(
    events: &tokio::sync::mpsc::UnboundedSender<ExExEvent>,
    db: &ReferenceIndexDb,
    notification: ExExNotification<MorphPrimitives>,
) -> eyre::Result<()> {
    match notification {
        ExExNotification::ChainCommitted { new } => {
            let tx = db.tx_mut()?;
            for block in new.blocks_iter() {
                write_block(
                    &tx,
                    block.number(),
                    block.hash(),
                    block.timestamp(),
                    &block.body().transactions,
                )?;
            }
            update_indexed_to(&tx, new.tip().number())?;
            tx.commit()?;
            events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
        }
        ExExNotification::ChainReverted { old } => {
            let parent = old.first().number().saturating_sub(1);
            let tx = db.tx_mut()?;
            for block in old.blocks_iter() {
                delete_block(&tx, block.number())?;
            }
            update_indexed_to(&tx, parent)?;
            tx.commit()?;
            // FinishedHeight not sent on revert per spec.
        }
        ExExNotification::ChainReorged { old, new } => {
            let tx = db.tx_mut()?;
            for block in old.blocks_iter() {
                delete_block(&tx, block.number())?;
            }
            for block in new.blocks_iter() {
                write_block(
                    &tx,
                    block.number(),
                    block.hash(),
                    block.timestamp(),
                    &block.body().transactions,
                )?;
            }
            update_indexed_to(&tx, new.tip().number())?;
            tx.commit()?;
            events.send(ExExEvent::FinishedHeight(new.tip().num_hash()))?;
        }
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
        target: "morph::reference_index",
        indexed_to,
        "reference index ready"
    );

    Ok(())
}
