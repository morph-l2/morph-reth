//! Morph EVM Handler implementation.

use alloy_primitives::{Address, Bytes, U256};
use revm::{
    ExecuteEvm,
    context::{
        Cfg, ContextTr, JournalTr, Transaction,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    context_interface::{
        Block, cfg::gas_params::Eip2780TxInfo, journaled_state::account::JournaledAccountTr,
        result::ResultGas,
    },
    handler::{EvmTr, FrameTr, Handler, MainnetHandler, post_execution, pre_execution, validation},
    inspector::{Inspector, InspectorHandler},
    interpreter::{Gas, GasTracker, InitialAndFloorGas, interpreter::EthInterpreter},
};

use crate::{
    MorphEvm, MorphInvalidTransaction,
    error::MorphHaltReason,
    evm::MorphContext,
    l1block::L1BlockInfo,
    token_fee::{
        TokenFeeInfo, TokenRegistryEntry, compute_mapping_slot_for_address,
        encode_balance_of_calldata, read_balance_from_storage,
    },
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
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        MainnetHandler::default()
            .execution_result(evm, result, result_gas)
            .map(|result| result.map_haltreason(Into::into))
    }

    #[inline]
    fn apply_eip7702_auth_list(
        &self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &mut GasTracker,
    ) -> Result<Option<u64>, Self::Error> {
        pre_execution::apply_eip7702_auth_list(evm.ctx(), init_and_floor_gas)
    }

    #[inline]
    fn validate_against_state_and_deduct_caller(
        &self,
        evm: &mut Self::Evm,
        _init_and_floor_gas: &mut InitialAndFloorGas,
    ) -> Result<(), Self::Error> {
        // Reset per-transaction caches from the previous iteration.
        evm.cached_l1_data_fee = U256::ZERO;
        evm.pre_fee_refund = 0;
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

        // MorphTx (0x7F) can use token fee (fee_token_id > 0) or ETH fee (fee_token_id == 0).
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
            // fee_token_id == 0 follows standard ETH reimbursement flow
            post_execution::reimburse_caller(evm.ctx(), exec_result.gas(), U256::ZERO)?;
            return Ok(());
        }

        // Standard ETH-based fee handling
        post_execution::reimburse_caller(evm.ctx(), exec_result.gas(), U256::ZERO)?;
        Ok(())
    }

    #[inline]
    fn refund(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        eip7702_refund: i64,
    ) -> Result<(), Self::Error> {
        // L1 message tx follows go-ethereum semantics: no gas refunds.
        // Keep gas_used as actual consumed gas without applying post-exec refund.
        if evm.ctx_ref().tx().is_l1_msg() {
            // revm::Gas::used() subtracts `refunded` by default.
            // For L1 messages we must zero it out, otherwise gas_used is undercounted.
            exec_result.gas_mut().set_refund(0);
            return Ok(());
        }
        exec_result.gas_mut().record_refund(evm.pre_fee_refund);
        post_execution::refund(
            evm.ctx().cfg().gas_params(),
            exec_result.gas_mut(),
            eip7702_refund,
        );
        Ok(())
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

        // MorphTx maps to `TransactionType::Custom`, so revm's standard path
        // does not enforce EIP-1559 fee-cap rules. Those rules are independent
        // of which asset ultimately pays the fee, matching Morph geth's preCheck.
        if evm.ctx_ref().tx().is_morph_tx() && !evm.ctx_ref().cfg().is_fee_charge_disabled() {
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
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let tx = evm.ctx_ref().tx();
        let cfg = evm.ctx_ref().cfg();
        let spec = (*cfg.spec()).into();
        let disable_eip7623 = cfg.is_eip7623_disabled();
        let is_amsterdam_eip8037 = cfg.is_amsterdam_eip8037_enabled();
        let tx_gas_limit_cap = cfg.tx_gas_limit_cap();
        // Derive the EIP-2780 intrinsic-gas info the same way revm's own handler does
        // rather than hardcoding `None`. Every Morph hardfork maps below AMSTERDAM today
        // (asserted by `test_morph_hardforks_do_not_enable_amsterdam_state_gas`), so this
        // is `None` in practice — but a hardcoded `None` would silently diverge from
        // upstream intrinsic gas the moment that mapping changes.
        let eip2780 = cfg.is_amsterdam_eip2780_enabled().then(|| Eip2780TxInfo {
            value: tx.value(),
            // Self-transfer: a `Call` whose recipient is the sender itself.
            is_self_transfer: tx.kind().to() == Some(&tx.caller()),
        });

        // For L1 message transactions, handle intrinsic gas specially
        if tx.is_l1_msg() {
            // Calculate intrinsic gas (same as normal transactions). If intrinsic gas
            // > gas_limit, fall back to gas_limit (matching go-ethereum's behavior for
            // L1 messages, which prepay gas on L1 and must always execute).
            let initial_and_floor = validation::validate_initial_tx_gas_with_gas_params(
                tx,
                spec,
                cfg.gas_params(),
                disable_eip7623,
                is_amsterdam_eip8037,
                tx_gas_limit_cap,
                eip2780,
            )
            .unwrap_or_else(|_| InitialAndFloorGas::new(tx.gas_limit(), 0));

            return Ok(initial_and_floor);
        }

        // Normal transaction validation
        let initial_and_floor = validation::validate_initial_tx_gas_with_gas_params(
            tx,
            spec,
            cfg.gas_params(),
            disable_eip7623,
            is_amsterdam_eip8037,
            tx_gas_limit_cap,
            eip2780,
        )
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
        let hardfork = *evm.ctx_ref().cfg().spec();

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
            &caller.account().info,
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
        let token_fee_info = evm.cached_token_fee_info.ok_or_else(|| {
            MorphInvalidTransaction::TokenTransferFailed {
                reason: "cached_token_fee_info not set by validate_and_deduct_token_fee".into(),
            }
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
            result.map(|_| ())
        };

        if let Err(err) = refund_result {
            // A contract may reject a refund, but unavailable state is not a verdict
            // about the contract. Internal calls have already taken the context error,
            // so it must reach the executor here rather than disappearing at finalization.
            if matches!(err, EVMError::Database(_)) {
                return Err(err);
            }
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
                &caller.account().info,
                nonce,
                cfg.is_eip3607_disabled(),
                cfg.is_nonce_check_disabled(),
            )?;
        }

        let caller_addr = evm.ctx_ref().tx().caller();
        let is_call = evm.ctx_ref().tx().kind().is_call();
        let is_fee_charge_disabled = evm.ctx_ref().cfg().is_fee_charge_disabled();

        // Real transactions must cover the transferred ETH value before token metadata is loaded.
        // Simulations retain reth's existing value-check behavior and only add token eligibility.
        if !is_fee_charge_disabled {
            let tx_value = evm.ctx_ref().tx().value();
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
        }

        let token_registry_entry =
            TokenRegistryEntry::load(evm.ctx_mut().journal_mut().db_mut(), token_id)?
                .ok_or(MorphInvalidTransaction::TokenNotRegistered(token_id))?
                .ensure_usable(token_id)?;

        // Simulations validate token eligibility but skip balance lookup and fee deduction.
        if is_fee_charge_disabled {
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

        let hardfork = *evm.ctx_ref().cfg().spec();

        let token_fee_info = load_token_fee_info(evm, token_registry_entry, caller_addr)?;

        let beneficiary = evm.ctx_ref().block().beneficiary();
        let rlp_bytes = evm.ctx_ref().tx().rlp_bytes.clone().unwrap_or_default();
        let gas_limit = evm.ctx_ref().tx().gas_limit();
        let fee_limit_from_tx = evm.ctx_ref().tx().fee_limit.unwrap_or_default();
        let basefee = evm.ctx_ref().block().basefee() as u128;
        let effective_gas_price = evm.ctx_ref().tx().effective_gas_price(basefee);

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

        let fee_limit = token_fee_info.effective_fee_limit(fee_limit_from_tx);

        // Check if caller has sufficient token balance
        if fee_limit < token_amount_required {
            return Err(MorphInvalidTransaction::InsufficientTokenBalance {
                required: token_amount_required,
                available: fee_limit,
            }
            .into());
        }

        if token_amount_required.is_zero() {
            // Geth skips both transfer modes for a zero fee. Nonce and caches below
            // still need their normal per-transaction updates.
        } else if let Some(balance_slot) = token_fee_info.balance_slot {
            // Transfer with token slot.
            let journal = evm.ctx_mut().journal_mut();
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
            evm.pre_fee_refund = transfer_erc20_with_evm(
                evm,
                caller_addr,
                beneficiary,
                token_fee_info.token_address,
                token_amount_required,
                Some(token_fee_info.balance),
            )?;
        }

        if token_fee_info.balance_slot.is_none() {
            // balanceOf runs even for a zero fee. Geth Prepare clears its access
            // list/transient storage before the main transaction in that case too.
            // Cache fee Transfer logs separately from the journal.
            //
            // go-ethereum's StateDB.logs is independent of the state snapshot/revert
            // mechanism — fee logs survive regardless of main tx result. In revm they
            // would not: the `finalize()` below clears the journal's logs, and whatever
            // survived would still be dropped when `execution_result` commits the
            // transaction. So the fee logs are kept out of the handler pipeline entirely
            // and merged back in the receipt builder.
            evm.pre_fee_logs = std::mem::take(&mut evm.ctx_mut().journal_mut().logs);

            // State changes should be marked cold to avoid warm access in the main tx execution.
            // Fee deduction ran a real EVM frame, so its state writes must survive while the
            // frame's metadata must not: go-ethereum's `StateDB.Prepare` rebuilds the access
            // list and resets transient storage before the main transaction
            // (core/state/statedb.go:1066). `finalize()` is the nearest revm equivalent — it
            // commits the deduction's state and drops the journal, undo history, logs and
            // transient storage.
            //
            // `mark_cold` below only has to *drop* the warmth this frame's own CALL created; it
            // does not restore what `Prepare` would have left warm, and must not try to. That
            // warmth arrives later from upstream, which is why the two cannot be swapped:
            // `run_without_catch_error` runs this deduction inside `validate()`, then
            // `pre_execution()` → `pre_execution::load_accounts` re-warms the coinbase
            // (EIP-3651) and the transaction's access list, and the nonce bump just below
            // re-loads the caller. If a future change reorders those phases, a main frame that
            // reads `COINBASE` would be charged 2600 instead of go-ethereum's 100; nothing here
            // would catch it, because no fixture's main frame touches the coinbase.
            //
            // The `transaction_id` handling inside `finalize()` is load-bearing, not incidental.
            // Warming a slot goes through `EvmStorageSlot::mark_warm_with_transaction_id`, which
            // re-baselines the EIP-2200 `original_value` to the present value whenever the slot's
            // transaction id differs from the journal's (revm-state/src/lib.rs). That must not
            // happen to the slot the deduction just cleared: re-baselining it to zero would make
            // the main frame's SSTORE a *create* (SSTORE_SET, 20000) rather than a *recreate*
            // (100), and would drop the `SubRefund` that cancels the deduction frame's `+4800`.
            // Measured on `main_restores_cleared_slot`: 23_291 gas becomes 38_391 (+19_900
            // -4_800), and the state root moves with the fee it implies.
            //
            // It does not happen because ids stay equal throughout execution. revm advances the
            // id only when a transaction finishes — `commit_tx()` from `execution_result`, or
            // `discard_tx()` on the error path — both after the main frame is done;
            // `ExecuteEvm::finalize` then resets it to ZERO before the next transaction. So
            // across this deduction and the main frame the journal's id is 0 — and this
            // `finalize()` keeps it at 0 rather than advancing it. Swapping in `commit_tx()` here
            // would leave the deduction-warmed slots holding 0 while the journal held 1, and the
            // main frame's first touch of them would re-baseline `original_value`; the call-path
            // fixtures under `bin/morph-statetest` catch exactly that. An explicit `mark_cold`
            // carries no such risk: it drives only the warm/cold gas decision, never the
            // re-baseline.
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

/// Performs an ERC20 balance transfer by directly `sload`/`sstore`-ing the token contract storage
/// using the known `balance` mapping base slot, returning the computed storage slots for `from`/`to`.
///
/// The token account is loaded and touched here, ahead of the checkpoint, rather than by the
/// callers. The journal's `sload`/`sstore` panic instead of erroring when the account is absent
/// from `journal.state`, and touching keeps the token among the transaction's state changes even
/// for a self-transfer that writes no slot, as go-ethereum's `SetState` still marks it dirty.
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
    let _ = journal.load_account_mut(token)?;
    journal.touch(token);
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

/// Loads internal-call code without changing the account's access-list temperature.
/// Geth's direct Call/StaticCall resolve code without executing a CALL opcode.
///
/// This is also what puts `address` into `journal.state`, which [`evm_call`] depends on:
/// a `CallValue::Transfer` frame runs `Journal::transfer_loaded`, and its zero-value path
/// is `self.state.get_mut(&to).unwrap()` — a panic, not an error. In an ordinary CALL the
/// account is there because the opcode's `load_acc_and_calc_gas` put it there; an internal
/// call has no opcode, so this is the only load. Resolving the bytecode some other way
/// (caching it, hoisting it, short-circuiting on a known code hash) must keep the load.
fn internal_call_code<DB: alloy_evm::Database>(
    journal: &mut revm::Journal<DB>,
    address: Address,
) -> Result<(alloy_primitives::B256, revm::state::Bytecode), DB::Error> {
    let account = journal.load_account_with_code(address)?;
    let was_cold = account.is_cold;
    let code = (
        account.info.code_hash(),
        account.info.code.clone().unwrap_or_default(),
    );
    if was_cold {
        journal
            .state
            .get_mut(&address)
            .expect("account was loaded")
            .mark_cold();
    }
    Ok(code)
}

/// Executes a fee-token frame while retaining the outer transaction's ORIGIN/GASPRICE.
/// The frame owns its VM checkpoint; a successful call is not rolled back merely
/// because the token's return value or balance delta fails a later business check.
fn evm_call<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    caller: Address,
    target: Address,
    calldata: Bytes,
    is_static: bool,
) -> Result<revm::handler::FrameResult, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    use revm::context_interface::LocalContextTr;
    use revm::interpreter::interpreter_action::FrameInit;
    use revm::interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, FrameInput, SharedMemory,
    };

    // Frame execution reports database failures through ctx.error. Check both before
    // and after so a refund cannot overwrite an error from the main transaction.
    take_context_error(evm)?;
    let mut known_bytecode = internal_call_code(evm.ctx_mut().journal_mut(), target)?;
    if let Some(delegate) = known_bytecode.1.eip7702_address() {
        known_bytecode = internal_call_code(evm.ctx_mut().journal_mut(), delegate)?;
    }
    // Fee frames are top-level frames that run in the middle of a transaction, so
    // neither of revm's truncation points covers them: `free_child_context` only
    // releases a *child* frame's region, and `LocalContext::clear` only runs once the
    // whole transaction is done. Carve this frame's memory out above whatever the
    // shared buffer already holds and release it on the way out, so the main
    // transaction frame still starts on zeroed memory the way go-ethereum's
    // per-run `NewMemory()` guarantees. The frame keeps using the context's buffer
    // rather than one of its own because a nested call hands its callee a
    // `CallInput::SharedBuffer` range, and a precompile callee resolves that range
    // against the context's buffer (`CallInput::as_bytes`) rather than against the
    // calling frame's memory.
    let mut fee_frame_memory =
        SharedMemory::new_with_buffer(evm.ctx_ref().local().shared_memory_buffer().clone());
    let mut memory = fee_frame_memory.new_child_context();
    memory.set_memory_limit(evm.ctx_ref().cfg().memory_limit());
    let frame = FrameInit {
        depth: 0,
        memory,
        frame_input: FrameInput::Call(Box::new(CallInputs {
            input: CallInput::Bytes(calldata),
            return_memory_offset: 0..0,
            gas_limit: EVM_CALL_GAS_LIMIT,
            reservoir: 0,
            bytecode_address: target,
            known_bytecode,
            target_address: target,
            caller,
            // A zero transfer also performs geth StaticCall's legacy account touch.
            value: CallValue::Transfer(U256::ZERO),
            scheme: if is_static {
                CallScheme::StaticCall
            } else {
                CallScheme::Call
            },
            is_static,
            charged_new_account_state_gas: false,
        })),
    };
    let result = MorphEvmHandler::<DB, I>::new().run_exec_loop(evm, frame);
    fee_frame_memory.free_child_context();
    let result = result?;
    take_context_error(evm)?;
    Ok(result)
}

/// Moves a database failure recorded on the context into the return path.
#[inline]
fn take_context_error<DB, I>(
    evm: &mut MorphEvm<DB, I>,
) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    revm::context_interface::context::take_error::<
        EVMError<DB::Error, MorphInvalidTransaction>,
        DB::Error,
    >(&mut evm.ctx_mut().error)
}

