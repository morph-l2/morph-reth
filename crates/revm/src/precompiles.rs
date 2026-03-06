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
//! | Hardfork         | Base      | Changes                                                   | Notes                         |
//! |------------------|-----------|----------------------------------------------------------|-------------------------------|
//! | Bernoulli/Curie  | Berlin    | ripemd160/blake2f as disabled stubs; modexp 32B limit    | -                             |
//! | Morph203/Viridian| Bernoulli | blake2f/ripemd160 re-enabled; BN256 pairing 4-pair limit | -                             |
//! | Emerald          | Morph203  | BLS12-381, P256verify; modexp EIP-7823/7883 upgrade      | NO KZG (0x0a)                 |
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
    /// BLS12-381 G1 Add precompile address (0x0b)
    pub const BLS12_G1ADD: Address = u64_to_address(0x0b);
    /// BLS12-381 G1 MultiExp precompile address (0x0c)
    pub const BLS12_G1MULTIEXP: Address = u64_to_address(0x0c);
    /// BLS12-381 G2 Add precompile address (0x0d)
    pub const BLS12_G2ADD: Address = u64_to_address(0x0d);
    /// BLS12-381 G2 MultiExp precompile address (0x0e)
    pub const BLS12_G2MULTIEXP: Address = u64_to_address(0x0e);
    /// BLS12-381 Pairing precompile address (0x0f)
    pub const BLS12_PAIRING: Address = u64_to_address(0x0f);
    /// BLS12-381 Map FP to G1 precompile address (0x10)
    pub const BLS12_MAP_FP_TO_G1: Address = u64_to_address(0x10);
    /// BLS12-381 Map FP2 to G2 precompile address (0x11)
    pub const BLS12_MAP_FP2_TO_G2: Address = u64_to_address(0x11);
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

/// Checks if a 32-byte big-endian length field at `offset` in `data` exceeds 32.
///
/// Right-pads with zeros if `data` is shorter than `offset + 32`, matching
/// go-ethereum's `getData` semantics.
fn modexp_len_exceeds_32(data: &[u8], offset: usize) -> bool {
    let mut buf = [0u8; 32];
    let start = offset.min(data.len());
    let end = (offset + 32).min(data.len());
    let n = end.saturating_sub(start);
    if n > 0 {
        buf[..n].copy_from_slice(&data[start..end]);
    }
    // A big-endian 256-bit value > 32 iff any of the high 31 bytes is non-zero,
    // or the lowest byte exceeds 32.
    buf[..31].iter().any(|&b| b != 0) || buf[31] > 32
}

/// Wraps Berlin modexp with go-ethereum's 32-byte input length limit.
///
/// go-ethereum enforces `base_len, exp_len, mod_len <= 32` when `eip2565=true`
/// and neither `eip7823` nor `eip7883` is active (Bernoulli through Viridian).
/// Without this limit, morph-reth would accept arbitrarily large modexp inputs
/// that go-ethereum rejects, causing a consensus split.
///
/// Ref: <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go#L643-L648>
fn modexp_with_32byte_limit(input: &[u8], gas_limit: u64) -> PrecompileResult {
    // The first 96 bytes of modexp input are three 32-byte big-endian length fields:
    // [0..32] = base_len, [32..64] = exp_len, [64..96] = mod_len
    if modexp_len_exceeds_32(input, 0)
        || modexp_len_exceeds_32(input, 32)
        || modexp_len_exceeds_32(input, 64)
    {
        return Err(PrecompileError::Other(
            "modexp temporarily only accepts inputs of 32 bytes (256 bits) or less".into(),
        ));
    }

    // Delegate to Berlin modexp (EIP-2565 gas pricing, standard computation)
    Precompiles::berlin()
        .get(&addresses::MODEXP)
        .expect("Berlin precompiles must include modexp")
        .execute(input, gas_limit)
}

