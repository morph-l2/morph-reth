//! Morph `eth_call` / `eth_estimateGas` overrides.
//!
//! - [`EstimateCall::estimate_gas_with`] is overridden to relax
//!   `cfg_env.tx_gas_limit_cap` (which [`morph_evm::MorphEvmConfig::evm_env`]
//!   seeds to `header.gas_limit()` to neutralize Osaka EIP-7825) so
//!   `blockOverrides.gasLimit` values above the header `gas_limit` take effect,
//!   and to clamp `max_gas_limit` by `call_gas_limit()` (`--rpc.gascap`, with
//!   `0` remaining uncapped) so zero-priced, ETH-paid, token-paid, explicit
//!   `gas`, and diagnostic-retry estimations all respect `--rpc.gascap` (aligning
//!   with `morph-geth` `DoEstimateGas` and upstream `reth`).
//! - [`Call::caller_gas_allowance`] is overridden so `eth_estimateGas` caps
//!   gas by `balance − value − l1_fee` (ETH path) or a token-denominated limit
//!   (MorphTx `fee_token_id > 0`, see `token_gas_allowance`), clamped to
//!   `call_gas_limit()` when non-zero. `eth_call` and sibling call/trace RPCs
//!   (`eth_callMany`, `eth_createAccessList`, `debug_traceCall*`,
//!   `trace_call*`) are detected via `cfg_env.disable_block_gas_limit = true`
//!   and skip the L1-fee extension: fee-token callers start from `u64::MAX`
//!   (since they may hold zero ETH) while ETH callers delegate to the upstream
//!   balance allowance, and both are clamped to `call_gas_limit()` when
//!   non-zero (backporting `paradigmxyz/reth#27586` so `--rpc.gascap` holds
//!   even under `blockOverrides.gasLimit`).

