use crate::{MorphBlockExecutionCtx, evm::MorphEvm};
use alloy_consensus::Receipt;
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{
        EthBlockExecutor,
        receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
    },
};
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxEnvelope, MorphTxType};
use morph_revm::{
    CURIE_L1_GAS_PRICE_ORACLE_STORAGE, L1_GAS_PRICE_ORACLE_ADDRESS, MorphHaltReason,
    evm::MorphContext,
};
use reth_revm::{Inspector, State, context::result::ResultAndState};
use revm::{Database as RevmDatabase, database::states::StorageSlot};

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

        let inner = Receipt {
            status: result.is_success().into(),
            cumulative_gas_used,
            logs: result.into_logs(),
        };

        // Create the appropriate receipt variant based on transaction type
        // TODO: Add L1 fee calculation from execution context
        match tx.tx_type() {
            MorphTxType::Legacy => MorphReceipt::Legacy(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip2930 => MorphReceipt::Eip2930(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip1559 => MorphReceipt::Eip1559(MorphTransactionReceipt::new(inner)),
            MorphTxType::Eip7702 => MorphReceipt::Eip7702(MorphTransactionReceipt::new(inner)),
            MorphTxType::L1Msg => MorphReceipt::L1Msg(inner),
            MorphTxType::AltFee => MorphReceipt::AltFee(MorphTransactionReceipt::new(inner)),
        }
    }
}

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

        // 2. Load L1 gas oracle contract into cache for L1 fee calculations
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

/// Applies the Morph Curie hard fork to the state.
///
/// Updates L1GasPriceOracle storage slots:
/// - Sets `l1BlobBaseFee` slot to 1
/// - Sets `commitScalar` slot to initial value
/// - Sets `blobScalar` slot to initial value
/// - Sets `isCurie` slot to 1 (true)
///
/// This function should only be called once at the Curie transition block.
/// Reference: `consensus/misc/curie.go` in morph go-ethereum
fn apply_curie_hard_fork<DB: RevmDatabase>(state: &mut State<DB>) -> Result<(), DB::Error> {
    tracing::info!(target: "morph::evm", "Applying Curie hard fork");

    let oracle = state.load_cache_account(L1_GAS_PRICE_ORACLE_ADDRESS)?;

    // Create storage updates
    let new_storage = CURIE_L1_GAS_PRICE_ORACLE_STORAGE
        .into_iter()
        .map(|(slot, present_value)| {
            (
                slot,
                StorageSlot {
                    present_value,
                    previous_or_original_value: oracle.storage_slot(slot).unwrap_or_default(),
                },
            )
        })
        .collect();

    // Get existing account info or use default
    let oracle_info = oracle.account_info().unwrap_or_default();

    // Create transition for oracle storage update
    let transition = oracle.change(oracle_info, new_storage);

    // Add transition to state
    if let Some(s) = state.transition_state.as_mut() {
        s.add_transitions(vec![(L1_GAS_PRICE_ORACLE_ADDRESS, transition)]);
    }

    Ok(())
}
