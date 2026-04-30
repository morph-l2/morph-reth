//! Persistent Morph transaction reference index.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use alloy_consensus as _;
use alloy_rlp as _;
use morph_chainspec as _;
use morph_primitives as _;
use reth_codecs as _;
use reth_primitives_traits as _;
use reth_provider as _;
use reth_storage_api as _;
use tokio as _;
use tracing as _;

pub mod backfill;
pub mod db;
pub mod reader;
pub mod reconcile;
pub mod tables;
pub mod types;
pub mod writer;

pub use db::ReferenceIndexDb;
pub use reader::ReferenceIndexReader;
pub use types::{
    BackfillState, JADE_NOT_ACTIVE_SENTINEL, ReferenceIndexError, ReferenceQuery,
    ReferenceTransactionResult, SCHEMA_VERSION,
};

/// Default number of canonical blocks the ExEx may lag before RPC returns an error.
pub const DEFAULT_LAG_THRESHOLD: u64 = 16;

/// Default number of blocks checked during startup reconcile for offline reorgs.
pub const DEFAULT_MAX_REORG_DEPTH: u64 = 64;

/// Default backfill batch size.
pub const DEFAULT_BACKFILL_BATCH_BLOCKS: u64 = 256;
