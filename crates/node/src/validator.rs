//! Morph engine validator.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use alloy_primitives::B256;
#[cfg(test)]
use alloy_primitives::keccak256;
use morph_chainspec::{
    L2_MESSAGE_QUEUE_ADDRESS, L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, MorphChainSpec,
    MorphHardforks,
};
use morph_consensus::{MorphConsensus, MorphConsensusError};
use morph_payload_types::{
    MorphExecutionData, MorphPayloadTypes, MorphPayloadValidationMode, TreeHeaderCache,
};
use morph_primitives::{
    Block, BlockBody, MorphHeader, MorphPrimitives, MorphReceipt, MorphTxEnvelope,
};
use parking_lot::Mutex;
use reth_chain_state::ExecutedBlock;
use reth_chainspec::EthChainSpec;
use reth_consensus::{Consensus, FullConsensus, HeaderValidator, ReceiptRootBloom};
use reth_engine_tree::tree::payload_validator::TreeCtx;
use reth_engine_tree::tree::{
    BasicEngineValidator, CacheWaitDurations, EngineApiTreeState, EngineValidator,
    TxPoolPrewarmSource, TxPoolPrewarmTransaction, TxPoolPrewarmTransactions, WaitForCaches,
    error::{InsertBlockError, InsertBlockErrorKind},
    state_root_strategy::{
        DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootHandle,
        PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext,
        StateRootJobOutcome, StateRootStrategy,
    },
};
use reth_errors::{ConsensusError, ProviderError, ProviderResult};
use reth_evm::{
    ConfigureEvm,
    block::BlockExecutionResult,
    revm::{context::Block as _, database::BundleState},
};
use reth_node_api::{
    AddOnsContext, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError, NodeTypes,
    PayloadAttributes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::BuiltPayloadExecutedBlock;
use reth_primitives_traits::{RecoveredBlock, SealedBlock, SealedHeader};
use reth_provider::{
    BlockExecutionOutput, BlockNumReader, BlockReader, ChainSpecProvider, DatabaseProviderFactory,
    PruneCheckpointReader, StageCheckpointReader, StateProviderFactory, StateReader,
    StateRootProvider, StorageSettingsCache, TryIntoHistoricalStateProvider,
};
use reth_storage_overlay::OverlayManager;
use reth_tracing::tracing;
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
};
use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
};

/// Builder for Morph engine validator (payload validation).
///
/// Creates a validator for validating engine API payloads.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for MorphEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = MorphNode>,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
{
    type Validator = MorphEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(MorphEngineValidator::new())
    }
}

/// Builder for Morph tree engine validator.
///
/// This wires [`MorphEngineValidator`] into upstream payload validation, installs the
/// pre-Jade-aware `MorphStateRootStrategy`, and wraps the result in
/// [`MorphTreeEngineValidator`] for the L1-queue and withdraw-trie-root checks.
#[derive(Debug, Clone)]
pub struct MorphTreeEngineValidatorBuilder<PVB = MorphEngineValidatorBuilder> {
    payload_validator_builder: PVB,
    tree_headers: Arc<TreeHeaderCache>,
}

impl<PVB> MorphTreeEngineValidatorBuilder<PVB> {
    /// Creates a new instance with the given payload validator builder.
    pub fn new(payload_validator_builder: PVB) -> Self {
        Self {
            payload_validator_builder,
            tree_headers: Arc::new(TreeHeaderCache::new()),
        }
    }

    /// Attaches a shared [`TreeHeaderCache`] so the custom Morph L2 Engine API can
    /// resolve unpersisted in-memory `tree_state` headers.
    pub fn with_tree_headers(mut self, tree_headers: Arc<TreeHeaderCache>) -> Self {
        self.tree_headers = tree_headers;
        self
    }
}

impl<PVB> Default for MorphTreeEngineValidatorBuilder<PVB>
where
    PVB: Default,
{
    fn default() -> Self {
        Self::new(PVB::default())
    }
}

impl<Node, PVB> EngineValidatorBuilder<Node> for MorphTreeEngineValidatorBuilder<PVB>
where
    Node: FullNodeComponents<
            Types = MorphNode,
            Evm: reth_node_api::ConfigureEngineEvm<
                <<Node::Types as NodeTypes>::Payload as PayloadTypes>::ExecutionData,
            >,
        >,
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec> + StateProviderFactory,
    PVB: PayloadValidatorBuilder<Node, Validator = MorphEngineValidator>,
{
    type EngineValidator = MorphTreeEngineValidator<Node::Provider, Node::Evm>;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: reth_node_api::TreeConfig,
        overlay_manager: OverlayManager<MorphPrimitives>,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = self.payload_validator_builder.build(ctx).await?;
        let data_dir = ctx
            .config
            .datadir
            .clone()
            .resolve_datadir(ctx.config.chain.chain());
        let invalid_block_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        let chain_spec = ctx.node.provider().chain_spec();
        let post_execution_validator = validator.clone();

        let provider = ctx.node.provider().clone();
        let engine_consensus = Arc::new(MorphEngineConsensus::new(MorphConsensus::new(
            chain_spec.clone(),
        )));
        let consensus: Arc<dyn FullConsensus<MorphPrimitives>> = engine_consensus.clone();
        let state_root_strategy: Arc<
            dyn StateRootStrategy<MorphPrimitives, Node::Provider, Node::Evm>,
        > = Arc::new(MorphStateRootStrategy::new(chain_spec.clone()));
        let tree_config = strict_morph_tree_config(tree_config);
        let txpool_prewarming = tree_config.txpool_prewarming();
        let mut validator = BasicEngineValidator::new(
            provider.clone(),
            consensus,
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            overlay_manager,
            ctx.node.task_executor().clone(),
        )
        .with_state_root_strategy(state_root_strategy);

        if txpool_prewarming {
            validator = validator
                .with_txpool_prewarming(MorphTxPoolPrewarmSource::new(ctx.node.pool().clone()));
        }

        Ok(MorphTreeEngineValidator::new(
            validator,
            provider,
            chain_spec,
            engine_consensus,
            post_execution_validator,
            self.tree_headers,
        ))
    }
}

/// [`TransactionPool`]-backed candidate source for engine cache prewarming.
#[derive(Debug)]
struct MorphTxPoolPrewarmSource<P>(P);

impl<P> MorphTxPoolPrewarmSource<P> {
    const fn new(pool: P) -> Self {
        Self(pool)
    }
}

