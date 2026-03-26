//! Morph Node implementation.
//!
//! This module provides the core `MorphNode` type for assembling a Morph L2 node
//! using reth's node builder framework.
//!
//! # Components
//!
//! The node is assembled from the following builders:
//! - [`MorphPoolBuilder`]: Transaction pool with L1 fee validation
//! - [`MorphExecutorBuilder`]: EVM executor with Morph-specific logic
//! - [`MorphConsensusBuilder`]: Consensus validation for L2 blocks
//! - [`MorphPayloadBuilderBuilder`]: Block building with L1 message handling
//!

use super::{
    add_ons::MorphAddOns,
    args::MorphArgs,
    components::{
        MorphConsensusBuilder, MorphExecutorBuilder, MorphPayloadBuilderBuilder, MorphPoolBuilder,
    },
};
use alloy_consensus::BlockHeader;
use alloy_hardforks::EthereumHardforks;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use morph_chainspec::MorphChainSpec;
use morph_payload_builder::MorphBuilderConfig;
use morph_payload_types::{MorphPayloadAttributes, MorphPayloadTypes};
use morph_primitives::{Block, MorphHeader, MorphPrimitives, MorphReceipt, MorphTxEnvelope};
use reth_node_api::{FullNodeComponents, FullNodeTypes, NodeTypes, PayloadTypes};
use reth_node_builder::{
    DebugNode, Node, NodeAdapter,
    components::{BasicPayloadServiceBuilder, ComponentsBuilder},
};
use reth_node_ethereum::EthereumNetworkBuilder;
use reth_payload_primitives::PayloadAttributesBuilder;
use reth_primitives_traits::SealedHeader;
use reth_provider::{
    BlockWriter, CanonChainTracker, DBProvider, DatabaseProviderFactory, EthStorage,
    providers::ProviderFactoryBuilder,
};
use std::sync::Arc;

/// Type configuration for a Morph L2 node.
///
/// `MorphNode` implements reth's [`NodeTypes`] trait, defining the core types
/// used throughout the node:
/// - Primitives: [`MorphPrimitives`] (block, header, transaction, receipt types)
/// - ChainSpec: [`MorphChainSpec`] (hardfork configuration)
/// - Payload: [`MorphPayloadTypes`] (payload building types)
#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct MorphNode {
    /// Morph-specific CLI arguments.
    pub args: MorphArgs,
}

impl MorphNode {
    /// Creates a new [`MorphNode`] with the given CLI arguments.
    pub const fn new(args: MorphArgs) -> Self {
        Self { args }
    }

    /// Instantiates a [`ProviderFactoryBuilder`] for a Morph node.
    pub fn provider_factory_builder() -> ProviderFactoryBuilder<Self> {
        ProviderFactoryBuilder::default()
    }

    /// Returns a [`ComponentsBuilder`] configured for a Morph node.
    pub fn components<N>(
        payload_builder_config: MorphBuilderConfig,
    ) -> ComponentsBuilder<
        N,
        MorphPoolBuilder,
        BasicPayloadServiceBuilder<MorphPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        MorphExecutorBuilder,
        MorphConsensusBuilder,
    >
    where
        N: FullNodeTypes<Types = Self>,
    {
        ComponentsBuilder::default()
            .node_types::<N>()
            .pool(MorphPoolBuilder::default())
            .executor(MorphExecutorBuilder::default())
            .payload(BasicPayloadServiceBuilder::new(
                MorphPayloadBuilderBuilder::new(payload_builder_config),
            ))
            .network(EthereumNetworkBuilder::default())
            .consensus(MorphConsensusBuilder::default())
    }
}

impl NodeTypes for MorphNode {
    type Primitives = MorphPrimitives;
    type ChainSpec = MorphChainSpec;
    type Storage = EthStorage<MorphTxEnvelope, MorphHeader>;
    type Payload = MorphPayloadTypes;
}

impl<N> Node<N> for MorphNode
where
    N: FullNodeTypes<Types = Self>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        MorphPoolBuilder,
        BasicPayloadServiceBuilder<MorphPayloadBuilderBuilder>,
        EthereumNetworkBuilder,
        MorphExecutorBuilder,
        MorphConsensusBuilder,
    >;

    type AddOns = MorphAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        // Build payload config from args
        let payload_config =
            MorphBuilderConfig::default().with_max_da_block_size(self.args.max_tx_payload_bytes);

        let payload_config = if let Some(max_tx) = self.args.max_tx_per_block {
            payload_config.with_max_tx_per_block(max_tx)
        } else {
            payload_config
        };

        Self::components(payload_config)
    }

    fn add_ons(&self) -> Self::AddOns {
        MorphAddOns::new()
    }
}

// =============================================================================
// DebugNode Implementation
// =============================================================================

