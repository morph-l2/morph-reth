//! P2P network E2E tests.

use morph_node::test_utils::{TestNodeBuilder, make_transfer_tx, wallet_at_index};
use reth_network::NetworkInfo;
use reth_provider::BlockNumReader;
use reth_transaction_pool::TransactionPool;

use super::helpers::{NETWORK_POLL_BUDGET, POLL_INTERVAL};

/// A transaction a peer already holds when the session opens reaches a node that has not
/// imported a block yet.
///
/// Peers announce their pool only once, when the session opens. If the node dropped that
/// announcement until its first block, a sequencer restarting while RPC nodes hold pending
/// transactions would never receive them.
#[tokio::test(flavor = "multi_thread")]
async fn peer_pool_reaches_node_before_first_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Built separately so they are not connected until the transaction is pending.
    let (mut sequencers, wallet) = TestNodeBuilder::new().build().await?;
    let (mut rpcs, _) = TestNodeBuilder::new().build().await?;
    let mut sequencer = sequencers.pop().unwrap();
    let mut rpc = rpcs.pop().unwrap();

    let tx = make_transfer_tx(wallet.chain_id, wallet_at_index(1, wallet.chain_id), 0).await;
    let tx_hash = rpc.rpc.inject_tx(tx).await?;

    assert!(
        sequencer.inner.network.is_syncing(),
        "the node must still be in its start-up sync state"
    );
    sequencer.connect(&mut rpc).await;

    let deadline = tokio::time::Instant::now() + NETWORK_POLL_BUDGET;
    while !sequencer.inner.pool.contains(&tx_hash) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the peer's pending transaction never reached the node"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    assert_eq!(sequencer.inner.provider.best_block_number()?, 0);

    Ok(())
}
