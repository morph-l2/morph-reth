//! Morph network builder.

use reth_network::{NetworkSyncUpdater, SyncState};
use reth_node_api::FullNodeTypes;
use reth_node_builder::{BuilderContext, components::NetworkBuilder};
use reth_node_ethereum::EthereumNetworkBuilder;
use reth_transaction_pool::TransactionPool;

/// Builder for the P2P network.
///
/// Builds the standard reth network and accepts transaction gossip from start-up instead of
/// from the first block the consensus client imports.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphNetworkBuilder;

impl<Node, Pool> NetworkBuilder<Node, Pool> for MorphNetworkBuilder
where
    Node: FullNodeTypes,
    Pool: TransactionPool,
    EthereumNetworkBuilder: NetworkBuilder<Node, Pool>,
{
    type Network = <EthereumNetworkBuilder as NetworkBuilder<Node, Pool>>::Network;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network = EthereumNetworkBuilder::default()
            .build_network(ctx, pool)
            .await?;

        // reth ignores peer transactions while the network is initially syncing: from the
        // `Syncing` state the launcher sets on every start until the first switch to `Idle`,
        // which otherwise waits for the first block the consensus client imports. That window
        // drops the pool each peer announces only once, when its session opens, so a restarted
        // sequencer would never see what RPC nodes held while it was down. Blocks arrive through
        // the engine API only, so there is no p2p sync to wait for: switching once here, before
        // any session can open, marks the initial sync done for the life of the process.
        network.update_sync_state(SyncState::Syncing);
        network.update_sync_state(SyncState::Idle);

        Ok(network)
    }
}
