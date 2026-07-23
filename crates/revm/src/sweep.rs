use crate::{MorphEvm, MorphInvalidTransaction, MorphTxEnv, SweepConfig, handler::MorphEvmHandler};
use alloy_evm::Database;
use alloy_primitives::{Address, B256, Bytes, Log, U256, b256};
use revm::{
    context::{TxEnv, result::EVMError},
    context_interface::{Cfg, ContextTr, JournalTr, LocalContextTr, context::take_error},
    handler::{EvmTr, Handler},
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, FrameInput, SharedMemory,
        interpreter_action::FrameInit,
    },
    state::Bytecode,
};
use std::{cell::RefCell, collections::HashSet};

/// Maximum sweep candidates checked after one transaction.
pub const MAX_CANDIDATES_PER_TX: usize = 16;
/// Maximum sweep candidates checked in one block.
pub const MAX_CANDIDATES_PER_BLOCK: usize = 64;
/// Gas limit for the Registry resolver static call.
pub const RESOLVE_GAS_LIMIT: u64 = 50_000;
/// Gas limit for each ERC-20 `balanceOf` static call.
pub const BALANCE_OF_GAS_LIMIT: u64 = 50_000;
/// Gas limit for each ERC-20 `transfer` call.
pub const TRANSFER_GAS_LIMIT: u64 = 200_000;
/// Fixed system-gas debit for every checked candidate.
pub const CANDIDATE_SYSTEM_GAS: u64 = 350_000;
/// Maximum sweep system gas in one block.
pub const BLOCK_SYSTEM_GAS: u64 = 22_400_000;

#[derive(Debug)]
struct TraceReplayContext {
    transaction_hashes: Vec<B256>,
    next_transaction: usize,
    finish_hash: B256,
    remaining_candidates: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct TraceReplayTransaction {
    transaction_hash: B256,
    pub(crate) allowance: usize,
}

thread_local! {
    static TRACE_REPLAY_CONTEXT: RefCell<Option<TraceReplayContext>> = const { RefCell::new(None) };
}

/// Starts a canonical block trace replay on the current worker thread.
///
/// Trace RPC replay closures are synchronous, so a thread-local context shares
/// the block budget across their otherwise independent EVM instances.
pub fn begin_sweep_trace_replay(transaction_hashes: Vec<B256>) {
    let Some(finish_hash) = transaction_hashes.last().copied() else {
        clear_sweep_trace_replay();
        return;
    };
    TRACE_REPLAY_CONTEXT.with(|context| {
        *context.borrow_mut() = Some(TraceReplayContext {
            transaction_hashes,
            next_transaction: 0,
            finish_hash,
            remaining_candidates: MAX_CANDIDATES_PER_BLOCK,
        });
    });
}

/// Sets the target transaction that ends a single-transaction canonical replay.
pub fn set_sweep_trace_replay_target(target: B256) {
    TRACE_REPLAY_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let Some(replay) = context.as_mut() else {
            return;
        };
        if replay.transaction_hashes[replay.next_transaction..].contains(&target) {
            replay.finish_hash = target;
        } else {
            *context = None;
        }
    });
}

/// Clears any canonical trace replay state on the current worker thread.
pub fn clear_sweep_trace_replay() {
    TRACE_REPLAY_CONTEXT.with(|context| {
        *context.borrow_mut() = None;
    });
}

/// RAII boundary for one synchronous state-at-block RPC replay closure.
#[derive(Debug)]
#[must_use = "the scope must live for the entire replay closure"]
pub struct SweepTraceReplayScope;

/// Clears stale replay state and returns a guard that also clears on drop.
pub fn sweep_trace_replay_scope() -> SweepTraceReplayScope {
    clear_sweep_trace_replay();
    SweepTraceReplayScope
}

impl Drop for SweepTraceReplayScope {
    fn drop(&mut self) {
        clear_sweep_trace_replay();
    }
}

pub(crate) fn sweep_trace_replay_transaction(
    rlp_bytes: Option<&Bytes>,
) -> Option<TraceReplayTransaction> {
    let Some(rlp_bytes) = rlp_bytes else {
        clear_sweep_trace_replay();
        return None;
    };
    let transaction_hash = alloy_primitives::keccak256(rlp_bytes);
    TRACE_REPLAY_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let replay = context.as_ref()?;
        if replay.transaction_hashes.get(replay.next_transaction) != Some(&transaction_hash) {
            *context = None;
            return None;
        }
        Some(TraceReplayTransaction {
            transaction_hash,
            allowance: replay.remaining_candidates.min(MAX_CANDIDATES_PER_TX),
        })
    })
}

pub(crate) fn finish_sweep_trace_replay_transaction(
    transaction: Option<TraceReplayTransaction>,
    checked_candidates: usize,
) {
    let Some(transaction) = transaction else {
        return;
    };
    TRACE_REPLAY_CONTEXT.with(|context| {
        let mut context = context.borrow_mut();
        let Some(replay) = context.as_mut() else {
            return;
        };
        if replay.transaction_hashes.get(replay.next_transaction)
            != Some(&transaction.transaction_hash)
        {
            *context = None;
            return;
        }
        replay.remaining_candidates = replay
            .remaining_candidates
            .saturating_sub(checked_candidates);
        replay.next_transaction += 1;
        if transaction.transaction_hash == replay.finish_hash
            || replay.next_transaction == replay.transaction_hashes.len()
        {
            *context = None;
        }
    });
}

const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const REQUEST_TOPIC: B256 =
    b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");
const SWEEP_TOPIC: B256 = b256!("035b37215a69e14a80883933d6aa84f0919a67af9410a4a73e8a23baeca011f0");
const RESOLVE_SELECTOR: [u8; 4] = [0x9f, 0xaa, 0x2f, 0x2f];
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

/// A token/deposit pair eligible for a sweep check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepCandidate {
    /// ERC-20 token contract.
    pub token: Address,
    /// Deposit address.
    pub deposit: Address,
}

/// Classification for a sweep business failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SweepFailureReason {
    /// Registry resolver reverted or halted.
    ResolverCallFailed,
    /// Registry resolver returned a non-canonical address.
    ResolverMalformed,
    /// Registry resolver returned the zero address.
    ResolverZero,
    /// Deposit has ordinary code or an EIP-7702 delegation.
    DepositHasCode,
    /// Pre-transfer `balanceOf` reverted or halted.
    BalanceCallFailed,
    /// Pre-transfer `balanceOf` returned malformed data.
    BalanceMalformed,
    /// Deposit token balance is zero.
    BalanceZero,
    /// Token `transfer` reverted or halted.
    TransferCallFailed,
    /// Token `transfer` returned ABI `false`.
    TransferFalse,
    /// Token `transfer` returned malformed data.
    TransferMalformed,
    /// Post-transfer `balanceOf` reverted or halted.
    PostBalanceCallFailed,
    /// Post-transfer `balanceOf` returned malformed data.
    PostBalanceMalformed,
    /// Deposit retained a non-zero post-transfer balance.
    PostBalanceNonZero,
    /// Transfer call emitted no matching canonical `Transfer`.
    MissingTransferLog,
    /// Transfer call emitted more than one matching canonical `Transfer`.
    DuplicateTransferLog,
}

