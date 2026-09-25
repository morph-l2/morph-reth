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

use alloy_primitives::{Address, B256, Bytes, U256};
use morph_node::test_utils::{
    HardforkSchedule, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder, sign_authorization,
    wallet_at_index,
};
use reth_payload_primitives::BuiltPayload;

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

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, mut wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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
    let (mut nodes, wallet) = TestNodeBuilder::new()
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

    let (mut nodes, wallet) = TestNodeBuilder::new()
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
// MorphTx pool rejection — invalid token and insufficient balance
// =============================================================================

/// MorphTx v0 with an unregistered fee_token_id (99) is rejected by the pool.
///
/// The L2TokenRegistry only has token_id=1 registered in the test genesis.
/// Token 99 does not exist, so the pool should reject the transaction.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_invalid_token_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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
    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

/// Build calldata for ERC20 `transfer(address,uint256)`.
fn erc20_transfer_calldata(to: Address, amount: U256) -> Bytes {
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);

    let mut address_word = [0u8; 32];
    address_word[12..].copy_from_slice(to.as_slice());
    calldata.extend_from_slice(&address_word);

    calldata.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(calldata)
}

fn erc20_transfer_topic() -> B256 {
    alloy_primitives::keccak256("Transfer(address,address,uint256)")
}

fn address_topic(address: Address) -> B256 {
    let mut topic = [0u8; 32];
    topic[12..].copy_from_slice(address.as_slice());
    B256::from(topic)
}

/// After a successful MorphTx v0 with ERC20 fee, the sender's token balance
/// must decrease (fee was charged from tokens, not ETH).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_balance_decreases() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use reth_provider::StateProviderFactory;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

/// A sender holding fee tokens but no ETH must reach `pending` and be mined.
///
/// The pool used to charge a token-fee MorphTx's `gas_limit * max_fee_per_gas` against
/// the sender's ETH balance, which left every such transaction in `queued`: never built
/// into a block, and never propagated, since reth only announces pending transactions.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_token_fee_from_zero_eth_sender_is_pending_and_mined() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use reth_provider::{AccountReader, StateProviderFactory};
    use reth_transaction_pool::TransactionPool;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let token = morph_node::test_utils::TEST_TOKEN_ADDRESS;

    // Hand a fresh account ten fee tokens and no ETH.
    let payer = alloy_signer_local::PrivateKeySigner::random();
    let payer_address = payer.address();
    let token_grant = U256::from(10u128.pow(19));
    let grant = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(token)
        .with_data(erc20_transfer_calldata(payer_address, token_grant))
        .build_signed()?;
    node.rpc.inject_tx(grant).await?;
    node.advance_block().await?;

    let balance_slot = token_balance_slot(payer_address);
    let state = node.inner.provider.latest()?;
    let eth_balance = state
        .basic_account(&payer_address)?
        .map(|account| account.balance);
    assert_eq!(eth_balance.unwrap_or_default(), U256::ZERO);
    assert_eq!(state.storage(token, balance_slot)?, Some(token_grant));

    // A v0 and a v2 MorphTx, both paying gas in the fee token.
    let v0 = MorphTxBuilder::new(wallet.chain_id, payer.clone(), 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .build_signed()?;
    let v2 = MorphTxBuilder::new(wallet.chain_id, payer, 1)
        .with_v2_token_fee(TEST_TOKEN_ID)
        .build_signed()?;
    node.rpc.inject_tx(v0).await?;
    node.rpc.inject_tx(v2).await?;
    assert_eq!(
        node.inner.pool.pending_and_queued_txn_count(),
        (2, 0),
        "token-fee transactions from a sender without ETH must be pending"
    );

    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 2);

    let state = node.inner.provider.latest()?;
    let account = state
        .basic_account(&payer_address)?
        .expect("the payer exists once its transactions are mined");
    assert_eq!(account.nonce, 2);
    assert_eq!(account.balance, U256::ZERO);
    let tokens_left = state.storage(token, balance_slot)?.unwrap_or_default();
    assert!(
        tokens_left < token_grant,
        "gas must be charged in the fee token"
    );

    Ok(())
}

