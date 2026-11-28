//! Basic RPC response verification tests.
//!
//! Ensures that the node's JSON-RPC interface returns correct data
//! for common eth_ namespace methods after blocks have been produced.

use alloy_consensus::{BlockHeader, SignableTransaction, TxLegacy, transaction::TxHashRef};
use alloy_eips::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, Sealable, TxKind, U256};
use alloy_signer::SignerSync;
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{
    HardforkSchedule, MorphTestNode, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder, advance_chain,
    make_transfer_tx, sign_authorization, wallet_at_index,
};
use morph_primitives::MorphTxEnvelope;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{
    AccountReader, BlockReader, BlockReaderIdExt, HeaderProvider, StateProviderFactory,
    TransactionsProvider,
};
use serde_json::Value;

use super::helpers::wallet_to_arc;

/// Block number advances correctly after producing blocks.
#[tokio::test(flavor = "multi_thread")]
async fn block_number_advances_correctly() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    // Before any blocks: genesis is block 0
    let number_before = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .map(|h| h.number())
        .unwrap_or(0);
    assert_eq!(number_before, 0);

    // Advance 3 blocks
    advance_chain(3, &mut node, wallet).await?;

    let number_after = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .map(|h| h.number())
        .unwrap_or(0);
    assert_eq!(number_after, 3);

    Ok(())
}

/// Block hash returned by the payload builder matches what's stored in the DB.
#[tokio::test(flavor = "multi_thread")]
async fn block_hash_consistent_with_storage() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(3, &mut node, wallet).await?;

    for (i, payload) in payloads.iter().enumerate() {
        let expected_hash = payload.block().hash();
        let block_num = (i + 1) as u64;

        let header = node
            .inner
            .provider
            .header_by_number(block_num)?
            .expect("header should be stored");

        // Verify the stored header, when hashed, matches what the payload builder returned
        assert_eq!(
            header.hash_slow(),
            expected_hash,
            "block {block_num}: stored hash does not match payload hash"
        );
    }

    Ok(())
}

/// Each produced block contains the expected number of transactions.
#[tokio::test(flavor = "multi_thread")]
async fn block_transaction_count_correct() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(3, &mut node, wallet).await?;

    for (i, payload) in payloads.iter().enumerate() {
        let block_num = (i + 1) as u64;

        let stored_block = node
            .inner
            .provider
            .block_by_number(block_num)?
            .expect("block should be stored");

        // Block from provider must have the same tx count as from payload builder
        assert_eq!(
            stored_block.body.transactions.len(),
            payload.block().body().transactions.len(),
            "block {block_num}: tx count mismatch between payload and stored block"
        );
        assert_eq!(
            stored_block.body.transactions.len(),
            1,
            "each advance_chain block should have 1 tx"
        );
    }

    Ok(())
}

/// Transactions are retrievable by hash after block import.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_retrievable_by_hash() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(1, &mut node, wallet).await?;
    let block = payloads[0].block();

    let tx = block
        .body()
        .transactions
        .first()
        .expect("block should have a tx");
    let tx_hash = *tx.tx_hash();

    // Retrieve via provider
    let fetched = node
        .inner
        .provider
        .transaction_by_hash(tx_hash)?
        .expect("tx should be retrievable by hash");

    assert_eq!(*fetched.tx_hash(), tx_hash);

    Ok(())
}

/// Block gas_used reflects the actual execution cost of transactions.
#[tokio::test(flavor = "multi_thread")]
async fn block_gas_used_reflects_execution() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let payloads = advance_chain(1, &mut node, wallet).await?;
    let block = payloads[0].block();

    // A simple EIP-1559 transfer uses exactly 21,000 gas
    assert_eq!(
        block.header().inner.gas_used,
        21_000,
        "simple transfer should use exactly 21,000 gas"
    );

    Ok(())
}

/// ETH balance decreases after a transfer transaction.
#[tokio::test(flavor = "multi_thread")]
async fn balance_decreases_after_eth_transfer() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    // Get balance before
    let state_before = node.inner.provider.latest()?;
    let sender = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let bal_before = state_before
        .basic_account(&sender)?
        .map(|a| a.balance)
        .unwrap_or_default();

    advance_chain(1, &mut node, wallet).await?;

    let state_after = node.inner.provider.latest()?;
    let bal_after = state_after
        .basic_account(&sender)?
        .map(|a| a.balance)
        .unwrap_or_default();

    assert!(
        bal_after < bal_before,
        "balance should decrease after transfer (gas + value spent)"
    );
    Ok(())
}

/// Nonce increments by 1 after a successful transaction.
#[tokio::test(flavor = "multi_thread")]
async fn nonce_increments_after_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    let sender = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

    let state_before = node.inner.provider.latest()?;
    let nonce_before = state_before
        .basic_account(&sender)?
        .map(|a| a.nonce)
        .unwrap_or(0);
    assert_eq!(nonce_before, 0, "nonce should start at 0");

    advance_chain(1, &mut node, wallet).await?;

    let state_after = node.inner.provider.latest()?;
    let nonce_after = state_after
        .basic_account(&sender)?
        .map(|a| a.nonce)
        .unwrap_or(0);
    assert_eq!(nonce_after, 1, "nonce should be 1 after one tx");
    Ok(())
}

/// `eth_getTransactionReceipt` exposes Morph-specific receipt fields over JSON-RPC.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_receipt_exposes_morph_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let reference = B256::with_last_byte(0x44);
    let memo = alloy_primitives::Bytes::from_static(b"invoice-42");
    let expected_reference = reference.to_string();
    let expected_memo = memo.to_string();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .with_memo(memo)
        .with_data(vec![0xaa; 16])
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
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let receipt: Value = client
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;

    assert_eq!(receipt["type"].as_str(), Some("0x7f"));
    // version is JSON-RPC quantity-encoded (string "0x1"), not a number.
    assert_eq!(receipt["version"].as_str(), Some("0x1"));
    assert_eq!(receipt["feeTokenID"].as_str(), Some("0x1"));
    assert_eq!(
        receipt["reference"].as_str(),
        Some(expected_reference.as_str())
    );
    assert_eq!(receipt["memo"].as_str(), Some(expected_memo.as_str()));
    assert!(
        receipt["feeRate"].as_str().is_some(),
        "feeRate should be serialized for token-fee MorphTx receipts"
    );
    assert!(
        receipt["tokenScale"].as_str().is_some(),
        "tokenScale should be serialized for token-fee MorphTx receipts"
    );
    assert!(
        receipt["feeLimit"].as_str().is_some(),
        "feeLimit should be serialized for token-fee MorphTx receipts"
    );
    assert!(
        receipt["l1Fee"]
            .as_str()
            .is_some_and(|value| value != "0x0"),
        "l1Fee should be serialized as a non-zero quantity for calldata txs"
    );

    Ok(())
}

/// MorphTx v1 pays directly in ETH when `feeTokenID` is zero, so no token
/// exchange rate or scale applies to its receipt.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_eth_receipt_omits_token_exchange_metadata() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
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
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let receipt: Value = client
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;

    assert_eq!(receipt["type"].as_str(), Some("0x7f"));
    assert_eq!(receipt["version"].as_str(), Some("0x1"));
    assert_eq!(receipt["feeTokenID"].as_str(), Some("0x0"));
    assert_eq!(receipt["feeLimit"].as_str(), Some("0x0"));
    assert!(receipt["feeRate"].is_null());
    assert!(receipt["tokenScale"].is_null());

    Ok(())
}

/// Standard (non-MorphTx) receipts must keep Morph fee pointers as JSON null.
///
/// `@morph-network/viem` and indexers treat `feeTokenID != null` as "this is an
/// alt-fee transaction". Filling `"0x0"` would misclassify every EIP-1559 tx.
#[tokio::test(flavor = "multi_thread")]
async fn standard_transaction_receipt_exposes_null_fee_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .unwrap()
        .tx_hash();
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let receipt: Value = client
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;

    assert_ne!(receipt["type"].as_str(), Some("0x7f"));
    // geth's version field is a value type, so unset receipts still emit "0x0".
    assert_eq!(receipt["version"].as_str(), Some("0x0"));
    for field in ["feeTokenID", "feeRate", "tokenScale", "feeLimit"] {
        assert!(
            receipt[field].is_null(),
            "{field} must be JSON null for non-MorphTx receipts, got {}",
            receipt[field]
        );
    }
    assert!(receipt["reference"].is_null());
    assert!(receipt["memo"].is_null());

    Ok(())
}