impl SweepFailureReason {
    /// Stable snake_case label for metrics dimensions.
    ///
    /// These strings are a low-cardinality observability contract; keep them
    /// stable so dashboards and alerts survive refactors.
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::ResolverCallFailed => "resolver_call_failed",
            Self::ResolverMalformed => "resolver_malformed",
            Self::ResolverZero => "resolver_zero",
            Self::DepositHasCode => "deposit_has_code",
            Self::BalanceCallFailed => "balance_call_failed",
            Self::BalanceMalformed => "balance_malformed",
            Self::BalanceZero => "balance_zero",
            Self::TransferCallFailed => "transfer_call_failed",
            Self::TransferFalse => "transfer_false",
            Self::TransferMalformed => "transfer_malformed",
            Self::PostBalanceCallFailed => "post_balance_call_failed",
            Self::PostBalanceMalformed => "post_balance_malformed",
            Self::PostBalanceNonZero => "post_balance_non_zero",
            Self::MissingTransferLog => "missing_transfer_log",
            Self::DuplicateTransferLog => "duplicate_transfer_log",
        }
    }
}

/// A checked candidate that did not complete a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepFailure {
    /// Candidate that failed.
    pub candidate: SweepCandidate,
    /// Business-failure classification.
    pub reason: SweepFailureReason,
}

/// A successfully swept candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepSuccess {
    /// Candidate that was swept.
    pub candidate: SweepCandidate,
    /// Registry-resolved master recipient.
    pub master: Address,
    /// Full pre-transfer deposit balance.
    pub amount: U256,
    /// Receipt-relative offset of the matching token `Transfer` log.
    pub transfer_log_offset: u32,
}

/// Internal consensus invariant violation while constructing sweep receipt logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SweepInvariantError {
    /// The receipt-relative transfer log offset cannot be represented as `uint32`.
    #[error("sweep transfer log offset exceeds uint32")]
    TransferLogOffsetOverflow,
}

/// Take-once result cached by [`MorphEvm`] after transaction execution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepOutcome {
    /// Token call logs and EL-synthesized `Swept` logs.
    pub logs: Vec<Log>,
    /// Number of candidates checked against the supplied allowance.
    pub checked_candidates: usize,
    /// Fixed system-gas debit for checked candidates.
    pub system_gas_used: u64,
    /// Successful sweeps.
    pub successes: Vec<SweepSuccess>,
    /// Classified business failures.
    pub failures: Vec<SweepFailure>,
}

#[inline]
fn address_from_topic(topic: B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

#[inline]
fn is_canonical_address_topic(topic: B256) -> bool {
    topic.as_slice()[..12] == [0; 12]
}

/// Parses one exact ERC-20 transfer or Registry sweep-request log.
pub fn parse_sweep_candidate(log: &Log, registry: Address) -> Option<SweepCandidate> {
    let topics = log.topics();
    // The `Transfer` branch deliberately does NOT require canonical address
    // topics: it takes the low 20 bytes of `topics[2]` regardless of the high
    // bytes, matching go-ethereum's `common.BytesToAddress`. Keeping the same
    // lenient extraction is what makes reth and geth agree on the candidate set
    // for a non-canonically-encoded token log. The `SweepRequested`
    // branch below is strict instead, because that event is emitted only by the
    // Registry, whose Solidity `indexed address` topics are always zero-padded;
    // a non-canonical request topic therefore cannot originate from the real
    // Registry and must be rejected.
    if topics.len() == 3 && topics[0] == TRANSFER_TOPIC && log.data.data.len() == 32 {
        return Some(SweepCandidate {
            token: log.address,
            deposit: address_from_topic(topics[2]),
        });
    }

    if log.address == registry
        && topics.len() == 3
        && topics[0] == REQUEST_TOPIC
        && log.data.data.is_empty()
        && is_canonical_address_topic(topics[1])
        && is_canonical_address_topic(topics[2])
    {
        return Some(SweepCandidate {
            token: address_from_topic(topics[1]),
            deposit: address_from_topic(topics[2]),
        });
    }

    None
}

/// Collects first-seen, deduplicated sweep candidates.
pub fn collect_sweep_candidates(main_logs: &[Log], registry: Address) -> Vec<SweepCandidate> {
    let mut seen = HashSet::with_capacity(MAX_CANDIDATES_PER_TX);
    main_logs
        .iter()
        .filter_map(|log| parse_sweep_candidate(log, registry))
        .filter(|candidate| seen.insert(*candidate))
        .take(MAX_CANDIDATES_PER_TX)
        .collect()
}

/// Builds the EL-synthesized protocol settlement log.
pub fn build_sweep_log(
    registry: Address,
    candidate: SweepCandidate,
    master: Address,
    amount: U256,
    transfer_log_offset: u32,
) -> Log {
    let mut data = Vec::with_capacity(64);
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    data.extend_from_slice(&[0; 28]);
    data.extend_from_slice(&transfer_log_offset.to_be_bytes());
    Log::new_unchecked(
        registry,
        vec![
            SWEEP_TOPIC,
            B256::left_padding_from(candidate.token.as_slice()),
            B256::left_padding_from(candidate.deposit.as_slice()),
            B256::left_padding_from(master.as_slice()),
        ],
        Bytes::from(data),
    )
}

#[inline]
fn encode_address_call(selector: [u8; 4], address: Address) -> Bytes {
    let mut data = Vec::with_capacity(36);
    data.extend_from_slice(&selector);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(address.as_slice());
    Bytes::from(data)
}

#[inline]
fn encode_two_address_call(selector: [u8; 4], first: Address, second: Address) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&selector);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(first.as_slice());
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(second.as_slice());
    Bytes::from(data)
}

#[inline]
fn encode_transfer_call(recipient: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&TRANSFER_SELECTOR);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(recipient.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

fn load_call_bytecode<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    target: Address,
) -> Result<(B256, Bytecode), DB::Error>
where
    DB: Database,
{
    let delegated = {
        let account = evm.ctx_mut().journal_mut().load_account_with_code(target)?;
        account
            .info
            .code
            .as_ref()
            .and_then(Bytecode::eip7702_address)
    };
    let account = evm
        .ctx_mut()
        .journal_mut()
        .load_account_with_code(delegated.unwrap_or(target))?;
    Ok((
        account.info.code_hash(),
        account.info.code.clone().unwrap_or_default(),
    ))
}

#[derive(Debug)]
struct InternalCall {
    output: Option<Bytes>,
}