/// Queries the token using a genuine static frame, as geth's StaticCall does.
/// Successful reads retain access-list warming; writes and malformed results fail.
pub(crate) fn evm_call_balance_of<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    token: Address,
    account: Address,
) -> Result<U256, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    let result = evm_call(
        evm,
        account,
        token,
        encode_balance_of_calldata(account),
        true,
    )?;
    let output = &result.interpreter_result().output;
    if !result.instruction_result().is_ok() || output.len() < 32 {
        return Err(MorphInvalidTransaction::TokenBalanceQueryFailed.into());
    }
    Ok(U256::from_be_slice(&output[..32]))
}

/// Resolves the caller's fee-token balance against the **executing** EVM.
///
/// go-ethereum reads it through `st.evm` (`GetAltTokenBalanceHybrid`, core/token_gas.go:43),
/// so the `balanceOf` call sees the real block context, the real chain config and the user as
/// `msg.sender`. Building a throwaway EVM here instead — as this path used to, through
/// `system_call_one` — answers under `BlockEnv::default()` and `CfgEnv::default()`: block 0,
/// timestamp 1, chain id 1, zero coinbase and base fee, with `SYSTEM_ADDRESS` as the sender.
/// For any token whose `balanceOf` reads that context the two clients would charge different
/// fees for the same transaction. The gas budget was never the problem: this crate's
/// `system_call_one` set the limit to its own `SYSTEM_CALL_GAS_LIMIT` — `exec.rs`, 200_000,
/// which deliberately shadows revm's `SYSTEM_CALL_GAS_LIMIT` of 30_000_000 at the
/// `SystemCallEvm` impl — and that 200k is go-ethereum's `maxGas`. [`EVM_CALL_GAS_LIMIT`]
/// carries the same number forward.
fn load_token_fee_info<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    entry: TokenRegistryEntry,
    caller: Address,
) -> Result<TokenFeeInfo, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    let balance = match entry.balance_slot() {
        // Slot mode is a plain storage read with no environment to get wrong. It goes
        // through the database rather than the journal deliberately: the journal is empty
        // at this point in the transaction, and an `sload` here would warm a slot that the
        // fee deduction below is careful to leave cold.
        Some(slot) => read_balance_from_storage(
            evm.ctx_mut().journal_mut().db_mut(),
            entry.token_address(),
            caller,
            slot,
        )?,
        None => evm_call_balance_of(evm, entry.token_address(), caller)?,
    };
    Ok(entry.into_fee_info(caller, balance))
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
/// Returns the signed refund counter for successful transfers; the deduction phase
/// carries it into transaction gas accounting, while reimbursement ignores it.
fn transfer_erc20_with_evm<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    from: Address,
    to: Address,
    token_address: Address,
    token_amount: U256,
    from_balance_before: Option<U256>,
) -> Result<i64, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: alloy_evm::Database,
{
    if token_amount.is_zero() {
        return Ok(0);
    }
    // Read sender balance before transfer if not provided.
    let from_balance_before = match from_balance_before {
        Some(b) => b,
        None => evm_call_balance_of(evm, token_address, from)?,
    };

    // Geth checks affordability before executing the token contract.
    let expected_balance = from_balance_before
        .checked_sub(token_amount)
        .ok_or_else(|| MorphInvalidTransaction::TokenTransferFailed {
            reason: format!(
                "sender balance {from_balance_before} less than token amount {token_amount}"
            ),
        })?;

    let calldata = build_transfer_calldata(to, token_amount);
    let frame_result =
        evm_call(evm, from, token_address, calldata, false).map_err(|e| match e {
            EVMError::Database(_) => e,
            _ => EVMError::Transaction(MorphInvalidTransaction::TokenTransferFailed {
                reason: format!("Error: {e:?}"),
            }),
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
    let from_balance_after = evm_call_balance_of(evm, token_address, from)?;

    // Verify sender balance decreased by exactly the transfer amount.
    // Matches go-ethereum's transferAltTokenByEVM which always checks this,
    // even for self-transfers (from == to), where it would fail because the
    // net balance change is zero but the expected decrease is `token_amount`.
    if from_balance_after != expected_balance {
        return Err(MorphInvalidTransaction::TokenTransferFailed {
            reason: format!(
                "sender balance mismatch: expected {expected_balance}, got {from_balance_after}"
            ),
        }
        .into());
    }

    Ok(frame_result.gas().refunded())
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
    // Simulation paths must not consume the caller balance.
    if cfg.is_fee_charge_disabled() {
        return Ok(balance);
    }

    let basefee = block.basefee() as u128;
    let blob_price = block.blob_gasprice().unwrap_or_default();
    let is_balance_check_disabled = cfg.is_balance_check_disabled();

    // Validate balance against max possible spending using max_fee_per_gas (not effective_gas_price).
    // go-eth's buyGas() checks: balance >= gasFeeCap * gas + value + l1DataFee.
    // This ensures the sender can afford the worst-case gas cost before deducting the actual cost.
    if !is_balance_check_disabled {
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
    use crate::MorphTxEnv;
    use crate::token_fee::tests::{TokenReadFailure, UnreadableTokenDb};
    use crate::{
        MorphBlockEnv,
        token_fee::{L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot},
    };
    use alloy_primitives::{B256, Bytes, TxKind, address, keccak256};
    use morph_chainspec::hardfork::MorphHardfork;
    use morph_primitives::MORPH_TX_TYPE_ID;
    use revm::{
        context::{BlockEnv, TxEnv},
        context_interface::{cfg::gas_params::GasId, result::InvalidTransaction},
        database::{CacheDB, EmptyDB},
        inspector::NoOpInspector,
        state::{AccountInfo, Bytecode},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    fn finish_transaction_with_refund(
        code: Bytes,
    ) -> Result<ExecutionResult<MorphHaltReason>, EVMError<TokenReadFailure, MorphInvalidTransaction>>
    {
        let caller = address!("1000000000000000000000000000000000000001");
        let beneficiary = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let target = address!("4000000000000000000000000000000000000004");
        let mut inner = CacheDB::new(EmptyDB::default());
        inner.insert_account_info(
            token,
            AccountInfo {
                code_hash: keccak256(code.as_ref()),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        let mut evm = MorphEvm::new(
            MorphContext::new(UnreadableTokenDb { inner, token }, MorphHardfork::Emerald),
            NoOpInspector,
        );
        evm.block.inner.beneficiary = beneficiary;
        // Produce a real successful main frame. The remainder of this probe enters the
        // normal reimbursement and result-finalization phases with an unused gas budget.
        let frame = evm_call(&mut evm, caller, target, Bytes::new(), false).unwrap();
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                caller,
                gas_price: 1,
                gas_limit: 30_000,
                kind: TxKind::Call(target),
                ..Default::default()
            },
            fee_token_id: Some(1),
            ..Default::default()
        };
        evm.cached_token_fee_info = Some(TokenFeeInfo {
            token_address: token,
            is_active: true,
            price_ratio: U256::from(1),
            scale: U256::from(1),
            caller,
            balance: U256::from(100_000),
            balance_slot: None,
            ..Default::default()
        });
        let mut handler = MorphEvmHandler::<_, NoOpInspector>::default();
        handler
            .reimburse_caller_token_fee(&mut evm, &Gas::new(1_000))
            .and_then(|_| handler.execution_result(&mut evm, frame, ResultGas::default()))
    }

    /// A readable token under [`UnreadableTokenDb`], so any failure reported below comes
    /// from the context, not from a read.
    fn readable_token_evm() -> MorphEvm<UnreadableTokenDb, NoOpInspector> {
        let token = address!("3000000000000000000000000000000000000003");
        let mut inner = CacheDB::new(EmptyDB::default());
        insert_contract(
            &mut inner,
            token,
            alloy_primitives::bytes!("6000545f5260205ff3"),
        );
        inner
            .insert_account_storage(token, U256::ZERO, U256::from(42))
            .unwrap();
        MorphEvm::new(
            MorphContext::new(
                UnreadableTokenDb {
                    inner,
                    // Nothing reads this address, so every storage read succeeds.
                    token: address!("9000000000000000000000000000000000000009"),
                },
                MorphHardfork::Emerald,
            ),
            NoOpInspector,
        )
    }

    #[test]
    fn a_nested_call_does_not_run_in_a_context_the_main_frame_already_poisoned() {
        let token = address!("3000000000000000000000000000000000000003");
        let account = address!("1000000000000000000000000000000000000001");
        let mut evm = readable_token_evm();
        assert_eq!(
            evm_call_balance_of(&mut evm, token, account).unwrap(),
            U256::from(42),
            "sanity: the token is readable"
        );

        // The main frame halted on a failed read; post-execution then reaches this call.
        evm.ctx_mut().error = Err(revm::context_interface::context::ContextError::Db(
            TokenReadFailure,
        ));
        let result = evm_call_balance_of(&mut evm, token, account);
        assert!(
            matches!(result, Err(EVMError::Database(TokenReadFailure))),
            "the main frame's failure must be reported, not overwritten by a nested call: {result:?}"
        );
        assert!(
            evm.ctx_ref().error.is_ok(),
            "the failure has been moved into the return path"
        );
    }

    #[test]
    fn a_refund_after_a_poisoned_main_frame_reports_the_main_frame_failure() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("3000000000000000000000000000000000000003");
        let mut evm = readable_token_evm();
        evm.block.inner.beneficiary = address!("2000000000000000000000000000000000000002");
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                caller,
                gas_price: 1,
                gas_limit: 30_000,
                ..Default::default()
            },
            fee_token_id: Some(1),
            ..Default::default()
        };
        evm.cached_token_fee_info = Some(TokenFeeInfo {
            token_address: token,
            is_active: true,
            price_ratio: U256::from(1),
            scale: U256::from(1),
            caller,
            balance: U256::from(100_000),
            balance_slot: None,
            ..Default::default()
        });
        evm.ctx_mut().error = Err(revm::context_interface::context::ContextError::Db(
            TokenReadFailure,
        ));

        let result = MorphEvmHandler::<_, NoOpInspector>::default()
            .reimburse_caller_token_fee(&mut evm, &Gas::new(1_000));
        assert!(
            matches!(result, Err(EVMError::Database(TokenReadFailure))),
            "{result:?}"
        );
    }

    #[test]
    fn refund_database_failure_aborts_final_execution_result() {
        let result = finish_transaction_with_refund(alloy_primitives::bytes!("6000545f5260205ff3"));
        assert!(
            matches!(result, Err(EVMError::Database(TokenReadFailure))),
            "refund I/O must abort execution, not finalize success without a refund: {result:?}"
        );
    }

    #[test]
    fn refund_contract_revert_still_allows_transaction_to_finish() {
        let result = finish_transaction_with_refund(alloy_primitives::bytes!("5f5ffd"));
        assert!(
            matches!(result, Ok(ExecutionResult::Success { .. })),
            "{result:?}"
        );
    }

    #[test]
    fn token_transfer_database_failure_is_not_transaction_invalidity() {
        let token = address!("3000000000000000000000000000000000000003");
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let mut inner = CacheDB::new(EmptyDB::default());
        insert_contract(
            &mut inner,
            token,
            alloy_primitives::bytes!("6000545f5260205ff3"),
        );
        let mut evm = MorphEvm::new(
            MorphContext::new(UnreadableTokenDb { inner, token }, MorphHardfork::Emerald),
            NoOpInspector,
        );
        let result = transfer_erc20_with_evm(
            &mut evm,
            from,
            to,
            token,
            U256::from(1),
            Some(U256::from(10)),
        );
        assert!(
            matches!(result, Err(EVMError::Database(TokenReadFailure))),
            "{result:?}"
        );
    }

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

    #[derive(Debug)]
    struct TokenAccessTrackingDb {
        inner: CacheDB<EmptyDB>,
        token: Address,
        token_accessed: Arc<AtomicBool>,
    }

    impl revm::Database for TokenAccessTrackingDb {
        type Error = std::convert::Infallible;

        fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            if address == self.token {
                self.token_accessed.store(true, Ordering::Relaxed);
            }
            revm::Database::basic(&mut self.inner, address)
        }

        fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
            revm::Database::code_by_hash(&mut self.inner, code_hash)
        }

        fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
            if address == self.token {
                self.token_accessed.store(true, Ordering::Relaxed);
            }
            revm::Database::storage(&mut self.inner, address, index)
        }

        fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
            revm::Database::block_hash(&mut self.inner, number)
        }
    }

    /// `<opcode> PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN` — a `balanceOf` that reports one piece
    /// of its environment instead of a balance, so a call made under the wrong environment
    /// shows up in the value that comes back.
    fn code_returning(opcode: u8) -> Bytes {
        Bytes::from(vec![opcode, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3])
    }

    fn insert_contract(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytes) {
        db.insert_account_info(
            address,
            AccountInfo {
                code_hash: keccak256(code.as_ref()),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
    }

    /// Loads token 1's registry entry and resolves `caller`'s balance against `evm`.
    fn probe_fee_token_balance(db: CacheDB<EmptyDB>, block: BlockEnv, caller: Address) -> U256 {
        let mut evm = MorphEvm::new(MorphContext::new(db, MorphHardfork::Emerald), NoOpInspector);
        evm.block = MorphBlockEnv { inner: block };

        let entry = TokenRegistryEntry::load(evm.ctx_mut().journal_mut().db_mut(), 1)
            .unwrap()
            .unwrap();
        load_token_fee_info(&mut evm, entry, caller)
            .unwrap()
            .balance
    }

    #[test]
    fn fee_token_balance_is_read_under_the_executing_block_environment() {
        const TIMESTAMP: u64 = 1_767_765_600;
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");

        let mut db = CacheDB::new(EmptyDB::default());
        insert_test_fee_token(&mut db, 1, token, true);
        insert_contract(&mut db, token, code_returning(0x42)); // TIMESTAMP

        let balance = probe_fee_token_balance(
            db,
            BlockEnv {
                timestamp: U256::from(TIMESTAMP),
                ..Default::default()
            },
            caller,
        );

        // `BlockEnv::default()` reports timestamp 1, which is what a throwaway EVM would
        // have answered with regardless of the block being executed.
        assert_eq!(balance, U256::from(TIMESTAMP));
    }

    #[test]
    fn fee_token_balance_query_names_the_queried_account_as_the_caller() {
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");

        let mut db = CacheDB::new(EmptyDB::default());
        insert_test_fee_token(&mut db, 1, token, true);
        insert_contract(&mut db, token, code_returning(0x33)); // CALLER

        let balance = probe_fee_token_balance(db, BlockEnv::default(), caller);

        // go-ethereum queries as the account being asked about, not as the zero address and
        // not as `SYSTEM_ADDRESS`.
        assert_eq!(balance, U256::from_be_bytes(caller.into_word().0));
    }

    fn insert_test_fee_token(
        db: &mut CacheDB<EmptyDB>,
        token_id: u16,
        token: Address,
        is_active: bool,
    ) {
        insert_test_fee_token_config(db, token_id, token, is_active, U256::from(1), U256::from(1));
    }

    fn insert_test_fee_token_config(
        db: &mut CacheDB<EmptyDB>,
        token_id: u16,
        token: Address,
        is_active: bool,
        price_ratio: U256,
        scale: U256,
    ) {
        let mut token_id_bytes = [0u8; 32];
        token_id_bytes[30..32].copy_from_slice(&token_id.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &token_id_bytes);

        db.insert_account_storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            base,
            U256::from_be_bytes(token.into_word().0),
        )
        .unwrap();

        let mut status = [0u8; 32];
        status[30] = 18;
        status[31] = u8::from(is_active);
        db.insert_account_storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            base + U256::from(2),
            U256::from_be_bytes(status),
        )
        .unwrap();
        db.insert_account_storage(L2_TOKEN_REGISTRY_ADDRESS, base + U256::from(3), scale)
            .unwrap();
        db.insert_account_storage(
            L2_TOKEN_REGISTRY_ADDRESS,
            compute_mapping_slot(U256::from(153), &token_id_bytes),
            price_ratio,
        )
        .unwrap();
    }

    fn token_fee_simulation_evm<DB>(
        db: DB,
        caller: Address,
        token_id: u16,
    ) -> MorphEvm<DB, NoOpInspector>
    where
        DB: alloy_evm::Database,
    {
        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.cfg.disable_fee_charge = true;
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                gas_limit: 21_000,
                caller,
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            fee_token_id: Some(token_id),
            ..Default::default()
        };
        evm
    }

    #[test]
    fn validate_env_rejects_token_fee_morph_tx_below_base_fee() {
        let mut evm = MorphEvm::new(
            MorphContext::new(CacheDB::new(EmptyDB::default()), MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv {
                basefee: 100,
                ..Default::default()
            },
        };
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                gas_limit: 21_000,
                gas_price: 99,
                gas_priority_fee: Some(1),
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            fee_token_id: Some(1),
            ..Default::default()
        };

        let err =
            <MorphEvmHandler<_, _> as Handler>::validate_env(&MorphEvmHandler::default(), &mut evm)
                .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::EthInvalidTransaction(
                InvalidTransaction::GasPriceLessThanBasefee
            ))
        ));
    }

    #[test]
    fn validate_initial_tx_gas_uses_configured_gas_params() {
        let mut evm = MorphEvm::new(
            MorphContext::new(CacheDB::new(EmptyDB::default()), MorphHardfork::default()),
            NoOpInspector,
        );
        let mut gas_params = evm.cfg.gas_params.clone();
        gas_params.override_gas([(GasId::tx_base_stipend(), 30_000)]);
        evm.cfg.set_gas_params(gas_params);
        evm.tx = MorphTxEnv {
            inner: TxEnv {
                gas_limit: 25_000,
                kind: TxKind::Call(Address::ZERO),
                ..Default::default()
            },
            ..Default::default()
        };

        let err = <MorphEvmHandler<_, _> as Handler>::validate_initial_tx_gas(
            &MorphEvmHandler::default(),
            &mut evm,
        )
        .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::EthInvalidTransaction(
                InvalidTransaction::CallGasCostMoreThanGasLimit {
                    initial_gas: 30_000,
                    gas_limit: 25_000,
                }
            ))
        ));
    }

    #[test]
    fn transfer_erc20_with_evm_keeps_state_on_post_call_validation_failure() {
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
        assert_eq!(slot_state.present_value, U256::from(1));
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
        assert!(evm.ctx_ref().journal().state.is_empty());
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

        // Nothing loads the token first: the helper has to put it in `journal.state` itself.
        let journal = evm.ctx_mut().journal_mut();
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

        assert!(evm_call_balance_of(&mut evm, token, account).is_err());
        assert_eq!(
            revm::Database::storage(evm.ctx_mut().journal_mut().db_mut(), token, U256::ZERO)
                .unwrap(),
            original_balance
        );
    }

    /// `disable_fee_charge` must leave the caller balance untouched.
    #[test]
    fn calculate_caller_fee_is_short_circuited_by_disable_fee_charge() {
        use revm::context::{CfgEnv, TxEnv};

        let balance = U256::from(1_000_000_000_000u128);
        let mut cfg = CfgEnv::<MorphHardfork>::default();
        cfg.disable_fee_charge = true;

        let tx = TxEnv {
            gas_limit: 21_000,
            gas_price: 1_000_000_000,
            value: U256::from(42u64),
            ..Default::default()
        };
        let block = BlockEnv::default();
        let l1_data_fee = U256::from(1_234u64);

        let new_balance =
            calculate_caller_fee_with_l1_cost(balance, tx, block, cfg, l1_data_fee).unwrap();
        assert_eq!(new_balance, balance);
    }

    #[test]
    fn validate_and_deduct_token_fee_rejects_unregistered_token_in_simulation() {
        let caller = address!("1000000000000000000000000000000000000001");
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        let mut evm = token_fee_simulation_evm(db, caller, 65535);

        let err = MorphEvmHandler::default()
            .validate_against_state_and_deduct_caller(&mut evm, &mut InitialAndFloorGas::default())
            .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::TokenNotRegistered(65535))
        ));
    }

    #[test]
    fn validate_and_deduct_token_fee_rejects_inactive_token_in_simulation() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;

        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_test_fee_token(&mut db, token_id, token, false);
        let mut evm = token_fee_simulation_evm(db, caller, token_id);

        let err = MorphEvmHandler::default()
            .validate_against_state_and_deduct_caller(&mut evm, &mut InitialAndFloorGas::default())
            .unwrap_err();

        assert!(matches!(
            err,
            EVMError::Transaction(MorphInvalidTransaction::TokenNotActive(42))
        ));
    }

    #[test]
    fn validate_and_deduct_token_fee_rejects_invalid_config_in_simulation() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;

        for (case, price_ratio, scale) in [
            ("zero price ratio", U256::ZERO, U256::from(1)),
            ("zero scale", U256::from(1), U256::ZERO),
        ] {
            let mut db = CacheDB::new(EmptyDB::default());
            db.insert_account_info(
                caller,
                AccountInfo {
                    balance: U256::from(1_000_000),
                    ..Default::default()
                },
            );
            insert_test_fee_token_config(&mut db, token_id, token, true, price_ratio, scale);
            let mut evm = token_fee_simulation_evm(db, caller, token_id);

            let err = MorphEvmHandler::default()
                .validate_against_state_and_deduct_caller(
                    &mut evm,
                    &mut InitialAndFloorGas::default(),
                )
                .unwrap_err();

            assert!(
                matches!(
                    err,
                    EVMError::Transaction(MorphInvalidTransaction::InvalidTokenConfig(42))
                ),
                "{case} must be rejected"
            );
        }
    }

    #[test]
    fn token_fee_simulation_does_not_load_caller_token_balance() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;
        let token_accessed = Arc::new(AtomicBool::new(false));
        let mut inner = CacheDB::new(EmptyDB::default());
        inner.insert_account_info(
            caller,
            AccountInfo {
                balance: U256::from(1_000_000),
                ..Default::default()
            },
        );
        insert_test_fee_token(&mut inner, token_id, token, true);

        let db = TokenAccessTrackingDb {
            inner,
            token,
            token_accessed: Arc::clone(&token_accessed),
        };
        let mut evm = token_fee_simulation_evm(db, caller, token_id);

        MorphEvmHandler::default()
            .validate_against_state_and_deduct_caller(&mut evm, &mut InitialAndFloorGas::default())
            .unwrap();

        assert!(
            !token_accessed.load(Ordering::Relaxed),
            "simulation must not query the fee-token contract or its balance storage"
        );
    }
    const FEE_REFUND_TOKEN_ID: u16 = 1;
    const FEE_REFUND_GAS_LIMIT: u64 = 100_000;
    const FEE_REFUND_GAS_PRICE: u128 = 10;
    /// `gas_limit * effective_gas_price` (the L1 data fee is zero with an empty
    /// gas-price oracle), converted at scale 1 / price_ratio 1.
    const FEE_REFUND_TOKEN_FEE: u64 = 1_000_000;
    const FEE_REFUND_CALLER: Address = address!("1000000000000000000000000000000000000001");
    const FEE_REFUND_TOKEN: Address = address!("3000000000000000000000000000000000000003");
    const FEE_REFUND_BENEFICIARY: Address = address!("530000000000000000000000000000000000000a");
    /// Plain EOA target: the main frame does nothing beyond intrinsic gas.
    const FEE_REFUND_TARGET: Address = address!("4200000000000000000000000000000000000042");

    /// Minimal call-mode ERC20: `balance[addr]` lives at slot `uint256(addr)` (no
    /// keccak), so `CALLER`, `calldataload(4)` and `balanceOf`'s argument all name the
    /// same slot. Dispatch is on `CALLDATASIZE`:
    /// - 68 bytes => `transfer(address,uint256)`: SSTORE(caller, SLOAD(caller) - amount),
    ///   SSTORE(to, SLOAD(to) + amount), return `true`.
    /// - anything else => `balanceOf(address)`: return SLOAD(calldataload(4)).
    fn fee_refund_slotless_erc20_code() -> Bytes {
        Bytes::from(vec![
            0x36, // CALLDATASIZE
            0x60, 0x44, // PUSH1 68
            0x14, // EQ
            0x60, 0x13, // PUSH1 19 (transfer JUMPDEST)
            0x57, // JUMPI
            // balanceOf(address)
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD
            0x54, // SLOAD
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
            // transfer(address,uint256)
            0x5b, // JUMPDEST (pc 19)
            0x60, 0x24, // PUSH1 36
            0x35, // CALLDATALOAD  -> amount
            0x80, // DUP1          -> amount amount
            0x33, // CALLER        -> caller amount amount
            0x54, // SLOAD         -> bal_from amount amount
            0x03, // SUB           -> bal_from-amount amount
            0x33, // CALLER        -> caller new_from amount
            0x55, // SSTORE        -> amount
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD  -> to amount
            0x80, // DUP1          -> to to amount
            0x54, // SLOAD         -> bal_to to amount
            0x82, // DUP3          -> amount bal_to to amount
            0x01, // ADD           -> new_to to amount
            0x90, // SWAP1         -> to new_to amount
            0x55, // SSTORE        -> amount
            0x50, // POP
            0x60, 0x01, // PUSH1 1
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
        ])
    }

    fn fee_refund_balance_slot(account: Address) -> U256 {
        U256::from_be_bytes(account.into_word().0)
    }

    fn fee_refund_evm(payer_token_balance: U256) -> MorphEvm<CacheDB<EmptyDB>, NoOpInspector> {
        let code = fee_refund_slotless_erc20_code();
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_info(FEE_REFUND_CALLER, AccountInfo::default());
        db.insert_account_info(
            FEE_REFUND_TOKEN,
            AccountInfo {
                code_hash: keccak256(code.as_ref()),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        db.insert_account_storage(
            FEE_REFUND_TOKEN,
            fee_refund_balance_slot(FEE_REFUND_CALLER),
            payer_token_balance,
        )
        .unwrap();
        // `balanceSlot` word left at zero => call mode (`balance_slot == None`).
        insert_test_fee_token(&mut db, FEE_REFUND_TOKEN_ID, FEE_REFUND_TOKEN, true);

        let mut evm = MorphEvm::new(
            MorphContext::new(db, MorphHardfork::default()),
            NoOpInspector,
        );
        evm.block = MorphBlockEnv {
            inner: BlockEnv {
                basefee: 1,
                beneficiary: FEE_REFUND_BENEFICIARY,
                gas_limit: 30_000_000,
                ..Default::default()
            },
        };
        // Production Morph configuration disables the Ethereum calldata gas floor.
        evm.cfg.disable_eip7623 = true;
        evm
    }

    fn fee_refund_present_value(
        evm: &MorphEvm<CacheDB<EmptyDB>, NoOpInspector>,
        account: Address,
    ) -> U256 {
        evm.ctx_ref()
            .journal()
            .state
            .get(&FEE_REFUND_TOKEN)
            .and_then(|acct| acct.storage.get(&fee_refund_balance_slot(account)))
            .map(|slot| slot.present_value)
            .expect("fee-token slot must be in the journal")
    }

    /// Runs one call-mode token-fee MorphTx (plain call to an EOA) and returns
    /// `(gas_used, final refund applied to the main frame, payer token balance after
    /// reimbursement)`.
    fn fee_refund_run_token_fee_tx(payer_token_balance: U256) -> (u64, u64, U256) {
        let mut evm = fee_refund_evm(payer_token_balance);
        let tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: MORPH_TX_TYPE_ID,
                caller: FEE_REFUND_CALLER,
                gas_limit: FEE_REFUND_GAS_LIMIT,
                gas_price: FEE_REFUND_GAS_PRICE,
                kind: TxKind::Call(FEE_REFUND_TARGET),
                ..Default::default()
            },
            fee_token_id: Some(FEE_REFUND_TOKEN_ID),
            ..Default::default()
        };

        let result = evm
            .transact_one(tx)
            .expect("token-fee MorphTx must execute");
        assert!(result.is_success(), "expected success, got {result:?}");
        let gas_used = result.tx_gas_used();
        let gas_refunded = result.gas().final_refunded();

        // Sanity: the fee was charged in call mode and equals exactly FEE_REFUND_TOKEN_FEE.
        let info = evm
            .cached_token_fee_info()
            .expect("token fee info is cached");
        assert_eq!(
            info.balance_slot, None,
            "token must be registered in call mode"
        );
        assert_eq!(
            info.balance, payer_token_balance,
            "balanceOf must see the seeded balance"
        );
        assert_eq!(
            info.eth_to_token_amount(U256::from(
                FEE_REFUND_GAS_LIMIT as u128 * FEE_REFUND_GAS_PRICE
            )),
            U256::from(FEE_REFUND_TOKEN_FEE)
        );

        (
            gas_used,
            gas_refunded,
            fee_refund_present_value(&evm, FEE_REFUND_CALLER),
        )
    }

    #[test]
    fn deduction_sstore_refund_reaches_transaction_gas() {
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let (gas, refund, balance) = fee_refund_run_token_fee_tx(fee);
        assert_eq!((gas, refund), (16_800, 4_200));
        assert_eq!(balance, fee - U256::from(168_000));
        let (gas, refund, balance) = fee_refund_run_token_fee_tx(fee + U256::from(1));
        assert_eq!((gas, refund), (21_000, 0));
        assert_eq!(balance, fee + U256::from(1) - U256::from(210_000));
    }

    #[test]
    fn balance_queries_reject_state_writes() {
        let mut evm = fee_refund_evm(U256::from(FEE_REFUND_TOKEN_FEE));
        let code = mutating_return_code(1, 1);
        evm.ctx_mut().journal_mut().db_mut().insert_account_info(
            FEE_REFUND_TOKEN,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        assert!(evm_call_balance_of(&mut evm, FEE_REFUND_TOKEN, FEE_REFUND_CALLER).is_err());
    }

    #[test]
    fn internal_calls_preserve_origin_and_effective_gas_price() {
        for (opcode, expected) in [
            (0x32, U256::from_be_slice(FEE_REFUND_CALLER.as_slice())),
            (0x3a, U256::from(3)),
        ] {
            let mut evm = fee_refund_evm(U256::from(FEE_REFUND_TOKEN_FEE));
            evm.tx.inner.caller = FEE_REFUND_CALLER;
            evm.tx.inner.tx_type = MORPH_TX_TYPE_ID;
            evm.tx.inner.gas_price = 10;
            evm.tx.inner.gas_priority_fee = Some(2);
            let code = Bytes::from(vec![opcode, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
            evm.ctx_mut().journal_mut().db_mut().insert_account_info(
                FEE_REFUND_TOKEN,
                AccountInfo {
                    code_hash: keccak256(&code),
                    code: Some(Bytecode::new_raw(code)),
                    ..Default::default()
                },
            );
            assert_eq!(
                evm_call_balance_of(&mut evm, FEE_REFUND_TOKEN, FEE_REFUND_BENEFICIARY).unwrap(),
                expected
            );
            assert_eq!(evm.tx.inner.caller, FEE_REFUND_CALLER);
        }
    }

    #[test]
    fn zero_token_transfer_does_not_call_the_contract() {
        let mut evm = fee_refund_evm(U256::ZERO);
        let code = mutating_return_code(1, 0);
        evm.ctx_mut().journal_mut().db_mut().insert_account_info(
            FEE_REFUND_TOKEN,
            AccountInfo {
                code_hash: keccak256(&code),
                code: Some(Bytecode::new_raw(code)),
                ..Default::default()
            },
        );
        transfer_erc20_with_evm(
            &mut evm,
            FEE_REFUND_CALLER,
            FEE_REFUND_BENEFICIARY,
            FEE_REFUND_TOKEN,
            U256::ZERO,
            Some(U256::ZERO),
        )
        .unwrap();
        assert!(evm.ctx_ref().journal().state.is_empty());
    }

    /// Runs one transaction against `code` deployed at [`FEE_REFUND_TARGET`] and
    /// returns what the main frame returned. `fee_token_id` selects the call-mode
    /// token-fee path or the ordinary ETH-fee path.
    fn fee_refund_run_probe(code: Bytes, fee_token_id: Option<u16>) -> Bytes {
        let mut evm = fee_refund_evm(U256::from(FEE_REFUND_TOKEN_FEE));
        let db = evm.ctx_mut().journal_mut().db_mut();
        insert_contract(db, FEE_REFUND_TARGET, code);
        db.insert_account_info(
            FEE_REFUND_CALLER,
            AccountInfo {
                balance: U256::from(FEE_REFUND_GAS_LIMIT as u128 * FEE_REFUND_GAS_PRICE),
                ..Default::default()
            },
        );
        let tx = MorphTxEnv {
            inner: TxEnv {
                tx_type: if fee_token_id.is_some() {
                    MORPH_TX_TYPE_ID
                } else {
                    0
                },
                caller: FEE_REFUND_CALLER,
                gas_limit: FEE_REFUND_GAS_LIMIT,
                gas_price: FEE_REFUND_GAS_PRICE,
                kind: TxKind::Call(FEE_REFUND_TARGET),
                ..Default::default()
            },
            fee_token_id,
            ..Default::default()
        };
        let result = evm.transact_one(tx).expect("probe must execute");
        assert!(result.is_success(), "expected success, got {result:?}");
        result.output().cloned().unwrap_or_default()
    }

    /// Minimal proxy: copies its calldata into memory and `DELEGATECALL`s
    /// `implementation`, the way mainnet's call-mode fee tokens reach theirs.
    fn delegating_proxy_code(implementation: Address) -> Bytes {
        let mut code = vec![
            0x36, // CALLDATASIZE      (size)
            0x5f, // PUSH0             (offset)
            0x5f, // PUSH0             (destOffset)
            0x37, // CALLDATACOPY
            0x5f, // PUSH0             (retSize)
            0x5f, // PUSH0             (retOffset)
            0x36, // CALLDATASIZE      (argsSize)
            0x5f, // PUSH0             (argsOffset)
            0x73, // PUSH20 implementation
        ];
        code.extend_from_slice(implementation.as_slice());
        code.extend_from_slice(&[
            0x5a, // GAS
            0xf4, // DELEGATECALL
            0x3d, // RETURNDATASIZE    (size)
            0x5f, // PUSH0             (offset)
            0x5f, // PUSH0             (destOffset)
            0x3e, // RETURNDATACOPY
            0x50, // POP               (DELEGATECALL success flag)
            0x3d, // RETURNDATASIZE    (size)
            0x5f, // PUSH0             (offset)
            0xf3, // RETURN
        ]);
        Bytes::from(code)
    }

    /// Same ERC20, except `balanceOf` returns its result through the identity
    /// precompile, reading the precompile's arguments out of memory.
    fn fee_refund_precompile_erc20_code() -> Bytes {
        let mut code = vec![
            0x36, // CALLDATASIZE
            0x60, 0x44, // PUSH1 68
            0x14, // EQ
            0x60, 0x1e, // PUSH1 30 (transfer JUMPDEST)
            0x57, // JUMPI
            // balanceOf(address), answered by identity(mem[0..32])
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD
            0x54, // SLOAD
            0x5f, // PUSH0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32   (retSize)
            0x60, 0x20, // PUSH1 32   (retOffset)
            0x60, 0x20, // PUSH1 32   (argsSize)
            0x5f, // PUSH0            (argsOffset)
            0x60, 0x04, // PUSH1 4    (identity precompile)
            0x5a, // GAS
            0xfa, // STATICCALL
            0x50, // POP
            0x60, 0x20, // PUSH1 32   (size)
            0x60, 0x20, // PUSH1 32   (offset)
            0xf3, // RETURN
        ];
        assert_eq!(code.len(), 30, "the transfer JUMPDEST moved");
        code.extend_from_slice(&fee_refund_slotless_erc20_code()[19..]);
        Bytes::from(code)
    }

    /// A precompile called from inside a fee frame resolves its arguments against
    /// the context's shared buffer, so the fee frames have to keep writing into that
    /// buffer rather than into one of their own.
    #[test]
    fn fee_token_frames_reach_a_precompile_through_memory() {
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let mut evm = fee_refund_evm(fee);
        let db = evm.ctx_mut().journal_mut().db_mut();
        insert_contract(db, FEE_REFUND_TOKEN, fee_refund_precompile_erc20_code());
        assert_eq!(
            fee_refund_run_fee_tx(&mut evm),
            fee_refund_run_token_fee_tx(fee)
        );
    }

    /// Mainnet's call-mode fee tokens are proxies, so a fee frame's nested
    /// `DELEGATECALL` reads its calldata back out of the frame's memory.
    #[test]
    fn fee_token_frames_reach_a_delegating_proxy_implementation() {
        const IMPLEMENTATION: Address = address!("3000000000000000000000000000000000000004");
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let mut evm = fee_refund_evm(fee);
        let db = evm.ctx_mut().journal_mut().db_mut();
        insert_contract(db, IMPLEMENTATION, fee_refund_slotless_erc20_code());
        insert_contract(db, FEE_REFUND_TOKEN, delegating_proxy_code(IMPLEMENTATION));

        // Indistinguishable from the same transaction against an unproxied token:
        // the implementation saw exactly the calldata the fee frame wrote.
        assert_eq!(
            fee_refund_run_fee_tx(&mut evm),
            fee_refund_run_token_fee_tx(fee)
        );
    }

    /// Runs the standard call-mode token-fee MorphTx on an already-built `evm` and
    /// reports it the way [`fee_refund_run_token_fee_tx`] does.
    fn fee_refund_run_fee_tx(
        evm: &mut MorphEvm<CacheDB<EmptyDB>, NoOpInspector>,
    ) -> (u64, u64, U256) {
        let result = evm
            .transact_one(MorphTxEnv {
                inner: TxEnv {
                    tx_type: MORPH_TX_TYPE_ID,
                    caller: FEE_REFUND_CALLER,
                    gas_limit: FEE_REFUND_GAS_LIMIT,
                    gas_price: FEE_REFUND_GAS_PRICE,
                    kind: TxKind::Call(FEE_REFUND_TARGET),
                    ..Default::default()
                },
                fee_token_id: Some(FEE_REFUND_TOKEN_ID),
                ..Default::default()
            })
            .expect("token-fee MorphTx must execute");
        assert!(result.is_success(), "expected success, got {result:?}");
        (
            result.tx_gas_used(),
            result.gas().final_refunded(),
            fee_refund_present_value(evm, FEE_REFUND_CALLER),
        )
    }

    /// go-ethereum allocates a fresh `Memory` for every interpreter run
    /// (`core/vm/interpreter.go`), so a transaction's frame always starts on zeroed
    /// memory. The fee-token frames run on the transaction's shared memory buffer,
    /// so they have to hand it back the length they found it at.
    #[test]
    fn fee_token_frames_do_not_leak_memory_into_the_main_frame() {
        // `MSIZE` and `MLOAD(0)`, each returned as the frame's 32-byte output.
        for probe in [
            vec![0x59, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3],
            vec![0x5f, 0x51, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3],
        ] {
            let code = Bytes::from(probe);
            assert_eq!(
                U256::from_be_slice(&fee_refund_run_probe(code.clone(), None)),
                U256::ZERO,
                "ETH-fee control"
            );
            assert_eq!(
                U256::from_be_slice(&fee_refund_run_probe(code, Some(FEE_REFUND_TOKEN_ID))),
                U256::ZERO,
                "a token-fee main frame must start on zeroed memory too"
            );
        }
    }
}
