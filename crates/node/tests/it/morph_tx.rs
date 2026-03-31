//! MorphTx (type 0x7F) integration tests.
//!
//! Tests the full lifecycle of MorphTx transactions:
//! - Pool acceptance/rejection based on version and fee type
//! - Block inclusion with fee token payment
//! - Receipt fields (version, fee_token_id, fee_rate, token_scale)
//!
//! # Test ERC20 Setup
//!
//! The test genesis (`tests/assets/test-genesis.json`) pre-deploys:
//! - L2TokenRegistry at `0x5300000000000000000000000000000000000021`
//!   with token_id=1 registered, price_ratio=1e18, decimals=18
//! - Test ERC20 at `0x5300000000000000000000000000000000000022`
//!   with 1000 tokens pre-funded for test account 0 and 1

use alloy_primitives::Address;
use morph_node::test_utils::{HardforkSchedule, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder};
use reth_payload_primitives::BuiltPayload;

use super::helpers::wallet_to_arc;

// =============================================================================
// MorphTx v1 (ETH fee) — simplest variant, no token contract needed
// =============================================================================

/// MorphTx v1 with ETH fee is accepted by the pool and included in a block.
///
/// fee_token_id=0 means ETH payment, same as EIP-1559 but the receipt
/// preserves version=1 in the MorphTx-specific fields.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_eth_fee_included_in_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Build a MorphTx v1 with ETH fee
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v1_eth_fee()
        .with_to(Address::with_last_byte(0x42))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();

    assert_eq!(
        block.body().transactions.len(),
        1,
        "MorphTx v1 should be included in block"
    );

    // Verify transaction type is 0x7F (MorphTx)

    let tx = block.body().transactions.first().unwrap();
    assert!(
        tx.is_morph_tx(),
        "transaction should be MorphTx (type 0x7F)"
    );

    Ok(())
}

/// Multiple MorphTx v1 transactions are included in sequence.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_multiple_in_sequence() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, mut wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Inject 3 MorphTx v1 (ETH fee) with sequential nonces
    for i in 0..3 {
        let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), i)
            .with_v1_eth_fee()
            .build_signed()?;
        node.rpc.inject_tx(raw_tx).await?;
        wallet.inner_nonce += 1;
    }

    let payload = node.advance_block().await?;
    assert_eq!(
        payload.block().body().transactions.len(),
        3,
        "all 3 MorphTx v1 should be included"
    );

    Ok(())
}

// =============================================================================
// MorphTx v0 (ERC20 fee) — needs L2TokenRegistry + token balance in genesis
// =============================================================================

/// MorphTx v0 with ERC20 fee is accepted and included in a block.
///
/// This test relies on the test genesis having:
/// - L2TokenRegistry with token_id=1 registered
/// - Test ERC20 with 1000 tokens pre-funded for test account 0
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_erc20_fee_included_in_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(Address::with_last_byte(0x42))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();

    assert_eq!(
        block.body().transactions.len(),
        1,
        "MorphTx v0 with ERC20 fee should be included"
    );

    let tx = block.body().transactions.first().unwrap();
    assert!(tx.is_morph_tx());
    assert_eq!(
        tx.fee_token_id(),
        Some(TEST_TOKEN_ID),
        "fee_token_id should be preserved"
    );

    Ok(())
}

/// MorphTx v1 with ERC20 fee (fee_token_id > 0, version=1).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_erc20_fee_included_in_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_to(Address::with_last_byte(0x42))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();

    assert_eq!(block.body().transactions.len(), 1);

    let tx = block.body().transactions.first().unwrap();
    assert!(tx.is_morph_tx());
    assert_eq!(tx.fee_token_id(), Some(TEST_TOKEN_ID));

    Ok(())
}

// =============================================================================
// MorphTx v1 Jade gating
// =============================================================================

/// MorphTx v1 is rejected by the pool when Jade hardfork is NOT active.
///
/// Before Jade, only MorphTx v0 is allowed. Version 1 transactions must
/// be rejected at the pool level to prevent inclusion in pre-Jade blocks.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_rejected_before_jade() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // Use PreJade schedule — Jade is NOT active
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .build_signed()?;

    // Pool should reject v1 MorphTx before Jade activation
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "MorphTx v1 should be rejected by pool before Jade"
    );

    Ok(())
}

/// MorphTx v0 (ERC20 fee) IS accepted before Jade.
///
/// Only v1 is gated — v0 has always been valid.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_accepted_before_jade() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(
        payload.block().body().transactions.len(),
        1,
        "MorphTx v0 should still be accepted pre-Jade"
    );

    Ok(())
}