/// `eth_getBlockReceipts` uses the same Morph receipt converter as per-tx receipt RPCs.
#[tokio::test(flavor = "multi_thread")]
async fn block_receipts_expose_morph_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let reference = B256::with_last_byte(0x66);
    let memo = alloy_primitives::Bytes::from_static(b"block-receipts");
    let expected_reference = reference.to_string();
    let expected_memo = memo.to_string();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .with_memo(memo)
        .with_data(vec![0xbb; 16])
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let block_number = payload.block().number();
    let block_number_param = format!("0x{block_number:x}");
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .unwrap()
        .tx_hash();
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let tx_receipt: Value = client
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;
    let block_receipts: Value = client
        .request("eth_getBlockReceipts", (block_number_param,))
        .await?;
    let block_receipt = block_receipts
        .as_array()
        .and_then(|receipts| receipts.first())
        .ok_or_else(|| eyre::eyre!("expected a block receipt"))?;

    for field in [
        "type",
        "version",
        "feeTokenID",
        "feeRate",
        "tokenScale",
        "feeLimit",
        "reference",
        "memo",
        "l1Fee",
    ] {
        assert_eq!(
            block_receipt[field], tx_receipt[field],
            "block receipt field {field} must match eth_getTransactionReceipt"
        );
    }
    assert_eq!(block_receipt["type"].as_str(), Some("0x7f"));
    assert_eq!(
        block_receipt["reference"].as_str(),
        Some(expected_reference.as_str())
    );
    assert_eq!(block_receipt["memo"].as_str(), Some(expected_memo.as_str()));

    Ok(())
}

/// `eth_getTransactionByHash` exposes MorphTx reference and memo over JSON-RPC.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_by_hash_exposes_morph_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let reference = B256::with_last_byte(0x55);
    let memo = alloy_primitives::Bytes::from_static(b"memo-check");
    let expected_reference = reference.to_string();
    let expected_memo = memo.to_string();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .with_memo(memo)
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
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let tx: Value = client
        .request("eth_getTransactionByHash", (tx_hash,))
        .await?;

    assert_eq!(tx["hash"].as_str(), Some(tx_hash.to_string().as_str()));
    assert_eq!(tx["type"].as_str(), Some("0x7f"));
    // version is JSON-RPC quantity-encoded (string "0x1"), not a number.
    assert_eq!(tx["version"].as_str(), Some("0x1"));
    assert_eq!(tx["feeTokenID"].as_str(), Some("0x1"));
    assert!(tx["feeLimit"].as_str().is_some());
    assert_eq!(tx["reference"].as_str(), Some(expected_reference.as_str()));
    assert_eq!(tx["memo"].as_str(), Some(expected_memo.as_str()));

    Ok(())
}

/// `eth_getTransactionByHash` exposes the MorphTx v2 authorization list with the
/// same tuple shape as an EIP-7702 transaction.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_by_hash_exposes_authorization_list_for_morph_tx_v2() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    let authority_signer = wallet_at_index(1, chain_id);
    let delegate = Address::with_last_byte(0x42);
    let authorization = sign_authorization(&authority_signer, chain_id, delegate, 0)?;
    let expected_tuple = serde_json::to_value(&authorization)?;

    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_token_fee(TEST_TOKEN_ID)
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
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
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let tx: Value = client
        .request("eth_getTransactionByHash", (tx_hash,))
        .await?;

    assert_eq!(tx["type"].as_str(), Some("0x7f"));
    assert_eq!(tx["version"].as_str(), Some("0x2"));
    assert_eq!(tx["feeTokenID"].as_str(), Some("0x1"));
    let list = tx["authorizationList"]
        .as_array()
        .expect("MorphTx v2 must expose authorizationList");
    assert_eq!(list.len(), 1);
    for key in ["chainId", "address", "nonce", "yParity", "r", "s"] {
        assert_eq!(
            list[0][key], expected_tuple[key],
            "authorization tuple `{key}` must match the 0x04 JSON shape"
        );
    }

    // Receipt shape is unchanged: only `version` moves to 0x2.
    let receipt: Value = client
        .request("eth_getTransactionReceipt", (tx_hash,))
        .await?;
    assert_eq!(receipt["type"].as_str(), Some("0x7f"));
    assert_eq!(receipt["version"].as_str(), Some("0x2"));
    assert!(receipt.get("authorizationList").is_none());

    Ok(())
}

/// `eth_estimateGas` for a MorphTx v2 request executes with the authorization
/// list, so the estimate covers the 25 000 gas per authorization on top of the
/// plain-call cost. The version is never part of the request: a memo makes the
/// request a MorphTx (V1), the list raises it to V2, and a legacy `version`
/// key changes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_for_morph_tx_v2_includes_authorization_gas() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();

    // Produce a block so the L1 gas oracle state (genesis alloc) is live.
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let authority_signer = wallet_at_index(1, chain_id);
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        0,
    )?;

    let base_request = serde_json::json!({
        "from": sender,
        "to": Address::with_last_byte(0x99),
        "value": "0x0",
        "maxFeePerGas": "0x4a817c800",
        "maxPriorityFeePerGas": "0x4a817c800",
        "memo": "0x6d",
    });

    let v1_request = base_request.clone();
    let v1_estimate: alloy_primitives::U64 = client
        .request("eth_estimateGas", (v1_request.clone(),))
        .await?;

    // A legacy `version` key is ignored: same request, same estimate.
    let mut legacy_request = v1_request;
    legacy_request["version"] = serde_json::json!("0x1");
    let legacy_estimate: alloy_primitives::U64 =
        client.request("eth_estimateGas", (legacy_request,)).await?;
    assert_eq!(legacy_estimate, v1_estimate);

    let mut v2_request = base_request;
    v2_request["authorizationList"] = serde_json::json!([serde_json::to_value(&authorization)?]);
    let v2_estimate: alloy_primitives::U64 = client
        .request("eth_estimateGas", (v2_request.clone(),))
        .await?;

    assert!(
        v1_estimate.to::<u64>() >= 21_000,
        "v1 estimate: {v1_estimate}"
    );
    assert!(
        v2_estimate.to::<u64>() >= v1_estimate.to::<u64>() + 25_000,
        "v2 estimate {v2_estimate} must add the per-authorization intrinsic gas over v1 {v1_estimate}"
    );

    // `eth_call` takes the same V2 shape (fee charge disabled, list still applied).
    let call_result: Value = client
        .request("eth_call", (v2_request.clone(), "latest"))
        .await?;
    assert_eq!(call_result.as_str(), Some("0x"));

    // Without authorizations (`[]` or no key at all) the request is a v1 again,
    // so it costs exactly what the v1 estimate costs.
    let mut empty_v2_request = v2_request;
    empty_v2_request["authorizationList"] = serde_json::json!([]);
    let empty_v2_estimate: alloy_primitives::U64 = client
        .request("eth_estimateGas", (empty_v2_request.clone(),))
        .await?;
    assert_eq!(
        empty_v2_estimate, v1_estimate,
        "an empty authorization list must cost the same gas as v1"
    );
    empty_v2_request
        .as_object_mut()
        .unwrap()
        .remove("authorizationList");
    let absent_list_estimate: alloy_primitives::U64 = client
        .request("eth_estimateGas", (empty_v2_request.clone(),))
        .await?;
    assert_eq!(absent_list_estimate, v1_estimate);
    let call_result: Value = client
        .request("eth_call", (empty_v2_request, "latest"))
        .await?;
    assert_eq!(call_result.as_str(), Some("0x"));

    Ok(())
}

/// Simulation is not fork-gated, exactly like V1 (geth only gates
/// `setDefaults`, i.e. the send paths): before Celadon `eth_estimateGas` and
/// `eth_call` still simulate a V2 request, while sending the same transaction
/// is rejected by the pool.
#[tokio::test(flavor = "multi_thread")]
async fn simulation_of_morph_tx_v2_is_not_fork_gated_before_celadon() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreCeladon)
        .build()
        .await?;
    let node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let authority_signer = wallet_at_index(1, chain_id);
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        0,
    )?;

    let request = serde_json::json!({
        "from": sender,
        "to": Address::with_last_byte(0x99),
        "value": "0x0",
        "maxFeePerGas": "0x4a817c800",
        "maxPriorityFeePerGas": "0x4a817c800",
        "memo": "0x6d",
        "authorizationList": [serde_json::to_value(&authorization)?],
    });
    let estimate: alloy_primitives::U64 = client
        .request("eth_estimateGas", (request.clone(),))
        .await?;
    assert!(
        estimate.to::<u64>() >= 21_000 + 25_000,
        "pre-Celadon estimate must still price the authorization: {estimate}"
    );
    let call_result: Value = client.request("eth_call", (request, "latest")).await?;
    assert_eq!(call_result.as_str(), Some("0x"));

    // Sending the same transaction is where the fork gate lives.
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;
    let err = node
        .rpc
        .inject_tx(raw_tx)
        .await
        .expect_err("MorphTx v2 must be rejected by the pool before Celadon");
    assert!(
        err.to_string().contains("not yet active"),
        "unexpected error: {err}"
    );

    Ok(())
}

