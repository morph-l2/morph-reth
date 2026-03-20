//! Transaction pool E2E tests.
//!
//! Verifies transaction pool acceptance and rejection behavior:
//! - L1 messages must NOT enter the pool
//! - Nonce-too-low transactions are rejected
//! - Insufficient balance transactions are rejected
//! - Legacy (type 0x00) transactions are accepted

use alloy_consensus::TxLegacy;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};
use morph_node::test_utils::{
    L1MessageBuilder, TestNodeBuilder, advance_chain, make_eip4844_tx, make_transfer_tx,
};
use morph_primitives::MorphTxEnvelope;
use reth_payload_primitives::BuiltPayload;

use super::helpers::wallet_to_arc;

/// L1 message transactions must be rejected by the pool.
///
/// L1 messages are injected via payload attributes, never through the
/// transaction pool. The pool MUST reject them to prevent unauthorized
/// L1→L2 deposits.
#[tokio::test(flavor = "multi_thread")]
async fn l1_message_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let l1_msg = L1MessageBuilder::new(0)
        .with_target(Address::with_last_byte(0x01))
        .with_gas_limit(21_000)
        .build_encoded();

    let result = node.rpc.inject_tx(l1_msg).await;
    assert!(
        result.is_err(),
        "L1 messages must be rejected by the transaction pool"
    );

    Ok(())
}

/// A legacy (type 0x00) transaction is accepted by the pool and included in a block.
#[tokio::test(flavor = "multi_thread")]
async fn legacy_tx_accepted() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Build a legacy transaction (type 0x00)
    use alloy_signer::SignerSync;
    let legacy_tx = TxLegacy {
        chain_id: Some(wallet.chain_id),
        nonce: 0,
        gas_limit: 21_000,
        gas_price: 20_000_000_000u128,
        to: TxKind::Call(Address::with_last_byte(0x42)),
        value: U256::from(100),
        input: Bytes::new(),
    };

    use alloy_consensus::SignableTransaction;
    let sig = wallet
        .inner
        .sign_hash_sync(&legacy_tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let signed = legacy_tx.into_signed(sig);
    let envelope = MorphTxEnvelope::Legacy(signed);
    let encoded: Bytes = envelope.encoded_2718().into();

    node.rpc.inject_tx(encoded).await?;
    let payload = node.advance_block().await?;

    assert_eq!(
        payload.block().body().transactions.len(),
        1,
        "legacy transaction should be included"
    );

    Ok(())
}

/// A transaction with nonce too low is rejected by the pool.
///
/// After advancing 1 block (nonce 0 used), submitting another tx
/// with nonce 0 should fail.
#[tokio::test(flavor = "multi_thread")]
async fn nonce_too_low_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet = wallet_to_arc(wallet);

    // Advance 1 block (uses nonce 0)
    advance_chain(1, &mut node, wallet.clone()).await?;

    // Try to submit another tx with nonce 0 (already used)
    let w = wallet.lock().await;
    let stale_tx = make_transfer_tx(w.chain_id, w.inner.clone(), 0).await;
    drop(w);

    let result = node.rpc.inject_tx(stale_tx).await;
    assert!(
        result.is_err(),
        "transaction with nonce=0 (already used) should be rejected"
    );

    Ok(())
}

/// A transaction with higher-than-expected nonce is accepted by pool (queued).
///
/// The pool should accept future-nonce transactions for queuing, even
/// though they can't be executed immediately.
#[tokio::test(flavor = "multi_thread")]
async fn future_nonce_queued() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    // Submit tx with nonce=5 (account nonce is 0, so this is "future")
    let future_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 5).await;
    let result = node.rpc.inject_tx(future_tx).await;

    // Pool should accept the transaction for queuing (not reject it)
    assert!(
        result.is_ok(),
        "future nonce tx should be accepted for queuing"
    );

    Ok(())
}

