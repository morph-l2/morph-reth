//! Morph transaction handler.
//!
//! Every Morph transaction type runs through [`prepare`] and [`execute`], in the order
//! go-ethereum's `TransitionDb` runs them: charge the fee, reset the per-transaction access
//! state, apply EIP-7702 authorizations, run the message, then refund and pay the fee recipient.
//! That one flow keeps the rules below in a single place instead of split between the standard
//! evm2 handlers and hooks:
//!
//! - L1 data fee calculation and deduction
//! - ERC20 token fee deduction and reimbursement for MorphTx (0x7F)
//! - L2 execution fee handling, paid in full to the fee recipient
//! - L1 message transactions (0x7E), which pay no fees on L2

use std::sync::OnceLock;

use alloy_primitives::{Address, B256, Bytes, U256};
use evm2::{
    EvmFeatures, SpecId, TxResult,
    bytecode::Bytecode,
    env::{TxEnv, TxEnvExt},
    ethereum::{
        access_list_counts,
        eip7702::{AuthRefunds, apply_auth_list, eip7702_authorization_gas},
        execute_initial_frame, finalize_gas, initial_gas_and_reservoir, intrinsic_and_floor_gas,
        prepare_initial_frame, validate_block_gas_limit, validate_chain_id,
        validate_create_initcode, validate_floor_gas, validate_gas_price, validate_intrinsic_gas,
        validate_nonce_not_overflow, validate_priority_fee, validate_tx_gas_limit_cap,
        warm_access_list, warm_base_accounts,
    },
    evm::State,
    handler::GasSettlement,
    interpreter::{GasTracker, Host, Message, MessageExt, MessageKind, MessageResult, Word},
    registry::{HandlerError, HandlerResult, TxRegistry, TxRequest, handler},
};
use morph_chainspec::hardfork::MorphHardfork;
use morph_primitives::{
    L1_TX_TYPE_ID, MORPH_TX_TYPE_ID, transaction::morph_transaction::MORPH_TX_VERSION_2,
};

use crate::{
    MorphInvalidTransaction, MorphTxEnv,
    evm::{MorphEvm, MorphEvmTypes, MorphMessageExt, MorphTxResultExt},
    l1block::{L1_GAS_PRICE_ORACLE_ADDRESS, L1BlockInfo},
    token_fee::{
        TokenFeeInfo, TokenRegistryEntry, compute_mapping_slot_for_address,
        encode_balance_of_calldata, read_balance_from_storage,
    },
    tx::MorphTxExt,
};

const LEGACY_TX_TYPE: u8 = 0x00;
const EIP2930_TX_TYPE: u8 = 0x01;
const EIP1559_TX_TYPE: u8 = 0x02;
const EIP7702_TX_TYPE: u8 = 0x04;

/// Gas limit for internal EVM calls (ERC20 transfer, balanceOf).
const EVM_CALL_GAS_LIMIT: u64 = 200_000;

/// A transaction request as seen by the Morph handler.
type MorphTxRequest<'a, 'host> = TxRequest<'a, 'host, MorphEvmTypes, MorphTxEnv>;

/// Returns the transaction registry of a Morph hardfork.
///
/// EIP-7702 transactions exist from Prague on. Every other type is accepted by every Morph
/// hardfork, all of which are at least Cancun.
pub fn morph_tx_registry(
    spec: MorphHardfork,
) -> TxRegistry<MorphEvmTypes, TxResult<MorphEvmTypes>> {
    static WITH_EIP7702: OnceLock<TxRegistry<MorphEvmTypes, TxResult<MorphEvmTypes>>> =
        OnceLock::new();
    static WITHOUT_EIP7702: OnceLock<TxRegistry<MorphEvmTypes, TxResult<MorphEvmTypes>>> =
        OnceLock::new();

    if SpecId::from(spec).enables(SpecId::PRAGUE) {
        WITH_EIP7702.get_or_init(|| build_registry(true)).clone()
    } else {
        WITHOUT_EIP7702
            .get_or_init(|| build_registry(false))
            .clone()
    }
}

fn build_registry(eip7702: bool) -> TxRegistry<MorphEvmTypes, TxResult<MorphEvmTypes>> {
    let mut registry = TxRegistry::new();
    let tx_types = [
        LEGACY_TX_TYPE,
        EIP2930_TX_TYPE,
        EIP1559_TX_TYPE,
        L1_TX_TYPE_ID,
        MORPH_TX_TYPE_ID,
    ];
    for tx_type in tx_types {
        registry.register(
            tx_type,
            |tx: &MorphTxEnv| Some(tx),
            handler(prepare, execute),
        );
    }
    if eip7702 {
        registry.register(
            EIP7702_TX_TYPE,
            |tx: &MorphTxEnv| Some(tx),
            handler(prepare, execute),
        );
    }
    registry
}

/// The token fee charged by [`prepare`], needed again to refund unused gas.
#[derive(Clone, Copy, Debug)]
struct AppliedTokenFee {
    /// Registry data of the fee token, so that the deduction and the refund use the same
    /// `price_ratio`/`scale`, matching go-ethereum's `st.feeRate`/`st.tokenScale`.
    info: TokenFeeInfo,
    /// Token-unit numerator the deduction overcharged by rounding up.
    ///
    /// From Celadon on, the unused-gas refund adds this back before rounding down, so the
    /// caller pays `ceil` of the net fee instead of `ceil(prepaid) - ceil(refund)`, which
    /// under-collects. Mirrors go-ethereum's `st.altFeeRoundingCredit`.
    rounding_credit: U256,
    /// Signed refund counter of the fee deduction call. Applied before the final refund cap;
    /// the refund transfer's own counter is excluded.
    refund_counter: i64,
}

/// What [`prepare`] established for [`execute`].
#[derive(Debug)]
pub struct PreparedMorphTx {
    caller: Address,
    /// Effective gas price of the transaction.
    gas_price: U256,
    intrinsic: u64,
    floor_gas: u64,
    /// The sender's nonce before this transaction bumped it, which names a created contract.
    sender_nonce: u64,
    tx_env: TxEnv<MorphEvmTypes>,
    l1_data_fee: U256,
    token_fee: Option<AppliedTokenFee>,
}

/// Validates a transaction and applies its pre-execution state changes.
fn prepare(req: &mut MorphTxRequest<'_, '_>) -> HandlerResult<PreparedMorphTx> {
    let caller = req.tx.signer();
    let tx: &MorphTxEnv = req.tx.inner();
    let host: &mut MorphEvm<'_> = req.host;

    let basefee = host.block().basefee.saturating_to::<u128>();
    let gas_price = U256::from(tx.effective_gas_price_at(basefee));
    let tx_env = TxEnvExt {
        origin: caller,
        gas_price,
        chain_id: U256::from(host.version().chain_id),
        ..TxEnvExt::default()
    };

    if tx.is_l1_msg() {
        // If intrinsic gas exceeds the gas limit, fall back to the gas limit: L1 messages
        // prepay gas on L1 and must always execute, matching go-ethereum.
        let (intrinsic, _) = intrinsic_gas(host, tx, caller);
        let sender_nonce = bump_sender_nonce(host, caller)?;
        warm_transaction_accounts(host, tx, caller)?;
        return Ok(PreparedMorphTx {
            caller,
            gas_price,
            intrinsic: intrinsic.min(tx.gas_limit),
            floor_gas: 0,
            sender_nonce,
            tx_env,
            l1_data_fee: U256::ZERO,
            token_fee: None,
        });
    }

    validate_env(host, tx)?;
    let (intrinsic, floor_gas) = intrinsic_gas(host, tx, caller);
    validate_intrinsic_gas(tx.gas_limit, intrinsic, 0)?;
    validate_floor_gas(tx.gas_limit, floor_gas)?;

    let token_id = tx.fee_token_id.unwrap_or_default();
    let charged = if tx.is_morph_tx() && token_id > 0 {
        deduct_token_fee(host, tx, caller, &tx_env, token_id)?
    } else {
        deduct_eth_fee(host, tx, caller)?
    };
    warm_transaction_accounts(host, tx, caller)?;

    Ok(PreparedMorphTx {
        caller,
        gas_price,
        intrinsic,
        floor_gas,
        sender_nonce: charged.sender_nonce,
        tx_env,
        l1_data_fee: charged.l1_data_fee,
        token_fee: charged.token_fee,
    })
}

/// Executes a prepared transaction and settles its gas.
fn execute(
    req: MorphTxRequest<'_, '_>,
    prepared: PreparedMorphTx,
) -> HandlerResult<TxResult<MorphEvmTypes>> {
    let PreparedMorphTx {
        caller,
        gas_price,
        intrinsic,
        floor_gas,
        sender_nonce,
        tx_env,
        l1_data_fee,
        token_fee,
    } = prepared;
    let tx: &MorphTxEnv = req.tx.inner();
    let host: &mut MorphEvm<'_> = req.host;

    let execution_refund = apply_authorizations(host, tx)?;

    let (execution_gas_limit, reservoir) =
        initial_gas_and_reservoir(host.version(), tx.gas_limit, intrinsic, 0);
    let mut tx_gas =
        GasTracker::new_with_execution_gas_and_reservoir(execution_gas_limit, reservoir);
    let frame = prepare_initial_frame(
        host,
        caller,
        sender_nonce,
        tx.kind,
        &tx.data,
        tx.value,
        &mut tx_gas,
    )?;
    let mut result = execute_initial_frame(
        host,
        &tx_env,
        frame,
        &mut tx_gas,
        execution_gas_limit,
        reservoir,
    )?;

    if tx.is_l1_msg() {
        // L1 messages follow go-ethereum: no gas refund, so gas used is the gas consumed.
        result.gas.set_refunded(0);
        return settle(host, tx, caller, gas_price, floor_gas, result);
    }

    let fee_refund = token_fee.map_or(0, |fee| fee.refund_counter);
    result.gas.record_refund(fee_refund);
    result
        .gas
        .record_refund(i64::try_from(execution_refund).unwrap_or(i64::MAX));
    let mut output = settle(host, tx, caller, gas_price, floor_gas, result)?;

    let unused_gas = tx.gas_limit.saturating_sub(output.tx_gas_used());
    match token_fee {
        Some(fee) => reimburse_token_fee(host, &tx_env, caller, gas_price, unused_gas, fee)?,
        // A token-fee transaction that charged nothing (fee charging is disabled) has
        // nothing to reimburse or pay out, in either asset.
        None if tx.uses_token_fee() => {}
        None if host.feature(EvmFeatures::FEE_CHARGE) => {
            let refund = gas_price.saturating_mul(U256::from(unused_gas));
            host.state_mut().account(&caller)?.add_balance(refund);
            // Morph pays the whole fee to the fee recipient: there is no base fee burn.
            let beneficiary = host.block().beneficiary;
            let reward = gas_price
                .saturating_mul(U256::from(output.tx_gas_used()))
                .saturating_add(l1_data_fee);
            host.state_mut().account(&beneficiary)?.add_balance(reward);
        }
        None => {}
    }

    output.ext = MorphTxResultExt {
        l1_data_fee,
        token_fee_info: token_fee.map(|fee| fee.info),
    };
    Ok(output)
}

/// Finalizes gas accounting for the transaction.
fn settle(
    host: &mut MorphEvm<'_>,
    tx: &MorphTxEnv,
    caller: Address,
    gas_price: U256,
    floor_gas: u64,
    result: MessageResult<MorphEvmTypes>,
) -> HandlerResult<TxResult<MorphEvmTypes>> {
    finalize_gas(
        host,
        GasSettlement {
            caller,
            gas_price,
            gas_limit: tx.gas_limit,
            floor_gas,
            initial_state_gas: 0,
            state_refund: 0,
            result,
        },
    )
}

/// Validates the fields of a non-L1-message transaction that need no state.
fn validate_env(host: &MorphEvm<'_>, tx: &MorphTxEnv) -> HandlerResult<()> {
    let version = host.version();
    let basefee = host.block().basefee;
    let max_fee = U256::from(tx.gas_price);
    let priority_fee = U256::from(tx.gas_priority_fee.unwrap_or_default());
    let effective_gas_price = max_fee.min(basefee.saturating_add(priority_fee));

    // Legacy transactions are the only standard type that can omit the chain id.
    validate_chain_id(
        version,
        tx.chain_id,
        tx.tx_type == LEGACY_TX_TYPE || tx.is_morph_tx(),
    )?;
    validate_tx_gas_limit_cap(version, tx.gas_limit)?;

    match tx.tx_type {
        LEGACY_TX_TYPE | EIP2930_TX_TYPE => validate_gas_price(version, max_fee, basefee)?,
        EIP1559_TX_TYPE => {
            validate_priority_fee(version, max_fee, priority_fee)?;
            validate_gas_price(version, effective_gas_price, basefee)?;
        }
        EIP7702_TX_TYPE => {
            validate_priority_fee(version, max_fee, priority_fee)?;
            validate_gas_price(version, effective_gas_price, basefee)?;
            if tx.authorization_list.is_empty() {
                return Err(HandlerError::EmptyAuthorizationList);
            }
        }
        _ => {}
    }

    validate_block_gas_limit(version, tx.gas_limit, host.block().gas_limit)?;
    validate_create_initcode(version, tx.kind, &tx.data)?;
    validate_nonce_not_overflow(tx.nonce)?;

    if tx.is_morph_tx() {
        // The EIP-1559 fee-cap rules hold whichever asset ends up paying the fee, matching
        // Morph geth's preCheck. Unlike the standard types, a disabled base fee check does
        // not relax them while fees are being charged.
        if host.feature(EvmFeatures::FEE_CHARGE) {
            validate_priority_fee(version, max_fee, priority_fee)?;
            if effective_gas_price < basefee {
                return Err(HandlerError::FeeCapLessThanBaseFee {
                    max_fee_per_gas: effective_gas_price,
                    base_fee: basefee,
                });
            }
        }
        validate_morph_tx_authorization_list(host, tx)?;
    }

    Ok(())
}

/// Static EIP-7702 rules for MorphTx:
///
/// - V0/V1 must not carry an authorization list
/// - V2 with an empty list needs no extra checks (it behaves like V1)
/// - V2 with authorizations requires Prague (always true past Viridian,
///   asserted anyway) and a call target (no CREATE)
fn validate_morph_tx_authorization_list(host: &MorphEvm<'_>, tx: &MorphTxEnv) -> HandlerResult<()> {
    let version = tx.version.unwrap_or_default();

    if version < MORPH_TX_VERSION_2 {
        if !tx.authorization_list.is_empty() {
            return Err(MorphInvalidTransaction::AuthorizationListNotSupported { version }.into());
        }
        return Ok(());
    }

    if tx.authorization_list.is_empty() {
        return Ok(());
    }

    if !host.spec_id().enables(SpecId::PRAGUE) {
        return Err(MorphInvalidTransaction::Eip7702NotSupported.into());
    }
    if tx.kind.is_create() {
        return Err(MorphInvalidTransaction::AuthorizationListCreate.into());
    }

    Ok(())
}

/// Returns the intrinsic gas and calldata floor gas of `tx`.
fn intrinsic_gas(host: &MorphEvm<'_>, tx: &MorphTxEnv, caller: Address) -> (u64, u64) {
    let version = host.version();
    let (access_list_accounts, access_list_storage_keys) = access_list_counts(&tx.access_list);
    let (intrinsic, floor_gas) = intrinsic_and_floor_gas(
        version,
        caller,
        tx.kind,
        &tx.data,
        access_list_accounts,
        access_list_storage_keys,
        tx.value,
    );
    (
        intrinsic + eip7702_authorization_gas(version, tx.authorization_list.len()),
        floor_gas,
    )
}

