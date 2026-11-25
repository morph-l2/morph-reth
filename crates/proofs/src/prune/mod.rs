//! Proof storage pruner for removing stale trie data.

mod error;
pub use error::{MorphProofStoragePrunerResult, PrunerError, PrunerOutput};

mod pruner;
pub use pruner::{DEFAULT_PRUNE_BATCH_SIZE, MorphProofStoragePruner, persisted_canonical_head};

mod metrics;

mod task;
pub use task::MorphProofStoragePrunerTask;
