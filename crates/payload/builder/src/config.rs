//! Configuration for the Morph payload builder.

use core::time::Duration;
use reth_chainspec::MIN_TRANSACTION_GAS;
use std::{fmt::Debug, time::Instant};

/// Minimal data bytes size per transaction.
/// This is a conservative estimate for the minimum encoded transaction size.
pub(crate) const MIN_TRANSACTION_DATA_SIZE: u64 = 115;

/// Settings for the Morph payload builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MorphBuilderConfig {
    /// Optional gas limit for payload building.
    ///
    /// This is a soft limit that can be lower than the block gas limit.
    /// When set, the builder will stop adding transactions once this limit is approached.
    /// This allows operators to:
    /// - Reserve gas for specific transactions
    /// - Reduce payload building time
    /// - Control block utilization
    ///
    /// If `None`, the block gas limit from the EVM environment is used.
    pub gas_limit: Option<u64>,

    /// Time limit for payload building.
    ///
    /// The builder will stop adding pool transactions and return the current payload
    /// once this duration has elapsed since the start of building.
    /// This ensures timely block production even with large mempools.
    pub time_limit: Duration,

    /// Maximum total data availability size for a block.
    ///
    /// L2 transactions need to be published to L1 for data availability.
    /// This limit controls the maximum size of transaction data in a single block.
    /// If `None`, no DA limit is enforced.
    pub max_da_block_size: Option<u64>,
}

impl Default for MorphBuilderConfig {
    fn default() -> Self {
        Self {
            gas_limit: None,
            // Default to 1 second - leaves time for consensus
            time_limit: Duration::from_secs(1),
            // No DA limit by default
            max_da_block_size: None,
        }
    }
}

impl MorphBuilderConfig {
    /// Creates a new [`MorphBuilderConfig`] with the specified parameters.
    pub const fn new(
        gas_limit: Option<u64>,
        time_limit: Duration,
        max_da_block_size: Option<u64>,
    ) -> Self {
        Self { gas_limit, time_limit, max_da_block_size }
    }

    /// Sets the gas limit.
    pub const fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    /// Sets the time limit.
    pub const fn with_time_limit(mut self, time_limit: Duration) -> Self {
        self.time_limit = time_limit;
        self
    }

    /// Sets the maximum DA block size.
    pub const fn with_max_da_block_size(mut self, max_da_block_size: u64) -> Self {
        self.max_da_block_size = Some(max_da_block_size);
        self
    }

    /// Creates a [`PayloadBuildingBreaker`] for this configuration.
    pub(crate) fn breaker(&self, block_gas_limit: u64) -> PayloadBuildingBreaker {
        // Use configured gas limit or fall back to block gas limit
        let effective_gas_limit = self.gas_limit.unwrap_or(block_gas_limit);
        PayloadBuildingBreaker::new(self.time_limit, effective_gas_limit, self.max_da_block_size)
    }
}

/// Used in the payload builder to exit the transactions execution loop early.
///
/// The breaker checks three conditions:
/// 1. Time limit - stop if building takes too long
/// 2. Gas limit - stop if remaining gas is insufficient for any transaction
/// 3. DA limit - stop if data availability size limit is reached
#[derive(Debug, Clone)]
pub struct PayloadBuildingBreaker {
    /// When the payload building started.
    start: Instant,
    /// Maximum time allowed for building.
    time_limit: Duration,
    /// Gas limit for the payload.
    gas_limit: u64,
    /// Maximum DA block size.
    max_da_block_size: Option<u64>,
}

impl PayloadBuildingBreaker {
    /// Creates a new [`PayloadBuildingBreaker`].
    fn new(time_limit: Duration, gas_limit: u64, max_da_block_size: Option<u64>) -> Self {
        Self {
            start: Instant::now(),
            time_limit,
            gas_limit,
            max_da_block_size,
        }
    }

    /// Returns whether the payload building should stop.
    ///
    /// Returns `true` if any of the following conditions are met:
    /// - Time limit has been exceeded
    /// - Gas limit has been reached (leaving room for at least one minimal transaction)
    /// - DA size limit has been reached (leaving room for at least one minimal transaction)
    pub fn should_break(&self, cumulative_gas_used: u64, cumulative_da_size_used: u64) -> bool {
        // Check time limit
        if self.start.elapsed() >= self.time_limit {
            tracing::trace!(
                target: "payload_builder",
                elapsed = ?self.start.elapsed(),
                time_limit = ?self.time_limit,
                "time limit reached"
            );
            return true;
        }

        // Check gas limit - stop if remaining gas can't fit even the smallest transaction
        if cumulative_gas_used > self.gas_limit.saturating_sub(MIN_TRANSACTION_GAS) {
            tracing::trace!(
                target: "payload_builder",
                cumulative_gas_used,
                gas_limit = self.gas_limit,
                "gas limit reached"
            );
            return true;
        }

        // Check DA size limit if configured
        if let Some(max_size) = self.max_da_block_size
            && cumulative_da_size_used > max_size.saturating_sub(MIN_TRANSACTION_DATA_SIZE)
        {
            tracing::trace!(
                target: "payload_builder",
                cumulative_da_size_used,
                max_da_block_size = max_size,
                "DA size limit reached"
            );
            return true;
        }

        false
    }

