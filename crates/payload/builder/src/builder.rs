//! Morph payload builder implementation.

use crate::MorphPayloadBuilderError;
use alloy_consensus::{BlockHeader, Transaction, Typed2718};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Bytes, U256};
use alloy_rlp::Encodable;
use morph_chainspec::MorphChainSpec;
use morph_evm::{MorphEvmConfig, MorphNextBlockEnvAttributes};
use morph_payload_types::{ExecutableL2Data, MorphBuiltPayload, MorphPayloadBuilderAttributes};
use morph_primitives::{MorphHeader, MorphTxEnvelope};
use reth_basic_payload_builder::{
    is_better_payload, BuildArguments, BuildOutcome, BuildOutcomeKind, MissingPayloadBehaviour,
    PayloadBuilder, PayloadConfig,
};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{
    block::BlockExecutionError,
    execute::{BlockBuilder, BlockBuilderOutcome},
    ConfigureEvm, Database, Evm, NextBlockEnvAttributes,
};
use reth_payload_builder::PayloadId;
use reth_payload_primitives::{PayloadBuilderAttributes, PayloadBuilderError};
use reth_payload_util::{BestPayloadTransactions, NoopPayloadTransactions, PayloadTransactions};
use reth_primitives_traits::SealedHeader;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use revm::context_interface::Block as RevmBlock;
use std::sync::Arc;

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
}

