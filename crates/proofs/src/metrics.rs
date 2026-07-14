//! Metrics for bounded historical-proof storage.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

use crate::{OperationDurations, WriteCounts, cursor};

/// The production storage type. Kept as an alias so callers remain backend-agnostic
/// without introducing another production backend.
pub type MorphProofsStorage<S> = S;

/// Trie cursor used by proof generation.
pub type MorphProofsTrieCursor<C> = cursor::MorphProofsTrieCursor<C>;
/// Hashed account cursor used by proof generation.
pub type MorphProofsHashedAccountCursor<C> = cursor::MorphProofsHashedAccountCursor<C>;
/// Hashed storage cursor used by proof generation.
pub type MorphProofsHashedStorageCursor<C> = cursor::MorphProofsHashedStorageCursor<C>;

/// Proof-history storage and lifecycle metrics.
#[derive(Metrics, Clone)]
#[metrics(scope = "morph.proofs")]
pub struct ProofHistoryMetrics {
    /// Total time spent processing a block.
    pub block_total_duration_seconds: Histogram,
    /// Time spent executing a block when catch-up re-execution is required.
    pub block_execution_duration_seconds: Histogram,
    /// Time spent calculating a state root.
    pub block_state_root_duration_seconds: Histogram,
    /// Time spent persisting a block.
    pub block_write_duration_seconds: Histogram,
    /// Number of account-trie updates written.
    pub account_trie_updates_written_total: Counter,
    /// Number of storage-trie updates written.
    pub storage_trie_updates_written_total: Counter,
    /// Number of account leaves written.
    pub hashed_accounts_written_total: Counter,
    /// Number of storage leaves written.
    pub hashed_storages_written_total: Counter,
    /// Earliest block currently served by proof history.
    pub earliest_block: Gauge,
    /// Latest block currently served by proof history.
    pub latest_block: Gauge,
    /// Number of prune failures.
    pub prune_errors_total: Counter,
    /// Number of unwind failures.
    pub unwind_errors_total: Counter,
}

impl ProofHistoryMetrics {
    /// Records latency and write-count measurements for one processed block or batch.
    pub fn record_block(durations: &OperationDurations, counts: Option<&WriteCounts>) {
        let metrics = Self::default();
        metrics
            .block_total_duration_seconds
            .record(durations.total_duration_seconds.as_secs_f64());
        metrics
            .block_execution_duration_seconds
            .record(durations.execution_duration_seconds.as_secs_f64());
        metrics
            .block_state_root_duration_seconds
            .record(durations.state_root_duration_seconds.as_secs_f64());
        metrics
            .block_write_duration_seconds
            .record(durations.write_duration_seconds.as_secs_f64());
        if let Some(counts) = counts {
            metrics
                .account_trie_updates_written_total
                .increment(counts.account_trie_updates_written_total);
            metrics
                .storage_trie_updates_written_total
                .increment(counts.storage_trie_updates_written_total);
            metrics
                .hashed_accounts_written_total
                .increment(counts.hashed_accounts_written_total);
            metrics
                .hashed_storages_written_total
                .increment(counts.hashed_storages_written_total);
        }
    }

    /// Updates the gauges for the inclusive durable proof window.
    pub fn set_window(earliest: u64, latest: u64) {
        let metrics = Self::default();
        metrics.earliest_block.set(earliest as f64);
        metrics.latest_block.set(latest as f64);
    }

    pub(crate) fn record_prune_error() {
        Self::default().prune_errors_total.increment(1);
    }

    pub(crate) fn record_unwind_error() {
        Self::default().unwind_errors_total.increment(1);
    }
}
