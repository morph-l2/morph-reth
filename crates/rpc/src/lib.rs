//! Morph RPC implementation and type conversions.

#![cfg_attr(docsrs, feature(doc_cfg, doc_auto_cfg))]

pub mod error;
pub mod eth;
pub mod types;

pub use error::MorphEthApiError;
pub use eth::{MorphEthApi, MorphEthApiBuilder, MorphRpcConverter, MorphRpcTypes};
pub use types::*;
