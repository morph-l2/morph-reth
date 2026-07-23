//! Runtime observability for the derived reference index.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge},
};

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.reference_index")]
pub(crate) struct ReferenceIndexMetrics {
    /// Current runtime phase encoded by `ReferenceIndexPhase` (readiness lives here:
    /// `Live` == serving).
    pub(crate) phase: Gauge,
    /// Canonical block lag (`head - indexed`) after the latest reconciliation turn.
    pub(crate) lag_blocks: Gauge,
    /// Canonical suffix repairs performed.
    pub(crate) reorgs_total: Counter,
    /// Failed reconciliation turns.
    pub(crate) failures_total: Counter,
    /// Bounded RPC catch-up waits that timed out and still returned `IndexBehind`.
    pub(crate) rpc_wait_timeouts_total: Counter,
}
