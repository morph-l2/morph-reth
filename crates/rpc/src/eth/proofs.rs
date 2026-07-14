//! Historical EIP-1186 proof RPC override.

use std::time::Instant;

use alloy_eips::BlockId;
use alloy_primitives::Address;
use alloy_rpc_types_eth::EIP1186AccountProofResponse;
use alloy_serde::JsonStorageKey;
use async_trait::async_trait;
use jsonrpsee::{
    core::RpcResult,
    proc_macros::rpc,
    types::{ErrorCode, ErrorObject},
};
use morph_proofs::{MorphProofsStorage, MorphProofsStore};
use reth_metrics::{
    Metrics,
    metrics::{Counter, Histogram},
};
use reth_provider::StateProofProvider;
use reth_rpc_api::eth::helpers::FullEthApi;
use reth_rpc_eth_types::EthApiError;

use crate::state::MorphProofStateProviderFactory;

/// Maximum number of storage keys accepted by one `eth_getProof` request.
pub const MAX_PROOF_KEYS: usize = 1024;

/// Validates the storage-key count before any database work starts.
#[derive(Debug)]
pub struct ProofKeyLimit;

impl ProofKeyLimit {
    /// Rejects requests above [`MAX_PROOF_KEYS`].
    pub fn check(keys_len: usize) -> Result<(), ErrorObject<'static>> {
        if keys_len > MAX_PROOF_KEYS {
            return Err(ErrorObject::owned(
                ErrorCode::InvalidParams.code(),
                format!("too many storage keys: max {MAX_PROOF_KEYS}, got {keys_len}"),
                None::<()>,
            ));
        }
        Ok(())
    }
}

#[derive(Metrics, Clone)]
#[metrics(scope = "morph.rpc.proofs")]
struct ProofRpcMetrics {
    /// Total `eth_getProof` requests.
    requests_total: Counter,
    /// Successful `eth_getProof` responses.
    successful_responses_total: Counter,
    /// Failed `eth_getProof` responses.
    failures_total: Counter,
    /// Successful request latency in seconds.
    latency_seconds: Histogram,
}

#[cfg_attr(not(test), rpc(server, namespace = "eth"))]
#[cfg_attr(test, rpc(server, client, namespace = "eth"))]
pub trait EthProofApiOverride {
    /// Returns EIP-1186 account and storage proofs at a retained canonical block.
    #[method(name = "getProof")]
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse>;
}

/// `eth_getProof` implementation backed exclusively by Morph proof history.
#[derive(Debug)]
pub struct EthProofApiExt<Eth, P> {
    state_provider_factory: MorphProofStateProviderFactory<Eth, P>,
}

impl<Eth, P> EthProofApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    /// Creates the historical proof RPC override.
    pub const fn new(eth_api: Eth, storage: MorphProofsStorage<P>) -> Self {
        Self {
            state_provider_factory: MorphProofStateProviderFactory::new(eth_api, storage),
        }
    }
}

#[async_trait]
impl<Eth, P> EthProofApiOverrideServer for EthProofApiExt<Eth, P>
where
    Eth: FullEthApi + Send + Sync + 'static,
    P: MorphProofsStore + Clone + 'static,
{
    async fn get_proof(
        &self,
        address: Address,
        keys: Vec<JsonStorageKey>,
        block: Option<BlockId>,
    ) -> RpcResult<EIP1186AccountProofResponse> {
        ProofKeyLimit::check(keys.len())?;

        let metrics = ProofRpcMetrics::default();
        let start = Instant::now();
        metrics.requests_total.increment(1);
        let storage_keys = keys.iter().map(JsonStorageKey::as_b256).collect::<Vec<_>>();

        let result = (|| {
            let proof = self
                .state_provider_factory
                .state_provider(block)
                .map_err(|error| ErrorObject::from(EthApiError::from(error)))?
                .proof(Default::default(), address, &storage_keys)
                .map_err(|error| ErrorObject::from(EthApiError::from(error)))?;
            Ok(proof.into_eip1186_response(keys))
        })();

        match &result {
            Ok(_) => {
                metrics
                    .latency_seconds
                    .record(start.elapsed().as_secs_f64());
                metrics.successful_responses_total.increment(1);
            }
            Err(_) => metrics.failures_total.increment(1),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use jsonrpsee::types::ErrorCode;

    use super::{MAX_PROOF_KEYS, ProofKeyLimit};

    #[test]
    fn enforces_get_proof_key_limit() {
        assert!(ProofKeyLimit::check(MAX_PROOF_KEYS).is_ok());
        let error = ProofKeyLimit::check(MAX_PROOF_KEYS + 1).expect_err("must reject");
        assert_eq!(error.code(), ErrorCode::InvalidParams.code());
    }
}
