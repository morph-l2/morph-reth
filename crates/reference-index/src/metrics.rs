//! Runtime observability for the derived reference index.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.reference_index")]
pub(crate) struct ReferenceIndexMetrics {
    /// Current runtime phase encoded by `ReferenceIndexPhase`.
    pub(crate) phase: Gauge,
    /// Current canonical target block.
    pub(crate) target_block: Gauge,
    /// Last durably indexed canonical block, or zero before activation.
    pub(crate) indexed_block: Gauge,
    /// Canonical block lag observed after the latest reconciliation turn.
    pub(crate) lag_blocks: Gauge,
    /// One only when the runtime phase is live; zero otherwise.
    pub(crate) rpc_ready: Gauge,
    /// Canonical blocks committed to the index.
    pub(crate) indexed_blocks_total: Counter,
    /// Durable index batches committed.
    pub(crate) committed_batches_total: Counter,
    /// Canonical suffix repairs performed.
    pub(crate) reorgs_total: Counter,
    /// Canonical notification receiver lag events.
    pub(crate) lagged_notifications_total: Counter,
    /// Failed reconciliation turns.
    pub(crate) failures_total: Counter,
    /// Wall-clock duration of a reconciliation turn.
    pub(crate) batch_duration_seconds: Histogram,
    /// RPC queries that entered the bounded server-side catch-up wait.
    pub(crate) rpc_waits_total: Counter,
    /// Bounded RPC waits that timed out and still returned `IndexBehind`.
    pub(crate) rpc_wait_timeouts_total: Counter,
    /// Wall-clock duration a query spent in the bounded catch-up wait.
    pub(crate) rpc_wait_duration_seconds: Histogram,
}