/// Regression for the mainnet block 19720219 shape:
///
/// - tx `0xc267450129e51457a280fa82c74364d312e47885c09d15c78f6a0895844913c9`
/// - block `0xfbd17c5a73553cbd71f4654c189759a6262e6e52e76a19d760da4ab2b4e98a52`
/// - mainnet gas used: 59_335
///
/// The important shape is not the exact mainnet state, but that the MorphTx pays
/// fees in the same ERC20 contract it calls. Fee deduction touches the sender's
/// balance slot before the main ERC20 `transfer` SLOAD/SSTORE pair, so this
/// catches regressions in the `sload_morph`, `sstore_morph`, and reimburse
/// cold/warm-state handling.
///
/// `EXPECTED_GAS_USED = 48_128` is the sandbox golden, NOT the mainnet
/// 59_335. The sandbox uses a minimal hand-written ERC20 with one
/// storage slot per `transfer`, while the mainnet token's compiled
/// bytecode does extra checks; initial balances and call data sizes also
/// differ. What's locked is the bug-vs-fix delta: a regression in
/// `sload_morph`/`sstore_morph` causes the main tx's SSTORE on
/// `sender.balanceOf` to be charged 2900 (SSTORE_RESET) instead of 100
/// (dirty), pushing `cumulative_gas_used` ~2800 above the golden and
/// tripping this assertion before the change reaches mainnet.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_fee_transfer_to_fee_token_contract_gas_regression() -> eyre::Result<()> {
    token_fee_transfer_gas_regression(HardforkSchedule::AllActive, false, 100_000, 48_128).await
}

/// Access-list warming must not replace the pre-fee original balance with the
/// post-fee balance. The sender SSTORE remains dirty (100 gas, not 2,900).
/// Adding one address and one slot costs 4,300 intrinsic gas and saves 2,000
/// on the sender's SLOAD: 48,128 + 4,300 - (2,100 - 100) = 50,428.
/// A 51,000 limit also catches the old behavior as a failed transfer (OOG).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_fee_access_list_gas_regression() -> eyre::Result<()> {
    for schedule in [HardforkSchedule::PreJade, HardforkSchedule::AllActive] {
        for gas_limit in [100_000, 51_000] {
            token_fee_transfer_gas_regression(schedule, true, gas_limit, 50_428).await?;
        }
    }
    Ok(())
}