/// Rejects a sender that cannot send `tx`: a contract (EIP-3607) or the wrong nonce.
fn validate_sender(host: &mut MorphEvm<'_>, caller: Address, tx_nonce: u64) -> HandlerResult<()> {
    let eip3607 = host.feature(EvmFeatures::EIP3607);
    let nonce_check = host.feature(EvmFeatures::NONCE_CHECK);
    let mut sender = host.state_mut().account(&caller)?;

    if eip3607 && !sender.is_empty_code_hash() {
        let code = sender.load_code()?;
        if !code.is_empty() && !code.is_eip7702() {
            return Err(HandlerError::RejectCallerWithCode);
        }
    }
    if nonce_check && sender.nonce() != tx_nonce {
        return Err(HandlerError::InvalidNonce {
            expected: sender.nonce(),
            got: tx_nonce,
        });
    }
    Ok(())
}

/// Bumps the sender's nonce, returning its previous value.
fn bump_sender_nonce(host: &mut MorphEvm<'_>, caller: Address) -> HandlerResult<u64> {
    let mut sender = host.state_mut().account(&caller)?;
    let nonce = sender.nonce();
    sender.bump_nonce();
    Ok(nonce)
}

/// Warms what EIP-2929/2930/3651 start a transaction with, as `StateDB.Prepare` does.
///
/// This must run after the fee deduction: the deduction's own warmth has been dropped by then
/// (see [`cool_loaded_state`]) and these accounts are warm regardless of it.
fn warm_transaction_accounts(
    host: &mut MorphEvm<'_>,
    tx: &MorphTxEnv,
    caller: Address,
) -> HandlerResult<()> {
    host.state_mut().account(&caller)?.warm();
    warm_base_accounts(host, tx.kind);
    warm_access_list(host, &tx.access_list);
    Ok(())
}

/// Applies the transaction's EIP-7702 authorization list and returns the gas to refund.
///
/// A `0x04` transaction and a MorphTx V2 go through the same routine, with the same refund
/// accounting. The list is empty for every other transaction.
fn apply_authorizations(host: &mut MorphEvm<'_>, tx: &MorphTxEnv) -> HandlerResult<u64> {
    if tx.authorization_list.is_empty() {
        return Ok(0);
    }
    let chain_id = host.version().chain_id;
    let mut refunds = AuthRefunds::new(host.version());
    apply_auth_list(host, chain_id, &tx.authorization_list, &mut refunds)?;
    Ok(refunds.execution_refund)
}

/// The fee a transaction was charged during [`prepare`].
struct ChargedFee {
    l1_data_fee: U256,
    token_fee: Option<AppliedTokenFee>,
    sender_nonce: u64,
}

/// Fetches the L1 data fee parameters for this transaction and prices it.
///
/// Must NOT use a per-block cache because the oracle can be updated by a regular transaction
/// (from the external gas-oracle service) within the same block. Subsequent user txs must see
/// the updated fee parameters, matching go-ethereum's per-tx `L1BlockInfo` read.
fn l1_data_fee(host: &mut MorphEvm<'_>, tx: &MorphTxEnv) -> HandlerResult<U256> {
    let hardfork = host.config_spec_id();
    let state = host.state_mut();
    let l1_block_info = L1BlockInfo::try_fetch_with(
        |slot| state.storage_slot_untracked(&L1_GAS_PRICE_ORACLE_ADDRESS, &slot),
        hardfork,
    )?;
    let rlp_bytes = tx.rlp_bytes.as_ref().map(Bytes::as_ref).unwrap_or_default();
    Ok(l1_block_info.calculate_tx_l1_cost(rlp_bytes, hardfork))
}

/// Validates the caller and deducts ETH-based gas fees, including the L1 data fee.
fn deduct_eth_fee(
    host: &mut MorphEvm<'_>,
    tx: &MorphTxEnv,
    caller: Address,
) -> HandlerResult<ChargedFee> {
    let l1_data_fee = l1_data_fee(host, tx)?;
    validate_sender(host, caller, tx.nonce)?;

    let basefee = host.block().basefee.saturating_to::<u128>();
    let fee_charge = host.feature(EvmFeatures::FEE_CHARGE);
    let balance_check = host.feature(EvmFeatures::BALANCE_CHECK);

    let mut sender = host.state_mut().account(&caller)?;
    let new_balance = calculate_caller_fee_with_l1_cost(
        sender.balance(),
        tx,
        basefee,
        l1_data_fee,
        fee_charge,
        balance_check,
    )?;
    sender.set_balance(new_balance);
    let sender_nonce = sender.nonce();
    sender.bump_nonce();

    Ok(ChargedFee {
        l1_data_fee,
        token_fee: None,
        sender_nonce,
    })
}

/// Validates the caller and deducts token-based gas fees.
///
/// This handles gas payment using ERC20 tokens instead of ETH.
fn deduct_token_fee(
    host: &mut MorphEvm<'_>,
    tx: &MorphTxEnv,
    caller: Address,
    tx_env: &TxEnv<MorphEvmTypes>,
    token_id: u16,
) -> HandlerResult<ChargedFee> {
    // Token ID 0 means ETH — routed through deduct_eth_fee instead.
    if token_id == 0 {
        return Err(MorphInvalidTransaction::TokenIdZeroNotSupported.into());
    }

    // Validate account nonce and code (EIP-3607) BEFORE any state mutations.
    validate_sender(host, caller, tx.nonce)?;

    let fee_charge = host.feature(EvmFeatures::FEE_CHARGE);

    // Real transactions must cover the transferred ETH value before token metadata is loaded.
    // Simulations retain the existing value-check behavior and only add token eligibility.
    if fee_charge && !tx.value.is_zero() {
        let balance = host.state_mut().account(&caller)?.balance();
        if balance < tx.value {
            return Err(HandlerError::InsufficientFunds {
                fee: tx.value,
                balance,
            });
        }
    }

    let state = host.state_mut();
    let token_registry_entry = TokenRegistryEntry::load_with(
        |address, slot| state.storage_slot_untracked(&address, &slot),
        token_id,
    )?
    .ok_or(MorphInvalidTransaction::TokenNotRegistered(token_id))?
    .ensure_usable(token_id)?;

    // Simulations validate token eligibility but skip balance lookup and fee deduction.
    if !fee_charge {
        let sender_nonce = bump_sender_nonce(host, caller)?;
        return Ok(ChargedFee {
            l1_data_fee: U256::ZERO,
            token_fee: None,
            sender_nonce,
        });
    }

    let token_fee_info = load_token_fee_info(host, tx_env, token_registry_entry, caller)?;

    let beneficiary = host.block().beneficiary;
    let fee_limit_from_tx = tx.fee_limit.unwrap_or_default();

    // Get RLP-encoded transaction bytes for L1 fee calculation
    let l1_data_fee = l1_data_fee(host, tx)?;

    // Calculate L2 gas fee using effective_gas_price (= min(gasTipCap + baseFee, gasFeeCap)),
    // matching go-ethereum's buyAltTokenGas() which uses st.gasPrice (effective gas price).
    // Using the max fee would overcharge when tip + basefee < feeCap.
    let l2_gas_fee = U256::from(tx.gas_limit).saturating_mul(tx_env.gas_price);

    // Total fee in ETH
    let total_eth_fee = l2_gas_fee.saturating_add(l1_data_fee);

    // Calculate token amount required for total fee. The credit is the part of
    // one token unit the rounding-up overcharged; from Celadon on the refund
    // hands it back (see `reimburse_token_fee`).
    let (token_amount_required, rounding_credit) =
        token_fee_info.eth_to_token_amount_with_credit(total_eth_fee);

    let fee_limit = token_fee_info.effective_fee_limit(fee_limit_from_tx);

    // Check if caller has sufficient token balance
    if fee_limit < token_amount_required {
        return Err(MorphInvalidTransaction::InsufficientTokenBalance {
            required: token_amount_required,
            available: fee_limit,
        }
        .into());
    }

    let mut refund_counter = 0;
    if token_amount_required.is_zero() {
        // Geth skips both transfer modes for a zero fee. Nonce and fee bookkeeping below
        // still need their normal per-transaction updates.
    } else if let Some(balance_slot) = token_fee_info.balance_slot {
        // Transfer with token slot.
        transfer_erc20_with_slot(
            host,
            caller,
            beneficiary,
            token_fee_info.token_address,
            token_amount_required,
            balance_slot,
        )?;
    } else {
        // Transfer with evm call (from=caller, balance known from token registry).
        refund_counter = transfer_erc20_with_evm(
            host,
            tx_env,
            caller,
            beneficiary,
            token_fee_info.token_address,
            token_amount_required,
            Some(token_fee_info.balance),
        )?;
    }

    // Everything the deduction touched stays, but none of its access metadata may reach
    // the main transaction. A zero fee still ran `balanceOf`, which warmed the token.
    cool_loaded_state(host.state_mut());

    let sender_nonce = bump_sender_nonce(host, caller)?;

    Ok(ChargedFee {
        l1_data_fee,
        token_fee: Some(AppliedTokenFee {
            info: token_fee_info,
            rounding_credit,
            refund_counter,
        }),
        sender_nonce,
    })
}

/// Drops the access metadata a fee deduction left behind, keeping its state writes.
///
/// go-ethereum's `StateDB.Prepare` rebuilds the access list and resets transient storage after
/// the deduction, before the main transaction (core/state/statedb.go:1066). The equivalent
/// here is to mark everything cold and clear transient storage: the slots' `original` values
/// are untouched, so the EIP-2200 metering of a main-frame SSTORE still sees the slot's
/// pre-transaction value and its gas stays equal to go-ethereum's.
///
/// Transient storage can only belong to accounts a frame ran in, and those are all loaded.
fn cool_loaded_state(state: &mut State<'_>) {
    let loaded: Vec<(Address, Vec<Word>)> = state
        .loaded_accounts()
        .map(|(address, slots)| (*address, slots.copied().collect()))
        .collect();

    for (address, slots) in loaded {
        state.set_account_warm(&address, false);
        for slot in slots {
            state.set_storage_warm(&address, slot, false);
        }
        state.take_transient_storage(&address);
    }
}

/// Reimburse unused gas fees in ERC20 tokens.
///
/// Uses the token data cached from the deduction phase to ensure a consistent
/// price_ratio/scale, matching go-ethereum's `st.feeRate`/`st.tokenScale`.
fn reimburse_token_fee(
    host: &mut MorphEvm<'_>,
    tx_env: &TxEnv<MorphEvmTypes>,
    caller: Address,
    gas_price: U256,
    unused_gas: u64,
    fee: AppliedTokenFee,
) -> HandlerResult<()> {
    let beneficiary = host.block().beneficiary;
    let hardfork = host.config_spec_id();
    let AppliedTokenFee {
        info,
        rounding_credit,
        ..
    } = fee;

    let reimburse_eth = gas_price.saturating_mul(U256::from(unused_gas));
    if reimburse_eth.is_zero() {
        return Ok(());
    }

    // From Celadon on, add back the numerator the deduction's rounding-up
    // overcharged and round down, so the caller ends up paying `ceil` of the
    // net fee. Before Celadon both halves round up independently, which
    // under-collects; that is mainnet's history and must stay bit-identical.
    // Matches go-ethereum's `refundGas` gate on `IsCeladon`.
    let token_amount_required = if hardfork.is_celadon() {
        info.eth_to_token_amount_floor(reimburse_eth, rounding_credit)
    } else {
        info.eth_to_token_amount(reimburse_eth)
    };

    // Rounding down can land on zero, which the ceiling never did for a
    // non-zero `reimburse_eth`. go-ethereum's `TransferAltTokenHybrid`
    // returns early on a zero amount: no `Transfer(.., 0)` log on the
    // call path, and no slot writes on the direct-slot path.
    if token_amount_required.is_zero() {
        return Ok(());
    }

    // Attempt token refund. Matches go-ethereum's refundGas() which silently logs
    // and continues on failure: "Continue execution even if refund fails - refund
    // should not cause transaction to fail" (state_transition.go:698).
    let refund_result = if let Some(balance_slot) = info.balance_slot {
        transfer_erc20_with_slot(
            host,
            beneficiary,
            caller,
            info.token_address,
            token_amount_required,
            balance_slot,
        )
    } else {
        transfer_erc20_with_evm(
            host,
            tx_env,
            beneficiary,
            caller,
            info.token_address,
            token_amount_required,
            None,
        )
        .map(|_| ())
    };

    if let Err(err) = refund_result {
        // A contract may reject a refund, but unavailable state is not a verdict
        // about the contract, so it must reach the executor.
        if matches!(err, HandlerError::Database(_) | HandlerError::Fatal(_)) {
            return Err(err);
        }
        tracing::error!(
            target: "morph::evm",
            token = %info.token_address,
            %err,
            "failed to refund alt token gas, continuing execution"
        );
    }

    Ok(())
}

/// Performs an ERC20 balance transfer by directly reading and writing the token contract storage
/// using the known `balance` mapping base slot.
///
/// The token is touched ahead of the checkpoint, so that it stays among the transaction's state
/// changes even for a self-transfer that writes no slot, as go-ethereum's `SetState` still marks
/// it dirty. A failure undoes the balance writes; the token stays touched.
fn transfer_erc20_with_slot(
    host: &mut MorphEvm<'_>,
    from: Address,
    to: Address,
    token: Address,
    token_amount: U256,
    token_balance_slot: U256,
) -> HandlerResult<()> {
    host.state_mut().account(&token)?.touch();

    let features = host.version().features;
    let checkpoint = host.state().checkpoint();
    let result = write_erc20_balances(
        host.state_mut(),
        from,
        to,
        token,
        token_amount,
        token_balance_slot,
    );
    if result.is_err() {
        host.state_mut().rollback(checkpoint, features);
    }
    result
}

fn write_erc20_balances(
    state: &mut State<'_>,
    from: Address,
    to: Address,
    token: Address,
    token_amount: U256,
    token_balance_slot: U256,
) -> HandlerResult<()> {
    // Sub amount (checked: reject if insufficient, matching go-ethereum's
    // changeAltTokenBalanceByState which returns an error on underflow)
    let from_storage_slot = compute_mapping_slot_for_address(token_balance_slot, from);
    let from_balance = state.storage_slot(&token, from_storage_slot)?.current();
    let new_from_balance = from_balance.checked_sub(token_amount).ok_or(
        MorphInvalidTransaction::InsufficientTokenBalance {
            required: token_amount,
            available: from_balance,
        },
    )?;

    // Self-transfers are a no-op after the balance check above.
    let to_storage_slot = compute_mapping_slot_for_address(token_balance_slot, to);
    if from_storage_slot == to_storage_slot {
        return Ok(());
    }

    // Add amount (checked: unlike go-ethereum's unbounded big.Int Add,
    // we reject on overflow to maintain token conservation)
    let to_balance = state.storage_slot(&token, to_storage_slot)?.current();
    let new_to_balance = to_balance.checked_add(token_amount).ok_or_else(|| {
        MorphInvalidTransaction::TokenTransferFailed {
            reason: "recipient token balance overflow".into(),
        }
    })?;

    state
        .storage_slot(&token, from_storage_slot)?
        .set(new_from_balance);
    state
        .storage_slot(&token, to_storage_slot)?
        .set(new_to_balance);
    Ok(())
}