/// `eth_call` applies a self-delegating V2 authorization list before the call,
/// with the same nonce rule as real execution: the sender's nonce is bumped
/// first, so the tuple must carry `state nonce + 1`. Calling the sender itself
/// then executes the delegate's code; a tuple signed with the current state
/// nonce is skipped and the call hits an EOA (empty return).
#[tokio::test(flavor = "multi_thread")]
async fn eth_call_applies_self_delegation_for_morph_tx_v2() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use super::helpers::{RETURN_WORD_42_RUNTIME, init_code_for};
    use morph_node::test_utils::make_deploy_tx;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();

    // Block 1: deploy the returning delegate; the sender's on-chain nonce is now 1.
    let deploy_tx = make_deploy_tx(
        chain_id,
        wallet.inner.clone(),
        0,
        init_code_for(RETURN_WORD_42_RUNTIME),
    )?;
    node.rpc.inject_tx(deploy_tx).await?;
    node.advance_block().await?;
    let delegate = Address::create(&sender, 0);

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let call_with_auth_nonce = |auth_nonce: u64| -> eyre::Result<serde_json::Value> {
        let authorization = sign_authorization(&wallet.inner, chain_id, delegate, auth_nonce)?;
        Ok(serde_json::json!({
            "from": sender,
            "to": sender,
            "nonce": "0x1",
            "memo": "0x6d",
            "maxFeePerGas": "0x4a817c800",
            "maxPriorityFeePerGas": "0x4a817c800",
            "authorizationList": [serde_json::to_value(&authorization)?],
        }))
    };

    // state nonce 1 → bumped to 2 before the list is applied → tuple nonce 2 applies.
    let result: Value = client
        .request("eth_call", (call_with_auth_nonce(2)?, "latest"))
        .await?;
    assert_eq!(
        result.as_str(),
        Some(format!("0x{:064x}", 0x42).as_str()),
        "the call must execute the delegate's code via the sender"
    );

    // A tuple signed with the un-bumped nonce is skipped: the sender stays an EOA.
    let result: Value = client
        .request("eth_call", (call_with_auth_nonce(1)?, "latest"))
        .await?;
    assert_eq!(result.as_str(), Some("0x"));

    // Nothing leaked from the simulation into the canonical state.
    let state = node.inner.provider.latest()?;
    assert!(
        state
            .account_code(&sender)?
            .is_none_or(|code| code.is_empty())
    );

    Ok(())
}

/// Produces a simple one-transaction block on the standard Jade profile and returns the
/// node and identifiers needed by the replay-based debug / trace RPCs.
async fn build_standard_jade_block_for_debug_trace() -> eyre::Result<(MorphTestNode, B256, B256)> {
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let tx = TxLegacy {
        chain_id: Some(wallet.chain_id),
        nonce: 0,
        gas_limit: 21_000,
        gas_price: 20_000_000_000u128,
        to: TxKind::Call(Address::with_last_byte(0x42)),
        value: U256::from(100),
        input: Bytes::new(),
    };
    let sig = wallet
        .inner
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let raw_tx: Bytes = MorphTxEnvelope::Legacy(tx.into_signed(sig))
        .encoded_2718()
        .into();
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .expect("produced block should contain the submitted tx")
        .tx_hash();
    let block_hash = payload.block().hash();

    Ok((node, tx_hash, block_hash))
}

/// Comprehensive test: debug + trace replay APIs on a standard Jade block with Cancun active.
///
/// Uses internal APIs (debug_api / trace_api) directly via `node.rpc.inner`.
/// This avoids HTTP serialization overhead
/// and the TaskManager lifetime pitfalls of the HTTP path.
#[tokio::test(flavor = "multi_thread")]
async fn debug_trace_replay_apis_work_for_standard_jade_block() -> eyre::Result<()> {
    use alloy_rpc_types_eth::TransactionRequest;
    use morph_rpc::MorphTransactionRequest;

    reth_tracing::init_test_tracing();

    let (node, tx_hash, block_hash) = build_standard_jade_block_for_debug_trace().await?;

    // Verify parent_beacon_block_root is None (Morph L2 does not use beacon chain)
    let block = node
        .inner
        .provider
        .block_by_hash(block_hash)?
        .expect("block should exist");
    assert!(
        block.header.inner.parent_beacon_block_root.is_none(),
        "Morph L2 blocks must not carry parentBeaconBlockRoot"
    );

    // ----------------------------------------------------------------
    // debug_traceTransaction (default structLogs tracer)
    // ----------------------------------------------------------------
    node.rpc
        .inner
        .debug_api()
        .debug_trace_transaction(tx_hash, Default::default())
        .await?;

    // ----------------------------------------------------------------
    // debug_traceBlock by hash and by number
    // ----------------------------------------------------------------
    let traces_by_hash = node
        .rpc
        .inner
        .debug_api()
        .debug_trace_block(block_hash.into(), Default::default())
        .await?;
    assert_eq!(
        traces_by_hash.len(),
        1,
        "block should contain exactly one tx trace"
    );

    let traces_by_number = node
        .rpc
        .inner
        .debug_api()
        .debug_trace_block(1u64.into(), Default::default())
        .await?;
    assert_eq!(traces_by_number.len(), 1);

    // ----------------------------------------------------------------
    // trace_transaction (parity-style)
    // ----------------------------------------------------------------
    let parity_traces = node
        .rpc
        .inner
        .trace_api()
        .trace_transaction(tx_hash)
        .await?;
    assert!(
        parity_traces.is_some_and(|t| !t.is_empty()),
        "trace_transaction should return non-empty traces"
    );

    // ----------------------------------------------------------------
    // trace_block (parity-style)
    // ----------------------------------------------------------------
    let block_traces = node
        .rpc
        .inner
        .trace_api()
        .trace_block(block_hash.into())
        .await?;
    assert!(
        block_traces.is_some_and(|t| !t.is_empty()),
        "trace_block should return non-empty traces"
    );

    // ----------------------------------------------------------------
    // debug_traceCall
    // ----------------------------------------------------------------
    let call = MorphTransactionRequest::from(TransactionRequest {
        from: Some(Address::with_last_byte(0x01)),
        to: Some(Address::with_last_byte(0x42).into()),
        gas: Some(21_000),
        gas_price: Some(20_000_000_000),
        value: Some(U256::ZERO),
        ..Default::default()
    });
    node.rpc
        .inner
        .debug_api()
        .debug_trace_call(call, Some(block_hash.into()), Default::default())
        .await?;

    Ok(())
}

/// `eth_estimateGas` rejects a request whose sender cannot cover the L1 data fee.
///
/// Exercises the `MorphEthApi::caller_gas_allowance` override. In reth v2.0.0
/// the upstream `estimate_gas_with` sets `cfg_env.disable_fee_charge = true`,
/// so the EVM handler short-circuits and never checks balance — the L1 fee
/// cap must instead be enforced in the gas-allowance pre-check.
///
/// Setup:
/// - An unfunded random sender (`balance = 0`).
/// - A zero-value transfer with a non-zero `gasPrice` so the request goes
///   through the balance-based allowance cap.
///
/// Expected:
/// - The RPC returns an error whose message contains
///   `"insufficient funds for l1 fee"` (matches go-ethereum's error string
///   for this case).
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_reports_insufficient_funds_for_l1_fee() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Produce a block so the L1 gas oracle state (genesis alloc) is live
    // and `caller_gas_allowance` can read the Curie slots.
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    // Unfunded random sender.
    let poor_sender = Address::random();
    let recipient = Address::random();

    let params = (serde_json::json!({
        "from": poor_sender,
        "to": recipient,
        "value": "0x0",
        "gasPrice": "0x3b9aca00", // 1 Gwei — ensures the balance-based cap runs
    }),);

    let result: Result<Value, _> = client.request("eth_estimateGas", params).await;

    let err =
        result.expect_err("eth_estimateGas must fail for a sender that cannot cover the L1 fee");
    let err_str = err.to_string();
    assert!(
        err_str.contains("insufficient funds for l1 fee"),
        "expected 'insufficient funds for l1 fee', got: {err_str}"
    );

    Ok(())
}