impl<P> TxPoolPrewarmSource<MorphPrimitives> for MorphTxPoolPrewarmSource<P>
where
    P: TransactionPool<Transaction: PoolTransaction<Consensus = MorphTxEnvelope>>
        + Clone
        + Send
        + Sync
        + std::fmt::Debug
        + 'static,
{
    fn best_transactions(
        &self,
        parent_hash: B256,
    ) -> Option<TxPoolPrewarmTransactions<MorphPrimitives>> {
        let block_info = self.0.block_info();
        if block_info.last_seen_block_hash != parent_hash {
            return None;
        }

        let mut best = self
            .0
            .best_transactions_with_attributes(BestTransactionsAttributes::new(
                block_info.pending_basefee,
                block_info
                    .pending_blob_fee
                    .map(|fee| u64::try_from(fee).unwrap_or(u64::MAX)),
            ));
        best.allow_updates_out_of_order();
        best.skip_blobs();

        Some(Box::new(best.map(|transaction| TxPoolPrewarmTransaction {
            hash: *transaction.hash(),
            sender: transaction.sender(),
            transaction: transaction.transaction.clone_into_consensus(),
        })))
    }
}

/// Morph only relaxes the header-root equality check before Jade, through
/// [`MorphStateRootStrategy`]. The upstream debug skip also suppresses trie
/// updates, so allowing it here would make both pre-Jade persistence and
/// post-Jade validation unsafe.
fn strict_morph_tree_config(tree_config: reth_node_api::TreeConfig) -> reth_node_api::TreeConfig {
    if tree_config.skip_state_root() {
        tracing::warn!(
            target: "reth::cli",
            "ignoring --debug.skip-state-root: Morph requires trie updates before Jade and strict state-root validation from Jade onward"
        );
    }
    tree_config.with_skip_state_root(false)
}

#[derive(Debug, Default)]
struct ValidationModeCache {
    entries: HashMap<B256, MorphPayloadValidationMode>,
    order: VecDeque<B256>,
}

/// Engine-tree consensus adapter that applies [`MorphPayloadValidationMode`] exemptions
/// per block hash when `BasicEngineValidator` validates locally built or L1-derived safe
/// payloads, while defaulting to full [`MorphConsensus`] validation for all untrusted blocks.
#[derive(Debug)]
struct MorphEngineConsensus {
    inner: MorphConsensus,
    modes: Mutex<ValidationModeCache>,
}

impl MorphEngineConsensus {
    const MAX_MODES: usize = 1024;

    fn new(inner: MorphConsensus) -> Self {
        Self {
            inner,
            modes: Mutex::new(ValidationModeCache::default()),
        }
    }

    const fn inner(&self) -> &MorphConsensus {
        &self.inner
    }

    fn record_mode(&self, block_hash: B256, mode: MorphPayloadValidationMode) {
        if mode == MorphPayloadValidationMode::Full {
            // `Full` is the default when absent. Never delete or downgrade an existing
            // `LocallyBuilt` or `Safe` entry: `BasicEngineValidator` runs
            // `spawn_convert_and_validate` on a background thread, and buffered
            // `Safe` / `LocallyBuilt` payloads later pass through `validate_block`.
            return;
        }
        let mut cache = self.modes.lock();
        match cache.entries.get_mut(&block_hash) {
            Some(existing) => {
                if mode > *existing {
                    *existing = mode;
                }
            }
            None => {
                cache.entries.insert(block_hash, mode);
                cache.order.push_back(block_hash);
                while cache.entries.len() > Self::MAX_MODES {
                    let Some(evicted) = cache.order.pop_front() else {
                        break;
                    };
                    cache.entries.remove(&evicted);
                }
            }
        }
    }

    fn mode_for(&self, block_hash: B256) -> MorphPayloadValidationMode {
        self.modes
            .lock()
            .entries
            .get(&block_hash)
            .copied()
            .unwrap_or(MorphPayloadValidationMode::Full)
    }
}

impl HeaderValidator<MorphHeader> for MorphEngineConsensus {
    fn validate_header(&self, header: &SealedHeader<MorphHeader>) -> Result<(), ConsensusError> {
        match self.mode_for(header.hash()) {
            MorphPayloadValidationMode::Full => self.inner.validate_header(header),
            MorphPayloadValidationMode::LocallyBuilt | MorphPayloadValidationMode::Safe => {
                self.inner.validate_header_structural(header)
            }
        }
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader<MorphHeader>,
        parent: &SealedHeader<MorphHeader>,
    ) -> Result<(), ConsensusError> {
        match self.mode_for(header.hash()) {
            MorphPayloadValidationMode::Full => {
                self.inner.validate_header_against_parent(header, parent)
            }
            MorphPayloadValidationMode::LocallyBuilt => self
                .inner
                .validate_header_against_parent_locally_built(header, parent),
            MorphPayloadValidationMode::Safe => self
                .inner
                .validate_header_against_parent_structural(header, parent),
        }
    }
}

impl Consensus<Block> for MorphEngineConsensus {
    fn validate_body_against_header(
        &self,
        body: &BlockBody,
        header: &SealedHeader<MorphHeader>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_body_against_header(body, header)
    }

    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<Block>,
    ) -> Result<(), ConsensusError> {
        match self.mode_for(block.hash()) {
            MorphPayloadValidationMode::Full => self.inner.validate_block_pre_execution(block),
            MorphPayloadValidationMode::LocallyBuilt | MorphPayloadValidationMode::Safe => self
                .inner
                .validate_block_pre_execution_without_da_limit(block),
        }
    }

    fn is_transient_error(&self, error: &ConsensusError) -> bool {
        self.inner.is_transient_error(error)
    }
}

impl FullConsensus<MorphPrimitives> for MorphEngineConsensus {
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<Block>,
        result: &BlockExecutionResult<MorphReceipt>,
        receipt_root_bloom: Option<ReceiptRootBloom>,
        block_access_list_hash: Option<B256>,
    ) -> Result<(), ConsensusError> {
        self.inner.validate_block_post_execution(
            block,
            result,
            receipt_root_bloom,
            block_access_list_hash,
        )
    }
}

#[derive(Debug, Default)]
struct KnownWithdrawTrieRootCache {
    entries: HashMap<B256, B256>,
    order: VecDeque<B256>,
}

/// Upstream engine validator plus Morph-specific validation hooks.
///
/// State execution, caching and trie maintenance remain entirely upstream. This
/// wrapper preserves the parent-aware L1 queue invariant and the optional
/// consensus-layer withdraw-trie-root cross-check.
///
/// Every [`EngineValidator`] method must be forwarded to `inner`, including the
/// ones that carry a default body in the trait: a missing forward silently
/// replaces upstream behaviour with the empty default instead of failing to
/// compile. Re-check this impl against the trait on every reth upgrade.
pub struct MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm,
{
    inner: BasicEngineValidator<P, Evm, MorphEngineValidator>,
    provider: P,
    chain_spec: Arc<MorphChainSpec>,
    engine_consensus: Arc<MorphEngineConsensus>,
    post_execution_validator: MorphEngineValidator,
    tree_headers: Arc<TreeHeaderCache>,
    known_withdraw_trie_roots: Mutex<KnownWithdrawTrieRootCache>,
}

impl<P, Evm> MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm,
{
    const MAX_KNOWN_WITHDRAW_TRIE_ROOTS: usize = 8192;

