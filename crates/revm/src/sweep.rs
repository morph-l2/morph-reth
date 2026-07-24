//! Consensus execution support for bounded ERC-20 deposit sweeping.
//!
//! The module discovers transaction-local sweep triggers, executes isolated
//! post-transaction token calls, and returns receipt logs plus a block effect.
//! [`SweepBlockSession`] is the canonical seam shared by block execution and
//! trace replay: speculative transactions receive an immutable [`SweepTxPlan`],
//! while only committed [`SweepBlockEffect`] values advance block budget and Registry
//! request deduplication state.

use crate::{MorphEvm, MorphInvalidTransaction, MorphTxEnv, SweepConfig, handler::MorphEvmHandler};
use alloy_evm::Database;
use alloy_primitives::{Address, B256, Bytes, Log, U256, b256};
use revm::{
    context::{JournalEntry, TxEnv, result::EVMError},
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
/// Maximum raw Registry/Transfer triggers preflighted after one transaction.
pub const MAX_PREFLIGHTS_PER_TX: usize = 64;
/// Gas limit for the Registry resolver static call.
pub const RESOLVE_GAS_LIMIT: u64 = 50_000;
/// Gas limit for each ERC-20 `balanceOf` static call.
pub const BALANCE_OF_GAS_LIMIT: u64 = 50_000;
/// Gas limit for the fixed EIP-7702 sweep delegate call.
pub const SWEEP_GAS_LIMIT: u64 = 250_000;
/// Fixed worst-case system-gas debit for resolver plus two preflight balance calls.
pub const PREFLIGHT_SYSTEM_GAS: u64 = RESOLVE_GAS_LIMIT + 2 * BALANCE_OF_GAS_LIMIT;
/// Fixed worst-case debit for the delegate call plus two post-execution balance calls.
pub const SWEEP_EXECUTION_SYSTEM_GAS: u64 = SWEEP_GAS_LIMIT + 2 * BALANCE_OF_GAS_LIMIT;
/// Total system-gas debit for a candidate that reaches execution.
pub const CANDIDATE_SYSTEM_GAS: u64 = PREFLIGHT_SYSTEM_GAS + SWEEP_EXECUTION_SYSTEM_GAS;
/// Maximum combined preflight and execution debit for one transaction.
///
/// The two quotas are intentionally independent: a transaction can discard
/// invalid triggers without consuming its eligible-execution allowance.
pub const TX_SYSTEM_GAS: u64 = 10_400_000;
/// Maximum sweep system gas in one block.
pub const BLOCK_SYSTEM_GAS: u64 = 22_400_000;
/// Maximum sweep candidates checked in one block.
pub const MAX_CANDIDATES_PER_BLOCK: usize = (BLOCK_SYSTEM_GAS / CANDIDATE_SYSTEM_GAS) as usize;
/// Maximum raw triggers preflighted in one block.
pub const MAX_PREFLIGHTS_PER_BLOCK: usize = (BLOCK_SYSTEM_GAS / PREFLIGHT_SYSTEM_GAS) as usize;

const _: () = {
    assert!(PREFLIGHT_SYSTEM_GAS == RESOLVE_GAS_LIMIT + 2 * BALANCE_OF_GAS_LIMIT);
    assert!(SWEEP_EXECUTION_SYSTEM_GAS == SWEEP_GAS_LIMIT + 2 * BALANCE_OF_GAS_LIMIT);
    assert!(CANDIDATE_SYSTEM_GAS == PREFLIGHT_SYSTEM_GAS + SWEEP_EXECUTION_SYSTEM_GAS);
    assert!(MAX_CANDIDATES_PER_TX as u64 * CANDIDATE_SYSTEM_GAS <= TX_SYSTEM_GAS);
    assert!(TX_SYSTEM_GAS <= BLOCK_SYSTEM_GAS);
    assert!(MAX_CANDIDATES_PER_BLOCK == (BLOCK_SYSTEM_GAS / CANDIDATE_SYSTEM_GAS) as usize);
    assert!(MAX_PREFLIGHTS_PER_BLOCK == (BLOCK_SYSTEM_GAS / PREFLIGHT_SYSTEM_GAS) as usize);
};

#[derive(Debug)]
struct TraceReplayContext {
    transaction_hashes: Vec<B256>,
    next_transaction: usize,
    finish_hash: B256,
    session: SweepBlockSession,
}

#[derive(Debug, Clone)]
pub(crate) struct TraceReplayTransaction {
    transaction_hash: B256,
    pub(crate) plan: SweepTxPlan,
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
            session: SweepBlockSession::default(),
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
            plan: replay.session.plan(),
        })
    })
}

pub(crate) fn finish_sweep_trace_replay_transaction(
    transaction: Option<TraceReplayTransaction>,
    block_effect: &SweepBlockEffect,
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
        replay.session.commit(block_effect);
        replay.next_transaction += 1;
        if transaction.transaction_hash == replay.finish_hash
            || replay.next_transaction == replay.transaction_hashes.len()
        {
            *context = None;
        }
    });
}

pub(crate) fn initial_sweep_execution_mode() -> SweepExecutionMode {
    TRACE_REPLAY_CONTEXT.with(|context| {
        if context.borrow().is_some() {
            SweepExecutionMode::TraceReplay
        } else {
            SweepExecutionMode::Disabled
        }
    })
}

const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const REQUEST_TOPIC: B256 =
    b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");
const SWEEP_TOPIC: B256 = b256!("035b37215a69e14a80883933d6aa84f0919a67af9410a4a73e8a23baeca011f0");
const RESOLVE_SELECTOR: [u8; 4] = [0x9f, 0xaa, 0x2f, 0x2f];
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];
const SWEEP_SELECTOR: [u8; 4] = [0x62, 0xc0, 0x67, 0x67];

/// A token/deposit pair eligible for a sweep check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepCandidate {
    /// ERC-20 token contract.
    pub token: Address,
    /// Deposit address.
    pub deposit: Address,
}

/// Opaque block-level accounting produced by one sweep phase.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepBlockEffect {
    preflighted_candidates: usize,
    checked_candidates: usize,
    system_gas_used: u64,
    seen_registry_requests: Vec<SweepCandidate>,
}

impl SweepBlockEffect {
    /// Number of raw triggers that consumed policy preflight resources.
    pub const fn preflighted_candidates(&self) -> usize {
        self.preflighted_candidates
    }

    /// Number of policy-eligible candidates that consumed execution resources.
    pub const fn checked_candidates(&self) -> usize {
        self.checked_candidates
    }

