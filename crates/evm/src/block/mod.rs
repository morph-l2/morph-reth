//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions
//! - Hardfork application logic (Curie, etc.)

pub(crate) mod curie;
mod receipt;

pub(crate) use receipt::MorphReceiptBuilder;

use crate::{MorphBlockExecutionCtx, evm::MorphEvm};
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::EthBlockExecutor,
};
use curie::apply_curie_hard_fork;
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{L1_GAS_PRICE_ORACLE_ADDRESS, MorphHaltReason, evm::MorphContext};
use reth_revm::{Inspector, State, context::result::ResultAndState};

/// Block executor for Morph. Wraps an inner [`EthBlockExecutor`].
pub(crate) struct MorphBlockExecutor<'a, DB: Database, I> {
    /// Inner Ethereum block executor.
    pub(crate) inner: EthBlockExecutor<
        'a,
        MorphEvm<&'a mut State<DB>, I>,
        &'a MorphChainSpec,
        MorphReceiptBuilder,
    >,
}

impl<'a, DB, I> MorphBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<&'a mut State<DB>>>,
{
    pub(crate) fn new(
        evm: MorphEvm<&'a mut State<DB>, I>,
        ctx: MorphBlockExecutionCtx<'a>,
        chain_spec: &'a MorphChainSpec,
    ) -> Self {
        Self {
            inner: EthBlockExecutor::new(
                evm,
                ctx.inner,
                chain_spec,
                MorphReceiptBuilder::default(),
            ),
        }
    }
}

impl<'a, DB, I> BlockExecutor for MorphBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<&'a mut State<DB>>>,
{
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;
    type Evm = MorphEvm<&'a mut State<DB>, I>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // 1. Apply base Ethereum pre-execution changes (state clear flag, EIP-2935, EIP-4788)
        self.inner.apply_pre_execution_changes()?;

        // 2. Load L1 gas oracle contract into cache for Curie hardfork upgrade
        let _ = self
            .inner
            .evm_mut()
            .db_mut()
            .load_cache_account(L1_GAS_PRICE_ORACLE_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        // 3. Apply Curie hardfork at the transition block
        // Only executes once at the exact block where Curie activates
        let block_number = self.inner.evm.block().number.saturating_to();
        if self
            .inner
            .spec
            .morph_fork_activation(MorphHardfork::Curie)
            .transitions_at_block(block_number)
            && let Err(err) = apply_curie_hard_fork(self.inner.evm_mut().db_mut())
        {
            return Err(BlockExecutionError::msg(format!(
                "error occurred at Curie fork: {err:?}"
            )));
        }

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<MorphHaltReason>, BlockExecutionError> {
        self.inner.execute_transaction_without_commit(tx)
    }

    fn commit_transaction(
        &mut self,
        output: ResultAndState<MorphHaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        self.inner.commit_transaction(output, tx)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        self.inner.finish()
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.inner.set_state_hook(hook)
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        self.inner.evm_mut()
    }

    fn evm(&self) -> &Self::Evm {
        self.inner.evm()
    }
}
