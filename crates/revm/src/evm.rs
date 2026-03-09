use crate::{MorphBlockEnv, MorphTxEnv, precompiles::MorphPrecompiles};
use alloy_evm::Database;
use alloy_primitives::{Log, U256, keccak256};
use morph_chainspec::hardfork::MorphHardfork;
use revm::{
    Context, Inspector,
    context::{CfgEnv, ContextError, Evm, FrameStack},
    context_interface::{cfg::gas::BLOCKHASH, host::LoadError},
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, FrameTr, ItemOrResult, instructions::EthInstructions,
    },
    inspector::InspectorEvmTr,
    interpreter::{
        Host, Instruction, InstructionContext, interpreter::EthInterpreter,
        interpreter_types::StackTr,
    },
    primitives::BLOCK_HASH_HISTORY,
};

/// The Morph EVM context type.
pub type MorphContext<DB> = Context<MorphBlockEnv, MorphTxEnv, CfgEnv<MorphHardfork>, DB>;

#[inline]
fn as_u64_saturated(value: U256) -> u64 {
    let limbs = value.as_limbs();
    if limbs[1] == 0 && limbs[2] == 0 && limbs[3] == 0 {
        limbs[0]
    } else {
        u64::MAX
    }
}

#[inline]
fn morph_blockhash_value(chain_id: u64, number: u64) -> U256 {
    let mut input = [0_u8; 16];
    input[..8].copy_from_slice(&chain_id.to_be_bytes());
    input[8..].copy_from_slice(&number.to_be_bytes());
    U256::from_be_bytes(keccak256(input).0)
}

#[inline]
fn morph_blockhash_result(chain_id: u64, current_number: u64, requested_number: u64) -> U256 {
    // Match Morph geth exactly:
    // valid range is [current-256, current), otherwise push zero.
    let lower = current_number.saturating_sub(BLOCK_HASH_HISTORY);
    if requested_number >= lower && requested_number < current_number {
        morph_blockhash_value(chain_id, requested_number)
    } else {
        U256::ZERO
    }
}

/// Morph custom SLOAD opcode.
///
/// This wraps the standard SLOAD logic and adds a fix for the revm-state 9.0.0
/// `mark_warm_with_transaction_id` behavior change.
///
/// ## Background
///
/// In revm-state 9.0.0, `EvmStorageSlot::mark_warm_with_transaction_id` resets
/// `original_value = present_value` when a cold slot becomes warm. This is
/// semantically correct for standard per-transaction state, but breaks Morph's
/// token fee mechanism where storage slots are modified *before* the main
/// transaction and then marked cold.
///
/// When the main transaction SLOADs such a slot, the cold→warm transition
/// resets `original_value`, making the slot appear "clean" (original == present).
/// A subsequent SSTORE then uses EIP-2200 "clean" pricing (2900 gas) instead of
/// "dirty" pricing (100 gas), creating a **2800 gas** discrepancy vs go-ethereum.
///
/// ## Fix
///
/// After the standard SLOAD completes (including `mark_warm_with_transaction_id`),
/// this instruction checks whether the loaded slot is one of the fee-deducted
/// slots whose original DB value was saved in `MorphTxEnv::fee_slot_original_values`.
/// If so, it restores `original_value` on the `EvmStorageSlot` in journal state
/// to the true DB value, so that SSTORE gas calculation sees the correct
/// dirty/clean status.
fn sload_morph<DB: Database>(context: InstructionContext<'_, MorphContext<DB>, EthInterpreter>) {
    let Some(([], index)) = StackTr::popn_top::<0>(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };

    let target = context.interpreter.input.target_address;
    let key = *index;

    // Berlin+ path (Morph is always post-Berlin).
    // Charge WARM_STORAGE_READ_COST (100) as static gas via the Instruction wrapper,
    // then charge the additional cold cost (2000) here if the slot is cold.
    let additional_cold_cost = context.host.gas_params().cold_storage_additional_cost();
    let skip_cold = context.interpreter.gas.remaining() < additional_cold_cost;

    match context.host.sload_skip_cold_load(target, key, skip_cold) {
        Ok(storage) => {
            if storage.is_cold && !context.interpreter.gas.record_cost(additional_cold_cost) {
                context.interpreter.halt_oog();
                return;
            }
            *index = storage.data;
        }
        Err(LoadError::ColdLoadSkipped) => {
            context.interpreter.halt_oog();
            return;
        }
        Err(LoadError::DBError) => {
            context.interpreter.halt_fatal();
            return;
        }
    }

    // Morph fix: restore original_value for slots modified by token fee deduction.
    // After mark_warm_with_transaction_id reset original_value = present_value,
    // we set it back to the true DB value so SSTORE sees the slot as dirty.
    //
    // We use `.iter().find()` instead of `.remove()` so the entry is kept for the
    // lifetime of the transaction. This is revert-safe: if a sub-call REVERTs,
    // the journal rolls the slot back to cold, and a later SLOAD to the same slot
    // would corrupt original_value again — the kept entry ensures every cold→warm
    // transition is corrected, not just the first one.
    if let Some(&(_, _, original_db_value)) = context
        .host
        .tx
        .fee_slot_original_values
        .iter()
        .find(|(addr, slot_key, _)| *addr == target && *slot_key == key)
        && let Some(acc) = context.host.journaled_state.state.get_mut(&target)
        && let Some(slot) = acc.storage.get_mut(&key)
    {
        slot.original_value = original_db_value;
    }
}