    /// Fixed system-gas debit for preflight and execution.
    pub const fn system_gas_used(&self) -> u64 {
        self.system_gas_used
    }

    /// Newly preflighted Registry requests, in deterministic receipt order.
    pub fn seen_registry_requests(&self) -> &[SweepCandidate] {
        &self.seen_registry_requests
    }

    fn record_preflight(&mut self, trigger: SweepTrigger) {
        self.preflighted_candidates += 1;
        self.system_gas_used = self.system_gas_used.saturating_add(PREFLIGHT_SYSTEM_GAS);
        if trigger.kind == SweepTriggerKind::RegistryRequest {
            self.seen_registry_requests.push(trigger.candidate);
        }
    }

    fn record_execution(&mut self) {
        self.checked_candidates += 1;
        self.system_gas_used = self
            .system_gas_used
            .saturating_add(SWEEP_EXECUTION_SYSTEM_GAS);
    }
}

/// Explicit sweep authority and block state supplied to one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepTxPlan {
    candidate_allowance: usize,
    preflight_allowance: usize,
    system_gas_allowance: u64,
    seen_registry_requests: Vec<SweepCandidate>,
}

impl SweepTxPlan {
    /// Builds an isolated plan with no prior block-level request history.
    ///
    /// This is intended for single-transaction state tests. Canonical block
    /// execution should obtain plans from [`SweepBlockSession::plan`].
    pub fn single_transaction(candidate_allowance: usize) -> Self {
        let candidate_allowance = candidate_allowance.min(MAX_CANDIDATES_PER_TX);
        let preflight_allowance = if candidate_allowance == 0 {
            0
        } else {
            MAX_PREFLIGHTS_PER_TX
        };
        Self {
            candidate_allowance,
            preflight_allowance,
            system_gas_allowance: u64::try_from(preflight_allowance)
                .expect("per-transaction preflight allowance must fit in u64")
                .saturating_mul(PREFLIGHT_SYSTEM_GAS)
                .saturating_add(
                    u64::try_from(candidate_allowance)
                        .expect("per-transaction sweep allowance must fit in u64")
                        .saturating_mul(SWEEP_EXECUTION_SYSTEM_GAS),
                )
                .min(TX_SYSTEM_GAS),
            seen_registry_requests: Vec::new(),
        }
    }

    /// Maximum candidates that this transaction may check.
    pub const fn candidate_allowance(&self) -> usize {
        self.candidate_allowance
    }

    /// Maximum raw triggers that this transaction may preflight.
    pub const fn preflight_allowance(&self) -> usize {
        self.preflight_allowance
    }

    /// Maximum fixed system gas that this transaction may consume.
    pub const fn system_gas_allowance(&self) -> u64 {
        self.system_gas_allowance
    }

    /// Returns whether this Registry request was already consumed in the block.
    pub fn has_seen_registry_request(&self, candidate: SweepCandidate) -> bool {
        self.seen_registry_requests.contains(&candidate)
    }
}

/// Canonical sweep budget and request deduplication state for one block.
#[derive(Debug, Clone)]
pub struct SweepBlockSession {
    remaining_candidates: usize,
    remaining_preflights: usize,
    remaining_system_gas: u64,
    seen_registry_requests: HashSet<SweepCandidate>,
}

impl Default for SweepBlockSession {
    fn default() -> Self {
        Self {
            remaining_candidates: MAX_CANDIDATES_PER_BLOCK,
            remaining_preflights: MAX_PREFLIGHTS_PER_BLOCK,
            remaining_system_gas: BLOCK_SYSTEM_GAS,
            seen_registry_requests: HashSet::with_capacity(MAX_CANDIDATES_PER_BLOCK),
        }
    }
}

impl SweepBlockSession {
    /// Builds the immutable plan for the next speculative transaction.
    pub fn plan(&self) -> SweepTxPlan {
        let gas_allowance = self.remaining_system_gas.min(TX_SYSTEM_GAS);
        let candidate_allowance = self.remaining_candidates.min(MAX_CANDIDATES_PER_TX).min(
            usize::try_from(gas_allowance / CANDIDATE_SYSTEM_GAS)
                .expect("sweep candidate allowance must fit in usize"),
        );
        let preflight_allowance = self.remaining_preflights.min(MAX_PREFLIGHTS_PER_TX).min(
            usize::try_from(gas_allowance / PREFLIGHT_SYSTEM_GAS)
                .expect("sweep preflight allowance must fit in usize"),
        );
        let mut seen_registry_requests = self
            .seen_registry_requests
            .iter()
            .copied()
            .collect::<Vec<_>>();
        seen_registry_requests.sort_unstable_by(|left, right| {
            left.token
                .cmp(&right.token)
                .then_with(|| left.deposit.cmp(&right.deposit))
        });
        SweepTxPlan {
            candidate_allowance,
            preflight_allowance,
            system_gas_allowance: gas_allowance,
            seen_registry_requests,
        }
    }

    /// Applies the effect of a committed transaction.
    pub fn commit(&mut self, effect: &SweepBlockEffect) {
        let expected_system_gas = u64::try_from(effect.preflighted_candidates)
            .expect("sweep preflight count must fit in u64")
            .checked_mul(PREFLIGHT_SYSTEM_GAS)
            .and_then(|preflight_gas| {
                u64::try_from(effect.checked_candidates)
                    .expect("sweep candidate count must fit in u64")
                    .checked_mul(SWEEP_EXECUTION_SYSTEM_GAS)
                    .and_then(|execution_gas| preflight_gas.checked_add(execution_gas))
            })
            .expect("sweep system gas must not overflow");
        assert_eq!(
            effect.system_gas_used, expected_system_gas,
            "sweep block effect has inconsistent system gas"
        );
        self.remaining_candidates = self
            .remaining_candidates
            .checked_sub(effect.checked_candidates)
            .expect("committed sweep candidates exceed the block limit");
        self.remaining_preflights = self
            .remaining_preflights
            .checked_sub(effect.preflighted_candidates)
            .expect("committed sweep preflights exceed the block limit");
        self.remaining_system_gas = self
            .remaining_system_gas
            .checked_sub(effect.system_gas_used)
            .expect("committed sweep system gas exceeds the block limit");
        self.seen_registry_requests
            .extend(effect.seen_registry_requests.iter().copied());
    }

    /// Remaining candidate checks in the block.
    pub const fn remaining_candidates(&self) -> usize {
        self.remaining_candidates
    }

    /// Remaining trigger preflights in the block.
    pub const fn remaining_preflights(&self) -> usize {
        self.remaining_preflights
    }

    /// Remaining fixed system gas in the block.
    pub const fn remaining_system_gas(&self) -> u64 {
        self.remaining_system_gas
    }
}

