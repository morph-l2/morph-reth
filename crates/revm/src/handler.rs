//! Morph EVM Handler implementation.

use alloy_primitives::{Address, Bytes, U256};
use revm::{
    ExecuteEvm,
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
    MorphEvm, MorphInvalidTransaction, MorphTxEnv,
    error::MorphHaltReason,
    evm::MorphContext,
    l1block::L1BlockInfo,
    token_fee::{TokenFeeInfo, compute_mapping_slot_for_address, encode_balance_of_calldata},
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
        // Reset per-transaction caches from the previous iteration.
        evm.cached_l1_data_fee = U256::ZERO;
        evm.cached_token_fee_info = None;
        evm.pre_fee_logs.clear();
        evm.post_fee_logs.clear();

        let (_, tx, _, journal, _, _) = evm.ctx().all_mut();

        if tx.is_l1_msg() {
            let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

            // CREATE nonce is bumped later in make_create_frame
            if tx.kind().is_call() {
                caller.bump_nonce();
            }
            return Ok(());
        }

        // MorphTx (0x7F) can use token fee (fee_token_id > 0) or ETH fee (fee_token_id == 0).
        if evm.ctx_ref().tx().is_morph_tx() {
            let token_id = evm.ctx_ref().tx().fee_token_id.unwrap_or_default();
            if token_id > 0 {
                return self.validate_and_deduct_token_fee(evm, token_id);
            }
            return self.validate_and_deduct_eth_fee(evm);
        }

        // Standard ETH-based fee handling
        self.validate_and_deduct_eth_fee(evm)
    }

    fn reimburse_caller(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        let (_, tx, _, _, _, _) = evm.ctx().all_mut();

        // L1 message gas is prepaid on L1, no reimbursement needed.
        if tx.is_l1_msg() {
            return Ok(());
        }

        // MorphTx (0x7F) with token fee: reimburse unused tokens.
        // fee_token_id == 0 falls through to the standard ETH reimbursement below.
        if tx.is_morph_tx() {
            let token_id = tx.fee_token_id.unwrap_or_default();
            if token_id > 0 {
                // When fee charge was disabled (eth_call), no token was deducted and
                // cached_token_fee_info was not set — skip reimbursement entirely.
                if evm.cached_token_fee_info.is_none() {
                    return Ok(());
                }
                return self.reimburse_caller_token_fee(evm, exec_result.gas());
            }
        }

        // Standard ETH-based fee handling (also handles MorphTx with fee_token_id == 0)
        post_execution::reimburse_caller(evm.ctx(), exec_result.gas(), U256::ZERO)?;
        Ok(())
    }

    #[inline]
    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) {
        // L1 message tx follows go-ethereum semantics: no gas refunds.
        // Keep gas_used as actual consumed gas without applying post-exec refund.
        if evm.ctx_ref().tx().is_l1_msg() {
            // revm::Gas::used() subtracts `refunded` by default.
            // For L1 messages we must zero it out, otherwise gas_used is undercounted.
            exec_result.gas_mut().set_refund(0);
            return;
        }
        let spec = evm.ctx().cfg().spec().into();
        post_execution::refund(spec, exec_result.gas_mut(), eip7702_refund);
    }

    #[inline]
    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
    ) -> Result<(), Self::Error> {
        // Reuse the L1 data fee cached during validate_and_deduct_eth_fee /
        // validate_and_deduct_token_fee, avoiding a redundant calculate_tx_l1_cost call.
        // Read before ctx().all_mut() borrows evm.
        let l1_data_fee = evm.cached_l1_data_fee;

        let (block, tx, _, journal, _, _) = evm.ctx().all_mut();

        // L1 messages skip all reward.
        // Token-fee MorphTx rewards are already applied when token fee is deducted.
        if tx.is_l1_msg() || (tx.is_morph_tx() && tx.fee_token_id.unwrap_or_default() > 0) {
            return Ok(());
        }

        let beneficiary = block.beneficiary();

        let basefee = block.basefee() as u128;
        let effective_gas_price = tx.effective_gas_price(basefee);

        let gas_used = exec_result.gas().used();

        let execution_fee = U256::from(effective_gas_price).saturating_mul(U256::from(gas_used));

        // reward beneficiary
        journal
            .load_account_mut(beneficiary)?
            .incr_balance(execution_fee.saturating_add(l1_data_fee));

        Ok(())
    }

    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        // For L1 message transactions
        if evm.ctx_ref().tx().is_l1_msg() {
            // L1 messages have zero gas price, so skip gas price validation
            return Ok(());
        }

        // Standard validation.
        // Note: revm maps MorphTx (type 0x7F) to `TransactionType::Custom`,
        // which skips gas-price checks entirely.
        validation::validate_env::<_, Self::Error>(evm.ctx())?;

        // For MorphTx V1 with ETH fee (fee_token_id == 0), gas price must be validated
        // against basefee — the same rule that applies to EIP-1559 transactions.
        // Token-fee MorphTx (fee_token_id > 0) intentionally skips this check because
        // fees are paid in ERC20 tokens.
        // Skip for simulation contexts (eth_call / eth_estimateGas) where fee charge
        // is disabled, matching go-ethereum's NoBaseFee behaviour.
        if evm.ctx_ref().tx().is_morph_tx()
            && !evm.ctx_ref().tx().uses_token_fee()
            && !evm.ctx_ref().cfg().is_fee_charge_disabled()
        {
            let base_fee = Some(evm.ctx_ref().block().basefee() as u128);
            validation::validate_priority_fee_tx(
                evm.ctx_ref().tx().max_fee_per_gas(),
                evm.ctx_ref()
                    .tx()
                    .max_priority_fee_per_gas()
                    .unwrap_or_default(),
                base_fee,
                evm.ctx_ref().cfg().is_priority_fee_check_disabled(),
            )?;
        }

        Ok(())
    }

    #[inline]
    fn validate_initial_tx_gas(&self, evm: &Self::Evm) -> Result<InitialAndFloorGas, Self::Error> {
        let tx = evm.ctx_ref().tx();
        let spec = evm.ctx_ref().cfg().spec().into();
        let disable_eip7623 = evm.ctx_ref().cfg().is_eip7623_disabled();

        // For L1 message transactions, handle intrinsic gas specially
        if tx.is_l1_msg() {
            // Calculate intrinsic gas (same as normal transactions)
            let initial_and_floor = validation::validate_initial_tx_gas(tx, spec, disable_eip7623)
                .unwrap_or_else(|_| {
                    // If intrinsic gas > gas_limit, use gas_limit as intrinsic gas
                    // This matches go-ethereum's behavior for L1 messages
                    InitialAndFloorGas {
                        initial_gas: tx.gas_limit(),
                        floor_gas: 0,
                    }
                });

            return Ok(initial_and_floor);
        }

        // Normal transaction validation
        let initial_and_floor = validation::validate_initial_tx_gas(tx, spec, disable_eip7623)
            .map_err(MorphInvalidTransaction::EthInvalidTransaction)?;

        Ok(initial_and_floor)
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
    #[inline]
    fn validate_and_deduct_eth_fee(
        &self,
        evm: &mut MorphEvm<DB, I>,
    ) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>> {
        let hardfork = evm.ctx_ref().cfg().spec();

        // Fetch L1 block info from the L1 Gas Price Oracle contract per-tx.
        // Must NOT use a per-block cache because the oracle can be updated by a
        // regular transaction (from the external gas-oracle service) within the
        // same block.  Subsequent user txs must see the updated fee parameters,
        // matching go-ethereum's per-tx L1BlockInfo read.
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().db_mut(), hardfork)?;

        let rlp_bytes = evm
            .ctx_ref()
            .tx()
            .rlp_bytes
            .as_ref()
            .map(|b| b.as_ref())
            .unwrap_or_default();

        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(rlp_bytes, hardfork);
        evm.cached_l1_data_fee = l1_data_fee;

        let (block, tx, cfg, journal, _, _) = evm.ctx().all_mut();

        let mut caller = journal.load_account_with_code_mut(tx.caller())?.data;

        pre_execution::validate_account_nonce_and_code(
            &caller.info,
            tx.nonce(),
            cfg.is_eip3607_disabled(),
            cfg.is_nonce_check_disabled(),
        )?;

        let new_balance_after_l2_fee =
            calculate_caller_fee_with_l1_cost(*caller.balance(), tx, block, cfg, l1_data_fee)?;

        caller.set_balance(new_balance_after_l2_fee);

        // CREATE nonce is bumped later in make_create_frame
        if tx.kind().is_call() {
            caller.bump_nonce();
        }

        Ok(())
    }

    /// Reimburse unused gas fees in ERC20 tokens.
    ///
    /// Uses the cached `TokenFeeInfo` from the deduction phase to ensure
    /// consistent price_ratio/scale, matching go-ethereum's `st.feeRate`/`st.tokenScale`.
    #[inline]
    fn reimburse_caller_token_fee(
        &self,
        evm: &mut MorphEvm<DB, I>,
        gas: &Gas,
    ) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>> {
        let caller = evm.ctx_ref().tx().caller();
        let beneficiary = evm.ctx_ref().block().beneficiary();
        let basefee = evm.ctx.block().basefee() as u128;
        let effective_gas_price = evm.ctx.tx().effective_gas_price(basefee);

        let refunded = gas.refunded().max(0) as u64;
        let reimburse_eth = U256::from(
            effective_gas_price.saturating_mul(gas.remaining().saturating_add(refunded) as u128),
        );

        if reimburse_eth.is_zero() {
            return Ok(());
        }

        // Use cached token fee info from the deduction phase (set in validate_and_deduct_token_fee).
        // This ensures the same price_ratio/scale is used for both deduction and reimbursement.
        // The cache is kept populated (not taken) so the block executor's receipt builder
        // can also read it without re-querying the DB.
        let token_fee_info =
            evm.cached_token_fee_info
                .ok_or(MorphInvalidTransaction::TokenTransferFailed {
                    reason: "cached_token_fee_info not set by validate_and_deduct_token_fee".into(),
                })?;

        // Calculate token amount required for total fee
        let token_amount_required = token_fee_info.eth_to_token_amount(reimburse_eth);

        // Attempt token refund. Matches go-ethereum's refundGas() which silently logs
        // and continues on failure: "Continue execution even if refund fails - refund
        // should not cause transaction to fail" (state_transition.go:698).
        let refund_result = if let Some(balance_slot) = token_fee_info.balance_slot {
            let journal = evm.ctx().journal_mut();
            transfer_erc20_with_slot(
                journal,
                beneficiary,
                caller,
                token_fee_info.token_address,
                token_amount_required,
                balance_slot,
            )
            .map(|_| ())
        } else {
            // Cache refund Transfer logs separately, matching the pre_fee_logs
            // pattern from validate_and_deduct_token_fee.
            let log_count_before = evm.ctx_mut().journal_mut().logs.len();
            let result = transfer_erc20_with_evm(
                evm,
                beneficiary,
                caller,
                token_fee_info.token_address,
                token_amount_required,
                None,
            );
            let refund_logs: Vec<_> = evm
                .ctx_mut()
                .journal_mut()
                .logs
                .drain(log_count_before..)
                .collect();
            evm.post_fee_logs = refund_logs;
            result
        };

        if let Err(err) = refund_result {
            tracing::error!(
                target: "morph::evm",
                token_id = ?evm.ctx_ref().tx().fee_token_id,
                %err,
                "failed to refund alt token gas, continuing execution"
            );
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
        // Token ID 0 means ETH — routed through validate_and_deduct_eth_fee instead.
        if token_id == 0 {
            return Err(MorphInvalidTransaction::TokenIdZeroNotSupported.into());
        }

        {
            let (_, tx, cfg, journal, _, _) = evm.ctx_mut().all_mut();
            let caller_addr = tx.caller();
            let nonce = tx.nonce();

            // Validate account nonce and code (EIP-3607) BEFORE any state mutations,
            // matching the order used in validate_and_deduct_eth_fee.
            let caller = journal.load_account_with_code_mut(caller_addr)?.data;
            pre_execution::validate_account_nonce_and_code(
                &caller.info,
                nonce,
                cfg.is_eip3607_disabled(),
                cfg.is_nonce_check_disabled(),
            )?;
        }

        let caller_addr = evm.ctx_ref().tx().caller();
        let is_call = evm.ctx_ref().tx().kind().is_call();

        // eth_call (disable_fee_charge): skip token fee deduction entirely.
        // Only nonce/code validation (above) and nonce bump are needed.
        // This matches the ETH path's disable_fee_charge semantics and ensures
        // eth_call is a pure simulation without token registry lookups, balance
        // checks, or ERC20 transfers.
        if evm.ctx_ref().cfg().is_fee_charge_disabled() {
            if is_call {
                let mut caller = evm
                    .ctx_mut()
                    .journal_mut()
                    .load_account_with_code_mut(caller_addr)?
                    .data;
                caller.bump_nonce();
            }
            return Ok(());
        }

        let beneficiary = evm.ctx_ref().block().beneficiary();
        let hardfork = evm.ctx_ref().cfg().spec();
        let tx_value = evm.ctx_ref().tx().value();
        let rlp_bytes = evm.ctx_ref().tx().rlp_bytes.clone().unwrap_or_default();
        let gas_limit = evm.ctx_ref().tx().gas_limit();
        let fee_limit_from_tx = evm.ctx_ref().tx().fee_limit.unwrap_or_default();
        let basefee = evm.ctx_ref().block().basefee() as u128;
        let effective_gas_price = evm.ctx_ref().tx().effective_gas_price(basefee);

        // Check that caller has enough ETH to cover the value transfer.
        // This matches go-ethereum's buyAltTokenGas() which checks
        // state.GetBalance(from) >= value before proceeding.
        // Without this, the tx would proceed to EVM execution and fail there
        // (consuming gas), whereas go-ethereum rejects at the preCheck stage
        // (not consuming gas).
        if !tx_value.is_zero() {
            let caller_eth_balance = *evm
                .ctx_mut()
                .journal_mut()
                .load_account_mut(caller_addr)?
                .data
                .balance();
            if caller_eth_balance < tx_value {
                return Err(MorphInvalidTransaction::EthInvalidTransaction(
                    InvalidTransaction::LackOfFundForMaxFee {
                        fee: Box::new(tx_value),
                        balance: Box::new(caller_eth_balance),
                    },
                )
                .into());
            }
        }

        // Fetch token fee info from Token Registry
        let token_fee_info = TokenFeeInfo::load_for_caller(
            evm.ctx_mut().journal_mut().db_mut(),
            token_id,
            caller_addr,
            hardfork,
        )?
        .ok_or(MorphInvalidTransaction::TokenNotRegistered(token_id))?;

        if !token_fee_info.is_active {
            return Err(MorphInvalidTransaction::TokenNotActive(token_id).into());
        }

        // Get RLP-encoded transaction bytes for L1 fee calculation
        // Fetch L1 block info per-tx (same rationale as validate_and_deduct_eth_fee).
        let l1_block_info = L1BlockInfo::try_fetch(evm.ctx_mut().journal_mut().db_mut(), hardfork)?;
        let l1_data_fee = l1_block_info.calculate_tx_l1_cost(rlp_bytes.as_ref(), hardfork);

        // Calculate L2 gas fee using effective_gas_price (= min(gasTipCap + baseFee, gasFeeCap)),
        // matching go-ethereum's buyAltTokenGas() which uses st.gasPrice (effective gas price).
        // tx.gas_price() returns max_fee_per_gas and would overcharge when tip + basefee < feeCap.
        let l2_gas_fee = U256::from(gas_limit).saturating_mul(U256::from(effective_gas_price));

        // Total fee in ETH
        let total_eth_fee = l2_gas_fee.saturating_add(l1_data_fee);

        // Calculate token amount required for total fee
        let token_amount_required = token_fee_info.eth_to_token_amount(total_eth_fee);

        // Determine fee limit
        let mut fee_limit = fee_limit_from_tx;
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

        if let Some(balance_slot) = token_fee_info.balance_slot {
            // Transfer with token slot.
            // Ensure token account is loaded into the journal state, because `sload`/`sstore`
            // assume the account is present.
            let journal = evm.ctx_mut().journal_mut();
            let _ = journal.load_account_mut(token_fee_info.token_address)?;
            journal.touch(token_fee_info.token_address);
            let (from_storage_slot, to_storage_slot) = transfer_erc20_with_slot(
                journal,
                caller_addr,
                beneficiary,
                token_fee_info.token_address,
                token_amount_required,
                balance_slot,
            )?;
            // We don't want the fee-token account/slots we touched during validation to become
            // warm for the rest of the transaction execution.
            if let Some(token_acc) = journal.state.get_mut(&token_fee_info.token_address) {
                token_acc.mark_cold();
                if let Some(slot) = token_acc.storage.get_mut(&from_storage_slot) {
                    slot.mark_cold();
                }
                if let Some(slot) = token_acc.storage.get_mut(&to_storage_slot) {
                    slot.mark_cold();
                }
            }
        } else {
            // Transfer with evm call (from=caller, balance known from token registry).
            transfer_erc20_with_evm(
                evm,
                caller_addr,
                beneficiary,
                token_fee_info.token_address,
                token_amount_required,
                Some(token_fee_info.balance),
            )?;

            // Cache fee Transfer logs separately from the journal.
            //
            // go-ethereum's StateDB.logs is independent of the state snapshot/revert
            // mechanism — fee logs survive regardless of main tx result. revm's
            // ExecutionResult::Revert has no logs field, so we keep fee logs out of
            // the handler pipeline entirely and merge them in the receipt builder.
            evm.pre_fee_logs = std::mem::take(&mut evm.ctx_mut().journal_mut().logs);

            // State changes should be marked cold to avoid warm access in the main tx execution.
            // finalize() clears journal state (including logs, which we already took above).
            let mut state = evm.finalize();
            state.iter_mut().for_each(|(_, acc)| {
                acc.mark_cold();
                acc.storage.iter_mut().for_each(|(_, slot)| {
                    slot.mark_cold();
                });
            });
            evm.ctx_mut().journal_mut().state.extend(state);
        }

        // CREATE nonce is bumped later in make_create_frame
        if is_call {
            let mut caller = evm
                .ctx_mut()
                .journal_mut()
                .load_account_with_code_mut(caller_addr)?
                .data;
            caller.bump_nonce();
        }

        // Cache token fee info for the reimburse phase, ensuring consistent
        // price_ratio/scale between deduction and reimbursement.
        evm.cached_token_fee_info = Some(token_fee_info);
        evm.cached_l1_data_fee = l1_data_fee;

        Ok(())
    }
}

