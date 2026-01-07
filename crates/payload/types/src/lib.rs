//! Morph payload types.
//!
//! This crate provides the core types used in the Morph Engine API, including:
//! - [`ExecutableL2Data`]: Block data for AssembleL2Block/ValidateL2Block/NewL2Block
//! - [`SafeL2Data`]: Safe block data for NewSafeL2Block (derivation)
//! - [`MorphPayloadAttributes`]: Extended payload attributes for block building
//! - [`MorphBuiltPayload`]: Built payload result
//!
//! These types are designed to be compatible with the go-ethereum L2 Engine API
//! while also supporting the standard Ethereum Engine API.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod attributes;
mod built;
mod executable_l2_data;
mod params;
mod safe_l2_data;

// Re-export main types
pub use attributes::{MorphPayloadAttributes, MorphPayloadBuilderAttributes};
pub use built::MorphBuiltPayload;
pub use executable_l2_data::ExecutableL2Data;
pub use params::{AssembleL2BlockParams, GenericResponse};
pub use safe_l2_data::SafeL2Data;