use crate::MorphEthApiError;
use crate::eth::{MorphEthApi, MorphNodeCore};
use alloy_evm::{
    call::{CallError, caller_gas_allowance as upstream_caller_gas_allowance},
    overrides::{apply_block_overrides, apply_state_overrides},
};
use alloy_network::TransactionBuilder;
use alloy_primitives::{TxKind, U256};
use alloy_rpc_types_eth::state::EvmOverrides;
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphTxExt, TokenFeeInfo};
use reth_chainspec::MIN_TRANSACTION_GAS;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor, TransactionEnvMut, TxEnvFor, env::BlockEnvironment};
use reth_provider::ChainSpecProvider;
use reth_revm::{
    database::{EvmStateProvider, StateProviderDatabase},
    db::State,
};
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    AsEthApiError, EthApiTypes, FromEthApiError, IntoEthApiError, RpcNodeCore,
    helpers::{
        Call, EthCall,
        estimate::{EstimateCall, update_estimated_gas_range},
    },
};
use reth_rpc_eth_types::{
    EthApiError, RpcInvalidTransactionError,
    error::{
        FromEvmError,
        api::{FromEvmHalt, FromRevert},
    },
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use revm::{
    Database,
    context::{Block, Transaction as RevmTransaction},
    context_interface::{Cfg, result::ExecutionResult},
    primitives::KECCAK_EMPTY,
};
use tracing::trace;

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
    fn estimate_gas_with<S>(
        &self,
        mut evm_env: EvmEnvFor<Self::Evm>,
        mut request: RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>,
        state: S,
        overrides: EvmOverrides,
    ) -> Result<U256, Self::Error>
    where
        S: EvmStateProvider,
    {
        // Disabled because eth_estimateGas is sometimes used with eoa senders.
        // See <https://github.com/paradigmxyz/reth/issues/1959>
        evm_env.cfg_env.disable_eip3607 = true;

        // The basefee should be ignored for eth_estimateGas and similar.
        // See <https://github.com/ethereum/go-ethereum/blob/ee8e83fa5f6cb261dad2ed0a7bbcde4930c41e6c/internal/ethapi/api.go#L985>
        evm_env.cfg_env.disable_base_fee = true;

        // Disable additional fee charges (e.g. L2 operator / L1 data fees) for gas estimation,
        // consistent with `prepare_call_env` for `eth_call`.
        evm_env.cfg_env.disable_fee_charge = true;

        // Morph does not enforce EIP-7825's 2^24 transaction gas cap. `MorphEvmConfig::evm_env`
        // seeds `tx_gas_limit_cap = Some(header.gas_limit())` to neutralize Osaka's default cap
        // during block execution; relax it to `u64::MAX` here (matching `prepare_call_env` and
        // `simulate_v1`) so a higher `blockOverrides.gasLimit` is not clamped back to the
        // pre-override header `gas_limit`.
        relax_estimate_tx_gas_limit_cap(&mut evm_env);

        // Set nonce to None so that the correct nonce is chosen by the EVM.
        request.as_mut().take_nonce();

        let tx_request_gas_limit = request.as_ref().gas_limit();

        let mut db = State::builder()
            .with_database(StateProviderDatabase::new(state))
            .build();

        // Apply any block overrides before deriving block-derived limits and the tx env so
        // overrides for `gasLimit`, `baseFee` and `blobBaseFee` are visible to estimation.
        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, &mut db, evm_env.block_env.inner_mut());
        }

        // Apply any state overrides if specified.
        if let Some(state_override) = overrides.state {
            apply_state_overrides(state_override, &mut db).map_err(Self::Error::from_eth_err)?;
        }

        // Derive the effective ceiling from the (possibly overridden) block gas limit and clamp
        // by `--rpc.gascap` (`0` means unlimited), matching `morph-geth` `DoEstimateGas` and
        // upstream `reth` so zero-priced, ETH-paid, token-paid, explicit-`gas`, and diagnostic
        // retry executions all respect `call_gas_limit()`.
        let max_gas_limit = resolve_estimate_max_gas_limit(&evm_env, self.call_gas_limit());

        // Determine the highest possible gas limit, considering both the request's specified
        // limit and the capped block limit.
        let mut highest_gas_limit = tx_request_gas_limit
            .unwrap_or(max_gas_limit)
            .min(max_gas_limit);

        // Pre-seed the request's gas limit to the capped ceiling before constructing `tx_env` so
        // `MorphTransactionRequest::try_into_tx_env` encodes the capped gas limit (rather than an
        // uncapped `blockOverrides.gasLimit` or oversized explicit `gas`) into `tx_env.rlp_bytes`
        // for L1 data-fee estimation, matching `morph-geth` `DoEstimateGas` / `ToMessage(gasCap)`.
        request.as_mut().set_gas_limit(highest_gas_limit);

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        // Check if this is a basic transfer (no input data to account with no code).
        // Query through `db` so `stateOverrides` applied above are visible.
        let is_basic_transfer = if tx_env.input().is_empty()
            && let TxKind::Call(to) = tx_env.kind()
        {
            match db.basic(to) {
                Ok(Some(account)) => {
                    account.code_hash.is_zero() || account.code_hash == KECCAK_EMPTY
                }
                Ok(None) => true,
                Err(_) => false,
            }
        } else {
            false
        };

        // Check funds of the sender (only useful to check if transaction gas price is more than 0).
        if tx_env.gas_price() > 0 {
            highest_gas_limit =
                highest_gas_limit.min(self.caller_gas_allowance(&mut db, &evm_env, &tx_env)?);
        }

        // Cap the transaction gas limit to the computed ceiling.
        tx_env.set_gas_limit(tx_env.gas_limit().min(highest_gas_limit));

        // Track whether the request was constrained below `max_gas_limit` (by an explicit `gas`
        // parameter or caller balance allowance) so OOG-like reverts can be disambiguated via a
        // retry at `max_gas_limit` without re-running unconstrained requests twice.
        let gas_constrained = tx_env.gas_limit() < max_gas_limit;

        let mut evm = self.evm_config().evm_with_env(&mut db, evm_env);

        // Only short-circuit basic transfers when the capped gas budget covers
        // `MIN_TRANSACTION_GAS` (21,000); otherwise fall through so sub-21k caps (e.g.
        // `--rpc.gascap < 21_000`, explicit `gas < 21_000`, or low balance allowance)
        // reject with `GasRequiredExceedsAllowance`.
        if is_basic_transfer && highest_gas_limit >= MIN_TRANSACTION_GAS {
            let mut min_tx_env = tx_env.clone();
            min_tx_env.set_gas_limit(MIN_TRANSACTION_GAS);

            if let Ok(res) = evm
                .transact(min_tx_env)
                .map_err(<Self::Error as FromEvmError<Self::Evm>>::from_evm_err)
                && res.result.is_success()
            {
                return Ok(U256::from(MIN_TRANSACTION_GAS));
            }
        }

        trace!(
            target: "rpc::eth::estimate",
            ?tx_env,
            gas_limit = tx_env.gas_limit(),
            is_basic_transfer,
            "Starting gas estimation"
        );

        // Execute the transaction with the highest possible gas limit.
        let mut res = match evm
            .transact(tx_env.clone())
            .map_err(<Self::Error as FromEvmError<Self::Evm>>::from_evm_err)
        {
            Err(err) if err.is_gas_too_high() && gas_constrained => {
                return Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit);
            }
            Err(err) if err.is_gas_too_low() => {
                return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                    gas_limit: tx_env.gas_limit(),
                }
                .into_eth_err());
            }
            ethres => ethres?,
        };

        let gas_refund = match res.result {
            ExecutionResult::Success { gas, .. } => gas.final_refunded(),
            ExecutionResult::Halt { reason, .. } => {
                return Err(Self::Error::from_evm_halt(reason, tx_env.gas_limit()));
            }
            ExecutionResult::Revert { output, .. } => {
                return if gas_constrained {
                    Self::map_out_of_gas_err(&mut evm, tx_env, max_gas_limit)
                } else {
                    Err(Self::Error::from_revert(output))
                };
            }
        };

        // We know the tx succeeded with the configured gas limit, so use that as the upper bound
        // for binary search.
        highest_gas_limit = tx_env.gas_limit();

        let mut gas_used = res.result.tx_gas_used();
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        // Optimistic probe using `(gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63`.
        let optimistic_gas_limit = compute_optimistic_gas_limit(gas_used, gas_refund);
        if optimistic_gas_limit < highest_gas_limit {
            let mut optimistic_tx_env = tx_env.clone();
            optimistic_tx_env.set_gas_limit(optimistic_gas_limit);

            res = evm
                .transact(optimistic_tx_env)
                .map_err(<Self::Error as FromEvmError<Self::Evm>>::from_evm_err)?;
            if res.result.is_success() {
                gas_used = res.result.tx_gas_used();
            }
            update_estimated_gas_range(
                res.result,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        }

        let mut mid_gas_limit =
            initial_binary_search_mid(gas_used, lowest_gas_limit, highest_gas_limit);

        trace!(
            target: "rpc::eth::estimate",
            ?highest_gas_limit,
            ?lowest_gas_limit,
            ?mid_gas_limit,
            "Starting binary search for gas"
        );

        while lowest_gas_limit.saturating_add(1) < highest_gas_limit {
            let ratio = (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64);
            if ratio < ESTIMATE_GAS_ERROR_RATIO {
                break;
            }

            let mut mid_tx_env = tx_env.clone();
            mid_tx_env.set_gas_limit(mid_gas_limit);

            match evm
                .transact(mid_tx_env)
                .map_err(<Self::Error as FromEvmError<Self::Evm>>::from_evm_err)
            {
                Err(err) if err.is_gas_too_high() => {
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    lowest_gas_limit = mid_gas_limit;
                }
                ethres => {
                    res = ethres?;
                    update_estimated_gas_range(
                        res.result,
                        mid_gas_limit,
                        &mut highest_gas_limit,
                        &mut lowest_gas_limit,
                    )?;
                }
            }

            mid_gas_limit = ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64;
        }

        Ok(U256::from(highest_gas_limit))
    }
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

    fn caller_gas_allowance(
        &self,
        mut db: impl Database<Error: Into<EthApiError>>,
        evm_env: &EvmEnvFor<<Self as RpcNodeCore>::Evm>,
        tx_env: &TxEnvFor<<Self as RpcNodeCore>::Evm>,
    ) -> Result<u64, <Self as EthApiTypes>::Error> {
        // `eth_call` and sibling call/trace RPCs (`eth_callMany`,
        // `eth_createAccessList`, `debug_traceCall*`, `trace_call*`) reach
        // here via `prepare_call_env`, which sets
        // `cfg_env.disable_block_gas_limit = true`: no L1-fee cap.
        //
        // Token-fee callers can legitimately hold zero ETH, so start from
        // `u64::MAX` instead of the ETH balance cap. In both modes, clamp to
        // `call_gas_limit()` when non-zero (`0` means uncapped) so
        // `prepare_call_env`'s `cap.min(block_env.gas_limit())` cannot exceed
        // `--rpc.gascap` via `blockOverrides.gasLimit` or a block gas limit
        // above `--rpc.gascap` (backporting `paradigmxyz/reth#27586`).
        if evm_env.cfg_env.disable_block_gas_limit {
            let allowance = if tx_env.fee_token_id.is_some_and(|id| id > 0) {
                u64::MAX
            } else {
                upstream_caller_gas_allowance(&mut db, tx_env).map_err(|e| match e {
                    CallError::Database(db_err) => MorphEthApiError::Eth(db_err.into()),
                    CallError::InsufficientFunds(_) => {
                        MorphEthApiError::InsufficientFundsForTransfer
                    }
                })?
            };
            return Ok(cap_call_gas_allowance(allowance, self.call_gas_limit()));
        }

        // eth_estimateGas path.
        let l1_fee = self.estimate_l1_fee(&mut db, evm_env, tx_env)?;

        let allowance = if let Some(fee_token_id) = tx_env.fee_token_id.filter(|id| *id > 0) {
            self.caller_gas_allowance_with_token(
                &mut db,
                tx_env.caller(),
                tx_env.value(),
                l1_fee,
                fee_token_id,
                tx_env.fee_limit,
                tx_env.gas_price(),
            )?
        } else {
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
            gas_allowance_from_balance(available - l1_fee, tx_env.gas_price())
        };

        Ok(cap_call_gas_allowance(allowance, self.call_gas_limit()))
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

/// Relaxes `cfg_env.tx_gas_limit_cap` for `eth_estimateGas` (mirroring `prepare_call_env` and
/// `simulate_v1`) so [`morph_evm::MorphEvmConfig::evm_env`]'s `Some(header.gas_limit())` does not
/// clamp `blockOverrides.gasLimit` values that exceed the header's gas limit.
fn relax_estimate_tx_gas_limit_cap<Spec, BlockEnv>(evm_env: &mut reth_evm::EvmEnv<Spec, BlockEnv>)
where
    Spec: Clone + Into<revm::primitives::hardfork::SpecId>,
{
    if !evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
        evm_env.cfg_env.tx_gas_limit_cap = Some(u64::MAX);
    }
}

