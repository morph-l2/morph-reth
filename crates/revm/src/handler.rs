//! Morph EVM Handler implementation.

use std::fmt::Debug;

use alloy_primitives::U256;
use revm::{
    context::{
        Cfg, ContextTr, JournalTr, Transaction,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    context_interface::Block,
    handler::{EvmTr, FrameTr, Handler, MainnetHandler, post_execution, pre_execution, validation},
    inspector::{Inspector, InspectorHandler},
    interpreter::{Gas, InitialAndFloorGas, interpreter::EthInterpreter},
};

use crate::{
    MorphEvm, MorphInvalidTransaction,
    error::MorphHaltReason,
    evm::MorphContext,
    l1block::L1BlockInfo,
    token_fee::{TokenFeeInfo, get_mapping_account_slot},
    tx::MorphTxExt,
};

/// Morph EVM [`Handler`] implementation.
///
/// This handler implements Morph-specific transaction fee logic:
/// - L1 data fee calculation and deduction
/// - L2 execution fee handling
/// - Gas reimbursement for unused gas
/// - L1 message transaction handling (no gas fees)
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
        // L1 message transactions skip all validation - everything is handled on L1 side
        if evm.ctx_ref().tx().is_l1_msg() {
            return Ok(());
        }

        // Check if transaction is AltFeeTx (tx_type 0x7F) which uses token fee
        if evm.ctx_ref().tx().is_alt_fee_tx() {
            // Get fee_token_id directly from MorphTxEnv
            let token_id = evm.ctx_ref().tx().fee_token_id.unwrap_or_default();
            return self.validate_and_deduct_token_fee(evm, token_id);
        }

        // Standard ETH-based fee handling
        self.validate_and_deduct_eth_fee(evm)
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // For L1 message transactions, no reimbursement is needed
        if evm.ctx_ref().tx().is_l1_msg() {
            return Ok(());
        }

        // Check if transaction is AltFeeTx (tx_type 0x7F) which uses token fee
        if evm.ctx_ref().tx().is_alt_fee_tx() {
            // Get fee_token_id directly from MorphTxEnv
            let token_id = evm.ctx_ref().tx().fee_token_id.unwrap_or_default();
            return self.reimburse_caller_token_fee(evm, exec_result.gas(), token_id);
        }

        // Standard ETH-based fee handling
        post_execution::reimburse_caller(evm.ctx(), exec_result.gas(), U256::ZERO)?;
        Ok(())
    }

    #[inline]
    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // L1 message transactions skip all validation - everything is handled on L1 side
        if evm.ctx_ref().tx().is_l1_msg() {
            return Ok(());
        }
        // AltFeeTx rewards are already applied when gasFee is deducted.
        if evm.ctx_ref().tx().is_alt_fee_tx() {
            return Ok(());
        }

        let beneficiary = evm.ctx_ref().block().beneficiary();

        let basefee = evm.ctx_ref().block().basefee() as u128;
        let effective_gas_price = evm.ctx_ref().tx().effective_gas_price(basefee);

        // Get the current hardfork for L1 fee calculation
        let hardfork = evm.ctx_ref().cfg().spec();

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().db_mut(), hardfork)?;

        // Get RLP-encoded transaction bytes for L1 fee calculation
        // This represents the full transaction data posted to L1 for data availability
        let rlp_bytes = evm
            .ctx_ref()
            .tx()
            .rlp_bytes
            .as_ref()
            .map(|b| b.as_ref())
            .unwrap_or_default();

        // Calculate L1 data fee based on full RLP-encoded transaction
        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(rlp_bytes, hardfork);

        // Get mutable access to journal components
        let journal = evm.ctx().journal_mut();

        let gas_spent = exec_result.gas().spent();
        let gas_refunded = exec_result.gas().refunded() as u64;
        let gas_used = gas_spent - gas_refunded;

        let execution_fee = U256::from(effective_gas_price).saturating_mul(U256::from(gas_used));

        // reward beneficiary
        journal
            .load_account_mut(beneficiary)?
            .incr_balance(execution_fee.saturating_add(l1_data_fee));

        Ok(())
    }

    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // For L1 message transactions, skip certain validations
        if evm.ctx_ref().tx().is_l1_msg() {
            // L1 messages have zero gas price, so skip gas price validation
            return Ok(());
        }

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

