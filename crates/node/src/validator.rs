//! Morph engine validator.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, keccak256};
use dashmap::DashMap;
use morph_chainspec::{
    L2_MESSAGE_QUEUE_ADDRESS, L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT, MorphChainSpec,
    MorphHardforks,
};
use morph_payload_types::{MorphExecutionData, MorphPayloadTypes};
use morph_primitives::{MorphHeader, MorphPrimitives};
use parking_lot::Mutex;
use reth_chain_state::ExecutedBlock;
use reth_chainspec::EthChainSpec;
use reth_engine_tree::tree::payload_validator::TreeCtx;
use reth_engine_tree::tree::{
    BasicEngineValidator, CacheWaitDurations, EngineApiTreeState, EngineValidator, WaitForCaches,
    error::{InsertBlockError, InsertBlockErrorKind},
    state_root_strategy::{
        DefaultStateRootStrategy, LazyHashedPostState, PayloadStateRootHandle,
        PayloadStateRootJobContext, PreparedStateRootJob, StateRootJob, StateRootJobContext,
        StateRootJobOutcome, StateRootStrategy,
    },
};
use reth_errors::{ConsensusError, ProviderResult};
use reth_evm::{ConfigureEvm, revm::context::Block as _};
use reth_node_api::{
    AddOnsContext, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError, NodeTypes,
    PayloadAttributes, PayloadTypes, PayloadValidator,
};
use reth_node_builder::{
    invalid_block_hook::InvalidBlockHookExt,
    rpc::{EngineValidatorBuilder, PayloadValidatorBuilder},
};
use reth_payload_primitives::BuiltPayloadExecutedBlock;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_provider::{
    BlockExecutionOutput, BlockNumReader, BlockReader, ChainSpecProvider, DatabaseProviderFactory,
    PruneCheckpointReader, StageCheckpointReader, StateProviderFactory, StateReader,
    StateRootProvider, StorageSettingsCache, TryIntoHistoricalStateProvider,
};
use reth_storage_overlay::OverlayManager;
use reth_tracing::tracing;
use std::{collections::VecDeque, sync::Arc};

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
/// This wires [`MorphEngineValidator`] into both payload validation and state-root
/// decision/validation hooks.
#[derive(Debug, Clone)]
pub struct MorphTreeEngineValidatorBuilder<PVB = MorphEngineValidatorBuilder> {
    payload_validator_builder: PVB,
}

