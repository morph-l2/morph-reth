//! Consensus rule enforcement integration tests.
//!
//! Verifies that the Morph node correctly rejects blocks that violate protocol
//! consensus rules:
//! - L1 messages must precede all L2 transactions (ordering constraint)
//! - L1 messages within a block must have strictly sequential queue indices
//! - Post-Jade blocks with a wrong state root are rejected

use alloy_primitives::B256;
use morph_node::test_utils::{
    HardforkSchedule, L1MessageBuilder, TestNodeBuilder, make_transfer_tx,
};
use reth_payload_primitives::BuiltPayload;

use super::helpers::{
    advance_block_with_l1_messages, build_block_no_submit, craft_and_try_import_block,
    expect_payload_build_failure,
};

/// A block where an L2 transaction appears before an L1 message is rejected.
///
/// Morph protocol requires that all L1 messages occupy the leading positions in
/// a block. A block with an L2 tx followed by an L1 msg violates this rule.
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_after_l2_tx_is_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Inject an L2 transfer into the pool so that the payload builder picks it up.
    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    node.rpc.inject_tx(raw_tx).await?;

    // Build a valid block: 1 L1 message + 1 L2 tx from pool (correct order).
    let l1_msg = L1MessageBuilder::new(0).build_encoded();
    let base_payload = build_block_no_submit(&mut node, vec![l1_msg]).await?;

    // The valid block must have 2 transactions with L1 message first.
    assert_eq!(base_payload.block().body().transactions.len(), 2);
    assert!(base_payload.block().body().transactions[0].is_l1_msg());

    // Swap the order so the L2 tx appears first and the L1 message comes second.
    let accepted = craft_and_try_import_block(&mut node, &base_payload, |block| {
        block.body.transactions.swap(0, 1);
    })
    .await?;

    assert!(
        !accepted,
        "block with L2 tx before L1 message must be rejected by consensus"
    );

    Ok(())
}

/// Two L1 messages with the same queue index in one block are rejected at build time.
///
/// Queue indices within a block must be strictly increasing. Duplicate indices
/// would create an ambiguous ordering and break the cross-block monotonicity
/// invariant tracked in the parent header's `next_l1_msg_index` field.
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_duplicate_queue_index_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Both messages claim queue index 0 — this is a protocol violation.
    let msg_a = L1MessageBuilder::new(0).build_encoded();
    let msg_b = L1MessageBuilder::new(0).build_encoded();

    let error = expect_payload_build_failure(&mut node, vec![msg_a, msg_b]).await?;

    assert!(
        error.to_lowercase().contains("queue index"),
        "error message should mention queue index, got: {error}"
    );

    Ok(())
}

/// L1 messages with a gap in queue indices are rejected at build time.
///
/// Queue indices must be contiguous. A gap (e.g. 0 then 2, skipping 1) means
/// a message was dropped, which is not allowed by the L2MessageQueue contract.
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_gap_queue_index_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Index 0 then index 2 — index 1 is skipped.
    let msg_0 = L1MessageBuilder::new(0).build_encoded();
    let msg_2 = L1MessageBuilder::new(2).build_encoded();

    let error = expect_payload_build_failure(&mut node, vec![msg_0, msg_2]).await?;

    assert!(
        error.to_lowercase().contains("queue index"),
        "error message should mention queue index, got: {error}"
    );

    Ok(())
}

/// A post-Jade block with a tampered state root is rejected.
///
/// After the Jade hardfork, morph-reth uses a standard MPT state root and
/// validates it on import. Any mismatch must cause the block to be INVALID.
#[tokio::test(flavor = "multi_thread")]
async fn post_jade_state_root_mismatch_is_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::AllActive)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    // Build a valid block without submitting it.
    let base_payload = build_block_no_submit(&mut node, vec![]).await?;

    // Replace the state root with a bogus value and try to import.
    let accepted = craft_and_try_import_block(&mut node, &base_payload, |block| {
        block.header.inner.state_root = B256::from([0xFF; 32]);
    })
    .await?;

    assert!(
        !accepted,
        "post-Jade block with wrong state root must be rejected"
    );

    Ok(())
}

/// A block whose number jumps ahead (parent is genesis at 0, block claims number 2) is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn block_number_jump_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let base = build_block_no_submit(&mut node, vec![]).await?;
    // Base block has number=1. Change to 2 -> gap
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.inner.number = 2;
    })
    .await?;
    assert!(!accepted, "block number jump (0->2) should be rejected");
    Ok(())
}