    /// Returns the elapsed time since building started.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = MorphBuilderConfig::default();
        assert_eq!(config.gas_limit, None);
        assert_eq!(config.time_limit, Duration::from_secs(1));
        assert_eq!(config.max_da_block_size, None);
    }

    #[test]
    fn test_config_builder_pattern() {
        let config = MorphBuilderConfig::default()
            .with_gas_limit(20_000_000)
            .with_time_limit(Duration::from_millis(500))
            .with_max_da_block_size(128 * 1024);

        assert_eq!(config.gas_limit, Some(20_000_000));
        assert_eq!(config.time_limit, Duration::from_millis(500));
        assert_eq!(config.max_da_block_size, Some(128 * 1024));
    }

    #[test]
    fn test_breaker_should_break_on_time_limit() {
        let breaker = PayloadBuildingBreaker::new(
            Duration::from_millis(100),
            30_000_000,
            Some(128 * 1024),
        );

        // Should not break immediately
        assert!(!breaker.should_break(0, 0));

        // Wait for time limit
        std::thread::sleep(Duration::from_millis(150));

        // Should break now
        assert!(breaker.should_break(0, 0));
    }

    #[test]
    fn test_breaker_should_break_on_gas_limit() {
        // Set gas_limit = 2 * MIN_TRANSACTION_GAS = 42000
        // Threshold = 42000 - 21000 = 21000
        // should_break returns true when cumulative_gas_used > threshold
        let gas_limit = 2 * MIN_TRANSACTION_GAS;
        let breaker = PayloadBuildingBreaker::new(Duration::from_secs(10), gas_limit, None);

        // At threshold (21000), should NOT break (21000 > 21000 is false)
        assert!(!breaker.should_break(MIN_TRANSACTION_GAS, 0));

        // Just over threshold, should break (21001 > 21000 is true)
        assert!(breaker.should_break(MIN_TRANSACTION_GAS + 1, 0));
    }

    #[test]
    fn test_breaker_should_break_on_da_limit() {
        // Set max_da = 2 * MIN_TRANSACTION_DATA_SIZE = 230
        // Threshold = 230 - 115 = 115
        let max_da_size = 2 * MIN_TRANSACTION_DATA_SIZE;
        let breaker =
            PayloadBuildingBreaker::new(Duration::from_secs(10), 30_000_000, Some(max_da_size));

        // At threshold (115), should NOT break (115 > 115 is false)
        assert!(!breaker.should_break(0, MIN_TRANSACTION_DATA_SIZE));

        // Just over threshold, should break (116 > 115 is true)
        assert!(breaker.should_break(0, MIN_TRANSACTION_DATA_SIZE + 1));
    }

    #[test]
    fn test_breaker_no_da_limit() {
        let breaker = PayloadBuildingBreaker::new(Duration::from_secs(10), 30_000_000, None);

        // Should not break even with huge DA size when no limit is set
        assert!(!breaker.should_break(0, u64::MAX));
    }

    #[test]
    fn test_config_breaker_uses_block_gas_limit_when_not_configured() {
        let config = MorphBuilderConfig::default();
        // When gas_limit is None, breaker uses block_gas_limit
        let block_gas_limit = 2 * MIN_TRANSACTION_GAS;

        let breaker = config.breaker(block_gas_limit);

        // Threshold = 42000 - 21000 = 21000
        // At threshold, should NOT break
        assert!(!breaker.should_break(MIN_TRANSACTION_GAS, 0));
        // Just over, should break
        assert!(breaker.should_break(MIN_TRANSACTION_GAS + 1, 0));
    }

    #[test]
    fn test_config_breaker_uses_configured_gas_limit() {
        // Configure a gas limit lower than block gas limit
        let configured_limit = 2 * MIN_TRANSACTION_GAS; // 42000
        let config = MorphBuilderConfig::default().with_gas_limit(configured_limit);
        let block_gas_limit = 30_000_000; // Much higher

        let breaker = config.breaker(block_gas_limit);

        // Should use configured_limit, not block_gas_limit
        // Threshold = 42000 - 21000 = 21000
        assert!(!breaker.should_break(MIN_TRANSACTION_GAS, 0));
        assert!(breaker.should_break(MIN_TRANSACTION_GAS + 1, 0));

        // Verify it's not using block_gas_limit
        // If using block_gas_limit, threshold would be ~29,979,000
        // and 21001 would NOT trigger the breaker
        // Since it does trigger, we know it's using configured_limit
    }
}
