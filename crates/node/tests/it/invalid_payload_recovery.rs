//! Invalid Engine payload recovery scenarios.

use alloy_consensus::{BlockHeader, Sealable, transaction::TxHashRef};
use alloy_primitives::B256;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{TestNodeBuilder, make_transfer_tx};
use morph_payload_types::GenericResponse;
use morph_primitives::MorphHeader;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{AccountReader, StateProviderFactory};

use super::helpers::{
    build_block_no_submit, canonical_snapshot, payload_with_receipts_root, wait_for_pool_membership,
};

/// Behavior contract:
/// - fault: the payload commits to a receipts root that execution cannot produce;
/// - evidence: both validation and import reject it, canonical/account state stay
///   unchanged, and the unmodified payload can still be imported afterwards.
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
    wait_for_pool_membership(&node, tx_hash, true).await?;

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

    let client = node.auth_server_handle().http_client();
    let validation: GenericResponse = client
        .request("engine_validateL2Block", (invalid_data.clone(),))
        .await?;
    assert!(
        !validation.success,
        "execution must reject a self-consistent header hash with the wrong receipts root"
    );
    assert_eq!(
        canonical_snapshot(&node)?,
        before,
        "validation must not mutate canonical state"
    );

    let import: Result<MorphHeader, _> =
        client.request("engine_newL2BlockV2", (invalid_data,)).await;
    assert!(
        import.is_err(),
        "newL2BlockV2 must reject the invalid execution result"
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
    wait_for_pool_membership(&node, tx_hash, true).await?;

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
    wait_for_pool_membership(&node, tx_hash, false).await?;

    Ok(())
}
