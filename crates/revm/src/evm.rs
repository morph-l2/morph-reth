//! The Morph EVM type family and its construction.

use crate::{
    MorphBlockEnv, MorphTxEnv, handler::morph_tx_registry, precompiles::morph_precompiles,
    token_fee::TokenFeeInfo,
};
use alloy_primitives::{Address, Log, U256, keccak256};
use evm2::{
    EvmConfig, EvmConfigSelector, EvmFeatures, EvmTypesHost, ExecutionConfig, Inspector,
    OpcodeConfig, SpecId, Version,
    constants::BLOCK_HASH_HISTORY,
    evm::DynDatabase,
    interpreter::{Interpreter, Message, MessageResult, Word, instructions::invalid, op},
};
use evm2_macros::instruction;
use morph_chainspec::hardfork::MorphHardfork;

/// The Morph runtime EVM type family.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct MorphEvmTypes;

impl EvmTypesHost for MorphEvmTypes {
    type ConfigSelector = MorphConfigSelector;
    type SpecId = MorphHardfork;
    type Tx = MorphTxEnv;
    type EvmExt = ();
    type MessageExt = MorphMessageExt;
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = MorphTxResultExt;
    type BlockEnvExt = ();
    type Host<'a> = evm2::Evm<'a, Self>;
}

/// An evm2 EVM configured with the Morph type family.
pub type MorphEvm<'a> = evm2::Evm<'a, MorphEvmTypes>;

/// Environment a Morph EVM runs under: hardfork, runtime version and block fields.
///
/// Built by `ConfigureEvm::evm_env` for execution. The pool uses the same type so that the
/// balance it resolves for a call-mode fee token matches what execution would see; go-ethereum
/// likewise builds a `vm.BlockContext` from the header before querying one
/// (`pool.getBalanceFunc`, core/tx_pool.go:330).
pub type MorphEvmEnv = reth_evm_ethereum::EthEvmEnv<MorphEvmTypes>;

/// Morph data carried by frame messages.
///
/// Default for every frame the interpreter creates, so it only ever identifies the top-level
/// frames the handler starts on behalf of the protocol.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MorphMessageExt {
    /// Whether this is a fee-token `balanceOf`/`transfer` frame.
    pub fee_frame: bool,
}

/// Morph data carried by a transaction result, for receipts and RPC.
#[derive(Clone, Copy, Debug, Default)]
pub struct MorphTxResultExt {
    /// The L1 data fee charged, zero for L1 messages and when fee charging is disabled.
    pub l1_data_fee: U256,
    /// The registry data used to charge a token fee, absent when the fee was paid in ETH.
    pub token_fee_info: Option<TokenFeeInfo>,
}

/// Morph EVM configuration for one inherited base [`SpecId`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct MorphConfig<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32>;

impl<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32> EvmConfig<MorphEvmTypes>
    for MorphConfig<BASE_SPEC_ID, CUSTOM_SPEC_ID>
{
    const BASE_SPEC_ID: SpecId = SpecId::try_from_u32(BASE_SPEC_ID).unwrap();
    const OPCODE_CONFIG: &'static OpcodeConfig<MorphEvmTypes> =
        &morph_opcode_config::<BASE_SPEC_ID, CUSTOM_SPEC_ID>();
}

/// Applies Morph's opcode differences to the table of the inherited base spec.
const fn morph_opcode_config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32>()
-> OpcodeConfig<MorphEvmTypes> {
    let mut config =
        OpcodeConfig::<MorphEvmTypes>::base::<MorphConfig<BASE_SPEC_ID, CUSTOM_SPEC_ID>>();
    // Morph geth does not read header hashes for BLOCKHASH.
    let blockhash_gas = config.static_gas(op::BLOCKHASH);
    config.set_instruction::<morph_blockhash>(op::BLOCKHASH, blockhash_gas);
    // SELFDESTRUCT, BLOBHASH and BLOBBASEFEE are disabled on Morph: they halt like an
    // undefined opcode, which consumes all gas of the frame.
    config.set_instruction::<invalid<MorphEvmTypes>>(op::SELFDESTRUCT, 0);
    config.set_instruction::<invalid<MorphEvmTypes>>(op::BLOBHASH, 0);
    config.set_instruction::<invalid<MorphEvmTypes>>(op::BLOBBASEFEE, 0);
    config
}

