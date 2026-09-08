//! Mixed L1/L2 block construction under independent resource limits.

use alloy_consensus::{BlockHeader, Transaction, TxReceipt, transaction::TxHashRef};
use alloy_primitives::B256;
use alloy_primitives::{Bytes, U256};
use alloy_rlp::Encodable;
use morph_node::test_utils::{
    L1MessageBuilder, MorphTestNode, MorphTxBuilder, TEST_FEE_VAULT_ADDRESS, TEST_TOKEN_ADDRESS,
    TEST_TOKEN_ID, TestNodeBuilder, make_transfer_tx, test_token_balance_slot, wallet_at_index,
};
use morph_payload_types::{AssembleL2BlockParams, ExecutableL2Data};
use morph_primitives::{Block, MorphReceipt};
use reth_provider::{BlockReaderIdExt, ReceiptProvider, StateProviderFactory};

use super::helpers::{
    assemble_l2_block, canonical_block, import_l2_block, transaction_hashes, wait_until_evicted,
    wait_until_pooled,
};

const HIGH_FEE: u128 = 20_000_000_000;
const LOW_FEE: u128 = 10_000_000_000;

async fn assemble_and_import(
    node: &MorphTestNode,
    l1_messages: Vec<Bytes>,
) -> eyre::Result<ExecutableL2Data> {
    let latest = node
        .inner
        .provider
        .latest_header()?
        .ok_or_else(|| eyre::eyre!("canonical head is missing"))?;
    let mut params = AssembleL2BlockParams::new(latest.number() + 1, l1_messages);
    params.timestamp = Some(latest.timestamp() + 1);

    let data = assemble_l2_block(node, params).await?;
    import_l2_block(node, data.clone()).await?;
    Ok(data)
}

fn token_balance(node: &MorphTestNode, slot: B256) -> eyre::Result<U256> {
    Ok(node
        .inner
        .provider
        .latest()?
        .storage(TEST_TOKEN_ADDRESS, slot)?
        .unwrap_or_default())
}

fn assert_l1_prefix(block: &Block, expected_l1_messages: u64) {
    let mut seen_l2 = false;
    let mut l1_count = 0;

    for tx in &block.body.transactions {
        if tx.is_l1_msg() {
            assert!(!seen_l2, "L1 messages must form a block prefix");
            assert_eq!(tx.queue_index(), Some(l1_count));
            l1_count += 1;
        } else {
            seen_l2 = true;
        }
    }

    assert_eq!(l1_count, expected_l1_messages);
    assert_eq!(block.header.next_l1_msg_index, expected_l1_messages);
}

/// Data-availability bytes the builder attributes to a block, which is the RLP
/// network length of each pool transaction. L1 messages are excluded because
/// their data already lives on L1.
fn l2_da_bytes(block: &Block) -> u64 {
    block
        .body
        .transactions
        .iter()
        .filter(|tx| !tx.is_l1_msg())
        .map(|tx| tx.length() as u64)
        .sum()
}

/// Gas removed from the block gas pool by the canonical transactions.
///
/// L1 messages reserve their full gas limit, while regular transactions return unused gas and
/// therefore consume only the delta reported by their cumulative receipt gas.
fn block_gas_pool_used(node: &MorphTestNode, block: &Block) -> eyre::Result<u64> {
    let mut previous_cumulative_gas = 0;
    let mut gas_pool_used = 0u64;

    for tx in &block.body.transactions {
        let receipt = node
            .inner
            .provider
            .receipt_by_hash(*tx.tx_hash())?
            .ok_or_else(|| eyre::eyre!("missing receipt for transaction {}", tx.tx_hash()))?;
        let cumulative_gas = receipt.cumulative_gas_used();
        let tx_gas_used = cumulative_gas
            .checked_sub(previous_cumulative_gas)
            .ok_or_else(|| eyre::eyre!("receipt cumulative gas decreased"))?;
        previous_cumulative_gas = cumulative_gas;

        let gas_pool_charge = if tx.is_l1_msg() {
            tx.gas_limit()
        } else {
            tx_gas_used
        };
        gas_pool_used = gas_pool_used
            .checked_add(gas_pool_charge)
            .ok_or_else(|| eyre::eyre!("block gas-pool usage overflowed u64"))?;
    }

    Ok(gas_pool_used)
}

