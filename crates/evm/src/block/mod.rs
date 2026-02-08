//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions
//! - Hardfork application logic (Curie, etc.)

pub(crate) mod curie;
mod receipt;

pub(crate) use receipt::{
    DefaultMorphReceiptBuilder, MorphReceiptBuilder, MorphReceiptBuilderCtx, MorphTxTokenFeeInfo,
};

use crate::{MorphBlockExecutionCtx, evm::MorphEvm};
use alloy_consensus::Transaction;
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
};
use alloy_primitives::U256;
use curie::apply_curie_hard_fork;
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{
    L1BlockInfo, L1_GAS_PRICE_ORACLE_ADDRESS, MorphHaltReason, TokenFeeInfo,
    evm::MorphContext,
};
use reth_chainspec::EthereumHardforks;
use reth_revm::{DatabaseCommit, Inspector, State, context::result::ResultAndState};
use revm::context::Block;
use std::marker::PhantomData;

/// Block executor for Morph.
///
/// This executor handles Morph-specific logic including:
/// - L1 fee calculation for transactions
/// - Token fee information extraction for MorphTx (0x7F) transactions
/// - Curie hardfork application
pub(crate) struct MorphBlockExecutor<'a, DB: Database, I> {
    /// The EVM used by executor
    evm: MorphEvm<&'a mut State<DB>, I>,
    /// Chain specification
    spec: &'a MorphChainSpec,
    /// Receipt builder
    receipt_builder: DefaultMorphReceiptBuilder,
    /// Context for block execution
    #[allow(dead_code)]
    ctx: MorphBlockExecutionCtx<'a>,
    /// Receipts of executed transactions
    receipts: Vec<MorphReceipt>,
    /// Total gas used by executed transactions
    gas_used: u64,
    /// Phantom data for inspector type
    _phantom: PhantomData<I>,
}

impl<'a, DB, I> MorphBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<&'a mut State<DB>>>,
{
    pub(crate) fn new(
        evm: MorphEvm<&'a mut State<DB>, I>,
        ctx: MorphBlockExecutionCtx<'a>,
        spec: &'a MorphChainSpec,
    ) -> Self {
        Self {
            evm,
            spec,
            receipt_builder: DefaultMorphReceiptBuilder,
            ctx,
            receipts: Vec::new(),
            gas_used: 0,
            _phantom: PhantomData,
        }
    }

    /// Calculate the L1 data fee for a transaction.
    ///
    /// Returns `U256::ZERO` for L1 message transactions, or the calculated L1 fee
    /// based on the L1 Gas Price Oracle state.
    fn calculate_l1_fee(
        &mut self,
        tx: &MorphTxEnvelope,
    ) -> Result<U256, BlockExecutionError> {
        // L1 message transactions don't pay L1 fees
        if tx.is_l1_msg() {
            return Ok(U256::ZERO);
        }

        // Get the RLP-encoded transaction bytes
        let rlp_bytes = tx.rlp();

        // Determine the current hardfork based on block number and timestamp
        let block = self.evm.block();
        let block_number: u64 = block.number.to();
        let timestamp: u64 = block.timestamp.to();
        let hardfork = self.spec.morph_hardfork_at(block_number, timestamp);

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(self.evm.db_mut(), hardfork)
            .map_err(|e| BlockExecutionError::msg(format!("Failed to fetch L1 block info: {e:?}")))?;

        // Calculate L1 data fee
        Ok(l1_block_info.calculate_tx_l1_cost(rlp_bytes.as_ref(), hardfork))
    }

    /// Extract token fee information for MorphTx (0x7F) transactions.
    ///
    /// This queries the L2TokenRegistry contract to get the exchange rate and scale
    /// for the specified token ID.
    fn get_token_fee_info(
        &mut self,
        tx: &MorphTxEnvelope,
    ) -> Result<Option<MorphTxTokenFeeInfo>, BlockExecutionError> {
        // Only MorphTx transactions have token fee information
        if !tx.is_morph_tx() {
            return Ok(None);
        }

        let fee_token_id = tx.fee_token_id().unwrap(); // Safe because we checked is_morph_tx()
        let fee_limit = tx.fee_limit().unwrap(); // Safe because we checked is_morph_tx()

        // Determine the current hardfork based on block number and timestamp
        let block = self.evm.block();
        let block_number: u64 = block.number.to();
        let timestamp: u64 = block.timestamp.to();
        let hardfork = self.spec.morph_hardfork_at(block_number, timestamp);

        // Fetch token fee info from L2TokenRegistry contract
        // Note: We use the transaction sender as the caller address
        // This is needed to check token balance when validating MorphTx
        let sender = tx.signer_unchecked().unwrap_or_default();

        let token_info = TokenFeeInfo::fetch(self.evm.db_mut(), fee_token_id, sender, hardfork)
            .map_err(|e| BlockExecutionError::msg(format!("Failed to fetch token fee info: {e:?}")))?;

        Ok(token_info.map(|info| MorphTxTokenFeeInfo {
            fee_token_id,
            fee_rate: info.price_ratio,
            token_scale: info.scale,
            fee_limit,
        }))
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
        // 1. Set state clear flag if the block is after the Spurious Dragon hardfork
        let block_number: u64 = self.evm.block().number.to();
        let state_clear_flag = self.spec.is_spurious_dragon_active_at_block(block_number);
        self.evm.db_mut().set_state_clear_flag(state_clear_flag);

        // 2. Load L1 gas oracle contract into cache
        let _ = self
            .evm
            .db_mut()
            .load_cache_account(L1_GAS_PRICE_ORACLE_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        // 3. Apply Curie hardfork at the transition block
        // Only executes once at the exact block where Curie activates
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

        // TODO: Apply EIP-2935 blockhashes contract call when needed

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<ResultAndState<MorphHaltReason>, BlockExecutionError> {
        // The sum of the transaction's gas limit and the gas utilized in this block prior,
        // must be no greater than the block's gasLimit.
        let block_available_gas = self.evm.block().gas_limit() - self.gas_used;
        if tx.tx().gas_limit() > block_available_gas {
            return Err(BlockExecutionError::msg(format!(
                "transaction gas limit {} exceeds block available gas {}",
                tx.tx().gas_limit(),
                block_available_gas
            )));
        }

        // Execute the transaction
        self.evm.transact(&tx).map_err(|err| BlockExecutionError::evm(err, *tx.tx().tx_hash()))
    }

    fn commit_transaction(
        &mut self,
        output: ResultAndState<MorphHaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        
        let ResultAndState { result, state } = output;

        // Calculate L1 fee for the transaction
        let l1_fee = self.calculate_l1_fee(tx.tx())?;

        // Get token fee information for MorphTx transactions
        let token_fee_info = self.get_token_fee_info(tx.tx())?;

        // Update cumulative gas used
        let gas_used = result.gas_used();
        self.gas_used += gas_used;

        // Build receipt
        let ctx: MorphReceiptBuilderCtx<'_, Self::Evm> = MorphReceiptBuilderCtx {
            tx: tx.tx(),
            result,
            cumulative_gas_used: self.gas_used,
            l1_fee,
            token_fee_info,
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

    fn set_state_hook(&mut self, _hook: Option<Box<dyn OnStateHook>>) {
        // State hooks are not yet supported for Morph block executor
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }
}
