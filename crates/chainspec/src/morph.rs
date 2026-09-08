//! Morph Mainnet chain specification.

use crate::{
    MORPH_MAINNET_GENESIS_HASH, MORPH_MAINNET_GENESIS_STATE_ROOT,
    MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK, MorphChainSpec, spec::GenesisConfig,
};
use alloy_genesis::Genesis;
use std::sync::{Arc, LazyLock};

/// Morph Mainnet chain specification.
pub static MORPH_MAINNET: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
        .expect("Failed to parse Morph Mainnet genesis");

    // Preserve the historical genesis JSON while matching morph-geth's built-in
    // mainnet config, which applies the current 720 KiB runtime limit.
    let config = GenesisConfig::default()
        .with_state_root(MORPH_MAINNET_GENESIS_STATE_ROOT, MORPH_MAINNET_GENESIS_HASH)
        .with_max_tx_payload_bytes_per_block(MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK);

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
    fn test_morph_mainnet_payload_limit_preserves_genesis() {
        let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
            .expect("mainnet genesis should parse");
        let genesis_limit = crate::MorphGenesisInfo::extract_from(&genesis.config.extra_fields)
            .expect("mainnet morph config should parse")
            .morph_chain_info
            .max_tx_payload_bytes_per_block;
        assert_eq!(genesis_limit, 122_880);
        assert_eq!(
            MORPH_MAINNET.max_tx_payload_bytes_per_block(),
            MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK
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
