//! Background canonical-cursor runtime for the Morph transaction reference index.

use crate::{
    CanonicalChain, DEFAULT_BACKFILL_BATCH_BLOCKS, LIVE_LAG_SLO, ReferenceIndexError,
    ReferenceIndexHandle, ReferenceIndexPhase, ReferenceIndexProgress,
    backfill::{commit_canonical_batch, resolve_jade_first_block},
    db::ReferenceIndexDb,
    reconcile::rollback_suffix,
    writer::clear_jade_first_block_number,
};
use alloy_primitives::B256;
use morph_primitives::MorphPrimitives;
use reth_chain_state::{CanonStateNotification, CanonStateNotifications};
use reth_db_api::transaction::DbTx;
use reth_tasks::TaskExecutor;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::{
    broadcast::{
        Receiver,
        error::{RecvError, TryRecvError},
    },
    watch,
};
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

const RAPID_SYNC_BLOCK_THRESHOLD: u64 = 64;
const RAPID_SYNC_WINDOW: Duration = Duration::from_secs(2);
const RECONCILE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);
const TARGET: &str = "morph::reference_index";

#[derive(Debug)]
struct RapidSyncDetector {
    window_started: Instant,
    blocks_in_window: u64,
}

impl RapidSyncDetector {
    fn new(now: Instant) -> Self {
        Self {
            window_started: now,
            blocks_in_window: 0,
        }
    }

    fn reset_expired_window(&mut self, now: Instant) {
        if now.saturating_duration_since(self.window_started) >= RAPID_SYNC_WINDOW {
            self.window_started = now;
            self.blocks_in_window = 0;
        }
    }

    fn record_blocks(&mut self, now: Instant, blocks: usize) {
        self.reset_expired_window(now);
        self.blocks_in_window = self.blocks_in_window.saturating_add(blocks as u64);
    }

    fn record_lagged(&mut self, now: Instant) {
        self.reset_expired_window(now);
        self.blocks_in_window = RAPID_SYNC_BLOCK_THRESHOLD + 1;
    }

    fn should_defer(&mut self, now: Instant) -> bool {
        self.reset_expired_window(now);
        self.blocks_in_window > RAPID_SYNC_BLOCK_THRESHOLD
    }
}

#[derive(Debug)]
enum PendingBroadcastEvent<T> {
    Item(T),
    Lagged(u64),
}

/// Drain notifications already queued before starting another blocking batch.
fn drain_ready_broadcast<T: Clone>(
    receiver: &mut Receiver<T>,
    mut on_event: impl FnMut(PendingBroadcastEvent<T>),
) -> bool {
    loop {
        match receiver.try_recv() {
            Ok(item) => on_event(PendingBroadcastEvent::Item(item)),
            Err(TryRecvError::Lagged(skipped)) => {
                on_event(PendingBroadcastEvent::Lagged(skipped));
            }
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Closed) => return false,
        }
    }
}

fn observe_canonical_notification(
    notification: CanonStateNotification<MorphPrimitives>,
    detector: &mut RapidSyncDetector,
) {
    let changed_blocks =
        notification.committed().len() + notification.reverted().map_or(0, |chain| chain.len());
    detector.record_blocks(Instant::now(), changed_blocks);
    debug!(target: TARGET, changed_blocks, "canonical notification woke reference index");
}

fn observe_lagged_notification(skipped: u64, detector: &mut RapidSyncDetector) {
    detector.record_lagged(Instant::now());
    warn!(target: TARGET, skipped, "reference index lagged canonical notifications; reconciling from provider");
}

/// Immutable runtime configuration derived from the node configuration.
#[derive(Debug, Clone)]
pub struct ReferenceIndexConfig {
    path: PathBuf,
    chain_id: u64,
    genesis_hash: B256,
    jade_timestamp: u64,
}

impl ReferenceIndexConfig {
    /// Create the immutable identity and activation configuration for one index DB.
    pub fn new(
        path: impl AsRef<Path>,
        chain_id: u64,
        genesis_hash: B256,
        jade_timestamp: u64,
    ) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            chain_id,
            genesis_hash,
            jade_timestamp,
        }
    }
}

/// Single-writer runtime that reconciles a durable cursor with the canonical chain.
pub struct ReferenceIndexRuntime<C> {
    config: ReferenceIndexConfig,
    chain: C,
    handle: ReferenceIndexHandle,
    progress_tx: watch::Sender<ReferenceIndexProgress>,
}

impl<C: CanonicalChain> ReferenceIndexRuntime<C> {
    /// Create a single-writer runtime and its cloneable RPC query handle.
    pub fn new(config: ReferenceIndexConfig, chain: C) -> (Self, ReferenceIndexHandle) {
        let (handle, progress_tx) = ReferenceIndexHandle::new_with_progress(config.jade_timestamp);
        (
            Self {
                config,
                chain,
                handle: handle.clone(),
                progress_tx,
            },
            handle,
        )
    }

    fn publish_progress(&self, phase: ReferenceIndexPhase, indexed_to: Option<u64>) {
        self.handle.set_phase(phase);
        let _ = self.progress_tx.send(ReferenceIndexProgress {
            phase,
            indexed_to,
            unavailable: phase == ReferenceIndexPhase::Unavailable,
        });
    }

    fn set_phase(&self, phase: ReferenceIndexPhase) {
        let current = *self.progress_tx.borrow();
        let changed = current.phase != phase;
        self.handle.set_phase(phase);
        if changed {
            let _ = self.progress_tx.send(ReferenceIndexProgress {
                phase,
                indexed_to: current.indexed_to,
                unavailable: phase == ReferenceIndexPhase::Unavailable,
            });
        }
    }

    fn set_indexed_to(&self, indexed_to: Option<u64>) {
        let phase = self.handle.phase();
        let _ = self.progress_tx.send(ReferenceIndexProgress {
            phase,
            indexed_to,
            unavailable: phase == ReferenceIndexPhase::Unavailable,
        });
    }

