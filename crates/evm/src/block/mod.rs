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
    Database, Evm,
    block::{BlockExecutionError, BlockExecutionResult, BlockExecutor, ExecutableTx, OnStateHook},
};
use alloy_primitives::U256;
use curie::apply_curie_hard_fork;
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{
    L1_GAS_PRICE_ORACLE_ADDRESS, L1BlockInfo, MorphHaltReason, TokenFeeInfo, evm::MorphContext,
};
use reth_chainspec::EthereumHardforks;
use reth_revm::{DatabaseCommit, Inspector, State, context::result::ResultAndState};
use revm::context::Block;
use std::marker::PhantomData;

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
pub(crate) struct MorphBlockExecutor<'a, DB: Database, I> {
    /// The EVM used by executor
    evm: MorphEvm<&'a mut State<DB>, I>,
    /// Chain specification
    spec: &'a MorphChainSpec,
    /// Receipt builder
    receipt_builder: &'a DefaultMorphReceiptBuilder,
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
    /// Creates a new [`MorphBlockExecutor`].
    ///
    /// # Arguments
    /// * `evm` - The EVM instance configured for Morph execution
    /// * `spec` - Chain specification containing hardfork information
    /// * `receipt_builder` - Builder for constructing transaction receipts
    pub(crate) fn new(
        evm: MorphEvm<&'a mut State<DB>, I>,
        spec: &'a MorphChainSpec,
        receipt_builder: &'a DefaultMorphReceiptBuilder,
    ) -> Self {
        Self {
            evm,
            spec,
            receipt_builder,
            receipts: Vec::new(),
            gas_used: 0,
            _phantom: PhantomData,
        }
    }

    /// Calculate the L1 data fee for a transaction.
    ///
    /// The L1 fee compensates for the cost of posting transaction data to Ethereum L1.
    /// This is a key component of L2 transaction costs on Morph.
    ///
    /// # Calculation Steps
    /// 1. Check if transaction is an L1 message (which don't pay L1 fees)
    /// 2. Get RLP-encoded transaction bytes
    /// 3. Fetch L1 block info from L1 Gas Price Oracle contract
    /// 4. Calculate fee based on transaction size and L1 gas price
    ///
    /// # Arguments
    /// * `tx` - The transaction to calculate L1 fee for
    /// * `hardfork` - The current Morph hardfork (affects fee calculation formula)
    ///
    /// # Returns
    /// - `Ok(U256::ZERO)` for L1 message transactions
    /// - `Ok(fee)` for regular transactions, where fee = f(tx_size, l1_gas_price, hardfork)
    /// - `Err` if L1 block info cannot be fetched
    ///
    /// # Errors
    /// Returns error if the L1 Gas Price Oracle contract state cannot be read.
    fn calculate_l1_fee(
        &mut self,
        tx: &MorphTxEnvelope,
        hardfork: MorphHardfork,
    ) -> Result<U256, BlockExecutionError> {
        // L1 message transactions don't pay L1 fees
        if tx.is_l1_msg() {
            return Ok(U256::ZERO);
        }

        // Get the RLP-encoded transaction bytes
        let rlp_bytes = tx.rlp();

        // Fetch L1 block info from the L1 Gas Price Oracle contract
        let l1_block_info = L1BlockInfo::try_fetch(self.evm.db_mut(), hardfork).map_err(|e| {
            BlockExecutionError::msg(format!("Failed to fetch L1 block info: {e:?}"))
        })?;

        // Calculate L1 data fee
        Ok(l1_block_info.calculate_tx_l1_cost(rlp_bytes.as_ref(), hardfork))
    }

    /// Extract MorphTx-specific fields for MorphTx (0x7F) transactions.
    ///
    /// MorphTx transactions include:
    /// - Token fee information (when using ERC20 for gas payment)
    /// - Transaction metadata (version, reference, memo)
    ///
    /// # How MorphTx Token Fees Work
    /// 1. User specifies a `fee_token_id` (registered ERC20 token)
    /// 2. User specifies a `fee_limit` (max tokens willing to pay)
    /// 3. System fetches token exchange rate from L2TokenRegistry
    /// 4. System converts ETH fee to token amount using: `token_fee = eth_fee * fee_rate / token_scale`
    /// 5. System validates user has sufficient token balance
    ///
    /// # Arguments
    /// * `tx` - The transaction to extract fields from
    /// * `hardfork` - The current Morph hardfork (affects token registry behavior)
    ///
    /// # Returns
    /// - `Ok(None)` for non-MorphTx transactions
    /// - `Ok(Some(fields))` for MorphTx with valid fields
    /// - `Err` if MorphTx is missing required fields or token info cannot be fetched
    ///
    /// # Errors
    /// Returns error if:
    /// - MorphTx is missing `fee_token_id` or `fee_limit`
    /// - Transaction sender cannot be extracted
    /// - L2TokenRegistry contract cannot be queried
    fn get_morph_tx_fields(
        &mut self,
        tx: &MorphTxEnvelope,
        hardfork: MorphHardfork,
    ) -> Result<Option<MorphReceiptTxFields>, BlockExecutionError> {
        // Only MorphTx transactions have these fields
        if !tx.is_morph_tx() {
            return Ok(None);
        }

        let fee_token_id = tx
            .fee_token_id()
            .ok_or_else(|| BlockExecutionError::msg("MorphTx missing fee_token_id"))?;
        let fee_limit = tx
            .fee_limit()
            .ok_or_else(|| BlockExecutionError::msg("MorphTx missing fee_limit"))?;

        // Extract version, reference, and memo from the transaction
        let version = tx.version().unwrap_or(0);
        let reference = tx.reference();
        let memo = tx.memo();

        // Fetch token fee info from L2TokenRegistry contract
        // Note: We use the transaction sender as the caller address
        // This is needed to check token balance when validating MorphTx
        let sender = tx
            .signer_unchecked()
            .map_err(|_| BlockExecutionError::msg("Failed to extract signer from MorphTx"))?;

        let token_info =
            TokenFeeInfo::load_for_caller(self.evm.db_mut(), fee_token_id, sender, hardfork)
                .map_err(|e| {
                    BlockExecutionError::msg(format!("Failed to fetch token fee info: {e:?}"))
                })?;

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

impl<'a, DB, I> BlockExecutor for MorphBlockExecutor<'a, DB, I>
where
    DB: Database,
    I: Inspector<MorphContext<&'a mut State<DB>>>,
{
    type Transaction = MorphTxEnvelope;
    type Receipt = MorphReceipt;
    type Evm = MorphEvm<&'a mut State<DB>, I>;

    /// Applies pre-execution state changes before processing transactions.
    ///
    /// This method performs initialization required before executing any transactions:
    ///
    /// 1. **State Clear Flag**: Sets the flag that enables EIP-161 state trie clearing
    ///    if the Spurious Dragon hardfork is active
    ///
    /// 2. **L1 Gas Oracle Cache**: Loads the L1 Gas Price Oracle contract into the
    ///    account cache to optimize L1 fee calculations for all transactions
    ///
    /// 3. **Curie Hardfork**: At the exact Curie activation block, applies the
    ///    hardfork state changes (updates to L1 Gas Price Oracle contract)
    ///
    /// # Errors
    /// Returns error if:
    /// - L1 Gas Price Oracle account cannot be loaded
    /// - Curie hardfork application fails at transition block
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

        Ok(())
    }

    /// Executes a transaction without committing state changes.
    ///
    /// This method validates the transaction can fit in the remaining block gas,
    /// then executes it in the EVM. The state changes are returned but not yet
    /// committed to the database.
    ///
    /// # Gas Validation
    /// Before execution, validates that:
    /// ```text
    /// tx.gas_limit + cumulative_gas_used <= block.gas_limit
    /// ```
    ///
    /// # Returns
    /// Returns the execution result and state changes that can be committed later.
    ///
    /// # Errors
    /// Returns error if:
    /// - Transaction gas limit exceeds available block gas
    /// - EVM execution fails (reverts, halts, out of gas, etc.)
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
        self.evm
            .transact(&tx)
            .map_err(|err| BlockExecutionError::evm(err, *tx.tx().tx_hash()))
    }

    /// Commits a transaction's execution result and builds its receipt.
    ///
    /// This method performs post-execution processing for a transaction:
    ///
    /// 1. **L1 Fee Calculation**: Calculates the L1 data fee for the transaction
    /// 2. **Token Fee Info**: For MorphTx, extracts token fee information
    /// 3. **Gas Accounting**: Updates cumulative gas used for the block
    /// 4. **Receipt Building**: Constructs receipt with all Morph-specific fields
    /// 5. **State Commit**: Commits the EVM state changes to the database
    ///
    /// # Arguments
    /// * `output` - The execution result from `execute_transaction_without_commit`
    /// * `tx` - The original transaction
    ///
    /// # Returns
    /// The gas used by this transaction.
    ///
    /// # Errors
    /// Returns error if L1 fee calculation or token fee info extraction fails.
    fn commit_transaction(
        &mut self,
        output: ResultAndState<MorphHaltReason>,
        tx: impl ExecutableTx<Self>,
    ) -> Result<u64, BlockExecutionError> {
        let ResultAndState { result, state } = output;

        // Determine hardfork once and reuse for both L1 fee and token fee calculations
        let block_number: u64 = self.evm.block().number.to();
        let timestamp: u64 = self.evm.block().timestamp.to();
        let hardfork = self.spec.morph_hardfork_at(block_number, timestamp);

        // Calculate L1 fee for the transaction
        let l1_fee = self.calculate_l1_fee(tx.tx(), hardfork)?;

        // Get MorphTx-specific fields for MorphTx transactions
        let morph_tx_fields = self.get_morph_tx_fields(tx.tx(), hardfork)?;

        // Update cumulative gas used
        let gas_used = result.gas_used();
        self.gas_used += gas_used;

        // Build receipt
        let ctx: MorphReceiptBuilderCtx<'_, Self::Evm> = MorphReceiptBuilderCtx {
            tx: tx.tx(),
            result,
            cumulative_gas_used: self.gas_used,
            l1_fee,
            morph_tx_fields,
        };
        self.receipts.push(self.receipt_builder.build_receipt(ctx));

        // Commit state changes
        self.evm.db_mut().commit(state);

        Ok(gas_used)
    }

    /// Finalizes block execution and returns the results.
    ///
    /// Consumes the executor and returns the EVM instance along with the
    /// complete execution results including all transaction receipts.
    ///
    /// # Returns
    /// A tuple containing:
    /// - The EVM instance (for potential reuse or state access)
    /// - Block execution result with receipts, gas used, and empty requests
    ///
    /// Note: `blob_gas_used` is always 0 as Morph doesn't support EIP-4844 blobs.
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

    /// Sets a state hook for observing state changes.
    ///
    /// Note: State hooks are not yet implemented for the Morph block executor.
    /// This is a no-op placeholder for future implementation.
    fn set_state_hook(&mut self, _hook: Option<Box<dyn OnStateHook>>) {
        // State hooks are not yet supported for Morph block executor
    }

    /// Returns a mutable reference to the EVM instance.
    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    /// Returns a reference to the EVM instance.
    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}
