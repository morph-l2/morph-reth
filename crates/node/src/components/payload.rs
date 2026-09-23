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

fn apply_chain_payload_limit(
    mut config: MorphBuilderConfig,
    chain_max_tx_payload_bytes: u64,
) -> eyre::Result<MorphBuilderConfig> {
    let max_da_block_size = config
        .max_da_block_size
        .unwrap_or(chain_max_tx_payload_bytes);
    eyre::ensure!(
        max_da_block_size <= chain_max_tx_payload_bytes,
        "--morph.max-tx-payload-bytes ({max_da_block_size}) exceeds the chain consensus limit \
         ({chain_max_tx_payload_bytes})"
    );
    config = config.with_max_da_block_size(max_da_block_size);
    Ok(config)
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
        let chain_max_tx_payload_bytes = ctx.chain_spec().max_tx_payload_bytes_per_block();
        let mut config = apply_chain_payload_limit(self.config, chain_max_tx_payload_bytes)?;
        let max_da_block_size = config
            .max_da_block_size
            .expect("chain payload limit is always applied");

        let desired_gas_limit = ctx.payload_builder_config().gas_limit();
        if let Some(desired) = desired_gas_limit {
            config = config.with_desired_gas_limit(desired);
        }

        let builder =
            MorphPayloadBuilder::with_config(pool, evm_config, ctx.provider().clone(), config);

        info!(
            target: "morph::node",
            ?desired_gas_limit,
            max_da_block_size,
            chain_max_tx_payload_bytes,
            "Payload builder initialized"
        );

        Ok(builder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_limit_is_the_default_builder_limit() {
        let config = apply_chain_payload_limit(MorphBuilderConfig::default(), 1024).unwrap();
        assert_eq!(config.max_da_block_size, Some(1024));
    }

    #[test]
    fn builder_limit_can_be_lower_than_chain_limit() {
        let config = apply_chain_payload_limit(
            MorphBuilderConfig::default().with_max_da_block_size(512),
            1024,
        )
        .unwrap();
        assert_eq!(config.max_da_block_size, Some(512));
    }

    #[test]
    fn builder_limit_cannot_exceed_chain_limit() {
        let err = apply_chain_payload_limit(
            MorphBuilderConfig::default().with_max_da_block_size(1025),
            1024,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("exceeds the chain consensus limit")
        );
    }
}