    fn new(
        inner: BasicEngineValidator<P, Evm, MorphEngineValidator>,
        provider: P,
        chain_spec: Arc<MorphChainSpec>,
        engine_consensus: Arc<MorphEngineConsensus>,
        post_execution_validator: MorphEngineValidator,
        tree_headers: Arc<TreeHeaderCache>,
    ) -> Self {
        Self {
            inner,
            provider,
            chain_spec,
            engine_consensus,
            post_execution_validator,
            tree_headers,
            known_withdraw_trie_roots: Mutex::new(KnownWithdrawTrieRootCache::default()),
        }
    }

    fn record_known_withdraw_trie_root(&self, block_hash: B256, root: B256) {
        let mut cache = self.known_withdraw_trie_roots.lock();
        let is_new = cache.entries.insert(block_hash, root).is_none();
        if is_new {
            cache.order.push_back(block_hash);
            while cache.entries.len() > Self::MAX_KNOWN_WITHDRAW_TRIE_ROOTS {
                let Some(evicted) = cache.order.pop_front() else {
                    break;
                };
                cache.entries.remove(&evicted);
            }
        }
    }

    fn get_known_withdraw_trie_root(&self, block_hash: B256) -> Option<B256> {
        self.known_withdraw_trie_roots
            .lock()
            .entries
            .get(&block_hash)
            .copied()
    }

    fn updated_withdraw_trie_root_from_bundle_state(bundle_state: &BundleState) -> Option<B256> {
        bundle_state
            .account(&L2_MESSAGE_QUEUE_ADDRESS)
            .and_then(|account| {
                account
                    .storage
                    .get(&L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT)
                    .map(|slot| B256::from(slot.present_value()))
            })
    }
}

impl<P, Evm> MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm<Primitives = MorphPrimitives>,
    P: BlockReader<Header = MorphHeader> + StateProviderFactory,
{
    fn lookup_sealed_header(
        &self,
        hash: B256,
    ) -> ProviderResult<Option<SealedHeader<MorphHeader>>> {
        if let Some(header) = self.tree_headers.get(&hash) {
            return Ok(Some(header));
        }
        self.provider.sealed_header_by_hash(hash)
    }

    /// Walks in-memory `tree_state` ancestors from `parent_hash` without hitting MDBX so
    /// unpersisted side-branch parents still resolve their `withdraw_trie_root` even if
    /// evicted from `KnownWithdrawTrieRootCache`.
    fn find_tree_ancestor_withdraw_trie_root(
        &self,
        parent_hash: B256,
        ctx: &TreeCtx<'_, MorphPrimitives>,
    ) -> (Option<B256>, B256) {
        let mut cur = parent_hash;
        loop {
            if let Some(root) = self.get_known_withdraw_trie_root(cur) {
                return (Some(root), cur);
            }
            let Some(executed) = ctx.state().tree_state().executed_block_by_hash(cur) else {
                return (None, cur);
            };
            if let Some(updated) =
                Self::updated_withdraw_trie_root_from_bundle_state(&executed.execution_output.state)
            {
                return (Some(updated), cur);
            }
            cur = executed.recovered_block().parent_hash();
        }
    }

    fn resolve_post_block_withdraw_trie_root(
        &self,
        parent_hash: B256,
        bundle_state: &BundleState,
        tree_hint: Option<(Option<B256>, B256)>,
    ) -> ProviderResult<B256> {
        if let Some(updated) = Self::updated_withdraw_trie_root_from_bundle_state(bundle_state) {
            return Ok(updated);
        }
        if let Some(parent_root) = self.get_known_withdraw_trie_root(parent_hash) {
            return Ok(parent_root);
        }
        if let Some((Some(ancestor_root), _)) = tree_hint {
            self.record_known_withdraw_trie_root(parent_hash, ancestor_root);
            return Ok(ancestor_root);
        }
        let fallback_hash = tree_hint.map_or(parent_hash, |(_, ancestor_hash)| ancestor_hash);
        let state = self.provider.state_by_block_hash(fallback_hash)?;
        let slot_val = state
            .storage(
                L2_MESSAGE_QUEUE_ADDRESS,
                B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT),
            )?
            .unwrap_or_default();
        let actual = B256::from(slot_val);
        self.record_known_withdraw_trie_root(parent_hash, actual);
        Ok(actual)
    }

    /// Validates `header.next_l1_msg_index` during `convert_payload_to_block` (which runs on
    /// `AlreadySeen` hits for both `tree_state` and DB blocks) using both in-memory
    /// `tree_headers` and `provider` to resolve the parent header.
    fn validate_l1_msg_index_on_conversion(
        &self,
        block: &SealedBlock<morph_primitives::Block>,
    ) -> Result<(), NewPayloadError> {
        let Some(parent) = self
            .lookup_sealed_header(block.parent_hash())
            .map_err(|e| NewPayloadError::Other(Box::new(e)))?
        else {
            return Ok(());
        };

        let mut expected = parent.next_l1_msg_index;
        let mut first_l1_msg = true;
        for tx in block.body().transactions() {
            if !tx.is_l1_msg() {
                break;
            }
            let Some(queue_index) = tx.queue_index() else {
                return Ok(());
            };
            if first_l1_msg {
                first_l1_msg = false;
                if queue_index < parent.next_l1_msg_index {
                    return Ok(());
                }
            }
            expected = queue_index.checked_add(1).ok_or_else(|| {
                NewPayloadError::Other(Box::new(MorphConsensusError::InvalidNextL1MessageIndex {
                    expected: u64::MAX,
                    actual: block.header().next_l1_msg_index,
                }))
            })?;
        }

        let has_l1_msgs = !first_l1_msg;
        let actual = block.header().next_l1_msg_index;
        let is_jade = self
            .chain_spec
            .is_jade_active_at_timestamp(block.header().timestamp());
        let invalid = if is_jade {
            actual != expected
        } else {
            has_l1_msgs && actual < expected
        };
        if invalid {
            return Err(NewPayloadError::Other(Box::new(
                MorphConsensusError::InvalidNextL1MessageIndex { expected, actual },
            )));
        }

        Ok(())
    }

    /// Enforces parent-aware L1 message queue invariants:
    ///
    /// 1. Across **all** hardforks (both pre-Jade and post-Jade), if the block contains
    ///    leading L1 messages, the first message's `queue_index` must be `>= parent.next_l1_msg_index`.
    ///    Violations are cross-block L1 message replays and return cacheable
    ///    [`MorphConsensusError::L1MessagesNotInOrder`] (since transactions are committed by the
    ///    block hash). Forward skips (`first_queue_index > parent.next_l1_msg_index`) remain
    ///    permitted.
    /// 2. From Jade onward, `header.next_l1_msg_index` must match the exact derived index
    ///    (`parent.next_l1_msg_index` when no L1 messages are present, or `last_queue_index + 1`
    ///    otherwise). Mismatches return transient [`MorphConsensusError::InvalidNextL1MessageIndex`]
    ///    so an unhashed header field cannot poison `invalid_headers`.
    fn validate_l1_msg_index_against_parent(
        &self,
        block: &SealedBlock<morph_primitives::Block>,
        parent: &MorphHeader,
    ) -> Result<(), ConsensusError> {
        let mut expected = parent.next_l1_msg_index;
        let mut first_l1_msg = true;
        for tx in block.body().transactions() {
            if !tx.is_l1_msg() {
                break;
            }
            let queue_index = tx.queue_index().ok_or_else(|| {
                ConsensusError::msg("L1 message transaction is missing queue index")
            })?;
            if first_l1_msg {
                first_l1_msg = false;
                if queue_index < parent.next_l1_msg_index {
                    return Err(ConsensusError::other(
                        MorphConsensusError::L1MessagesNotInOrder {
                            expected: parent.next_l1_msg_index,
                            actual: queue_index,
                        },
                    ));
                }
            }
            expected = queue_index.checked_add(1).ok_or_else(|| {
                ConsensusError::other(MorphConsensusError::InvalidNextL1MessageIndex {
                    expected: u64::MAX,
                    actual: block.header().next_l1_msg_index,
                })
            })?;
        }

        if !self
            .chain_spec
            .is_jade_active_at_timestamp(block.header().timestamp())
        {
            return Ok(());
        }

        // Typed so that `MorphConsensus::is_transient_error` keeps this rejection out of
        // reth's invalid-header cache: the field is not covered by the block hash, so
        // the cache entry would also match the honest block.
        let actual = block.header().next_l1_msg_index;
        if actual != expected {
            return Err(ConsensusError::other(
                MorphConsensusError::InvalidNextL1MessageIndex { expected, actual },
            ));
        }
        Ok(())
    }

    fn validate_next_l1_msg_index(
        &self,
        block: &SealedBlock<morph_primitives::Block>,
        ctx: &TreeCtx<'_, MorphPrimitives>,
    ) -> Result<(), InsertBlockErrorKind> {
        let parent_hash = block.parent_hash();
        let parent = match ctx.state().tree_state().sealed_header_by_hash(&parent_hash) {
            Some(parent) => Some(parent),
            None => self.lookup_sealed_header(parent_hash)?,
        };
        let Some(parent) = parent else {
            // The upstream validator reports the missing parent with the
            // canonical provider error before executing the block.
            return Ok(());
        };

        self.validate_l1_msg_index_against_parent(block, parent.header())
            .map_err(Into::into)
    }

    fn validate_next_l1_msg_index_for_block(
        &self,
        block: &SealedBlock<morph_primitives::Block>,
        ctx: &TreeCtx<'_, MorphPrimitives>,
    ) -> Result<(), reth_engine_tree::tree::error::InsertPayloadError<morph_primitives::Block>>
    {
        self.validate_next_l1_msg_index(block, ctx)
            .map_err(|kind| InsertBlockError::new(block.clone(), kind).into())
    }

    /// Runs structural consensus validation before admitting a locally built block via
    /// `InsertExecutedBlock` (originating from either `assembleL2Block(V2)` or
    /// `newSafeL2Block`), exempting wall-clock `TimestampIsInFuture`, `GasLimit` 1/1024
    /// bounds, `BaseFeeOverLimit`, and the local DA payload byte limit to match
    /// morph-geth's `verified` and `NewSafeL2Block` (`safe = true`) paths.
    fn validate_pre_inserted_block(
        &self,
        block: &RecoveredBlock<morph_primitives::Block>,
    ) -> ProviderResult<()> {
        let consensus = self.engine_consensus.inner();
        let sealed_header = block.sealed_header();
        consensus
            .validate_header_structural(sealed_header)
            .map_err(ProviderError::other)?;
        if let Some(parent) = self.lookup_sealed_header(block.parent_hash())? {
            consensus
                .validate_header_against_parent_structural(sealed_header, &parent)
                .map_err(ProviderError::other)?;
            self.validate_l1_msg_index_against_parent(block.sealed_block(), parent.header())
                .map_err(ProviderError::other)?;
        }
        consensus
            .validate_block_pre_execution_without_da_limit(block.sealed_block())
            .map_err(ProviderError::other)?;
        Ok(())
    }

    fn verify_and_record_withdraw_trie_root(
        &self,
        expectation: WithdrawTrieRootExpectation,
        tree_hint: Option<(Option<B256>, B256)>,
        output: reth_engine_tree::tree::ValidationOutput<MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let sealed_block = output.executed_block.sealed_block();
        let block_hash = sealed_block.hash();
        let parent_hash = sealed_block.parent_hash();
        let actual = self
            .resolve_post_block_withdraw_trie_root(
                parent_hash,
                &output.executed_block.execution_output.state,
                tree_hint,
            )
            .map_err(|err| InsertBlockError::new(sealed_block.clone(), err.into()))?;

        if let WithdrawTrieRootExpectation::Verify(expected) = expectation
            && actual != expected
        {
            return Err(InsertBlockError::consensus_error(
                ConsensusError::other(MorphConsensusError::WithdrawTrieRootMismatch {
                    expected,
                    actual,
                }),
                sealed_block.clone(),
            )
            .into());
        }

        self.record_known_withdraw_trie_root(block_hash, actual);
        self.tree_headers.insert(
            output
                .executed_block
                .recovered_block()
                .sealed_header()
                .clone(),
        );
        Ok(output)
    }
}