/// Loads internal-call code without changing the account's access-list temperature.
/// Geth's direct Call/StaticCall resolve code without executing a CALL opcode.
fn internal_call_code(host: &mut MorphEvm<'_>, address: Address) -> HandlerResult<Bytecode> {
    Ok(host.state_mut().account(&address)?.load_code()?)
}

/// Executes a fee-token frame while retaining the outer transaction's ORIGIN/GASPRICE.
/// The frame owns its VM checkpoint; a successful call is not rolled back merely
/// because the token's return value or balance delta fails a later business check.
fn evm_call(
    host: &mut MorphEvm<'_>,
    tx_env: &TxEnv<MorphEvmTypes>,
    caller: Address,
    target: Address,
    calldata: Bytes,
    is_static: bool,
) -> HandlerResult<MessageResult<MorphEvmTypes>> {
    let mut code = internal_call_code(host, target)?;
    let mut code_address = target;
    if let Some(delegate) = code.eip7702_address() {
        code = internal_call_code(host, delegate)?;
        code_address = delegate;
    }

    let mut message: Message<MorphEvmTypes> = MessageExt {
        kind: if is_static {
            MessageKind::StaticCall
        } else {
            MessageKind::Call
        },
        depth: host.version().initial_call_depth,
        gas_limit: EVM_CALL_GAS_LIMIT,
        reservoir: 0,
        destination: target,
        call_target: target,
        caller,
        input: calldata.into(),
        // A zero transfer also performs geth StaticCall's legacy account touch.
        value: U256::ZERO,
        code,
        code_address,
        caller_is_static: false,
        salt: B256::ZERO,
        ext: MorphMessageExt { fee_frame: true },
        _non_exhaustive: (),
    };
    Ok(host.execute_message(tx_env, &mut message)?)
}

/// Queries the token using a genuine static frame, as geth's StaticCall does.
/// Successful reads retain access-list warming; writes and malformed results fail.
pub(crate) fn evm_call_balance_of(
    host: &mut MorphEvm<'_>,
    tx_env: &TxEnv<MorphEvmTypes>,
    token: Address,
    account: Address,
) -> HandlerResult<U256> {
    let result = evm_call(
        host,
        tx_env,
        account,
        token,
        encode_balance_of_calldata(account),
        true,
    )?;
    if !result.stop.is_success() || result.output.len() < 32 {
        return Err(MorphInvalidTransaction::TokenBalanceQueryFailed.into());
    }
    Ok(U256::from_be_slice(&result.output[..32]))
}

/// Resolves the caller's fee-token balance against the **executing** EVM.
///
/// go-ethereum reads it through `st.evm` (`GetAltTokenBalanceHybrid`, core/token_gas.go:43),
/// so the `balanceOf` call sees the real block context, the real chain config and the user as
/// `msg.sender`.
fn load_token_fee_info(
    host: &mut MorphEvm<'_>,
    tx_env: &TxEnv<MorphEvmTypes>,
    entry: TokenRegistryEntry,
    caller: Address,
) -> HandlerResult<TokenFeeInfo> {
    let balance = match entry.balance_slot() {
        // Slot mode is a plain storage read with no environment to get wrong. It must not
        // go through the journaled accessors: a tracked load would warm a slot that the fee
        // deduction below is careful to leave cold.
        Some(slot) => {
            let state = host.state_mut();
            read_balance_from_storage(
                |address, slot| state.storage_slot_untracked(&address, &slot),
                entry.token_address(),
                caller,
                slot,
            )?
        }
        None => evm_call_balance_of(host, tx_env, entry.token_address(), caller)?,
    };
    Ok(entry.into_fee_info(caller, balance))
}

/// Matches go-ethereum's `transferAltTokenByEVM` validation:
/// 1. Checks EVM call succeeded (no revert)
/// 2. Validates ABI-decoded bool return value (supports old tokens with no return data)
/// 3. Verifies sender balance changed by the expected amount
///
/// `from_balance_before` is the sender's balance before the transfer. If `None`,
/// the balance is queried via EVM call (matching go-eth's nil `userBalanceBefore`).
/// Returns the signed refund counter for successful transfers; the deduction phase
/// carries it into transaction gas accounting, while reimbursement ignores it.
fn transfer_erc20_with_evm(
    host: &mut MorphEvm<'_>,
    tx_env: &TxEnv<MorphEvmTypes>,
    from: Address,
    to: Address,
    token_address: Address,
    token_amount: U256,
    from_balance_before: Option<U256>,
) -> HandlerResult<i64> {
    if token_amount.is_zero() {
        return Ok(0);
    }
    // Read sender balance before transfer if not provided.
    let from_balance_before = match from_balance_before {
        Some(balance) => balance,
        None => evm_call_balance_of(host, tx_env, token_address, from)?,
    };

    // Geth checks affordability before executing the token contract.
    let expected_balance = from_balance_before
        .checked_sub(token_amount)
        .ok_or_else(|| MorphInvalidTransaction::TokenTransferFailed {
            reason: format!(
                "sender balance {from_balance_before} less than token amount {token_amount}"
            ),
        })?;

    let calldata = build_transfer_calldata(to, token_amount);
    let result = evm_call(host, tx_env, from, token_address, calldata, false)?;

    if !result.stop.is_success() {
        return Err(MorphInvalidTransaction::TokenTransferFailed {
            reason: format!("{:?}", result.stop),
        }
        .into());
    }

    // Validate ABI bool return value, matching go-ethereum behavior:
    // - No return data: accepted (old tokens that don't return bool)
    // - 32+ bytes with last byte == 1: accepted (standard ERC20)
    // - Otherwise: rejected
    let output = &result.output;
    if !output.is_empty() && (output.len() < 32 || output[31] != 1) {
        return Err(MorphInvalidTransaction::TokenTransferFailed {
            reason: "alt token transfer returned failure".to_string(),
        }
        .into());
    }

    // Verify sender balance changed by the expected amount, matching go-ethereum.
    let from_balance_after = evm_call_balance_of(host, tx_env, token_address, from)?;

    // Verify sender balance decreased by exactly the transfer amount.
    // Matches go-ethereum's transferAltTokenByEVM which always checks this,
    // even for self-transfers (from == to), where it would fail because the
    // net balance change is zero but the expected decrease is `token_amount`.
    if from_balance_after != expected_balance {
        return Err(MorphInvalidTransaction::TokenTransferFailed {
            reason: format!(
                "sender balance mismatch: expected {expected_balance}, got {from_balance_after}"
            ),
        }
        .into());
    }

    Ok(result.gas.refunded())
}

/// Build the calldata for ERC20 `transfer(address,uint256)` call.
///
/// Method selector: `0xa9059cbb`
#[inline]
fn build_transfer_calldata(to: Address, token_amount: U256) -> Bytes {
    let method_id = [0xa9u8, 0x05, 0x9c, 0xbb];
    // Encode calldata: method_id + padded to address + amount
    let mut calldata = Vec::with_capacity(68);
    calldata.extend_from_slice(&method_id);
    let mut address_bytes = [0u8; 32];
    address_bytes[12..32].copy_from_slice(to.as_slice());
    calldata.extend_from_slice(&address_bytes);
    calldata.extend_from_slice(&token_amount.to_be_bytes::<32>());
    Bytes::from(calldata)
}

