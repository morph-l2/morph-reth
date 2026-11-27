//! Morph `eth_call` / `eth_estimateGas` / `eth_simulateV1` overrides.
//!
//! [`Call::caller_gas_allowance`] is overridden so `eth_estimateGas` caps
//! gas by `balance − value − l1_fee` (ETH path) or the fee token balance
//! (MorphTx `fee_token_id > 0`). `eth_call` and `eth_createAccessList`
//! are detected by the block gas limit check being disabled and keep the
//! plain ETH allowance, without the L1-fee extension.
//!
//! [`EstimateCall::estimate_gas_with`] is also overridden to fix three
//! upstream estimation defects:
//! 1. Cap balance allowance using `tx_env.gas_price` (`maxFeePerGas` for
//!    dynamic-fee requests) instead of `effective_gas_price`.
//! 2. Guard the `is_basic_transfer` 21,000-gas shortcut with
//!    `highest_gas_limit >= MIN_TRANSACTION_GAS` so sub-21k allowances or
//!    explicit `gas < 21_000` cannot bypass the cap.
//! 3. Retry at `max_gas_limit` on revert / gas-too-high whenever
//!    `highest_gas_limit < max_gas_limit`, covering EIP-1559 and MorphTx
//!    requests whose legacy `gasPrice` field is absent.
//!
//! [`EthCall::simulate_v1`] is overridden so `eth_simulateV1` disables fee
//! charging (including Morph's non-zero L1 data fee) when `validation: false`
//! and passes each transaction's real EIP-2718 encoding when `validation: true`
//! so Curie `commit + blob` (and pre-Curie calldata) L1 fees are accurately
//! charged and validated.

use crate::MorphEthApiError;
use crate::eth::{MorphEthApi, MorphNodeCore};
use alloy_consensus::BlockHeader;
use alloy_eips::eip2718::{Encodable2718, WithEncoded};
use alloy_network::TransactionBuilder;
use alloy_primitives::{Address, B256, TxKind, U256, U512};
use alloy_rpc_types_eth::{
    BlockId,
    simulate::{SimBlock, SimulatePayload, SimulatedBlock},
    state::EvmOverrides,
};
use evm2::{
    EvmFeatures, TxResult,
    bytecode::Bytecode,
    evm::{AccountInfo, CacheDB, Db},
    interpreter::{InstrStop, Word},
    registry::HandlerError,
};
use evm2_inspectors::transfer::TransferInspector;
use morph_chainspec::{MorphChainSpec, MorphHardforks};
use morph_revm::{L1BlockInfo, MorphEvmEnv, MorphTxExt, TokenFeeInfo};
use reth_chainspec::{EthChainSpec, EthereumHardforks, MIN_TRANSACTION_GAS};
use reth_errors::{ProviderError, RethError};
use reth_evm::{
    BlockExecutionError, BundleState, ConfigureEvm, Database, Evm, EvmEnv, EvmEnvFor, EvmTypesFor,
    TransactionEnvMut, TxEnvFor,
    database::StateProviderDatabase,
    execute::{
        BlockBuilder, BlockBuilderOutcome, BlockExecutor, BlockTransactionResult,
        map_database_error,
    },
};
use reth_execution_types::BundleSource;
use reth_primitives_traits::NodePrimitives;
use reth_provider::ChainSpecProvider;
use reth_rpc_convert::{RpcConvert, RpcTxReq};
use reth_rpc_eth_api::{
    AsEthApiError, EthApiTypes, FromEthApiError, FromEvmError, IntoEthApiError, RpcBlock,
    RpcNodeCore,
    helpers::{
        Call, EthCall, LoadBlock, LoadPendingBlock, SpawnBlocking,
        call::SimulatedBlocksResult,
        estimate::{EstimateCall, update_estimated_gas_range},
    },
};
use reth_rpc_eth_types::{
    EthApiError, RpcInvalidTransactionError,
    cache::db::{apply_block_overrides, apply_state_overrides, apply_state_overrides_with_state},
    error::api::{FromEvmHalt, FromRevert},
    simulate::{self, EthSimulateError, SimulatedState},
};
use reth_rpc_server_types::constants::gas_oracle::{CALL_STIPEND_GAS, ESTIMATE_GAS_ERROR_RATIO};
use reth_storage_api::{EvmStateProvider, StateProvider, noop::NoopProvider};
use reth_tasks::cancel::is_cancelled;
use tracing::trace;

