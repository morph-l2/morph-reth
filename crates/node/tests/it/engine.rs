//! Engine API behavior integration tests.
//!
//! Verifies engine-level semantics that are distinct from consensus rule
//! enforcement — in particular the state-root validation gating introduced
//! by the Jade hardfork.

use alloy_consensus::transaction::TxHashRef;
use alloy_consensus::{BlockHeader, Sealable};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{HardforkSchedule, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder};
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphPayloadAttributes,
    MorphPayloadTypes, SafeL2Data,
};
use morph_primitives::MorphHeader;
use reth_node_api::PayloadTypes;
use reth_payload_builder::BuildNewPayload;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{BlockIdReader, BlockReaderIdExt};

use super::helpers::{
    assemble_l2_block, build_block_no_submit, canonical_block, canonical_snapshot,
    craft_and_try_import_block, head_timestamp, import_l2_block, transaction_hashes,
    wait_until_pooled,
};

/// Pre-Jade: a block with a wrong state root is still accepted.
///
/// Before Jade, morph-reth computes an MPT state root but the canonical
/// chain uses ZK-trie roots. Rather than implementing ZK-trie, morph-reth
/// skips state root validation entirely in pre-Jade mode. A tampered state
/// root must therefore not cause rejection.
///
/// This is the mirror image of `post_jade_state_root_mismatch_is_rejected`
/// in `consensus.rs` — together they prove the Jade hardfork boundary.
#[tokio::test(flavor = "multi_thread")]
async fn state_root_validation_skipped_pre_jade() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
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
        accepted,
        "pre-Jade block with wrong state root must be accepted (state root validation skipped)"
    );

    Ok(())
}

/// P2P-downloaded blocks enter through the block-input path, where no
/// Engine-API withdraw-root expectation has been registered locally.
#[tokio::test(flavor = "multi_thread")]
async fn p2p_downloaded_block_imports_without_registered_expectation() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::AllActive)
        .with_num_nodes(2)
        .build()
        .await?;
    let node1 = nodes.pop().expect("two nodes requested");
    let mut node0 = nodes.pop().expect("two nodes requested");

    let payload = build_block_no_submit(&mut node0, vec![]).await?;
    let head_hash = payload.block().hash();
    let execution_data = MorphPayloadTypes::block_to_payload(payload.block().clone(), None);
    let status = node0
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(execution_data)
        .await?;
    assert!(
        status.is_valid(),
        "source node rejected its block: {status:?}"
    );
    node0.update_forkchoice(head_hash, head_hash).await?;

    node1.sync_to(head_hash).await?;
    Ok(())
}

/// `engine_newL2Block` can import consecutive blocks assembled over the authenticated RPC.
#[tokio::test(flavor = "multi_thread")]
async fn new_l2_block_imports_consecutive_assembled_blocks_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let auth = node.auth_server_handle();
    let client = auth.http_client();
    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(1);

    let data: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let expected_hash = data.hash;

    let _: () = client.request("engine_newL2Block", (data,)).await?;

    let latest = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("latest header must exist after importing the block");

    assert_eq!(
        latest.number(),
        1,
        "engine_newL2Block should advance the head"
    );
    assert_eq!(
        latest.hash(),
        expected_hash,
        "imported canonical head should match the assembled block hash"
    );

    let mut params = AssembleL2BlockParams::empty(2);
    params.timestamp = Some(latest.timestamp() + 1);

    let data: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let expected_hash = data.hash;

    let _: () = client.request("engine_newL2Block", (data,)).await?;

    let latest = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("latest header must exist after importing the second block");

    assert_eq!(
        latest.number(),
        2,
        "engine_newL2Block should expose the first imported block as the parent immediately"
    );
    assert_eq!(
        latest.hash(),
        expected_hash,
        "second imported canonical head should match the assembled block hash"
    );

    Ok(())
}