impl<P, Evm> EngineValidator<MorphPayloadTypes, MorphPrimitives>
    for MorphTreeEngineValidator<P, Evm>
where
    P: BlockReader<Header = MorphHeader> + StateProviderFactory + Send + Sync + 'static,
    Evm: ConfigureEvm<Primitives = MorphPrimitives> + 'static,
    BasicEngineValidator<P, Evm, MorphEngineValidator>:
        EngineValidator<MorphPayloadTypes, MorphPrimitives>,
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &<MorphPayloadTypes as PayloadTypes>::PayloadAttributes,
        header: &MorphHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.inner
            .validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
        &self,
        mut payload: MorphExecutionData,
    ) -> Result<SealedBlock<morph_primitives::Block>, NewPayloadError> {
        let block_hash = payload.block.hash();
        self.validate_l1_msg_index_on_conversion(payload.block.as_ref())?;
        self.engine_consensus
            .record_mode(block_hash, payload.validation_mode);

        match payload.expected_withdraw_trie_root {
            None => {}
            Some(root) if root.is_zero() => {
                self.post_execution_validator
                    .take_withdraw_trie_root_expectation(block_hash);
                payload.expected_withdraw_trie_root = None;
            }
            Some(expected) => {
                if let Some(actual) = self.get_known_withdraw_trie_root(block_hash) {
                    self.post_execution_validator
                        .take_withdraw_trie_root_expectation(block_hash);
                    payload.expected_withdraw_trie_root = None;
                    if actual != expected {
                        return Err(NewPayloadError::Other(Box::new(
                            MorphConsensusError::WithdrawTrieRootMismatch { expected, actual },
                        )));
                    }
                } else if self
                    .provider
                    .sealed_header_by_hash(block_hash)
                    .map_err(|e| NewPayloadError::Other(Box::new(e)))?
                    .is_some()
                {
                    match self.provider.state_by_block_hash(block_hash) {
                        Ok(state) => {
                            let slot_val = state
                                .storage(
                                    L2_MESSAGE_QUEUE_ADDRESS,
                                    B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT),
                                )
                                .map_err(|e| NewPayloadError::Other(Box::new(e)))?
                                .unwrap_or_default();
                            let actual = B256::from(slot_val);
                            self.record_known_withdraw_trie_root(block_hash, actual);
                            self.post_execution_validator
                                .take_withdraw_trie_root_expectation(block_hash);
                            payload.expected_withdraw_trie_root = None;
                            if actual != expected {
                                return Err(NewPayloadError::Other(Box::new(
                                    MorphConsensusError::WithdrawTrieRootMismatch {
                                        expected,
                                        actual,
                                    },
                                )));
                            }
                        }
                        Err(
                            ProviderError::StateAtBlockPruned(_)
                            | ProviderError::StateForHashNotFound(_),
                        ) => {
                            tracing::warn!(
                                target: "morph::engine_validator",
                                %block_hash,
                                "skipping withdraw_trie_root check on persisted block with pruned historical state"
                            );
                            self.post_execution_validator
                                .take_withdraw_trie_root_expectation(block_hash);
                            payload.expected_withdraw_trie_root = None;
                        }
                        Err(err) => {
                            return Err(NewPayloadError::Other(Box::new(err)));
                        }
                    }
                }
            }
        }
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        mut payload: MorphExecutionData,
        ctx: TreeCtx<'_, MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let block_hash = payload.block.hash();
        let parent_hash = payload.block.parent_hash();
        // Extract the expectation directly onto the stack so the live `validate_payload`
        // path never inserts into (or can leak from) `expected_withdraw_trie_roots`.
        // Also clear any residual entry left by an earlier buffered conversion.
        let expected_withdraw_trie_root = payload.expected_withdraw_trie_root.take();
        self.post_execution_validator
            .take_withdraw_trie_root_expectation(block_hash);

        self.validate_next_l1_msg_index_for_block(payload.block.as_ref(), &ctx)?;
        let tree_hint = Some(self.find_tree_ancestor_withdraw_trie_root(parent_hash, &ctx));
        self.engine_consensus
            .record_mode(block_hash, payload.validation_mode);

        let output = self.inner.validate_payload(payload, ctx)?;
        let expectation = MorphEngineValidator::expectation_from_root(expected_withdraw_trie_root);
        self.verify_and_record_withdraw_trie_root(expectation, tree_hint, output)
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<morph_primitives::Block>,
        ctx: TreeCtx<'_, MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let block_hash = block.hash();
        let parent_hash = block.parent_hash();
        let l1_validation = self.validate_next_l1_msg_index_for_block(&block, &ctx);
        self.post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, l1_validation)?;
        let tree_hint = Some(self.find_tree_ancestor_withdraw_trie_root(parent_hash, &ctx));

        let validation = self.inner.validate_block(block, ctx);
        let output = self
            .post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, validation)?;
        let expectation = self
            .post_execution_validator
            .take_withdraw_trie_root_expectation(block_hash)
            .unwrap_or(WithdrawTrieRootExpectation::SkipValidation);
        self.verify_and_record_withdraw_trie_root(expectation, tree_hint, output)
    }

    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<MorphPrimitives>,
    ) -> ProviderResult<ExecutedBlock<MorphPrimitives>> {
        self.validate_pre_inserted_block(block.recovered_block.as_ref())?;
        let actual = self.resolve_post_block_withdraw_trie_root(
            block.recovered_block.parent_hash(),
            &block.execution_output.state,
            None,
        )?;
        self.record_known_withdraw_trie_root(block.recovered_block.hash(), actual);
        let executed = self.inner.on_inserted_executed_block(block)?;
        self.tree_headers
            .insert(executed.recovered_block().sealed_header().clone());
        Ok(executed)
    }

    fn on_canonical_head_changed(&self, hash: B256, state: &EngineApiTreeState<MorphPrimitives>) {
        if let Some(header) = state.tree_state().sealed_header_by_hash(&hash) {
            self.tree_headers.insert(header);
        }
        self.inner.on_canonical_head_changed(hash, state);
    }

    fn payload_builder_resources(
        &self,
        parent_hash: B256,
        parent_header: &MorphHeader,
        timestamp: u64,
        state: &mut EngineApiTreeState<MorphPrimitives>,
    ) -> reth_payload_builder::PayloadBuilderResources {
        self.inner
            .payload_builder_resources(parent_hash, parent_header, timestamp, state)
    }
}

