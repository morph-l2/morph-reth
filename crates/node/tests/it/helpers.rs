//! Shared test helper utilities used across integration test modules.

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::PayloadAttributes;
use morph_node::test_utils::MorphTestNode;
use morph_payload_types::{
    MorphBuiltPayload, MorphPayloadAttributes, MorphPayloadBuilderAttributes, MorphPayloadTypes,
};
use reth_e2e_test_utils::wallet::Wallet;
use reth_node_api::PayloadTypes;
use reth_payload_primitives::{BuiltPayload, PayloadBuilderAttributes};
use reth_provider::BlockReaderIdExt;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Wrap a [`Wallet`] in an `Arc<Mutex<>>` for use in `advance_chain`.
pub(crate) fn wallet_to_arc(wallet: Wallet) -> Arc<Mutex<Wallet>> {
    Arc::new(Mutex::new(wallet))
}

/// Advance one block with the given L1 messages injected via custom payload attributes.
///
/// This bypasses the node's default attributes generator and instead creates
/// custom attributes with L1 messages, then submits the block via the engine API.
///
/// L2 transactions already in the pool will also be included after the L1 messages.
///
/// NOTE: Uses direct `resolve_kind` polling instead of the event stream to
/// avoid state leakage between sequential calls in multi-block tests.
pub(crate) async fn advance_block_with_l1_messages(
    node: &mut MorphTestNode,
    l1_messages: Vec<Bytes>,
) -> eyre::Result<MorphBuiltPayload> {
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
        transactions: Some(l1_messages),
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let attrs = MorphPayloadBuilderAttributes::try_new(head_hash, rpc_attrs, 3)
        .map_err(|e| eyre::eyre!("failed to build payload attributes: {e}"))?;

    let payload_id = node
        .inner
        .payload_builder_handle
        .send_new_payload(attrs)
        .await?
        .map_err(|e| eyre::eyre!("payload build failed: {e}"))?;

    // Brief delay before polling to let the payload builder process pool transactions.
    // Without this, the builder might emit its first result before picking up L2 txs.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // Poll until the payload builder has produced a result (or 10s timeout)
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

    // Submit via engine API and wait for canonical head to update
    node.submit_payload(payload.clone()).await?;
    let block_hash = payload.block().hash();
    node.update_forkchoice(block_hash, block_hash).await?;
    // Ensure the canonical head is actually at this block before returning,
    // so the next payload build sees the correct parent.
    node.sync_to(block_hash).await?;

    Ok(payload)
}

/// Build a block with L1 messages but do NOT submit it.
/// Returns the built payload for inspection or modification.
pub(crate) async fn build_block_no_submit(
    node: &mut MorphTestNode,
    l1_messages: Vec<Bytes>,
) -> eyre::Result<MorphBuiltPayload> {
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
        transactions: Some(l1_messages),
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let attrs = MorphPayloadBuilderAttributes::try_new(head_hash, rpc_attrs, 3)
        .map_err(|e| eyre::eyre!("failed to build payload attributes: {e}"))?;

    let payload_id = node
        .inner
        .payload_builder_handle
        .send_new_payload(attrs)
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

/// Craft a block by modifying a valid payload, then try to import it via engine API.
///
/// Returns `true` if the block was accepted (VALID/SYNCING), `false` if rejected (INVALID).
/// The modification function receives a mutable reference to the unsealed block.
///
/// After modification, `transactions_root` is recomputed and the block is re-sealed.
pub(crate) async fn craft_and_try_import_block(
    node: &mut MorphTestNode,
    base_payload: &MorphBuiltPayload,
    modify: impl FnOnce(&mut morph_primitives::Block),
) -> eyre::Result<bool> {
    use alloy_consensus::proofs;
    use reth_primitives_traits::SealedBlock;

    // Extract unsealed block.
    // sealed.header() returns &SealedHeader<MorphHeader>; .inner is the MorphHeader itself.
    let sealed = base_payload.block();
    let morph_header: morph_primitives::MorphHeader = sealed.header().inner.clone().into();
    let body = sealed.body().clone();
    let mut block = morph_primitives::Block::new(morph_header, body);

    // Apply the caller's modification
    modify(&mut block);

    // Recompute transactions_root into the inner alloy Header field
    block.header.inner.transactions_root =
        proofs::calculate_transaction_root(&block.body.transactions);

    // Seal with the new hash (recomputes block hash from header)
    let modified_sealed = SealedBlock::seal_slow(block);

    // Convert to execution payload and try to import
    let execution_data = MorphPayloadTypes::block_to_payload(modified_sealed);
    let status = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(execution_data)
        .await?;

    // Only VALID means the block was fully accepted and executed.
    // SYNCING (unknown parent) or INVALID both count as "not accepted".
    Ok(status.is_valid())
}

/// Try to build a block with the given L1 messages but expect the payload builder to fail.
///
/// Returns `Ok(error_message)` if the builder rejects the payload,
/// `Err(...)` if the builder unexpectedly succeeds.
pub(crate) async fn expect_payload_build_failure(
    node: &mut MorphTestNode,
    l1_messages: Vec<Bytes>,
) -> eyre::Result<String> {
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
        transactions: Some(l1_messages),
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let attrs = MorphPayloadBuilderAttributes::try_new(head_hash, rpc_attrs, 3)
        .map_err(|e| eyre::eyre!("failed to build payload attributes: {e}"))?;

    let payload_id = match node
        .inner
        .payload_builder_handle
        .send_new_payload(attrs)
        .await?
    {
        Ok(id) => id,
        Err(e) => return Ok(e.to_string()),
    };

    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if tokio::time::Instant::now() > deadline {
            return Err(eyre::eyre!(
                "timeout — payload builder neither succeeded nor failed"
            ));
        }
        match node
            .inner
            .payload_builder_handle
            .best_payload(payload_id)
            .await
        {
            Some(Err(e)) => return Ok(e.to_string()),
            Some(Ok(_)) => {
                return Err(eyre::eyre!(
                    "expected payload build failure, but it succeeded"
                ));
            }
            None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    }
}