// =============================================================================
// Mixed transaction types in one block
// =============================================================================

/// A block can contain both EIP-1559 and MorphTx transactions.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_tx_types_in_one_block() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let wallet_arc = wallet_to_arc(wallet);

    // Inject EIP-1559 transfer
    let eip1559_tx = {
        let mut w = wallet_arc.lock().await;
        let nonce = w.inner_nonce;
        w.inner_nonce += 1;
        morph_node::test_utils::make_transfer_tx(w.chain_id, w.inner.clone(), nonce).await
    };
    node.rpc.inject_tx(eip1559_tx).await?;

    // Inject MorphTx v1 (ETH fee)
    let morph_tx = {
        let w = wallet_arc.lock().await;
        let nonce = w.inner_nonce;
        MorphTxBuilder::new(w.chain_id, w.inner.clone(), nonce)
            .with_v1_eth_fee()
            .build_signed()?
    };
    node.rpc.inject_tx(morph_tx).await?;

    let payload = node.advance_block().await?;
    let block = payload.block();

    assert_eq!(
        block.body().transactions.len(),
        2,
        "block should have both EIP-1559 and MorphTx"
    );

    // Verify transaction types

    let types: Vec<bool> = block
        .body()
        .transactions
        .iter()
        .map(|tx| tx.is_morph_tx())
        .collect();

    assert!(
        types.contains(&false) && types.contains(&true),
        "block should contain both EIP-1559 and MorphTx"
    );

    Ok(())
}

// =============================================================================
// MorphTx pool rejection — invalid token and insufficient balance
// =============================================================================

/// MorphTx v0 with an unregistered fee_token_id (99) is rejected by the pool.
///
/// The L2TokenRegistry only has token_id=1 registered in the test genesis.
/// Token 99 does not exist, so the pool should reject the transaction.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_invalid_token_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(99)
        .build_signed()?;

    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "MorphTx with unregistered token_id=99 must be rejected"
    );

    Ok(())
}

/// MorphTx v0 from an account with zero token balance is rejected by the pool.
///
/// Account index 2 has ETH but no tokens in the test genesis. Attempting to pay
/// fees with TEST_TOKEN_ID should fail because the sender has no token balance.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_insufficient_token_balance_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    // Account 2 has ETH only, no tokens in genesis
    let signer = morph_node::test_utils::wallet_at_index(2, wallet.chain_id);

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, signer, 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .build_signed()?;

    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "MorphTx from account with no token balance must be rejected"
    );

    Ok(())
}

/// MorphTx v0 with fee_token_id=0 must be rejected (v0 requires token fee).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_fee_token_id_zero_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_raw_morph_config(
            0,
            0,
            alloy_primitives::U256::from(100_000_000_000_000_000_000u128),
        )
        .build_signed()?;
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "v0 MorphTx with fee_token_id=0 must be rejected"
    );
    Ok(())
}

// NOTE: v0 + reference / v0 + memo tests are omitted because v0's wire
// format does not encode reference/memo fields. Setting them in the builder
// has no effect — they get dropped during RLP encoding, so the pool never
// sees them. These constraints are enforced at the consensus validation
// level (TxMorph::validate_version), tested in crates/primitives unit tests.

/// MorphTx with memo > 64 bytes must be rejected (any version).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_memo_exceeds_64_bytes_rejected() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_memo(alloy_primitives::Bytes::from(vec![0xBB; 65])) // 65 bytes > 64 max
        .build_signed()?;
    let result = node.rpc.inject_tx(raw_tx).await;
    assert!(
        result.is_err(),
        "MorphTx with memo > 64 bytes must be rejected"
    );
    Ok(())
}

/// MorphTx v0 with fee_limit=0 should be accepted — the handler uses the
/// full account token balance as the effective limit.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_fee_limit_zero_accepted() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_raw_morph_config(0, TEST_TOKEN_ID, alloy_primitives::U256::ZERO) // fee_limit=0
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(
        payload.block().body().transactions.len(),
        1,
        "fee_limit=0 should be accepted"
    );
    Ok(())
}

// =============================================================================
// ERC20 token fee — balance deduction and revert behavior
// =============================================================================

/// Helper: compute the ERC20 balance storage slot for an account.
///
/// For the test token (balance mapping at slot 1):
///   slot = keccak256(address_left_padded_to_32 ++ slot_1_as_be32)
fn token_balance_slot(account: Address) -> alloy_primitives::B256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(account.as_slice());
    preimage[63] = 1; // slot 1
    alloy_primitives::keccak256(preimage)
}

