//! L1 message handling integration tests.
//!
//! Verifies that L1 message transactions (type 0x7E) follow Morph's protocol rules:
//! - Must appear at the start of the block before any L2 transactions
//! - Must have strictly sequential queue indices within a block
//! - Queue index must continue monotonically across blocks
//! - Gas is prepaid on L1; L2 block gas accounting reflects this

use alloy_primitives::{Address, U256};
use morph_node::test_utils::{L1MessageBuilder, TestNodeBuilder, advance_empty_block};
use reth_payload_primitives::BuiltPayload;

use super::helpers::advance_block_with_l1_messages;

/// A single L1 message is included at the start of the block.
#[tokio::test(flavor = "multi_thread")]
async fn single_l1_message_included() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let l1_msg = L1MessageBuilder::new(0)
        .with_target(Address::with_last_byte(0x01))
        .with_value(U256::ZERO)
        .with_gas_limit(21_000)
        .build_encoded();

    let payload = advance_block_with_l1_messages(&mut node, vec![l1_msg]).await?;
    let block = payload.block();

    assert_eq!(block.body().transactions.len(), 1);

    let tx = block.body().transactions.first().unwrap();
    assert!(tx.is_l1_msg(), "only transaction must be an L1 message");
    assert_eq!(tx.queue_index(), Some(0));

    Ok(())
}

/// Three L1 messages with queue indices 0, 1, 2 are all included in one block.
#[tokio::test(flavor = "multi_thread")]
async fn three_sequential_l1_messages_in_one_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let l1_msgs = L1MessageBuilder::build_sequential(0, 3);

    let payload = advance_block_with_l1_messages(&mut node, l1_msgs).await?;
    let block = payload.block();

    assert_eq!(block.body().transactions.len(), 3);

    for (expected_qi, tx) in block.body().transactions.iter().enumerate() {
        assert!(tx.is_l1_msg(), "tx {expected_qi} should be L1 message");
        assert_eq!(
            tx.queue_index(),
            Some(expected_qi as u64),
            "queue_index should be {expected_qi}"
        );
    }

    Ok(())
}

/// L1 messages across multiple blocks must have strictly continuous queue indices.
///
/// Block 1: queue indices 0, 1 → next expected is 2
/// Block 2: queue indices 2, 3 → continues from where block 1 left off
///
/// This verifies that `next_l1_msg_index` from parent header is correctly
/// used to enforce cross-block continuity.
#[tokio::test(flavor = "multi_thread")]
async fn l1_messages_across_blocks_continuous() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Block 1: queue indices 0, 1
    let block1_msgs = L1MessageBuilder::build_sequential(0, 2);
    let payload1 = advance_block_with_l1_messages(&mut node, block1_msgs).await?;
    assert_eq!(payload1.block().body().transactions.len(), 2);

    // Block 2: queue indices 2, 3 (continues from where block 1 left off)
    let block2_msgs = L1MessageBuilder::build_sequential(2, 2);
    let payload2 = advance_block_with_l1_messages(&mut node, block2_msgs).await?;
    assert_eq!(payload2.block().body().transactions.len(), 2);

    let block2_txs = payload2.block().body().transactions.as_slice();
    assert_eq!(block2_txs[0].queue_index(), Some(2));
    assert_eq!(block2_txs[1].queue_index(), Some(3));

    Ok(())
}

/// When a block has no L1 messages, queue index tracking is unchanged.
/// L1 messages in a later block can continue from any higher index.
#[tokio::test(flavor = "multi_thread")]
async fn l1_messages_resume_after_empty_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Block 1: queue indices 0, 1
    let first_msgs = L1MessageBuilder::build_sequential(0, 2);
    let payload1 = advance_block_with_l1_messages(&mut node, first_msgs).await?;
    assert_eq!(payload1.block().body().transactions.len(), 2);

    // Block 2: no L1 messages (truly empty block, pool is also empty)
    let payload2 = advance_empty_block(&mut node).await?;
    assert_eq!(payload2.block().body().transactions.len(), 0);

    // Block 3: queue index continues from 2
    let third_msgs = L1MessageBuilder::build_sequential(2, 2);
    let payload3 = advance_block_with_l1_messages(&mut node, third_msgs).await?;

    let txs = payload3.block().body().transactions.as_slice();
    assert_eq!(txs[0].queue_index(), Some(2));
    assert_eq!(txs[1].queue_index(), Some(3));

    Ok(())
}

/// L1 message gas is tracked in gasUsed (prepaid on L1).
///
/// The block's `gasUsed` increases to reflect the execution cost of L1 messages,
/// but no L2 account is charged. The actual cost must not exceed the `gas_limit`
/// specified in the L1 message.
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_gas_is_tracked() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let gas_limit = 50_000u64;
    let l1_msg = L1MessageBuilder::new(0)
        .with_target(Address::with_last_byte(0x42))
        .with_gas_limit(gas_limit)
        .build_encoded();

    let payload = advance_block_with_l1_messages(&mut node, vec![l1_msg]).await?;
    let block = payload.block();

    // gasUsed should be > 0 (execution cost is tracked)
    assert!(
        block.header().inner.gas_used > 0,
        "L1 message gas usage must be tracked"
    );
    // Must not exceed the message's own gas limit
    assert!(
        block.header().inner.gas_used <= gas_limit,
        "gas used {} must not exceed L1 message gas limit {}",
        block.header().inner.gas_used,
        gas_limit
    );

    Ok(())
}
