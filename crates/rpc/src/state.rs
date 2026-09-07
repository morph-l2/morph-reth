//! State-provider routing for bounded historical proofs.

use alloy_eips::BlockId;
use alloy_primitives::B256;
use jsonrpsee::types::ErrorObject;
use morph_proofs::{MorphProofsStorage, MorphProofsStore, provider::MorphProofsStateProviderRef};
use reth_provider::{
    BlockHashReader, BlockIdReader, ProviderError, StateProvider, StateProviderFactory,
};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::{EthApiError, error::ToRpcError};
use thiserror::Error;
use tracing::warn;

use crate::error::MorphEthApiError;

/// A request cannot be served by the current durable proof window.
///
/// `Display` carries the full diagnosis for server-side logs; [`ToRpcError`] deliberately sends the
/// client less than that. See [`Self::to_rpc_error`].
#[derive(Debug, Error, PartialEq, Eq)]
enum ProofWindowError {
    #[error("historical proof database is not initialized")]
    Uninitialized,
    #[error("block {requested} is outside the historical proof window [{earliest}, {latest}]")]
    Outside {
        requested: u64,
        earliest: u64,
        latest: u64,
    },
    #[error(
        "historical proof database is not canonical at block {block}: stored {stored}, canonical {canonical:?}"
    )]
    CanonicalMismatch {
        block: u64,
        stored: B256,
        canonical: Option<B256>,
    },
}

impl ToRpcError for ProofWindowError {
    /// Reports every variant as `StateNotAvailable` (-32005).
    ///
    /// These are client conditions — a block tag this node cannot serve a proof for — so they must
    /// not surface as internal server errors, which is what any error reaching reth through
    /// `ProviderError` becomes: only a handful of `ProviderError` variants are specialised in
    /// `EthApiError::from`, and the rest fall through to `Internal`. Routing through
    /// [`EthApiError::other`] keeps the code under our control and spares clients from
    /// string-matching on messages.
    ///
    /// Not EIP-4444's 4444 (`PrunedHistoryUnavailable`), which is about missing block history; what
    /// is unavailable here is a state proof for a block whose header and body we still serve.
    ///
    /// `CanonicalMismatch` sends no hashes. The canonical hash is public via `eth_getBlockByNumber`,
    /// but the stored one would tell a caller which abandoned branch this node's proof database is
    /// stuck on, which is not otherwise observable. The full pair stays in the log line emitted at
    /// the call site.
    fn to_rpc_error(&self) -> ErrorObject<'static> {
        let message = match self {
            Self::Uninitialized => {
                "state proof unavailable: historical proof database is not initialized".to_string()
            }
            Self::Outside {
                requested,
                earliest,
                latest,
            } => format!(
                "state proof unavailable for block {requested}: outside the retained proof window [{earliest}, {latest}]"
            ),
            Self::CanonicalMismatch { block, .. } => {
                format!("state proof unavailable for block {block}")
            }
        };
        ErrorObject::owned(
            MorphEthApiError::STATE_NOT_AVAILABLE_CODE,
            message,
            None::<()>,
        )
    }
}

impl From<ProofWindowError> for EthApiError {
    fn from(error: ProofWindowError) -> Self {
        Self::other(error)
    }
}

fn validate_canonical_anchor(
    block: u64,
    stored: B256,
    canonical: Option<B256>,
) -> Result<(), ProofWindowError> {
    if canonical != Some(stored) {
        return Err(ProofWindowError::CanonicalMismatch {
            block,
            stored,
            canonical,
        });
    }
    Ok(())
}

fn validate_window(
    requested: u64,
    earliest: Option<u64>,
    latest: Option<u64>,
) -> Result<(), ProofWindowError> {
    let (Some(earliest), Some(latest)) = (earliest, latest) else {
        return Err(ProofWindowError::Uninitialized);
    };
    if requested < earliest || requested > latest {
        return Err(ProofWindowError::Outside {
            requested,
            earliest,
            latest,
        });
    }
    Ok(())
}

/// Builds providers that source account/storage state only from Morph proof history.
#[derive(Debug)]
pub struct MorphProofStateProviderFactory<Eth, P> {
    eth_api: Eth,
    storage: MorphProofsStorage<P>,
}

impl<Eth, P> MorphProofStateProviderFactory<Eth, P> {
    /// Creates a proof state-provider factory.
    pub const fn new(eth_api: Eth, storage: MorphProofsStorage<P>) -> Self {
        Self { eth_api, storage }
    }

    /// Returns the underlying Eth API used for proof task scheduling and chain reads.
    pub const fn eth_api(&self) -> &Eth {
        &self.eth_api
    }
}

// Hand-written so the `Clone` bounds land on the impl rather than the struct: proof handlers clone
// the factory into a blocking task, where the returned provider only borrows the local clone.
impl<Eth: Clone, P: Clone> Clone for MorphProofStateProviderFactory<Eth, P> {
    fn clone(&self) -> Self {
        Self {
            eth_api: self.eth_api.clone(),
            storage: self.storage.clone(),
        }
    }
}