/// Calculate the new balance after deducting L2 fees and L1 data fee.
///
/// # Returns
/// The new balance after deducting all fees, or an error if balance is insufficient.
fn calculate_caller_fee_with_l1_cost(
    balance: U256,
    tx: &MorphTxEnv,
    basefee: u128,
    l1_data_fee: U256,
    fee_charge: bool,
    balance_check: bool,
) -> HandlerResult<U256> {
    // Simulation paths must not consume the caller balance.
    if !fee_charge {
        return Ok(balance);
    }

    // Validate balance against max possible spending using max_fee_per_gas (not effective_gas_price).
    // go-eth's buyGas() checks: balance >= gasFeeCap * gas + value + l1DataFee.
    // This ensures the sender can afford the worst-case gas cost before deducting the actual cost.
    if balance_check {
        let max_gas_spending = (tx.gas_limit as u128)
            .checked_mul(tx.gas_price)
            .map(U256::from)
            .ok_or(MorphInvalidTransaction::OverflowPaymentInTransaction)?;
        let max_spending = max_gas_spending
            .checked_add(tx.value)
            .and_then(|v| v.checked_add(l1_data_fee))
            .ok_or(MorphInvalidTransaction::OverflowPaymentInTransaction)?;
        if balance < max_spending {
            return Err(HandlerError::InsufficientFunds {
                fee: max_spending,
                balance,
            });
        }
    }

    // Deduct using effective_gas_price (not max_fee_per_gas).
    // go-eth's buyGas(): SubBalance(from, gasPrice * gas + l1DataFee)
    let gas_balance_spending = (tx.gas_limit as u128)
        .checked_mul(tx.effective_gas_price_at(basefee))
        .map(U256::from)
        .ok_or(MorphInvalidTransaction::OverflowPaymentInTransaction)?;
    let total_fee_deduction = gas_balance_spending.saturating_add(l1_data_fee);

    let mut new_balance = balance.saturating_sub(total_fee_deduction);

    if !balance_check {
        // Make sure the caller's balance is at least the value of the transaction.
        new_balance = new_balance.max(tx.value);
    }

    Ok(new_balance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_fee::{
        L2_TOKEN_REGISTRY_ADDRESS, compute_mapping_slot,
        tests::{UnreadableTokenDb, is_token_read_failure},
    };
    use crate::{MorphBlockEnv, MorphEvmEnv, evm::morph_evm, evm::morph_evm_env};
    use alloy_consensus::transaction::Recovered;
    use alloy_eips::eip7702::{
        Authorization, RecoveredAuthority, RecoveredAuthorization, SignedAuthorization,
    };
    use alloy_primitives::{Log, TxKind, address, b256};
    use evm2::{
        Inspector,
        ethereum::LazyAuthorization,
        evm::{AccountInfo, Database, Db, DynDatabase, InMemoryDB},
        interpreter::Interpreter,
        version::GasId,
    };
    use morph_chainspec::hardfork::MorphHardfork;
    use morph_primitives::transaction::morph_transaction::MORPH_TX_VERSION_1;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    /// The environment most tests run under: a block with room for any test transaction.
    fn test_env(spec: MorphHardfork, block: MorphBlockEnv) -> MorphEvmEnv {
        morph_evm_env(spec, 1, 30_000_000, block)
    }

    fn evm_on<'a>(
        database: impl DynDatabase + 'a,
        spec: MorphHardfork,
        block: MorphBlockEnv,
    ) -> MorphEvm<'a> {
        morph_evm(database, test_env(spec, block))
    }

    fn evm_with_spec(spec: MorphHardfork) -> MorphEvm<'static> {
        evm_on(InMemoryDB::default(), spec, MorphBlockEnv::default())
    }

    /// A transaction environment with `origin` and no gas price.
    fn tx_env_of(origin: Address) -> TxEnv<MorphEvmTypes> {
        TxEnvExt {
            origin,
            ..TxEnvExt::default()
        }
    }

    fn insert_contract(db: &mut InMemoryDB, address: Address, code: Bytes) {
        db.insert_account_info(
            &address,
            AccountInfo::default().with_code(Bytecode::new_legacy(code)),
        );
    }

    fn fund(db: &mut InMemoryDB, address: Address, balance: u64) {
        db.insert_account_info(
            &address,
            AccountInfo::default().with_balance(U256::from(balance)),
        );
    }

    fn transact(
        evm: &mut MorphEvm<'_>,
        tx: MorphTxEnv,
        caller: Address,
    ) -> HandlerResult<TxResult<MorphEvmTypes>> {
        let tx = Recovered::new_unchecked(tx, caller);
        evm.transact(&tx).map(|executed| executed.commit())
    }

    fn storage_of(evm: &mut MorphEvm<'_>, address: Address, key: U256) -> U256 {
        evm.state_mut()
            .storage_slot_untracked(&address, &key)
            .unwrap()
    }

    fn morph_error(error: &HandlerError) -> &MorphInvalidTransaction {
        error
            .external_ref::<MorphInvalidTransaction>()
            .unwrap_or_else(|| panic!("expected a Morph error, got {error:?}"))
    }

    fn no_state_loaded(evm: &MorphEvm<'_>) -> bool {
        evm.state().loaded_accounts().next().is_none()
    }

    const REFUND_PROBE_TOKEN: Address = address!("3000000000000000000000000000000000000003");

    /// Runs the token refund against a token implementing `code`.
    fn refund_against_token(database: impl DynDatabase) -> HandlerResult<()> {
        let caller = address!("1000000000000000000000000000000000000001");
        let block = MorphBlockEnv {
            beneficiary: address!("2000000000000000000000000000000000000002"),
            ..Default::default()
        };
        let mut evm = evm_on(database, MorphHardfork::Emerald, block);
        reimburse_token_fee(
            &mut evm,
            &tx_env_of(caller),
            caller,
            U256::from(1),
            1_000,
            AppliedTokenFee {
                info: TokenFeeInfo {
                    token_address: REFUND_PROBE_TOKEN,
                    is_active: true,
                    price_ratio: U256::from(1),
                    scale: U256::from(1),
                    caller,
                    balance: U256::from(100_000),
                    balance_slot: None,
                    ..Default::default()
                },
                rounding_credit: U256::ZERO,
                refund_counter: 0,
            },
        )
    }

    fn token_db_with_code(code: Bytes) -> InMemoryDB {
        let mut inner = InMemoryDB::default();
        insert_contract(&mut inner, REFUND_PROBE_TOKEN, code);
        inner
    }

    #[test]
    fn refund_database_failure_aborts_final_execution_result() {
        let result = refund_against_token(Db::new(UnreadableTokenDb {
            inner: token_db_with_code(alloy_primitives::bytes!("6000545f5260205ff3")),
            token: REFUND_PROBE_TOKEN,
        }));
        let error = result.unwrap_err();
        assert!(
            is_token_read_failure(&error),
            "refund I/O must abort execution, not finalize success without a refund: {error:?}"
        );
    }

    #[test]
    fn refund_contract_revert_still_allows_transaction_to_finish() {
        let result = refund_against_token(token_db_with_code(alloy_primitives::bytes!("5f5ffd")));
        assert!(result.is_ok(), "{result:?}");
    }

    #[test]
    fn token_transfer_database_failure_is_not_transaction_invalidity() {
        let token = address!("3000000000000000000000000000000000000003");
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let mut inner = InMemoryDB::default();
        insert_contract(
            &mut inner,
            token,
            alloy_primitives::bytes!("6000545f5260205ff3"),
        );
        let mut evm = evm_on(
            Db::new(UnreadableTokenDb { inner, token }),
            MorphHardfork::Emerald,
            MorphBlockEnv::default(),
        );
        let error = transfer_erc20_with_evm(
            &mut evm,
            &tx_env_of(from),
            from,
            to,
            token,
            U256::from(1),
            Some(U256::from(10)),
        )
        .unwrap_err();
        assert!(is_token_read_failure(&error), "{error:?}");
    }

    fn mutating_return_code(write_value: u8, return_value: u8) -> Bytes {
        Bytes::from(vec![
            0x60,
            write_value, // PUSH1 write_value
            0x60,
            0x00, // PUSH1 slot 0
            0x55, // SSTORE
            0x60,
            return_value, // PUSH1 return_value
            0x60,
            0x00, // PUSH1 offset 0
            0x52, // MSTORE
            0x60,
            0x20, // PUSH1 size 32
            0x60,
            0x00, // PUSH1 offset 0
            0xf3, // RETURN
        ])
    }

    #[derive(Debug)]
    struct TokenAccessTrackingDb {
        inner: InMemoryDB,
        token: Address,
        token_accessed: Arc<AtomicBool>,
    }

    impl Database for TokenAccessTrackingDb {
        type Error = evm2::DatabaseError;

        fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
            if *address == self.token {
                self.token_accessed.store(true, Ordering::Relaxed);
            }
            DynDatabase::get_account(&mut self.inner, address)
        }

        fn get_code_by_hash(&mut self, code_hash: &B256) -> Result<Bytecode, Self::Error> {
            DynDatabase::get_code_by_hash(&mut self.inner, code_hash)
        }

        fn get_storage(&mut self, address: &Address, key: &Word) -> Result<Word, Self::Error> {
            if *address == self.token {
                self.token_accessed.store(true, Ordering::Relaxed);
            }
            DynDatabase::get_storage(&mut self.inner, address, key)
        }

        fn get_block_hash(&mut self, number: &Word) -> Result<B256, Self::Error> {
            DynDatabase::get_block_hash(&mut self.inner, number)
        }
    }

    /// `<opcode> PUSH0 MSTORE PUSH1 0x20 PUSH0 RETURN` — a `balanceOf` that reports one piece
    /// of its environment instead of a balance, so a call made under the wrong environment
    /// shows up in the value that comes back.
    fn code_returning(opcode: u8) -> Bytes {
        Bytes::from(vec![opcode, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3])
    }

    /// Loads token 1's registry entry and resolves `caller`'s balance against `evm`.
    fn probe_fee_token_balance(db: InMemoryDB, block: MorphBlockEnv, caller: Address) -> U256 {
        let mut evm = evm_on(db, MorphHardfork::Emerald, block);
        let state = evm.state_mut();
        let entry = TokenRegistryEntry::load_with(
            |address, slot| state.storage_slot_untracked(&address, &slot),
            1,
        )
        .unwrap()
        .unwrap();
        load_token_fee_info(&mut evm, &tx_env_of(caller), entry, caller)
            .unwrap()
            .balance
    }

    #[test]
    fn fee_token_balance_is_read_under_the_executing_block_environment() {
        const TIMESTAMP: u64 = 1_767_765_600;
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");

        let mut db = InMemoryDB::default();
        insert_test_fee_token(&mut db, 1, token, true);
        insert_contract(&mut db, token, code_returning(0x42)); // TIMESTAMP

        let balance = probe_fee_token_balance(
            db,
            MorphBlockEnv {
                timestamp: U256::from(TIMESTAMP),
                ..Default::default()
            },
            caller,
        );

        // `BlockEnv::default()` reports timestamp 1, which is what a throwaway EVM would
        // have answered with regardless of the block being executed.
        assert_eq!(balance, U256::from(TIMESTAMP));
    }

    #[test]
    fn fee_token_balance_query_names_the_queried_account_as_the_caller() {
        let token = address!("5300000000000000000000000000000000000042");
        let caller = address!("1000000000000000000000000000000000000001");

        let mut db = InMemoryDB::default();
        insert_test_fee_token(&mut db, 1, token, true);
        insert_contract(&mut db, token, code_returning(0x33)); // CALLER

        let balance = probe_fee_token_balance(db, MorphBlockEnv::default(), caller);

        // go-ethereum queries as the account being asked about, not as the zero address and
        // not as `SYSTEM_ADDRESS`.
        assert_eq!(balance, U256::from_be_bytes(caller.into_word().0));
    }

    fn insert_test_fee_token(db: &mut InMemoryDB, token_id: u16, token: Address, is_active: bool) {
        insert_test_fee_token_config(db, token_id, token, is_active, U256::from(1), U256::from(1));
    }

    fn insert_test_fee_token_config(
        db: &mut InMemoryDB,
        token_id: u16,
        token: Address,
        is_active: bool,
        price_ratio: U256,
        scale: U256,
    ) {
        let mut token_id_bytes = [0u8; 32];
        token_id_bytes[30..32].copy_from_slice(&token_id.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &token_id_bytes);

        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &base,
            &U256::from_be_bytes(token.into_word().0),
        );

        let mut status = [0u8; 32];
        status[30] = 18;
        status[31] = u8::from(is_active);
        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &(base + U256::from(2)),
            &U256::from_be_bytes(status),
        );
        db.insert_account_storage(&L2_TOKEN_REGISTRY_ADDRESS, &(base + U256::from(3)), &scale);
        db.insert_account_storage(
            &L2_TOKEN_REGISTRY_ADDRESS,
            &compute_mapping_slot(U256::from(153), &token_id_bytes),
            &price_ratio,
        );
    }

    /// An environment with fee charging disabled, as simulations run.
    fn simulation_env() -> MorphEvmEnv {
        let mut env = test_env(MorphHardfork::default(), MorphBlockEnv::default());
        env.version.features.remove(EvmFeatures::FEE_CHARGE);
        env
    }

    fn token_fee_simulation_tx(token_id: u16) -> MorphTxEnv {
        MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            gas_limit: 21_000,
            kind: TxKind::Call(Address::ZERO),
            fee_token_id: Some(token_id),
            ..Default::default()
        }
    }

    /// Runs the deduction of a simulated token-fee transaction.
    fn simulate_token_fee<'a>(
        database: impl DynDatabase + 'a,
        caller: Address,
        token_id: u16,
    ) -> HandlerResult<ChargedFee> {
        let mut evm = morph_evm(database, simulation_env());
        deduct_token_fee(
            &mut evm,
            &token_fee_simulation_tx(token_id),
            caller,
            &tx_env_of(caller),
            token_id,
        )
    }

    #[test]
    fn validate_env_rejects_token_fee_morph_tx_below_base_fee() {
        let evm = evm_on(
            InMemoryDB::default(),
            MorphHardfork::default(),
            MorphBlockEnv {
                basefee: U256::from(100),
                ..Default::default()
            },
        );
        let tx = MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            gas_limit: 21_000,
            gas_price: 99,
            gas_priority_fee: Some(1),
            kind: TxKind::Call(Address::ZERO),
            fee_token_id: Some(1),
            ..Default::default()
        };

        assert!(matches!(
            validate_env(&evm, &tx).unwrap_err(),
            HandlerError::FeeCapLessThanBaseFee { .. }
        ));
    }

    // =========================================================================
    // MorphTx V2 (EIP-7702 authorization list) handler rules
    // =========================================================================

    fn sample_signed_authorization() -> LazyAuthorization {
        let signed: SignedAuthorization = Authorization {
            chain_id: U256::from(1),
            address: Address::with_last_byte(0x42),
            nonce: 0,
        }
        .into_signed(alloy_primitives::Signature::new(
            U256::from(1),
            U256::from(2),
            true,
        ));
        LazyAuthorization::Signed(signed)
    }

    fn recovered_authorization(
        authority: Address,
        delegate: Address,
        chain_id: u64,
        auth_nonce: u64,
    ) -> LazyAuthorization {
        LazyAuthorization::Recovered(RecoveredAuthorization::new_unchecked(
            Authorization {
                chain_id: U256::from(chain_id),
                address: delegate,
                nonce: auth_nonce,
            },
            RecoveredAuthority::Valid(authority),
        ))
    }

    fn morph_tx_env_with_authorizations(
        version: Option<u8>,
        kind: TxKind,
        authorization_list: Vec<LazyAuthorization>,
    ) -> MorphTxEnv {
        MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            gas_limit: 100_000,
            kind,
            authorization_list,
            version,
            fee_token_id: Some(0),
            ..Default::default()
        }
    }

    fn v2_env_with(authorization_list: Vec<LazyAuthorization>) -> MorphTxEnv {
        morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_2),
            TxKind::Call(Address::ZERO),
            authorization_list,
        )
    }

    fn validate_env_of(spec: MorphHardfork, tx: &MorphTxEnv) -> HandlerResult<()> {
        validate_env(&evm_with_spec(spec), tx)
    }

    #[test]
    fn validate_env_accepts_v2_morph_tx_with_authorization_list() {
        let tx = v2_env_with(vec![sample_signed_authorization()]);
        assert!(validate_env_of(MorphHardfork::Celadon, &tx).is_ok());
    }

    #[test]
    fn validate_env_rejects_v1_morph_tx_with_authorization_list() {
        let tx = morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_1),
            TxKind::Call(Address::ZERO),
            vec![sample_signed_authorization()],
        );

        let err = validate_env_of(MorphHardfork::Celadon, &tx).unwrap_err();
        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::AuthorizationListNotSupported {
                version: MORPH_TX_VERSION_1
            }
        ));
    }

    /// A V2 with an empty list is a V1 in all but the version byte: no 7702
    /// static rule applies (`EmptyAuthorizationList` is `0x04`-only).
    #[test]
    fn validate_env_accepts_v2_morph_tx_with_empty_authorization_list() {
        let tx = v2_env_with(vec![]);
        assert!(validate_env_of(MorphHardfork::Celadon, &tx).is_ok());
    }

    /// Without authorizations a V2 may create a contract, exactly like V1.
    #[test]
    fn validate_env_accepts_v2_morph_tx_create_without_authorizations() {
        let tx = morph_tx_env_with_authorizations(Some(MORPH_TX_VERSION_2), TxKind::Create, vec![]);
        assert!(validate_env_of(MorphHardfork::Celadon, &tx).is_ok());
    }

    #[test]
    fn validate_env_rejects_v2_morph_tx_create() {
        let tx = morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_2),
            TxKind::Create,
            vec![sample_signed_authorization()],
        );

        let err = validate_env_of(MorphHardfork::Celadon, &tx).unwrap_err();
        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::AuthorizationListCreate
        ));
    }

    #[test]
    fn validate_env_rejects_v2_morph_tx_before_prague() {
        // Structurally unreachable on Morph (Celadon > Viridian = Prague), but the
        // guard mirrors the `Eip7702NotSupported` rule of `0x04`.
        let tx = v2_env_with(vec![sample_signed_authorization()]);

        let err = validate_env_of(MorphHardfork::Morph203, &tx).unwrap_err();
        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::Eip7702NotSupported
        ));
    }

    #[test]
    fn validate_env_keeps_accepting_v1_morph_tx_without_authorizations() {
        let tx = morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_1),
            TxKind::Call(Address::ZERO),
            vec![],
        );
        assert!(validate_env_of(MorphHardfork::Celadon, &tx).is_ok());
    }

    fn account_nonce_and_code(evm: &mut MorphEvm<'_>, address: Address) -> (u64, Bytecode) {
        let mut account = evm.state_mut().account(&address).unwrap();
        (account.nonce(), account.load_code().unwrap())
    }

    /// A V2 list must apply exactly like a `0x04` transaction, and report the EIP-7702 refund
    /// for authorities that already exist.
    #[test]
    fn apply_authorizations_delegates_v2_morph_tx_authorities() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);

        let mut db = InMemoryDB::default();
        fund(&mut db, authority, 1);
        let mut evm = evm_on(db, MorphHardfork::Celadon, MorphBlockEnv::default());
        let tx = v2_env_with(vec![recovered_authorization(authority, delegate, 1, 0)]);

        let refund = apply_authorizations(&mut evm, &tx).unwrap();
        assert_eq!(
            refund,
            evm.version().gas_params.get(GasId::TxEip7702AuthRefund),
            "an existing authority earns the regular EIP-7702 refund"
        );
        assert_eq!(refund, 12_500);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 1, "authority nonce is consumed by the delegation");
        assert!(code.is_eip7702());
        assert_eq!(code, Bytecode::new_eip7702(delegate));
    }

    #[test]
    fn apply_authorizations_skips_invalid_v2_tuples() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);

        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![
            // wrong chain id
            recovered_authorization(authority, delegate, 999, 0),
            // wrong nonce (authority is at 0)
            recovered_authorization(authority, delegate, 1, 5),
        ]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 0);
        assert!(code.is_empty());
    }

    #[test]
    fn apply_authorizations_is_noop_for_morph_tx_without_authorizations() {
        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_1),
            TxKind::Call(Address::ZERO),
            vec![],
        );

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);
    }

    fn evm_with_authority(authority: Address, info: AccountInfo) -> MorphEvm<'static> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(&authority, info);
        evm_on(db, MorphHardfork::Celadon, MorphBlockEnv::default())
    }

    /// A never-seen authority is created by the delegation and earns no refund
    /// (the 25 000 intrinsic gas pays for the new account).
    #[test]
    fn apply_authorizations_creates_fresh_authority_without_refund() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);

        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![recovered_authorization(authority, delegate, 1, 0)]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 1);
        assert_eq!(code, Bytecode::new_eip7702(delegate));
    }

    /// An authority that is a real contract can never be delegated (EIP-7702 rule 5).
    #[test]
    fn apply_authorizations_skips_authority_with_contract_code() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);
        // PUSH1 0 PUSH1 0 REVERT
        let contract_code =
            Bytecode::new_legacy(Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xfd]));

        let mut evm = evm_with_authority(
            authority,
            AccountInfo::default()
                .with_balance(U256::from(1))
                .with_nonce(3)
                .with_code(contract_code.clone()),
        );
        let tx = v2_env_with(vec![recovered_authorization(authority, delegate, 1, 3)]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 3, "contract authority is left untouched");
        assert_eq!(code, contract_code);
    }

    /// An already-delegated authority can be re-pointed at a new delegate.
    #[test]
    fn apply_authorizations_redelegates_already_delegated_authority() {
        let authority = Address::with_last_byte(0xaa);
        let first = Address::with_last_byte(0x41);
        let second = Address::with_last_byte(0x42);

        let mut evm = evm_with_authority(
            authority,
            AccountInfo::default()
                .with_balance(U256::from(1))
                .with_nonce(5)
                .with_code(Bytecode::new_eip7702(first)),
        );
        let tx = v2_env_with(vec![recovered_authorization(authority, second, 1, 5)]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 12_500);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 6);
        assert_eq!(code, Bytecode::new_eip7702(second));
    }

    /// Delegating to the zero address clears the designator (EIP-7702 rule 8).
    #[test]
    fn apply_authorizations_zero_address_clears_delegation() {
        let authority = Address::with_last_byte(0xaa);

        let mut evm = evm_with_authority(
            authority,
            AccountInfo::default()
                .with_balance(U256::from(1))
                .with_nonce(5)
                .with_code(Bytecode::new_eip7702(Address::with_last_byte(0x41))),
        );
        let tx = v2_env_with(vec![recovered_authorization(
            authority,
            Address::ZERO,
            1,
            5,
        )]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 12_500);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 6);
        assert!(
            code.is_empty(),
            "zero-address delegation must clear the code"
        );
    }

    /// A tuple whose authority could not be recovered (the shape `MorphTxEnv::from_envelope`
    /// produces for a bad signature) is skipped, not fatal.
    #[test]
    fn apply_authorizations_skips_tuple_with_invalid_authority() {
        let delegate = Address::with_last_byte(0x42);
        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![LazyAuthorization::Recovered(
            RecoveredAuthorization::new_unchecked(
                Authorization {
                    chain_id: U256::from(1),
                    address: delegate,
                    nonce: 0,
                },
                RecoveredAuthority::Invalid,
            ),
        )]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);
        assert!(
            no_state_loaded(&evm),
            "no account may have been loaded for delegation"
        );
    }

    /// `chainId = 0` tuples are valid on every chain (EIP-7702 rule 1).
    #[test]
    fn apply_authorizations_accepts_chain_id_zero_tuple() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);

        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![recovered_authorization(authority, delegate, 0, 0)]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 1);
        assert_eq!(code, Bytecode::new_eip7702(delegate));
    }

    /// `nonce = 2^64 - 1` tuples are skipped (EIP-7702 rule 2).
    #[test]
    fn apply_authorizations_skips_nonce_max_tuple() {
        let authority = Address::with_last_byte(0xaa);
        let delegate = Address::with_last_byte(0x42);

        let mut evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![recovered_authorization(
            authority,
            delegate,
            1,
            u64::MAX,
        )]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 0);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 0);
        assert!(code.is_empty());
    }

    /// Two tuples for the same authority apply in order: the second one sees the
    /// nonce bumped by the first, and both earn the refund.
    #[test]
    fn apply_authorizations_applies_consecutive_tuples_for_same_authority() {
        let authority = Address::with_last_byte(0xaa);
        let first = Address::with_last_byte(0x41);
        let second = Address::with_last_byte(0x42);

        let mut evm = evm_with_authority(
            authority,
            AccountInfo::default().with_balance(U256::from(1)),
        );
        let tx = v2_env_with(vec![
            recovered_authorization(authority, first, 1, 0),
            recovered_authorization(authority, second, 1, 1),
        ]);

        assert_eq!(apply_authorizations(&mut evm, &tx).unwrap(), 25_000);

        let (nonce, code) = account_nonce_and_code(&mut evm, authority);
        assert_eq!(nonce, 2);
        assert_eq!(code, Bytecode::new_eip7702(second));
    }

    /// Simulation (`eth_call`, fee charge disabled) still enforces the static
    /// V2 rules; only the fee-cap check is fee-dependent.
    #[test]
    fn validate_env_enforces_v2_rules_when_fee_charge_is_disabled() {
        let mut env = test_env(MorphHardfork::Celadon, MorphBlockEnv::default());
        env.version.features.remove(EvmFeatures::FEE_CHARGE);
        let evm = morph_evm(InMemoryDB::default(), env);
        let tx = morph_tx_env_with_authorizations(
            Some(MORPH_TX_VERSION_2),
            TxKind::Create,
            vec![sample_signed_authorization()],
        );

        let err = validate_env(&evm, &tx).unwrap_err();
        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::AuthorizationListCreate
        ));
    }

    /// Intrinsic gas charges 25 000 per authorization regardless of the transaction type, so
    /// a MorphTx V2 needs no Morph-specific handling here (design doc 5.6).
    #[test]
    fn intrinsic_gas_charges_per_authorization_for_morph_tx_v2() {
        let evm = evm_with_spec(MorphHardfork::Celadon);
        let tx = v2_env_with(vec![
            sample_signed_authorization(),
            sample_signed_authorization(),
        ]);

        // 21_000 base + 2 × 25_000 per authorization.
        let (intrinsic, floor) = intrinsic_gas(&evm, &tx, Address::ZERO);
        assert_eq!(intrinsic, 71_000);
        assert_eq!(floor, 0, "Morph runs without the EIP-7623 calldata floor");
        assert_eq!(
            validate_intrinsic_gas(70_999, intrinsic, 0),
            Err(HandlerError::IntrinsicGasTooLow {
                required: 71_000,
                got: 70_999,
            })
        );
        assert_eq!(validate_intrinsic_gas(71_000, intrinsic, 0), Ok(()));
    }

    #[test]
    fn intrinsic_gas_uses_configured_gas_params() {
        let mut env = test_env(MorphHardfork::default(), MorphBlockEnv::default());
        env.version.gas_params.set(GasId::TxBaseStipend, 30_000);
        let evm = morph_evm(InMemoryDB::default(), env);
        let tx = MorphTxEnv {
            gas_limit: 25_000,
            kind: TxKind::Call(Address::ZERO),
            ..Default::default()
        };

        assert_eq!(intrinsic_gas(&evm, &tx, Address::ZERO).0, 30_000);
    }

    #[test]
    fn transfer_erc20_with_evm_keeps_state_on_post_call_validation_failure() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let original_balance = U256::from(50);

        let mut db = InMemoryDB::default();
        fund(&mut db, from, 1_000_000);
        insert_contract(&mut db, token, mutating_return_code(1, 0));
        db.insert_account_storage(&token, &U256::ZERO, &original_balance);
        let mut evm = evm_on(db, MorphHardfork::default(), MorphBlockEnv::default());

        let err = transfer_erc20_with_evm(
            &mut evm,
            &tx_env_of(from),
            from,
            to,
            token,
            U256::from(4),
            Some(original_balance),
        )
        .unwrap_err();

        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::TokenTransferFailed { .. }
        ));
        assert_eq!(
            evm.state().get_storage(&token, &U256::ZERO),
            Some(U256::from(1))
        );
    }

    #[test]
    fn transfer_erc20_with_evm_reverts_state_on_expected_balance_underflow() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");

        let mut db = InMemoryDB::default();
        fund(&mut db, from, 1_000_000);
        insert_contract(&mut db, token, mutating_return_code(1, 1));
        db.insert_account_storage(&token, &U256::ZERO, &U256::from(50));
        let mut evm = evm_on(db, MorphHardfork::default(), MorphBlockEnv::default());

        let err = transfer_erc20_with_evm(
            &mut evm,
            &tx_env_of(from),
            from,
            to,
            token,
            U256::from(1),
            Some(U256::ZERO),
        )
        .unwrap_err();

        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::TokenTransferFailed { .. }
        ));
        assert!(no_state_loaded(&evm));
    }

    #[test]
    fn transfer_erc20_with_slot_reverts_sender_on_recipient_overflow() {
        let from = address!("1000000000000000000000000000000000000001");
        let to = address!("2000000000000000000000000000000000000002");
        let token = address!("3000000000000000000000000000000000000003");
        let balance_slot = U256::from(7);
        let from_storage_slot = compute_mapping_slot_for_address(balance_slot, from);
        let to_storage_slot = compute_mapping_slot_for_address(balance_slot, to);

        let mut db = InMemoryDB::default();
        db.insert_account_info(&token, AccountInfo::default());
        db.insert_account_storage(&token, &from_storage_slot, &U256::from(10));
        db.insert_account_storage(&token, &to_storage_slot, &U256::MAX);
        let mut evm = evm_on(db, MorphHardfork::default(), MorphBlockEnv::default());

        let err = transfer_erc20_with_slot(&mut evm, from, to, token, U256::from(1), balance_slot)
            .unwrap_err();

        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::TokenTransferFailed { .. }
        ));
        assert_eq!(
            storage_of(&mut evm, token, from_storage_slot),
            U256::from(10)
        );
    }

    #[test]
    fn evm_call_balance_of_is_read_only() {
        let token = address!("3000000000000000000000000000000000000003");
        let account = address!("1000000000000000000000000000000000000001");
        let original_balance = U256::from(50);

        let mut db = InMemoryDB::default();
        insert_contract(&mut db, token, mutating_return_code(1, 42));
        db.insert_account_storage(&token, &U256::ZERO, &original_balance);
        let mut evm = evm_on(db, MorphHardfork::default(), MorphBlockEnv::default());

        assert!(evm_call_balance_of(&mut evm, &tx_env_of(account), token, account).is_err());
        assert_eq!(storage_of(&mut evm, token, U256::ZERO), original_balance);
    }

    /// Disabled fee charging must leave the caller balance untouched.
    #[test]
    fn calculate_caller_fee_is_short_circuited_by_disabled_fee_charge() {
        let balance = U256::from(1_000_000_000_000u128);
        let tx = MorphTxEnv {
            gas_limit: 21_000,
            gas_price: 1_000_000_000,
            value: U256::from(42u64),
            ..Default::default()
        };
        let l1_data_fee = U256::from(1_234u64);

        let new_balance =
            calculate_caller_fee_with_l1_cost(balance, &tx, 0, l1_data_fee, false, true).unwrap();
        assert_eq!(new_balance, balance);
    }

    #[test]
    fn token_fee_simulation_rejects_unregistered_token() {
        let caller = address!("1000000000000000000000000000000000000001");
        let mut db = InMemoryDB::default();
        fund(&mut db, caller, 1_000_000);

        let err = simulate_token_fee(db, caller, 65535).err().unwrap();

        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::TokenNotRegistered(65535)
        ));
    }

    #[test]
    fn token_fee_simulation_rejects_inactive_token() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;

        let mut db = InMemoryDB::default();
        fund(&mut db, caller, 1_000_000);
        insert_test_fee_token(&mut db, token_id, token, false);

        let err = simulate_token_fee(db, caller, token_id).err().unwrap();

        assert!(matches!(
            morph_error(&err),
            MorphInvalidTransaction::TokenNotActive(42)
        ));
    }

    #[test]
    fn token_fee_simulation_rejects_invalid_config() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;

        for (case, price_ratio, scale) in [
            ("zero price ratio", U256::ZERO, U256::from(1)),
            ("zero scale", U256::from(1), U256::ZERO),
        ] {
            let mut db = InMemoryDB::default();
            fund(&mut db, caller, 1_000_000);
            insert_test_fee_token_config(&mut db, token_id, token, true, price_ratio, scale);

            let err = simulate_token_fee(db, caller, token_id).err().unwrap();

            assert!(
                matches!(
                    morph_error(&err),
                    MorphInvalidTransaction::InvalidTokenConfig(42)
                ),
                "{case} must be rejected"
            );
        }
    }

    #[test]
    fn token_fee_simulation_does_not_load_caller_token_balance() {
        let caller = address!("1000000000000000000000000000000000000001");
        let token = address!("2000000000000000000000000000000000000002");
        let token_id = 42u16;
        let token_accessed = Arc::new(AtomicBool::new(false));
        let mut inner = InMemoryDB::default();
        fund(&mut inner, caller, 1_000_000);
        insert_test_fee_token(&mut inner, token_id, token, true);

        let db = TokenAccessTrackingDb {
            inner,
            token,
            token_accessed: Arc::clone(&token_accessed),
        };

        assert!(simulate_token_fee(Db::new(db), caller, token_id).is_ok());

        assert!(
            !token_accessed.load(Ordering::Relaxed),
            "simulation must not query the fee-token contract or its balance storage"
        );
    }

    const FEE_REFUND_TOKEN_ID: u16 = 1;
    const FEE_REFUND_GAS_LIMIT: u64 = 100_000;
    const FEE_REFUND_GAS_PRICE: u128 = 10;
    /// `gas_limit * effective_gas_price` (the L1 data fee is zero with an empty
    /// gas-price oracle), converted at scale 1 / price_ratio 1.
    const FEE_REFUND_TOKEN_FEE: u64 = 1_000_000;
    const FEE_REFUND_CALLER: Address = address!("1000000000000000000000000000000000000001");
    const FEE_REFUND_TOKEN: Address = address!("3000000000000000000000000000000000000003");
    const FEE_REFUND_BENEFICIARY: Address = address!("530000000000000000000000000000000000000a");
    /// Plain EOA target: the main frame does nothing beyond intrinsic gas.
    const FEE_REFUND_TARGET: Address = address!("4200000000000000000000000000000000000042");

    /// Minimal call-mode ERC20: `balance[addr]` lives at slot `uint256(addr)` (no
    /// keccak), so `CALLER`, `calldataload(4)` and `balanceOf`'s argument all name the
    /// same slot. Dispatch is on `CALLDATASIZE`:
    /// - 68 bytes => `transfer(address,uint256)`: SSTORE(caller, SLOAD(caller) - amount),
    ///   SSTORE(to, SLOAD(to) + amount), return `true`.
    /// - anything else => `balanceOf(address)`: return SLOAD(calldataload(4)).
    fn fee_refund_slotless_erc20_code() -> Bytes {
        Bytes::from(vec![
            0x36, // CALLDATASIZE
            0x60, 0x44, // PUSH1 68
            0x14, // EQ
            0x60, 0x13, // PUSH1 19 (transfer JUMPDEST)
            0x57, // JUMPI
            // balanceOf(address)
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD
            0x54, // SLOAD
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
            // transfer(address,uint256)
            0x5b, // JUMPDEST (pc 19)
            0x60, 0x24, // PUSH1 36
            0x35, // CALLDATALOAD  -> amount
            0x80, // DUP1          -> amount amount
            0x33, // CALLER        -> caller amount amount
            0x54, // SLOAD         -> bal_from amount amount
            0x03, // SUB           -> bal_from-amount amount
            0x33, // CALLER        -> caller new_from amount
            0x55, // SSTORE        -> amount
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD  -> to amount
            0x80, // DUP1          -> to to amount
            0x54, // SLOAD         -> bal_to to amount
            0x82, // DUP3          -> amount bal_to to amount
            0x01, // ADD           -> new_to to amount
            0x90, // SWAP1         -> to new_to amount
            0x55, // SSTORE        -> amount
            0x50, // POP
            0x60, 0x01, // PUSH1 1
            0x60, 0x00, // PUSH1 0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x60, 0x00, // PUSH1 0
            0xf3, // RETURN
        ])
    }

    fn fee_refund_balance_slot(account: Address) -> U256 {
        U256::from_be_bytes(account.into_word().0)
    }

    fn fee_refund_db(payer_token_balance: U256) -> InMemoryDB {
        let mut db = InMemoryDB::default();
        db.insert_account_info(&FEE_REFUND_CALLER, AccountInfo::default());
        insert_contract(&mut db, FEE_REFUND_TOKEN, fee_refund_slotless_erc20_code());
        db.insert_account_storage(
            &FEE_REFUND_TOKEN,
            &fee_refund_balance_slot(FEE_REFUND_CALLER),
            &payer_token_balance,
        );
        // `balanceSlot` word left at zero => call mode (`balance_slot == None`).
        insert_test_fee_token(&mut db, FEE_REFUND_TOKEN_ID, FEE_REFUND_TOKEN, true);
        db
    }

    fn fee_refund_block() -> MorphBlockEnv {
        MorphBlockEnv {
            basefee: U256::from(1),
            beneficiary: FEE_REFUND_BENEFICIARY,
            gas_limit: U256::from(30_000_000),
            ..Default::default()
        }
    }

    fn fee_refund_evm(db: InMemoryDB) -> MorphEvm<'static> {
        evm_on(db, MorphHardfork::default(), fee_refund_block())
    }

    fn fee_refund_tx() -> MorphTxEnv {
        MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            gas_limit: FEE_REFUND_GAS_LIMIT,
            gas_price: FEE_REFUND_GAS_PRICE,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            fee_token_id: Some(FEE_REFUND_TOKEN_ID),
            ..Default::default()
        }
    }

    /// Runs one call-mode token-fee MorphTx (plain call to an EOA) and returns
    /// `(gas_used, final refund applied to the main frame, payer token balance after
    /// reimbursement)`.
    fn fee_refund_run_token_fee_tx(payer_token_balance: U256) -> (u64, u64, U256) {
        let mut evm = fee_refund_evm(fee_refund_db(payer_token_balance));
        let result = transact(&mut evm, fee_refund_tx(), FEE_REFUND_CALLER)
            .expect("token-fee MorphTx must execute");
        assert!(result.status, "expected success, got {result:?}");

        // Sanity: the fee was charged in call mode and equals exactly FEE_REFUND_TOKEN_FEE.
        let info = result
            .ext
            .token_fee_info
            .expect("token fee info is reported");
        assert_eq!(
            info.balance_slot, None,
            "token must be registered in call mode"
        );
        assert_eq!(
            info.balance, payer_token_balance,
            "balanceOf must see the seeded balance"
        );
        assert_eq!(
            info.eth_to_token_amount(U256::from(
                FEE_REFUND_GAS_LIMIT as u128 * FEE_REFUND_GAS_PRICE
            )),
            U256::from(FEE_REFUND_TOKEN_FEE)
        );

        (
            result.tx_gas_used(),
            result.refunded,
            storage_of(
                &mut evm,
                FEE_REFUND_TOKEN,
                fee_refund_balance_slot(FEE_REFUND_CALLER),
            ),
        )
    }

    #[test]
    fn deduction_sstore_refund_reaches_transaction_gas() {
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let (gas, refund, balance) = fee_refund_run_token_fee_tx(fee);
        assert_eq!((gas, refund), (16_800, 4_200));
        assert_eq!(balance, fee - U256::from(168_000));
        let (gas, refund, balance) = fee_refund_run_token_fee_tx(fee + U256::from(1));
        assert_eq!((gas, refund), (21_000, 0));
        assert_eq!(balance, fee + U256::from(1) - U256::from(210_000));
    }

    #[test]
    fn balance_queries_reject_state_writes() {
        let mut db = fee_refund_db(U256::from(FEE_REFUND_TOKEN_FEE));
        insert_contract(&mut db, FEE_REFUND_TOKEN, mutating_return_code(1, 1));
        let mut evm = fee_refund_evm(db);
        assert!(
            evm_call_balance_of(
                &mut evm,
                &tx_env_of(FEE_REFUND_CALLER),
                FEE_REFUND_TOKEN,
                FEE_REFUND_CALLER
            )
            .is_err()
        );
    }

    /// Fee frames run under the environment the transaction's preparation built, so ORIGIN and
    /// GASPRICE are the outer transaction's.
    #[test]
    fn internal_calls_preserve_origin_and_effective_gas_price() {
        for (opcode, expected) in [
            (0x32, U256::from_be_slice(FEE_REFUND_CALLER.as_slice())),
            (0x3a, U256::from(3)),
        ] {
            let mut db = fee_refund_db(U256::from(FEE_REFUND_TOKEN_FEE));
            fund(&mut db, FEE_REFUND_CALLER, 10_000_000);
            insert_contract(&mut db, FEE_REFUND_TOKEN, code_returning(opcode));
            let mut evm = fee_refund_evm(db);
            let tx = MorphTxEnv {
                gas_price: 10,
                gas_priority_fee: Some(2),
                fee_token_id: Some(0),
                ..fee_refund_tx()
            };
            let mut request = TxRequest {
                envelope: &tx,
                tx: Recovered::new_unchecked(&tx, FEE_REFUND_CALLER),
                host: &mut evm,
                _non_exhaustive: (),
            };
            let prepared = prepare(&mut request).unwrap();

            assert_eq!(
                evm_call_balance_of(
                    &mut evm,
                    &prepared.tx_env,
                    FEE_REFUND_TOKEN,
                    FEE_REFUND_BENEFICIARY
                )
                .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn zero_token_transfer_does_not_call_the_contract() {
        let mut db = fee_refund_db(U256::ZERO);
        insert_contract(&mut db, FEE_REFUND_TOKEN, mutating_return_code(1, 0));
        let mut evm = fee_refund_evm(db);
        transfer_erc20_with_evm(
            &mut evm,
            &tx_env_of(FEE_REFUND_CALLER),
            FEE_REFUND_CALLER,
            FEE_REFUND_BENEFICIARY,
            FEE_REFUND_TOKEN,
            U256::ZERO,
            Some(U256::ZERO),
        )
        .unwrap();
        assert!(no_state_loaded(&evm));
    }

    /// Runs one transaction against `code` deployed at [`FEE_REFUND_TARGET`] and
    /// returns what the main frame returned. `fee_token_id` selects the call-mode
    /// token-fee path or the ordinary ETH-fee path.
    fn fee_refund_run_probe(code: Bytes, fee_token_id: Option<u16>) -> Bytes {
        let mut db = fee_refund_db(U256::from(FEE_REFUND_TOKEN_FEE));
        insert_contract(&mut db, FEE_REFUND_TARGET, code);
        fund(
            &mut db,
            FEE_REFUND_CALLER,
            (FEE_REFUND_GAS_LIMIT as u128 * FEE_REFUND_GAS_PRICE) as u64,
        );
        let tx = MorphTxEnv {
            tx_type: if fee_token_id.is_some() {
                MORPH_TX_TYPE_ID
            } else {
                0
            },
            fee_token_id,
            ..fee_refund_tx()
        };
        let result =
            transact(&mut fee_refund_evm(db), tx, FEE_REFUND_CALLER).expect("probe must execute");
        assert!(result.status, "expected success, got {result:?}");
        result.output
    }

    /// Minimal proxy: copies its calldata into memory and `DELEGATECALL`s
    /// `implementation`, the way mainnet's call-mode fee tokens reach theirs.
    fn delegating_proxy_code(implementation: Address) -> Bytes {
        Bytes::from(delegate_body(implementation))
    }

    fn delegate_body(implementation: Address) -> Vec<u8> {
        let mut code = vec![
            0x36, // CALLDATASIZE      (size)
            0x5f, // PUSH0             (offset)
            0x5f, // PUSH0             (destOffset)
            0x37, // CALLDATACOPY
            0x5f, // PUSH0             (retSize)
            0x5f, // PUSH0             (retOffset)
            0x36, // CALLDATASIZE      (argsSize)
            0x5f, // PUSH0             (argsOffset)
            0x73, // PUSH20 implementation
        ];
        code.extend_from_slice(implementation.as_slice());
        code.extend_from_slice(&[
            0x5a, // GAS
            0xf4, // DELEGATECALL
            0x3d, // RETURNDATASIZE    (size)
            0x5f, // PUSH0             (offset)
            0x5f, // PUSH0             (destOffset)
            0x3e, // RETURNDATACOPY
            0x50, // POP               (DELEGATECALL success flag)
            0x3d, // RETURNDATASIZE    (size)
            0x5f, // PUSH0             (offset)
            0xf3, // RETURN
        ]);
        code
    }

    /// Same ERC20, except `balanceOf` returns its result through the identity
    /// precompile, reading the precompile's arguments out of memory.
    fn fee_refund_precompile_erc20_code() -> Bytes {
        let mut code = vec![
            0x36, // CALLDATASIZE
            0x60, 0x44, // PUSH1 68
            0x14, // EQ
            0x60, 0x1e, // PUSH1 30 (transfer JUMPDEST)
            0x57, // JUMPI
            // balanceOf(address), answered by identity(mem[0..32])
            0x60, 0x04, // PUSH1 4
            0x35, // CALLDATALOAD
            0x54, // SLOAD
            0x5f, // PUSH0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32   (retSize)
            0x60, 0x20, // PUSH1 32   (retOffset)
            0x60, 0x20, // PUSH1 32   (argsSize)
            0x5f, // PUSH0            (argsOffset)
            0x60, 0x04, // PUSH1 4    (identity precompile)
            0x5a, // GAS
            0xfa, // STATICCALL
            0x50, // POP
            0x60, 0x20, // PUSH1 32   (size)
            0x60, 0x20, // PUSH1 32   (offset)
            0xf3, // RETURN
        ];
        assert_eq!(code.len(), 30, "the transfer JUMPDEST moved");
        code.extend_from_slice(&fee_refund_slotless_erc20_code()[19..]);
        Bytes::from(code)
    }

    /// A fee frame that calls a precompile must hand it its arguments out of the frame's memory.
    #[test]
    fn fee_token_frames_reach_a_precompile_through_memory() {
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let mut db = fee_refund_db(fee);
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            fee_refund_precompile_erc20_code(),
        );
        assert_eq!(fee_refund_run_fee_tx(db), fee_refund_run_token_fee_tx(fee));
    }

    /// Mainnet's call-mode fee tokens are proxies, so a fee frame's nested
    /// `DELEGATECALL` reads its calldata back out of the frame's memory.
    #[test]
    fn fee_token_frames_reach_a_delegating_proxy_implementation() {
        const IMPLEMENTATION: Address = address!("3000000000000000000000000000000000000004");
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let mut db = fee_refund_db(fee);
        insert_contract(&mut db, IMPLEMENTATION, fee_refund_slotless_erc20_code());
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            delegating_proxy_code(IMPLEMENTATION),
        );

        // Indistinguishable from the same transaction against an unproxied token:
        // the implementation saw exactly the calldata the fee frame wrote.
        assert_eq!(fee_refund_run_fee_tx(db), fee_refund_run_token_fee_tx(fee));
    }

    /// Runs the standard call-mode token-fee MorphTx over `db` and reports it the way
    /// [`fee_refund_run_token_fee_tx`] does.
    fn fee_refund_run_fee_tx(db: InMemoryDB) -> (u64, u64, U256) {
        let mut evm = fee_refund_evm(db);
        let result = transact(&mut evm, fee_refund_tx(), FEE_REFUND_CALLER)
            .expect("token-fee MorphTx must execute");
        assert!(result.status, "expected success, got {result:?}");
        (
            result.tx_gas_used(),
            result.refunded,
            storage_of(
                &mut evm,
                FEE_REFUND_TOKEN,
                fee_refund_balance_slot(FEE_REFUND_CALLER),
            ),
        )
    }

    /// go-ethereum allocates a fresh `Memory` for every interpreter run
    /// (`core/vm/interpreter.go`), so a transaction's frame always starts on zeroed
    /// memory, even after the fee-token frames ran.
    #[test]
    fn fee_token_frames_do_not_leak_memory_into_the_main_frame() {
        // `MSIZE` and `MLOAD(0)`, each returned as the frame's 32-byte output.
        for probe in [
            vec![0x59, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3],
            vec![0x5f, 0x51, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3],
        ] {
            let code = Bytes::from(probe);
            assert_eq!(
                U256::from_be_slice(&fee_refund_run_probe(code.clone(), None)),
                U256::ZERO,
                "ETH-fee control"
            );
            assert_eq!(
                U256::from_be_slice(&fee_refund_run_probe(code, Some(FEE_REFUND_TOKEN_ID))),
                U256::ZERO,
                "a token-fee main frame must start on zeroed memory too"
            );
        }
    }

    /// `GAS; <code>; GAS; SWAP1; SUB; PUSH0; MSTORE; PUSH1 32; PUSH0; RETURN`: returns the gas
    /// `measured` consumed, so a difference in its warm/cold pricing is visible in the output.
    fn gas_meter(measured: &[u8]) -> Vec<u8> {
        let mut code = vec![0x5a]; // GAS
        code.extend_from_slice(measured);
        code.extend_from_slice(&[
            0x5a, // GAS
            0x90, // SWAP1
            0x03, // SUB
            0x5f, // PUSH0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x5f, // PUSH0
            0xf3, // RETURN
        ]);
        code
    }

    /// The deduction's `balanceOf`/`transfer` frames warm the token. go-ethereum's
    /// `StateDB.Prepare` drops that before the main transaction, so the main frame must pay the
    /// same cold-access price it pays when the fee is in ETH.
    #[test]
    fn main_frame_sees_the_fee_token_account_cold() {
        let mut measured = vec![0x73]; // PUSH20 token
        measured.extend_from_slice(FEE_REFUND_TOKEN.as_slice());
        measured.extend_from_slice(&[0x31, 0x50]); // BALANCE; POP
        let code = Bytes::from(gas_meter(&measured));

        let eth = U256::from_be_slice(&fee_refund_run_probe(code.clone(), None));
        let token = U256::from_be_slice(&fee_refund_run_probe(code, Some(FEE_REFUND_TOKEN_ID)));

        assert_eq!(token, eth);
        assert_eq!(token, U256::from(2_600 + 3 + 2 + 2));
    }

    /// `to == token` runs [`token_slot_probe_code`] as the main frame: calling with empty
    /// calldata measures an `SLOAD` of the payer's balance slot, which the deduction warmed.
    fn token_slot_probe_code(payer: Address) -> Bytes {
        let mut measure = vec![0x73]; // PUSH20 payer
        measure.extend_from_slice(payer.as_slice());
        measure.extend_from_slice(&[0x54, 0x50]); // SLOAD; POP
        let meter = gas_meter(&measure);

        // CALLDATASIZE; ISZERO; PUSH2 <probe>; JUMPI; <ERC20 relocated after the prefix>
        let erc20 = relocated_erc20(7);
        let probe_offset = 7 + erc20.len();

        let mut code = vec![
            0x36, // CALLDATASIZE
            0x15, // ISZERO
            0x61,
            (probe_offset >> 8) as u8,
            probe_offset as u8, // PUSH2 <probe>
            0x57,               // JUMPI
            0x00,               // padding, replaced below
        ];
        code.pop();
        code.push(0x5b); // JUMPDEST so that the ERC20 body starts on a valid position
        code.extend_from_slice(&erc20);
        code.push(0x5b); // JUMPDEST at <probe>
        code.extend_from_slice(&meter);
        Bytes::from(code)
    }

    /// [`fee_refund_slotless_erc20_code`], moved to start at `offset`.
    fn relocated_erc20(offset: usize) -> Vec<u8> {
        let mut code = fee_refund_slotless_erc20_code().to_vec();
        // The only absolute jump target in the body is the `PUSH1 19` at index 5.
        code[5] = u8::try_from(19 + offset).unwrap();
        code
    }

    /// Runs a token-fee or ETH-fee call to the token itself and returns the probe's output.
    fn run_token_slot_probe(fee_token_id: Option<u16>) -> U256 {
        let fee = U256::from(FEE_REFUND_TOKEN_FEE * 2);
        let mut db = fee_refund_db(fee);
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            token_slot_probe_code(FEE_REFUND_CALLER),
        );
        fund(
            &mut db,
            FEE_REFUND_CALLER,
            (FEE_REFUND_GAS_LIMIT as u128 * FEE_REFUND_GAS_PRICE) as u64,
        );
        let tx = MorphTxEnv {
            tx_type: if fee_token_id.is_some() {
                MORPH_TX_TYPE_ID
            } else {
                0
            },
            kind: TxKind::Call(FEE_REFUND_TOKEN),
            fee_token_id,
            ..fee_refund_tx()
        };
        let result =
            transact(&mut fee_refund_evm(db), tx, FEE_REFUND_CALLER).expect("probe must execute");
        assert!(result.status, "expected success, got {result:?}");
        U256::from_be_slice(&result.output)
    }

    #[test]
    fn main_frame_sees_the_fee_token_slot_cold() {
        let eth = run_token_slot_probe(None);
        let token = run_token_slot_probe(Some(FEE_REFUND_TOKEN_ID));
        assert_eq!(token, eth);
        assert_eq!(token, U256::from(2_100 + 3 + 2 + 2));
    }

    /// `TSTORE(0, 1)` before forwarding `transfer` to an ERC20 implementation, and a report of
    /// `TLOAD(0)` when called with no calldata.
    fn transient_writing_token_code(implementation: Address) -> Bytes {
        let delegate = delegate_body(implementation);
        // Layout: dispatch | delegate | transfer prefix | report.
        let dispatch_len = 14;
        let delegate_start = dispatch_len;
        let transfer_prefix = delegate_start + 1 + delegate.len();
        let report = transfer_prefix + 8;

        let mut code = vec![
            0x36, // CALLDATASIZE
            0x60,
            0x44, // PUSH1 68
            0x14, // EQ
            0x60,
            u8::try_from(transfer_prefix).unwrap(), // PUSH1 <transfer prefix>
            0x57,                                   // JUMPI
            0x36,                                   // CALLDATASIZE
            0x15,                                   // ISZERO
            0x60,
            u8::try_from(report).unwrap(), // PUSH1 <report>
            0x57,                          // JUMPI
            0x60,
            u8::try_from(delegate_start).unwrap(), // PUSH1 <delegate>
            0x56,                                  // JUMP
        ];
        code.truncate(dispatch_len);
        code.push(0x5b); // JUMPDEST <delegate>
        code.extend_from_slice(&delegate);
        assert_eq!(code.len(), transfer_prefix);
        code.extend_from_slice(&[
            0x5b, // JUMPDEST <transfer prefix>
            0x60,
            0x01, // PUSH1 1
            0x5f, // PUSH0
            0x5d, // TSTORE
            0x60,
            u8::try_from(delegate_start).unwrap(), // PUSH1 <delegate>
            0x56,                                  // JUMP
        ]);
        assert_eq!(code.len(), report);
        code.extend_from_slice(&[
            0x5b, // JUMPDEST <report>
            0x5f, // PUSH0
            0x5c, // TLOAD
            0x5f, // PUSH0
            0x52, // MSTORE
            0x60, 0x20, // PUSH1 32
            0x5f, // PUSH0
            0xf3, // RETURN
        ]);
        Bytes::from(code)
    }

    /// The deduction's transient writes must not leak into the main transaction:
    /// `StateDB.Prepare` resets transient storage in go-ethereum.
    #[test]
    fn main_frame_does_not_see_the_fee_frames_transient_storage() {
        const IMPLEMENTATION: Address = address!("3000000000000000000000000000000000000004");
        let fee = U256::from(FEE_REFUND_TOKEN_FEE * 2);
        let mut db = fee_refund_db(fee);
        insert_contract(&mut db, IMPLEMENTATION, fee_refund_slotless_erc20_code());
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            transient_writing_token_code(IMPLEMENTATION),
        );
        let tx = MorphTxEnv {
            kind: TxKind::Call(FEE_REFUND_TOKEN),
            ..fee_refund_tx()
        };

        let result =
            transact(&mut fee_refund_evm(db), tx, FEE_REFUND_CALLER).expect("probe must execute");

        assert!(result.status, "expected success, got {result:?}");
        assert_eq!(U256::from_be_slice(&result.output), U256::ZERO);
    }

    /// Forwards to `implementation`, emitting `LOG1(topic)` first when called with `transfer`
    /// calldata. `balanceOf` runs in a static frame, where a log would halt it.
    fn logging_token_code(implementation: Address, topic: u8) -> Bytes {
        const DELEGATE: u8 = 13;
        let mut code = vec![
            0x36, // CALLDATASIZE
            0x60, 0x44, // PUSH1 68
            0x14, // EQ
            0x15, // ISZERO
            0x60, DELEGATE, // PUSH1 <delegate>
            0x57,     // JUMPI
            0x60, topic, // PUSH1 topic
            0x5f,  // PUSH0
            0x5f,  // PUSH0
            0xa1,  // LOG1
            0x5b,  // JUMPDEST <delegate>
        ];
        assert_eq!(code.len(), DELEGATE as usize + 1);
        code.extend_from_slice(&delegate_body(implementation));
        Bytes::from(code)
    }

    /// `LOG1(topic)`, then `REVERT` when `reverts`.
    fn logging_target_code(topic: u8, reverts: bool) -> Bytes {
        let mut code = vec![
            0x60, topic, // PUSH1 topic
            0x5f,  // PUSH0
            0x5f,  // PUSH0
            0xa1,  // LOG1
        ];
        if reverts {
            code.extend_from_slice(&[0x5f, 0x5f, 0xfd]); // PUSH0; PUSH0; REVERT
        } else {
            code.push(0x00); // STOP
        }
        Bytes::from(code)
    }

    fn logged_topic(log: &Log) -> u8 {
        log.topics()[0].0[31]
    }

    /// Runs a token-fee transaction whose token logs `0xaa` and whose target logs `0xbb`.
    fn run_logging_token_fee_tx(reverts: bool) -> Vec<Log> {
        const IMPLEMENTATION: Address = address!("3000000000000000000000000000000000000004");
        let fee = U256::from(FEE_REFUND_TOKEN_FEE);
        let mut db = fee_refund_db(fee);
        insert_contract(&mut db, IMPLEMENTATION, fee_refund_slotless_erc20_code());
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            logging_token_code(IMPLEMENTATION, 0xaa),
        );
        insert_contract(
            &mut db,
            FEE_REFUND_TARGET,
            logging_target_code(0xbb, reverts),
        );

        let result = transact(&mut fee_refund_evm(db), fee_refund_tx(), FEE_REFUND_CALLER)
            .expect("token-fee MorphTx must execute");
        assert_eq!(result.status, !reverts);
        result.logs
    }

    /// Logs read in the order things happened: deduction, main frame, refund.
    #[test]
    fn fee_logs_surround_the_main_frame_logs() {
        let logs = run_logging_token_fee_tx(false);
        assert_eq!(
            logs.iter().map(logged_topic).collect::<Vec<_>>(),
            vec![0xaa, 0xbb, 0xaa]
        );
    }

    /// go-ethereum's fee logs live outside the state snapshot, so a reverted main frame keeps
    /// them while dropping its own.
    #[test]
    fn fee_logs_survive_a_reverted_main_frame() {
        let logs = run_logging_token_fee_tx(true);
        assert_eq!(
            logs.iter().map(logged_topic).collect::<Vec<_>>(),
            vec![0xaa, 0xaa]
        );
    }

    /// Records every frame and instruction an inspector is shown.
    #[derive(Debug, Default)]
    struct CountingInspector {
        calls: usize,
        steps: usize,
        logs: usize,
    }

    impl Inspector<MorphEvmTypes> for CountingInspector {
        fn step(&mut self, _interp: &mut Interpreter<'_, '_, MorphEvmTypes>) {
            self.steps += 1;
        }

        fn log(&mut self, _log: &Log, _host: &mut MorphEvm<'_>) {
            self.logs += 1;
        }

        fn call(
            &mut self,
            _interp: &mut Interpreter<'_, '_, MorphEvmTypes>,
            _message: &mut Message<MorphEvmTypes>,
        ) -> Option<MessageResult<MorphEvmTypes>> {
            self.calls += 1;
            None
        }
    }

    /// Runs the logging token-fee transaction with `inspector` installed.
    fn inspect_logging_tx(inspector: &mut dyn Inspector<MorphEvmTypes>) {
        const IMPLEMENTATION: Address = address!("3000000000000000000000000000000000000004");
        let mut db = fee_refund_db(U256::from(FEE_REFUND_TOKEN_FEE));
        insert_contract(&mut db, IMPLEMENTATION, fee_refund_slotless_erc20_code());
        insert_contract(
            &mut db,
            FEE_REFUND_TOKEN,
            logging_token_code(IMPLEMENTATION, 0xaa),
        );
        insert_contract(&mut db, FEE_REFUND_TARGET, logging_target_code(0xbb, false));
        let mut evm = fee_refund_evm(db);
        let tx = Recovered::new_unchecked(fee_refund_tx(), FEE_REFUND_CALLER);
        let result = evm
            .transact_with_inspector(&tx, inspector)
            .expect("token-fee MorphTx must execute")
            .discard();
        assert!(result.status, "expected success, got {result:?}");
    }

    /// Tracers see a token-fee transaction the way go-ethereum presents it: the fee frames are
    /// system calls, and the only frame left is the transaction's own.
    #[test]
    fn inspectors_do_not_see_fee_frames() {
        let mut filtered = crate::FeeFrameFilter::new(CountingInspector::default());
        inspect_logging_tx(&mut filtered);
        let filtered = filtered.into_inner();

        assert_eq!(filtered.calls, 1, "only the main frame is a traced call");
        assert_eq!(filtered.logs, 1, "only the main frame's log is traced");
        // The main frame is `PUSH1 PUSH0 PUSH0 LOG1 STOP`.
        assert_eq!(filtered.steps, 5);

        // Control: the fee frames really do run, and an unfiltered inspector sees them.
        let mut unfiltered = CountingInspector::default();
        inspect_logging_tx(&mut unfiltered);
        assert!(unfiltered.calls > filtered.calls);
        assert!(unfiltered.logs > filtered.logs);
    }

    // =========================================================================
    // Native-fee and L1 message settlement
    // =========================================================================

    #[test]
    fn eth_fee_pays_the_whole_fee_to_the_beneficiary() {
        let caller = FEE_REFUND_CALLER;
        let mut db = InMemoryDB::default();
        fund(&mut db, caller, 10_000_000);
        let mut evm = evm_on(
            db,
            MorphHardfork::default(),
            MorphBlockEnv {
                basefee: U256::from(5),
                beneficiary: FEE_REFUND_BENEFICIARY,
                gas_limit: U256::from(30_000_000),
                ..Default::default()
            },
        );
        let tx = MorphTxEnv {
            gas_limit: 100_000,
            gas_price: 10,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            ..Default::default()
        };

        let result = transact(&mut evm, tx, caller).unwrap();

        assert!(result.status);
        assert_eq!(result.tx_gas_used(), 21_000);
        let caller_after = evm
            .state_mut()
            .account_info_untracked(&caller)
            .unwrap()
            .unwrap();
        assert_eq!(caller_after.balance, U256::from(10_000_000 - 210_000u64));
        assert_eq!(caller_after.nonce, 1);
        let beneficiary = evm
            .state_mut()
            .account_info_untracked(&FEE_REFUND_BENEFICIARY)
            .unwrap()
            .unwrap();
        assert_eq!(
            beneficiary.balance,
            U256::from(210_000u64),
            "Morph burns no base fee"
        );
    }

    #[test]
    fn eth_fee_requires_balance_for_the_max_fee_and_the_l1_fee() {
        let caller = FEE_REFUND_CALLER;
        let mut db = InMemoryDB::default();
        // Enough for the effective fee, not for the max fee.
        fund(&mut db, caller, 1_000_000);
        let mut evm = evm_on(db, MorphHardfork::default(), MorphBlockEnv::default());
        let tx = MorphTxEnv {
            tx_type: EIP1559_TX_TYPE,
            chain_id: Some(1),
            gas_limit: 100_000,
            gas_price: 11,
            gas_priority_fee: Some(1),
            kind: TxKind::Call(FEE_REFUND_TARGET),
            ..Default::default()
        };

        assert!(matches!(
            transact(&mut evm, tx, caller),
            Err(HandlerError::InsufficientFunds { .. })
        ));
    }

    /// `PUSH0 PUSH0 SSTORE STOP` against a set slot 0: clears it, earning a refund.
    fn storage_clearing_code() -> Bytes {
        Bytes::from_static(&[0x5f, 0x5f, 0x55, 0x00])
    }

    #[test]
    fn l1_message_pays_no_fees_and_keeps_no_refund() {
        let caller = FEE_REFUND_CALLER;
        let mut db = InMemoryDB::default();
        insert_contract(&mut db, FEE_REFUND_TARGET, storage_clearing_code());
        db.insert_account_storage(&FEE_REFUND_TARGET, &U256::ZERO, &U256::from(1));
        let mut evm = evm_on(
            db,
            MorphHardfork::default(),
            MorphBlockEnv {
                beneficiary: FEE_REFUND_BENEFICIARY,
                ..Default::default()
            },
        );
        let tx = MorphTxEnv {
            tx_type: L1_TX_TYPE_ID,
            gas_limit: 100_000,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            // A queue index, unrelated to the sender's account nonce.
            nonce: 77,
            ..Default::default()
        };

        let result = transact(&mut evm, tx, caller).unwrap();

        assert!(result.status);
        assert_eq!(result.refunded, 0, "L1 messages keep no refund");
        // 21_000 + PUSH0 + PUSH0 + SSTORE (cold 2_100 + 2_900).
        assert_eq!(result.tx_gas_used(), 21_000 + 2 + 2 + 5_000);
        assert_eq!(result.ext.l1_data_fee, U256::ZERO);
        let sender = evm
            .state_mut()
            .account_info_untracked(&caller)
            .unwrap()
            .unwrap();
        assert_eq!(sender.nonce, 1, "the account nonce advances regardless");
        assert!(
            evm.state_mut()
                .account_info_untracked(&FEE_REFUND_BENEFICIARY)
                .unwrap()
                .is_none(),
            "the fee recipient is untouched"
        );
    }

    /// An L1 message prepaid on L1 must run even if its gas limit does not cover intrinsic gas.
    #[test]
    fn l1_message_below_intrinsic_gas_consumes_its_gas_limit() {
        let caller = FEE_REFUND_CALLER;
        let mut evm = evm_with_spec(MorphHardfork::default());
        let tx = MorphTxEnv {
            tx_type: L1_TX_TYPE_ID,
            gas_limit: 20_000,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            ..Default::default()
        };

        let result = transact(&mut evm, tx, caller).unwrap();

        assert_eq!(result.tx_gas_used(), 20_000);
    }

    #[test]
    fn registry_only_accepts_eip7702_from_prague() {
        let caller = FEE_REFUND_CALLER;
        let tx = MorphTxEnv {
            tx_type: EIP7702_TX_TYPE,
            chain_id: Some(1),
            gas_limit: 100_000,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            authorization_list: vec![sample_signed_authorization()],
            ..Default::default()
        };

        let mut cancun = evm_with_spec(MorphHardfork::Morph203);
        assert!(matches!(
            transact(&mut cancun, tx.clone(), caller),
            Err(HandlerError::UnsupportedTransactionType(EIP7702_TX_TYPE))
        ));

        let mut prague = evm_with_spec(MorphHardfork::Viridian);
        assert!(transact(&mut prague, tx, caller).is_ok());
    }

    /// `<opcode>; STOP`, for an opcode that takes no arguments.
    fn opcode_probe(opcode: u8) -> Bytes {
        Bytes::from(vec![opcode, 0x00])
    }

    /// Runs `code` as the main frame and returns whether it succeeded and its gas used.
    fn run_opcode_probe(spec: MorphHardfork, code: Bytes) -> (bool, u64) {
        let mut db = InMemoryDB::default();
        insert_contract(&mut db, FEE_REFUND_TARGET, code);
        let mut evm = evm_on(db, spec, MorphBlockEnv::default());
        let tx = MorphTxEnv {
            gas_limit: 100_000,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            ..Default::default()
        };
        let result = transact(&mut evm, tx, FEE_REFUND_CALLER).unwrap();
        (result.status, result.tx_gas_used())
    }

    /// SELFDESTRUCT, BLOBHASH and BLOBBASEFEE halt on Morph, consuming the whole gas limit.
    #[test]
    fn disabled_opcodes_halt_and_consume_all_gas() {
        for code in [
            // PUSH0; SELFDESTRUCT
            Bytes::from_static(&[0x5f, 0xff]),
            // PUSH0; BLOBHASH
            Bytes::from_static(&[0x5f, 0x49, 0x00]),
            opcode_probe(0x4a), // BLOBBASEFEE
        ] {
            assert_eq!(
                run_opcode_probe(MorphHardfork::Celadon, code.clone()),
                (false, 100_000),
                "{code}"
            );
        }
    }

    /// BLOCKHASH answers from the chain id and number rather than the header chain.
    #[test]
    fn blockhash_uses_morph_derivation() {
        // PUSH4 2662437; BLOCKHASH; PUSH0; MSTORE; PUSH1 32; PUSH0; RETURN
        let code = alloy_primitives::bytes!("630028a025405f5260205ff3");
        let mut db = InMemoryDB::default();
        insert_contract(&mut db, FEE_REFUND_TARGET, code);
        let env = morph_evm_env(
            MorphHardfork::default(),
            2818,
            30_000_000,
            MorphBlockEnv {
                number: U256::from(2_662_438u64),
                ..Default::default()
            },
        );
        let mut evm = morph_evm(db, env);
        let tx = MorphTxEnv {
            gas_limit: 100_000,
            kind: TxKind::Call(FEE_REFUND_TARGET),
            ..Default::default()
        };

        let result = transact(&mut evm, tx, FEE_REFUND_CALLER).unwrap();

        assert!(result.status, "{result:?}");
        // Golden vector captured from geth debug_traceTransaction for block 2662438.
        assert_eq!(
            B256::from_slice(&result.output),
            b256!("6d2426d9b8d6f63ec1a38d3a4e7b88f318ebe9f0d837c4781852168c5bd2678e")
        );
    }
}

