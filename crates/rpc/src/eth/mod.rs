//! Morph `eth_` RPC wiring and conversions.

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
use reth_rpc_convert::{RpcConverter, RpcTypes};
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use std::marker::PhantomData;

pub mod api;
pub mod call;
pub mod receipt;
pub mod transaction;

pub use api::MorphEthApi;

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
    > + crate::eth::api::MorphNodeCore
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