    fn open_db(&self) -> Result<ReferenceIndexDb, ReferenceIndexError> {
        if let Some(db) = self.handle.db() {
            return Ok(db);
        }
        let db = ReferenceIndexDb::open(
            &self.config.path,
            self.config.chain_id,
            self.config.genesis_hash,
        )?;
        let indexed_to = db.indexed_to()?;
        self.handle.install_db(db.clone());
        self.set_indexed_to(indexed_to);
        Ok(db)
    }

    fn reconcile_cursor(
        &self,
        db: &ReferenceIndexDb,
        jade_first_block: u64,
        head: crate::CanonicalTip,
    ) -> Result<(), ReferenceIndexError> {
        let Some(cursor) = db.indexed_to()? else {
            return Ok(());
        };

        let lowest_retained = db.lowest_indexed_block_number()?.ok_or_else(|| {
            ReferenceIndexError::CorruptMetadata(
                "durable cursor exists without any IndexedBlocks entries",
            )
        })?;
        if lowest_retained > cursor {
            return Err(ReferenceIndexError::CorruptMetadata(
                "lowest IndexedBlocks entry exceeds durable cursor",
            ));
        }

        if head.number < lowest_retained {
            self.set_phase(ReferenceIndexPhase::Repairing);
            self.handle.metrics().reorgs_total.increment(1);
            if jade_first_block > 0 && lowest_retained <= jade_first_block {
                rollback_suffix(db, None, jade_first_block, DEFAULT_BACKFILL_BATCH_BLOCKS)?;
                self.set_indexed_to(None);
                return Ok(());
            }
            return Err(ReferenceIndexError::ManualRebuildRequired { jade_first_block });
        }

        let candidate = cursor.min(head.number);
        let indexed_hash = db.indexed_block_hash(candidate)?.ok_or_else(|| {
            ReferenceIndexError::CorruptMetadata(
                "durable cursor range is missing an IndexedBlocks hash",
            )
        })?;
        if cursor == head.number && indexed_hash == head.hash {
            return Ok(());
        }
        if cursor < head.number && self.chain.canonical_hash(cursor)? == Some(indexed_hash) {
            return Ok(());
        }

        self.set_phase(ReferenceIndexPhase::Repairing);
        self.handle.metrics().reorgs_total.increment(1);
        // Bound the rewind by the lowest retained `IndexedBlocks` breadcrumb
        // (floored by `jade_first_block`) rather than live canonical `finalized`.
        // An offline or `Deferred` node catching up across an already-finalized
        // replacement chain still retains its unpruned breadcrumbs and can rewind
        // cleanly to its common ancestor instead of requiring a manual rebuild.
        let rewind_floor = lowest_retained.max(jade_first_block).min(candidate);
        let mut ancestor = None;
        let mut number = candidate;
        loop {
            let indexed_hash = db.indexed_block_hash(number)?.ok_or_else(|| {
                ReferenceIndexError::CorruptMetadata(
                    "indexed canonical range contains a block-hash gap",
                )
            })?;
            if self.chain.canonical_hash(number)? == Some(indexed_hash) {
                ancestor = Some(number);
                break;
            }
            if number == rewind_floor {
                break;
            }
            number -= 1;
        }

        if let Some(ancestor) = ancestor {
            rollback_suffix(
                db,
                Some(ancestor),
                jade_first_block,
                DEFAULT_BACKFILL_BATCH_BLOCKS,
            )?;
            self.set_indexed_to(Some(ancestor));
            return Ok(());
        }

        // Pre-finality reorg touching `jade_first_block` itself: if Jade did not
        // activate at genesis and `jade_first_block` has not been pruned, roll back
        // all post-Jade blocks and clear `IndexedTo` + `JadeFirstBlockNumber` so the
        // caller re-resolves Jade activation against the replacement canonical chain.
        if jade_first_block > 0 && lowest_retained <= jade_first_block {
            rollback_suffix(db, None, jade_first_block, DEFAULT_BACKFILL_BATCH_BLOCKS)?;
            self.set_indexed_to(None);
            return Ok(());
        }

        Err(ReferenceIndexError::ManualRebuildRequired { jade_first_block })
    }

    /// Perform one synchronous reconciliation/backfill turn.
    pub fn synchronize_once(&mut self, defer: bool) -> Result<(), ReferenceIndexError> {
        let result = self.synchronize_once_inner(defer);
        if let Err(error) = &result {
            self.handle.metrics().failures_total.increment(1);
            // Only unrecoverable failures gate the read path. `run` retries every
            // other error with backoff, so leaving the phase untouched lets the
            // cursor-vs-tip comparison surface `IndexBehind` — which tells clients
            // to retry — instead of `IndexUnavailable`, which tells them to stop.
            if error.requires_manual_rebuild() {
                self.set_phase(ReferenceIndexPhase::Unavailable);
            }
        }
        result
    }

