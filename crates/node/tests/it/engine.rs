//! Engine API behavior integration tests.
//!
//! Verifies engine-level semantics that are distinct from consensus rule
//! enforcement — in particular the state-root validation gating introduced
//! by the Jade hardfork.

use alloy_consensus::{BlockHeader, Sealable};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{HardforkSchedule, TestNodeBuilder};
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphPayloadAttributes, SafeL2Data,
};
use morph_primitives::MorphHeader;
use reth_payload_builder::BuildNewPayload;
use reth_payload_primitives::BuiltPayload;
use reth_provider::BlockReaderIdExt;

use super::helpers::{build_block_no_submit, craft_and_try_import_block};

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

/// `engine_assembleL2BlockV2` builds on an explicitly given parent hash (not the head).
///
/// V2 keys assembly on a parent hash rather than a block number, so the sequencer can
/// build on any parent — including one that is no longer the canonical head. Here a
/// second block 1' is assembled on genesis after block 1 is already canonical, then
/// imported as a reorg. The three params are positional (`parentHash`, `timestamp`,
/// `txs`), with `timestamp` as a bare JSON number, matching go-ethereum's signature.
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

    let empty_txs: Vec<alloy_primitives::Bytes> = Vec::new();

    // Assemble + import block 1 on genesis.
    let block1: ExecutableL2Data = client
        .request(
            "engine_assembleL2BlockV2",
            (genesis_hash, Some(now - 6), empty_txs.clone()),
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
            (genesis_hash, Some(now - 3), empty_txs),
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
        },
        transactions: Some(vec![]),
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
            trie_handle: None,
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
