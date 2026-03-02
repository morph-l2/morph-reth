//! Morph-specific precompile provider.
//!
//! This module provides Morph-specific precompile sets that match the Go implementation
//! at <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go>.
//!
//! ## Precompile Sets by Hardfork (Incremental Evolution)
//!
//! ```text
//! Berlin (base)
//!   └── Bernoulli/Curie = Berlin with ripemd160/blake2f replaced by disabled stubs
//!         └── Morph203/Viridian = Bernoulli with ripemd160/blake2f re-enabled (working)
//!               └── Emerald = Morph203 + Osaka precompiles
//! ```
//!
//! | Hardfork         | Base      | Added                                                     | Notes                         |
//! |------------------|-----------|----------------------------------------------------------|-------------------------------|
//! | Bernoulli/Curie  | Berlin    | -                                                        | ripemd160/blake2f as disabled stubs |
//! | Morph203/Viridian| Bernoulli | blake2f, ripemd160 (working)                             | replaces disabled stubs       |
//! | Emerald          | Morph203  | Osaka (P256verify, BLS12-381, point eval, etc)           | -                             |
//!
//! ## Why Disabled Stubs?
//!
//! go-ethereum's `PrecompiledContractsBernoulli` includes 0x03 (ripemd160) and 0x09 (blake2f)
//! as disabled stubs (`ripemd160hashDisabled`, `blake2FDisabled`). This has two effects:
//!
//! 1. Both addresses are included in `PrecompiledAddressesBernoulli`, so they get **warmed**
//!    via `StateDB.Prepare` (EIP-2929). CALL costs 100 gas (warm) instead of 2600 (cold).
//!
//! 2. When called, go-eth's CALL handler sets `gas = 0` for any non-revert error, consuming
//!    all forwarded gas. revm's `PrecompileError` result also causes all forwarded gas to
//!    be consumed (parent does not reclaim gas when sub-call is not ok-or-revert).
//!
//! Omitting these stubs causes morph-reth to treat 0x03/0x09 as cold empty accounts (2600
//! base cost, forwarded gas returned), creating a gas mismatch vs go-ethereum.

use alloy_primitives::Address;
use morph_chainspec::hardfork::MorphHardfork;
use revm::{
    context::Cfg,
    context_interface::ContextTr,
    handler::{EthPrecompiles, PrecompileProvider},
    interpreter::{CallInputs, InterpreterResult},
    precompile::{Precompile, PrecompileError, PrecompileId, PrecompileResult, Precompiles},
    primitives::{OnceLock, hardfork::SpecId},
};
use std::boxed::Box;
use std::string::String;

/// Standard precompile addresses
pub mod addresses {
    use super::Address;
    use revm::precompile::u64_to_address;

    /// ecrecover precompile address (1)
    pub const ECRECOVER: Address = u64_to_address(1);
    /// sha256 precompile address (2)
    pub const SHA256: Address = u64_to_address(2);
    /// ripemd160 precompile address (3)
    pub const RIPEMD160: Address = u64_to_address(3);
    /// identity/datacopy precompile address (4)
    pub const IDENTITY: Address = u64_to_address(4);
    /// modexp precompile address (5)
    pub const MODEXP: Address = u64_to_address(5);
    /// bn256Add precompile address (6)
    pub const BN256_ADD: Address = u64_to_address(6);
    /// bn256ScalarMul precompile address (7)
    pub const BN256_MUL: Address = u64_to_address(7);
    /// bn256Pairing precompile address (8)
    pub const BN256_PAIRING: Address = u64_to_address(8);
    /// blake2f precompile address (9)
    pub const BLAKE2F: Address = u64_to_address(9);
    /// point evaluation precompile address (10) - EIP-4844
    pub const POINT_EVALUATION: Address = u64_to_address(10);
    /// P256verify precompile address (256) - RIP-7212
    pub const P256_VERIFY: Address = u64_to_address(256);
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
            // Morph203 and Viridian share the same precompile set
            MorphHardfork::Morph203 | MorphHardfork::Viridian => morph203(),
            // Emerald: adds Osaka precompiles (P256verify, BLS12-381, etc)
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

/// Disabled stub for ripemd160 (0x03) in Bernoulli/Curie hardfork.
///
/// Returns `PrecompileError` to consume all forwarded gas, matching go-ethereum's behavior
/// where a disabled precompile error causes the CALL handler to burn all remaining gas.
fn ripemd160_disabled(_input: &[u8], _gas_limit: u64) -> PrecompileResult {
    Err(PrecompileError::Other(
        "ripemd160 precompile disabled in Bernoulli/Curie hardfork".into(),
    ))
}

/// Disabled stub for blake2f (0x09) in Bernoulli/Curie hardfork.
///
/// Returns `PrecompileError` to consume all forwarded gas, matching go-ethereum's behavior
/// where a disabled precompile error causes the CALL handler to burn all remaining gas.
fn blake2f_disabled(_input: &[u8], _gas_limit: u64) -> PrecompileResult {
    Err(PrecompileError::Other(
        "blake2f precompile disabled in Bernoulli/Curie hardfork".into(),
    ))
}

/// Returns precompiles for Bernoulli hardfork.
///
/// Based on Berlin with ripemd160 (0x03) and blake2f (0x09) replaced by disabled stubs.
/// All 9 Berlin addresses are present (so they get warmed via EIP-2929), but 0x03/0x09
/// consume all forwarded gas and return failure when called.
///
/// Matches: <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go#L136-L148>
pub fn bernoulli() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // Start from Berlin (9 precompiles including 0x03 and 0x09).
        let mut precompiles = Precompiles::berlin().clone();

