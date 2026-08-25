use alloy_primitives::{B256, U64};
use serde::{Deserialize, Serialize};

/// Canonical chain snapshot used to linearize reference-index queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanonicalTip {
    /// Canonical head block number.
    pub number: u64,
    /// Canonical head block hash.
    pub hash: B256,
    /// Canonical head timestamp.
    pub timestamp: u64,
}

/// Current reference index database schema version.
///
/// Version 1 was used only by the pre-release ExEx prototype. There is no
/// migration path: operators remove that derived database manually.
pub const SCHEMA_VERSION: u32 = 2;

/// Validated query parameters for reference lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceQuery {
    pub(crate) reference: B256,
    pub(crate) offset: u64,
    pub(crate) limit: u64,
}

impl ReferenceQuery {
    /// Default page size.
    pub const DEFAULT_LIMIT: u64 = 100;
    /// Maximum accepted page size.
    pub const MAX_LIMIT: u64 = 100;
    /// Maximum accepted result offset.
    pub const MAX_OFFSET: u64 = 10_000;

    /// Validate and normalize public RPC query arguments.
    pub fn new(
        reference: B256,
        offset: Option<u64>,
        limit: Option<u64>,
    ) -> Result<Self, ReferenceIndexError> {
        let offset = offset.unwrap_or_default();
        let limit = limit.unwrap_or(Self::DEFAULT_LIMIT);

        if limit > Self::MAX_LIMIT {
            return Err(ReferenceIndexError::LimitTooLarge { limit });
        }
        if offset > Self::MAX_OFFSET {
            return Err(ReferenceIndexError::OffsetTooLarge { offset });
        }

        Ok(Self {
            reference,
            offset,
            limit,
        })
    }
}

/// RPC result entry for a Morph transaction reference hit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceTransactionResult {
    /// Hash of the matching Morph transaction.
    pub transaction_hash: B256,
    /// Canonical block number containing the transaction.
    pub block_number: U64,
    /// Timestamp of the containing canonical block.
    pub block_timestamp: U64,
    /// Zero-based transaction position within the block.
    pub transaction_index: U64,
}

/// Errors returned by the reference index.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceIndexError {
    #[error("reference index initializing")]
    Initializing,
    #[error("reference index is behind")]
    IndexBehind,
    #[error("reference index is unavailable")]
    IndexUnavailable,
    #[error("reference query limit too large: {limit}")]
    LimitTooLarge { limit: u64 },
    #[error("reference query offset too large: {offset}")]
    OffsetTooLarge { offset: u64 },
    #[error("reference index backfill batch size must be greater than zero")]
    InvalidBackfillBatchSize,
    #[error("reference index metadata is incomplete: {0}")]
    CorruptMetadata(&'static str),
    #[error("reference index chain identity mismatch: {0}")]
    ChainIdentityMismatch(&'static str),
    #[error("reference index schema mismatch: expected {expected}, got {actual}")]
    SchemaMismatch { expected: u32, actual: u32 },
    #[error(
        "reference index has no canonical ancestor at or after Jade block {jade_first_block}; manual rebuild required"
    )]
    ManualRebuildRequired { jade_first_block: u64 },
    #[error(transparent)]
    Database(#[from] reth_db_api::DatabaseError),
    #[error(transparent)]
    Provider(#[from] reth_errors::ProviderError),
    #[error(transparent)]
    Other(#[from] eyre::Report),
}

impl ReferenceIndexError {
    /// Errors that cannot be repaired while the existing derived DB remains.
    pub(crate) fn requires_manual_rebuild(&self) -> bool {
        matches!(
            self,
            Self::CorruptMetadata(_)
                | Self::ChainIdentityMismatch(_)
                | Self::SchemaMismatch { .. }
                | Self::ManualRebuildRequired { .. }
        )
    }
}