/// Morph custom BLOCKHASH opcode.
///
/// Morph geth does not read historical header hashes for BLOCKHASH. Instead it returns:
/// `keccak256(chain_id(8-byte big-endian) || block_number(8-byte big-endian))`
/// for numbers within the 256-block lookup window.
fn blockhash_morph<DB: Database>(
    context: InstructionContext<'_, MorphContext<DB>, EthInterpreter>,
) {
    let Some(([], number)) = StackTr::popn_top::<0>(&mut context.interpreter.stack) else {
        context.interpreter.halt_underflow();
        return;
    };

    let requested_number_u64 = as_u64_saturated(*number);
    let current_number_u64 = as_u64_saturated(context.host.block_number());
    let chain_id_u64 = as_u64_saturated(context.host.chain_id());

    *number = morph_blockhash_result(chain_id_u64, current_number_u64, requested_number_u64);
}

/// MorphEvm extends the Evm with Morph specific types and logic.
#[derive(Debug, derive_more::Deref, derive_more::DerefMut)]
#[expect(clippy::type_complexity)]
pub struct MorphEvm<DB: Database, I> {
    /// Inner EVM type.
    #[deref]
    #[deref_mut]
    pub inner: Evm<
        MorphContext<DB>,
        I,
        EthInstructions<EthInterpreter, MorphContext<DB>>,
        MorphPrecompiles,
        EthFrame<EthInterpreter>,
    >,
    /// Preserved logs from the last transaction
    pub logs: Vec<Log>,
}

impl<DB: Database, I> MorphEvm<DB, I> {
    /// Create a new Morph EVM.
    ///
    /// The precompiles are automatically selected based on the hardfork spec
    /// configured in the context's cfg.
    pub fn new(ctx: MorphContext<DB>, inspector: I) -> Self {
        // Get the current hardfork spec from context and create matching precompiles
        let spec = ctx.cfg.spec;
        let precompiles = MorphPrecompiles::new_with_spec(spec);
        let mut instructions = EthInstructions::new_mainnet();

        // Morph custom SLOAD: restores original_value after revm-state 9.0.0's
        // mark_warm_with_transaction_id resets it, fixing EIP-2200 gas for
        // token fee deducted slots. Static gas = WARM_STORAGE_READ_COST (100).
        instructions.insert_instruction(
            0x54, // SLOAD
            Instruction::new(
                sload_morph::<DB>,
                revm::context_interface::cfg::gas::WARM_STORAGE_READ_COST,
            ),
        );
        // Morph custom BLOCKHASH implementation (matches Morph geth).
        instructions.insert_instruction(0x40, Instruction::new(blockhash_morph::<DB>, BLOCKHASH));
        // SELFDESTRUCT is disabled in Morph
        instructions.insert_instruction(0xff, Instruction::unknown());
        // BLOBHASH is disabled in Morph
        instructions.insert_instruction(0x49, Instruction::unknown());
        // BLOBBASEFEE is disabled in Morph
        instructions.insert_instruction(0x4a, Instruction::unknown());
        Self::new_inner(Evm {
            ctx,
            inspector,
            instruction: instructions,
            precompiles,
            frame_stack: FrameStack::new(),
        })
    }

