//! Morph payload types.
//!
//! This crate provides the core types used in the Morph Engine API, including:
//! - [`ExecutableL2Data`]: Block data for AssembleL2Block/ValidateL2Block/NewL2Block
//! - [`SafeL2Data`]: Safe block data for NewSafeL2Block (derivation)
//! - [`MorphPayloadAttributes`]: Extended payload attributes for block building
//! - [`MorphBuiltPayload`]: Built payload result
//! - [`MorphEngineTypes`]: Engine types for the Morph Engine API
//! - [`MorphPayloadTypes`]: Default payload types implementation
//!
//! These types are designed to be compatible with the standard Ethereum Engine API.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod attributes;
mod built;
mod executable_l2_data;
mod params;
mod safe_l2_data;

use core::marker::PhantomData;

use alloy_rpc_types_engine::{
    ExecutionData, ExecutionPayload, ExecutionPayloadEnvelopeV2, ExecutionPayloadEnvelopeV3,
    ExecutionPayloadEnvelopeV4, ExecutionPayloadV1,
};
use morph_primitives::Block;
use reth_engine_primitives::EngineTypes;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};
use reth_primitives_traits::{NodePrimitives, SealedBlock};
use serde::{Deserialize, Serialize};

// Re-export main types
pub use attributes::{MorphPayloadAttributes, MorphPayloadBuilderAttributes};
pub use built::MorphBuiltPayload;
pub use executable_l2_data::ExecutableL2Data;
pub use params::{AssembleL2BlockParams, GenericResponse};
pub use safe_l2_data::SafeL2Data;

/// The types used in the Morph beacon consensus engine.
///
/// This is a generic wrapper that allows customizing the payload types.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MorphEngineTypes<T: PayloadTypes = MorphPayloadTypes> {
    _marker: PhantomData<T>,
}

impl<T> PayloadTypes for MorphEngineTypes<T>
where
    T: PayloadTypes<ExecutionData = ExecutionData>,
    T::BuiltPayload: BuiltPayload<Primitives: NodePrimitives<Block = Block>>,
{
    type ExecutionData = T::ExecutionData;
    type BuiltPayload = T::BuiltPayload;
    type PayloadAttributes = T::PayloadAttributes;
    type PayloadBuilderAttributes = T::PayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> ExecutionData {
        let (payload, sidecar) =
            ExecutionPayload::from_block_unchecked(block.hash(), &block.into_block());
        ExecutionData { payload, sidecar }
    }
}

impl<T> EngineTypes for MorphEngineTypes<T>
where
    T: PayloadTypes<ExecutionData = ExecutionData>,
    T::BuiltPayload: BuiltPayload<Primitives: NodePrimitives<Block = Block>>
        + TryInto<ExecutionPayloadV1>
        + TryInto<ExecutionPayloadEnvelopeV2>
        + TryInto<ExecutionPayloadEnvelopeV3>
        + TryInto<ExecutionPayloadEnvelopeV4>,
{
    type ExecutionPayloadEnvelopeV1 = ExecutionPayloadV1;
    type ExecutionPayloadEnvelopeV2 = ExecutionPayloadEnvelopeV2;
    type ExecutionPayloadEnvelopeV3 = ExecutionPayloadEnvelopeV3;
    type ExecutionPayloadEnvelopeV4 = ExecutionPayloadEnvelopeV4;
    type ExecutionPayloadEnvelopeV5 = ExecutionPayloadEnvelopeV4;
}

/// A default payload type for [`MorphEngineTypes`].
///
/// This type provides the concrete implementations for Morph's payload handling.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MorphPayloadTypes;

impl PayloadTypes for MorphPayloadTypes {
    type ExecutionData = ExecutionData;
    type BuiltPayload = MorphBuiltPayload;
    type PayloadAttributes = MorphPayloadAttributes;
    type PayloadBuilderAttributes = MorphPayloadBuilderAttributes;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        let (payload, sidecar) =
            ExecutionPayload::from_block_unchecked(block.hash(), &block.into_block());
        ExecutionData { payload, sidecar }
    }
}
