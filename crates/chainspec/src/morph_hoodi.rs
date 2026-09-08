//! Morph Hoodi (testnet) chain specification.

use crate::{
    MORPH_HOODI_GENESIS_HASH, MORPH_HOODI_GENESIS_STATE_ROOT, MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK,
    MorphChainSpec, spec::GenesisConfig,
};
use alloy_genesis::Genesis;
use std::sync::{Arc, LazyLock};

/// Morph Hoodi (testnet) chain specification.
pub static MORPH_HOODI: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/hoodi.json"))
        .expect("Failed to parse Morph Hoodi genesis");

    // Preserve the historical genesis JSON while matching morph-geth's built-in
    // Hoodi config, which applies the current 720 KiB runtime limit.
    let config = GenesisConfig::default()
        .with_state_root(MORPH_HOODI_GENESIS_STATE_ROOT, MORPH_HOODI_GENESIS_HASH)
        .with_max_tx_payload_bytes_per_block(MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK);

    MorphChainSpec::from_genesis_with_config(genesis, config).into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MORPH_HOODI_CHAIN_ID, hardfork::MorphHardforks};
    use alloy_primitives::address;
    use reth_chainspec::EthChainSpec;

    #[test]
    fn test_morph_hoodi_chain_id() {
        assert_eq!(MORPH_HOODI.inner.chain.id(), MORPH_HOODI_CHAIN_ID);
    }

    #[test]
    fn test_morph_hoodi_genesis_hash() {
        assert_eq!(MORPH_HOODI.genesis_hash(), MORPH_HOODI_GENESIS_HASH);
    }

    #[test]
    fn test_morph_hoodi_fee_vault() {
        assert!(MORPH_HOODI.is_fee_vault_enabled());
        // Fee vault address is parsed from genesis JSON
        assert_eq!(
            MORPH_HOODI.fee_vault_address(),
            Some(address!("29107CB79Ef8f69fE1587F77e283d47E84c5202f"))
        );
    }

    #[test]
    fn test_morph_hoodi_payload_limit_preserves_genesis() {
        let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/hoodi.json"))
            .expect("Hoodi genesis should parse");
        let genesis_limit = crate::MorphGenesisInfo::extract_from(&genesis.config.extra_fields)
            .expect("Hoodi morph config should parse")
            .morph_chain_info
            .max_tx_payload_bytes_per_block;
        assert_eq!(genesis_limit, 122_880);
        assert_eq!(
            MORPH_HOODI.max_tx_payload_bytes_per_block(),
            MORPH_MAX_TX_PAYLOAD_BYTES_PER_BLOCK
        );
    }

    #[test]
    fn test_morph_hoodi_hardforks() {
        // Block-based hardforks should be active at block 0
        assert!(MORPH_HOODI.is_bernoulli_active_at_block(0));
        assert!(MORPH_HOODI.is_curie_active_at_block(0));

        // Timestamp-based hardforks from go-ethereum MorphHoodiChainConfig
        // Morph203 is active from genesis (timestamp 0)
        assert!(MORPH_HOODI.is_morph203_active_at_timestamp(0));

        assert!(!MORPH_HOODI.is_viridian_active_at_timestamp(1761544799));
        assert!(MORPH_HOODI.is_viridian_active_at_timestamp(1761544800));

        assert!(!MORPH_HOODI.is_emerald_active_at_timestamp(1766987999));
        assert!(MORPH_HOODI.is_emerald_active_at_timestamp(1766988000));

        assert!(!MORPH_HOODI.is_jade_active_at_timestamp(1774418399));
        assert!(MORPH_HOODI.is_jade_active_at_timestamp(1774418400));
    }
}
