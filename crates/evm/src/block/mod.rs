//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions

mod receipt;

pub(crate) use receipt::{
    DefaultMorphReceiptBuilder, MorphReceiptBuilder, MorphReceiptBuilderCtx, MorphReceiptTxFields,
};

use crate::evm::MorphEvm;
use alloy_consensus::{Transaction, transaction::TxHashRef};
use alloy_eip7928::{BlockAccessIndex, BlockAccessList};
use alloy_primitives::{Address, B256};
use evm2::{TxResultWithState, evm::Bal, registry::HandlerError};
use morph_primitives::{MorphReceipt, MorphTxEnvelope, MorphTxType};
use morph_revm::{
    L1_GAS_PRICE_ORACLE_ADDRESS, MorphEvmTypes, MorphInvalidTransaction, TokenFeeInfo,
};
use reth_evm::{
    BlockExecutionError, BlockExecutionOutput, BlockExecutor, BlockTransactionResult,
    BlockValidationError, ExecutorTx, GasOutput, InternalBlockExecutionError, InvalidTxError,
    RecoveredTx, execute::map_database_error,
};
use reth_execution_types::{BlockExecutionResult, BlockState, StateUpdate};
use std::{any::Any, sync::Arc};

/// A transaction validation failure reported by the Morph EVM.
#[derive(Debug, Clone, PartialEq, Eq)]
struct MorphInvalidTxError(HandlerError);

impl core::fmt::Display for MorphInvalidTxError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl core::error::Error for MorphInvalidTxError {}

impl InvalidTxError for MorphInvalidTxError {
    fn is_nonce_too_low(&self) -> bool {
        matches!(self.0, HandlerError::InvalidNonce { expected, got } if got < expected)
    }

    fn is_gas_limit_too_high(&self) -> bool {
        matches!(
            self.0,
            HandlerError::GasLimitMoreThanBlock { .. }
                | HandlerError::TxGasLimitGreaterThanCap { .. }
        )
    }

    fn is_gas_limit_too_low(&self) -> bool {
        matches!(self.0, HandlerError::IntrinsicGasTooLow { .. })
    }

    /// Exposes the wrapped [`HandlerError`] so RPC layers can recover the structured error.
    fn as_any(&self) -> &(dyn Any + 'static) {
        &self.0
    }
}

/// Maps a handler error for transaction `hash` onto reth's block execution errors.
///
/// Rejections of the transaction, Morph's own included, are block validation errors. A failed
/// state read is not a verdict on the transaction, so it stays an internal error unless the
/// database classifies it as invalid input.
fn map_transaction_error(error: HandlerError, hash: B256) -> BlockExecutionError {
    match error {
        HandlerError::Database(ref database_error) if !database_error.is_fatal() => {
            BlockValidationError::EVM {
                hash,
                error: Box::new(error),
            }
            .into()
        }
        HandlerError::Database(_)
        | HandlerError::Fatal(_)
        | HandlerError::WrongTransactionType { .. } => InternalBlockExecutionError::EVM {
            hash,
            error: Box::new(error),
        }
        .into(),
        error => BlockValidationError::InvalidTx {
            hash,
            error: Box::new(MorphInvalidTxError(error)),
        }
        .into(),
    }
}

/// Returns the Morph error carried by a block execution error, if there is one.
pub fn morph_invalid_tx_error(error: &BlockExecutionError) -> Option<&MorphInvalidTransaction> {
    let BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) = error
    else {
        return None;
    };
    error
        .as_any()
        .downcast_ref::<HandlerError>()?
        .external_ref::<MorphInvalidTransaction>()
}

/// The fields a receipt needs from the transaction itself.
#[derive(Debug, Clone)]
struct TxReceiptInput {
    tx_type: MorphTxType,
    is_l1_msg: bool,
    gas_limit: u64,
    sender: Address,
    morph_tx_fields: Option<MorphReceiptTxFields>,
}

/// The result of executing a Morph transaction.
///
/// Carries the EVM result together with the transaction data needed to build its receipt
/// during [`MorphBlockExecutor::commit_transaction`].
#[derive(Debug)]
pub struct MorphTxResult {
    /// The raw EVM execution result and state diff.
    result: TxResultWithState<MorphEvmTypes>,
    input: TxReceiptInput,
}

impl BlockTransactionResult<MorphEvmTypes> for MorphTxResult {
    fn result(&self) -> &TxResultWithState<MorphEvmTypes> {
        &self.result
    }
}