impl<N, Rpc> EthCall for MorphEthApi<N, Rpc>
where
    N: MorphNodeCore,
    N::Provider: ChainSpecProvider<ChainSpec = MorphChainSpec>,
    Rpc:
        reth_rpc_convert::RpcConvert<Primitives = N::Primitives, Error = EthApiError, Evm = N::Evm>,
    MorphEthApiError: reth_rpc_eth_types::error::FromEvmError<N::Evm>,
{
    async fn simulate_v1(
        &self,
        payload: SimulatePayload<RpcTxReq<<Self::RpcConvert as RpcConvert>::Network>>,
        block: Option<BlockId>,
    ) -> SimulatedBlocksResult<Self::NetworkTypes, Self::Error> {
        if payload.block_state_calls.len() > self.max_simulate_blocks() as usize {
            return Err(EthApiError::other(EthSimulateError::TooManyBlocks).into());
        }

        let block = block.unwrap_or_default();

        let SimulatePayload {
            block_state_calls,
            trace_transfers,
            validation,
            return_full_transactions,
        } = payload;

        if block_state_calls.is_empty() {
            return Err(EthApiError::InvalidParams(String::from("calls are empty.")).into());
        }

        let permit = self
            .acquire_owned_blocking_io()
            .await
            .map_err(|_| EthApiError::InternalEthError)?;

        let base_block = self
            .recovered_block(block)
            .await?
            .ok_or_else(|| EthApiError::other(EthSimulateError::BlockNotFound { block }))?;
        let parent = base_block.sealed_header().clone();
        let max_simulate_blocks = self.max_simulate_blocks();
        let state_at = if block.is_pending() {
            block
        } else {
            parent.hash().into()
        };

        self.spawn_with_state_at_block(state_at, move |this, db| {
            let _permit = permit;
            let state_provider = db.db.into_inner().into_inner().0;
            let mut db = CacheDB::new(Db::new(StateProviderDatabase::new(
                (&state_provider).into_evm_state_provider(),
            )));
            let mut parent = parent;
            let mut simulation_state = this
                .compute_state_root_for_eth_simulate()
                .then(SimulatedState::default);

            let chain_id = this.provider().chain_spec().chain_id();

            let block_state_calls = simulate::sanitize_chain(
                block_state_calls,
                &parent,
                chain_id,
                max_simulate_blocks,
            )?;

            let mut blocks: Vec<SimulatedBlock<RpcBlock<Self::NetworkTypes>>> =
                Vec::with_capacity(block_state_calls.len());

            let call_gas_limit = this.call_gas_limit();
            let mut remaining_call_gas_limit = (call_gas_limit > 0).then_some(call_gas_limit);

            for block in block_state_calls {
                let SimBlock {
                    block_overrides,
                    state_overrides,
                    calls,
                } = block;

                let attributes = this
                    .pending_env_builder()
                    .pending_env_attributes(&parent, block_overrides.as_ref())
                    .map_err(Self::Error::from_eth_err)?;

                let mut evm_env = this
                    .evm_config()
                    .next_evm_env(&parent, &attributes)
                    .map_err(RethError::other)
                    .map_err(Self::Error::from_eth_err)?;

                // Always disable EIP-3607
                evm_env.version_mut().features.remove(EvmFeatures::EIP3607);

                if !validation || !evm_env.version().feature(EvmFeatures::EIP8037) {
                    evm_env.version_mut().tx_gas_limit_cap = u64::MAX;
                }

                if !validation {
                    // If not explicitly required, disable nonce check, basefee check, and
                    // fee charging (including Morph's non-zero L1 data fee and token fees)
                    // so zero-balance callers succeed and no phantom fees are debited/credited.
                    evm_env
                        .version_mut()
                        .features
                        .remove(EvmFeatures::NONCE_CHECK);
                    evm_env
                        .version_mut()
                        .features
                        .remove(EvmFeatures::BASE_FEE_CHECK);
                    evm_env
                        .version_mut()
                        .features
                        .remove(EvmFeatures::FEE_CHARGE);
                    evm_env.block_env_mut().basefee = U256::ZERO;
                }

                evm_env.block_env_mut().prevrandao = U256::ZERO;
                if !this
                    .provider()
                    .chain_spec()
                    .is_paris_active_at_block(evm_env.block_env().number.to())
                {
                    evm_env.block_env_mut().difficulty = parent.difficulty();
                }

                if let Some(block_overrides) = block_overrides {
                    if let Some(gas_limit_override) = block_overrides.gas_limit
                        && gas_limit_override > evm_env.block_env().gas_limit.to::<u64>()
                        && gas_limit_override > this.call_gas_limit()
                    {
                        return Err(EthApiError::other(EthSimulateError::GasLimitReached).into());
                    }
                    apply_block_overrides(block_overrides, &mut db, &mut evm_env);
                }
                if let Some(ref state_overrides) = state_overrides {
                    let mut changes = simulation_state.as_ref().map(|_| BundleState::default());
                    apply_state_overrides_with_state(
                        state_overrides.clone(),
                        &mut db,
                        changes.as_mut(),
                    )
                    .map_err(Self::Error::from_eth_err)?;
                    if let Some(simulation_state) = simulation_state.as_mut()
                        && let Some(changes) = changes
                    {
                        simulation_state
                            .extend(&state_provider, &changes)
                            .map_err(Self::Error::from_eth_err)?;
                    }
                }

                let chain_id = evm_env.chain_id();
                let build_bal = this
                    .provider()
                    .chain_spec()
                    .is_amsterdam_active_at_timestamp(evm_env.block_env().timestamp.to::<u64>());

                let ctx = this
                    .evm_config()
                    .context_for_next_block(&parent, attributes)
                    .map_err(RethError::other)
                    .map_err(Self::Error::from_eth_err)?;
                let map_err = |e: EthApiError| -> Self::Error {
                    match e.as_simulate_error() {
                        Some(sim_err) => Self::Error::from_eth_err(EthApiError::other(sim_err)),
                        None => Self::Error::from_eth_err(e),
                    }
                };

                // EIP-7708 already emits transfer logs: https://eips.ethereum.org/EIPS/eip-7708
                let trace_transfers =
                    trace_transfers && !evm_env.version().feature(EvmFeatures::EIP7708);
                let mut evm = this.evm_config().evm_with_env(&mut db, evm_env.clone());
                if trace_transfers {
                    evm.set_inspector(TransferInspector::new(false).with_logs(true));
                }
                let mut builder = this
                    .evm_config()
                    .create_block_builder(evm, evm_env, &parent, ctx);
                if build_bal {
                    builder.executor_mut().enable_block_access_list_builder();
                }

                if let Some(ref state_overrides) = state_overrides {
                    simulate::apply_precompile_overrides(state_overrides, builder.evm_mut())
                        .map_err(|e| Self::Error::from_eth_err(EthApiError::other(e)))?;
                }

                let (result, results) = execute_simulated_transactions(
                    builder,
                    &*state_provider,
                    calls,
                    &mut remaining_call_gas_limit,
                    chain_id,
                    validation,
                    simulation_state.as_mut(),
                    this.converter(),
                )
                .map_err(map_err)?;

                db.commit_source(&BundleSource(&result.execution_state));
                let simulated_header = result.block.clone_sealed_header();
                db.insert_block_hash(
                    &U256::from(simulated_header.number()),
                    &simulated_header.hash(),
                );
                parent = simulated_header;

                let block =
                    simulate::build_simulated_block::<Self::Error, _, EvmTypesFor<Self::Evm>>(
                        result.block,
                        results,
                        return_full_transactions.into(),
                        this.converter(),
                    )?;

                blocks.push(block);
            }

            Ok(blocks)
        })
        .await
    }
}