/// `eth_call` does NOT reject an unfunded sender on L1-fee grounds.
///
/// This is the companion to `estimate_gas_reports_insufficient_funds_for_l1_fee`
/// — same caller/scenario, but invoked via `eth_call` instead of
/// `eth_estimateGas`. morph-geth's `DoCall` uses
/// `ApplyMessage(..., Big0)`, so L1 fee is not deducted for this RPC
/// path; our override must stay out of `eth_call`'s way.
///
/// Setup:
/// - An unfunded random sender (`balance = 0`).
/// - `eth_call` with non-zero `gasPrice`, without explicit `gas` — the
///   path that routes through `Call::caller_gas_allowance`.
///
/// Expected:
/// - The call succeeds (empty output is fine; we only check that no
///   `"insufficient funds"` error is returned).
#[tokio::test(flavor = "multi_thread")]
async fn eth_call_does_not_reject_unfunded_sender_on_l1_fee() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let poor_sender = Address::random();
    let recipient = Address::random();

    let params = (
        serde_json::json!({
            "from": poor_sender,
            "to": recipient,
            "value": "0x0",
            "gasPrice": "0x3b9aca00",
        }),
        "latest",
    );

    let result: Result<Value, _> = client.request("eth_call", params).await;

    // eth_call may surface a revert error for other reasons, but it must
    // never bubble up our L1-fee / transfer affordability errors.
    if let Err(err) = &result {
        let err_str = err.to_string();
        assert!(
            !err_str.contains("insufficient funds"),
            "eth_call must not reject on L1-fee grounds: {err_str}"
        );
    }

    Ok(())
}

/// MorphTx token-fee `eth_call` does NOT reject a zero-ETH sender on
/// `(ETH_balance − value) / gas_price` grounds.
///
/// Guards against a regression where the shared
/// `Call::caller_gas_allowance` hook short-circuits to upstream's
/// ETH-based allowance for `eth_call` / `createAccessList`. For a
/// MorphTx paying fees in a token, the caller can legitimately hold
/// zero ETH — gas and L1 fee come out of the fee token. Applying the
/// upstream ETH cap would set the allowance to 0 (or reject outright)
/// and break token-fee `eth_call` / `createAccessList`.
///
/// Setup:
/// - An unfunded random sender (`balance = 0`).
/// - `eth_call` with `feeTokenID = 1`, non-zero `gasPrice`, no explicit
///   `gas` — the path that routes through `caller_gas_allowance`.
///
/// Expected:
/// - The call must not surface our `insufficient funds for transfer`
///   or `insufficient funds for l1 fee` errors (it may return a
///   pass-through EVM/handler error such as "invalid fee token" if the
///   test genesis doesn't have token 1 wired up, which is out of scope).
#[tokio::test(flavor = "multi_thread")]
async fn eth_call_token_fee_does_not_reject_zero_eth_sender() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let poor_sender = Address::random();
    let recipient = Address::random();

    let params = (
        serde_json::json!({
            "from": poor_sender,
            "to": recipient,
            "value": "0x0",
            "gasPrice": "0x3b9aca00",
            "feeTokenID": "0x1",
            "feeLimit": "0xde0b6b3a7640000",   // 1e18 token units
        }),
        "latest",
    );

    let result: Result<Value, _> = client.request("eth_call", params).await;

    if let Err(err) = &result {
        let err_str = err.to_string();
        assert!(
            !err_str.contains("insufficient funds for transfer")
                && !err_str.contains("insufficient funds for l1 fee"),
            "token-fee eth_call must not reject on ETH-balance grounds: {err_str}"
        );
    }

    Ok(())
}

/// `eth_estimateGas` rejects a request whose sender cannot afford `tx.value`.
///
/// Exercises the first balance check in `MorphEthApi::caller_gas_allowance`
/// — the `value` check that fires before the L1 fee is compared to the balance.
///
/// Setup:
/// - An unfunded random sender (`balance = 0`).
/// - A non-zero `value`, which makes `value > balance` trivially true.
///
/// Expected:
/// - The RPC returns an error whose message contains
///   `"insufficient funds for transfer"` (matches go-ethereum's error string
///   for this case).
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_reports_insufficient_funds_for_transfer() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let poor_sender = Address::random();
    let recipient = Address::random();

    let params = (serde_json::json!({
        "from": poor_sender,
        "to": recipient,
        "value": "0x64", // 100 wei > 0 balance
        "gasPrice": "0x3b9aca00",
    }),);

    let result: Result<Value, _> = client.request("eth_estimateGas", params).await;

    let err =
        result.expect_err("eth_estimateGas must fail when value exceeds the sender's balance");
    let err_str = err.to_string();
    assert!(
        err_str.contains("insufficient funds for transfer"),
        "expected 'insufficient funds for transfer', got: {err_str}"
    );

    Ok(())
}

/// Simulation RPCs reject requests specifying an unregistered fee token ID.
#[tokio::test(flavor = "multi_thread")]
async fn simulation_rpcs_reject_unregistered_fee_token() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let sender = Address::random();
    let recipient = Address::random();

    let request = serde_json::json!({
        "from": sender,
        "to": recipient,
        "value": "0x0",
        "feeTokenID": "0xffff", // 65535, unregistered
    });

    let result: Result<Value, _> = client.request("eth_estimateGas", (request.clone(),)).await;

    let err = result.expect_err("eth_estimateGas must fail for unregistered fee token");
    let err_str = err.to_string();
    assert!(
        err_str.contains("Token with ID 65535 is not registered"),
        "expected 'Token with ID 65535 is not registered', got: {err_str}"
    );

    let result: Result<Value, _> = client.request("eth_call", (request, "latest")).await;
    let err = result.expect_err("eth_call must fail for unregistered fee token");
    let err_str = err.to_string();
    assert!(
        err_str.contains("Token with ID 65535 is not registered"),
        "expected 'Token with ID 65535 is not registered', got: {err_str}"
    );

    Ok(())
}

/// A legacy `gasPrice` must not strip `feeTokenID` on simulation RPCs.
///
/// geth applies the "legacy gasPrice forces a standard transaction" rule only in
/// `toTransaction` (real transaction construction), never in `ToMessage`, so
/// `eth_call` / `eth_estimateGas` keep the fee token. Dropping it made morph-reth
/// silently price a token-fee request as an ETH transaction: with an unregistered
/// token the request succeeded with `0x5208` instead of being rejected.
#[tokio::test(flavor = "multi_thread")]
async fn simulation_rpcs_keep_fee_token_with_legacy_gas_price() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let sender = Address::random();
    let recipient = Address::random();

    // A non-zero legacy gasPrice previously forced the standard-transaction path.
    let request = serde_json::json!({
        "from": sender,
        "to": recipient,
        "value": "0x0",
        "gasPrice": "0x3b9aca00",
        "feeTokenID": "0xffff", // 65535, unregistered
    });

    let result: Result<Value, _> = client.request("eth_estimateGas", (request.clone(),)).await;
    let err = result
        .expect_err("eth_estimateGas must not drop feeTokenID when gasPrice is set")
        .to_string();
    assert!(
        err.contains("invalid fee token"),
        "expected 'invalid fee token', got: {err}"
    );

    let result: Result<Value, _> = client.request("eth_call", (request, "latest")).await;
    let err = result
        .expect_err("eth_call must not drop feeTokenID when gasPrice is set")
        .to_string();
    assert!(
        err.contains("Token with ID 65535 is not registered"),
        "expected 'Token with ID 65535 is not registered', got: {err}"
    );

    Ok(())
}

/// Native Reth 2.5.2 `eth_getProof` and `eth_getMultiProof` succeed on latest without proof-history.
#[tokio::test(flavor = "multi_thread")]
async fn native_get_proof_and_get_multi_proof_succeed_on_latest() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let sender = wallet.inner.address();

    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    // 1. eth_getProof on latest
    let proof: Value = client
        .request("eth_getProof", (sender, Vec::<String>::new(), "latest"))
        .await?;
    assert_eq!(
        proof["address"].as_str().unwrap().to_lowercase(),
        sender.to_string().to_lowercase()
    );
    assert!(proof["accountProof"].is_array());

    // 2. eth_getMultiProof on latest
    let multi_proof: Value = client
        .request(
            "eth_getMultiProof",
            (vec![(sender, Vec::<B256>::new())], "latest"),
        )
        .await?;
    assert!(multi_proof.is_array());
    assert_eq!(multi_proof.as_array().unwrap().len(), 1);
    assert_eq!(
        multi_proof[0]["address"].as_str().unwrap().to_lowercase(),
        sender.to_string().to_lowercase()
    );
    assert!(multi_proof[0]["accountProof"].is_array());

    Ok(())
}

