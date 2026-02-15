//! Morph payload builder implementation.

use crate::{MorphBuilderConfig, MorphPayloadBuilderError, config::PayloadBuildingBreaker};
use alloy_consensus::{BlockHeader, Transaction, Typed2718};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, address};
use alloy_rlp::Encodable;
use morph_chainspec::MorphChainSpec;
use morph_evm::{MorphEvmConfig, MorphNextBlockEnvAttributes};
use morph_payload_types::{ExecutableL2Data, MorphBuiltPayload, MorphPayloadBuilderAttributes};
use morph_primitives::{MorphHeader, MorphTxEnvelope};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, BuildOutcomeKind, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig, is_better_payload,
};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{
    ConfigureEvm, Database, Evm, NextBlockEnvAttributes,
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_builder::PayloadId;
use reth_payload_primitives::{
    BuiltPayloadExecutedBlock, PayloadBuilderAttributes, PayloadBuilderError,
};
use reth_payload_util::{BestPayloadTransactions, NoopPayloadTransactions, PayloadTransactions};
use reth_primitives_traits::{RecoveredBlock, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use revm::context_interface::Block as RevmBlock;
use std::sync::Arc;

// =============================================================================
// L2 Message Queue Constants
// =============================================================================

/// L2 Message Queue contract address.
///
/// Manages the L1-to-L2 message queue and stores the withdraw trie root.
const L2_MESSAGE_QUEUE_ADDRESS: Address = address!("5300000000000000000000000000000000000001");

/// Storage slot for the withdraw trie root (`messageRoot`) in L2MessageQueue contract.
/// This is slot 33, which stores the Merkle root for L2→L1 messages.
const L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT: U256 = U256::from_limbs([33, 0, 0, 0]);

/// Reads the withdraw trie root from the L2MessageQueue contract storage.
fn read_withdraw_trie_root<DB: revm::Database>(db: &mut DB) -> Result<B256, DB::Error> {
    let value = db.storage(
        L2_MESSAGE_QUEUE_ADDRESS,
        L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT,
    )?;
    Ok(B256::from(value))
}

// =============================================================================
// Payload Transactions
// =============================================================================

/// A type that returns the [`PayloadTransactions`] that should be included in the pool.
pub trait MorphPayloadTransactions<Transaction>: Clone + Send + Sync + Unpin + 'static {
    /// Returns an iterator that yields the transactions in the order they should get included in
    /// the new payload.
    fn best_transactions<Pool: TransactionPool<Transaction = Transaction>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = Transaction>;
}

impl<T: PoolTransaction> MorphPayloadTransactions<T> for () {
    fn best_transactions<Pool: TransactionPool<Transaction = T>>(
        &self,
        pool: Pool,
        attr: BestTransactionsAttributes,
    ) -> impl PayloadTransactions<Transaction = T> {
        BestPayloadTransactions::new(pool.best_transactions_with_attributes(attr))
    }
}

/// Morph's payload builder.
///
/// Builds L2 blocks by executing:
/// 1. Forced transactions from payload attributes (L1 messages first)
/// 2. Pool transactions (if allowed)
#[derive(Clone, Debug)]
pub struct MorphPayloadBuilder<Pool, Client, Txs = ()> {
    /// The EVM configuration.
    pub evm_config: MorphEvmConfig,
    /// Transaction pool.
    pub pool: Pool,
    /// Node client for state access.
    pub client: Client,
    /// The type responsible for yielding the best transactions to include.
    pub best_transactions: Txs,
    /// Builder configuration.
    pub config: MorphBuilderConfig,
}

impl<Pool, Client> MorphPayloadBuilder<Pool, Client, ()> {
    /// Creates a new [`MorphPayloadBuilder`] with default configuration.
    pub fn new(pool: Pool, evm_config: MorphEvmConfig, client: Client) -> Self {
        Self {
            evm_config,
            pool,
            client,
            best_transactions: (),
            config: MorphBuilderConfig::default(),
        }
    }

    /// Creates a new [`MorphPayloadBuilder`] with the specified configuration.
    pub const fn with_config(
        pool: Pool,
        evm_config: MorphEvmConfig,
        client: Client,
        config: MorphBuilderConfig,
    ) -> Self {
        Self {
            evm_config,
            pool,
            client,
            best_transactions: (),
            config,
        }
    }
}

impl<Pool, Client, Txs> MorphPayloadBuilder<Pool, Client, Txs> {
    /// Configures the type responsible for yielding the transactions that should be included in
    /// the payload.
    pub fn with_transactions<T>(self, best_transactions: T) -> MorphPayloadBuilder<Pool, Client, T>
    where
        T: MorphPayloadTransactions<Pool::Transaction>,
        Pool: TransactionPool,
    {
        let Self {
            evm_config,
            pool,
            client,
            config,
            ..
        } = self;
        MorphPayloadBuilder {
            evm_config,
            pool,
            client,
            best_transactions,
            config,
        }
    }

    /// Sets the builder configuration.
    pub fn set_config(mut self, config: MorphBuilderConfig) -> Self {
        self.config = config;
        self
    }
}

impl<Pool, Client, Txs> MorphPayloadBuilder<Pool, Client, Txs>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = MorphChainSpec>,
{
    /// Constructs a Morph payload from the transactions sent via the payload attributes.
    fn build_payload<'a, BestTxs>(
        &self,
        args: BuildArguments<MorphPayloadBuilderAttributes, MorphBuiltPayload>,
        best: impl FnOnce(BestTransactionsAttributes) -> BestTxs + Send + Sync + 'a,
    ) -> Result<BuildOutcome<MorphBuiltPayload>, PayloadBuilderError>
    where
        BestTxs:
            PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>> + 'a,
    {
        let BuildArguments {
            mut cached_reads,
            config,
            cancel,
            best_payload,
        } = args;

        let ctx = MorphPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            config,
            cancel,
            best_payload,
            builder_config: self.config.clone(),
        };

        let state_provider = self.client.state_by_block_hash(ctx.parent().hash())?;
        let state = StateProviderDatabase::new(&state_provider);

        // Reuse cached reads from previous runs for incremental payload building
        build_payload_inner(cached_reads.as_db_mut(state), &state_provider, ctx, best)
            .map(|out| out.with_cached_reads(cached_reads))
    }
}

