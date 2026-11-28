//! End-to-end tests for `morph_getTransactionHashesByReference`.
//!
//! The node add-on owns the reference-index runtime. These tests only produce
//! canonical blocks and call the public RPC, so they cover notification wakeups,
//! provider-backed backfill, cursor readiness, and RPC registration together.

use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{B256, U64};
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{
    HardforkSchedule, MorphTestNode, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder,
};
use morph_reference_index::ReferenceTransactionResult;
use morph_rpc::ReferenceQueryArgs;
use reth_payload_primitives::BuiltPayload;
use std::time::Duration;

async fn wait_for_reference_query(
    node: &MorphTestNode,
    args: ReferenceQueryArgs,
) -> eyre::Result<Vec<ReferenceTransactionResult>> {
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_error = None;

    while tokio::time::Instant::now() < deadline {
        match client
            .request("morph_getTransactionHashesByReference", (args.clone(),))
            .await
        {
            Ok(results) => return Ok(results),
            Err(error) => {
                last_error = Some(error.to_string());
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }
    }

    Err(eyre::eyre!(
        "reference index did not become queryable before timeout: {}",
        last_error.as_deref().unwrap_or("no RPC response")
    ))
}

fn query_args(reference: B256, offset: Option<u64>, limit: Option<u64>) -> ReferenceQueryArgs {
    ReferenceQueryArgs {
        reference,
        offset,
        limit,
    }
}

/// Before Jade, the namespace is available and returns an empty result without
/// requiring per-block index writes.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_is_empty_before_jade() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let node = nodes.pop().unwrap();
    assert!(
        !node.inner.data_dir.exex_wal().exists(),
        "reference index must not create an ExEx WAL"
    );
    let results =
        wait_for_reference_query(&node, query_args(B256::with_last_byte(0x98), None, None)).await?;

    assert!(results.is_empty());
    Ok(())
}

/// Produce one block with a reference-carrying MorphTx and verify the live
/// background index returns it through the public RPC.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_finds_single_morph_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let reference = B256::with_last_byte(0x99);
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .unwrap()
        .tx_hash();

    let results = wait_for_reference_query(&node, query_args(reference, None, None)).await?;

    assert_eq!(results.len(), 1, "should find exactly one transaction");
    assert_eq!(results[0].transaction_hash, tx_hash);
    assert_eq!(results[0].transaction_index, U64::from(0u64));

    Ok(())
}

/// Produce multiple blocks with the same reference and verify RPC pagination.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_pagination() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let reference = B256::with_last_byte(0xaa);
    let mut tx_hashes = Vec::new();

    for nonce in 0..3u64 {
        let raw_tx = MorphTxBuilder::new(
            wallet.chain_id,
            wallet.inner.clone(),
            wallet.inner_nonce + nonce,
        )
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .build_signed()?;
        node.rpc.inject_tx(raw_tx).await?;
        let payload = node.advance_block().await?;
        tx_hashes.push(
            *payload
                .block()
                .body()
                .transactions
                .first()
                .unwrap()
                .tx_hash(),
        );
    }

    let page1 = wait_for_reference_query(&node, query_args(reference, Some(0), Some(2))).await?;
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].transaction_hash, tx_hashes[0]);
    assert_eq!(page1[1].transaction_hash, tx_hashes[1]);

    let page2 = wait_for_reference_query(&node, query_args(reference, Some(2), Some(2))).await?;
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].transaction_hash, tx_hashes[2]);

    Ok(())
}

/// A different reference key returns an empty successful result once the index
/// is exactly at the canonical head.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_no_results_for_unrelated_reference() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let reference = B256::with_last_byte(0xbb);
    let other_reference = B256::with_last_byte(0xcc);

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;

    let results = wait_for_reference_query(&node, query_args(other_reference, None, None)).await?;
    assert!(
        results.is_empty(),
        "unrelated reference should return nothing"
    );

    Ok(())
}

/// M-M06: `morph_` RPC methods must NOT be registered on HTTP transports that do not
/// enable the `morph` module (e.g. `RpcModuleSelection::Standard` / `eth,net,web3`).
#[tokio::test(flavor = "multi_thread")]
async fn morph_rpc_not_exposed_when_morph_module_not_configured() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _wallet) = TestNodeBuilder::new()
        .with_http_api(reth_rpc_builder::RpcModuleSelection::Standard)
        .build()
        .await?;
    let node = nodes.pop().unwrap();
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    // Standard `eth_` methods remain available.
    let block_number: U64 = client
        .request("eth_blockNumber", jsonrpsee::rpc_params![])
        .await?;
    assert_eq!(block_number, U64::ZERO);

    // `morph_` namespace is not registered and must return Method Not Found (-32601).
    let err = client
        .request::<Vec<ReferenceTransactionResult>, _>(
            "morph_getTransactionHashesByReference",
            (query_args(B256::with_last_byte(0x42), None, None),),
        )
        .await
        .expect_err("morph_ must not be registered under Standard HTTP modules");
    match err {
        jsonrpsee::core::ClientError::Call(obj) => {
            assert_eq!(
                obj.code(),
                -32601,
                "expected Method Not Found (-32601), got {obj:?}"
            );
        }
        other => panic!("expected JSON-RPC Call error (-32601), got {other:?}"),
    }

    Ok(())
}

/// M-M06: `morph_` RPC methods ARE registered when `morph` is explicitly enabled via
/// `--http.api eth,morph` (`RethRpcModule::Other("morph")`), while unconfigured modules
/// remain disabled.
#[tokio::test(flavor = "multi_thread")]
async fn morph_rpc_exposed_when_morph_module_explicitly_configured() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let selection: reth_rpc_builder::RpcModuleSelection = "eth,morph".parse()?;
    assert!(selection.contains(&reth_rpc_builder::RethRpcModule::Other("morph".into())));

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_http_api(selection)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let reference = B256::with_last_byte(0xdd);
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .unwrap()
        .tx_hash();

    let results = wait_for_reference_query(&node, query_args(reference, None, None)).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].transaction_hash, tx_hash);

    // Confirm unconfigured module (`txpool`) is not accidentally exposed.
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let err = client
        .request::<serde_json::Value, _>("txpool_status", jsonrpsee::rpc_params![])
        .await
        .expect_err("unconfigured txpool module must not be registered");
    match err {
        jsonrpsee::core::ClientError::Call(obj) => {
            assert_eq!(obj.code(), -32601);
        }
        other => panic!("expected JSON-RPC Call error (-32601), got {other:?}"),
    }

    Ok(())
}
