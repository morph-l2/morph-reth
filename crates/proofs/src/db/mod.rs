//! MDBX-backed implementation of [`MorphProofsStore`](crate::MorphProofsStore).

mod models;
pub use models::*;

mod store;
pub use store::{MdbxProofsStorage, MdbxProofsStorageOptions, ProofDbIdentity, ProofWindowBounds};

mod cursor;
pub use cursor::{
    BlockNumberVersionedCursor, Dup, MdbxAccountCursor, MdbxStorageCursor, MdbxTrieCursor,
};

mod batch;
pub use batch::{DupRw, MdbxBatchSession};
