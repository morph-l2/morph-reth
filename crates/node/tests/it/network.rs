//! P2P network E2E tests.

use morph_node::test_utils::{TestNodeBuilder, make_transfer_tx, wallet_at_index};
use reth_network::NetworkInfo;
use reth_provider::BlockNumReader;
use reth_transaction_pool::TransactionPool;

use super::helpers::{NETWORK_POLL_BUDGET, POLL_INTERVAL, assemble_l2_block, import_l2_block};
use morph_payload_types::AssembleL2BlockParams;

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

/// A node starting pipeline backfill can still receive a peer's pending pool
/// announcement while its chain is catching up.
#[tokio::test(flavor = "multi_thread")]
async fn peer_pool_reaches_node_during_initial_backfill() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut sources, wallet) = TestNodeBuilder::new().build().await?;
    let source = sources.pop().unwrap();
    let mut tip = None;
    for number in 1..=5 {
        let mut params = AssembleL2BlockParams::empty(number);
        params.timestamp = Some(number);
        let block = assemble_l2_block(&source, params).await?;
        import_l2_block(&source, block.clone()).await?;
        tip = Some(block.hash);
    }
    let tip = tip.expect("five blocks were imported");

    let tx = make_transfer_tx(wallet.chain_id, wallet_at_index(1, wallet.chain_id), 0).await;
    let tx_hash = source.rpc.inject_tx(tx).await?;

    // Launch must be able to dial the source during backfill: the upstream
    // launcher can wait for that initial backfill before returning the handle.
    let enode = source.network.record().to_string();
    let (mut followers, _) = tokio::time::timeout(
        NETWORK_POLL_BUDGET,
        TestNodeBuilder::new()
            .with_debug_tip(tip)
            .with_trusted_peer(enode)
            .build(),
    )
    .await??;
    let follower = followers.pop().unwrap();
    let deadline = tokio::time::Instant::now() + NETWORK_POLL_BUDGET;
    while !follower.inner.pool.contains(&tx_hash)
        || follower.inner.provider.best_block_number()? < 5
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "backfill did not finish with the peer's pending transaction in the pool"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    Ok(())
}