/// Selects the opcode table for each Morph hardfork.
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct MorphConfigSelector;

/// Morph's opcode overrides do not vary between its hardforks, so the only table parameter is
/// the inherited base spec.
const MORPH_CUSTOM_SPEC_ID: u32 = 0;

impl EvmConfigSelector<MorphEvmTypes> for MorphConfigSelector {
    type Config<const BASE_SPEC_ID: u32, const CUSTOM_SPEC_ID: u32> =
        MorphConfig<BASE_SPEC_ID, CUSTOM_SPEC_ID>;

    fn execution_config(spec_id: MorphHardfork) -> ExecutionConfig<MorphEvmTypes> {
        let base = SpecId::from(spec_id);
        if base.enables(SpecId::OSAKA) {
            ExecutionConfig::for_config::<MorphConfig<{ SpecId::OSAKA as u32 }, MORPH_CUSTOM_SPEC_ID>>(
            )
        } else if base.enables(SpecId::PRAGUE) {
            ExecutionConfig::for_config::<
                MorphConfig<{ SpecId::PRAGUE as u32 }, MORPH_CUSTOM_SPEC_ID>,
            >()
        } else {
            ExecutionConfig::for_config::<
                MorphConfig<{ SpecId::CANCUN as u32 }, MORPH_CUSTOM_SPEC_ID>,
            >()
        }
    }
}

#[inline]
fn morph_blockhash_value(chain_id: u64, number: u64) -> Word {
    let mut input = [0_u8; 16];
    input[..8].copy_from_slice(&chain_id.to_be_bytes());
    input[8..].copy_from_slice(&number.to_be_bytes());
    Word::from_be_bytes(keccak256(input).0)
}

#[inline]
fn morph_blockhash_result(chain_id: u64, current_number: u64, requested_number: u64) -> Word {
    // Match Morph geth exactly:
    // valid range is [current-256, current), otherwise push zero.
    let lower = current_number.saturating_sub(BLOCK_HASH_HISTORY);
    if requested_number >= lower && requested_number < current_number {
        morph_blockhash_value(chain_id, requested_number)
    } else {
        Word::ZERO
    }
}

/// Morph custom BLOCKHASH opcode.
///
/// Morph geth does not read historical header hashes for BLOCKHASH. Instead it returns:
/// `keccak256(chain_id(8-byte big-endian) || block_number(8-byte big-endian))`
/// for numbers within the 256-block lookup window.
#[instruction(EvmTypes = MorphEvmTypes)]
pub fn morph_blockhash(cx: _, [number]: [Word]) -> out {
    let requested_number = number.saturating_to::<u64>();
    let current_number = cx.state.host().block().number.saturating_to::<u64>();
    let chain_id = cx.state.tx().chain_id.saturating_to::<u64>();
    *out = morph_blockhash_result(chain_id, current_number, requested_number);
}

/// Returns the runtime version of a Morph block with the given chain id and gas limit.
///
/// Two standard rules do not apply to Morph:
/// - EIP-7623 is off, matching Morph geth's calldata pricing.
/// - The EIP-7825 transaction gas cap is the block gas limit. Historical mainnet transactions
///   (e.g. block 20459477, `gas_limit=21,165,068`) exceed the EIP-7825 cap of 2^24 even though
///   that rule arrives with the OSAKA spec Morph maps later hardforks to.
pub fn morph_version(spec: MorphHardfork, chain_id: u64, block_gas_limit: u64) -> Version {
    let mut version = Version::new(spec.into());
    version.chain_id = chain_id;
    version.features.remove(EvmFeatures::EIP7623);
    version.tx_gas_limit_cap = block_gas_limit;
    version
}

/// Returns the environment of a Morph block.
pub fn morph_evm_env(
    spec: MorphHardfork,
    chain_id: u64,
    block_gas_limit: u64,
    block: MorphBlockEnv,
) -> MorphEvmEnv {
    MorphEvmEnv::new_with_version(spec, block, morph_version(spec, chain_id, block_gas_limit))
}

