//! Morph RPC implementation and type conversions.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod error;
pub mod eth;
pub mod eth_config;
pub mod morph;
pub mod proof_status;
pub mod state;
pub mod types;

pub use error::MorphEthApiError;
pub use eth::{MorphEthApi, MorphEthApiBuilder, MorphRpcConverter, MorphRpcTypes};
pub use eth_config::{MorphEthConfigApiServer, MorphEthConfigHandler};
pub use morph::{MorphRpc, MorphRpcHandler, MorphRpcServer, ReferenceQueryArgs};
pub use proof_status::{ProofStatusApiExt, ProofStatusApiOverrideServer, ProofsSyncStatus};
pub use types::*;
