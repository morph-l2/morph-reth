//! Morph-specific precompile provider.
//!
//! This module provides Morph-specific precompile sets that match the Go implementation
//! at <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go>.
//!
//! ## Precompile Sets by Hardfork (Incremental Evolution)
//!
//! ```text
//! Berlin (base)
//!   └── Bernoulli = Berlin - ripemd160 - blake2f
//!         └── Curie = Bernoulli (same)
//!               └── Morph203 = Bernoulli + blake2f + ripemd160
//!                     └── Viridian = Morph203 (same)
//!                           └── Emerald = Viridian + Osaka precompiles
//! ```
//!
//! | Hardfork    | Base      | Added                                           | Disabled          |
//! |-------------|-----------|------------------------------------------------|-------------------|
//! | Bernoulli   | Berlin    | -                                              | ripemd160/blake2f |
//! | Curie       | Bernoulli | -                                              | -                 |
//! | Morph203    | Bernoulli | blake2f, ripemd160                             | -                 |
//! | Viridian    | Morph203  | -                                              | -                 |
//! | Emerald     | Viridian  | Osaka (P256verify, BLS12-381, point eval, etc) | -                 |

use alloy_primitives::Address;
use morph_chainspec::hardfork::MorphHardfork;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::Precompiles,
    primitives::{OnceLock, hardfork::SpecId},
};
use std::boxed::Box;
use std::string::String;

/// Standard precompile addresses
pub mod addresses {
    use super::Address;

    /// ecrecover precompile address (0x01)
    pub const ECRECOVER: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
    ]);
    /// sha256 precompile address (0x02)
    pub const SHA256: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
    ]);
    /// ripemd160 precompile address (0x03)
    pub const RIPEMD160: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x03,
    ]);
    /// identity/datacopy precompile address (0x04)
    pub const IDENTITY: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x04,
    ]);
    /// modexp precompile address (0x05)
    pub const MODEXP: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x05,
    ]);
    /// bn256Add precompile address (0x06)
    pub const BN256_ADD: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x06,
    ]);
    /// bn256ScalarMul precompile address (0x07)
    pub const BN256_MUL: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x07,
    ]);
    /// bn256Pairing precompile address (0x08)
    pub const BN256_PAIRING: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x08,
    ]);
    /// blake2f precompile address (0x09)
    pub const BLAKE2F: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x09,
    ]);
    /// point evaluation precompile address (0x0a) - EIP-4844
    pub const POINT_EVALUATION: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x0a,
    ]);
    /// P256verify precompile address (0x100) - RIP-7212
    pub const P256_VERIFY: Address = Address::new([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0x00,
    ]);
}

/// Morph precompile provider.
///
/// Implements Morph-specific precompile sets that match the Go implementation.
/// Each hardfork has specific precompiles enabled/disabled.
#[derive(Debug, Clone)]
pub struct MorphPrecompiles {
    /// Inner Ethereum precompile provider.
    inner: EthPrecompiles,
    /// Current Morph hardfork.
    spec: MorphHardfork,
}

impl MorphPrecompiles {
    /// Create a new precompile provider with the given Morph hardfork.
    ///
    /// Maps hardforks to their precompile sets based on the Go implementation:
    /// <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go>
    #[inline]
    pub fn new_with_spec(spec: MorphHardfork) -> Self {
        let precompiles = match spec {
            // Bernoulli and Curie share the same precompile set
            // Go implementation has no PrecompiledContractsCurie
            MorphHardfork::Bernoulli | MorphHardfork::Curie => bernoulli(),
            // Morph203: enables blake2f
            MorphHardfork::Morph203 => morph203(),
            // Viridian: adds P256verify (RIP-7212)
            MorphHardfork::Viridian => viridian(),
            // Emerald: adds BLS12-381, all precompiles enabled
            MorphHardfork::Emerald | _ => emerald(),
        };

        Self {
            inner: EthPrecompiles {
                precompiles,
                spec: SpecId::default(),
            },
            spec,
        }
    }

    /// Returns the underlying precompiles.
    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }

    /// Returns whether the address is a precompile.
    #[inline]
    pub fn contains(&self, address: &Address) -> bool {
        self.inner.contains(address)
    }
}

impl Default for MorphPrecompiles {
    fn default() -> Self {
        Self::new_with_spec(MorphHardfork::default())
    }
}

