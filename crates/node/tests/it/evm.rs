//! EVM execution E2E tests.
//!
//! Verifies correct EVM behavior in real blocks:
//! - Contract deployment and storage reads
//! - Constructor revert handling
//! - BLOCKHASH custom Morph semantics
//! - SELFDESTRUCT opcode behavior
//! - L1 fee calculation for calldata

use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{Address, B256, U256, keccak256};
use morph_node::test_utils::{MorphTxBuilder, TestNodeBuilder, make_deploy_tx, wallet_at_index};
use reth_payload_primitives::BuiltPayload;
use reth_provider::{ReceiptProvider, StateProviderFactory};

// =============================================================================
// Bytecode constants
// =============================================================================

/// Init code: stores 42 at slot 0, returns empty runtime code.
///
/// PUSH1 42; PUSH1 0; SSTORE; PUSH1 0; PUSH1 0; RETURN
const STORE_42: &[u8] = &[
    0x60, 0x2a, // PUSH1 42
    0x60, 0x00, // PUSH1 0 (slot)
    0x55, // SSTORE
    0x60, 0x00, // PUSH1 0 (return length)
    0x60, 0x00, // PUSH1 0 (return offset)
    0xf3, // RETURN → empty runtime code
];

/// Init code: reads BLOCKHASH(NUMBER-1), stores result at slot 0, returns empty runtime code.
///
/// PUSH1 1; NUMBER; SUB; BLOCKHASH; PUSH1 0; SSTORE; PUSH1 0; PUSH1 0; RETURN
const STORE_BLOCKHASH: &[u8] = &[
    0x60, 0x01, // PUSH1 1
    0x43, // NUMBER
    0x03, // SUB → block.number - 1
    0x40, // BLOCKHASH
    0x60, 0x00, // PUSH1 0 (slot)
    0x55, // SSTORE
    0x60, 0x00, // PUSH1 0 (return length)
    0x60, 0x00, // PUSH1 0 (return offset)
    0xf3, // RETURN
];

/// Init code: constructor always REVERTs.
///
/// PUSH1 0; PUSH1 0; REVERT
const REVERT_ALWAYS: &[u8] = &[
    0x60, 0x00, // PUSH1 0 (revert length)
    0x60, 0x00, // PUSH1 0 (revert offset)
    0xfd, // REVERT
];

/// Init code: deploys a contract whose runtime code calls SELFDESTRUCT(address(0)).
///
/// Constructor copies 3 bytes of runtime code (PUSH1 0; SELFDESTRUCT) into memory
/// and returns them as the deployed bytecode.
///
/// Init code layout (15 bytes total):
///   bytes 0..12:  constructor (CODECOPY + RETURN)
///   bytes 12..15: runtime code [PUSH1 0; SELFDESTRUCT]
const SELFDESTRUCT_CONTRACT_INIT: &[u8] = &[
    // Constructor: copy runtime code into memory and return it
    0x60, 0x03, // PUSH1 3 (runtime code size)
    0x60, 0x0c, // PUSH1 12 (offset of runtime code within init code)
    0x60, 0x00, // PUSH1 0 (memory destination)
    0x39, // CODECOPY
    0x60, 0x03, // PUSH1 3 (return size)
    0x60, 0x00, // PUSH1 0 (return offset)
    0xf3, // RETURN
    // Runtime code (at offset 12):
    0x60, 0x00, // PUSH1 0 (beneficiary = address(0))
    0xff, // SELFDESTRUCT
];

// =============================================================================
// Helpers
// =============================================================================

/// Chain ID used in the test genesis.
const TEST_CHAIN_ID: u64 = 2910;

/// Address of the first test account (funded in test genesis).
const ACCOUNT0: Address = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");

// =============================================================================
// Tests
// =============================================================================

/// Deploying a contract that writes to storage: the value is visible via the state provider.
#[tokio::test(flavor = "multi_thread")]
async fn contract_deploy_stores_state() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let signer = wallet_at_index(0, TEST_CHAIN_ID);
    let raw_tx = make_deploy_tx(TEST_CHAIN_ID, signer, 0, STORE_42)?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
    let block = payload.block();

    // Verify the deploy tx was included
    assert_eq!(block.body().transactions.len(), 1);

    // Contract address: CREATE(ACCOUNT0, nonce=0)
    let contract_addr = Address::create(&ACCOUNT0, 0);

    // Read slot 0 from the deployed contract
    let state = node.inner.provider.latest()?;
    let slot_val = state
        .storage(contract_addr, B256::ZERO)?
        .unwrap_or_default();

    assert_eq!(
        slot_val,
        U256::from(42),
        "contract slot 0 must be 42 after deployment"
    );

    Ok(())
}

