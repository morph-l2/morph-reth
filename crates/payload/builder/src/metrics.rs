//! Metrics for the Morph payload builder.
//!
//! Tracks per-transaction and per-block timing analogous to geth's
//! `miner/commit/*` and `processor/block/transactions`.
//!
//! L1 message / pool transaction skip events are intentionally NOT exposed
//! as metrics. They are rare enough (L1) or high-frequency enough (pool)
//! that logging is a more useful observability tool; see the log sites in
//! `builder.rs`.

use reth_metrics::{
    Metrics,
    metrics::{Gauge, Histogram},
};

/// Metrics for the Morph payload builder.
///
/// Scope: `morph.payload_builder`
///
/// | Metric | Type | geth equivalent |
/// |--------|------|-----------------|
/// | `commit_txs_all_duration_seconds` | Histogram | `miner/commit/txs_all` |
/// | `commit_tx_apply_duration_seconds` | Histogram | `miner/commit/tx_apply` |
/// | `payload_build_duration_seconds` | Histogram | tempo-inspired |
/// | `block_transactions` | Gauge | `processor/block/transactions` |
#[derive(Metrics, Clone)]
#[metrics(scope = "morph.payload_builder")]
pub(crate) struct MorphPayloadBuilderMetrics {
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
    /// Number of transactions included in the most recently successfully built block.
    ///
    /// Only updated when a payload reaches `BuildOutcomeKind::Better`; Aborted and
    /// Cancelled builds do not update this gauge.
    /// Analogous to geth's `processor/block/transactions`.
    pub(crate) block_transactions: Gauge,
}
