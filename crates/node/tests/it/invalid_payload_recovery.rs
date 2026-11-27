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

/// Locally assembled blocks bypass the 0-second wall-clock `TimestampIsInFuture` check on the
/// assembling sequencer (matching morph-geth's `AssembleL2Block -> NewL2Block` `verified` path,
/// including HA switchover where `parent.timestamp > now` causes `payload_timestamp` to raise
/// the child timestamp past local wall-clock), while external followers that did not assemble
/// the block still enforce `TimestampIsInFuture`.
#[tokio::test(flavor = "multi_thread")]
async fn locally_assembled_future_timestamp_accepted_on_assembler_and_rejected_on_follower()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().with_num_nodes(2).build().await?;
    let follower = nodes.pop().expect("two nodes requested");
    let sequencer = nodes.pop().expect("two nodes requested");

    let raw_tx0 = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    let tx0_hash = sequencer.rpc.inject_tx(raw_tx0).await?;
    wait_until_pooled(&sequencer, tx0_hash).await?;

    let genesis = canonical_snapshot(&sequencer)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let future_timestamp = now + 3600;

    // 1. Sequencer locally assembles a non-empty block ahead of local wall clock.
    let future_block = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: genesis.hash,
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

    // Wait for `on_inserted_executed_block` to pre-insert the locally assembled block.
    let deadline = tokio::time::Instant::now() + LOCAL_POLL_BUDGET;
    loop {
        if sequencer
            .inner
            .provider
            .pending_block_num_hash()?
            .map(|p| p.hash)
            == Some(future_block.hash)
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "locally assembled non-empty block must be pre-inserted into tree_state"
        );
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    // Follower did NOT assemble `future_block`: both validate and import must reject it as future.
    let follower_client = follower.auth_server_handle().http_client();
    let follower_validation: GenericResponse = follower_client
        .request("engine_validateL2Block", (future_block.clone(),))
        .await?;
    assert!(
        !follower_validation.success,
        "follower must reject untrusted future-timestamp block in engine_validateL2Block"
    );
    let follower_err = import_l2_block(&follower, future_block.clone())
        .await
        .expect_err("follower must reject untrusted future-timestamp block on import")
        .to_string();
    assert!(
        follower_err.to_lowercase().contains("future"),
        "expected future-timestamp consensus rejection on follower, got: {follower_err}"
    );
    assert_eq!(
        canonical_snapshot(&follower)?,
        genesis,
        "follower canonical state must remain unchanged"
    );

    // Sequencer locally assembled `future_block`: validate and import must succeed.
    let seq_client = sequencer.auth_server_handle().http_client();
    let seq_validation: GenericResponse = seq_client
        .request("engine_validateL2Block", (future_block.clone(),))
        .await?;
    assert!(seq_validation.success);
    let imported1 = import_l2_block(&sequencer, future_block.clone()).await?;
    assert_eq!(imported1.hash_slow(), future_block.hash);

    // 2. HA clock-behind-parent path: `parent.timestamp == now + 3600 > now`.
    //    `assemble_l2_block_v2` with `timestamp: Some(now)` raises timestamp to `now + 3600`.
    //    Build an EMPTY block so `InsertExecutedBlock` is skipped and `import_l2_block`
    //    exercises `new_payload -> validate_payload` under `LocallyBuilt`.
    let empty_ha_block = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: future_block.hash,
            transactions: vec![],
            timestamp: Some(now),
        },
    )
    .await?;
    assert!(empty_ha_block.transactions.is_empty());
    assert_eq!(empty_ha_block.timestamp, future_timestamp);
    let imported2 = import_l2_block(&sequencer, empty_ha_block.clone()).await?;
    assert_eq!(imported2.hash_slow(), empty_ha_block.hash);

    // 3. Reorg locally-built path (`number <= canonical_head`, where reth also skips
    //    `InsertExecutedBlock` even for non-empty blocks): rebuild height 2 on `future_block`.
    let raw_tx1 = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 1).await;
    let tx1_hash = sequencer.rpc.inject_tx(raw_tx1).await?;
    wait_until_pooled(&sequencer, tx1_hash).await?;

    let reorg_ha_block = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: future_block.hash,
            transactions: vec![],
            timestamp: Some(now),
        },
    )
    .await?;
    assert_eq!(reorg_ha_block.transactions.len(), 1);
    assert_eq!(reorg_ha_block.timestamp, future_timestamp);
    let imported2_reorg = import_l2_block(&sequencer, reorg_ha_block.clone()).await?;
    assert_eq!(imported2_reorg.hash_slow(), reorg_ha_block.hash);
    assert_eq!(canonical_snapshot(&sequencer)?.hash, reorg_ha_block.hash);

    Ok(())
}