async fn token_fee_transfer_gas_regression(
    schedule: HardforkSchedule,
    access_list: bool,
    gas_limit: u64,
    expected_gas_used: u64,
) -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_schedule(schedule)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let sender = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    let recipient = Address::with_last_byte(0x99);
    let fee_vault = alloy_primitives::address!("530000000000000000000000000000000000000a");
    let amount = U256::from(100);

    let sender_slot = token_balance_slot(sender);
    let recipient_slot = token_balance_slot(recipient);
    let fee_vault_slot = token_balance_slot(fee_vault);

    let state_before = node.inner.provider.latest()?;
    let sender_before = state_before
        .storage(token_addr, sender_slot)?
        .unwrap_or_default();
    let recipient_before = state_before
        .storage(token_addr, recipient_slot)?
        .unwrap_or_default();
    let fee_vault_before = state_before
        .storage(token_addr, fee_vault_slot)?
        .unwrap_or_default();

    let access_list = if access_list {
        alloy_eips::eip2930::AccessList(vec![alloy_eips::eip2930::AccessListItem {
            address: token_addr,
            storage_keys: vec![sender_slot],
        }])
    } else {
        Default::default()
    };
    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(token_addr)
        .with_data(erc20_transfer_calldata(recipient, amount))
        .with_fees(20_000_000_000, 20_000_000_000)
        .with_gas_limit(gas_limit)
        .with_access_list(access_list)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;

    assert_eq!(payload.block().body().transactions.len(), 1);
    assert_eq!(payload.block().header().inner.gas_used, expected_gas_used);

    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .expect("block should contain regression tx")
        .tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("receipt must exist");

    assert!(receipt.status(), "ERC20 transfer must succeed");

    let transfer_topic = erc20_transfer_topic();
    let transfer_logs: Vec<_> = receipt
        .logs()
        .iter()
        .filter(|log| log.address == token_addr && log.topics().first() == Some(&transfer_topic))
        .collect();
    // In call mode the fee is moved by real ERC20 calls, so the transaction's own
    // transfer arrives bracketed by them, in go-ethereum's order: deduction, main,
    // reimbursement.
    assert_eq!(
        transfer_logs.len(),
        3,
        "receipt should carry the fee deduction, the main transfer and the fee refund"
    );
    assert_eq!(transfer_logs[0].topics()[1], address_topic(sender));
    assert_eq!(transfer_logs[0].topics()[2], address_topic(fee_vault));
    assert_eq!(transfer_logs[1].topics()[1], address_topic(sender));
    assert_eq!(transfer_logs[1].topics()[2], address_topic(recipient));
    assert_eq!(
        transfer_logs[1].data.data.as_ref(),
        amount.to_be_bytes::<32>()
    );
    assert_eq!(transfer_logs[2].topics()[1], address_topic(fee_vault));
    assert_eq!(transfer_logs[2].topics()[2], address_topic(sender));

    let state_after = node.inner.provider.latest()?;
    let sender_after = state_after
        .storage(token_addr, sender_slot)?
        .unwrap_or_default();
    let recipient_after = state_after
        .storage(token_addr, recipient_slot)?
        .unwrap_or_default();
    let fee_vault_after = state_after
        .storage(token_addr, fee_vault_slot)?
        .unwrap_or_default();

    let sender_delta = sender_before - sender_after;
    let recipient_delta = recipient_after - recipient_before;
    let fee_vault_delta = fee_vault_after - fee_vault_before;

    assert_eq!(recipient_delta, amount);
    assert!(
        fee_vault_delta > U256::ZERO,
        "fee vault should keep the net charged token fee"
    );
    assert_eq!(
        sender_delta,
        amount + fee_vault_delta,
        "sender should only lose the main transfer amount plus net token fee"
    );

    let morph_primitives::MorphReceipt::Morph(morph_receipt) = &receipt else {
        panic!("expected a Morph receipt");
    };
    // The fixture's 18-decimal token has a 1:1 conversion rate, so there is
    // no conversion rounding. Settlement must use actual gas, not the limit.
    let scale = U256::from(1_000_000_000_000_000_000u128);
    assert_eq!(morph_receipt.fee_rate, Some(scale));
    assert_eq!(morph_receipt.token_scale, Some(scale));
    assert_eq!(
        fee_vault_delta,
        U256::from(expected_gas_used) * U256::from(20_000_000_000u64) + morph_receipt.l1_fee,
        "net token fee must equal execution gas fee plus L1 data fee"
    );
    assert_eq!(receipt.cumulative_gas_used(), expected_gas_used);

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
///   4. Verify: the receipt still carries both fee `Transfer` events
///
/// This exercises the handler's `validate_and_deduct_token_fee` (charges fee
/// upfront) and `reimburse_caller_token_fee` (partial refund for unused gas)
/// paths when the main transaction execution reverts.
///
/// The log assertion is the point of running the fee path on a *reverting* main
/// frame. go-ethereum keeps `StateDB.logs` outside the state snapshot/revert
/// mechanism, so the deduction's `Transfer` survives a main-frame revert; that
/// is the entire reason morph-reth caches fee logs in `pre_fee_logs` /
/// `post_fee_logs` instead of leaving them in the journal (`crates/evm/src/block/receipt.rs`).
/// A regression there -- the fee logs dropped, or restored into the reverted
/// frame -- changes the receipt's logs and therefore the block's receipts root,
/// and no state assertion in this test would notice. This is the only test that
/// runs the production receipt builder against a reverting main frame.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v0_token_fee_still_charged_on_revert() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use morph_node::test_utils::{make_deploy_tx, wallet_at_index};
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
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

    // Both fee transfers must survive the main frame's revert: go-ethereum keeps
    // `StateDB.logs` outside the state snapshot/revert mechanism, so the deduction's
    // `Transfer` is still in the receipt while the reverted main frame contributes
    // none. Order is go-ethereum's: deduction, (empty) main frame, reimbursement.
    let fee_vault = morph_node::test_utils::TEST_FEE_VAULT_ADDRESS;
    let transfer_topic = erc20_transfer_topic();
    let transfer_logs: Vec<_> = receipt
        .logs()
        .iter()
        .filter(|log| log.address == token_addr && log.topics().first() == Some(&transfer_topic))
        .collect();
    assert_eq!(
        transfer_logs.len(),
        2,
        "receipt must carry the fee deduction and the fee reimbursement even though \
         the main frame reverted; dropping the deduction's log changes the receipts root. \
         got {transfer_logs:?}"
    );
    assert_eq!(
        (transfer_logs[0].topics()[1], transfer_logs[0].topics()[2]),
        (address_topic(sender), address_topic(fee_vault)),
        "first log must be the fee deduction (sender -> fee vault)"
    );
    assert_ne!(
        transfer_logs[0].data.data.as_ref(),
        [0u8; 32],
        "the deduction must move a non-zero fee"
    );
    assert_eq!(
        (transfer_logs[1].topics()[1], transfer_logs[1].topics()[2]),
        (address_topic(fee_vault), address_topic(sender)),
        "second log must be the fee reimbursement (fee vault -> sender)"
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

// =============================================================================
// MorphTx v2 (EIP-7702 authorization list) — Celadon gating and delegation
// =============================================================================

/// Asserts that `authority` is delegated to `delegate` (`0xef0100 || delegate`)
/// and returns its nonce.
fn assert_delegated(
    state: &dyn reth_provider::StateProvider,
    authority: Address,
    delegate: Address,
) -> eyre::Result<u64> {
    let account = state
        .basic_account(&authority)?
        .ok_or_else(|| eyre::eyre!("authority account {authority} must exist"))?;
    let code = state
        .account_code(&authority)?
        .ok_or_else(|| eyre::eyre!("delegation designator must be written"))?;
    let code_bytes = code.original_bytes();
    assert_eq!(
        &code_bytes[..3],
        &[0xef, 0x01, 0x00],
        "authority code must be an EIP-7702 delegation designator"
    );
    assert_eq!(
        &code_bytes[3..],
        delegate.as_slice(),
        "delegation must point at the authorized address"
    );
    Ok(account.nonce)
}

/// MorphTx v2 with ETH fee applies its authorization list exactly like an
/// EIP-7702 transaction: the authority is delegated, its nonce is consumed,
/// and the receipt reports version 2.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_eth_fee_applies_delegation() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    // Account 1 authorizes a delegation to 0x42; account 0 carries it in a MorphTx v2.
    let authority_signer = wallet_at_index(1, chain_id);
    let authority = authority_signer.address();
    let delegate = Address::with_last_byte(0x42);
    let authorization = sign_authorization(&authority_signer, chain_id, delegate, 0)?;

    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();
    assert_eq!(
        block.body().transactions.len(),
        1,
        "MorphTx v2 should be included in block"
    );
    let tx = block.body().transactions.first().unwrap();
    assert!(tx.is_morph_tx());
    assert_eq!(tx.version(), Some(2));

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status(), "MorphTx v2 call must succeed");
    // Intrinsic gas: 21_000 base + 25_000 per authorization = 46_000. The
    // authority already exists in genesis, so the EIP-7702 refund of 12_500
    // applies, capped by EIP-3529 at gas_used / 5 = 9_200 → 36_800.
    assert_eq!(
        receipt.cumulative_gas_used(),
        36_800,
        "ETH-fee path must settle the EIP-7702 refund like 0x04"
    );
    let morph_primitives::MorphReceipt::Morph(morph_receipt) = &receipt else {
        panic!("expected a Morph receipt");
    };
    assert_eq!(morph_receipt.version, Some(2));

    let state = node.inner.provider.latest()?;
    let nonce = assert_delegated(&*state, authority, delegate)?;
    assert_eq!(nonce, 1, "delegation consumes the authority nonce");

    Ok(())
}

