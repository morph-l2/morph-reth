//! Persistent Morph transaction reference index.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_consensus as _;
use alloy_rlp as _;
use morph_primitives as _;
use reth_codecs as _;
use reth_primitives_traits as _;
use reth_provider as _;
use reth_storage_api as _;
use tokio as _;
use tracing as _;

mod backfill;
mod db;
mod metrics;
mod reader;
mod reconcile;
mod runtime;
mod source;
mod tables;
mod types;
mod writer;

pub use reader::{ReferenceIndexHandle, ReferenceIndexPhase};
pub use runtime::{ReferenceIndexConfig, ReferenceIndexRuntime};
pub use source::{CanonicalBlock, CanonicalChain};
pub use types::{
    CanonicalTip, ReferenceIndexError, ReferenceQuery, ReferenceTransactionResult, SCHEMA_VERSION,
};

/// Default backfill batch size.
pub(crate) const DEFAULT_BACKFILL_BATCH_BLOCKS: u64 = 512;

/// Operational live-lag service-level objective. RPC readiness remains lag zero.
pub(crate) const LIVE_LAG_SLO: u64 = 16;
