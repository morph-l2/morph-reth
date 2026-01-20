//! Morph Hoodi (testnet) chain specification.

use crate::MorphChainSpec;
use alloy_genesis::Genesis;
use std::sync::{Arc, LazyLock};

/// Morph Hoodi (testnet) chain specification.
pub static MORPH_HOODI: LazyLock<Arc<MorphChainSpec>> = LazyLock::new(|| {
    let genesis: Genesis = serde_json::from_str(include_str!("../res/genesis/hoodi.json"))
        .expect("Failed to parse Morph Hoodi genesis");
    MorphChainSpec::from(genesis).into()
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MORPH_FEE_VAULT_ADDRESS_HOODI, MORPH_HOODI_CHAIN_ID, hardfork::MorphHardforks};

    #[test]
    fn test_morph_hoodi_chain_id() {
        assert_eq!(MORPH_HOODI.inner.chain.id(), MORPH_HOODI_CHAIN_ID);
    }

    #[test]
    fn test_morph_hoodi_fee_vault() {
        assert!(MORPH_HOODI.is_fee_vault_enabled());
        assert_eq!(
            MORPH_HOODI.fee_vault_address(),
            Some(MORPH_FEE_VAULT_ADDRESS_HOODI)
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