/// Two tuples for two different authorities are both applied; the intrinsic
/// gas and the refund scale with the list length (refund capped at gas/5).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_applies_multiple_authorities_in_one_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    let authority_1 = wallet_at_index(1, chain_id);
    let authority_2 = wallet_at_index(2, chain_id);
    let delegate_1 = Address::with_last_byte(0x42);
    let delegate_2 = Address::with_last_byte(0x43);
    let authorizations = vec![
        sign_authorization(&authority_1, chain_id, delegate_1, 0)?,
        sign_authorization(&authority_2, chain_id, delegate_2, 0)?,
    ];

    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(authorizations)
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let tx = payload.block().body().transactions.first().unwrap();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status());
    // 21_000 + 2 × 25_000 = 71_000; refund 2 × 12_500 = 25_000 capped at 71_000 / 5 = 14_200.
    assert_eq!(receipt.cumulative_gas_used(), 56_800);

    let state = node.inner.provider.latest()?;
    assert_eq!(
        assert_delegated(&*state, authority_1.address(), delegate_1)?,
        1
    );
    assert_eq!(
        assert_delegated(&*state, authority_2.address(), delegate_2)?,
        1
    );

    Ok(())
}

/// A sender delegating itself must sign the tuple with `tx.nonce + 1`, since
/// the transaction nonce is consumed before the list is applied.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_sender_self_delegation_uses_nonce_plus_one() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use reth_provider::StateProviderFactory;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();
    let delegate = Address::with_last_byte(0x42);

    let authorization = sign_authorization(&wallet.inner, chain_id, delegate, 1)?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);

    let state = node.inner.provider.latest()?;
    let nonce = assert_delegated(&*state, sender, delegate)?;
    assert_eq!(nonce, 2, "tx nonce + authorization nonce both consumed");

    Ok(())
}

