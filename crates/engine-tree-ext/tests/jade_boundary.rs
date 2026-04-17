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
/// messages and poll `best_payload` until it returns.
async fn build_candidate_block(node: &mut MorphTestNode) -> eyre::Result<MorphBuiltPayload> {
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

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(eyre::eyre!("timeout waiting for payload"));
        }
        match node
            .inner
            .payload_builder_handle
            .best_payload(payload_id)
            .await
        {
            Some(Ok(p)) => return Ok(p),
            Some(Err(e)) => return Err(eyre::eyre!("payload build error: {e}")),
            None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
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
