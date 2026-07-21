//! Morph `eth_call` / `eth_estimateGas` overrides.
//!
//! [`Call::caller_gas_allowance`] is overridden so `eth_estimateGas` caps
//! gas by `balance − value − l1_fee` (ETH path) or the fee token balance
//! (MorphTx `fee_token_id > 0`). `eth_call` and `eth_createAccessList`
//! are detected via `cfg_env.disable_block_gas_limit = true` and fall
//! through to the upstream allowance without the L1-fee extension.

use crate::MorphEthApiError;
use crate::eth::{MorphEthApi, MorphNodeCore};
use alloy_consensus::transaction::TxHashRef;
use alloy_evm::call::{CallError, caller_gas_allowance as upstream_caller_gas_allowance};
use alloy_primitives::{B256, U256};
use alloy_rpc_types_eth::BlockId;
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_revm::{
    L1BlockInfo, MorphTxExt, TokenFeeInfo, recoverable_sweep_trace_replay_scope,
    set_recoverable_sweep_trace_replay_target,
};
use reth_errors::ProviderError;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, TxEnvFor};
use reth_primitives_traits::Recovered;
use reth_provider::ChainSpecProvider;
use reth_revm::{
    database::StateProviderDatabase,
    db::{State, bal::EvmDatabaseError},
};
use reth_rpc_eth_api::{
    EthApiTypes, FromEvmError, RpcNodeCore,
    helpers::{Call, EthCall, LoadState, SpawnBlocking, estimate::EstimateCall},
};
use reth_rpc_eth_types::{EthApiError, StateCacheDb, cache::db::StateProviderTraitObjWrapper};
use reth_storage_api::ProviderTx;
use revm::{Database, DatabaseCommit, context::Transaction as RevmTransaction};
use std::future::Future;

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

    fn compute_state_root_for_eth_simulate(&self) -> bool {
        self.eth_api().compute_state_root_for_eth_simulate()
    }

    fn evm_memory_limit(&self) -> u64 {
        self.eth_api().evm_memory_limit()
    }

    fn spawn_with_state_at_block<F, R>(
        &self,
        at: impl Into<BlockId>,
        f: F,
    ) -> impl Future<Output = Result<R, <Self as EthApiTypes>::Error>> + Send
    where
        F: FnOnce(Self, StateCacheDb) -> Result<R, <Self as EthApiTypes>::Error> + Send + 'static,
        R: Send + 'static,
    {
        let at = at.into();
        self.spawn_blocking_io_fut(async move |this| {
            let state = this.state_at_block_id(at).await?;
            let db = State::builder()
                .with_database(StateProviderDatabase::new(StateProviderTraitObjWrapper(
                    state,
                )))
                .build();
            let _scope = recoverable_sweep_trace_replay_scope();
            f(this, db)
        })
    }

    fn replay_transactions_until<'a, DB, I>(
        &self,
        db: &mut DB,
        evm_env: EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        transactions: I,
        target_tx_hash: B256,
    ) -> Result<usize, <Self as EthApiTypes>::Error>
    where
        DB: Database<Error = EvmDatabaseError<ProviderError>> + DatabaseCommit + core::fmt::Debug,
        I: IntoIterator<Item = Recovered<&'a ProviderTx<Self::Provider>>>,
    {
        set_recoverable_sweep_trace_replay_target(target_tx_hash);
        let mut evm = self.evm_config().evm_with_env(db, evm_env);
        let mut index = 0;
        for transaction in transactions {
            if *transaction.tx_hash() == target_tx_hash {
                break;
            }

            let tx_env = self.evm_config().tx_env(transaction);
            evm.transact_commit(tx_env).map_err(
                <<Self as EthApiTypes>::Error as FromEvmError<
                    <Self as RpcNodeCore>::Evm,
                >>::from_evm_err,
            )?;
            index += 1;
        }
        Ok(index)
    }

    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        evm_env: &EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<Self as RpcNodeCore>::Evm>,
    ) -> Result<u64, <Self as EthApiTypes>::Error> {
        // eth_call / eth_createAccessList: no L1-fee cap. Token-fee callers
        // can have zero ETH, so defer to the handler instead of the
        // upstream ETH allowance.
        if evm_env.cfg_env.disable_block_gas_limit {
            if tx_env.fee_token_id.is_some_and(|id| id > 0) {
                return Ok(u64::MAX);
            }
            return upstream_caller_gas_allowance(&mut db, tx_env).map_err(|e| match e {
                CallError::Database(db_err) => MorphEthApiError::Eth(db_err.into()),
                CallError::InsufficientFunds(_) => MorphEthApiError::InsufficientFundsForTransfer,
            });
        }

        // eth_estimateGas path.
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

        // allowance = (balance − value − l1_fee) / gas_price, done at wei
        // precision so the remainder is not lost across the two
        // subtractions.
        let balance = db
            .basic(tx_env.caller())
            .map_err(|e| MorphEthApiError::Eth(e.into()))?
            .map(|acc| acc.balance)
            .unwrap_or_default();
        let available = balance
            .checked_sub(tx_env.value())
            .ok_or(MorphEthApiError::InsufficientFundsForTransfer)?;
        // Reject at `l1_fee >= available` so the RPC surfaces the real
        // reason rather than a downstream "gas required exceeds allowance 0".
        if l1_fee >= available {
            return Err(MorphEthApiError::InsufficientFundsForL1Fee);
        }
        Ok(gas_allowance_from_balance(
            available - l1_fee,
            tx_env.gas_price(),
        ))
    }
}

