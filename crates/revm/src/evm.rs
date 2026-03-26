use crate::{
    MorphBlockEnv, MorphTxEnv, l1block::L1BlockInfo, precompiles::MorphPrecompiles,
    token_fee::TokenFeeInfo,
};
use alloy_evm::Database;
use alloy_primitives::{U256, keccak256};
use morph_chainspec::hardfork::MorphHardfork;
use revm::{
    Context, Inspector,
    context::{CfgEnv, ContextError, Evm, FrameStack, Journal},
    handler::{
        EthFrame, EvmTr, FrameInitOrResult, FrameTr, ItemOrResult, instructions::EthInstructions,
    },
    inspector::InspectorEvmTr,
    interpreter::{
        Host, Instruction, InstructionContext, gas::BLOCKHASH, interpreter::EthInterpreter,
        interpreter_types::StackTr,
    },
    primitives::BLOCK_HASH_HISTORY,
};

/// The Morph EVM context type.
///
/// Uses [`L1BlockInfo`] as the `CHAIN` type parameter.  Note that L1 fee
/// parameters are fetched **per-transaction** (not per-block) because the
/// L1 Gas Price Oracle can be updated by a regular transaction mid-block.
pub type MorphContext<DB> =
    Context<MorphBlockEnv, MorphTxEnv, CfgEnv<MorphHardfork>, DB, Journal<DB>, L1BlockInfo>;

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
    /// Cached token fee info from the validation/deduction phase.
    /// Ensures consistent price_ratio/scale between deduct and reimburse,
    /// matching go-ethereum's `st.feeRate`/`st.tokenScale` caching pattern.
    pub(crate) cached_token_fee_info: Option<TokenFeeInfo>,
    /// Cached L1 data fee calculated during handler validation.
    /// Avoids re-encoding the full transaction RLP in the block executor's
    /// receipt-building path (the handler already has the encoded bytes via
    /// `MorphTxEnv.rlp_bytes`).
    pub(crate) cached_l1_data_fee: U256,
    /// Transfer event logs from token fee deduction (pre-execution phase).
    ///
    /// In go-ethereum, `buyAltTokenGas()` emits Transfer events into `StateDB.logs`
    /// which is independent of the state snapshot/revert mechanism — logs survive
    /// regardless of main tx result. revm's `ExecutionResult::Revert` has no `logs`
    /// field, so we cache fee-related logs separately from the journal and merge
    /// them into the receipt in the block executor.
    pub(crate) pre_fee_logs: Vec<alloy_primitives::Log>,
    /// Transfer event logs from token fee reimbursement (post-execution phase).
    pub(crate) post_fee_logs: Vec<alloy_primitives::Log>,
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
            cached_token_fee_info: None,
            cached_l1_data_fee: U256::ZERO,
            pre_fee_logs: Vec::new(),
            post_fee_logs: Vec::new(),
        }
    }
}

impl<DB: Database, I> MorphEvm<DB, I> {
    /// Consumes self and returns a new Evm type with given Inspector.
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

    /// Returns the cached token fee info set during handler validation.
    ///
    /// The cache is populated by `validate_and_deduct_token_fee` and persists
    /// through the handler lifecycle so that post-execution code (e.g., the
    /// block executor's receipt builder) can reuse it without re-reading the DB.
    #[inline]
    pub fn cached_token_fee_info(&self) -> Option<TokenFeeInfo> {
        self.cached_token_fee_info
    }

    /// Returns the L1 data fee cached during handler validation.
    ///
    /// Set in `validate_and_deduct_eth_fee` / `validate_and_deduct_token_fee` and
    /// reused by `reward_beneficiary` and the block executor's receipt builder,
    /// avoiding redundant `calculate_tx_l1_cost` calls and RLP re-encoding.
    #[inline]
    pub fn cached_l1_data_fee(&self) -> U256 {
        self.cached_l1_data_fee
    }

    /// Takes the cached pre-execution fee logs (token fee deduction Transfer events).
    #[inline]
    pub fn take_pre_fee_logs(&mut self) -> Vec<alloy_primitives::Log> {
        std::mem::take(&mut self.pre_fee_logs)
    }

    /// Takes the cached post-execution fee logs (token fee reimbursement Transfer events).
    #[inline]
    pub fn take_post_fee_logs(&mut self) -> Vec<alloy_primitives::Log> {
        std::mem::take(&mut self.post_fee_logs)
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
    fn morph_blockhash_block_zero() {
        // Block 0 requested from block 1 — block 0 is within [1-256, 1) = [0, 1), so valid
        let result = morph_blockhash_result(2818, 1, 0);
        assert_ne!(result, U256::ZERO, "block 0 from block 1 should be valid");

        // Block 0 requested from block 0 — current block returns zero
        let result = morph_blockhash_result(2818, 0, 0);
        assert_eq!(
            result,
            U256::ZERO,
            "block 0 from block 0 should be zero (current block)"
        );
    }

    #[test]
    fn morph_blockhash_chain_id_zero() {
        // chain_id=0 should still produce a deterministic hash
        let result = morph_blockhash_value(0, 100);
        assert_ne!(result, U256::ZERO, "chain_id=0 should still produce a hash");

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
                U256::ZERO,
                "block {n} from block 5 should be valid"
            );
        }
        // Block 5 (current) should be zero
        assert_eq!(morph_blockhash_result(chain_id, 5, 5), U256::ZERO);
    }

    #[test]
    fn morph_blockhash_boundary_256() {
        let chain_id = 2818;
        let current = 300;

        // current - 256 = 44 (inclusive lower bound)
        assert_ne!(
            morph_blockhash_result(chain_id, current, 44),
            U256::ZERO,
            "block current-256 should be valid"
        );

        // current - 257 = 43 (out of range)
        assert_eq!(
            morph_blockhash_result(chain_id, current, 43),
            U256::ZERO,
            "block current-257 should be zero"
        );

        // current - 1 = 299 (valid, most recent)
        assert_ne!(
            morph_blockhash_result(chain_id, current, 299),
            U256::ZERO,
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
