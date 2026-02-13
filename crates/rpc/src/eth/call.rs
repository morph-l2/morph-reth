//! Morph `eth_call` and `eth_estimateGas` overrides.

use crate::MorphEthApiError;
use crate::eth::{MorphEthApi, MorphNodeCore};
use morph_chainspec::MorphChainSpec;
use reth_provider::ChainSpecProvider;
use reth_rpc_eth_api::helpers::{Call, EthCall, estimate::EstimateCall};
use reth_rpc_eth_types::EthApiError;

impl<N, Rpc> EthCall for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> EstimateCall for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> Call for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
    fn call_gas_limit(&self) -> u64 {
        self.eth_api().gas_cap()
    }

    fn max_simulate_blocks(&self) -> u64 {
        self.eth_api().max_simulate_blocks()
    }

    fn evm_memory_limit(&self) -> u64 {
        self.eth_api().evm_memory_limit()
    }
}