/// Wraps BN256 pairing with go-ethereum's 4-pair input length limit.
///
/// go-ethereum limits BN256 pairing to at most 4 pairs (768 bytes) from Morph203
/// onwards via `limitInputLength: true`. Without this limit, morph-reth would
/// accept larger pairing inputs, which can cause a consensus split if gas
/// accounting differs (the underlying computation is the same, but block gas
/// limits and metering become inconsistent).
///
/// Ref: <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go#L860-L865>
fn bn256_pairing_with_4pair_limit(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if input.len() > 4 * 192 {
        return Err(PrecompileError::Other(
            "bad elliptic curve pairing size".into(),
        ));
    }

    // Delegate to Berlin/Istanbul BN256 pairing
    Precompiles::berlin()
        .get(&addresses::BN256_PAIRING)
        .expect("Berlin precompiles must include BN256 pairing")
        .execute(input, gas_limit)
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

        // Replace modexp (0x05) with 32-byte input limit wrapper.
        // go-ethereum's Bernoulli modexp has eip2565=true but neither eip7823 nor eip7883,
        // which enforces base/exp/mod <= 32 bytes. Berlin modexp in revm has no such limit.
        precompiles.extend([Precompile::new(
            PrecompileId::ModExp,
            addresses::MODEXP,
            modexp_with_32byte_limit,
        )]);

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
        // Start from Bernoulli and re-enable blake2f + ripemd160
        let mut precompiles = bernoulli().clone();

        let berlin = Precompiles::berlin();
        // Re-enable blake2f (0x09) — was disabled stub in Bernoulli
        if let Some(blake2f) = berlin.get(&addresses::BLAKE2F) {
            precompiles.extend([blake2f.clone()]);
        }
        // Re-enable ripemd160 (0x03) — was disabled stub in Bernoulli
        if let Some(ripemd) = berlin.get(&addresses::RIPEMD160) {
            precompiles.extend([ripemd.clone()]);
        }

        // Replace BN256 pairing (0x08) with 4-pair limited version.
        // go-ethereum's Morph203 uses `limitInputLength: true` which caps
        // pairing input to 4 pairs (768 bytes).
        precompiles.extend([Precompile::new(
            PrecompileId::Bn254Pairing,
            addresses::BN256_PAIRING,
            bn256_pairing_with_4pair_limit,
        )]);

        precompiles
    })
}