/// Alt-token refund rounding, gated on Celadon.
///
/// Uses the registry's direct-slot path so the arithmetic is the only variable;
/// the call path routes the same `token_amount_required` through an ERC20
/// `transfer`, which [`tests`] already covers.
#[cfg(test)]
mod refund_rounding_tests {
    use super::*;
    use crate::{
        L2_TOKEN_REGISTRY_ADDRESS, MorphBlockEnv, TokenFeeInfo, compute_mapping_slot,
        evm::{morph_evm, morph_evm_env},
    };
    use alloy_consensus::transaction::Recovered;
    use alloy_primitives::{TxKind, address};
    use evm2::evm::{AccountInfo, InMemoryDB};

    const CALLER: Address = address!("1000000000000000000000000000000000000001");
    const BENEFICIARY: Address = address!("2000000000000000000000000000000000000002");
    const TOKEN: Address = address!("3000000000000000000000000000000000000003");
    const BALANCE_SLOT: U256 = U256::from_limbs([7, 0, 0, 0]);
    /// A `price_ratio` of 3 against a `scale` of 1 makes two of every three wei
    /// an inexact conversion, which is what the two roundings disagree about.
    const PRICE_RATIO: u64 = 3;
    const FEE_TOKEN_ID: u16 = 1;
    const VAULT_BALANCE: u64 = 1_000_000;
    const STARTING_BALANCE: u64 = 1_000_000;