/// Behavior contract:
/// - fault pressure: one L1 message plus three independent pool transactions
///   cannot all fit under the block gas limit;
/// - evidence: the L1 prefix and two higher-priority L2 transactions are
///   canonical, gas stays bounded, and the excluded transaction remains pooled.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_respects_gas_limit_without_losing_pool_transactions() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const BLOCK_GAS_LIMIT: u64 = 110_000;
    const L1_MESSAGE_GAS_LIMIT: u64 = 50_000;
    const OVERFLOW_TX_GAS_LIMIT: u64 = 30_000;

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_gas_limit(BLOCK_GAS_LIMIT)
        .build()
        .await?;
    let node = nodes.pop().expect("one node requested");
    let signer_b = wallet_at_index(1, wallet.chain_id);
    let signer_c = wallet_at_index(2, wallet.chain_id);

    let regular_hash = node
        .rpc
        .inject_tx(make_transfer_tx(wallet.chain_id, wallet.inner, 0).await)
        .await?;
    let morph_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_b, 0)
                .with_v1_eth_fee()
                .with_gas_limit(OVERFLOW_TX_GAS_LIMIT)
                .with_fees(HIGH_FEE, HIGH_FEE)
                .build_signed()?,
        )
        .await?;
    let overflow_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_c, 0)
                .with_v1_eth_fee()
                .with_gas_limit(OVERFLOW_TX_GAS_LIMIT)
                .with_fees(LOW_FEE, LOW_FEE)
                .build_signed()?,
        )
        .await?;

    let data = assemble_and_import(
        &node,
        vec![
            L1MessageBuilder::new(0)
                .with_gas_limit(L1_MESSAGE_GAS_LIMIT)
                .build_encoded(),
        ],
    )
    .await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);

    assert_l1_prefix(&block, 1);
    assert!(block.header.inner.gas_used <= block.header.inner.gas_limit);
    let gas_pool_used = block_gas_pool_used(&node, &block)?;
    assert!(gas_pool_used <= block.header.inner.gas_limit);
    // Header gasUsed intentionally excludes unused gas reserved by the L1 message, so prove the
    // packing limit bound against reconstructed gas-pool usage instead.
    assert!(
        gas_pool_used + OVERFLOW_TX_GAS_LIMIT > block.header.inner.gas_limit,
        "gas limit did not actually bind: gas pool used {} of {}",
        gas_pool_used,
        block.header.inner.gas_limit
    );
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&morph_hash));
    assert!(!hashes.contains(&overflow_hash));
    wait_until_evicted(&node, regular_hash).await?;
    wait_until_evicted(&node, morph_hash).await?;
    wait_until_pooled(&node, overflow_hash).await?;

    Ok(())
}

