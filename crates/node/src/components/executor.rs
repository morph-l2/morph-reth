//! Morph EVM executor builder.

use crate::MorphNode;
use morph_evm::{MorphEvmConfig, evm::MorphEvmFactory};
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::ExecutorBuilder};

/// Builder for [`MorphEvmConfig`].
///
/// Creates the EVM configuration with Morph-specific execution logic.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for MorphExecutorBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
{
    type EVM = MorphEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = MorphEvmConfig::new(ctx.chain_spec(), MorphEvmFactory::default());
        Ok(evm_config)
    }
}