impl<N> DebugNode<N> for MorphNode
where
    N: FullNodeComponents<Types = Self>,
    N::Provider: CanonChainTracker<Header = MorphHeader> + DatabaseProviderFactory,
    <N::Provider as DatabaseProviderFactory>::ProviderRW:
        BlockWriter<Block = Block, Receipt = MorphReceipt> + DBProvider,
{
    type RpcBlock = alloy_rpc_types_eth::Block<MorphTxEnvelope>;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> reth_node_api::BlockTy<Self> {
        // Convert RPC block to consensus block, mapping header to MorphHeader
        let block = rpc_block.into_consensus();
        alloy_consensus::Block {
            header: block.header.into(),
            body: alloy_consensus::BlockBody {
                transactions: block.body.transactions,
                ommers: block.body.ommers.into_iter().map(Into::into).collect(),
                withdrawals: block.body.withdrawals,
            },
        }
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<
        <<Self as NodeTypes>::Payload as PayloadTypes>::PayloadAttributes,
        MorphHeader,
    > {
        MorphPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}

// =============================================================================
// Payload Attributes Builder
// =============================================================================

/// Builder for Morph payload attributes used in debug/local mining mode.
///
/// This creates payload attributes for local block building, primarily used
/// for testing and development purposes.
#[derive(Debug, Clone)]
pub struct MorphPayloadAttributesBuilder {
    chain_spec: Arc<MorphChainSpec>,
}

impl MorphPayloadAttributesBuilder {
    /// Creates a new builder with the given chain specification.
    pub const fn new(chain_spec: Arc<MorphChainSpec>) -> Self {
        Self { chain_spec }
    }
}

impl PayloadAttributesBuilder<MorphPayloadAttributes, MorphHeader>
    for MorphPayloadAttributesBuilder
{
    fn build(&self, parent: &SealedHeader<MorphHeader>) -> MorphPayloadAttributes {
        let timestamp = std::cmp::max(parent.timestamp().saturating_add(1), unix_timestamp_now());

        MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp,
                prev_randao: B256::random(),
                suggested_fee_recipient: Address::random(),
                withdrawals: self
                    .chain_spec
                    .is_shanghai_active_at_timestamp(timestamp)
                    .then(Default::default),
                parent_beacon_block_root: self
                    .chain_spec
                    .is_cancun_active_at_timestamp(timestamp)
                    .then(B256::random),
            },
            // No L1 transactions in local mining mode
            transactions: None,
            gas_limit: None,
            base_fee_per_gas: None,
        }
    }
}

/// Returns the current unix timestamp in seconds.
fn unix_timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use morph_chainspec::MORPH_HOODI;
    use reth_payload_primitives::PayloadAttributesBuilder;

    #[test]
    fn morph_node_default() {
        let node = MorphNode::default();
        assert_eq!(
            node.args.max_tx_payload_bytes,
            super::super::args::MORPH_DEFAULT_MAX_TX_PAYLOAD_BYTES
        );
        assert!(node.args.max_tx_per_block.is_none());
    }

    #[test]
    fn morph_node_new_with_args() {
        let args = super::super::args::MorphArgs {
            max_tx_payload_bytes: 200_000,
            max_tx_per_block: Some(500),
        };
        let node = MorphNode::new(args);
        assert_eq!(node.args.max_tx_payload_bytes, 200_000);
        assert_eq!(node.args.max_tx_per_block, Some(500));
    }

    #[test]
    fn payload_attributes_builder_produces_valid_attributes() {
        let chain_spec = MORPH_HOODI.clone();
        let builder = MorphPayloadAttributesBuilder::new(chain_spec);

        // Create a parent header at a known timestamp
        let parent_header = MorphHeader {
            inner: alloy_consensus::Header {
                timestamp: 1_000_000,
                ..Default::default()
            },
            ..Default::default()
        };
        let parent = reth_primitives_traits::SealedHeader::seal_slow(parent_header);

        let attrs = builder.build(&parent);

        // Timestamp should be at least parent + 1
        assert!(attrs.inner.timestamp > 1_000_000);
        // No L1 transactions in local mining mode
        assert!(attrs.transactions.is_none());
        assert!(attrs.gas_limit.is_none());
        assert!(attrs.base_fee_per_gas.is_none());
    }

    #[test]
    fn payload_attributes_builder_timestamp_uses_wall_clock_when_ahead() {
        let chain_spec = MORPH_HOODI.clone();
        let builder = MorphPayloadAttributesBuilder::new(chain_spec);

        // Use a parent header with timestamp = 0 (very old)
        let parent_header = MorphHeader {
            inner: alloy_consensus::Header {
                timestamp: 0,
                ..Default::default()
            },
            ..Default::default()
        };
        let parent = reth_primitives_traits::SealedHeader::seal_slow(parent_header);

        let attrs = builder.build(&parent);

        // When parent is very old, timestamp should be approximately wall clock time
        let now = unix_timestamp_now();
        assert!(attrs.inner.timestamp >= now.saturating_sub(2));
        assert!(attrs.inner.timestamp <= now.saturating_add(2));
    }

    #[test]
    fn node_types_check() {
        // Verify that MorphNode implements NodeTypes with the correct associated types
        fn assert_node_types<
            N: reth_node_api::NodeTypes<
                    Primitives = morph_primitives::MorphPrimitives,
                    ChainSpec = morph_chainspec::MorphChainSpec,
                    Payload = morph_payload_types::MorphPayloadTypes,
                >,
        >() {
        }
        assert_node_types::<MorphNode>();
    }
}