/// Implementation of the [`PayloadBuilder`] trait for [`MorphPayloadBuilder`].
impl<Pool, Client, Txs> PayloadBuilder for MorphPayloadBuilder<Pool, Client, Txs>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>> + Clone,
    Client: StateProviderFactory + ChainSpecProvider<ChainSpec = MorphChainSpec> + Clone,
    Txs: MorphPayloadTransactions<Pool::Transaction>,
{
    type Attributes = MorphPayloadBuilderAttributes;
    type BuiltPayload = MorphBuiltPayload;

    fn try_build(
        &self,
        args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> Result<BuildOutcome<Self::BuiltPayload>, PayloadBuilderError> {
        let pool = self.pool.clone();
        self.build_payload(args, |attrs| {
            self.best_transactions.best_transactions(pool, attrs)
        })
    }

    fn on_missing_payload(
        &self,
        _args: BuildArguments<Self::Attributes, Self::BuiltPayload>,
    ) -> MissingPayloadBehaviour<Self::BuiltPayload> {
        // Wait for the job that's already in progress
        MissingPayloadBehaviour::AwaitInProgress
    }

    fn build_empty_payload(
        &self,
        config: PayloadConfig<Self::Attributes, MorphHeader>,
    ) -> Result<Self::BuiltPayload, PayloadBuilderError> {
        let args = BuildArguments {
            config,
            cached_reads: Default::default(),
            cancel: Default::default(),
            best_payload: None,
        };
        self.build_payload(args, |_| {
            NoopPayloadTransactions::<Pool::Transaction>::default()
        })?
        .into_payload()
        .ok_or(PayloadBuilderError::MissingPayload)
    }
}

/// Container type that holds all necessities to build a new payload.
#[derive(Debug)]
struct MorphPayloadBuilderCtx {
    /// The EVM configuration.
    evm_config: MorphEvmConfig,
    /// Payload configuration.
    config: PayloadConfig<MorphPayloadBuilderAttributes, MorphHeader>,
    /// Marker to check whether the job has been cancelled.
    cancel: reth_revm::cancelled::CancelOnDrop,
    /// The currently best payload.
    best_payload: Option<MorphBuiltPayload>,
    /// Builder configuration with limits.
    builder_config: MorphBuilderConfig,
}

impl MorphPayloadBuilderCtx {
    /// Returns the parent block the payload will be built on.
    fn parent(&self) -> &SealedHeader<MorphHeader> {
        &self.config.parent_header
    }

    /// Returns the builder attributes.
    fn attributes(&self) -> &MorphPayloadBuilderAttributes {
        &self.config.attributes
    }

    /// Returns the unique ID for this payload job.
    fn payload_id(&self) -> PayloadId {
        self.attributes().payload_id()
    }

    /// Returns true if the fees are higher than the previous payload.
    fn is_better_payload(&self, total_fees: U256) -> bool {
        is_better_payload(self.best_payload.as_ref(), total_fees)
    }

    /// Returns the current fee settings for transactions from the mempool.
    fn best_transaction_attributes(&self, base_fee: u64) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(base_fee, None)
    }

    /// Executes all sequencer transactions that are included in the payload attributes.
    ///
    /// These transactions are forced and come from the sequencer (L1 messages first).
    /// Returns the executed transaction bytes for inclusion in ExecutableL2Data.
    fn execute_sequencer_transactions(
        &self,
        builder: &mut impl BlockBuilder<Primitives = morph_primitives::MorphPrimitives>,
        info: &mut ExecutionInfo,
    ) -> Result<Vec<Bytes>, PayloadBuilderError> {
        let block_gas_limit = builder.evm().block().gas_limit();
        let base_fee = builder.evm().block().basefee();
        let mut executed_txs: Vec<Bytes> = Vec::new();
        // Track gas spent by each transaction for error reporting
        let mut gas_spent_by_transactions: Vec<u64> = Vec::new();

        for (tx_idx, tx_with_encoded) in self.attributes().transactions.iter().enumerate() {
            // The transaction is already recovered in `try_new` via `try_into_recovered()`.
            // For L1 message transactions (which have no signature), this extracts
            // the `from` address directly from the transaction.
            let recovered_tx = tx_with_encoded.value();
            let tx_bytes = tx_with_encoded.encoded_bytes();

            // Blob transactions are not supported on L2
            if recovered_tx.is_eip4844() {
                return Err(PayloadBuilderError::other(
                    MorphPayloadBuilderError::BlobTransactionRejected,
                ));
            }

            let tx_gas = recovered_tx.gas_limit();

            // Check if adding this transaction would exceed block gas limit
            if info.cumulative_gas_used + tx_gas > block_gas_limit {
                gas_spent_by_transactions.push(tx_gas);
                return Err(PayloadBuilderError::other(
                    MorphPayloadBuilderError::BlockGasLimitExceededBySequencerTransactions {
                        gas_spent_by_tx: gas_spent_by_transactions,
                        gas: block_gas_limit,
                    },
                ));
            }

            // Execute the transaction
            let gas_used = match builder.execute_transaction(recovered_tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    tracing::warn!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        %error,
                        ?recovered_tx,
                        "invalid sequencer transaction in forced list"
                    );
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSequencerTransaction {
                            error: error.to_string(),
                        },
                    ));
                }
                Err(BlockExecutionError::Validation(err)) => {
                    tracing::warn!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        %err,
                        ?recovered_tx,
                        "validation error in sequencer transaction"
                    );
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSequencerTransaction {
                            error: err.to_string(),
                        },
                    ));
                }
                Err(err) => {
                    // Fatal error - this is a bug or misconfiguration
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // For L1 messages, track the next L1 message index.
            // L1 gas is prepaid on L1, so no fees are collected here.
            let gas_used = if recovered_tx.is_l1_msg() {
                // Update next_l1_message_index to be queue_index + 1
                if let Some(queue_index) = recovered_tx.queue_index() {
                    info.next_l1_message_index = queue_index + 1;
                }
                // Use actual gas consumed (including intrinsic gas)
                gas_used
            } else {
                // Calculate fees for L2 transactions: effective_tip * gas_used
                let effective_tip = recovered_tx
                    .effective_tip_per_gas(base_fee)
                    .unwrap_or_default();
                info.total_fees += U256::from(effective_tip) * U256::from(gas_used);
                gas_used
            };

            info.cumulative_gas_used += gas_used;
            gas_spent_by_transactions.push(gas_used);

            // Increment transaction count
            info.transaction_count += 1;

            // Store the original transaction bytes for ExecutableL2Data
            executed_txs.push(tx_bytes.clone());
        }

        Ok(executed_txs)
    }

    /// Executes the best transactions from the mempool.
    ///
    /// Returns `Ok(Some(()))` if the job was cancelled or breaker triggered, `Ok(None)` otherwise.
    /// Executed transaction bytes are appended to the provided vector.
    fn execute_pool_transactions<BestTxs>(
        &self,
        builder: &mut impl BlockBuilder<Primitives = morph_primitives::MorphPrimitives>,
        info: &mut ExecutionInfo,
        executed_txs: &mut Vec<Bytes>,
        mut best_txs: BestTxs,
        breaker: &PayloadBuildingBreaker,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        BestTxs: PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
    {
        let block_gas_limit = builder.evm().block().gas_limit();
        let base_fee = builder.evm().block().basefee();

        while let Some(tx) = best_txs.next(()) {
            // Check if the job was cancelled
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            // Check if the breaker triggers (time/gas/DA/tx count limits)
            if breaker.should_break(
                info.cumulative_gas_used,
                info.cumulative_da_bytes_used,
                info.transaction_count,
            ) {
                tracing::debug!(
                    target: "payload_builder",
                    cumulative_gas_used = info.cumulative_gas_used,
                    cumulative_da_bytes_used = info.cumulative_da_bytes_used,
                    transaction_count = info.transaction_count,
                    elapsed = ?breaker.elapsed(),
                    "breaker triggered, stopping pool transaction execution"
                );
                return Ok(Some(()));
            }

            let tx = tx.into_consensus();

            // Skip blob transactions and L1 messages from pool
            if tx.is_eip4844() || tx.is_l1_msg() {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // Check if the transaction exceeds block limits
            if info.is_tx_over_limits(
                tx.gas_limit(),
                tx.length() as u64,
                block_gas_limit,
                self.builder_config.max_da_block_size,
            ) {
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // Execute the transaction
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        // If the nonce is too low, we can skip this transaction
                        // but don't mark as invalid - the sender may have other valid txs
                        tracing::trace!(
                            target: "payload_builder",
                            %error,
                            ?tx,
                            "skipping nonce too low transaction"
                        );
                    } else {
                        // If the transaction is invalid for other reasons,
                        // skip it and all of its descendants from this sender
                        tracing::trace!(
                            target: "payload_builder",
                            %error,
                            ?tx,
                            "skipping invalid transaction and its descendants"
                        );
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                    }
                    continue;
                }
                Err(BlockExecutionError::Validation(err)) => {
                    // Other validation errors - skip transaction and descendants
                    tracing::trace!(
                        target: "payload_builder",
                        %err,
                        ?tx,
                        "validation error in pool transaction, skipping"
                    );
                    best_txs.mark_invalid(tx.signer(), tx.nonce());
                    continue;
                }
                Err(err) => {
                    // Fatal error - should not continue
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };

            // Update execution info
            info.cumulative_gas_used += gas_used;
            info.cumulative_da_bytes_used += tx.length() as u64;
            info.transaction_count += 1;

            // Calculate fees: effective_tip * gas_used
            let effective_tip = tx.effective_tip_per_gas(base_fee).unwrap_or_default();
            info.total_fees += U256::from(effective_tip) * U256::from(gas_used);

            // Store the transaction bytes for ExecutableL2Data
            let mut tx_bytes = Vec::new();
            tx.encode_2718(&mut tx_bytes);
            executed_txs.push(Bytes::from(tx_bytes));
        }

        Ok(None)
    }
}

