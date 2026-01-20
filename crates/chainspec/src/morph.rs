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
    use crate::{MORPH_MAINNET_CHAIN_ID, hardfork::MorphHardforks};
    use alloy_primitives::address;

    #[test]
    fn test_morph_mainnet_chain_id() {
        assert_eq!(MORPH_MAINNET.inner.chain.id(), MORPH_MAINNET_CHAIN_ID);
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