/// Computes the `max_gas_limit` ceiling for `eth_estimateGas` after `blockOverrides` have been
/// applied to `evm_env`, clamping by `gas_cap` (`--rpc.gascap`, where `0` means unlimited).
fn resolve_estimate_max_gas_limit<Spec, B>(evm_env: &reth_evm::EvmEnv<Spec, B>, gas_cap: u64) -> u64
where
    Spec: Clone + Into<revm::primitives::hardfork::SpecId>,
    B: Block,
{
    let block_gas_limit = evm_env.block_env.gas_limit();
    let max_gas_limit = if evm_env.cfg_env.is_amsterdam_eip8037_enabled() {
        block_gas_limit
    } else {
        evm_env.cfg_env.tx_gas_limit_cap().min(block_gas_limit)
    };
    cap_call_gas_allowance(max_gas_limit, gas_cap)
}

/// Computes the optimistic 63/64 gas limit probe `(gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63`
/// in `u128` before saturating to `u64::MAX`, avoiding premature `u64` saturation prior to `/ 63`.
fn compute_optimistic_gas_limit(gas_used: u64, gas_refund: u64) -> u64 {
    let scaled =
        (u128::from(gas_used) + u128::from(gas_refund) + u128::from(CALL_STIPEND_GAS)) * 64 / 63;
    scaled.min(u128::from(u64::MAX)) as u64
}