/// MorphTx v2 with ERC20 fee: the delegation is applied and the fee (including
/// the per-authorization intrinsic gas) is charged in tokens.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_token_fee_applies_delegation_and_charges_tokens() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    let sender = wallet.inner.address();
    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let fee_vault = alloy_primitives::address!("530000000000000000000000000000000000000a");
    let bal_slot = token_balance_slot(sender);
    let fee_vault_slot = token_balance_slot(fee_vault);
    let state_before = node.inner.provider.latest()?;
    let bal_before = state_before
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();
    let fee_vault_before = state_before
        .storage(token_addr, fee_vault_slot)?
        .unwrap_or_default();

    let authority_signer = wallet_at_index(1, chain_id);
    let authority = authority_signer.address();
    let delegate = Address::with_last_byte(0x42);
    let authorization = sign_authorization(&authority_signer, chain_id, delegate, 0)?;

    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_token_fee(TEST_TOKEN_ID)
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
        .with_fees(20_000_000_000, 20_000_000_000)
        .build_signed()?;

    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();
    assert_eq!(block.body().transactions.len(), 1);
    let tx = block.body().transactions.first().unwrap();
    assert_eq!(tx.fee_token_id(), Some(TEST_TOKEN_ID));

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status());
    // Intrinsic gas: 21_000 base + 25_000 per authorization = 46_000. The
    // authority already exists in genesis, so the EIP-7702 refund of 12_500
    // applies, capped by EIP-3529 at gas_used / 5 = 9_200 → 36_800.
    assert_eq!(
        receipt.cumulative_gas_used(),
        36_800,
        "gas used must include the per-authorization intrinsic cost minus the capped refund"
    );
    let morph_primitives::MorphReceipt::Morph(morph_receipt) = &receipt else {
        panic!("expected a Morph receipt");
    };
    assert_eq!(morph_receipt.version, Some(2));
    assert_eq!(morph_receipt.fee_token_id, Some(TEST_TOKEN_ID));

    let state = node.inner.provider.latest()?;
    assert_delegated(&*state, authority, delegate)?;
    let bal_after = state.storage(token_addr, bal_slot)?.unwrap_or_default();
    let fee_vault_after = state
        .storage(token_addr, fee_vault_slot)?
        .unwrap_or_default();
    assert!(
        bal_after < bal_before,
        "token balance must decrease (fee paid in tokens)"
    );

    // The fixture token converts 1:1, so the net token fee must be exactly the
    // post-refund gas used × gas price + L1 data fee: the EIP-7702 refund has
    // to flow through the token reimbursement path, not only the ETH one.
    let scale = U256::from(1_000_000_000_000_000_000u128);
    assert_eq!(morph_receipt.fee_rate, Some(scale));
    assert_eq!(morph_receipt.token_scale, Some(scale));
    let fee_vault_delta = fee_vault_after - fee_vault_before;
    assert_eq!(
        fee_vault_delta,
        U256::from(36_800u64) * U256::from(20_000_000_000u64) + morph_receipt.l1_fee,
        "net token fee must equal post-refund gas used × price plus the L1 data fee"
    );
    assert_eq!(
        bal_before - bal_after,
        fee_vault_delta,
        "sender loses exactly the net token fee"
    );

    Ok(())
}

