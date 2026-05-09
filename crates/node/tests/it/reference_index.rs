//! Integration tests for `morph_getTransactionHashesByReference`.
//!
//! These tests spin up a real Morph test node, produce blocks with reference-
//! carrying `MorphTx` transactions, then run the reference index backfill +
//! reconcile against the node's provider, and verify the query results.
//!
//! Because `reth_e2e_test_utils::setup_engine` doesn't support ExEx injection,
//! we test the storage layer (backfill/reconcile/query) directly rather than
//! through the live ExEx.  The ExEx logic is separately covered by the unit
//! tests in `morph-reference-index`.

use alloy_consensus::transaction::TxHashRef;
use alloy_primitives::{B256, U64};
use morph_node::test_utils::{MorphTxBuilder, TEST_TOKEN_ID, TestNodeBuilder};
use morph_reference_index::{
    DEFAULT_LAG_THRESHOLD, DEFAULT_MAX_REORG_DEPTH, ReferenceIndexDb, ReferenceIndexReader,
    ReferenceQuery, backfill::run_backfill, reconcile::run_startup_reconcile,
};
use reth_payload_primitives::BuiltPayload;
use reth_provider::BlockNumReader;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

async fn open_and_backfill_index<P>(provider: &P, dir: &TempDir) -> ReferenceIndexDb
where
    P: reth_provider::BlockReader<Block = morph_primitives::Block>
        + reth_provider::HeaderProvider<Header = morph_primitives::MorphHeader>
        + BlockNumReader
        + reth_provider::BlockHashReader
        + reth_provider::ChainSpecProvider<ChainSpec = morph_chainspec::MorphChainSpec>,
{
    let chain_spec = provider.chain_spec();
    let chain_id = reth_chainspec::EthChainSpec::chain(chain_spec.as_ref()).id();
    let genesis_hash = reth_chainspec::EthChainSpec::genesis_hash(chain_spec.as_ref());

    let db = ReferenceIndexDb::open(dir.path(), chain_id, genesis_hash).unwrap();

    let head = provider.best_block_number().unwrap();
    run_backfill(&db, provider, chain_spec.as_ref(), head, 256).unwrap();
    run_startup_reconcile(&db, provider, head, DEFAULT_MAX_REORG_DEPTH).unwrap();

    db.set_ready(true);
    db
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Produce one block with a reference-carrying MorphTx and verify the index
/// returns it for the correct reference.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_finds_single_morph_tx() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let dir = TempDir::new()?;
    let db = open_and_backfill_index(&node.inner.provider, &dir).await;

    let reader = ReferenceIndexReader::new(db, DEFAULT_LAG_THRESHOLD);
    let canonical_tip = node.inner.provider.best_block_number()?;
    let query = ReferenceQuery::new(reference, None, None).unwrap();
    let results = reader.query(query, canonical_tip)?;

    assert_eq!(results.len(), 1, "should find exactly one transaction");
    assert_eq!(results[0].transaction_hash, tx_hash);
    assert_eq!(results[0].transaction_index, U64::from(0u64));

    Ok(())
}

/// Produce multiple blocks with the same reference and verify pagination.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_pagination() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
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

    let dir = TempDir::new()?;
    let db = open_and_backfill_index(&node.inner.provider, &dir).await;

    let reader = ReferenceIndexReader::new(db, DEFAULT_LAG_THRESHOLD);
    let canonical_tip = node.inner.provider.best_block_number()?;

    // Page 1: offset=0, limit=2 → first two entries.
    let page1 = reader.query(
        ReferenceQuery::new(reference, Some(0), Some(2)).unwrap(),
        canonical_tip,
    )?;
    assert_eq!(page1.len(), 2);
    assert_eq!(page1[0].transaction_hash, tx_hashes[0]);
    assert_eq!(page1[1].transaction_hash, tx_hashes[1]);

    // Page 2: offset=2, limit=2 → last entry only.
    let page2 = reader.query(
        ReferenceQuery::new(reference, Some(2), Some(2)).unwrap(),
        canonical_tip,
    )?;
    assert_eq!(page2.len(), 1);
    assert_eq!(page2[0].transaction_hash, tx_hashes[2]);

    Ok(())
}

/// A different reference key returns no results.
#[tokio::test(flavor = "multi_thread")]
async fn reference_index_no_results_for_unrelated_reference() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = TestNodeBuilder::new().build().await?;
    let mut node = nodes.pop().unwrap();
    let reference = B256::with_last_byte(0xbb);
    let other_reference = B256::with_last_byte(0xcc);

    let raw_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), wallet.inner_nonce)
        .with_v1_token_fee(TEST_TOKEN_ID)
        .with_reference(reference)
        .build_signed()?;
    node.rpc.inject_tx(raw_tx).await?;
    node.advance_block().await?;

    let dir = TempDir::new()?;
    let db = open_and_backfill_index(&node.inner.provider, &dir).await;
    let reader = ReferenceIndexReader::new(db, DEFAULT_LAG_THRESHOLD);
    let canonical_tip = node.inner.provider.best_block_number()?;

    let results = reader.query(
        ReferenceQuery::new(other_reference, None, None).unwrap(),
        canonical_tip,
    )?;
    assert!(
        results.is_empty(),
        "unrelated reference should return nothing"
    );

    Ok(())
}
