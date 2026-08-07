//! Block execution for Morph L2.
//!
//! This module provides block execution functionality for Morph, including:
//! - [`MorphBlockExecutor`]: The main block executor
//! - [`MorphBlockExecutorFactory`]: Factory for creating block executors
//! - [`MorphReceiptBuilder`]: Receipt construction for transactions

mod factory;
mod metrics;
mod receipt;

use metrics::SweepMetrics;

pub(crate) use factory::MorphBlockExecutorFactory;
pub(crate) use receipt::{
    DefaultMorphReceiptBuilder, MorphReceiptBuilder, MorphReceiptBuilderCtx, MorphReceiptTxFields,
};

use crate::{BlockSweepGasLimitExceeded, evm::MorphEvm};
use alloy_consensus::Transaction;
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::{
    Database, Evm, RecoveredTx,
    block::{
        BlockExecutionError, BlockExecutionResult, BlockExecutor, BlockValidationError,
        ExecutableTx, GasOutput, TxResult,
    },
};
use alloy_primitives::{Address, Log, U256};
use morph_chainspec::{MorphChainSpec, MorphHardfork, MorphHardforks};
use morph_primitives::{MorphReceipt, MorphTxEnvelope};
use morph_revm::{
    BLOCK_SWEEP_GAS_LIMIT, L1_GAS_PRICE_ORACLE_ADDRESS, MorphHaltReason, SweepBlockSession,
    SweepExecutionMode, SweepOutcome, TokenFeeInfo, evm::MorphContext,
};
use reth_primitives_traits::Recovered;
use reth_revm::{DatabaseCommit, Inspector, context::result::ResultAndState};
use revm::context::Block;