/// A constructor that REVERTs: the receipt status is false, gas is consumed.
#[tokio::test(flavor = "multi_thread")]
async fn contract_revert_receipt_status_false() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let signer = wallet_at_index(0, TEST_CHAIN_ID);
    let raw_tx = make_deploy_tx(TEST_CHAIN_ID, signer, 0, REVERT_ALWAYS)?;
    node.rpc.inject_tx(raw_tx).await?;

    let payload = node.advance_block().await?;
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
        .expect("receipt must exist after block import");

    use alloy_consensus::TxReceipt;
    assert!(
        !receipt.status(),
        "constructor revert → receipt status must be false"
    );
    assert!(
        receipt.as_receipt().cumulative_gas_used > 0,
        "gas must be consumed even for failed deployment"
    );

    Ok(())
}

/// Contract state written in block N is readable from block N+1.
#[tokio::test(flavor = "multi_thread")]
async fn contract_state_persists_across_blocks() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();
    use morph_node::test_utils::advance_empty_block;

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Block 1: deploy the contract
    let signer = wallet_at_index(0, TEST_CHAIN_ID);
    let raw_tx = make_deploy_tx(TEST_CHAIN_ID, signer, 0, STORE_42)?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;

    // Block 2: empty block
    advance_empty_block(&mut node).await?;

    // Slot 0 should still hold 42 after another block
    let contract_addr = Address::create(&ACCOUNT0, 0);
    let state = node.inner.provider.latest()?;
    let slot_val = state
        .storage(contract_addr, B256::ZERO)?
        .unwrap_or_default();

    assert_eq!(
        slot_val,
        U256::from(42),
        "contract state must persist across blocks"
    );

    Ok(())
}

/// BLOCKHASH opcode inside a constructor returns the Morph custom keccak256 value.
///
/// Morph's BLOCKHASH formula: keccak256(chain_id_be8 || block_number_be8)
/// At block 1, BLOCKHASH(0) = keccak256(2910u64 BE || 0u64 BE)
#[tokio::test(flavor = "multi_thread")]
async fn blockhash_opcode_returns_morph_custom_value() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Deploy STORE_BLOCKHASH in block 1.
    // Constructor executes BLOCKHASH(NUMBER - 1) = BLOCKHASH(0).
    let signer = wallet_at_index(0, TEST_CHAIN_ID);
    let raw_tx = make_deploy_tx(TEST_CHAIN_ID, signer, 0, STORE_BLOCKHASH)?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;

    let contract_addr = Address::create(&ACCOUNT0, 0);
    let state = node.inner.provider.latest()?;
    let stored = state
        .storage(contract_addr, B256::ZERO)?
        .unwrap_or_default();

    // Expected: keccak256(2910u64 BE || 0u64 BE) as U256
    // This matches morph_blockhash_value(chain_id=2910, number=0) in crates/revm/src/evm.rs
    let mut hash_input = [0u8; 16];
    hash_input[..8].copy_from_slice(&TEST_CHAIN_ID.to_be_bytes());
    // hash_input[8..] stays zero (block number = 0)
    let expected = U256::from_be_bytes(*keccak256(hash_input));

    assert_eq!(
        stored, expected,
        "BLOCKHASH(0) at block 1 must match Morph custom keccak formula"
    );

    Ok(())
}