    fn slot_of(account: Address) -> U256 {
        compute_mapping_slot_for_address(BALANCE_SLOT, account)
    }

    fn token_balance(evm: &mut MorphEvm<'_>, account: Address) -> U256 {
        evm.state_mut()
            .storage_slot_untracked(&TOKEN, &slot_of(account))
            .expect("read token balance")
    }

    fn ceil_to_token(eth_amount: u64) -> U256 {
        TokenFeeInfo {
            price_ratio: U256::from(PRICE_RATIO),
            scale: U256::from(1u64),
            ..Default::default()
        }
        .eth_to_token_amount(U256::from(eth_amount))
    }

    fn block() -> MorphBlockEnv {
        MorphBlockEnv {
            beneficiary: BENEFICIARY,
            basefee: U256::ZERO,
            gas_limit: U256::from(30_000_000),
            ..Default::default()
        }
    }

    // -------------------------------------------------------------------------
    // Whole-transaction coverage: deduction, call, refund
    // -------------------------------------------------------------------------

    /// Registers the fee token in the L2TokenRegistry on the direct-slot path.
    ///
    /// `balanceSlot` is stored one-based, so zero there means "call mode".
    fn register_slot_mode_token(db: &mut InMemoryDB) {
        let mut token_id_bytes = [0u8; 32];
        token_id_bytes[30..32].copy_from_slice(&FEE_TOKEN_ID.to_be_bytes());
        let base = compute_mapping_slot(U256::from(151), &token_id_bytes);

        let mut put = |slot: U256, value: U256| {
            db.insert_account_storage(&L2_TOKEN_REGISTRY_ADDRESS, &slot, &value);
        };
        put(base, U256::from_be_bytes(TOKEN.into_word().0));
        put(base + U256::from(1), BALANCE_SLOT + U256::from(1));
        let mut status = [0u8; 32];
        status[30] = 18; // decimals
        status[31] = 1; // isActive
        put(base + U256::from(2), U256::from_be_bytes(status));
        put(base + U256::from(3), U256::from(1u64)); // scale
        put(
            compute_mapping_slot(U256::from(153), &token_id_bytes),
            U256::from(PRICE_RATIO),
        );
    }

