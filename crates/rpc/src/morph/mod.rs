//! Morph-specific `morph_` RPC namespace.
//!
//! Currently provides `morph_getTransactionHashesByReference` backed by the
//! persistent reference index.

pub mod handler;
pub mod rpc;

pub use handler::{MorphRpc, MorphRpcHandler};
pub use rpc::{MorphRpcServer, ReferenceQueryArgs};
