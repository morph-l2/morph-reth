//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphBlockExecutorFactory`]: Factory for creating block executors
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions
//! - Hardfork application logic (Curie, etc.)

pub(crate) mod curie;
mod factory;
mod receipt;

pub(crate) use factory::MorphBlockExecutorFactory;
pub(crate) use receipt::{
    DefaultMorphReceiptBuilder, MorphReceiptBuilder, MorphReceiptBuilderCtx, MorphReceiptTxFields,
};

use crate::evm::MorphEvm;
use alloy_consensus::Transaction;
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook,
        StateChangeSource, TxResult,
    },
};
use alloy_primitives::{Address, Log, U256};
use curie::apply_curie_hard_fork;
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{L1_GAS_PRICE_ORACLE_ADDRESS, MorphHaltReason, TokenFeeInfo, evm::MorphContext};
use reth_primitives_traits::Recovered;
use reth_revm::{DatabaseCommit, Inspector, context::result::ResultAndState};
use revm::context::Block;

/// The result of executing a Morph transaction.
///
/// Carries the EVM result together with the recovered transaction and cached fee
/// information that are needed during [`MorphBlockExecutor::commit_transaction`].
pub(crate) struct MorphTxResult {
    /// The raw EVM execution result and state diff.
    pub result: ResultAndState<MorphHaltReason>,
    /// Recovered transaction (consensus tx + signer).
    pub recovered: Recovered<MorphTxEnvelope>,
    /// L1 data fee read from the handler cache immediately after execution.
    pub l1_fee: U256,
    /// Token-fee deduction Transfer logs (survive main-tx revert).
    pub pre_fee_logs: Vec<Log>,
    /// Token-fee reimbursement Transfer logs (survive main-tx revert).
    pub post_fee_logs: Vec<Log>,
}

impl TxResult for MorphTxResult {
    type HaltReason = MorphHaltReason;

    fn result(&self) -> &ResultAndState<Self::HaltReason> {
        &self.result
    }