impl<PVB> MorphTreeEngineValidatorBuilder<PVB> {
    /// Creates a new instance with the given payload validator builder.
    pub const fn new(payload_validator_builder: PVB) -> Self {
        Self {
            payload_validator_builder,
        }
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
    Node::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
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
        let state_root_strategy: Arc<
            dyn StateRootStrategy<MorphPrimitives, Node::Provider, Node::Evm>,
        > = Arc::new(MorphStateRootStrategy::new(chain_spec.clone()));
        let tree_config = strict_morph_tree_config(tree_config);
        let validator = BasicEngineValidator::new(
            provider.clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            invalid_block_hook,
            overlay_manager,
            ctx.node.task_executor().clone(),
        )
        .with_state_root_strategy(state_root_strategy);

        Ok(MorphTreeEngineValidator::new(
            validator,
            provider,
            chain_spec,
            post_execution_validator,
        ))
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

/// Upstream engine validator plus Morph-specific validation hooks.
///
/// State execution, caching and trie maintenance remain entirely upstream. This
/// wrapper preserves the parent-aware L1 queue invariant and the optional
/// consensus-layer withdraw-trie-root cross-check.
pub struct MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm,
{
    inner: BasicEngineValidator<P, Evm, MorphEngineValidator>,
    provider: P,
    chain_spec: Arc<MorphChainSpec>,
    post_execution_validator: MorphEngineValidator,
}

impl<P, Evm> MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm,
{
    const fn new(
        inner: BasicEngineValidator<P, Evm, MorphEngineValidator>,
        provider: P,
        chain_spec: Arc<MorphChainSpec>,
        post_execution_validator: MorphEngineValidator,
    ) -> Self {
        Self {
            inner,
            provider,
            chain_spec,
            post_execution_validator,
        }
    }
}

impl<P, Evm> MorphTreeEngineValidator<P, Evm>
where
    Evm: ConfigureEvm<Primitives = MorphPrimitives>,
    P: BlockReader<Header = MorphHeader>,
{
    fn validate_next_l1_msg_index(
        &self,
        block: &SealedBlock<morph_primitives::Block>,
        ctx: &TreeCtx<'_, MorphPrimitives>,
    ) -> Result<(), InsertBlockErrorKind> {
        if !self
            .chain_spec
            .is_jade_active_at_timestamp(block.header().timestamp())
        {
            return Ok(());
        }

        let parent_hash = block.parent_hash();
        let parent = match ctx.state().tree_state().sealed_header_by_hash(&parent_hash) {
            Some(parent) => Some(parent),
            None => self.provider.sealed_header_by_hash(parent_hash)?,
        };
        let Some(parent) = parent else {
            // The upstream validator reports the missing parent with the
            // canonical provider error before executing the block.
            return Ok(());
        };

        let mut expected = parent.next_l1_msg_index;
        for tx in block.body().transactions() {
            if !tx.is_l1_msg() {
                break;
            }
            let queue_index = tx.queue_index().ok_or_else(|| {
                ConsensusError::msg("L1 message transaction is missing queue index")
            })?;
            expected = queue_index.checked_add(1).ok_or_else(|| {
                ConsensusError::msg(format!(
                    "invalid block.NextL1MsgIndex: expected {}, got {}",
                    u64::MAX,
                    block.header().next_l1_msg_index
                ))
            })?;
        }

        let actual = block.header().next_l1_msg_index;
        if actual != expected {
            return Err(ConsensusError::msg(format!(
                "invalid block.NextL1MsgIndex: expected {expected}, got {actual}"
            ))
            .into());
        }
        Ok(())
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

    fn validate_withdraw_trie_root(
        &self,
        output: reth_engine_tree::tree::ValidationOutput<MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let block = output.executed_block.recovered_block();
        if let Err(err) = self
            .post_execution_validator
            .validate_withdraw_trie_root_update(block.hash(), || {
                output.executed_block.hashed_state()
            })
        {
            return Err(InsertBlockError::consensus_error(
                err,
                output.executed_block.sealed_block().clone(),
            )
            .into());
        }

        Ok(output)
    }
}

impl<P, Evm> EngineValidator<MorphPayloadTypes, MorphPrimitives>
    for MorphTreeEngineValidator<P, Evm>
where
    P: BlockReader<Header = MorphHeader> + Send + Sync + 'static,
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
        payload: MorphExecutionData,
    ) -> Result<SealedBlock<morph_primitives::Block>, NewPayloadError> {
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        payload: MorphExecutionData,
        ctx: TreeCtx<'_, MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let block_hash = payload.block.hash();
        // `convert_payload_to_block` may already have registered a withdraw-root
        // expectation for this hash. Clear it on every early reject path so a
        // later re-import of the same hash does not observe a stale entry.
        let l1_validation = self.validate_next_l1_msg_index_for_block(payload.block.as_ref(), &ctx);
        self.post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, l1_validation)?;

        let validation = self.inner.validate_payload(payload, ctx);
        let output = self
            .post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, validation)?;
        self.validate_withdraw_trie_root(output)
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<morph_primitives::Block>,
        ctx: TreeCtx<'_, MorphPrimitives>,
    ) -> reth_engine_tree::tree::ValidationOutcome<MorphPrimitives> {
        let block_hash = block.hash();
        let l1_validation = self.validate_next_l1_msg_index_for_block(&block, &ctx);
        self.post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, l1_validation)?;

        let validation = self.inner.validate_block(block, ctx);
        let output = self
            .post_execution_validator
            .clear_withdraw_trie_root_expectation_on_error(block_hash, validation)?;
        self.validate_withdraw_trie_root(output)
    }

    fn on_inserted_executed_block(
        &self,
        block: BuiltPayloadExecutedBlock<MorphPrimitives>,
    ) -> ProviderResult<ExecutedBlock<MorphPrimitives>> {
        self.inner.on_inserted_executed_block(block)
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

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphEngineValidator {
    expected_withdraw_trie_roots: Arc<DashMap<B256, WithdrawTrieRootExpectation>>,
    expected_withdraw_trie_root_order: Arc<Mutex<VecDeque<B256>>>,
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

    fn record_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
        expectation: WithdrawTrieRootExpectation,
    ) {
        let is_new_entry = self
            .expected_withdraw_trie_roots
            .insert(block_hash, expectation)
            .is_none();

        if is_new_entry {
            let mut order = self.expected_withdraw_trie_root_order.lock();
            order.push_back(block_hash);

            while self.expected_withdraw_trie_roots.len() > Self::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS {
                let Some(evicted_hash) = order.pop_front() else {
                    break;
                };
                self.expected_withdraw_trie_roots.remove(&evicted_hash);
            }
        }
    }

    fn take_withdraw_trie_root_expectation(
        &self,
        block_hash: B256,
    ) -> Option<WithdrawTrieRootExpectation> {
        let removed = self
            .expected_withdraw_trie_roots
            .remove(&block_hash)
            .map(|(_, expected)| expected);

        if removed.is_some() {
            self.expected_withdraw_trie_root_order
                .lock()
                .retain(|hash| *hash != block_hash);
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
        let WithdrawTrieRootExpectation::Verify(expected) = expectation else {
            return Ok(());
        };

        let Some(actual) =
            Self::updated_withdraw_trie_root_from_sorted_hashed_state(state_updates().as_ref())
        else {
            // The slot was not touched, so its value is unchanged from the parent.
            return Ok(());
        };
        if actual != expected {
            return Err(ConsensusError::msg(format!(
                "withdraw trie root mismatch: expected {expected}, got {actual}"
            )));
        }
        Ok(())
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

        let expectation = match expected_withdraw_trie_root {
            Some(root) if root != B256::ZERO => WithdrawTrieRootExpectation::Verify(root),
            // Match morph-geth: zero means the caller did not provide this optional
            // CL/EL cross-check value, not that the expected root is actually zero.
            _ => WithdrawTrieRootExpectation::SkipValidation,
        };
        self.record_withdraw_trie_root_expectation(sealed_block.hash(), expectation);

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
        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );

        let oldest = key(0);
        let newest = key(MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS);
        validator.record_withdraw_trie_root_expectation(
            newest,
            WithdrawTrieRootExpectation::Verify(B256::from([0xbb; 32])),
        );

        assert_eq!(
            validator.expected_withdraw_trie_roots.len(),
            MorphEngineValidator::MAX_EXPECTED_WITHDRAW_TRIE_ROOTS
        );
        assert!(!validator.expected_withdraw_trie_roots.is_empty());
        assert!(
            validator
                .expected_withdraw_trie_roots
                .get(&newest)
                .is_some()
        );
        assert!(
            validator
                .expected_withdraw_trie_roots
                .get(&oldest)
                .is_none()
        );
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
    /// `validate_block_post_execution_with_hashed_state` without calling
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
            gas_limit: None,
            base_fee_per_gas: None,
        };
        assert!(
            validator
                .validate_payload_attributes_against_header(&attr_future, parent.header())
                .is_ok()
        );
    }
}
