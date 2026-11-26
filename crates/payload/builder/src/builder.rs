//! Morph payload builder implementation.

use crate::metrics::MorphPayloadBuilderMetrics;
use crate::{MorphBuilderConfig, MorphPayloadBuilderError, config::PayloadBuildingBreaker};
use alloy_consensus::{BlockHeader, Transaction, Typed2718, transaction::TxHashRef};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256};
use morph_chainspec::{
    L2_MESSAGE_QUEUE_ADDRESS, L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, MorphChainSpec,
    MorphHardforks,
};
use morph_evm::{MorphEvmConfig, MorphInvalidTransaction, MorphNextBlockEnvAttributes};
use morph_payload_types::{
    ExecutableL2Data, MorphBuiltPayload, MorphPayloadAttributes, MorphPayloadBuilderAttributes,
};
use morph_primitives::{
    MorphHeader, MorphTxEnvelope,
    transaction::morph_transaction::{MORPH_TX_VERSION_1, MORPH_TX_VERSION_2},
};
use reth_basic_payload_builder::{
    BuildArguments, BuildOutcome, BuildOutcomeKind, MissingPayloadBehaviour, PayloadBuilder,
    PayloadConfig, is_better_payload,
};
use reth_chainspec::ChainSpecProvider;
use reth_evm::{
    ConfigureEvm, Database, Evm, InvalidTxError, NextBlockEnvAttributes,
    block::{BlockExecutionError, BlockValidationError},
    execute::{BlockBuilder, BlockBuilderOutcome},
};
use reth_execution_cache::{CachedStateMetrics, CachedStateMetricsSource, CachedStateProvider};
use reth_execution_types::BlockExecutionOutput;
use reth_payload_builder::PayloadId;
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_payload_util::{BestPayloadTransactions, NoopPayloadTransactions, PayloadTransactions};
use reth_primitives_traits::{FastInstant as Instant, RecoveredBlock, SealedHeader};
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_storage_api::{StateProvider, StateProviderFactory};
use reth_transaction_pool::{BestTransactionsAttributes, PoolTransaction, TransactionPool};
use revm::context_interface::Block as RevmBlock;
use std::collections::HashSet;
use std::sync::Arc;

/// Returns true when `error` wraps a deterministic Morph token-fee error that cannot
/// succeed without an on-chain configuration or token-contract state change, so a failing
/// pool transaction should be evicted rather than left in `Pending` to burn up to 200k gas
/// on every subsequent block-building tick.
///
/// Explicitly excludes [`MorphInvalidTransaction::InsufficientTokenBalance`], which is a
/// normal balance shortfall (and may be caused by an earlier transaction in the same block).
fn is_evictable_token_fee_error(error: &dyn InvalidTxError) -> bool {
    let Some(morph_err) = (error as &dyn std::any::Any).downcast_ref::<MorphInvalidTransaction>()
    else {
        return false;
    };
    matches!(
        morph_err,
        MorphInvalidTransaction::TokenTransferFailed { .. }
            | MorphInvalidTransaction::TokenBalanceQueryFailed
            | MorphInvalidTransaction::TokenNotRegistered(_)
            | MorphInvalidTransaction::TokenNotActive(_)
            | MorphInvalidTransaction::InvalidTokenConfig(_)
    )
}

