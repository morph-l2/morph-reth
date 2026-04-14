//! Metrics for the Morph payload builder.
//!
//! Tracks per-transaction and per-block timing plus L1 message skip counters,
//! analogous to geth's `miner/skipped_txs/l1/*`, `miner/commit/*`, and
//! `processor/block/transactions`.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics for the Morph payload builder.
///
/// Scope: `morph.payload_builder`
///
/// | Metric | Type | geth equivalent |
/// |--------|------|-----------------|
/// | `l1_tx_gas_limit_exceeded_total` | Counter | `miner/skipped_txs/l1/gas_limit_exceeded` |
/// | `l1_tx_strange_err_total` | Counter | `miner/skipped_txs/l1/strange_err` |
/// | `commit_txs_all_duration_seconds` | Histogram | `miner/commit/txs_all` |
/// | `commit_tx_duration_seconds` | Histogram | `miner/commit/tx_all` |
/// | `commit_tx_apply_duration_seconds` | Histogram | `miner/commit/tx_apply` |
/// | `block_transactions` | Gauge | `processor/block/transactions` |
#[derive(Metrics, Clone)]
#[metrics(scope = "morph.payload_builder")]
pub(crate) struct MorphPayloadBuilderMetrics {
    // -------------------------------------------------------------------------
    // L1 message skip counters
    // -------------------------------------------------------------------------
    /// Number of L1 message transactions skipped because they would exceed the
    /// block gas limit.
    ///
    /// In morph-reth this causes the entire block build to fail rather than
    /// silently dropping the transaction, so this counter is incremented on the
    /// error path before the error is propagated.
    ///
    /// Analogous to geth's `miner/skipped_txs/l1/gas_limit_exceeded`.
    pub(crate) l1_tx_gas_limit_exceeded_total: Counter,

    /// Number of L1 message transactions that failed with an unexpected error
    /// (invalid transaction, EVM error, etc.).
    ///
    /// Analogous to geth's `miner/skipped_txs/l1/strange_err`.
    pub(crate) l1_tx_strange_err_total: Counter,

    // -------------------------------------------------------------------------
    // Block-level timing
    // -------------------------------------------------------------------------
    /// Total time to execute all transactions (L1 messages + pool) for one block,
    /// in seconds.
    ///
    /// Measured from before L1 message execution to after pool transaction execution.
    /// Analogous to geth's `miner/commit/txs_all`.
    pub(crate) commit_txs_all_duration_seconds: Histogram,

    // -------------------------------------------------------------------------
    // Per-transaction timing
    // -------------------------------------------------------------------------
    /// Time for the EVM execution of a single transaction (`execute_transaction`
    /// call only, excluding pre/post checks), in seconds.
    ///
    /// Analogous to geth's `miner/commit/tx_apply`.
    pub(crate) commit_tx_apply_duration_seconds: Histogram,

    // -------------------------------------------------------------------------
    // End-to-end payload build timing
    // -------------------------------------------------------------------------
    /// Total time for the entire payload build pipeline (state setup + tx
    /// execution + finalize/state-root + seal), in seconds.
    ///
    /// Broader than `commit_txs_all_duration_seconds` which only covers the
    /// transaction execution phase.
    /// Inspired by tempo's `payload_build_duration_seconds`.
    pub(crate) payload_build_duration_seconds: Histogram,

    // -------------------------------------------------------------------------
    // Block summary gauges
    // -------------------------------------------------------------------------
    /// Number of transactions included in the most recently built block.
    ///
    /// Set once per successful block build.
    /// Analogous to geth's `processor/block/transactions`.
    pub(crate) block_transactions: Gauge,
}

impl MorphPayloadBuilderMetrics {
    /// Increments the pool transaction skip counter with the given reason label.
    ///
    /// Inspired by tempo's `pool_transactions_skipped_total` with `reason` label.
    #[inline]
    pub(crate) fn inc_pool_tx_skipped(&self, reason: &'static str) {
        metrics::counter!("morph_payload_builder_pool_transactions_skipped_total", "reason" => reason)
            .increment(1);
    }
}
