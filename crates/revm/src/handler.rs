//! Morph EVM Handler implementation.

use std::fmt::Debug;

use alloy_primitives::U256;
use revm::{
    context::{
        Cfg, ContextTr, JournalTr, Transaction,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    context_interface::Block,
    handler::{
        EvmTr, FrameTr, Handler, MainnetHandler,
        pre_execution,
        validation,
    },
    inspector::{Inspector, InspectorHandler},
    interpreter::{
        InitialAndFloorGas,
        interpreter::EthInterpreter,
    },
};

use crate::{
    MorphEvm, MorphInvalidTransaction,
    error::MorphHaltReason,
    evm::MorphContext,
    l1block::L1BlockInfo,
};

/// Morph EVM [`Handler`] implementation.
///
/// This handler implements Morph-specific transaction fee logic:
/// - L1 data fee calculation and deduction
/// - L2 execution fee handling
/// - Gas reimbursement for unused gas
#[derive(Debug)]
pub struct MorphEvmHandler<DB, I> {
    /// Phantom data to avoid type inference issues.
    _phantom: core::marker::PhantomData<(DB, I)>,
}

impl<DB, I> MorphEvmHandler<DB, I> {
    /// Create a new [`MorphEvmHandler`] handler instance
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB, I> Default for MorphEvmHandler<DB, I> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB, I> Handler for MorphEvmHandler<DB, I>
where
    DB: alloy_evm::Database,
{
    type Evm = MorphEvm<DB, I>;
    type Error = EVMError<DB::Error, MorphInvalidTransaction>;
    type HaltReason = MorphHaltReason;

    #[inline]
    fn run(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match self.run_without_catch_error(evm) {
            Ok(output) => Ok(output),
            Err(err) => self.catch_error(evm, err),
        }
    }

    #[inline]
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        evm.logs.clear();
        if !result.instruction_result().is_ok() {
            evm.logs = evm.journal_mut().take_logs();
        }

        MainnetHandler::default()
            .execution_result(evm, result)
            .map(|result| result.map_haltreason(Into::into))
    }

    #[inline]
    fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        pre_execution::apply_eip7702_auth_list(evm.ctx())
    }

    #[inline]
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<(), Self::Error> {
        // Get the current hardfork for L1 fee calculation
        let hardfork = evm.ctx_ref().cfg().spec();

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().db_mut(), hardfork)?;

        // Get transaction input data for L1 fee calculation
        let tx_input = evm.ctx_ref().tx().input();

        // Calculate L1 data fee
        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(tx_input, hardfork);

        // Get mutable access to context components
        let (block, tx, cfg, journal, _, _) = evm.ctx().all_mut();

        // Load caller's account
        let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

        // Validate account nonce and code (EIP-3607)
        pre_execution::validate_account_nonce_and_code(
            &caller.info,
            tx.nonce(),
            cfg.is_eip3607_disabled(),
            cfg.is_nonce_check_disabled(),
        )?;

        // Calculate L2 fee and validate balance
        // This includes: gas_limit * gas_price + value + blob_fee
        let new_balance_after_l2_fee =
            calculate_caller_fee_with_l1_cost(*caller.balance(), tx, block, cfg, l1_data_fee)?;

        // Set the new balance (deducting L2 fee + L1 data fee)
        caller.set_balance(new_balance_after_l2_fee);

        // Bump nonce for calls (CREATE nonce is bumped in make_create_frame)
        if tx.kind().is_call() {
            caller.bump_nonce();
        }

        Ok(())
    }

    fn reimburse_caller(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // For Morph L2, we don't reimburse caller
        // The L2 execution fee is handled by the sequencer
        // L1 data fee is a fixed cost that is not refunded
        Ok(())
    }

    #[inline]
    fn reward_beneficiary(
        &self,
        _evm: &mut Self::Evm,
        _exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // For Morph L2, beneficiary reward is handled differently
        // The sequencer collects fees through the L1 fee vault mechanism
        Ok(())
    }

    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        validation::validate_env::<_, Self::Error>(evm.ctx())?;
        Ok(())
    }

    #[inline]
    fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let tx = evm.ctx_ref().tx();
        let cfg = evm.ctx_ref().cfg();
        let spec = cfg.spec().into();
        Ok(
            validation::validate_initial_tx_gas(tx, spec, cfg.is_eip7623_disabled())
                .map_err(MorphInvalidTransaction::EthInvalidTransaction)?,
        )
    }

    fn catch_error(
        &self,
        evm: &mut Self::Evm,
        error: Self::Error,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        MainnetHandler::default()
            .catch_error(evm, error)
            .map(|result| result.map_haltreason(Into::into))
    }
}

impl<DB, I> InspectorHandler for MorphEvmHandler<DB, I>
where
    DB: alloy_evm::Database,
    I: Inspector<MorphContext<DB>>,
{
    type IT = EthInterpreter;

    fn inspect_run(
        &mut self,
        evm: &mut Self::Evm,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        match self.inspect_run_without_catch_error(evm) {
            Ok(output) => Ok(output),
            Err(e) => self.catch_error(evm, e),
        }
    }
}

/// Calculate the new balance after deducting L2 fees and L1 data fee.
///
/// This is a Morph-specific version of `pre_execution::calculate_caller_fee` that
/// also includes the L1 data fee in the balance calculation.
///
/// # Arguments
/// * `balance` - Current caller balance
/// * `tx` - Transaction
/// * `block` - Block environment
/// * `cfg` - Configuration
/// * `l1_data_fee` - L1 data fee calculated from L1BlockInfo
///
/// # Returns
/// The new balance after deducting all fees, or an error if balance is insufficient.
fn calculate_caller_fee_with_l1_cost(
    balance: U256,
    tx: impl Transaction,
    block: impl Block,
    cfg: impl Cfg,
    l1_data_fee: U256,
) -> Result<U256, InvalidTransaction> {
    let basefee = block.basefee() as u128;
    let blob_price = block.blob_gasprice().unwrap_or_default();
    let is_balance_check_disabled = cfg.is_balance_check_disabled();

    // Calculate L2 effective balance spending (gas + value + blob fees)
    let effective_balance_spending = tx
        .effective_balance_spending(basefee, blob_price)
        .expect("effective balance is always smaller than max balance so it can't overflow");

    // Total spending = L2 fees + L1 data fee
    let total_spending = effective_balance_spending.saturating_add(l1_data_fee);

    // Check if caller has enough balance for total spending
    if !is_balance_check_disabled {
        if balance < total_spending {
            return Err(InvalidTransaction::LackOfFundForMaxFee {
                fee: Box::new(total_spending),
                balance: Box::new(balance),
            });
        }
    }

    // Calculate gas balance spending (excluding value transfer)
    let gas_balance_spending = effective_balance_spending - tx.value();

    // Total fee deduction = L2 gas fees + L1 data fee (value is transferred separately)
    let total_fee_deduction = gas_balance_spending.saturating_add(l1_data_fee);

    // New balance after fee deduction
    let mut new_balance = balance.saturating_sub(total_fee_deduction);

    if is_balance_check_disabled {
        // Make sure the caller's balance is at least the value of the transaction.
        new_balance = new_balance.max(tx.value());
    }

    Ok(new_balance)
}