impl<P, Evm> WaitForCaches for MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm,
    BasicEngineValidator<P, Evm, MorphEngineValidator>: WaitForCaches,
{
    fn wait_for_caches(&self) -> CacheWaitDurations {
        self.inner.wait_for_caches()
    }
}

/// Uses upstream's optimized state-root path from Jade onward. Before Jade it
/// computes and persists the real MPT updates but trusts the historical header
/// root, which is a ZK-trie commitment and cannot be compared to the MPT root.
#[derive(Debug)]
struct MorphStateRootStrategy {
    chain_spec: Arc<MorphChainSpec>,
    default: DefaultStateRootStrategy,
}

impl MorphStateRootStrategy {
    fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self {
            chain_spec,
            default: DefaultStateRootStrategy::default(),
        }
    }
}

impl<P, Evm> StateRootStrategy<MorphPrimitives, P, Evm> for MorphStateRootStrategy
where
    P: DatabaseProviderFactory
        + BlockReader<Header = MorphHeader>
        + StateProviderFactory
        + StateReader
        + Clone
        + Send
        + Sync
        + 'static,
    P::Provider: BlockNumReader
        + PruneCheckpointReader
        + StageCheckpointReader
        + StorageSettingsCache
        + TryIntoHistoricalStateProvider
        + 'static,
    Evm: ConfigureEvm<Primitives = MorphPrimitives> + 'static,
    DefaultStateRootStrategy: StateRootStrategy<MorphPrimitives, P, Evm>,
{
    fn prepare(
        &self,
        ctx: StateRootJobContext<'_, MorphPrimitives, P, Evm>,
    ) -> ProviderResult<PreparedStateRootJob<MorphPrimitives>> {
        let timestamp: u64 = ctx.env().evm_env.block_env.timestamp().saturating_to();
        if self.chain_spec.is_jade_active_at_timestamp(timestamp) {
            return self.default.prepare(ctx);
        }

        Ok(PreparedStateRootJob::new(
            Box::new(PreJadeStateRootJob {
                provider_builder: ctx.provider_builder(),
            }),
            None,
        ))
    }

    fn prepare_payload_builder(
        &self,
        ctx: PayloadStateRootJobContext<'_, MorphPrimitives, P>,
    ) -> ProviderResult<Option<PayloadStateRootHandle>> {
        if self.chain_spec.is_jade_active_at_timestamp(ctx.timestamp()) {
            return self.default.prepare_payload_builder(ctx);
        }
        Ok(None)
    }
}

