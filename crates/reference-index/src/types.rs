use alloy_primitives::{B256, U64};
use serde::{Deserialize, Serialize};

/// Current reference index database schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Stored Jade activation sentinel for chains where Jade has not activated.
pub const JADE_NOT_ACTIVE_SENTINEL: u64 = u64::MAX;

/// Persistent backfill progress state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum BackfillState {
    NotStarted = 0,
    InProgress = 1,
    Complete = 2,
}

impl TryFrom<u8> for BackfillState {
    type Error = ReferenceIndexError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::NotStarted),
            1 => Ok(Self::InProgress),
            2 => Ok(Self::Complete),
            other => Err(ReferenceIndexError::InvalidBackfillState(other)),
        }
    }
}

/// Validated query parameters for reference lookups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReferenceQuery {
    pub reference: B256,
    pub offset: u64,
    pub limit: u64,
}

impl ReferenceQuery {
    pub const DEFAULT_LIMIT: u64 = 100;
    pub const MAX_LIMIT: u64 = 100;
    pub const MAX_OFFSET: u64 = 10_000;

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
    pub transaction_hash: B256,
    pub block_number: U64,
    pub block_timestamp: U64,
    pub transaction_index: U64,
}

/// Errors returned by the reference index.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceIndexError {
    #[error("reference index initializing")]
    Initializing,
    #[error("reference index is behind")]
    IndexBehind,
    #[error("reference query limit too large: {limit}")]
    LimitTooLarge { limit: u64 },
    #[error("reference query offset too large: {offset}")]
    OffsetTooLarge { offset: u64 },
    #[error("invalid backfill state: {0}")]
    InvalidBackfillState(u8),
    #[error("reference index chain identity mismatch: {0}")]
    ChainIdentityMismatch(&'static str),
    #[error("reference index schema mismatch: expected {expected}, got {actual}")]
    SchemaMismatch { expected: u32, actual: u32 },
    #[error(transparent)]
    Database(#[from] reth_db_api::DatabaseError),
    #[error(transparent)]
    Provider(#[from] reth_errors::ProviderError),
    #[error(transparent)]
    Other(#[from] eyre::Report),
}