    fn into_result(self) -> ResultAndState<Self::HaltReason> {
        self.result
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
/// ## Hardfork Application
/// The executor applies hardfork-specific state changes at transition blocks,
/// such as the Curie hardfork which updates the L1 Gas Price Oracle contract.
///
/// ## Execution Flow
/// 1. `apply_pre_execution_changes`: Set up state, load contracts, apply hardforks
/// 2. `execute_transaction_without_commit`: Execute transaction in EVM
/// 3. `commit_transaction`: Calculate fees, build receipt, commit state
/// 4. `finish`: Return final execution result with all receipts
pub(crate) struct MorphBlockExecutor<DB: Database, I> {
    /// The EVM used by executor (owned, not a reference)
    evm: MorphEvm<DB, I>,
    /// Chain specification
    spec: std::sync::Arc<MorphChainSpec>,
    /// Receipt builder
    receipt_builder: DefaultMorphReceiptBuilder,
    /// Receipts of executed transactions
    receipts: Vec<MorphReceipt>,
    /// Total gas used by executed transactions
    gas_used: u64,
    /// Cached hardfork for this block (constant across all transactions).
    /// Set in `apply_pre_execution_changes`, reused in `commit_transaction`.
    hardfork: MorphHardfork,
    /// Hook for notifying the engine about per-transaction state changes.
    /// Used by `StateRootTask` for incremental sparse trie computation.
    state_hook: Option<Box<dyn OnStateHook>>,
}

impl<DB, I> MorphBlockExecutor<DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<DB>>,
{
    /// Creates a new [`MorphBlockExecutor`].
    ///
    /// # Arguments
    /// * `evm` - The EVM instance configured for Morph execution
    /// * `spec` - Chain specification containing hardfork information
    /// * `receipt_builder` - Builder for constructing transaction receipts
    pub(crate) fn new(
        evm: MorphEvm<DB, I>,
        spec: std::sync::Arc<MorphChainSpec>,
        receipt_builder: DefaultMorphReceiptBuilder,
    ) -> Self {
        Self {
            evm,
            spec,
            receipt_builder,
            receipts: Vec::new(),
            gas_used: 0,
            hardfork: MorphHardfork::default(),
            state_hook: None,
        }
    }

    /// Extract MorphTx-specific fields for MorphTx (0x7F) transactions.
    ///
    /// MorphTx transactions include:
    /// - Token fee information (when using ERC20 for gas payment)
    /// - Transaction metadata (version, reference, memo)
    fn get_morph_tx_fields(
        &mut self,
        tx: &MorphTxEnvelope,
        sender: Address,
        hardfork: MorphHardfork,
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

        let version = tx.version().unwrap_or(0);
        let reference = tx.reference();
        let memo = tx.memo().cloned();

        if fee_token_id == 0 {
            return Ok(Some(MorphReceiptTxFields {
                version,
                fee_token_id: 0,
                fee_rate: U256::ZERO,
                token_scale: U256::ZERO,
                fee_limit,
                reference,
                memo,
            }));
        }

        let token_info = match self.evm.cached_token_fee_info() {
            Some(info) => Some(info),
            None => {
                TokenFeeInfo::load_for_caller(self.evm.db_mut(), fee_token_id, sender, hardfork)
                    .map_err(|e| {
                        BlockExecutionError::msg(format!("Failed to fetch token fee info: {e:?}"))
                    })?
            }
        };

        Ok(token_info.map(|info| MorphReceiptTxFields {
            version,
            fee_token_id,
            fee_rate: info.price_ratio,
            token_scale: info.scale,
            fee_limit,
            reference,
            memo,
        }))
    }
}

impl<DB, I> BlockExecutor for MorphBlockExecutor<DB, I>
where
    DB: Database + DatabaseCommit,
    I: Inspector<MorphContext<DB>>,
{
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;
    type Evm = MorphEvm<DB, I>;
    type Result = MorphTxResult;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        // Pre-warm the L1 gas oracle contract in the underlying DB cache so that
        // subsequent per-tx L1BlockInfo reads in the handler are fast.
        let _ = self
            .evm
            .db_mut()
            .basic(L1_GAS_PRICE_ORACLE_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        let block_number: u64 = self.evm.block().number.to();
        let hardfork = self
            .spec
            .morph_hardfork_at(block_number, self.evm.block().timestamp.to::<u64>());
        self.hardfork = hardfork;

        // Apply Curie hardfork at the transition block
        if self
            .spec
            .morph_fork_activation(MorphHardfork::Curie)
            .transitions_at_block(block_number)
            && let Err(err) = apply_curie_hard_fork(self.evm.db_mut())
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
    ) -> Result<Self::Result, BlockExecutionError> {
        let (tx_env, recovered) = tx.into_parts();

        // Validate gas limit fits in remaining block gas.
        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;
        if recovered.tx().gas_limit() > block_available_gas {
            return Err(BlockExecutionError::msg(format!(
                "transaction gas limit {} exceeds block available gas {}",
                recovered.tx().gas_limit(),
                block_available_gas
            )));
        }

        // Clone the consensus tx and signer BEFORE transact (since we can't move out later).
        let consensus_tx = recovered.tx().clone();
        let signer = *recovered.signer();

        // Execute the transaction
        let result = self
            .evm
            .transact(tx_env)
            .map_err(|err| BlockExecutionError::evm(err, *consensus_tx.tx_hash()))?;

        // Read caches from the EVM immediately after execution, before the next tx resets them.
        let l1_fee = self.evm.cached_l1_data_fee();
        let pre_fee_logs = self.evm.take_pre_fee_logs();
        let post_fee_logs = self.evm.take_post_fee_logs();

        Ok(MorphTxResult {
            result,
            recovered: Recovered::new_unchecked(consensus_tx, signer),
            l1_fee,
            pre_fee_logs,
            post_fee_logs,
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> Result<u64, BlockExecutionError> {
        let MorphTxResult {
            result: ResultAndState { result, state },
            recovered,
            l1_fee,
            pre_fee_logs,
            post_fee_logs,
        } = output;

        // Notify the state hook (e.g. StateRootTask) BEFORE committing.
        if let Some(hook) = &mut self.state_hook {
            hook.on_state(StateChangeSource::Transaction(self.receipts.len()), &state);
        }

        let gas_used = result.gas_used();
        self.gas_used += gas_used;

        // Get MorphTx-specific fields using the recovered transaction.
        let (tx, signer) = recovered.into_parts();
        let morph_tx_fields = self.get_morph_tx_fields(&tx, signer, self.hardfork)?;

        // Build receipt.
        let ctx: MorphReceiptBuilderCtx<'_, Self::Evm> = MorphReceiptBuilderCtx {
            tx: &tx,
            result,
            cumulative_gas_used: self.gas_used,
            l1_fee,
            morph_tx_fields,
            pre_fee_logs,
            post_fee_logs,
        };
        self.receipts.push(self.receipt_builder.build_receipt(ctx));

        // Commit state changes
        self.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    fn finish(
        self,
    ) -> Result<(Self::Evm, BlockExecutionResult<Self::Receipt>), BlockExecutionError> {
        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Default::default(),
                gas_used: self.gas_used,
                blob_gas_used: 0,
            },
        ))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.state_hook = hook;
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}