    fn synchronize_once_inner(&mut self, defer: bool) -> Result<(), ReferenceIndexError> {
        let db = self.open_db()?;
        let head = self.chain.head()?;
        if head.timestamp < self.config.jade_timestamp {
            let indexed_to = db.indexed_to()?;
            let has_indexed_cursor = indexed_to.is_some();
            let has_jade_first = db.jade_first_block_number()?.is_some();
            if defer && (has_indexed_cursor || has_jade_first) {
                self.handle.metrics().lag_blocks.set(0.0);
                self.publish_progress(ReferenceIndexPhase::Deferred, indexed_to);
                return Ok(());
            }
            if has_indexed_cursor {
                let jade_first_block = db.jade_first_block_number()?.ok_or_else(|| {
                    ReferenceIndexError::CorruptMetadata(
                        "durable cursor exists without JadeFirstBlockNumber",
                    )
                })?;
                let lowest_retained = db.lowest_indexed_block_number()?.ok_or_else(|| {
                    ReferenceIndexError::CorruptMetadata(
                        "durable cursor exists without any IndexedBlocks entries",
                    )
                })?;
                self.set_phase(ReferenceIndexPhase::Repairing);
                self.handle.metrics().reorgs_total.increment(1);
                if jade_first_block > 0 && lowest_retained <= jade_first_block {
                    rollback_suffix(&db, None, jade_first_block, DEFAULT_BACKFILL_BATCH_BLOCKS)?;
                    self.set_indexed_to(None);
                } else {
                    return Err(ReferenceIndexError::ManualRebuildRequired { jade_first_block });
                }
            } else if has_jade_first {
                let tx = db.tx_mut()?;
                clear_jade_first_block_number(&tx)?;
                tx.commit()?;
            }
            self.handle.metrics().lag_blocks.set(0.0);
            self.publish_progress(ReferenceIndexPhase::PreJade, None);
            return Ok(());
        }
        if defer {
            let indexed_to = db.indexed_to()?;
            self.handle
                .metrics()
                .lag_blocks
                .set(head.number.saturating_sub(indexed_to.unwrap_or_default()) as f64);
            self.publish_progress(ReferenceIndexPhase::Deferred, indexed_to);
            return Ok(());
        }

        if db.indexed_to()?.is_some() {
            let stored_jade_first = db.jade_first_block_number()?.ok_or_else(|| {
                ReferenceIndexError::CorruptMetadata(
                    "durable cursor exists without JadeFirstBlockNumber",
                )
            })?;
            self.reconcile_cursor(&db, stored_jade_first, head)?;
        }

        let cursor = db.indexed_to()?;
        let jade_first_block = match (cursor, db.jade_first_block_number()?) {
            (Some(_), Some(block)) => block,
            (Some(_), None) => {
                return Err(ReferenceIndexError::CorruptMetadata(
                    "durable cursor exists without JadeFirstBlockNumber",
                ));
            }
            (None, _) => resolve_jade_first_block(&self.chain, head, self.config.jade_timestamp)?
                .ok_or_else(|| {
                ReferenceIndexError::Other(eyre::eyre!(
                    "Jade is active at the canonical head but its first block was not found"
                ))
            })?,
        };
        let finalized = self.chain.finalized_block_number()?;
        let start = cursor.map_or(jade_first_block, |cursor| cursor.saturating_add(1));
        if start > head.number {
            self.handle.metrics().lag_blocks.set(0.0);
            self.publish_progress(ReferenceIndexPhase::Live, cursor);
            return Ok(());
        }

        self.publish_progress(ReferenceIndexPhase::Backfill, cursor);
        let end = start
            .saturating_add(DEFAULT_BACKFILL_BATCH_BLOCKS - 1)
            .min(head.number);
        let blocks = self.chain.canonical_blocks(start..=end)?;
        let expected_len = end.saturating_sub(start).saturating_add(1) as usize;
        if blocks.len() != expected_len {
            return Err(ReferenceIndexError::Other(eyre::eyre!(
                "canonical block range {start}..={end} returned {} of {expected_len} blocks",
                blocks.len()
            )));
        }

        // Verify batch parent linkage against the durable cursor (or Jade
        // activation boundary for the initial batch) so a TOCTOU reorg between
        // `reconcile_cursor` and `canonical_blocks` can never splice a new
        // branch onto an orphaned indexed prefix.
        if let Some(cursor) = cursor {
            let indexed_cursor_hash = db.indexed_block_hash(cursor)?.ok_or_else(|| {
                ReferenceIndexError::CorruptMetadata(
                    "durable cursor is missing an IndexedBlocks hash",
                )
            })?;
            if blocks[0].parent_hash != indexed_cursor_hash {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical chain reorged before backfill batch at {start}: parent hash {} does not match indexed cursor {cursor} ({indexed_cursor_hash})",
                    blocks[0].parent_hash
                )));
            }
        } else {
            if blocks[0].timestamp < self.config.jade_timestamp {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical chain shifted Jade activation past block {start} while reading first batch"
                )));
            }
            if start > 0 {
                let prev = start - 1;
                let prev_hash_before = self.chain.canonical_hash(prev)?;
                let prev_timestamp = self.chain.block_timestamp(prev)?;
                let prev_hash_after = self.chain.canonical_hash(prev)?;
                if prev_hash_before != Some(blocks[0].parent_hash)
                    || prev_hash_after != Some(blocks[0].parent_hash)
                {
                    return Err(ReferenceIndexError::Other(eyre::eyre!(
                        "canonical predecessor {prev} changed while reading first Jade batch at {start}"
                    )));
                }
                if prev_timestamp.is_none_or(|ts| ts >= self.config.jade_timestamp) {
                    return Err(ReferenceIndexError::Other(eyre::eyre!(
                        "canonical chain shifted Jade activation earlier than block {start} while reading first batch"
                    )));
                }
            }
        }

        for (offset, block) in blocks.iter().enumerate() {
            let expected_number = start + offset as u64;
            if block.number != expected_number {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical block changed while reading backfill batch at {expected_number}"
                )));
            }
            if offset > 0 && block.parent_hash != blocks[offset - 1].hash {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical parent hash mismatch inside backfill batch at {expected_number}"
                )));
            }
            let canonical_hash = self.chain.canonical_hash(expected_number)?;
            if canonical_hash != Some(block.hash) {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical block changed while reading backfill batch at {expected_number}"
                )));
            }
        }
        commit_canonical_batch(&db, jade_first_block, start, &blocks, finalized)?;

        let current_head = self.chain.head()?;
        let lag = current_head.number.saturating_sub(end);
        self.handle.metrics().lag_blocks.set(lag as f64);
        if lag > LIVE_LAG_SLO {
            debug!(target: TARGET, lag, slo = LIVE_LAG_SLO, "reference index remains behind its live-lag SLO");
        }
        let next_phase = if end == current_head.number
            && blocks
                .last()
                .is_some_and(|block| block.hash == current_head.hash)
        {
            ReferenceIndexPhase::Live
        } else {
            ReferenceIndexPhase::Backfill
        };
        self.publish_progress(next_phase, Some(end));
        Ok(())
    }

    /// Run the non-critical background reconciler until node shutdown, or until it
    /// stops in [`ReferenceIndexPhase::Unavailable`].
    ///
    /// Notifications are wake-ups and rate signals only. Canonical data is
    /// always re-read through [`CanonicalChain`] before a durable commit.
    pub async fn run(
        mut self,
        task_executor: TaskExecutor,
        mut notifications: CanonStateNotifications<MorphPrimitives>,
    ) {
        let handle = self.handle.clone();
        let mut poll = tokio::time::interval(RECONCILE_POLL_INTERVAL);
        poll.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // Consume tokio's immediate first tick so a queued canonical
        // notification can classify rapid sync before the first DB batch.
        poll.tick().await;
        let mut detector = RapidSyncDetector::new(Instant::now());
        let mut notifications_open = true;
        let mut sync_requested = false;
        let mut retry_backoff = Duration::from_secs(1);
        let mut logged_phase = handle.phase();

        loop {
            if notifications_open {
                notifications_open = drain_ready_broadcast(&mut notifications, |event| {
                    sync_requested = true;
                    match event {
                        PendingBroadcastEvent::Item(notification) => {
                            observe_canonical_notification(notification, &mut detector);
                        }
                        PendingBroadcastEvent::Lagged(skipped) => {
                            observe_lagged_notification(skipped, &mut detector);
                        }
                    }
                });
                if !notifications_open {
                    warn!(target: TARGET, "canonical notification channel closed; reference index falling back to polling");
                }
            }
            if sync_requested
                && detector.should_defer(Instant::now())
                && handle.phase() == ReferenceIndexPhase::Deferred
            {
                // Stay completely off the index DB for the current rapid-sync
                // window. Polling resumes catch-up as soon as the window resets.
                sync_requested = false;
            }
            if sync_requested {
                let defer = detector.should_defer(Instant::now());
                let job = task_executor.spawn_blocking(move || {
                    let result = self.synchronize_once(defer);
                    (self, result)
                });
                let (returned_runtime, result) = match job.await {
                    Ok(output) => output,
                    Err(error) if error.is_cancelled() => {
                        debug!(target: TARGET, "reference index blocking task cancelled during shutdown");
                        return;
                    }
                    Err(error) => {
                        handle.set_phase(ReferenceIndexPhase::Unavailable);
                        warn!(target: TARGET, %error, "reference index blocking task stopped unexpectedly");
                        return;
                    }
                };
                self = returned_runtime;

                match result {
                    Ok(()) => {
                        retry_backoff = Duration::from_secs(1);
                        let phase = handle.phase();
                        if phase != logged_phase {
                            info!(target: TARGET, ?phase, "reference index phase changed");
                            logged_phase = phase;
                        }
                        sync_requested = phase == ReferenceIndexPhase::Backfill;
                        if sync_requested {
                            tokio::task::yield_now().await;
                            // Loop back through the non-blocking receiver drain before
                            // starting another bounded backfill batch.
                            continue;
                        }
                    }
                    Err(error) => {
                        if error.requires_manual_rebuild() {
                            warn!(target: TARGET, %error, "reference index requires manual rebuild; execution continues");
                            return;
                        }
                        warn!(target: TARGET, %error, ?retry_backoff, "reference index reconciliation failed; execution continues");
                        tokio::time::sleep(retry_backoff).await;
                        retry_backoff = retry_backoff.saturating_mul(2).min(MAX_RETRY_BACKOFF);
                        sync_requested = true;
                        continue;
                    }
                }
            }

            tokio::select! {
                biased;
                notification = notifications.recv(), if notifications_open => {
                    sync_requested = true;
                    match notification {
                        Ok(notification) => {
                            observe_canonical_notification(notification, &mut detector);
                        }
                        Err(RecvError::Lagged(skipped)) => {
                            observe_lagged_notification(skipped, &mut detector);
                        }
                        Err(RecvError::Closed) => {
                            notifications_open = false;
                            warn!(target: TARGET, "canonical notification channel closed; reference index falling back to polling");
                        }
                    }
                }
                _ = poll.tick() => {
                    sync_requested = true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CanonicalBlock, CanonicalChain, CanonicalTip, ReferenceQuery};
    use alloy_primitives::B256;
    use parking_lot::RwLock;
    use std::{
        ops::RangeInclusive,
        sync::Arc,
        time::{Duration, Instant},
    };

    #[test]
    fn rapid_sync_detector_defers_after_more_than_64_blocks_in_one_window() {
        let start = Instant::now();
        let mut detector = RapidSyncDetector::new(start);

        detector.record_blocks(start + Duration::from_secs(1), 64);
        assert!(!detector.should_defer(start + Duration::from_secs(1)));

        detector.record_blocks(start + Duration::from_millis(1500), 1);
        assert!(detector.should_defer(start + Duration::from_millis(1999)));
        assert!(!detector.should_defer(start + Duration::from_millis(2001)));
    }

    #[test]
    fn rapid_sync_detector_does_not_mix_adjacent_windows() {
        let start = Instant::now();
        let mut detector = RapidSyncDetector::new(start);

        detector.record_blocks(start + Duration::from_millis(1900), 32);
        detector.record_blocks(start + Duration::from_millis(2100), 33);

        assert!(!detector.should_defer(start + Duration::from_millis(2100)));
    }

    #[test]
    fn normal_block_activity_does_not_extend_rapid_sync_deferral() {
        let start = Instant::now();
        let mut detector = RapidSyncDetector::new(start);

        detector.record_blocks(start, 65);
        assert!(detector.should_defer(start + Duration::from_secs(1)));

        detector.record_blocks(start + Duration::from_secs(1), 1);

        assert!(!detector.should_defer(start + Duration::from_millis(2001)));
    }

    #[test]
    fn lagged_notifications_defer_only_for_the_current_window() {
        let start = Instant::now();
        let mut detector = RapidSyncDetector::new(start);

        detector.record_lagged(start);

        assert!(detector.should_defer(start + Duration::from_secs(1)));
        assert!(!detector.should_defer(start + Duration::from_millis(2001)));
    }

    #[test]
    fn queued_notifications_are_drained_before_the_next_batch() {
        let (sender, mut receiver) = tokio::sync::broadcast::channel(128);
        for _ in 0..65 {
            sender.send(1usize).unwrap();
        }
        let now = Instant::now();
        let mut detector = RapidSyncDetector::new(now);
        let mut received = 0usize;

        let remains_open = drain_ready_broadcast(&mut receiver, |event| match event {
            PendingBroadcastEvent::Item(blocks) => {
                received += 1;
                detector.record_blocks(now, blocks);
            }
            PendingBroadcastEvent::Lagged(_) => panic!("receiver should not lag"),
        });

        assert!(remains_open);
        assert_eq!(received, 65);
        assert!(detector.should_defer(now));
    }

    #[derive(Clone, Debug)]
    struct TestChain {
        blocks: Arc<RwLock<Vec<CanonicalBlock>>>,
        finalized: Arc<RwLock<Option<u64>>>,
        generation: Arc<RwLock<u8>>,
    }

    impl TestChain {
        fn make_hash(number: u64, generation: u8) -> B256 {
            let mut bytes = [0u8; 32];
            bytes[0] = generation;
            bytes[24..].copy_from_slice(&number.to_be_bytes());
            B256::from(bytes)
        }

        fn linear(count: u64, first_timestamp: u64) -> Self {
            let mut blocks = Vec::with_capacity(count as usize);
            let mut parent_hash = B256::ZERO;
            for number in 0..count {
                let hash = Self::make_hash(number, 0);
                blocks.push(CanonicalBlock {
                    number,
                    hash,
                    parent_hash,
                    timestamp: first_timestamp + number,
                    transactions: Vec::new(),
                });
                parent_hash = hash;
            }
            Self {
                blocks: Arc::new(RwLock::new(blocks)),
                finalized: Arc::new(RwLock::new(None)),
                generation: Arc::new(RwLock::new(0)),
            }
        }

        fn push_block(&self, timestamp: u64) {
            let mut blocks = self.blocks.write();
            let generation = *self.generation.read();
            let number = blocks.len() as u64;
            let parent_hash = blocks.last().map_or(B256::ZERO, |block| block.hash);
            let hash = Self::make_hash(number, generation);
            blocks.push(CanonicalBlock {
                number,
                hash,
                parent_hash,
                timestamp,
                transactions: Vec::new(),
            });
        }

        fn replace_suffix(&self, from: u64) {
            let mut generation_guard = self.generation.write();
            *generation_guard = generation_guard.wrapping_add(1).max(1);
            let generation = *generation_guard;
            let mut blocks = self.blocks.write();
            let mut parent_hash = if from == 0 {
                B256::ZERO
            } else {
                blocks[from as usize - 1].hash
            };
            for block in blocks.iter_mut().skip(from as usize) {
                block.parent_hash = parent_hash;
                block.hash = Self::make_hash(block.number, generation);
                parent_hash = block.hash;
            }
        }

        fn replace_suffix_with_timestamps(&self, from: u64, timestamps: &[u64]) {
            let mut generation_guard = self.generation.write();
            *generation_guard = generation_guard.wrapping_add(1).max(1);
            let generation = *generation_guard;
            let mut blocks = self.blocks.write();
            blocks.truncate(from as usize);
            let mut parent_hash = blocks.last().map_or(B256::ZERO, |block| block.hash);
            for (offset, &timestamp) in timestamps.iter().enumerate() {
                let number = from + offset as u64;
                let hash = Self::make_hash(number, generation);
                blocks.push(CanonicalBlock {
                    number,
                    hash,
                    parent_hash,
                    timestamp,
                    transactions: Vec::new(),
                });
                parent_hash = hash;
            }
        }

        fn truncate(&self, len: u64) {
            self.blocks.write().truncate(len as usize);
        }

        fn set_finalized(&self, number: u64) {
            *self.finalized.write() = Some(number);
        }
    }

    impl CanonicalChain for TestChain {
        fn head(&self) -> Result<CanonicalTip, crate::ReferenceIndexError> {
            let blocks = self.blocks.read();
            let block = blocks.last().unwrap();
            Ok(CanonicalTip {
                number: block.number,
                hash: block.hash,
                timestamp: block.timestamp,
            })
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, crate::ReferenceIndexError> {
            Ok(self
                .blocks
                .read()
                .get(number as usize)
                .map(|block| block.hash))
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, crate::ReferenceIndexError> {
            Ok(self
                .blocks
                .read()
                .get(number as usize)
                .map(|block| block.timestamp))
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, crate::ReferenceIndexError> {
            Ok(*self.finalized.read())
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, crate::ReferenceIndexError> {
            let blocks = self.blocks.read();
            Ok(range
                .filter_map(|number| blocks.get(number as usize).cloned())
                .collect())
        }
    }

    #[test]
    fn pre_jade_sync_serves_empty_results_without_a_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 100);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), crate::ReferenceIndexPhase::PreJade);
        // Pre-Jade is answered as a complete empty result at the RPC layer via
        // `is_pre_jade`; the reader never sees the query and no cursor is written.
        assert!(handle.is_pre_jade(chain.head().unwrap().timestamp));
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
    }

    #[test]
    fn post_jade_backfill_commits_at_most_512_blocks_per_turn() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(600, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), crate::ReferenceIndexPhase::Backfill);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(511));

        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), crate::ReferenceIndexPhase::Live);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(599));

        let query = ReferenceQuery::new(B256::with_last_byte(0xaa), None, None).unwrap();
        assert!(
            handle
                .query_at(query, chain.head().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn an_idle_reconciliation_keeps_the_live_snapshot_stable() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);

        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        // A no-op reconciliation still serves reads at the unchanged tip.
        let query = ReferenceQuery::new(B256::with_last_byte(0xaa), None, None).unwrap();
        assert!(
            handle
                .query_at(query, chain.head().unwrap())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reorged_suffix_is_rolled_back_and_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), crate::ReferenceIndexPhase::Live);

        chain.replace_suffix(7);
        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), crate::ReferenceIndexPhase::Live);
        let query = ReferenceQuery::new(B256::with_last_byte(0xaa), None, None).unwrap();
        assert!(
            handle
                .query_at(query, chain.head().unwrap())
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            handle.db().unwrap().indexed_block_hash(7).unwrap(),
            chain.canonical_hash(7).unwrap()
        );
    }

    #[test]
    fn finalized_prunes_breadcrumbs_below_it_and_repairs_a_suffix_above_it() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        // Without finality yet, every breadcrumb back to the Jade start is kept.
        assert!(
            handle
                .db()
                .unwrap()
                .indexed_block_hash(0)
                .unwrap()
                .is_some()
        );

        // Finalize block 5, then reorg a suffix strictly above it.
        chain.set_finalized(5);
        chain.replace_suffix(7);
        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        // The suffix above finalized is repaired to the canonical hashes.
        assert_eq!(
            db.indexed_block_hash(7).unwrap(),
            chain.canonical_hash(7).unwrap()
        );
        // Breadcrumbs below finalized are pruned; finalized and above survive.
        assert!(db.indexed_block_hash(4).unwrap().is_none());
        assert!(db.indexed_block_hash(5).unwrap().is_some());
    }

    #[test]
    fn reorg_reaching_below_pruned_finalized_floor_requires_manual_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);

        // Finalize block 5 and commit one more block so breadcrumbs `0..=4` are
        // pruned from `IndexedBlocks` (raising the retained rewind floor to 5).
        chain.set_finalized(5);
        chain.push_block(210);
        runtime.synchronize_once(false).unwrap();
        assert_eq!(
            handle.db().unwrap().lowest_indexed_block_number().unwrap(),
            Some(5)
        );

        // Now diverge below the pruned finalized floor: the common ancestor is
        // below retained breadcrumbs, so reconciliation must surface
        // `ManualRebuildRequired` instead of silently repairing corruption.
        chain.replace_suffix(3);
        let error = runtime.synchronize_once(false).unwrap_err();

        assert!(matches!(
            error,
            ReferenceIndexError::ManualRebuildRequired { .. }
        ));
        assert_eq!(handle.phase(), ReferenceIndexPhase::Unavailable);
    }

    #[test]
    fn offline_catchup_rewinds_across_live_finalized_when_breadcrumbs_retained() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        // Node indexes blocks 0..=9 on an unfinalized fork, retaining all
        // breadcrumbs `0..=9`.
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(
            handle.db().unwrap().lowest_indexed_block_number().unwrap(),
            Some(0)
        );

        // While the node was offline or `Deferred`, the canonical chain reorged
        // from block 3 and finalized block 5 on the replacement branch.
        chain.replace_suffix(3);
        chain.set_finalized(5);
        chain.push_block(210);

        // Because breadcrumbs `0..=4` were never pruned in our local DB, the
        // rewind floor is derived from retained breadcrumbs rather than live
        // canonical `finalized`, allowing a clean recovery to ancestor 2.
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        assert_eq!(db.indexed_to().unwrap(), Some(10));
        assert_eq!(
            db.indexed_block_hash(5).unwrap(),
            chain.canonical_hash(5).unwrap()
        );
        assert_eq!(
            db.indexed_block_hash(10).unwrap(),
            chain.canonical_hash(10).unwrap()
        );
        // After committing the replacement chain past finalized=5, breadcrumbs
        // strictly below 5 are pruned.
        assert!(db.indexed_block_hash(4).unwrap().is_none());
        assert!(db.indexed_block_hash(5).unwrap().is_some());
    }

    #[test]
    fn pure_revert_rolls_the_cursor_back_to_the_new_head() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();

        chain.truncate(7);
        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(6));
        assert_eq!(
            handle.db().unwrap().indexed_block_hash(6).unwrap(),
            chain.canonical_hash(6).unwrap()
        );
    }

    #[test]
    fn reorg_without_a_post_jade_ancestor_requires_a_manual_rebuild() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        runtime.synchronize_once(false).unwrap();

        chain.replace_suffix(0);
        let error = runtime.synchronize_once(false).unwrap_err();

        assert!(matches!(
            error,
            ReferenceIndexError::ManualRebuildRequired {
                jade_first_block: 0
            }
        ));
        assert_eq!(handle.phase(), ReferenceIndexPhase::Unavailable);
        assert!(
            dir.path().exists(),
            "runtime must never delete the index DB"
        );
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(9));
    }

    #[derive(Clone, Debug)]
    struct ShortRangeChain(TestChain);

    impl CanonicalChain for ShortRangeChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            self.0.head()
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            self.0.canonical_hash(number)
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            self.0.block_timestamp(number)
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            self.0.finalized_block_number()
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            let mut blocks = self.0.canonical_blocks(range)?;
            blocks.pop();
            Ok(blocks)
        }
    }

    #[test]
    fn incomplete_provider_batch_does_not_advance_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let chain = ShortRangeChain(TestChain::linear(10, 200));
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);

        let error = runtime.synchronize_once(false).unwrap_err();

        assert!(error.to_string().contains("returned 9 of 10 blocks"));
        // A short provider batch is transient: `run` retries it with backoff, so
        // the phase must stay where the turn left it rather than latch to
        // `Unavailable`, which would gate the read path for the whole window.
        assert_eq!(handle.phase(), ReferenceIndexPhase::Backfill);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
    }

    #[test]
    fn transient_failure_leaves_queries_retryable() {
        let dir = tempfile::tempdir().unwrap();
        let chain = ShortRangeChain(TestChain::linear(10, 200));
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap_err();

        // The read path must answer `IndexBehind` (retry) and not
        // `IndexUnavailable` (stop retrying) while catch-up is still pending.
        let query = crate::ReferenceQuery::new(B256::ZERO, None, None).unwrap();
        assert!(matches!(
            handle.query_at(query, chain.0.head().unwrap()),
            Err(ReferenceIndexError::IndexBehind)
        ));
    }

    #[test]
    fn deferred_sync_performs_no_per_block_index_writes() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);

        runtime.synchronize_once(true).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Deferred);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
        assert_eq!(handle.db().unwrap().indexed_block_hash(0).unwrap(), None);
    }

    #[derive(Clone, Debug)]
    struct ReorgOnBatchReadChain {
        inner: TestChain,
        reorg_from: u64,
        triggered: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CanonicalChain for ReorgOnBatchReadChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            self.inner.head()
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            self.inner.canonical_hash(number)
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            self.inner.block_timestamp(number)
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            self.inner.finalized_block_number()
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            if !self
                .triggered
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // Simulate a TOCTOU reorg occurring strictly between
                // `reconcile_cursor` and `canonical_blocks`.
                self.inner.replace_suffix(self.reorg_from);
            }
            self.inner.canonical_blocks(range)
        }
    }

    #[test]
    fn toctou_reorg_between_reconcile_and_batch_read_aborts_without_splicing() {
        let dir = tempfile::tempdir().unwrap();
        let inner = TestChain::linear(5, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let chain = ReorgOnBatchReadChain {
            inner: inner.clone(),
            reorg_from: 3,
            // Start armed=true for initial sync, then re-arm before the second turn.
            triggered: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let original_block_4 = inner.canonical_hash(4).unwrap().unwrap();
        assert_eq!(
            handle.db().unwrap().indexed_block_hash(4).unwrap(),
            Some(original_block_4)
        );

        // Append block 5 on the original chain and arm the TOCTOU reorg at block 3
        // to fire right when `canonical_blocks(5..=5)` is invoked.
        inner.push_block(205);
        chain
            .triggered
            .store(false, std::sync::atomic::Ordering::SeqCst);

        let error = runtime.synchronize_once(false).unwrap_err();
        assert!(
            !error.requires_manual_rebuild(),
            "TOCTOU reorg must abort as a transient error: {error}"
        );
        assert!(error.to_string().contains("parent hash"));
        // Cursor must NOT have advanced with an orphaned splice at block 5.
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(4));
        assert_eq!(handle.db().unwrap().indexed_block_hash(5).unwrap(), None);
        assert_eq!(
            handle.db().unwrap().indexed_block_hash(4).unwrap(),
            Some(original_block_4)
        );

        // The next turn reconciles the reorg from block 3 and commits the true
        // canonical suffix cleanly.
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        assert_eq!(db.indexed_to().unwrap(), Some(5));
        for n in 0..=5 {
            assert_eq!(
                db.indexed_block_hash(n).unwrap(),
                inner.canonical_hash(n).unwrap()
            );
        }
    }

    #[derive(Clone, Debug)]
    struct BrokenParentLinkChain(TestChain);

    impl CanonicalChain for BrokenParentLinkChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            self.0.head()
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            self.0.canonical_hash(number)
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            self.0.block_timestamp(number)
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            self.0.finalized_block_number()
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            let mut blocks = self.0.canonical_blocks(range)?;
            if blocks.len() >= 2 {
                blocks[1].parent_hash = B256::repeat_byte(0xee);
            }
            Ok(blocks)
        }
    }

    #[test]
    fn non_linked_batch_is_rejected_without_committing() {
        let dir = tempfile::tempdir().unwrap();
        let chain = BrokenParentLinkChain(TestChain::linear(5, 200));
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);

        let error = runtime.synchronize_once(false).unwrap_err();
        assert!(error.to_string().contains("parent hash mismatch"));
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
    }

    #[derive(Clone, Debug)]
    struct ShiftJadeOnFirstBatchChain {
        inner: TestChain,
        triggered: Arc<std::sync::atomic::AtomicBool>,
    }

    impl CanonicalChain for ShiftJadeOnFirstBatchChain {
        fn head(&self) -> Result<CanonicalTip, ReferenceIndexError> {
            self.inner.head()
        }

        fn canonical_hash(&self, number: u64) -> Result<Option<B256>, ReferenceIndexError> {
            self.inner.canonical_hash(number)
        }

        fn block_timestamp(&self, number: u64) -> Result<Option<u64>, ReferenceIndexError> {
            self.inner.block_timestamp(number)
        }

        fn finalized_block_number(&self) -> Result<Option<u64>, ReferenceIndexError> {
            self.inner.finalized_block_number()
        }

        fn canonical_blocks(
            &self,
            range: RangeInclusive<u64>,
        ) -> Result<Vec<CanonicalBlock>, ReferenceIndexError> {
            if !self
                .triggered
                .swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                // Shift Jade activation earlier (from block 5 to block 4) right
                // before the first batch is read.
                self.inner
                    .replace_suffix_with_timestamps(4, &[205, 206, 207, 208, 209, 210]);
            }
            self.inner.canonical_blocks(range)
        }
    }

    #[test]
    fn first_jade_batch_aborts_if_activation_boundary_shifts_during_read() {
        let dir = tempfile::tempdir().unwrap();
        // Blocks 0..=9 have timestamps 200..=209; Jade activates at 205 (block 5).
        let inner = TestChain::linear(10, 200);
        let chain = ShiftJadeOnFirstBatchChain {
            inner: inner.clone(),
            triggered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 205);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);

        let error = runtime.synchronize_once(false).unwrap_err();
        assert!(!error.requires_manual_rebuild());
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
        assert_eq!(
            handle.db().unwrap().jade_first_block_number().unwrap(),
            None
        );

        // Next turn resolves the new Jade activation block (4) and syncs cleanly.
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        assert_eq!(db.jade_first_block_number().unwrap(), Some(4));
        assert_eq!(db.indexed_to().unwrap(), Some(9));
        for n in 4..=9 {
            assert_eq!(
                db.indexed_block_hash(n).unwrap(),
                inner.canonical_hash(n).unwrap()
            );
        }
    }

    #[test]
    fn pre_finality_reorg_touching_jade_first_block_rolls_back_and_re_resolves() {
        let dir = tempfile::tempdir().unwrap();
        // Blocks 0..=9 have timestamps 200..=209; Jade activates at 205 (block 5).
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 205);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(
            handle.db().unwrap().jade_first_block_number().unwrap(),
            Some(5)
        );
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(9));

        // Pre-finality reorg replaces blocks 3..=9 so Jade now activates earlier at block 3.
        chain.replace_suffix_with_timestamps(3, &[205, 206, 207, 208, 209, 210, 211]);
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        assert_eq!(db.jade_first_block_number().unwrap(), Some(3));
        assert_eq!(db.indexed_to().unwrap(), Some(9));
        for n in 3..=9 {
            assert_eq!(
                db.indexed_block_hash(n).unwrap(),
                chain.canonical_hash(n).unwrap()
            );
        }

        // Another pre-finality reorg shifts Jade later to block 7.
        chain.replace_suffix_with_timestamps(3, &[201, 202, 203, 204, 205, 206, 207]);
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(db.jade_first_block_number().unwrap(), Some(7));
        assert_eq!(db.indexed_to().unwrap(), Some(9));
        for n in 3..=6 {
            assert_eq!(db.indexed_block_hash(n).unwrap(), None);
        }
        for n in 7..=9 {
            assert_eq!(
                db.indexed_block_hash(n).unwrap(),
                chain.canonical_hash(n).unwrap()
            );
        }
    }

    #[test]
    fn pre_finality_reorg_shorter_than_old_jade_first_block_recovers() {
        let dir = tempfile::tempdir().unwrap();
        // Blocks 0..=9 have timestamps 200..=209; Jade activates at 205 (block 5).
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 205);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();
        assert_eq!(
            handle.db().unwrap().jade_first_block_number().unwrap(),
            Some(5)
        );
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(9));

        // Replacement chain activates Jade at block 3 and currently ends at block 4
        // (`head.number < old_jade_first_block`).
        chain.replace_suffix_with_timestamps(3, &[205, 206]);
        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        let db = handle.db().unwrap();
        assert_eq!(db.jade_first_block_number().unwrap(), Some(3));
        assert_eq!(db.indexed_to().unwrap(), Some(4));
        for n in 3..=4 {
            assert_eq!(
                db.indexed_block_hash(n).unwrap(),
                chain.canonical_hash(n).unwrap()
            );
        }
        for n in 5..=9 {
            assert_eq!(db.indexed_block_hash(n).unwrap(), None);
        }
    }

    #[test]
    fn pure_revert_back_to_pre_jade_rolls_back_and_allows_reactivation() {
        let dir = tempfile::tempdir().unwrap();
        // Blocks 0..=9 have timestamps 200..=209; Jade activates at 205 (block 5).
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 205);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());

        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(
            handle.db().unwrap().jade_first_block_number().unwrap(),
            Some(5)
        );
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(9));

        // Pure revert back to block 4 (timestamp 204 < 205: PreJade).
        chain.truncate(5);

        // Deferred turn must not mutate the index DB.
        runtime.synchronize_once(true).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Deferred);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), Some(9));

        // Normal reconciliation rolls back 5..=9 and clears both metadata keys.
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::PreJade);
        let db = handle.db().unwrap();
        assert_eq!(db.indexed_to().unwrap(), None);
        assert_eq!(db.jade_first_block_number().unwrap(), None);
        for n in 5..=9 {
            assert_eq!(db.indexed_block_hash(n).unwrap(), None);
        }

        // Chain later advances past Jade again at block 6 (with pre-Jade block 5).
        chain.push_block(204);
        chain.push_block(205);
        chain.push_block(206);
        runtime.synchronize_once(false).unwrap();
        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(db.jade_first_block_number().unwrap(), Some(6));
        assert_eq!(db.indexed_to().unwrap(), Some(7));
        assert_eq!(db.indexed_block_hash(5).unwrap(), None);
        assert_eq!(
            db.indexed_block_hash(6).unwrap(),
            chain.canonical_hash(6).unwrap()
        );
        assert_eq!(
            db.indexed_block_hash(7).unwrap(),
            chain.canonical_hash(7).unwrap()
        );
    }

    #[tokio::test]
    async fn watch_progress_notifies_on_commit_phase_transition_unavailable_and_drop() {
        let dir = tempfile::tempdir().unwrap();
        let chain = TestChain::linear(10, 200);
        let config = ReferenceIndexConfig::new(dir.path(), 2818, B256::ZERO, 200);
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain.clone());
        let mut rx = handle.subscribe_progress();

        assert_eq!(
            *rx.borrow(),
            ReferenceIndexProgress {
                phase: ReferenceIndexPhase::Opening,
                indexed_to: None,
                unavailable: false,
            }
        );

        // Initial backfill + live commit notifies watchers with cursor height.
        runtime.synchronize_once(false).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            ReferenceIndexProgress {
                phase: ReferenceIndexPhase::Live,
                indexed_to: Some(9),
                unavailable: false,
            }
        );

        // Transition to Deferred notifies watchers while preserving cursor height.
        runtime.synchronize_once(true).unwrap();
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            ReferenceIndexProgress {
                phase: ReferenceIndexPhase::Deferred,
                indexed_to: Some(9),
                unavailable: false,
            }
        );

        // Unrecoverable reorg notifies watchers with Unavailable.
        chain.replace_suffix(0);
        let _ = runtime.synchronize_once(false).unwrap_err();
        rx.changed().await.unwrap();
        assert_eq!(
            *rx.borrow(),
            ReferenceIndexProgress {
                phase: ReferenceIndexPhase::Unavailable,
                indexed_to: Some(9),
                unavailable: true,
            }
        );

        // Dropping the runtime drops the exclusive watch sender and wakes waiters immediately.
        drop(runtime);
        assert!(rx.changed().await.is_err());
    }
}
