//! Morph Hoodi (testnet) chain specification.

use crate::{
    MORPH_HOODI_GENESIS_HASH, MORPH_HOODI_GENESIS_STATE_ROOT, MorphChainSpec,
    genesis::MorphGenesisInfo,
    spec::{build_morph_hardforks_from_genesis, make_genesis_header},
};
use alloy_genesis::Genesis;
use reth_chainspec::ChainSpec;
use reth_primitives_traits::SealedHeader;
use std::sync::{Arc, LazyLock};

/// Morph Hoodi (testnet) chain specification.
pub static MORPH_HOODI: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/hoodi.json"))
        .expect("Failed to parse Morph Hoodi genesis");

    let chain_info = MorphGenesisInfo::extract_from(&genesis.config.extra_fields)
        .expect("failed to extract morph genesis info");

    // Build hardforks from genesis
    let hardforks = build_morph_hardforks_from_genesis(&genesis);

    // Build genesis header with ZK-trie state root (from go-ethereum)
    let header = make_genesis_header(&genesis, MORPH_HOODI_GENESIS_STATE_ROOT);

    MorphChainSpec {
        inner: ChainSpec {
            chain: genesis.config.chain_id.into(),
            genesis_header: SealedHeader::new(header, MORPH_HOODI_GENESIS_HASH),
            genesis,
            hardforks,
            ..Default::default()
        },
        info: chain_info,
    }
    .into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MORPH_HOODI_CHAIN_ID, hardfork::MorphHardforks};
    use alloy_primitives::{address, b256};
    use reth_chainspec::EthChainSpec;

    #[test]
    fn test_morph_hoodi_chain_id() {
        assert_eq!(MORPH_HOODI.inner.chain.id(), MORPH_HOODI_CHAIN_ID);
    }

    #[test]
    fn test_morph_hoodi_genesis_hash() {
        let expected = b256!("2cbcff7ec8d68255cb130d5274217cded0c83c417b9ed5e045e1ffcc3ebfc35c");
        assert_eq!(MORPH_HOODI.genesis_hash(), expected);
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
    fn test_morph_hoodi_hardforks() {
        // Block-based hardforks should be active at block 0
        assert!(MORPH_HOODI.is_bernoulli_active_at_block(0));
        assert!(MORPH_HOODI.is_curie_active_at_block(0));
        // Timestamp-based hardforks should be active at timestamp 0
        assert!(MORPH_HOODI.is_morph203_active_at_timestamp(0));
        // Note: Viridian and Emerald may not be active at timestamp 0 on Hoodi
        // depending on the genesis configuration
    }
}