impl<N, Rpc> MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
{
    /// Estimate the L1 data fee. Zero for L1 messages.
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

    /// Allowance for MorphTx ERC20 fee-token callers: cap by token
    /// balance, not by ETH balance.
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

        // ETH only needs to cover `value`; gas + L1 fee are paid in tokens.
        let eth_balance = db
            .basic(caller)
            .map_err(|e| MorphEthApiError::Eth(e.into()))?
            .map(|acc| acc.balance)
            .unwrap_or_default();

        token_gas_allowance(
            eth_balance,
            value,
            &token_fee_info,
            l1_fee,
            fee_limit,
            gas_price,
            self.eth_api().gas_cap(),
        )
    }
}

/// Compute the gas allowance for a MorphTx fee-token caller.
///
/// Pure function over the inputs `caller_gas_allowance_with_token` would
/// load from the database, factored out so the affordability and bounding
/// logic can be unit-tested without an EVM/DB stack.
///
/// The `gas_cap` argument is the per-call RPC ceiling (`EthApiNodeBackend::gas_cap()`),
/// only consumed by the EVM-call-mode + no-`fee_limit` fallback to avoid
/// returning `u64::MAX`. See the in-body comment for the security rationale.
fn token_gas_allowance(
    eth_balance: U256,
    value: U256,
    token_fee_info: &TokenFeeInfo,
    l1_fee: U256,
    fee_limit: Option<U256>,
    gas_price: u128,
    gas_cap: u64,
) -> Result<u64, MorphEthApiError> {
    if !token_fee_info.is_active
        || token_fee_info.price_ratio.is_zero()
        || token_fee_info.scale.is_zero()
    {
        return Err(MorphEthApiError::InvalidFeeToken);
    }

    if eth_balance < value {
        return Err(MorphEthApiError::InsufficientFundsForTransfer);
    }

    // Determine the token-denominated affordability cap.
    //
    // - Slot mode (`balance_slot.is_some()`): RPC reads balance directly
    //   from token storage; cap by `min(balance, fee_limit)`. The
    //   trusted balance is the natural ceiling.
    // - EVM-call mode (`balance_slot.is_none()`): RPC cannot resolve the
    //   balance without spinning up an EVM (the handler does that at real
    //   execution via `load_for_caller`). On the estimateGas path
    //   `disable_fee_charge=true` short-circuits the handler's check, so
    //   there is no natural balance ceiling — we MUST enforce `gas_cap`
    //   here, matching `eth_call`'s effective ceiling. Trusting a
    //   user-supplied `fee_limit` alone would let `fee_limit = U256::MAX`
    //   bypass the operator-configured `--rpc.gascap`.
    let (limit, clamp_to_gas_cap) = match (token_fee_info.balance_slot, fee_limit) {
        (Some(_), Some(limit)) if !limit.is_zero() => (token_fee_info.balance.min(limit), false),
        (Some(_), _) => (token_fee_info.balance, false),
        (None, Some(limit)) if !limit.is_zero() => (limit, true),
        (None, _) => return Ok(gas_cap),
    };

    let allowance = gas_allowance_from_token_limit(limit, l1_fee, token_fee_info, gas_price)?;
    Ok(if clamp_to_gas_cap {
        allowance.min(gas_cap)
    } else {
        allowance
    })
}