/// Forged non-zero `withdraw_trie_root` must be rejected across ALL known-block paths:
/// 1. `AlreadySeen` in `tree_state` via `assembleL2BlockV2` pre-insertion (untouched zero slot),
/// 2. Live execution AND `AlreadySeen` in `tree_state` on a follower when an earlier block set a
///    non-zero `withdraw_trie_root` and the target block did NOT touch the slot in its own diff,
/// 3. DB-persisted `AlreadySeen` after advancing past the persistence threshold,
/// while `B256::ZERO` (skip optional check) and the true root continue to succeed without
/// poisoning `invalid_headers`.
#[tokio::test(flavor = "multi_thread")]
async fn forged_withdraw_trie_root_rejected_across_all_known_block_paths() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use morph_node::test_utils::MorphTxBuilder;

    // 7-byte runtime on L2MessageQueue (`0x5300...0001`): `SSTORE(CALLDATALOAD(0), CALLDATALOAD(32))`.
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_code(
            morph_chainspec::L2_MESSAGE_QUEUE_ADDRESS,
            "0x6020355f355500",
        )
        .with_num_nodes(2)
        .build()
        .await?;
    let follower = nodes.pop().expect("two nodes requested");
    let sequencer = nodes.pop().expect("two nodes requested");

    let seq_client = sequencer.auth_server_handle().http_client();
    let fol_client = follower.auth_server_handle().http_client();

    // --- Path 1: Pre-inserted `tree_state` block with untouched zero withdraw slot ---
    let raw_tx0 = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    let tx0_hash = sequencer.rpc.inject_tx(raw_tx0).await?;
    wait_until_pooled(&sequencer, tx0_hash).await?;

    let genesis = canonical_snapshot(&sequencer)?;
    let block1 = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: genesis.hash,
            transactions: vec![],
            timestamp: Some(head_timestamp(&sequencer)? + 1),
        },
    )
    .await?;
    assert_eq!(block1.withdraw_trie_root, B256::ZERO);

    let deadline = tokio::time::Instant::now() + LOCAL_POLL_BUDGET;
    loop {
        if sequencer
            .inner
            .provider
            .pending_block_num_hash()?
            .map(|p| p.hash)
            == Some(block1.hash)
        {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(POLL_INTERVAL).await;
    }

    let forged_root = B256::repeat_byte(0xde);
    let mut block1_forged = block1.clone();
    block1_forged.withdraw_trie_root = forged_root;

    let res: GenericResponse = seq_client
        .request("engine_validateL2Block", (block1_forged.clone(),))
        .await?;
    assert!(
        !res.success,
        "pre-inserted block with forged non-zero withdraw_trie_root must fail validateL2Block"
    );
    assert!(
        seq_client
            .request::<(), _>("engine_newL2Block", (block1_forged.clone(),))
            .await
            .is_err(),
        "pre-inserted block with forged non-zero withdraw_trie_root must fail newL2Block"
    );
    assert!(
        import_l2_block(&sequencer, block1_forged).await.is_err(),
        "pre-inserted block with forged non-zero withdraw_trie_root must fail newL2BlockV2"
    );

    // Honest block1 (withdraw_trie_root == ZERO) must still validate and import on both nodes.
    let ok_res: GenericResponse = seq_client
        .request("engine_validateL2Block", (block1.clone(),))
        .await?;
    assert!(ok_res.success);
    import_l2_block(&sequencer, block1.clone()).await?;
    import_l2_block(&follower, block1.clone()).await?;

    // --- Path 2: Block 2 updates withdraw slot to R1; Block 3 leaves slot untouched ---
    let real_root = B256::repeat_byte(0xab);
    let mut wtr_calldata = Vec::with_capacity(64);
    wtr_calldata.extend_from_slice(
        B256::from(morph_chainspec::L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT).as_slice(),
    );
    wtr_calldata.extend_from_slice(real_root.as_slice());
    let set_wtr_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(morph_chainspec::L2_MESSAGE_QUEUE_ADDRESS)
        .with_data(wtr_calldata)
        .with_gas_limit(100_000)
        .build_signed()?;
    let set_wtr_hash = sequencer.rpc.inject_tx(set_wtr_tx).await?;
    wait_until_pooled(&sequencer, set_wtr_hash).await?;

    let block2 = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: block1.hash,
            transactions: vec![],
            timestamp: Some(block1.timestamp + 1),
        },
    )
    .await?;
    assert_eq!(
        block2.withdraw_trie_root, real_root,
        "assembled block2 must reflect updated withdraw_trie_root slot"
    );
    import_l2_block(&sequencer, block2.clone()).await?;
    import_l2_block(&follower, block2.clone()).await?;

    // Block 3 is empty (does NOT write slot 0 in its BundleState), so post-Block-3 root is still `real_root`.
    let block3 = assemble_l2_block_v2(
        &sequencer,
        AssembleL2BlockV2Params {
            parent_hash: block2.hash,
            transactions: vec![],
            timestamp: Some(block2.timestamp + 1),
        },
    )
    .await?;
    assert!(block3.transactions.is_empty());
    assert_eq!(block3.withdraw_trie_root, real_root);
    import_l2_block(&sequencer, block3.clone()).await?;

    // On `follower` (which did NOT assemble `block3`):
    // 2a. Live execution with forged non-zero root when slot 0 was untouched in Block 3 must fail!
    let mut block3_forged = block3.clone();
    block3_forged.withdraw_trie_root = B256::repeat_byte(0xee);
    let live_bad: GenericResponse = fol_client
        .request("engine_validateL2Block", (block3_forged.clone(),))
        .await?;
    assert!(
        !live_bad.success,
        "live validateL2Block must reject forged withdraw_trie_root even when slot was untouched in block"
    );

    // 2b. Validate `block3` with ZERO root on `follower` so `block3` enters `follower`'s `tree_state`.
    let mut block3_zero = block3.clone();
    block3_zero.withdraw_trie_root = B256::ZERO;
    let live_zero: GenericResponse = fol_client
        .request("engine_validateL2Block", (block3_zero,))
        .await?;
    assert!(live_zero.success);

    // 2c. Now `block3` hits `AlreadySeen` in `follower`'s `tree_state`: forged root must STILL fail!
    let seen_bad: GenericResponse = fol_client
        .request("engine_validateL2Block", (block3_forged.clone(),))
        .await?;
    assert!(
        !seen_bad.success,
        "AlreadySeen in tree_state must reject forged non-zero withdraw_trie_root"
    );
    assert!(
        import_l2_block(&follower, block3_forged).await.is_err(),
        "AlreadySeen import must reject forged non-zero withdraw_trie_root"
    );

    // Honest `real_root` must still validate and import cleanly on `follower`.
    let seen_good: GenericResponse = fol_client
        .request("engine_validateL2Block", (block3.clone(),))
        .await?;
    assert!(seen_good.success);
    import_l2_block(&follower, block3.clone()).await?;

    // --- Path 3: Advance blocks so `block2` is persisted to MDBX and verify DB AlreadySeen ---
    let mut parent_hash = block3.hash;
    let mut ts = block3.timestamp;
    for _ in 0..6 {
        ts += 1;
        let next_blk = assemble_l2_block_v2(
            &sequencer,
            AssembleL2BlockV2Params {
                parent_hash,
                transactions: vec![],
                timestamp: Some(ts),
            },
        )
        .await?;
        import_l2_block(&sequencer, next_blk.clone()).await?;
        import_l2_block(&follower, next_blk.clone()).await?;
        parent_hash = next_blk.hash;
    }

    let mut block2_forged = block2.clone();
    block2_forged.withdraw_trie_root = B256::repeat_byte(0x99);
    let db_err = import_l2_block(&follower, block2_forged)
        .await
        .expect_err("persisted block must reject forged non-zero withdraw_trie_root")
        .to_string();
    assert!(
        db_err.contains("withdraw trie root mismatch"),
        "expected withdraw trie root mismatch on persisted block, got: {db_err}"
    );
    let db_good = import_l2_block(&follower, block2.clone()).await?;
    assert_eq!(db_good.hash_slow(), block2.hash);

    // --- Path 4: Cold-MDBX lookup (`known_withdraw_trie_roots` cache empty!) ---
    // On a freshly booted node, genesis is persisted in MDBX (`provider.sealed_header_by_hash`
    // returns `Some`) while `known_withdraw_trie_roots` is completely empty, exercising the
    // cold-DB `state_by_block_hash` fallback in `MorphTreeEngineValidator::convert_payload_to_block`.
    let genesis_wtr = B256::repeat_byte(0x77);
    let (mut cold_nodes, _) = TestNodeBuilder::new()
        .with_account_storage(
            morph_chainspec::L2_MESSAGE_QUEUE_ADDRESS,
            B256::from(morph_chainspec::L2_MESSAGE_QUEUE_WITHDRAW_TRIE_ROOT_SLOT),
            genesis_wtr,
        )
        .build()
        .await?;
    let cold_node = cold_nodes.pop().unwrap();
    let cold_genesis = std::sync::Arc::new(reth_primitives_traits::SealedBlock::seal_slow(
        reth_provider::BlockReader::block_by_number(&cold_node.inner.provider, 0)?
            .expect("genesis block in MDBX"),
    ));
    let forged_genesis_payload =
        morph_payload_types::MorphExecutionData::with_expected_withdraw_trie_root(
            cold_genesis.clone(),
            B256::repeat_byte(0x88),
        );
    let cold_bad_status = cold_node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(forged_genesis_payload)
        .await?;
    assert!(
        cold_bad_status.is_invalid(),
        "cold-MDBX block with forged withdraw_trie_root must be rejected: {cold_bad_status:?}"
    );
    let honest_genesis_payload =
        morph_payload_types::MorphExecutionData::with_expected_withdraw_trie_root(
            cold_genesis,
            genesis_wtr,
        );
    let cold_ok_status = cold_node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(honest_genesis_payload)
        .await?;
    assert!(
        cold_ok_status.is_valid(),
        "cold-MDBX block with matching withdraw_trie_root must succeed: {cold_ok_status:?}"
    );

    Ok(())
}