/// Unsafe `engine_newL2Block` imports must not advance the safe tag.
///
/// The FCU sent by the import path should keep `safe` as zero unless the caller explicitly
/// updates safe through `engine_setBlockTags` / `engine_newSafeL2Block`. Otherwise each
/// unsafe block would mark its parent safe, which is not Morph's L1-derived safe semantics.
#[tokio::test(flavor = "multi_thread")]
async fn new_l2_block_does_not_advance_safe_tag() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let auth = node.auth_server_handle();
    let client = auth.http_client();

    let initial_safe = node.inner.provider.safe_block_hash()?;

    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(1);
    let block1: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let block1_hash = block1.hash;
    let _: () = client.request("engine_newL2Block", (block1,)).await?;

    let mut params = AssembleL2BlockParams::empty(2);
    params.timestamp = Some(2);
    let block2: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let _: () = client.request("engine_newL2Block", (block2,)).await?;

    let safe_after_unsafe_imports = node.inner.provider.safe_block_hash()?;

    assert_eq!(
        safe_after_unsafe_imports, initial_safe,
        "unsafe imports must not advance the RPC-visible safe tag"
    );
    assert_ne!(
        safe_after_unsafe_imports,
        Some(block1_hash),
        "unsafe import must not mark the previous unsafe block as safe"
    );

    Ok(())
}

/// `engine_newL2BlockV2` imports a block built on the current head and returns its header.
///
/// V2 selects the parent via `data.parent_hash` (rather than requiring it to equal the
/// current head as V1 does). For a block extending the head the two behave identically;
/// this pins the additive happy path before the reorg behavior is exercised separately.
#[tokio::test(flavor = "multi_thread")]
async fn new_l2_block_v2_imports_block_on_current_head() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let auth = node.auth_server_handle();
    let client = auth.http_client();
    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(1);

    let data: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let expected_hash = data.hash;

    let header: MorphHeader = client.request("engine_newL2BlockV2", (data,)).await?;

    assert_eq!(header.number(), 1, "returned header should be block 1");
    assert_eq!(
        header.hash_slow(),
        expected_hash,
        "returned header hash should match the assembled block"
    );

    let latest = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("latest header must exist after importing the block");
    assert_eq!(
        latest.number(),
        1,
        "engine_newL2BlockV2 should advance the head"
    );
    assert_eq!(
        latest.hash(),
        expected_hash,
        "imported canonical head should match the assembled block hash"
    );

    Ok(())
}

/// `engine_newL2BlockV2` reorganizes the canonical chain onto a sibling block.
///
/// Two distinct blocks are assembled at height 2 on the same parent (block 1) while the
/// head still points at block 1. Importing the first makes it canonical; importing the
/// second — which builds on the same parent, not on the new head — must reorg the head
/// onto it. This is the core capability the centralized sequencer relies on
/// (`NewL2BlockV2` + `SetCanonical`); the V1 path would reject the sibling with a
/// wrong-parent-hash error. Near-wall-clock timestamps keep the blocks out of the
/// historical-finalization fallback so the engine permits the reorg.
#[tokio::test(flavor = "multi_thread")]
async fn new_l2_block_v2_reorgs_onto_sibling_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let auth = node.auth_server_handle();
    let client = auth.http_client();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Block 1 on genesis. Timestamps sit a few seconds in the past: recent enough to
    // stay out of the historical-finalization fallback (so the engine permits the
    // reorg) but not in the future (which header validation would reject).
    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(now - 6);
    let block1: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let block1_hash = block1.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block1,)).await?;

    // Assemble two distinct siblings at height 2 on block 1. The head stays at block 1
    // until we import, so both build on it. They differ only by timestamp, hence by hash.
    let mut params_a = AssembleL2BlockParams::empty(2);
    params_a.timestamp = Some(now - 4);
    let block2a: ExecutableL2Data = client
        .request("engine_assembleL2Block", (params_a,))
        .await?;

    let mut params_b = AssembleL2BlockParams::empty(2);
    params_b.timestamp = Some(now - 2);
    let block2b: ExecutableL2Data = client
        .request("engine_assembleL2Block", (params_b,))
        .await?;

    assert_eq!(block2a.parent_hash, block1_hash, "2a must build on block 1");
    assert_eq!(block2b.parent_hash, block1_hash, "2b must build on block 1");
    assert_ne!(
        block2a.hash, block2b.hash,
        "siblings must have distinct hashes"
    );
    let block2b_hash = block2b.hash;

    // Import sibling A → canonical head = 2a.
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block2a,)).await?;
    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after importing sibling A");
    assert_eq!(head.number(), 2, "sibling A should be at height 2");

    // Import sibling B (builds on block 1, not on 2a) → must reorg the head onto it.
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block2b,)).await?;
    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after importing sibling B");
    assert_eq!(head.number(), 2, "head stays at height 2 after the reorg");
    assert_eq!(head.hash(), block2b_hash, "head must reorg onto sibling B");

    Ok(())
}