/// Validates MorphTx (0x7F) hardfork activation gates and structural field rules against
/// the target block timestamp, matching `MorphConsensus::validate_block_pre_execution`.
fn validate_morph_tx_for_block(
    tx: &MorphTxEnvelope,
    chain_spec: &MorphChainSpec,
    block_timestamp: u64,
) -> Result<(), String> {
    let MorphTxEnvelope::Morph(signed) = tx else {
        return Ok(());
    };
    let morph_tx = signed.tx();

    if !chain_spec.is_emerald_active_at_timestamp(block_timestamp) {
        return Err("MorphTx type is not yet active (emerald fork not reached)".into());
    }
    if !chain_spec.is_jade_active_at_timestamp(block_timestamp)
        && morph_tx.version == MORPH_TX_VERSION_1
    {
        return Err("MorphTx version 1 is not yet active (jade fork not reached)".into());
    }
    if !chain_spec.is_celadon_active_at_timestamp(block_timestamp)
        && morph_tx.version == MORPH_TX_VERSION_2
    {
        return Err("MorphTx version 2 is not yet active (celadon fork not reached)".into());
    }
    morph_tx.validate().map_err(str::to_string)
}

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
/// 1. the transactions supplied in the payload attributes, in order
/// 2. the best pool transactions — unless the attributes set `no_tx_pool`
///
/// Sequencer assembly supplies only L1 messages in step 1 and relies on step 2 for L2
/// transactions, matching go-ethereum's `AssembleL2Block`. Derivation sets `no_tx_pool` and
/// supplies the complete committed transaction list, matching go-ethereum's `NewSafeL2Block`,
/// which executes the block through `BlockChain.ProcessBlock` rather than the miner.
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
        args: BuildArguments<MorphPayloadAttributes, MorphBuiltPayload>,
        best: impl FnOnce(BestTransactionsAttributes) -> BestTxs + Send + Sync + 'a,
    ) -> Result<BuildOutcome<MorphBuiltPayload>, PayloadBuilderError>
    where
        BestTxs:
            PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>> + 'a,
    {
        let BuildArguments {
            mut cached_reads,
            execution_cache,
            state_root_handle,
            config,
            cancel,
            best_payload,
        } = args;

        // Convert RPC-level MorphPayloadAttributes to builder-level MorphPayloadBuilderAttributes
        let parent_hash = config.parent_header.hash();
        let payload_id = config.payload_id;
        let parent_header = config.parent_header.clone();
        let parent_block_info = config.parent_block_info;
        let builder_attrs = MorphPayloadBuilderAttributes::try_new(
            parent_hash,
            config.attributes,
            morph_payload_types::MORPH_PAYLOAD_BUILDER_VERSION,
        )
        .map_err(|e| PayloadBuilderError::Other(e.into()))?;
        let builder_config = PayloadConfig {
            parent_header,
            parent_block_info,
            attributes: builder_attrs,
            payload_id,
        };

        let ctx = MorphPayloadBuilderCtx {
            evm_config: self.evm_config.clone(),
            config: builder_config,
            cancel,
            best_payload,
            builder_config: self.config.clone(),
            metrics: MorphPayloadBuilderMetrics::default(),
        };

        // When `--engine.share-execution-cache-with-payload-builder` is set,
        // reth's engine provides a SavedCache snapshot associated with the parent
        // block. Wrap the state provider so account/storage/code reads consult
        // the cache before hitting the DB — amortizes cross-block cost when the
        // payload builder and engine both touch overlapping state.
        let mut state_provider: Box<dyn StateProvider> =
            self.client.state_by_block_hash(ctx.parent().hash())?;
        if let Some(execution_cache) = execution_cache {
            // reth v2.2.0 dropped `SavedCache::metrics`; the canonical pattern
            // (see `reth-ethereum-payload`) is to materialize a fresh zeroed
            // metrics handle on every payload build — cheap because morph-reth
            // builds payloads on demand rather than every 12s like upstream.
            state_provider = Box::new(CachedStateProvider::new(
                state_provider,
                execution_cache.cache().clone(),
                Some(CachedStateMetrics::zeroed(
                    CachedStateMetricsSource::Builder,
                )),
            ));
        }
        let state = StateProviderDatabase::new(state_provider.as_ref());

        // Reuse cached reads from previous runs for incremental payload building
        build_payload_inner(
            cached_reads.as_db_mut(state),
            state_provider.as_ref(),
            &self.pool,
            ctx,
            best,
            state_root_handle,
        )
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
    type Attributes = MorphPayloadAttributes;
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
            execution_cache: None,
            state_root_handle: None,
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
    /// Prometheus metrics for this payload build job.
    metrics: MorphPayloadBuilderMetrics,
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

    /// Executes the transactions supplied in the payload attributes, in order.
    ///
    /// Under sequencer assembly these are the L1 messages from the L1 bridge, which must be
    /// executed first and carry strictly sequential queue indices. Under a deterministic build
    /// (`no_tx_pool`) the list is the complete committed block, so it may also contain L2
    /// transactions after the L1 messages; those are charged fees normally.
    ///
    /// Behaviour when a transaction does not fit the remaining block gas depends on the policy:
    ///
    /// - assembly: packing stops and the leftover messages roll over to the next block via
    ///   `next_l1_msg_index`. A single message larger than the whole block gas limit is skipped
    ///   for this height (not included, index not advanced) so assemble still returns a block.
    /// - deterministic build: this is a hard error, mirroring go-ethereum's `ProcessBlock`
    ///   returning `ErrGasLimitReached` and rejecting the whole block.
    ///
    /// Returns the executed transaction bytes for inclusion in ExecutableL2Data.
    fn execute_supplied_transactions(
        &self,
        builder: &mut impl BlockBuilder<Primitives = morph_primitives::MorphPrimitives>,
        info: &mut ExecutionInfo,
    ) -> Result<Vec<Bytes>, PayloadBuilderError> {
        let block_gas_limit = builder.evm().block().gas_limit();
        let base_fee = builder.evm().block().basefee();
        let supplied_tx_count = self.attributes().transactions.len();
        let mut executed_txs: Vec<Bytes> = Vec::with_capacity(supplied_tx_count);
        let mut saw_l2_transaction = false;

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

            // L1 messages must precede every L2 transaction and carry strictly sequential
            // queue indices. Validate ordering, queue index, overflow, and MorphTx hardfork /
            // structural gates *before* EVM execution so invalid supplied transactions are
            // rejected cleanly without mutating block state.
            let is_l1_msg = recovered_tx.is_l1_msg();
            let next_l1_message_index = if is_l1_msg {
                if saw_l2_transaction {
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::L1MessageAfterRegularTx,
                    ));
                }
                let queue_index = recovered_tx.queue_index().ok_or_else(|| {
                    PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction {
                            error: "L1 message transaction is missing queue index".to_string(),
                        },
                    )
                })?;
                if queue_index != info.next_l1_message_index {
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction {
                            error: format!(
                                "invalid L1 message queue index: expected {}, got {}",
                                info.next_l1_message_index, queue_index
                            ),
                        },
                    ));
                }
                let next_index = queue_index.checked_add(1).ok_or_else(|| {
                    PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction {
                            error: format!("L1 message queue index overflow: {queue_index}"),
                        },
                    )
                })?;
                Some(next_index)
            } else {
                saw_l2_transaction = true;
                if let Err(reason) = validate_morph_tx_for_block(
                    recovered_tx.inner(),
                    self.evm_config.chain_spec(),
                    self.attributes().timestamp,
                ) {
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction { error: reason },
                    ));
                }
                None
            };

            let tx_gas = recovered_tx.gas_limit();

            // Match morph-geth: stop L1 packing when the next message does not fit
            // remaining gas, and still seal the block with what already fits.
            // L1 messages are excluded from DA payload size (prepaid on L1).
            if info.is_tx_over_limits(tx_gas, 0, block_gas_limit) {
                // A deterministic build must reproduce the committed block exactly. Dropping
                // the tail would silently seal a *different* block, and since SafeL2Data
                // carries no expected hash to check against, the divergence would only
                // surface many blocks later. Reject instead, as go-ethereum's ProcessBlock
                // does via ErrGasLimitReached.
                if self.attributes().no_tx_pool {
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::BlockGasLimitExceeded {
                            tx_index: tx_idx,
                            tx_gas,
                            gas_pool_used: info.gas_pool_used,
                            block_gas_limit,
                        },
                    ));
                }

                if info.transaction_count == 0 {
                    tracing::warn!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        tx_gas,
                        block_gas_limit,
                        "Single L1 message gas limit exceeded for current block"
                    );
                } else {
                    tracing::debug!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        tx_gas,
                        cumulative_gas_used = info.cumulative_gas_used,
                        gas_pool_used = info.gas_pool_used,
                        block_gas_limit,
                        "L1 message would exceed remaining block gas; stopping L1 packing"
                    );
                }
                break;
            }

            // Execute the transaction and record EVM execution time.
            let apply_started = Instant::now();
            // `BlockBuilder::execute_transaction` returns alloy-evm's `GasOutput`;
            // pre-Amsterdam morph treats regular and state gas as a single number,
            // so collapse to `tx_gas_used()` immediately.
            let gas_used = match builder.execute_transaction(recovered_tx.clone()) {
                Ok(gas_output) => gas_output.tx_gas_used(),
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    tracing::warn!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        %error,
                        ?recovered_tx,
                        "invalid supplied transaction in payload attributes"
                    );
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction {
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
                        "validation error in supplied transaction"
                    );
                    return Err(PayloadBuilderError::other(
                        MorphPayloadBuilderError::InvalidSuppliedTransaction {
                            error: err.to_string(),
                        },
                    ));
                }
                Err(err) => {
                    // Fatal error - this is a bug or misconfiguration
                    tracing::error!(
                        target: "payload_builder",
                        tx_index = tx_idx,
                        %err,
                        ?recovered_tx,
                        "fatal EVM execution error on supplied transaction"
                    );
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };
            self.metrics
                .commit_tx_apply_duration_seconds
                .record(apply_started.elapsed());

            // For L1 messages, advance the next L1 message index.
            // L1 gas is prepaid on L1, so no fees are collected here.
            if let Some(next_index) = next_l1_message_index {
                info.next_l1_message_index = next_index;
            } else {
                // Calculate fees for L2 transactions: effective_tip * gas_used
                let effective_tip = recovered_tx
                    .effective_tip_per_gas(base_fee)
                    .unwrap_or_default();
                info.total_fees += U256::from(effective_tip) * U256::from(gas_used);
            }

            info.cumulative_gas_used += gas_used;
            // Morph geth reports actual L1 execution gas in receipts but does not return unused
            // L1-message gas to the block gas pool. Regular transactions do return unused gas.
            info.gas_pool_used += if is_l1_msg { tx_gas } else { gas_used };

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
    fn execute_pool_transactions<BestTxs, Pool>(
        &self,
        builder: &mut impl BlockBuilder<Primitives = morph_primitives::MorphPrimitives>,
        info: &mut ExecutionInfo,
        executed_txs: &mut Vec<Bytes>,
        best_txs: BestTxs,
        pool: &Pool,
        breaker: &PayloadBuildingBreaker,
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        BestTxs: PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
        Pool: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
    {
        self.execute_pool_transactions_with_evictor(
            builder,
            info,
            executed_txs,
            best_txs,
            breaker,
            |tx_hash| {
                pool.remove_transactions(vec![tx_hash]);
            },
        )
    }

    fn execute_pool_transactions_with_evictor<BestTxs>(
        &self,
        builder: &mut impl BlockBuilder<Primitives = morph_primitives::MorphPrimitives>,
        info: &mut ExecutionInfo,
        executed_txs: &mut Vec<Bytes>,
        mut best_txs: BestTxs,
        breaker: &PayloadBuildingBreaker,
        mut evict_from_pool: impl FnMut(B256),
    ) -> Result<Option<()>, PayloadBuilderError>
    where
        BestTxs: PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
    {
        let block_gas_limit = builder.evm().block().gas_limit();
        let base_fee = builder.evm().block().basefee();
        let mut executed_senders: HashSet<Address> = HashSet::new();

        while let Some(tx) = best_txs.next(()) {
            // Check if the job was cancelled
            if self.cancel.is_cancelled() {
                return Ok(Some(()));
            }

            // Check if the breaker triggers (time, gas, or DA limits)
            if breaker.should_break(info.gas_pool_used, info.cumulative_da_bytes_used) {
                tracing::debug!(
                    target: "payload_builder",
                    cumulative_gas_used = info.cumulative_gas_used,
                    gas_pool_used = info.gas_pool_used,
                    cumulative_da_bytes_used = info.cumulative_da_bytes_used,
                    transaction_count = info.transaction_count,
                    elapsed = ?breaker.elapsed(),
                    "breaker triggered, stopping pool transaction execution"
                );
                return Ok(Some(()));
            }

            let tx = tx.into_consensus();

            // Skip blob transactions and L1 messages from pool. These should
            // never reach the pool under normal operation (pool filters them
            // out at admission), so their presence here indicates either a
            // bug in the admission path or a node configuration mismatch —
            // warn loudly.
            if tx.is_eip4844() || tx.is_l1_msg() {
                tracing::warn!(
                    target: "payload_builder",
                    signer = %tx.signer(),
                    nonce = tx.nonce(),
                    is_blob = tx.is_eip4844(),
                    is_l1_msg = tx.is_l1_msg(),
                    "unexpected blob or L1-message transaction in the pool; skipping"
                );
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // Enforce MorphTx hardfork gates and structural validation against the target
            // block's timestamp (a reorg build via assembleL2BlockV2 may target an older
            // fork than the canonical head at which the pool admitted the tx).
            if let Err(reason) = validate_morph_tx_for_block(
                tx.inner(),
                self.evm_config.chain_spec(),
                self.attributes().timestamp,
            ) {
                tracing::debug!(
                    target: "payload_builder",
                    signer = %tx.signer(),
                    nonce = tx.nonce(),
                    block_timestamp = self.attributes().timestamp,
                    %reason,
                    "skipping pool MorphTx invalid for target block timestamp"
                );
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            // Skip transactions that cannot fit in remaining block gas or DA size.
            let tx_size = tx.encode_2718_len() as u64;
            if info.is_tx_over_limits(tx.gas_limit(), tx_size, block_gas_limit) {
                tracing::debug!(
                    target: "payload_builder",
                    signer = %tx.signer(),
                    nonce = tx.nonce(),
                    tx_gas_limit = tx.gas_limit(),
                    tx_size,
                    block_gas_limit,
                    max_da_block_size = self.builder_config.max_da_block_size,
                    "pool transaction exceeds remaining block gas or DA size; skipping"
                );
                best_txs.mark_invalid(tx.signer(), tx.nonce());
                continue;
            }

            let apply_started = Instant::now();
            // Same reasoning as the supplied-transaction branch above: collapse `GasOutput`
            // into a single u64 since we are still pre-Amsterdam.
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_output) => gas_output.tx_gas_used(),
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    // These three variants fire on the fast path of every
                    // pool sweep and can be extremely noisy under load. Keep
                    // them at `trace` so default operation stays quiet; turn
                    // on `RUST_LOG=morph_payload_builder=trace` to diagnose
                    // pool-skip rates.
                    if error.is_nonce_too_low() {
                        // Nonce too low: sender may have other valid txs.
                        tracing::trace!(
                            target: "payload_builder",
                            %error,
                            ?tx,
                            "skipping nonce too low transaction"
                        );
                    } else {
                        // Other invalid: skip this tx AND its descendants.
                        tracing::trace!(
                            target: "payload_builder",
                            %error,
                            ?tx,
                            "skipping invalid transaction and its descendants"
                        );
                        best_txs.mark_invalid(tx.signer(), tx.nonce());
                        if is_evictable_token_fee_error(error.as_ref())
                            && !executed_senders.contains(&tx.signer())
                        {
                            tracing::debug!(
                                target: "payload_builder",
                                tx_hash = %tx.tx_hash(),
                                signer = %tx.signer(),
                                nonce = tx.nonce(),
                                %error,
                                "evicting pool transaction after deterministic token-fee failure"
                            );
                            evict_from_pool(*tx.tx_hash());
                        }
                    }
                    continue;
                }
                Err(BlockExecutionError::Validation(err)) => {
                    // Other validation errors - skip transaction and descendants.
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
                    // Fatal error - should not continue.
                    tracing::error!(
                        target: "payload_builder",
                        signer = %tx.signer(),
                        nonce = tx.nonce(),
                        %err,
                        "fatal EVM execution error on pool transaction; aborting build"
                    );
                    return Err(PayloadBuilderError::EvmExecutionError(Box::new(err)));
                }
            };
            self.metrics
                .commit_tx_apply_duration_seconds
                .record(apply_started.elapsed());
            executed_senders.insert(tx.signer());

            // Update execution info
            info.cumulative_gas_used += gas_used;
            info.gas_pool_used += gas_used;
            info.cumulative_da_bytes_used += tx_size;
            info.transaction_count += 1;

            // Calculate fees: effective_tip * gas_used
            let effective_tip = tx.effective_tip_per_gas(base_fee).unwrap_or_default();
            info.total_fees += U256::from(effective_tip) * U256::from(gas_used);

            // Store the transaction bytes for ExecutableL2Data
            let mut tx_bytes = Vec::with_capacity(tx.encode_2718_len());
            tx.encode_2718(&mut tx_bytes);
            executed_txs.push(Bytes::from(tx_bytes));
        }

        Ok(None)
    }
}