// Helper methods for MorphEvmHandler
impl<DB, I> MorphEvmHandler<DB, I>
where
    DB: alloy_evm::Database,
{
    /// Validate and deduct ETH-based gas fees.
    fn validate_and_deduct_eth_fee(
        &self,
        evm: &mut MorphEvm<DB, I>,
    ) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>> {
        // Get the current hardfork for L1 fee calculation
        let hardfork = evm.ctx_ref().cfg().spec();

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().db_mut(), hardfork)?;

        // Get RLP-encoded transaction bytes for L1 fee calculation
        // This represents the full transaction data posted to L1 for data availability
        let rlp_bytes = evm
            .ctx_ref()
            .tx()
            .rlp_bytes
            .as_ref()
            .map(|b| b.as_ref())
            .unwrap_or_default();

        // Calculate L1 data fee based on full RLP-encoded transaction
        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(rlp_bytes, hardfork);

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

    /// Validate and deduct token-based gas fees.
    ///
    /// This handles gas payment using ERC20 tokens instead of ETH.
    fn reimburse_caller_token_fee(
        &self,
        evm: &mut MorphEvm<DB, I>,
        gas: &Gas,
        token_id: u16,
    ) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>> {
        // Get caller address
        let caller = evm.ctx_ref().tx().caller();
        // Get coinbase address
        let beneficiary = evm.ctx_ref().block().beneficiary();
        let basefee = evm.ctx.block().basefee() as u128;
        let effective_gas_price = evm.ctx.tx().effective_gas_price(basefee);

        let reimburse_eth = U256::from(
            effective_gas_price.saturating_mul((gas.remaining() + gas.refunded() as u64) as u128),
        );

        // Fetch token fee info from Token Registry
        let token_fee_info = TokenFeeInfo::try_fetch(evm.ctx_mut().db_mut(), token_id, caller)?
            .ok_or(MorphInvalidTransaction::TokenNotRegistered(token_id))?;

        // Check if token is active
        if !token_fee_info.is_active {
            return Err(MorphInvalidTransaction::TokenNotActive(token_id).into());
        }

        // Calculate token amount required for total fee
        let token_amount_required = token_fee_info.calculate_token_amount(reimburse_eth);

        // Get mutable access to journal components
        let journal = evm.ctx().journal_mut();

        // Transfer with token slot.
        if let Some(balance_slot) = token_fee_info.balance_slot {
            // Sub amount
            let token_storage_slot = get_mapping_account_slot(balance_slot, beneficiary);
            let balance = journal
                .sload(token_fee_info.token_address, token_storage_slot)
                .unwrap_or_default();
            journal.sstore(
                caller,
                token_storage_slot,
                balance.saturating_sub(token_amount_required),
            )?;

            // Add amount
            let token_storage_slot = get_mapping_account_slot(balance_slot, caller);
            let balance = journal
                .sload(token_fee_info.token_address, token_storage_slot)
                .unwrap_or_default();
            journal.sstore(
                caller,
                token_storage_slot,
                balance.saturating_add(token_amount_required),
            )?;
        }
        Ok(())
    }

    /// Validate and deduct token-based gas fees.
    ///
    /// This handles gas payment using ERC20 tokens instead of ETH.
    fn validate_and_deduct_token_fee(
        &self,
        evm: &mut MorphEvm<DB, I>,
        token_id: u16,
    ) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>> {
        // Token ID 0 not supported for gas payment.
        if token_id == 0 {
            return Err(MorphInvalidTransaction::TokenIdZeroNotSupported.into());
        }

        // Get caller address
        let caller_addr = evm.ctx_ref().tx().caller();
        // Get coinbase address
        let beneficiary = evm.ctx_ref().block().beneficiary();

        // Fetch token fee info from Token Registry
        let token_fee_info =
            TokenFeeInfo::try_fetch(evm.ctx_mut().db_mut(), token_id, caller_addr)?
                .ok_or(MorphInvalidTransaction::TokenNotRegistered(token_id))?;

        // Check if token is active
        if !token_fee_info.is_active {
            return Err(MorphInvalidTransaction::TokenNotActive(token_id).into());
        }

        // Get the current hardfork for L1 fee calculation
        let hardfork = evm.ctx_ref().cfg().spec();

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().db_mut(), hardfork)?;

        // Get RLP-encoded transaction bytes for L1 fee calculation
        // This represents the full transaction data posted to L1 for data availability
        let rlp_bytes = evm
            .ctx_ref()
            .tx()
            .rlp_bytes
            .as_ref()
            .map(|b| b.as_ref())
            .unwrap_or_default();

        // Calculate L1 data fee (in ETH) based on full RLP-encoded transaction
        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(rlp_bytes, hardfork);

        // Calculate L2 gas fee (in ETH)
        let gas_limit = evm.ctx_ref().tx().gas_limit();
        let gas_price = evm.ctx_ref().tx().gas_price();
        let l2_gas_fee = U256::from(gas_limit).saturating_mul(U256::from(gas_price));

        // Total fee in ETH
        let total_eth_fee = l2_gas_fee.saturating_add(l1_data_fee);

        // Calculate token amount required for total fee
        let token_amount_required = token_fee_info.calculate_token_amount(total_eth_fee);

        // Determine fee limit
        let mut fee_limit = evm.ctx_ref().tx().fee_limit.unwrap_or_default();
        if fee_limit.is_zero() || fee_limit > token_fee_info.balance {
            fee_limit = token_fee_info.balance
        }

        // Check if caller has sufficient token balance
        if fee_limit < token_amount_required {
            return Err(MorphInvalidTransaction::InsufficientTokenBalance {
                required: token_amount_required,
                available: fee_limit,
            }
            .into());
        }

        // Get mutable access to context components
        let (_, tx, cfg, journal, _, _) = evm.ctx().all_mut();

        // Transfer with token slot.
        if let Some(balance_slot) = token_fee_info.balance_slot {
            // Ensure token account is loaded into the journal state, because `sload`/`sstore`
            // assume the account is present.
            let _ = journal.load_account_mut(token_fee_info.token_address)?;

            // Sub amount
            let caller_token_storage_slot = get_mapping_account_slot(balance_slot, caller_addr);
            let new_token_balance = token_fee_info.balance.saturating_sub(token_amount_required);
            journal.sstore(
                token_fee_info.token_address,
                caller_token_storage_slot,
                new_token_balance,
            )?;

            // Add amount
            let beneficiary_token_storage_slot = get_mapping_account_slot(balance_slot, beneficiary);
            let balance = journal
                .sload(token_fee_info.token_address, beneficiary_token_storage_slot)
                .unwrap_or_default();
            journal.sstore(
                token_fee_info.token_address,
                beneficiary_token_storage_slot,
                balance.saturating_add(token_amount_required),
            )?;

            // We don't want the fee-token account/slots we touched during validation to become
            // warm for the rest of the transaction execution.
            if let Some(token_acc) = journal.state.get_mut(&token_fee_info.token_address) {
                token_acc.mark_cold();
                if let Some(slot) = token_acc.storage.get_mut(&caller_token_storage_slot) {
                    slot.mark_cold();
                }
                if let Some(slot) = token_acc.storage.get_mut(&beneficiary_token_storage_slot) {
                    slot.mark_cold();
                }
            }
        }

        // Load caller's account for nonce/code validation
        let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

        // Validate account nonce and code (EIP-3607)
        pre_execution::validate_account_nonce_and_code(
            &caller.info,
            tx.nonce(),
            cfg.is_eip3607_disabled(),
            cfg.is_nonce_check_disabled(),
        )?;

        // Bump nonce for calls (CREATE nonce is bumped in make_create_frame)
        if tx.kind().is_call() {
            caller.bump_nonce();
        }

        Ok(())
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
    if !is_balance_check_disabled && balance < total_spending {
        return Err(InvalidTransaction::LackOfFundForMaxFee {
            fee: Box::new(total_spending),
            balance: Box::new(balance),
        });
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