/// Execute `f` within a journal checkpoint. Commits on `Ok`, reverts on `Err`.
#[inline]
fn with_journal_checkpoint<DB, T, E>(
    journal: &mut revm::Journal<DB>,
    f: impl FnOnce(&mut revm::Journal<DB>) -> Result<T, E>,
) -> Result<T, E>
where
    DB: alloy_evm::Database,
{
    let checkpoint = journal.checkpoint();
    match f(journal) {
        Ok(val) => {
            journal.checkpoint_commit();
            Ok(val)
        }
        Err(err) => {
            journal.checkpoint_revert(checkpoint);
            Err(err)
        }
    }
}

/// Execute `f` within a journal checkpoint, saving and restoring `evm.tx`.
///
/// On `Ok` the checkpoint is committed; on `Err` it is reverted.
/// `evm.tx` is always restored to its original value regardless of the outcome,
/// so callers of [`evm_call`] inside `f` do not need to manage `evm.tx` themselves.
#[inline]
fn with_evm_checkpoint<DB, I, T>(
    evm: &mut MorphEvm<DB, I>,
    f: impl FnOnce(&mut MorphEvm<DB, I>) -> Result<T, EVMError<DB::Error, MorphInvalidTransaction>>,
) -> Result<T, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    let tx_origin = std::mem::take(&mut evm.tx);
    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
    let result = f(evm);
    evm.tx = tx_origin;
    match result {
        Ok(val) => {
            evm.ctx_mut().journal_mut().checkpoint_commit();
            Ok(val)
        }
        Err(err) => {
            evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
            Err(err)
        }
    }
}

