//! Invalid Engine payload recovery scenarios.

use alloy_consensus::{BlockHeader, Sealable, transaction::TxHashRef};
use alloy_primitives::B256;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{TestNodeBuilder, make_transfer_tx};
use morph_payload_types::{AssembleL2BlockParams, AssembleL2BlockV2Params, GenericResponse};
use morph_primitives::MorphHeader;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{AccountReader, BlockIdReader, StateProviderFactory};

use super::helpers::{
    LOCAL_POLL_BUDGET, POLL_INTERVAL, assemble_l2_block, assemble_l2_block_v2,
    build_block_no_submit, canonical_snapshot, head_timestamp, import_l2_block,
    payload_with_receipts_root, wait_until_evicted, wait_until_pooled,
};

/// Behavior contract:
/// - fault: the payload commits to a receipts root that execution cannot produce;
/// - evidence: import rejects it for that specific reason rather than on a cheaper
///   pre-execution check, validation agrees, canonical/account state stay unchanged,
///   and the unmodified payload can still be imported afterwards.
#[tokio::test(flavor = "multi_thread")]
async fn invalid_receipts_root_preserves_canonical_state_then_recovers() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().expect("one node requested");
    let sender = wallet.inner.address();
    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce).await;
    node.rpc.inject_tx(raw_tx).await?;

    let base_payload = build_block_no_submit(&mut node, vec![]).await?;
    let tx_hash = *base_payload
        .block()
        .body()
        .transactions
        .first()
        .expect("payload builder should include the pending transfer")
        .tx_hash();

    let before = canonical_snapshot(&node)?;
    let nonce_before = node
        .inner
        .provider
        .latest()?
        .basic_account(&sender)?
        .map_or(0, |account| account.nonce);

    let wrong_root = B256::repeat_byte(0x44);
    assert_ne!(
        wrong_root,
        base_payload.block().header().receipts_root(),
        "test fault must change the receipts root"
    );
    let invalid_data = payload_with_receipts_root(&base_payload, wrong_root);
    let validation_data = payload_with_receipts_root(&base_payload, B256::repeat_byte(0x55));
    assert_eq!(
        invalid_data.parent_hash, before.hash,
        "the payload must be a direct child of the head, so the receipts root is the only fault"
    );
    assert_ne!(
        validation_data.hash, invalid_data.hash,
        "validation must use a distinct payload to avoid the rejected-payload cache"
    );

    let client = node.auth_server_handle().http_client();
    let import_error = client
        .request::<MorphHeader, _>("engine_newL2BlockV2", (invalid_data,))
        .await
        .expect_err("newL2BlockV2 must reject the invalid execution result")
        .to_string();
    assert!(
        import_error.contains("receipt root mismatch"),
        "expected a post-execution receipt-root rejection, got: {import_error}"
    );
    // A payload whose header hash disagrees with its own fields is rejected before
    // execution, so that outcome would leave the execution path untested.
    assert!(
        !import_error.contains("block hash mismatch"),
        "payload was rejected by the pre-execution hash check: {import_error}"
    );
    assert_eq!(
        canonical_snapshot(&node)?,
        before,
        "failed import must leave head, state root, queue index, and safe tag unchanged"
    );
    let nonce_after_rejection = node
        .inner
        .provider
        .latest()?
        .basic_account(&sender)?
        .map_or(0, |account| account.nonce);
    assert_eq!(
        nonce_after_rejection, nonce_before,
        "failed execution must not commit account state"
    );
    wait_until_pooled(&node, tx_hash).await?;

    let validation: GenericResponse = client
        .request("engine_validateL2Block", (validation_data,))
        .await?;
    assert!(
        !validation.success,
        "validation must report the invalid payload as unusable"
    );
    assert_eq!(
        canonical_snapshot(&node)?,
        before,
        "validation must not mutate canonical state"
    );

    let valid_data = base_payload.executable_data().clone();
    let valid_hash = valid_data.hash;
    let imported: MorphHeader = client.request("engine_newL2BlockV2", (valid_data,)).await?;
    assert_eq!(imported.hash_slow(), valid_hash);

    let recovered = canonical_snapshot(&node)?;
    assert_eq!(recovered.number, before.number + 1);
    assert_eq!(recovered.hash, valid_hash);
    let nonce_after_recovery = node
        .inner
        .provider
        .latest()?
        .basic_account(&sender)?
        .map_or(0, |account| account.nonce);
    assert_eq!(nonce_after_recovery, nonce_before + 1);
    wait_until_evicted(&node, tx_hash).await?;

    Ok(())
}

/// Behavior contract:
/// - fault: a peer relays a validly signed block whose `NextL1MsgIndex` was corrupted in
///   transit; the field is not covered by the block hash, so the copy shares its hash with
///   the honest block;
/// - evidence: the follower rejects the corrupted copy for that reason, and the honest
///   block is still accepted afterwards instead of being refused as linking to a
///   previously rejected block until the invalid-header entry is evicted or the node
///   restarts (geth re-derives the field and accepts the honest block as well).
#[tokio::test(flavor = "multi_thread")]
async fn corrupted_next_l1_msg_index_does_not_poison_the_honest_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().with_num_nodes(2).build().await?;
    let follower = nodes.pop().expect("two nodes requested");
    let leader = nodes.pop().expect("two nodes requested");

    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(head_timestamp(&leader)? + 1);
    let honest = assemble_l2_block(&leader, params).await?;
    import_l2_block(&leader, honest.clone()).await?;
    let before = canonical_snapshot(&follower)?;

    let mut corrupted = honest.clone();
    corrupted.next_l1_message_index += 1;

    let err = import_l2_block(&follower, corrupted)
        .await
        .expect_err("the corrupted copy must be rejected");
    assert!(
        err.to_string().contains("invalid block.NextL1MsgIndex"),
        "rejection should name the corrupted field, got: {err}"
    );
    assert_eq!(
        canonical_snapshot(&follower)?,
        before,
        "a rejected block must leave the canonical chain untouched"
    );

    let header = import_l2_block(&follower, honest.clone()).await?;
    assert_eq!(
        header.hash_slow(),
        honest.hash,
        "the honest block shares the corrupted copy's hash"
    );
    assert_eq!(
        canonical_snapshot(&follower)?.hash,
        honest.hash,
        "the honest block must become the follower's head"
    );

    Ok(())
}

