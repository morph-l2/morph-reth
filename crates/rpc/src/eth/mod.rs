//! Morph `eth_` RPC wiring and conversions.

use crate::MorphEthApiError;
use crate::eth::receipt::MorphReceiptConverter;
use crate::types::{MorphRpcReceipt, MorphRpcTransaction, MorphTransactionRequest};
use alloy_primitives::U256;
use alloy_rpc_types_eth::Header as RpcHeader;
use eyre::Result;
use morph_chainspec::{MORPH_DEFAULT_PRIORITY_FEE, MorphChainSpec};
use morph_evm::MorphEvmConfig;
use morph_primitives::{MorphHeader, MorphPrimitives};
use reth_node_api::{FullNodeComponents, NodeTypes};
use reth_node_builder::rpc::{EthApiBuilder, EthApiCtx, RpcNodeCoreFor};
use reth_provider::ChainSpecProvider;
use reth_rpc::EthApi;
use reth_rpc_convert::{RpcConvert, RpcConverter, RpcTypes};
use reth_rpc_eth_api::{
    EthApiTypes, FromEthApiError, RpcNodeCore, RpcNodeCoreExt,
    helpers::{
        EthApiSpec, EthBlocks, EthFees, EthState, EthSubscriptions, EthTransactions, LoadBlock,
        LoadFee, LoadPendingBlock, LoadState, LoadTransaction, SpawnBlocking, Trace,
        bal::GetBlockAccessList, pending_block::PendingEnvBuilder,
    },
};
use reth_rpc_eth_types::{
    EthApiError, EthApiSettings, EthStateCache, FeeHistoryCache, GasPriceOracle,
};
use reth_tasks::{
    Runtime,
    pool::{BlockingTaskGuard, BlockingTaskPool},
};
use std::{fmt, marker::PhantomData, sync::Arc, time::Duration};

pub mod call;
pub mod proofs;
pub mod receipt;
pub mod transaction;

// ===== RPC type wiring =====
/// Morph RPC response type definitions.
#[derive(Clone, Debug, Default)]
pub struct MorphRpcTypes;

impl RpcTypes for MorphRpcTypes {
    type Header = RpcHeader<MorphHeader>;
    type Receipt = MorphRpcReceipt;
    type Log = alloy_rpc_types_eth::Log;
    type TransactionResponse = MorphRpcTransaction;
    type TransactionRequest = MorphTransactionRequest;
}

