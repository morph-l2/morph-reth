//! Morph node RPC add-ons.

use crate::{MorphNode, validator::MorphEngineValidatorBuilder};
use morph_evm::MorphEvmConfig;
use morph_primitives::MorphHeader;
use morph_rpc::MorphEthApiBuilder;
use reth_node_api::{AddOnsContext, FullNodeComponents, FullNodeTypes, NodeAddOns, NodePrimitives};
use reth_node_builder::{
    NodeAdapter,
    rpc::{
        BasicEngineValidatorBuilder, EngineValidatorAddOn, EngineValidatorBuilder, EthApiBuilder,
        NoopEngineApiBuilder, PayloadValidatorBuilder, RethRpcAddOns, RpcAddOns,
    },
};
use reth_provider::ChainSpecProvider;
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcNodeCore;
use reth_tracing::tracing;

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
    EVB = BasicEngineValidatorBuilder<PVB>,
    RpcMiddleware = Identity,
> {
    /// Inner RPC add-ons from reth.
    inner: RpcAddOns<N, EthB, PVB, NoopEngineApiBuilder, EVB, RpcMiddleware>,
}

impl<N> MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
{
    /// Creates a new [`MorphAddOns`] with default configuration.
    pub fn new() -> Self {
        Self {
            inner: RpcAddOns::new(
                MorphEthApiBuilder::default(),
                MorphEngineValidatorBuilder,
                NoopEngineApiBuilder::default(),
                BasicEngineValidatorBuilder::default(),
                Identity::default(),
            ),
        }
    }
}

impl<N> Default for MorphAddOns<NodeAdapter<N>, MorphEthApiBuilder>
where
    N: FullNodeTypes<Types = MorphNode>,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<N, EthB, PVB, EVB> NodeAddOns<N> for MorphAddOns<N, EthB, PVB, EVB>
where
    N: FullNodeComponents<Types = MorphNode, Evm = MorphEvmConfig>,
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
                    morph_engine_api::RealMorphL2EngineApi::new(provider, payload_builder, chain_spec);

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
