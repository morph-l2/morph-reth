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
    L1MessageBuilder, MorphTestNode, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder, advance_chain,
};
use morph_primitives::MorphTxEnvelope;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{
    AccountReader, BlockReader, BlockReaderIdExt, HeaderProvider, ReceiptProvider,
    StateProviderFactory, TransactionsProvider,
};
use reth_tasks::TaskManager;
use serde_json::Value;

use super::helpers::wallet_to_arc;

/// Block number advances correctly after producing blocks.
#[tokio::test(flavor = "multi_thread")]
async fn block_number_advances_correctly() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

/// MorphTx v0 receipt stored in the database carries the expected ERC20 fee fields.
///
/// After including a MorphTx v0 (ERC20 fee) in a block, the receipt retrieved
/// from the provider must have `fee_token_id`, `fee_rate`, `token_scale`, and
/// `fee_limit` populated by the receipt builder.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_receipt_contains_fee_fields() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Build and inject a MorphTx v0 with ERC20 fee payment
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;

    // Extract the transaction hash from the sealed block
    let tx = payload
        .block()
        .body()
        .transactions
        .first()
        .expect("block must contain the MorphTx");
    let tx_hash = *tx.tx_hash();

    // Retrieve the receipt from the provider
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("receipt must exist after block import");

    // The receipt must be the Morph variant and carry populated fee fields
    match &receipt {
        morph_primitives::MorphReceipt::Morph(morph_receipt) => {
            assert_eq!(
                morph_receipt.fee_token_id,
                Some(TEST_TOKEN_ID),
                "fee_token_id must match the submitted transaction"
            );
            assert!(
                morph_receipt.fee_rate.is_some(),
                "fee_rate must be present in MorphTx v0 receipt"
            );
            assert!(
                morph_receipt.token_scale.is_some(),
                "token_scale must be present in MorphTx v0 receipt"
            );
            assert!(
                morph_receipt.fee_limit.is_some(),
                "fee_limit must be present in MorphTx v0 receipt"
            );
        }
        other => panic!(
            "expected MorphReceipt::Morph variant, got {:?}",
            other.tx_type()
        ),
    }

    Ok(())
}

/// ETH balance decreases after a transfer transaction.
#[tokio::test(flavor = "multi_thread")]
async fn balance_decreases_after_eth_transfer() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

/// L1 message receipt has l1_fee = 0 (gas is prepaid on L1).
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_receipt_l1_fee_is_zero() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let l1_msg = L1MessageBuilder::new(0)
        .with_target(alloy_primitives::Address::with_last_byte(0x42))
        .with_gas_limit(50_000)
        .build_encoded();
    let payload = super::helpers::advance_block_with_l1_messages(&mut node, vec![l1_msg]).await?;

    let tx = payload.block().body().transactions.first().unwrap();
    let tx_hash = *tx.tx_hash();

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("L1 message receipt must exist");

    assert_eq!(
        receipt.l1_fee(),
        alloy_primitives::U256::ZERO,
        "L1 message l1_fee must be 0"
    );
    Ok(())
}

/// `eth_getTransactionReceipt` exposes Morph-specific receipt fields over JSON-RPC.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_receipt_exposes_morph_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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
    assert_eq!(receipt["version"].as_u64(), Some(1));
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

/// `eth_getTransactionByHash` exposes MorphTx reference and memo over JSON-RPC.
#[tokio::test(flavor = "multi_thread")]
async fn transaction_by_hash_exposes_morph_fields_over_rpc() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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
    assert_eq!(tx["version"].as_u64(), Some(1));
    assert_eq!(tx["feeTokenID"].as_str(), Some("0x1"));
    assert!(tx["feeLimit"].as_str().is_some());
    assert_eq!(tx["reference"].as_str(), Some(expected_reference.as_str()));
    assert_eq!(tx["memo"].as_str(), Some(expected_memo.as_str()));

    Ok(())
}

/// Produces a simple one-transaction block on the standard Jade profile and returns the
/// node, task manager, and identifiers needed by the replay-based debug / trace RPCs.
async fn build_standard_jade_block_for_debug_trace()
-> eyre::Result<(MorphTestNode, TaskManager, B256, B256)> {
    let (mut nodes, tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    Ok((node, tasks, tx_hash, block_hash))
}

/// Comprehensive test: debug + trace replay APIs on a standard Jade block with Cancun active.
///
/// Uses internal APIs (debug_api / trace_api) directly via `node.rpc.inner`,
/// matching the approach on `main`.  This avoids HTTP serialization overhead
/// and the TaskManager lifetime pitfalls of the HTTP path.
#[tokio::test(flavor = "multi_thread")]
async fn debug_trace_replay_apis_work_for_standard_jade_block() -> eyre::Result<()> {
    use alloy_rpc_types_eth::TransactionRequest;
    use morph_rpc::MorphTransactionRequest;

    reth_tracing::init_test_tracing();

    let (node, _tasks, tx_hash, block_hash) = build_standard_jade_block_for_debug_trace().await?;

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
