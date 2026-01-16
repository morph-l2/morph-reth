use crate::{
    MorphBlockExecutionCtx, evm::MorphEvm,
    system_contracts::{
        BLOB_ENABLED, GPO_IS_BLOB_ENABLED_SLOT, L1_GAS_PRICE_ORACLE_ADDRESS,
        L1_GAS_PRICE_ORACLE_INIT_STORAGE,
    },
};
use alloy_consensus::Receipt;
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
    eth::{
        EthBlockExecutor,
        receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx},
    },
};
use morph_chainspec::MorphChainSpec;
use morph_primitives::{MorphReceipt, MorphTransactionReceipt, MorphTxEnvelope, MorphTxType};
use morph_revm::{MorphHaltReason, evm::MorphContext};
use reth_revm::{Inspector, State, context::result::ResultAndState};
use revm::{
    Database as RevmDatabase,
    database::states::StorageSlot,
};

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
    /// Reference to the chain spec for hardfork checks.
    #[allow(dead_code)]
    chain_spec: &'a MorphChainSpec,
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
            chain_spec,
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

        // 3. Initialize L1 gas price oracle storage (idempotent - only applies if not already done)
        if let Err(err) = init_l1_gas_price_oracle_storage(self.inner.evm_mut().db_mut()) {
            return Err(BlockExecutionError::msg(format!(
                "error occurred at L1 gas price oracle initialization: {err:?}"
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

/// Initializes L1 gas price oracle storage slots for blob-based L1 fee calculations.
///
/// Updates L1GasPriceOracle storage slots:
/// - Sets `isBlobEnabled` slot to 1 (true)
/// - Sets `l1BlobBaseFee` slot to 1
/// - Sets `commitScalar` slot to initial value
/// - Sets `blobScalar` slot to initial value
///
/// This function is idempotent - if already applied (isBlobEnabled == 1), it's a no-op.
fn init_l1_gas_price_oracle_storage<DB: RevmDatabase>(
    state: &mut State<DB>,
) -> Result<(), DB::Error> {
    // No-op if already applied (check isBlobEnabled slot).
    // This makes the function idempotent so we can call it on every block.
    if state
        .database
        .storage(L1_GAS_PRICE_ORACLE_ADDRESS, GPO_IS_BLOB_ENABLED_SLOT)?
        == BLOB_ENABLED
    {
        return Ok(());
    }

    tracing::info!(target: "morph::evm", "Initializing L1 gas price oracle storage slots");

    let oracle = state.load_cache_account(L1_GAS_PRICE_ORACLE_ADDRESS)?;

    // Create storage updates
    let new_storage = L1_GAS_PRICE_ORACLE_INIT_STORAGE
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
