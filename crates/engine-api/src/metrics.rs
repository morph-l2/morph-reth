//! Metrics for the Morph L2 Engine API.
//!
//! Tracks per-method latency and failure counts for custom Morph Engine API
//! endpoints, plus chain head health gauges analogous to geth's
//! `chain/head/block` and `chain/head/timegap`.

use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, Histogram},
};

/// Metrics for the custom Morph L2 Engine API endpoints.
///
/// Each method tracks:
/// - A latency histogram (`*_duration_seconds`)
/// - A failure counter (`*_failures_total`)
///
/// Additionally, two chain-head gauges are updated after each successful block
/// import (equivalent to geth's `chain/head/block` and `chain/head/timegap`):
/// - `head_block_number`
/// - `head_block_timegap_seconds`
#[derive(Metrics, Clone)]
#[metrics(scope = "morph.engine")]
pub(crate) struct MorphEngineApiMetrics {
    // -------------------------------------------------------------------------
    // assembleL2Block
    // -------------------------------------------------------------------------
    /// Latency for `engine_assembleL2Block` calls.
    pub(crate) assemble_l2_block_duration_seconds: Histogram,
    /// Number of `engine_assembleL2Block` calls that returned an error.
    pub(crate) assemble_l2_block_failures_total: Counter,

    // -------------------------------------------------------------------------
    // newL2Block
    // -------------------------------------------------------------------------
    /// Latency for `engine_newL2Block` calls.
    pub(crate) new_l2_block_duration_seconds: Histogram,
    /// Number of `engine_newL2Block` calls that returned an error.
    pub(crate) new_l2_block_failures_total: Counter,

    // -------------------------------------------------------------------------
    // validateL2Block
    // -------------------------------------------------------------------------
    /// Latency for `engine_validateL2Block` calls.
    pub(crate) validate_l2_block_duration_seconds: Histogram,
    /// Number of `engine_validateL2Block` calls that returned `success: false`.
    pub(crate) validate_l2_block_failures_total: Counter,

    // -------------------------------------------------------------------------
    // newSafeL2Block
    // -------------------------------------------------------------------------
    /// Latency for `engine_newSafeL2Block` calls.
    pub(crate) new_safe_l2_block_duration_seconds: Histogram,
    /// Number of `engine_newSafeL2Block` calls that returned an error.
    pub(crate) new_safe_l2_block_failures_total: Counter,

    // -------------------------------------------------------------------------
    // Chain head health
    // -------------------------------------------------------------------------
    /// Seconds elapsed since the latest imported block's timestamp.
    ///
    /// A large value indicates the node is behind the chain tip.
    /// Updated on every successful block import.
    /// Analogous to geth's `chain/head/timegap` gauge.
    pub(crate) head_block_timegap_seconds: Gauge,
}