/// Returns precompiles for Bernoulli hardfork.
///
/// Based on Berlin but with ripemd160 and blake2f excluded.
/// Enabled: ecrecover, sha256, identity, modexp, bn256 ops
/// Disabled: ripemd160 (0x03), blake2f (0x09)
///
/// Matches: <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go#L136-L148>
pub fn bernoulli() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let berlin = Precompiles::berlin();

        // Create a set with only ripemd160 and blake2f to exclude
        let mut to_exclude = Precompiles::default();
        if let Some(ripemd) = berlin.get(&addresses::RIPEMD160) {
            to_exclude.extend([ripemd.clone()]);
        }
        if let Some(blake2f) = berlin.get(&addresses::BLAKE2F) {
            to_exclude.extend([blake2f.clone()]);
        }

        // Return berlin precompiles minus the excluded ones
        berlin.difference(&to_exclude)
    })
}

/// Returns precompiles for Morph203 hardfork.
///
/// Based on Bernoulli with blake2f and ripemd160 re-enabled.
/// Enabled: ecrecover, sha256, ripemd160, identity, modexp, bn256 ops, blake2f
///
/// Matches: PrecompiledContractsMorph203 in Go
pub fn morph203() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // Start from Bernoulli and add blake2f + ripemd160
        let mut precompiles = bernoulli().clone();

        let berlin = Precompiles::berlin();
        // Add blake2f back (was disabled in Bernoulli)
        if let Some(blake2f) = berlin.get(&addresses::BLAKE2F) {
            precompiles.extend([blake2f.clone()]);
        }
        // Add ripemd160 back (was disabled in Bernoulli)
        if let Some(ripemd) = berlin.get(&addresses::RIPEMD160) {
            precompiles.extend([ripemd.clone()]);
        }

        precompiles
    })
}

/// Returns precompiles for Viridian hardfork.
///
/// Same as Morph203 - no additional precompiles.
/// Enabled: ecrecover, sha256, ripemd160, identity, modexp, bn256 ops, blake2f
///
/// Matches: PrecompiledContractsViridian in Go (same as Morph203)
pub fn viridian() -> &'static Precompiles {
    // Viridian uses the same precompile set as Morph203
    morph203()
}

/// Returns precompiles for Emerald hardfork.
///
/// Based on Viridian with Osaka precompiles added.
/// - All standard precompiles (ecrecover, sha256, ripemd160, identity, modexp, bn256 ops, blake2f)
/// - Osaka precompiles (P256verify RIP-7212, BLS12-381 EIP-2537, etc.)
///
/// Matches: PrecompiledContractsEmerald in Go
pub fn emerald() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // Start from Viridian
        let mut precompiles = viridian().clone();

        // Add Osaka precompiles (includes P256verify, BLS12-381, etc.)
        let osaka = Precompiles::osaka();
        for addr in osaka.addresses() {
            // Skip precompiles we already have
            if !precompiles.contains(addr) {
                if let Some(precompile) = osaka.get(addr) {
                    precompiles.extend([precompile.clone()]);
                }
            }
        }

        precompiles
    })
}

