//! Multi-node canonical reorganization scenarios.

use alloy_consensus::transaction::TxHashRef;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;
use eyre::WrapErr;
use morph_node::test_utils::{MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder, wallet_at_index};
use morph_payload_types::{AssembleL2BlockParams, AssembleL2BlockV2Params, ExecutableL2Data};
use reth_provider::{AccountReader, StateProviderFactory};

use super::helpers::{
    assemble_l2_block, assemble_l2_block_v2, canonical_block, canonical_snapshot, head_timestamp,
    import_l2_block, reference_query, sync_optimistically, transaction_hashes,
    wait_for_reference_query, wait_until_evicted, wait_until_pooled,
};

/// Whether an assembled block carries the given transaction.
fn includes_transaction(data: &ExecutableL2Data, tx_hash: B256) -> bool {
    data.transactions.iter().any(|raw| {
        let mut raw = raw.as_ref();
        morph_primitives::MorphTxEnvelope::decode_2718(&mut raw)
            .is_ok_and(|tx| *tx.tx_hash() == tx_hash)
    })
}

/// Behavior contract:
/// - fault: canonical block 2A contains a referenced MorphTx and is replaced
///   by sibling 2B containing a different referenced MorphTx;
/// - evidence: both nodes converge on 2B and equivalent account state, 2A's
///   transaction returns to both pools, and both reference indexes replace A
///   with B after processing the canonical notification.
#[tokio::test(flavor = "multi_thread")]
async fn sibling_reorg_converges_state_pool_and_reference_index() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().with_num_nodes(2).build().await?;
    let follower = nodes.pop().expect("two nodes requested");
    let leader = nodes.pop().expect("two nodes requested");

    // Both sibling blocks descend from block 1; their timestamps and transaction
    // sets distinguish the two branches.
    let genesis_timestamp = head_timestamp(&leader)?;
    let block1_timestamp = genesis_timestamp + 1;

    let mut block1_params = AssembleL2BlockParams::empty(1);
    block1_params.timestamp = Some(block1_timestamp);
    let block1 = assemble_l2_block(&leader, block1_params).await?;
    let block1_hash = block1.hash;
    import_l2_block(&leader, block1).await?;
    sync_optimistically(&follower, block1_hash).await?;

    let reference_a = B256::with_last_byte(0xa1);
    let tx_a = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference_a)
        .build_signed()?;
    let tx_a_hash = leader.rpc.inject_tx(tx_a).await?;

    let mut block2a_params = AssembleL2BlockParams::empty(2);
    block2a_params.timestamp = Some(block1_timestamp + 1);
    let block2a = assemble_l2_block(&leader, block2a_params).await?;
    assert_eq!(block2a.parent_hash, block1_hash);
    assert!(
        includes_transaction(&block2a, tx_a_hash),
        "branch A must contain its referenced MorphTx"
    );
    let block2a_hash = block2a.hash;
    import_l2_block(&leader, block2a).await?;
    wait_until_evicted(&leader, tx_a_hash).await?;
    sync_optimistically(&follower, block2a_hash).await?;
    // Baseline for the post-reorg re-admission assertion below: without this, a
    // gossiped-and-never-removed transaction would satisfy it.
    wait_until_evicted(&follower, tx_a_hash).await?;

    for (name, node) in [("leader", &leader), ("follower", &follower)] {
        wait_for_reference_query(node, reference_query(reference_a), |results| {
            results.len() == 1 && results[0].transaction_hash == tx_a_hash
        })
        .await
        .wrap_err_with(|| format!("{name} did not index branch A"))?;
    }

    let reference_b = B256::with_last_byte(0xb2);
    let signer_b = wallet_at_index(1, wallet.chain_id);
    let sender_a = wallet.inner.address();
    let sender_b = signer_b.address();
    let tx_b = MorphTxBuilder::new(wallet.chain_id, signer_b, 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference_b)
        .build_signed()?;
    let tx_b_hash = leader.rpc.inject_tx(tx_b).await?;
    // Establish that B reached the follower's pool before it becomes canonical;
    // otherwise a later absence would not prove reorg-driven eviction.
    wait_until_pooled(&follower, tx_b_hash).await?;

    let mut block2b_params = AssembleL2BlockV2Params::empty(block1_hash);
    block2b_params.timestamp = Some(block1_timestamp + 2);
    let block2b = assemble_l2_block_v2(&leader, block2b_params).await?;
    assert_eq!(block2b.parent_hash, block1_hash);
    assert_ne!(block2b.hash, block2a_hash);
    assert!(
        includes_transaction(&block2b, tx_b_hash),
        "branch B must contain its referenced MorphTx"
    );
    let block2b_hash = block2b.hash;
    import_l2_block(&leader, block2b).await?;

    wait_until_pooled(&leader, tx_a_hash).await?;
    wait_until_evicted(&leader, tx_b_hash).await?;
    sync_optimistically(&follower, block2b_hash).await?;
    wait_until_pooled(&follower, tx_a_hash).await?;
    wait_until_evicted(&follower, tx_b_hash).await?;

    for (name, node) in [("leader", &leader), ("follower", &follower)] {
        wait_for_reference_query(node, reference_query(reference_b), |results| {
            results.len() == 1 && results[0].transaction_hash == tx_b_hash
        })
        .await
        .wrap_err_with(|| format!("{name} did not index branch B"))?;

        // Ordering matters: branch B is indexed in the same turn that rolls back
        // branch A, so an empty result here cannot be "not indexed yet".
        wait_for_reference_query(node, reference_query(reference_a), <[_]>::is_empty)
            .await
            .wrap_err_with(|| format!("{name} retained branch A"))?;
    }

    let leader_head = canonical_snapshot(&leader)?;
    assert_eq!(leader_head.hash, block2b_hash);
    assert_eq!(
        leader_head,
        canonical_snapshot(&follower)?,
        "both nodes must expose the same head, state root, queue index, and safe tag"
    );

    let leader_state = leader.inner.provider.latest()?;
    let follower_state = follower.inner.provider.latest()?;
    assert_eq!(
        leader_state.basic_account(&sender_a)?,
        follower_state.basic_account(&sender_a)?,
        "reverted sender state must converge"
    );
    assert_eq!(
        leader_state.basic_account(&sender_b)?,
        follower_state.basic_account(&sender_b)?,
        "replacement-branch sender state must converge"
    );
    assert_eq!(
        leader_state
            .basic_account(&sender_a)?
            .expect("funded sender A")
            .nonce,
        0,
        "branch A nonce change must be reverted"
    );
    assert_eq!(
        leader_state
            .basic_account(&sender_b)?
            .expect("funded sender B")
            .nonce,
        1,
        "branch B nonce change must be canonical"
    );

    for node in [&leader, &follower] {
        let hashes = transaction_hashes(&canonical_block(node, 2)?);
        assert!(hashes.contains(&tx_b_hash));
        assert!(!hashes.contains(&tx_a_hash));
    }

    Ok(())
}
