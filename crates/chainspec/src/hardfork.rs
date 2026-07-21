//! Morph-specific hardfork definitions and traits.
//!
//! This module provides the infrastructure for managing hardfork transitions in Morph.
//!
//! ## Adding a New Hardfork
//!
//! When a new hardfork is needed (e.g., `Vivace`):
//!
//! ### In `hardfork.rs`:
//! 1. Add a new variant to `MorphHardfork` enum
//! 2. Add `is_vivace()` method to `MorphHardfork` impl
//! 3. Add `is_vivace_active_at_timestamp()` to `MorphHardforks` trait
//! 4. Update `morph_hardfork_at()` to check for the new hardfork first (latest hardfork is checked first)
//! 5. Add `MorphHardfork::Vivace => Self::OSAKA` (or appropriate SpecId) in `From<MorphHardfork> for SpecId`
//! 6. Update `From<SpecId> for MorphHardfork` to check for the new hardfork first
//! 7. Add test `test_is_vivace` and update existing `is_*` tests to include the new variant
//!
//! ### In `spec.rs`:
//! 8. Add `vivace_time: Option<u64>` field to `MorphGenesisInfo`
//! 9. Extract `vivace_time` in `From<Genesis> for MorphChainSpec`
//! 10. Add `(MorphHardfork::Vivace, vivace_time)` to `morph_forks` vec
//! 11. Update tests to include `"vivaceTime": <timestamp>` in genesis JSON
//!
//! ### In genesis files and generator:
//! 12. Add `"vivaceTime": 0` to `genesis/dev.json`
//! 13. Add `vivace_time: Option<u64>` arg to `xtask/src/genesis_args.rs`
//! 14. Add insertion of `"vivaceTime"` to chain_config.extra_fields
//!
//! ## Current State
//!
//! Bernoulli and Curie use block-based activation, while Morph203, Viridian,
//! Emerald, Jade, and Onyx use timestamp-based activation.

use alloy_evm::revm::primitives::hardfork::SpecId;
use alloy_hardforks::hardfork;
use reth_chainspec::{EthereumHardforks, ForkCondition};

hardfork!(
    /// Morph-specific hardforks for network upgrades.
    ///
    /// Note: Bernoulli and Curie use block-based activation, while Morph203, Viridian,
    /// Emerald, Jade, and Onyx use timestamp-based activation (matching go-ethereum behavior).
    #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
    #[derive(Default)]
    MorphHardfork {
        /// Bernoulli hardfork (block-based).
        Bernoulli,
        /// Curie hardfork (block-based).
        Curie,
        /// Morph203 hardfork (timestamp-based).
        Morph203,
        /// Viridian hardfork (timestamp-based).
        Viridian,
        /// Emerald hardfork (timestamp-based).
        Emerald,
        /// Jade hardfork (timestamp-based).
        Jade,
        /// Onyx hardfork (timestamp-based).
        #[default]
        Onyx,
    }
);

impl MorphHardfork {
    /// Returns `true` if this hardfork is Bernoulli or later.
    #[inline]
    pub fn is_bernoulli(self) -> bool {
        self >= Self::Bernoulli
    }

    /// Returns `true` if this hardfork is Curie or later.
    #[inline]
    pub fn is_curie(self) -> bool {
        self >= Self::Curie
    }

    /// Returns `true` if this hardfork is Morph203 or later.
    #[inline]
    pub fn is_morph203(self) -> bool {
        self >= Self::Morph203
    }

    /// Returns `true` if this hardfork is Viridian or later.
    #[inline]
    pub fn is_viridian(self) -> bool {
        self >= Self::Viridian
    }

    /// Returns `true` if this hardfork is Emerald or later.
    #[inline]
    pub fn is_emerald(self) -> bool {
        self >= Self::Emerald
    }

    /// Returns `true` if this hardfork is Jade or later.
    #[inline]
    pub fn is_jade(self) -> bool {
        self >= Self::Jade
    }

    /// Returns `true` if this hardfork is Onyx or later.
    #[inline]
    pub fn is_onyx(self) -> bool {
        self >= Self::Onyx
    }
}

/// Trait for querying Morph-specific hardfork activations.
#[auto_impl::auto_impl(&, Arc)]
pub trait MorphHardforks: EthereumHardforks {
    /// Retrieves activation condition for a Morph-specific hardfork
    fn morph_fork_activation(&self, fork: MorphHardfork) -> ForkCondition;