/// Execute `f` within a journal snapshot that always reverts, saving and restoring `evm.tx`.
///
/// This gives `f` read-only (StaticCall-like) semantics: any state changes made by
/// [`evm_call`] inside `f` are discarded when `f` returns.
#[inline]
fn with_evm_snapshot<DB, I, T>(
    evm: &mut MorphEvm<DB, I>,
    f: impl FnOnce(&mut MorphEvm<DB, I>) -> T,
) -> T
where
    DB: alloy_evm::Database,
{
    let tx_origin = std::mem::take(&mut evm.tx);
    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
    let result = f(evm);
    evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
    evm.tx = tx_origin;
    result
}

/// Performs an ERC20 balance transfer by directly `sload`/`sstore`-ing the token contract storage
/// using the known `balance` mapping base slot, returning the computed storage slots for `from`/`to`.
#[inline]
fn transfer_erc20_with_slot<DB>(
    journal: &mut revm::Journal<DB>,
    from: Address,
    to: Address,
    token: Address,
    token_amount: U256,
    token_balance_slot: U256,
) -> Result<(U256, U256), EVMError<<DB>::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    with_journal_checkpoint(journal, |journal| {
        // Sub amount (checked: reject if insufficient, matching go-ethereum's
        // changeAltTokenBalanceByState which returns an error on underflow)
        let from_storage_slot = compute_mapping_slot_for_address(token_balance_slot, from);
        let from_balance = *journal.sload(token, from_storage_slot)?;
        let new_from_balance = from_balance.checked_sub(token_amount).ok_or(
            MorphInvalidTransaction::InsufficientTokenBalance {
                required: token_amount,
                available: from_balance,
            },
        )?;

        // Self-transfers are a no-op after the balance check above.
        let to_storage_slot = compute_mapping_slot_for_address(token_balance_slot, to);
        if from_storage_slot == to_storage_slot {
            return Ok((from_storage_slot, to_storage_slot));
        }

        // Add amount (checked: unlike go-ethereum's unbounded big.Int Add,
        // we reject on overflow to maintain token conservation)
        let to_balance = *journal.sload(token, to_storage_slot)?;
        let new_to_balance = to_balance.checked_add(token_amount).ok_or(
            MorphInvalidTransaction::TokenTransferFailed {
                reason: "recipient token balance overflow".into(),
            },
        )?;

        journal.sstore(token, from_storage_slot, new_from_balance)?;
        journal.sstore(token, to_storage_slot, new_to_balance)?;
        Ok((from_storage_slot, to_storage_slot))
    })
}