/// Execution information collected during payload building.
#[derive(Debug, Default)]
struct ExecutionInfo {
    /// Cumulative gas used by all executed transactions.
    cumulative_gas_used: u64,
    /// Cumulative DA bytes used (for L2 data availability).
    cumulative_da_bytes_used: u64,
    /// Total fees collected from executed transactions.
    total_fees: U256,
    /// Next L1 message queue index.
    next_l1_message_index: u64,
    /// Number of transactions executed (including both sequencer and pool transactions).
    transaction_count: u64,
}

impl ExecutionInfo {
    /// Creates a new [`ExecutionInfo`] with the initial next L1 message index from parent.
    const fn new(next_l1_message_index: u64) -> Self {
        Self {
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            next_l1_message_index,
            transaction_count: 0,
        }
    }

    /// Returns true if the transaction would exceed the block limits.
    fn is_tx_over_limits(
        &self,
        tx_gas_limit: u64,
        tx_size: u64,
        block_gas_limit: u64,
        block_da_limit: Option<u64>,
    ) -> bool {
        // Check DA limit if configured
        if block_da_limit.is_some_and(|da_limit| self.cumulative_da_bytes_used + tx_size > da_limit)
        {
            return true;
        }

        // Check gas limit
        self.cumulative_gas_used + tx_gas_limit > block_gas_limit
    }
}

