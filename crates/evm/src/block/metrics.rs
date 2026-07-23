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
    /// Candidates checked after committed transactions; each is charged a fixed
    /// system-gas debit regardless of outcome.
    candidates_checked_total: Counter,
    /// Candidates that swept a deposit balance to its registered master.
    sweeps_total: Counter,
    /// Checked candidates that did not sweep, across every classified reason.
    failures_total: Counter,
    /// Deposits skipped because they carried code or an EIP-7702 delegation.
    ///
    /// Broken out separately from [`Self::failures_total`] because a rising
    /// value is an operational signal (misconfigured deposit or 7702 abuse),
    /// not just a benign no-op.
    code_skipped_total: Counter,
    /// Fixed sweep system gas committed to blocks. Its rate against
    /// the per-block budget shows how close blocks run to the sweep cap.
    system_gas_total: Counter,
}

impl SweepMetrics {
    /// Records one committed transaction's sweep outcome.
    pub(crate) fn record(
        &self,
        checked_candidates: usize,
        successes: usize,
        failures: &[SweepFailure],
        system_gas_used: u64,
    ) {
        self.candidates_checked_total
            .increment(checked_candidates as u64);
        self.sweeps_total.increment(successes as u64);
        self.failures_total.increment(failures.len() as u64);
        self.system_gas_total.increment(system_gas_used);

        for failure in failures {
            if matches!(failure.reason, SweepFailureReason::DepositHasCode) {
                self.code_skipped_total.increment(1);
            }
            // A labeled counter gives the full per-reason breakdown; the derive
            // macro cannot express dynamic labels, so emit it directly.
            counter!(FAILURES_BY_REASON, "reason" => failure.reason.as_label()).increment(1);
        }
    }
}