/// Morph RPC converter with custom receipt conversion.
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
            Types: NodeTypes<Primitives = MorphPrimitives, ChainSpec = MorphChainSpec>,
            Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
        >,
    NetworkT: RpcTypes<
            Header = RpcHeader<MorphHeader>,
            Receipt = MorphRpcReceipt,
            TransactionResponse = MorphRpcTransaction,
            TransactionRequest = MorphTransactionRequest,
        >,
    MorphRpcConverter<N, NetworkT>: reth_rpc_convert::RpcConvert<
            Network = NetworkT,
            Primitives = MorphPrimitives,
            Error = reth_rpc_eth_types::EthApiError,
            Evm = MorphEvmConfig,
        >,
{
    type EthApi = MorphEthApi<RpcNodeCoreFor<N>, MorphRpcConverter<N, NetworkT>>;

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

    fn eth_api_settings(&self) -> &EthApiSettings {
        self.inner.eth_api.eth_api_settings()
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
    fn io_task_spawner(&self) -> &Runtime {
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

    /// Suggests a priority fee using the single-sequencer L2 algorithm
    /// ([`GasPriceOracle::op_suggest_tip_cap`]).
    ///
    /// Upstream `GasPriceOracle::suggest_tip_cap` pushes `U256::ZERO` for every empty block
    /// and scans up to `2 * max_block_history` (2,048) blocks against a 1,024-entry LRU cache.
    /// On Morph L2, where empty blocks are frequent, that caused `eth_maxPriorityFeePerGas` to
    /// collapse to `0x0` (making `MORPH_DEFAULT_PRIORITY_FEE` dead code) and risked full LRU
    /// thrashing on quiet networks. Inspecting only the latest block keeps suggestions O(1),
    /// skips L1 message transactions, enforces the configured floor (defaulting to
    /// [`MORPH_DEFAULT_PRIORITY_FEE`]), and scales up by 10% above the median tip when a block
    /// reaches gas capacity.
    async fn suggested_priority_fee(&self) -> Result<U256, Self::Error>
    where
        Self: 'static,
    {
        let min_suggested_fee = self
            .gas_oracle()
            .config()
            .default_suggested_fee
            .unwrap_or_else(|| U256::from(MORPH_DEFAULT_PRIORITY_FEE));
        self.gas_oracle()
            .op_suggest_tip_cap(min_suggested_fee)
            .await
            .map_err(Self::Error::from_eth_err)
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

    async fn send_pool_transaction(
        &self,
        origin: reth_transaction_pool::TransactionOrigin,
        tx: reth_primitives_traits::WithEncoded<reth_transaction_pool::PoolTx<Self::Pool>>,
    ) -> Result<alloy_primitives::B256, Self::Error> {
        reth_rpc_eth_api::helpers::EthTransactions::send_pool_transaction(
            &self.inner.eth_api,
            origin,
            tx,
        )
        .await
        .map_err(Into::into)
    }

    async fn send_transaction(
        &self,
        origin: reth_transaction_pool::TransactionOrigin,
        tx: reth_primitives_traits::WithEncoded<
            reth_primitives_traits::Recovered<reth_transaction_pool::PoolPooledTx<Self::Pool>>,
        >,
    ) -> Result<alloy_primitives::B256, Self::Error> {
        reth_rpc_eth_api::helpers::EthTransactions::send_transaction(
            &self.inner.eth_api,
            origin,
            tx,
        )
        .await
        .map_err(Into::into)
    }

    fn send_raw_transaction_sync(
        &self,
        tx: alloy_primitives::Bytes,
        timeout_ms: Option<u64>,
    ) -> impl std::future::Future<
        Output = Result<reth_rpc_eth_api::RpcReceipt<Self::NetworkTypes>, Self::Error>,
    > + Send
    where
        Self: reth_rpc_eth_api::helpers::LoadReceipt,
    {
        let this = self.clone();
        let configured_timeout = self.send_raw_transaction_sync_timeout();
        let timeout_duration = timeout_ms
            .filter(|timeout_ms| *timeout_ms > 0)
            .map(Duration::from_millis)
            .map(|timeout| timeout.min(configured_timeout))
            .unwrap_or(configured_timeout);
        async move {
            use futures::StreamExt;
            use reth_chain_state::CanonStateSubscriptions;
            use reth_rpc_eth_types::block::convert_transaction_receipt;

            let mut stream = this.provider().canonical_state_stream();
            let hash = EthTransactions::send_raw_transaction(&this, tx).await?;

            // DB receipt fallback right after submission (covers fast inclusion or
            // already-committed notifications before the stream is polled).
            if let Some(receipt) = this.transaction_receipt(hash).await? {
                return Ok(receipt);
            }

            let receipt_res = tokio::time::timeout(timeout_duration, async {
                while let Some(notification) = stream.next().await {
                    let chain = notification.committed();
                    if let Some((block, tx, receipt, all_receipts)) =
                        chain.find_transaction_and_receipt_by_hash(hash)
                        && let Some(receipt) = convert_transaction_receipt(
                            block,
                            all_receipts,
                            tx,
                            receipt,
                            this.converter(),
                        )
                        .transpose()?
                    {
                        return Ok(Some(receipt));
                    }
                }
                Ok::<_, Self::Error>(None)
            })
            .await;

            match receipt_res {
                Ok(Ok(Some(receipt))) => return Ok(receipt),
                Ok(Err(err)) => return Err(err),
                Ok(Ok(None)) | Err(_) => {}
            }

            // Final DB receipt fallback before returning the EIP-7966 timeout error.
            if let Some(receipt) = this.transaction_receipt(hash).await? {
                return Ok(receipt);
            }

            Err(reth_rpc_eth_api::FromEthApiError::from_eth_err(
                EthApiError::TransactionConfirmationTimeout {
                    hash,
                    duration: timeout_duration,
                },
            ))
        }
    }
}

impl<N, Rpc> EthSubscriptions for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    Rpc: RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
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
    fn apply_pre_execution_changes(
        &self,
        _block: &reth_primitives_traits::RecoveredBlock<
            reth_provider::ProviderBlock<Self::Provider>,
        >,
        _db: &mut reth_rpc_eth_types::StateCacheDb,
    ) -> Result<reth_trie_common::bundle_state::BundleState, Self::Error> {
        // Morph has no Ethereum-style (EIP-4788) pre-block system calls to replay. The
        // upstream default would only run the Morph executor's pre-execution step,
        // which just pre-warms the L1 gas oracle account.
        Ok(Default::default())
    }
}

impl<N, Rpc> GetBlockAccessList for MorphEthApi<N, Rpc>
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