/// Builds the payload on top of the state.
fn build_payload_inner<'a, DB, BestTxs>(
    db: DB,
    state_provider: &impl StateProvider,
    ctx: MorphPayloadBuilderCtx,
    best: impl FnOnce(BestTransactionsAttributes) -> BestTxs + Send + Sync + 'a,
) -> Result<BuildOutcomeKind<MorphBuiltPayload>, PayloadBuilderError>
where
    DB: Database<Error = reth_evm::execute::ProviderError>,
    BestTxs: PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>> + 'a,
{
    let attributes = ctx.attributes();

    tracing::debug!(
        target: "payload_builder",
        id = %ctx.payload_id(),
        parent_hash = ?ctx.parent().hash(),
        parent_number = ctx.parent().number(),
        "building new payload"
    );

    let mut db = State::builder()
        .with_database(db)
        .with_bundle_update()
        .build();

    // Build next block env attributes
    let next_block_attrs = MorphNextBlockEnvAttributes {
        inner: NextBlockEnvAttributes {
            timestamp: attributes.inner.timestamp,
            suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
            prev_randao: attributes.inner.prev_randao,
            gas_limit: attributes.gas_limit.unwrap_or(ctx.parent().gas_limit()),
            withdrawals: Some(attributes.inner.withdrawals.clone()),
            parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
            extra_data: Default::default(),
        },
        base_fee_per_gas: attributes.base_fee_per_gas,
    };

    // Create block builder
    let mut builder = ctx
        .evm_config
        .builder_for_next_block(&mut db, ctx.parent(), next_block_attrs)
        .map_err(PayloadBuilderError::other)?;

    // 1. Apply pre-execution changes (system contracts, etc.)
    builder.apply_pre_execution_changes().map_err(|err| {
        tracing::warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    // Initialize next_l1_message_index from parent header
    let mut info = ExecutionInfo::new(ctx.parent().next_l1_msg_index);
    let base_fee = builder.evm().block().basefee();
    let block_gas_limit = builder.evm().block().gas_limit();

    // Create breaker for early exit from pool transaction execution
    let breaker = ctx.builder_config.breaker(block_gas_limit);

    // Execute sequencer transactions (L1 messages and forced transactions)
    let mut executed_txs = ctx.execute_sequencer_transactions(&mut builder, &mut info)?;

    if attributes.include_tx_pool() {
        // Execute pool transactions (best transactions from mempool)
        let best_txs = best(ctx.best_transaction_attributes(base_fee));
        if ctx
            .execute_pool_transactions(
                &mut builder,
                &mut info,
                &mut executed_txs,
                best_txs,
                &breaker,
            )?
            .is_some()
        {
            // Check if it was a cancellation or just breaker triggered
            if ctx.cancel.is_cancelled() {
                return Ok(BuildOutcomeKind::Cancelled);
            }
            // Breaker triggered - continue with current transactions
            tracing::debug!(
                target: "payload_builder",
                elapsed = ?breaker.elapsed(),
                cumulative_gas_used = info.cumulative_gas_used,
                cumulative_da_bytes_used = info.cumulative_da_bytes_used,
                tx_count = executed_txs.len(),
                "breaker stopped pool execution, finalizing payload"
            );
        }
    } else {
        tracing::debug!(
            target: "payload_builder",
            tx_count = executed_txs.len(),
            "skipping txpool inclusion: explicit transaction list provided"
        );
    }

    // Check if this payload is better than the previous one
    if !ctx.is_better_payload(info.total_fees) {
        return Ok(BuildOutcomeKind::Aborted {
            fees: info.total_fees,
        });
    }

    // Read withdraw_trie_root from L2MessageQueue contract storage
    // This must be done before finish() consumes the builder
    let withdraw_trie_root = read_withdraw_trie_root(builder.evm_mut().db_mut())
        .map_err(|err| PayloadBuilderError::other(MorphPayloadBuilderError::Database(err)))?;

    // 6. Finish building the block
    let BlockBuilderOutcome {
        execution_result,
        hashed_state,
        trie_updates,
        mut block,
    } = builder.finish(state_provider)?;

    // Update MorphHeader with next_l1_msg_index.
    // Since hash_slow() only hashes the inner header, we can update the
    // MorphHeader's L2-specific fields without changing the block hash.
    let (mut morph_block, senders) = block.split();
    morph_block = morph_block.map_header(|mut header: MorphHeader| {
        header.next_l1_msg_index = info.next_l1_message_index;
        // batch_hash remains B256::ZERO - it will be set by the batch submitter
        header
    });
    block = RecoveredBlock::new_unhashed(morph_block, senders);

    // Get the sealed block from the recovered block
    let sealed_block = Arc::new(block.sealed_block().clone());
    let header = sealed_block.header();

    tracing::debug!(
        target: "payload_builder",
        id = %ctx.payload_id(),
        sealed_block_header = ?header,
        "sealed built block"
    );

    // Build ExecutableL2Data from the sealed block
    // ExecutableL2Data expects raw 256-byte bloom, not RLP-encoded bytes.
    let logs_bloom_bytes = header.logs_bloom().as_slice().to_vec();

    let executable_data = ExecutableL2Data {
        parent_hash: header.parent_hash(),
        miner: header.beneficiary(),
        number: header.number(),
        gas_limit: header.gas_limit(),
        base_fee_per_gas: header.base_fee_per_gas().map(|f| f as u128),
        timestamp: header.timestamp(),
        transactions: executed_txs,
        state_root: header.state_root(),
        gas_used: execution_result.gas_used,
        receipts_root: header.receipts_root(),
        logs_bloom: Bytes::from(logs_bloom_bytes),
        withdraw_trie_root,
        next_l1_message_index: info.next_l1_message_index,
        hash: sealed_block.hash(),
    };

    let execution_output = BlockExecutionOutput {
        state: db.take_bundle(),
        result: execution_result,
    };

    let executed = BuiltPayloadExecutedBlock {
        recovered_block: Arc::new(block),
        execution_output: Arc::new(execution_output),
        // Keep unsorted; conversion to sorted is deferred until required.
        hashed_state: Err(Arc::new(hashed_state)).into(),
        trie_updates: Err(Arc::new(trie_updates)).into(),
    };

    let payload = MorphBuiltPayload::new(
        ctx.payload_id(),
        sealed_block,
        info.total_fees,
        executable_data,
        Some(executed),
    );

    Ok(BuildOutcomeKind::Better { payload })
}