impl<Pool, Client> MorphPayloadBuilder<Pool, Client, ()> {
    /// Creates a new [`MorphPayloadBuilder`].
    pub const fn new(pool: Pool, evm_config: MorphEvmConfig, client: Client) -> Self {
        Self {
            evm_config,
            pool,
            client,
            best_transactions: (),
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
            ..
        } = self;
        MorphPayloadBuilder {
            evm_config,
            pool,
            client,
            best_transactions,
        }
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
            chain_spec: self.client.chain_spec(),
            config,
            cancel,
            best_payload,
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
#[allow(dead_code)]
struct MorphPayloadBuilderCtx {
    /// The EVM configuration.
    evm_config: MorphEvmConfig,
    /// The chain specification.
    chain_spec: Arc<MorphChainSpec>,
    /// Payload configuration.
    config: PayloadConfig<MorphPayloadBuilderAttributes, MorphHeader>,
    /// Marker to check whether the job has been cancelled.
    cancel: reth_revm::cancelled::CancelOnDrop,
    /// The currently best payload.
    best_payload: Option<MorphBuiltPayload>,
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
    #[allow(dead_code)]
    fn best_transaction_attributes(
        &self,
        base_fee: u64,
        blob_gas_price: Option<u64>,
    ) -> BestTransactionsAttributes {
        BestTransactionsAttributes::new(base_fee, blob_gas_price)
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
}

impl ExecutionInfo {
    /// Creates a new [`ExecutionInfo`].
    const fn new() -> Self {
        Self {
            cumulative_gas_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            next_l1_message_index: 0,
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
        parent_number = ctx.parent().number,
        "building new payload"
    );

    let mut db = State::builder()
        .with_database(db)
        .with_bundle_update()
        .build();

    // Build next block env attributes using the inner EthPayloadBuilderAttributes
    let next_block_attrs = MorphNextBlockEnvAttributes {
        inner: NextBlockEnvAttributes {
            timestamp: attributes.inner.timestamp,
            suggested_fee_recipient: attributes.inner.suggested_fee_recipient,
            prev_randao: attributes.inner.prev_randao,
            gas_limit: ctx.parent().gas_limit,
            withdrawals: Some(attributes.inner.withdrawals.clone()),
            parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
            extra_data: Default::default(),
        },
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

    let mut info = ExecutionInfo::new();
    let block_gas_limit = builder.evm().block().gas_limit();
    let base_fee = builder.evm().block().basefee();

    // Collect executed transactions for ExecutableL2Data
    let mut executed_txs: Vec<Bytes> = Vec::new();

    // 2. Execute forced transactions from payload attributes (L1 messages first)
    // Transactions are already decoded and recovered in MorphPayloadBuilderAttributes
    for tx_with_encoded in &attributes.transactions {
        let recovered_tx = tx_with_encoded.value();
        let tx_bytes = tx_with_encoded.encoded_bytes();

        // Blob transactions are not supported
        if recovered_tx.is_eip4844() {
            return Err(PayloadBuilderError::other(
                MorphPayloadBuilderError::BlobTransactionRejected,
            ));
        }

        let tx_gas = recovered_tx.gas_limit();

        // Check gas limit
        if info.cumulative_gas_used + tx_gas > block_gas_limit {
            return Err(PayloadBuilderError::other(
                MorphPayloadBuilderError::BlockGasLimitExceededBySequencerTransactions {
                    gas_spent_by_tx: vec![tx_gas],
                    gas: block_gas_limit,
                },
            ));
        }

        // Execute the transaction
        let gas_used = match builder.execute_transaction(recovered_tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(err)) => {
                tracing::trace!(
                    target: "payload_builder",
                    %err,
                    ?recovered_tx,
                    "Error in sequencer transaction, skipping."
                );
                continue;
            }
            Err(err) => {
                return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
            }
        };

        // For L1 messages, use full gas limit (no refund) and no fees
        let gas_used = if recovered_tx.is_l1_msg() {
            recovered_tx.gas_limit()
            // L1 messages have zero gas price, so no fees are collected
        } else {
            // Calculate fees for L2 transactions: effective_tip * gas_used
            let effective_tip = recovered_tx
                .effective_tip_per_gas(base_fee)
                .unwrap_or_default();
            info.total_fees += U256::from(effective_tip) * U256::from(gas_used);
            gas_used
        };

        info.cumulative_gas_used += gas_used;

        // Store the original transaction bytes for ExecutableL2Data
        executed_txs.push(tx_bytes.clone());
    }

    // 3. Execute pool transactions (best transactions from mempool)
    let mut best_txs = best(ctx.best_transaction_attributes(base_fee, None));

    while let Some(tx) = best_txs.next(()) {
        // Check if the job was cancelled
        if ctx.cancel.is_cancelled() {
            return Ok(BuildOutcomeKind::Cancelled);
        }

        let tx = tx.into_consensus();

        // Skip blob transactions and L1 messages from pool
        if tx.is_eip4844() || tx.is_l1_msg() {
            best_txs.mark_invalid(tx.signer(), tx.nonce());
            continue;
        }

        // Check if the transaction exceeds block limits
        if info.is_tx_over_limits(tx.gas_limit(), tx.length() as u64, block_gas_limit, None) {
            best_txs.mark_invalid(tx.signer(), tx.nonce());
            continue;
        }

        // Execute the transaction
        let gas_used = match builder.execute_transaction(tx.clone()) {
            Ok(gas_used) => gas_used,
            Err(BlockExecutionError::Validation(err)) => {
                tracing::trace!(
                    target: "payload_builder",
                    %err,
                    ?tx,
                    "Error in pool transaction, skipping."
                );
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }
            Err(err) => {
                return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
            }
        };

        // Update execution info
        info.cumulative_gas_used += gas_used;
        info.cumulative_da_bytes_used += tx.length() as u64;

        // Calculate fees: effective_tip * gas_used
        let effective_tip = tx.effective_tip_per_gas(base_fee).unwrap_or_default();
        info.total_fees += U256::from(effective_tip) * U256::from(gas_used);

        // Store the transaction bytes for ExecutableL2Data
        let mut tx_bytes = Vec::new();
        tx.encode_2718(&mut tx_bytes);
        executed_txs.push(Bytes::from(tx_bytes));
    }

    // Check if this payload is better than the previous one
    if !ctx.is_better_payload(info.total_fees) {
        return Ok(BuildOutcomeKind::Aborted {
            fees: info.total_fees,
        });
    }

    // Finish building the block
    let BlockBuilderOutcome {
        execution_result,
        block,
        ..
    } = builder.finish(state_provider)?;

    let sealed_block = Arc::new(block.sealed_block().clone());
    let header = sealed_block.header();

    tracing::debug!(
        target: "payload_builder",
        id = %ctx.payload_id(),
        sealed_block_header = ?header,
        "sealed built block"
    );

    // Build ExecutableL2Data from the sealed block
    let mut logs_bloom_bytes = Vec::new();
    header.logs_bloom().encode(&mut logs_bloom_bytes);

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
        withdraw_trie_root: Default::default(), // TODO: compute withdraw trie root
        next_l1_message_index: info.next_l1_message_index,
        hash: sealed_block.hash(),
    };

    let payload = MorphBuiltPayload::new(
        ctx.payload_id(),
        sealed_block,
        info.total_fees,
        executable_data,
    );

    Ok(BuildOutcomeKind::Better { payload })
}