/// A future-nonce transaction queued in the pool is promoted and included once
/// the gap transactions are submitted.
///
/// Sequence:
/// 1. Submit nonce=2 → queued (gap: nonces 0 and 1 are missing)
/// 2. Build an empty block → nonce=2 cannot execute, block is empty
/// 3. Submit nonce=0 and nonce=1 → all three are now pending
/// 4. Build another block → all three transactions should be included
#[tokio::test(flavor = "multi_thread")]
async fn future_nonce_queued_then_promoted_after_gap_filled() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Submit nonce=2 — this is a future nonce; nonces 0 and 1 are missing
    let future_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 2).await;
    node.rpc.inject_tx(future_tx).await?;

    // Build an empty block: nonce=2 is queued but cannot be executed yet.
    // We must use advance_empty_block to avoid hanging (advance_block waits for ≥1 tx).
    let empty_payload = morph_node::test_utils::advance_empty_block(&mut node).await?;
    assert_eq!(
        empty_payload.block().body().transactions.len(),
        0,
        "block should be empty when only a queued (future-nonce) tx is in the pool"
    );

    // Submit nonce=0 and nonce=1 to fill the gap
    let tx0 = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    node.rpc.inject_tx(tx0).await?;
    let tx1 = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 1).await;
    node.rpc.inject_tx(tx1).await?;

    // Build blocks until all 3 transactions are included.
    // Typically one block suffices (nonces 0, 1, 2 are all pending after promotion),
    // but we allow up to two blocks in case the queued tx isn't promoted until after
    // the first block is sealed.
    // Use advance_empty_block (bypasses event stream) — it still picks up pool txs.
    let payload_a = morph_node::test_utils::advance_empty_block(&mut node).await?;
    let count_a = payload_a.block().body().transactions.len();

    let total = if count_a < 3 {
        let payload_b = morph_node::test_utils::advance_empty_block(&mut node).await?;
        count_a + payload_b.block().body().transactions.len()
    } else {
        count_a
    };

    assert_eq!(
        total, 3,
        "all 3 transactions (nonces 0, 1, 2) should eventually be included"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn eip2930_accepted_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = morph_node::test_utils::make_eip2930_tx(wallet.chain_id, wallet.inner.clone(), 0)?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn eip4844_tx_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let blob_tx = make_eip4844_tx(wallet.chain_id, wallet.inner.clone(), 0)?;
    let result = node.rpc.inject_tx(blob_tx).await;
    assert!(
        result.is_err(),
        "EIP-4844 blob transactions (type 0x03) must be rejected"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_tx_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let raw_tx = make_transfer_tx(wallet.chain_id, wallet.inner.clone(), 0).await;
    node.rpc.inject_tx(raw_tx.clone()).await?;
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(result.is_err(), "duplicate transaction must be rejected");
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tx_gas_limit_exceeds_block_limit_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_signer::SignerSync;
    let tx = TxEip1559 {
        chain_id: wallet.chain_id,
        nonce: 0,
        gas_limit: 30_000_001, // exceeds 30M block gas limit
        max_fee_per_gas: 20_000_000_000u128,
        max_priority_fee_per_gas: 20_000_000_000u128,
        to: alloy_primitives::TxKind::Call(alloy_primitives::Address::with_last_byte(0x42)),
        value: alloy_primitives::U256::from(100),
        access_list: Default::default(),
        input: alloy_primitives::Bytes::new(),
    };
    let sig = wallet
        .inner
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("{e}"))?;
    let envelope = morph_primitives::MorphTxEnvelope::Eip1559(tx.into_signed(sig));
    use alloy_eips::eip2718::Encodable2718;
    let raw: alloy_primitives::Bytes = envelope.encoded_2718().into();

    let result = node.rpc.inject_tx(raw).await;
    assert!(
        result.is_err(),
        "tx with gas_limit > block gas limit must be rejected"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn tx_max_fee_below_base_fee_accepted_for_queuing() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_signer::SignerSync;
    let tx = TxEip1559 {
        chain_id: wallet.chain_id,
        nonce: 0,
        gas_limit: 21_000,
        max_fee_per_gas: 500_000u128, // below base fee of 1_000_000
        max_priority_fee_per_gas: 500_000u128,
        to: alloy_primitives::TxKind::Call(alloy_primitives::Address::with_last_byte(0x42)),
        value: alloy_primitives::U256::from(100),
        access_list: Default::default(),
        input: alloy_primitives::Bytes::new(),
    };
    let sig = wallet
        .inner
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("{e}"))?;
    let envelope = morph_primitives::MorphTxEnvelope::Eip1559(tx.into_signed(sig));
    use alloy_eips::eip2718::Encodable2718;
    let raw: alloy_primitives::Bytes = envelope.encoded_2718().into();

    // reth pools low-fee txs for future execution when baseFee drops,
    // so they are accepted into the queued set, not rejected outright.
    let result = node.rpc.inject_tx(raw).await;
    assert!(
        result.is_ok(),
        "tx with maxFeePerGas < baseFee should be accepted for queuing"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_zero_eth_balance_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    use morph_node::test_utils::MorphTxBuilder;
    let poor_signer = alloy_signer_local::PrivateKeySigner::random();
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, poor_signer, 0)
        .with_v1_eth_fee()
        .build_signed()?;
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "MorphTx from zero-balance account must be rejected"
    );
    Ok(())
}
