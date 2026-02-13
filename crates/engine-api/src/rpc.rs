//! Morph L2 Engine API JSON-RPC handler.
//!
//! This module implements the JSON-RPC interface for the L2 Engine API,
//! allowing the sequencer to interact with the execution layer via RPC.

use crate::{EngineApiResult, api::MorphL2EngineApi};
use alloy_primitives::B256;
use jsonrpsee::{core::RpcResult, proc_macros::rpc, RpcModule};
use morph_payload_types::{AssembleL2BlockParams, ExecutableL2Data, GenericResponse, SafeL2Data};
use morph_primitives::MorphHeader;
use reth_rpc_api::IntoEngineApiRpcModule;
use std::sync::Arc;

/// Morph L2 Engine RPC API trait.
///
/// This trait defines the JSON-RPC interface for the L2 Engine API.
/// It uses the `jsonrpsee` proc macro to generate the RPC server implementation.
#[rpc(server, namespace = "engine")]
pub trait MorphL2EngineRpc {
    /// Build a new L2 block with the given transactions.
    ///
    /// # JSON-RPC Method
    ///
    /// `engine_assembleL2Block`
    #[method(name = "assembleL2Block")]
    async fn assemble_l2_block(&self, params: AssembleL2BlockParams)
    -> RpcResult<ExecutableL2Data>;

    /// Validate an L2 block without importing it.
    ///
    /// # JSON-RPC Method
    ///
    /// `engine_validateL2Block`
    #[method(name = "validateL2Block")]
    async fn validate_l2_block(&self, data: ExecutableL2Data) -> RpcResult<GenericResponse>;

    /// Import and finalize a new L2 block.
    ///
    /// # JSON-RPC Method
    ///
    /// `engine_newL2Block`
    #[method(name = "newL2Block")]
    async fn new_l2_block(&self, data: ExecutableL2Data, batch_hash: Option<B256>)
    -> RpcResult<()>;

    /// Import a safe L2 block from derivation.
    ///
    /// # JSON-RPC Method
    ///
    /// `engine_newSafeL2Block`
    #[method(name = "newSafeL2Block")]
    async fn new_safe_l2_block(&self, data: SafeL2Data) -> RpcResult<MorphHeader>;
}

/// Implementation of the L2 Engine RPC API.
///
/// This struct wraps an implementation of `MorphL2EngineApi` and provides
/// the JSON-RPC interface.
#[derive(Debug, Clone)]
pub struct MorphL2EngineRpcHandler<Api> {
    inner: Arc<Api>,
}

impl<Api> MorphL2EngineRpcHandler<Api> {
    /// Creates a new `MorphL2EngineRpcHandler`.
    pub fn new(api: Api) -> Self {
        Self {
            inner: Arc::new(api),
        }
    }

    /// Creates a new `MorphL2EngineRpcHandler` from an `Arc`.
    pub fn from_arc(api: Arc<Api>) -> Self {
        Self { inner: api }
    }
}

#[async_trait::async_trait]
impl<Api> MorphL2EngineRpcServer for MorphL2EngineRpcHandler<Api>
where
    Api: MorphL2EngineApi + 'static,
{
    async fn assemble_l2_block(
        &self,
        params: AssembleL2BlockParams,
    ) -> RpcResult<ExecutableL2Data> {
        tracing::debug!(target: "morph::engine", block_number = params.number, "assembling L2 block");

        self.inner.assemble_l2_block(params).await.map_err(|e| {
            tracing::error!(target: "morph::engine", error = %e, "failed to assemble L2 block");
            e.into()
        })
    }

    async fn validate_l2_block(&self, data: ExecutableL2Data) -> RpcResult<GenericResponse> {
        tracing::debug!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            "validating L2 block"
        );

        self.inner.validate_l2_block(data).await.map_err(|e| {
            tracing::error!(target: "morph::engine", error = %e, "failed to validate L2 block");
            e.into()
        })
    }

    async fn new_l2_block(
        &self,
        data: ExecutableL2Data,
        batch_hash: Option<B256>,
    ) -> RpcResult<()> {
        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            block_hash = %data.hash,
            ?batch_hash,
            "importing new L2 block"
        );

        self.inner
            .new_l2_block(data, batch_hash)
            .await
            .map_err(|e| {
                tracing::error!(target: "morph::engine", error = %e, "failed to import L2 block");
                e.into()
            })
    }

    async fn new_safe_l2_block(&self, data: SafeL2Data) -> RpcResult<MorphHeader> {
        tracing::info!(
            target: "morph::engine",
            block_number = data.number,
            "importing safe L2 block"
        );

        self.inner.new_safe_l2_block(data).await.map_err(|e| {
            tracing::error!(target: "morph::engine", error = %e, "failed to import safe L2 block");
            e.into()
        })
    }
}

/// Converts an `EngineApiResult` into a `RpcResult`.
pub fn into_rpc_result<T>(result: EngineApiResult<T>) -> RpcResult<T> {
    result.map_err(|e| e.into_rpc_error())
}

/// Converts `MorphL2EngineRpcHandler` into an RPC module.
///
/// This implementation allows the handler to be used as an Engine API module
/// in reth's RPC server.
impl<Api> IntoEngineApiRpcModule for MorphL2EngineRpcHandler<Api>
where
    Api: MorphL2EngineApi + 'static,
{
    fn into_rpc_module(self) -> RpcModule<()> {
        self.into_rpc().remove_context()
    }
}
