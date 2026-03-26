//! Engine API behavior integration tests.
//!
//! Verifies engine-level semantics that are distinct from consensus rule
//! enforcement — in particular the state-root validation gating introduced
//! by the Jade hardfork.

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256};
use alloy_rpc_types_engine::PayloadAttributes;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{HardforkSchedule, TestNodeBuilder};
use morph_payload_types::{
    AssembleL2BlockParams, ExecutableL2Data, GenericResponse, MorphPayloadAttributes,
    MorphPayloadBuilderAttributes,
};
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes};
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

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new()
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

/// `engine_newL2Block` can import a block assembled over the authenticated RPC.
#[tokio::test(flavor = "multi_thread")]
async fn new_l2_block_imports_assembled_block_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
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

    Ok(())
}

/// `engine_validateL2Block` rejects a tampered block hash over authenticated RPC.
#[tokio::test(flavor = "multi_thread")]
async fn validate_l2_block_rejects_tampered_hash_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?;
    let (head_hash, head_ts) = head
        .map(|h| (h.hash(), h.timestamp()))
        .unwrap_or((B256::ZERO, 0));

    let attrs = MorphPayloadBuilderAttributes::try_new(
        head_hash,
        MorphPayloadAttributes {
            inner: PayloadAttributes {
                timestamp: head_ts + 1,
                prev_randao: B256::repeat_byte(0xAA),
                suggested_fee_recipient: Address::ZERO,
                withdrawals: Some(vec![]),
                parent_beacon_block_root: Some(B256::ZERO),
            },
            transactions: Some(vec![]),
            gas_limit: None,
            base_fee_per_gas: None,
        },
        3,
    )?;

    let payload_id = node
        .inner
        .payload_builder_handle
        .send_new_payload(attrs)
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
