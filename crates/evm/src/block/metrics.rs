//! Observability for the Onyx sweep engine.
//!
//! Metrics are recorded only from committed transactions, so speculative or
//! discarded candidates (e.g. a payload-builder attempt that is rolled back)
//! never move these counters. The reference index intentionally does not
//! consume these events.

use morph_revm::{SweepFailure, SweepFailureReason};
use reth_metrics::{
    Metrics,
    metrics::{Counter, counter},
};

/// Name of the labeled failure-reason series. Must stay under the same scope
/// as the `#[metrics(scope = ...)]` struct below so both share one namespace.
const FAILURES_BY_REASON: &str = "morph.sweep.failures_by_reason";

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.sweep")]
pub(crate) struct SweepMetrics {
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
}

impl SweepMetrics {
    /// Records one committed transaction's sweep outcome.
    pub(crate) fn record(
        &self,
        successes: usize,
        failures: &[SweepFailure],
        transfer_gas_used: u64,
        tx_out_of_gas: bool,
    ) {
        self.sweeps_total.increment(successes as u64);
        self.failures_total.increment(failures.len() as u64);
        self.tx_transfer_gas_total.increment(transfer_gas_used);
        self.tx_out_of_gas_total
            .increment(u64::from(tx_out_of_gas));

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
