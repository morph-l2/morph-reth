//! Block building integration tests.
//!
//! Verifies that the Morph payload builder correctly assembles blocks under
//! various conditions: empty blocks, pool transactions, and mixed L1+L2 ordering.

use alloy_primitives::{Address, U256};
use morph_node::test_utils::{
    L1MessageBuilder, TestNodeBuilder, advance_chain, advance_empty_block,
};
use reth_payload_primitives::BuiltPayload;

use super::helpers::{advance_block_with_l1_messages, wallet_to_arc};

/// An empty block (no pool transactions, no L1 messages) should be built
/// successfully with 0 transactions and valid header fields.
#[tokio::test(flavor = "multi_thread")]
async fn empty_block_has_no_transactions() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let payload = advance_empty_block(&mut node).await?;
    let block = payload.block();

    assert_eq!(
        block.body().transactions.len(),
        0,
        "empty block should have no transactions"
    );
    assert_eq!(block.header().inner.number, 1, "block number should be 1");

    Ok(())
}

/// A block containing a single EIP-1559 transfer transaction.
#[tokio::test(flavor = "multi_thread")]
async fn block_with_single_transfer() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(1, &mut node, wallet).await?;

    let block = payloads[0].block();
    assert_eq!(block.header().inner.number, 1);
    assert_eq!(
        block.body().transactions.len(),
        1,
        "block should contain the transfer tx"
    );

    Ok(())
}

/// Advance 10 blocks with sequential transfers; verify block numbers are monotonic.
#[tokio::test(flavor = "multi_thread")]
async fn sequential_blocks_with_transfers() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(10, &mut node, wallet).await?;

    assert_eq!(payloads.len(), 10);
    for (i, payload) in payloads.iter().enumerate() {
        let block = payload.block();
        assert_eq!(block.header().inner.number, (i + 1) as u64);
        assert_eq!(block.body().transactions.len(), 1);
    }

    Ok(())
}

/// A block with a single L1 message at the start and no L2 transactions.
#[tokio::test(flavor = "multi_thread")]
async fn block_with_l1_message_only() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let l1_msg = L1MessageBuilder::new(0)
        .with_target(Address::with_last_byte(0xAA))
        .with_value(U256::from(0))
        .with_gas_limit(50_000)
        .build_encoded();

    let payload = advance_block_with_l1_messages(&mut node, vec![l1_msg]).await?;
    let block = payload.block();

    assert_eq!(block.header().inner.number, 1);
    assert_eq!(
        block.body().transactions.len(),
        1,
        "block should contain the L1 message"
    );

    Ok(())
}

/// A block with L1 messages preceding L2 pool transactions.
/// L1 messages must always appear first in the block.
#[tokio::test(flavor = "multi_thread")]
async fn l1_messages_precede_l2_transactions() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Inject L2 transaction into the pool first
    let wallet_arc = wallet_to_arc(wallet);
    let raw_tx = {
        let mut w = wallet_arc.lock().await;
        let nonce = w.inner_nonce;
        w.inner_nonce += 1;
        morph_node::test_utils::make_transfer_tx(w.chain_id, w.inner.clone(), nonce).await
    };
    node.rpc.inject_tx(raw_tx).await?;

    // Build a block with an L1 message — L2 tx from pool should follow
    let l1_msg = L1MessageBuilder::new(0)
        .with_target(Address::with_last_byte(0xBB))
        .with_gas_limit(50_000)
        .build_encoded();

    let payload = advance_block_with_l1_messages(&mut node, vec![l1_msg]).await?;
    let block = payload.block();

    // Should have 2 transactions: 1 L1 message + 1 L2 transfer
    assert_eq!(
        block.body().transactions.len(),
        2,
        "block should have 1 L1 message + 1 L2 tx"
    );

    // First transaction must be the L1 message (type 0x7E)
    let first_tx = block.body().transactions.first().unwrap();
    assert!(
        first_tx.is_l1_msg(),
        "first transaction in block must be an L1 message"
    );

    Ok(())
}

/// Multiple L1 messages with strictly sequential queue indices in one block.
#[tokio::test(flavor = "multi_thread")]
async fn multiple_l1_messages_sequential_queue_indices() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let l1_msgs = L1MessageBuilder::build_sequential(0, 3);

    let payload = advance_block_with_l1_messages(&mut node, l1_msgs).await?;
    let block = payload.block();

    assert_eq!(block.body().transactions.len(), 3);

    for (expected_index, tx) in block.body().transactions.iter().enumerate() {
        assert!(tx.is_l1_msg());
        assert_eq!(
            tx.queue_index(),
            Some(expected_index as u64),
            "queue_index should be sequential"
        );
    }

    Ok(())
}
