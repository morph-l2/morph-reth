//! Multi-node canonical reorganization scenarios.

use alloy_consensus::transaction::TxHashRef;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::B256;
use eyre::WrapErr;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder, wallet_at_index};
use morph_payload_types::{AssembleL2BlockParams, ExecutableL2Data};
use morph_primitives::MorphHeader;
use reth_provider::{AccountReader, BlockReader, StateProviderFactory};

use super::helpers::{
    canonical_snapshot, reference_query, sync_optimistically, wait_for_pool_membership,
    wait_for_reference_query,
};

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
    let client = leader.auth_server_handle().http_client();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();

    let mut block1_params = AssembleL2BlockParams::empty(1);
    block1_params.timestamp = Some(now - 6);
    let block1: ExecutableL2Data = client
        .request("engine_assembleL2Block", (block1_params,))
        .await?;
    let block1_hash = block1.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block1,)).await?;
    sync_optimistically(&follower, block1_hash).await?;

    let reference_a = B256::with_last_byte(0xa1);
    let tx_a = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference_a)
        .build_signed()?;
    let tx_a_hash = leader.rpc.inject_tx(tx_a).await?;
    wait_for_pool_membership(&leader, tx_a_hash, true).await?;

    let mut block2a_params = AssembleL2BlockParams::empty(2);
    block2a_params.timestamp = Some(now - 4);
    let block2a: ExecutableL2Data = client
        .request("engine_assembleL2Block", (block2a_params,))
        .await?;
    assert_eq!(block2a.parent_hash, block1_hash);
    assert!(
        block2a.transactions.iter().any(|raw| {
            let mut raw = raw.as_ref();
            morph_primitives::MorphTxEnvelope::decode_2718(&mut raw)
                .is_ok_and(|tx| *tx.tx_hash() == tx_a_hash)
        }),
        "branch A must contain its referenced MorphTx"
    );
    let block2a_hash = block2a.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block2a,)).await?;
    wait_for_pool_membership(&leader, tx_a_hash, false).await?;
    sync_optimistically(&follower, block2a_hash).await?;

    for (name, node) in [("leader", &leader), ("follower", &follower)] {
        let indexed_a = wait_for_reference_query(node, reference_query(reference_a), |results| {
            results.len() == 1 && results[0].transaction_hash == tx_a_hash
        })
        .await
        .wrap_err_with(|| format!("{name} did not index branch A"))?;
        assert_eq!(indexed_a[0].transaction_hash, tx_a_hash);
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
    wait_for_pool_membership(&leader, tx_b_hash, true).await?;

    let block2b: ExecutableL2Data = client
        .request(
            "engine_assembleL2BlockV2",
            (serde_json::json!({
                "parentHash": block1_hash,
                "timestamp": format!("{:#x}", now - 2),
                "transactions": [],
            }),),
        )
        .await?;
    assert_eq!(block2b.parent_hash, block1_hash);
    assert_ne!(block2b.hash, block2a_hash);
    assert!(
        block2b.transactions.iter().any(|raw| {
            let mut raw = raw.as_ref();
            morph_primitives::MorphTxEnvelope::decode_2718(&mut raw)
                .is_ok_and(|tx| *tx.tx_hash() == tx_b_hash)
        }),
        "branch B must contain its referenced MorphTx"
    );
    let block2b_hash = block2b.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block2b,)).await?;

    wait_for_pool_membership(&leader, tx_a_hash, true).await?;
    wait_for_pool_membership(&leader, tx_b_hash, false).await?;
    sync_optimistically(&follower, block2b_hash).await?;
    wait_for_pool_membership(&follower, tx_a_hash, true).await?;

    for (name, node) in [("leader", &leader), ("follower", &follower)] {
        let indexed_b = wait_for_reference_query(node, reference_query(reference_b), |results| {
            results.len() == 1 && results[0].transaction_hash == tx_b_hash
        })
        .await
        .wrap_err_with(|| format!("{name} did not index branch B"))?;
        assert_eq!(indexed_b[0].transaction_hash, tx_b_hash);

        let indexed_a = wait_for_reference_query(node, reference_query(reference_a), |results| {
            results.is_empty()
        })
        .await
        .wrap_err_with(|| format!("{name} retained branch A"))?;
        assert!(
            indexed_a.is_empty(),
            "orphaned branch A must not leave a reference-index entry"
        );
    }

    let leader_head = canonical_snapshot(&leader)?;
    let follower_head = canonical_snapshot(&follower)?;
    assert_eq!(leader_head.hash, block2b_hash);
    assert_eq!(follower_head.hash, block2b_hash);
    assert_eq!(leader_head.number, follower_head.number);
    assert_eq!(leader_head.state_root, follower_head.state_root);
    assert_eq!(
        leader_head.next_l1_message_index,
        follower_head.next_l1_message_index
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
        let canonical_block = node
            .inner
            .provider
            .block_by_number(2)?
            .expect("canonical block 2");
        let hashes: Vec<_> = canonical_block
            .body
            .transactions
            .iter()
            .map(|tx| *tx.tx_hash())
            .collect();
        assert!(hashes.contains(&tx_b_hash));
        assert!(!hashes.contains(&tx_a_hash));
    }

    Ok(())
}