/// Computes the initial probe point for binary search over `(lowest_gas_limit, highest_gas_limit]`,
/// using `min(gas_used * 3, midpoint)` only when `gas_used * 3 > lowest_gas_limit` so a failed
/// optimistic probe or large gas refund can never place `mid_gas_limit <= lowest_gas_limit`.
fn initial_binary_search_mid(gas_used: u64, lowest_gas_limit: u64, highest_gas_limit: u64) -> u64 {
    let midpoint = ((u128::from(highest_gas_limit) + u128::from(lowest_gas_limit)) / 2) as u64;
    let triple_used = gas_used.saturating_mul(3);
    if triple_used > lowest_gas_limit {
        std::cmp::min(triple_used, midpoint)
    } else {
        midpoint
    }
}

/// Cap a call or estimation gas ceiling / allowance by the configured RPC gas cap (`--rpc.gascap`).
///
/// Matches upstream `reth` (`paradigmxyz/reth#27586`) and `morph-geth` (`DoEstimateGas`)
/// conventions where `gas_cap == 0` disables the cap (`--rpc.gascap max` maps to `u64::MAX`,
/// which is likewise a no-op under `.min(...)`).
fn cap_call_gas_allowance(allowance: u64, gas_cap: u64) -> u64 {
    if gas_cap == 0 {
        allowance
    } else {
        allowance.min(gas_cap)
    }
}