/// Execution information collected during payload building.
#[derive(Debug, Default)]
struct ExecutionInfo {
    /// Cumulative gas reported in transaction receipts and the block header.
    cumulative_gas_used: u64,
    /// Gas unavailable to later transactions under Morph geth's block gas-pool rules.
    ///
    /// L1 messages contribute their full gas limit here, while regular transactions contribute
    /// their actual gas used.
    gas_pool_used: u64,
    /// Cumulative encoded L2 transaction bytes counted toward the DA packing cap.
    /// L1 messages are not included.
    cumulative_da_bytes_used: u64,
    /// Total fees collected from executed transactions.
    total_fees: U256,
    /// Next L1 message queue index.
    next_l1_message_index: u64,
    /// Number of transactions executed (including both sequencer and pool transactions).
    transaction_count: u64,
    /// Maximum DA block size from the builder config.
    max_da_block_size: Option<u64>,
}

impl ExecutionInfo {
    /// Creates a new [`ExecutionInfo`] with the initial next L1 message index from parent.
    const fn new(next_l1_message_index: u64, max_da_block_size: Option<u64>) -> Self {
        Self {
            cumulative_gas_used: 0,
            gas_pool_used: 0,
            cumulative_da_bytes_used: 0,
            total_fees: U256::ZERO,
            next_l1_message_index,
            transaction_count: 0,
            max_da_block_size,
        }
    }

    /// Returns true if the transaction would exceed remaining block gas or DA size.
    ///
    /// An overflowing sum counts as over the limit: wrapping would otherwise let a
    /// transaction with an absurd gas limit through and produce an invalid block.
    fn is_tx_over_limits(&self, tx_gas_limit: u64, tx_size: u64, block_gas_limit: u64) -> bool {
        if self
            .gas_pool_used
            .checked_add(tx_gas_limit)
            .is_none_or(|total_gas| total_gas > block_gas_limit)
        {
            return true;
        }

        if let Some(da_limit) = self.max_da_block_size {
            return self
                .cumulative_da_bytes_used
                .checked_add(tx_size)
                .is_none_or(|total_da| total_da > da_limit);
        }

        false
    }
}

