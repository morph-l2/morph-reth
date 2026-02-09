//! Morph `eth_` RPC wiring and conversions.

use crate::MorphEthApiError;
use crate::eth::receipt::MorphReceiptConverter;
use crate::types::{MorphRpcReceipt, MorphRpcTransaction, MorphTransactionRequest};
use alloy_rpc_types_eth::Header as RpcHeader;
use eyre::Result;
use morph_chainspec::MorphChainSpec;
use morph_evm::MorphEvmConfig;
use morph_primitives::{MorphHeader, MorphPrimitives};
use reth_evm::ConfigureEvm;
use reth_node_api::{FullNodeComponents, FullNodeTypes, HeaderTy, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx};
use reth_provider::ChainSpecProvider;
use reth_rpc::EthApi;
use reth_rpc_convert::{RpcConvert, RpcConverter, RpcTypes};
use reth_rpc_eth_api::{
    EthApiTypes, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        EthApiSpec, EthBlocks, EthFees, EthState, EthTransactions, LoadBlock, LoadFee,
        LoadPendingBlock, LoadState, LoadTransaction, SpawnBlocking, Trace,
        pending_block::{BuildPendingEnv, PendingEnvBuilder},
    },
};
use reth_rpc_eth_types::{EthApiError, EthStateCache, FeeHistoryCache, GasPriceOracle};
use reth_tasks::{
    TaskSpawner,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use std::{fmt, marker::PhantomData, sync::Arc, time::Duration};

pub mod call;
pub mod receipt;
pub mod transaction;

// ===== RPC type wiring =====
/// Morph RPC response type definitions.
#[derive(Clone, Debug, Default)]
pub struct MorphRpcTypes;

impl RpcTypes for MorphRpcTypes {
    type Header = RpcHeader<MorphHeader>;
    type Receipt = MorphRpcReceipt;
    type TransactionResponse = MorphRpcTransaction;
    type TransactionRequest = MorphTransactionRequest;
}

/// Morph RPC converter with custom receipt and header conversion.
pub type MorphRpcConverter<N, NetworkT> =
    RpcConverter<NetworkT, <N as FullNodeComponents>::Evm, MorphReceiptConverter>;

// ===== Builder entrypoint =====
/// Builder for Morph `eth_` RPC that installs Morph RPC conversions.
#[derive(Debug)]
pub struct MorphEthApiBuilder<NetworkT = MorphRpcTypes> {
    _nt: PhantomData<NetworkT>,
}

impl<NetworkT> Default for MorphEthApiBuilder<NetworkT> {
    fn default() -> Self {
        Self { _nt: PhantomData }
    }
}

impl<NetworkT> MorphEthApiBuilder<NetworkT> {
    /// Creates a new builder.
    pub fn new() -> Self {
        Self::default()
    }
}

impl<N, NetworkT> EthApiBuilder<N> for MorphEthApiBuilder<NetworkT>
where
    N: FullNodeComponents<
            Evm = MorphEvmConfig,
            Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
            Types: NodeTypes<Primitives = MorphPrimitives, ChainSpec = MorphChainSpec>,
            Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
        > + crate::eth::MorphNodeCore
        + reth_rpc_eth_api::RpcNodeCore<
            Provider = <N as FullNodeTypes>::Provider,
            Pool = <N as FullNodeComponents>::Pool,
        >,
    NetworkT: RpcTypes<
            Header = RpcHeader<MorphHeader>,
            Receipt = MorphRpcReceipt,
            TransactionResponse = MorphRpcTransaction,
            TransactionRequest = MorphTransactionRequest,
        >,
    MorphRpcConverter<N, NetworkT>: reth_rpc_convert::RpcConvert<
            Network = NetworkT,
            Primitives = <N as reth_rpc_eth_api::RpcNodeCore>::Primitives,
            Error = reth_rpc_eth_types::EthApiError,
            Evm = <N as reth_rpc_eth_api::RpcNodeCore>::Evm,
        >,
{
    type EthApi = MorphEthApi<N, MorphRpcConverter<N, NetworkT>>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> Result<Self::EthApi> {
        let rpc_converter = RpcConverter::new(MorphReceiptConverter::default());
        Ok(MorphEthApi::new(
            ctx.eth_api_builder()
                .with_rpc_converter(rpc_converter)
                .build(),
        ))
    }
}

// ===== Core API wrapper =====
/// Adapter for [`EthApi`], which holds all the data required to serve core `eth_` API.
pub type EthApiNodeBackend<N, Rpc> = EthApi<N, Rpc>;

/// A helper trait with requirements for [`RpcNodeCore`] to be used in [`MorphEthApi`].
pub trait MorphNodeCore: RpcNodeCore<Evm = MorphEvmConfig, Primitives = MorphPrimitives> {}
impl<T> MorphNodeCore for T where T: RpcNodeCore<Evm = MorphEvmConfig, Primitives = MorphPrimitives> {}

/// Morph `Eth` API implementation.
///
/// This wraps a default `Eth` implementation, and provides additional functionality
/// where the Morph spec deviates from the default (ethereum) spec, e.g. L1 fee
/// and ERC20 fee token support in `eth_estimateGas`.
pub struct MorphEthApi<N: MorphNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    inner: Arc<MorphEthApiInner<N, Rpc>>,
}

impl<N: MorphNodeCore, Rpc: RpcConvert> Clone for MorphEthApi<N, Rpc> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<N: MorphNodeCore, Rpc: RpcConvert> MorphEthApi<N, Rpc> {
    /// Creates a new [`MorphEthApi`].
    pub fn new(eth_api: EthApiNodeBackend<N, Rpc>) -> Self {
        let inner = Arc::new(MorphEthApiInner { eth_api });
        Self { inner }
    }