/// A V2 call that reverts still applies the delegation (it is applied before
/// the call frame, like 0x04) and still pays the token fee.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_reverting_call_still_applies_delegation_and_charges_tokens() -> eyre::Result<()>
{
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use morph_node::test_utils::make_deploy_tx;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();
    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let bal_slot = token_balance_slot(sender);

    // Block 1: deploy a contract whose runtime always reverts.
    let deploy_tx = make_deploy_tx(chain_id, wallet.inner.clone(), 0, RUNTIME_REVERT_INIT)?;
    node.rpc.inject_tx(deploy_tx).await?;
    node.advance_block().await?;
    let revert_contract = Address::create(&sender, 0);

    let bal_before = node
        .inner
        .provider
        .latest()?
        .storage(token_addr, bal_slot)?
        .unwrap_or_default();

    // Block 2: V2 token-fee call into the reverting contract, carrying a delegation.
    let authority_signer = wallet_at_index(1, chain_id);
    let authority = authority_signer.address();
    let delegate = Address::with_last_byte(0x42);
    let authorization = sign_authorization(&authority_signer, chain_id, delegate, 0)?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 1)
        .with_v2_token_fee(TEST_TOKEN_ID)
        .with_authorization_list(vec![authorization])
        .with_to(revert_contract)
        .with_gas_limit(100_000)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;

    let tx = payload.block().body().transactions.first().unwrap();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(!receipt.status(), "call must revert");

    let state = node.inner.provider.latest()?;
    assert_eq!(
        assert_delegated(&*state, authority, delegate)?,
        1,
        "delegation survives the reverted call"
    );
    let bal_after = state.storage(token_addr, bal_slot)?.unwrap_or_default();
    assert!(
        bal_after < bal_before,
        "token fee is still charged when the call reverts"
    );

    Ok(())
}

/// After a self-delegation the sender's account carries code; both fee paths
/// must keep accepting its MorphTxs (EIP-3607 exempts delegation designators).
#[tokio::test(flavor = "multi_thread")]
async fn delegated_sender_can_keep_sending_morph_txs() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();
    let delegate = Address::with_last_byte(0x42);

    // Block 1: self-delegate (tx nonce 0, authorization nonce 1).
    let authorization = sign_authorization(&wallet.inner, chain_id, delegate, 1)?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;
    let state = node.inner.provider.latest()?;
    assert_eq!(assert_delegated(&*state, sender, delegate)?, 2);

    // Block 2: ETH-fee MorphTx v1 from the delegated sender.
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 2)
        .with_v1_eth_fee()
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*payload.block().body().transactions[0].tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status());

    // Block 3: token-fee MorphTx v0 from the delegated sender.
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 3)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*payload.block().body().transactions[0].tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status());

    let state = node.inner.provider.latest()?;
    assert_eq!(
        assert_delegated(&*state, sender, delegate)?,
        4,
        "delegation stays in place across later transactions"
    );

    Ok(())
}