/// Gas limit for internal EVM calls (ERC20 transfer, balanceOf).
const EVM_CALL_GAS_LIMIT: u64 = 200_000;

/// Execute an internal EVM call, matching go-ethereum's `evm.Call()` semantics.
///
/// Unlike `system_call_one_with_caller`, this only runs the handler's `execution()`
/// phase — NOT `execution_result()`. This means:
/// - Logs emitted during the call (e.g., ERC20 Transfer events) remain in the journal
/// - State changes remain in the journal
///
/// **Caller is responsible for saving/restoring `evm.tx` if needed.**
fn evm_call<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    caller: Address,
    target: Address,
    calldata: Bytes,
) -> Result<revm::handler::FrameResult, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    evm.tx = MorphTxEnv {
        inner: revm::context::TxEnv {
            caller,
            kind: target.into(),
            data: calldata,
            gas_limit: EVM_CALL_GAS_LIMIT,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut h = MorphEvmHandler::<DB, I>::new();
    h.execution(evm, &InitialAndFloorGas::new(0, 0))
}

/// Query ERC20 `balanceOf(address)` via an internal EVM call.
///
/// Uses [`with_evm_snapshot`] to match go-ethereum's StaticCall semantics:
/// all state changes and `evm.tx` mutations are reverted after the call.
fn evm_call_balance_of<DB, I>(evm: &mut MorphEvm<DB, I>, token: Address, account: Address) -> U256
where
    DB: alloy_evm::Database,
{
    with_evm_snapshot(evm, |evm| {
        let calldata = encode_balance_of_calldata(account);
        match evm_call(evm, Address::ZERO, token, calldata) {
            Ok(ref result) if result.instruction_result().is_ok() => {
                let output = &result.interpreter_result().output;
                if output.len() >= 32 {
                    U256::from_be_slice(&output[..32])
                } else {
                    U256::ZERO
                }
            }
            _ => U256::ZERO,
        }
    })
}

/// Matches go-ethereum's `transferAltTokenByEVM` validation:
/// 1. Checks EVM call succeeded (no revert)
/// 2. Validates ABI-decoded bool return value (supports old tokens with no return data)
/// 3. Verifies sender balance changed by the expected amount
///
/// Uses [`evm_call`] instead of `system_call_one_with_caller` so that event logs
/// (e.g., ERC20 Transfer) naturally remain in the journal and appear in the
/// transaction receipt, matching go-ethereum's `evm.Call()` behavior.
///
/// `from_balance_before` is the sender's balance before the transfer. If `None`,
/// the balance is queried via EVM call (matching go-eth's nil `userBalanceBefore`).
fn transfer_erc20_with_evm<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    from: Address,
    to: Address,
    token_address: Address,
    token_amount: U256,
    from_balance_before: Option<U256>,
) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    // Read sender balance before transfer if not provided.
    // This uses with_evm_snapshot internally, so evm.tx is safe.
    let from_balance_before = match from_balance_before {
        Some(b) => b,
        None => evm_call_balance_of(evm, token_address, from),
    };

    with_evm_checkpoint(evm, |evm| {
        let calldata = build_transfer_calldata(to, token_amount);
        let frame_result = evm_call(evm, from, token_address, calldata).map_err(|e| {
            EVMError::Transaction(MorphInvalidTransaction::TokenTransferFailed {
                reason: format!("Error: {e:?}"),
            })
        })?;

        if !frame_result.instruction_result().is_ok() {
            return Err(MorphInvalidTransaction::TokenTransferFailed {
                reason: format!("{:?}", frame_result.interpreter_result()),
            }
            .into());
        }

        // Validate ABI bool return value, matching go-ethereum behavior:
        // - No return data: accepted (old tokens that don't return bool)
        // - 32+ bytes with last byte == 1: accepted (standard ERC20)
        // - Otherwise: rejected
        let output = &frame_result.interpreter_result().output;
        if !output.is_empty() && (output.len() < 32 || output[31] != 1) {
            return Err(MorphInvalidTransaction::TokenTransferFailed {
                reason: "alt token transfer returned failure".to_string(),
            }
            .into());
        }

        // Verify sender balance changed by the expected amount, matching go-ethereum.
        // evm_call_balance_of uses with_evm_snapshot, so evm.tx is safe here too.
        let from_balance_after = evm_call_balance_of(evm, token_address, from);

        // Verify sender balance decreased by exactly the transfer amount.
        // Matches go-ethereum's transferAltTokenByEVM which always checks this,
        // even for self-transfers (from == to), where it would fail because the
        // net balance change is zero but the expected decrease is `token_amount`.
        let expected_balance = from_balance_before.checked_sub(token_amount).ok_or(
            MorphInvalidTransaction::TokenTransferFailed {
                reason: format!(
                    "sender balance {from_balance_before} less than token amount {token_amount}"
                ),
            },
        )?;
        if from_balance_after != expected_balance {
            return Err(MorphInvalidTransaction::TokenTransferFailed {
                reason: format!(
                    "sender balance mismatch: expected {expected_balance}, got {from_balance_after}"
                ),
            }
            .into());
        }

        Ok(())
    })
}