/// Builds a Morph EVM over `database`.
pub fn morph_evm<'a>(database: impl DynDatabase + 'a, env: MorphEvmEnv) -> MorphEvm<'a> {
    let MorphEvmEnv {
        spec,
        version,
        block,
    } = env;
    MorphEvm::new_with_execution_config(
        ExecutionConfig::for_spec_and_version(spec, version),
        spec,
        block,
        morph_tx_registry(spec),
        database,
        morph_precompiles(spec),
    )
}

/// Hides the protocol's fee-token frames from an inspector.
///
/// go-ethereum wraps those calls in `OnSystemCallStart`/`OnSystemCallEnd`, so tracers do not see
/// them as part of the transaction. evm2 inspects every message it runs, and exposes no way for
/// a handler to pause the inspector, so every inspector installed on a Morph EVM goes through
/// this filter, which ignores everything between the start and end of a
/// [`MorphMessageExt::fee_frame`] frame.
#[derive(Debug)]
pub struct FeeFrameFilter<I> {
    inner: I,
    in_fee_frame: bool,
}

impl<I> FeeFrameFilter<I> {
    /// Wraps `inner`.
    pub const fn new(inner: I) -> Self {
        Self {
            inner,
            in_fee_frame: false,
        }
    }

    /// Returns the wrapped inspector.
    pub fn into_inner(self) -> I {
        self.inner
    }

    /// Returns the wrapped inspector.
    pub const fn inner(&self) -> &I {
        &self.inner
    }
}

impl<I: Inspector<MorphEvmTypes>> Inspector<MorphEvmTypes> for FeeFrameFilter<I> {
    fn initialize_interp(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        if !self.in_fee_frame {
            self.inner.initialize_interp(interp);
        }
    }

    fn step(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        if !self.in_fee_frame {
            self.inner.step(interp);
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
        if !self.in_fee_frame {
            self.inner.step_end(interp);
        }
    }

    fn log(&mut self, log: &Log, host: &mut MorphEvm<'_>) {
        if !self.in_fee_frame {
            self.inner.log(log, host);
        }
    }

    fn call(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &mut Message<MorphEvmTypes>,
    ) -> Option<MessageResult<MorphEvmTypes>> {
        if message.ext.fee_frame {
            self.in_fee_frame = true;
        }
        if self.in_fee_frame {
            return None;
        }
        self.inner.call(interp, message)
    }

    fn call_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &Message<MorphEvmTypes>,
        result: &mut MessageResult<MorphEvmTypes>,
    ) {
        if message.ext.fee_frame {
            self.in_fee_frame = false;
            return;
        }
        if !self.in_fee_frame {
            self.inner.call_end(interp, message, result);
        }
    }

    fn create(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &mut Message<MorphEvmTypes>,
    ) -> Option<MessageResult<MorphEvmTypes>> {
        if self.in_fee_frame {
            return None;
        }
        self.inner.create(interp, message)
    }

    fn create_end(
        &mut self,
        interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
        message: &Message<MorphEvmTypes>,
        result: &mut MessageResult<MorphEvmTypes>,
    ) {
        if !self.in_fee_frame {
            self.inner.create_end(interp, message, result);
        }
    }