/// The pool's EIP-7702 authority tracking applies to MorphTx v2: an authority
/// that already has more in-flight transactions than the delegated slot limit
/// cannot be referenced by a new authorization (`AuthorityReserved`).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_authorization_for_busy_authority_is_rejected_by_pool() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use morph_node::test_utils::make_transfer_tx;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    // Account 1 has two transactions in flight (above the default slot limit of 1).
    let authority_signer = wallet_at_index(1, chain_id);
    for nonce in 0..2 {
        let raw_tx = make_transfer_tx(chain_id, authority_signer.clone(), nonce).await;
        node.rpc.inject_tx(raw_tx).await?;
    }

    // Account 0 now tries to carry a delegation signed by account 1.
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        2,
    )?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .build_signed()?;

    let err = node
        .rpc
        .inject_tx(raw_tx)
        .await
        .expect_err("authorization for an authority with two in-flight txs must be rejected");
    assert!(
        err.to_string().contains("authority already reserved"),
        "unexpected error: {err}"
    );

    Ok(())
}

/// A pending MorphTx v2 authorization reserves the authority: the authority may
/// keep only the delegated in-flight slot limit (1) of its own transactions.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_pending_authorization_limits_authority_inflight_txs() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use morph_node::test_utils::make_transfer_tx;

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    // Account 0's pending V2 carries a delegation signed by account 1.
    let authority_signer = wallet_at_index(1, chain_id);
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        0,
    )?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;

    // Account 1 may still use its single delegated slot ...
    let first = make_transfer_tx(chain_id, authority_signer.clone(), 0).await;
    node.rpc.inject_tx(first).await?;

    // ... but not a second in-flight transaction.
    let second = make_transfer_tx(chain_id, authority_signer.clone(), 1).await;
    let err = node
        .rpc
        .inject_tx(second)
        .await
        .expect_err("second in-flight tx from a pending authority must be rejected");
    assert!(
        err.to_string()
            .contains("in-flight transaction limit reached"),
        "unexpected error: {err}"
    );

    Ok(())
}

/// MorphTx v2 is rejected by the pool while Celadon is not active.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_rejected_before_celadon() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreCeladon)
        .build()
        .await?;
    let node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;

    let authority_signer = wallet_at_index(1, chain_id);
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        0,
    )?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .build_signed()?;

    let err = node
        .rpc
        .inject_tx(raw_tx)
        .await
        .expect_err("MorphTx v2 should be rejected by pool before Celadon");
    assert!(
        err.to_string().contains("not yet active"),
        "unexpected error: {err}"
    );

    Ok(())
}

/// MorphTx v1 keeps working after Celadon (only v2 is new).
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v1_still_accepted_after_celadon() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);

    Ok(())
}

/// MorphTx v2 without authorizations is accepted and executes exactly like a
/// v1 transaction: plain call cost, no delegation, receipt `version` 0x2, and
/// `authorizationList: []` in the RPC transaction object.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_without_authorizations_executes_like_v1() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use jsonrpsee::core::client::ClientT;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let sender = wallet.inner.address();

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_to(Address::with_last_byte(0x99))
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let block = payload.block();
    assert_eq!(
        block.body().transactions.len(),
        1,
        "MorphTx v2 without authorizations should be included in block"
    );
    let tx = block.body().transactions.first().unwrap();
    assert!(tx.is_morph_tx());
    assert_eq!(tx.version(), Some(2));

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status(), "plain v2 call must succeed");
    assert_eq!(
        receipt.cumulative_gas_used(),
        21_000,
        "no authorizations: plain call cost, no 7702 gas or refund"
    );
    let morph_primitives::MorphReceipt::Morph(morph_receipt) = &receipt else {
        panic!("expected a Morph receipt");
    };
    assert_eq!(morph_receipt.version, Some(2));

    // Nothing was delegated: the sender stays a plain EOA.
    let state = node.inner.provider.latest()?;
    assert!(
        state
            .account_code(&sender)?
            .is_none_or(|code| code.is_empty()),
        "sender must not carry any code"
    );

    // RPC transaction object: version 0x2 and an explicit empty list, the same
    // shape go-ethereum returns.
    let client = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let rpc_tx: serde_json::Value = client
        .request("eth_getTransactionByHash", (*tx.tx_hash(),))
        .await?;
    assert_eq!(rpc_tx["type"].as_str(), Some("0x7f"));
    assert_eq!(rpc_tx["version"].as_str(), Some("0x2"));
    assert_eq!(
        rpc_tx["authorizationList"],
        serde_json::json!([]),
        "an empty v2 list is emitted as [] in the RPC transaction object"
    );

    Ok(())
}