fn internal_call<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    caller: Address,
    target: Address,
    calldata: Bytes,
    gas_limit: u64,
    is_static: bool,
) -> Result<InternalCall, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let original_tx = std::mem::replace(
        &mut evm.tx,
        MorphTxEnv {
            inner: TxEnv {
                caller,
                kind: target.into(),
                data: calldata.clone(),
                gas_limit,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let snapshot = is_static.then(|| evm.ctx_mut().journal_mut().checkpoint());

    let result = (|| {
        let known_bytecode = load_call_bytecode(evm, target)?;
        let mut memory =
            SharedMemory::new_with_buffer(evm.ctx_ref().local().shared_memory_buffer().clone());
        memory.set_memory_limit(evm.ctx_ref().cfg().memory_limit());
        let frame_init = FrameInit {
            depth: 0,
            memory,
            frame_input: FrameInput::Call(Box::new(CallInputs {
                input: CallInput::Bytes(calldata),
                return_memory_offset: 0..0,
                gas_limit,
                reservoir: 0,
                bytecode_address: target,
                known_bytecode,
                target_address: target,
                caller,
                value: CallValue::Transfer(U256::ZERO),
                scheme: if is_static {
                    CallScheme::StaticCall
                } else {
                    CallScheme::Call
                },
                is_static,
                charged_new_account_state_gas: false,
            })),
        };
        let mut handler = MorphEvmHandler::<DB, I>::new();
        let frame_result = handler.run_exec_loop(evm, frame_init)?;
        take_error::<EVMError<DB::Error, MorphInvalidTransaction>, _>(evm.ctx().error())?;
        Ok(InternalCall {
            output: frame_result
                .instruction_result()
                .is_ok()
                .then(|| frame_result.interpreter_result().output.clone()),
        })
    })();

    if let Some(snapshot) = snapshot {
        evm.ctx_mut().journal_mut().checkpoint_revert(snapshot);
    }
    evm.frame_stack().clear();
    evm.ctx_mut().local_mut().clear();
    evm.tx = original_tx;
    result
}

#[inline]
fn decode_address(output: &Bytes) -> Result<Address, SweepFailureReason> {
    if output.len() != 32 || output[..12] != [0; 12] {
        return Err(SweepFailureReason::ResolverMalformed);
    }
    let address = Address::from_slice(&output[12..]);
    if address.is_zero() {
        return Err(SweepFailureReason::ResolverZero);
    }
    Ok(address)
}

#[inline]
fn decode_balance(
    output: &Bytes,
    malformed: SweepFailureReason,
) -> Result<U256, SweepFailureReason> {
    if output.len() != 32 {
        return Err(malformed);
    }
    Ok(U256::from_be_slice(output))
}

#[inline]
fn classify_transfer_output(output: &Bytes) -> Result<(), SweepFailureReason> {
    if output.is_empty() {
        return Ok(());
    }
    if output.len() != 32 {
        return Err(SweepFailureReason::TransferMalformed);
    }
    match U256::from_be_slice(output) {
        U256::ZERO => Err(SweepFailureReason::TransferFalse),
        value if value == U256::from(1) => Ok(()),
        _ => Err(SweepFailureReason::TransferMalformed),
    }
}

#[inline]
fn is_matching_transfer(
    log: &Log,
    candidate: SweepCandidate,
    master: Address,
    amount: U256,
) -> bool {
    log.address == candidate.token
        && log.topics()
            == [
                TRANSFER_TOPIC,
                B256::left_padding_from(candidate.deposit.as_slice()),
                B256::left_padding_from(master.as_slice()),
            ]
        && log.data.data.as_ref() == amount.to_be_bytes::<32>()
}

fn push_failure(outcome: &mut SweepOutcome, candidate: SweepCandidate, reason: SweepFailureReason) {
    outcome.failures.push(SweepFailure { candidate, reason });
}

fn checked_transfer_log_offset(
    receipt_prefix_logs: usize,
    earlier_sweep_logs: usize,
    matching_log_offset: usize,
) -> Result<u32, SweepInvariantError> {
    receipt_prefix_logs
        .checked_add(earlier_sweep_logs)
        .and_then(|offset| offset.checked_add(matching_log_offset))
        .and_then(|offset| u32::try_from(offset).ok())
        .ok_or(SweepInvariantError::TransferLogOffsetOverflow)
}

fn check_candidate<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidate: SweepCandidate,
    receipt_prefix_logs: usize,
    outcome: &mut SweepOutcome,
) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let resolver = internal_call(
        evm,
        Address::ZERO,
        config.registry_address,
        encode_two_address_call(RESOLVE_SELECTOR, candidate.token, candidate.deposit),
        RESOLVE_GAS_LIMIT,
        true,
    )?;
    let Some(resolver_output) = resolver.output else {
        push_failure(outcome, candidate, SweepFailureReason::ResolverCallFailed);
        return Ok(());
    };
    let master = match decode_address(&resolver_output) {
        Ok(master) => master,
        Err(reason) => {
            push_failure(outcome, candidate, reason);
            return Ok(());
        }
    };

    let deposit_has_code = evm
        .ctx_mut()
        .journal_mut()
        .load_account_with_code(candidate.deposit)?
        .info
        .code
        .as_ref()
        .is_some_and(|code| !code.is_empty());
    if deposit_has_code {
        push_failure(outcome, candidate, SweepFailureReason::DepositHasCode);
        return Ok(());
    }

    let balance = internal_call(
        evm,
        Address::ZERO,
        candidate.token,
        encode_address_call(BALANCE_OF_SELECTOR, candidate.deposit),
        BALANCE_OF_GAS_LIMIT,
        true,
    )?;
    let Some(balance_output) = balance.output else {
        push_failure(outcome, candidate, SweepFailureReason::BalanceCallFailed);
        return Ok(());
    };
    let balance = match decode_balance(&balance_output, SweepFailureReason::BalanceMalformed) {
        Ok(balance) => balance,
        Err(reason) => {
            push_failure(outcome, candidate, reason);
            return Ok(());
        }
    };
    if balance.is_zero() {
        push_failure(outcome, candidate, SweepFailureReason::BalanceZero);
        return Ok(());
    }

    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
    let log_start = evm.ctx_ref().journal().logs.len();
    let candidate_result = (|| {
        let transfer = internal_call(
            evm,
            candidate.deposit,
            candidate.token,
            encode_transfer_call(master, balance),
            TRANSFER_GAS_LIMIT,
            false,
        )?;
        let Some(transfer_output) = transfer.output else {
            return Ok(Err(SweepFailureReason::TransferCallFailed));
        };
        if let Err(reason) = classify_transfer_output(&transfer_output) {
            return Ok(Err(reason));
        }

        let post_balance = internal_call(
            evm,
            Address::ZERO,
            candidate.token,
            encode_address_call(BALANCE_OF_SELECTOR, candidate.deposit),
            BALANCE_OF_GAS_LIMIT,
            true,
        )?;
        let Some(post_balance_output) = post_balance.output else {
            return Ok(Err(SweepFailureReason::PostBalanceCallFailed));
        };
        let post_balance = match decode_balance(
            &post_balance_output,
            SweepFailureReason::PostBalanceMalformed,
        ) {
            Ok(balance) => balance,
            Err(reason) => return Ok(Err(reason)),
        };
        if !post_balance.is_zero() {
            return Ok(Err(SweepFailureReason::PostBalanceNonZero));
        }

        let mut matching_logs = evm.ctx_ref().journal().logs[log_start..]
            .iter()
            .enumerate()
            .filter(|(_, log)| is_matching_transfer(log, candidate, master, balance));
        let Some((matching_log_offset, _)) = matching_logs.next() else {
            return Ok(Err(SweepFailureReason::MissingTransferLog));
        };
        if matching_logs.next().is_some() {
            Ok(Err(SweepFailureReason::DuplicateTransferLog))
        } else {
            Ok(Ok(matching_log_offset))
        }
    })();

    match candidate_result {
        Err(error) => {
            evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
            Err(error)
        }
        Ok(Err(reason)) => {
            evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
            push_failure(outcome, candidate, reason);
            Ok(())
        }
        Ok(Ok(matching_log_offset)) => {
            let transfer_log_offset = match checked_transfer_log_offset(
                receipt_prefix_logs,
                outcome.logs.len(),
                matching_log_offset,
            ) {
                Ok(offset) => offset,
                Err(error) => {
                    evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
                    return Err(EVMError::Custom(error.to_string()));
                }
            };
            let call_logs = evm.ctx_ref().journal().logs[log_start..].to_vec();
            evm.ctx_mut().journal_mut().logs.truncate(log_start);
            evm.ctx_mut().journal_mut().checkpoint_commit();

            outcome.logs.extend(call_logs);
            outcome.logs.push(build_sweep_log(
                config.registry_address,
                candidate,
                master,
                balance,
                transfer_log_offset,
            ));
            outcome.successes.push(SweepSuccess {
                candidate,
                master,
                amount: balance,
                transfer_log_offset,
            });
            Ok(())
        }
    }
}