/// `engine_newSafeL2Block` with `parentHash` reorganizes onto a non-head parent.
///
/// Mirrors derivation's `deriveForce`: after the local chain already has block 2A
/// (imported live), the derivation pipeline re-derives block 2 from L1 batch data and
/// pins its parent to block 1. The safe path executes the block on that pinned parent
/// and the engine reorgs the head onto the L1-canonical block 2B. Without `parentHash`
/// the safe path requires `number == head + 1` and would reject this.
#[tokio::test(flavor = "multi_thread")]
async fn new_safe_l2_block_with_parent_hash_reorgs_onto_non_head_parent() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let auth = node.auth_server_handle();
    let client = auth.http_client();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Block 1 on genesis (live import). Past-but-recent timestamps keep the blocks out
    // of the historical-finalization fallback (so the reorg is permitted) without
    // tripping the future-timestamp header check.
    let mut p1 = AssembleL2BlockParams::empty(1);
    p1.timestamp = Some(now - 6);
    let block1: ExecutableL2Data = client.request("engine_assembleL2Block", (p1,)).await?;
    let block1_hash = block1.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block1,)).await?;

    // Block 2A on block 1 (live import) → canonical head = 2A.
    let mut p2 = AssembleL2BlockParams::empty(2);
    p2.timestamp = Some(now - 4);
    let block2a: ExecutableL2Data = client.request("engine_assembleL2Block", (p2,)).await?;
    let block2a_hash = block2a.hash;
    let gas_limit = block2a.gas_limit;
    let base_fee = block2a.base_fee_per_gas;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block2a,)).await?;

    // Re-derive block 2 from L1 data, parent pinned to block 1. A different timestamp
    // gives it a distinct hash from 2A, forcing a real reorg.
    let safe = SafeL2Data {
        number: 2,
        gas_limit,
        base_fee_per_gas: base_fee,
        timestamp: now - 2,
        transactions: vec![],
        parent_hash: Some(block1_hash),
    };

    let header: MorphHeader = client.request("engine_newSafeL2Block", (safe,)).await?;
    assert_eq!(header.number(), 2, "returned safe header is at height 2");

    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after safe reorg");
    assert_eq!(
        head.number(),
        2,
        "head stays at height 2 after the safe reorg"
    );
    assert_ne!(
        head.hash(),
        block2a_hash,
        "head must have reorged off block 2A"
    );
    assert_eq!(
        head.hash(),
        header.hash_slow(),
        "canonical head must match the returned safe header"
    );

    Ok(())
}

