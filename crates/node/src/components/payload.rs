//! Morph payload builder builder.

use crate::MorphNode;
use morph_evm::MorphEvmConfig;
use morph_payload_builder::{MorphBuilderConfig, MorphPayloadBuilder};
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::PayloadBuilderBuilder};
use reth_node_core::cli::config::PayloadBuilderConfig;
use reth_tracing::tracing::info;
use reth_transaction_pool::blobstore::InMemoryBlobStore;

/// Builder for [`MorphPayloadBuilder`].
///
/// Creates the payload builder for constructing L2 blocks with:
/// - L1 message transaction handling
/// - Sequencer forced transaction support
/// - Pool transaction inclusion
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphPayloadBuilderBuilder {
    /// Configuration for the payload builder.
    config: MorphBuilderConfig,
}

impl MorphPayloadBuilderBuilder {
    /// Creates a new [`MorphPayloadBuilderBuilder`] with the given configuration.
    pub const fn new(config: MorphBuilderConfig) -> Self {
        Self { config }
    }

    /// Sets the maximum DA block size (transaction payload bytes per block).
    pub fn with_max_da_block_size(mut self, max_da_block_size: u64) -> Self {
        self.config = self.config.with_max_da_block_size(max_da_block_size);
        self
    }
}

impl<Node>
    PayloadBuilderBuilder<
        Node,
        morph_txpool::MorphTransactionPool<Node::Provider, InMemoryBlobStore>,
        MorphEvmConfig,
    > for MorphPayloadBuilderBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
{
    type PayloadBuilder = MorphPayloadBuilder<
        morph_txpool::MorphTransactionPool<Node::Provider, InMemoryBlobStore>,
        Node::Provider,
    >;

    async fn build_payload_builder(
        self,
        ctx: &BuilderContext<Node>,
        pool: morph_txpool::MorphTransactionPool<Node::Provider, InMemoryBlobStore>,
        evm_config: MorphEvmConfig,
    ) -> eyre::Result<Self::PayloadBuilder> {
        let mut config = self.config;
        let desired_gas_limit = ctx.payload_builder_config().gas_limit();
        if let Some(desired) = desired_gas_limit {
            config = config.with_desired_gas_limit(desired);
        }

        let builder =
            MorphPayloadBuilder::with_config(pool, evm_config, ctx.provider().clone(), config);

        info!(
            target: "morph::node",
            ?desired_gas_limit,
            "Payload builder initialized"
        );

        Ok(builder)
    }
}
