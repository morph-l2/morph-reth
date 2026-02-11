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
use reth_rpc_builder::Identity;
use reth_rpc_eth_api::RpcNodeCore;

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
        self.inner.launch_add_ons(ctx).await
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