/// After a successful MorphTx v0 with ERC20 fee, the sender's token balance
/// must decrease (fee was charged from tokens, not ETH).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_balance_decreases() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use reth_provider::StateProviderFactory;

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let sender = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let bal_slot = token_balance_slot(sender);

    // Token balance before
    let state_before = node.inner.provider.latest()?;
    let bal_before = state_before
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();
    assert!(
        bal_before > alloy_primitives::U256::ZERO,
        "test account must have pre-funded tokens"
    );

    // Send a MorphTx v0 with ERC20 fee (simple call, should succeed)
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(Address::with_last_byte(0x42))
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;

    // Token balance after
    let state_after = node.inner.provider.latest()?;
    let bal_after = state_after
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();

    assert!(
        bal_after < bal_before,
        "token balance must decrease after MorphTx v0 (fee deducted in tokens)"
    );

    Ok(())
}

/// Init code that deploys a contract whose runtime always reverts.
///
/// Constructor (12 bytes): CODECOPY + RETURN → deploys runtime below.
/// Runtime (5 bytes): PUSH1 0; PUSH1 0; REVERT.
const RUNTIME_REVERT_INIT: &[u8] = &[
    0x60, 0x05, // PUSH1 5 (runtime code size)
    0x60, 0x0C, // PUSH1 12 (offset of runtime in init code)
    0x60, 0x00, // PUSH1 0 (memory dest)
    0x39, // CODECOPY
    0x60, 0x05, // PUSH1 5 (return size)
    0x60, 0x00, // PUSH1 0 (return offset)
    0xf3, // RETURN
    // Runtime code (at offset 12):
    0x60, 0x00, // PUSH1 0
    0x60, 0x00, // PUSH1 0
    0xfd, // REVERT
];

/// When the main tx reverts, the ERC20 gas fee is still charged.
///
/// Scenario:
///   1. Block 1: Deploy a contract whose runtime always reverts (EIP-1559 tx)
///   2. Block 2: Call that contract with MorphTx v0 (ERC20 fee)
///   3. Verify: receipt.status = false, but token balance decreased
///
/// This exercises the handler's `validate_and_deduct_token_fee` (charges fee
/// upfront) and `reimburse_caller_token_fee` (partial refund for unused gas)
/// paths when the main transaction execution reverts.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_fee_still_charged_on_revert() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use morph_node::test_utils::{make_deploy_tx, wallet_at_index};
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let sender = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let bal_slot = token_balance_slot(sender);
    let chain_id = wallet.chain_id;

    // Token balance before any transactions
    let bal_before = node
        .inner
        .provider
        .latest()?
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();

    // Block 1: deploy the "runtime revert" contract with a standard EIP-1559 tx
    let deploy_signer = wallet_at_index(0, chain_id);
    let deploy_tx = make_deploy_tx(chain_id, deploy_signer, 0, RUNTIME_REVERT_INIT)?;
    node.rpc.inject_tx(deploy_tx).await?;
    node.advance_block().await?;

    let revert_contract = Address::create(&sender, 0);

    // Block 2: call the reverting contract with MorphTx v0 (ERC20 fee)
    let morph_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 1)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(revert_contract)
        .with_gas_limit(100_000)
        .build_signed()?;
    node.rpc.inject_tx(morph_tx).await?;
    let payload = node.advance_block().await?;

    // Verify receipt: status must be false (main tx reverted)
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .unwrap()
        .tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("receipt must exist");

    assert!(
        !receipt.status(),
        "main tx should revert (runtime REVERT contract)"
    );

    // Token balance must have decreased even though the main tx reverted.
    // Fee was deducted upfront; only unused gas is partially refunded.
    let bal_after = node
        .inner
        .provider
        .latest()?
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();

    assert!(
        bal_after < bal_before,
        "token balance must decrease even when main tx reverts \
         (fee deducted upfront, partial refund for unused gas). \
         before={bal_before}, after={bal_after}"
    );

    // The receipt should carry MorphTx-specific fee fields
    match &receipt {
        morph_primitives::MorphReceipt::Morph(morph_receipt) => {
            assert_eq!(
                morph_receipt.fee_token_id,
                Some(TEST_TOKEN_ID),
                "receipt must carry fee_token_id"
            );
            assert!(
                morph_receipt.fee_rate.is_some(),
                "receipt must carry fee_rate"
            );
            assert!(
                morph_receipt.token_scale.is_some(),
                "receipt must carry token_scale"
            );
        }
        other => panic!(
            "expected MorphReceipt::Morph variant, got {:?}",
            other.tx_type()
        ),
    }

    Ok(())
}
