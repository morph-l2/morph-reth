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
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MorphExecutionData {
    /// The built block.
    pub block: Arc<SealedBlock<Block>>,
}

impl MorphExecutionData {
    /// Creates a new `MorphExecutionData` from a sealed block.
    pub fn new(block: Arc<SealedBlock<Block>>) -> Self {
        Self { block }
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