/// Decodes a 32-byte hex string (with `0x` prefix) returned by EVM calls into a [`U256`].
fn decode_u256_word(hex: &str) -> U256 {
    let bytes =
        alloy_primitives::hex::decode(hex.trim_start_matches("0x")).expect("valid hex return data");
    assert_eq!(bytes.len(), 32, "expected 32-byte ABI word, got {hex}");
    U256::from_be_slice(&bytes)
}

/// Decodes two concatenated 32-byte hex words returned by the 2-address balance reader.
fn decode_two_u256_words(hex: &str) -> (U256, U256) {
    let bytes =
        alloy_primitives::hex::decode(hex.trim_start_matches("0x")).expect("valid hex return data");
    assert_eq!(bytes.len(), 64, "expected 64-byte ABI output, got {hex}");
    (
        U256::from_be_slice(&bytes[..32]),
        U256::from_be_slice(&bytes[32..]),
    )
}

/// ABI-encodes a single address as a 32-byte calldata word.
fn encode_address_word(addr: Address) -> String {
    let mut buf = [0u8; 32];
    buf[12..].copy_from_slice(addr.as_slice());
    format!("0x{}", alloy_primitives::hex::encode(buf))
}

/// ABI-encodes two addresses as 64 bytes of calldata.
fn encode_two_address_words(a: Address, b: Address) -> String {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(a.as_slice());
    buf[44..64].copy_from_slice(b.as_slice());
    format!("0x{}", alloy_primitives::hex::encode(buf))
}

/// ABI-encodes ERC-20 `balanceOf(address)`.
fn encode_balance_of(addr: Address) -> String {
    let mut buf = [0u8; 36];
    buf[..4].copy_from_slice(&[0x70, 0xa0, 0x82, 0x31]);
    buf[16..].copy_from_slice(addr.as_slice());
    format!("0x{}", alloy_primitives::hex::encode(buf))
}

/// `eth_simulateV1` with `validation: false` must disable L1 and L2 fee charging
/// for both ETH and token-fee MorphTxs post-Curie with non-zero L1 oracle fees:
/// - Omitted-`from` and zero-balance callers succeed.
/// - Funded caller balances and FeeVault balance are unchanged by L1/L2 fees
///   (and `value > 0` transfers move only `value`).
/// - Call-mode token-fee MorphTxs neither deduct tokens nor emit fee `Transfer` logs.
#[tokio::test(flavor = "multi_thread")]
async fn simulate_v1_validation_false_skips_l1_and_l2_fees() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const L1_ORACLE: Address =
        alloy_primitives::address!("530000000000000000000000000000000000000f");
    const FEE_VAULT: Address =
        alloy_primitives::address!("530000000000000000000000000000000000000a");
    const TEST_TOKEN: Address =
        alloy_primitives::address!("5300000000000000000000000000000000000022");
    const L1_BASE_FEE: u64 = 10_000_000_000; // 10 gwei (non-zero Curie commit fee)
    const L1_BLOB_BASE_FEE: u64 = 10_000_000_000; // 10 gwei (non-zero Curie blob fee)
    // Runtime bytecode: return `BALANCE(calldataload(0))` (32 bytes).
    const BALANCE_READER_CODE: &str = "0x6000353160005260206000f3";

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_storage(L1_ORACLE, U256::from(1u64), U256::from(L1_BASE_FEE))
        .with_account_storage(L1_ORACLE, U256::from(6u64), U256::from(L1_BLOB_BASE_FEE))
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();
    let funded = wallet.inner.address();
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let latest_state = node.inner.provider.latest()?;
    let funded_account = latest_state
        .basic_account(&funded)?
        .expect("funded wallet exists");
    let initial_funded_balance = funded_account.balance;
    let initial_funded_nonce = funded_account.nonce;
    let initial_vault_balance = latest_state
        .basic_account(&FEE_VAULT)?
        .map(|a| a.balance)
        .unwrap_or_default();

    // Read funded wallet's initial call-mode fee token balance via eth_call.
    let initial_token_hex: String = client
        .request(
            "eth_call",
            (
                serde_json::json!({
                    "to": TEST_TOKEN,
                    "input": encode_balance_of(funded),
                }),
                "latest",
            ),
        )
        .await?;
    let initial_token_balance = decode_u256_word(&initial_token_hex);
    assert!(
        initial_token_balance > U256::ZERO,
        "funded wallet must hold genesis test fee tokens"
    );

    let reader_addr = Address::random();
    let unfunded_caller = Address::random();
    let recipient = Address::random();
    let value_recipient = Address::random();
    let transfer_value = U256::from(1_000_000_000_000_000_000u128); // 1 ETH
    let calldata = format!("0x{}", alloy_primitives::hex::encode(vec![0xaa; 128]));

    let payload = serde_json::json!({
        "blockStateCalls": [{
            "stateOverrides": {
                reader_addr.to_string(): {
                    "code": BALANCE_READER_CODE,
                },
            },
            "calls": [
                // 0: omitted `from` (spec default 0x0000...0000, zero ETH)
                {
                    "to": recipient,
                    "input": calldata,
                },
                // 1: explicit zero-balance caller with non-zero gasPrice
                {
                    "from": unfunded_caller,
                    "to": recipient,
                    "gasPrice": "0x3b9aca00",
                    "input": calldata,
                },
                // 2: funded caller, value = 0, non-zero EIP-1559 fee caps + calldata
                {
                    "from": funded,
                    "to": recipient,
                    "maxFeePerGas": "0x77359400",
                    "maxPriorityFeePerGas": "0x3b9aca00",
                    "input": calldata,
                },
                // 3: check funded caller ETH balance unchanged
                {
                    "to": reader_addr,
                    "input": encode_address_word(funded),
                },
                // 4: check FeeVault ETH balance unchanged (no phantom reward)
                {
                    "to": reader_addr,
                    "input": encode_address_word(FEE_VAULT),
                },
                // 5: funded caller, value = 1 ETH transfer
                {
                    "from": funded,
                    "to": value_recipient,
                    "value": format!("{transfer_value:#x}"),
                    "maxFeePerGas": "0x77359400",
                    "maxPriorityFeePerGas": "0x3b9aca00",
                },
                // 6: check funded caller ETH balance decreased by exact transfer_value
                {
                    "to": reader_addr,
                    "input": encode_address_word(funded),
                },
                // 7: check value_recipient received exact transfer_value
                {
                    "to": reader_addr,
                    "input": encode_address_word(value_recipient),
                },
                // 8: check FeeVault ETH balance still unchanged
                {
                    "to": reader_addr,
                    "input": encode_address_word(FEE_VAULT),
                },
                // 9: call-mode token-fee MorphTx from unfunded caller (0 ETH, 0 tokens)
                {
                    "from": unfunded_caller,
                    "to": recipient,
                    "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                    "feeLimit": "0x0",
                    "input": calldata,
                },
                // 10: call-mode token-fee MorphTx from funded wallet (must emit 0 logs & deduct 0 tokens)
                {
                    "from": funded,
                    "to": recipient,
                    "maxFeePerGas": "0x77359400",
                    "maxPriorityFeePerGas": "0x3b9aca00",
                    "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                    "feeLimit": "0xde0b6b3a7640000",
                    "input": calldata,
                },
                // 11: check funded wallet token balance unchanged
                {
                    "to": TEST_TOKEN,
                    "input": encode_balance_of(funded),
                },
                // 12: check FeeVault token balance unchanged (0)
                {
                    "to": TEST_TOKEN,
                    "input": encode_balance_of(FEE_VAULT),
                },
            ],
        }],
        "validation": false,
        "returnFullTransactions": true,
    });

    let blocks: Value = client
        .request("eth_simulateV1", (payload, "latest"))
        .await?;
    let block = &blocks[0];
    let calls = block["calls"].as_array().expect("calls array");
    assert_eq!(calls.len(), 13);

    for (idx, call) in calls.iter().enumerate() {
        assert_eq!(
            call["status"].as_str(),
            Some("0x1"),
            "call {idx} must succeed under validation=false: {call}"
        );
    }

    // Funded caller ETH balance unchanged after value=0 call.
    assert_eq!(
        decode_u256_word(calls[3]["returnData"].as_str().unwrap()),
        initial_funded_balance,
        "validation=false must not deduct L1/L2 fees from funded caller"
    );
    // FeeVault ETH balance unchanged after value=0 call.
    assert_eq!(
        decode_u256_word(calls[4]["returnData"].as_str().unwrap()),
        initial_vault_balance,
        "validation=false must not credit phantom fees to FeeVault"
    );
    // Funded caller ETH balance reduced by exact transfer_value after value>0 call.
    assert_eq!(
        decode_u256_word(calls[6]["returnData"].as_str().unwrap()),
        initial_funded_balance - transfer_value,
        "value transfer under validation=false must debit only tx.value"
    );
    assert_eq!(
        decode_u256_word(calls[7]["returnData"].as_str().unwrap()),
        transfer_value,
        "recipient must receive exact transfer_value"
    );
    assert_eq!(
        decode_u256_word(calls[8]["returnData"].as_str().unwrap()),
        initial_vault_balance,
        "FeeVault balance must remain unchanged after value transfer"
    );

    // Call-mode token-fee MorphTxs must emit zero fee Transfer logs and move zero tokens.
    assert!(
        calls[9]["logs"].as_array().unwrap().is_empty(),
        "unfunded token-fee MorphTx must emit no logs under validation=false"
    );
    assert!(
        calls[10]["logs"].as_array().unwrap().is_empty(),
        "funded call-mode token-fee MorphTx must emit no fee Transfer logs under validation=false"
    );
    assert_eq!(
        decode_u256_word(calls[11]["returnData"].as_str().unwrap()),
        initial_token_balance,
        "funded caller token balance must be unchanged under validation=false"
    );
    assert_eq!(
        decode_u256_word(calls[12]["returnData"].as_str().unwrap()),
        U256::ZERO,
        "FeeVault token balance must remain zero under validation=false"
    );

    // Verify caller nonce bumped across funded ETH and MorphTx calls (indices 2, 5, 10).
    let txs = block["transactions"]
        .as_array()
        .expect("transactions array");
    assert_eq!(
        txs[2]["nonce"].as_str(),
        Some(format!("{initial_funded_nonce:#x}").as_str())
    );
    assert_eq!(
        txs[5]["nonce"].as_str(),
        Some(format!("{:#x}", initial_funded_nonce + 1).as_str())
    );
    assert_eq!(
        txs[10]["nonce"].as_str(),
        Some(format!("{:#x}", initial_funded_nonce + 2).as_str())
    );

    // Also exercise the `traceTransfers: true` (`TransferInspector`) branch under
    // `validation: false`: a 1 ETH transfer emits exactly 1 synthetic ETH transfer
    // log (to `value_recipient`) and zero FeeVault transfer logs.
    let trace_payload = serde_json::json!({
        "blockStateCalls": [{
            "calls": [{
                "from": funded,
                "to": value_recipient,
                "value": format!("{transfer_value:#x}"),
                "maxFeePerGas": "0x77359400",
                "maxPriorityFeePerGas": "0x3b9aca00",
            }],
        }],
        "traceTransfers": true,
        "validation": false,
    });
    let trace_blocks: Value = client
        .request("eth_simulateV1", (trace_payload, "latest"))
        .await?;
    let trace_call = &trace_blocks[0]["calls"][0];
    assert_eq!(trace_call["status"].as_str(), Some("0x1"));
    let trace_logs = trace_call["logs"].as_array().expect("trace logs array");
    assert_eq!(
        trace_logs.len(),
        1,
        "traceTransfers with validation=false must record only the value transfer, not fee transfers: {trace_logs:?}"
    );

    Ok(())
}

