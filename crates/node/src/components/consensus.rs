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
pub struct MorphConsensusBuilder {
    disable_tx_payload_size_limit: bool,
}

impl MorphConsensusBuilder {
    /// Disables the DA-derived payload-size check for synthetic execution benchmarks.
    pub const fn without_tx_payload_size_limit(mut self) -> Self {
        self.disable_tx_payload_size_limit = true;
        self
    }
}

impl<Node> ConsensusBuilder<Node> for MorphConsensusBuilder
where
    Node: FullNodeTypes<Types = MorphNode>,
{
    type Consensus = MorphConsensus;

    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let consensus = MorphConsensus::new(ctx.chain_spec());
        if self.disable_tx_payload_size_limit {
            Ok(consensus.without_tx_payload_size_limit())
        } else {
            Ok(consensus)
        }
    }
}
