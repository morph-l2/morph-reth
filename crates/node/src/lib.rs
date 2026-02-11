//! Morph node implementation.
//!
//! This crate provides the complete node implementation for Morph L2,
//! including node types, component builders, and RPC configuration.
//!
//! # Main Types
//!
//! - [`MorphNode`]: The main node type implementing reth's [`NodeTypes`] trait
//! - [`MorphAddOns`]: RPC add-ons with Morph-specific API extensions
//!
//! # Components
//!
//! The node is assembled from modular components:
//! - [`MorphPoolBuilder`]: Transaction pool with L1 fee validation
//! - [`MorphExecutorBuilder`]: EVM executor with Morph-specific logic
//! - [`MorphConsensusBuilder`]: Consensus validation for L2 blocks
//! - [`MorphPayloadBuilderBuilder`]: Block building with L1 message handling
//!

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod add_ons;
pub mod args;
pub mod components;
pub mod node;
pub mod validator;

// Re-export main node types
pub use add_ons::MorphAddOns;
pub use args::MorphArgs;
pub use components::{
    MorphConsensusBuilder, MorphExecutorBuilder, MorphPayloadBuilderBuilder, MorphPoolBuilder,
};
pub use node::{MorphNode, MorphPayloadAttributesBuilder};
pub use validator::{MorphEngineValidator, MorphEngineValidatorBuilder};

// Re-export morph-rpc for convenience
pub use morph_rpc as rpc;

// Re-export payload types
pub use morph_payload_types::{MorphExecutionData, MorphPayloadTypes};