/// `eth_simulateV1` with `validation: true` must wrap simulated transactions with
/// their real EIP-2718 encoding so post-Curie `commit + blob` L1 fees are charged
/// and validated accurately (for both standard ETH txs and token-fee MorphTxs).
#[tokio::test(flavor = "multi_thread")]
async fn simulate_v1_validation_true_charges_full_curie_l1_and_l2_fees() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const L1_ORACLE: Address =
        alloy_primitives::address!("530000000000000000000000000000000000000f");
    const FEE_VAULT: Address =
        alloy_primitives::address!("530000000000000000000000000000000000000a");
    const L1_BASE_FEE: u64 = 10_000_000_000; // 10 gwei
    const L1_BLOB_BASE_FEE: u64 = 10_000_000_000; // 10 gwei
    // Matches `tests/assets/test-genesis.json` slots 7 & 8.
    const COMMIT_SCALAR: u128 = 0x35ba5d7b55;
    const BLOB_SCALAR: u128 = 0x18e38a4c;
    const L1_FEE_PRECISION: u128 = 1_000_000_000;
    // Runtime bytecode: return `(BALANCE(calldataload(0)), BALANCE(calldataload(32)))` (64 bytes).
    const DUAL_BALANCE_READER_CODE: &str = "0x600035316000526020353160205260406000f3";

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_storage(L1_ORACLE, U256::from(1u64), U256::from(L1_BASE_FEE))
        .with_account_storage(L1_ORACLE, U256::from(6u64), U256::from(L1_BLOB_BASE_FEE))
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let funded_wallet = wallet.inner.address();
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let initial_vault_balance = node
        .inner
        .provider
        .latest()?
        .basic_account(&FEE_VAULT)?
        .map(|a| a.balance)
        .unwrap_or_default();

    let sender = Address::random();
    let recipient = Address::random();
    let reader_addr = Address::random();
    let calldata = Bytes::from(vec![0xab; 1024]);
    let gas_limit = 50_000u64;
    let max_fee_per_gas = 2_000_000_000u128; // 2 gwei
    let max_priority_fee_per_gas = 1_000_000_000u128; // 1 gwei

    // Build the simulated EIP-1559 envelope with both the 3-byte zero signature
    // (`TryIntoSimTx`) and the full 64-byte placeholder signature (`U256::MAX, U256::MAX, true`)
    // used for L1 fee sizing.
    let zero_sig = alloy_primitives::Signature::new(U256::ZERO, U256::ZERO, false);
    let placeholder_sig = alloy_primitives::Signature::new(U256::MAX, U256::MAX, true);
    let tx_inner = alloy_consensus::TxEip1559 {
        chain_id,
        nonce: 0,
        gas_limit,
        max_fee_per_gas,
        max_priority_fee_per_gas,
        to: TxKind::Call(recipient),
        value: U256::ZERO,
        access_list: Default::default(),
        input: calldata.clone(),
    };
    let zero_sig_len = MorphTxEnvelope::Eip1559(tx_inner.clone().into_signed(zero_sig))
        .encoded_2718()
        .len() as u128;
    let encoded_len = MorphTxEnvelope::Eip1559(tx_inner.into_signed(placeholder_sig))
        .encoded_2718()
        .len() as u128;
    assert_eq!(
        encoded_len,
        zero_sig_len + 64,
        "placeholder signature must add exactly 64 bytes for L1 fee sizing"
    );
    let commit_only_l1_fee =
        U256::from((COMMIT_SCALAR * u128::from(L1_BASE_FEE)) / L1_FEE_PRECISION);
    let zero_sig_l1_fee = U256::from(
        (COMMIT_SCALAR * u128::from(L1_BASE_FEE)
            + BLOB_SCALAR * zero_sig_len * u128::from(L1_BLOB_BASE_FEE))
            / L1_FEE_PRECISION,
    );
    let full_l1_fee = U256::from(
        (COMMIT_SCALAR * u128::from(L1_BASE_FEE)
            + BLOB_SCALAR * encoded_len * u128::from(L1_BLOB_BASE_FEE))
            / L1_FEE_PRECISION,
    );
    assert!(
        full_l1_fee > zero_sig_l1_fee && zero_sig_l1_fee > commit_only_l1_fee * U256::from(2u64),
        "placeholder-signature Curie blob fee must strictly exceed zero-signature and commit-only fees"
    );

    let l2_max_fee = U256::from(gas_limit) * U256::from(max_fee_per_gas);
    let tx_call = serde_json::json!({
        "from": sender,
        "to": recipient,
        "gas": format!("{gas_limit:#x}"),
        "maxFeePerGas": format!("{max_fee_per_gas:#x}"),
        "maxPriorityFeePerGas": format!("{max_priority_fee_per_gas:#x}"),
        "input": calldata.to_string(),
    });

    // (1) Balance covers L2 + zero-signature L1 fee (which omits the 64B placeholder
    //     signature bytes), NOT full placeholder-signature Curie L1 fee: must fail
    //     with insufficient funds (-38014).
    let insufficient_balance = l2_max_fee + zero_sig_l1_fee;
    let fail_payload = serde_json::json!({
        "blockStateCalls": [{
            "stateOverrides": {
                sender.to_string(): {
                    "balance": format!("{insufficient_balance:#x}"),
                },
            },
            "calls": [tx_call],
        }],
        "validation": true,
    });
    let err = client
        .request::<Value, _>("eth_simulateV1", (fail_payload, "latest"))
        .await
        .expect_err(
            "validation=true must reject when balance omits 64B placeholder signature L1 fee",
        );
    let err_str = err.to_string();
    assert!(
        err_str.contains("insufficient funds") || err_str.contains("-38014"),
        "expected insufficient funds error, got: {err_str}"
    );

    // (2) Balance covers exact `l2_max_fee + full_l1_fee`: must succeed, charge exact
    //     `l2_used_fee + full_l1_fee` to sender and FeeVault, AND with
    //     `returnFullTransactions: true` keep zero `r`/`s`/`v` on the returned tx.
    let exact_budget = l2_max_fee + full_l1_fee;
    let pass_payload = serde_json::json!({
        "blockStateCalls": [{
            "stateOverrides": {
                sender.to_string(): {
                    "balance": format!("{exact_budget:#x}"),
                },
                reader_addr.to_string(): {
                    "code": DUAL_BALANCE_READER_CODE,
                },
            },
            "calls": [
                tx_call,
                // Executed immediately after Call 0 commits and BEFORE Call 1's own
                // `reward_beneficiary` runs, so it observes exact post-Call-0 balances
                // of both `sender` and `FEE_VAULT`.
                {
                    "from": funded_wallet,
                    "to": reader_addr,
                    "gas": "0x186a0",
                    "maxFeePerGas": format!("{max_fee_per_gas:#x}"),
                    "maxPriorityFeePerGas": format!("{max_priority_fee_per_gas:#x}"),
                    "input": encode_two_address_words(sender, FEE_VAULT),
                },
            ],
        }],
        "validation": true,
        "returnFullTransactions": true,
    });
    let blocks: Value = client
        .request("eth_simulateV1", (pass_payload, "latest"))
        .await?;
    let block = &blocks[0];
    let calls = block["calls"].as_array().expect("calls array");
    assert_eq!(calls[0]["status"].as_str(), Some("0x1"));
    assert_eq!(calls[1]["status"].as_str(), Some("0x1"));

    // Returned full transaction must keep zero r/s/v per the eth_simulateV1 spec.
    let txs = block["transactions"]
        .as_array()
        .expect("full transactions array");
    assert_eq!(txs.len(), 2);
    assert_eq!(txs[0]["r"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["s"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["v"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["yParity"].as_str(), Some("0x0"));

    let block_base_fee = u128::from_str_radix(
        block["baseFeePerGas"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )?;
    let gas_used = u64::from_str_radix(
        calls[0]["gasUsed"]
            .as_str()
            .unwrap()
            .trim_start_matches("0x"),
        16,
    )?;
    let effective_gas_price = max_fee_per_gas.min(block_base_fee + max_priority_fee_per_gas);
    let expected_total_fee = U256::from(gas_used) * U256::from(effective_gas_price) + full_l1_fee;

    let (sender_balance_after, vault_balance_after) =
        decode_two_u256_words(calls[1]["returnData"].as_str().unwrap());
    assert_eq!(
        sender_balance_after,
        exact_budget - expected_total_fee,
        "sender must be charged exact L2 execution fee + full Curie (commit + blob) L1 fee"
    );
    assert_eq!(
        vault_balance_after,
        initial_vault_balance + expected_total_fee,
        "FeeVault must receive exact L2 execution fee + full Curie (commit + blob) L1 fee"
    );

    // (3) Token-fee MorphTx under `validation: true`: a `feeLimit` that only covers
    //     L2 (`gas_limit * effective_gas_price`) + commit-only L1 fee (+ half the blob fee)
    //     must fail, while covering full Curie L1 + L2 succeeds and emits the call-mode
    //     ERC-20 `Transfer` fee log(s). (Genesis token ID 1 has 1:1 price_ratio == scale == 1e18.)
    let l2_effective_max_fee = U256::from(gas_limit) * U256::from(effective_gas_price);
    let token_tight_limit = l2_effective_max_fee
        + commit_only_l1_fee
        + (full_l1_fee - commit_only_l1_fee) / U256::from(2u64);
    let morph_fail_payload = serde_json::json!({
        "blockStateCalls": [{
            "calls": [{
                "from": funded_wallet,
                "to": recipient,
                "gas": format!("{gas_limit:#x}"),
                "maxFeePerGas": format!("{max_fee_per_gas:#x}"),
                "maxPriorityFeePerGas": format!("{max_priority_fee_per_gas:#x}"),
                "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                "feeLimit": format!("{token_tight_limit:#x}"),
                "input": calldata.to_string(),
            }],
        }],
        "validation": true,
    });
    client
        .request::<Value, _>("eth_simulateV1", (morph_fail_payload, "latest"))
        .await
        .expect_err("token-fee MorphTx must fail under validation=true when feeLimit omits Curie blob L1 fee");

    let token_generous_limit = l2_max_fee + full_l1_fee * U256::from(2u64);
    let morph_pass_payload = serde_json::json!({
        "blockStateCalls": [{
            "calls": [{
                "from": funded_wallet,
                "to": recipient,
                "gas": format!("{gas_limit:#x}"),
                "maxFeePerGas": format!("{max_fee_per_gas:#x}"),
                "maxPriorityFeePerGas": format!("{max_priority_fee_per_gas:#x}"),
                "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                "feeLimit": format!("{token_generous_limit:#x}"),
                "input": calldata.to_string(),
            }],
        }],
        "validation": true,
    });
    let morph_blocks: Value = client
        .request("eth_simulateV1", (morph_pass_payload, "latest"))
        .await?;
    let morph_call = &morph_blocks[0]["calls"][0];
    assert_eq!(morph_call["status"].as_str(), Some("0x1"));
    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let logs = morph_call["logs"].as_array().expect("logs array");
    assert!(
        logs.iter().any(|log| {
            log["topics"]
                .as_array()
                .and_then(|t| t.first())
                .and_then(Value::as_str)
                == Some(transfer_topic)
        }),
        "validated call-mode token-fee MorphTx must emit ERC-20 Transfer fee logs: {logs:?}"
    );

    Ok(())
}

/// `eth_simulateV1(validation: true)` with legacy `gasPrice` + `feeTokenID` on a
/// zero-ETH / funded-token caller must execute as a token-fee MorphTx (`0x7f`)
/// rather than silently degrading into a Legacy ETH transaction.
#[tokio::test(flavor = "multi_thread")]
async fn simulate_v1_validation_true_legacy_gas_price_with_fee_token_id_executes_morph_tx()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const L1_ORACLE: Address =
        alloy_primitives::address!("530000000000000000000000000000000000000f");
    const TEST_TOKEN: Address =
        alloy_primitives::address!("5300000000000000000000000000000000000022");
    const L1_BASE_FEE: u64 = 10_000_000_000; // 10 gwei
    const L1_BLOB_BASE_FEE: u64 = 10_000_000_000; // 10 gwei

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_storage(L1_ORACLE, U256::from(1u64), U256::from(L1_BASE_FEE))
        .with_account_storage(L1_ORACLE, U256::from(6u64), U256::from(L1_BLOB_BASE_FEE))
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();
    let token_holder = wallet.inner.address();
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let initial_token_hex: String = client
        .request(
            "eth_call",
            (
                serde_json::json!({
                    "to": TEST_TOKEN,
                    "input": encode_balance_of(token_holder),
                }),
                "latest",
            ),
        )
        .await?;
    let initial_token_balance = decode_u256_word(&initial_token_hex);
    assert!(
        initial_token_balance > U256::ZERO,
        "genesis token holder must have non-zero fee token balance"
    );

    let recipient = Address::random();
    let gas_limit = 50_000u64;
    let gas_price = 2_000_000_000u128; // 2 gwei
    let calldata = Bytes::from(vec![0xcd; 128]);

    // Zero out `token_holder`'s ETH balance via `stateOverrides` so a Legacy ETH
    // fallback would fail immediately with insufficient ETH, while preserving its
    // ERC-20 token balance in `TEST_TOKEN`.
    let payload = serde_json::json!({
        "blockStateCalls": [{
            "stateOverrides": {
                token_holder.to_string(): {
                    "balance": "0x0",
                },
            },
            "calls": [
                {
                    "from": token_holder,
                    "to": recipient,
                    "gas": format!("{gas_limit:#x}"),
                    "gasPrice": format!("{gas_price:#x}"),
                    "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                    "input": calldata.to_string(),
                },
                {
                    "from": token_holder,
                    "to": TEST_TOKEN,
                    "gas": "0x186a0",
                    "gasPrice": format!("{gas_price:#x}"),
                    "feeTokenID": format!("{TEST_TOKEN_ID:#x}"),
                    "input": encode_balance_of(token_holder),
                },
            ],
        }],
        "validation": true,
        "returnFullTransactions": true,
    });

    let blocks: Value = client
        .request("eth_simulateV1", (payload, "latest"))
        .await?;
    let block = &blocks[0];
    let calls = block["calls"].as_array().expect("calls array");
    assert_eq!(calls[0]["status"].as_str(), Some("0x1"));
    assert_eq!(calls[1]["status"].as_str(), Some("0x1"));

    // Returned full transaction must be a MorphTx (`0x7f`) with `feeTokenID = 1`
    // and zero `r`/`s`/`v` signature fields.
    let txs = block["transactions"]
        .as_array()
        .expect("full transactions array");
    assert_eq!(txs[0]["type"].as_str(), Some("0x7f"));
    assert_eq!(txs[0]["feeTokenID"].as_str(), Some("0x1"));
    assert_eq!(txs[0]["r"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["s"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["v"].as_str(), Some("0x0"));
    assert_eq!(txs[0]["yParity"].as_str(), Some("0x0"));

    // ERC-20 `Transfer` fee log emitted on Call 0.
    let transfer_topic = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef";
    let logs = calls[0]["logs"].as_array().expect("logs array");
    assert!(
        logs.iter().any(|log| {
            log["topics"]
                .as_array()
                .and_then(|t| t.first())
                .and_then(Value::as_str)
                == Some(transfer_topic)
        }),
        "gasPrice + feeTokenID MorphTx must emit ERC-20 Transfer fee logs: {logs:?}"
    );

    // Token balance observed during Call 1 must have decreased from Call 0's fee deduction
    // (and Call 1's own upfront deduction).
    let token_balance_after = decode_u256_word(calls[1]["returnData"].as_str().unwrap());
    assert!(
        token_balance_after < initial_token_balance,
        "token fees must be deducted from ERC-20 balance: before={initial_token_balance}, after={token_balance_after}"
    );

    Ok(())
}

/// `eth_maxPriorityFeePerGas` and `eth_gasPrice` must never collapse to zero on
/// empty blocks, and must scale above the median tip when a block reaches capacity.
#[tokio::test(flavor = "multi_thread")]
async fn gas_price_oracle_returns_floor_on_empty_blocks_and_scales_at_capacity() -> eyre::Result<()>
{
    use alloy_consensus::TxEip1559;
    use morph_chainspec::{MORPH_BASE_FEE, MORPH_DEFAULT_PRIORITY_FEE};

    reth_tracing::init_test_tracing();

    // 1. Genesis and consecutive empty blocks must return MORPH_DEFAULT_PRIORITY_FEE,
    //    not 0x0 (which upstream `GasPriceOracle::suggest_tip_cap` produced).
    let (mut nodes, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let expected_tip = U256::from(MORPH_DEFAULT_PRIORITY_FEE);
    let expected_gas_price = U256::from(MORPH_BASE_FEE + MORPH_DEFAULT_PRIORITY_FEE);

    // Block 0 (genesis)
    let genesis_tip: U256 = client
        .request("eth_maxPriorityFeePerGas", jsonrpsee::rpc_params![])
        .await?;
    let genesis_gas_price: U256 = client
        .request("eth_gasPrice", jsonrpsee::rpc_params![])
        .await?;
    assert_eq!(genesis_tip, expected_tip);
    assert_eq!(genesis_gas_price, expected_gas_price);

    // Advance 3 consecutive empty blocks.
    for _ in 0..3 {
        let payload = node.advance_block().await?;
        assert!(payload.block().body().transactions.is_empty());
    }

    let empty_tip: U256 = client
        .request("eth_maxPriorityFeePerGas", jsonrpsee::rpc_params![])
        .await?;
    let empty_gas_price: U256 = client
        .request("eth_gasPrice", jsonrpsee::rpc_params![])
        .await?;
    assert_eq!(
        empty_tip, expected_tip,
        "empty blocks must keep MORPH_DEFAULT_PRIORITY_FEE floor"
    );
    assert_eq!(empty_gas_price, expected_gas_price);

    // 2. When a block reaches capacity (`gas_used + max_tx_gas_used > gas_limit`),
    //    `eth_maxPriorityFeePerGas` scales up to 110% of the block's median tip.
    let (mut cap_nodes, cap_wallet) = TestNodeBuilder::new()
        .with_gas_limit(30_000)
        .build()
        .await?;
    let mut cap_node = cap_nodes.pop().unwrap();
    let cap_client = cap_node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let high_tip = 10_000_000u128; // 0.01 Gwei > MORPH_DEFAULT_PRIORITY_FEE
    let tx = TxEip1559 {
        chain_id: cap_wallet.chain_id,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 20_000_000,
        max_priority_fee_per_gas: high_tip,
        to: TxKind::Call(Address::with_last_byte(0x42)),
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig = cap_wallet
        .inner
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("{e}"))?;
    let raw: Bytes = MorphTxEnvelope::Eip1559(tx.into_signed(sig))
        .encoded_2718()
        .into();

    cap_node.rpc.inject_tx(raw).await?;
    let payload = cap_node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);
    assert_eq!(payload.block().header().gas_used(), 21_000);
    assert!(
        payload.block().header().gas_used() + 21_000 > payload.block().header().gas_limit(),
        "test block must be at capacity"
    );

    let congested_tip: U256 = cap_client
        .request("eth_maxPriorityFeePerGas", jsonrpsee::rpc_params![])
        .await?;
    let congested_gas_price: U256 = cap_client
        .request("eth_gasPrice", jsonrpsee::rpc_params![])
        .await?;
    let expected_congested_tip = U256::from(high_tip + high_tip / 10);
    assert_eq!(congested_tip, expected_congested_tip);
    assert_eq!(
        congested_gas_price,
        expected_congested_tip + U256::from(MORPH_BASE_FEE)
    );

    // Next empty block immediately returns to the floor.
    cap_node.advance_block().await?;
    let recovered_tip: U256 = cap_client
        .request("eth_maxPriorityFeePerGas", jsonrpsee::rpc_params![])
        .await?;
    assert_eq!(recovered_tip, expected_tip);

    Ok(())
}

/// `eth_estimateGas` / `eth_call` with `maxPriorityFeePerGas: 0x0` and omitted
/// `maxFeePerGas` must treat the call as zero-priced (matching `morph-geth`),
/// rather than pricing it at `base_fee` and failing zero-balance senders with
/// `insufficient funds for l1 fee`.
#[tokio::test(flavor = "multi_thread")]
async fn estimate_gas_allows_zero_priority_fee_without_max_fee_for_zero_balance_sender()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    advance_chain(1, &mut node, wallet_to_arc(wallet)).await?;

    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;

    let zero_balance_sender = Address::random();
    let recipient = Address::random();

    // 1. Omitted maxFeePerGas + maxPriorityFeePerGas = 0 -> succeeds for 0-balance sender.
    let zero_tip_req = serde_json::json!({
        "from": zero_balance_sender,
        "to": recipient,
        "maxPriorityFeePerGas": "0x0",
    });
    let gas: U256 = client
        .request("eth_estimateGas", (zero_tip_req.clone(),))
        .await?;
    assert_eq!(gas, U256::from(21_000u64));

    let call_res: Bytes = client.request("eth_call", (zero_tip_req, "latest")).await?;
    assert!(call_res.is_empty());

    // 2. Explicit non-zero maxFeePerGas + maxPriorityFeePerGas = 0 -> still checks balance/L1 fee.
    let explicit_max_fee_req = serde_json::json!({
        "from": zero_balance_sender,
        "to": recipient,
        "maxFeePerGas": "0x3b9aca00",
        "maxPriorityFeePerGas": "0x0",
    });
    let err = client
        .request::<Value, _>("eth_estimateGas", (explicit_max_fee_req,))
        .await
        .expect_err("explicit non-zero maxFeePerGas must still enforce L1 fee / balance check")
        .to_string();
    assert!(
        err.contains("insufficient funds for l1 fee"),
        "expected insufficient funds for l1 fee, got: {err}"
    );

    // 3. Unregistered feeTokenID with maxPriorityFeePerGas = 0 -> still rejected.
    let bad_token_req = serde_json::json!({
        "from": zero_balance_sender,
        "to": recipient,
        "feeTokenID": "0xffff",
        "maxPriorityFeePerGas": "0x0",
    });
    let err = client
        .request::<Value, _>("eth_estimateGas", (bad_token_req,))
        .await
        .expect_err("unregistered fee token must still be rejected")
        .to_string();
    assert!(
        err.contains("Token with ID 65535 is not registered"),
        "expected unregistered fee token error, got: {err}"
    );

    Ok(())
}
