//! Morph revm specific implementations.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod block;
// Suppress unused_crate_dependencies warnings
#[cfg(not(test))]
use tracing as _;
#[cfg(not(test))]
use alloy_consensus as _;
#[cfg(not(test))]
use alloy_sol_types as _;
#[cfg(not(test))]
use morph_primitives as _;

mod common;
pub use common::{MorphStateAccess, MorphTx};
pub mod error;
pub mod evm;
pub mod exec;
pub mod handler;
mod tx;

pub use block::MorphBlockEnv;
pub use error::{MorphHaltReason, MorphInvalidTransaction};
pub use evm::MorphEvm;
pub use tx::MorphTxEnv;
