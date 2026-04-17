//! Decides whether strict state-root equality must be enforced for a block.
//!
//! Pre-Jade Morph blocks store ZK-trie roots in `header.state_root`, while reth
//! computes MPT roots. Skipping the strict check pre-Jade is safe because the
//! first post-Jade block's successful strict check retroactively anchors the
//! MPT state accumulated across the pre-Jade range (transitivity of EVM
//! execution; see the design spec).

use morph_chainspec::{MorphChainSpec, MorphHardforks};

/// Returns `true` iff reth must enforce the strict
/// `computed_state_root == header.state_root()` check at the given block
/// timestamp. Post-Jade: true. Pre-Jade: false.
pub fn state_root_enforced_at(chain_spec: &MorphChainSpec, timestamp: u64) -> bool {
    chain_spec.is_jade_active_at_timestamp(timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_genesis::Genesis;
    use serde_json::json;
    use std::sync::Arc;

    fn chain_spec_with_jade_at(jade_time: u64) -> Arc<MorphChainSpec> {
        let genesis: Genesis = serde_json::from_value(json!({
            "config": {
                "chainId": 1337,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph": {},
                "jadeForkTime": jade_time
            },
            "alloc": {}
        }))
        .expect("valid test genesis");
        Arc::new(MorphChainSpec::from(genesis))
    }

    #[test]
    fn skipped_before_jade() {
        let cs = chain_spec_with_jade_at(1_000);
        assert!(!state_root_enforced_at(&cs, 0));
        assert!(!state_root_enforced_at(&cs, 500));
        assert!(!state_root_enforced_at(&cs, 999));
    }

    #[test]
    fn strict_at_and_after_jade() {
        let cs = chain_spec_with_jade_at(1_000);
        assert!(state_root_enforced_at(&cs, 1_000));
        assert!(state_root_enforced_at(&cs, 1_001));
        assert!(state_root_enforced_at(&cs, u64::MAX));
    }

    #[test]
    fn skipped_when_jade_unset() {
        let genesis: Genesis = serde_json::from_value(json!({
            "config": { "chainId": 1337, "bernoulliBlock": 0, "curieBlock": 0, "morph": {} },
            "alloc": {}
        }))
        .unwrap();
        let cs = Arc::new(MorphChainSpec::from(genesis));
        assert!(!state_root_enforced_at(&cs, 0));
        assert!(!state_root_enforced_at(&cs, 1_000_000));
    }
}