/// M-H07: Even when a non-empty block hash is already in `tree_state` (pre-inserted by
/// `assembleL2BlockV2`, so reth's `on_new_payload` would short-circuit on `AlreadySeen`),
/// tampering with the unhashed `next_l1_message_index` must be rejected by
/// `engine_validateL2Block`, `engine_newL2Block`, and `engine_newL2BlockV2` without
/// poisoning `invalid_headers` for the honest block.
#[tokio::test(flavor = "multi_thread")]
async fn tampered_next_l1_msg_index_rejected_when_block_already_seen() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().expect("one node requested");

    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce).await;
    let tx_hash = node.rpc.inject_tx(raw_tx).await?;
    wait_until_pooled(&node, tx_hash).await?;

    let before = canonical_snapshot(&node)?;
    let honest = assemble_l2_block_v2(
        &node,
        AssembleL2BlockV2Params {
            parent_hash: before.hash,
            transactions: vec![],
            timestamp: Some(head_timestamp(&node)? + 1),
        },
    )
    .await?;
    assert_eq!(honest.transactions.len(), 1);

    // Wait until the engine tree has pre-inserted the non-empty block so `AlreadySeen`
    // is guaranteed to be armed in `tree_state`.
    let deadline = tokio::time::Instant::now() + LOCAL_POLL_BUDGET;
    loop {
        if node
            .inner
            .provider
            .pending_block_num_hash()?
            .map(|p| p.hash)
            == Some(honest.hash)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "precondition: non-empty candidate was not pre-inserted into tree_state"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let mut tampered = honest.clone();
    tampered.next_l1_message_index += 1;

    let client = node.auth_server_handle().http_client();

    let validation: GenericResponse = client
        .request("engine_validateL2Block", (tampered.clone(),))
        .await?;
    assert!(
        !validation.success,
        "engine_validateL2Block must reject tampered next_l1_message_index on AlreadySeen block"
    );

    let err_v1 = client
        .request::<(), _>("engine_newL2Block", (tampered.clone(),))
        .await
        .expect_err("engine_newL2Block must reject tampered next_l1_message_index")
        .to_string();
    assert!(
        err_v1.contains("invalid block.NextL1MsgIndex"),
        "unexpected engine_newL2Block error: {err_v1}"
    );

    let err_v2 = import_l2_block(&node, tampered)
        .await
        .expect_err("engine_newL2BlockV2 must reject tampered next_l1_message_index")
        .to_string();
    assert!(
        err_v2.contains("invalid block.NextL1MsgIndex"),
        "unexpected engine_newL2BlockV2 error: {err_v2}"
    );

    assert_eq!(
        canonical_snapshot(&node)?,
        before,
        "rejected tampered requests must leave canonical chain unchanged"
    );

    // The honest copy sharing the same block hash must still import cleanly.
    let imported = import_l2_block(&node, honest.clone()).await?;
    assert_eq!(imported.hash_slow(), honest.hash);
    assert_eq!(canonical_snapshot(&node)?.hash, honest.hash);

    Ok(())
}

/// M-H07: A locally built non-empty block that violates consensus (e.g., future timestamp)
/// must be rejected by `on_inserted_executed_block` so it is NOT pre-inserted into
/// `tree_state` and cannot subsequently bypass validation via `AlreadySeen`.
#[tokio::test(flavor = "multi_thread")]
async fn pre_inserted_consensus_invalid_block_is_rejected_and_not_cached() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().expect("one node requested");

    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce).await;
    let tx_hash = node.rpc.inject_tx(raw_tx).await?;
    wait_until_pooled(&node, tx_hash).await?;

    let before = canonical_snapshot(&node)?;
    let future_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 3600;

    let future_block = assemble_l2_block_v2(
        &node,
        AssembleL2BlockV2Params {
            parent_hash: before.hash,
            transactions: vec![],
            timestamp: Some(future_timestamp),
        },
    )
    .await?;
    assert_eq!(
        future_block.transactions.len(),
        1,
        "precondition: block must be non-empty so InsertExecutedBlock is emitted"
    );

    // Give the async engine tree task ample time to process InsertExecutedBlock;
    // `on_inserted_executed_block` must reject the future-timestamp header.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        node.inner.provider.pending_block_num_hash()?,
        None,
        "consensus-invalid future block must not be pre-inserted into tree_state"
    );

    // Importing it must fail consensus validation rather than hitting AlreadySeen.
    let err = import_l2_block(&node, future_block)
        .await
        .expect_err("future-timestamp block must be rejected on import")
        .to_string();
    assert!(
        err.to_lowercase().contains("future"),
        "expected future-timestamp consensus rejection, got: {err}"
    );
    assert_eq!(
        canonical_snapshot(&node)?,
        before,
        "canonical state must remain unchanged after rejecting future block"
    );

    Ok(())
}
