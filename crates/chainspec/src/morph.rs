//! Morph Mainnet chain specification.

use crate::{
    MORPH_MAINNET_GENESIS_HASH, MORPH_MAINNET_GENESIS_STATE_ROOT, MorphChainSpec,
    genesis::MorphGenesisInfo,
    spec::{build_morph_hardforks_from_genesis, make_genesis_header},
};
use alloy_genesis::Genesis;
use reth_chainspec::ChainSpec;
use reth_primitives_traits::SealedHeader;
use std::sync::{Arc, LazyLock};

/// Morph Mainnet chain specification.
pub static MORPH_MAINNET: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
        .expect("Failed to parse Morph Mainnet genesis");

    let chain_info = MorphGenesisInfo::extract_from(&genesis.config.extra_fields)
        .expect("failed to extract morph genesis info");

    // Build hardforks from genesis
    let hardforks = build_morph_hardforks_from_genesis(&genesis);

    // Build genesis header with ZK-trie state root (from go-ethereum)
    let header = make_genesis_header(&genesis, MORPH_MAINNET_GENESIS_STATE_ROOT);

    MorphChainSpec {
        inner: ChainSpec {
            chain: genesis.config.chain_id.into(),
            genesis_header: SealedHeader::new(header, MORPH_MAINNET_GENESIS_HASH),
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
    use crate::{MORPH_MAINNET_CHAIN_ID, hardfork::MorphHardforks};
    use alloy_primitives::{address, b256};
    use reth_chainspec::EthChainSpec;

    #[test]
    fn test_morph_mainnet_chain_id() {
        assert_eq!(MORPH_MAINNET.inner.chain.id(), MORPH_MAINNET_CHAIN_ID);
    }

    #[test]
    fn test_morph_mainnet_genesis_hash() {
        let expected = b256!("649c9b1f9f831771529dbf286a63dd071530d73c8fa410997eebaf449acfa7a9");
        assert_eq!(MORPH_MAINNET.genesis_hash(), expected);
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
        // Block-based hardforks should be active at block 0
        assert!(MORPH_MAINNET.is_bernoulli_active_at_block(0));
        // Curie is activated at a later block on mainnet
        // Timestamp-based hardforks may not be active at timestamp 0 on mainnet
        // depending on the genesis configuration
    }
}
