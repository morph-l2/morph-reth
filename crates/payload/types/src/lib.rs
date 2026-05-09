//! Morph payload types.
//!
//! This crate provides the core types used in the Morph Engine API, including:
//! - [`ExecutableL2Data`]: Block data for AssembleL2Block/ValidateL2Block/NewL2Block
//! - [`SafeL2Data`]: Safe block data for NewSafeL2Block (derivation)
//! - [`MorphPayloadAttributes`]: Extended payload attributes for block building
//! - [`MorphBuiltPayload`]: Built payload result
//! - [`MorphPayloadTypes`]: Payload types for reth node framework
//!
//! These types are designed to be compatible with the Morph L2 Engine API.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod attributes;
mod built;
mod executable_l2_data;
mod params;
mod safe_l2_data;

use alloy_consensus::BlockHeader as _;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{B256, Bytes};
use morph_primitives::Block;
use reth_payload_primitives::{BuiltPayload, ExecutionPayload, PayloadTypes};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use std::sync::Arc;

// Feature unification: Ensure reth-ethereum-primitives' serde features are enabled
// for transitive dependencies (via reth-payload-builder → reth-chain-state).
// This is required to satisfy trait bounds on EthereumReceipt in test builds.
use reth_ethereum_primitives as _;

// Re-export main types
pub use attributes::{MorphPayloadAttributes, MorphPayloadBuilderAttributes};
pub use built::MorphBuiltPayload;
pub use executable_l2_data::ExecutableL2Data;
pub use params::{AssembleL2BlockParams, GenericResponse};
pub use safe_l2_data::SafeL2Data;

// =============================================================================
// MorphPayloadTypes - Required for reth NodeBuilder framework
// =============================================================================

/// Payload types for Morph node.
///
/// This type is required by reth's `NodeTypes` trait to define how payloads
/// are built and represented in the node framework.
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct MorphPayloadTypes;

/// Execution data for Morph node. Simply wraps a sealed block.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MorphExecutionData {
    /// The built block.
    pub block: Arc<SealedBlock<Block>>,
    /// Optional expected withdraw trie root supplied by custom engine APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_withdraw_trie_root: Option<B256>,
}

impl MorphExecutionData {
    /// Creates a new `MorphExecutionData` from a sealed block.
    pub fn new(block: Arc<SealedBlock<Block>>) -> Self {
        Self {
            block,
            expected_withdraw_trie_root: None,
        }
    }

    /// Creates a new `MorphExecutionData` with an expected withdraw trie root.
    pub fn with_expected_withdraw_trie_root(
        block: Arc<SealedBlock<Block>>,
        expected_withdraw_trie_root: B256,
    ) -> Self {
        Self {
            block,
            expected_withdraw_trie_root: Some(expected_withdraw_trie_root),
        }
    }
}

impl ExecutionPayload for MorphExecutionData {
    fn parent_hash(&self) -> B256 {
        self.block.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.block.hash()
    }

    fn block_number(&self) -> u64 {
        self.block.number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        // Morph L2 doesn't have withdrawals
        None
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        self.block.parent_beacon_block_root()
    }

    fn timestamp(&self) -> u64 {
        self.block.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.block.gas_used()
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        None
    }

    fn transaction_count(&self) -> usize {
        self.block.body().transactions().count()
    }
}

impl PayloadTypes for MorphPayloadTypes {
    type ExecutionData = MorphExecutionData;
    type BuiltPayload = MorphBuiltPayload;
    type PayloadAttributes = MorphPayloadAttributes;
    type PayloadBuilderAttributes = MorphPayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        MorphExecutionData::new(Arc::new(block))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use morph_primitives::{BlockBody, MorphHeader};
    use reth_primitives_traits::Block as _;

    fn create_test_block() -> SealedBlock<Block> {
        let header: MorphHeader = Header::default().into();
        let body = BlockBody::default();
        let block = Block::new(header, body);
        block.seal_slow()
    }

    // =========================================================================
    // MorphExecutionData tests
    // =========================================================================

    #[test]
    fn test_execution_data_new_no_withdraw_root() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::new(block);
        assert!(data.expected_withdraw_trie_root.is_none());
    }

    #[test]
    fn test_execution_data_with_withdraw_root() {
        let block = Arc::new(create_test_block());
        let root = B256::from([0xAA; 32]);
        let data = MorphExecutionData::with_expected_withdraw_trie_root(block, root);
        assert_eq!(data.expected_withdraw_trie_root, Some(root));
    }

    #[test]
    fn test_execution_data_with_zero_withdraw_root() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::with_expected_withdraw_trie_root(block, B256::ZERO);
        assert_eq!(data.expected_withdraw_trie_root, Some(B256::ZERO));
    }

    #[test]
    fn test_execution_payload_trait_no_withdrawals() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::new(block);
        // L2 doesn't have withdrawals
        assert!(data.withdrawals().is_none());
    }

    #[test]
    fn test_execution_payload_trait_no_access_list() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::new(block);
        assert!(data.block_access_list().is_none());
    }

    #[test]
    fn test_execution_payload_trait_empty_block_counts() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::new(block.clone());
        assert_eq!(data.transaction_count(), 0);
        assert_eq!(data.gas_used(), 0);
        assert_eq!(data.block_number(), 0);
        assert_eq!(data.block_hash(), block.hash());
    }

    #[test]
    fn test_execution_payload_trait_timestamps_and_hashes() {
        let header = MorphHeader {
            inner: Header {
                timestamp: 1_700_000_000,
                parent_hash: B256::from([0x11; 32]),
                ..Default::default()
            },
            ..Default::default()
        };
        let block = Block::new(header, BlockBody::default());
        let sealed = Arc::new(block.seal_slow());
        let data = MorphExecutionData::new(sealed.clone());

        assert_eq!(data.timestamp(), 1_700_000_000);
        assert_eq!(data.parent_hash(), B256::from([0x11; 32]));
        assert_eq!(data.block_hash(), sealed.hash());
    }

    // =========================================================================
    // MorphPayloadTypes::block_to_payload tests
    // =========================================================================

    #[test]
    fn test_block_to_payload() {
        let block = create_test_block();
        let hash = block.hash();
        let data = MorphPayloadTypes::block_to_payload(block);
        assert_eq!(data.block_hash(), hash);
        assert!(data.expected_withdraw_trie_root.is_none());
    }
}
