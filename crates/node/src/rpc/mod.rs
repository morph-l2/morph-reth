//! Morph RPC implementation
//!
//! This module provides the Morph-specific RPC types and API implementations.
//! Following Tempo's pattern, RPC is implemented as a submodule of the node crate.

pub mod error;
pub mod morph_api;
pub mod types;

pub use error::MorphEthApiError;
pub use morph_api::{MorphApiServer, MorphRpc};
pub use types::*;
