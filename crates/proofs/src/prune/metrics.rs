//! Pruner metrics.

use reth_metrics::{
    Metrics,
    metrics::{Gauge, Histogram},
};

use crate::PrunerOutput;

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.proofs.pruner")]
pub(super) struct PrunerMetrics {
    /// Pruning duration.
    total_duration_seconds: Histogram,
    /// Number of pruned blocks.
    pruned_blocks: Gauge,
    /// Number of account trie updates written.
    account_trie_updates_written: Gauge,
    /// Number of storage trie updates written.
    storage_trie_updates_written: Gauge,
    /// Number of hashed accounts written.
    hashed_accounts_written: Gauge,
    /// Number of hashed storages written.
    hashed_storages_written: Gauge,
}

impl PrunerMetrics {
    pub(super) fn record_prune_result(result: PrunerOutput) {
        let blocks_pruned = result.end_block.saturating_sub(result.start_block);
        if blocks_pruned == 0 {
            return;
        }

        let metrics = Self::default();
        metrics
            .total_duration_seconds
            .record(result.duration.as_secs_f64());
        metrics.pruned_blocks.set(blocks_pruned as f64);

        let counts = &result.write_counts;
        metrics
            .account_trie_updates_written
            .set(counts.account_trie_updates_written_total as f64);
        metrics
            .storage_trie_updates_written
            .set(counts.storage_trie_updates_written_total as f64);
        metrics
            .hashed_accounts_written
            .set(counts.hashed_accounts_written_total as f64);
        metrics
            .hashed_storages_written
            .set(counts.hashed_storages_written_total as f64);
    }
}