impl<'a, Eth, P> MorphProofStateProviderFactory<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'a,
{
    /// Returns a provider only when `block_id` is inside the durable proof window.
    ///
    /// Returns [`EthApiError`] rather than `ProviderError` so the window conditions keep their own
    /// RPC error code: a `ProviderError` would be folded into `EthApiError::Internal` by reth's
    /// blanket arm, turning a client condition into a server error. See
    /// [`ProofWindowError::to_rpc_error`].
    pub fn state_provider(
        &'a self,
        block_id: Option<BlockId>,
    ) -> Result<Box<dyn StateProvider + 'a>, EthApiError> {
        let block_id = block_id.unwrap_or_default();
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))?;
        if let BlockId::Hash(requested) = block_id {
            let canonical_hash = self.eth_api.provider().block_hash(block_number)?;
            if canonical_hash != Some(requested.block_hash) {
                return Err(EthApiError::HeaderNotFound(block_id));
            }
        }

        // Pin window validation and all proof cursors to one MDBX snapshot. Otherwise a prune
        // committed between the bounds check and proof generation could remap version zero to a
        // newer baseline and produce a proof for the wrong state root.
        let proof_tx = self
            .storage
            .ro_tx()
            .map_err(|error| ProviderError::Database(error.into()))?;
        let earliest = self
            .storage
            .get_earliest_block_number_with_tx(&proof_tx)
            .map_err(|error| ProviderError::Database(error.into()))?
            .map(|(number, _)| number);
        let latest = self
            .storage
            .get_latest_block_number_with_tx(&proof_tx)
            .map_err(|error| ProviderError::Database(error.into()))?;
        validate_window(block_number, earliest, latest.map(|(number, _)| number))?;

        let (latest_number, latest_hash) = latest.ok_or(ProofWindowError::Uninitialized)?;
        let canonical_latest_hash = self.eth_api.provider().block_hash(latest_number)?;
        if let Err(error) =
            validate_canonical_anchor(latest_number, latest_hash, canonical_latest_hash)
        {
            // The hashes identify the branch this node's proof database is stuck on, so they stay
            // here and never reach the caller.
            warn!(
                target: "morph::rpc",
                %error,
                "Refusing proof request: proof history is not on the canonical chain"
            );
            return Err(error.into());
        }

        // Bytecode is content-addressed and block hashes are canonical-chain data, so a latest
        // provider is sufficient for those auxiliary reads. Account/storage trie reads below are
        // always routed to proof history; no Reth historical overlay is constructed.
        let auxiliary = self.eth_api.provider().latest()?;
        Ok(Box::new(MorphProofsStateProviderRef::new_with_tx(
            auxiliary,
            &self.storage,
            block_number,
            proof_tx,
        )))
    }
}

#[cfg(test)]
mod tests {
    use alloy_primitives::B256;
    use reth_rpc_eth_types::error::ToRpcError;

    use super::{MorphEthApiError, ProofWindowError, validate_canonical_anchor, validate_window};

    #[test]
    fn rejects_an_empty_window() {
        assert_eq!(
            validate_window(10, None, None),
            Err(ProofWindowError::Uninitialized)
        );
    }

    #[test]
    fn accepts_only_inclusive_window_bounds() {
        assert!(validate_window(10, Some(10), Some(20)).is_ok());
        assert!(validate_window(20, Some(10), Some(20)).is_ok());
        assert!(matches!(
            validate_window(9, Some(10), Some(20)),
            Err(ProofWindowError::Outside { .. })
        ));
        assert!(matches!(
            validate_window(21, Some(10), Some(20)),
            Err(ProofWindowError::Outside { .. })
        ));
    }

    #[test]
    fn fails_closed_while_proof_history_is_on_a_stale_fork() {
        let stored = B256::repeat_byte(0x11);
        assert!(validate_canonical_anchor(20, stored, Some(stored)).is_ok());
        assert!(matches!(
            validate_canonical_anchor(20, stored, Some(B256::repeat_byte(0x22))),
            Err(ProofWindowError::CanonicalMismatch { block: 20, .. })
        ));
        assert!(matches!(
            validate_canonical_anchor(20, stored, None),
            Err(ProofWindowError::CanonicalMismatch { block: 20, .. })
        ));
    }

    #[test]
    fn reports_every_window_condition_as_state_not_available() {
        // Not an internal server error: these are all "this node cannot serve a proof for that
        // block", which the client fixes by choosing another block, not by retrying.
        for error in [
            ProofWindowError::Uninitialized,
            ProofWindowError::Outside {
                requested: 9,
                earliest: 10,
                latest: 20,
            },
            ProofWindowError::CanonicalMismatch {
                block: 20,
                stored: B256::repeat_byte(0x11),
                canonical: Some(B256::repeat_byte(0x22)),
            },
        ] {
            assert_eq!(
                error.to_rpc_error().code(),
                MorphEthApiError::STATE_NOT_AVAILABLE_CODE,
                "unexpected code for {error:?}"
            );
        }
    }

    #[test]
    fn keeps_the_window_bounds_in_the_client_message() {
        // The bounds are already public through `debug_proofsSyncStatus`, and a client needs them
        // to pick a servable block.
        let message = ProofWindowError::Outside {
            requested: 9,
            earliest: 10,
            latest: 20,
        }
        .to_rpc_error()
        .message()
        .to_string();
        assert!(message.contains('9'), "{message}");
        assert!(message.contains("[10, 20]"), "{message}");
    }

    #[test]
    fn withholds_branch_hashes_from_the_client() {
        let stored = B256::repeat_byte(0x11);
        let canonical = B256::repeat_byte(0x22);
        let error = ProofWindowError::CanonicalMismatch {
            block: 20,
            stored,
            canonical: Some(canonical),
        };

        // `stored` would reveal which abandoned branch this node's proof database is stuck on.
        let message = error.to_rpc_error().message().to_string();
        assert!(
            !message.contains(&stored.to_string()) && !message.contains(&canonical.to_string()),
            "hashes must not reach the client: {message}"
        );
        assert!(message.contains("20"), "{message}");

        // The full pair stays available for the server-side log line.
        let logged = error.to_string();
        assert!(logged.contains(&stored.to_string()), "{logged}");
    }
}