    fn selfdestruct(
        &mut self,
        contract: &Address,
        target: &Address,
        value: &U256,
        host: &mut MorphEvm<'_>,
    ) {
        if !self.in_fee_frame {
            self.inner.selfdestruct(contract, target, value, host);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::b256;

    #[test]
    fn morph_blockhash_matches_geth_vector() {
        // Golden vector captured from geth debug_traceTransaction for block 2662438:
        // BLOCKHASH(2662437) => 0x6d24... using chain_id=2818.
        let expected = Word::from_be_bytes(
            b256!("6d2426d9b8d6f63ec1a38d3a4e7b88f318ebe9f0d837c4781852168c5bd2678e").0,
        );
        assert_eq!(morph_blockhash_value(2818, 2_662_437), expected);
    }

    #[test]
    fn morph_blockhash_block_zero() {
        // Block 0 requested from block 1 — block 0 is within [1-256, 1) = [0, 1), so valid
        let result = morph_blockhash_result(2818, 1, 0);
        assert_ne!(result, Word::ZERO, "block 0 from block 1 should be valid");

        // Block 0 requested from block 0 — current block returns zero
        let result = morph_blockhash_result(2818, 0, 0);
        assert_eq!(
            result,
            Word::ZERO,
            "block 0 from block 0 should be zero (current block)"
        );
    }

    #[test]
    fn morph_blockhash_chain_id_zero() {
        // chain_id=0 should still produce a deterministic hash
        let result = morph_blockhash_value(0, 100);
        assert_ne!(result, Word::ZERO, "chain_id=0 should still produce a hash");

        // Different chain_ids produce different hashes
        let result_0 = morph_blockhash_value(0, 100);
        let result_1 = morph_blockhash_value(1, 100);
        assert_ne!(
            result_0, result_1,
            "different chain_ids should produce different hashes"
        );
    }

    #[test]
    fn morph_blockhash_small_current_block() {
        let chain_id = 2818;
        // current_number = 5, so valid range is [0, 5)
        // Block 0 through 4 should be valid
        for n in 0..5 {
            assert_ne!(
                morph_blockhash_result(chain_id, 5, n),
                Word::ZERO,
                "block {n} from block 5 should be valid"
            );
        }
        // Block 5 (current) should be zero
        assert_eq!(morph_blockhash_result(chain_id, 5, 5), Word::ZERO);
    }

    #[test]
    fn morph_blockhash_boundary_256() {
        let chain_id = 2818;
        let current = 300;

        // current - 256 = 44 (inclusive lower bound)
        assert_ne!(
            morph_blockhash_result(chain_id, current, 44),
            Word::ZERO,
            "block current-256 should be valid"
        );

        // current - 257 = 43 (out of range)
        assert_eq!(
            morph_blockhash_result(chain_id, current, 43),
            Word::ZERO,
            "block current-257 should be zero"
        );

        // current - 1 = 299 (valid, most recent)
        assert_ne!(
            morph_blockhash_result(chain_id, current, 299),
            Word::ZERO,
            "block current-1 should be valid"
        );
    }

    #[test]
    fn morph_blockhash_deterministic() {
        // Same inputs always produce the same output
        let a = morph_blockhash_value(2818, 1000);
        let b = morph_blockhash_value(2818, 1000);
        assert_eq!(a, b, "blockhash should be deterministic");
    }

    #[test]
    fn morph_blockhash_window_matches_geth_rules() {
        let chain_id = 2818_u64;
        let current = 2_662_438_u64;

        // current block and future blocks must return zero
        assert_eq!(
            morph_blockhash_result(chain_id, current, current),
            Word::ZERO
        );
        assert_eq!(
            morph_blockhash_result(chain_id, current, current + 1),
            Word::ZERO
        );

        // more than 256 blocks in the past returns zero
        assert_eq!(
            morph_blockhash_result(chain_id, current, current - 257),
            Word::ZERO
        );

        // lower bound is inclusive (current - 256 is valid)
        assert_ne!(
            morph_blockhash_result(chain_id, current, current - 256),
            Word::ZERO
        );
    }

    #[test]
    fn morph_version_turns_off_eip7623_and_the_eip7825_cap() {
        let version = morph_version(MorphHardfork::Celadon, 2818, 30_000_000);
        assert_eq!(version.chain_id, 2818);
        assert!(!version.feature(EvmFeatures::EIP7623));
        assert_eq!(version.tx_gas_limit_cap, 30_000_000);
    }

    #[test]
    fn execution_config_maps_each_hardfork_to_its_base_spec() {
        for (fork, spec) in [
            (MorphHardfork::Bernoulli, SpecId::CANCUN),
            (MorphHardfork::Curie, SpecId::CANCUN),
            (MorphHardfork::Morph203, SpecId::CANCUN),
            (MorphHardfork::Viridian, SpecId::PRAGUE),
            (MorphHardfork::Emerald, SpecId::OSAKA),
            (MorphHardfork::Jade, SpecId::OSAKA),
            (MorphHardfork::Celadon, SpecId::OSAKA),
        ] {
            assert_eq!(
                MorphConfigSelector::execution_config(fork).base_spec_id(),
                spec
            );
        }
    }
}
