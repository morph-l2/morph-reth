//! Mixed L1/L2 block construction under independent resource limits.

use alloy_consensus::{BlockHeader, transaction::TxHashRef};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes};
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{
    L1MessageBuilder, MorphTestNode, MorphTxBuilder, TEST_TOKEN_ADDRESS, TEST_TOKEN_ID,
    TestNodeBuilder, make_transfer_tx, wallet_at_index,
};
use morph_payload_types::{AssembleL2BlockParams, ExecutableL2Data};
use morph_primitives::{Block, MorphHeader, MorphReceipt};
use reth_provider::{BlockReader, BlockReaderIdExt, ReceiptProvider, StateProviderFactory};

use super::helpers::wait_for_pool_membership;

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

    let client = node.auth_server_handle().http_client();
    let data: ExecutableL2Data = client.request("engine_assembleL2Block", (params,)).await?;
    let _: MorphHeader = client
        .request("engine_newL2BlockV2", (data.clone(),))
        .await?;
    Ok(data)
}

fn canonical_block(node: &MorphTestNode, number: u64) -> eyre::Result<Block> {
    node.inner
        .provider
        .block_by_number(number)?
        .ok_or_else(|| eyre::eyre!("canonical block {number} is missing"))
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

fn transaction_hashes(block: &Block) -> Vec<B256> {
    block
        .body
        .transactions
        .iter()
        .map(|tx| *tx.tx_hash())
        .collect()
}

fn token_balance_slot(account: Address) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(account.as_slice());
    preimage[63] = 1;
    alloy_primitives::keccak256(preimage)
}

/// Behavior contract:
/// - fault pressure: one L1 message plus three independent pool transactions
///   cannot all fit under the block gas limit;
/// - evidence: the L1 prefix and two higher-priority L2 transactions are
///   canonical, gas stays bounded, and the excluded transaction remains pooled.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_respects_gas_limit_without_losing_pool_transactions() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_gas_limit(80_000)
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
                .with_gas_limit(30_000)
                .with_fees(HIGH_FEE, HIGH_FEE)
                .build_signed()?,
        )
        .await?;
    let overflow_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_c, 0)
                .with_v1_eth_fee()
                .with_gas_limit(30_000)
                .with_fees(LOW_FEE, LOW_FEE)
                .build_signed()?,
        )
        .await?;

    let data = assemble_and_import(
        &node,
        vec![
            L1MessageBuilder::new(0)
                .with_gas_limit(50_000)
                .build_encoded(),
        ],
    )
    .await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);

    assert_l1_prefix(&block, 1);
    assert!(block.header.inner.gas_used <= block.header.inner.gas_limit);
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&morph_hash));
    assert!(!hashes.contains(&overflow_hash));
    wait_for_pool_membership(&node, regular_hash, false).await?;
    wait_for_pool_membership(&node, morph_hash, false).await?;
    wait_for_pool_membership(&node, overflow_hash, true).await?;

    Ok(())
}

/// Behavior contract:
/// - fault pressure: individually valid pool transactions cumulatively exceed
///   the configured DA payload-byte limit;
/// - evidence: the independently summed canonical L2 bytes stay within the
///   limit, fee receipt and token state agree, and the overflow remains pooled.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_respects_da_limit_and_preserves_fee_accounting() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const MAX_DA_BYTES: u64 = 1_200;
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_max_tx_payload_bytes(MAX_DA_BYTES)
        .build()
        .await?;
    let node = nodes.pop().expect("one node requested");
    let signer_b = wallet_at_index(1, wallet.chain_id);
    let signer_c = wallet_at_index(2, wallet.chain_id);
    let token_payer = signer_b.address();
    let balance_slot = token_balance_slot(token_payer);
    let token_before = node
        .inner
        .provider
        .latest()?
        .storage(TEST_TOKEN_ADDRESS, balance_slot)?
        .unwrap_or_default();

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
        .with_data(vec![0x22; 700])
        .with_fees(LOW_FEE, LOW_FEE)
        .build_signed()?;
    assert!(
        overflow_raw.len() < MAX_DA_BYTES as usize,
        "overflow must be caused by cumulative DA bytes, not one oversized transaction"
    );
    let overflow_hash = node.rpc.inject_tx(overflow_raw).await?;

    let data = assemble_and_import(&node, vec![L1MessageBuilder::new(0).build_encoded()]).await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);
    let l2_bytes: u64 = block
        .body
        .transactions
        .iter()
        .filter(|tx| !tx.is_l1_msg())
        .map(|tx| tx.encode_2718_len() as u64)
        .sum();

    assert_l1_prefix(&block, 1);
    assert!(l2_bytes <= MAX_DA_BYTES);
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&token_fee_hash));
    assert!(!hashes.contains(&overflow_hash));

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(token_fee_hash)?
        .expect("token-fee receipt must exist");
    match receipt {
        MorphReceipt::Morph(receipt) => {
            assert_eq!(receipt.fee_token_id, Some(TEST_TOKEN_ID));
            assert!(receipt.fee_rate.is_some());
            assert!(receipt.token_scale.is_some());
            assert!(receipt.fee_limit.is_some());
        }
        other => panic!("expected Morph receipt, got {:?}", other.tx_type()),
    }
    let token_after = node
        .inner
        .provider
        .latest()?
        .storage(TEST_TOKEN_ADDRESS, balance_slot)?
        .unwrap_or_default();
    assert!(
        token_after < token_before,
        "canonical token-fee transaction must debit the payer"
    );

    let l1_hash = *block.body.transactions[0].tx_hash();
    let l1_receipt = node
        .inner
        .provider
        .receipt_by_hash(l1_hash)?
        .expect("L1 receipt must exist");
    assert_eq!(l1_receipt.l1_fee(), alloy_primitives::U256::ZERO);

    wait_for_pool_membership(&node, regular_hash, false).await?;
    wait_for_pool_membership(&node, token_fee_hash, false).await?;
    wait_for_pool_membership(&node, overflow_hash, true).await?;

    Ok(())
}

/// Behavior contract:
/// - fault pressure: the configured transaction count includes mandatory L1
///   messages as well as pool transactions;
/// - evidence: the block stops exactly at the count, preserves the L1 prefix,
///   and leaves the lower-priority overflow transaction in the pool.
#[tokio::test(flavor = "multi_thread")]
async fn mixed_block_counts_l1_messages_toward_transaction_limit() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_max_tx_per_block(3)
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
                .with_fees(HIGH_FEE, HIGH_FEE)
                .build_signed()?,
        )
        .await?;
    let overflow_hash = node
        .rpc
        .inject_tx(
            MorphTxBuilder::new(wallet.chain_id, signer_c, 0)
                .with_v1_eth_fee()
                .with_fees(LOW_FEE, LOW_FEE)
                .build_signed()?,
        )
        .await?;

    let data = assemble_and_import(&node, vec![L1MessageBuilder::new(0).build_encoded()]).await?;
    let block = canonical_block(&node, data.number)?;
    let hashes = transaction_hashes(&block);

    assert_eq!(block.body.transactions.len(), 3);
    assert_l1_prefix(&block, 1);
    assert!(hashes.contains(&regular_hash));
    assert!(hashes.contains(&morph_hash));
    assert!(!hashes.contains(&overflow_hash));
    wait_for_pool_membership(&node, regular_hash, false).await?;
    wait_for_pool_membership(&node, morph_hash, false).await?;
    wait_for_pool_membership(&node, overflow_hash, true).await?;

    Ok(())
}