/// Convert a token-denominated affordability cap into a gas allowance.
///
/// Subtracts the L1 fee (converted to tokens) from `limit_token`, then
/// converts the remaining token budget back to ETH and divides by
/// `gas_price`. Returns `InsufficientFundsForL1Fee` if the L1 fee swallows
/// the entire limit, matching the semantics surfaced for the ETH path.
fn gas_allowance_from_token_limit(
    limit_token: U256,
    l1_fee: U256,
    token_info: &TokenFeeInfo,
    gas_price: u128,
) -> Result<u64, MorphEthApiError> {
    let l1_fee_in_token = token_info.eth_to_token_amount(l1_fee);
    if l1_fee_in_token >= limit_token {
        return Err(MorphEthApiError::InsufficientFundsForL1Fee);
    }
    let available_token = limit_token - l1_fee_in_token;
    let available_eth = token_amount_to_eth(available_token, token_info)
        .ok_or(MorphEthApiError::InvalidFeeToken)?;
    Ok(gas_allowance_from_balance(available_eth, gas_price))
}

/// `U256 / u128 → u64`, saturating.
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

/// Balance → gas units. `gas_price == 0` → `u64::MAX`.
fn gas_allowance_from_balance(balance: U256, gas_price: u128) -> u64 {
    if gas_price == 0 {
        return u64::MAX;
    }
    saturating_div_u128(balance, gas_price)
}