/// Executes independent sweep candidates against the post-main state.
pub(crate) fn execute_sweeps<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidates: &[SweepCandidate],
    receipt_prefix_logs: usize,
    allowance: usize,
) -> Result<SweepOutcome, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let mut outcome = SweepOutcome::default();
    for candidate in candidates
        .iter()
        .copied()
        .take(allowance.min(MAX_CANDIDATES_PER_TX))
    {
        outcome.checked_candidates += 1;
        outcome.system_gas_used = outcome.system_gas_used.saturating_add(CANDIDATE_SYSTEM_GAS);
        check_candidate(evm, config, candidate, receipt_prefix_logs, &mut outcome)?;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MorphBlockEnv, MorphEvm, SweepConfig, evm::MorphContext};
    use alloy_primitives::{Address, B256, Bytes, Log, U256, address, b256};
    use morph_chainspec::hardfork::MorphHardfork;
    use revm::{
        DatabaseRef, ExecuteEvm, InspectEvm,
        context::{
            BlockEnv, TxEnv,
            result::{ExecutionResult, Output, ResultGas, SuccessReason},
        },
        context_interface::JournalTr,
        database::{CacheDB, EmptyDB},
        database_interface::DBErrorMarker,
        inspector::NoOpInspector,
        primitives::{StorageKey, StorageValue, TxKind},
        state::{AccountInfo, Bytecode},
    };
    use std::{collections::HashMap, fmt};

    const REGISTRY: Address = address!("5300000000000000000000000000000000000023");
    const DEPOSIT: Address = address!("1000000000000000000000000000000000000001");
    const MASTER: Address = address!("2000000000000000000000000000000000000002");
    const TOKEN_A: Address = address!("3000000000000000000000000000000000000003");
    const TOKEN_B: Address = address!("4000000000000000000000000000000000000004");
    const INITIAL_BALANCE: u64 = 9;

    const TRANSFER_TOPIC: B256 =
        b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
    const REQUEST_TOPIC: B256 =
        b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");
    const SWEEP_TOPIC: B256 =
        b256!("035b37215a69e14a80883933d6aa84f0919a67af9410a4a73e8a23baeca011f0");

    fn address_topic(address: Address) -> B256 {
        B256::left_padding_from(address.as_slice())
    }

    fn transfer_log(token: Address, from: Address, to: Address, amount: U256) -> Log {
        Log::new_unchecked(
            token,
            vec![TRANSFER_TOPIC, address_topic(from), address_topic(to)],
            Bytes::copy_from_slice(&amount.to_be_bytes::<32>()),
        )
    }

    fn request_log(token: Address, deposit: Address) -> Log {
        Log::new_unchecked(
            REGISTRY,
            vec![REQUEST_TOPIC, address_topic(token), address_topic(deposit)],
            Bytes::new(),
        )
    }

    #[test]
    fn failure_reason_labels_are_stable_and_distinct() {
        let reasons = [
            SweepFailureReason::ResolverCallFailed,
            SweepFailureReason::ResolverMalformed,
            SweepFailureReason::ResolverZero,
            SweepFailureReason::DepositHasCode,
            SweepFailureReason::BalanceCallFailed,
            SweepFailureReason::BalanceMalformed,
            SweepFailureReason::BalanceZero,
            SweepFailureReason::TransferCallFailed,
            SweepFailureReason::TransferFalse,
            SweepFailureReason::TransferMalformed,
            SweepFailureReason::PostBalanceCallFailed,
            SweepFailureReason::PostBalanceMalformed,
            SweepFailureReason::PostBalanceNonZero,
            SweepFailureReason::MissingTransferLog,
            SweepFailureReason::DuplicateTransferLog,
        ];
        let labels: HashSet<&str> = reasons.iter().map(|r| r.as_label()).collect();
        assert_eq!(
            labels.len(),
            reasons.len(),
            "every failure reason must map to a distinct metrics label"
        );
        assert!(
            labels.iter().all(|label| !label.is_empty()),
            "metrics labels must be non-empty"
        );
    }

    #[test]
    fn parses_only_exact_transfer_and_request_logs() {
        let exact_transfer = transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(7));
        let exact_request = request_log(TOKEN_B, DEPOSIT);

        assert_eq!(
            parse_sweep_candidate(&exact_transfer, REGISTRY),
            Some(SweepCandidate {
                token: TOKEN_A,
                deposit: DEPOSIT,
            })
        );
        assert_eq!(
            parse_sweep_candidate(&exact_request, REGISTRY),
            Some(SweepCandidate {
                token: TOKEN_B,
                deposit: DEPOSIT,
            })
        );

        let malformed = [
            Log::new_unchecked(
                TOKEN_A,
                vec![TRANSFER_TOPIC, address_topic(DEPOSIT)],
                Bytes::from([0_u8; 32]),
            ),
            Log::new_unchecked(
                TOKEN_A,
                vec![
                    TRANSFER_TOPIC,
                    address_topic(Address::ZERO),
                    address_topic(DEPOSIT),
                ],
                Bytes::from([0_u8; 31]),
            ),
            Log::new_unchecked(
                TOKEN_A,
                vec![
                    B256::ZERO,
                    address_topic(Address::ZERO),
                    address_topic(DEPOSIT),
                ],
                Bytes::from([0_u8; 32]),
            ),
            Log::new_unchecked(
                Address::ZERO,
                vec![
                    REQUEST_TOPIC,
                    address_topic(TOKEN_A),
                    address_topic(DEPOSIT),
                ],
                Bytes::new(),
            ),
            Log::new_unchecked(
                REGISTRY,
                vec![
                    REQUEST_TOPIC,
                    address_topic(TOKEN_A),
                    address_topic(DEPOSIT),
                ],
                Bytes::from([0]),
            ),
        ];
        for log in malformed {
            assert_eq!(parse_sweep_candidate(&log, REGISTRY), None);
        }

        let mut noncanonical_token = address_topic(TOKEN_A);
        noncanonical_token.0[0] = 1;
        let malformed_request = Log::new_unchecked(
            REGISTRY,
            vec![REQUEST_TOPIC, noncanonical_token, address_topic(DEPOSIT)],
            Bytes::new(),
        );
        assert_eq!(parse_sweep_candidate(&malformed_request, REGISTRY), None);
    }

    #[test]
    fn candidates_preserve_order_deduplicate_and_cap_at_sixteen() {
        let mut logs = vec![
            transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(1)),
            request_log(TOKEN_A, DEPOSIT),
            request_log(TOKEN_B, DEPOSIT),
        ];
        for i in 0_u8..20 {
            let token = Address::with_last_byte(i.saturating_add(10));
            let deposit = Address::with_last_byte(i.saturating_add(100));
            logs.push(transfer_log(token, Address::ZERO, deposit, U256::from(1)));
        }

        let candidates = collect_sweep_candidates(&logs, REGISTRY);

        assert_eq!(candidates.len(), MAX_CANDIDATES_PER_TX);
        assert_eq!(
            candidates[0],
            SweepCandidate {
                token: TOKEN_A,
                deposit: DEPOSIT,
            }
        );
        assert_eq!(
            candidates[1],
            SweepCandidate {
                token: TOKEN_B,
                deposit: DEPOSIT,
            }
        );
        assert_eq!(
            candidates[2],
            SweepCandidate {
                token: Address::with_last_byte(10),
                deposit: Address::with_last_byte(100),
            }
        );
    }

    #[test]
    fn sweep_event_uses_canonical_abi_and_receipt_relative_offset() {
        let candidate = SweepCandidate {
            token: TOKEN_A,
            deposit: DEPOSIT,
        };

        let log = build_sweep_log(REGISTRY, candidate, MASTER, U256::from(0x1234), 0x0102_0304);

        assert_eq!(log.address, REGISTRY);
        assert_eq!(
            log.topics(),
            &[
                SWEEP_TOPIC,
                address_topic(TOKEN_A),
                address_topic(DEPOSIT),
                address_topic(MASTER),
            ]
        );
        assert_eq!(log.data.data.len(), 64);
        assert_eq!(
            U256::from_be_slice(&log.data.data[..32]),
            U256::from(0x1234)
        );
        assert_eq!(&log.data.data[32..60], &[0_u8; 28]);
        assert_eq!(&log.data.data[60..], &0x0102_0304_u32.to_be_bytes());
    }

    #[derive(Clone, Copy)]
    enum ResolverMode {
        Master,
        Zero,
        Malformed,
        Revert,
        Mutating,
    }

    #[derive(Clone, Copy)]
    enum BalanceMode {
        Normal,
        Zero,
        Malformed,
        Revert,
        MalformedAfterTransfer,
        RevertAfterTransfer,
    }

    #[derive(Clone, Copy)]
    enum TransferMode {
        Empty,
        True,
        False,
        Malformed,
        Revert,
        PostBalanceNonZero,
        MissingLog,
        DuplicateLog,
        ExtraLog,
    }

    #[derive(Clone, Copy)]
    enum MainMode {
        Success,
        SuccessWithState,
        Revert,
    }

    struct Assembler {
        code: Vec<u8>,
        labels: HashMap<&'static str, usize>,
        fixups: Vec<(usize, &'static str)>,
    }

    impl Assembler {
        fn new() -> Self {
            Self {
                code: Vec::new(),
                labels: HashMap::new(),
                fixups: Vec::new(),
            }
        }

        fn op(&mut self, opcode: u8) {
            self.code.push(opcode);
        }

        fn push(&mut self, bytes: &[u8]) {
            assert!(!bytes.is_empty() && bytes.len() <= 32);
            self.code.push(0x5f + bytes.len() as u8);
            self.code.extend_from_slice(bytes);
        }

        fn push_u8(&mut self, value: u8) {
            self.push(&[value]);
        }

        fn push_b256(&mut self, value: B256) {
            self.push(value.as_slice());
        }

        fn label(&mut self, label: &'static str) {
            self.labels.insert(label, self.code.len());
            self.op(0x5b);
        }

        fn jumpi(&mut self, label: &'static str) {
            self.op(0x61);
            let offset = self.code.len();
            self.code.extend_from_slice(&[0, 0]);
            self.fixups.push((offset, label));
            self.op(0x57);
        }

        fn finish(mut self) -> Bytes {
            for (offset, label) in self.fixups {
                let destination = u16::try_from(self.labels[label]).unwrap().to_be_bytes();
                self.code[offset..offset + 2].copy_from_slice(&destination);
            }
            Bytes::from(self.code)
        }
    }

    fn return_word(asm: &mut Assembler, word: B256) {
        asm.push_b256(word);
        asm.push_u8(0);
        asm.op(0x52);
        asm.push_u8(32);
        asm.push_u8(0);
        asm.op(0xf3);
    }

    fn registry_code(mode: ResolverMode) -> Bytes {
        let mut asm = Assembler::new();
        match mode {
            ResolverMode::Master => return_word(&mut asm, address_topic(MASTER)),
            ResolverMode::Zero => return_word(&mut asm, B256::ZERO),
            ResolverMode::Malformed => {
                asm.push_u8(1);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(1);
                asm.push_u8(31);
                asm.op(0xf3);
            }
            ResolverMode::Revert => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xfd);
            }
            ResolverMode::Mutating => {
                asm.push_u8(1);
                asm.push_u8(0);
                asm.op(0x55);
                return_word(&mut asm, address_topic(MASTER));
            }
        }
        asm.finish()
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct OpcodeStorageError;

    impl fmt::Display for OpcodeStorageError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("opcode storage error")
        }
    }

    impl std::error::Error for OpcodeStorageError {}
    impl DBErrorMarker for OpcodeStorageError {}

    #[derive(Debug, Default)]
    struct FailingStorageDb;

    impl DatabaseRef for FailingStorageDb {
        type Error = OpcodeStorageError;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage_ref(
            &self,
            address: Address,
            _index: StorageKey,
        ) -> Result<StorageValue, Self::Error> {
            if address == TOKEN_B {
                Err(OpcodeStorageError)
            } else {
                Ok(StorageValue::ZERO)
            }
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    fn emit_transfer(asm: &mut Assembler) {
        asm.push_u8(4);
        asm.op(0x35);
        asm.push_b256(address_topic(DEPOSIT));
        asm.push_b256(TRANSFER_TOPIC);
        asm.push_u8(32);
        asm.push_u8(0);
        asm.op(0xa3);
    }

    fn emit_main_candidate(asm: &mut Assembler) {
        asm.push_u8(1);
        asm.push_u8(0);
        asm.op(0x52);
        asm.push_b256(address_topic(DEPOSIT));
        asm.push_b256(address_topic(Address::ZERO));
        asm.push_b256(TRANSFER_TOPIC);
        asm.push_u8(32);
        asm.push_u8(0);
        asm.op(0xa3);
    }

    fn token_code_with_main(
        balance_mode: BalanceMode,
        transfer_mode: TransferMode,
        main_mode: MainMode,
    ) -> Bytes {
        let mut asm = Assembler::new();

        asm.push_u8(0);
        asm.op(0x35);
        asm.push_u8(0xe0);
        asm.op(0x1c);
        asm.op(0x80);
        asm.push(&[0x70, 0xa0, 0x82, 0x31]);
        asm.op(0x14);
        asm.jumpi("balance");
        asm.push(&[0xa9, 0x05, 0x9c, 0xbb]);
        asm.op(0x14);
        asm.jumpi("transfer");
        emit_main_candidate(&mut asm);
        match main_mode {
            MainMode::Success => asm.op(0x00),
            MainMode::SuccessWithState => {
                asm.push_u8(7);
                asm.push_u8(1);
                asm.op(0x55);
                asm.push_u8(9);
                asm.push_u8(2);
                asm.op(0x5d);
                asm.op(0x00);
            }
            MainMode::Revert => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xfd);
            }
        }

        asm.label("balance");
        match balance_mode {
            BalanceMode::Normal => {
                asm.push_u8(0);
                asm.op(0x54);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            BalanceMode::Zero => return_word(&mut asm, B256::ZERO),
            BalanceMode::Malformed => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(1);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            BalanceMode::Revert => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xfd);
            }
            BalanceMode::MalformedAfterTransfer | BalanceMode::RevertAfterTransfer => {
                asm.push_u8(0);
                asm.op(0x54);
                asm.op(0x80);
                asm.op(0x15);
                asm.jumpi("post_balance_failure");
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
                asm.label("post_balance_failure");
                if matches!(balance_mode, BalanceMode::MalformedAfterTransfer) {
                    asm.push_u8(0);
                    asm.push_u8(0);
                    asm.op(0x52);
                    asm.push_u8(1);
                    asm.push_u8(0);
                    asm.op(0xf3);
                } else {
                    asm.push_u8(0);
                    asm.push_u8(0);
                    asm.op(0xfd);
                }
            }
        }

        asm.label("transfer");
        if matches!(transfer_mode, TransferMode::Revert) {
            asm.push_u8(0);
            asm.push_u8(0);
            asm.op(0xfd);
            return asm.finish();
        }

        asm.push_u8(36);
        asm.op(0x35);
        asm.push_u8(0);
        asm.op(0x52);
        let post_balance = u8::from(matches!(transfer_mode, TransferMode::PostBalanceNonZero));
        asm.push_u8(post_balance);
        asm.push_u8(0);
        asm.op(0x55);

        if !matches!(transfer_mode, TransferMode::MissingLog) {
            if matches!(transfer_mode, TransferMode::ExtraLog) {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xa0);
            }
            emit_transfer(&mut asm);
            if matches!(transfer_mode, TransferMode::DuplicateLog) {
                emit_transfer(&mut asm);
            }
        }

        match transfer_mode {
            TransferMode::Empty
            | TransferMode::PostBalanceNonZero
            | TransferMode::MissingLog
            | TransferMode::DuplicateLog
            | TransferMode::ExtraLog => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            TransferMode::True => return_word(&mut asm, B256::from(U256::from(1))),
            TransferMode::False => return_word(&mut asm, B256::ZERO),
            TransferMode::Malformed => {
                asm.push_u8(1);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(1);
                asm.push_u8(31);
                asm.op(0xf3);
            }
            TransferMode::Revert => unreachable!(),
        }

        asm.finish()
    }

    fn insert_code<ExtDB>(db: &mut CacheDB<ExtDB>, address: Address, raw: Bytes) {
        let code = Bytecode::new_raw(raw);
        db.insert_account_info(
            address,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );
    }

    fn make_evm(
        resolver_mode: ResolverMode,
        tokens: &[(Address, BalanceMode, TransferMode)],
        deposit_code: Option<Bytecode>,
        enabled: bool,
    ) -> MorphEvm<CacheDB<EmptyDB>, NoOpInspector> {
        make_evm_with_main_mode(
            resolver_mode,
            tokens,
            deposit_code,
            enabled,
            MainMode::Success,
        )
    }

    fn make_evm_with_main_mode(
        resolver_mode: ResolverMode,
        tokens: &[(Address, BalanceMode, TransferMode)],
        deposit_code: Option<Bytecode>,
        enabled: bool,
        main_mode: MainMode,
    ) -> MorphEvm<CacheDB<EmptyDB>, NoOpInspector> {
        let mut db = CacheDB::new(EmptyDB::default());
        insert_code(&mut db, REGISTRY, registry_code(resolver_mode));
        for (token, balance_mode, transfer_mode) in tokens {
            insert_code(
                &mut db,
                *token,
                token_code_with_main(*balance_mode, *transfer_mode, main_mode),
            );
            db.insert_account_storage(*token, U256::ZERO, U256::from(INITIAL_BALANCE))
                .unwrap();
        }
        if let Some(code) = deposit_code {
            db.insert_account_info(
                DEPOSIT,
                AccountInfo {
                    code_hash: code.hash_slow(),
                    code: Some(code),
                    ..Default::default()
                },
            );
        }

        let mut evm = MorphEvm::new(MorphContext::new(db, MorphHardfork::Onyx), NoOpInspector);
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
            sweep: enabled.then_some(SweepConfig {
                registry_address: REGISTRY,
            }),
        };
        evm
    }

    fn main_tx() -> MorphTxEnv {
        MorphTxEnv {
            inner: TxEnv {
                caller: Address::ZERO,
                kind: TxKind::Call(TOKEN_A),
                gas_limit: 500_000,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn candidate(token: Address) -> SweepCandidate {
        SweepCandidate {
            token,
            deposit: DEPOSIT,
        }
    }

    fn execute_one(
        resolver_mode: ResolverMode,
        balance_mode: BalanceMode,
        transfer_mode: TransferMode,
    ) -> (MorphEvm<CacheDB<EmptyDB>, NoOpInspector>, SweepOutcome) {
        let mut evm = make_evm(
            resolver_mode,
            &[(TOKEN_A, balance_mode, transfer_mode)],
            None,
            true,
        );
        let outcome = execute_sweeps(
            &mut evm,
            SweepConfig {
                registry_address: REGISTRY,
            },
            &[candidate(TOKEN_A)],
            0,
            1,
        )
        .unwrap();
        (evm, outcome)
    }

    fn token_balance<ExtDB>(
        evm: &mut MorphEvm<CacheDB<ExtDB>, NoOpInspector>,
        token: Address,
    ) -> U256
    where
        ExtDB: DatabaseRef + fmt::Debug,
        ExtDB::Error: fmt::Debug,
    {
        let _ = evm.ctx_mut().journal_mut().load_account_mut(token).unwrap();
        *evm.ctx_mut()
            .journal_mut()
            .sload(token, U256::ZERO)
            .unwrap()
    }

    fn only_failure(outcome: &SweepOutcome) -> SweepFailureReason {
        assert_eq!(outcome.failures.len(), 1);
        outcome.failures[0].reason
    }

    #[test]
    fn opcode_storage_error_propagates_and_reverts_the_whole_sweep_phase() {
        let mut db = CacheDB::new(FailingStorageDb);
        insert_code(&mut db, REGISTRY, registry_code(ResolverMode::Master));
        insert_code(
            &mut db,
            TOKEN_A,
            token_code_with_main(BalanceMode::Normal, TransferMode::True, MainMode::Success),
        );
        db.insert_account_storage(TOKEN_A, U256::ZERO, U256::from(INITIAL_BALANCE))
            .unwrap();
        insert_code(
            &mut db,
            TOKEN_B,
            token_code_with_main(BalanceMode::Normal, TransferMode::True, MainMode::Success),
        );

        let mut evm = MorphEvm::new(MorphContext::new(db, MorphHardfork::Onyx), NoOpInspector);
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
            sweep: Some(SweepConfig {
                registry_address: REGISTRY,
            }),
        };
        evm.tx.inner.caller = Address::with_last_byte(0xee);
        evm.tx.inner.gas_limit = 777;
        evm.set_sweep_candidate_allowance(2);
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![
                transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(1)),
                transfer_log(TOKEN_B, Address::ZERO, DEPOSIT, U256::from(1)),
            ],
            output: Output::Call(Bytes::new()),
        };

        let error = evm.apply_sweep(&result).unwrap_err();

        assert!(matches!(error, EVMError::Database(OpcodeStorageError)));
        assert_eq!(
            token_balance(&mut evm, TOKEN_A),
            U256::from(INITIAL_BALANCE)
        );
        assert_eq!(evm.tx.inner.caller, Address::with_last_byte(0xee));
        assert_eq!(evm.tx.inner.gas_limit, 777);
        assert!(evm.frame_stack().index().is_none());
        assert!(
            evm.ctx_ref()
                .local()
                .shared_memory_buffer()
                .borrow()
                .is_empty()
        );
        assert!(evm.ctx_ref().journal().logs.is_empty());
    }

    #[test]
    fn onyx_allowance_and_success_result_gate_the_hook() {
        let mut disabled = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            false,
        );
        disabled.set_sweep_candidate_allowance(1);
        assert!(disabled.transact_one(main_tx()).unwrap().is_success());
        assert_eq!(
            disabled.take_sweep_outcome().unwrap(),
            SweepOutcome::default()
        );

        let mut no_allowance = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        assert!(no_allowance.transact_one(main_tx()).unwrap().is_success());
        assert_eq!(
            no_allowance.take_sweep_outcome().unwrap(),
            SweepOutcome::default()
        );

        let mut reverted = make_evm_with_main_mode(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
            MainMode::Revert,
        );
        reverted.set_sweep_candidate_allowance(1);
        assert!(!reverted.transact_one(main_tx()).unwrap().is_success());
        assert_eq!(
            reverted.take_sweep_outcome().unwrap(),
            SweepOutcome::default()
        );

        let mut enabled = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        enabled.set_sweep_candidate_allowance(1);
        let result = enabled.transact_one(main_tx()).unwrap();
        let outcome = enabled.take_sweep_outcome().unwrap();
        assert!(result.is_success());
        assert_eq!(result.logs().len(), 1);
        assert_eq!(result.output(), Some(&Bytes::new()));
        assert_eq!(outcome.successes.len(), 1);
        assert!(enabled.take_sweep_outcome().is_none());
    }

    #[test]
    fn replay_and_inspector_paths_apply_sweep_state() {
        let mut replay = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        replay.tx = main_tx();
        replay.set_sweep_candidate_allowance(1);
        let replayed = replay.replay().unwrap();
        assert!(replayed.result.is_success());
        assert_eq!(
            replayed.state[&TOKEN_A].storage[&U256::ZERO].present_value,
            U256::ZERO
        );
        assert_eq!(replay.take_sweep_outcome().unwrap().successes.len(), 1);

        let mut inspected = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        inspected.set_sweep_candidate_allowance(1);
        let result = inspected.inspect_one_tx(main_tx()).unwrap();
        assert!(result.is_success());
        assert_eq!(token_balance(&mut inspected, TOKEN_A), U256::ZERO);
        assert_eq!(inspected.take_sweep_outcome().unwrap().successes.len(), 1);
    }

    #[test]
    fn inspected_sweep_db_error_discards_main_and_sweep_state() {
        for inspect_one_only in [true, false] {
            let mut db = CacheDB::new(FailingStorageDb);
            insert_code(&mut db, REGISTRY, registry_code(ResolverMode::Master));
            insert_code(
                &mut db,
                TOKEN_B,
                token_code_with_main(
                    BalanceMode::Normal,
                    TransferMode::True,
                    MainMode::SuccessWithState,
                ),
            );
            db.insert_account_storage(TOKEN_B, U256::from(1), U256::ZERO)
                .unwrap();

            let mut evm = MorphEvm::new(MorphContext::new(db, MorphHardfork::Onyx), NoOpInspector);
            evm.block = MorphBlockEnv {
                inner: BlockEnv::default(),
                sweep: Some(SweepConfig {
                    registry_address: REGISTRY,
                }),
            };
            evm.set_sweep_candidate_allowance(1);
            let mut tx = main_tx();
            tx.inner.kind = TxKind::Call(TOKEN_B);

            let error = if inspect_one_only {
                evm.inspect_one_tx(tx.clone()).map(|_| ()).unwrap_err()
            } else {
                evm.inspect_tx(tx.clone()).map(|_| ()).unwrap_err()
            };
            assert!(matches!(error, EVMError::Database(OpcodeStorageError)));
            assert!(evm.take_sweep_outcome().is_none());
            assert_eq!(evm.sweep_candidate_allowance, None);
            assert!(evm.pre_fee_logs.is_empty());
            assert!(evm.post_fee_logs.is_empty());
            assert!(evm.frame_stack().index().is_none());
            assert!(
                evm.ctx_ref()
                    .local()
                    .shared_memory_buffer()
                    .borrow()
                    .is_empty()
            );

            let journal = evm.ctx_ref().journal();
            assert!(journal.state.is_empty());
            assert!(journal.journal.is_empty());
            assert!(journal.transient_storage.is_empty());
            assert!(journal.logs.is_empty());
            assert!(journal.warm_addresses.access_list().is_empty());
            assert!(journal.warm_addresses.coinbase().is_none());

            let later = evm.inspect_tx(tx).unwrap();
            assert!(later.result.is_success());
            assert_eq!(later.result.logs().len(), 1);
            assert_eq!(
                later.state[&TOKEN_B].storage[&U256::from(1)].present_value,
                U256::from(7)
            );
            assert_eq!(evm.take_sweep_outcome().unwrap(), SweepOutcome::default());
            assert!(evm.ctx_ref().journal().state.is_empty());
        }
    }

    #[test]
    fn later_transaction_discard_cannot_revert_completed_sweep() {
        let mut evm = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        evm.set_sweep_candidate_allowance(1);
        assert!(evm.transact_one(main_tx()).unwrap().is_success());
        let outcome = evm.take_sweep_outcome().unwrap();
        assert_eq!(outcome.logs.len(), 2);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);

        let mut invalid_tx = main_tx();
        invalid_tx.inner.nonce = 99;
        assert!(evm.transact_one(invalid_tx).is_err());

        let journal = evm.ctx_ref().journal();
        assert_eq!(
            journal.state[&TOKEN_A].storage[&U256::ZERO].present_value,
            U256::ZERO
        );
        assert!(journal.journal.is_empty());
        assert!(journal.transient_storage.is_empty());
        assert!(journal.warm_addresses.access_list().is_empty());
        assert!(journal.warm_addresses.coinbase().is_none());
        assert!(
            journal.state[&TOKEN_A].is_cold_transaction_id(journal.transaction_id),
            "completed sweep accounts must be cold for the next transaction"
        );
    }

    #[test]
    fn resolver_zero_malformed_revert_and_static_violation_are_skipped() {
        for (mode, expected) in [
            (ResolverMode::Zero, SweepFailureReason::ResolverZero),
            (
                ResolverMode::Malformed,
                SweepFailureReason::ResolverMalformed,
            ),
            (ResolverMode::Revert, SweepFailureReason::ResolverCallFailed),
            (
                ResolverMode::Mutating,
                SweepFailureReason::ResolverCallFailed,
            ),
        ] {
            let (mut evm, outcome) = execute_one(mode, BalanceMode::Normal, TransferMode::True);
            assert_eq!(only_failure(&outcome), expected);
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
            if matches!(mode, ResolverMode::Mutating) {
                assert_eq!(token_balance(&mut evm, REGISTRY), U256::ZERO);
            }
        }
    }

    #[test]
    fn deposit_code_and_eip7702_delegation_are_skipped() {
        let code_cases = [
            Bytecode::new_raw(Bytes::from_static(&[0x00])),
            Bytecode::new_eip7702(Address::with_last_byte(0xaa)),
        ];
        for code in code_cases {
            let mut evm = make_evm(
                ResolverMode::Master,
                &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
                Some(code),
                true,
            );
            let outcome = execute_sweeps(
                &mut evm,
                SweepConfig {
                    registry_address: REGISTRY,
                },
                &[candidate(TOKEN_A)],
                0,
                1,
            )
            .unwrap();

            assert_eq!(only_failure(&outcome), SweepFailureReason::DepositHasCode);
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
        }
    }

    #[test]
    fn balance_failures_skip_without_mutating_token_state() {
        for (mode, expected) in [
            (BalanceMode::Zero, SweepFailureReason::BalanceZero),
            (BalanceMode::Malformed, SweepFailureReason::BalanceMalformed),
            (BalanceMode::Revert, SweepFailureReason::BalanceCallFailed),
        ] {
            let (mut evm, outcome) = execute_one(ResolverMode::Master, mode, TransferMode::True);
            assert_eq!(only_failure(&outcome), expected);
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
        }
    }

    #[test]
    fn token_empty_and_true_returns_succeed() {
        for mode in [TransferMode::Empty, TransferMode::True] {
            let (mut evm, outcome) = execute_one(ResolverMode::Master, BalanceMode::Normal, mode);
            assert_eq!(outcome.successes.len(), 1);
            assert!(outcome.failures.is_empty());
            assert_eq!(outcome.logs.len(), 2);
            assert_eq!(outcome.successes[0].amount, U256::from(INITIAL_BALANCE));
            assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
        }
    }

    #[test]
    fn transfer_offset_points_to_matching_log_after_other_call_logs() {
        let (mut evm, outcome) = execute_one(
            ResolverMode::Master,
            BalanceMode::Normal,
            TransferMode::ExtraLog,
        );

        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.logs.len(), 3);
        assert_eq!(outcome.successes[0].transfer_log_offset, 1);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
    }

    #[test]
    fn transfer_offset_rejects_arithmetic_and_u32_overflow() {
        assert_eq!(checked_transfer_log_offset(4, 2, 1), Ok(7));
        assert_eq!(
            checked_transfer_log_offset(usize::MAX, 1, 0),
            Err(SweepInvariantError::TransferLogOffsetOverflow)
        );
        assert_eq!(
            checked_transfer_log_offset(u32::MAX as usize, 1, 0),
            Err(SweepInvariantError::TransferLogOffsetOverflow)
        );
    }

    #[test]
    fn token_validation_failures_revert_candidate_state_and_logs() {
        for (balance_mode, transfer_mode, expected) in [
            (
                BalanceMode::Normal,
                TransferMode::False,
                SweepFailureReason::TransferFalse,
            ),
            (
                BalanceMode::Normal,
                TransferMode::Malformed,
                SweepFailureReason::TransferMalformed,
            ),
            (
                BalanceMode::Normal,
                TransferMode::Revert,
                SweepFailureReason::TransferCallFailed,
            ),
            (
                BalanceMode::Normal,
                TransferMode::PostBalanceNonZero,
                SweepFailureReason::PostBalanceNonZero,
            ),
            (
                BalanceMode::MalformedAfterTransfer,
                TransferMode::True,
                SweepFailureReason::PostBalanceMalformed,
            ),
            (
                BalanceMode::RevertAfterTransfer,
                TransferMode::True,
                SweepFailureReason::PostBalanceCallFailed,
            ),
            (
                BalanceMode::Normal,
                TransferMode::MissingLog,
                SweepFailureReason::MissingTransferLog,
            ),
            (
                BalanceMode::Normal,
                TransferMode::DuplicateLog,
                SweepFailureReason::DuplicateTransferLog,
            ),
        ] {
            let (mut evm, outcome) = execute_one(ResolverMode::Master, balance_mode, transfer_mode);
            assert_eq!(only_failure(&outcome), expected);
            assert!(outcome.logs.is_empty());
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
        }
    }

    #[test]
    fn candidates_are_isolated_and_charge_fixed_budget() {
        let mut evm = make_evm(
            ResolverMode::Master,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::True),
                (TOKEN_B, BalanceMode::Normal, TransferMode::MissingLog),
            ],
            None,
            true,
        );
        let outcome = execute_sweeps(
            &mut evm,
            SweepConfig {
                registry_address: REGISTRY,
            },
            &[candidate(TOKEN_A), candidate(TOKEN_B)],
            4,
            2,
        )
        .unwrap();

        assert_eq!(outcome.checked_candidates, 2);
        assert_eq!(outcome.system_gas_used, 2 * CANDIDATE_SYSTEM_GAS);
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
        assert_eq!(
            token_balance(&mut evm, TOKEN_B),
            U256::from(INITIAL_BALANCE)
        );
        assert_eq!(outcome.logs.len(), 2);
        assert_eq!(outcome.successes[0].transfer_log_offset, 4);
    }

    #[test]
    fn trace_replay_context_shares_budget_and_clears_at_target() {
        clear_sweep_trace_replay();
        let first = Bytes::from_static(b"canonical-first");
        let second = Bytes::from_static(b"canonical-second");
        let first_hash = alloy_primitives::keccak256(&first);
        let second_hash = alloy_primitives::keccak256(&second);
        begin_sweep_trace_replay(vec![first_hash, second_hash]);
        assert!(sweep_trace_replay_transaction(Some(&Bytes::from_static(b"synthetic"))).is_none());
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));

        begin_sweep_trace_replay(vec![first_hash, second_hash]);
        let first_transaction =
            sweep_trace_replay_transaction(Some(&first)).expect("first transaction");
        assert_eq!(first_transaction.allowance, MAX_CANDIDATES_PER_TX);
        finish_sweep_trace_replay_transaction(Some(first_transaction), 16);

        set_sweep_trace_replay_target(second_hash);
        let second_transaction =
            sweep_trace_replay_transaction(Some(&second)).expect("second transaction");
        assert_eq!(second_transaction.allowance, MAX_CANDIDATES_PER_TX);
        finish_sweep_trace_replay_transaction(Some(second_transaction), 0);

        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));
        assert!(sweep_trace_replay_transaction(Some(&Bytes::from_static(b"synthetic"))).is_none());
    }

    #[test]
    fn trace_replay_error_clears_context_before_later_raw_inspection() {
        clear_sweep_trace_replay();
        let raw = Bytes::from_static(b"canonical-error");
        begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&raw)]);

        let mut evm = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        let mut invalid = main_tx();
        invalid.rlp_bytes = Some(raw);
        invalid.inner.nonce = 99;
        assert!(evm.inspect_one_tx(invalid).is_err());
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));

        let result = evm.inspect_one_tx(main_tx()).unwrap();
        assert!(result.is_success());
        assert_eq!(evm.take_sweep_outcome().unwrap(), SweepOutcome::default());
        assert_eq!(
            token_balance(&mut evm, TOKEN_A),
            U256::from(INITIAL_BALANCE)
        );
    }

    #[test]
    fn explicit_zero_allowance_cannot_fall_back_to_trace_context() {
        let raw = Bytes::from_static(b"canonical-explicit-zero");
        begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&raw)]);
        let mut evm = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        evm.set_sweep_candidate_allowance(0);
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));

        let mut tx = main_tx();
        tx.rlp_bytes = Some(raw);
        assert!(evm.transact_one(tx).unwrap().is_success());
        assert_eq!(evm.take_sweep_outcome().unwrap(), SweepOutcome::default());
        assert_eq!(
            token_balance(&mut evm, TOKEN_A),
            U256::from(INITIAL_BALANCE)
        );
    }

    #[test]
    fn trace_replay_scope_clears_context_on_error_and_panic() {
        let stale = Bytes::from_static(b"stale-before-scope");
        begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&stale)]);
        let pending_error = Bytes::from_static(b"pending-before-error");
        let error: Result<(), ()> = {
            let _scope = sweep_trace_replay_scope();
            TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));
            begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&pending_error)]);
            Err(())
        };
        assert!(error.is_err());
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));
        assert!(
            sweep_trace_replay_transaction(Some(&pending_error)).is_none(),
            "matching bytes from the failed replay must not inherit sweep authority"
        );

        let unwind = std::panic::catch_unwind(|| {
            let _scope = sweep_trace_replay_scope();
            let pending = Bytes::from_static(b"pending-before-panic");
            begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&pending)]);
            panic!("deliberate trace replay panic");
        });
        assert!(unwind.is_err());
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));
    }
}