/// A block pointing to a non-existent parent hash is not accepted as valid.
#[tokio::test(flavor = "multi_thread")]
async fn wrong_parent_hash_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let base = build_block_no_submit(&mut node, vec![]).await?;
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.inner.parent_hash = alloy_primitives::B256::from([0xFF; 32]);
    })
    .await?;
    assert!(
        !accepted,
        "block with unknown parent hash should not be accepted"
    );
    Ok(())
}

/// A block whose timestamp equals the parent's timestamp is rejected (pre-Emerald).
/// Under Emerald+, `timestamp == parent.timestamp` is legal, so we use PreViridian
/// schedule which is pre-Emerald.
#[tokio::test(flavor = "multi_thread")]
async fn timestamp_not_greater_than_parent_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new()
        .with_schedule(morph_node::test_utils::HardforkSchedule::PreViridian)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let base = build_block_no_submit(&mut node, vec![]).await?;
    // Set timestamp to 0 (same as genesis parent timestamp)
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.inner.timestamp = 0;
    })
    .await?;
    assert!(
        !accepted,
        "timestamp <= parent.timestamp should be rejected"
    );
    Ok(())
}

/// A block claiming gasUsed > gasLimit is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn gas_used_exceeds_gas_limit_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let base = build_block_no_submit(&mut node, vec![]).await?;
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.inner.gas_used = block.header.inner.gas_limit + 1;
    })
    .await?;
    assert!(!accepted, "gasUsed > gasLimit should be rejected");
    Ok(())
}

/// A block with gas limit more than 1/1024 higher than parent is rejected.
#[tokio::test(flavor = "multi_thread")]
async fn gas_limit_excessive_increase_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let base = build_block_no_submit(&mut node, vec![]).await?;
    // Double the gas limit -- far exceeds 1/1024 change
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.inner.gas_limit *= 2;
    })
    .await?;
    assert!(!accepted, "gas limit excessive increase should be rejected");
    Ok(())
}

/// A block whose next_l1_msg_index is less than parent's value is rejected.
///
/// First advance one block with L1 messages (sets next_l1_msg_index = 2),
/// then try to import a block with next_l1_msg_index = 0.
#[tokio::test(flavor = "multi_thread")]
async fn next_l1_msg_index_decreases_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Block 1: include 2 L1 messages -> next_l1_msg_index becomes 2
    let l1_msgs = L1MessageBuilder::build_sequential(0, 2);
    advance_block_with_l1_messages(&mut node, l1_msgs).await?;

    // Build block 2 (no L1 msgs), modify next_l1_msg_index to 0 (< parent's 2)
    let base2 = build_block_no_submit(&mut node, vec![]).await?;
    let accepted = craft_and_try_import_block(&mut node, &base2, |block| {
        block.header.next_l1_msg_index = 0;
    })
    .await?;
    assert!(!accepted, "next_l1_msg_index < parent should be rejected");
    Ok(())
}

/// A block with L1 messages but next_l1_msg_index too low is rejected.
///
/// Block has L1 messages with queue indices 0, 1 -> next_l1_msg_index should be >= 2.
/// We set it to 1 (insufficient) -> should be rejected.
#[tokio::test(flavor = "multi_thread")]
async fn next_l1_msg_index_insufficient_for_l1_msgs() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Build block with 2 L1 messages (queue 0,1) but don't submit
    let l1_msgs = L1MessageBuilder::build_sequential(0, 2);
    let base = build_block_no_submit(&mut node, l1_msgs).await?;

    // Modify next_l1_msg_index to 1 (should be >= 2)
    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.next_l1_msg_index = 1;
    })
    .await?;
    assert!(!accepted, "next_l1_msg_index < required should be rejected");
    Ok(())
}

/// A block may advance `next_l1_msg_index` past the included messages to account for skips.
#[tokio::test(flavor = "multi_thread")]
async fn next_l1_msg_index_can_skip_past_included_messages() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Build block with queue indices 0,1 and then advance header.next_l1_msg_index to 4.
    // This models the sequencer skipping queue indices 2 and 3 while still including 0 and 1.
    let l1_msgs = L1MessageBuilder::build_sequential(0, 2);
    let base = build_block_no_submit(&mut node, l1_msgs).await?;

    let accepted = craft_and_try_import_block(&mut node, &base, |block| {
        block.header.next_l1_msg_index = 4;
    })
    .await?;

    assert!(
        accepted,
        "next_l1_msg_index may advance past included L1 messages to represent skipped queue indices"
    );
    Ok(())
}