/// Structural consensus violations (such as non-empty `extra_data` or `gas_used > gas_limit`)
/// must still be rejected even when `MorphPayloadValidationMode::LocallyBuilt` or `Safe` is set.
#[tokio::test(flavor = "multi_thread")]
async fn locally_built_and_safe_modes_still_reject_structural_consensus_violations()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use morph_payload_types::{MorphPayloadTypes, MorphPayloadValidationMode};
    use reth_node_api::PayloadTypes;
    use reth_primitives_traits::SealedBlock;

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().expect("one node requested");

    let base_payload = build_block_no_submit(&mut node, vec![]).await?;
    for mode in [
        MorphPayloadValidationMode::LocallyBuilt,
        MorphPayloadValidationMode::Safe,
    ] {
        let mut bad_extra = morph_primitives::Block::new(
            base_payload.block().header().clone(),
            base_payload.block().body().clone(),
        );
        bad_extra.header.inner.extra_data = alloy_primitives::Bytes::from_static(b"invalid");
        let payload_extra =
            MorphPayloadTypes::block_to_payload(SealedBlock::seal_slow(bad_extra), None)
                .with_validation_mode(mode);
        let status = node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .new_payload(payload_extra)
            .await?;
        assert!(
            status.is_invalid(),
            "non-empty extra_data must be rejected in {mode:?} mode: {status:?}"
        );

        let mut bad_gas = morph_primitives::Block::new(
            base_payload.block().header().clone(),
            base_payload.block().body().clone(),
        );
        bad_gas.header.inner.gas_used = bad_gas.header.inner.gas_limit + 1;
        let payload_gas =
            MorphPayloadTypes::block_to_payload(SealedBlock::seal_slow(bad_gas), None)
                .with_validation_mode(mode);
        let status = node
            .inner
            .add_ons_handle
            .beacon_engine_handle
            .new_payload(payload_gas)
            .await?;
        assert!(
            status.is_invalid(),
            "gas_used > gas_limit must be rejected in {mode:?} mode: {status:?}"
        );
    }

    Ok(())
}
