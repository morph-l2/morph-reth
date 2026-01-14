//! Morph Mainnet chain specification.

use crate::MorphChainSpec;
use alloy_genesis::Genesis;
use std::sync::{Arc, LazyLock};

/// Morph Mainnet chain specification.
pub static MORPH_MAINNET: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/mainnet.json"))
        .expect("Failed to parse Morph Mainnet genesis");
    MorphChainSpec::from(genesis).into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MORPH_FEE_VAULT_ADDRESS_MAINNET, MORPH_MAINNET_CHAIN_ID, hardfork::MorphHardforks,
    };

    #[test]
    fn test_morph_mainnet_chain_id() {
        assert_eq!(MORPH_MAINNET.inner.chain.id(), MORPH_MAINNET_CHAIN_ID);
    }

    #[test]
    fn test_morph_mainnet_fee_vault() {
        assert!(MORPH_MAINNET.is_fee_vault_enabled());
        assert_eq!(
            MORPH_MAINNET.fee_vault_address(),
            Some(MORPH_FEE_VAULT_ADDRESS_MAINNET)
        );
    }

    #[test]
    fn test_morph_mainnet_hardforks() {
        // All hardforks should be active at genesis
        assert!(MORPH_MAINNET.is_bernoulli_active_at_timestamp(0));
        assert!(MORPH_MAINNET.is_curie_active_at_timestamp(0));
        assert!(MORPH_MAINNET.is_morph203_active_at_timestamp(0));
        assert!(MORPH_MAINNET.is_viridian_active_at_timestamp(0));
        assert!(MORPH_MAINNET.is_emerald_active_at_timestamp(0));
    }
}
