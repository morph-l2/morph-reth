//! Background canonical-cursor runtime for the Morph transaction reference index.

use crate::{
    CanonicalChain, DEFAULT_BACKFILL_BATCH_BLOCKS, LIVE_LAG_SLO, ReferenceIndexError,
    ReferenceIndexHandle, ReferenceIndexPhase,
    backfill::{commit_canonical_batch, resolve_jade_first_block},
    db::ReferenceIndexDb,
    reconcile::rollback_suffix,
};
use alloy_primitives::B256;
use morph_primitives::MorphPrimitives;
use reth_chain_state::{CanonStateNotification, CanonStateNotifications};
use reth_tasks::TaskExecutor;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::broadcast::{
    Receiver,
    error::{RecvError, TryRecvError},
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

fn observe_lagged_notification(
    skipped: u64,
    detector: &mut RapidSyncDetector,
    handle: &ReferenceIndexHandle,
) {
    handle.metrics().lagged_notifications_total.increment(1);
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
}

impl<C: CanonicalChain> ReferenceIndexRuntime<C> {
    /// Create a single-writer runtime and its cloneable RPC query handle.
    pub fn new(config: ReferenceIndexConfig, chain: C) -> (Self, ReferenceIndexHandle) {
        let handle = ReferenceIndexHandle::new(config.jade_timestamp);
        (
            Self {
                config,
                chain,
                handle: handle.clone(),
            },
            handle,
        )
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
        self.handle.install_db(db.clone());
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

        self.handle.set_phase(ReferenceIndexPhase::Repairing);
        self.handle.metrics().reorgs_total.increment(1);
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
            if number == jade_first_block {
                break;
            }
            number -= 1;
        }

        let ancestor =
            ancestor.ok_or(ReferenceIndexError::ManualRebuildRequired { jade_first_block })?;
        rollback_suffix(db, Some(ancestor), DEFAULT_BACKFILL_BATCH_BLOCKS)
    }

    /// Perform one synchronous reconciliation/backfill turn.
    pub fn synchronize_once(&mut self, defer: bool) -> Result<(), ReferenceIndexError> {
        let started_at = Instant::now();
        let result = self.synchronize_once_inner(defer);
        self.handle
            .metrics()
            .batch_duration_seconds
            .record(started_at.elapsed());
        if result.is_err() {
            self.handle.metrics().failures_total.increment(1);
            self.handle.set_phase(ReferenceIndexPhase::Unavailable);
        }
        result
    }

    fn synchronize_once_inner(&mut self, defer: bool) -> Result<(), ReferenceIndexError> {
        let db = self.open_db()?;
        let head = self.chain.head()?;
        self.handle.metrics().target_block.set(head.number as f64);
        if head.timestamp < self.config.jade_timestamp {
            self.handle.metrics().indexed_block.set(0.0);
            self.handle.metrics().lag_blocks.set(0.0);
            self.handle.set_phase(ReferenceIndexPhase::PreJade);
            return Ok(());
        }
        if defer {
            let indexed_to = db.indexed_to()?.unwrap_or_default();
            self.handle.metrics().indexed_block.set(indexed_to as f64);
            self.handle
                .metrics()
                .lag_blocks
                .set(head.number.saturating_sub(indexed_to) as f64);
            self.handle.set_phase(ReferenceIndexPhase::Deferred);
            return Ok(());
        }

        let jade_first_block = match db.jade_first_block_number()? {
            Some(block) => block,
            None => resolve_jade_first_block(&self.chain, head, self.config.jade_timestamp)?
                .ok_or_else(|| {
                    ReferenceIndexError::Other(eyre::eyre!(
                        "Jade is active at the canonical head but its first block was not found"
                    ))
                })?,
        };
        self.reconcile_cursor(&db, jade_first_block, head)?;
        let start = db
            .indexed_to()?
            .map_or(jade_first_block, |cursor| cursor.saturating_add(1));
        if start > head.number {
            self.handle.metrics().indexed_block.set(head.number as f64);
            self.handle.metrics().lag_blocks.set(0.0);
            self.handle.set_phase(ReferenceIndexPhase::Live);
            return Ok(());
        }

        self.handle.set_phase(ReferenceIndexPhase::Backfill);
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
        for (offset, block) in blocks.iter().enumerate() {
            let expected_number = start + offset as u64;
            let canonical_hash = self.chain.canonical_hash(expected_number)?;
            if block.number != expected_number || canonical_hash != Some(block.hash) {
                return Err(ReferenceIndexError::Other(eyre::eyre!(
                    "canonical block changed while reading backfill batch at {expected_number}"
                )));
            }
        }
        commit_canonical_batch(&db, jade_first_block, start, &blocks)?;
        self.handle
            .metrics()
            .indexed_blocks_total
            .increment(blocks.len() as u64);
        self.handle.metrics().committed_batches_total.increment(1);

        let current_head = self.chain.head()?;
        self.handle
            .metrics()
            .target_block
            .set(current_head.number as f64);
        self.handle.metrics().indexed_block.set(end as f64);
        let lag = current_head.number.saturating_sub(end);
        self.handle.metrics().lag_blocks.set(lag as f64);
        if lag > LIVE_LAG_SLO {
            debug!(target: TARGET, lag, slo = LIVE_LAG_SLO, "reference index remains behind its live-lag SLO");
        }
        if end == current_head.number
            && blocks
                .last()
                .is_some_and(|block| block.hash == current_head.hash)
        {
            self.handle.set_phase(ReferenceIndexPhase::Live);
        }
        Ok(())
    }

    /// Run the non-critical background reconciler until node shutdown.
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
                            observe_lagged_notification(skipped, &mut detector, &handle);
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
                            observe_lagged_notification(skipped, &mut detector, &handle);
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
    }

    impl TestChain {
        fn linear(count: u64, first_timestamp: u64) -> Self {
            let blocks = (0..count)
                .map(|number| CanonicalBlock {
                    number,
                    hash: B256::with_last_byte(number as u8),
                    timestamp: first_timestamp + number,
                    transactions: Vec::new(),
                })
                .collect();
            Self {
                blocks: Arc::new(RwLock::new(blocks)),
            }
        }

        fn replace_suffix(&self, from: u64) {
            for block in self.blocks.write().iter_mut().skip(from as usize) {
                block.hash = B256::repeat_byte(0x80 | block.number as u8);
            }
        }

        fn truncate(&self, len: u64) {
            self.blocks.write().truncate(len as usize);
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
        let query = ReferenceQuery::new(B256::with_last_byte(0xaa), None, None).unwrap();
        assert!(
            handle
                .query_at(query, chain.head().unwrap())
                .unwrap()
                .is_empty()
        );
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
        let (mut runtime, handle) = ReferenceIndexRuntime::new(config, chain);
        runtime.synchronize_once(false).unwrap();
        let generation = handle.generation();

        runtime.synchronize_once(false).unwrap();

        assert_eq!(handle.phase(), ReferenceIndexPhase::Live);
        assert_eq!(handle.generation(), generation);
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
        assert_eq!(handle.phase(), ReferenceIndexPhase::Unavailable);
        assert_eq!(handle.db().unwrap().indexed_to().unwrap(), None);
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
}