/// Block executor for Morph L2 blocks.
///
/// This executor handles Morph-specific block execution logic, differing from
/// standard Ethereum execution in several key ways:
///
/// ## L1 Fee Calculation
/// All L2 transactions (except L1 messages) must pay an L1 data fee for posting
/// transaction data to L1. This fee is calculated based on:
/// - The RLP-encoded transaction size
/// - Current L1 gas price from the L1 Gas Price Oracle contract
/// - Hardfork-specific fee calculation logic (pre-Curie vs post-Curie)
///
/// ## Token Fee Support (MorphTx 0x7F)
/// MorphTx transactions allow users to pay gas fees using ERC20 tokens.
/// The executor extracts token fee information from the L2TokenRegistry contract,
/// including exchange rate and scale factor.
///
/// ## Execution Flow
/// 1. `apply_pre_execution_changes`: Set up state and load contracts
/// 2. `execute_transaction_without_commit`: Execute transaction in EVM
/// 3. `commit_transaction`: Build the receipt, commit state
/// 4. `finish`: Return final execution result with all receipts
#[expect(missing_debug_implementations)]
pub struct MorphBlockExecutor<'a> {
    /// The EVM used by executor (owned, not a reference)
    evm: MorphEvm<'a>,
    /// Receipt builder
    receipt_builder: DefaultMorphReceiptBuilder,
    /// Receipts of executed transactions
    receipts: Vec<MorphReceipt>,
    /// Total gas used by executed transactions
    gas_used: u64,
    /// Gas unavailable to later transactions in this block.
    ///
    /// Unlike receipt gas, L1 messages reserve their full gas limit because Morph geth does not
    /// return their unused gas to the block gas pool.
    gas_pool_used: u64,
    /// Accumulated state changes of the block.
    block_state: BlockState,
    state_update_hook: Option<Box<dyn FnMut(StateUpdate) + Send>>,
}

impl<'a> MorphBlockExecutor<'a> {
    /// Creates a new [`MorphBlockExecutor`].
    ///
    /// # Arguments
    /// * `evm` - The EVM instance configured for Morph execution
    /// * `receipt_builder` - Builder for constructing transaction receipts
    /// * `tx_count_hint` - Expected number of transactions, when known
    pub(crate) fn new(
        evm: MorphEvm<'a>,
        receipt_builder: DefaultMorphReceiptBuilder,
        tx_count_hint: Option<usize>,
    ) -> Self {
        Self {
            evm,
            receipt_builder,
            receipts: Vec::with_capacity(tx_count_hint.unwrap_or_default()),
            gas_used: 0,
            gas_pool_used: 0,
            block_state: BlockState::new(),
            state_update_hook: None,
        }
    }

    /// Extract MorphTx-specific fields for MorphTx (0x7F) transactions.
    ///
    /// MorphTx transactions include:
    /// - Token fee information (when using ERC20 for gas payment)
    /// - Transaction metadata (version, reference, memo)
    fn morph_tx_fields(
        tx: &MorphTxEnvelope,
    ) -> Result<Option<MorphReceiptTxFields>, BlockExecutionError> {
        if !tx.is_morph_tx() {
            return Ok(None);
        }

        let fee_token_id = tx
            .fee_token_id()
            .ok_or_else(|| BlockExecutionError::msg("MorphTx missing fee_token_id"))?;
        let fee_limit = tx
            .fee_limit()
            .ok_or_else(|| BlockExecutionError::msg("MorphTx missing fee_limit"))?;

        Ok(Some(MorphReceiptTxFields {
            version: tx.version().unwrap_or(0),
            fee_token_id,
            fee_rate: None,
            token_scale: None,
            fee_limit,
            reference: tx.reference(),
            memo: tx.memo().cloned(),
        }))
    }

    /// Completes `fields` with the token's exchange rate.
    ///
    /// Reuses the registry data the handler charged the fee with. Execution that did not charge
    /// a fee has none, so the rate comes straight from registry storage; only `price_ratio` and
    /// `scale` are read, and no temporary EVM is built to resolve a balance this receipt has no
    /// use for.
    fn fill_token_fee_fields(
        &mut self,
        mut fields: MorphReceiptTxFields,
        sender: Address,
        charged: Option<TokenFeeInfo>,
    ) -> Result<Option<MorphReceiptTxFields>, BlockExecutionError> {
        if fields.fee_token_id == 0 {
            return Ok(Some(fields));
        }

        let token_info = match charged {
            Some(info) => Some(info),
            None => {
                let state = self.evm.state_mut();
                TokenFeeInfo::load_storage_only_with(
                    |address, slot| state.storage_slot_untracked(&address, &slot),
                    fields.fee_token_id,
                    sender,
                )
                .map_err(map_database_error)?
            }
        };

        Ok(token_info.map(|info| {
            fields.fee_rate = Some(info.price_ratio);
            fields.token_scale = Some(info.scale);
            fields
        }))
    }