/// Compute the gas allowance for a MorphTx fee-token caller.
///
/// Pure function over the inputs `caller_gas_allowance_with_token` would
/// load from the database, factored out so the affordability and bounding
/// logic can be unit-tested without an EVM/DB stack.
///
/// The `gas_cap` argument is the per-call RPC ceiling (`EthApiNodeBackend::gas_cap()`,
/// with `0` meaning uncapped), only consumed in EVM-call mode: as the no-`fee_limit`
/// fallback and as the clamp on a user-supplied `fee_limit`. See the in-body comment
/// for the security rationale.
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
    //   execution via `load_token_fee_info`). On the estimateGas path
    //   `disable_fee_charge=true` short-circuits the handler's check, so
    //   there is no natural balance ceiling — we MUST enforce `gas_cap`
    //   here, matching `eth_call`'s effective ceiling. Trusting a
    //   user-supplied `fee_limit` alone would let `fee_limit = U256::MAX`
    //   bypass the operator-configured `--rpc.gascap`.
    let (limit, clamp_to_gas_cap) = match (token_fee_info.balance_slot, fee_limit) {
        (Some(_), Some(limit)) if !limit.is_zero() => (token_fee_info.balance.min(limit), false),
        (Some(_), _) => (token_fee_info.balance, false),
        (None, Some(limit)) if !limit.is_zero() => (limit, true),
        (None, _) => return Ok(cap_call_gas_allowance(u64::MAX, gas_cap)),
    };

    let allowance = gas_allowance_from_token_limit(limit, l1_fee, token_fee_info, gas_price)?;
    Ok(if clamp_to_gas_cap {
        cap_call_gas_allowance(allowance, gas_cap)
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

    #[test]
    fn cap_call_gas_allowance_clamps_to_nonzero_gas_cap() {
        let gas_cap = 50_000_000u64;
        assert_eq!(cap_call_gas_allowance(u64::MAX, gas_cap), gas_cap);
        assert_eq!(cap_call_gas_allowance(100_000_000, gas_cap), gas_cap);
        assert_eq!(cap_call_gas_allowance(gas_cap, gas_cap), gas_cap);
    }

    #[test]
    fn cap_call_gas_allowance_preserves_smaller_allowance() {
        let gas_cap = 50_000_000u64;
        assert_eq!(cap_call_gas_allowance(0, gas_cap), 0);
        assert_eq!(cap_call_gas_allowance(21_000, gas_cap), 21_000);
        assert_eq!(cap_call_gas_allowance(gas_cap - 1, gas_cap), gas_cap - 1);
    }

    #[test]
    fn cap_call_gas_allowance_zero_gas_cap_is_uncapped() {
        assert_eq!(cap_call_gas_allowance(u64::MAX, 0), u64::MAX);
        assert_eq!(cap_call_gas_allowance(100_000_000, 0), 100_000_000);
        assert_eq!(cap_call_gas_allowance(0, 0), 0);
    }

    #[test]
    fn token_evm_call_mode_zero_gas_cap_is_uncapped() {
        let token = token_1to1(None, U256::MAX);

        assert_eq!(
            token_gas_allowance(
                U256::ZERO,
                U256::ZERO,
                &token,
                U256::from(50u64),
                None,
                1,
                0,
            )
            .expect("gas_cap == 0 without fee_limit must be uncapped"),
            u64::MAX
        );
        assert_eq!(
            token_gas_allowance(
                U256::ZERO,
                U256::ZERO,
                &token,
                U256::from(50u64),
                Some(U256::ZERO),
                1,
                0,
            )
            .expect("gas_cap == 0 with zero fee_limit must be uncapped"),
            u64::MAX
        );
        assert_eq!(
            token_gas_allowance(
                U256::ZERO,
                U256::ZERO,
                &token,
                U256::from(50u64),
                Some(U256::from(500u64)),
                1,
                0,
            )
            .expect("gas_cap == 0 with fee_limit must preserve user budget"),
            450
        );
    }

    #[test]
    fn estimate_max_gas_limit_relaxes_header_tx_cap_and_clamps_to_rpc_gas_cap() {
        use alloy_consensus::Header;
        use alloy_rpc_types_eth::BlockOverrides;
        use morph_chainspec::MORPH_MAINNET;
        use morph_evm::MorphEvmConfig;
        use morph_primitives::MorphHeader;
        use revm::database::{CacheDB, EmptyDB};

        let evm_config = MorphEvmConfig::new_with_default_factory(MORPH_MAINNET.clone());
        let header = MorphHeader {
            inner: Header {
                number: 30_000_000,
                timestamp: 2_000_000_000, // Osaka+ (Celadon) active on mainnet
                gas_limit: 30_000_000,
                base_fee_per_gas: Some(1_000_000),
                ..Default::default()
            },
            next_l1_msg_index: 0,
        };

        let base_env = evm_config.evm_env(&header).expect("evm_env must succeed");
        assert_eq!(base_env.cfg_env.tx_gas_limit_cap, Some(30_000_000));

        // Without `blockOverrides.gasLimit`:
        // - `gas_cap > header.gas_limit()` keeps `header.gas_limit()` as the ceiling.
        // - `gas_cap < header.gas_limit()` clamps down to `gas_cap`.
        let mut no_override_env = base_env.clone();
        relax_estimate_tx_gas_limit_cap(&mut no_override_env);
        assert_eq!(
            resolve_estimate_max_gas_limit(&no_override_env, 50_000_000),
            30_000_000
        );
        assert_eq!(
            resolve_estimate_max_gas_limit(&no_override_env, 20_000_000),
            20_000_000
        );
        assert_eq!(
            resolve_estimate_max_gas_limit(&no_override_env, 0),
            30_000_000
        );

        // With `blockOverrides.gasLimit = 45_000_000 > header.gas_limit()`:
        // - Stale `tx_gas_limit_cap` would have clamped back to 30_000_000.
        // - After `relax_estimate_tx_gas_limit_cap`, the 45M override takes effect,
        //   while still clamping to non-zero `gas_cap` and treating `0` as uncapped.
        let mut override_env = base_env;
        relax_estimate_tx_gas_limit_cap(&mut override_env);
        let mut db = CacheDB::new(EmptyDB::default());
        apply_block_overrides(
            BlockOverrides {
                gas_limit: Some(45_000_000),
                ..Default::default()
            },
            &mut db,
            override_env.block_env.inner_mut(),
        );
        assert_eq!(
            resolve_estimate_max_gas_limit(&override_env, 50_000_000),
            45_000_000
        );
        assert_eq!(
            resolve_estimate_max_gas_limit(&override_env, 40_000_000),
            40_000_000
        );
        assert_eq!(resolve_estimate_max_gas_limit(&override_env, 0), 45_000_000);
        assert_eq!(
            resolve_estimate_max_gas_limit(&override_env, u64::MAX),
            45_000_000
        );

        // Lower `blockOverrides.gasLimit < header.gas_limit()` also tightens `max_gas_limit`.
        let mut lower_override_env = evm_config.evm_env(&header).expect("evm_env must succeed");
        relax_estimate_tx_gas_limit_cap(&mut lower_override_env);
        apply_block_overrides(
            BlockOverrides {
                gas_limit: Some(15_000_000),
                ..Default::default()
            },
            &mut db,
            lower_override_env.block_env.inner_mut(),
        );
        assert_eq!(
            resolve_estimate_max_gas_limit(&lower_override_env, 50_000_000),
            15_000_000
        );
    }

    #[test]
    fn compute_optimistic_gas_limit_uses_u128_without_shrinking_above_u64_div_64() {
        // Normal range matches `(gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63`.
        assert_eq!(
            compute_optimistic_gas_limit(200_000, 0),
            (200_000 + CALL_STIPEND_GAS) * 64 / 63
        );

        // Above `u64::MAX / 64`, `u64::saturating_mul(64) / 63` would collapse to `u64::MAX / 63`
        // (< `gas_used`!). Widening to `u128` keeps the result >= `gas_used` and saturates cleanly.
        let large_gas = u64::MAX / 2;
        let optimistic = compute_optimistic_gas_limit(large_gas, 0);
        assert!(
            optimistic > large_gas,
            "expected optimistic ({optimistic}) > large_gas ({large_gas})"
        );
        assert_eq!(compute_optimistic_gas_limit(u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn initial_binary_search_mid_never_regresses_below_lowest_gas_limit() {
        // Standard case (`gas_used * 3 > lowest_gas_limit` and `< midpoint`): uses `gas_used * 3`.
        assert_eq!(
            initial_binary_search_mid(100_000, 99_999, 30_000_000),
            300_000
        );

        // Refund-heavy or failed-optimistic case where `lowest_gas_limit >= gas_used * 3`:
        // falls back to `midpoint` strictly inside `(lowest_gas_limit, highest_gas_limit)`.
        let mid = initial_binary_search_mid(21_050, 205_511, 30_000_000);
        assert!(mid > 205_511 && mid < 30_000_000, "unexpected mid: {mid}");

        // Narrow bracket `[205_511, 209_000]` with `gas_used * 3 = 63_150 < lowest_gas_limit`:
        let narrow_mid = initial_binary_search_mid(21_050, 205_511, 209_000);
        assert_eq!(narrow_mid, (205_511 + 209_000) / 2);
    }
}