/// `eth = floor(token_amount * price_ratio / scale)`. `None` if
/// `price_ratio` or `scale` is zero.
///
/// Floor (not ceil) is deliberate: this is the inverse of the
/// protocol's `eth_to_token_amount`, which ceils to protect the
/// protocol (undercharging loses revenue). An affordability budget
/// must round in the opposite direction — the largest `eth` such
/// that the corresponding token charge still fits the user's
/// balance. With non-1:1 ratios, ceiling here would over-promise a
/// gas budget the user cannot actually settle at execution time.
fn token_amount_to_eth(token_amount: U256, info: &TokenFeeInfo) -> Option<U256> {
    if info.price_ratio.is_zero() || info.scale.is_zero() {
        return None;
    }
    Some(token_amount.saturating_mul(info.price_ratio) / info.scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(balance − value − l1_fee) / gas_price`, wei precision.
    /// `balance=15, value=0, l1_fee=6, gas_price=10` → 0 (not 1 from
    /// floor/floor composition).
    #[test]
    fn gas_allowance_subtracts_l1_fee_at_wei_precision() {
        let value = U256::ZERO;
        let l1_fee = U256::from(6u64);
        let gas_price = 10u128;

        let available = U256::from(15u64) - value - l1_fee;
        assert_eq!(gas_allowance_from_balance(available, gas_price), 0);

        let available = U256::from(16u64) - value - l1_fee;
        assert_eq!(gas_allowance_from_balance(available, gas_price), 1);
    }

    #[test]
    fn saturating_div_u128_handles_zero_divisor_and_overflow() {
        assert_eq!(saturating_div_u128(U256::from(100u64), 0), 0);
        assert_eq!(saturating_div_u128(U256::MAX, 1), u64::MAX);
        assert_eq!(saturating_div_u128(U256::from(9u64), 10), 0);
        assert_eq!(saturating_div_u128(U256::from(10u64), 10), 1);
    }

    #[test]
    fn gas_allowance_with_zero_gas_price_returns_u64_max() {
        assert_eq!(
            gas_allowance_from_balance(U256::from(1_000u64), 0),
            u64::MAX
        );
    }

    fn eth_path_allowance(
        balance: U256,
        value: U256,
        l1_fee: U256,
        gas_price: u128,
    ) -> Result<u64, MorphEthApiError> {
        let available = balance
            .checked_sub(value)
            .ok_or(MorphEthApiError::InsufficientFundsForTransfer)?;
        if l1_fee >= available {
            return Err(MorphEthApiError::InsufficientFundsForL1Fee);
        }
        Ok(gas_allowance_from_balance(available - l1_fee, gas_price))
    }

    #[test]
    fn eth_path_l1_fee_equal_to_available_errors_early() {
        let err = eth_path_allowance(U256::from(10u64), U256::ZERO, U256::from(10u64), 1)
            .expect_err("l1_fee == available must reject");
        assert!(matches!(err, MorphEthApiError::InsufficientFundsForL1Fee));
    }

    #[test]
    fn eth_path_one_wei_above_l1_fee_yields_positive_allowance() {
        let allowance = eth_path_allowance(U256::from(11u64), U256::ZERO, U256::from(10u64), 1)
            .expect("l1_fee < available must succeed");
        assert_eq!(allowance, 1);
    }

    #[test]
    fn eth_path_value_exceeds_balance_errors_before_l1_fee_check() {
        let err = eth_path_allowance(U256::from(5u64), U256::from(10u64), U256::ZERO, 1)
            .expect_err("value > balance must reject");
        assert!(matches!(
            err,
            MorphEthApiError::InsufficientFundsForTransfer
        ));
    }

    /// Build an active 1:1 token (`scale == price_ratio == 1`) so token math
    /// is identity and the test focuses on the limit-selection branches.
    fn token_1to1(balance_slot: Option<U256>, balance: U256) -> TokenFeeInfo {
        TokenFeeInfo {
            token_address: alloy_primitives::Address::ZERO,
            is_active: true,
            decimals: 18,
            price_ratio: U256::from(1u64),
            scale: U256::from(1u64),
            caller: alloy_primitives::Address::ZERO,
            balance,
            balance_slot,
        }
    }

    /// EVM-call mode with a user-supplied `fee_limit` that covers the L1 fee
    /// must return the remaining-budget gas, never `u64::MAX`.
    #[test]
    fn token_evm_call_mode_with_fee_limit_uses_user_budget() {
        let token = token_1to1(None, U256::ZERO);
        let allowance = token_gas_allowance(
            /* eth_balance */ U256::from(1u64),
            /* value */ U256::ZERO,
            &token,
            /* l1_fee */ U256::from(10u64),
            /* fee_limit */ Some(U256::from(110u64)),
            /* gas_price */ 1,
            /* gas_cap */ 50_000_000,
        )
        .expect("fee_limit covers L1 fee with 100 wei to spare");
        // (110 - 10) / 1 = 100 gas
        assert_eq!(allowance, 100);
    }

    /// EVM-call mode + `fee_limit` ≤ L1 fee: surface `InsufficientFundsForL1Fee`
    /// rather than silently capping at zero.
    #[test]
    fn token_evm_call_mode_l1_fee_swallows_limit_returns_clear_error() {
        let token = token_1to1(None, U256::ZERO);
        let err = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            /* l1_fee */ U256::from(100u64),
            /* fee_limit */ Some(U256::from(50u64)),
            1,
            50_000_000,
        )
        .expect_err("fee_limit cannot cover L1 fee");
        assert!(matches!(err, MorphEthApiError::InsufficientFundsForL1Fee));
    }

    /// EVM-call mode without a `fee_limit` must cap at the per-call `gas_cap`,
    /// never `u64::MAX` (which lets estimateGas binary-search 25× the
    /// block_gas_limit of free EVM work).
    #[test]
    fn token_evm_call_mode_without_fee_limit_falls_back_to_gas_cap() {
        let token = token_1to1(None, U256::ZERO);
        let cap = 5_000_000u64;
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            /* l1_fee */ U256::from(123u64),
            /* fee_limit */ None,
            1,
            cap,
        )
        .expect("no fee_limit must fall back to gas_cap, not u64::MAX");
        assert_eq!(allowance, cap, "must equal gas_cap, not u64::MAX");

        // Same with explicit Some(0), which the production code treats as
        // "no usable cap" via the `if !limit.is_zero()` guard.
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::from(123u64),
            Some(U256::ZERO),
            1,
            cap,
        )
        .expect("Some(0) fee_limit must fall back to gas_cap");
        assert_eq!(allowance, cap);
    }

    /// Slot-mode regression: `min(balance, fee_limit)` budget; gas_cap unused.
    #[test]
    fn token_slot_mode_caps_by_min_of_balance_and_fee_limit() {
        let token = token_1to1(
            Some(U256::from(42u64)),
            /* balance */ U256::from(1_000u64),
        );

        // fee_limit < balance → fee_limit binds.
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            /* l1_fee */ U256::from(50u64),
            /* fee_limit */ Some(U256::from(500u64)),
            /* gas_price */ 1,
            /* gas_cap (must NOT leak in) */ 9_999,
        )
        .expect("balance covers L1 fee");
        // (500 - 50) / 1 = 450
        assert_eq!(allowance, 450);

        // fee_limit > balance → balance binds.
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::from(50u64),
            Some(U256::from(10_000u64)),
            1,
            9_999,
        )
        .expect("balance covers L1 fee");
        // (1000 - 50) / 1 = 950
        assert_eq!(allowance, 950);

        // No fee_limit → balance binds.
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::from(50u64),
            None,
            1,
            9_999,
        )
        .expect("balance covers L1 fee");
        assert_eq!(allowance, 950);
    }

    /// Inactive / misconfigured token returns InvalidFeeToken before any
    /// limit math runs (regardless of mode).
    #[test]
    fn token_inactive_or_misconfigured_returns_invalid_fee_token() {
        let mut token = token_1to1(None, U256::ZERO);
        token.is_active = false;
        let err = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::ZERO,
            Some(U256::from(1_000u64)),
            1,
            50_000_000,
        )
        .expect_err("inactive token must reject");
        assert!(matches!(err, MorphEthApiError::InvalidFeeToken));

        let mut token = token_1to1(Some(U256::from(1u64)), U256::from(1_000u64));
        token.price_ratio = U256::ZERO;
        let err = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::ZERO,
            None,
            1,
            50_000_000,
        )
        .expect_err("zero price_ratio must reject");
        assert!(matches!(err, MorphEthApiError::InvalidFeeToken));
    }

    /// EVM-call mode with an absurd user-supplied `fee_limit` (e.g.
    /// `U256::MAX`) must clamp to `gas_cap`, never bypass the
    /// operator-configured `--rpc.gascap < block_gas_limit`.
    #[test]
    fn token_evm_call_mode_huge_fee_limit_clamps_to_gas_cap() {
        let token = token_1to1(None, U256::ZERO);
        let cap = 5_000_000u64;
        let allowance = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            /* l1_fee */ U256::ZERO,
            /* fee_limit */ Some(U256::MAX),
            /* gas_price */ 1,
            /* gas_cap */ cap,
        )
        .expect("huge fee_limit must clamp, not error");
        assert_eq!(allowance, cap, "must clamp to gas_cap, not u64::MAX");
    }

    /// `token_amount_to_eth` must floor, not ceil. Inverse of
    /// `eth_to_token_amount` (which ceils for protocol safety): an
    /// affordability budget must round toward the user so the returned
    /// wei budget is settleable at execution time.
    #[test]
    fn token_amount_to_eth_floors_non_unit_ratio() {
        // scale=10, price_ratio=3. eth_to_token_amount(1 wei) = ceil(10/3) = 4
        // tokens. So 1 token cannot afford even 1 wei of gas — budget = 0.
        let info = TokenFeeInfo {
            price_ratio: U256::from(3u64),
            scale: U256::from(10u64),
            ..token_1to1(None, U256::ZERO)
        };
        assert_eq!(
            token_amount_to_eth(U256::from(1u64), &info),
            Some(U256::ZERO)
        );
        assert_eq!(
            token_amount_to_eth(U256::from(3u64), &info),
            Some(U256::ZERO)
        );
        // 4 tokens → floor(12/10) = 1 wei. Roundtrip check:
        // eth_to_token_amount(1) = ceil(10/3) = 4, so 4 tokens → 1 wei is
        // exactly settleable.
        assert_eq!(
            token_amount_to_eth(U256::from(4u64), &info),
            Some(U256::from(1u64))
        );
        // Exact multiple: 10 tokens → 3 wei (floor and ceil agree).
        assert_eq!(
            token_amount_to_eth(U256::from(10u64), &info),
            Some(U256::from(3u64))
        );
    }
}