/// Behavior contract:
/// - fault pressure: individually valid pool transactions cumulatively exceed
///   the configured DA payload-byte limit, alongside a three-message L1 prefix;
/// - evidence: the independently summed canonical L2 bytes stay within the
///   limit, fee receipt and token state agree, and the overflow remains pooled.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_respects_da_limit_and_preserves_fee_accounting() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const MAX_DA_BYTES: u64 = 1_200;
    const OVERFLOW_DATA_BYTES: usize = 700;
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_max_tx_payload_bytes(MAX_DA_BYTES)
        .build()
        .await?;
    let node = nodes.pop().expect("one node requested");
    let signer_b = wallet_at_index(1, wallet.chain_id);
    let signer_c = wallet_at_index(2, wallet.chain_id);
    let payer_slot = test_token_balance_slot(signer_b.address());
    let vault_slot = test_token_balance_slot(TEST_FEE_VAULT_ADDRESS);
    let payer_before = token_balance(&node, payer_slot)?;
    let vault_before = token_balance(&node, vault_slot)?;

    let regular_hash = node
        .rpc
        .inject_tx(make_transfer_tx(wallet.chain_id, wallet.inner, 0).await)
        .await?;
    let token_fee_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_b, 0)
                .with_v1_token_fee(TEST_TOKEN_ID)
                .with_data(vec![0x11; 400])
                .with_fees(HIGH_FEE, HIGH_FEE)
                .build_signed()?,
        )
        .await?;
    let overflow_raw = MorphTxBuilder::new(wallet.chain_id, signer_c, 0)
        .with_v1_eth_fee()
        .with_data(vec![0x22; OVERFLOW_DATA_BYTES])
        .with_fees(LOW_FEE, LOW_FEE)
        .build_signed()?;
    assert!(
        overflow_raw.len() < MAX_DA_BYTES as usize,
        "overflow must be caused by cumulative DA bytes, not one oversized transaction"
    );
    let overflow_hash = node.rpc.inject_tx(overflow_raw).await?;

    // Three L1 messages also exercise the sequential queue-index prefix, which the
    // DA limit must not account for.
    let data = assemble_and_import(&node, L1MessageBuilder::build_sequential(0, 3)).await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);
    let l2_bytes = l2_da_bytes(&block);

    assert_l1_prefix(&block, 3);
    assert!(l2_bytes <= MAX_DA_BYTES);
    // Without this the assertion above would also hold for a block that stopped
    // far short of the limit, or that dropped the limit check altogether.
    assert!(
        l2_bytes + OVERFLOW_DATA_BYTES as u64 > MAX_DA_BYTES,
        "DA limit did not actually bind: {l2_bytes} of {MAX_DA_BYTES} bytes used"
    );
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&token_fee_hash));
    assert!(!hashes.contains(&overflow_hash));

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(token_fee_hash)?
        .expect("token-fee receipt must exist");
    let MorphReceipt::Morph(receipt) = receipt else {
        panic!("expected Morph receipt, got {:?}", receipt.tx_type());
    };
    assert_eq!(receipt.fee_token_id, Some(TEST_TOKEN_ID));
    let fee_limit = receipt.fee_limit.expect("fee_limit must be recorded");
    assert!(
        !receipt
            .fee_rate
            .expect("fee_rate must be recorded")
            .is_zero(),
        "a zero fee rate would silently make the token fee free"
    );
    assert!(
        !receipt
            .token_scale
            .expect("token_scale must be recorded")
            .is_zero()
    );

    // Conservation is checked instead of re-deriving the fee formula, which would
    // make the test pass for any consistent-but-wrong charge.
    let debited = payer_before - token_balance(&node, payer_slot)?;
    let credited = token_balance(&node, vault_slot)? - vault_before;
    assert!(debited > U256::ZERO, "token fee payer was not charged");
    assert_eq!(
        debited, credited,
        "tokens taken from the payer must arrive at the fee vault"
    );
    assert!(
        debited <= fee_limit,
        "token debit {debited} must stay within the signed fee limit {fee_limit}"
    );

    let l1_hash = *block.body.transactions[0].tx_hash();
    let l1_receipt = node
        .inner
        .provider
        .receipt_by_hash(l1_hash)?
        .expect("L1 receipt must exist");
    assert_eq!(l1_receipt.l1_fee(), U256::ZERO);

    wait_until_evicted(&node, regular_hash).await?;
    wait_until_evicted(&node, token_fee_hash).await?;
    wait_until_pooled(&node, overflow_hash).await?;

    Ok(())
}

/// Behavior contract:
/// - fault pressure: individually valid pool transactions cumulatively exceed
///   the configured DA payload-byte limit, alongside a three-message L1 prefix;
/// - evidence: the independently summed canonical L2 bytes stay within the
///   limit and the overflow remains pooled.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_respects_da_limit_with_l1_messages() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const MAX_DA_BYTES: u64 = 1_200;
    const OVERFLOW_DATA_BYTES: usize = 700;
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_max_tx_payload_bytes(MAX_DA_BYTES)
        .build()
        .await?;
    let node = nodes.pop().expect("one node requested");
    let signer_b = wallet_at_index(1, wallet.chain_id);
    let signer_c = wallet_at_index(2, wallet.chain_id);

    let regular_hash = node
        .rpc
        .inject_tx(make_transfer_tx(wallet.chain_id, wallet.inner, 0).await)
        .await?;
    let morph_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_b, 0)
                .with_v1_eth_fee()
                .with_data(vec![0x11; 400])
                .with_fees(HIGH_FEE, HIGH_FEE)
                .build_signed()?,
        )
        .await?;
    let overflow_raw = MorphTxBuilder::new(wallet.chain_id, signer_c, 0)
        .with_v1_eth_fee()
        .with_data(vec![0x22; OVERFLOW_DATA_BYTES])
        .with_fees(LOW_FEE, LOW_FEE)
        .build_signed()?;
    let overflow_hash = node.rpc.inject_tx(overflow_raw).await?;

    let data = assemble_and_import(&node, L1MessageBuilder::build_sequential(0, 3)).await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);

    assert_l1_prefix(&block, 3);
    assert!(l2_da_bytes(&block) <= MAX_DA_BYTES);
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&morph_hash));
    assert!(!hashes.contains(&overflow_hash));
    wait_until_evicted(&node, regular_hash).await?;
    wait_until_evicted(&node, morph_hash).await?;
    wait_until_pooled(&node, overflow_hash).await?;

    Ok(())
}
