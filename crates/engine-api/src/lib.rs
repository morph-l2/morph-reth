//! Morph L2 Engine API implementation.
//!
//! This crate provides the **custom L2 Engine API** for Morph, including:
//!
//! - [`MorphL2EngineApi`]: The L2 Engine API trait for block building and validation
//! - [`MorphL2EngineRpcServer`]: The JSON-RPC server implementation
//! - [`MorphValidationContext`]: Validation context with MPTFork hardfork support
//!
//! # L2 Engine API
//!
//! The L2 Engine API provides methods for the sequencer to interact with the
//! execution layer. These are **Morph-specific** methods, different from the
//! standard Ethereum Engine API:
//!
//! - `engine_assembleL2Block`: Build a new block with given transactions
//! - `engine_validateL2Block`: Validate a block without importing
//! - `engine_newL2Block`: Import and finalize a block
//! - `engine_newSafeL2Block`: Import a safe block from derivation

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

mod api;
mod builder;
mod error;
mod rpc;
mod validator;

pub use api::MorphL2EngineApi;
pub use builder::{MorphL2EngineApiBuilder, RealMorphL2EngineApi, StubMorphL2EngineApi};
pub use error::{EngineApiResult, MorphEngineApiError};
pub use rpc::{MorphL2EngineRpcHandler, MorphL2EngineRpcServer, into_rpc_result};
pub use validator::{MorphValidationContext, should_validate_state_root};