/// SELFDESTRUCT opcode (0xff) is disabled in Morph — calls result in a failed receipt.
///
/// Morph's EVM replaces SELFDESTRUCT with `Instruction::unknown()` to match
/// go-ethereum behavior. A contract that executes SELFDESTRUCT will halt with an
/// error, producing a receipt with status=false. Crucially, this must not panic.
#[tokio::test(flavor = "multi_thread")]
async fn selfdestruct_opcode_disabled() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let signer = wallet_at_index(0, TEST_CHAIN_ID);

    // Block 1: deploy the SELFDESTRUCT contract (constructor itself doesn't call SELFDESTRUCT,
    // so deployment should succeed — it just returns the runtime code)
    let raw_deploy = make_deploy_tx(TEST_CHAIN_ID, signer.clone(), 0, SELFDESTRUCT_CONTRACT_INIT)?;
    node.rpc.inject_tx(raw_deploy).await?;
    node.advance_block().await?;

    let contract_addr = Address::create(&ACCOUNT0, 0);

    // Block 2: call the contract (runtime code executes PUSH1 0; SELFDESTRUCT)
    // SELFDESTRUCT is disabled → transaction reverts → receipt.status() == false
    let raw_call = MorphTxBuilder::new(TEST_CHAIN_ID, signer, 1)
        .with_v1_eth_fee()
        .with_to(contract_addr)
        .with_gas_limit(100_000)
        .build_signed()?;
    node.rpc.inject_tx(raw_call).await?;
    let payload = node.advance_block().await?;

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
        .expect("receipt must exist for SELFDESTRUCT call");

    use alloy_consensus::TxReceipt;
    // Morph disables SELFDESTRUCT — the call must fail (not panic), status=false
    assert!(
        !receipt.status(),
        "SELFDESTRUCT is disabled in Morph — receipt must be false"
    );

    Ok(())
}

/// A transaction with calldata has a non-zero L1 fee (blob-based DA cost).
///
/// Post-Curie: l1_fee = (commitScalar * l1BaseFee + len * blobBaseFee * blobScalar) / 1e9
/// With l1BaseFee=0 but blobBaseFee=1 and blobScalar=417565260, any non-trivial tx size → fee > 0.
#[tokio::test(flavor = "multi_thread")]
async fn l1_fee_nonzero_for_calldata_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    // Transaction with 100 bytes of non-zero calldata
    let signer = wallet_at_index(0, TEST_CHAIN_ID);
    let raw_tx = MorphTxBuilder::new(TEST_CHAIN_ID, signer, 0)
        .with_v1_eth_fee()
        .with_data(vec![0xab; 100])
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

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("receipt must exist");

    assert!(
        receipt.l1_fee() > U256::ZERO,
        "L1 fee must be non-zero for a transaction with calldata (l1_fee={})",
        receipt.l1_fee()
    );

    Ok(())
}

/// A transaction with large calldata incurs a higher L1 fee than one with empty calldata.
///
/// Verifies that the blob-based L1 fee scales with transaction size, as expected
/// from Curie's `len * blobBaseFee * blobScalar` formula.
#[tokio::test(flavor = "multi_thread")]
async fn empty_calldata_vs_large_calldata_l1_fee_difference() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();

    let signer = wallet_at_index(0, TEST_CHAIN_ID);

    // Block 1: transaction with no extra calldata
    let tx_empty = MorphTxBuilder::new(TEST_CHAIN_ID, signer.clone(), 0)
        .with_v1_eth_fee()
        .build_signed()?;
    node.rpc.inject_tx(tx_empty).await?;
    let p1 = node.advance_block().await?;
    let hash_empty = *p1.block().body().transactions.first().unwrap().tx_hash();
    let receipt_empty = node
        .inner
        .provider
        .receipt_by_hash(hash_empty)?
        .expect("receipt for empty-calldata tx");

    // Block 2: transaction with 200 bytes of non-zero calldata
    let tx_large = MorphTxBuilder::new(TEST_CHAIN_ID, signer, 1)
        .with_v1_eth_fee()
        .with_data(vec![0xff; 200])
        .build_signed()?;
    node.rpc.inject_tx(tx_large).await?;
    let p2 = node.advance_block().await?;
    let hash_large = *p2.block().body().transactions.first().unwrap().tx_hash();
    let receipt_large = node
        .inner
        .provider
        .receipt_by_hash(hash_large)?
        .expect("receipt for large-calldata tx");

    assert!(
        receipt_large.l1_fee() > receipt_empty.l1_fee(),
        "large calldata tx must have higher L1 fee than empty calldata tx \
         (large={}, empty={})",
        receipt_large.l1_fee(),
        receipt_empty.l1_fee()
    );

    Ok(())
}
