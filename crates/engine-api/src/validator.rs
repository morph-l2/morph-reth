//! Morph Engine Validator utilities.
//!
//! This module provides utilities for state root validation according to
//! the MPTFork hardfork rules.
//!
//! **Important**: Morph skips state root validation before the MPTFork hardfork,
//! before MPTFork, Morph uses ZK-trie, and state root verification happens in the
//! ZK proof instead.

use morph_chainspec::{MorphChainSpec, MorphHardforks};
use std::sync::Arc;

/// Determines if state root validation should be performed for a given timestamp.
///
/// Before the MPTFork hardfork, state root validation is skipped because Morph
/// uses ZK-trie before MPTFork, and the state root verification happens in the
/// ZK proof instead.
///
/// # Arguments
///
/// * `chain_spec` - The chain specification
/// * `timestamp` - The block timestamp to check
///
/// # Returns
///
/// Returns `true` if state root validation should be performed (MPT fork is active),
/// `false` if MPT fork is not active (validation skipped, using ZK-trie).
pub fn should_validate_state_root(chain_spec: &MorphChainSpec, timestamp: u64) -> bool {
    chain_spec.is_mpt_fork_active_at_timestamp(timestamp)
}

/// Helper struct to hold chain spec for validation decisions.
#[derive(Debug, Clone)]
pub struct MorphValidationContext {
    chain_spec: Arc<MorphChainSpec>,
}

impl MorphValidationContext {
    /// Creates a new validation context.
    pub const fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self { chain_spec }
    }

    /// Returns whether state root validation should be performed at the given timestamp.
    pub fn should_validate_state_root(&self, timestamp: u64) -> bool {
        should_validate_state_root(&self.chain_spec, timestamp)
    }

    /// Returns the chain spec.
    pub fn chain_spec(&self) -> &MorphChainSpec {
        &self.chain_spec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::Genesis;
    use serde_json::json;

    fn create_test_chainspec(mpt_fork_time: Option<u64>) -> Arc<MorphChainSpec> {
        let mut genesis_json = json!({
            "config": {
                "chainId": 1337,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph": {}
            },
            "alloc": {}
        });

        if let Some(time) = mpt_fork_time {
            genesis_json["config"]["mptForkTime"] = json!(time);
        }

        let genesis: Genesis = serde_json::from_value(genesis_json).unwrap();
        Arc::new(MorphChainSpec::from(genesis))
    }

    #[test]
    fn test_should_validate_state_root_before_mpt_fork() {
        let chain_spec = create_test_chainspec(Some(1000));

        // Before MPT fork: should skip validation (return false, using ZK-trie)
        assert!(!should_validate_state_root(&chain_spec, 0));
        assert!(!should_validate_state_root(&chain_spec, 500));
        assert!(!should_validate_state_root(&chain_spec, 999));
    }

    #[test]
    fn test_should_validate_state_root_after_mpt_fork() {
        let chain_spec = create_test_chainspec(Some(1000));

        // After MPT fork: should validate (return true, using MPT)
        assert!(should_validate_state_root(&chain_spec, 1000));
        assert!(should_validate_state_root(&chain_spec, 2000));
    }

    #[test]
    fn test_should_validate_state_root_no_mpt_fork() {
        // If MPTFork is not set, should always return false
        let chain_spec = create_test_chainspec(None);

        assert!(!should_validate_state_root(&chain_spec, 0));
        assert!(!should_validate_state_root(&chain_spec, 1000));
    }

    #[test]
    fn test_validation_context() {
        let chain_spec = create_test_chainspec(Some(1000));
        let ctx = MorphValidationContext::new(chain_spec);

        // Before MPT fork: should skip validation (using ZK-trie)
        assert!(!ctx.should_validate_state_root(500));
        // After MPT fork: should validate (using MPT)
        assert!(ctx.should_validate_state_root(1000));
    }
}