/// Adapter exposing [`Evm::account_info`] as a [`Database`] for [`simulate::resolve_transaction`].
struct EvmAccountDb<'a, E>(&'a mut E);

impl<E: Evm> Database for EvmAccountDb<'_, E> {
    type Error = BlockExecutionError;

    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.account_info(address)
    }

    fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
        self.0
            .state_db_mut()
            .get_code_by_hash(code_hash)
            .map_err(map_database_error)
    }

    fn get_storage(&mut self, address: &Address, key: &Word) -> Result<Word, Self::Error> {
        self.0
            .state_db_mut()
            .get_storage(address, key)
            .map_err(map_database_error)
    }

    fn get_block_hash(&mut self, number: &Word) -> Result<B256, Self::Error> {
        self.0
            .state_db_mut()
            .get_block_hash(number)
            .map_err(map_database_error)
    }
}

/// Executes simulated transactions, matching upstream `reth_rpc_eth_types::simulate::execute_transactions`
/// while preserving each transaction's real EIP-2718 encoding when `validation == true` so Morph charges
/// full Curie `commit + blob` (or pre-Curie calldata) L1 data fees.
#[expect(clippy::too_many_arguments, clippy::type_complexity)]
fn execute_simulated_transactions<S, T, EvmTypes>(
    mut builder: S,
    state_provider: impl StateProvider,
    calls: Vec<RpcTxReq<T::Network>>,
    remaining_call_gas_limit: &mut Option<u64>,
    chain_id: u64,
    validation: bool,
    simulation_state: Option<&mut SimulatedState>,
    converter: &T,
) -> Result<(BlockBuilderOutcome<S::Primitives>, Vec<TxResult<EvmTypes>>), EthApiError>
where
    S: BlockBuilder<
            Primitives: NodePrimitives<SignedTx: Encodable2718>,
            Executor: BlockExecutor<Evm: Evm<EvmTypes = EvmTypes>>,
        >,
    T: RpcConvert<Primitives = S::Primitives>,
    EvmTypes: evm2::EvmTypes,
{
    if is_cancelled() {
        return Err(EthApiError::InternalEthError);
    }
    builder.apply_pre_execution_changes()?;

    let mut results = Vec::with_capacity(calls.len());
    let mut cumulative_tx_gas_used: u64 = 0;
    let mut block_regular_gas_used: u64 = 0;
    let mut block_state_gas_used: u64 = 0;
    let block_gas_limit = builder.evm_env().block_env().gas_limit.to::<u64>();
    let is_amsterdam = builder.evm_env().version().feature(EvmFeatures::EIP8037);
    let tx_gas_limit_cap = builder.evm_env().version().tx_gas_limit_cap;
    for mut call in calls {
        if is_cancelled() {
            return Err(EthApiError::InternalEthError);
        }
        let block_gas_remaining = if is_amsterdam {
            let regular_available_gas = block_gas_limit.saturating_sub(block_regular_gas_used);
            let state_available_gas = block_gas_limit.saturating_sub(block_state_gas_used);
            if regular_available_gas >= tx_gas_limit_cap {
                state_available_gas
            } else {
                regular_available_gas.min(state_available_gas)
            }
        } else {
            block_gas_limit.saturating_sub(cumulative_tx_gas_used)
        };
        let mut default_gas_limit = block_gas_remaining;

        if let Some(gas_limit) = call.as_ref().gas_limit()
            && gas_limit > block_gas_remaining
        {
            return Err(EthApiError::other(EthSimulateError::BlockGasLimitExceeded));
        }

        if let Some(remaining_call_gas_limit) = *remaining_call_gas_limit {
            if let Some(gas_limit) = call.as_ref().gas_limit() {
                if gas_limit > remaining_call_gas_limit {
                    call.as_mut().set_gas_limit(remaining_call_gas_limit);
                }
            } else {
                default_gas_limit = default_gas_limit.min(remaining_call_gas_limit);
            }
        }

        let block_base_fee = builder.evm_env().block_base_fee();
        let disable_nonce_check = !builder
            .evm_env()
            .version()
            .feature(EvmFeatures::NONCE_CHECK);
        let mut account_db = EvmAccountDb(builder.evm_mut());
        let tx = simulate::resolve_transaction(
            call,
            default_gas_limit,
            block_base_fee,
            chain_id,
            disable_nonce_check,
            &mut account_db,
            converter,
        )?;
        // When `validation == true`, pass the real EIP-2718 encoding so Morph's L1
        // fee calculation charges the full commit + blob (Curie) or calldata
        // (pre-Curie) fee. When `validation == false`, fee charging is disabled
        // (`!EvmFeatures::FEE_CHARGE`), so an empty envelope avoids wasted encoding.
        let encoded = if validation {
            tx.encoded_2718().into()
        } else {
            Default::default()
        };
        let tx = WithEncoded::new(encoded, tx);

        let mut tx_regular_gas_used = 0;
        let gas_output = builder.execute_transaction_with_result_closure(tx, |output| {
            let output = &output.result().result;
            tx_regular_gas_used = output.execution_gas_spent();
            results.push(output.clone())
        })?;

        // The transfer tracer keeps the transaction's logs, with the synthetic transfer logs
        // among them, outside the EVM's own logs.
        // SAFETY: `TransferInspector` has no lifetime parameters.
        if let Some(logs) = unsafe {
            builder
                .evm_mut()
                .with_inspector(|inspector: &mut TransferInspector| inspector.take_logs())
        } && let Some(result) = results.last_mut()
        {
            result.logs = logs;
        }

        let gas_used = gas_output.tx_gas_used();
        if let Some(remaining_call_gas_limit) = remaining_call_gas_limit.as_mut() {
            if gas_used > *remaining_call_gas_limit {
                return Err(EthApiError::other(EthSimulateError::GasLimitReached));
            }
            *remaining_call_gas_limit -= gas_used;
        }

        cumulative_tx_gas_used = cumulative_tx_gas_used.saturating_add(gas_used);
        block_regular_gas_used = block_regular_gas_used.saturating_add(tx_regular_gas_used);
        block_state_gas_used = block_state_gas_used.saturating_add(gas_output.state_gas_used());
    }

    let result = if let Some(simulation_state) = simulation_state {
        builder.finish_with_state_root(&state_provider, |output| {
            simulation_state
                .extend(&state_provider, &output.state)
                .map_err(BlockExecutionError::other)?;
            state_provider
                .state_root_with_updates(simulation_state.hashed_state().clone())
                .map(Some)
                .map_err(BlockExecutionError::other)
        })?
    } else {
        builder.finish(NoopProvider::default(), None)?
    };

    Ok((result, results))
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
        evm_env.version_mut().features.remove(EvmFeatures::EIP3607);
        evm_env
            .version_mut()
            .features
            .remove(EvmFeatures::BASE_FEE_CHECK);
        evm_env.version_mut().features.remove(
            EvmFeatures::FEE_CHARGE | EvmFeatures::BALANCE_CHECK | EvmFeatures::BALANCE_TOP_UP,
        );

        request.as_mut().take_nonce();

        let tx_request_gas_limit = request.as_ref().gas_limit();

        let mut db = CacheDB::new(Db::new(StateProviderDatabase::new(state)));

        if let Some(block_overrides) = overrides.block {
            apply_block_overrides(*block_overrides, &mut db, &mut evm_env);
        }

        if let Some(state_override) = overrides.state {
            apply_state_overrides(state_override, &mut db).map_err(Self::Error::from_eth_err)?;
        }

        let block_gas_limit = evm_env.block_env().gas_limit.to::<u64>();
        let mut max_gas_limit = evm_env.version().tx_gas_caps().0.min(block_gas_limit);

        let gas_cap = self.call_gas_limit();
        if gas_cap != 0 {
            max_gas_limit = max_gas_limit.min(gas_cap);
        }

        let mut highest_gas_limit = tx_request_gas_limit
            .unwrap_or(max_gas_limit)
            .min(max_gas_limit);

        let is_basic_transfer = if request
            .as_ref()
            .input()
            .is_none_or(|input| input.is_empty())
            && let Some(TxKind::Call(to)) = request.as_ref().kind()
        {
            match db.get_account(&to) {
                Ok(Some(account)) => account.is_empty_code_hash(),
                Ok(None) => true,
                Err(_) => false,
            }
        } else {
            false
        };

        let mut tx_env = self.create_txn_env(&evm_env, request, &mut db)?;

        // Cap by caller balance whenever the worst-case fee cap (`gas_price` for legacy,
        // `maxFeePerGas` for EIP-1559/EIP-7702/MorphTx) is non-zero.
        if tx_env.gas_price > 0 {
            let allowance = self.caller_gas_allowance(&mut db, &evm_env, &tx_env)?;
            highest_gas_limit = highest_gas_limit.min(allowance);
        }

        let gas_constrained = highest_gas_limit < max_gas_limit;

        trace!(
            target: "rpc::eth::estimate",
            ?tx_env,
            gas_limit = highest_gas_limit,
            is_basic_transfer,
            "Starting gas estimation"
        );

        let mut evm = self.evm_config().evm_with_env(&mut db, evm_env);
        let mut execute = move |gas_limit| {
            tx_env.inner_mut().set_gas_limit(gas_limit);
            evm.transact_result(&tx_env)
                .map_err(<Self::Error as FromEvmError<Self::Evm>>::from_evm_err)
        };

        // Only short-circuit basic transfers when the capped gas budget actually
        // covers MIN_TRANSACTION_GAS (21,000); otherwise fall through so the cap
        // reports `GasRequiredExceedsAllowance`.
        if is_basic_transfer
            && highest_gas_limit >= MIN_TRANSACTION_GAS
            && let Ok(res) = execute(MIN_TRANSACTION_GAS)
            && res.status
        {
            return Ok(U256::from(res.tx_gas_used()));
        }

        let mut res = match execute(highest_gas_limit) {
            Err(err) if err.is_gas_too_high() && gas_constrained => {
                let retry = execute(max_gas_limit)?;
                return map_out_of_gas_result::<_, EvmTypesFor<Self::Evm>>(
                    retry,
                    highest_gas_limit,
                );
            }
            Err(err) if err.is_gas_too_low() => {
                return Err(RpcInvalidTransactionError::GasRequiredExceedsAllowance {
                    gas_limit: highest_gas_limit,
                }
                .into_eth_err());
            }
            ethres => ethres?,
        };

        let gas_refund = if res.status {
            res.final_refunded()
        } else if res.stop == InstrStop::Revert {
            // Whenever `highest_gas_limit` was constrained below `max_gas_limit`
            // (whether by explicit `gas`, Legacy `gasPrice`, or EIP-1559/MorphTx
            // `maxFeePerGas` balance capping), retry with `max_gas_limit` to
            // distinguish inner-call out-of-gas reverts from genuine contract reverts.
            return if gas_constrained {
                let retry = execute(max_gas_limit)?;
                map_out_of_gas_result::<_, EvmTypesFor<Self::Evm>>(retry, highest_gas_limit)
            } else {
                Err(Self::Error::from_revert(res.output))
            };
        } else {
            return Err(Self::Error::from_evm_halt(res.stop, highest_gas_limit));
        };

        let mut gas_used = res.tx_gas_used();
        let mut lowest_gas_limit = gas_used.saturating_sub(1);

        let optimistic_gas_limit = (gas_used + gas_refund + CALL_STIPEND_GAS) * 64 / 63;
        if optimistic_gas_limit < highest_gas_limit {
            res = execute(optimistic_gas_limit)?;
            gas_used = res.tx_gas_used();
            update_estimated_gas_range::<EvmTypesFor<Self::Evm>>(
                res,
                optimistic_gas_limit,
                &mut highest_gas_limit,
                &mut lowest_gas_limit,
            )?;
        }

        let mut mid_gas_limit = std::cmp::min(
            gas_used * 3,
            ((highest_gas_limit as u128 + lowest_gas_limit as u128) / 2) as u64,
        );

        trace!(
            target: "rpc::eth::estimate",
            ?highest_gas_limit,
            ?lowest_gas_limit,
            ?mid_gas_limit,
            "Starting binary search for gas"
        );

        while lowest_gas_limit + 1 < highest_gas_limit {
            let ratio = (highest_gas_limit - lowest_gas_limit) as f64 / (highest_gas_limit as f64);
            if ratio < ESTIMATE_GAS_ERROR_RATIO {
                break;
            }

            if is_cancelled() {
                return Err(EthApiError::InternalEthError.into());
            }

            match execute(mid_gas_limit) {
                Err(err) if err.is_gas_too_high() => {
                    highest_gas_limit = mid_gas_limit;
                }
                Err(err) if err.is_gas_too_low() => {
                    lowest_gas_limit = mid_gas_limit;
                }
                ethres => {
                    update_estimated_gas_range::<EvmTypesFor<Self::Evm>>(
                        ethres?,
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

fn map_out_of_gas_result<Error, EvmTypes>(
    result: TxResult<EvmTypes>,
    requested_gas_limit: u64,
) -> Result<U256, Error>
where
    Error: FromEvmHalt<InstrStop> + FromRevert + FromEthApiError,
    EvmTypes: evm2::EvmTypes,
{
    if result.status {
        Err(RpcInvalidTransactionError::BasicOutOfGas(requested_gas_limit).into_eth_err())
    } else if result.stop == InstrStop::Revert {
        Err(Error::from_revert(result.output))
    } else {
        Err(Error::from_evm_halt(result.stop, requested_gas_limit))
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
        // eth_call / eth_createAccessList: no L1-fee cap. Token-fee callers
        // can have zero ETH, so defer to the handler instead of the
        // upstream ETH allowance.
        if !evm_env
            .version()
            .feature(EvmFeatures::BLOCK_GAS_LIMIT_CHECK)
        {
            if tx_env.fee_token_id.is_some_and(|id| id > 0) {
                return Ok(u64::MAX);
            }
            let balance = db
                .get_account(&tx_env.signer())
                .map_err(|e| MorphEthApiError::Eth(e.into()))?
                .map(|account| account.balance)
                .unwrap_or_default();
            let available = balance
                .checked_sub(tx_env.value)
                .ok_or(MorphEthApiError::InsufficientFundsForTransfer)?;
            return Ok(gas_allowance_from_balance(available, tx_env.gas_price));
        }

        // eth_estimateGas path.
        let l1_fee = self.estimate_l1_fee(&mut db, evm_env, tx_env)?;

        if let Some(fee_token_id) = tx_env.fee_token_id.filter(|id| *id > 0) {
            return self.caller_gas_allowance_with_token(
                &mut db,
                evm_env,
                tx_env.signer(),
                tx_env.value,
                l1_fee,
                fee_token_id,
                tx_env.fee_limit,
                tx_env.gas_price,
            );
        }

        // allowance = (balance − value − l1_fee) / gas_price, done at wei
        // precision so the remainder is not lost across the two
        // subtractions.
        let balance = db
            .get_account(&tx_env.signer())
            .map_err(|e| MorphEthApiError::Eth(e.into()))?
            .map(|acc| acc.balance)
            .unwrap_or_default();
        let available = balance
            .checked_sub(tx_env.value)
            .ok_or(MorphEthApiError::InsufficientFundsForTransfer)?;
        // Reject at `l1_fee >= available` so the RPC surfaces the real
        // reason rather than a downstream "gas required exceeds allowance 0".
        if l1_fee >= available {
            return Err(MorphEthApiError::InsufficientFundsForL1Fee);
        }
        Ok(gas_allowance_from_balance(
            available - l1_fee,
            tx_env.gas_price,
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

        let block_number = u64::try_from(evm_env.block_env().number)
            .map_err(|_| EthApiError::InvalidParams("invalid block number".to_string()))?;
        let timestamp = u64::try_from(evm_env.block_env().timestamp)
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
        evm_env: &MorphEvmEnv,
        caller: Address,
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
        let token_fee_info = load_token_fee_info_for_allowance(db, fee_token_id, caller, evm_env)?;

        // ETH only needs to cover `value`; gas + L1 fee are paid in tokens.
        let eth_balance = db
            .get_account(&caller)
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
        )
    }
}

/// Resolves fee-token metadata and `caller`'s balance (both direct-slot and call-mode `balanceOf`)
/// for `eth_estimateGas`, preserving database/provider failures instead of masking them as
/// [`MorphEthApiError::InvalidFeeToken`].
fn load_token_fee_info_for_allowance<DB: Database>(
    db: &mut DB,
    fee_token_id: u16,
    caller: Address,
    evm_env: &MorphEvmEnv,
) -> Result<TokenFeeInfo, MorphEthApiError> {
    match TokenFeeInfo::load_for_caller(db, fee_token_id, caller, evm_env) {
        Ok(Some(info)) => Ok(info),
        Ok(None) => Err(MorphEthApiError::InvalidFeeToken),
        Err(HandlerError::Database(db_err)) => {
            if let Some(provider_err) = db_err.downcast_ref::<ProviderError>() {
                return Err(MorphEthApiError::from(provider_err.clone()));
            }
            Err(MorphEthApiError::from(ProviderError::other(db_err)))
        }
        Err(HandlerError::Fatal(msg)) => Err(MorphEthApiError::Internal(msg.to_string())),
        Err(_) => Err(MorphEthApiError::InvalidFeeToken),
    }
}

/// Compute the gas allowance for a MorphTx fee-token caller.
///
/// Both slot-mode and call-mode balances are already resolved in `token_fee_info.balance`;
/// `--rpc.gascap` and `block_gas_limit` clamping are handled upstream by Reth's
/// `estimate_gas_with`.
fn token_gas_allowance(
    eth_balance: U256,
    value: U256,
    token_fee_info: &TokenFeeInfo,
    l1_fee: U256,
    fee_limit: Option<U256>,
    gas_price: u128,
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

    let limit = token_fee_info.effective_fee_limit(fee_limit.unwrap_or_default());
    gas_allowance_from_token_limit(limit, l1_fee, token_fee_info, gas_price)
}

/// Convert a token-denominated affordability cap into an exact gas allowance.
///
/// Execution charges `ceil((l1_fee + gas * gas_price) * scale / price_ratio) <= limit_token`.
/// For integers `limit_token >= 0` and `price_ratio > 0`, `ceil(A / price_ratio) <= limit_token`
/// holds iff `A <= limit_token * price_ratio`, i.e.:
///
/// `l1_fee + gas * gas_price <= floor((limit_token * price_ratio) / scale)`
///
/// Evaluating `limit_token * price_ratio` in [`U512`] avoids both intermediate `U256` saturation
/// and the double-rounding loss of converting `l1_fee` to whole tokens first.
fn gas_allowance_from_token_limit(
    limit_token: U256,
    l1_fee: U256,
    token_info: &TokenFeeInfo,
    gas_price: u128,
) -> Result<u64, MorphEthApiError> {
    if token_info.price_ratio.is_zero() || token_info.scale.is_zero() {
        return Err(MorphEthApiError::InvalidFeeToken);
    }

    let max_total_eth_u512 = (U512::from(limit_token) * U512::from(token_info.price_ratio))
        / U512::from(token_info.scale);
    let max_total_eth = max_total_eth_u512.saturating_to::<U256>();

    if l1_fee >= max_total_eth {
        return Err(MorphEthApiError::InsufficientFundsForL1Fee);
    }
    let available_eth = max_total_eth - l1_fee;
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

    /// Both call-mode (`balance_slot == None`) and slot-mode (`balance_slot == Some(_)`)
    /// resolve the caller's real token balance via `load_for_caller` and bound by
    /// `effective_fee_limit(fee_limit)` = `min(balance, fee_limit)` (or `balance` when `fee_limit == 0`).
    #[test]
    fn token_call_and_slot_modes_cap_by_min_of_resolved_balance_and_fee_limit() {
        for balance_slot in [None, Some(U256::from(42u64))] {
            let token = token_1to1(balance_slot, /* balance */ U256::from(1_000u64));

            // fee_limit < balance → fee_limit binds.
            let allowance = token_gas_allowance(
                U256::ZERO,
                U256::ZERO,
                &token,
                /* l1_fee */ U256::from(50u64),
                /* fee_limit */ Some(U256::from(500u64)),
                /* gas_price */ 1,
            )
            .expect("balance covers L1 fee");
            assert_eq!(allowance, 450);

            // fee_limit > balance → balance binds.
            let allowance = token_gas_allowance(
                U256::ZERO,
                U256::ZERO,
                &token,
                U256::from(50u64),
                Some(U256::from(10_000u64)),
                1,
            )
            .expect("balance covers L1 fee");
            assert_eq!(allowance, 950);

            // No fee_limit or explicit Some(0) → balance binds.
            for unlimited in [None, Some(U256::ZERO)] {
                let allowance = token_gas_allowance(
                    U256::ZERO,
                    U256::ZERO,
                    &token,
                    U256::from(50u64),
                    unlimited,
                    1,
                )
                .expect("balance covers L1 fee");
                assert_eq!(allowance, 950);
            }
        }
    }

    /// L1 fee >= effective token budget surfaces `InsufficientFundsForL1Fee`.
    #[test]
    fn token_l1_fee_swallows_limit_returns_clear_error() {
        let token = token_1to1(None, U256::from(1_000u64));
        let err = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            /* l1_fee */ U256::from(100u64),
            /* fee_limit */ Some(U256::from(50u64)),
            1,
        )
        .expect_err("fee_limit cannot cover L1 fee");
        assert!(matches!(err, MorphEthApiError::InsufficientFundsForL1Fee));
    }

    /// Inactive / misconfigured token returns InvalidFeeToken before any
    /// limit math runs (regardless of mode).
    #[test]
    fn token_inactive_or_misconfigured_returns_invalid_fee_token() {
        let mut token = token_1to1(None, U256::from(1_000u64));
        token.is_active = false;
        let err = token_gas_allowance(
            U256::ZERO,
            U256::ZERO,
            &token,
            U256::ZERO,
            Some(U256::from(1_000u64)),
            1,
        )
        .expect_err("inactive token must reject");
        assert!(matches!(err, MorphEthApiError::InvalidFeeToken));

        let mut token = token_1to1(Some(U256::from(1u64)), U256::from(1_000u64));
        token.price_ratio = U256::ZERO;
        let err = token_gas_allowance(U256::ZERO, U256::ZERO, &token, U256::ZERO, None, 1)
            .expect_err("zero price_ratio must reject");
        assert!(matches!(err, MorphEthApiError::InvalidFeeToken));

        let mut token = token_1to1(Some(U256::from(1u64)), U256::from(1_000u64));
        token.scale = U256::ZERO;
        let err = token_gas_allowance(U256::ZERO, U256::ZERO, &token, U256::ZERO, None, 1)
            .expect_err("zero scale must reject");
        assert!(matches!(err, MorphEthApiError::InvalidFeeToken));
    }

    /// Exact ETH-space conversion avoids L1-fee double-rounding and `U256` overflow:
    /// 1) When `scale=1, price_ratio=10`, 1 token buys up to 10 wei total (`ceil((6 + 4) / 10) == 1`),
    ///    so `l1_fee = 6` leaves 4 wei of execution gas rather than falsely failing `ceil(6/10) >= 1`.
    /// 2) Non-unit `scale=10, price_ratio=3`: 1..=3 tokens afford 0 wei, 4 tokens afford 1 wei.
    /// 3) Huge `limit_token * price_ratio > U256::MAX` uses `U512` instead of prematurely saturating.
    #[test]
    fn gas_allowance_from_token_limit_uses_exact_u512_eth_budget() {
        let coarse_token = TokenFeeInfo {
            price_ratio: U256::from(10u64),
            scale: U256::from(1u64),
            ..token_1to1(None, U256::from(1u64))
        };
        let allowance = gas_allowance_from_token_limit(
            /* limit_token */ U256::from(1u64),
            /* l1_fee */ U256::from(6u64),
            &coarse_token,
            /* gas_price */ 1,
        )
        .expect("1 token worth 10 wei covers 6 wei L1 fee + 4 gas");
        assert_eq!(allowance, 4);
        assert_eq!(
            coarse_token.eth_to_token_amount(U256::from(6u64 + 4u64)),
            U256::from(1u64)
        );

        let fractional_token = TokenFeeInfo {
            price_ratio: U256::from(3u64),
            scale: U256::from(10u64),
            ..token_1to1(None, U256::from(10u64))
        };
        for insufficient in [1u64, 3u64] {
            let err = gas_allowance_from_token_limit(
                U256::from(insufficient),
                U256::ZERO,
                &fractional_token,
                1,
            )
            .expect_err("cannot afford 1 wei");
            assert!(matches!(err, MorphEthApiError::InsufficientFundsForL1Fee));
        }
        assert_eq!(
            gas_allowance_from_token_limit(U256::from(4u64), U256::ZERO, &fractional_token, 1)
                .unwrap(),
            1
        );
        assert_eq!(
            gas_allowance_from_token_limit(U256::from(10u64), U256::ZERO, &fractional_token, 1)
                .unwrap(),
            3
        );

        // `limit_token * price_ratio > U256::MAX` with `scale = 1e18` still computes exact quotient.
        let scale = U256::from(10u128.pow(18));
        let high_ratio_token = TokenFeeInfo {
            price_ratio: scale,
            scale,
            ..token_1to1(None, U256::MAX)
        };
        let huge_limit = U256::MAX / U256::from(2u64);
        let allowance = gas_allowance_from_token_limit(
            huge_limit,
            U256::from(1_000u64),
            &high_ratio_token,
            1_000_000_000,
        )
        .expect("U512 product/quotient must not overflow");
        assert_eq!(allowance, u64::MAX);
    }

    #[test]
    fn load_token_fee_info_for_allowance_resolves_call_mode_and_propagates_db_errors() {
        use alloy_primitives::{B256, address, bytes};
        use evm2::{
            bytecode::Bytecode,
            evm::{AccountInfo, CacheDB, DynDatabase, EmptyDB, InMemoryDB},
        };
        use morph_chainspec::hardfork::MorphHardfork;
        use morph_revm::{L2_TOKEN_REGISTRY_ADDRESS, MorphBlockEnv, compute_mapping_slot};

        struct TestDb {
            inner: InMemoryDB,
            fail_reads: bool,
        }

        impl Database for TestDb {
            type Error = ProviderError;

            fn get_account(&mut self, addr: &Address) -> Result<Option<AccountInfo>, Self::Error> {
                if self.fail_reads {
                    return Err(ProviderError::BestBlockNotFound);
                }
                Ok(DynDatabase::get_account(&mut self.inner, addr).unwrap())
            }

            fn get_code_by_hash(&mut self, hash: &B256) -> Result<Bytecode, Self::Error> {
                if self.fail_reads {
                    return Err(ProviderError::BestBlockNotFound);
                }
                Ok(DynDatabase::get_code_by_hash(&mut self.inner, hash).unwrap())
            }

            fn get_storage(&mut self, addr: &Address, slot: &U256) -> Result<U256, Self::Error> {
                if self.fail_reads {
                    return Err(ProviderError::BestBlockNotFound);
                }
                Ok(DynDatabase::get_storage(&mut self.inner, addr, slot).unwrap())
            }

            fn get_block_hash(&mut self, num: &U256) -> Result<B256, Self::Error> {
                Ok(DynDatabase::get_block_hash(&mut self.inner, num).unwrap())
            }
        }

        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");
        let mut inner: InMemoryDB = CacheDB::new(EmptyDB::default());
        let mut key = [0u8; 32];
        key[31] = 1;
        let base = compute_mapping_slot(U256::from(151), &key);
        let mut packed = [0u8; 32];
        packed[30] = 18;
        packed[31] = 1;
        inner.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &base,
            &U256::from_be_bytes(token.into_word().0),
        );
        inner.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(1)),
            &U256::ZERO,
        );
        inner.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(2)),
            &U256::from_be_bytes(packed),
        );
        inner.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(3)),
            &U256::ONE,
        );
        inner.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &compute_mapping_slot(U256::from(153), &key),
            &U256::ONE,
        );
        inner.insert_account_info(
            &token,
            AccountInfo::default().with_code(Bytecode::new_legacy(bytes!("6000545f5260205ff3"))),
        );
        inner.insert_account_storage(&token, &U256::ZERO, &U256::from(420_000u64));

        let env = MorphEvmEnv::new(MorphHardfork::Celadon, MorphBlockEnv::default(), 2818);

        // 1. Call-mode resolves real `balanceOf`.
        let mut ok_db = TestDb {
            inner: inner.clone(),
            fail_reads: false,
        };
        let info = load_token_fee_info_for_allowance(&mut ok_db, 1, caller, &env).unwrap();
        assert_eq!(info.balance, U256::from(420_000u64));

        // 2. Unregistered token -> `InvalidFeeToken`.
        assert!(matches!(
            load_token_fee_info_for_allowance(&mut ok_db, 99, caller, &env).unwrap_err(),
            MorphEthApiError::InvalidFeeToken
        ));

        // 3. Database failure -> `MorphEthApiError::Eth(...)`, NOT `InvalidFeeToken`.
        let mut err_db = TestDb {
            inner,
            fail_reads: true,
        };
        let err = load_token_fee_info_for_allowance(&mut err_db, 1, caller, &env).unwrap_err();
        assert!(
            matches!(err, MorphEthApiError::Eth(_)),
            "DB failure must propagate as Eth error, got {err:?}"
        );
    }
}