/// Returns precompiles for Emerald hardfork.
///
/// Based on Morph203/Viridian with explicit additions matching go-ethereum's
/// `PrecompiledContractsEmerald`:
///
/// - Upgrades modexp (0x05) to EIP-7823 (1024-byte input cap) + EIP-7883 (new gas formula)
/// - Adds BLS12-381 precompiles (0x0b-0x11) from EIP-2537
/// - Adds P256verify (0x100) from RIP-7212
/// - Does **NOT** include KZG Point Evaluation (0x0a) — go-ethereum omits it
///
/// Ref: <https://github.com/morph-l2/go-ethereum/blob/main/core/vm/contracts.go#L152-L171>
pub fn emerald() -> &'static Precompiles {
    static INSTANCE: OnceLock<Precompiles> = OnceLock::new();
    INSTANCE.get_or_init(|| {
        let mut precompiles = morph203().clone();
        let osaka = Precompiles::osaka();

        // Upgrade modexp (0x05) from 32-byte-limited wrapper to osaka version.
        // Emerald uses eip7823=true (1024-byte input cap) + eip7883=true (new gas formula),
        // which replaces the Bernoulli~Viridian 32-byte restriction.
        if let Some(modexp) = osaka.get(&addresses::MODEXP) {
            precompiles.extend([modexp.clone()]);
        }

        // Add BLS12-381 precompiles (EIP-2537): 0x0b through 0x11
        for addr in [
            addresses::BLS12_G1ADD,
            addresses::BLS12_G1MULTIEXP,
            addresses::BLS12_G2ADD,
            addresses::BLS12_G2MULTIEXP,
            addresses::BLS12_PAIRING,
            addresses::BLS12_MAP_FP_TO_G1,
            addresses::BLS12_MAP_FP2_TO_G2,
        ] {
            if let Some(precompile) = osaka.get(&addr) {
                precompiles.extend([precompile.clone()]);
            }
        }

        // Add P256verify (RIP-7212) at 0x100
        if let Some(p256) = osaka.get(&addresses::P256_VERIFY) {
            precompiles.extend([p256.clone()]);
        }

        // NOTE: KZG Point Evaluation (0x0a) is intentionally NOT included.
        // go-ethereum's PrecompiledContractsEmerald skips 0x0a entirely.

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

        // Should have all 9 Berlin addresses (ecrecover, sha256, ripemd160, identity,
        // modexp, bn256 add/mul/pairing, blake2f)
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));
        assert!(precompiles.contains(&addresses::IDENTITY));
        assert!(precompiles.contains(&addresses::MODEXP));
        assert!(precompiles.contains(&addresses::BN256_ADD));
        assert!(precompiles.contains(&addresses::BN256_MUL));
        assert!(precompiles.contains(&addresses::BN256_PAIRING));

        // ripemd160 (0x03) and blake2f (0x09) ARE present as disabled stubs.
        assert!(precompiles.contains(&addresses::RIPEMD160));
        assert!(precompiles.contains(&addresses::BLAKE2F));

        // Exact count: 9 precompiles (matching go-eth PrecompiledContractsBernoulli)
        assert_eq!(precompiles.len(), 9);
    }

    #[test]
    fn test_bernoulli_modexp_rejects_large_input() {
        let precompiles = bernoulli();
        let modexp = precompiles.get(&addresses::MODEXP).unwrap();

        // base_len=33 (exceeds 32-byte limit) — should be rejected
        let mut input = vec![0u8; 96];
        input[31] = 33; // base_len = 33
        input[63] = 32; // exp_len = 32
        input[95] = 32; // mod_len = 32
        let result = modexp.execute(&input, 100_000);
        assert!(
            result.is_err(),
            "modexp with base_len=33 should be rejected"
        );

        // base_len=32, exp_len=32, mod_len=32 — should succeed
        input[31] = 32;
        let result = modexp.execute(&input, 100_000);
        assert!(result.is_ok(), "modexp with all lens=32 should succeed");
    }

    #[test]
    fn test_curie_uses_bernoulli_precompiles() {
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);

        assert_eq!(bernoulli_p.precompiles().len(), curie_p.precompiles().len());
        assert!(curie_p.contains(&addresses::SHA256));
        assert!(curie_p.contains(&addresses::RIPEMD160));
        assert!(curie_p.contains(&addresses::BLAKE2F));
    }

    #[test]
    fn test_morph203_precompiles() {
        let precompiles = morph203();

        // blake2f and ripemd160 re-enabled (working, not disabled stubs)
        assert!(precompiles.contains(&addresses::BLAKE2F));
        assert!(precompiles.contains(&addresses::RIPEMD160));
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));

        // No Osaka-era precompiles yet
        assert!(!precompiles.contains(&addresses::P256_VERIFY));
        assert!(!precompiles.contains(&addresses::POINT_EVALUATION));

        // Same count as Bernoulli (9 addresses, different implementations)
        assert_eq!(precompiles.len(), 9);
    }

    #[test]
    fn test_morph203_pairing_rejects_large_input() {
        let precompiles = morph203();
        let pairing = precompiles.get(&addresses::BN256_PAIRING).unwrap();

        // 5 pairs (960 bytes) — exceeds 4-pair limit, should be rejected
        let input = vec![0u8; 5 * 192];
        let result = pairing.execute(&input, 1_000_000);
        assert!(result.is_err(), "pairing with 5 pairs should be rejected");

        // 4 pairs (768 bytes) — within limit, should not be rejected for size
        // (may still fail due to invalid curve points, but not for size)
        let input = vec![0u8; 4 * 192];
        let result = pairing.execute(&input, 1_000_000);
        // Zero-input pairing is valid and returns true
        assert!(
            result.is_ok(),
            "pairing with 4 pairs should not be rejected for size"
        );
    }

    #[test]
    fn test_emerald_precompiles() {
        let precompiles = emerald();

        // All standard precompiles (0x01-0x09)
        assert!(precompiles.contains(&addresses::ECRECOVER));
        assert!(precompiles.contains(&addresses::SHA256));
        assert!(precompiles.contains(&addresses::RIPEMD160));
        assert!(precompiles.contains(&addresses::IDENTITY));
        assert!(precompiles.contains(&addresses::MODEXP));
        assert!(precompiles.contains(&addresses::BN256_ADD));
        assert!(precompiles.contains(&addresses::BN256_MUL));
        assert!(precompiles.contains(&addresses::BN256_PAIRING));
        assert!(precompiles.contains(&addresses::BLAKE2F));

        // BLS12-381 precompiles (0x0b-0x11)
        assert!(precompiles.contains(&addresses::BLS12_G1ADD));
        assert!(precompiles.contains(&addresses::BLS12_G1MULTIEXP));
        assert!(precompiles.contains(&addresses::BLS12_G2ADD));
        assert!(precompiles.contains(&addresses::BLS12_G2MULTIEXP));
        assert!(precompiles.contains(&addresses::BLS12_PAIRING));
        assert!(precompiles.contains(&addresses::BLS12_MAP_FP_TO_G1));
        assert!(precompiles.contains(&addresses::BLS12_MAP_FP2_TO_G2));

        // P256verify (0x100)
        assert!(precompiles.contains(&addresses::P256_VERIFY));

        // KZG Point Evaluation (0x0a) must NOT be included
        assert!(
            !precompiles.contains(&addresses::POINT_EVALUATION),
            "Emerald must NOT include KZG Point Evaluation (0x0a)"
        );

        // Exact count: 9 (standard) + 7 (BLS12-381) + 1 (P256verify) = 17
        // Matching go-eth PrecompiledContractsEmerald which has 17 entries
        assert_eq!(precompiles.len(), 17);
    }

    #[test]
    fn test_emerald_modexp_accepts_large_input() {
        let precompiles = emerald();
        let modexp = precompiles.get(&addresses::MODEXP).unwrap();

        // base_len=64 — should succeed in Emerald (32-byte limit lifted)
        let mut input = vec![0u8; 96 + 64 + 32 + 64]; // base_len + exp_len + mod_len + data
        input[31] = 64; // base_len = 64
        input[63] = 32; // exp_len = 32
        input[95] = 64; // mod_len = 64
        let result = modexp.execute(&input, 1_000_000);
        assert!(result.is_ok(), "Emerald modexp should accept base_len=64");
    }

    #[test]
    fn test_precompile_counts() {
        assert_eq!(bernoulli().len(), 9);
        assert_eq!(morph203().len(), 9);
        assert_eq!(emerald().len(), 17);
    }

    #[test]
    fn test_hardfork_specific_precompiles() {
        let bernoulli_p = MorphPrecompiles::new_with_spec(MorphHardfork::Bernoulli);
        let curie_p = MorphPrecompiles::new_with_spec(MorphHardfork::Curie);
        let morph203_p = MorphPrecompiles::new_with_spec(MorphHardfork::Morph203);
        let viridian_p = MorphPrecompiles::new_with_spec(MorphHardfork::Viridian);
        let emerald_p = MorphPrecompiles::new_with_spec(MorphHardfork::Emerald);

        // Bernoulli/Curie: disabled stubs present, same set
        assert!(bernoulli_p.contains(&addresses::RIPEMD160));
        assert!(bernoulli_p.contains(&addresses::BLAKE2F));
        assert_eq!(bernoulli_p.precompiles().len(), curie_p.precompiles().len());

        // Morph203/Viridian: re-enabled, no P256verify, same set
        assert!(morph203_p.contains(&addresses::RIPEMD160));
        assert!(morph203_p.contains(&addresses::BLAKE2F));
        assert!(!morph203_p.contains(&addresses::P256_VERIFY));
        assert_eq!(
            morph203_p.precompiles().len(),
            viridian_p.precompiles().len()
        );

        // Emerald: full set with BLS12-381 + P256verify, no KZG
        assert!(emerald_p.contains(&addresses::P256_VERIFY));
        assert!(emerald_p.contains(&addresses::BLS12_G1ADD));
        assert!(!emerald_p.contains(&addresses::POINT_EVALUATION));
    }

    #[test]
    fn test_modexp_len_check() {
        // Value = 0 (all zeros) — should NOT exceed 32
        assert!(!modexp_len_exceeds_32(&[0u8; 32], 0));

        // Value = 32 — should NOT exceed 32
        let mut data = [0u8; 32];
        data[31] = 32;
        assert!(!modexp_len_exceeds_32(&data, 0));

        // Value = 33 — should exceed 32
        data[31] = 33;
        assert!(modexp_len_exceeds_32(&data, 0));

        // Value has non-zero high byte — definitely exceeds 32
        data[0] = 1;
        data[31] = 0;
        assert!(modexp_len_exceeds_32(&data, 0));

        // Empty input (right-padded to all zeros) — value = 0, should NOT exceed
        assert!(!modexp_len_exceeds_32(&[], 0));
    }
}