        // Replace ripemd160 (0x03) and blake2f (0x09) with disabled stubs.
        // This keeps them in warm_addresses() so EIP-2929 warms them (100 gas instead of
        // 2600 cold), matching go-ethereum's PrecompiledContractsBernoulli behavior.
        precompiles.extend([
            Precompile::new(
                PrecompileId::Ripemd160,
                addresses::RIPEMD160,
                ripemd160_disabled,
            ),
            Precompile::new(PrecompileId::Blake2F, addresses::BLAKE2F, blake2f_disabled),
        ]);

        precompiles
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

/// Returns precompiles for Emerald hardfork.
///
/// Based on Morph203/Viridian with Osaka precompiles added.
/// - All standard precompiles (ecrecover, sha256, ripemd160, identity, modexp, bn256 ops, blake2f)
/// - Osaka precompiles (P256verify RIP-7212, BLS12-381 EIP-2537, etc.)
///
/// Matches: PrecompiledContractsEmerald in Go
pub fn emerald() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        // Start from Morph203/Viridian
        let mut precompiles = morph203().clone();

        // Add Osaka precompiles (includes P256verify, BLS12-381, etc.)
        let osaka = Precompiles::osaka();
        for addr in osaka.addresses() {
            // Skip precompiles we already have
            if !precompiles.contains(addr)
                && let Some(precompile) = osaka.get(addr)
            {
                precompiles.extend([precompile.clone()]);
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

        // ripemd160 (0x03) and blake2f (0x09) ARE present as disabled stubs.
        // They must be in the precompile set so they get warmed (EIP-2929: 100 gas warm
        // instead of 2600 cold), matching go-ethereum's PrecompiledContractsBernoulli
        // which includes &ripemd160hashDisabled{} and &blake2FDisabled{}.
        assert!(precompiles.contains(&addresses::RIPEMD160));
        assert!(precompiles.contains(&addresses::BLAKE2F));
    }

    #[test]
    fn test_curie_uses_bernoulli_precompiles() {
        // Curie uses the same precompile set as Bernoulli
        // Go implementation has no PrecompiledContractsCurie
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);

        // Both should have the same precompiles
        assert_eq!(bernoulli_p.precompiles().len(), curie_p.precompiles().len());

        // Both should have sha256 enabled and 0x03/0x09 as disabled stubs (present in set)
        assert!(curie_p.contains(&addresses::SHA256));
        assert!(curie_p.contains(&addresses::RIPEMD160));
        assert!(curie_p.contains(&addresses::BLAKE2F));
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
        let emerald_count = emerald().len();

        // Bernoulli and Morph203 have the same number of addresses (9), but
        // Bernoulli has 0x03/0x09 as disabled stubs while Morph203 re-enables them.
        assert_eq!(morph203_count, bernoulli_count);

        // Emerald should have more than Morph203 (adds Osaka precompiles)
        assert!(emerald_count > morph203_count);
    }

    #[test]
    fn test_hardfork_specific_precompiles() {
        // Verify that each hardfork has the expected precompile configuration
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);
        let morph203_p = MorphPrecompiles::new_with_spec(MorphHardfork::Morph203);
        let viridian_p = MorphPrecompiles::new_with_spec(MorphHardfork::Viridian);
        let emerald_p = MorphPrecompiles::new_with_spec(MorphHardfork::Emerald);

        // Bernoulli and Curie: ripemd160 and blake2f are present as disabled stubs (same precompile set).
        // They're in the set to ensure EIP-2929 warming matches go-ethereum.
        assert!(bernoulli_p.contains(&addresses::RIPEMD160));
        assert!(bernoulli_p.contains(&addresses::BLAKE2F));
        assert!(curie_p.contains(&addresses::RIPEMD160));
        assert!(curie_p.contains(&addresses::BLAKE2F));

        // Morph203 and Viridian: blake2f + ripemd160 enabled, no P256verify (same precompile set)
        assert!(morph203_p.contains(&addresses::RIPEMD160));
        assert!(morph203_p.contains(&addresses::BLAKE2F));
        assert!(!morph203_p.contains(&addresses::P256_VERIFY));
        assert!(viridian_p.contains(&addresses::RIPEMD160));
        assert!(viridian_p.contains(&addresses::BLAKE2F));
        assert!(!viridian_p.contains(&addresses::P256_VERIFY));
        assert_eq!(
            morph203_p.precompiles().len(),
            viridian_p.precompiles().len()
        );

        // Emerald: all precompiles enabled including Osaka precompiles (P256verify, BLS12-381, etc)
        assert!(emerald_p.contains(&addresses::RIPEMD160));
        assert!(emerald_p.contains(&addresses::P256_VERIFY));
    }
}
