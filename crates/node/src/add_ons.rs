//! Morph node RPC add-ons.

use crate::{
    MorphNode,
    validator::{MorphEngineValidatorBuilder, MorphTreeEngineValidatorBuilder},
};
use alloy_hardforks::ForkCondition;
use morph_chainspec::{MorphHardfork, MorphHardforks};
use morph_evm::MorphEvmConfig;
use morph_primitives::{Block, MorphHeader, MorphReceipt};
use morph_proofs::{MdbxProofsStorage, MorphProofsStorage};
use morph_reference_index::{ReferenceIndexConfig, ReferenceIndexRuntime};
use morph_rpc::{
    ExecutionWitnessApiExt, ExecutionWitnessApiOverrideServer, MorphEthApiBuilder,
    MorphEthConfigApiServer, MorphEthConfigHandler, ProofStatusApiExt,
    ProofStatusApiOverrideServer,
    eth::proofs::{EthProofApiExt, EthProofApiOverrideServer},
    morph::{MorphRpc, MorphRpcHandler, MorphRpcServer},
};
use reth_chain_state::CanonStateSubscriptions;
use reth_chainspec::EthChainSpec;
use reth_node_api::{AddOnsContext, FullNodeComponents, FullNodeTypes, NodeAddOns, NodePrimitives};
use reth_node_builder::{
    NodeAdapter,
    rpc::{
        EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder, NoopEngineApiBuilder,
        PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_provider::{
    BlockWriter, CanonChainTracker, ChainSpecProvider, DBProvider, DatabaseProviderFactory,
};
use reth_prune_types::PruneMode;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcNodeCore;
use reth_tracing::tracing;
use std::sync::Arc;

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
    AuthHttpMiddleware = Identity,
> {
    /// Inner RPC add-ons from reth.
    inner: RpcAddOns<N, EthB, PVB, NoopEngineApiBuilder, EVB, RpcMiddleware, AuthHttpMiddleware>,
    /// Optional storage plus the `eth_getMultiProof` account-target limit, used to replace the
    /// historical proof RPCs on the normal and auth servers.
    ///
    /// Held as one unit because the limit is only ever enforced by the RPC that this storage
    /// installs; there is no way to configure one without the other.
    proof_history: Option<(MorphProofsStorage<Arc<MdbxProofsStorage>>, usize)>,
}

impl<N> MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
{
    /// Creates a new [`MorphAddOns`] with default configuration.
    pub fn new() -> Self {
        let pvb = MorphEngineValidatorBuilder::default();
        Self {
            inner: RpcAddOns::new(
                MorphEthApiBuilder::default(),
                pvb.clone(),
                NoopEngineApiBuilder::default(),
                MorphTreeEngineValidatorBuilder::new(pvb),
                Identity::default(),
                Identity::default(),
            ),
            proof_history: None,
        }
    }

    /// Attach initialized proof-history storage, and the account-target limit its
    /// `eth_getMultiProof` override enforces, to the RPC add-ons.
    pub fn with_proof_history(
        mut self,
        storage: MorphProofsStorage<Arc<MdbxProofsStorage>>,
        max_multi_proof_targets: usize,
    ) -> Self {
        self.proof_history = Some((storage, max_multi_proof_targets));
        self
    }
}

fn ensure_reference_index_pruning_compatible(
    bodies_history_prune_mode: Option<PruneMode>,
) -> eyre::Result<()> {
    let can_prune_bodies =
        bodies_history_prune_mode.is_some_and(|mode| mode.next_pruned_block(None).is_some());
    if can_prune_bodies {
        eyre::bail!(
            "Morph reference index requires canonical block bodies from Jade onward; configured bodies-history pruning mode {bodies_history_prune_mode:?} can delete required bodies"
        )
    }
    Ok(())
}

impl<N> Default for MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
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
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
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
        let payload_builder = ctx.node.payload_builder_handle().clone();
        let chain_spec = ctx.node.provider().chain_spec();
        let beacon_engine_handle = ctx.beacon_engine_handle.clone();
        let task_executor = ctx.node.task_executor().clone();
        let block_tag_tracker = std::sync::Arc::new(morph_engine_api::BlockTagTracker::default());

        // The reference index backfills directly from canonical block bodies. Read the
        // effective modes from the provider so both CLI arguments and reth.toml are covered.
        let bodies_history_prune_mode = provider
            .database_provider_ro()?
            .prune_modes_ref()
            .bodies_history;
        ensure_reference_index_pruning_compatible(bodies_history_prune_mode)?;

        let canonical_notifications = provider.subscribe_to_canonical_state();
        let jade_timestamp = match chain_spec.morph_fork_activation(MorphHardfork::Jade) {
            ForkCondition::Timestamp(timestamp) => timestamp,
            ForkCondition::Never => u64::MAX,
            condition => eyre::bail!(
                "Morph reference index requires timestamp-based Jade activation, got {condition:?}"
            ),
        };
        let reference_index_path = ctx
            .config
            .datadir()
            .data_dir()
            .join("morph")
            .join("reference_index");
        let reference_index_config = ReferenceIndexConfig::new(
            &reference_index_path,
            chain_spec.chain().id(),
            chain_spec.genesis_hash(),
            jade_timestamp,
        );
        let (reference_index_runtime, reference_index_handle) =
            ReferenceIndexRuntime::new(reference_index_config, provider.clone());
        let runtime_executor = task_executor.clone();
        task_executor.spawn_task(async move {
            reference_index_runtime
                .run(runtime_executor, canonical_notifications)
                .await;
        });

        tracing::info!(
            target: "morph::reference_index",
            path = %reference_index_path.display(),
            "Morph reference index background runtime started"
        );

        // Create Morph eth_config handler (EIP-7910 + morph extension)
        let eth_config_handler =
            MorphEthConfigHandler::new(ctx.node.provider().clone(), ctx.node.evm_config().clone());

        let morph_rpc_ctx = MorphRpc::new(reference_index_handle, provider.clone());
        let reference_rpc_handler = MorphRpcHandler::new(morph_rpc_ctx);
        let proof_history = self.proof_history;

        // Use launch_add_ons_with to register custom Engine API and eth_config
        self.inner
            .launch_add_ons_with(ctx, move |container| {
                let reth_node_builder::rpc::RpcModuleContainer {
                    modules,
                    auth_module,
                    registry,
                    ..
                } = container;

                if let Some((storage, max_multi_proof_targets)) = proof_history {
                    let eth_api = registry.eth_api().clone();
                    let eth_api_witness = registry.eth_api().clone();
                    modules
                        .replace_configured(
                            EthProofApiExt::new(
                                eth_api.clone(),
                                storage.clone(),
                                max_multi_proof_targets,
                            )
                            .into_rpc(),
                        )
                        .map_err(|error| {
                            eyre::eyre!("Failed to replace normal historical proof RPCs: {error}")
                        })?;
                    auth_module
                        .replace_auth_methods(
                            EthProofApiExt::new(
                                eth_api,
                                storage.clone(),
                                max_multi_proof_targets,
                            )
                            .into_rpc(),
                        )
                        .map_err(|error| {
                            eyre::eyre!("Failed to replace auth historical proof RPCs: {error}")
                        })?;
                    // Route the debug-namespace witness RPCs to proof history as well. Left on
                    // reth's default they would rebuild the parent trie by replaying changesets
                    // backwards from the tip, which is exactly the cost this storage exists to
                    // avoid. Auth server is deliberately untouched: witness consumers use HTTP.
                    modules
                        .replace_configured(
                            ExecutionWitnessApiExt::new(eth_api_witness, storage.clone()).into_rpc(),
                        )
                        .map_err(|error| {
                            eyre::eyre!("Failed to replace historical execution witness RPCs: {error}")
                        })?;
                    modules
                        .replace_configured(ProofStatusApiExt::new(storage).into_rpc())
                        .map_err(|error| {
                            eyre::eyre!("Failed to register debug_proofsSyncStatus: {error}")
                        })?;
                    tracing::info!(target: "morph::node", "Historical proof RPCs registered");
                }

                // Register Morph eth_config handler (EIP-7910 + morph extension)
                // This provides eth_config on HTTP/WS/IPC for morphnode compatibility.
                tracing::debug!(target: "morph::node", "Registering Morph eth_config handler");
                modules
                    .merge_configured(eth_config_handler.into_rpc())
                    .map_err(|e| eyre::eyre!("Failed to register eth_config handler: {}", e))?;
                tracing::info!(target: "morph::node", "Morph eth_config handler registered successfully");

                // The namespace remains registered while the index catches up; handlers return
                // a structured unavailable/behind error until the durable cursor is live.
                tracing::debug!(target: "morph::node", "Registering morph_ RPC namespace");
                modules
                    .merge_configured(reference_rpc_handler.into_rpc())
                    .map_err(|e| eyre::eyre!("Failed to register morph_ RPC: {}", e))?;
                tracing::info!(target: "morph::node", "morph_ RPC namespace registered");

                // Create and register Morph L2 Engine API
                tracing::debug!(target: "morph::node", "Registering Morph L2 Engine API");

                // Create the Engine API implementation
                let engine_api =
                    morph_engine_api::RealMorphL2EngineApi::new(
                        provider,
                        payload_builder,
                        chain_spec,
                        beacon_engine_handle,
                        block_tag_tracker,
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
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
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

#[cfg(test)]
mod tests {
    use super::ensure_reference_index_pruning_compatible;
    use reth_prune_types::PruneMode;

    #[test]
    fn reference_index_accepts_no_bodies_history_pruning() {
        assert!(ensure_reference_index_pruning_compatible(None).is_ok());
    }

    #[test]
    fn reference_index_accepts_noop_bodies_history_pruning() {
        assert!(ensure_reference_index_pruning_compatible(Some(PruneMode::Before(0))).is_ok());
    }

    #[test]
    fn reference_index_rejects_effective_bodies_history_pruning() {
        for mode in [
            PruneMode::Before(1),
            PruneMode::Distance(10_064),
            PruneMode::Full,
        ] {
            let error = ensure_reference_index_pruning_compatible(Some(mode)).unwrap_err();
            assert!(error.to_string().contains("bodies-history pruning"));
        }
    }
}