    /// Convenience method to check if Bernoulli hardfork is active at a given block number.
    /// Note: Bernoulli uses block-based activation.
    fn is_bernoulli_active_at_block(&self, block_number: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Bernoulli)
            .active_at_block(block_number)
    }

    /// Convenience method to check if Curie hardfork is active at a given block number.
    /// Note: Curie uses block-based activation.
    fn is_curie_active_at_block(&self, block_number: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Curie)
            .active_at_block(block_number)
    }

    /// Convenience method to check if Morph203 hardfork is active at a given timestamp.
    fn is_morph203_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Morph203)
            .active_at_timestamp(timestamp)
    }

    /// Convenience method to check if Viridian hardfork is active at a given timestamp.
    fn is_viridian_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Viridian)
            .active_at_timestamp(timestamp)
    }

    /// Convenience method to check if Emerald hardfork is active at a given timestamp.
    fn is_emerald_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Emerald)
            .active_at_timestamp(timestamp)
    }

    /// Convenience method to check if Jade hardfork is active at a given timestamp.
    fn is_jade_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Jade)
            .active_at_timestamp(timestamp)
    }

    /// Convenience method to check if Onyx hardfork is active at a given timestamp.
    fn is_onyx_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::Onyx)
            .active_at_timestamp(timestamp)
    }

    /// Retrieves the latest Morph hardfork active at a given block and timestamp.
    ///
    /// Note: This method checks both block-based (Bernoulli, Curie) and
    /// timestamp-based (Morph203, Viridian, Emerald, Jade, Onyx) hardforks.
    fn morph_hardfork_at(&self, block_number: u64, timestamp: u64) -> MorphHardfork {
        if self.is_onyx_active_at_timestamp(timestamp) {
            MorphHardfork::Onyx
        } else if self.is_jade_active_at_timestamp(timestamp) {
            MorphHardfork::Jade
        } else if self.is_emerald_active_at_timestamp(timestamp) {
            MorphHardfork::Emerald
        } else if self.is_viridian_active_at_timestamp(timestamp) {
            MorphHardfork::Viridian
        } else if self.is_morph203_active_at_timestamp(timestamp) {
            MorphHardfork::Morph203
        } else if self.is_curie_active_at_block(block_number) {
            MorphHardfork::Curie
        } else {
            // Default to Bernoulli (baseline)
            MorphHardfork::Bernoulli
        }
    }
}

impl From<MorphHardfork> for SpecId {
    /// Maps a [`MorphHardfork`] to the closest standard [`SpecId`].
    ///
    /// The mapping must match go-ethereum Morph's EVM instruction sets:
    /// - Bernoulli/Curie/Morph203 = CANCUN gas tables (MCOPY, TSTORE/TLOAD, transient storage)
    /// - Viridian = PRAGUE (adds EIP-7702 delegation designator)
    /// - Emerald/Jade/Onyx = OSAKA (adds EIP-7939 CLZ opcode)
    fn from(value: MorphHardfork) -> Self {
        match value {
            MorphHardfork::Bernoulli | MorphHardfork::Curie | MorphHardfork::Morph203 => {
                Self::CANCUN
            }
            MorphHardfork::Viridian => Self::PRAGUE,
            MorphHardfork::Emerald | MorphHardfork::Jade | MorphHardfork::Onyx => Self::OSAKA,
        }
    }
}

