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
use morph_node::test_utils::{HardforkSchedule, MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder};
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

/// Optimized runtime for:
///
/// ```solidity
/// contract Slot1Token {
///     uint256 private dummy;
///     mapping(address => uint256) public balanceOf; // slot 1
///     event Transfer(address indexed from, address indexed to, uint256 value);
///     function transfer(address to, uint256 amount) external returns (bool) { ... }
/// }
/// ```
///
/// Keeping `balanceOf` at slot 1 lets the test token use the same storage layout
/// as `tests/assets/test-genesis.json` and the token registry's direct-slot path.
const SLOT1_ERC20_RUNTIME_CODE: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb1461006a575b5f5ffd5b61005761004636600461015e565b60016020525f908152604090205481565b6040519081526020015b60405180910390f35b61007d61007836600461017e565b61008d565b6040519015158152602001610061565b335f90815260016020526040812054828110156100da5760405162461bcd60e51b815260206004820152600760248201526662616c616e636560c81b604482015260640160405180910390fd5b335f81815260016020908152604080832087860390556001600160a01b03881680845292819020805488019055518681529192917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef910160405180910390a35060019392505050565b80356001600160a01b0381168114610159575f5ffd5b919050565b5f6020828403121561016e575f5ffd5b61017782610143565b9392505050565b5f5f6040838503121561018f575f5ffd5b61019883610143565b94602093909301359350505056";

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
    reth_tracing::init_test_tracing();
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;
    use reth_provider::{ReceiptProvider, StateProviderFactory};

    const EXPECTED_GAS_USED: u64 = 48_128;
    let token_addr = morph_node::test_utils::TEST_TOKEN_ADDRESS;
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_code(token_addr, SLOT1_ERC20_RUNTIME_CODE)
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

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_to(token_addr)
        .with_data(erc20_transfer_calldata(recipient, amount))
        .with_gas_limit(100_000)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    let payload = node.advance_block().await?;

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
    assert_eq!(
        transfer_logs.len(),
        1,
        "the main ERC20 transfer should execute against the fee token contract"
    );
    assert_eq!(transfer_logs[0].topics()[1], address_topic(sender));
    assert_eq!(transfer_logs[0].topics()[2], address_topic(recipient));

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

    assert_eq!(receipt.cumulative_gas_used(), EXPECTED_GAS_USED);

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
