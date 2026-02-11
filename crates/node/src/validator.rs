//! Morph engine validator.

use crate::MorphNode;
use alloy_consensus::BlockHeader;
use morph_payload_types::{MorphExecutionData, MorphPayloadTypes};
use morph_primitives::MorphHeader;
use reth_node_api::{
    AddOnsContext, FullNodeComponents, InvalidPayloadAttributesError, NewPayloadError,
    PayloadAttributes, PayloadValidator,
};
use reth_node_builder::rpc::PayloadValidatorBuilder;
use reth_primitives_traits::SealedBlock;
use std::sync::Arc;

/// Builder for Morph engine validator (payload validation).
///
/// Creates a validator for validating engine API payloads.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct MorphEngineValidatorBuilder;

impl<Node> PayloadValidatorBuilder<Node> for MorphEngineValidatorBuilder
where
    Node: FullNodeComponents<Types = MorphNode>,
{
    type Validator = MorphEngineValidator;

    async fn build(self, _ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(MorphEngineValidator::new())
    }
}

/// Morph engine validator for payload validation.
///
/// This validator is used by the engine API to validate incoming payloads.
/// For Morph, most validation is deferred to the consensus layer.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct MorphEngineValidator;

impl MorphEngineValidator {
    /// Creates a new [`MorphEngineValidator`].
    pub const fn new() -> Self {
        Self
    }
}

impl PayloadValidator<MorphPayloadTypes> for MorphEngineValidator {
    type Block = morph_primitives::Block;

    fn convert_payload_to_block(
        &self,
        payload: MorphExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        Ok(Arc::unwrap_or_clone(payload.block))
    }

    fn validate_payload_attributes_against_header(
        &self,
        attr: &<MorphPayloadTypes as reth_node_api::PayloadTypes>::PayloadAttributes,
        header: &MorphHeader,
    ) -> Result<(), InvalidPayloadAttributesError> {
        // Ensure that payload attributes timestamp is not in the past
        if attr.timestamp() < header.timestamp() {
            return Err(InvalidPayloadAttributesError::InvalidTimestamp);
        }
        Ok(())
    }
}
