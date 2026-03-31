//! Hardfork boundary integration tests.
//!
//! Verifies that morph-reth behaves correctly across different hardfork
//! activation schedules. These tests parametrize `HardforkSchedule` to ensure
//! block building works under both "all active" and "pre-Jade" configurations.

use morph_node::test_utils::{
    HardforkSchedule, TestNodeBuilder, advance_chain, advance_empty_block,
};
use reth_payload_primitives::BuiltPayload;

use super::helpers::wallet_to_arc;

/// With all Morph hardforks active (including Jade), blocks are built
/// and the chain advances successfully.
#[tokio::test(flavor = "multi_thread")]
async fn all_active_chain_advances() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::AllActive)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(5, &mut node, wallet).await?;
    assert_eq!(payloads.len(), 5);

    for (i, payload) in payloads.iter().enumerate() {
        let block = payload.block();
        assert_eq!(block.header().inner.number, (i + 1) as u64);
        assert!(!block.body().transactions.is_empty());
    }

    Ok(())
}

/// With Jade disabled (pre-Jade schedule), blocks are still built correctly.
///
/// This tests the pre-Jade behavior:
/// - State root validation is skipped (ZK-trie not implemented)
/// - All other hardforks are active
#[tokio::test(flavor = "multi_thread")]
async fn pre_jade_chain_advances() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(5, &mut node, wallet).await?;
    assert_eq!(payloads.len(), 5);

    for (i, payload) in payloads.iter().enumerate() {
        let block = payload.block();
        assert_eq!(block.header().inner.number, (i + 1) as u64);
    }

    Ok(())
}

/// Verify that an empty block can be produced under pre-Jade schedule.
#[tokio::test(flavor = "multi_thread")]
async fn pre_jade_empty_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let payload = advance_empty_block(&mut node).await?;
    let block = payload.block();

    assert_eq!(block.header().inner.number, 1);
    assert_eq!(block.body().transactions.len(), 0);

    Ok(())
}

/// EIP-7702 transaction is accepted when Viridian hardfork is active.
#[tokio::test(flavor = "multi_thread")]
async fn eip7702_accepted_viridian_active() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::AllActive) // Viridian active
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = morph_node::test_utils::make_eip7702_tx(wallet.chain_id, wallet.inner.clone(), 0)?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(
        payload.block().body().transactions.len(),
        1,
        "EIP-7702 should be accepted"
    );
    Ok(())
}

/// EIP-7702 transaction is rejected when Viridian hardfork is NOT active.
#[tokio::test(flavor = "multi_thread")]
async fn eip7702_rejected_viridian_inactive() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreViridian) // Viridian NOT active
        .build()
        .await?;
    let node = nodes.pop().unwrap();

    let raw_tx = morph_node::test_utils::make_eip7702_tx(wallet.chain_id, wallet.inner.clone(), 0)?;
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(result.is_err(), "EIP-7702 must be rejected before Viridian");
    Ok(())
}
