//! Morph `eth_call` and `eth_estimateGas` overrides.

use crate::MorphEthApiError;
use crate::error::ToMorphErr;
use crate::eth::{MorphEthApi, MorphNodeCore};
use alloy_primitives::U256;
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphTxExt, TokenFeeInfo};
use reth_evm::{EvmEnvFor, TxEnvFor};
use reth_provider::ChainSpecProvider;
use reth_rpc_eth_api::{
    EthApiTypes, RpcNodeCore,
    helpers::{Call, EthCall, estimate::EstimateCall},
};
use reth_rpc_eth_types::EthApiError;
use revm::{Database, context::Transaction as RevmTransaction};

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

    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        evm_env: &EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<Self as RpcNodeCore>::Evm>,
    ) -> Result<u64, <Self as EthApiTypes>::Error> {
        let caller = tx_env.caller();
        let balance = db
            .basic(caller)
            .to_morph_err()?
            .map(|acc| acc.balance)
            .unwrap_or_default();

        let value = tx_env.value();
        if value > balance {
            return Err(MorphEthApiError::InsufficientFundsForTransfer);
        }

        let l1_fee = self.estimate_l1_fee(&mut db, evm_env, tx_env)?;

        if let Some(fee_token_id) = tx_env.fee_token_id.filter(|id| *id > 0) {
            return self.caller_gas_allowance_with_token(
                &mut db,
                caller,
                balance,
                value,
                l1_fee,
                fee_token_id,
                tx_env.fee_limit,
                tx_env.gas_price(),
            );
        }

        caller_gas_allowance_with_eth(balance, value, l1_fee, tx_env.gas_price())
    }
}

impl<N, Rpc> MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    fn estimate_l1_fee<DB>(
        &self,
        db: &mut DB,
        evm_env: &EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<Self as RpcNodeCore>::Evm>,
    ) -> Result<U256, EthApiError>
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
            return Ok(U256::ZERO);
        }

        let rlp_bytes = tx_env.rlp_bytes.as_ref().ok_or_else(|| {
            EthApiError::InvalidParams("missing rlp bytes for l1 fee".to_string())
        })?;

        let l1_info = L1BlockInfo::try_fetch(db, hardfork).map_err(|err| {
            EthApiError::InvalidParams(format!("failed to estimate L1 data fee: {err}"))
        })?;

        Ok(l1_info.calculate_tx_l1_cost(rlp_bytes, hardfork))
    }

    /// Calculate caller's gas allowance when paying with ERC20 tokens.
    ///
    /// Uses storage-only reads. For tokens without a known `balance_slot`,
    /// skips the token balance limit (EVM handler will verify during execution).
    #[allow(clippy::too_many_arguments)]
    fn caller_gas_allowance_with_token<DB>(
        &self,
        db: &mut DB,
        caller: alloy_primitives::Address,
        balance: U256,
        value: U256,
        l1_fee: U256,
        fee_token_id: u16,
        fee_limit: Option<U256>,
        gas_price: u128,
    ) -> Result<u64, MorphEthApiError>
    where
        DB: Database,
        DB::Error: Into<EthApiError>,
    {
        let token_fee_info = TokenFeeInfo::fetch_storage_only(db, fee_token_id, caller)
            .map_err(|_| MorphEthApiError::InvalidFeeToken)?
            .ok_or(MorphEthApiError::InvalidFeeToken)?;

        // Validate token is registered and active
        if !token_fee_info.is_active
            || token_fee_info.price_ratio.is_zero()
            || token_fee_info.scale.is_zero()
        {
            return Err(MorphEthApiError::InvalidFeeToken);
        }

        // Calculate base ETH allowance (for tx.value transfer)
        let eth_allowance = caller_gas_allowance_with_eth(balance, value, U256::ZERO, gas_price)?;

        // If balance_slot is unknown, we cannot accurately read the token balance
        // via storage. Skip the token balance limit and let the EVM handler
        // (`validate_and_deduct_token_fee`) verify the actual balance.
        if token_fee_info.balance_slot.is_none() {
            tracing::debug!(
                target: "morph::rpc",
                token_id = fee_token_id,
                "Token balance_slot unknown, skipping token balance limit in caller_gas_allowance"
            );
            return Ok(eth_allowance);
        }

        // Calculate token-based gas allowance
        let limit = match fee_limit {
            Some(limit) if !limit.is_zero() => token_fee_info.balance.min(limit),
            _ => token_fee_info.balance,
        };

        let l1_fee_in_token = token_fee_info.eth_to_token_amount(l1_fee);
        if l1_fee_in_token >= limit {
            return Err(MorphEthApiError::InsufficientFundsForL1Fee);
        }

        let available_token = limit - l1_fee_in_token;
        let available_eth = token_amount_to_eth(available_token, &token_fee_info)
            .ok_or(MorphEthApiError::InvalidFeeToken)?;

        let token_allowance = gas_allowance_from_balance(available_eth, gas_price);
        Ok(eth_allowance.min(token_allowance))
    }
}

fn caller_gas_allowance_with_eth(
    balance: U256,
    value: U256,
    l1_fee: U256,
    gas_price: u128,
) -> Result<u64, MorphEthApiError> {
    let mut available = balance.saturating_sub(value);
    if l1_fee >= available {
        return Err(MorphEthApiError::InsufficientFundsForL1Fee);
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
    let (eth_amount, remainder) = token_amount
        .saturating_mul(info.price_ratio)
        .div_rem(info.scale);
    if remainder.is_zero() {
        Some(eth_amount)
    } else {
        Some(eth_amount.saturating_add(U256::from(1)))
    }
}
