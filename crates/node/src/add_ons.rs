//! Morph node RPC add-ons.

use crate::{
    MorphNode,
    validator::{MorphEngineValidatorBuilder, MorphTreeEngineValidatorBuilder},
};
use alloy_consensus::BlockHeader;
use morph_evm::MorphEvmConfig;
use morph_primitives::{Block, MorphHeader, MorphReceipt};
use morph_rpc::MorphEthApiBuilder;
use reth_node_api::{AddOnsContext, FullNodeComponents, FullNodeTypes, NodeAddOns, NodePrimitives};
use reth_node_builder::{
    NodeAdapter,
    rpc::{
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, NoopEngineApiBuilder,
        PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_provider::{
    BlockNumReader, BlockWriter, CanonChainTracker, ChainSpecProvider, DBProvider,
    DatabaseProviderFactory,
};
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcNodeCore;
use reth_stages_types::{StageCheckpoint, StageId};
use reth_storage_api::StageCheckpointWriter;
use reth_tracing::tracing;
use tokio_stream::StreamExt;

/// Flush `StageId::Finish` every N canonical blocks.
const STAGE_FINISH_FLUSH_INTERVAL: u64 = 128;

/// Morph node add-ons for RPC and Engine API.
///
/// This wraps reth's [`RpcAddOns`] with Morph-specific configuration:
/// - Uses [`MorphEthApiBuilder`] for the eth_ RPC namespace
/// - Uses [`MorphEngineValidatorBuilder`] for payload validation
/// - Uses [`NoopEngineApiBuilder`] (Morph uses custom L2 Engine API)
#[derive(Debug)]
pub struct MorphAddOns<
    N: FullNodeComponents,
    EthB: EthApiBuilder<N> = MorphEthApiBuilder,
    PVB = MorphEngineValidatorBuilder,
    EVB = MorphTreeEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    /// Inner RPC add-ons from reth.
    inner: RpcAddOns<N, EthB, PVB, NoopEngineApiBuilder, EVB, RpcMiddleware>,
}

impl<N> MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider + StageCheckpointWriter,
{
    /// Creates a new [`MorphAddOns`] with default configuration.
    pub fn new() -> Self {
        Self::with_geth_rpc_url(None)
    }

    /// Creates a new [`MorphAddOns`] with an optional geth RPC URL for state root validation.
    pub fn with_geth_rpc_url(geth_rpc_url: Option<String>) -> Self {
        let pvb = MorphEngineValidatorBuilder::default().with_geth_rpc_url(geth_rpc_url);
        Self {
            inner: RpcAddOns::new(
                MorphEthApiBuilder::default(),
                pvb.clone(),
                NoopEngineApiBuilder::default(),
                MorphTreeEngineValidatorBuilder::new(pvb),
                Identity::default(),
            ),
        }
    }
}

impl<N> Default for MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider + StageCheckpointWriter,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<N, EthB, PVB, EVB> NodeAddOns<N> for MorphAddOns<N, EthB, PVB, EVB>
where
    N: FullNodeComponents<Types = MorphNode, Evm = MorphEvmConfig>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider + StageCheckpointWriter,
    EthB: EthApiBuilder<N>,
    PVB: Send + PayloadValidatorBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthB::EthApi:
        RpcNodeCore<Evm = MorphEvmConfig, Primitives: NodePrimitives<BlockHeader = MorphHeader>>,
{
    type Handle = <RpcAddOns<N, EthB, PVB, NoopEngineApiBuilder, EVB> as NodeAddOns<N>>::Handle;

    async fn launch_add_ons(self, ctx: AddOnsContext<'_, N>) -> eyre::Result<Self::Handle> {
        use morph_engine_api::MorphL2EngineRpcServer; // Import the RPC trait for into_rpc() method

        // Get components from ctx.node BEFORE calling launch_add_ons_with
        // This is necessary because we can't access ctx.node inside the closure
        let provider = ctx.node.provider().clone();
        tracing::info!(
            target: "morph::node",
            best_before = ?provider.best_block_number(),
            db_last_before = ?provider.last_block_number(),
            "launch_add_ons: provider startup snapshot"
        );
        let provider_for_stage = provider.clone();

        let payload_builder = ctx.node.payload_builder_handle().clone();
        let chain_spec = ctx.node.provider().chain_spec();
        let beacon_engine_handle = ctx.beacon_engine_handle.clone();
        let engine_events = ctx.engine_events.clone();
        let task_executor = ctx.node.task_executor().clone();
        let engine_state_tracker =
            std::sync::Arc::new(morph_engine_api::EngineStateTracker::default());

        // Keep a local view of canonical head/forkchoice from reth engine events.
        // Also persist StageId::Finish in batches so restart head lookup tracks
        // engine-imported progress without adding a DB commit on every block.
        let tracker_for_events = engine_state_tracker.clone();
        task_executor.spawn_critical("morph engine state tracker", async move {
            let mut listener = engine_events.new_listener();
            let mut last_flushed_finish = 0_u64;
            let mut latest_canonical_head = 0_u64;
            while let Some(event) = listener.next().await {
                tracker_for_events.on_consensus_engine_event(&event);
                if let reth_node_api::ConsensusEngineEvent::CanonicalChainCommitted(header, _) =
                    &event
                {
                    latest_canonical_head = header.number();
                    if latest_canonical_head.saturating_sub(last_flushed_finish)
                        >= STAGE_FINISH_FLUSH_INTERVAL
                        && persist_stage_finish(&provider_for_stage, latest_canonical_head)
                    {
                        last_flushed_finish = latest_canonical_head;
                    }
                }
            }

            // Best-effort flush of any trailing progress when listener terminates.
            if latest_canonical_head > last_flushed_finish {
                let _ = persist_stage_finish(&provider_for_stage, latest_canonical_head);
            }
        });

        // Use launch_add_ons_with to register custom Engine API
        self.inner
            .launch_add_ons_with(ctx, move |container| {
                let reth_node_builder::rpc::RpcModuleContainer {
                    auth_module, ..
                } = container;

                // Create and register Morph L2 Engine API
                tracing::debug!(target: "morph::node", "Registering Morph L2 Engine API");

                // Create the Engine API implementation
                let engine_api =
                    morph_engine_api::RealMorphL2EngineApi::new(
                        provider,
                        payload_builder,
                        chain_spec,
                        beacon_engine_handle,
                        engine_state_tracker,
                    );

                // Create the RPC handler
                let handler = morph_engine_api::MorphL2EngineRpcHandler::new(engine_api);

                // Register to the `engine` namespace (for authenticated RPC)
                // This adds the custom L2 Engine API methods (assembleL2Block, validateL2Block, etc.)
                auth_module
                    .merge_auth_methods(handler.into_rpc())
                    .map_err(|e| eyre::eyre!("Failed to register Morph L2 Engine API: {}", e))?;

                tracing::info!(target: "morph::node", "Morph L2 Engine API registered successfully");

                Ok(())
            })
            .await
    }
}

impl<N, EthB, PVB, EVB> RethRpcAddOns<N> for MorphAddOns<N, EthB, PVB, EVB>
where
    N: FullNodeComponents<Types = MorphNode, Evm = MorphEvmConfig>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider + StageCheckpointWriter,
    EthB: EthApiBuilder<N>,
    PVB: PayloadValidatorBuilder<N>,
    EVB: EngineValidatorBuilder<N>,
    EthB::EthApi:
        RpcNodeCore<Evm = MorphEvmConfig, Primitives: NodePrimitives<BlockHeader = MorphHeader>>,
{
    type EthApi = EthB::EthApi;

    fn hooks_mut(&mut self) -> &mut reth_node_builder::rpc::RpcHooks<N, Self::EthApi> {
        self.inner.hooks_mut()
    }
}

impl<N, EthB, PVB, EVB> EngineValidatorAddOn<N> for MorphAddOns<N, EthB, PVB, EVB>
where
    N: FullNodeComponents<Types = MorphNode, Evm = MorphEvmConfig>,
    EthB: EthApiBuilder<N>,
    PVB: Send,
    EVB: EngineValidatorBuilder<N>,
{
    type ValidatorBuilder = EVB;

    fn engine_validator_builder(&self) -> Self::ValidatorBuilder {
        self.inner.engine_validator_builder()
    }
}

/// Persists `StageId::Finish` to the database so that [`BlockchainProvider::new`] initializes
/// `canonical_in_memory_state` and `tree_state.current_canonical_head` correctly on restart.
///
/// Morph-reth bypasses the staged-sync pipeline entirely — blocks arrive only via the engine API
/// — so the pipeline's Finish stage never runs. We update this checkpoint in batched writes from
/// canonical-commit events to amortize database commit overhead during high-throughput sync.
fn persist_stage_finish<P>(provider: &P, block_number: u64) -> bool
where
    P: DatabaseProviderFactory,
    P::ProviderRW: StageCheckpointWriter + DBProvider,
{
    match provider.database_provider_rw() {
        Ok(provider_rw) => {
            if let Err(e) = provider_rw
                .save_stage_checkpoint(StageId::Finish, StageCheckpoint::new(block_number))
            {
                tracing::error!(
                    target: "morph::node",
                    block_number,
                    error = %e,
                    "failed to save StageId::Finish checkpoint"
                );
                return false;
            }
            if let Err(e) = provider_rw.commit() {
                tracing::error!(
                    target: "morph::node",
                    block_number,
                    error = %e,
                    "failed to commit StageId::Finish checkpoint"
                );
                return false;
            }
            tracing::debug!(
                target: "morph::node",
                block_number,
                "flushed StageId::Finish checkpoint"
            );
            true
        }
        Err(e) => {
            tracing::error!(
                target: "morph::node",
                block_number,
                error = %e,
                "failed to open database provider for StageId::Finish write"
            );
            false
        }
    }
}
