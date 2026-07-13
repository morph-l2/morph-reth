//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphBlockExecutorFactory`]: Factory for creating block executors
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions

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
        BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, GasOutput, TxResult,
    },
};
use alloy_primitives::{Address, Log, U256};
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
pub struct MorphTxResult {
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
/// ## Execution Flow
/// 1. `apply_pre_execution_changes`: Set up state and load contracts
/// 2. `execute_transaction_without_commit`: Execute transaction in EVM
/// 3. `commit_transaction`: Calculate fees, build receipt, commit state
/// 4. `finish`: Return final execution result with all receipts
pub struct MorphBlockExecutor<DB: Database, I> {
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
            .db_mut()
            .basic(L1_GAS_PRICE_ORACLE_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        let block_number: u64 = self.evm.block().number.to();
        let hardfork = self
            .spec
            .morph_hardfork_at(block_number, self.evm.block().timestamp.to::<u64>());
        self.hardfork = hardfork;

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

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let MorphTxResult {
            result: ResultAndState { result, state },
            recovered,
            l1_fee,
            pre_fee_logs,
            post_fee_logs,
        } = output;

        // EIP-8037 separates regular and state gas; pre-Amsterdam morph treats
        // them as a single number, so use the unified `tx_gas_used` getter.
        let gas_used = result.gas().tx_gas_used();
        self.gas_used += gas_used;

        // Get MorphTx-specific fields using the recovered transaction. Errors here
        // are tracing-only — the trait API no longer permits us to surface errors
        // from `commit_transaction`.
        let (tx, signer) = recovered.into_parts();
        let morph_tx_fields = match self.get_morph_tx_fields(&tx, signer, self.hardfork) {
            Ok(fields) => fields,
            Err(err) => {
                tracing::error!(
                    target: "morph::evm",
                    %err,
                    "failed to load MorphTx receipt fields; emitting receipt without them"
                );
                None
            }
        };

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

        // Morph is pre-EIP-8037, so all gas is regular gas (no state-gas tracking).
        GasOutput::new(gas_used)
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