/// A safe block reorged away by a later `engine_newSafeL2Block(parentHash)` must not leave a
/// stale `safe` tag that fails the next forkchoice.
///
/// Regression for the FCU carrying a cached/stale safe hash: block 2A is imported via the
/// *safe* path (which sets it as the canonical safe block), then re-derivation pins block 2B
/// onto block 1, reorging 2A away. The import forkchoice must not carry the now-non-canonical
/// 2A as `safe` — reth rejects a non-canonical safe with `invalid_state`, failing the import.
/// Unlike `new_safe_l2_block_with_parent_hash_reorgs_onto_non_head_parent`, here 2A is
/// imported via the safe path, so it IS recorded as the safe block before the reorg.
#[tokio::test(flavor = "multi_thread")]
async fn safe_block_reorg_does_not_carry_stale_safe_into_forkchoice() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let auth = node.auth_server_handle();
    let client = auth.http_client();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Block 1 on genesis.
    let mut p1 = AssembleL2BlockParams::empty(1);
    p1.timestamp = Some(now - 6);
    let block1: ExecutableL2Data = client.request("engine_assembleL2Block", (p1,)).await?;
    let block1_hash = block1.hash;
    let gas_limit = block1.gas_limit;
    let base_fee = block1.base_fee_per_gas;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block1,)).await?;

    // Block 2A imported via the SAFE path (parent = head = block 1) → 2A becomes the
    // canonical safe block.
    let safe_2a = SafeL2Data {
        number: 2,
        gas_limit,
        base_fee_per_gas: base_fee,
        timestamp: now - 4,
        transactions: vec![],
        parent_hash: Some(block1_hash),
    };
    let header_2a: MorphHeader = client.request("engine_newSafeL2Block", (safe_2a,)).await?;
    let block2a_hash = header_2a.hash_slow();
    let safe_after_2a = node.inner.provider.safe_block_hash()?;
    assert_eq!(
        safe_after_2a,
        Some(block2a_hash),
        "precondition: 2A must be recorded as safe before reorg test"
    );

    // Re-derive block 2B pinned to block 1, reorging 2A away. The forkchoice must not carry
    // the stale safe (2A): it is no longer canonical, so reth would return invalid_state.
    let safe_2b = SafeL2Data {
        number: 2,
        gas_limit,
        base_fee_per_gas: base_fee,
        timestamp: now - 2,
        transactions: vec![],
        parent_hash: Some(block1_hash),
    };
    let header_2b: MorphHeader = client.request("engine_newSafeL2Block", (safe_2b,)).await?;
    assert_ne!(
        header_2b.hash_slow(),
        block2a_hash,
        "2B must differ from 2A"
    );

    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after safe reorg");
    assert_eq!(
        head.number(),
        2,
        "head stays at height 2 after the safe reorg"
    );
    assert_eq!(head.hash(), header_2b.hash_slow(), "head reorged onto 2B");
    assert_ne!(head.hash(), block2a_hash, "head reorged off 2A");

    Ok(())
}

/// `engine_assembleL2BlockV2` builds on an explicitly given parent hash (not the head).
///
/// V2 keys assembly on a parent hash rather than a block number, so the sequencer can
/// build on any parent — including one that is no longer the canonical head. Here a
/// second block 1' is assembled on genesis after block 1 is already canonical, then
/// imported as a reorg. V2 uses a single params object, matching the V1
/// `engine_assembleL2Block` style.
#[tokio::test(flavor = "multi_thread")]
async fn assemble_l2_block_v2_builds_on_explicit_parent() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let auth = node.auth_server_handle();
    let client = auth.http_client();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let genesis = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("genesis header");
    let genesis_hash = genesis.hash();

    // Assemble + import block 1 on genesis.
    let block1: ExecutableL2Data = client
        .request(
            "engine_assembleL2BlockV2",
            (serde_json::json!({
                "parentHash": genesis_hash,
                "timestamp": format!("{:#x}", now - 6),
                "transactions": [],
            }),),
        )
        .await?;
    assert_eq!(block1.number, 1, "assembled block is at height 1");
    assert_eq!(
        block1.parent_hash, genesis_hash,
        "assembled block must build on the given parent"
    );
    let block1_hash = block1.hash;
    let _: MorphHeader = client.request("engine_newL2BlockV2", (block1,)).await?;

    // Assemble a sibling 1' on genesis (still a valid parent though no longer the head),
    // distinguished by timestamp, and import it as a reorg.
    let block1_prime: ExecutableL2Data = client
        .request(
            "engine_assembleL2BlockV2",
            (serde_json::json!({
                "parentHash": genesis_hash,
                "timestamp": format!("{:#x}", now - 3),
                "transactions": [],
            }),),
        )
        .await?;
    assert_eq!(block1_prime.parent_hash, genesis_hash);
    assert_eq!(block1_prime.number, 1);
    assert_ne!(
        block1_prime.hash, block1_hash,
        "sibling must differ from block 1"
    );
    let block1_prime_hash = block1_prime.hash;

    let _: MorphHeader = client
        .request("engine_newL2BlockV2", (block1_prime,))
        .await?;
    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after sibling import");
    assert_eq!(head.number(), 1, "head stays at height 1 after reorg");
    assert_eq!(
        head.hash(),
        block1_prime_hash,
        "head must reorg onto the sibling assembled via V2"
    );

    Ok(())
}