impl From<SpecId> for MorphHardfork {
    /// Maps a [`SpecId`] to the *latest compatible* [`MorphHardfork`].
    ///
    /// Since multiple Morph hardforks share the same SpecId (e.g.
    /// Bernoulli/Curie/Morph203 all map to CANCUN), this returns the
    /// latest hardfork for the given spec level.
    fn from(spec: SpecId) -> Self {
        if spec.is_enabled_in(SpecId::OSAKA) {
            Self::Onyx
        } else if spec.is_enabled_in(SpecId::PRAGUE) {
            Self::Viridian
        } else {
            // CANCUN or below → Morph203 (latest CANCUN-level hardfork)
            Self::Morph203
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::revm::context_interface::cfg::gas_params::GasParams;

    #[test]
    fn test_morph_hardfork_to_specid_mapping() {
        assert_eq!(SpecId::from(MorphHardfork::Bernoulli), SpecId::CANCUN);
        assert_eq!(SpecId::from(MorphHardfork::Curie), SpecId::CANCUN);
        assert_eq!(SpecId::from(MorphHardfork::Morph203), SpecId::CANCUN);
        assert_eq!(SpecId::from(MorphHardfork::Viridian), SpecId::PRAGUE);
        assert_eq!(SpecId::from(MorphHardfork::Emerald), SpecId::OSAKA);
        assert_eq!(SpecId::from(MorphHardfork::Jade), SpecId::OSAKA);
        assert_eq!(SpecId::from(MorphHardfork::Onyx), SpecId::OSAKA);
    }

    #[test]
    fn test_onyx_is_latest_osaka_hardfork() {
        assert!(!MorphHardfork::Jade.is_onyx());
        assert!(MorphHardfork::Onyx.is_onyx());
        assert_eq!(SpecId::from(MorphHardfork::Onyx), SpecId::OSAKA);
        assert_eq!(MorphHardfork::from(SpecId::OSAKA), MorphHardfork::Onyx);
    }

    #[test]
    fn test_morph_hardforks_do_not_enable_amsterdam_state_gas() {
        let forks = [
            MorphHardfork::Bernoulli,
            MorphHardfork::Curie,
            MorphHardfork::Morph203,
            MorphHardfork::Viridian,
            MorphHardfork::Emerald,
            MorphHardfork::Jade,
            MorphHardfork::Onyx,
        ];

        for fork in forks {
            let spec = SpecId::from(fork);
            assert!(
                spec < SpecId::AMSTERDAM,
                "MorphHardfork {fork:?} maps to SpecId {spec:?}, which would enable Amsterdam-era gas semantics"
            );

            let params = GasParams::new_spec(spec);
            assert_eq!(
                params.tx_eip7702_state_gas(),
                0,
                "MorphHardfork {fork:?} must not enable EIP-8037 state gas"
            );
        }
    }

    #[test]
    fn test_eip7702_refund_stays_regular_for_morph_specs() {
        for spec in [SpecId::CANCUN, SpecId::PRAGUE, SpecId::OSAKA] {
            let params = GasParams::new_spec(spec);
            assert_eq!(
                params.tx_eip7702_state_refund(2, 2),
                0,
                "spec={spec:?}: Morph must not route EIP-7702 refunds to state gas"
            );
            assert_eq!(
                params.tx_eip7702_auth_refund_regular(),
                params.tx_eip7702_auth_refund(),
                "spec={spec:?}: Morph EIP-7702 refunds must remain subject to the regular refund cap"
            );
        }
    }

    #[test]
    fn test_specid_to_morph_hardfork_mapping() {
        assert_eq!(MorphHardfork::from(SpecId::CANCUN), MorphHardfork::Morph203);
        assert_eq!(MorphHardfork::from(SpecId::PRAGUE), MorphHardfork::Viridian);
        assert_eq!(MorphHardfork::from(SpecId::OSAKA), MorphHardfork::Onyx);
    }

    /// SpecIds below CANCUN should map to Morph203 (the latest CANCUN-level hardfork).
    #[test]
    fn test_specid_below_cancun_maps_to_morph203() {
        assert_eq!(
            MorphHardfork::from(SpecId::SHANGHAI),
            MorphHardfork::Morph203
        );
        assert_eq!(
            MorphHardfork::from(SpecId::HOMESTEAD),
            MorphHardfork::Morph203
        );
    }

    /// Verify bidirectional mapping consistency: Hardfork -> SpecId -> Hardfork
    /// always returns the latest hardfork sharing that SpecId.
    #[test]
    fn test_specid_roundtrip_returns_latest_for_spec() {
        // Bernoulli -> CANCUN -> Morph203 (latest CANCUN hardfork)
        let spec = SpecId::from(MorphHardfork::Bernoulli);
        assert_eq!(MorphHardfork::from(spec), MorphHardfork::Morph203);

        // Emerald -> OSAKA -> Onyx (latest OSAKA hardfork)
        let spec = SpecId::from(MorphHardfork::Emerald);
        assert_eq!(MorphHardfork::from(spec), MorphHardfork::Onyx);
    }
}
