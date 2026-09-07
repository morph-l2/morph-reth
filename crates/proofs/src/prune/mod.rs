//! Proof storage pruner for removing stale trie data.

mod error;
pub use error::{MorphProofStoragePrunerResult, PrunerError, PrunerOutput};

mod pruner;
pub use pruner::MorphProofStoragePruner;

mod metrics;

mod task;
pub use task::MorphProofStoragePrunerTask;
