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
use morph_primitives::{Block, MorphHeader};
use reth_payload_primitives::{BuiltPayload, ExecutionPayload, PayloadTypes};
use reth_primitives_traits::{NodePrimitives, SealedBlock, SealedHeader};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

// Feature unification: Ensure reth-ethereum-primitives' serde features are enabled
// for transitive dependencies (via reth-payload-primitives → reth-chain-state).
// This is required to satisfy trait bounds on EthereumReceipt in test builds.
use reth_ethereum_primitives as _;

// Re-export main types
pub use attributes::{
    MORPH_PAYLOAD_BUILDER_VERSION, MorphPayloadAttributes, MorphPayloadBuilderAttributes,
};
pub use built::MorphBuiltPayload;
pub use executable_l2_data::ExecutableL2Data;
pub use params::{AssembleL2BlockParams, AssembleL2BlockV2Params, GenericResponse};
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

/// Consensus validation mode applied when importing a [`MorphExecutionData`] payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum MorphPayloadValidationMode {
    /// Standard untrusted payload validation (`VerifyHeader` + `ValidateBody` + execution).
    #[default]
    Full,
    /// Locally assembled block (`AssembleL2Block` / `AssembleL2BlockV2` `verified` path in
    /// morph-geth). Skips wall-clock `TimestampIsInFuture`, `GasLimit` 1/1024 bound, and local
    /// DA payload byte limit.
    LocallyBuilt,
    /// L1-derived safe block (`NewSafeL2Block` with `safe = true` in morph-geth). Skips
    /// `VerifyHeader` (`TimestampIsInFuture`, `TimestampIsInPast`, `GasLimit` bounds,
    /// `BaseFeeOverLimit`) and `ValidateBody`'s local DA payload byte limit.
    Safe,
}

/// Bounded in-memory cache of validated [`SealedHeader<MorphHeader>`] entries currently held in
/// the engine tree, shared between the tree validator and the custom Morph L2 Engine API so
/// unpersisted in-memory parent headers are visible before MDBX persistence.
#[derive(Debug, Default)]
pub struct TreeHeaderCache {
    inner: Mutex<TreeHeaderCacheInner>,
}

#[derive(Debug, Default)]
struct TreeHeaderCacheInner {
    entries: HashMap<B256, SealedHeader<MorphHeader>>,
    order: VecDeque<B256>,
}

impl TreeHeaderCache {
    /// Maximum number of sealed headers retained in memory.
    pub const MAX_ENTRIES: usize = 8192;

    /// Creates an empty [`TreeHeaderCache`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a validated sealed header in the cache.
    pub fn insert(&self, header: SealedHeader<MorphHeader>) {
        let hash = header.hash();
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let is_new = inner.entries.insert(hash, header).is_none();
        if is_new {
            inner.order.push_back(hash);
            while inner.entries.len() > Self::MAX_ENTRIES {
                let Some(evicted) = inner.order.pop_front() else {
                    break;
                };
                inner.entries.remove(&evicted);
            }
        }
    }

    /// Looks up a sealed header by block hash.
    pub fn get(&self, hash: &B256) -> Option<SealedHeader<MorphHeader>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entries
            .get(hash)
            .cloned()
    }
}

/// Execution data for Morph node: a sealed block plus optional cross-check metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MorphExecutionData {
    /// The built block.
    pub block: Arc<SealedBlock<Block>>,
    /// Optional expected withdraw trie root supplied by custom engine APIs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_withdraw_trie_root: Option<B256>,
    /// Validation mode governing consensus header/body exemptions.
    ///
    /// Strictly internal in-process metadata (`#[serde(skip)]`) so external JSON/serde
    /// payloads can never deserialize `LocallyBuilt` or `Safe`.
    #[serde(skip)]
    pub validation_mode: MorphPayloadValidationMode,
}

