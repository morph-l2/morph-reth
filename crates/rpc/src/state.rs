//! State-provider routing for bounded historical proofs.

use alloy_eips::BlockId;
use alloy_primitives::B256;
use morph_proofs::{MorphProofsStorage, MorphProofsStore, provider::MorphProofsStateProviderRef};
use reth_provider::{
    BlockHashReader, BlockIdReader, ProviderError, ProviderResult, StateProvider,
    StateProviderFactory,
};
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;
use thiserror::Error;

/// A request cannot be served by the current durable proof window.
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
}

impl<'a, Eth, P> MorphProofStateProviderFactory<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'a,
{
    /// Returns a provider only when `block_id` is inside the durable proof window.
    pub fn state_provider(
        &'a self,
        block_id: Option<BlockId>,
    ) -> ProviderResult<Box<dyn StateProvider + 'a>> {
        let block_id = block_id.unwrap_or_default();
        let block_number = self
            .eth_api
            .provider()
            .block_number_for_id(block_id)?
            .ok_or(EthApiError::HeaderNotFound(block_id))
            .map_err(ProviderError::other)?;
        if let BlockId::Hash(requested) = block_id {
            let canonical_hash = self.eth_api.provider().block_hash(block_number)?;
            if canonical_hash != Some(requested.block_hash) {
                return Err(ProviderError::other(EthApiError::HeaderNotFound(block_id)));
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
        validate_window(block_number, earliest, latest.map(|(number, _)| number))
            .map_err(ProviderError::other)?;

        let (latest_number, latest_hash) =
            latest.ok_or_else(|| ProviderError::other(ProofWindowError::Uninitialized))?;
        let canonical_latest_hash = self.eth_api.provider().block_hash(latest_number)?;
        validate_canonical_anchor(latest_number, latest_hash, canonical_latest_hash)
            .map_err(ProviderError::other)?;

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

    use super::{ProofWindowError, validate_canonical_anchor, validate_window};

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
}
