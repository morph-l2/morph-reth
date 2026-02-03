//! Morph `eth_call` and `eth_estimateGas` overrides.

use crate::eth::api::{MorphEthApi, MorphNodeCore};
use crate::MorphEthApiError;
use alloy_primitives::U256;
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphTxExt, TokenFeeInfo};
use reth_evm::{EvmEnvFor, TxEnvFor};
use reth_provider::ChainSpecProvider;
use reth_rpc_eth_api::{
    helpers::{estimate::EstimateCall, Call, EthCall},
    EthApiTypes, RpcNodeCore,
};
use reth_rpc_eth_types::{error::FromEthApiError, EthApiError};
use revm::{context::Transaction as RevmTransaction, Database};

const ERR_INSUFFICIENT_FUNDS_FOR_L1_FEE: &str = "insufficient funds for l1 fee";
const ERR_INSUFFICIENT_FUNDS_FOR_TRANSFER: &str = "insufficient funds for transfer";
const ERR_INVALID_TOKEN: &str = "invalid token";

impl<N, Rpc> EthCall for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc: reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

impl<N, Rpc> Call for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc: reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
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

    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        evm_env: &EvmEnvFor<<MorphEthApi<N, Rpc> as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<MorphEthApi<N, Rpc> as RpcNodeCore>::Evm>,
    ) -> Result<u64, <Self as EthApiTypes>::Error> {
        let caller = tx_env.caller();
        let balance = db
            .basic(caller)
            .map_err(Into::<EthApiError>::into)
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)?
            .map(|acc| acc.balance)
            .unwrap_or_default();

        let value = tx_env.value();
        if value > balance {
            return Err(<MorphEthApiError as FromEthApiError>::from_eth_err(
                EthApiError::InvalidParams(ERR_INSUFFICIENT_FUNDS_FOR_TRANSFER.to_string()),
            ));
        }

        let (hardfork, l1_fee) = self
            .estimate_l1_fee(&mut db, evm_env, tx_env)
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)?;

        if let Some(fee_token_id) = tx_env.fee_token_id.filter(|id| *id > 0) {
            return self.caller_gas_allowance_with_token(
                &mut db,
                caller,
                balance,
                value,
                hardfork,
                l1_fee,
                fee_token_id,
                tx_env.fee_limit,
                tx_env.gas_price(),
            );
        }

        caller_gas_allowance_with_eth(balance, value, l1_fee, tx_env.gas_price())
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)
    }
}

impl<N, Rpc> MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc: reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    fn estimate_l1_fee<DB>(
        &self,
        db: &mut DB,
        evm_env: &EvmEnvFor<<MorphEthApi<N, Rpc> as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<MorphEthApi<N, Rpc> as RpcNodeCore>::Evm>,
    ) -> Result<(morph_chainspec::MorphHardfork, U256), EthApiError>
    where
        DB: Database,
        DB::Error: Into<EthApiError>,
    {
        let block_number = u64::try_from(evm_env.block_env.number)
            .map_err(|_| EthApiError::InvalidParams("invalid block number".to_string()))?;
        let timestamp = u64::try_from(evm_env.block_env.timestamp)
            .map_err(|_| EthApiError::InvalidParams("invalid block timestamp".to_string()))?;
        let chain_spec = self.provider().chain_spec();
        let hardfork = chain_spec.morph_hardfork_at(block_number, timestamp);

        if tx_env.is_l1_msg() {
            return Ok((hardfork, U256::ZERO));
        }

        let rlp_bytes = tx_env.rlp_bytes.as_ref().ok_or_else(|| {
            EthApiError::InvalidParams("missing rlp bytes for l1 fee".to_string())
        })?;

        let l1_info = L1BlockInfo::try_fetch(db, hardfork).map_err(|err| {
            EthApiError::InvalidParams(format!("failed to estimate L1 data fee: {err}"))
        })?;

        Ok((hardfork, l1_info.calculate_tx_l1_cost(rlp_bytes, hardfork)))
    }

    fn caller_gas_allowance_with_token<DB>(
        &self,
        db: &mut DB,
        caller: alloy_primitives::Address,
        balance: U256,
        value: U256,
        hardfork: morph_chainspec::MorphHardfork,
        l1_fee: U256,
        fee_token_id: u16,
        fee_limit: Option<U256>,
        gas_price: u128,
    ) -> Result<u64, MorphEthApiError>
    where
        DB: Database,
        DB::Error: Into<EthApiError>,
    {
        let token_fee_info = TokenFeeInfo::try_fetch(db, fee_token_id, caller, hardfork)
            .map_err(|_| EthApiError::InvalidParams(ERR_INVALID_TOKEN.to_string()))
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)?
            .ok_or_else(|| EthApiError::InvalidParams(ERR_INVALID_TOKEN.to_string()))
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)?;

        if !token_fee_info.is_active
            || token_fee_info.price_ratio.is_zero()
            || token_fee_info.scale.is_zero()
        {
            return Err(<MorphEthApiError as FromEthApiError>::from_eth_err(
                EthApiError::InvalidParams(ERR_INVALID_TOKEN.to_string()),
            ));
        }

        let limit = match fee_limit {
            Some(limit) if !limit.is_zero() => token_fee_info.balance.min(limit),
            _ => token_fee_info.balance,
        };

        let l1_fee_in_token = token_fee_info.calculate_token_amount(l1_fee);
        if l1_fee_in_token >= limit {
            return Err(<MorphEthApiError as FromEthApiError>::from_eth_err(
                EthApiError::InvalidParams(ERR_INSUFFICIENT_FUNDS_FOR_L1_FEE.to_string()),
            ));
        }

        let available_token = limit - l1_fee_in_token;
        let available_eth =
            token_amount_to_eth(available_token, &token_fee_info).ok_or_else(|| {
                EthApiError::InvalidParams(ERR_INVALID_TOKEN.to_string())
            })?;

        caller_gas_allowance_with_eth(balance, value, U256::ZERO, gas_price)
            .and_then(|allowance| {
                let allowance_eth = gas_allowance_from_balance(available_eth, gas_price);
                Ok(allowance.min(allowance_eth))
            })
            .map_err(<MorphEthApiError as FromEthApiError>::from_eth_err)
    }
}

impl<N, Rpc> EstimateCall for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc: reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
}

fn caller_gas_allowance_with_eth(
    balance: U256,
    value: U256,
    l1_fee: U256,
    gas_price: u128,
) -> Result<u64, EthApiError> {
    let mut available = balance.saturating_sub(value);
    if l1_fee >= available {
        return Err(EthApiError::InvalidParams(
            ERR_INSUFFICIENT_FUNDS_FOR_L1_FEE.to_string(),
        ));
    }
    available -= l1_fee;
    Ok(gas_allowance_from_balance(available, gas_price))
}

fn gas_allowance_from_balance(balance: U256, gas_price: u128) -> u64 {
    if gas_price == 0 {
        return u64::MAX;
    }
    let gas_price = U256::from(gas_price);
    let allowance = balance / gas_price;
    if allowance > U256::from(u64::MAX) {
        u64::MAX
    } else {
        allowance.to::<u64>()
    }
}

fn token_amount_to_eth(token_amount: U256, info: &TokenFeeInfo) -> Option<U256> {
    if info.price_ratio.is_zero() || info.scale.is_zero() {
        return None;
    }
    Some(token_amount.saturating_mul(info.price_ratio) / info.scale)
}
