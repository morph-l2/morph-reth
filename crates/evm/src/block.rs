use crate::{MorphBlockExecutionCtx, evm::MorphEvm};
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{
        EthBlockExecutor,
        receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
    },
};
use reth_revm::{Inspector, State, context::result::ResultAndState};
use morph_chainspec::MorphChainSpec;
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{MorphHaltReason, evm::MorphContext};


/// Builder for [`MorphReceipt`].
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub(crate) struct MorphReceiptBuilder;

impl ReceiptBuilder for MorphReceiptBuilder {
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;

    fn build_receipt<E: Evm>(
        &self,
        ctx: ReceiptBuilderCtx<'_, Self::Transaction, E>,
    ) -> Self::Receipt {
        let ReceiptBuilderCtx {
            tx,
            result,
            cumulative_gas_used,
            ..
        } = ctx;
        MorphReceipt {
            tx_type: tx.tx_type(),
            // Success flag was added in `EIP-658: Embedding transaction status code in
            // receipts`.
            success: result.is_success(),
            cumulative_gas_used,
            logs: result.into_logs(),
        }
    }
}

/// Block executor for Morph. Wraps an inner [`EthBlockExecutor`].
pub(crate) struct MorphBlockExecutor<'a, DB: Database, I> {
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
            inner: EthBlockExecutor::new(evm, ctx.inner, chain_spec, MorphReceiptBuilder::default()),
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
        self.inner.apply_pre_execution_changes()
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
