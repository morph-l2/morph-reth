//! Observability for the Onyx sweep engine.
//!
//! Metrics are recorded only from committed transactions, so speculative or
//! discarded candidates (e.g. a payload-builder attempt that is rolled back)
//! never move these counters. The reference index intentionally does not
//! consume these events.

use morph_revm::{SweepFailureReason, SweepOutcome};
use reth_metrics::{
    Metrics,
    metrics::{Counter, Gauge, counter},
};

/// Name of the labeled failure-reason series. Must stay under the same scope
/// as the `#[metrics(scope = ...)]` struct below so both share one namespace.
const FAILURES_BY_REASON: &str = "morph.sweep.failures_by_reason";

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.sweep")]
pub(crate) struct SweepMetrics {
    /// Candidates that reached per-candidate execution, settled or not.
    ///
    /// Counted even when a transaction-level over-limit rolled the sweeps back,
    /// because the work was still performed and every client reproduces it.
    candidates_total: Counter,
    /// Successful sweeps after committed transactions.
    sweeps_total: Counter,
    /// Checked candidates that did not settle, across every classified reason.
    failures_total: Counter,
    /// Sources skipped because they have ordinary code (not a plain EOA).
    ///
    /// Broken out separately from [`Self::failures_total`] because a rising
    /// value is an operational signal (misconfigured source or code deployment),
    /// not just a benign no-op.
    code_skipped_total: Counter,
    /// Actual `transfer` gas consumed by committed sweeps. Its rate against the
    /// per-block budget shows how close blocks run to the sweep cap.
    tx_transfer_gas_total: Counter,
    /// Transactions that failed with `SweepOutOfGas` (cumulative `transfer` gas
    /// hit `TX_SWEEP_GAS_LIMIT`).
    tx_out_of_gas_total: Counter,
    /// Cumulative `transfer` gas of the block being executed, against the 20M cap.
    ///
    /// A gauge rather than a counter: it is the running block sum the builder
    /// defers on and import rejects on, so its useful reading is the latest value.
    block_transfer_gas_used: Gauge,
    /// `resolveSweep` static calls attempted. Their gas is outside every meter, so
    /// this is the only visibility into the query workload a block imposes.
    resolver_calls_total: Counter,
    /// `resolveSweep` calls that exhausted their fixed 50k limit.
    resolver_oog_total: Counter,
    /// Pre-transfer `balanceOf` static calls attempted.
    balance_calls_total: Counter,
    /// `balanceOf` calls that exhausted their fixed 50k limit.
    balance_oog_total: Counter,
}

impl SweepMetrics {
    /// Records one committed transaction's sweep outcome.
    ///
    /// `block_transfer_gas_used` is the block's running total after this
    /// transaction, which the caller owns because it lives in the block session.
    pub(crate) fn record(&self, outcome: &SweepOutcome, block_transfer_gas_used: u64) {
        let SweepOutcome {
            candidates_checked,
            query_stats,
            successes,
            failures,
            block_effect,
            tx_out_of_gas,
            logs: _,
        } = outcome;

        self.candidates_total.increment(*candidates_checked);
        self.sweeps_total.increment(successes.len() as u64);
        self.failures_total.increment(failures.len() as u64);
        self.tx_transfer_gas_total
            .increment(block_effect.transfer_gas_used());
        self.tx_out_of_gas_total
            .increment(u64::from(*tx_out_of_gas));
        self.block_transfer_gas_used
            .set(block_transfer_gas_used as f64);

        self.resolver_calls_total
            .increment(query_stats.resolver_calls);
        self.resolver_oog_total.increment(query_stats.resolver_oog);
        self.balance_calls_total
            .increment(query_stats.balance_calls);
        self.balance_oog_total.increment(query_stats.balance_oog);

        for failure in failures {
            if matches!(failure.reason, SweepFailureReason::SourceHasCode) {
                self.code_skipped_total.increment(1);
            }
            // A labeled counter gives the full per-reason breakdown; the derive
            // macro cannot express dynamic labels, so emit it directly.
            counter!(FAILURES_BY_REASON, "reason" => failure.reason.as_label()).increment(1);
        }
    }
}