    /// Returns a reference to the [`EthApiNodeBackend`].
    pub fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        self.inner.eth_api()
    }
}

// ===== Trait implementations =====
impl<N, Rpc> EthApiTypes for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    type Error = MorphEthApiError;
    type NetworkTypes = Rpc::Network;
    type RpcConvert = Rpc;

    fn converter(&self) -> &Self::RpcConvert {
        self.inner.eth_api.converter()
    }
}

impl<N, Rpc> RpcNodeCore for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    type Primitives = N::Primitives;
    type Provider = N::Provider;
    type Pool = N::Pool;
    type Evm = <N as RpcNodeCore>::Evm;
    type Network = <N as RpcNodeCore>::Network;

    #[inline]
    fn pool(&self) -> &Self::Pool {
        self.inner.eth_api.pool()
    }

    #[inline]
    fn evm_config(&self) -> &Self::Evm {
        self.inner.eth_api.evm_config()
    }

    #[inline]
    fn network(&self) -> &Self::Network {
        self.inner.eth_api.network()
    }

    #[inline]
    fn provider(&self) -> &Self::Provider {
        self.inner.eth_api.provider()
    }
}

impl<N, Rpc> RpcNodeCoreExt for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn cache(&self) -> &EthStateCache<N::Primitives> {
        self.inner.eth_api.cache()
    }
}

impl<N, Rpc> EthApiSpec for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn starting_block(&self) -> alloy_primitives::U256 {
        self.inner.eth_api.starting_block()
    }
}

impl<N, Rpc> SpawnBlocking for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn io_task_spawner(&self) -> impl TaskSpawner {
        self.inner.eth_api.task_spawner()
    }

    #[inline]
    fn tracing_task_pool(&self) -> &BlockingTaskPool {
        self.inner.eth_api.blocking_task_pool()
    }

    #[inline]
    fn tracing_task_guard(&self) -> &BlockingTaskGuard {
        self.inner.eth_api.blocking_task_guard()
    }

    #[inline]
    fn blocking_io_task_guard(&self) -> &std::sync::Arc<tokio::sync::Semaphore> {
        self.inner.eth_api.blocking_io_request_semaphore()
    }
}

impl<N, Rpc> LoadFee for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    #[inline]
    fn gas_oracle(&self) -> &GasPriceOracle<Self::Provider> {
        self.inner.eth_api.gas_oracle()
    }

    #[inline]
    fn fee_history_cache(&self) -> &FeeHistoryCache<reth_provider::ProviderHeader<N::Provider>> {
        self.inner.eth_api.fee_history_cache()
    }
}

impl<N, Rpc> LoadPendingBlock for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
    fn pending_block(
        &self,
    ) -> &tokio::sync::Mutex<Option<reth_rpc_eth_types::PendingBlock<N::Primitives>>> {
        self.inner.eth_api.pending_block()
    }

    fn pending_env_builder(&self) -> &dyn PendingEnvBuilder<N::Evm> {
        self.inner.eth_api.pending_env_builder()
    }

    fn pending_block_kind(&self) -> reth_rpc_eth_types::builder::config::PendingBlockKind {
        self.inner.eth_api.pending_block_kind()
    }
}

impl<N, Rpc> LoadBlock for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> EthBlocks for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> LoadTransaction for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

impl<N, Rpc> EthTransactions for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
    #[inline]
    fn signers(
        &self,
    ) -> &reth_rpc_eth_api::helpers::spec::SignersForRpc<Self::Provider, Self::NetworkTypes> {
        self.inner.eth_api.signers()
    }

    #[inline]
    fn send_raw_transaction_sync_timeout(&self) -> Duration {
        self.inner.eth_api.send_raw_transaction_sync_timeout()
    }

    async fn send_transaction(
        &self,
        tx: reth_primitives_traits::WithEncoded<
            reth_primitives_traits::Recovered<reth_transaction_pool::PoolPooledTx<Self::Pool>>,
        >,
    ) -> Result<alloy_primitives::B256, Self::Error> {
        reth_rpc_eth_api::helpers::EthTransactions::send_transaction(&self.inner.eth_api, tx)
            .await
            .map_err(Into::into)
    }
}

impl<N, Rpc> EthFees for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> LoadState for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    Self: LoadPendingBlock,
{
}

impl<N, Rpc> EthState for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    Self: LoadPendingBlock,
{
    #[inline]
    fn max_proof_window(&self) -> u64 {
        self.inner.eth_api.eth_proof_window()
    }
}

impl<N, Rpc> Trace for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
}

// ===== Internal container =====
impl<N: MorphNodeCore, Rpc: RpcConvert> fmt::Debug for MorphEthApi<N, Rpc> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MorphEthApi").finish_non_exhaustive()
    }
}

/// Container type `MorphEthApi`
#[allow(missing_debug_implementations)]
pub struct MorphEthApiInner<N: MorphNodeCore, Rpc: RpcConvert> {
    /// Gateway to node's core components.
    pub eth_api: EthApiNodeBackend<N, Rpc>,
}

impl<N: MorphNodeCore, Rpc: RpcConvert> MorphEthApiInner<N, Rpc> {
    /// Returns a reference to the [`EthApiNodeBackend`].
    const fn eth_api(&self) -> &EthApiNodeBackend<N, Rpc> {
        &self.eth_api
    }
}
