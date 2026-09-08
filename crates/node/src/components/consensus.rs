//! Morph consensus builder.

use crate::MorphNode;
use morph_consensus::MorphConsensus;
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::ConsensusBuilder};

/// Builder for [`MorphConsensus`].
///
/// Creates the consensus engine with Morph-specific validation rules.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for MorphConsensusBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
{
    type Consensus = MorphConsensus;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        Ok(MorphConsensus::new(ctx.chain_spec()))
    }
}
