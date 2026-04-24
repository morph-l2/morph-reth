//! End-to-end verification of retroactive trust across the Jade hardfork
//! boundary.
//!
//! These tests exercise the `MorphBasicEngineValidator` through the real
//! `MorphNode` stack: they spin up an ephemeral node, build a valid block,
//! tamper with the header's `state_root`, and re-import via the Engine API.
//!
//! Retroactive-trust invariants:
//! * Pre-Jade: header state root is NOT compared against the MPT root the
//!   validator computes — a mismatched `state_root` must still import.
//! * Post-Jade: the validator enforces strict MPT equality — the same
//!   tampered `state_root` must be rejected as INVALID.
//!
//! Two existing sibling tests verify these invariants at the crate-of-use
//! level (`crates/node/tests/it/engine.rs::state_root_validation_skipped_pre_jade`
//! and `crates/node/tests/it/consensus.rs::post_jade_state_root_mismatch_is_rejected`).
//! The tests in this file pin the contract to the engine-tree-ext crate: if
//! someone tweaks `MorphBasicEngineValidator` in a way that breaks the
//! boundary, `cargo test -p morph-engine-tree-ext` is expected to catch it.

#![cfg(feature = "test-utils")]

use alloy_consensus::{BlockHeader, proofs};
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use morph_node::test_utils::{HardforkSchedule, MorphTestNode, TestNodeBuilder};
use morph_payload_types::{MorphBuiltPayload, MorphPayloadAttributes, MorphPayloadTypes};
use reth_node_api::PayloadTypes;
use reth_payload_builder::BuildNewPayload;
use reth_payload_primitives::BuiltPayload;
use reth_primitives_traits::SealedBlock;
use reth_provider::BlockReaderIdExt;

/// Build an empty block through the payload builder without submitting it.
///
/// `node.advance_block()` would time out waiting for a non-empty payload since
/// the pool is empty — instead, drive the builder directly with empty L1
/// messages and resolve the payload synchronously via
/// `PayloadKind::WaitForPending`. This avoids fixed sleeps + polling, which
/// were flake-prone on loaded CI runners.
async fn build_candidate_block(node: &mut MorphTestNode) -> eyre::Result<MorphBuiltPayload> {
    const BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
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

    tokio::time::timeout(
        BUILD_TIMEOUT,
        node.inner
            .payload_builder_handle
            .resolve_kind(payload_id, reth_node_api::PayloadKind::WaitForPending),
    )
    .await
    .map_err(|_| eyre::eyre!("payload build timed out after {:?}", BUILD_TIMEOUT))?
    .ok_or_else(|| eyre::eyre!("no payload response for id {payload_id:?}"))?
    .map_err(|e| eyre::eyre!("payload build error: {e}"))
}

/// Tamper with a payload's header and ask the engine to import the result.
///
/// Returns `true` if the engine accepted the block (VALID), `false` otherwise.
async fn try_import_with_tampered_state_root(
    node: &mut MorphTestNode,
    base: &MorphBuiltPayload,
    bogus_state_root: B256,
) -> eyre::Result<bool> {
    let sealed = base.block();
    let morph_header: morph_primitives::MorphHeader = sealed.header().inner.clone().into();
    let body = sealed.body().clone();
    let mut block = morph_primitives::Block::new(morph_header, body);

    block.header.inner.state_root = bogus_state_root;
    block.header.inner.transactions_root =
        proofs::calculate_transaction_root(&block.body.transactions);

    let modified_sealed = SealedBlock::seal_slow(block);
    let execution_data = MorphPayloadTypes::block_to_payload(modified_sealed);
    let status = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(execution_data)
        .await?;

    Ok(status.is_valid())
}

#[tokio::test(flavor = "multi_thread")]
async fn pre_jade_block_with_tampered_state_root_imports() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let base_payload = build_candidate_block(&mut node).await?;
    let accepted =
        try_import_with_tampered_state_root(&mut node, &base_payload, B256::from([0xFF; 32]))
            .await?;

    assert!(
        accepted,
        "pre-Jade block with tampered state_root must be accepted — retroactive trust skips \
         state-root validation before Jade"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn post_jade_block_with_tampered_state_root_is_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::AllActive)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let base_payload = build_candidate_block(&mut node).await?;
    let accepted =
        try_import_with_tampered_state_root(&mut node, &base_payload, B256::from([0xFF; 32]))
            .await?;

    assert!(
        !accepted,
        "post-Jade block with tampered state_root must be rejected — MorphBasicEngineValidator \
         enforces strict MPT root equality after Jade"
    );
    Ok(())
}

/// Regression test: P2P-downloaded blocks enter the engine tree via the
/// Block-input path (`insert_block`), which never invokes
/// `convert_payload_to_block` and therefore registers no withdraw-trie-root
/// expectation. The validator must treat the missing entry as
/// `SkipValidation` so the downloaded block is accepted; otherwise sync
/// stalls indefinitely with `"missing withdraw trie root expectation
/// cache entry"`.
///
/// The test runs two interconnected nodes: node[0] builds and imports a
/// block via the Engine API (Payload path → expectation registered on
/// node[0] only), then node[1] points its forkchoice at the new head so
/// reth's downloader fetches the block from node[0] over P2P. The download
/// lands in node[1]'s engine tree as a `Block` input.
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

    let payload = build_candidate_block(&mut node0).await?;
    let head_hash = payload.block().hash();

    let exec_data = MorphPayloadTypes::block_to_payload(payload.block().clone());
    let status = node0
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(exec_data)
        .await?;
    assert!(
        status.is_valid(),
        "node[0] must accept its own freshly-built block, got {status:?}"
    );
    node0.update_forkchoice(head_hash, head_hash).await?;

    // sync_to keeps issuing FCU until node[1]'s head matches `head_hash`.
    // Since node[1] does not have the block, reth's engine tree will trigger
    // its block downloader against the connected peer (node[0]) — downloaded
    // blocks enter via the Block input path, exercising the bug.
    node1.sync_to(head_hash).await?;

    Ok(())
}
