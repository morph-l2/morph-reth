//! Shared test helper utilities used across integration test modules.

use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256, Bytes};
use alloy_rpc_types_engine::PayloadAttributes;
use morph_node::test_utils::MorphTestNode;
use morph_payload_types::{
    ExecutableL2Data, MorphBuiltPayload, MorphPayloadAttributes, MorphPayloadTypes,
};
use morph_reference_index::ReferenceTransactionResult;
use morph_rpc::ReferenceQueryArgs;
use reth_e2e_test_utils::wallet::Wallet;
use reth_node_api::PayloadTypes;
use reth_payload_builder::BuildNewPayload;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{BlockIdReader, BlockReaderIdExt};
use reth_transaction_pool::TransactionPool;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// Observable canonical-chain state used to prove a rejected payload had no side effects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CanonicalSnapshot {
    pub(crate) hash: B256,
    pub(crate) number: u64,
    pub(crate) state_root: B256,
    pub(crate) next_l1_message_index: u64,
    pub(crate) safe_hash: Option<B256>,
}

/// Wrap a [`Wallet`] in an `Arc<Mutex<>>` for use in `advance_chain`.
pub(crate) fn wallet_to_arc(wallet: Wallet) -> Arc<Mutex<Wallet>> {
    Arc::new(Mutex::new(wallet))
}

/// Read canonical state only through node interfaces visible to scenario tests.
pub(crate) fn canonical_snapshot(node: &MorphTestNode) -> eyre::Result<CanonicalSnapshot> {
    let latest = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .ok_or_else(|| eyre::eyre!("canonical head is missing"))?;

    Ok(CanonicalSnapshot {
        hash: latest.hash(),
        number: latest.number(),
        state_root: latest.state_root(),
        next_l1_message_index: latest.next_l1_msg_index,
        safe_hash: node.inner.provider.safe_block_hash()?,
    })
}

/// Wait until a transaction is present in or absent from the pool.
pub(crate) async fn wait_for_pool_membership(
    node: &MorphTestNode,
    tx_hash: B256,
    expected: bool,
) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let present = node.inner.pool.contains(&tx_hash);
        if present == expected {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "transaction {tx_hash} pool membership stayed {present}, expected {expected}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Drive a connected follower to a block without marking every observed tip
/// safe or finalized.
///
/// Upstream `sync_to` finalizes its target, which makes a later sibling reorg an
/// invalid test setup and causes reorg-aware indexes to require manual repair.
pub(crate) async fn sync_optimistically(node: &MorphTestNode, target: B256) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        if canonical_snapshot(node)?.hash == target {
            tokio::time::sleep(Duration::from_millis(100)).await;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "node did not optimistically sync to {target} before timeout"
            ));
        }
        node.update_optimistic_forkchoice(target).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Query the public reference-index RPC, retrying while the index catches up.
pub(crate) async fn wait_for_reference_query(
    node: &MorphTestNode,
    args: ReferenceQueryArgs,
    ready: impl Fn(&[ReferenceTransactionResult]) -> bool,
) -> eyre::Result<Vec<ReferenceTransactionResult>> {
    use jsonrpsee::core::client::ClientT;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_observation = String::from("no RPC response");

    while tokio::time::Instant::now() < deadline {
        match client
            .request::<Vec<ReferenceTransactionResult>, _>(
                "morph_getTransactionHashesByReference",
                (args.clone(),),
            )
            .await
        {
            Ok(results) if ready(&results) => return Ok(results),
            Ok(results) => last_observation = format!("results={results:?}"),
            Err(error) => {
                last_observation = error.to_string();
                if last_observation.contains("reference index is unavailable") {
                    return Err(eyre::eyre!(last_observation));
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    Err(eyre::eyre!(
        "reference index did not reach the expected state before timeout: {last_observation}"
    ))
}

pub(crate) const fn reference_query(reference: B256) -> ReferenceQueryArgs {
    ReferenceQueryArgs {
        reference,
        offset: None,
        limit: None,
    }
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
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(l1_messages),
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
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(l1_messages),
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

    let sealed = base_payload.block();
    let morph_header: morph_primitives::MorphHeader = sealed.header().inner.clone().into();
    let body = sealed.body().clone();
    let mut block = morph_primitives::Block::new(morph_header, body);

    modify(&mut block);
    block.header.inner.transactions_root =
        proofs::calculate_transaction_root(&block.body.transactions);

    let modified_sealed = SealedBlock::seal_slow(block);
    let execution_data = MorphPayloadTypes::block_to_payload(modified_sealed, None);
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

/// Return Engine RPC data whose receipts root and block hash agree with each
/// other but disagree with the actual execution result.
///
/// This avoids accidentally testing only the earlier cached-hash check.
pub(crate) fn payload_with_receipts_root(
    base_payload: &MorphBuiltPayload,
    receipts_root: B256,
) -> ExecutableL2Data {
    use alloy_consensus::Sealable;

    let mut header: morph_primitives::MorphHeader =
        base_payload.block().header().inner.clone().into();
    header.inner.receipts_root = receipts_root;

    let mut data = base_payload.executable_data().clone();
    data.receipts_root = receipts_root;
    data.hash = header.hash_slow();
    data
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
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(l1_messages),
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let payload_id = match node
        .inner
        .payload_builder_handle
        .send_new_payload(BuildNewPayload {
            attributes: rpc_attrs,
            parent_hash: head_hash,
            cache: None,
            state_root_handle: None,
        })
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
