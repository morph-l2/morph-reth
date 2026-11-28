//! Forward-only proof-history execution extension for Morph.
//!
//! Derived from an MIT-licensed upstream implementation.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

mod sync_target;
use std::{sync::Arc, time::Duration};

use alloy_consensus::BlockHeader;
use alloy_eips::{NumHash, eip1898::BlockWithParent};
use futures::TryStreamExt;
use morph_proofs::{
    DEFAULT_PROOFS_HISTORY_WINDOW, DEFAULT_PRUNE_BATCH_SIZE, MorphProofStoragePruner,
    MorphProofsBatchStore, MorphProofsStorage, MorphProofsStorageError,
    live::{BatchBlock, LiveTrieCollector},
    metrics::ProofHistoryMetrics,
    persisted_canonical_head,
    proof::state_root_at,
};
use reth_execution_types::Chain;
use reth_exex::{ExExContext, ExExEvent, ExExNotification, ExExNotificationsStream};
use reth_node_api::{FullNodeComponents, NodePrimitives, NodeTypes};
use reth_provider::{
    BlockHashReader, BlockNumReader, BlockReader, HeaderProvider, TransactionVariant,
};
pub use sync_target::{CachedBlockTrieData, SyncTarget, SyncTargetState};
use tokio::task;
use tracing::{debug, error, info, warn};

/// Default safety threshold for the gap between stored earliest block and the configured
/// window target. When exceeded on startup, the node refuses to auto-prune and asks the
/// operator to run `proofs prune` manually. The threshold is a backstop against accidental
/// misconfiguration (e.g. shrinking the proofs window by orders of magnitude) — it is not a
/// measure of pruner throughput. Override via
/// [`MorphProofsExExBuilder::with_max_prune_blocks_startup`].
const DEFAULT_MAX_PRUNE_BLOCKS_STARTUP: u64 = 1_000;

/// How many blocks to process in a single sync turn before yielding.
const SYNC_BLOCKS_PER_TURN: usize = 50;

/// Backoff after a failed forward-sync turn. The work is re-armed so the loop
/// retries instead of waiting indefinitely for the next notification.
const SYNC_RETRY_DELAY: Duration = Duration::from_secs(1);

/// Default interval between proof-storage prune runs. Default is 15 seconds.
const DEFAULT_PRUNE_INTERVAL: Duration = Duration::from_secs(15);

/// Default verification interval: disabled
const DEFAULT_VERIFICATION_INTERVAL: u64 = 0; // disabled

/// Builder for [`MorphProofsExEx`].
#[derive(Debug)]
pub struct MorphProofsExExBuilder<Node, Storage>
where
    Node: FullNodeComponents,
{
    ctx: ExExContext<Node>,
    storage: MorphProofsStorage<Storage>,
    proofs_history_window: u64,
    proofs_history_prune_interval: Duration,
    verification_interval: u64,
    max_prune_blocks_startup: u64,
}

impl<Node, Storage> MorphProofsExExBuilder<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Create a new builder with required parameters and defaults.
    pub const fn new(ctx: ExExContext<Node>, storage: MorphProofsStorage<Storage>) -> Self {
        Self {
            ctx,
            storage,
            proofs_history_window: DEFAULT_PROOFS_HISTORY_WINDOW,
            proofs_history_prune_interval: DEFAULT_PRUNE_INTERVAL,
            verification_interval: DEFAULT_VERIFICATION_INTERVAL,
            max_prune_blocks_startup: DEFAULT_MAX_PRUNE_BLOCKS_STARTUP,
        }
    }

    /// Sets the window to span blocks for proofs history.
    pub const fn with_proofs_history_window(mut self, window: u64) -> Self {
        self.proofs_history_window = window;
        self
    }

    /// Sets the interval between proof-storage prune runs.
    pub const fn with_proofs_history_prune_interval(mut self, interval: Duration) -> Self {
        self.proofs_history_prune_interval = interval;
        self
    }

    /// Sets the verification interval.
    pub const fn with_verification_interval(mut self, interval: u64) -> Self {
        self.verification_interval = interval;
        self
    }

    /// Sets the safety threshold for blocks that may be auto-pruned at startup.
    /// See `DEFAULT_MAX_PRUNE_BLOCKS_STARTUP`.
    pub const fn with_max_prune_blocks_startup(mut self, max_blocks: u64) -> Self {
        self.max_prune_blocks_startup = max_blocks;
        self
    }

    /// Builds the [`MorphProofsExEx`].
    pub fn build(self) -> MorphProofsExEx<Node, Storage> {
        MorphProofsExEx {
            ctx: self.ctx,
            storage: self.storage,
            proofs_history_window: self.proofs_history_window,
            proofs_history_prune_interval: self.proofs_history_prune_interval,
            verification_interval: self.verification_interval,
            max_prune_blocks_startup: self.max_prune_blocks_startup,
        }
    }
}

/// Proofs `ExEx` - processes blocks and tracks state changes within the proof-history window.
///
/// Saves trie nodes to make proofs faster. This handles the process of saving new
/// blocks as they're added on top of the state written by `morph-reth proofs init`;
/// the proof RPC overrides are served from the saved data.
///
#[derive(Debug)]
pub struct MorphProofsExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// The `ExEx` context containing the node related utilities e.g. provider, notifications,
    /// events.
    ctx: ExExContext<Node>,
    /// The type of storage DB.
    storage: MorphProofsStorage<Storage>,
    /// The window to span blocks for proofs history. Value is the number of blocks, received as
    /// cli arg.
    proofs_history_window: u64,
    /// Interval between proof-storage prune runs
    proofs_history_prune_interval: Duration,
    /// Verification interval: perform full block execution every N blocks for data integrity.
    /// If 0, verification is disabled (always use fast path when available).
    /// If 1, verification is always enabled (always execute blocks).
    verification_interval: u64,
    /// Maximum blocks the startup check is willing to schedule for auto-prune. See
    /// [`DEFAULT_MAX_PRUNE_BLOCKS_STARTUP`].
    max_prune_blocks_startup: u64,
}

impl<Node, Storage> MorphProofsExEx<Node, Storage>
where
    Node: FullNodeComponents,
{
    /// Create a new `MorphProofsExEx` instance.
    pub fn new(ctx: ExExContext<Node>, storage: MorphProofsStorage<Storage>) -> Self {
        MorphProofsExExBuilder::new(ctx, storage).build()
    }

    /// Create a new builder for `MorphProofsExEx`.
    pub const fn builder(
        ctx: ExExContext<Node>,
        storage: MorphProofsStorage<Storage>,
    ) -> MorphProofsExExBuilder<Node, Storage> {
        MorphProofsExExBuilder::new(ctx, storage)
    }
}

/// Outcome of attempting to unwind proof history for a reorg or revert notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevertOutcome {
    /// The revert landed cleanly and proof storage is anchored on the reverted parent.
    Completed,
    /// The revert target was beyond the stored tip, so no unwind occurred and the existing sync
    /// health state was left untouched.
    Skipped,
    /// A retryable I/O or lock error prevented the revert; caller must requeue and back off.
    TransientError,
    /// A structural error (e.g. `UnwindBeyondEarliest`, `BlockHashMismatch`) prevents applying
    /// this revert without a deeper reorg or manual re-initialization.
    StructuralError,
}