/// How the next EVM transaction is authorized to execute sweeps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SweepExecutionMode {
    /// No sweep hook is allowed, including synthetic calls and estimates.
    #[default]
    Disabled,
    /// Canonical block execution with an explicit speculative plan.
    Canonical(SweepTxPlan),
    /// Canonical trace replay using the thread-local replay coordinator.
    TraceReplay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SweepTriggerKind {
    RegistryRequest,
    TokenTransfer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SweepTrigger {
    candidate: SweepCandidate,
    kind: SweepTriggerKind,
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
    /// Registry policy returned a zero or invalid token code hash.
    PolicyMalformed,
    /// Runtime token code does not match the Registry-pinned policy.
    TokenCodeMismatch,
    /// Deposit is not delegated to the chain-configured EIP-7702 implementation.
    DepositDelegationMismatch,
    /// Pre-transfer `balanceOf` reverted or halted.
    BalanceCallFailed,
    /// Pre-transfer `balanceOf` returned malformed data.
    BalanceMalformed,
    /// Deposit token balance is zero.
    BalanceZero,
    /// Deposit balance is below the Registry policy minimum.
    BelowMinimum,
    /// Master `balanceOf` reverted or halted.
    MasterBalanceCallFailed,
    /// Master `balanceOf` returned malformed data.
    MasterBalanceMalformed,
    /// Fixed sweep delegate reverted or halted.
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
    /// Master did not receive the full deposit balance.
    MasterBalanceMismatch,
    /// Transfer call emitted no matching canonical `Transfer`.
    MissingTransferLog,
    /// Transfer call emitted more than one matching canonical `Transfer`.
    DuplicateTransferLog,
    /// Sweep execution attempted a persistent side effect outside token storage.
    ScopeViolation,
    /// Candidate passed policy but could not fit in the remaining system budget.
    DeferredByBudget,
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
            Self::PolicyMalformed => "policy_malformed",
            Self::TokenCodeMismatch => "token_code_mismatch",
            Self::DepositDelegationMismatch => "deposit_delegation_mismatch",
            Self::BalanceCallFailed => "balance_call_failed",
            Self::BalanceMalformed => "balance_malformed",
            Self::BalanceZero => "balance_zero",
            Self::BelowMinimum => "below_minimum",
            Self::MasterBalanceCallFailed => "master_balance_call_failed",
            Self::MasterBalanceMalformed => "master_balance_malformed",
            Self::TransferCallFailed => "transfer_call_failed",
            Self::TransferFalse => "transfer_false",
            Self::TransferMalformed => "transfer_malformed",
            Self::PostBalanceCallFailed => "post_balance_call_failed",
            Self::PostBalanceMalformed => "post_balance_malformed",
            Self::PostBalanceNonZero => "post_balance_non_zero",
            Self::MasterBalanceMismatch => "master_balance_mismatch",
            Self::MissingTransferLog => "missing_transfer_log",
            Self::DuplicateTransferLog => "duplicate_transfer_log",
            Self::ScopeViolation => "scope_violation",
            Self::DeferredByBudget => "deferred_by_budget",
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
    /// Block-level budget and request-deduplication effect.
    pub block_effect: SweepBlockEffect,
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

fn parse_transfer_candidate(log: &Log) -> Option<SweepCandidate> {
    let topics = log.topics();
    (topics.len() == 3 && topics[0] == TRANSFER_TOPIC && log.data.data.len() == 32).then(|| {
        SweepCandidate {
            token: log.address,
            deposit: address_from_topic(topics[2]),
        }
    })
}

fn parse_registry_request(log: &Log, registry: Address) -> Option<SweepCandidate> {
    let topics = log.topics();
    (log.address == registry
        && topics.len() == 3
        && topics[0] == REQUEST_TOPIC
        && log.data.data.is_empty()
        && is_canonical_address_topic(topics[1])
        && is_canonical_address_topic(topics[2]))
    .then(|| SweepCandidate {
        token: address_from_topic(topics[1]),
        deposit: address_from_topic(topics[2]),
    })
}

/// Parses one exact ERC-20 transfer or Registry sweep-request log.
pub fn parse_sweep_candidate(log: &Log, registry: Address) -> Option<SweepCandidate> {
    // The `Transfer` branch deliberately does NOT require canonical address
    // topics: it takes the low 20 bytes of `topics[2]` regardless of the high
    // bytes, matching go-ethereum's `common.BytesToAddress`. Keeping the same
    // lenient extraction is what makes reth and geth agree on the candidate set
    // for a non-canonically-encoded token log. The `SweepRequested`
    // branch below is strict instead, because that event is emitted only by the
    // Registry, whose Solidity `indexed address` topics are always zero-padded;
    // a non-canonical request topic therefore cannot originate from the real
    // Registry and must be rejected.
    if let Some(candidate) = parse_transfer_candidate(log) {
        return Some(candidate);
    }
    parse_registry_request(log, registry)
}

pub(crate) fn collect_sweep_triggers(
    main_logs: &[Log],
    registry: Address,
    plan: &SweepTxPlan,
) -> Vec<SweepTrigger> {
    collect_transaction_sweep_triggers(main_logs, None, registry, plan)
}

pub(crate) fn collect_transaction_sweep_triggers(
    main_logs: &[Log],
    post_fee_candidate: Option<SweepCandidate>,
    registry: Address,
    plan: &SweepTxPlan,
) -> Vec<SweepTrigger> {
    let mut seen = HashSet::with_capacity(plan.preflight_allowance());
    let mut triggers = Vec::with_capacity(plan.preflight_allowance());

    for log in main_logs {
        if triggers.len() == plan.preflight_allowance() {
            break;
        }
        let Some(candidate) = parse_registry_request(log, registry) else {
            continue;
        };
        if !plan.has_seen_registry_request(candidate) && seen.insert(candidate) {
            triggers.push(SweepTrigger {
                candidate,
                kind: SweepTriggerKind::RegistryRequest,
            });
        }
    }

    let transfers = main_logs
        .iter()
        .filter_map(parse_transfer_candidate)
        .chain(post_fee_candidate);
    for candidate in transfers {
        if triggers.len() == plan.preflight_allowance() {
            break;
        }
        if seen.insert(candidate) {
            triggers.push(SweepTrigger {
                candidate,
                kind: SweepTriggerKind::TokenTransfer,
            });
        }
    }

    triggers
}

/// Collects first-seen, deduplicated sweep candidates with Registry requests first.
pub fn collect_sweep_candidates(main_logs: &[Log], registry: Address) -> Vec<SweepCandidate> {
    collect_sweep_triggers(main_logs, registry, &SweepBlockSession::default().plan())
        .into_iter()
        .map(|trigger| trigger.candidate)
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
fn encode_sweep_call(token: Address, recipient: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(100);
    data.extend_from_slice(&SWEEP_SELECTOR);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(token.as_slice());
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedSweepPolicy {
    master: Address,
    token_code_hash: B256,
    minimum_amount: U256,
}

fn decode_sweep_policy(output: &Bytes) -> Result<ResolvedSweepPolicy, SweepFailureReason> {
    if output.len() != 96 {
        return Err(SweepFailureReason::ResolverMalformed);
    }
    let master = decode_address(&output.slice(..32))?;
    let token_code_hash = B256::from_slice(&output[32..64]);
    let minimum_amount = U256::from_be_slice(&output[64..96]);
    if token_code_hash.is_zero() || minimum_amount.is_zero() || master.is_zero() {
        return Err(SweepFailureReason::PolicyMalformed);
    }
    Ok(ResolvedSweepPolicy {
        master,
        token_code_hash,
        minimum_amount,
    })
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

fn journal_is_within_sweep_scope(entries: &[JournalEntry], candidate: SweepCandidate) -> bool {
    entries.iter().all(|entry| match entry {
        JournalEntry::AccountWarmed { .. } | JournalEntry::StorageWarmed { .. } => true,
        JournalEntry::AccountTouched { address } => {
            *address == candidate.deposit || *address == candidate.token
        }
        JournalEntry::StorageChanged { address, .. } => *address == candidate.token,
        JournalEntry::AccountDestroyed { .. }
        | JournalEntry::BalanceChange { .. }
        | JournalEntry::BalanceTransfer { .. }
        | JournalEntry::NonceChange { .. }
        | JournalEntry::NonceBump { .. }
        | JournalEntry::AccountCreated { .. }
        | JournalEntry::TransientStorageChange { .. }
        | JournalEntry::CodeChange { .. } => false,
    })
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
    tracing::debug!(
        token = ?candidate.token,
        deposit = ?candidate.deposit,
        reason = reason.as_label(),
        "sweep candidate did not settle"
    );
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

#[derive(Debug, Clone, Copy)]
struct PreparedSweep {
    candidate: SweepCandidate,
    master: Address,
    amount: U256,
    master_balance_before: U256,
}

fn read_token_balance<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    token: Address,
    account: Address,
    call_failed: SweepFailureReason,
    malformed: SweepFailureReason,
) -> Result<Result<U256, SweepFailureReason>, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let balance = internal_call(
        evm,
        Address::ZERO,
        token,
        encode_address_call(BALANCE_OF_SELECTOR, account),
        BALANCE_OF_GAS_LIMIT,
        true,
    )?;
    let Some(output) = balance.output else {
        return Ok(Err(call_failed));
    };
    Ok(decode_balance(&output, malformed))
}

fn preflight_candidate<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidate: SweepCandidate,
) -> Result<Result<PreparedSweep, SweepFailureReason>, EVMError<DB::Error, MorphInvalidTransaction>>
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
        return Ok(Err(SweepFailureReason::ResolverCallFailed));
    };
    let policy = match decode_sweep_policy(&resolver_output) {
        Ok(policy) => policy,
        Err(reason) => {
            return Ok(Err(reason));
        }
    };
    if policy.master == candidate.deposit {
        return Ok(Err(SweepFailureReason::PolicyMalformed));
    }

    let token_matches_policy = {
        let account = evm
            .ctx_mut()
            .journal_mut()
            .load_account_with_code(candidate.token)?;
        account
            .info
            .code
            .as_ref()
            .is_some_and(|code| !code.is_empty() && code.eip7702_address().is_none())
            && account.info.code_hash() == policy.token_code_hash
    };
    if !token_matches_policy {
        return Ok(Err(SweepFailureReason::TokenCodeMismatch));
    }

    let delegated_address = evm
        .ctx_mut()
        .journal_mut()
        .load_account_with_code(candidate.deposit)?
        .info
        .code
        .as_ref()
        .and_then(Bytecode::eip7702_address);
    if delegated_address != Some(config.delegate_address) {
        return Ok(Err(SweepFailureReason::DepositDelegationMismatch));
    }
    let delegate_matches_config = {
        let account = evm
            .ctx_mut()
            .journal_mut()
            .load_account_with_code(config.delegate_address)?;
        account
            .info
            .code
            .as_ref()
            .is_some_and(|code| !code.is_empty() && code.eip7702_address().is_none())
            && account.info.code_hash() == config.delegate_code_hash
    };
    if !delegate_matches_config {
        return Ok(Err(SweepFailureReason::DepositDelegationMismatch));
    }

    let balance = match read_token_balance(
        evm,
        candidate.token,
        candidate.deposit,
        SweepFailureReason::BalanceCallFailed,
        SweepFailureReason::BalanceMalformed,
    )? {
        Ok(balance) => balance,
        Err(reason) => return Ok(Err(reason)),
    };
    if balance.is_zero() {
        return Ok(Err(SweepFailureReason::BalanceZero));
    }
    if balance < policy.minimum_amount {
        return Ok(Err(SweepFailureReason::BelowMinimum));
    }
    let master_balance_before = match read_token_balance(
        evm,
        candidate.token,
        policy.master,
        SweepFailureReason::MasterBalanceCallFailed,
        SweepFailureReason::MasterBalanceMalformed,
    )? {
        Ok(balance) => balance,
        Err(reason) => return Ok(Err(reason)),
    };

    Ok(Ok(PreparedSweep {
        candidate,
        master: policy.master,
        amount: balance,
        master_balance_before,
    }))
}

fn execute_prepared_sweep<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    prepared: PreparedSweep,
    receipt_prefix_logs: usize,
    outcome: &mut SweepOutcome,
) -> Result<(), EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let candidate = prepared.candidate;
    let master = prepared.master;
    let balance = prepared.amount;

    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
    let log_start = checkpoint.log_i;
    let candidate_result = (|| {
        let transfer = internal_call(
            evm,
            config.delegate_address,
            candidate.deposit,
            encode_sweep_call(candidate.token, master, balance),
            SWEEP_GAS_LIMIT,
            false,
        )?;
        let Some(transfer_output) = transfer.output else {
            return Ok(Err(SweepFailureReason::TransferCallFailed));
        };
        if let Err(reason) = classify_transfer_output(&transfer_output) {
            return Ok(Err(reason));
        }

        let post_balance = match read_token_balance(
            evm,
            candidate.token,
            candidate.deposit,
            SweepFailureReason::PostBalanceCallFailed,
            SweepFailureReason::PostBalanceMalformed,
        )? {
            Ok(balance) => balance,
            Err(reason) => return Ok(Err(reason)),
        };
        if !post_balance.is_zero() {
            return Ok(Err(SweepFailureReason::PostBalanceNonZero));
        }

        let master_balance_after = match read_token_balance(
            evm,
            candidate.token,
            master,
            SweepFailureReason::MasterBalanceCallFailed,
            SweepFailureReason::MasterBalanceMalformed,
        )? {
            Ok(balance) => balance,
            Err(reason) => return Ok(Err(reason)),
        };
        if master_balance_after.checked_sub(prepared.master_balance_before) != Some(balance) {
            return Ok(Err(SweepFailureReason::MasterBalanceMismatch));
        }

        let journal = evm.ctx_ref().journal();
        if !journal_is_within_sweep_scope(&journal.journal[checkpoint.journal_i..], candidate) {
            return Ok(Err(SweepFailureReason::ScopeViolation));
        }

        let call_logs = &journal.logs[log_start..];
        let mut matching_logs = call_logs
            .iter()
            .enumerate()
            .filter(|(_, log)| is_matching_transfer(log, candidate, master, balance));
        let Some((matching_log_offset, _)) = matching_logs.next() else {
            return Ok(Err(SweepFailureReason::MissingTransferLog));
        };
        if matching_logs.next().is_some() {
            return Ok(Err(SweepFailureReason::DuplicateTransferLog));
        }
        if call_logs.len() != 1 {
            return Ok(Err(SweepFailureReason::ScopeViolation));
        }
        Ok(Ok(matching_log_offset))
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
#[cfg(test)]
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
    let triggers = candidates
        .iter()
        .copied()
        .map(|candidate| SweepTrigger {
            candidate,
            kind: SweepTriggerKind::TokenTransfer,
        })
        .collect::<Vec<_>>();
    let plan = SweepTxPlan::single_transaction(allowance);
    execute_sweep_triggers(evm, config, &triggers, receipt_prefix_logs, &plan)
}

pub(crate) fn execute_sweep_triggers<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    triggers: &[SweepTrigger],
    receipt_prefix_logs: usize,
    plan: &SweepTxPlan,
) -> Result<SweepOutcome, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let mut outcome = SweepOutcome::default();
    for trigger in triggers.iter().copied().take(plan.preflight_allowance()) {
        if outcome.block_effect.checked_candidates() == plan.candidate_allowance() {
            break;
        }
        let next_preflight_gas = outcome
            .block_effect
            .system_gas_used()
            .saturating_add(PREFLIGHT_SYSTEM_GAS);
        if next_preflight_gas > plan.system_gas_allowance() {
            break;
        }
        outcome.block_effect.record_preflight(trigger);

        let prepared = match preflight_candidate(evm, config, trigger.candidate)? {
            Ok(prepared) => prepared,
            Err(reason) => {
                push_failure(&mut outcome, trigger.candidate, reason);
                continue;
            }
        };

        let next_execution_gas = outcome
            .block_effect
            .system_gas_used()
            .saturating_add(SWEEP_EXECUTION_SYSTEM_GAS);
        if next_execution_gas > plan.system_gas_allowance() {
            push_failure(
                &mut outcome,
                trigger.candidate,
                SweepFailureReason::DeferredByBudget,
            );
            break;
        }
        outcome.block_effect.record_execution();
        execute_prepared_sweep(evm, config, prepared, receipt_prefix_logs, &mut outcome)?;
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
    const DELEGATE: Address = address!("5300000000000000000000000000000000000024");
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
            SweepFailureReason::PolicyMalformed,
            SweepFailureReason::TokenCodeMismatch,
            SweepFailureReason::DepositDelegationMismatch,
            SweepFailureReason::BalanceCallFailed,
            SweepFailureReason::BalanceMalformed,
            SweepFailureReason::BalanceZero,
            SweepFailureReason::BelowMinimum,
            SweepFailureReason::MasterBalanceCallFailed,
            SweepFailureReason::MasterBalanceMalformed,
            SweepFailureReason::TransferCallFailed,
            SweepFailureReason::TransferFalse,
            SweepFailureReason::TransferMalformed,
            SweepFailureReason::PostBalanceCallFailed,
            SweepFailureReason::PostBalanceMalformed,
            SweepFailureReason::PostBalanceNonZero,
            SweepFailureReason::MasterBalanceMismatch,
            SweepFailureReason::MissingTransferLog,
            SweepFailureReason::DuplicateTransferLog,
            SweepFailureReason::ScopeViolation,
            SweepFailureReason::DeferredByBudget,
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
    fn registry_requests_take_priority_over_transfer_candidates() {
        let requested = SweepCandidate {
            token: TOKEN_B,
            deposit: DEPOSIT,
        };
        let mut logs = (0_u8..MAX_CANDIDATES_PER_TX as u8)
            .map(|index| {
                transfer_log(
                    Address::with_last_byte(index.saturating_add(10)),
                    Address::ZERO,
                    Address::with_last_byte(index.saturating_add(100)),
                    U256::from(1),
                )
            })
            .collect::<Vec<_>>();
        logs.push(request_log(requested.token, requested.deposit));

        let candidates = collect_sweep_candidates(&logs, REGISTRY);

        assert_eq!(candidates.len(), MAX_CANDIDATES_PER_TX);
        assert_eq!(candidates[0], requested);
    }

    #[test]
    fn refund_candidate_follows_main_transfers_and_shares_deduplication() {
        let requested = candidate(TOKEN_B);
        let main_transfer = candidate(TOKEN_A);
        let refund = SweepCandidate {
            token: Address::with_last_byte(5),
            deposit: DEPOSIT,
        };
        let logs = vec![
            transfer_log(
                main_transfer.token,
                Address::ZERO,
                main_transfer.deposit,
                U256::from(1),
            ),
            request_log(requested.token, requested.deposit),
        ];
        let plan = SweepBlockSession::default().plan();

        let triggers = collect_transaction_sweep_triggers(&logs, Some(refund), REGISTRY, &plan);

        assert_eq!(
            triggers,
            vec![
                SweepTrigger {
                    candidate: requested,
                    kind: SweepTriggerKind::RegistryRequest,
                },
                SweepTrigger {
                    candidate: main_transfer,
                    kind: SweepTriggerKind::TokenTransfer,
                },
                SweepTrigger {
                    candidate: refund,
                    kind: SweepTriggerKind::TokenTransfer,
                },
            ]
        );
        assert_eq!(
            collect_transaction_sweep_triggers(&logs, Some(main_transfer), REGISTRY, &plan),
            triggers[..2]
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
        WrongCodeHash,
        MinimumTooHigh,
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
        MissingMasterCredit,
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

        fn jump(&mut self, label: &'static str) {
            self.op(0x61);
            let offset = self.code.len();
            self.code.extend_from_slice(&[0, 0]);
            self.fixups.push((offset, label));
            self.op(0x56);
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

    fn sweep_delegate_code() -> Bytes {
        let mut asm = Assembler::new();

        asm.push(DELEGATE.as_slice());
        asm.op(0x33);
        asm.op(0x14);
        asm.jumpi("authorized");
        asm.jump("failure");

        asm.label("authorized");
        asm.push(&[0xa9, 0x05, 0x9c, 0xbb]);
        asm.push_u8(0xe0);
        asm.op(0x1b);
        asm.push_u8(0);
        asm.op(0x52);
        asm.push_u8(36);
        asm.op(0x35);
        asm.push_u8(4);
        asm.op(0x52);
        asm.push_u8(68);
        asm.op(0x35);
        asm.push_u8(36);
        asm.op(0x52);

        asm.push_u8(32);
        asm.push_u8(0);
        asm.push_u8(68);
        asm.push_u8(0);
        asm.push_u8(0);
        asm.push_u8(4);
        asm.op(0x35);
        asm.op(0x5a);
        asm.op(0xf1);
        asm.op(0x15);
        asm.jumpi("failure");

        asm.op(0x3d);
        asm.op(0x80);
        asm.op(0x15);
        asm.jumpi("empty_return");
        asm.push_u8(32);
        asm.op(0x14);
        asm.op(0x15);
        asm.jumpi("failure");
        asm.push_u8(0);
        asm.op(0x51);
        asm.op(0x15);
        asm.jumpi("failure");
        asm.jump("success");

        asm.label("empty_return");
        asm.op(0x50);
        asm.label("success");
        asm.push_u8(0);
        asm.push_u8(0);
        asm.op(0xf3);

        asm.label("failure");
        asm.push_u8(0);
        asm.push_u8(0);
        asm.op(0xfd);
        asm.finish()
    }

    fn test_sweep_config() -> SweepConfig {
        let code = Bytecode::new_raw(sweep_delegate_code());
        SweepConfig {
            registry_address: REGISTRY,
            delegate_address: DELEGATE,
            delegate_code_hash: code.hash_slow(),
        }
    }

    fn registry_code(mode: ResolverMode) -> Bytes {
        let mut asm = Assembler::new();
        match mode {
            ResolverMode::Master | ResolverMode::WrongCodeHash | ResolverMode::MinimumTooHigh => {
                asm.push_b256(address_topic(MASTER));
                asm.push_u8(0);
                asm.op(0x52);
                if matches!(mode, ResolverMode::WrongCodeHash) {
                    asm.push_b256(B256::repeat_byte(0x99));
                } else {
                    asm.push_u8(4);
                    asm.op(0x35);
                    asm.op(0x3f);
                }
                asm.push_u8(32);
                asm.op(0x52);
                asm.push_u8(if matches!(mode, ResolverMode::MinimumTooHigh) {
                    10
                } else {
                    1
                });
                asm.push_u8(64);
                asm.op(0x52);
                asm.push_u8(96);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            ResolverMode::Zero => {
                asm.push_u8(96);
                asm.push_u8(0);
                asm.op(0xf3);
            }
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
                asm.push_u8(4);
                asm.op(0x35);
                asm.push(MASTER.as_slice());
                asm.op(0x14);
                asm.jumpi("master_balance");
                asm.push_u8(0);
                asm.op(0x54);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
                asm.label("master_balance");
                asm.push_u8(1);
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
        asm.op(0x32);
        asm.push(DELEGATE.as_slice());
        asm.op(0x14);
        asm.jumpi("expected_origin");
        asm.push_u8(0);
        asm.push_u8(0);
        asm.op(0xfd);
        asm.label("expected_origin");
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
        if !matches!(transfer_mode, TransferMode::MissingMasterCredit) {
            asm.push_u8(36);
            asm.op(0x35);
            asm.push_u8(1);
            asm.op(0x54);
            asm.op(0x01);
            asm.push_u8(1);
            asm.op(0x55);
        }

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
            | TransferMode::MissingMasterCredit
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
        insert_code(&mut db, DELEGATE, sweep_delegate_code());
        for (token, balance_mode, transfer_mode) in tokens {
            insert_code(
                &mut db,
                *token,
                token_code_with_main(*balance_mode, *transfer_mode, main_mode),
            );
            db.insert_account_storage(*token, U256::ZERO, U256::from(INITIAL_BALANCE))
                .unwrap();
        }
        let code = deposit_code.unwrap_or_else(|| Bytecode::new_eip7702(DELEGATE));
        db.insert_account_info(
            DEPOSIT,
            AccountInfo {
                code_hash: code.hash_slow(),
                code: Some(code),
                ..Default::default()
            },
        );

        let mut evm = MorphEvm::new(MorphContext::new(db, MorphHardfork::Onyx), NoOpInspector);
        evm.block = MorphBlockEnv {
            inner: BlockEnv::default(),
            sweep: enabled.then_some(test_sweep_config()),
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
        let outcome =
            execute_sweeps(&mut evm, test_sweep_config(), &[candidate(TOKEN_A)], 0, 1).unwrap();
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
        insert_code(&mut db, DELEGATE, sweep_delegate_code());
        db.insert_account_info(
            DEPOSIT,
            AccountInfo {
                code_hash: Bytecode::new_eip7702(DELEGATE).hash_slow(),
                code: Some(Bytecode::new_eip7702(DELEGATE)),
                ..Default::default()
            },
        );
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
            sweep: Some(test_sweep_config()),
        };
        evm.tx.inner.caller = Address::with_last_byte(0xee);
        evm.tx.inner.gas_limit = 777;
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(2),
        ));
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
        disabled.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
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
        reverted.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
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
        enabled.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
        let result = enabled.transact_one(main_tx()).unwrap();
        let outcome = enabled.take_sweep_outcome().unwrap();
        assert!(result.is_success());
        assert_eq!(result.logs().len(), 1);
        assert_eq!(result.output(), Some(&Bytes::new()));
        assert_eq!(outcome.successes.len(), 1);
        assert!(enabled.take_sweep_outcome().is_none());
    }

    #[test]
    fn fee_logs_are_receipt_only_and_refund_candidate_preserves_offsets() {
        let mut evm = make_evm(
            ResolverMode::Master,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::True),
                (TOKEN_B, BalanceMode::Normal, TransferMode::True),
            ],
            None,
            true,
        );
        evm.pre_fee_logs = vec![transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(1))];
        evm.post_fee_logs = vec![transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(1))];
        evm.post_fee_sweep_candidate = Some(candidate(TOKEN_B));
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(2),
        ));
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: Vec::new(),
            output: Output::Call(Bytes::new()),
        };

        evm.apply_sweep(&result).unwrap();
        let outcome = evm.take_sweep_outcome().unwrap();

        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.successes[0].candidate, candidate(TOKEN_B));
        assert_eq!(outcome.successes[0].transfer_log_offset, 2);
        assert_eq!(
            token_balance(&mut evm, TOKEN_A),
            U256::from(INITIAL_BALANCE)
        );
    }

    #[test]
    fn main_log_and_slot_fee_refund_trigger_sweeps_in_order() {
        let mut evm = make_evm(
            ResolverMode::Master,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::True),
                (TOKEN_B, BalanceMode::Normal, TransferMode::True),
            ],
            None,
            true,
        );
        evm.post_fee_sweep_candidate = Some(candidate(TOKEN_B));
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(2),
        ));
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![transfer_log(TOKEN_A, Address::ZERO, DEPOSIT, U256::from(1))],
            output: Output::Call(Bytes::new()),
        };

        evm.apply_sweep(&result).unwrap();
        let outcome = evm.take_sweep_outcome().unwrap();

        assert_eq!(outcome.successes.len(), 2);
        assert_eq!(outcome.successes[0].candidate, candidate(TOKEN_A));
        assert_eq!(outcome.successes[0].transfer_log_offset, 1);
        assert_eq!(outcome.successes[1].candidate, candidate(TOKEN_B));
        assert_eq!(outcome.successes[1].transfer_log_offset, 3);
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
        replay.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
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
        inspected.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
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
            insert_code(&mut db, DELEGATE, sweep_delegate_code());
            db.insert_account_info(
                DEPOSIT,
                AccountInfo {
                    code_hash: Bytecode::new_eip7702(DELEGATE).hash_slow(),
                    code: Some(Bytecode::new_eip7702(DELEGATE)),
                    ..Default::default()
                },
            );
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
                sweep: Some(test_sweep_config()),
            };
            evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
                SweepTxPlan::single_transaction(1),
            ));
            let mut tx = main_tx();
            tx.inner.kind = TxKind::Call(TOKEN_B);

            let error = if inspect_one_only {
                evm.inspect_one_tx(tx.clone()).map(|_| ()).unwrap_err()
            } else {
                evm.inspect_tx(tx.clone()).map(|_| ()).unwrap_err()
            };
            assert!(matches!(error, EVMError::Database(OpcodeStorageError)));
            assert!(evm.take_sweep_outcome().is_none());
            assert_eq!(evm.sweep_execution_mode, SweepExecutionMode::Disabled);
            assert!(evm.pre_fee_logs.is_empty());
            assert!(evm.post_fee_logs.is_empty());
            assert!(evm.post_fee_sweep_candidate.is_none());
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
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(1),
        ));
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
    fn pinned_token_code_and_minimum_are_enforced_before_execution() {
        for (mode, expected) in [
            (
                ResolverMode::WrongCodeHash,
                SweepFailureReason::TokenCodeMismatch,
            ),
            (
                ResolverMode::MinimumTooHigh,
                SweepFailureReason::BelowMinimum,
            ),
        ] {
            let (mut evm, outcome) = execute_one(mode, BalanceMode::Normal, TransferMode::True);
            assert_eq!(only_failure(&outcome), expected);
            assert_eq!(outcome.block_effect.preflighted_candidates(), 1);
            assert_eq!(outcome.block_effect.checked_candidates(), 0);
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
        }
    }

    #[test]
    fn journal_scope_allows_only_token_storage_and_nonpersistent_warming() {
        let candidate = candidate(TOKEN_A);
        let allowed = vec![
            JournalEntry::AccountWarmed {
                address: Address::with_last_byte(0xfe),
            },
            JournalEntry::StorageWarmed {
                address: Address::with_last_byte(0xfd),
                key: U256::ZERO,
            },
            JournalEntry::AccountTouched { address: DEPOSIT },
            JournalEntry::AccountTouched { address: TOKEN_A },
            JournalEntry::StorageChanged {
                address: TOKEN_A,
                key: U256::ZERO,
                had_value: U256::from(INITIAL_BALANCE),
            },
        ];
        assert!(journal_is_within_sweep_scope(&allowed, candidate));

        for forbidden in [
            JournalEntry::StorageChanged {
                address: MASTER,
                key: U256::ZERO,
                had_value: U256::ZERO,
            },
            JournalEntry::BalanceChange {
                address: TOKEN_A,
                old_balance: U256::ZERO,
            },
            JournalEntry::NonceBump { address: TOKEN_A },
            JournalEntry::TransientStorageChange {
                address: TOKEN_A,
                key: U256::ZERO,
                had_value: U256::ZERO,
            },
            JournalEntry::CodeChange { address: TOKEN_A },
        ] {
            assert!(!journal_is_within_sweep_scope(&[forbidden], candidate));
        }
    }

    #[test]
    fn ordinary_code_and_wrong_eip7702_delegation_are_skipped() {
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
            let outcome =
                execute_sweeps(&mut evm, test_sweep_config(), &[candidate(TOKEN_A)], 0, 1).unwrap();

            assert_eq!(
                only_failure(&outcome),
                SweepFailureReason::DepositDelegationMismatch
            );
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
    fn extra_transfer_call_logs_violate_the_sweep_scope() {
        let (mut evm, outcome) = execute_one(
            ResolverMode::Master,
            BalanceMode::Normal,
            TransferMode::ExtraLog,
        );

        assert!(outcome.successes.is_empty());
        assert!(outcome.logs.is_empty());
        assert_eq!(only_failure(&outcome), SweepFailureReason::ScopeViolation);
        assert_eq!(
            token_balance(&mut evm, TOKEN_A),
            U256::from(INITIAL_BALANCE)
        );
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
                SweepFailureReason::TransferCallFailed,
            ),
            (
                BalanceMode::Normal,
                TransferMode::Malformed,
                SweepFailureReason::TransferCallFailed,
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
                BalanceMode::Normal,
                TransferMode::MissingMasterCredit,
                SweepFailureReason::MasterBalanceMismatch,
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
            test_sweep_config(),
            &[candidate(TOKEN_A), candidate(TOKEN_B)],
            4,
            2,
        )
        .unwrap();

        assert_eq!(outcome.block_effect.checked_candidates, 2);
        assert_eq!(
            outcome.block_effect.system_gas_used,
            2 * CANDIDATE_SYSTEM_GAS
        );
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
    fn block_session_commits_budget_and_only_deduplicates_registry_requests() {
        let request = candidate(TOKEN_A);
        let transfer = candidate(TOKEN_B);
        let mut session = SweepBlockSession::default();

        let first = session.plan();
        assert_eq!(first.candidate_allowance(), MAX_CANDIDATES_PER_TX);
        assert_eq!(first.preflight_allowance(), MAX_PREFLIGHTS_PER_TX);
        assert_eq!(first.system_gas_allowance(), TX_SYSTEM_GAS);
        assert!(!first.has_seen_registry_request(request));
        assert!(!first.has_seen_registry_request(transfer));

        session.commit(&SweepBlockEffect {
            preflighted_candidates: 2,
            checked_candidates: 2,
            system_gas_used: 2 * CANDIDATE_SYSTEM_GAS,
            seen_registry_requests: vec![request],
        });

        let next = session.plan();
        assert_eq!(next.candidate_allowance(), MAX_CANDIDATES_PER_TX);
        assert!(next.has_seen_registry_request(request));
        assert!(!next.has_seen_registry_request(transfer));
        assert_eq!(session.remaining_candidates(), MAX_CANDIDATES_PER_BLOCK - 2);
        assert_eq!(
            session.remaining_system_gas(),
            BLOCK_SYSTEM_GAS - 2 * CANDIDATE_SYSTEM_GAS
        );
    }

    #[test]
    fn invalid_triggers_can_use_the_full_preflight_quota_without_execution_debits() {
        let mut evm = make_evm(ResolverMode::Zero, &[], None, true);
        let triggers = (1..=MAX_PREFLIGHTS_PER_TX)
            .map(|index| SweepTrigger {
                candidate: SweepCandidate {
                    token: Address::with_last_byte(u8::try_from(index).unwrap()),
                    deposit: DEPOSIT,
                },
                kind: SweepTriggerKind::TokenTransfer,
            })
            .collect::<Vec<_>>();

        let outcome = execute_sweep_triggers(
            &mut evm,
            test_sweep_config(),
            &triggers,
            0,
            &SweepBlockSession::default().plan(),
        )
        .unwrap();

        assert_eq!(
            outcome.block_effect.preflighted_candidates(),
            MAX_PREFLIGHTS_PER_TX
        );
        assert_eq!(outcome.block_effect.checked_candidates(), 0);
        assert_eq!(
            outcome.block_effect.system_gas_used(),
            MAX_PREFLIGHTS_PER_TX as u64 * PREFLIGHT_SYSTEM_GAS
        );
        assert_eq!(outcome.failures.len(), MAX_PREFLIGHTS_PER_TX);
    }

    #[test]
    fn seen_request_is_skipped_but_a_later_transfer_for_the_pair_remains_eligible() {
        let pair = candidate(TOKEN_A);
        let mut session = SweepBlockSession::default();
        session.commit(&SweepBlockEffect {
            preflighted_candidates: 1,
            checked_candidates: 1,
            system_gas_used: CANDIDATE_SYSTEM_GAS,
            seen_registry_requests: vec![pair],
        });
        let logs = vec![
            request_log(pair.token, pair.deposit),
            transfer_log(pair.token, Address::ZERO, pair.deposit, U256::from(1)),
        ];

        let triggers = collect_sweep_triggers(&logs, REGISTRY, &session.plan());

        assert_eq!(
            triggers,
            vec![SweepTrigger {
                candidate: pair,
                kind: SweepTriggerKind::TokenTransfer,
            }]
        );
    }

    #[test]
    fn sweep_outcome_effect_records_only_checked_registry_requests() {
        let mut evm = make_evm(
            ResolverMode::Master,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::True),
                (TOKEN_B, BalanceMode::Normal, TransferMode::True),
            ],
            None,
            true,
        );
        let request = candidate(TOKEN_A);
        let transfer = candidate(TOKEN_B);
        let outcome = execute_sweep_triggers(
            &mut evm,
            test_sweep_config(),
            &[
                SweepTrigger {
                    candidate: request,
                    kind: SweepTriggerKind::RegistryRequest,
                },
                SweepTrigger {
                    candidate: transfer,
                    kind: SweepTriggerKind::TokenTransfer,
                },
            ],
            0,
            &SweepBlockSession::default().plan(),
        )
        .unwrap();

        assert_eq!(
            outcome.block_effect,
            SweepBlockEffect {
                preflighted_candidates: 2,
                checked_candidates: 2,
                system_gas_used: 2 * CANDIDATE_SYSTEM_GAS,
                seen_registry_requests: vec![request],
            }
        );
    }

    #[test]
    fn trace_replay_context_shares_budget_request_set_and_clears_at_target() {
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
        assert_eq!(
            first_transaction.plan.candidate_allowance(),
            MAX_CANDIDATES_PER_TX
        );
        let request = candidate(TOKEN_A);
        finish_sweep_trace_replay_transaction(
            Some(first_transaction),
            &SweepBlockEffect {
                preflighted_candidates: 16,
                checked_candidates: 16,
                system_gas_used: 16 * CANDIDATE_SYSTEM_GAS,
                seen_registry_requests: vec![request],
            },
        );

        set_sweep_trace_replay_target(second_hash);
        let second_transaction =
            sweep_trace_replay_transaction(Some(&second)).expect("second transaction");
        assert_eq!(
            second_transaction.plan.candidate_allowance(),
            MAX_CANDIDATES_PER_TX
        );
        assert!(second_transaction.plan.has_seen_registry_request(request));
        finish_sweep_trace_replay_transaction(
            Some(second_transaction),
            &SweepBlockEffect::default(),
        );

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
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(0),
        ));
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
    fn explicit_disabled_mode_revokes_trace_authority_for_later_evm() {
        let _scope = sweep_trace_replay_scope();
        let raw = Bytes::from_static(b"canonical-explicit-disabled");
        begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&raw)]);

        let mut revoked = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        revoked.set_sweep_execution_mode(SweepExecutionMode::Disabled);

        let mut later = make_evm(
            ResolverMode::Master,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        let mut tx = main_tx();
        tx.rlp_bytes = Some(raw);
        assert!(later.transact_one(tx).unwrap().is_success());
        assert_eq!(later.take_sweep_outcome().unwrap(), SweepOutcome::default());
        assert_eq!(
            token_balance(&mut later, TOKEN_A),
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
