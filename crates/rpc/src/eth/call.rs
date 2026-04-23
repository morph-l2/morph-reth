//! Morph `eth_call` and `eth_estimateGas` overrides.
//!
//! # Why we override [`Call::caller_gas_allowance`]
//!
//! reth v2.0.0's `estimate_gas_with` sets `cfg_env.disable_fee_charge = true`
//! for both `eth_call` and `eth_estimateGas` (see upstream `reth#18470`).
//! In revm this causes `calculate_caller_fee` to `return Ok(balance)`
//! immediately (see `revm-handler/src/pre_execution.rs`), skipping the
//! caller balance check and L1 fee deduction entirely. Relying on the EVM
//! handler to enforce balance is therefore a no-op on both RPC paths.
//!
//! go-ethereum's `DoEstimateGas` handles this by capping the binary-search
//! `hi` based on `balance − value − l1DataFee`. We mirror that here by
//! overriding `caller_gas_allowance`:
//!
//! * **ETH path** — delegate `(balance − value) / gas_price` to upstream
//!   [`alloy_evm::call::caller_gas_allowance`], then subtract the L1 fee
//!   expressed in gas units.
//! * **MorphTx token-fee path** — read the token balance from the L2 token
//!   registry and use it (scaled via `price_ratio`/`scale`) as the
//!   allowance base, matching go-ethereum's token branch in
//!   `DoEstimateGas`.
//!
//! # Scoping: estimateGas only
//!
//! `Call::caller_gas_allowance` is a shared hook — upstream also calls it
//! from `prepare_call_env` (`eth_call`) and `create_access_list_with`
//! (`eth_createAccessList`) when no explicit gas limit is provided.
//! morph-geth's `DoCall`, however, uses `ApplyMessage(..., Big0)` and
//! does **not** reject `eth_call` on L1-fee grounds. Adding our L1 fee
//! cap unconditionally would make `eth_call` over-reject.
//!
//! We key the Morph extension off `cfg_env.disable_block_gas_limit`,
//! which upstream sets at each call site:
//!
//! | call site                                     | `disable_block_gas_limit` |
//! |-----------------------------------------------|---------------------------|
//! | `EstimateCall::estimate_gas_with` (estimate)  | `false` (default)         |
//! | `Call::prepare_call_env` (`eth_call`)         | `true`                    |
//! | `EthCall::create_access_list_with`            | `true`                    |
//!
//! When the flag is `true` we fall through to upstream's default
//! `(balance − value) / gas_price` and skip the L1 fee cap, keeping
//! `eth_call` semantics aligned with `DoCall`. See the followup note
//! for the (different, symmetric) `eth_createAccessList` gap.

use crate::MorphEthApiError;
use crate::eth::{MorphEthApi, MorphNodeCore};
use alloy_evm::call::{CallError, caller_gas_allowance as upstream_caller_gas_allowance};
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

    /// Compute the upper bound on gas that the caller can afford.
    ///
    /// This overrides the upstream default, which only divides
    /// `(balance − value) / gas_price`, by additionally accounting for the
    /// Morph L1 data fee — matching go-ethereum's `DoEstimateGas` behaviour
    /// (`available.Sub(available, l1DataFee)` before dividing by `feeCap`).
    ///
    /// MorphTx transactions paying fees in ERC20 tokens (`fee_token_id > 0`)
    /// take a dedicated path that uses the token balance — scaled through
    /// the fee token's `price_ratio`/`scale` — as the allowance base.
    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        evm_env: &EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<Self as RpcNodeCore>::Evm>,
    ) -> Result<u64, <Self as EthApiTypes>::Error> {
        // When the flag is set, the caller is `prepare_call_env` (eth_call)
        // or `create_access_list_with` (eth_createAccessList). In both cases
        // morph-geth's corresponding path does not deduct the L1 fee for the
        // balance check, so we fall through to upstream's default
        // `(balance − value) / gas_price`. Only `estimate_gas_with` leaves
        // the flag at its default `false`, which is where Morph's
        // `DoEstimateGas` parity belongs.
        if evm_env.cfg_env.disable_block_gas_limit {
            return upstream_caller_gas_allowance(&mut db, tx_env).map_err(|e| match e {
                CallError::Database(db_err) => MorphEthApiError::Eth(db_err.into()),
                CallError::InsufficientFunds(_) => MorphEthApiError::InsufficientFundsForTransfer,
            });
        }

        let l1_fee = self.estimate_l1_fee(&mut db, evm_env, tx_env)?;

        if let Some(fee_token_id) = tx_env.fee_token_id.filter(|id| *id > 0) {
            return self.caller_gas_allowance_with_token(
                &mut db,
                tx_env.caller(),
                tx_env.value(),
                l1_fee,
                fee_token_id,
                tx_env.fee_limit,
                tx_env.gas_price(),
            );
        }

        // ETH path: reuse upstream's `(balance − value) / gas_price` and
        // then deduct the L1 data fee expressed in gas units.
        let base = upstream_caller_gas_allowance(&mut db, tx_env).map_err(|e| match e {
            CallError::Database(db_err) => MorphEthApiError::Eth(db_err.into()),
            CallError::InsufficientFunds(_) => MorphEthApiError::InsufficientFundsForTransfer,
        })?;

        let gas_price = tx_env.gas_price();
        if gas_price == 0 {
            // Zero gas price → allowance is unbounded by fee; L1 fee cannot cap.
            return Ok(base);
        }

        let l1_fee_gas = saturating_div_u128(l1_fee, gas_price);
        if l1_fee_gas >= base {
            return Err(MorphEthApiError::InsufficientFundsForL1Fee);
        }
        Ok(base - l1_fee_gas)
    }
}