impl<Node, Storage, Primitives> MorphProofsExEx<Node, Storage>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = Primitives>>,
    Primitives: NodePrimitives,
    Storage: MorphProofsBatchStore + Clone + 'static,
{
    /// Main execution loop for the `ExEx`
    pub async fn run(mut self) -> eyre::Result<()> {
        self.ensure_initialized()?;
        let sync_healthy = self.reconcile_on_startup()?;
        ProofHistoryMetrics::set_sync_healthy(sync_healthy);
        let sync_target = self.spawn_sync_task(sync_healthy);

        // If storage is healthy and behind tip, start syncing immediately rather than waiting
        // for the first notification.
        if sync_healthy {
            let best_block = self.ctx.provider().best_block_number()?;
            let latest_stored = self
                .storage
                .get_latest_block_number()?
                .map(|(n, _)| n)
                .unwrap_or(0);
            if latest_stored < best_block {
                info!(
                    target: "morph::proofs_exex",
                    latest_stored,
                    best_block,
                    "Storage behind tip, starting sync immediately"
                );
                sync_target.update_state(SyncTargetState::SyncUpTo { to: best_block });
            }
        }

        self.ctx.notifications.set_without_head();

        while let Some(notification) = self.ctx.notifications.try_next().await? {
            self.handle_notification(notification, &sync_target)?;
        }

        Ok(())
    }

    /// Reconciles stored proof history against Reth's canonical chain on startup.
    ///
    /// Returns `Ok(true)` if the proof DB is anchored on the canonical chain (unwinding any
    /// ahead-of-tip or off-fork suffix to a verified common ancestor when possible), or
    /// `Ok(false)` when no canonical anchor exists in the retained window (in which case proof
    /// sync starts unhealthy and proof RPCs / periodic pruning stay paused instead of crashing
    /// the node).
    fn reconcile_on_startup(&self) -> eyre::Result<bool> {
        let Some((earliest_num, earliest_hash)) = self.storage.get_earliest_block_number()? else {
            return Ok(false);
        };
        let Some((latest_num, latest_hash)) = self.storage.get_latest_block_number()? else {
            return Ok(false);
        };

        let provider = self.ctx.provider();
        let best_block = provider.best_block_number()?;

        if latest_num <= best_block && provider.block_hash(latest_num)? == Some(latest_hash) {
            ProofHistoryMetrics::set_window(earliest_num, latest_num);
            return Ok(true);
        }

        if earliest_num > best_block || provider.block_hash(earliest_num)? != Some(earliest_hash) {
            warn!(
                target: "morph::proofs_exex",
                earliest_num,
                ?earliest_hash,
                latest_num,
                ?latest_hash,
                best_block,
                "Proof storage shares no canonical anchor with the chain; proof sync stays unhealthy until a reorg or manual re-initialization recovers it"
            );
            return Ok(false);
        }

        let canonical_match = |number: u64| -> eyre::Result<Option<NumHash>> {
            if let Some(stored_hash) = self.storage.get_block_hash(number)? {
                return Ok((provider.block_hash(number)? == Some(stored_hash))
                    .then(|| NumHash::new(number, stored_hash)));
            }
            // Legacy upgraded proof DB lacking a `BlockHashes` row for `number`: fall back to
            // checking the stored state root against the canonical header.
            let Some(header) = provider.header_by_number(number)? else {
                return Ok(None);
            };
            let Some(canonical_hash) = provider.block_hash(number)? else {
                return Ok(None);
            };
            let stored_state_root = state_root_at(&self.storage, number)?;
            Ok((stored_state_root == header.state_root())
                .then(|| NumHash::new(number, canonical_hash)))
        };

        let highest_candidate = best_block.min(latest_num.saturating_sub(1));
        let mut common = NumHash::new(earliest_num, earliest_hash);
        if highest_candidate > earliest_num {
            if let Some(found) = canonical_match(highest_candidate)? {
                common = found;
            } else {
                let mut mismatch = highest_candidate;
                while mismatch - common.number > 1 {
                    let mid = common.number + (mismatch - common.number) / 2;
                    match canonical_match(mid)? {
                        Some(found) => common = found,
                        None => mismatch = mid,
                    }
                }
            }
        }

        if common.number < latest_num {
            info!(
                target: "morph::proofs_exex",
                latest_num,
                ?latest_hash,
                best_block,
                unwind_to = common.number,
                unwind_to_hash = ?common.hash,
                "Proof storage is ahead of or diverged from canonical chain on startup; unwinding to common ancestor"
            );
            let unwind_from = BlockWithParent::new(
                common.hash,
                NumHash::new(common.number.saturating_add(1), Default::default()),
            );
            self.storage.unwind_history(unwind_from)?;
            ProofHistoryMetrics::set_window(earliest_num, common.number);
        }

        Ok(true)
    }

    /// Ensure proofs storage is initialized
    fn ensure_initialized(&self) -> eyre::Result<()> {
        // Check if proofs storage is initialized
        let earliest_block_number = match self.storage.get_earliest_block_number()? {
            Some((n, _)) => n,
            None => {
                return Err(eyre::eyre!(
                    "Proofs storage not initialized. Please run 'morph-reth proofs init --proofs-history.storage-path <PATH>' first."
                ));
            }
        };

        let latest_block_number = match self.storage.get_latest_block_number()? {
            Some((n, _)) => n,
            None => {
                return Err(eyre::eyre!(
                    "Proofs storage not initialized. Please run 'morph-reth proofs init --proofs-history.storage-path <PATH>' first."
                ));
            }
        };

        // Check if we have accumulated too much history for the configured window.
        // If the gap between what we have and what we want to keep is too large, the auto-pruner
        // will stall the node.
        let target_earliest = latest_block_number.saturating_sub(self.proofs_history_window);
        if target_earliest > earliest_block_number {
            let blocks_to_prune = target_earliest - earliest_block_number;
            if blocks_to_prune > self.max_prune_blocks_startup {
                return Err(eyre::eyre!(
                    "Configuration requires pruning {} blocks, which exceeds the safety threshold of {}. \
                     Huge prune operations can stall the node. \
                     Please run 'morph-reth proofs prune' manually before starting the node.",
                    blocks_to_prune,
                    self.max_prune_blocks_startup
                ));
            }
        }

        // Proof storage and Reth have independent commit boundaries, so the proof tip can be
        // ahead after an ungraceful shutdown. Let forward sync retain those blocks and resume once
        // the canonical chain passes the stored tip. Proof RPCs independently validate the
        // canonical anchor before serving data.
        ProofHistoryMetrics::set_window(earliest_block_number, latest_block_number);

        Ok(())
    }

    /// Spawn the background sync task and return the shared [`SyncTarget`].
    ///
    /// The sync target buffers trie data from notifications so the sync loop
    /// can use pre-computed data for blocks even when it is many blocks behind
    /// the chain tip. Blocks whose trie data was evicted from the cache fall
    /// back to full execution. Periodic pruning is driven sequentially from the
    /// same loop (single-writer model) so pruning can never race with reorgs or
    /// unwinds.
    fn spawn_sync_task(&self, initial_sync_healthy: bool) -> Arc<SyncTarget> {
        let sync_target = Arc::new(SyncTarget::new());
        let task_sync_target = Arc::clone(&sync_target);

        let task_storage = self.storage.clone();
        let task_provider = self.ctx.provider().clone();
        let task_evm_config = self.ctx.evm_config().clone();
        let verification_interval = self.verification_interval;
        let proofs_history_window = self.proofs_history_window;
        let proofs_history_prune_interval = self.proofs_history_prune_interval;

        // A blocking task, not a default one: each turn holds an MDBX write transaction for up to
        // `SYNC_BLOCKS_PER_TURN` blocks and may execute those blocks in full, which would otherwise
        // occupy an async worker thread that the Engine API and RPC share.
        self.ctx.task_executor().spawn_critical_blocking_task(
            "morph::proofs_exex::proofs_storage_sync_loop",
            async move {
                let storage = task_storage.clone();
                let task_collector =
                    LiveTrieCollector::new(task_evm_config, task_provider.clone(), &storage);
                let pruner = MorphProofStoragePruner::new(
                    task_storage.clone(),
                    task_provider.clone(),
                    proofs_history_window,
                    DEFAULT_PRUNE_BATCH_SIZE,
                );
                Self::sync_loop(
                    task_sync_target,
                    task_storage,
                    task_provider,
                    &task_collector,
                    &pruner,
                    verification_interval,
                    proofs_history_prune_interval,
                    initial_sync_healthy,
                )
                .await;
            },
        );

        sync_target
    }

    /// Drains pending sync states and drives periodic pruning sequentially for the lifetime of the
    /// node.
    ///
    /// Proof history is a best-effort side index: no failure propagates out of this loop. A failed
    /// turn is classified for observability; transient revert and forward-sync failures are
    /// re-armed after a backoff, and structural failures are abandoned until later notifications
    /// supply recovery work. Periodic pruning runs only when idle (no pending revert/sync state)
    /// and healthy (`sync_healthy == true`), capped at Reth's persisted canonical head.
    #[allow(clippy::too_many_arguments)]
    async fn sync_loop(
        sync_target: Arc<SyncTarget>,
        storage: MorphProofsStorage<Storage>,
        provider: Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        pruner: &MorphProofStoragePruner<Storage, Node::Provider>,
        verification_interval: u64,
        prune_interval: Duration,
        initial_sync_healthy: bool,
    ) {
        info!(target: "morph::proofs_exex", "Starting proofs storage sync loop");
        let mut sync_healthy = initial_sync_healthy;
        let mut next_prune_at = tokio::time::Instant::now() + prune_interval;
        let mut consecutive_prune_failures = 0u64;

        loop {
            if let Some(state) = sync_target.take_state() {
                match state {
                    SyncTargetState::Revert { revert_to } => {
                        match Self::run_revert(&storage, collector, revert_to) {
                            RevertOutcome::Completed => {
                                sync_healthy = true;
                                sync_target.mark_revert_complete(&revert_to);
                            }
                            RevertOutcome::Skipped => {
                                // Preserve `sync_healthy`: a no-op revert above `latest` did not
                                // verify lineage or repair an unhealthy/diverged store.
                                sync_target.mark_revert_complete(&revert_to);
                            }
                            RevertOutcome::TransientError => {
                                sync_healthy = false;
                                sync_target.requeue_state(SyncTargetState::Revert { revert_to });
                                tokio::time::sleep(SYNC_RETRY_DELAY).await;
                            }
                            RevertOutcome::StructuralError => {
                                sync_healthy = false;
                            }
                        }
                    }
                    SyncTargetState::RevertThenSync { revert_to, sync_to } => {
                        // Only advance once the revert landed or was a no-op above `latest`.
                        // Marking a failed revert complete would strip a revert that arrived while
                        // this one was running, and syncing forward on the abandoned branch would
                        // just be refused by the store's parent check.
                        match Self::run_revert(&storage, collector, revert_to) {
                            RevertOutcome::Completed => {
                                sync_healthy = true;
                                sync_target.mark_revert_complete(&revert_to);
                                sync_healthy = Self::sync_forward(
                                    &sync_target,
                                    &storage,
                                    &provider,
                                    collector,
                                    verification_interval,
                                    sync_to,
                                    sync_healthy,
                                )
                                .await;
                            }
                            RevertOutcome::Skipped => {
                                // Preserve `sync_healthy` while proceeding to `sync_forward`,
                                // which will verify parent lineage when storing new blocks.
                                sync_target.mark_revert_complete(&revert_to);
                                sync_healthy = Self::sync_forward(
                                    &sync_target,
                                    &storage,
                                    &provider,
                                    collector,
                                    verification_interval,
                                    sync_to,
                                    sync_healthy,
                                )
                                .await;
                            }
                            RevertOutcome::TransientError => {
                                sync_healthy = false;
                                sync_target.requeue_state(SyncTargetState::RevertThenSync {
                                    revert_to,
                                    sync_to,
                                });
                                tokio::time::sleep(SYNC_RETRY_DELAY).await;
                            }
                            RevertOutcome::StructuralError => {
                                sync_healthy = false;
                            }
                        }
                    }
                    SyncTargetState::SyncUpTo { to } => {
                        sync_healthy = Self::sync_forward(
                            &sync_target,
                            &storage,
                            &provider,
                            collector,
                            verification_interval,
                            to,
                            sync_healthy,
                        )
                        .await;
                    }
                }
                continue;
            }

            if sync_healthy && tokio::time::Instant::now() >= next_prune_at {
                Self::run_prune_tick(
                    pruner,
                    &provider,
                    &sync_target,
                    &mut consecutive_prune_failures,
                );
                next_prune_at = tokio::time::Instant::now() + prune_interval;
                continue;
            }

            tokio::select! {
                biased;
                _ = sync_target.notified() => {}
                _ = tokio::time::sleep_until(next_prune_at), if sync_healthy => {}
            }
        }
    }

    fn run_prune_tick(
        pruner: &MorphProofStoragePruner<Storage, Node::Provider>,
        provider: &Node::Provider,
        sync_target: &SyncTarget,
        consecutive_failures: &mut u64,
    ) {
        let persisted_head = match persisted_canonical_head(provider) {
            Ok(head) => head,
            Err(error) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                ProofHistoryMetrics::set_prune_consecutive_failures(*consecutive_failures);
                warn!(
                    target: "morph::proofs_exex",
                    ?error,
                    consecutive_failures = *consecutive_failures,
                    "Failed to resolve Reth persisted head before proof-history prune"
                );
                return;
            }
        };

        match pruner.try_run_with_abort(Some(persisted_head), || sync_target.has_pending_state()) {
            Ok(_) => {
                *consecutive_failures = 0;
                ProofHistoryMetrics::set_prune_consecutive_failures(0);
            }
            Err(error) => {
                *consecutive_failures = consecutive_failures.saturating_add(1);
                ProofHistoryMetrics::set_prune_consecutive_failures(*consecutive_failures);
                warn!(
                    target: "morph::proofs_exex",
                    ?error,
                    consecutive_failures = *consecutive_failures,
                    "Periodic proof-history prune failed"
                );
            }
        }
    }

    /// Runs a revert, classifies whatever it returns, and reports the [`RevertOutcome`].
    ///
    /// Nothing propagates: a reorg the store cannot represent is not a reason to take the node down.
    /// The caller must not treat a failed revert as done, because `mark_revert_complete` could strip
    /// a newer revert that arrived while this one was running. Both transient and structural
    /// failures clear the health gauge so periodic pruning pauses and `debug_proofsSyncStatus`
    /// accurately reflects that the store is still on the abandoned branch until retry or recovery
    /// lands. A no-op revert beyond `latest` returns [`RevertOutcome::Skipped`] without flipping
    /// the health gauge, so an unhealthy or diverged store cannot falsely recover on a shallow
    /// revert.
    fn run_revert(
        storage: &MorphProofsStorage<Storage>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        revert_to: BlockWithParent,
    ) -> RevertOutcome {
        match Self::handle_revert(storage, collector, revert_to) {
            Ok(true) => {
                ProofHistoryMetrics::set_sync_healthy(true);
                RevertOutcome::Completed
            }
            Ok(false) => RevertOutcome::Skipped,
            Err(error) => {
                let transient = Self::is_retryable_sync_error(&error);
                ProofHistoryMetrics::record_sync_error(transient);
                ProofHistoryMetrics::set_sync_healthy(false);
                if transient {
                    warn!(
                        target: "morph::proofs_exex",
                        ?error,
                        revert_to = revert_to.block.number,
                        "Transient proof-history revert failure; requeuing revert for retry"
                    );
                    RevertOutcome::TransientError
                } else {
                    warn!(
                        target: "morph::proofs_exex",
                        ?error,
                        revert_to = revert_to.block.number,
                        "Proof history stays on the abandoned branch; proof RPCs refuse requests until a later reorg or re-initialization recovers it"
                    );
                    RevertOutcome::StructuralError
                }
            }
        }
    }

    /// Reverts proof history to `revert_to`.
    ///
    /// Returns `Ok(true)` if storage was unwound (`latest >= revert_to.block.number`), or
    /// `Ok(false)` if `revert_to` was already beyond `latest` and no unwind occurred.
    ///
    /// The store legitimately refuses to unwind past its earliest block, which it must do to keep a
    /// baseline anchor, and that refusal is reachable in normal operation: `proofs init` writes only
    /// `EarliestBlock`, so until the first block is indexed `earliest == latest` and any reorg of
    /// the tip is already "beyond earliest".
    fn handle_revert(
        storage: &MorphProofsStorage<Storage>,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        revert_to: BlockWithParent,
    ) -> Result<bool, MorphProofsStorageError> {
        let latest = match storage.get_latest_block_number() {
            Ok(Some((n, _))) => n,
            Ok(None) => return Err(MorphProofsStorageError::NoBlocksFound),
            Err(error) => return Err(error),
        };

        if latest >= revert_to.block.number {
            info!(
                target: "morph::proofs_exex",
                revert_to = revert_to.block.number,
                latest,
                "Reverting proofs storage"
            );
            collector.unwind_history(revert_to)?;
            Ok(true)
        } else {
            debug!(
                target: "morph::proofs_exex",
                revert_to = revert_to.block.number,
                latest,
                "Revert target beyond stored blocks, skipping"
            );
            Ok(false)
        }
    }

    #[cfg(test)]
    fn handle_revert_for_test(
        &self,
        revert_to: BlockWithParent,
    ) -> Result<bool, MorphProofsStorageError> {
        let collector = LiveTrieCollector::new(
            self.ctx.evm_config().clone(),
            self.ctx.provider().clone(),
            &self.storage,
        );
        Self::handle_revert(&self.storage, &collector, revert_to)
    }

    #[cfg(test)]
    fn run_revert_for_test(&self, revert_to: BlockWithParent) -> RevertOutcome {
        let collector = LiveTrieCollector::new(
            self.ctx.evm_config().clone(),
            self.ctx.provider().clone(),
            &self.storage,
        );
        Self::run_revert(&self.storage, &collector, revert_to)
    }

    async fn sync_forward(
        sync_target: &SyncTarget,
        storage: &MorphProofsStorage<Storage>,
        provider: &Node::Provider,
        collector: &LiveTrieCollector<'_, Node::Evm, Node::Provider, Storage>,
        verification_interval: u64,
        target: u64,
        mut sync_healthy: bool,
    ) -> bool {
        loop {
            // Check for higher-priority state (e.g. revert) before processing.
            if sync_target.has_pending_state() {
                return sync_healthy;
            }

            let latest = match storage.get_latest_block_number() {
                Ok(Some((n, _))) => n,
                Ok(None) => {
                    // The store lost its tip. Retrying cannot rebuild it, so stop and let the
                    // health gauge and the RPC anchor check report an unusable index.
                    ProofHistoryMetrics::record_sync_error(false);
                    ProofHistoryMetrics::set_sync_healthy(false);
                    error!(target: "morph::proofs_exex", "No blocks stored in proofs storage during sync");
                    return false;
                }
                Err(error) => {
                    ProofHistoryMetrics::record_sync_error(true);
                    error!(target: "morph::proofs_exex", ?error, "Failed to get latest block");
                    Self::requeue_sync_up_to(sync_target, target).await;
                    return false;
                }
            };

            if latest >= target {
                return sync_healthy;
            }

            let end = latest
                .saturating_add(SYNC_BLOCKS_PER_TURN as u64)
                .min(target);
            info!(
                target: "morph::proofs_exex",
                start = latest + 1,
                end,
                target,
                blocks = end - latest,
                "Processing proofs storage sync turn"
            );

            let mut batch: Vec<BatchBlock<Primitives>> =
                Vec::with_capacity((end - latest) as usize);
            for block_num in (latest + 1)..=end {
                let cached = sync_target.take(block_num);
                match Self::build_batch_entry(block_num, cached, provider, verification_interval) {
                    Ok(entry) => batch.push(entry),
                    Err(error) => {
                        // A block missing from the provider usually means a reorg landed mid-batch;
                        // the notification that follows supplies the revert.
                        ProofHistoryMetrics::record_sync_error(true);
                        error!(target: "morph::proofs_exex", block_number = block_num, ?error, "Preparing block for batch failed");
                        Self::requeue_sync_up_to(sync_target, target).await;
                        return false;
                    }
                }
            }

            if let Err(error) = collector.execute_and_store_batch(batch) {
                let transient = Self::is_retryable_sync_error(&error);
                ProofHistoryMetrics::record_sync_error(transient);
                if transient {
                    error!(target: "morph::proofs_exex", start = latest + 1, end, ?error, "Transient batch processing failure");
                    Self::requeue_sync_up_to(sync_target, target).await;
                } else {
                    // The batch does not belong on the stored tip. Re-running it would fail the
                    // same way; the recovery path is the revert a later notification brings, or a
                    // re-initialization.
                    ProofHistoryMetrics::set_sync_healthy(false);
                    error!(target: "morph::proofs_exex", start = latest + 1, end, ?error, "Structural batch processing failure; waiting for a later notification");
                }
                return false;
            }
            sync_healthy = true;
            ProofHistoryMetrics::set_sync_healthy(true);

            info!(target: "morph::proofs_exex", latest_stored = latest, target, "Batch processed, yielding");
            task::yield_now().await;
        }
    }

    /// Re-arm a forward-sync target after a transient failure, then back off.
    ///
    /// `take_state()` already consumed the original `SyncUpTo`, so returning without this would
    /// leave the loop parked on `notified()` until the next chain event. Requeuing puts the failed
    /// target back as the base and merges anything that arrived while it ran on top, so a newer
    /// notification still wins.
    async fn requeue_sync_up_to(sync_target: &SyncTarget, target: u64) {
        sync_target.requeue_state(SyncTargetState::SyncUpTo { to: target });
        tokio::time::sleep(SYNC_RETRY_DELAY).await;
    }

    /// Whether re-running the same work could succeed.
    ///
    /// Storage and provider I/O can fail for reasons that clear on their own. Every other variant
    /// means the work does not belong where it was applied, and only a reorg or a re-initialization
    /// changes that.
    const fn is_retryable_sync_error(error: &MorphProofsStorageError) -> bool {
        matches!(
            error,
            MorphProofsStorageError::DatabaseError(_)
                | MorphProofsStorageError::ProviderError(_)
                | MorphProofsStorageError::TryLockError
        )
    }

    fn build_batch_entry(
        block_number: u64,
        cached: Option<CachedBlockTrieData>,
        provider: &Node::Provider,
        verification_interval: u64,
    ) -> eyre::Result<BatchBlock<Primitives>> {
        let should_verify =
            verification_interval > 0 && block_number.is_multiple_of(verification_interval);
        let has_cached = cached.is_some();

        if let Some(cached) = cached
            && !should_verify
        {
            debug!(
                target: "morph::proofs_exex",
                block_number,
                "Using pre-computed state from notification"
            );
            return Ok(BatchBlock::Cached {
                block_with_parent: cached.block_with_parent,
                sorted_trie_updates: cached.trie_data.trie_updates(),
                sorted_post_state: cached.trie_data.hashed_state(),
            });
        }

        if has_cached {
            info!(
                target: "morph::proofs_exex",
                block_number,
                verification_interval,
                "Periodic verification: performing full block execution despite cached data"
            );
        } else {
            debug!(
                target: "morph::proofs_exex",
                block_number,
                "No cached trie data, falling back to full execution"
            );
        }

        debug!(
            target: "morph::proofs_exex",
            block_number,
            "Fetching block from provider for execution",
        );

        let block = provider
            .recovered_block(block_number.into(), TransactionVariant::NoHash)?
            .ok_or_else(|| eyre::eyre!("Missing block {} in provider", block_number))?;

        Ok(BatchBlock::Execute(Box::new(block)))
    }

    fn handle_notification(
        &self,
        notification: ExExNotification<Primitives>,
        sync_target: &SyncTarget,
    ) -> eyre::Result<()> {
        match &notification {
            ExExNotification::ChainCommitted { new } => {
                self.handle_chain_committed(Arc::clone(new), sync_target)?
            }
            ExExNotification::ChainReorged { old, new } => {
                self.handle_chain_reorged(Arc::clone(old), Arc::clone(new), sync_target)?
            }
            ExExNotification::ChainReverted { old } => {
                self.handle_chain_reverted(Arc::clone(old), sync_target)?
            }
        }

        if let Some(committed_chain) = notification.committed_chain() {
            let tip = committed_chain.tip().num_hash();
            debug!(
                target: "morph::proofs_exex",
                block_number = tip.number,
                block_hash = ?tip.hash,
                "Sending FinishedHeight event"
            );
            self.ctx.events.send(ExExEvent::FinishedHeight(tip))?;
        }

        Ok(())
    }

    fn handle_chain_committed(
        &self,
        new: Arc<Chain<Primitives>>,
        sync_target: &SyncTarget,
    ) -> eyre::Result<()> {
        debug!(
            target: "morph::proofs_exex",
            block_number = new.tip().number(),
            block_hash = ?new.tip().hash(),
            "ChainCommitted notification received",
        );

        // Cache trie data for all blocks in the chain so the sync loop can
        // use pre-computed data even when it is many blocks behind.
        let total_blocks = new.blocks().len();
        let mut cached_count = 0usize;
        for (&block_number, block) in new.blocks() {
            if let Some(trie_data) = new.trie_data_at(block_number) {
                sync_target.insert(
                    block_number,
                    CachedBlockTrieData {
                        block_with_parent: block.block_with_parent(),
                        trie_data: trie_data.clone(),
                    },
                );
                cached_count += 1;
            } else {
                debug!(
                    target: "morph::proofs_exex",
                    block_number,
                    "Notification block missing trie data"
                );
            }
        }

        debug!(
            target: "morph::proofs_exex",
            tip = new.tip().number(),
            total_blocks,
            cached_count,
            missing = total_blocks - cached_count,
            "Cached notification trie data"
        );

        sync_target.update_state(SyncTargetState::SyncUpTo {
            to: new.tip().number(),
        });
        Ok(())
    }

    fn handle_chain_reorged(
        &self,
        old: Arc<Chain<Primitives>>,
        new: Arc<Chain<Primitives>>,
        sync_target: &SyncTarget,
    ) -> eyre::Result<()> {
        info!(
            target: "morph::proofs_exex",
            old_block_number = old.tip().number(),
            old_block_hash = ?old.tip().hash(),
            new_block_number = new.tip().number(),
            new_block_hash = ?new.tip().hash(),
            "ChainReorged notification received",
        );

        if old.fork_block() != new.fork_block() {
            return Err(eyre::eyre!(
                "Fork blocks do not match: old fork block {:?}, new fork block {:?}",
                old.fork_block(),
                new.fork_block()
            ));
        }

        let first_old = old.first().block_with_parent();

        // Invalidate any cached blocks from the old chain.
        sync_target.clear_from(first_old.block.number);

        // Cache trie data for all blocks in the new chain.
        for (&block_number, block) in new.blocks() {
            if let Some(trie_data) = new.trie_data_at(block_number) {
                sync_target.insert(
                    block_number,
                    CachedBlockTrieData {
                        block_with_parent: block.block_with_parent(),
                        trie_data: trie_data.clone(),
                    },
                );
            } else {
                debug!(
                    target: "morph::proofs_exex",
                    block_number,
                    "Reorged block missing trie data"
                );
            }
        }

        sync_target.update_state(SyncTargetState::RevertThenSync {
            revert_to: first_old,
            sync_to: new.tip().number(),
        });

        Ok(())
    }

    fn handle_chain_reverted(
        &self,
        old: Arc<Chain<Primitives>>,
        sync_target: &SyncTarget,
    ) -> eyre::Result<()> {
        info!(
            target: "morph::proofs_exex",
            old_block_number = old.tip().number(),
            old_block_hash = ?old.tip().hash(),
            "ChainReverted notification received",
        );

        let first_old = old.first().block_with_parent();

        // Invalidate any cached blocks that are being reverted.
        sync_target.clear_from(first_old.block.number);

        sync_target.update_state(SyncTargetState::Revert {
            revert_to: first_old,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, default::Default, sync::Arc, time::Duration};

    use alloy_consensus::private::alloy_primitives::B256;
    use alloy_eips::{BlockNumHash, NumHash, eip1898::BlockWithParent};
    use morph_proofs::{BlockStateDiff, MdbxProofsStorage, MorphProofsStorage, MorphProofsStore};
    use reth_db::test_utils::tempdir_path;
    use reth_ethereum_primitives::{Block, EthPrimitives, Receipt};
    use reth_execution_types::{Chain, ExecutionOutcome};
    use reth_primitives_traits::RecoveredBlock;
    use reth_trie::{
        ComputedTrieData, HashedPostStateSorted, LazyTrieData, updates::TrieUpdatesSorted,
    };

    use super::*;

    // -------------------------------------------------------------------------
    // Helpers: deterministic blocks and deterministic Chain with precomputed updates
    // -------------------------------------------------------------------------
    fn b256(byte: u8) -> B256 {
        B256::new([byte; 32])
    }

    fn hash_for_num(num: u64) -> B256 {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&num.to_be_bytes());
        B256::new(out)
    }

    fn mk_block(num: u64) -> RecoveredBlock<Block> {
        let mut b: RecoveredBlock<Block> = Default::default();
        b.set_block_number(num);
        b.set_hash(hash_for_num(num));
        b.set_parent_hash(hash_for_num(num.saturating_sub(1)));
        b
    }

    fn mk_chain_with_updates(
        from: u64,
        to: u64,
        hash_override: Option<B256>,
    ) -> Chain<reth_ethereum_primitives::EthPrimitives> {
        let mut blocks: Vec<RecoveredBlock<Block>> = Vec::new();
        let mut trie_data = BTreeMap::new();

        for n in from..=to {
            let mut b = mk_block(n);
            if let Some(hash) = hash_override {
                b.set_hash(hash);
            }
            blocks.push(b);

            let data = LazyTrieData::ready(ComputedTrieData::new(
                Arc::new(HashedPostStateSorted::default()),
                Arc::new(TrieUpdatesSorted::default()),
            ));
            trie_data.insert(n, data);
        }

        let execution_outcome: ExecutionOutcome<Receipt> = ExecutionOutcome {
            bundle: Default::default(),
            receipts: Vec::new(),
            requests: Vec::new(),
            first_block: from,
        };

        Chain::new(blocks, execution_outcome, trie_data)
    }

    /// Store blocks directly into proofs storage (bypasses the sync loop).
    fn store_blocks<S: MorphProofsStore>(from: u64, to: u64, storage: &MorphProofsStorage<S>) {
        for n in from..=to {
            let chain = mk_chain_with_updates(n, n, None);
            let block = chain.blocks().get(&n).unwrap();
            storage
                .store_trie_updates(block.block_with_parent(), BlockStateDiff::default())
                .expect("store trie update");
        }
    }

    fn init_storage_at<S: MorphProofsStore>(
        storage: MorphProofsStorage<S>,
        genesis_block: NumHash,
    ) {
        storage
            .set_earliest_block_number(genesis_block.number, genesis_block.hash)
            .expect("set earliest");
        storage
            .store_trie_updates(
                BlockWithParent::new(genesis_block.hash, genesis_block),
                BlockStateDiff::default(),
            )
            .expect("store trie update");
    }

    fn init_storage<S: MorphProofsStore>(storage: MorphProofsStorage<S>) {
        init_storage_at(storage, NumHash::new(0, b256(0x00)));
    }

    // Initialize exex with config
    fn build_test_exex<NodeT, Store>(
        ctx: ExExContext<NodeT>,
        storage: MorphProofsStorage<Store>,
    ) -> MorphProofsExEx<NodeT, Store>
    where
        NodeT: FullNodeComponents,
        Store: MorphProofsStore + Clone + 'static,
    {
        MorphProofsExEx::builder(ctx, storage)
            .with_proofs_history_window(20)
            .with_proofs_history_prune_interval(Duration::from_secs(3600))
            .with_verification_interval(1000)
            .with_max_prune_blocks_startup(1000)
            .build()
    }

    #[tokio::test]
    async fn handle_notification_chain_committed() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let new_chain = Arc::new(mk_chain_with_updates(1, 1, None));
        let notif = ExExNotification::ChainCommitted { new: new_chain };

        let sync_target = SyncTarget::new();

        exex.handle_notification(notif, &sync_target)
            .expect("handle chain commit");

        // Committed blocks are cached in sync target, not stored directly
        assert!(sync_target.take(1).is_some(), "block 1 should be cached");
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(state, SyncTargetState::SyncUpTo { to: 1 }));
    }

    #[tokio::test]
    async fn handle_notification_chain_committed_caches_already_stored_blocks() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());

        // Pre-store blocks 1..5 so storage is at block 5
        store_blocks(1, 5, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let sync_target = SyncTarget::new();

        // Handle notification for block 5 which is already stored - still caches
        let new_chain = Arc::new(mk_chain_with_updates(5, 5, Some(hash_for_num(10))));
        let notif = ExExNotification::ChainCommitted { new: new_chain };
        exex.handle_notification(notif, &sync_target)
            .expect("handle chain commit");

        // State is set (sync loop will see latest >= target and skip)
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(state, SyncTargetState::SyncUpTo { to: 5 }));

        // Storage is unchanged (notification handler doesn't write to storage)
        let latest = proofs
            .get_latest_block_number()
            .expect("get latest block")
            .expect("ok");
        assert_eq!(latest.0, 5);
        assert_eq!(latest.1, hash_for_num(5));
    }

    #[tokio::test]
    async fn handle_notification_chain_reorged() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());
        store_blocks(1, 10, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let sync_target = SyncTarget::new();

        // Now the tip is 10, and we want to reorg from block 6..12
        let old_chain = Arc::new(mk_chain_with_updates(6, 10, None));
        let new_chain = Arc::new(mk_chain_with_updates(6, 12, None));

        // Notification: chain reorged 6..12
        let notif = ExExNotification::ChainReorged {
            new: new_chain,
            old: old_chain,
        };

        exex.handle_notification(notif, &sync_target)
            .expect("handle chain re-orged");

        // Should have RevertThenSync state
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 12 }
            if revert_to.block.number == 6
        ));

        // New chain blocks should be cached
        for n in 6..=12 {
            assert!(sync_target.take(n).is_some(), "block {n} should be cached");
        }

        // Storage unchanged (sync loop handles the actual revert)
        let latest = proofs
            .get_latest_block_number()
            .expect("get latest block")
            .expect("ok")
            .0;
        assert_eq!(latest, 10);
    }

    #[tokio::test]
    async fn handle_notification_chain_reorged_beyond_stored_blocks() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());
        store_blocks(1, 10, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let sync_target = SyncTarget::new();

        // Now the tip is 10, and we want to reorg from block 12..15
        // Both chains share the same fork block (block 11)
        let old_chain = Arc::new(mk_chain_with_updates(12, 15, None));
        let new_chain = Arc::new(mk_chain_with_updates(12, 20, None));

        // Notification: chain reorged 12..20
        let notif = ExExNotification::ChainReorged {
            new: new_chain,
            old: old_chain,
        };

        exex.handle_notification(notif, &sync_target)
            .expect("handle chain re-orged");

        // State is set; sync loop will detect revert is beyond stored blocks
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(
            state,
            SyncTargetState::RevertThenSync { revert_to, sync_to: 20 }
            if revert_to.block.number == 12
        ));

        // Storage unchanged
        let latest = proofs
            .get_latest_block_number()
            .expect("get latest block")
            .expect("ok")
            .0;
        assert_eq!(latest, 10);
    }

    #[tokio::test]
    async fn handle_notification_chain_reverted() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());
        store_blocks(1, 10, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let sync_target = SyncTarget::new();

        // Now the tip is 10, and we want to revert from block 9..10
        let old_chain = Arc::new(mk_chain_with_updates(9, 10, None));

        // Notification: chain reverted 9..10
        let notif = ExExNotification::ChainReverted { old: old_chain };

        exex.handle_notification(notif, &sync_target)
            .expect("handle chain reverted");

        // Should have Revert state
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(
            state,
            SyncTargetState::Revert { revert_to }
            if revert_to.block.number == 9
        ));

        // Storage unchanged (sync loop handles the actual revert)
        let latest = proofs
            .get_latest_block_number()
            .expect("get latest block")
            .expect("ok")
            .0;
        assert_eq!(latest, 10);
    }

    #[tokio::test]
    async fn handle_notification_chain_reverted_beyond_stored_blocks() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());
        store_blocks(1, 5, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        let sync_target = SyncTarget::new();

        // Now the tip is 5, and we want to revert from block 9..10
        let old_chain = Arc::new(mk_chain_with_updates(9, 10, None));

        // Notification: chain reverted 9..10
        let notif = ExExNotification::ChainReverted { old: old_chain };

        exex.handle_notification(notif, &sync_target)
            .expect("handle chain reverted");

        // State is set; sync loop will detect revert is beyond stored blocks
        let state = sync_target.take_state().expect("should have pending state");
        assert!(matches!(
            state,
            SyncTargetState::Revert { revert_to }
            if revert_to.block.number == 9
        ));

        // Storage unchanged
        let latest = proofs
            .get_latest_block_number()
            .expect("get latest block")
            .expect("ok")
            .0;
        assert_eq!(latest, 5);
    }

    /// A reorg the store cannot represent must not take the node down.
    ///
    /// Right after `proofs init` the store holds only its anchor, so `earliest == latest` and any
    /// reorg of the tip is already "beyond earliest" — the single most likely reorg to hit a freshly
    /// initialized node. `run_revert` therefore has to contain that failure: it runs inside a
    /// critical task whose error path is a panic, so propagating `Err` would shut the node down.
    #[tokio::test]
    async fn handle_revert_contains_unwind_failure() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);
        init_storage(proofs.clone());

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let exex = build_test_exex(ctx, proofs.clone());

        // Unwinding the anchor itself is what the store refuses, to keep a baseline.
        let initial_anchor = BlockWithParent::new(b256(0x00), NumHash::new(0, b256(0x00)));
        assert!(matches!(
            exex.handle_revert_for_test(initial_anchor),
            Err(MorphProofsStorageError::UnwindBeyondEarliest { .. })
        ));
        // The sync loop keys `mark_revert_complete` off this, so a contained failure must never
        // report success: doing so would strip a reorg that queued up while the revert was running.
        assert_eq!(
            exex.run_revert_for_test(initial_anchor),
            RevertOutcome::StructuralError,
            "a failed revert must not be reported as landed"
        );

        assert_eq!(
            proofs
                .get_latest_block_number()
                .expect("latest")
                .expect("initialized")
                .0,
            0,
            "a contained unwind failure must leave proof history unchanged"
        );
    }

    #[tokio::test]
    async fn ensure_initialized_errors_on_storage_not_initialized() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());
        let error = exex.ensure_initialized().expect_err("should return error");
        assert!(
            error.to_string().contains("not initialized"),
            "expected an uninitialized-storage error, got: {error}"
        );
    }

    #[tokio::test]
    async fn ensure_initialized_errors_when_prune_exceeds_threshold() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());

        for i in 1..1100 {
            proofs
                .store_trie_updates(
                    BlockWithParent::new(
                        hash_for_num(i - 1),
                        BlockNumHash::new(i, hash_for_num(i)),
                    ),
                    BlockStateDiff::default(),
                )
                .expect("store trie update");
        }

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());
        let error = exex.ensure_initialized().expect_err("should return error");
        assert!(
            error.to_string().contains("exceeds the safety threshold"),
            "expected the startup prune-threshold error, got: {error}"
        );
    }

    #[tokio::test]
    async fn ensure_initialized_allows_latest_hash_to_differ_from_canonical_chain() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        // Startup accepts an independently committed proof tip. Proof RPCs still validate the
        // canonical anchor before serving data.
        let genesis = handle.genesis.num_hash();
        assert_ne!(genesis.hash, b256(0x00), "fixture must not collide");
        init_storage_at(proofs.clone(), NumHash::new(genesis.number, b256(0x00)));

        let exex = build_test_exex(ctx, proofs.clone());
        exex.ensure_initialized()
            .expect("startup should not validate the proof tip against the canonical chain");
    }

    #[tokio::test]
    async fn ensure_initialized_allows_proof_storage_ahead_of_canonical_chain() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let genesis = handle.genesis.num_hash();
        init_storage_at(proofs.clone(), genesis);
        proofs
            .store_trie_updates(
                BlockWithParent::new(genesis.hash, NumHash::new(1, b256(0x01))),
                BlockStateDiff::default(),
            )
            .expect("store proof tip ahead of canonical chain");

        let exex = build_test_exex(ctx, proofs.clone());
        exex.ensure_initialized()
            .expect("proof storage may be ahead after an ungraceful shutdown");
    }

    #[tokio::test]
    async fn ensure_initialized_succeeds() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        init_storage_at(proofs.clone(), handle.genesis.num_hash());

        let exex = build_test_exex(ctx, proofs.clone());
        exex.ensure_initialized().expect("should not return error");
    }

    #[tokio::test]
    async fn handle_notification_schedules_async_on_gap() {
        // MDBX proofs storage
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        init_storage(proofs.clone());

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let exex = build_test_exex(ctx, proofs.clone());

        // Notification: chain committed 5..10 (Blocks 1,2,3,4 are missing from storage)
        let new_chain = Arc::new(mk_chain_with_updates(5, 10, None));
        let notif = ExExNotification::ChainCommitted { new: new_chain };

        let sync_target = SyncTarget::new();

        // Process notification
        exex.handle_notification(notif, &sync_target)
            .expect("handle chain commit should return ok immediately");

        // Verify the sync target state was set
        let state = sync_target.take_state().expect("should have pending state");
        assert!(
            matches!(state, SyncTargetState::SyncUpTo { to: 10 }),
            "Should have scheduled sync to block 10"
        );

        // Verify Main Thread did NOT process it
        // Because we didn't spawn the actual worker thread in this test, storage should still be at
        // 0. This proves the 'handle_notification' returned instantly without doing the
        // heavy lifting.
        let latest = proofs
            .get_latest_block_number()
            .expect("get")
            .expect("ok")
            .0;
        assert_eq!(
            latest, 0,
            "Main thread should not have processed the blocks synchronously"
        );
    }

    // -------------------------------------------------------------------------
    // build_batch_entry: cached fast path vs forced full re-execution
    // -------------------------------------------------------------------------

    /// Concrete `MorphProofsExEx` used to reach the private `build_batch_entry`.
    type TestProofsExEx = MorphProofsExEx<reth_exex_test_utils::Adapter, Arc<MdbxProofsStorage>>;

    fn cached_trie_data(num: u64) -> CachedBlockTrieData {
        CachedBlockTrieData {
            block_with_parent: BlockWithParent::new(
                hash_for_num(num.saturating_sub(1)),
                NumHash::new(num, hash_for_num(num)),
            ),
            trie_data: LazyTrieData::ready(ComputedTrieData::new(
                Arc::new(HashedPostStateSorted::default()),
                Arc::new(TrieUpdatesSorted::default()),
            )),
        }
    }

    /// The test provider only holds genesis, so any block above it that reaches the
    /// execution branch fails with `Missing block`. That error is therefore a precise
    /// witness that the cached fast path was *not* taken.
    fn assert_took_execution_path(result: eyre::Result<BatchBlock<EthPrimitives>>, num: u64) {
        let error = result.expect_err("execution path must fail on the genesis-only test provider");
        assert!(
            error.to_string().contains(&format!("Missing block {num}")),
            "expected a missing-block error proving full execution was attempted, got: {error}"
        );
    }

    #[tokio::test]
    async fn build_batch_entry_uses_cache_when_verification_is_disabled() {
        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        let entry =
            TestProofsExEx::build_batch_entry(5, Some(cached_trie_data(5)), ctx.provider(), 0)
                .expect("cached entry");
        assert!(
            matches!(entry, BatchBlock::Cached { .. }),
            "interval 0 must never force re-execution"
        );
    }

    #[tokio::test]
    async fn build_batch_entry_uses_cache_off_the_verification_interval() {
        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        // 5 % 3 != 0 and 7 % 3 != 0, so both stay on the cached path.
        for num in [5, 7] {
            let entry = TestProofsExEx::build_batch_entry(
                num,
                Some(cached_trie_data(num)),
                ctx.provider(),
                3,
            )
            .expect("cached entry");
            assert!(
                matches!(entry, BatchBlock::Cached { .. }),
                "block {num} is not a multiple of 3 and must use the cache"
            );
        }
    }

    #[tokio::test]
    async fn build_batch_entry_forces_execution_on_the_verification_interval() {
        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        // 6 % 3 == 0 and 5 % 1 == 0: cached data must be discarded in favour of execution.
        assert_took_execution_path(
            TestProofsExEx::build_batch_entry(6, Some(cached_trie_data(6)), ctx.provider(), 3),
            6,
        );
        assert_took_execution_path(
            TestProofsExEx::build_batch_entry(5, Some(cached_trie_data(5)), ctx.provider(), 1),
            5,
        );
    }

    #[tokio::test]
    async fn build_batch_entry_falls_back_to_execution_without_cache() {
        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");

        assert_took_execution_path(
            TestProofsExEx::build_batch_entry(5, None, ctx.provider(), 0),
            5,
        );
    }

    #[tokio::test]
    async fn build_batch_entry_returns_execute_variant_for_an_available_block() {
        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let genesis = handle.genesis.number;

        // Genesis is the one block the test provider can recover, so this pins the
        // positive `Execute` variant rather than only the missing-block error.
        let entry = TestProofsExEx::build_batch_entry(
            genesis,
            Some(cached_trie_data(genesis)),
            ctx.provider(),
            1,
        )
        .expect("genesis is recoverable");
        match entry {
            BatchBlock::Execute(block) => assert_eq!(block.number, genesis),
            BatchBlock::Cached { .. } => {
                panic!("verification interval 1 must discard cached data for every block")
            }
        }

        // Same block, verification disabled: back on the cached path.
        let cached = TestProofsExEx::build_batch_entry(
            genesis,
            Some(cached_trie_data(genesis)),
            ctx.provider(),
            0,
        )
        .expect("cached entry");
        assert!(matches!(cached, BatchBlock::Cached { .. }));
    }

    #[test]
    fn hash_for_num_encodes_full_block_number() {
        assert_eq!(hash_for_num(0), b256(0x00));
        assert_ne!(
            hash_for_num(0),
            hash_for_num(256),
            "block numbers must not wrap on a single byte"
        );
        let _genesis = mk_block(0);
    }

    #[tokio::test]
    async fn reconcile_on_startup_unwinds_ahead_of_tip_proof_storage() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let genesis = handle.genesis.num_hash();

        // Proof DB persisted block 1 ahead of Reth's genesis tip (e.g., unclean shutdown).
        init_storage_at(proofs.clone(), genesis);
        proofs
            .store_trie_updates(
                BlockWithParent::new(genesis.hash, NumHash::new(1, b256(0x01))),
                BlockStateDiff::default(),
            )
            .expect("store ahead-of-tip block");
        assert_eq!(
            proofs.get_latest_block_number().unwrap(),
            Some((1, b256(0x01)))
        );

        let exex = build_test_exex(ctx, proofs.clone());
        assert!(
            exex.reconcile_on_startup().expect("reconcile"),
            "ahead-of-tip proof DB with canonical genesis must reconcile cleanly"
        );
        assert_eq!(
            proofs.get_latest_block_number().unwrap(),
            Some((genesis.number, genesis.hash)),
            "ahead-of-tip block must be unwound back to canonical tip"
        );
    }

    #[tokio::test]
    async fn reconcile_on_startup_reports_unhealthy_when_earliest_diverges() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);

        let (ctx, handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let genesis = handle.genesis.num_hash();
        let non_canonical = b256(0xEE);
        assert_ne!(genesis.hash, non_canonical);

        init_storage_at(proofs.clone(), NumHash::new(genesis.number, non_canonical));
        let exex = build_test_exex(ctx, proofs.clone());

        assert!(
            !exex.reconcile_on_startup().expect("reconcile"),
            "divergent earliest block must keep proof sync unhealthy instead of crashing"
        );
        assert_eq!(
            proofs.get_latest_block_number().unwrap(),
            Some((genesis.number, non_canonical))
        );
    }

    /// Test wrapper that injects a single transient `TryLockError` on `unwind_history`, then
    /// delegates all subsequent calls to the underlying `MdbxProofsStorage`.
    #[derive(Debug, Clone)]
    struct FailFirstUnwindStore {
        inner: Arc<MdbxProofsStorage>,
        fail_next_unwind: Arc<std::sync::atomic::AtomicBool>,
    }

    impl MorphProofsStore for FailFirstUnwindStore {
        type StorageTrieCursor<'tx> =
            <Arc<MdbxProofsStorage> as MorphProofsStore>::StorageTrieCursor<'tx>;
        type AccountTrieCursor<'tx> =
            <Arc<MdbxProofsStorage> as MorphProofsStore>::AccountTrieCursor<'tx>;
        type StorageCursor<'tx> = <Arc<MdbxProofsStorage> as MorphProofsStore>::StorageCursor<'tx>;
        type AccountHashedCursor<'tx> =
            <Arc<MdbxProofsStorage> as MorphProofsStore>::AccountHashedCursor<'tx>;
        type Tx<'tx> = <Arc<MdbxProofsStorage> as MorphProofsStore>::Tx<'tx>;

        fn get_earliest_block_number(
            &self,
        ) -> morph_proofs::MorphProofsStorageResult<Option<(u64, B256)>> {
            self.inner.get_earliest_block_number()
        }
        fn get_latest_block_number(
            &self,
        ) -> morph_proofs::MorphProofsStorageResult<Option<(u64, B256)>> {
            self.inner.get_latest_block_number()
        }
        fn get_block_hash(
            &self,
            block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Option<B256>> {
            self.inner.get_block_hash(block_number)
        }
        fn get_earliest_block_number_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
        ) -> morph_proofs::MorphProofsStorageResult<Option<(u64, B256)>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner.get_earliest_block_number_with_tx(tx)
        }
        fn get_latest_block_number_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
        ) -> morph_proofs::MorphProofsStorageResult<Option<(u64, B256)>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner.get_latest_block_number_with_tx(tx)
        }
        fn storage_trie_cursor<'tx>(
            &'tx self,
            hashed_address: B256,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::StorageTrieCursor<'tx>> {
            self.inner
                .storage_trie_cursor(hashed_address, max_block_number)
        }
        fn account_trie_cursor<'tx>(
            &'tx self,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::AccountTrieCursor<'tx>> {
            self.inner.account_trie_cursor(max_block_number)
        }
        fn storage_hashed_cursor<'tx>(
            &'tx self,
            hashed_address: B256,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::StorageCursor<'tx>> {
            self.inner
                .storage_hashed_cursor(hashed_address, max_block_number)
        }
        fn account_hashed_cursor<'tx>(
            &'tx self,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::AccountHashedCursor<'tx>> {
            self.inner.account_hashed_cursor(max_block_number)
        }
        fn ro_tx<'tx>(&'tx self) -> morph_proofs::MorphProofsStorageResult<Self::Tx<'tx>> {
            self.inner.ro_tx()
        }
        fn storage_trie_cursor_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
            hashed_address: B256,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::StorageTrieCursor<'tx>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner
                .storage_trie_cursor_with_tx(tx, hashed_address, max_block_number)
        }
        fn account_trie_cursor_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::AccountTrieCursor<'tx>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner.account_trie_cursor_with_tx(tx, max_block_number)
        }
        fn storage_hashed_cursor_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
            hashed_address: B256,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::StorageCursor<'tx>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner
                .storage_hashed_cursor_with_tx(tx, hashed_address, max_block_number)
        }
        fn account_hashed_cursor_with_tx<'tx, 'db>(
            &self,
            tx: &'tx Self::Tx<'db>,
            max_block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<Self::AccountHashedCursor<'tx>>
        where
            Self: 'db,
            'db: 'tx,
        {
            self.inner
                .account_hashed_cursor_with_tx(tx, max_block_number)
        }
        fn store_trie_updates(
            &self,
            block_ref: BlockWithParent,
            block_state_diff: BlockStateDiff,
        ) -> morph_proofs::MorphProofsStorageResult<morph_proofs::WriteCounts> {
            self.inner.store_trie_updates(block_ref, block_state_diff)
        }
        fn fetch_trie_updates(
            &self,
            block_number: u64,
        ) -> morph_proofs::MorphProofsStorageResult<BlockStateDiff> {
            self.inner.fetch_trie_updates(block_number)
        }
        fn prune_earliest_state(
            &self,
            new_earliest_block_ref: BlockWithParent,
        ) -> morph_proofs::MorphProofsStorageResult<morph_proofs::WriteCounts> {
            self.inner.prune_earliest_state(new_earliest_block_ref)
        }
        fn unwind_history(
            &self,
            to: BlockWithParent,
        ) -> morph_proofs::MorphProofsStorageResult<()> {
            if self
                .fail_next_unwind
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return Err(MorphProofsStorageError::TryLockError);
            }
            self.inner.unwind_history(to)
        }
        fn replace_updates(
            &self,
            latest_common_block: BlockNumHash,
            blocks_to_add: Vec<(BlockWithParent, BlockStateDiff)>,
        ) -> morph_proofs::MorphProofsStorageResult<()> {
            self.inner
                .replace_updates(latest_common_block, blocks_to_add)
        }
        fn set_earliest_block_number(
            &self,
            block_number: u64,
            hash: B256,
        ) -> morph_proofs::MorphProofsStorageResult<()> {
            self.inner.set_earliest_block_number(block_number, hash)
        }
    }

    impl MorphProofsBatchStore for FailFirstUnwindStore {
        type BatchSession<'a> = <Arc<MdbxProofsStorage> as MorphProofsBatchStore>::BatchSession<'a>;

        fn with_batch_session<R, F>(&self, f: F) -> morph_proofs::MorphProofsStorageResult<R>
        where
            F: FnOnce(&mut Self::BatchSession<'_>) -> morph_proofs::MorphProofsStorageResult<R>,
        {
            self.inner.with_batch_session(f)
        }
    }

    #[tokio::test]
    async fn transient_revert_failure_requeues_and_retries_until_complete() {
        let dir = tempdir_path();
        let inner = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        init_storage(Arc::clone(&inner));
        store_blocks(1, 3, &inner);

        let flaky = FailFirstUnwindStore {
            inner: Arc::clone(&inner),
            fail_next_unwind: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let exex = build_test_exex(ctx, flaky.clone());

        let revert_to = BlockWithParent::new(hash_for_num(1), NumHash::new(2, hash_for_num(2)));
        let sync_target = SyncTarget::new();
        sync_target.update_state(SyncTargetState::RevertThenSync {
            revert_to,
            sync_to: 4,
        });

        // First attempt consumes state and hits the injected transient error.
        let consumed = sync_target.take_state().expect("pending revert");
        let SyncTargetState::RevertThenSync { revert_to, sync_to } = consumed else {
            panic!("expected RevertThenSync");
        };
        assert_eq!(
            exex.run_revert_for_test(revert_to),
            RevertOutcome::TransientError,
            "retryable unwind failure must return TransientError"
        );
        // Storage was not yet unwound; requeue preserves both revert_to and sync_to.
        assert_eq!(
            inner.get_latest_block_number().unwrap(),
            Some((3, hash_for_num(3)))
        );
        sync_target.requeue_state(SyncTargetState::RevertThenSync { revert_to, sync_to });

        // Retry consumes the requeued state and succeeds cleanly.
        let retried = sync_target.take_state().expect("requeued revert");
        let SyncTargetState::RevertThenSync {
            revert_to: retry_revert,
            sync_to: retry_sync,
        } = retried
        else {
            panic!("expected requeued RevertThenSync");
        };
        assert_eq!(retry_revert, revert_to);
        assert_eq!(retry_sync, 4);
        assert_eq!(
            exex.run_revert_for_test(retry_revert),
            RevertOutcome::Completed,
            "subsequent retry must succeed"
        );
        sync_target.mark_revert_complete(&retry_revert);
        assert_eq!(
            inner.get_latest_block_number().unwrap(),
            Some((1, hash_for_num(1)))
        );
    }

    #[tokio::test]
    async fn skipped_revert_beyond_latest_returns_skipped_without_touching_storage() {
        let dir = tempdir_path();
        let store = Arc::new(MdbxProofsStorage::new(dir.as_path()).expect("env"));
        let proofs: MorphProofsStorage<Arc<MdbxProofsStorage>> = Arc::clone(&store);
        init_storage(proofs.clone());
        store_blocks(1, 3, &proofs);

        let (ctx, _handle) = reth_exex_test_utils::test_exex_context()
            .await
            .expect("exex test context");
        let exex = build_test_exex(ctx, proofs.clone());

        // Revert target above stored `latest` (3) must be reported as skipped (`Ok(false)` /
        // `RevertOutcome::Skipped`) so callers preserve `sync_healthy` instead of treating a
        // no-op revert as proof that storage recovered.
        let revert_above_latest =
            BlockWithParent::new(hash_for_num(4), NumHash::new(5, hash_for_num(5)));
        assert!(
            !exex
                .handle_revert_for_test(revert_above_latest)
                .expect("skip above latest")
        );
        assert_eq!(
            exex.run_revert_for_test(revert_above_latest),
            RevertOutcome::Skipped,
            "no-op revert beyond latest must return Skipped, not Completed"
        );
        assert_eq!(
            proofs.get_latest_block_number().unwrap(),
            Some((3, hash_for_num(3))),
            "skipped revert must not mutate stored tip"
        );

        // By contrast, a revert within stored history executes the unwind and returns `Ok(true)`.
        let revert_in_range =
            BlockWithParent::new(hash_for_num(1), NumHash::new(2, hash_for_num(2)));
        assert!(
            exex.handle_revert_for_test(revert_in_range)
                .expect("unwind in range")
        );
        assert_eq!(
            proofs.get_latest_block_number().unwrap(),
            Some((1, hash_for_num(1)))
        );
    }
}