/// `engine_validateL2Block` rejects a tampered block hash over authenticated RPC.
#[tokio::test(flavor = "multi_thread")]
async fn validate_l2_block_rejects_tampered_hash_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let auth = node.auth_server_handle();
    let client = auth.http_client();
    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(1);

    let mut data: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    data.hash = B256::from([0xFF; 32]);

    let response: GenericResponse = client.request("engine_validateL2Block", (data,)).await?;

    assert!(
        !response.success,
        "engine_validateL2Block should reject tampered block hashes"
    );

    Ok(())
}

/// A non-zero `prev_randao` must not change the built block hash on Morph L2.
#[tokio::test(flavor = "multi_thread")]
async fn payload_builder_hash_matches_block_hash_with_nonzero_prev_randao() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?;
    let (head_hash, head_ts) = head
        .map(|h| (h.hash(), h.timestamp()))
        .unwrap_or((B256::ZERO, 0));

    let rpc_attrs = MorphPayloadAttributes {
        inner: PayloadAttributes {
            timestamp: head_ts + 1,
            prev_randao: B256::repeat_byte(0xAA),
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(vec![]),
        no_tx_pool: false,
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let payload_id = node
        .inner
        .payload_builder_handle
        .send_new_payload(BuildNewPayload {
            attributes: rpc_attrs,
            parent_hash: head_hash,
            cache: None,
            state_root_handle: None,
        })
        .await?
        .map_err(|e| eyre::eyre!("payload build failed: {e}"))?;

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let payload = loop {
        if tokio::time::Instant::now() > deadline {
            return Err(eyre::eyre!("timeout waiting for payload {payload_id:?}"));
        }
        match node
            .inner
            .payload_builder_handle
            .best_payload(payload_id)
            .await
        {
            Some(Ok(p)) => break p,
            Some(Err(e)) => return Err(eyre::eyre!("payload build error: {e}")),
            None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    };

    assert_eq!(
        payload.block().header().mix_hash(),
        Some(B256::ZERO),
        "Morph blocks should always use a zero mix_hash"
    );
    assert_eq!(
        payload.block().hash(),
        payload.executable_data.hash,
        "ExecutableL2Data hash should match the built block hash"
    );

    Ok(())
}

/// Recast a sequencer-assembled block as the `SafeL2Data` that derivation would reconstruct
/// from an L1 batch: the same inputs, minus every execution output.
///
/// `parent_hash` is pinned so the follower reconstructs the block on the intended parent
/// rather than on whatever its current head happens to be.
fn safe_data_from(block: &ExecutableL2Data) -> SafeL2Data {
    SafeL2Data {
        number: block.number,
        gas_limit: block.gas_limit,
        base_fee_per_gas: block.base_fee_per_gas,
        timestamp: block.timestamp,
        transactions: block.transactions.clone(),
        parent_hash: Some(block.parent_hash),
    }
}

/// Hash of a raw encoded transaction.
fn tx_hash_of(raw: &alloy_primitives::Bytes) -> B256 {
    let mut raw = raw.as_ref();
    *morph_primitives::MorphTxEnvelope::decode_2718(&mut raw)
        .expect("raw transaction is decodable")
        .tx_hash()
}

/// Transaction hashes of an assembled block, in block order.
fn transaction_hashes_of(block: &ExecutableL2Data) -> Vec<B256> {
    block.transactions.iter().map(tx_hash_of).collect()
}

/// Regression test for #179: `engine_newSafeL2Block` must never pull from the local txpool.
///
/// A derivation follower's pool holds gossiped transactions that the sequencer committed to
/// *later* blocks. Before the fix, the safe path reused the sequencer assembly builder, which
/// unconditionally appended the best pool transactions, so an empty committed block absorbed
/// future transactions. The follower forked off the sequencer at that height and then stalled
/// when those transactions were supplied again at their real heights ("nonce too low").
///
/// `SafeL2Data` carries no expected block hash, so nothing downstream catches the divergence —
/// which is why the original failure surfaced 114 blocks past its cause.
///
/// Behavior contract:
/// - fault: the follower derives block 1 (empty) while its pool already holds the transactions
///   the sequencer committed to blocks 2 and 3;
/// - evidence: derived block 1 stays empty, blocks 2 and 3 accept their committed transactions
///   without a nonce error, and all three follower block hashes equal the sequencer's.
#[tokio::test(flavor = "multi_thread")]
async fn new_safe_l2_block_ignores_txpool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().with_num_nodes(2).build().await?;
    let follower = nodes.pop().expect("two nodes requested");
    let sequencer = nodes.pop().expect("two nodes requested");

    let genesis_timestamp = head_timestamp(&sequencer)?;

    // Sequencer block 1: empty. This is the block the follower will later derive while its
    // pool is already primed with the transactions of blocks 2 and 3.
    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(genesis_timestamp + 1);
    let block1 = assemble_l2_block(&sequencer, params).await?;
    assert!(
        block1.transactions.is_empty(),
        "sequencer block 1 must be empty for this scenario"
    );
    import_l2_block(&sequencer, block1.clone()).await?;

    // Two transactions from the same sender, committed one per block. Sequential nonces are
    // what make a premature inclusion fatal later: replaying nonce 0 at its real height fails
    // with "nonce too low" once the follower has already consumed it.
    let tx_nonce0 = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .build_signed()?;
    let tx_nonce1 = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .build_signed()?;
    let tx_nonce0_hash = tx_hash_of(&tx_nonce0);
    let tx_nonce1_hash = tx_hash_of(&tx_nonce1);

    // Sequencer block 2 commits nonce 0. Each transaction is injected only when its own block
    // is about to be built, so block 2 cannot take both.
    sequencer.rpc.inject_tx(tx_nonce0.clone()).await?;
    let mut params = AssembleL2BlockParams::empty(2);
    params.timestamp = Some(genesis_timestamp + 2);
    let block2 = assemble_l2_block(&sequencer, params).await?;
    assert_eq!(
        transaction_hashes_of(&block2),
        vec![tx_nonce0_hash],
        "sequencer block 2 must contain exactly nonce 0"
    );
    import_l2_block(&sequencer, block2.clone()).await?;

    // Sequencer block 3 commits nonce 1.
    sequencer.rpc.inject_tx(tx_nonce1.clone()).await?;
    let mut params = AssembleL2BlockParams::empty(3);
    params.timestamp = Some(genesis_timestamp + 3);
    let block3 = assemble_l2_block(&sequencer, params).await?;
    assert_eq!(
        transaction_hashes_of(&block3),
        vec![tx_nonce1_hash],
        "sequencer block 3 must contain exactly nonce 1"
    );
    import_l2_block(&sequencer, block3.clone()).await?;

    // Preload the follower's pool with both committed transactions, standing in for the gossip
    // a real follower receives from the sequencer. Submitting directly keeps the precondition
    // deterministic instead of depending on P2P timing; gossip may already have delivered
    // them, so a duplicate rejection here is not a failure — `wait_until_pooled` is the
    // assertion that matters. The follower never imported blocks 1-3, so nothing has evicted
    // either transaction. This is the precondition that made the original bug fire.
    let _ = follower.rpc.inject_tx(tx_nonce0).await;
    let _ = follower.rpc.inject_tx(tx_nonce1).await;
    wait_until_pooled(&follower, tx_nonce0_hash).await?;
    wait_until_pooled(&follower, tx_nonce1_hash).await?;

    // Derive block 1 on the follower. The pool holds nonce 0 and nonce 1; a builder that reads
    // the pool would pack both here.
    let derived1: MorphHeader = follower
        .auth_server_handle()
        .http_client()
        .request("engine_newSafeL2Block", (safe_data_from(&block1),))
        .await?;

    assert!(
        canonical_block(&follower, 1)?.body.transactions.is_empty(),
        "derived block 1 must stay empty: the follower pool must not leak into a committed block"
    );
    assert_eq!(
        derived1.hash_slow(),
        block1.hash,
        "derived block 1 hash must match the sequencer's"
    );

    // Both transactions must still be replayable at their committed heights. Before the fix
    // this failed here with "nonce 0 too low", because block 1 had already consumed them.
    let derived2: MorphHeader = follower
        .auth_server_handle()
        .http_client()
        .request("engine_newSafeL2Block", (safe_data_from(&block2),))
        .await?;
    assert_eq!(
        derived2.hash_slow(),
        block2.hash,
        "derived block 2 hash must match the sequencer's"
    );
    assert_eq!(
        transaction_hashes(&canonical_block(&follower, 2)?),
        vec![tx_nonce0_hash],
        "derived block 2 must contain exactly the committed nonce 0"
    );

    let derived3: MorphHeader = follower
        .auth_server_handle()
        .http_client()
        .request("engine_newSafeL2Block", (safe_data_from(&block3),))
        .await?;
    assert_eq!(
        derived3.hash_slow(),
        block3.hash,
        "derived block 3 hash must match the sequencer's"
    );
    assert_eq!(
        transaction_hashes(&canonical_block(&follower, 3)?),
        vec![tx_nonce1_hash],
        "derived block 3 must contain exactly the committed nonce 1"
    );

    Ok(())
}

/// A derived block whose transactions exceed its gas limit must be rejected, not truncated.
///
/// Sequencer assembly stops packing when the next transaction does not fit and seals the block
/// with what already fits — correct when the builder chooses the contents. Derivation does not
/// choose: the transaction list is fixed by what was committed to L1. Truncating it there would
/// seal a *different* block, and since `SafeL2Data` carries no expected hash, nothing
/// downstream would notice. go-ethereum reaches the same outcome structurally, because
/// `NewSafeL2Block` executes the block via `BlockChain.ProcessBlock`, whose gas pool returns
/// `ErrGasLimitReached` and fails the whole block.
///
/// Behavior contract:
/// - fault: a `SafeL2Data` whose second transaction cannot fit the block gas limit;
/// - evidence: the call fails and the canonical head does not advance.
#[tokio::test(flavor = "multi_thread")]
async fn new_safe_l2_block_rejects_transactions_over_gas_limit() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let genesis_timestamp = head_timestamp(&node)?;

    let mut params = AssembleL2BlockParams::empty(1);
    params.timestamp = Some(genesis_timestamp + 1);
    let block1 = assemble_l2_block(&node, params).await?;
    let block_gas_limit = block1.gas_limit;
    import_l2_block(&node, block1).await?;
    let head_before = canonical_snapshot(&node)?;

    // First transaction fits; the second one alone exceeds the whole block gas limit, so the
    // pair cannot be executed under it.
    let fits = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_gas_limit(100_000)
        .build_signed()?;
    let overflows = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_gas_limit(block_gas_limit)
        .build_signed()?;

    let safe = SafeL2Data {
        number: 2,
        gas_limit: block_gas_limit,
        base_fee_per_gas: None,
        timestamp: genesis_timestamp + 2,
        transactions: vec![fits, overflows],
        parent_hash: None,
    };

    let result: Result<MorphHeader, _> = node
        .auth_server_handle()
        .http_client()
        .request("engine_newSafeL2Block", (safe,))
        .await;
    assert!(
        result.is_err(),
        "a derived block that cannot fit its committed transactions must be rejected, \
         not silently truncated"
    );

    assert_eq!(
        canonical_snapshot(&node)?,
        head_before,
        "a rejected safe block must leave the canonical chain untouched"
    );

    Ok(())
}