impl<N, Rpc> MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    /// Estimates the L1 data fee for the given transaction.
    ///
    /// Returns zero for L1 message transactions since they don't pay L1 fees.
    /// Uses `tx_env.rlp_bytes` (populated by `MorphTransactionRequest::try_into_tx_env`)
    /// and the current `L1GasPriceOracle` state to compute the fee via
    /// `L1BlockInfo::calculate_tx_l1_cost`.
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
        if tx_env.is_l1_msg() {
            return Ok(U256::ZERO);
        }

        let block_number = u64::try_from(evm_env.block_env.inner.number)
            .map_err(|_| EthApiError::InvalidParams("invalid block number".to_string()))?;
        let timestamp = u64::try_from(evm_env.block_env.inner.timestamp)
            .map_err(|_| EthApiError::InvalidParams("invalid block timestamp".to_string()))?;
        let chain_spec = self.provider().chain_spec();
        let hardfork = chain_spec.morph_hardfork_at(block_number, timestamp);

        let rlp_bytes = tx_env.rlp_bytes.as_ref().ok_or_else(|| {
            EthApiError::InvalidParams("missing rlp bytes for l1 fee".to_string())
        })?;

        let l1_info = L1BlockInfo::try_fetch(db, hardfork).map_err(|err| {
            EthApiError::InvalidParams(format!("failed to estimate L1 data fee: {err}"))
        })?;

        Ok(l1_info.calculate_tx_l1_cost(rlp_bytes, hardfork))
    }

    /// Calculate caller's gas allowance when paying fees with an ERC20 token.
    ///
    /// Uses [`TokenFeeInfo::load_storage_only`] so it works on any
    /// `revm::Database` (no `Debug` bound), which is what the RPC
    /// `Call::caller_gas_allowance` trait hands us. When the token is
    /// registered but its `balance_slot` is unknown, we skip the token
    /// balance cap — the EVM handler re-validates the balance during
    /// the binary-search `executable(gas)` call via
    /// `validate_and_deduct_token_fee`.
    #[allow(clippy::too_many_arguments)]
    fn caller_gas_allowance_with_token<DB>(
        &self,
        db: &mut DB,
        caller: alloy_primitives::Address,
        value: U256,
        l1_fee: U256,
        fee_token_id: u16,
        fee_limit: Option<U256>,
        gas_price: u128,
    ) -> Result<u64, MorphEthApiError>
    where
        DB: revm::Database,
        DB::Error: Into<EthApiError>,
    {
        let token_fee_info = TokenFeeInfo::load_storage_only(db, fee_token_id, caller)
            .map_err(|_| MorphEthApiError::InvalidFeeToken)?
            .ok_or(MorphEthApiError::InvalidFeeToken)?;

        // Validate token is registered and active
        if !token_fee_info.is_active
            || token_fee_info.price_ratio.is_zero()
            || token_fee_info.scale.is_zero()
        {
            return Err(MorphEthApiError::InvalidFeeToken);
        }

        // Base ETH allowance enforces `value <= balance`; L1 fee is paid in
        // tokens on this path, so we don't deduct it from the ETH side.
        let eth_balance = db
            .basic(caller)
            .map_err(|e| MorphEthApiError::Eth(e.into()))?
            .map(|acc| acc.balance)
            .unwrap_or_default();
        let eth_available = eth_balance
            .checked_sub(value)
            .ok_or(MorphEthApiError::InsufficientFundsForTransfer)?;
        let eth_allowance = gas_allowance_from_balance(eth_available, gas_price);

        // If balance_slot is unknown, we cannot read the token balance via
        // storage alone. Skip the token balance cap and let the EVM handler
        // verify the balance during execution (see `validate_and_deduct_token_fee`).
        if token_fee_info.balance_slot.is_none() {
            tracing::debug!(
                target: "morph::rpc",
                token_id = fee_token_id,
                "Token balance_slot unknown, skipping token balance cap in caller_gas_allowance"
            );
            return Ok(eth_allowance);
        }

        // Calculate token-based gas allowance.
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

/// Saturating `U256 / u128` → `u64`.
///
/// Returns `0` when `divisor == 0` (the ETH path checks `gas_price == 0`
/// before calling this). Saturates to `u64::MAX` if the quotient overflows
/// a `u64`.
fn saturating_div_u128(dividend: U256, divisor: u128) -> u64 {
    if divisor == 0 {
        return 0;
    }
    let quotient = dividend / U256::from(divisor);
    if quotient > U256::from(u64::MAX) {
        u64::MAX
    } else {
        quotient.to::<u64>()
    }
}

/// Converts a balance to gas units based on the gas price.
///
/// Returns `u64::MAX` if gas price is zero (unlimited gas). Used by the
/// token-fee path to translate ETH-equivalent balances into a gas cap.
fn gas_allowance_from_balance(balance: U256, gas_price: u128) -> u64 {
    if gas_price == 0 {
        return u64::MAX;
    }
    saturating_div_u128(balance, gas_price)
}

/// Converts a token amount to ETH equivalent using the fee token info.
///
/// Uses the formula: `eth = token_amount * price_ratio / scale`.
/// Returns `None` if price_ratio or scale is zero.
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