    fn emit_state(hook: &mut Option<Box<dyn FnMut(StateUpdate) + Send>>, update: StateUpdate) {
        if !update.is_empty()
            && let Some(hook) = hook
        {
            hook(update);
        }
    }
}

impl<'a> BlockExecutor for MorphBlockExecutor<'a> {
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;
    type Evm = MorphEvm<'a>;
    type TransactionResultWithState = MorphTxResult;
    type BlockAccessList = Bal;

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.state_update_hook = Some(Box::new(hook));
        true
    }

    fn convert_block_access_list(
        block_access_list: &BlockAccessList,
    ) -> Result<Self::BlockAccessList, BlockExecutionError> {
        Bal::try_from(block_access_list.as_slice()).map_err(BlockExecutionError::other)
    }

    fn set_block_access_list(&mut self, block_access_list: Arc<Self::BlockAccessList>) {
        self.evm.state_mut().set_bal(block_access_list);
    }

    fn set_block_access_index(&mut self, index: BlockAccessIndex) {
        self.evm.state_mut().set_bal_index(index);
    }

    fn enable_block_access_list_builder(&mut self) {
        self.evm.state_mut().enable_bal_builder();
        self.evm.state_mut().reset_bal_index();
    }

    fn take_block_access_list(&mut self) -> Option<BlockAccessList> {
        self.evm.state_mut().take_bal_builder().map(Into::into)
    }

    /// Applies pre-execution state changes before processing transactions.
    ///
    /// This method performs initialization required before executing any transactions:
    ///
    /// 1. **L1 Gas Oracle Cache**: Loads the L1 Gas Price Oracle contract into the
    ///    account cache to optimize L1 fee calculations for all transactions
    ///
    /// # Errors
    /// Returns error if:
    /// - L1 Gas Price Oracle account cannot be loaded
    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // Pre-warm the L1 gas oracle contract in the underlying DB cache so that
        // subsequent per-tx L1BlockInfo reads in the handler are fast.
        let _ = self
            .evm
            .read_account_info(&L1_GAS_PRICE_ORACLE_ADDRESS)
            .map_err(map_database_error)?;

        Ok(())
    }

    fn into_state(self) -> reth_evm::BundleState {
        self.block_state.into_bundle()
    }

    fn validate_transaction_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        // Validate gas limit against the geth-compatible block gas pool. Receipt gas cannot be
        // used here: L1 messages report their actual execution cost in receipts but reserve
        // their full gas limit for block packing.
        let block_available_gas = self
            .evm
            .block()
            .gas_limit
            .saturating_to::<u64>()
            .saturating_sub(self.gas_pool_used);
        if gas_limit > block_available_gas {
            return Err(
                BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: gas_limit,
                    block_available_gas,
                }
                .into(),
            );
        }
        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        transaction: impl ExecutorTx<Self>,
    ) -> Result<Self::TransactionResultWithState, BlockExecutionError> {
        let (transaction, tx) = transaction.into_parts();
        self.validate_transaction_gas_limit(tx.tx().gas_limit())?;

        let input = TxReceiptInput {
            tx_type: tx.tx().tx_type(),
            is_l1_msg: tx.tx().is_l1_msg(),
            gas_limit: tx.tx().gas_limit(),
            sender: *tx.signer(),
            morph_tx_fields: Self::morph_tx_fields(tx.tx())?,
        };

        let result = morph_revm::MorphEvm::transact(&mut self.evm, &transaction)
            .map(|executed| executed.detach())
            .map_err(|err| map_transaction_error(err, *tx.tx().tx_hash()))?;

        Ok(MorphTxResult { result, input })
    }

    fn commit_transaction(
        &mut self,
        output: Self::TransactionResultWithState,
    ) -> Result<GasOutput, BlockExecutionError> {
        let MorphTxResult { result, input } = output;
        let TxResultWithState {
            result,
            pending_state,
            ..
        } = result;

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;

        // Morph geth's L1-message path deducts the full transaction gas limit from GasPool and
        // deliberately skips the refund path. Regular transactions return unused gas, so their
        // net gas-pool charge is the actual gas used.
        self.gas_pool_used += if input.is_l1_msg {
            input.gas_limit
        } else {
            gas_used
        };

        let morph_tx_fields = match input.morph_tx_fields {
            Some(fields) => {
                self.fill_token_fee_fields(fields, input.sender, result.ext.token_fee_info)?
            }
            None => None,
        };

        // Build receipt.
        let ctx = MorphReceiptBuilderCtx {
            tx_type: input.tx_type,
            result,
            cumulative_gas_used: self.gas_used,
            morph_tx_fields,
        };
        self.receipts.push(self.receipt_builder.build_receipt(ctx));

        // Commit state changes.
        let mut update = self.state_update_hook.is_some().then(StateUpdate::default);
        for changes in pending_state.account_changes() {
            self.block_state
                .commit_account_changes(changes, update.as_mut());
        }
        if let Some(update) = update {
            Self::emit_state(&mut self.state_update_hook, update);
        }
        // Reattach the finalized transaction so evm2 retains its account capacity and recycles
        // storage maps for the next transaction instead of dropping the detached allocations.
        self.evm.state_mut().set_pending_state(pending_state);
        self.evm.state_mut().commit_transaction();

        // Morph is pre-EIP-8037, so all gas is regular gas (no state-gas tracking).
        Ok(GasOutput::new(gas_used, 0))
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }

    fn finish_with_block_access_list(
        self,
    ) -> Result<(BlockExecutionOutput<Self::Receipt>, Option<BlockAccessList>), BlockExecutionError>
    {
        Ok((
            BlockExecutionOutput::new(
                BlockExecutionResult {
                    receipts: self.receipts,
                    requests: Default::default(),
                    gas_used: self.gas_used,
                    blob_gas_used: 0,
                },
                self.block_state.into_bundle(),
            ),
            None,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MorphEvmConfig, evm::MorphEvmFactory};
    use alloy_consensus::{Sealed, Signed, TxLegacy};
    use alloy_primitives::{Bytes, Signature, TxKind, U256};
    use evm2::evm::{AccountInfo, InMemoryDB};
    use morph_chainspec::{MorphChainSpec, hardfork::MorphHardfork};
    use morph_primitives::TxL1Msg;
    use morph_revm::{MorphBlockEnv, MorphTxEnv, morph_evm_env};
    use reth_evm::{BlockExecutorFactory, ExecutableTxParts};
    use reth_primitives_traits::Recovered;

    const SENDER: Address = Address::with_last_byte(0xaa);
    const TARGET: Address = Address::with_last_byte(0xbb);

    fn chain_spec() -> Arc<MorphChainSpec> {
        let genesis: alloy_genesis::Genesis = serde_json::from_value(serde_json::json!({
            "config": {
                "chainId": 1337,
                "homesteadBlock": 0,
                "eip150Block": 0,
                "eip155Block": 0,
                "eip158Block": 0,
                "byzantiumBlock": 0,
                "constantinopleBlock": 0,
                "petersburgBlock": 0,
                "istanbulBlock": 0,
                "berlinBlock": 0,
                "londonBlock": 0,
                "mergeNetsplitBlock": 0,
                "terminalTotalDifficulty": 0,
                "terminalTotalDifficultyPassed": true,
                "shanghaiTime": 0,
                "cancunTime": 0,
                "bernoulliBlock": 0,
                "curieBlock": 0,
                "morph203Time": 0,
                "viridianTime": 0,
                "morph": {}
            },
            "alloc": {}
        }))
        .unwrap();
        Arc::new(MorphChainSpec::from(genesis))
    }

    fn config() -> MorphEvmConfig {
        MorphEvmConfig::new(chain_spec(), MorphEvmFactory)
    }

    /// A signed transaction ready for the executor.
    fn executable(
        tx: MorphTxEnvelope,
    ) -> impl ExecutableTxParts<Recovered<MorphTxEnv>, MorphTxEnvelope> {
        reth_evm::WithTxEnv {
            tx_env: Recovered::new_unchecked(MorphTxEnv::from_envelope(&tx), SENDER),
            tx: Arc::new(Recovered::new_unchecked(tx, SENDER)),
        }
    }

    fn legacy_tx(nonce: u64, gas_limit: u64) -> MorphTxEnvelope {
        MorphTxEnvelope::Legacy(Signed::new_unhashed(
            TxLegacy {
                chain_id: Some(1337),
                nonce,
                gas_price: 1,
                gas_limit,
                to: TxKind::Call(TARGET),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            Signature::test_signature(),
        ))
    }

    fn l1_msg_tx(queue_index: u64, gas_limit: u64) -> MorphTxEnvelope {
        MorphTxEnvelope::L1Msg(Sealed::new(TxL1Msg {
            queue_index,
            gas_limit,
            to: TARGET,
            value: U256::ZERO,
            input: Bytes::default(),
            sender: SENDER,
        }))
    }

    fn funded_db() -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(
            &SENDER,
            AccountInfo::default().with_balance(U256::from(1_000_000_000u64)),
        );
        db
    }

    fn evm_with_gas_limit(db: InMemoryDB, gas_limit: u64) -> MorphEvm<'static> {
        MorphEvm::new(
            db,
            morph_evm_env(
                MorphHardfork::default(),
                1337,
                gas_limit,
                MorphBlockEnv {
                    gas_limit: U256::from(gas_limit),
                    ..Default::default()
                },
            ),
        )
    }

    /// An L1 message reserves its whole gas limit in the block gas pool even though its receipt
    /// reports only the gas it used, so a later transaction cannot be packed into the rest.
    #[test]
    fn l1_message_reserves_its_gas_limit_in_the_block_gas_pool() {
        let mut executor = MorphBlockExecutor::new(
            evm_with_gas_limit(funded_db(), 100_000),
            DefaultMorphReceiptBuilder,
            None,
        );

        let gas = executor
            .execute_transaction(executable(l1_msg_tx(0, 90_000)))
            .unwrap();
        assert_eq!(gas.tx_gas_used(), 21_000);

        let error = executor
            .execute_transaction(executable(legacy_tx(0, 21_000)))
            .unwrap_err();
        assert!(
            matches!(
                error,
                BlockExecutionError::Validation(
                    BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                        transaction_gas_limit: 21_000,
                        block_available_gas: 10_000,
                    }
                )
            ),
            "{error:?}"
        );
    }

    #[test]
    fn regular_transactions_return_unused_gas_to_the_block_gas_pool() {
        let mut executor = MorphBlockExecutor::new(
            evm_with_gas_limit(funded_db(), 100_000),
            DefaultMorphReceiptBuilder,
            None,
        );

        executor
            .execute_transaction(executable(legacy_tx(0, 90_000)))
            .unwrap();
        executor
            .execute_transaction(executable(legacy_tx(1, 21_000)))
            .unwrap();

        assert_eq!(executor.receipts().len(), 2);
        let output = executor.finish().unwrap();
        assert_eq!(output.result.gas_used, 42_000);
    }

    #[test]
    fn invalid_transactions_are_block_validation_errors() {
        let mut executor = MorphBlockExecutor::new(
            evm_with_gas_limit(funded_db(), 100_000),
            DefaultMorphReceiptBuilder,
            None,
        );

        let error = executor
            .execute_transaction(executable(legacy_tx(7, 21_000)))
            .unwrap_err();

        let BlockExecutionError::Validation(BlockValidationError::InvalidTx { error, .. }) = error
        else {
            panic!("expected an invalid transaction, got {error:?}");
        };
        assert!(!error.is_nonce_too_low(), "nonce 7 is too high");
    }

    #[test]
    fn database_failures_are_not_transaction_verdicts() {
        let hash = B256::repeat_byte(1);
        let fatal = evm2::DatabaseError::new(std::io::Error::other("read failed"), true);
        assert!(matches!(
            map_transaction_error(HandlerError::Database(fatal), hash),
            BlockExecutionError::Internal(InternalBlockExecutionError::EVM { .. })
        ));

        let invalid_input = evm2::DatabaseError::new(std::io::Error::other("bad input"), false);
        assert!(matches!(
            map_transaction_error(HandlerError::Database(invalid_input), hash),
            BlockExecutionError::Validation(BlockValidationError::EVM { .. })
        ));
    }

    #[test]
    fn morph_rejections_can_be_read_back() {
        let error = map_transaction_error(
            MorphInvalidTransaction::TokenNotActive(3).into(),
            B256::repeat_byte(1),
        );
        assert_eq!(
            morph_invalid_tx_error(&error),
            Some(&MorphInvalidTransaction::TokenNotActive(3))
        );
    }

    #[test]
    fn factory_creates_an_executor_for_a_block() {
        let config = config();
        let evm = config.evm_with_env(
            funded_db(),
            morph_evm_env(
                MorphHardfork::default(),
                1337,
                30_000_000,
                MorphBlockEnv::default(),
            ),
        );
        let ctx = reth_evm_ethereum::EthBlockExecutionCtx {
            tx_count_hint: Some(3),
            parent_hash: B256::ZERO,
            parent_beacon_block_root: None,
            ommers: &[],
            withdrawals: None,
            extra_data: Bytes::new(),
            slot_number: None,
        };
        let executor = config.create_executor(evm, ctx);
        assert!(executor.receipts().is_empty());
    }
}