struct PreJadeStateRootJob<P> {
    provider_builder: reth_engine_tree::tree::StateProviderBuilder<MorphPrimitives, P>,
}

impl<P> StateRootJob<MorphPrimitives> for PreJadeStateRootJob<P>
where
    P: DatabaseProviderFactory
        + BlockReader
        + StateProviderFactory
        + StateReader
        + Clone
        + Send
        + Sync
        + 'static,
    P::Provider: BlockNumReader
        + PruneCheckpointReader
        + StageCheckpointReader
        + StorageSettingsCache
        + TryIntoHistoricalStateProvider
        + 'static,
{
    fn name(&self) -> &'static str {
        "morph-pre-jade-trusted-header"
    }

    fn finish(
        &mut self,
        block: &RecoveredBlock<morph_primitives::Block>,
        _output: Arc<BlockExecutionOutput<morph_primitives::MorphReceipt>>,
        hashed_state: &LazyHashedPostState,
    ) -> ProviderResult<StateRootJobOutcome> {
        let provider = self.provider_builder.build()?;
        let (_mpt_root, trie_updates) =
            provider.state_root_with_updates(hashed_state.get().as_ref().clone())?;
        Ok(StateRootJobOutcome::new(
            block.header().state_root(),
            Arc::new(trie_updates),
        ))
    }
}