    /// A funded caller with the fee token registered on the direct-slot path.
    fn slot_mode_evm(spec: MorphHardfork) -> MorphEvm<'static> {
        let mut db = InMemoryDB::default();
        // Not empty, so that EIP-161 keeps the touched token when the transaction commits.
        db.insert_account_info(&TOKEN, AccountInfo::default().with_nonce(1));
        db.insert_account_info(&CALLER, AccountInfo::default());
        register_slot_mode_token(&mut db);
        db.insert_account_storage(&TOKEN, &slot_of(CALLER), &U256::from(STARTING_BALANCE));
        db.insert_account_storage(&TOKEN, &slot_of(BENEFICIARY), &U256::ZERO);

        morph_evm(db, morph_evm_env(spec, 1, 30_000_000, block()))
    }

    /// `calldata_len` bytes of non-zero calldata shift the transaction's gas
    /// cost, which is what moves `gas_used` through its remainders modulo
    /// `PRICE_RATIO`.
    fn fee_tx(gas_limit: u64, calldata_len: usize) -> MorphTxEnv {
        MorphTxEnv {
            tx_type: MORPH_TX_TYPE_ID,
            kind: TxKind::Call(Address::repeat_byte(0x0e)),
            gas_limit,
            gas_price: 1,
            data: Bytes::from(vec![0x01; calldata_len]),
            fee_token_id: Some(FEE_TOKEN_ID),
            ..Default::default()
        }
    }

    /// Runs a whole token-fee transaction and reports
    /// `(net token spend, gas used)`.
    fn net_token_spend(spec: MorphHardfork, gas_limit: u64, calldata_len: usize) -> (U256, u64) {
        let mut evm = slot_mode_evm(spec);
        let tx = Recovered::new_unchecked(fee_tx(gas_limit, calldata_len), CALLER);
        let result = evm
            .transact(&tx)
            .expect("token-fee MorphTx must execute")
            .commit();
        assert!(result.status, "expected success, got {result:?}");

        (
            U256::from(STARTING_BALANCE) - token_balance(&mut evm, CALLER),
            result.tx_gas_used(),
        )
    }

    /// The deduction must hand the refund the numerator its rounding-up
    /// overcharged.
    ///
    /// Asserted directly, because whether the credit changes the refunded amount
    /// depends on how prepaid and remaining gas land modulo `PRICE_RATIO` — an
    /// end-to-end assertion alone passes with the credit stuck at zero.
    #[test]
    fn deduction_records_the_rounding_credit_for_the_refund() {
        for (gas_limit, want_credit) in [(99_999u64, 0u64), (100_000, 2), (100_001, 1)] {
            let mut evm = slot_mode_evm(MorphHardfork::Celadon);
            let tx = fee_tx(gas_limit, 0);
            let tx_env = TxEnvExt {
                origin: CALLER,
                gas_price: U256::from(1),
                ..TxEnvExt::default()
            };
            let charged = deduct_token_fee(&mut evm, &tx, CALLER, &tx_env, FEE_TOKEN_ID)
                .unwrap_or_else(|err| panic!("deduction must succeed: {err}"));

            // The prepaid fee is `gas_limit` at price 1, so the credit is what
            // rounding `gas_limit / PRICE_RATIO` up left unused.
            assert_eq!(
                charged.token_fee.unwrap().rounding_credit,
                U256::from(want_credit),
                "gas_limit {gas_limit}"
            );
        }
    }

    /// From Celadon on, deduction and refund together charge exactly
    /// `ceil(net ETH fee)` in token units, whatever the prepaid amount rounded to.
    ///
    /// The sweep covers every remainder `gas_used` and the prepaid amount can
    /// have modulo `PRICE_RATIO`; the credit only changes the refund for some of
    /// those combinations, and the final assertion holds the sweep to covering
    /// them.
    #[test]
    fn celadon_charges_the_ceiling_of_the_net_fee_end_to_end() {
        let mut seen_remainders = [false; PRICE_RATIO as usize];
        for calldata_len in 0..PRICE_RATIO as usize {
            for gas_limit in 100_000u64..100_000 + PRICE_RATIO {
                let (spent, gas_used) =
                    net_token_spend(MorphHardfork::Celadon, gas_limit, calldata_len);
                assert_eq!(
                    spent,
                    ceil_to_token(gas_used),
                    "gas_limit {gas_limit}, calldata_len {calldata_len}, gas_used {gas_used}"
                );
                seen_remainders[(gas_used % PRICE_RATIO) as usize] = true;
            }
        }
        assert!(
            seen_remainders.iter().all(|seen| *seen),
            "the sweep must cover every gas_used remainder modulo {PRICE_RATIO}, \
             otherwise it misses the cases where the credit changes the refund"
        );
    }

    /// The pre-Celadon rule under-collects, so the fork gate is load-bearing.
    ///
    /// Rounding both halves up independently cancels out whenever the prepaid
    /// amount and the refund leave the same remainder, so the shortfall shows on
    /// only some transactions: the assertion is that at least one swept
    /// transaction is short, and that none over-collects.
    #[test]
    fn pre_celadon_under_collects_end_to_end() {
        let mut short = 0;
        for calldata_len in 0..PRICE_RATIO as usize {
            for gas_limit in 100_000u64..100_000 + PRICE_RATIO {
                let (spent, gas_used) =
                    net_token_spend(MorphHardfork::Jade, gas_limit, calldata_len);
                let ceiling = ceil_to_token(gas_used);
                assert!(
                    spent <= ceiling,
                    "pre-Celadon must never collect more than the ceiling of the net fee, \
                     gas_limit {gas_limit}, calldata_len {calldata_len}: spent {spent} > {ceiling}"
                );
                if spent < ceiling {
                    short += 1;
                }
            }
        }
        assert!(
            short > 0,
            "pre-Celadon must under-collect on at least one swept transaction, \
             otherwise this test cannot tell the two rules apart"
        );
    }

    // -------------------------------------------------------------------------
    // The refund step in isolation
    // -------------------------------------------------------------------------

    /// An EVM parked right before the refund, holding the state the deduction phase would
    /// have left behind: the fee already in the vault.
    fn evm_after_deduction(spec: MorphHardfork) -> MorphEvm<'static> {
        let mut db = InMemoryDB::default();
        db.insert_account_info(&TOKEN, AccountInfo::default());
        db.insert_account_storage(&TOKEN, &slot_of(BENEFICIARY), &U256::from(VAULT_BALANCE));
        db.insert_account_storage(&TOKEN, &slot_of(CALLER), &U256::ZERO);

        morph_evm(db, morph_evm_env(spec, 1, 30_000_000, block()))
    }

    /// Refunds `gas_left` worth of gas at a gas price of 1, so the ETH to refund equals the
    /// gas left and the arithmetic reads directly.
    fn refund(evm: &mut MorphEvm<'_>, gas_left: u64, rounding_credit: U256) {
        let tx_env = TxEnvExt {
            origin: CALLER,
            gas_price: U256::from(1),
            ..TxEnvExt::default()
        };
        reimburse_token_fee(
            evm,
            &tx_env,
            CALLER,
            U256::from(1),
            gas_left,
            AppliedTokenFee {
                info: TokenFeeInfo {
                    token_address: TOKEN,
                    is_active: true,
                    price_ratio: U256::from(PRICE_RATIO),
                    scale: U256::from(1u64),
                    caller: CALLER,
                    balance: U256::ZERO,
                    balance_slot: Some(BALANCE_SLOT),
                    ..Default::default()
                },
                rounding_credit,
                refund_counter: 0,
            },
        )
        .expect("refund must not fail");
    }

    /// Refunds `gas_left` worth of gas and reports what reached the caller.
    fn refund_with(spec: MorphHardfork, gas_left: u64, rounding_credit: U256) -> U256 {
        let mut evm = evm_after_deduction(spec);
        refund(&mut evm, gas_left, rounding_credit);
        token_balance(&mut evm, CALLER)
    }

    /// Before Celadon both halves of the fee round up independently. That
    /// under-collects, but it is mainnet's history and must not move.
    #[test]
    fn pre_celadon_refund_rounds_up() {
        // ceil(4 / 3) = 2, whatever credit the deduction recorded.
        assert_eq!(
            refund_with(MorphHardfork::Jade, 4, U256::ZERO),
            U256::from(2u64)
        );
        assert_eq!(
            refund_with(MorphHardfork::Jade, 4, U256::from(2u64)),
            U256::from(2u64)
        );
    }

    /// From Celadon on the refund adds the prepaid rounding credit and rounds
    /// down.
    #[test]
    fn celadon_refund_rounds_down_with_the_credit() {
        // floor((4 + 0) / 3) = 1 — a whole token unit less than the old rule.
        assert_eq!(
            refund_with(MorphHardfork::Celadon, 4, U256::ZERO),
            U256::from(1u64)
        );
        // floor((4 + 2) / 3) = 2: the credit can bring the refund back up.
        assert_eq!(
            refund_with(MorphHardfork::Celadon, 4, U256::from(2u64)),
            U256::from(2u64)
        );
    }

    /// Flooring can reach zero, where the ceiling always refunded at least one
    /// unit. A zero refund must move no tokens and write no slots, matching
    /// go-ethereum's `TransferAltTokenHybrid` early return.
    #[test]
    fn celadon_zero_refund_touches_nothing() {
        // floor(2 / 3) = 0 while ceil(2 / 3) = 1.
        assert_eq!(
            refund_with(MorphHardfork::Celadon, 2, U256::ZERO),
            U256::ZERO
        );
        assert_eq!(
            refund_with(MorphHardfork::Jade, 2, U256::ZERO),
            U256::from(1u64)
        );

        let mut evm = evm_after_deduction(MorphHardfork::Celadon);
        refund(&mut evm, 2, U256::ZERO);
        assert!(
            evm.logs().is_empty(),
            "a zero refund must emit no Transfer log"
        );
        assert!(
            evm.state()
                .loaded_accounts()
                .all(|(address, mut slots)| *address != TOKEN || slots.next().is_none()),
            "a zero refund must not load or write the token's balance slots"
        );
        assert_eq!(
            token_balance(&mut evm, BENEFICIARY),
            U256::from(VAULT_BALANCE),
            "a zero refund must leave the vault balance alone"
        );
    }
}