/// Build the calldata for ERC20 `transfer(address,uint256)` call.
///
/// Method selector: `0xa9059cbb`
#[inline]
fn build_transfer_calldata(to: Address, token_amount: alloy_primitives::Uint<256, 4>) -> Bytes {
    let method_id = [0xa9u8, 0x05, 0x9c, 0xbb];
    // Encode calldata: method_id + padded to address + amount
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&method_id);
    let mut address_bytes = [0u8; 32];
    address_bytes[12..32].copy_from_slice(to.as_slice());
    calldata.extend_from_slice(&address_bytes);
    calldata.extend_from_slice(&token_amount.to_be_bytes::<32>());
    Bytes::from(calldata)
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
#[inline]
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
    let is_fee_charge_disabled = cfg.is_fee_charge_disabled();

    // Validate balance against max possible spending using max_fee_per_gas (not effective_gas_price).
    // go-eth's buyGas() checks: balance >= gasFeeCap * gas + value + l1DataFee.
    // This ensures the sender can afford the worst-case gas cost before deducting the actual cost.
    if !is_balance_check_disabled && !is_fee_charge_disabled {
        let max_gas_spending = U256::from(
            (tx.gas_limit() as u128)
                .checked_mul(tx.max_fee_per_gas())
                .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?,
        );
        let max_spending = max_gas_spending
            .checked_add(tx.value())
            .and_then(|v| v.checked_add(l1_data_fee))
            .ok_or(InvalidTransaction::OverflowPaymentInTransaction)?;
        if balance < max_spending {
            return Err(InvalidTransaction::LackOfFundForMaxFee {
                fee: Box::new(max_spending),
                balance: Box::new(balance),
            });
        }
    }

    // Deduct using effective_gas_price (not max_fee_per_gas).
    // go-eth's buyGas(): SubBalance(from, gasPrice * gas + l1DataFee)
    let effective_balance_spending = tx.effective_balance_spending(basefee, blob_price)?;
    let gas_balance_spending = effective_balance_spending - tx.value();
    let total_fee_deduction = gas_balance_spending.saturating_add(l1_data_fee);

    let mut new_balance = balance.saturating_sub(total_fee_deduction);

    if is_balance_check_disabled {
        // Make sure the caller's balance is at least the value of the transaction.
        new_balance = new_balance.max(tx.value());
    }

    Ok(new_balance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MorphBlockEnv;
    use alloy_primitives::{Bytes, address, keccak256};
    use morph_chainspec::hardfork::MorphHardfork;
    use revm::{
        context::BlockEnv,
        database::{CacheDB, EmptyDB},
        inspector::NoOpInspector,
        state::{AccountInfo, Bytecode},
    };

    fn mutating_return_code(write_value: u8, return_value: u8) -> Bytes {
        Bytes::from(vec![
            0x60,
            write_value, // PUSH1 write_value
            0x60,
            0x00, // PUSH1 slot 0
            0x55, // SSTORE
            0x60,
            return_value, // PUSH1 return_value
            0x60,
            0x00, // PUSH1 offset 0
            0x52, // MSTORE
            0x60,
            0x20, // PUSH1 size 32
            0x60,
            0x00, // PUSH1 offset 0
            0xf3, // RETURN
        ])
    }

    #[test]
    fn transfer_erc20_with_evm_reverts_state_on_validation_failure() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let original_balance = U256::from(50);
        let contract_code = mutating_return_code(1, 0);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            from,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        db.insert_account_info(
            token,
            AccountInfo {
                code_hash: keccak256(contract_code.as_ref()),
                code: Some(Bytecode::new_raw(contract_code)),
                ..Default::default()
            },
        );
        db.insert_account_storage(token, U256::ZERO, original_balance)
            .unwrap();

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
        };

        let err = transfer_erc20_with_evm(
            &mut evm,
            from,
            to,
            token,
            U256::from(4),
            Some(original_balance),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::TokenTransferFailed { .. })
        ));
        let slot_state = evm
            .ctx_ref()
            .journal()
            .state
            .get(&token)
            .and_then(|account| account.storage.get(&U256::ZERO))
            .unwrap();
        assert_eq!(slot_state.present_value, original_balance);
    }

    #[test]
    fn transfer_erc20_with_evm_reverts_state_on_expected_balance_underflow() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let original_balance = U256::from(50);
        let contract_code = mutating_return_code(1, 1);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            from,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        db.insert_account_info(
            token,
            AccountInfo {
                code_hash: keccak256(contract_code.as_ref()),
                code: Some(Bytecode::new_raw(contract_code)),
                ..Default::default()
            },
        );
        db.insert_account_storage(token, U256::ZERO, original_balance)
            .unwrap();

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
        };

        let err =
            transfer_erc20_with_evm(&mut evm, from, to, token, U256::from(1), Some(U256::ZERO))
                .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::TokenTransferFailed { .. })
        ));
        let slot_state = evm
            .ctx_ref()
            .journal()
            .state
            .get(&token)
            .and_then(|account| account.storage.get(&U256::ZERO))
            .unwrap();
        assert_eq!(slot_state.present_value, original_balance);
    }

    #[test]
    fn transfer_erc20_with_slot_reverts_sender_on_recipient_overflow() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let balance_slot = U256::from(7);
        let from_storage_slot = compute_mapping_slot_for_address(balance_slot, from);
        let to_storage_slot = compute_mapping_slot_for_address(balance_slot, to);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(token, AccountInfo::default());
        db.insert_account_storage(token, from_storage_slot, U256::from(10))
            .unwrap();
        db.insert_account_storage(token, to_storage_slot, U256::MAX)
            .unwrap();

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
        };

        let journal = evm.ctx_mut().journal_mut();
        let _ = journal.load_account_mut(token).unwrap();
        journal.touch(token);

        let err = transfer_erc20_with_slot(journal, from, to, token, U256::from(1), balance_slot)
            .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::TokenTransferFailed { .. })
        ));
        let from_balance_after = *evm
            .ctx_mut()
            .journal_mut()
            .sload(token, from_storage_slot)
            .unwrap();
        assert_eq!(from_balance_after, U256::from(10));
    }

    #[test]
    fn evm_call_balance_of_is_read_only() {
        let token = address!("3000000000000000000000000000000000000003");
        let account = address!("1000000000000000000000000000000000000001");
        let original_balance = U256::from(50);
        let contract_code = mutating_return_code(1, 42);

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            token,
            AccountInfo {
                code_hash: keccak256(contract_code.as_ref()),
                code: Some(Bytecode::new_raw(contract_code)),
                ..Default::default()
            },
        );
        db.insert_account_storage(token, U256::ZERO, original_balance)
            .unwrap();

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
        };

        let balance = evm_call_balance_of(&mut evm, token, account);

        assert_eq!(balance, U256::from(42));
        let slot_state = evm
            .ctx_ref()
            .journal()
            .state
            .get(&token)
            .and_then(|acct| acct.storage.get(&U256::ZERO))
            .unwrap();
        assert_eq!(slot_state.present_value, original_balance);
    }
}