#[derive(Debug, Default)]
struct WithdrawTrieRootCache {
    entries: HashMap<B256, WithdrawTrieRootExpectation>,
    order: VecDeque<B256>,
}

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidator {
    expected_withdraw_trie_roots: Arc<Mutex<WithdrawTrieRootCache>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WithdrawTrieRootExpectation {
    SkipValidation,
    Verify(B256),
}

impl MorphEngineValidator {
    const MAX_EXPECTED_WITHDRAW_TRIE_ROOTS: usize = 4096;

    /// Creates a new [`MorphEngineValidator`].
    pub fn new() -> Self {
        Self::default()
    }

    fn expectation_from_root(root: Option<B256>) -> WithdrawTrieRootExpectation {
        match root {
            Some(root) if !root.is_zero() => WithdrawTrieRootExpectation::Verify(root),
            // Match morph-geth: zero means the caller did not provide this optional
            // CL/EL cross-check value, not that the expected root is actually zero.
            _ => WithdrawTrieRootExpectation::SkipValidation,
        }
    }

    fn record_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
        expectation: WithdrawTrieRootExpectation,
    ) {
        let mut cache = self.expected_withdraw_trie_roots.lock();
        let is_new_entry = cache.entries.insert(block_hash, expectation).is_none();

        if is_new_entry {
            cache.order.push_back(block_hash);

            while cache.entries.len() > Self::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
                let Some(evicted_hash) = cache.order.pop_front() else {
                    break;
                };
                cache.entries.remove(&evicted_hash);
            }
        }
    }

    fn take_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
    ) -> Option<WithdrawTrieRootExpectation> {
        let mut cache = self.expected_withdraw_trie_roots.lock();
        let removed = cache.entries.remove(&block_hash);

        if removed.is_some() {
            cache.order.retain(|hash| *hash != block_hash);
        }

        removed
    }

    fn clear_withdraw_trie_root_expectation_on_error<T, E>(
        &self,
        block_hash: B256,
        result: Result<T, E>,
    ) -> Result<T, E> {
        if result.is_err() {
            self.take_withdraw_trie_root_expectation(block_hash);
        }
        result
    }

    #[cfg(test)]
    fn updated_withdraw_trie_root_from_sorted_hashed_state(
        state_updates: &reth_trie::HashedPostStateSorted,
    ) -> Option<B256> {
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));

        state_updates
            .storages
            .get(&hashed_address)
            .and_then(|storage| {
                storage
                    .storage_slots
                    .binary_search_by_key(&hashed_slot, |(slot, _)| *slot)
                    .ok()
                    .map(|index| B256::from(storage.storage_slots[index].1))
            })
    }

    #[cfg(test)]
    fn validate_withdraw_trie_root_expectation(
        expectation: WithdrawTrieRootExpectation,
        state_updates: impl FnOnce() -> Arc<reth_trie::HashedPostStateSorted>,
    ) -> Result<(), ConsensusError> {
        let WithdrawTrieRootExpectation::Verify(expected) = expectation else {
            return Ok(());
        };

        let Some(actual) =
            Self::updated_withdraw_trie_root_from_sorted_hashed_state(state_updates().as_ref())
        else {
            // The slot was not touched, so its value is unchanged from the parent.
            return Ok(());
        };
        // Typed for the same reason as the `NextL1MsgIndex` check: the payload's withdraw
        // trie root is not covered by the block hash.
        if actual != expected {
            return Err(ConsensusError::other(
                MorphConsensusError::WithdrawTrieRootMismatch { expected, actual },
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn validate_withdraw_trie_root_update(
        &self,
        block_hash: B256,
        state_updates: impl FnOnce() -> Arc<reth_trie::HashedPostStateSorted>,
    ) -> Result<(), ConsensusError> {
        let Some(expectation) = self.take_withdraw_trie_root_expectation(block_hash) else {
            tracing::debug!(
                target: "morph::engine_validator",
                %block_hash,
                "no withdraw trie root expectation registered; skipping CL cross-check"
            );
            return Ok(());
        };
        Self::validate_withdraw_trie_root_expectation(expectation, state_updates)
    }
}

impl PayloadValidator<MorphPayloadTypes> for MorphEngineValidator {
    type Block = morph_primitives::Block;

    fn convert_payload_to_block(
        &self,
        payload: MorphExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        let expected_withdraw_trie_root = payload.expected_withdraw_trie_root;
        let sealed_block = Arc::unwrap_or_clone(payload.block);

        if expected_withdraw_trie_root.is_some() {
            let expectation = Self::expectation_from_root(expected_withdraw_trie_root);
            self.record_withdraw_trie_root_expectation(sealed_block.hash(), expectation);
        }

        Ok(sealed_block)
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &<MorphPayloadTypes as reth_node_api::PayloadTypes>::PayloadAttributes,
        header: &MorphHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // Ensure that payload attributes timestamp is not in the past
        if attr.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use reth_trie::{HashedPostState, HashedStorage};

    #[test]
    fn morph_tree_config_never_uses_upstream_empty_update_state_root_skip() {
        let config = reth_node_api::TreeConfig::default().with_skip_state_root(true);

        assert!(!strict_morph_tree_config(config).skip_state_root());
    }

    #[test]
    fn test_extract_updated_withdraw_trie_root_from_hashed_state() {
        let expected = B256::from([0x11; 32]);
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));

        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter([(hashed_slot, U256::from_be_bytes(expected.0))]),
        );

        assert_eq!(
            MorphEngineValidator::updated_withdraw_trie_root_from_sorted_hashed_state(
                &state.into_sorted(),
            ),
            Some(expected)
        );
    }

    #[test]
    fn test_extract_updated_withdraw_trie_root_from_hashed_state_missing_slot() {
        let state = HashedPostState::default();
        assert_eq!(
            MorphEngineValidator::updated_withdraw_trie_root_from_sorted_hashed_state(
                &state.into_sorted(),
            ),
            None
        );
    }

    #[test]
    fn test_withdraw_trie_root_expectation_cache_evicts_incrementally_not_clear_all() {
        let validator = MorphEngineValidator::new();
        let key = |n: usize| {
            let mut bytes = [0u8; 32];
            bytes[..8].copy_from_slice(&(n as u64).to_be_bytes());
            B256::from(bytes)
        };

        for i in 0..MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
            validator.record_withdraw_trie_root_expectation(
                key(i),
                WithdrawTrieRootExpectation::Verify(B256::from([0xaa; 32])),
            );
        }
        {
            let cache = validator.expected_withdraw_trie_roots.lock();
            assert_eq!(
                cache.entries.len(),
                MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
            );
            assert_eq!(cache.order.len(), cache.entries.len());
        }

        let oldest = key(0);
        let newest = key(MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS);
        validator.record_withdraw_trie_root_expectation(
            newest,
            WithdrawTrieRootExpectation::Verify(B256::from([0xbb; 32])),
        );

        let cache = validator.expected_withdraw_trie_roots.lock();
        assert_eq!(
            cache.entries.len(),
            MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );
        assert_eq!(cache.order.len(), cache.entries.len());
        assert!(!cache.entries.is_empty());
        assert!(cache.entries.contains_key(&newest));
        assert!(!cache.entries.contains_key(&oldest));
    }

    #[test]
    fn test_record_and_take_expectation_roundtrip() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x42; 32]);
        let expected_root = B256::from([0xee; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(expected_root),
        );

        // Take should return the expectation and remove it
        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(
            result,
            Some(WithdrawTrieRootExpectation::Verify(expected_root))
        );

        // Taking again should return None
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    #[test]
    fn validation_error_clears_withdraw_trie_root_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x43; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(B256::from([0xee; 32])),
        );

        let result = validator
            .clear_withdraw_trie_root_expectation_on_error(hash, Err::<(), _>("validation failed"));

        assert_eq!(result, Err("validation failed"));
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    #[test]
    fn validation_success_preserves_withdraw_trie_root_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x44; 32]);
        let expectation = WithdrawTrieRootExpectation::Verify(B256::from([0xee; 32]));
        validator.record_withdraw_trie_root_expectation(hash, expectation);

        let result = validator
            .clear_withdraw_trie_root_expectation_on_error(hash, Ok::<_, &str>("validated"));

        assert_eq!(result, Ok("validated"));
        assert_eq!(
            validator.take_withdraw_trie_root_expectation(hash),
            Some(expectation)
        );
    }

    #[test]
    fn test_record_skip_validation_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x99; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::SkipValidation,
        );

        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(result, Some(WithdrawTrieRootExpectation::SkipValidation));
    }

    #[test]
    fn test_duplicate_record_overwrites_value() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x11; 32]);
        let root1 = B256::from([0xaa; 32]);
        let root2 = B256::from([0xbb; 32]);

        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(root1),
        );
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(root2),
        );

        let result = validator.take_withdraw_trie_root_expectation(hash);
        assert_eq!(result, Some(WithdrawTrieRootExpectation::Verify(root2)));
    }

    #[test]
    fn test_take_nonexistent_returns_none() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0xff; 32]);
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    #[test]
    fn test_updated_withdraw_trie_root_wrong_address() {
        // If storage update is for a different address, should return None
        let wrong_address = keccak256(alloy_primitives::Address::ZERO);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));
        let state = HashedPostState::from_hashed_storage(
            wrong_address,
            HashedStorage::from_iter([(hashed_slot, U256::from_be_bytes([0x11; 32]))]),
        );
        assert!(
            MorphEngineValidator::updated_withdraw_trie_root_from_sorted_hashed_state(
                &state.into_sorted(),
            )
            .is_none()
        );
    }

    #[test]
    fn test_updated_withdraw_trie_root_wrong_slot() {
        // Correct address but wrong slot
        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let wrong_slot = keccak256(B256::from(alloy_primitives::U256::from(999)));
        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter([(wrong_slot, U256::from_be_bytes([0x22; 32]))]),
        );
        assert!(
            MorphEngineValidator::updated_withdraw_trie_root_from_sorted_hashed_state(
                &state.into_sorted(),
            )
            .is_none()
        );
    }

    fn empty_recovered_block_with_hash(
        hash: B256,
    ) -> reth_primitives_traits::RecoveredBlock<morph_primitives::Block> {
        let header = MorphHeader::default();
        let body = morph_primitives::BlockBody::default();
        let block = morph_primitives::Block::new(header, body);
        let sealed = reth_primitives_traits::SealedBlock::new_unchecked(block, hash);
        reth_primitives_traits::RecoveredBlock::new_sealed(sealed, Vec::new())
    }

    /// Block-input path (P2P sync, pipeline backfill) reaches
    /// `validate_withdraw_trie_root_update` without calling
    /// `convert_payload_to_block`, so no expectation is registered. The
    /// validator must treat the missing entry as `SkipValidation` and
    /// return `Ok` — otherwise sync stalls. The upstream strict state-root
    /// check (post-Jade) remains the source of truth.
    #[test]
    fn validate_block_post_execution_skips_when_no_expectation_registered() {
        let validator = MorphEngineValidator::new();
        let block = empty_recovered_block_with_hash(B256::from([0xab; 32]));

        let state = HashedPostState::default();
        let result = validator.validate_withdraw_trie_root_update(block.hash(), || {
            Arc::new(state.clone().into_sorted())
        });

        assert!(
            result.is_ok(),
            "missing expectation must be treated as SkipValidation, got {:?}",
            result.err()
        );
    }

    /// SkipValidation expectation (CL didn't supply a value) must be honored.
    #[test]
    fn validate_block_post_execution_honors_skip_validation_expectation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0xcd; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::SkipValidation,
        );
        let block = empty_recovered_block_with_hash(hash);

        let state = HashedPostState::default();
        let result = validator.validate_withdraw_trie_root_update(block.hash(), || {
            Arc::new(state.clone().into_sorted())
        });

        assert!(result.is_ok());
        // expectation must be consumed.
        assert!(
            validator
                .take_withdraw_trie_root_expectation(hash)
                .is_none()
        );
    }

    /// Verify expectation: when the slot wasn't touched we trust CL (no DB read)
    /// and pass through.
    #[test]
    fn validate_block_post_execution_passes_when_slot_unchanged() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x33; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(B256::from([0xee; 32])),
        );
        let block = empty_recovered_block_with_hash(hash);

        // empty hashed state → no withdraw-slot diff → skip per the doc-comment
        // explanation in the validator.
        let state = HashedPostState::default();
        let result = validator.validate_withdraw_trie_root_update(block.hash(), || {
            Arc::new(state.clone().into_sorted())
        });

        assert!(result.is_ok());
    }

    /// A zero withdraw-trie root from the CL means the value was not provided,
    /// matching morph-geth's long-standing zero-value compatibility behavior.
    #[test]
    fn validate_block_post_execution_treats_zero_expected_root_as_skip_validation() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x44; 32]);
        let block = empty_recovered_block_with_hash(hash);
        let payload = morph_payload_types::MorphExecutionData::with_expected_withdraw_trie_root(
            Arc::new(block.clone().into_sealed_block()),
            B256::ZERO,
        );

        validator
            .convert_payload_to_block(payload)
            .expect("payload conversion should succeed");

        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));
        let actual = B256::from([0xff; 32]);
        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter([(hashed_slot, U256::from_be_bytes(actual.0))]),
        );

        let result = validator.validate_withdraw_trie_root_update(block.hash(), || {
            Arc::new(state.clone().into_sorted())
        });

        assert!(
            result.is_ok(),
            "zero expected withdraw root should skip validation, got {:?}",
            result.err()
        );
    }

    #[test]
    fn convert_payload_to_block_none_root_does_not_leak_cache_entry() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x77; 32]);
        let block = empty_recovered_block_with_hash(hash);
        let payload =
            morph_payload_types::MorphExecutionData::new(Arc::new(block.into_sealed_block()));

        validator
            .convert_payload_to_block(payload)
            .expect("payload conversion should succeed");

        let cache = validator.expected_withdraw_trie_roots.lock();
        assert!(
            cache.entries.is_empty(),
            "None expected_withdraw_trie_root must not insert into cache"
        );
        assert!(cache.order.is_empty());
    }

    /// Verify expectation that mismatches an actually-updated slot must fail.
    #[test]
    fn validate_block_post_execution_rejects_mismatched_root() {
        let validator = MorphEngineValidator::new();
        let hash = B256::from([0x55; 32]);
        let expected = B256::from([0xee; 32]);
        let actual = B256::from([0xff; 32]);
        validator.record_withdraw_trie_root_expectation(
            hash,
            WithdrawTrieRootExpectation::Verify(expected),
        );

        let hashed_address = keccak256(L2_MESSAGE_QUEUE_ADDRESS);
        let hashed_slot = keccak256(B256::from(L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT));
        let state = HashedPostState::from_hashed_storage(
            hashed_address,
            HashedStorage::from_iter([(hashed_slot, U256::from_be_bytes(actual.0))]),
        );

        let block = empty_recovered_block_with_hash(hash);
        let err = validator
            .validate_withdraw_trie_root_update(block.hash(), || {
                Arc::new(state.clone().into_sorted())
            })
            .expect_err("mismatched root must fail");
        assert!(
            err.to_string().contains("withdraw trie root mismatch"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_validate_payload_attributes_timestamp_not_in_past() {
        use alloy_rpc_types_engine::PayloadAttributes;
        use morph_payload_types::MorphPayloadAttributes;
        use reth_node_api::PayloadValidator;

        let validator = MorphEngineValidator::new();

        // Create a header with timestamp 100
        let parent_header = MorphHeader {
            inner: alloy_consensus::Header {
                timestamp: 100,
                ..Default::default()
            },
            ..Default::default()
        };
        let parent = reth_primitives_traits::SealedHeader::seal_slow(parent_header);

        // Attributes with timestamp = 99 (before parent) should fail
        let attr = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 99,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: None,
            no_tx_pool: false,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr, parent.header())
                .is_err()
        );

        // Attributes with timestamp = 100 (equal to parent) should pass
        let attr_same = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 100,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: None,
            no_tx_pool: false,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr_same, parent.header())
                .is_ok()
        );

        // Attributes with timestamp = 101 (after parent) should pass
        let attr_future = MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: 101,
                prev_randao: B256::ZERO,
                suggested_fee_recipient: alloy_primitives::Address::ZERO,
                withdrawals: None,
                parent_beacon_block_root: None,
                slot_number: None,
                target_gas_limit: None,
            },
            transactions: None,
            no_tx_pool: false,
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr_future, parent.header())
                .is_ok()
        );
    }

    #[test]
    fn morph_engine_consensus_mode_records_monotonically_without_downgrade_or_full_deletion() {
        let consensus = MorphEngineConsensus::new(MorphConsensus::new(Arc::new(
            morph_chainspec::MORPH_MAINNET.as_ref().clone(),
        )));
        let hash = B256::from([0x91; 32]);

        assert_eq!(consensus.mode_for(hash), MorphPayloadValidationMode::Full);

        // `Full` never inserts into the bounded cache.
        consensus.record_mode(hash, MorphPayloadValidationMode::Full);
        assert!(consensus.modes.lock().entries.is_empty());

        // `LocallyBuilt` is retained and not deleted by a concurrent/subsequent `Full` call.
        consensus.record_mode(hash, MorphPayloadValidationMode::LocallyBuilt);
        assert_eq!(
            consensus.mode_for(hash),
            MorphPayloadValidationMode::LocallyBuilt
        );
        consensus.record_mode(hash, MorphPayloadValidationMode::Full);
        assert_eq!(
            consensus.mode_for(hash),
            MorphPayloadValidationMode::LocallyBuilt,
            "untrusted Full validation must never clobber LocallyBuilt mode"
        );

        // `Safe` upgrades `LocallyBuilt` and is never downgraded back to `LocallyBuilt` or `Full`.
        consensus.record_mode(hash, MorphPayloadValidationMode::Safe);
        assert_eq!(consensus.mode_for(hash), MorphPayloadValidationMode::Safe);
        consensus.record_mode(hash, MorphPayloadValidationMode::LocallyBuilt);
        assert_eq!(consensus.mode_for(hash), MorphPayloadValidationMode::Safe);
        consensus.record_mode(hash, MorphPayloadValidationMode::Full);
        assert_eq!(consensus.mode_for(hash), MorphPayloadValidationMode::Safe);

        // Bounded FIFO eviction at MAX_MODES (1024).
        for idx in 0..=MorphEngineConsensus::MAX_MODES {
            let mut raw = [0u8; 32];
            raw[..8].copy_from_slice(&(idx as u64).to_be_bytes());
            consensus.record_mode(B256::from(raw), MorphPayloadValidationMode::LocallyBuilt);
        }
        assert_eq!(
            consensus.modes.lock().entries.len(),
            MorphEngineConsensus::MAX_MODES
        );
        assert_eq!(
            consensus.mode_for(hash),
            MorphPayloadValidationMode::Full,
            "oldest mode entry must be evicted once MAX_MODES is exceeded"
        );
    }
}