/// Builds the payload on top of the state.
fn build_payload_inner<'a, DB, BestTxs, Pool>(
    db: DB,
    state_provider: &(impl StateProvider + ?Sized),
    pool: &Pool,
    ctx: MorphPayloadBuilderCtx,
    best: impl FnOnce(BestTransactionsAttributes) -> BestTxs + Send + Sync + 'a,
    mut state_root_handle: Option<reth_trie_parallel::state_root_task::PayloadStateRootHandle>,
) -> Result<BuildOutcomeKind<MorphBuiltPayload>, PayloadBuilderError>
where
    DB: Database<Error = reth_evm::execute::ProviderError>,
    BestTxs: PayloadTransactions<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>> + 'a,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>,
{
    let build_started = Instant::now();
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
            timestamp: attributes.timestamp,
            suggested_fee_recipient: attributes.suggested_fee_recipient,
            prev_randao: attributes.prev_randao,
            gas_limit: ctx
                .builder_config
                .next_header_gas_limit(ctx.parent().gas_limit(), attributes.gas_limit),
            withdrawals: Some(attributes.withdrawals.clone()),
            parent_beacon_block_root: attributes.parent_beacon_block_root,
            extra_data: Default::default(),
            // Morph L2 has no PoS slot semantics; field added in alloy 2.0.
            slot_number: None,
        },
        base_fee_per_gas: attributes.base_fee_per_gas,
    };

    // Create block builder
    let mut builder = ctx
        .evm_config
        .builder_for_next_block(&mut db, ctx.parent(), next_block_attrs)
        .map_err(PayloadBuilderError::other)?;

    // If the engine tree provided a sparse-trie state root handle, wire the
    // state hook so per-tx state diffs stream to the background trie task
    // during execution. The final `state_root()` recv() at finish time will
    // return quickly since most work is done concurrently.
    if let Some(handle) = state_root_handle.as_mut() {
        builder
            .evm_mut()
            .db_mut()
            .set_state_hook(Some(Box::new(handle.take_state_hook())));
    }

    // 1. Apply pre-execution changes (system contracts, etc.)
    builder.apply_pre_execution_changes().map_err(|err| {
        tracing::warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
        PayloadBuilderError::Internal(err.into())
    })?;

    // Initialize next_l1_message_index from parent header
    let mut info = ExecutionInfo::new(
        ctx.parent().next_l1_msg_index,
        ctx.builder_config.max_da_block_size,
    );
    let base_fee = builder.evm().block().basefee();
    let block_gas_limit = builder.evm().block().gas_limit();

    // Create breaker for early exit from pool transaction execution
    let breaker = ctx.builder_config.breaker(block_gas_limit);

    // Execute the supplied transactions (L1 messages must form a sequential prefix).
    let txs_all_started = Instant::now();
    let mut executed_txs = ctx.execute_supplied_transactions(&mut builder, &mut info)?;

    // Append the best pool transactions, unless the caller demanded a deterministic build.
    //
    // Derivation (`engine_newSafeL2Block`) sets `no_tx_pool`: a follower's pool holds
    // gossiped transactions that the sequencer committed to *later* blocks, so appending
    // them here would fork the follower off the sequencer chain.
    if ctx.attributes().include_tx_pool() {
        let best_txs = best(ctx.best_transaction_attributes(base_fee));
        if ctx
            .execute_pool_transactions(
                &mut builder,
                &mut info,
                &mut executed_txs,
                best_txs,
                pool,
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
                gas_pool_used = info.gas_pool_used,
                cumulative_da_bytes_used = info.cumulative_da_bytes_used,
                tx_count = executed_txs.len(),
                "breaker stopped pool execution, finalizing payload"
            );
        }
    } else {
        tracing::debug!(
            target: "payload_builder",
            tx_count = executed_txs.len(),
            "skipping txpool inclusion: deterministic build requested"
        );
    }

    // Record total transaction execution time.
    ctx.metrics
        .commit_txs_all_duration_seconds
        .record(txs_all_started.elapsed());

    // Check if this payload is better than the previous one
    if !ctx.is_better_payload(info.total_fees) {
        return Ok(BuildOutcomeKind::Aborted {
            fees: info.total_fees,
        });
    }

    // Read withdraw_trie_root from L2MessageQueue contract storage
    // This must be done before finish() consumes the builder
    let withdraw_trie_root =
        read_withdraw_trie_root(builder.evm_mut().db_mut()).map_err(|err| {
            PayloadBuilderError::other(MorphPayloadBuilderError::Storage(err.to_string()))
        })?;

    // 6. Finish building the block.
    //
    // When `state_root_handle` is provided, drop the state hook to signal FinishedStateUpdates
    // to the background sparse trie task (via `StateRootUpdateHook`'s Drop impl), then wait for
    // the final root. Fall back to synchronous state root if the task fails.
    let BlockBuilderOutcome {
        execution_result,
        hashed_state,
        trie_updates,
        mut block,
        block_access_list: _,
    } = if let Some(mut handle) = state_root_handle {
        builder.evm_mut().db_mut().set_state_hook(None);
        match handle.state_root() {
            Ok(outcome) => builder.finish(
                state_provider,
                Some((
                    outcome.state_root,
                    Arc::unwrap_or_clone(outcome.trie_updates),
                )),
            )?,
            Err(err) => {
                tracing::warn!(
                    target: "payload_builder",
                    id = %ctx.payload_id(),
                    %err,
                    "sparse trie task failed, falling back to sync state root",
                );
                builder.finish(state_provider, None)?
            }
        }
    } else {
        builder.finish(state_provider, None)?
    };

    // Update MorphHeader with next_l1_msg_index.
    // Since hash_slow() only hashes the inner header, we can update the
    // MorphHeader's L2-specific fields without changing the block hash.
    let (mut morph_block, senders) = block.split();
    morph_block = morph_block.map_header(|mut header: MorphHeader| {
        header.next_l1_msg_index = info.next_l1_message_index;
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
    let has_transactions = !executed_txs.is_empty();

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

    // reth inserts every resolved payload into the engine tree as an already-executed
    // block, and the tree only prunes such non-canonical blocks below a finalized block.
    // A sequencer assembles a candidate on every fast tick, discards the empty ones
    // without telling the EL, and never receives a finalized tag, so pre-inserted empty
    // candidates would stay in memory for the life of the process. Empty blocks
    // therefore carry no execution artifacts: the ones the CL does commit are executed
    // again on import, which costs nothing for a block without transactions. Blocks
    // with transactions keep the artifacts so their import remains a no-op.
    let executed = has_transactions.then(|| {
        let execution_output = BlockExecutionOutput {
            result: execution_result,
            state: db.take_bundle(),
        };
        BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block),
            execution_output: Arc::new(execution_output),
            // Keep unsorted; conversion to sorted is deferred until required.
            hashed_state: Arc::new(hashed_state),
            trie_updates: Arc::new(trie_updates),
        }
    });

    let payload = MorphBuiltPayload::new(
        ctx.payload_id(),
        sealed_block,
        info.total_fees,
        executable_data,
        executed,
    );

    // Only record block_transactions for successfully built payloads (not Aborted or Cancelled).
    ctx.metrics
        .block_transactions
        .set(info.transaction_count as f64);
    ctx.metrics
        .payload_build_duration_seconds
        .record(build_started.elapsed());

    Ok(BuildOutcomeKind::Better { payload })
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // ExecutionInfo tests
    // =========================================================================

    #[test]
    fn test_execution_info_default() {
        let info = ExecutionInfo::default();
        assert_eq!(info.cumulative_gas_used, 0);
        assert_eq!(info.gas_pool_used, 0);
        assert_eq!(info.cumulative_da_bytes_used, 0);
        assert_eq!(info.total_fees, U256::ZERO);
        assert_eq!(info.next_l1_message_index, 0);
        assert_eq!(info.transaction_count, 0);
        assert_eq!(info.max_da_block_size, None);
    }

    #[test]
    fn test_execution_info_new_with_l1_index() {
        let info = ExecutionInfo::new(42, Some(720 * 1024));
        assert_eq!(info.next_l1_message_index, 42);
        assert_eq!(info.cumulative_gas_used, 0);
        assert_eq!(info.gas_pool_used, 0);
        assert_eq!(info.cumulative_da_bytes_used, 0);
        assert_eq!(info.total_fees, U256::ZERO);
        assert_eq!(info.transaction_count, 0);
        assert_eq!(info.max_da_block_size, Some(720 * 1024));
    }

    #[test]
    fn test_execution_info_new_with_zero_index() {
        let info = ExecutionInfo::new(0, None);
        assert_eq!(info.next_l1_message_index, 0);
    }

    #[test]
    fn test_execution_info_new_with_max_index() {
        let info = ExecutionInfo::new(u64::MAX, None);
        assert_eq!(info.next_l1_message_index, u64::MAX);
    }

    // =========================================================================
    // is_tx_over_limits tests
    // =========================================================================

    #[test]
    fn test_is_tx_over_limits_within_gas() {
        let info = ExecutionInfo {
            gas_pool_used: 100_000,
            ..Default::default()
        };
        // tx_gas + gas_pool_used = 100_000 + 21_000 = 121_000, block limit = 30_000_000
        assert!(!info.is_tx_over_limits(21_000, 100, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_exceeds_gas_limit() {
        let info = ExecutionInfo {
            gas_pool_used: 29_990_000,
            ..Default::default()
        };
        // tx_gas + gas_pool_used = 29_990_000 + 21_000 = 30_011_000 > 30_000_000
        assert!(info.is_tx_over_limits(21_000, 100, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_exactly_at_gas_limit() {
        let info = ExecutionInfo {
            gas_pool_used: 29_979_000,
            ..Default::default()
        };
        // tx_gas + gas_pool_used = 29_979_000 + 21_000 = 30_000_000 == block limit
        // Uses > comparison, so exactly at limit is NOT over
        assert!(!info.is_tx_over_limits(21_000, 100, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_one_over_gas_limit() {
        let info = ExecutionInfo {
            gas_pool_used: 29_979_001,
            ..Default::default()
        };
        // tx_gas + gas_pool_used = 29_979_001 + 21_000 = 30_000_001 > 30_000_000
        assert!(info.is_tx_over_limits(21_000, 100, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_zero_gas_tx() {
        let info = ExecutionInfo::default();
        assert!(!info.is_tx_over_limits(0, 0, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_zero_block_gas_limit() {
        let info = ExecutionInfo::default();
        assert!(info.is_tx_over_limits(1, 0, 0));
        // 0 > 0 is false
        assert!(!info.is_tx_over_limits(0, 0, 0));
    }

    #[test]
    fn test_is_tx_over_limits_gas_sum_overflow() {
        let info = ExecutionInfo {
            gas_pool_used: 1,
            ..Default::default()
        };
        // Wrapping would yield 0 and wrongly report "fits"; overflow must count as over.
        assert!(info.is_tx_over_limits(u64::MAX, 0, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_exceeds_da_limit() {
        let info = ExecutionInfo {
            cumulative_da_bytes_used: 700_000,
            max_da_block_size: Some(720 * 1024),
            ..Default::default()
        };
        // 700_000 + 40_000 = 740_000 > 737_280
        assert!(info.is_tx_over_limits(21_000, 40_000, 30_000_000));
        // 700_000 + 10_000 = 710_000 < 737_280
        assert!(!info.is_tx_over_limits(21_000, 10_000, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_da_limit_none_ignores_da() {
        let info = ExecutionInfo {
            cumulative_da_bytes_used: u64::MAX,
            max_da_block_size: None,
            ..Default::default()
        };
        assert!(!info.is_tx_over_limits(21_000, 1_000, 30_000_000));
    }

    #[test]
    fn test_is_tx_over_limits_da_sum_overflow() {
        let info = ExecutionInfo {
            cumulative_da_bytes_used: 1,
            max_da_block_size: Some(720 * 1024),
            ..Default::default()
        };
        assert!(info.is_tx_over_limits(21_000, u64::MAX, 30_000_000));
    }

    // =========================================================================
    // MorphPayloadBuilder constructor tests
    // =========================================================================

    fn test_evm_config() -> MorphEvmConfig {
        MorphEvmConfig::new_with_default_factory(morph_chainspec::MORPH_MAINNET.clone())
    }

    #[test]
    fn test_morph_payload_builder_new_default_config() {
        let builder = MorphPayloadBuilder::<(), ()>::new((), test_evm_config(), ());
        assert_eq!(builder.config, MorphBuilderConfig::default());
    }

    #[test]
    fn test_morph_payload_builder_with_config() {
        let config = MorphBuilderConfig::default().with_gas_limit(10_000_000);
        let builder =
            MorphPayloadBuilder::<(), ()>::with_config((), test_evm_config(), (), config.clone());
        assert_eq!(builder.config, config);
    }

    #[test]
    fn test_morph_payload_builder_set_config() {
        let builder = MorphPayloadBuilder::<(), ()>::new((), test_evm_config(), ());
        let config = MorphBuilderConfig::default().with_gas_limit(5_000_000);
        let builder = builder.set_config(config.clone());
        assert_eq!(builder.config, config);
    }

    // =========================================================================
    // MorphPayloadBuilderCtx helper tests
    // =========================================================================

    fn test_ctx(best_payload: Option<MorphBuiltPayload>) -> MorphPayloadBuilderCtx {
        let attrs = MorphPayloadBuilderAttributes::try_new(
            B256::ZERO,
            morph_payload_types::MorphPayloadAttributes::default(),
            morph_payload_types::MORPH_PAYLOAD_BUILDER_VERSION,
        )
        .unwrap();
        let payload_id = attrs.payload_id();
        MorphPayloadBuilderCtx {
            evm_config: test_evm_config(),
            config: PayloadConfig::new(
                Arc::new(SealedHeader::seal_slow(MorphHeader::default())),
                attrs,
                payload_id,
            ),
            cancel: Default::default(),
            best_payload,
            builder_config: MorphBuilderConfig::default(),
            metrics: MorphPayloadBuilderMetrics::default(),
        }
    }

    #[test]
    fn test_best_transaction_attributes() {
        let ctx = test_ctx(None);
        let attrs = ctx.best_transaction_attributes(7_000_000_000);
        assert_eq!(attrs.basefee, 7_000_000_000);
        assert!(attrs.blob_fee.is_none());
    }

    #[test]
    fn test_is_better_payload_no_previous() {
        let ctx = test_ctx(None);
        assert!(ctx.is_better_payload(U256::ZERO));
        assert!(ctx.is_better_payload(U256::from(100)));
    }

    #[test]
    fn test_payload_id_is_deterministic() {
        let ctx = test_ctx(None);
        let id1 = ctx.payload_id();
        let id2 = ctx.payload_id();
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_parent_returns_correct_header() {
        let ctx = test_ctx(None);
        assert_eq!(ctx.parent().number(), 0);
    }

    // =========================================================================
    // read_withdraw_trie_root tests (requires mock DB)
    // =========================================================================

    struct MockDb {
        storage_value: U256,
    }

    impl revm::Database for MockDb {
        type Error = std::convert::Infallible;

        fn basic(
            &mut self,
            _address: alloy_primitives::Address,
        ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash(
            &mut self,
            _code_hash: B256,
        ) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Ok(revm::bytecode::Bytecode::default())
        }

        fn storage(
            &mut self,
            _address: alloy_primitives::Address,
            _index: U256,
        ) -> Result<U256, Self::Error> {
            Ok(self.storage_value)
        }

        fn block_hash(&mut self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    #[test]
    fn test_read_withdraw_trie_root_zero() {
        let mut db = MockDb {
            storage_value: U256::ZERO,
        };
        let root = read_withdraw_trie_root(&mut db).unwrap();
        assert_eq!(root, B256::ZERO);
    }

    #[test]
    fn test_read_withdraw_trie_root_nonzero() {
        let expected = B256::from([0xAB; 32]);
        let mut db = MockDb {
            storage_value: expected.into(),
        };
        let root = read_withdraw_trie_root(&mut db).unwrap();
        assert_eq!(root, expected);
    }

    #[test]
    fn test_read_withdraw_trie_root_max_value() {
        let mut db = MockDb {
            storage_value: U256::MAX,
        };
        let root = read_withdraw_trie_root(&mut db).unwrap();
        assert_eq!(root, B256::from(U256::MAX));
    }

    // =========================================================================
    // Shared scripted test helpers for M-H09, M-H10, and M-M12
    // =========================================================================

    struct ScriptedBlockBuilder {
        evm: <morph_evm::MorphEvmFactory as reth_evm::EvmFactory>::Evm<
            revm::database::CacheDB<revm::database::EmptyDB>,
            revm::inspector::NoOpInspector,
        >,
        outcomes: std::collections::HashMap<B256, Result<u64, MorphInvalidTransaction>>,
        executed_count: usize,
    }

    impl ScriptedBlockBuilder {
        fn new(
            outcomes: std::collections::HashMap<B256, Result<u64, MorphInvalidTransaction>>,
        ) -> Self {
            use reth_evm::{EvmEnv, EvmFactory as _};
            let mut evm_env = EvmEnv::new(
                revm::context::CfgEnv::new_with_spec(morph_chainspec::MorphHardfork::Celadon),
                morph_evm::MorphBlockEnv {
                    inner: revm::context::BlockEnv {
                        gas_limit: 30_000_000,
                        basefee: 1,
                        ..Default::default()
                    },
                },
            );
            evm_env.cfg_env.chain_id = 2818;
            let evm = morph_evm::MorphEvmFactory::default().create_evm(
                revm::database::CacheDB::new(revm::database::EmptyDB::default()),
                evm_env,
            );
            Self {
                evm,
                outcomes,
                executed_count: 0,
            }
        }
    }

    struct DummyExecutor;
    impl reth_evm::block::BlockExecutor for DummyExecutor {
        type Transaction = MorphTxEnvelope;
        type Receipt = morph_primitives::MorphReceipt;
        type Evm = <morph_evm::MorphEvmFactory as reth_evm::EvmFactory>::Evm<
            revm::database::CacheDB<revm::database::EmptyDB>,
            revm::inspector::NoOpInspector,
        >;
        type Result = morph_evm::MorphTxResult;

        fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
            Ok(())
        }
        fn execute_transaction_without_commit(
            &mut self,
            _tx: impl reth_evm::block::ExecutableTx<Self>,
        ) -> Result<Self::Result, BlockExecutionError> {
            unreachable!()
        }
        fn commit_transaction(&mut self, _output: Self::Result) -> reth_evm::execute::GasOutput {
            unreachable!()
        }
        fn finish(
            self,
        ) -> Result<
            (
                Self::Evm,
                reth_evm::block::BlockExecutionResult<Self::Receipt>,
            ),
            BlockExecutionError,
        > {
            unreachable!()
        }
        fn receipts(&self) -> &[Self::Receipt] {
            &[]
        }
        fn evm_mut(&mut self) -> &mut Self::Evm {
            unreachable!()
        }
        fn evm(&self) -> &Self::Evm {
            unreachable!()
        }
    }

    impl BlockBuilder for ScriptedBlockBuilder {
        type Primitives = morph_primitives::MorphPrimitives;
        type Executor = DummyExecutor;

        fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
            Ok(())
        }

        fn execute_transaction_with_commit_condition(
            &mut self,
            tx: impl reth_evm::execute::ExecutorTx<Self::Executor>,
            _f: impl FnOnce(
                &<Self::Executor as reth_evm::block::BlockExecutor>::Result,
            ) -> reth_evm::block::CommitChanges,
        ) -> Result<Option<reth_evm::execute::GasOutput>, BlockExecutionError> {
            self.executed_count += 1;
            let (_tx_env, recovered) = tx.into_parts();
            let hash = *recovered.tx_hash();
            match self.outcomes.remove(&hash).expect("scripted outcome") {
                Ok(gas_used) => Ok(Some(reth_evm::execute::GasOutput::new(gas_used))),
                Err(err) => Err(BlockExecutionError::Validation(
                    BlockValidationError::InvalidTx {
                        hash,
                        error: Box::new(err),
                    },
                )),
            }
        }

        fn finish(
            self,
            _state_provider: impl StateProvider,
            _state_root_precomputed: Option<(B256, reth_trie_common::updates::TrieUpdates)>,
        ) -> Result<BlockBuilderOutcome<Self::Primitives>, BlockExecutionError> {
            unreachable!()
        }

        fn executor_mut(&mut self) -> &mut Self::Executor {
            unreachable!()
        }

        fn executor(&self) -> &Self::Executor {
            unreachable!()
        }

        fn evm_mut(&mut self) -> &mut <Self::Executor as reth_evm::block::BlockExecutor>::Evm {
            &mut self.evm
        }

        fn evm(&self) -> &<Self::Executor as reth_evm::block::BlockExecutor>::Evm {
            &self.evm
        }

        fn into_executor(self) -> Self::Executor {
            unreachable!()
        }
    }

    struct ScriptedPoolTxs {
        txs: std::collections::VecDeque<morph_txpool::MorphPooledTransaction>,
        marked_invalid: Vec<(Address, u64)>,
    }

    impl PayloadTransactions for ScriptedPoolTxs {
        type Transaction = morph_txpool::MorphPooledTransaction;

        fn next(&mut self, _ctx: ()) -> Option<Self::Transaction> {
            self.txs.pop_front()
        }

        fn mark_invalid(&mut self, sender: Address, nonce: u64) {
            self.marked_invalid.push((sender, nonce));
        }
    }

    fn test_ctx_with_timestamp_and_txs(
        timestamp: u64,
        transactions: Vec<Bytes>,
    ) -> MorphPayloadBuilderCtx {
        let mut rpc_attrs = morph_payload_types::MorphPayloadAttributes::default();
        rpc_attrs.inner.timestamp = timestamp;
        rpc_attrs.transactions = Some(transactions);
        let attrs = MorphPayloadBuilderAttributes::try_new(
            B256::ZERO,
            rpc_attrs,
            morph_payload_types::MORPH_PAYLOAD_BUILDER_VERSION,
        )
        .unwrap();
        let payload_id = attrs.payload_id();
        MorphPayloadBuilderCtx {
            evm_config: test_evm_config(),
            config: PayloadConfig::new(
                Arc::new(SealedHeader::seal_slow(MorphHeader::default())),
                attrs,
                payload_id,
            ),
            cancel: Default::default(),
            best_payload: None,
            builder_config: MorphBuilderConfig::default(),
            metrics: MorphPayloadBuilderMetrics::default(),
        }
    }

    // =========================================================================
    // M-H09: Pre-execution queue_index validation & overflow protection tests
    // =========================================================================

    #[test]
    fn test_execute_supplied_transactions_rejects_bad_queue_index_before_execution() {
        use alloy_consensus::Sealable;
        use morph_primitives::TxL1Msg;

        let l1_msg = TxL1Msg {
            queue_index: 5,
            gas_limit: 50_000,
            to: Address::repeat_byte(0xaa),
            value: U256::ZERO,
            sender: Address::repeat_byte(0xbb),
            input: Bytes::new(),
        };
        let encoded = Bytes::from(MorphTxEnvelope::L1Msg(l1_msg.seal_slow()).encoded_2718());
        let ctx = test_ctx_with_timestamp_and_txs(1_800_000_000, vec![encoded]);
        let mut builder = ScriptedBlockBuilder::new(std::collections::HashMap::new());
        let mut info = ExecutionInfo::new(0, None);

        let err = ctx
            .execute_supplied_transactions(&mut builder, &mut info)
            .expect_err("non-sequential queue_index must be rejected");
        assert!(
            err.to_string()
                .contains("invalid L1 message queue index: expected 0, got 5"),
            "unexpected error: {err}"
        );
        assert_eq!(
            builder.executed_count, 0,
            "bad queue_index must be rejected before EVM execution"
        );
    }

    #[test]
    fn test_execute_supplied_transactions_rejects_queue_index_overflow() {
        use alloy_consensus::Sealable;
        use morph_primitives::TxL1Msg;

        let l1_msg = TxL1Msg {
            queue_index: u64::MAX,
            gas_limit: 50_000,
            to: Address::repeat_byte(0xaa),
            value: U256::ZERO,
            sender: Address::repeat_byte(0xbb),
            input: Bytes::new(),
        };
        let encoded = Bytes::from(MorphTxEnvelope::L1Msg(l1_msg.seal_slow()).encoded_2718());
        let ctx = test_ctx_with_timestamp_and_txs(1_800_000_000, vec![encoded]);
        let mut builder = ScriptedBlockBuilder::new(std::collections::HashMap::new());
        let mut info = ExecutionInfo::new(u64::MAX, None);

        let err = ctx
            .execute_supplied_transactions(&mut builder, &mut info)
            .expect_err("u64::MAX queue_index overflow must be rejected cleanly");
        assert!(
            err.to_string().contains("queue index overflow"),
            "unexpected error: {err}"
        );
        assert_eq!(
            builder.executed_count, 0,
            "overflowing queue_index must be rejected before EVM execution"
        );
    }

    // =========================================================================
    // M-H10: MorphTx hardfork & structural validation tests
    // =========================================================================

    #[test]
    fn test_validate_morph_tx_for_block_enforces_hardfork_gates_and_validate() {
        use alloy_consensus::Signed;
        use alloy_primitives::{Signature, TxKind};
        use morph_primitives::TxMorph;

        let chain_spec = morph_chainspec::MORPH_MAINNET.as_ref();
        // Mainnet fork timestamps: emerald=1767765600, jade=1775628000, celadon=1793772000
        let pre_emerald = 1_767_765_599;
        let pre_jade = 1_775_627_999;
        let pre_celadon = 1_793_771_999;
        let celadon = 1_793_772_000;

        let wrap = |tx: TxMorph| {
            MorphTxEnvelope::Morph(Signed::new_unchecked(
                tx,
                Signature::test_signature(),
                B256::repeat_byte(0x01),
            ))
        };

        let v0 = wrap(TxMorph {
            chain_id: 2818,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 1,
            version: 0,
            ..Default::default()
        });
        let v1 = wrap(TxMorph {
            chain_id: 2818,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        });
        let v2 = wrap(TxMorph {
            chain_id: 2818,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            version: MORPH_TX_VERSION_2,
            ..Default::default()
        });
        let bad_v1_fee_limit = wrap(TxMorph {
            chain_id: 2818,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            fee_limit: U256::from(100),
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        });

        // Pre-Emerald rejects every MorphTx version.
        assert!(validate_morph_tx_for_block(&v0, chain_spec, pre_emerald).is_err());
        assert!(validate_morph_tx_for_block(&v1, chain_spec, pre_emerald).is_err());
        assert!(validate_morph_tx_for_block(&v2, chain_spec, pre_emerald).is_err());

        // Emerald (pre-Jade) permits V0, rejects V1 & V2.
        assert!(validate_morph_tx_for_block(&v0, chain_spec, pre_jade).is_ok());
        assert!(validate_morph_tx_for_block(&v1, chain_spec, pre_jade).is_err());
        assert!(validate_morph_tx_for_block(&v2, chain_spec, pre_jade).is_err());

        // Jade (pre-Celadon) permits V0 & V1, rejects V2.
        assert!(validate_morph_tx_for_block(&v0, chain_spec, pre_celadon).is_ok());
        assert!(validate_morph_tx_for_block(&v1, chain_spec, pre_celadon).is_ok());
        assert!(validate_morph_tx_for_block(&v2, chain_spec, pre_celadon).is_err());

        // Celadon permits V2, but structural `TxMorph::validate()` still rejects bad fields.
        assert!(validate_morph_tx_for_block(&v2, chain_spec, celadon).is_ok());
        assert!(validate_morph_tx_for_block(&bad_v1_fee_limit, chain_spec, celadon).is_err());
    }

    #[test]
    fn test_execute_pool_transactions_skips_pre_celadon_v2_without_executing_or_evicting() {
        use alloy_consensus::Signed;
        use alloy_primitives::{Signature, TxKind};
        use morph_primitives::TxMorph;
        use morph_txpool::MorphPooledTransaction;
        use reth_primitives_traits::Recovered;

        let sender_v2 = Address::repeat_byte(0x11);
        let sender_v1 = Address::repeat_byte(0x22);

        let v2_tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            version: MORPH_TX_VERSION_2,
            ..Default::default()
        };
        let v2_hash = B256::repeat_byte(0xaa);
        let v2_rec = Recovered::new_unchecked(
            MorphTxEnvelope::Morph(Signed::new_unchecked(
                v2_tx,
                Signature::test_signature(),
                v2_hash,
            )),
            sender_v2,
        );
        let v2_len = v2_rec.encode_2718_len();
        let v2_pooled = MorphPooledTransaction::new(v2_rec, v2_len);

        let v1_tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            version: MORPH_TX_VERSION_1,
            ..Default::default()
        };
        let v1_hash = B256::repeat_byte(0xbb);
        let v1_rec = Recovered::new_unchecked(
            MorphTxEnvelope::Morph(Signed::new_unchecked(
                v1_tx,
                Signature::test_signature(),
                v1_hash,
            )),
            sender_v1,
        );
        let v1_len = v1_rec.encode_2718_len();
        let v1_pooled = MorphPooledTransaction::new(v1_rec, v1_len);

        // Only v1_hash has a scripted outcome; if v2 were executed it would panic.
        let mut outcomes = std::collections::HashMap::new();
        outcomes.insert(v1_hash, Ok(21_000));
        let mut builder = ScriptedBlockBuilder::new(outcomes);
        let mut info = ExecutionInfo::default();
        let mut executed_txs = Vec::new();
        let best_txs = ScriptedPoolTxs {
            txs: std::collections::VecDeque::from([v2_pooled, v1_pooled]),
            marked_invalid: Vec::new(),
        };
        // Pre-Celadon, post-Jade timestamp on MORPH_MAINNET.
        let ctx = test_ctx_with_timestamp_and_txs(1_793_771_999, vec![]);
        let breaker = ctx.builder_config.breaker(30_000_000);
        let mut evicted = Vec::new();

        ctx.execute_pool_transactions_with_evictor(
            &mut builder,
            &mut info,
            &mut executed_txs,
            best_txs,
            &breaker,
            |hash| evicted.push(hash),
        )
        .expect("pool execution should complete");

        assert_eq!(
            builder.executed_count, 1,
            "only V1 should execute; V2 must be skipped pre-Celadon"
        );
        assert_eq!(executed_txs.len(), 1);
        assert!(
            evicted.is_empty(),
            "fork-gated skip during reorg build must not evict from pool"
        );
    }

    #[test]
    fn test_execute_supplied_transactions_rejects_pre_celadon_v2_before_execution() {
        use alloy_consensus::Signed;
        use alloy_primitives::{Signature, TxKind};
        use morph_primitives::TxMorph;

        let v2_tx = TxMorph {
            chain_id: 2818,
            nonce: 0,
            gas_limit: 50_000,
            max_fee_per_gas: 100,
            max_priority_fee_per_gas: 10,
            to: TxKind::Call(Address::repeat_byte(0xee)),
            fee_token_id: 0,
            version: MORPH_TX_VERSION_2,
            ..Default::default()
        };
        let encoded = Bytes::from(
            MorphTxEnvelope::Morph(Signed::new_unchecked(
                v2_tx,
                Signature::test_signature(),
                B256::repeat_byte(0xcc),
            ))
            .encoded_2718(),
        );
        // Pre-Celadon timestamp on MORPH_MAINNET.
        let ctx = test_ctx_with_timestamp_and_txs(1_793_771_999, vec![encoded]);
        let mut builder = ScriptedBlockBuilder::new(std::collections::HashMap::new());
        let mut info = ExecutionInfo::default();

        let err = ctx
            .execute_supplied_transactions(&mut builder, &mut info)
            .expect_err("pre-Celadon supplied MorphTx V2 must be rejected");
        assert!(
            err.to_string().contains("celadon fork not reached"),
            "unexpected error: {err}"
        );
        assert_eq!(
            builder.executed_count, 0,
            "fork-invalid supplied MorphTx must be rejected before EVM execution"
        );
    }

    // =========================================================================
    // M-M12: Deterministic token-fee failure pool eviction tests
    // =========================================================================

    #[test]
    fn test_is_evictable_token_fee_error_classification() {
        use revm::context::result::InvalidTransaction;

        // Deterministic token-fee failures MUST evict.
        assert!(is_evictable_token_fee_error(
            &MorphInvalidTransaction::TokenTransferFailed {
                reason: "reverted".into(),
            }
        ));
        assert!(is_evictable_token_fee_error(
            &MorphInvalidTransaction::TokenBalanceQueryFailed
        ));
        assert!(is_evictable_token_fee_error(
            &MorphInvalidTransaction::TokenNotRegistered(1)
        ));
        assert!(is_evictable_token_fee_error(
            &MorphInvalidTransaction::TokenNotActive(1)
        ));
        assert!(is_evictable_token_fee_error(
            &MorphInvalidTransaction::InvalidTokenConfig(1)
        ));

        // InsufficientTokenBalance and standard ETH errors MUST NOT evict.
        assert!(!is_evictable_token_fee_error(
            &MorphInvalidTransaction::InsufficientTokenBalance {
                required: U256::from(100),
                available: U256::from(50),
            }
        ));
        assert!(!is_evictable_token_fee_error(
            &MorphInvalidTransaction::EthInvalidTransaction(InvalidTransaction::NonceTooLow {
                tx: 0,
                state: 1,
            })
        ));
        assert!(!is_evictable_token_fee_error(
            &MorphInvalidTransaction::EthInvalidTransaction(
                InvalidTransaction::LackOfFundForMaxFee {
                    fee: Box::new(U256::from(100)),
                    balance: Box::new(U256::ZERO),
                }
            )
        ));
    }

    #[test]
    fn test_execute_pool_transactions_evicts_only_unexecuted_senders_deterministic_token_errors() {
        use alloy_consensus::{Signed, TxEip1559};
        use alloy_primitives::{Signature, TxKind};
        use morph_primitives::TxMorph;
        use morph_txpool::MorphPooledTransaction;
        use reth_primitives_traits::Recovered;
        use std::collections::{HashMap, VecDeque};

        let make_morph_tx = |signer: Address, nonce: u64, tag: u8| {
            let tx = TxMorph {
                chain_id: 2818,
                nonce,
                gas_limit: 50_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: TxKind::Call(Address::repeat_byte(0xee)),
                fee_token_id: 1,
                ..Default::default()
            };
            let hash = B256::repeat_byte(tag);
            let env = MorphTxEnvelope::Morph(Signed::new_unchecked(
                tx,
                Signature::test_signature(),
                hash,
            ));
            let recovered = Recovered::new_unchecked(env, signer);
            let len = recovered.encode_2718_len();
            (hash, MorphPooledTransaction::new(recovered, len))
        };

        let make_eth_tx = |signer: Address, nonce: u64, tag: u8| {
            let tx = TxEip1559 {
                chain_id: 2818,
                nonce,
                gas_limit: 21_000,
                max_fee_per_gas: 100,
                max_priority_fee_per_gas: 10,
                to: TxKind::Call(Address::repeat_byte(0xee)),
                ..Default::default()
            };
            let hash = B256::repeat_byte(tag);
            let env = MorphTxEnvelope::Eip1559(Signed::new_unchecked(
                tx,
                Signature::test_signature(),
                hash,
            ));
            let recovered = Recovered::new_unchecked(env, signer);
            let len = recovered.encode_2718_len();
            (hash, MorphPooledTransaction::new(recovered, len))
        };

        let sender_blacklisted = Address::repeat_byte(0x01);
        let sender_low_bal = Address::repeat_byte(0x02);
        let sender_already_executed = Address::repeat_byte(0x03);
        let sender_inactive = Address::repeat_byte(0x04);

        let (h_transfer_fail, tx_transfer_fail) = make_morph_tx(sender_blacklisted, 0, 0x11);
        let (h_low_bal, tx_low_bal) = make_morph_tx(sender_low_bal, 0, 0x22);
        let (h_exec_ok, tx_exec_ok) = make_eth_tx(sender_already_executed, 0, 0x33);
        let (h_exec_then_fail, tx_exec_then_fail) = make_morph_tx(sender_already_executed, 1, 0x34);
        let (h_inactive, tx_inactive) = make_morph_tx(sender_inactive, 0, 0x44);

        let mut outcomes = HashMap::new();
        outcomes.insert(
            h_transfer_fail,
            Err(MorphInvalidTransaction::TokenTransferFailed {
                reason: "blacklisted".into(),
            }),
        );
        outcomes.insert(
            h_low_bal,
            Err(MorphInvalidTransaction::InsufficientTokenBalance {
                required: U256::from(100),
                available: U256::from(10),
            }),
        );
        outcomes.insert(h_exec_ok, Ok(21_000));
        outcomes.insert(
            h_exec_then_fail,
            Err(MorphInvalidTransaction::TokenTransferFailed {
                reason: "post-tx state change".into(),
            }),
        );
        outcomes.insert(h_inactive, Err(MorphInvalidTransaction::TokenNotActive(1)));

        let mut builder = ScriptedBlockBuilder::new(outcomes);
        let mut info = ExecutionInfo::default();
        let mut executed_txs = Vec::new();
        let best_txs = ScriptedPoolTxs {
            txs: VecDeque::from([
                tx_transfer_fail,
                tx_low_bal,
                tx_exec_ok,
                tx_exec_then_fail,
                tx_inactive,
            ]),
            marked_invalid: Vec::new(),
        };
        // Post-Celadon timestamp so MorphTx hardfork gates pass.
        let ctx = test_ctx_with_timestamp_and_txs(1_800_000_000, vec![]);
        let breaker = ctx.builder_config.breaker(30_000_000);
        let mut evicted = Vec::new();

        ctx.execute_pool_transactions_with_evictor(
            &mut builder,
            &mut info,
            &mut executed_txs,
            best_txs,
            &breaker,
            |hash| evicted.push(hash),
        )
        .expect("pool execution should complete");

        // Only sender_blacklisted (TokenTransferFailed, unexecuted) and sender_inactive
        // (TokenNotActive, unexecuted) should be evicted.
        // - sender_low_bal (InsufficientTokenBalance) must NOT be evicted.
        // - sender_already_executed nonce 1 (had nonce 0 succeed in this block) must NOT be evicted.
        assert_eq!(evicted, vec![h_transfer_fail, h_inactive]);
        assert_eq!(executed_txs.len(), 1);
    }
}