/// Without authorizations a v2 keeps v1's ability to create contracts: the
/// no-CREATE rule only applies to a non-empty authorization list, and the same
/// CREATE with an authorization attached is rejected by the pool.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_without_authorizations_can_create_contract() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use super::helpers::{RETURN_WORD_42_RUNTIME, init_code_for};
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();

    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 0)
        .with_v2_eth_fee()
        .with_create(init_code_for(RETURN_WORD_42_RUNTIME))
        .with_gas_limit(200_000)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let tx = payload
        .block()
        .body()
        .transactions
        .first()
        .expect("v2 CREATE without authorizations should be included");
    assert_eq!(tx.version(), Some(2));
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*tx.tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status(), "v2 CREATE must succeed");

    let contract = sender.create(0);
    let state = node.inner.provider.latest()?;
    let code = state
        .account_code(&contract)?
        .expect("contract code must be deployed");
    assert_eq!(code.original_bytes().as_ref(), RETURN_WORD_42_RUNTIME);

    // The same CREATE carrying an authorization is rejected up front.
    let authority_signer = wallet_at_index(1, chain_id);
    let authorization = sign_authorization(
        &authority_signer,
        chain_id,
        Address::with_last_byte(0x42),
        0,
    )?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 1)
        .with_v2_eth_fee()
        .with_create(init_code_for(RETURN_WORD_42_RUNTIME))
        .with_gas_limit(200_000)
        .with_authorization_list(vec![authorization])
        .build_signed()?;
    let err = node
        .rpc
        .inject_tx(raw_tx)
        .await
        .expect_err("v2 CREATE with authorizations must be rejected");
    assert!(
        err.to_string().contains("cannot create a contract"),
        "unexpected error: {err}"
    );

    Ok(())
}

/// A self-delegating V2 whose call targets the sender itself runs the delegate's
/// code in the same transaction: the list is applied (after the tx nonce bump)
/// before the call frame, so the sender already carries `0xef0100 || delegate`
/// when it is called, and the delegate's log is emitted from the sender's address.
#[tokio::test(flavor = "multi_thread")]
async fn morph_tx_v2_self_delegation_executes_delegate_code_in_same_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use super::helpers::{LOG_WORD_42_RUNTIME, init_code_for};
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use morph_node::test_utils::make_deploy_tx;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let chain_id = wallet.chain_id;
    let sender = wallet.inner.address();

    // Block 1: deploy the logging delegate.
    let deploy_tx = make_deploy_tx(
        chain_id,
        wallet.inner.clone(),
        0,
        init_code_for(LOG_WORD_42_RUNTIME),
    )?;
    node.rpc.inject_tx(deploy_tx).await?;
    node.advance_block().await?;
    let delegate = Address::create(&sender, 0);

    // Block 2: tx nonce 1, authorization nonce 2, call the sender itself.
    let authorization = sign_authorization(&wallet.inner, chain_id, delegate, 2)?;
    let raw_tx = MorphTxBuilder::new(chain_id, wallet.inner.clone(), 1)
        .with_v2_eth_fee()
        .with_authorization_list(vec![authorization])
        .with_to(sender)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 1);

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(*payload.block().body().transactions[0].tx_hash())?
        .expect("receipt must exist");
    assert!(receipt.status(), "delegated code must run successfully");
    let logs = receipt.logs();
    assert_eq!(
        logs.len(),
        1,
        "delegate code must have run inside the same tx"
    );
    assert_eq!(
        logs[0].address, sender,
        "delegated code executes in the sender's own context"
    );
    assert_eq!(
        logs[0].data.data.as_ref(),
        U256::from(0x42u64).to_be_bytes::<32>()
    );

    // deploy (0 → 1), V2 tx nonce (1 → 2), self-authorization (2 → 3)
    let state = node.inner.provider.latest()?;
    assert_eq!(assert_delegated(&*state, sender, delegate)?, 3);

    Ok(())
}