impl MorphExecutionData {
    /// Creates a new `MorphExecutionData` from a sealed block.
    pub fn new(block: Arc<SealedBlock<Block>>) -> Self {
        Self {
            block,
            expected_withdraw_trie_root: None,
            validation_mode: MorphPayloadValidationMode::Full,
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
            validation_mode: MorphPayloadValidationMode::Full,
        }
    }

    /// Sets the consensus validation mode for this payload.
    pub const fn with_validation_mode(
        mut self,
        validation_mode: MorphPayloadValidationMode,
    ) -> Self {
        self.validation_mode = validation_mode;
        self
    }
}

impl From<MorphBuiltPayload> for MorphExecutionData {
    fn from(value: MorphBuiltPayload) -> Self {
        Self::new(value.block)
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

    fn gas_limit(&self) -> u64 {
        self.block.gas_limit()
    }

    fn slot_number(&self) -> Option<u64> {
        // Morph L2 has no PoS slot semantics.
        None
    }
}

impl PayloadTypes for MorphPayloadTypes {
    type ExecutionData = MorphExecutionData;
    type BuiltPayload = MorphBuiltPayload;
    type PayloadAttributes = MorphPayloadAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
        _bal: Option<Bytes>,
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
        let data = MorphPayloadTypes::block_to_payload(block, None);
        assert_eq!(data.block_hash(), hash);
        assert!(data.expected_withdraw_trie_root.is_none());
        assert_eq!(data.validation_mode, MorphPayloadValidationMode::Full);
    }

    #[test]
    fn test_execution_data_validation_mode_builder() {
        let block = Arc::new(create_test_block());
        let data = MorphExecutionData::new(block);
        assert_eq!(data.validation_mode, MorphPayloadValidationMode::Full);

        let local = data
            .clone()
            .with_validation_mode(MorphPayloadValidationMode::LocallyBuilt);
        assert_eq!(
            local.validation_mode,
            MorphPayloadValidationMode::LocallyBuilt
        );

        let safe = data.with_validation_mode(MorphPayloadValidationMode::Safe);
        assert_eq!(safe.validation_mode, MorphPayloadValidationMode::Safe);
    }

    #[test]
    fn test_execution_data_serde_skips_and_ignores_validation_mode() {
        let block = Arc::new(create_test_block());
        let safe =
            MorphExecutionData::new(block).with_validation_mode(MorphPayloadValidationMode::Safe);

        let mut json_val = serde_json::to_value(&safe).expect("serialize MorphExecutionData");
        assert!(
            json_val.get("validation_mode").is_none() && json_val.get("validationMode").is_none(),
            "validation_mode must never be serialized"
        );

        json_val["validation_mode"] = serde_json::json!("Safe");
        json_val["validationMode"] = serde_json::json!("LocallyBuilt");
        let decoded: MorphExecutionData =
            serde_json::from_value(json_val).expect("deserialize MorphExecutionData");
        assert_eq!(
            decoded.validation_mode,
            MorphPayloadValidationMode::Full,
            "untrusted serde input must never set LocallyBuilt or Safe"
        );
    }

    #[test]
    fn test_tree_header_cache_insert_get_and_bounded_eviction() {
        let cache = TreeHeaderCache::new();
        let mut first_hash = B256::ZERO;
        let mut last_hash = B256::ZERO;

        for number in 0..=(TreeHeaderCache::MAX_ENTRIES as u64) {
            let header = SealedHeader::seal_slow(MorphHeader {
                inner: Header {
                    number,
                    ..Default::default()
                },
                next_l1_msg_index: number + 1,
            });
            if number == 0 {
                first_hash = header.hash();
            }
            if number == TreeHeaderCache::MAX_ENTRIES as u64 {
                last_hash = header.hash();
            }
            cache.insert(header);
        }

        assert!(
            cache.get(&first_hash).is_none(),
            "oldest entry must be evicted once MAX_ENTRIES is exceeded"
        );
        assert_eq!(
            cache.get(&last_hash).unwrap().next_l1_msg_index,
            TreeHeaderCache::MAX_ENTRIES as u64 + 1
        );
    }
}