    /// Inner helper function to create a new Morph EVM with empty logs.
    #[inline]
    #[expect(clippy::type_complexity)]
    fn new_inner(
        inner: Evm<
            MorphContext<DB>,
            I,
            EthInstructions<EthInterpreter, MorphContext<DB>>,
            MorphPrecompiles,
            EthFrame<EthInterpreter>,
        >,
    ) -> Self {
        Self {
            inner,
            logs: Vec::new(),
        }
    }
}

impl<DB: Database, I> MorphEvm<DB, I> {
    /// Consumed self and returns a new Evm type with given Inspector.
    pub fn with_inspector<OINSP>(self, inspector: OINSP) -> MorphEvm<DB, OINSP> {
        MorphEvm::new_inner(self.inner.with_inspector(inspector))
    }

    /// Consumes self and returns a new Evm type with given Precompiles.
    pub fn with_precompiles(self, precompiles: MorphPrecompiles) -> Self {
        Self::new_inner(self.inner.with_precompiles(precompiles))
    }

    /// Consumes self and returns the inner Inspector.
    pub fn into_inspector(self) -> I {
        self.inner.into_inspector()
    }

    /// Take logs from the EVM.
    #[inline]
    pub fn take_logs(&mut self) -> Vec<Log> {
        std::mem::take(&mut self.logs)
    }
}

impl<DB, I> EvmTr for MorphEvm<DB, I>
where
    DB: Database,
{
    type Context = MorphContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, MorphContext<DB>>;
    type Precompiles = MorphPrecompiles;
    type Frame = EthFrame<EthInterpreter>;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_stack(&mut self) -> &mut FrameStack<Self::Frame> {
        &mut self.inner.frame_stack
    }

    fn frame_init(
        &mut self,
        frame_input: <Self::Frame as FrameTr>::FrameInit,
    ) -> Result<
        ItemOrResult<&mut Self::Frame, <Self::Frame as FrameTr>::FrameResult>,
        ContextError<DB::Error>,
    > {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(&mut self) -> Result<FrameInitOrResult<Self::Frame>, ContextError<DB::Error>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: <Self::Frame as FrameTr>::FrameResult,
    ) -> Result<Option<<Self::Frame as FrameTr>::FrameResult>, ContextError<DB::Error>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, I> InspectorEvmTr for MorphEvm<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    type Inspector = I;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
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
        let expected = U256::from_be_bytes(
            b256!("6d2426d9b8d6f63ec1a38d3a4e7b88f318ebe9f0d837c4781852168c5bd2678e").0,
        );
        assert_eq!(morph_blockhash_value(2818, 2_662_437), expected);
    }

    #[test]
    fn morph_blockhash_window_matches_geth_rules() {
        let chain_id = 2818_u64;
        let current = 2_662_438_u64;

        // current block and future blocks must return zero
        assert_eq!(
            morph_blockhash_result(chain_id, current, current),
            U256::ZERO
        );
        assert_eq!(
            morph_blockhash_result(chain_id, current, current + 1),
            U256::ZERO
        );

        // more than 256 blocks in the past returns zero
        assert_eq!(
            morph_blockhash_result(chain_id, current, current - 257),
            U256::ZERO
        );

        // lower bound is inclusive (current - 256 is valid)
        assert_ne!(
            morph_blockhash_result(chain_id, current, current - 256),
            U256::ZERO
        );
    }
}