impl<CTX> PrecompileProvider<CTX> for MorphPrecompiles
where
    CTX: ContextTr<Cfg: Cfg<Spec = MorphHardfork>>,
{
    type Output = InterpreterResult;

    #[inline]
    fn set_spec(&mut self, spec: <CTX::Cfg as Cfg>::Spec) -> bool {
        if spec == self.spec {
            return false;
        }
        *self = Self::new_with_spec(spec);
        true
    }

    #[inline]
    fn run(
        &mut self,
        context: &mut CTX,
        inputs: &CallInputs,
    ) -> Result<Option<Self::Output>, String> {
        self.inner.run(context, inputs)
    }

    #[inline]
    fn warm_addresses(&self) -> Box<impl Iterator<Item = Address>> {
        self.inner.warm_addresses()
    }

    #[inline]
    fn contains(&self, address: &Address) -> bool {
        Self::contains(self, address)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bernoulli_precompiles() {
        let precompiles = bernoulli();

        // Should have ecrecover, sha256, identity, modexp, bn256 ops
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));
        assert!(precompiles.contains(&addresses::IDENTITY));
        assert!(precompiles.contains(&addresses::MODEXP));
        assert!(precompiles.contains(&addresses::BN256_ADD));

        // ripemd160 and blake2f should NOT be present (disabled in Bernoulli)
        // Matches Go: PrecompiledContractsBernoulli
        assert!(!precompiles.contains(&addresses::RIPEMD160));
        assert!(!precompiles.contains(&addresses::BLAKE2F));
    }

    #[test]
    fn test_curie_uses_bernoulli_precompiles() {
        // Curie uses the same precompile set as Bernoulli
        // Go implementation has no PrecompiledContractsCurie
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);

        // Both should have the same precompiles
        assert_eq!(bernoulli_p.precompiles().len(), curie_p.precompiles().len());

        // Both should have sha256 enabled, ripemd160/blake2f disabled
        assert!(curie_p.contains(&addresses::SHA256));
        assert!(!curie_p.contains(&addresses::RIPEMD160));
        assert!(!curie_p.contains(&addresses::BLAKE2F));
    }

    #[test]
    fn test_morph203_precompiles() {
        let precompiles = morph203();

        // Should have blake2f and ripemd160 re-enabled
        assert!(precompiles.contains(&addresses::BLAKE2F));
        assert!(precompiles.contains(&addresses::RIPEMD160));

        // All standard precompiles
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));

        // P256verify not yet added in Morph203
        assert!(!precompiles.contains(&addresses::P256_VERIFY));
    }

    #[test]
    fn test_viridian_precompiles() {
        let precompiles = viridian();

        // Same as Morph203 - no P256verify yet
        assert!(!precompiles.contains(&addresses::P256_VERIFY));

        // Should have blake2f and ripemd160 enabled (same as Morph203)
        assert!(precompiles.contains(&addresses::BLAKE2F));
        assert!(precompiles.contains(&addresses::RIPEMD160));

        // Viridian and Morph203 should be identical
        assert_eq!(viridian().len(), morph203().len());
    }

    #[test]
    fn test_emerald_precompiles() {
        let precompiles = emerald();

        // All standard precompiles should be enabled
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));
        assert!(precompiles.contains(&addresses::RIPEMD160)); // Now enabled!
        assert!(precompiles.contains(&addresses::IDENTITY));
        assert!(precompiles.contains(&addresses::MODEXP));
        assert!(precompiles.contains(&addresses::BN256_ADD));
        assert!(precompiles.contains(&addresses::BLAKE2F));

        // P256verify should be present
        assert!(precompiles.contains(&addresses::P256_VERIFY));
    }

    #[test]
    fn test_precompile_counts_increase() {
        let bernoulli_count = bernoulli().len();
        let morph203_count = morph203().len();
        let viridian_count = viridian().len();
        let emerald_count = emerald().len();

        // Morph203 should have more than Bernoulli (adds blake2f + ripemd160)
        assert!(morph203_count > bernoulli_count);

        // Viridian is the same as Morph203
        assert_eq!(viridian_count, morph203_count);

        // Emerald should have more than Viridian (adds P256verify + BLS12-381)
        assert!(emerald_count > viridian_count);
    }

    #[test]
    fn test_hardfork_specific_precompiles() {
        // Verify that each hardfork has the expected precompile configuration
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);
        let morph203_p = MorphPrecompiles::new_with_spec(MorphHardfork::Morph203);
        let viridian_p = MorphPrecompiles::new_with_spec(MorphHardfork::Viridian);
        let emerald_p = MorphPrecompiles::new_with_spec(MorphHardfork::Emerald);

        // Bernoulli and Curie: no ripemd160, no blake2f (same precompile set)
        assert!(!bernoulli_p.contains(&addresses::RIPEMD160));
        assert!(!bernoulli_p.contains(&addresses::BLAKE2F));
        assert!(!curie_p.contains(&addresses::RIPEMD160));
        assert!(!curie_p.contains(&addresses::BLAKE2F));

        // Morph203: blake2f + ripemd160 enabled, no P256verify
        assert!(morph203_p.contains(&addresses::RIPEMD160));
        assert!(morph203_p.contains(&addresses::BLAKE2F));
        assert!(!morph203_p.contains(&addresses::P256_VERIFY));

        // Viridian: same as Morph203, no P256verify
        assert!(viridian_p.contains(&addresses::RIPEMD160));
        assert!(viridian_p.contains(&addresses::BLAKE2F));
        assert!(!viridian_p.contains(&addresses::P256_VERIFY));

        // Emerald: all precompiles enabled including P256verify and BLS12-381
        assert!(emerald_p.contains(&addresses::RIPEMD160));
        assert!(emerald_p.contains(&addresses::P256_VERIFY));
    }
}