/// Validates the consensus-level, post-hoc sweep transfer-gas sum.
fn validate_block_sweep_gas_limit(transfer_gas_used: u64) -> Result<(), BlockExecutionError> {
    if transfer_gas_used > BLOCK_SWEEP_GAS_LIMIT {
        return Err(BlockValidationError::other(BlockSweepGasLimitExceeded {
            transfer_gas_used,
            limit: BLOCK_SWEEP_GAS_LIMIT,
        })
        .into());
    }

    Ok(())
}

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
    /// Sweep state, logs, and fixed system usage produced after the main transaction.
    pub sweep: SweepOutcome,
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
    /// Canonical sweep budget and request deduplication for this block.
    sweep_session: SweepBlockSession,
    /// Sweep observability counters.
    sweep_metrics: SweepMetrics,
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
            sweep_session: SweepBlockSession::default(),
            sweep_metrics: SweepMetrics::default(),
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

        let sweep_mode = if self.evm.block().sweep.is_some() {
            SweepExecutionMode::Canonical(self.sweep_session.plan())
        } else {
            SweepExecutionMode::Disabled
        };
        self.evm.set_sweep_execution_mode(sweep_mode);

        // Execute the transaction
        let result = self
            .evm
            .transact(tx_env)
            .map_err(|err| BlockExecutionError::evm(err, *consensus_tx.tx_hash()))?;

        // Read caches from the EVM immediately after execution, before the next tx resets them.
        let l1_fee = self.evm.cached_l1_data_fee();
        let pre_fee_logs = self.evm.take_pre_fee_logs();
        let post_fee_logs = self.evm.take_post_fee_logs();
        let sweep = self
            .evm
            .take_sweep_outcome()
            .ok_or_else(|| BlockExecutionError::msg("missing sweep outcome"))?;

        Ok(MorphTxResult {
            result,
            recovered: Recovered::new_unchecked(consensus_tx, signer),
            l1_fee,
            pre_fee_logs,
            post_fee_logs,
            sweep,
        })
    }

    fn commit_transaction(&mut self, output: Self::Result) -> GasOutput {
        let MorphTxResult {
            result: ResultAndState { result, state },
            recovered,
            l1_fee,
            pre_fee_logs,
            post_fee_logs,
            sweep,
        } = output;

        self.sweep_session.commit(&sweep.block_effect);

        // Observability only; recorded here so speculative/discarded candidates
        // never move the counters (this runs solely on committed transactions).
        self.sweep_metrics
            .record(&sweep, self.sweep_session.transfer_gas_used());

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
            sweep_logs: sweep.logs,
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
        // The block sweep allowance is a post-hoc sum, so it can only be checked
        // once every transaction has run. A block that went over it is invalid as a
        // whole — verifiers must reject it rather than turn its last transaction
        // into a failure (Onyx spec §5.4.1).
        validate_block_sweep_gas_limit(self.sweep_session.transfer_gas_used())?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Sealed, TxReceipt};
    use alloy_evm::{EvmEnv, block::CommitChanges};
    use alloy_primitives::{B256, Bytes, b256};
    use morph_chainspec::MORPH_MAINNET;
    use morph_primitives::transaction::TxL1Msg;
    use morph_revm::SweepConfig;
    use revm::{
        context::{BlockEnv, CfgEnv},
        database::{CacheDB, EmptyDB},
        state::{AccountInfo, Bytecode},
    };

    const REGISTRY: Address = Address::repeat_byte(0x23);
    const EMITTER_10: Address = Address::repeat_byte(0x10);
    const EMITTER_16: Address = Address::repeat_byte(0x16);
    const REVERTING_EMITTER: Address = Address::repeat_byte(0x17);
    const CALLER: Address = Address::repeat_byte(0xCA);
    const TRANSFER_TOPIC: B256 =
        b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
    const REQUEST_TOPIC: B256 =
        b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");

    fn push32(code: &mut Vec<u8>, value: B256) {
        code.push(0x7f);
        code.extend_from_slice(value.as_slice());
    }

    fn candidate_emitter_code(candidate_count: usize, reverts: bool) -> Bytecode {
        let mut code = vec![
            0x60, 0x01, // PUSH1 1
            0x5f, // PUSH0
            0x52, // MSTORE
        ];
        for index in 0..candidate_count {
            let source = Address::with_last_byte(u8::try_from(index + 1).unwrap());
            push32(&mut code, B256::left_padding_from(source.as_slice()));
            push32(&mut code, B256::ZERO);
            push32(&mut code, TRANSFER_TOPIC);
            code.extend_from_slice(&[
                0x60, 0x20, // PUSH1 32
                0x5f, // PUSH0
                0xa3, // LOG3
            ]);
        }
        if reverts {
            code.extend_from_slice(&[
                0x5f, // PUSH0
                0x5f, // PUSH0
                0xfd, // REVERT
            ]);
        } else {
            code.push(0x00); // STOP
        }
        Bytecode::new_raw(Bytes::from(code))
    }

    fn request_emitter_code(candidate: morph_revm::SweepCandidate) -> Bytecode {
        let mut code = Vec::new();
        push32(
            &mut code,
            B256::left_padding_from(candidate.source.as_slice()),
        );
        push32(
            &mut code,
            B256::left_padding_from(candidate.token.as_slice()),
        );
        push32(&mut code, REQUEST_TOPIC);
        code.extend_from_slice(&[
            0x5f, // PUSH0 data size
            0x5f, // PUSH0 data offset
            0xa3, // LOG3
            0x00, // STOP
        ]);
        Bytecode::new_raw(Bytes::from(code))
    }

    fn insert_code(db: &mut CacheDB<EmptyDB>, address: Address, code: Bytecode) {
        db.insert_account_info(
            address,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
    }

    fn test_executor() -> MorphBlockExecutor<CacheDB<EmptyDB>, revm::inspector::NoOpInspector> {
        let mut db = CacheDB::new(EmptyDB::default());
        insert_code(&mut db, EMITTER_10, candidate_emitter_code(10, false));
        insert_code(&mut db, EMITTER_16, candidate_emitter_code(16, false));
        insert_code(&mut db, REVERTING_EMITTER, candidate_emitter_code(1, true));

        let cfg_env = CfgEnv::<MorphHardfork>::default()
            .with_spec_and_mainnet_gas_params(MorphHardfork::Onyx);
        let evm = MorphEvm::new(
            db,
            EvmEnv {
                cfg_env,
                block_env: morph_revm::MorphBlockEnv {
                    inner: BlockEnv {
                        gas_limit: 30_000_000,
                        ..Default::default()
                    },
                    sweep: Some(SweepConfig {
                        registry_address: REGISTRY,
                    }),
                },
            },
        );

        MorphBlockExecutor::new(evm, MORPH_MAINNET.clone(), DefaultMorphReceiptBuilder)
    }

    fn test_executor_with_request_emitter(
        candidate: morph_revm::SweepCandidate,
    ) -> MorphBlockExecutor<CacheDB<EmptyDB>, revm::inspector::NoOpInspector> {
        let mut executor = test_executor();
        insert_code(
            executor.evm.db_mut(),
            REGISTRY,
            request_emitter_code(candidate),
        );
        executor
    }

    fn l1_message(to: Address, queue_index: u64) -> Recovered<MorphTxEnvelope> {
        let tx = MorphTxEnvelope::L1Msg(Sealed::new(TxL1Msg {
            queue_index,
            gas_limit: 1_000_000,
            to,
            value: U256::ZERO,
            sender: CALLER,
            input: Bytes::new(),
        }));
        Recovered::new_unchecked(tx, CALLER)
    }

    #[test]
    fn discarded_speculative_output_does_not_consume_sweep_transfer_gas() {
        let mut executor = test_executor();
        let mut discarded_seen_requests = 0;

        let discarded = executor
            .execute_transaction_with_commit_condition(l1_message(EMITTER_16, 0), |output| {
                discarded_seen_requests = output.sweep.block_effect.seen_registry_requests().len();
                CommitChanges::No
            })
            .unwrap();

        assert!(discarded.is_none());
        assert_eq!(discarded_seen_requests, 0);
        assert_eq!(executor.sweep_session.transfer_gas_used(), 0);

        let output = executor
            .execute_transaction_without_commit(l1_message(EMITTER_16, 0))
            .unwrap();
        // Emitter Transfer logs are candidates, but none are whitelisted /
        // registered, so no transfer gas is consumed and no sweep settles.
        assert_eq!(output.sweep.block_effect.transfer_gas_used(), 0);
        assert!(output.sweep.successes.is_empty());
        executor.commit_transaction(output);

        assert_eq!(executor.sweep_session.transfer_gas_used(), 0);
    }

    #[test]
    fn unwhitelisted_candidates_do_not_consume_sweep_transfer_gas() {
        let mut executor = test_executor();
        for queue_index in 0..4 {
            let output = executor
                .execute_transaction_without_commit(l1_message(EMITTER_16, queue_index as u64))
                .unwrap();
            assert_eq!(output.sweep.block_effect.transfer_gas_used(), 0);
            assert!(output.sweep.successes.is_empty());
            assert!(
                !output.sweep.failures.is_empty(),
                "each unwhitelisted candidate resolves to zero and is a no-op failure"
            );
            executor.commit_transaction(output);
        }

        assert_eq!(executor.sweep_session.transfer_gas_used(), 0);
    }

    #[test]
    fn block_deduplicates_requests_without_suppressing_later_transfers() {
        let request_pair = morph_revm::SweepCandidate {
            token: EMITTER_10,
            source: Address::with_last_byte(1),
        };
        let mut executor = test_executor_with_request_emitter(request_pair);

        let discarded_request = executor
            .execute_transaction_with_commit_condition(l1_message(REGISTRY, 0), |output| {
                assert_eq!(
                    output.sweep.block_effect.seen_registry_requests(),
                    &[request_pair]
                );
                CommitChanges::No
            })
            .unwrap();
        assert!(discarded_request.is_none());

        let first_request = executor
            .execute_transaction_without_commit(l1_message(REGISTRY, 1))
            .unwrap();
        assert_eq!(
            first_request.sweep.block_effect.seen_registry_requests(),
            &[request_pair]
        );
        executor.commit_transaction(first_request);

        let duplicate_request = executor
            .execute_transaction_without_commit(l1_message(REGISTRY, 2))
            .unwrap();
        // The duplicate request was already resolved this block; it is skipped
        // entirely and produces no failure record.
        assert!(duplicate_request.sweep.failures.is_empty());
        executor.commit_transaction(duplicate_request);

        let later_transfer = executor
            .execute_transaction_without_commit(l1_message(EMITTER_10, 3))
            .unwrap();
        // A Transfer candidate for the same (token, source) pair remains
        // eligible — Transfer candidates do not participate in block-level dedup.
        assert!(!later_transfer.sweep.failures.is_empty());
    }

    #[test]
    fn block_execution_gas_excludes_sweep_transfer_gas() {
        let mut executor = test_executor();
        let output = executor
            .execute_transaction_without_commit(l1_message(EMITTER_16, 0))
            .unwrap();
        let main_gas_used = output.result.result.gas().tx_gas_used();

        // No whitelisted/registered candidate here, so no transfer gas is metered.
        assert_eq!(output.sweep.block_effect.transfer_gas_used(), 0);
        let gas_output = executor.commit_transaction(output);
        assert_eq!(gas_output.tx_gas_used(), main_gas_used);

        let (_, block_result) = executor.finish().unwrap();
        assert_eq!(block_result.gas_used, main_gas_used);
        assert_eq!(
            block_result.receipts[0].cumulative_gas_used(),
            main_gas_used
        );
    }

    #[test]
    fn reverted_main_transaction_does_not_run_or_commit_sweep() {
        let mut executor = test_executor();
        let output = executor
            .execute_transaction_without_commit(l1_message(REVERTING_EMITTER, 0))
            .unwrap();

        assert!(!output.result.result.is_success());
        assert!(output.result.result.logs().is_empty());
        assert_eq!(output.sweep.block_effect.transfer_gas_used(), 0);
        assert!(output.sweep.logs.is_empty());
        assert!(output.sweep.successes.is_empty());
        assert!(output.sweep.failures.is_empty());
        assert!(
            !output.result.state.contains_key(&REGISTRY),
            "a reverted main transaction must not enter the sweep phase"
        );

        executor.commit_transaction(output);

        assert_eq!(executor.sweep_session.transfer_gas_used(), 0);
        let receipt = &executor.receipts[0];
        assert!(!receipt.status());
        assert!(receipt.logs().is_empty());
    }

    #[test]
    fn block_sweep_gas_limit_overflow_is_a_validation_error() {
        assert!(validate_block_sweep_gas_limit(BLOCK_SWEEP_GAS_LIMIT).is_ok());

        let error = validate_block_sweep_gas_limit(BLOCK_SWEEP_GAS_LIMIT + 1).unwrap_err();

        let validation = error
            .as_validation()
            .expect("block sweep overflow must be classified as validation");
        assert!(error.as_internal().is_none());
        let BlockValidationError::Other(source) = validation else {
            panic!("expected a typed block validation error, got {validation:?}");
        };
        assert_eq!(
            source.downcast_ref::<BlockSweepGasLimitExceeded>(),
            Some(&BlockSweepGasLimitExceeded {
                transfer_gas_used: BLOCK_SWEEP_GAS_LIMIT + 1,
                limit: BLOCK_SWEEP_GAS_LIMIT,
            })
        );
        assert_eq!(
            error.to_string(),
            format!(
                "block sweep transfer gas {} exceeds the {} limit",
                BLOCK_SWEEP_GAS_LIMIT + 1,
                BLOCK_SWEEP_GAS_LIMIT,
            )
        );
    }
}
