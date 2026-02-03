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
//! Emerald, and MPTFork use timestamp-based activation.

use alloy_evm::revm::primitives::hardfork::SpecId;
use alloy_hardforks::hardfork;
use reth_chainspec::{EthereumHardforks, ForkCondition};

hardfork!(
    /// Morph-specific hardforks for network upgrades.
    ///
    /// Note: Bernoulli and Curie use block-based activation, while Morph203, Viridian,
    /// Emerald, and MPTFork use timestamp-based activation (matching go-ethereum behavior).
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
        /// MPTFork hardfork (timestamp-based).
        #[default]
        MPTFork,
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

    /// Returns `true` if this hardfork is MPTFork or later.
    #[inline]
    pub fn is_mpt_fork(self) -> bool {
        self >= Self::MPTFork
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

    /// Convenience method to check if MPTFork hardfork is active at a given timestamp.
    fn is_mpt_fork_active_at_timestamp(&self, timestamp: u64) -> bool {
        self.morph_fork_activation(MorphHardfork::MPTFork)
            .active_at_timestamp(timestamp)
    }

    /// Retrieves the latest Morph hardfork active at a given block and timestamp.
    ///
    /// Note: This method checks both block-based (Bernoulli, Curie) and
    /// timestamp-based (Morph203, Viridian, Emerald, MPTFork) hardforks.
    fn morph_hardfork_at(&self, block_number: u64, timestamp: u64) -> MorphHardfork {
        if self.is_mpt_fork_active_at_timestamp(timestamp) {
            MorphHardfork::MPTFork
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
    fn from(value: MorphHardfork) -> Self {
        match value {
            MorphHardfork::Bernoulli => Self::OSAKA,
            MorphHardfork::Curie => Self::OSAKA,
            MorphHardfork::Morph203 => Self::OSAKA,
            MorphHardfork::Viridian => Self::OSAKA,
            MorphHardfork::Emerald => Self::OSAKA,
            MorphHardfork::MPTFork => Self::OSAKA,
        }
    }
}

impl From<SpecId> for MorphHardfork {
    /// Maps a [`SpecId`] to the *latest compatible* [`MorphHardfork`].
    ///
    /// Note: this is intentionally not a strict inverse of
    /// `From<MorphHardfork> for SpecId`, because multiple Morph
    /// hardforks may share the same underlying EVM spec.
    fn from(spec: SpecId) -> Self {
        if spec.is_enabled_in(SpecId::from(Self::MPTFork)) {
            Self::MPTFork
        } else if spec.is_enabled_in(SpecId::from(Self::Emerald)) {
            Self::Emerald
        } else if spec.is_enabled_in(SpecId::from(Self::Viridian)) {
            Self::Viridian
        } else if spec.is_enabled_in(SpecId::from(Self::Morph203)) {
            Self::Morph203
        } else if spec.is_enabled_in(SpecId::from(Self::Curie)) {
            Self::Curie
        } else {
            Self::Bernoulli
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reth_chainspec::Hardfork;

    #[test]
    fn test_morph203_hardfork_name() {
        let fork = MorphHardfork::Morph203;
        assert_eq!(fork.name(), "Morph203");
    }

    #[test]
    fn test_hardfork_trait_implementation() {
        let fork = MorphHardfork::Morph203;
        // Should implement Hardfork trait
        let _name: &str = Hardfork::name(&fork);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn test_morph_hardfork_serde() {
        let fork = MorphHardfork::Morph203;

        // Serialize to JSON
        let json = serde_json::to_string(&fork).unwrap();
        assert_eq!(json, "\"Morph203\"");

        // Deserialize from JSON
        let deserialized: MorphHardfork = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, fork);
    }

    #[test]
    fn test_is_curie() {
        assert!(!MorphHardfork::Bernoulli.is_curie());
        assert!(MorphHardfork::Curie.is_curie());
        assert!(MorphHardfork::Morph203.is_curie());
        assert!(MorphHardfork::Viridian.is_curie());
        assert!(MorphHardfork::Emerald.is_curie());
        assert!(MorphHardfork::MPTFork.is_curie());
    }

    #[test]
    fn test_is_morph203() {
        assert!(!MorphHardfork::Bernoulli.is_morph203());
        assert!(!MorphHardfork::Curie.is_morph203());
        assert!(MorphHardfork::Morph203.is_morph203());
        assert!(MorphHardfork::Viridian.is_morph203());
        assert!(MorphHardfork::Emerald.is_morph203());
        assert!(MorphHardfork::MPTFork.is_morph203());
    }

    #[test]
    fn test_is_viridian() {
        assert!(!MorphHardfork::Bernoulli.is_viridian());
        assert!(!MorphHardfork::Curie.is_viridian());
        assert!(!MorphHardfork::Morph203.is_viridian());
        assert!(MorphHardfork::Viridian.is_viridian());
        assert!(MorphHardfork::Emerald.is_viridian());
        assert!(MorphHardfork::MPTFork.is_viridian());
    }

    #[test]
    fn test_is_emerald() {
        assert!(!MorphHardfork::Bernoulli.is_emerald());
        assert!(!MorphHardfork::Curie.is_emerald());
        assert!(!MorphHardfork::Morph203.is_emerald());
        assert!(!MorphHardfork::Viridian.is_emerald());
        assert!(MorphHardfork::Emerald.is_emerald());
        assert!(MorphHardfork::MPTFork.is_emerald());
    }

    #[test]
    fn test_is_mpt_fork() {
        assert!(!MorphHardfork::Bernoulli.is_mpt_fork());
        assert!(!MorphHardfork::Curie.is_mpt_fork());
        assert!(!MorphHardfork::Morph203.is_mpt_fork());
        assert!(!MorphHardfork::Viridian.is_mpt_fork());
        assert!(!MorphHardfork::Emerald.is_mpt_fork());
        assert!(MorphHardfork::MPTFork.is_mpt_fork());
    }
}
