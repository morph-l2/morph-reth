//! Morph Mainnet chain specification.

use crate::{
    MORPH_MAINNET_GENESIS_HASH, MORPH_MAINNET_GENESIS_STATE_ROOT, MorphChainSpec,
    spec::GenesisConfig,
};
use alloy_genesis::Genesis;
use std::sync::{Arc, LazyLock};

/// Morph Mainnet chain specification.
pub static MORPH_MAINNET: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
        .expect("Failed to parse Morph Mainnet genesis");

    // Use ZK-trie state root (hardcoded constant from go-ethereum)
    let config = GenesisConfig::default()
        .with_state_root(MORPH_MAINNET_GENESIS_STATE_ROOT, MORPH_MAINNET_GENESIS_HASH);

    MorphChainSpec::from_genesis_with_config(genesis, config).into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MORPH_MAINNET_CHAIN_ID, hardfork::MorphHardforks};
    use alloy_primitives::address;
    use reth_chainspec::EthChainSpec;

    #[test]
    fn test_morph_mainnet_chain_id() {
        assert_eq!(MORPH_MAINNET.inner.chain.id(), MORPH_MAINNET_CHAIN_ID);
    }

    #[test]
    fn test_morph_mainnet_genesis_hash() {
        assert_eq!(MORPH_MAINNET.genesis_hash(), MORPH_MAINNET_GENESIS_HASH);
    }

    #[test]
    fn test_morph_mainnet_fee_vault() {
        assert!(MORPH_MAINNET.is_fee_vault_enabled());
        // Fee vault address is parsed from genesis JSON
        assert_eq!(
            MORPH_MAINNET.fee_vault_address(),
            Some(address!("530000000000000000000000000000000000000a"))
        );
    }

    #[test]
    fn test_morph_mainnet_hardforks() {
        // Block-based hardforks: both Bernoulli and Curie active from block 0
        assert!(MORPH_MAINNET.is_bernoulli_active_at_block(0));
        assert!(MORPH_MAINNET.is_curie_active_at_block(0));

        // Timestamp-based hardforks from go-ethereum MorphMainnetChainConfig
        assert!(!MORPH_MAINNET.is_morph203_active_at_timestamp(1747029599));
        assert!(MORPH_MAINNET.is_morph203_active_at_timestamp(1747029600));

        assert!(!MORPH_MAINNET.is_viridian_active_at_timestamp(1762149599));
        assert!(MORPH_MAINNET.is_viridian_active_at_timestamp(1762149600));

        assert!(!MORPH_MAINNET.is_emerald_active_at_timestamp(1767765599));
        assert!(MORPH_MAINNET.is_emerald_active_at_timestamp(1767765600));

        assert!(!MORPH_MAINNET.is_jade_active_at_timestamp(1775627999));
        assert!(MORPH_MAINNET.is_jade_active_at_timestamp(1775628000));
    }
}
