//! Block sync integration tests.
//!
//! Tests that the Morph node can produce and import blocks via the Engine API.

use morph_node::test_utils::{advance_chain, setup};
use reth_payload_primitives::BuiltPayload;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Verifies that the Morph node can sync a chain of blocks.
///
/// This is the core E2E test — it starts a real node, generates transfer
/// transactions, produces blocks via the payload builder, and imports them
/// through the Engine API (newPayload + forkchoiceUpdated).
#[tokio::test]
async fn can_sync() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, wallet) = setup(1, false).await?;
    let mut node = nodes.pop().unwrap();
    let wallet = Arc::new(Mutex::new(wallet));

    // Advance the chain by 10 blocks, each containing a transfer tx
    let payloads = advance_chain(10, &mut node, wallet.clone()).await?;

    assert_eq!(payloads.len(), 10, "should have produced 10 payloads");

    // Verify block numbers are sequential
    for (i, payload) in payloads.iter().enumerate() {
        let block = payload.block();
        assert_eq!(
            block.header().inner.number,
            (i + 1) as u64,
            "block number should be sequential"
        );
        // Each block should have at least one transaction (the transfer)
        assert!(
            !block.body().transactions.is_empty(),
            "block {} should contain transactions",
            i + 1
        );
    }

    Ok(())
}
