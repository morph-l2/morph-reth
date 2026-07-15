//! Bounded historical MPT storage for Morph `eth_getProof`.
//!
//! The versioned-trie implementation is adapted from Base's proof-history
//! storage at commit `b2673bbd927cb34d7cfad4d448bfbd5bd30eae88` under MIT.
//! Morph intentionally exposes only the MDBX production backend.

#![cfg_attr(not(test), warn(unused_crate_dependencies))]

pub mod api;
pub use api::{
    BlockStateDiff, InitialStateAnchor, InitialStateStatus, MorphProofsBatchSession,
    MorphProofsBatchStore, MorphProofsInitialStateStore, MorphProofsStore, OperationDurations,
    StorageBranchEntries, WriteCounts,
};

pub mod initialize;
pub use initialize::InitializationJob;

/// Default proof-history retention window: 7 days at a one-second block time.
pub const DEFAULT_PROOFS_HISTORY_WINDOW: u64 = 604_800;

#[cfg(any(test, feature = "test-utils"))]
pub mod in_memory;
#[cfg(any(test, feature = "test-utils"))]
pub use in_memory::{
    InMemoryAccountCursor, InMemoryBatchSession, InMemoryProofsStorage, InMemoryStorageCursor,
    InMemoryTrieCursor,
};

pub mod db;
pub use db::{
    MdbxAccountCursor, MdbxBatchSession, MdbxProofsStorage, MdbxProofsStorageOptions,
    MdbxStorageCursor, MdbxTrieCursor, ProofDbIdentity, ProofWindowBounds,
};

pub mod metrics;
pub use metrics::{
    MorphProofsHashedAccountCursor, MorphProofsHashedStorageCursor, MorphProofsStorage,
    MorphProofsTrieCursor, ProofHistoryMetrics,
};

pub mod proof;
pub mod provider;

mod batch_provider;
pub use batch_provider::MorphProofsBatchStateProviderRef;

pub mod live;

pub mod cursor;
pub mod cursor_factory;
pub use cursor_factory::{
    MorphProofsBatchHashedAccountCursorFactory, MorphProofsBatchTrieCursorFactory,
    MorphProofsHashedAccountCursorFactory, MorphProofsTrieCursorFactory,
};

pub mod error;
pub use error::{MorphProofsStorageError, MorphProofsStorageResult};

mod prune;
pub use prune::{
    MorphProofStoragePruner, MorphProofStoragePrunerResult, MorphProofStoragePrunerTask,
    PrunerError, PrunerOutput,
};
