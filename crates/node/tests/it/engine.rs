//! Engine API behavior integration tests.
//!
//! Verifies engine-level semantics that are distinct from consensus rule
//! enforcement — in particular the state-root validation gating introduced
//! by the Jade hardfork.

use alloy_primitives::B256;
use morph_node::test_utils::{HardforkSchedule, TestNodeBuilder};

use super::helpers::{build_block_no_submit, craft_and_try_import_block};

/// Pre-Jade: a block with a wrong state root is still accepted.
///
/// Before Jade, morph-reth computes an MPT state root but the canonical
/// chain uses ZK-trie roots. Rather than implementing ZK-trie, morph-reth
/// skips state root validation entirely in pre-Jade mode. A tampered state
/// root must therefore not cause rejection.
///
/// This is the mirror image of `post_jade_state_root_mismatch_is_rejected`
/// in `consensus.rs` — together they prove the Jade hardfork boundary.
#[tokio::test(flavor = "multi_thread")]
async fn state_root_validation_skipped_pre_jade() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, _tasks, _wallet) = TestNodeBuilder::new()
        .with_schedule(HardforkSchedule::PreJade)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    // Build a valid block without submitting it.
    let base_payload = build_block_no_submit(&mut node, vec![]).await?;

    // Replace the state root with a bogus value and try to import.
    let accepted = craft_and_try_import_block(&mut node, &base_payload, |block| {
        block.header.inner.state_root = B256::from([0xFF; 32]);
    })
    .await?;

    assert!(
        accepted,
        "pre-Jade block with wrong state root must be accepted (state root validation skipped)"
    );

    Ok(())
}
