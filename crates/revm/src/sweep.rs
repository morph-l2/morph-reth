//! Consensus execution support for bounded ERC-20 source sweeping.
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
    context::{TxEnv, result::EVMError},
    context_interface::{Cfg, ContextTr, JournalTr, LocalContextTr, context::take_error},
    handler::{EvmTr, Handler},
    interpreter::{
        CallInput, CallInputs, CallScheme, CallValue, FrameInput, SharedMemory,
        InstructionResult, interpreter_action::FrameInit,
    },
};
use std::{cell::RefCell, collections::HashSet};

/// Gas limit for the Registry resolver static call.
///
/// Fixed per Onyx spec §5.4. Its actual consumption is NEVER metered: it does
/// not enter the transaction allowance, the block allowance, or the user's
/// `gasUsed`. Only the limit is consensus — it decides whether the call
/// succeeds, and success decides state.
pub const RESOLVE_GAS_LIMIT: u64 = 50_000;
/// Gas limit for each ERC-20 `balanceOf` static call.
///
/// Same metering rule as [`RESOLVE_GAS_LIMIT`] (Onyx spec §5.4): fixed limit,
/// consumption never metered.
pub const BALANCE_OF_GAS_LIMIT: u64 = 50_000;
/// Maximum accumulated `transfer` gas for one transaction.
///
/// Only the sweep `transfer` call's ACTUAL gas counts toward this meter (Onyx
/// spec §5.4). When the cumulative transfer gas reaches this limit the next
/// transfer runs out of gas, classifying the whole transaction as
/// `SweepOutOfGas`: the transaction-level checkpoint reverts the main call and
/// every sweep, while the nonce increment and fee pre-deduction survive.
pub const TX_SWEEP_GAS_LIMIT: u64 = 1_000_000;
/// Maximum accumulated `transfer` gas in one block.
///
/// Only actual `transfer` gas counts toward this meter (Onyx spec §5.4). The
/// builder defers transactions that would exceed it; an already-produced block
/// whose transfer total exceeds it is invalid.
pub const BLOCK_SWEEP_GAS_LIMIT: u64 = 20_000_000;

const _: () = {
    // Capacity derivation per Onyx spec §5.4: 1M / 20M transaction / block
    // allowances with 33.3k–42.8k measured transfer costs imply ~23–30 sweeps
    // per transaction and ~467–600 per block. These are capacity estimates, not
    // consensus quotas — the protocol deliberately imposes NO candidate count
    // ceiling, NO preflight ceiling, NO fixed per-phase debit, and NO truncation.
    assert!(BLOCK_SWEEP_GAS_LIMIT > TX_SWEEP_GAS_LIMIT);
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
/// `keccak256("SweepFailed(address,address,address,bytes32)")`.
const SWEEP_FAILED_TOPIC: B256 =
    b256!("0f64fa58e4261d8832b5ea6c262c691ef36e73cb21998c4fb01a83997940797c");
const RESOLVE_SELECTOR: [u8; 4] = [0x9f, 0xaa, 0x2f, 0x2f];
const BALANCE_OF_SELECTOR: [u8; 4] = [0x70, 0xa0, 0x82, 0x31];

/// A token/source pair eligible for a sweep check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepCandidate {
    /// ERC-20 token contract.
    pub token: Address,
    /// Source address.
    pub source: Address,
}

/// Opaque block-level accounting produced by one sweep phase.
///
/// Only the actual `transfer` gas is metered (Onyx spec §5.4). The resolver and
/// `balanceOf` queries have fixed gas limits but their consumption is never
/// recorded here — not into the transaction allowance, the block allowance, or
/// the user's `gasUsed`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepBlockEffect {
    /// Actual `transfer` gas consumed by this transaction's sweeps.
    transfer_gas_used: u64,
    /// Newly seen Registry requests, in deterministic receipt order.
    seen_registry_requests: Vec<SweepCandidate>,
}

impl SweepBlockEffect {
    /// Actual `transfer` gas consumed by this transaction's sweeps.
    pub const fn transfer_gas_used(&self) -> u64 {
        self.transfer_gas_used
    }

    /// Newly seen Registry requests, in deterministic receipt order.
    pub fn seen_registry_requests(&self) -> &[SweepCandidate] {
        &self.seen_registry_requests
    }

    /// Records a Registry request entering resolver resolution.
    pub(crate) fn record_seen_registry_request(&mut self, candidate: SweepCandidate) {
        self.seen_registry_requests.push(candidate);
    }

    /// Adds actual `transfer` gas to the block meter.
    pub(crate) fn add_transfer_gas(&mut self, gas: u64) {
        self.transfer_gas_used = self.transfer_gas_used.saturating_add(gas);
    }
}

/// Explicit sweep authority and block state supplied to one transaction.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepTxPlan {
    /// Remaining `transfer` gas this transaction may still consume.
    ///
    /// Starts at `TX_SWEEP_GAS_LIMIT` for canonical execution and only decreases
    /// by each sweep transfer's actual gas. Speculative calls (eth_call /
    /// estimateGas) receive a fresh full allowance.
    remaining_transfer_gas: u64,
    /// Registry requests already resolved earlier in this block.
    seen_registry_requests: Vec<SweepCandidate>,
}

impl SweepTxPlan {
    /// Builds an isolated plan with a fresh full transfer allowance and no
    /// prior block-level request history.
    ///
    /// This is intended for single-transaction state tests and speculative RPC
    /// calls (eth_call / eth_estimateGas / createAccessList), which use an empty
    /// seen set and a new 1M transaction transfer meter (Onyx spec §8).
    pub fn single_transaction() -> Self {
        Self {
            remaining_transfer_gas: TX_SWEEP_GAS_LIMIT,
            seen_registry_requests: Vec::new(),
        }
    }

    /// Remaining `transfer` gas this transaction may still consume.
    pub const fn remaining_transfer_gas(&self) -> u64 {
        self.remaining_transfer_gas
    }

    /// Returns whether this Registry request was already consumed in the block.
    pub fn has_seen_registry_request(&self, candidate: SweepCandidate) -> bool {
        self.seen_registry_requests.contains(&candidate)
    }
}

/// Canonical block transfer-gas budget and request deduplication state.
#[derive(Debug, Clone)]
pub struct SweepBlockSession {
    remaining_transfer_gas: u64,
    seen_registry_requests: HashSet<SweepCandidate>,
}

impl Default for SweepBlockSession {
    fn default() -> Self {
        Self {
            remaining_transfer_gas: BLOCK_SWEEP_GAS_LIMIT,
            seen_registry_requests: HashSet::new(),
        }
    }
}

impl SweepBlockSession {
    /// Builds the immutable plan for the next speculative transaction.
    pub fn plan(&self) -> SweepTxPlan {
        let mut seen_registry_requests = self
            .seen_registry_requests
            .iter()
            .copied()
            .collect::<Vec<_>>();
        seen_registry_requests.sort_unstable_by(|left, right| {
            left.token
                .cmp(&right.token)
                .then_with(|| left.source.cmp(&right.source))
        });
        SweepTxPlan {
            remaining_transfer_gas: self.remaining_transfer_gas.min(TX_SWEEP_GAS_LIMIT),
            seen_registry_requests,
        }
    }

    /// Applies the effect of a committed transaction.
    pub fn commit(&mut self, effect: &SweepBlockEffect) {
        self.remaining_transfer_gas = self
            .remaining_transfer_gas
            .saturating_sub(effect.transfer_gas_used);
        self.seen_registry_requests
            .extend(effect.seen_registry_requests.iter().copied());
    }

    /// Remaining `transfer` gas in the block.
    pub const fn remaining_transfer_gas(&self) -> u64 {
        self.remaining_transfer_gas
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
    /// Registry resolver returned a malformed address word.
    ResolverMalformed,
    /// Registry resolved the source to the zero address: the source is not
    /// registered, is disabled, or the token is not whitelisted.
    ResolverZero,
    /// Registry resolved the source to itself. The Registry refuses such
    /// registrations, so this can only fire on a pre-existing record.
    SelfReference,
    /// Pre-transfer `balanceOf` reverted or halted.
    BalanceCallFailed,
    /// Pre-transfer `balanceOf` returned malformed data.
    BalanceMalformed,
    /// Source token balance is zero.
    BalanceZero,
    /// Token `transfer` reverted or halted.
    TransferCallFailed,
    /// Token `transfer` returned ABI `false`.
    TransferFalse,
    /// Token `transfer` returned malformed data.
    TransferMalformed,
    /// Transfer call emitted no matching canonical `Transfer`.
    MissingTransferLog,
    /// Transfer call emitted more than one matching canonical `Transfer`.
    DuplicateTransferLog,
    /// The transfer call produced more logs than the single canonical
    /// `Transfer`, or a malformed ABI / log shape was observed.
    ScopeViolation,
    /// Source has ordinary code (not a plain EOA).
    SourceHasCode,
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
            Self::SelfReference => "self_reference",
            Self::BalanceCallFailed => "balance_call_failed",
            Self::BalanceMalformed => "balance_malformed",
            Self::BalanceZero => "balance_zero",
            Self::TransferCallFailed => "transfer_call_failed",
            Self::TransferFalse => "transfer_false",
            Self::TransferMalformed => "transfer_malformed",
            Self::MissingTransferLog => "missing_transfer_log",
            Self::DuplicateTransferLog => "duplicate_transfer_log",
            Self::ScopeViolation => "scope_violation",
            Self::SourceHasCode => "source_has_code",
        }
    }

    /// Whether this classification is reported on-chain as a `SweepFailed` log.
    ///
    /// Two are deliberately silent:
    /// - [`Self::ResolverZero`] fires for every ERC-20 `Transfer` on the chain,
    ///   because candidate discovery cannot check registration. Logging it would
    ///   bury the real signals under chain-wide background noise.
    /// - [`Self::BalanceZero`] means there was nothing to sweep — a no-op rather
    ///   than a failure. Blind pokes hit it routinely.
    pub const fn is_reported_on_chain(self) -> bool {
        !matches!(self, Self::ResolverZero | Self::BalanceZero)
    }

    /// `SweepFailed.reason`: `keccak256` of the stable metrics label.
    ///
    /// Hashing the label instead of assigning ordinals keeps the on-chain
    /// encoding immune to classifications being added or removed — which this
    /// enum has already done once.
    pub fn as_log_reason(self) -> B256 {
        alloy_primitives::keccak256(self.as_label())
    }
}

/// A checked candidate that did not complete a sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepFailure {
    /// Candidate that failed.
    pub candidate: SweepCandidate,
    /// Destination the Registry resolved, when the candidate got that far.
    /// `None` means resolution itself produced no destination.
    pub destination: Option<Address>,
    /// Business-failure classification.
    pub reason: SweepFailureReason,
}

/// A successfully swept candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SweepSuccess {
    /// Candidate that was swept.
    pub candidate: SweepCandidate,
    /// Registry-resolved destination recipient.
    pub destination: Address,
    /// Full pre-transfer source balance.
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
    /// Block-level transfer-gas and request-deduplication effect.
    pub block_effect: SweepBlockEffect,
    /// Successful sweeps.
    pub successes: Vec<SweepSuccess>,
    /// Classified business failures.
    pub failures: Vec<SweepFailure>,
    /// The transaction's cumulative `transfer` gas hit `TX_SWEEP_GAS_LIMIT`,
    /// forcing the whole transaction to fail (`SweepOutOfGas`).
    ///
    /// When set, the main call and every earlier sweep must be reverted via the
    /// transaction-level checkpoint, and fees re-settled.
    pub tx_out_of_gas: bool,
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
            source: address_from_topic(topics[2]),
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
        source: address_from_topic(topics[2]),
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
    collect_transaction_sweep_triggers(main_logs, registry, plan).triggers
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CollectedSweepTriggers {
    pub(crate) triggers: Vec<SweepTrigger>,
}

pub(crate) fn collect_transaction_sweep_triggers(
    main_logs: &[Log],
    registry: Address,
    plan: &SweepTxPlan,
) -> CollectedSweepTriggers {
    let mut seen = HashSet::new();
    let mut triggers = Vec::new();

    // Registry `SweepRequested` logs first, in receipt order. They are the only
    // candidates subject to block-level dedup: the seen set records the point a
    // request enters resolver resolution, independent of resolution or sweep
    // outcome, and only advances on committed transactions.
    for log in main_logs {
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

    // ERC-20 `Transfer` logs after Registry requests, in receipt order.
    // Transaction-local dedup only — Transfer candidates never participate in
    // block-level dedup (Onyx spec §5.1).
    for candidate in main_logs.iter().filter_map(parse_transfer_candidate) {
        if seen.insert(candidate) {
            triggers.push(SweepTrigger {
                candidate,
                kind: SweepTriggerKind::TokenTransfer,
            });
        }
    }

    CollectedSweepTriggers { triggers }
}

/// Collects first-seen, deduplicated sweep candidates with Registry requests first.
pub fn collect_sweep_candidates(main_logs: &[Log], registry: Address) -> Vec<SweepCandidate> {
    collect_sweep_triggers(main_logs, registry, &SweepBlockSession::default().plan())
        .into_iter()
        .map(|trigger| trigger.candidate)
        .collect()
}

/// Builds the EL-synthesized protocol settlement log.
pub fn build_sweep_log(
    registry: Address,
    candidate: SweepCandidate,
    destination: Address,
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
            B256::left_padding_from(candidate.source.as_slice()),
            B256::left_padding_from(destination.as_slice()),
        ],
        Bytes::from(data),
    )
}

/// Builds the EL-synthesized `SweepFailed` log.
///
/// Indexed layout deliberately mirrors [`build_sweep_log`] so an indexer can use
/// one filter for both outcomes.
pub fn build_sweep_failed_log(
    registry: Address,
    candidate: SweepCandidate,
    destination: Address,
    reason: SweepFailureReason,
) -> Log {
    Log::new_unchecked(
        registry,
        vec![
            SWEEP_FAILED_TOPIC,
            B256::left_padding_from(candidate.token.as_slice()),
            B256::left_padding_from(candidate.source.as_slice()),
            B256::left_padding_from(destination.as_slice()),
        ],
        Bytes::from(reason.as_log_reason().0.to_vec()),
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

/// ERC-20 `transfer(address,uint256)` selector.
const TRANSFER_SELECTOR: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

#[inline]
fn encode_transfer(recipient: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&TRANSFER_SELECTOR);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(recipient.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    Bytes::from(data)
}

#[derive(Debug)]
struct InternalCall {
    output: Option<Bytes>,
    /// Actual gas consumed by the call frame, measured from the frame's own
    /// gas tracker (`gas_limit - remaining`). Used to meter the sweep transfer.
    actual_gas_used: u64,
    /// Whether the call frame halted with out-of-gas.
    out_of_gas: bool,
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
        let account = evm.ctx_mut().journal_mut().load_account_with_code(target)?;
        let known_bytecode = (
            account.info.code_hash(),
            account.info.code.clone().unwrap_or_default(),
        );
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
        let instruction_result = frame_result.instruction_result();
        Ok(InternalCall {
            output: instruction_result
                .is_ok()
                .then(|| frame_result.interpreter_result().output.clone()),
            actual_gas_used: gas_limit.saturating_sub(frame_result.gas().remaining()),
            out_of_gas: matches!(instruction_result, InstructionResult::OutOfGas),
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
    destination: Address,
    amount: U256,
) -> bool {
    log.address == candidate.token
        && log.topics()
            == [
                TRANSFER_TOPIC,
                B256::left_padding_from(candidate.source.as_slice()),
                B256::left_padding_from(destination.as_slice()),
            ]
        && log.data.data.as_ref() == amount.to_be_bytes::<32>()
}

/// Records a business failure and, when it is attributable and worth reporting,
/// appends a `SweepFailed` log.
///
/// Appending to `outcome.logs` is offset-safe: `Swept.transferLogOffset` is
/// derived from `outcome.logs.len()` at the time the successful candidate
/// commits, so failure logs are counted like any other preceding log.
fn push_failure(
    outcome: &mut SweepOutcome,
    registry: Address,
    candidate: SweepCandidate,
    destination: Option<Address>,
    reason: SweepFailureReason,
) {
    tracing::debug!(
        token = ?candidate.token,
        source = ?candidate.source,
        destination = ?destination,
        reason = reason.as_label(),
        "sweep candidate did not settle"
    );
    if let Some(destination) = destination.filter(|_| reason.is_reported_on_chain()) {
        outcome.logs.push(build_sweep_failed_log(
            registry,
            candidate,
            destination,
            reason,
        ));
    }
    outcome.failures.push(SweepFailure {
        candidate,
        destination,
        reason,
    });
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

/// Why a candidate never reached execution, plus the destination it resolved to
/// (if it got that far) so the failure can be reported on-chain.
#[derive(Debug, Clone, Copy)]
struct PreflightFailure {
    destination: Option<Address>,
    reason: SweepFailureReason,
}

impl PreflightFailure {
    const fn unresolved(reason: SweepFailureReason) -> Self {
        Self {
            destination: None,
            reason,
        }
    }

    const fn resolved(destination: Address, reason: SweepFailureReason) -> Self {
        Self {
            destination: Some(destination),
            reason,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PreparedSweep {
    candidate: SweepCandidate,
    destination: Address,
    amount: U256,
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

fn resolve_destination<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidate: SweepCandidate,
) -> Result<Result<Address, SweepFailureReason>, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let resolver = internal_call(
        evm,
        Address::ZERO,
        config.registry_address,
        encode_two_address_call(RESOLVE_SELECTOR, candidate.token, candidate.source),
        RESOLVE_GAS_LIMIT,
        true,
    )?;
    let Some(output) = resolver.output else {
        return Ok(Err(SweepFailureReason::ResolverCallFailed));
    };
    Ok(decode_address(&output))
}

/// Executes the resolver and pre-transfer checks for one candidate.
///
/// The resolver and `balanceOf` calls use their fixed gas limits and their
/// consumption is NEVER metered — not into the transaction allowance, the block
/// allowance, or the user's `gasUsed` (Onyx spec §5.4). On `Ok` a candidate is
/// ready to transfer; on `Err(PreflightFailure)` it is a candidate-level
/// business failure.
fn preflight_candidate<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidate: SweepCandidate,
) -> Result<Result<PreparedSweep, PreflightFailure>, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let destination = match resolve_destination(evm, config, candidate)? {
        Ok(destination) => destination,
        Err(reason) => return Ok(Err(PreflightFailure::unresolved(reason))),
    };

    // Defence in depth: the Registry refuses self-referencing registrations, so
    // this can only fire on a record written before that check existed. Sweeping
    // an address to itself is a no-op that would still burn a candidate slot.
    if destination == candidate.source {
        return Ok(Err(PreflightFailure::resolved(
            destination,
            SweepFailureReason::SelfReference,
        )));
    }

    // Reject sources that have ordinary code (not a plain EOA).
    {
        let account = evm
            .ctx_mut()
            .journal_mut()
            .load_account_with_code(candidate.source)?;
        if account
            .info
            .code
            .as_ref()
            .is_some_and(|code| !code.is_empty())
        {
            return Ok(Err(PreflightFailure::resolved(
                destination,
                SweepFailureReason::SourceHasCode,
            )));
        }
    }

    let balance = match read_token_balance(
        evm,
        candidate.token,
        candidate.source,
        SweepFailureReason::BalanceCallFailed,
        SweepFailureReason::BalanceMalformed,
    )? {
        Ok(balance) => balance,
        Err(reason) => return Ok(Err(PreflightFailure::resolved(destination, reason))),
    };
    if balance.is_zero() {
        return Ok(Err(PreflightFailure::resolved(
            destination,
            SweepFailureReason::BalanceZero,
        )));
    }

    Ok(Ok(PreparedSweep {
        candidate,
        destination,
        amount: balance,
    }))
}

/// Outcome of a single candidate's `transfer`.
enum TransferOutcome {
    /// The transfer settled; the candidate succeeded.
    Success {
        /// Actual gas consumed by the `transfer` call.
        actual_gas_used: u64,
    },
    /// The transfer was a candidate-level business failure.
    Failure {
        reason: SweepFailureReason,
        /// Actual gas consumed by the `transfer` call before the failure.
        actual_gas_used: u64,
    },
    /// The transfer exhausted the transaction transfer allowance. The whole
    /// transaction fails. The gas consumed up to the OOG is already counted in
    /// the block meter by the caller.
    TxOutOfGas,
}

/// Executes the sweep `transfer` for one prepared candidate.
///
/// The transfer is the ONLY call whose actual gas is metered: it is forwarded
/// the remaining transaction transfer allowance, its consumed gas is deducted
/// from that allowance, and it is the only call that can fail the whole
/// transaction (via `SweepOutOfGas`).
fn execute_prepared_sweep<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    prepared: PreparedSweep,
    transfer_gas_limit: u64,
    receipt_prefix_logs: usize,
    outcome: &mut SweepOutcome,
) -> Result<TransferOutcome, EVMError<DB::Error, MorphInvalidTransaction>>
where
    DB: Database,
{
    let candidate = prepared.candidate;
    let destination = prepared.destination;
    let balance = prepared.amount;

    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();
    let log_start = checkpoint.log_i;
    let transfer = internal_call(
        evm,
        candidate.source,
        candidate.token,
        encode_transfer(destination, balance),
        transfer_gas_limit,
        false,
    )?;
    let actual_gas_used = transfer.actual_gas_used;
    outcome.block_effect.add_transfer_gas(actual_gas_used);

    if transfer.out_of_gas {
        // The transfer used up the remaining transaction allowance. This is the
        // one sweep outcome that fails the whole transaction; revert the
        // candidate and signal it upward.
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::TxOutOfGas);
    }
    let Some(transfer_output) = transfer.output else {
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::Failure {
            reason: SweepFailureReason::TransferCallFailed,
            actual_gas_used,
        });
    };
    if let Err(reason) = classify_transfer_output(&transfer_output) {
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::Failure {
            reason,
            actual_gas_used,
        });
    }

    let journal = evm.ctx_ref().journal();
    let call_logs = &journal.logs[log_start..];
    let mut matching_logs = call_logs
        .iter()
        .enumerate()
        .filter(|(_, log)| is_matching_transfer(log, candidate, destination, balance));
    let Some((matching_log_offset, _)) = matching_logs.next() else {
        drop(matching_logs);
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::Failure {
            reason: SweepFailureReason::MissingTransferLog,
            actual_gas_used,
        });
    };
    if matching_logs.next().is_some() {
        drop(matching_logs);
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::Failure {
            reason: SweepFailureReason::DuplicateTransferLog,
            actual_gas_used,
        });
    }
    drop(matching_logs);
    // Scope: the call produced exactly one log. The protocol does NOT scan the
    // journal — the "log count is exactly one" rule plus whitelist admission
    // together constrain scope (Onyx spec §5.2 / §9).
    if call_logs.len() != 1 {
        evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);
        return Ok(TransferOutcome::Failure {
            reason: SweepFailureReason::ScopeViolation,
            actual_gas_used,
        });
    }

    let transfer_log_offset = match checked_transfer_log_offset(
        receipt_prefix_logs,
        outcome.logs.len(),
        matching_log_offset,
    ) {
        Ok(offset) => offset,
        Err(error) => {
            // Receipt-relative offset exceeds u32: the receipt would need more
            // than 4 billion preceding logs. This is an internal invariant
            // violation, not a token misbehavior — surface it as a hard error
            // rather than a misleading `SweepFailed`.
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
        destination,
        balance,
        transfer_log_offset,
    ));
    outcome.successes.push(SweepSuccess {
        candidate,
        destination,
        amount: balance,
        transfer_log_offset,
    });
    Ok(TransferOutcome::Success { actual_gas_used })
}

/// Executes independent sweep candidates against the post-main state.
#[cfg(test)]
pub(crate) fn execute_sweeps<DB, I>(
    evm: &mut MorphEvm<DB, I>,
    config: SweepConfig,
    candidates: &[SweepCandidate],
    receipt_prefix_logs: usize,
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
    let plan = SweepTxPlan::single_transaction();
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
    let registry = config.registry_address;
    let mut remaining_transfer_gas = plan.remaining_transfer_gas();

    for trigger in triggers.iter().copied() {
        if remaining_transfer_gas == 0 {
            outcome.tx_out_of_gas = true;
            break;
        }
        // A Registry request entering resolver resolution is recorded in the
        // block-level seen set, independent of resolution or sweep outcome
        // (Onyx spec §5.1).
        if trigger.kind == SweepTriggerKind::RegistryRequest {
            outcome
                .block_effect
                .record_seen_registry_request(trigger.candidate);
        }

        let prepared = match preflight_candidate(evm, config, trigger.candidate)? {
            Ok(prepared) => prepared,
            Err(failure) => {
                push_failure(
                    &mut outcome,
                    registry,
                    trigger.candidate,
                    failure.destination,
                    failure.reason,
                );
                continue;
            }
        };

        let transfer_gas_limit = remaining_transfer_gas;
        match execute_prepared_sweep(
            evm,
            config,
            prepared,
            transfer_gas_limit,
            receipt_prefix_logs,
            &mut outcome,
        )? {
            TransferOutcome::Success {
                actual_gas_used,
            } => {
                remaining_transfer_gas =
                    remaining_transfer_gas.saturating_sub(actual_gas_used);
            }
            TransferOutcome::Failure {
                reason,
                actual_gas_used,
            } => {
                remaining_transfer_gas =
                    remaining_transfer_gas.saturating_sub(actual_gas_used);
                push_failure(
                    &mut outcome,
                    registry,
                    prepared.candidate,
                    Some(prepared.destination),
                    reason,
                );
            }
            TransferOutcome::TxOutOfGas => {
                outcome.tx_out_of_gas = true;
                break;
            }
        }
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

    const REGISTRY: Address = address!("0000000000000000000000000000000000009001");
    const SOURCE: Address = address!("1000000000000000000000000000000000000001");
    const DESTINATION: Address = address!("2000000000000000000000000000000000000002");
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

    fn request_log(token: Address, source: Address) -> Log {
        Log::new_unchecked(
            REGISTRY,
            vec![REQUEST_TOPIC, address_topic(token), address_topic(source)],
            Bytes::new(),
        )
    }

    #[test]
    fn failure_reason_labels_are_stable_and_distinct() {
        let reasons = all_failure_reasons();
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
        let exact_transfer = transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(7));
        let exact_request = request_log(TOKEN_B, SOURCE);

        assert_eq!(
            parse_sweep_candidate(&exact_transfer, REGISTRY),
            Some(SweepCandidate {
                token: TOKEN_A,
                source: SOURCE,
            })
        );
        assert_eq!(
            parse_sweep_candidate(&exact_request, REGISTRY),
            Some(SweepCandidate {
                token: TOKEN_B,
                source: SOURCE,
            })
        );

        let malformed = [
            Log::new_unchecked(
                TOKEN_A,
                vec![TRANSFER_TOPIC, address_topic(SOURCE)],
                Bytes::from([0_u8; 32]),
            ),
            Log::new_unchecked(
                TOKEN_A,
                vec![
                    TRANSFER_TOPIC,
                    address_topic(Address::ZERO),
                    address_topic(SOURCE),
                ],
                Bytes::from([0_u8; 31]),
            ),
            Log::new_unchecked(
                TOKEN_A,
                vec![
                    B256::ZERO,
                    address_topic(Address::ZERO),
                    address_topic(SOURCE),
                ],
                Bytes::from([0_u8; 32]),
            ),
            Log::new_unchecked(
                Address::ZERO,
                vec![REQUEST_TOPIC, address_topic(TOKEN_A), address_topic(SOURCE)],
                Bytes::new(),
            ),
            Log::new_unchecked(
                REGISTRY,
                vec![REQUEST_TOPIC, address_topic(TOKEN_A), address_topic(SOURCE)],
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
            vec![REQUEST_TOPIC, noncanonical_token, address_topic(SOURCE)],
            Bytes::new(),
        );
        assert_eq!(parse_sweep_candidate(&malformed_request, REGISTRY), None);
    }

    #[test]
    fn candidates_preserve_order_and_deduplicate_without_a_cap() {
        let mut logs = vec![
            transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1)),
            request_log(TOKEN_A, SOURCE),
            request_log(TOKEN_B, SOURCE),
        ];
        for i in 0_u8..20 {
            let token = Address::with_last_byte(i.saturating_add(10));
            let source = Address::with_last_byte(i.saturating_add(100));
            logs.push(transfer_log(token, Address::ZERO, source, U256::from(1)));
        }

        let candidates = collect_sweep_candidates(&logs, REGISTRY);

        // The Onyx v1 model has NO candidate count ceiling: every first-seen
        // candidate is returned (3 seed logs, minus the TOKEN_A pair already
        // seen as a Registry request, plus 20 transfers = 22).
        assert_eq!(candidates.len(), 22);
        assert_eq!(
            candidates[0],
            SweepCandidate {
                token: TOKEN_A,
                source: SOURCE,
            }
        );
        assert_eq!(
            candidates[1],
            SweepCandidate {
                token: TOKEN_B,
                source: SOURCE,
            }
        );
        assert_eq!(
            candidates[2],
            SweepCandidate {
                token: Address::with_last_byte(10),
                source: Address::with_last_byte(100),
            }
        );
        assert_eq!(candidates[21].source, Address::with_last_byte(119));
    }

    #[test]
    fn registry_requests_take_priority_over_transfer_candidates() {
        let requested = SweepCandidate {
            token: TOKEN_B,
            source: SOURCE,
        };
        let mut logs = (0_u8..20)
            .map(|index| {
                transfer_log(
                    Address::with_last_byte(index.saturating_add(10)),
                    Address::ZERO,
                    Address::with_last_byte(index.saturating_add(100)),
                    U256::from(1),
                )
            })
            .collect::<Vec<_>>();
        logs.push(request_log(requested.token, requested.source));

        let candidates = collect_sweep_candidates(&logs, REGISTRY);

        assert_eq!(candidates.len(), 21);
        assert_eq!(candidates[0], requested);
    }

    #[test]
    fn collection_deduplicates_locally_and_prioritizes_registry_requests() {
        let requested = candidate(TOKEN_B);
        let main_transfer = candidate(TOKEN_A);
        let logs = vec![
            transfer_log(
                main_transfer.token,
                Address::ZERO,
                main_transfer.source,
                U256::from(1),
            ),
            request_log(requested.token, requested.source),
            transfer_log(
                requested.token,
                Address::ZERO,
                requested.source,
                U256::from(1),
            ),
        ];
        let plan = SweepBlockSession::default().plan();

        let collected = collect_transaction_sweep_triggers(&logs, REGISTRY, &plan);

        // The TOKEN_B pair is seen first as a Registry request, so its later
        // Transfer is transaction-locally deduplicated; Registry requests still
        // sort ahead of Transfer candidates. There is no `truncated` flag in the
        // v1 model.
        assert_eq!(
            collected.triggers,
            vec![
                SweepTrigger {
                    candidate: requested,
                    kind: SweepTriggerKind::RegistryRequest,
                },
                SweepTrigger {
                    candidate: main_transfer,
                    kind: SweepTriggerKind::TokenTransfer,
                },
            ]
        );
    }

    #[test]
    fn sweep_event_uses_canonical_abi_and_receipt_relative_offset() {
        let candidate = SweepCandidate {
            token: TOKEN_A,
            source: SOURCE,
        };

        let log = build_sweep_log(
            REGISTRY,
            candidate,
            DESTINATION,
            U256::from(0x1234),
            0x0102_0304,
        );

        assert_eq!(log.address, REGISTRY);
        assert_eq!(
            log.topics(),
            &[
                SWEEP_TOPIC,
                address_topic(TOKEN_A),
                address_topic(SOURCE),
                address_topic(DESTINATION),
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

    /// Registry resolver behaviors the preflight must distinguish.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ResolverMode {
        /// Source registered and enabled, token whitelisted.
        Destination,
        /// No record written: the source is unregistered.
        Unregistered,
        /// Record present but `enabled == false`.
        Disabled,
        /// Source registered and enabled, but the token is not whitelisted.
        TokenNotWhitelisted,
        /// Record whose destination is the source itself.
        SelfReference,
        /// Resolver returns a non-ABI address value.
        Malformed,
        /// Resolver reverts.
        Revert,
        /// Resolver burns a large bounded amount of gas before returning a
        /// destination, to prove query consumption is never metered.
        GasBurning,
    }

    #[derive(Clone, Copy)]
    enum BalanceMode {
        Normal,
        Zero,
        Malformed,
        Revert,
        /// Burns a large bounded amount of gas before returning the balance,
        /// to prove `balanceOf` consumption is never metered.
        GasBurning,
    }

    #[derive(Clone, Copy)]
    enum TransferMode {
        Empty,
        True,
        False,
        Malformed,
        Revert,
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

    /// Burns a large bounded amount of gas with a single expensive memory
    /// expansion, without touching state. Used to prove that resolver and
    /// `balanceOf` query consumption is never metered into the sweep transfer
    /// budget: the burn stays under the fixed 50k query limit but dwarfs the
    /// frame overhead.
    fn burn_gas(asm: &mut Assembler) {
        // MSTORE at offset 0x1C000 forces ~35.8k gas of memory expansion.
        asm.push(&[0x01, 0xc0, 0x00]);
        asm.push_u8(0);
        asm.op(0x52);
    }

    fn registry_code(mode: ResolverMode) -> Bytes {
        let mut asm = Assembler::new();
        match mode {
            ResolverMode::Destination => {
                return_word(&mut asm, B256::left_padding_from(DESTINATION.as_slice()))
            }
            ResolverMode::GasBurning => {
                burn_gas(&mut asm);
                return_word(&mut asm, B256::left_padding_from(DESTINATION.as_slice()))
            }
            ResolverMode::SelfReference => {
                return_word(&mut asm, B256::left_padding_from(SOURCE.as_slice()))
            }
            ResolverMode::Unregistered
            | ResolverMode::Disabled
            | ResolverMode::TokenNotWhitelisted => return_word(&mut asm, B256::ZERO),
            ResolverMode::Malformed => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(1);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            ResolverMode::Revert => {
                asm.push_u8(0);
                asm.push_u8(0);
                asm.op(0xfd);
            }
        }
        asm.finish()
    }

    fn test_sweep_config() -> SweepConfig {
        SweepConfig {
            registry_address: REGISTRY,
        }
    }

    /// Installs a test Registry with the requested resolver behavior.
    fn insert_registry_state<ExtDB: DatabaseRef>(
        db: &mut CacheDB<ExtDB>,
        mode: ResolverMode,
        _tokens: &[Address],
    ) where
        ExtDB::Error: std::fmt::Debug,
    {
        insert_code(db, REGISTRY, registry_code(mode));
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
        asm.push_b256(address_topic(SOURCE));
        asm.push_b256(TRANSFER_TOPIC);
        asm.push_u8(32);
        asm.push_u8(0);
        asm.op(0xa3);
    }

    fn emit_main_candidate(asm: &mut Assembler) {
        asm.push_u8(1);
        asm.push_u8(0);
        asm.op(0x52);
        asm.push_b256(address_topic(SOURCE));
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
                asm.push(DESTINATION.as_slice());
                asm.op(0x14);
                asm.jumpi("destination_balance");
                asm.push_u8(0);
                asm.op(0x54);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
                asm.label("destination_balance");
                asm.push_u8(1);
                asm.op(0x54);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
            }
            BalanceMode::Zero => return_word(&mut asm, B256::ZERO),
            BalanceMode::GasBurning => {
                burn_gas(&mut asm);
                asm.push_u8(0);
                asm.op(0x54);
                asm.push_u8(0);
                asm.op(0x52);
                asm.push_u8(32);
                asm.push_u8(0);
                asm.op(0xf3);
            }
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
        }

        asm.label("transfer");
        asm.op(0x33);
        asm.push(SOURCE.as_slice());
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
        asm.push_u8(0);
        asm.push_u8(0);
        asm.op(0x55);
        asm.push_u8(36);
        asm.op(0x35);
        asm.push_u8(1);
        asm.op(0x54);
        asm.op(0x01);
        asm.push_u8(1);
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
        source_code: Option<Bytecode>,
        enabled: bool,
    ) -> MorphEvm<CacheDB<EmptyDB>, NoOpInspector> {
        make_evm_with_main_mode(
            resolver_mode,
            tokens,
            source_code,
            enabled,
            MainMode::Success,
        )
    }

    fn make_evm_with_main_mode(
        resolver_mode: ResolverMode,
        tokens: &[(Address, BalanceMode, TransferMode)],
        source_code: Option<Bytecode>,
        enabled: bool,
        main_mode: MainMode,
    ) -> MorphEvm<CacheDB<EmptyDB>, NoOpInspector> {
        let mut db = CacheDB::new(EmptyDB::default());
        let token_addresses: Vec<Address> = tokens.iter().map(|(token, _, _)| *token).collect();
        insert_registry_state(&mut db, resolver_mode, &token_addresses);
        for (token, balance_mode, transfer_mode) in tokens {
            insert_code(
                &mut db,
                *token,
                token_code_with_main(*balance_mode, *transfer_mode, main_mode),
            );
            db.insert_account_storage(*token, U256::ZERO, U256::from(INITIAL_BALANCE))
                .unwrap();
        }
        // Default: source is a plain EOA (no code).
        let code = source_code.unwrap_or_default();
        db.insert_account_info(
            SOURCE,
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
            source: SOURCE,
        }
    }

    /// A distinct token address outside the low precompile range (0x01..=0x0a),
    /// for batches that must be executable rather than just collectible.
    fn batch_token(index: u8) -> Address {
        let mut bytes = [0_u8; 20];
        bytes[0] = 0x30;
        bytes[19] = index;
        Address::from(bytes)
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
            execute_sweeps(&mut evm, test_sweep_config(), &[candidate(TOKEN_A)], 0).unwrap();
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

    /// Every classification, in declaration order. Adding a variant without
    /// adding it here makes the label/encoding tests fail to cover it. After
    /// Onyx spec v1 removed the post-transfer balance checks and budget
    /// deferral, this is the 14-classification set.
    const fn all_failure_reasons() -> [SweepFailureReason; 14] {
        [
            SweepFailureReason::ResolverCallFailed,
            SweepFailureReason::ResolverMalformed,
            SweepFailureReason::ResolverZero,
            SweepFailureReason::SelfReference,
            SweepFailureReason::BalanceCallFailed,
            SweepFailureReason::BalanceMalformed,
            SweepFailureReason::BalanceZero,
            SweepFailureReason::TransferCallFailed,
            SweepFailureReason::TransferFalse,
            SweepFailureReason::TransferMalformed,
            SweepFailureReason::MissingTransferLog,
            SweepFailureReason::DuplicateTransferLog,
            SweepFailureReason::ScopeViolation,
            SweepFailureReason::SourceHasCode,
        ]
    }

    fn only_failure(outcome: &SweepOutcome) -> SweepFailureReason {
        assert_eq!(outcome.failures.len(), 1);
        outcome.failures[0].reason
    }

    #[track_caller]
    fn assert_sweep_failed_log(
        log: &Log,
        candidate: SweepCandidate,
        destination: Address,
        reason: SweepFailureReason,
    ) {
        assert_eq!(
            log.address, REGISTRY,
            "SweepFailed must be emitted by the Registry"
        );
        assert_eq!(
            log.topics(),
            [
                SWEEP_FAILED_TOPIC,
                address_topic(candidate.token),
                address_topic(candidate.source),
                address_topic(destination),
            ]
        );
        assert_eq!(&log.data.data[..], &reason.as_log_reason().0[..]);
    }

    /// The only failure log a candidate produced.
    #[track_caller]
    fn only_sweep_failed_log(outcome: &SweepOutcome) -> &Log {
        let logs: Vec<&Log> = outcome
            .logs
            .iter()
            .filter(|log| log.topics().first() == Some(&SWEEP_FAILED_TOPIC))
            .collect();
        assert_eq!(logs.len(), 1, "expected exactly one SweepFailed log");
        logs[0]
    }

    #[test]
    fn opcode_storage_error_propagates_and_reverts_the_whole_sweep_phase() {
        let mut db = CacheDB::new(FailingStorageDb);
        insert_registry_state(&mut db, ResolverMode::Destination, &[TOKEN_A, TOKEN_B]);
        db.insert_account_info(SOURCE, AccountInfo::default());
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
            SweepTxPlan::single_transaction(),
        ));
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![
                transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1)),
                transfer_log(TOKEN_B, Address::ZERO, SOURCE, U256::from(1)),
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
    fn canonical_execution_mode_and_success_result_gate_the_hook() {
        let mut disabled = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            false,
        );
        disabled.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
        ));
        assert!(disabled.transact_one(main_tx()).unwrap().is_success());
        assert_eq!(
            disabled.take_sweep_outcome().unwrap(),
            SweepOutcome::default()
        );

        let mut no_allowance = make_evm(
            ResolverMode::Destination,
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
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
            MainMode::Revert,
        );
        reverted.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
        ));
        assert!(!reverted.transact_one(main_tx()).unwrap().is_success());
        assert_eq!(
            reverted.take_sweep_outcome().unwrap(),
            SweepOutcome::default()
        );

        let mut enabled = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        enabled.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
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
    fn fee_logs_are_receipt_only_and_do_not_trigger_sweeps() {
        let mut evm = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        evm.pre_fee_logs = vec![transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1))];
        evm.post_fee_logs = vec![transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1))];
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
        ));
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1))],
            output: Output::Call(Bytes::new()),
        };

        evm.apply_sweep(&result).unwrap();
        let outcome = evm.take_sweep_outcome().unwrap();

        // Pre- and post-fee logs occupy receipt slots but never trigger sweeps;
        // only the main result logs do. The one sweep that runs is TOKEN_A, and
        // its Transfer lands after pre(1) + main(1) + post(1) = 3 prefix logs.
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.successes[0].candidate, candidate(TOKEN_A));
        assert_eq!(outcome.successes[0].transfer_log_offset, 3);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
    }

    #[test]
    fn main_logs_trigger_sweeps_in_order() {
        let mut evm = make_evm(
            ResolverMode::Destination,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::True),
                (TOKEN_B, BalanceMode::Normal, TransferMode::True),
            ],
            None,
            true,
        );
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
        ));
        let result: ExecutionResult<crate::MorphHaltReason> = ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![
                transfer_log(TOKEN_A, Address::ZERO, SOURCE, U256::from(1)),
                transfer_log(TOKEN_B, Address::ZERO, SOURCE, U256::from(1)),
            ],
            output: Output::Call(Bytes::new()),
        };

        evm.apply_sweep(&result).unwrap();
        let outcome = evm.take_sweep_outcome().unwrap();

        // TOKEN_A sweeps first: its Transfer is at receipt index 2 (2 main
        // prefix logs), then its Swept log. TOKEN_B's Transfer lands after.
        assert_eq!(outcome.successes.len(), 2);
        assert_eq!(outcome.successes[0].candidate, candidate(TOKEN_A));
        assert_eq!(outcome.successes[0].transfer_log_offset, 2);
        assert_eq!(outcome.successes[1].candidate, candidate(TOKEN_B));
        assert_eq!(outcome.successes[1].transfer_log_offset, 4);
    }

    #[test]
    fn replay_and_inspector_paths_apply_sweep_state() {
        let mut replay = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        replay.tx = main_tx();
        replay.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
        ));
        let replayed = replay.replay().unwrap();
        assert!(replayed.result.is_success());
        assert_eq!(
            replayed.state[&TOKEN_A].storage[&U256::ZERO].present_value,
            U256::ZERO
        );
        assert_eq!(replay.take_sweep_outcome().unwrap().successes.len(), 1);

        let mut inspected = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        inspected.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
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
            insert_registry_state(&mut db, ResolverMode::Destination, &[TOKEN_B]);
            db.insert_account_info(SOURCE, AccountInfo::default());
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
                SweepTxPlan::single_transaction(),
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
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(),
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

    /// `SweepFailed.reason` is `keccak256` of the metrics label, for every
    /// classification. This is the whole point of hashing the label rather than
    /// assigning ordinals: the two encodings cannot drift apart.
    #[test]
    fn sweep_failed_reason_encoding_matches_the_metrics_label() {
        let reasons = all_failure_reasons();
        for reason in reasons {
            assert_eq!(
                reason.as_log_reason(),
                alloy_primitives::keccak256(reason.as_label()),
                "{} must hash its own label",
                reason.as_label()
            );
        }
        let encodings: HashSet<B256> = reasons.iter().map(|r| r.as_log_reason()).collect();
        assert_eq!(
            encodings.len(),
            reasons.len(),
            "every reason must have a distinct on-chain encoding"
        );
    }

    /// Exactly two classifications stay off-chain, and for load-bearing reasons:
    /// `resolver_zero` fires for every ERC-20 transfer on the chain, and
    /// `balance_zero` is a no-op rather than a failure.
    #[test]
    fn only_background_noise_classifications_are_unreported() {
        let unreported: Vec<&str> = all_failure_reasons()
            .iter()
            .filter(|reason| !reason.is_reported_on_chain())
            .map(|reason| reason.as_label())
            .collect();
        assert_eq!(unreported, ["resolver_zero", "balance_zero"]);
    }

    /// An unresolved candidate has no destination to report, so it must emit
    /// nothing — otherwise every ERC-20 transfer on the chain would add a log.
    #[test]
    fn unresolved_and_empty_candidates_emit_no_failure_log() {
        for (mode, expected) in [
            (ResolverMode::Revert, SweepFailureReason::ResolverCallFailed),
            (
                ResolverMode::Malformed,
                SweepFailureReason::ResolverMalformed,
            ),
            (ResolverMode::Unregistered, SweepFailureReason::ResolverZero),
            (ResolverMode::Destination, SweepFailureReason::BalanceZero),
        ] {
            let balance_mode = if matches!(expected, SweepFailureReason::BalanceZero) {
                BalanceMode::Zero
            } else {
                BalanceMode::Normal
            };
            let (_, outcome) = execute_one(mode, balance_mode, TransferMode::True);
            assert_eq!(only_failure(&outcome), expected);
            assert!(
                outcome.logs.is_empty(),
                "{} must not produce a log",
                expected.as_label()
            );
        }
    }

    /// A failure log occupies a receipt slot like any other log, so a later
    /// candidate's `Swept.transferLogOffset` must account for it. Getting this
    /// wrong would mispoint every settlement record that follows a failure.
    #[test]
    fn failure_logs_shift_later_transfer_log_offsets() {
        let mut evm = make_evm(
            ResolverMode::Destination,
            &[
                (TOKEN_A, BalanceMode::Normal, TransferMode::MissingLog),
                (TOKEN_B, BalanceMode::Normal, TransferMode::True),
            ],
            None,
            true,
        );
        let outcome = execute_sweeps(
            &mut evm,
            test_sweep_config(),
            &[candidate(TOKEN_A), candidate(TOKEN_B)],
            4,
        )
        .unwrap();

        // SweepFailed(TOKEN_A), then Transfer(TOKEN_B) and Swept(TOKEN_B).
        assert_eq!(outcome.logs.len(), 3);
        assert_sweep_failed_log(
            &outcome.logs[0],
            candidate(TOKEN_A),
            DESTINATION,
            SweepFailureReason::MissingTransferLog,
        );
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(
            outcome.successes[0].transfer_log_offset, 5,
            "4 receipt-prefix logs + 1 preceding failure log"
        );
        assert_eq!(
            outcome.logs[2].topics().first(),
            Some(&SWEEP_TOPIC),
            "the settlement record follows its Transfer"
        );
    }

    /// The v1 model has no preflight quota and no batch-truncation flag: every
    /// trigger is processed, and only `tx_out_of_gas` can interrupt the loop.
    #[test]
    fn execution_never_truncates_a_large_trigger_batch() {
        let mut evm = make_evm(ResolverMode::Unregistered, &[], None, true);
        let triggers = (0_u8..70)
            .map(|index| SweepTrigger {
                candidate: SweepCandidate {
                    token: Address::with_last_byte(index.saturating_add(10)),
                    source: SOURCE,
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

        // All 70 unresolved triggers fail at the resolver; none is dropped.
        assert_eq!(outcome.failures.len(), 70);
        assert!(!outcome.tx_out_of_gas);
        assert!(outcome.successes.is_empty());
        // Unresolved candidates never reach a transfer, so no transfer gas is
        // metered and no Registry request is recorded.
        assert_eq!(outcome.block_effect.transfer_gas_used(), 0);
        assert!(outcome.block_effect.seen_registry_requests().is_empty());
    }

    #[test]
    fn successful_execution_does_not_flag_tx_out_of_gas() {
        let (_, outcome) = execute_one(
            ResolverMode::Destination,
            BalanceMode::Normal,
            TransferMode::True,
        );
        assert!(!outcome.tx_out_of_gas);
        assert_eq!(outcome.successes.len(), 1);
    }

    /// Every Registry result that must not produce a sweep, classified.
    #[test]
    fn non_sweepable_registry_states_are_skipped_without_touching_balances() {
        for (mode, expected) in [
            (ResolverMode::Revert, SweepFailureReason::ResolverCallFailed),
            (
                ResolverMode::Malformed,
                SweepFailureReason::ResolverMalformed,
            ),
            (ResolverMode::Unregistered, SweepFailureReason::ResolverZero),
            (ResolverMode::Disabled, SweepFailureReason::ResolverZero),
            (
                ResolverMode::TokenNotWhitelisted,
                SweepFailureReason::ResolverZero,
            ),
            (
                ResolverMode::SelfReference,
                SweepFailureReason::SelfReference,
            ),
        ] {
            let (mut evm, outcome) = execute_one(mode, BalanceMode::Normal, TransferMode::True);
            assert_eq!(only_failure(&outcome), expected, "mode {mode:?}");
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE),
                "mode {mode:?} must not move tokens"
            );
        }
    }

    #[test]
    fn ordinary_code_is_skipped() {
        let code_cases = [
            Bytecode::new_raw(Bytes::from_static(&[0x00])),
            Bytecode::new_eip7702(Address::with_last_byte(0xaa)),
        ];
        for code in code_cases {
            let mut evm = make_evm(
                ResolverMode::Destination,
                &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
                Some(code),
                true,
            );
            let outcome =
                execute_sweeps(&mut evm, test_sweep_config(), &[candidate(TOKEN_A)], 0).unwrap();

            assert_eq!(only_failure(&outcome), SweepFailureReason::SourceHasCode);
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
            let (mut evm, outcome) =
                execute_one(ResolverMode::Destination, mode, TransferMode::True);
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
            let (mut evm, outcome) =
                execute_one(ResolverMode::Destination, BalanceMode::Normal, mode);
            assert_eq!(outcome.successes.len(), 1);
            assert!(outcome.failures.is_empty());
            assert_eq!(outcome.logs.len(), 2);
            assert_eq!(outcome.successes[0].amount, U256::from(INITIAL_BALANCE));
            assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
        }
    }

    /// The resolver and `balanceOf` queries have fixed 50k gas limits, but their
    /// actual consumption is NEVER metered (Onyx spec §5.4). A query that burns
    /// tens of thousands of gas must not move the transfer meter at all.
    #[test]
    fn query_gas_is_never_metered_into_transfer_budget() {
        let (_, cheap_resolver) =
            execute_one(ResolverMode::Destination, BalanceMode::Normal, TransferMode::True);
        let (_, burning_resolver) =
            execute_one(ResolverMode::GasBurning, BalanceMode::Normal, TransferMode::True);
        assert_eq!(cheap_resolver.successes.len(), 1);
        assert_eq!(burning_resolver.successes.len(), 1);
        assert_eq!(
            cheap_resolver.block_effect.transfer_gas_used(),
            burning_resolver.block_effect.transfer_gas_used(),
            "resolver gas consumption must not enter the transfer meter"
        );
        assert!(cheap_resolver.block_effect.transfer_gas_used() > 0);

        let (_, cheap_balance) =
            execute_one(ResolverMode::Destination, BalanceMode::Normal, TransferMode::True);
        let (_, burning_balance) =
            execute_one(ResolverMode::Destination, BalanceMode::GasBurning, TransferMode::True);
        assert_eq!(burning_balance.successes.len(), 1);
        assert_eq!(
            cheap_balance.block_effect.transfer_gas_used(),
            burning_balance.block_effect.transfer_gas_used(),
            "balanceOf gas consumption must not enter the transfer meter"
        );
    }

    /// Only the sweep `transfer`'s actual gas is metered into the block effect,
    /// whether the transfer settles or fails at the business layer.
    #[test]
    fn transfer_actual_gas_is_metered_into_block_effect() {
        let (mut evm, outcome) =
            execute_one(ResolverMode::Destination, BalanceMode::Normal, TransferMode::True);
        assert!(outcome.block_effect.transfer_gas_used() > 0);
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);

        let (_, failed) =
            execute_one(ResolverMode::Destination, BalanceMode::Normal, TransferMode::False);
        assert!(failed.block_effect.transfer_gas_used() > 0);
        assert_eq!(only_failure(&failed), SweepFailureReason::TransferFalse);
    }

    /// The transaction-level transfer meter stops the loop: once the cumulative
    /// actual transfer gas exhausts `remaining_transfer_gas`, the outcome is
    /// flagged `tx_out_of_gas` and later candidates are not processed.
    #[test]
    fn transaction_level_oog_stops_the_sweep_loop() {
        let tokens: Vec<(Address, BalanceMode, TransferMode)> = (0_u8..5)
            .map(|index| (batch_token(index.saturating_add(10)), BalanceMode::Normal, TransferMode::True))
            .collect();
        let mut evm = make_evm(ResolverMode::Destination, &tokens, None, true);
        let triggers = tokens
            .iter()
            .map(|(token, _, _)| SweepTrigger {
                candidate: candidate(*token),
                kind: SweepTriggerKind::TokenTransfer,
            })
            .collect::<Vec<_>>();
        let plan = SweepTxPlan {
            remaining_transfer_gas: 100_000,
            seen_registry_requests: Vec::new(),
        };

        let outcome = execute_sweep_triggers(&mut evm, test_sweep_config(), &triggers, 0, &plan)
            .unwrap();

        assert!(outcome.tx_out_of_gas);
        assert!(
            outcome.successes.len() >= 1,
            "the first transfer must fit inside the bounded allowance"
        );
        assert!(
            outcome.successes.len() < triggers.len(),
            "the loop must stop before processing every candidate"
        );
        assert!(outcome.failures.is_empty());
        // The transfer gas spent before the OOG (including the aborted
        // transfer's partial consumption) is recorded in the block meter.
        assert!(outcome.block_effect.transfer_gas_used() > 0);
        assert!(outcome.block_effect.transfer_gas_used() <= 100_000);
    }

    /// The v1 model imposes NO candidate count ceiling: batches far beyond the
    /// old 16/64 quotas are collected and executed in full.
    #[test]
    fn large_candidate_batches_are_not_capped() {
        // Distinct token addresses whose last byte stays clear of the 0x01-0x0a
        // precompile range (0x0000...00XX collides with KZG/point evaluation).
        let tokens: Vec<(Address, BalanceMode, TransferMode)> = (0_u8..20)
            .map(|index| {
                (
                    Address::with_last_byte(0x50_u8.saturating_add(index)),
                    BalanceMode::Normal,
                    TransferMode::True,
                )
            })
            .collect();
        let mut evm = make_evm(ResolverMode::Destination, &tokens, None, true);
        let candidates: Vec<SweepCandidate> =
            tokens.iter().map(|(token, _, _)| candidate(*token)).collect();

        let outcome = execute_sweeps(&mut evm, test_sweep_config(), &candidates, 0).unwrap();

        assert_eq!(outcome.successes.len(), 20);
        assert!(!outcome.tx_out_of_gas);
    }

    /// A Registry request enters the block-level seen set the moment it reaches
    /// resolver resolution, independent of whether resolution or the sweep
    /// succeeds — so a failing request still suppresses a later duplicate.
    #[test]
    fn registry_request_dedup_records_at_resolution_independent_of_outcome() {
        let mut evm = make_evm(ResolverMode::Unregistered, &[], None, true);
        let request = candidate(TOKEN_A);
        let outcome = execute_sweep_triggers(
            &mut evm,
            test_sweep_config(),
            &[SweepTrigger {
                candidate: request,
                kind: SweepTriggerKind::RegistryRequest,
            }],
            0,
            &SweepBlockSession::default().plan(),
        )
        .unwrap();

        assert_eq!(outcome.block_effect.seen_registry_requests(), &[request]);
        assert_eq!(only_failure(&outcome), SweepFailureReason::ResolverZero);
        assert_eq!(outcome.block_effect.transfer_gas_used(), 0);
    }

    #[test]
    fn extra_transfer_call_logs_violate_the_sweep_scope() {
        let (mut evm, outcome) = execute_one(
            ResolverMode::Destination,
            BalanceMode::Normal,
            TransferMode::ExtraLog,
        );

        assert!(outcome.successes.is_empty());
        assert_eq!(only_failure(&outcome), SweepFailureReason::ScopeViolation);
        assert_eq!(
            outcome.logs.len(),
            1,
            "only the failure log survives the revert"
        );
        assert_sweep_failed_log(
            only_sweep_failed_log(&outcome),
            candidate(TOKEN_A),
            DESTINATION,
            SweepFailureReason::ScopeViolation,
        );
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
                TransferMode::MissingLog,
                SweepFailureReason::MissingTransferLog,
            ),
            (
                BalanceMode::Normal,
                TransferMode::DuplicateLog,
                SweepFailureReason::DuplicateTransferLog,
            ),
        ] {
            let (mut evm, outcome) =
                execute_one(ResolverMode::Destination, balance_mode, transfer_mode);
            assert_eq!(only_failure(&outcome), expected);
            // The candidate's own journal logs are reverted; the protocol failure
            // record is the one log that remains.
            assert_eq!(outcome.logs.len(), 1);
            assert_sweep_failed_log(
                only_sweep_failed_log(&outcome),
                candidate(TOKEN_A),
                DESTINATION,
                expected,
            );
            assert_eq!(
                token_balance(&mut evm, TOKEN_A),
                U256::from(INITIAL_BALANCE)
            );
        }
    }

    #[test]
    fn candidates_are_isolated_and_only_transfer_gas_is_metered() {
        let mut evm = make_evm(
            ResolverMode::Destination,
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
        )
        .unwrap();

        // TOKEN_A succeeds and its transfer's actual gas is metered; TOKEN_B
        // fails at the transfer with its gas metered too.
        assert!(outcome.block_effect.transfer_gas_used() > 0);
        assert_eq!(outcome.successes.len(), 1);
        assert_eq!(outcome.failures.len(), 1);
        assert_eq!(token_balance(&mut evm, TOKEN_A), U256::ZERO);
        assert_eq!(
            token_balance(&mut evm, TOKEN_B),
            U256::from(INITIAL_BALANCE)
        );
        // TOKEN_A's Transfer + Swept, then TOKEN_B's SweepFailed.
        assert_eq!(outcome.logs.len(), 3);
        assert_eq!(outcome.successes[0].transfer_log_offset, 4);
        assert_sweep_failed_log(
            only_sweep_failed_log(&outcome),
            candidate(TOKEN_B),
            DESTINATION,
            SweepFailureReason::MissingTransferLog,
        );
    }

    #[test]
    fn block_session_starts_full_and_commit_subtracts_transfer_gas() {
        let request = candidate(TOKEN_A);
        let transfer = candidate(TOKEN_B);
        let mut session = SweepBlockSession::default();

        assert_eq!(session.remaining_transfer_gas(), BLOCK_SWEEP_GAS_LIMIT);
        let first = session.plan();
        // The plan exposes the per-transaction cap, not the block budget, while
        // the block budget is above it.
        assert_eq!(first.remaining_transfer_gas(), TX_SWEEP_GAS_LIMIT);
        assert!(!first.has_seen_registry_request(request));
        assert!(!first.has_seen_registry_request(transfer));

        let mut effect = SweepBlockEffect::default();
        effect.add_transfer_gas(123_456);
        effect.record_seen_registry_request(request);
        session.commit(&effect);

        assert_eq!(
            session.remaining_transfer_gas(),
            BLOCK_SWEEP_GAS_LIMIT - 123_456
        );
        let next = session.plan();
        assert!(next.has_seen_registry_request(request));
        assert!(!next.has_seen_registry_request(transfer));

        // A second commit drags the block meter below the per-transaction cap,
        // so the plan starts exposing the block budget directly. Drain down to
        // 100_000 remaining.
        let mut drain = SweepBlockEffect::default();
        drain.add_transfer_gas(session.remaining_transfer_gas() - 100_000);
        session.commit(&drain);
        assert_eq!(session.remaining_transfer_gas(), 100_000);
        assert_eq!(session.plan().remaining_transfer_gas(), 100_000);
    }

    #[test]
    fn single_transaction_plan_starts_with_full_transfer_allowance() {
        let plan = SweepTxPlan::single_transaction();
        assert_eq!(plan.remaining_transfer_gas(), TX_SWEEP_GAS_LIMIT);
        assert!(!plan.has_seen_registry_request(candidate(TOKEN_A)));
    }

    #[test]
    fn seen_request_is_skipped_but_a_later_transfer_for_the_pair_remains_eligible() {
        let pair = candidate(TOKEN_A);
        let mut session = SweepBlockSession::default();
        let mut effect = SweepBlockEffect::default();
        effect.record_seen_registry_request(pair);
        session.commit(&effect);
        let logs = vec![
            request_log(pair.token, pair.source),
            transfer_log(pair.token, Address::ZERO, pair.source, U256::from(1)),
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
    fn sweep_outcome_effect_records_transfer_gas_and_registry_requests() {
        let mut evm = make_evm(
            ResolverMode::Destination,
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

        // Only the RegistryRequest trigger enters the block-level seen set; the
        // TokenTransfer for the same block is not deduplicated. Both candidates
        // succeed, and the block effect records the transfers' actual gas.
        assert_eq!(
            outcome.block_effect.seen_registry_requests(),
            &[request]
        );
        assert!(outcome.block_effect.transfer_gas_used() > 0);
        assert_eq!(outcome.successes.len(), 2);
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
            first_transaction.plan.remaining_transfer_gas(),
            TX_SWEEP_GAS_LIMIT
        );
        let request = candidate(TOKEN_A);
        finish_sweep_trace_replay_transaction(
            Some(first_transaction),
            &SweepBlockEffect {
                transfer_gas_used: BLOCK_SWEEP_GAS_LIMIT - 100_000,
                seen_registry_requests: vec![request],
            },
        );

        set_sweep_trace_replay_target(second_hash);
        let second_transaction =
            sweep_trace_replay_transaction(Some(&second)).expect("second transaction");
        // The block budget drains below the per-transaction cap, so the shared
        // session exposes the remaining transfer gas in the plan.
        assert_eq!(
            second_transaction.plan.remaining_transfer_gas(),
            100_000
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
            ResolverMode::Destination,
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
    fn explicit_plan_authority_cannot_fall_back_to_trace_context() {
        let raw = Bytes::from_static(b"canonical-explicit-zero");
        begin_sweep_trace_replay(vec![alloy_primitives::keccak256(&raw)]);
        let mut evm = make_evm(
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(SweepTxPlan {
            remaining_transfer_gas: 0,
            seen_registry_requests: Vec::new(),
        }));
        TRACE_REPLAY_CONTEXT.with(|context| assert!(context.borrow().is_none()));

        let mut tx = main_tx();
        tx.rlp_bytes = Some(raw);
        assert!(evm.transact_one(tx).unwrap().is_success());
        let outcome = evm.take_sweep_outcome().unwrap();
        assert!(outcome.tx_out_of_gas);
        assert_eq!(outcome.block_effect, SweepBlockEffect::default());
        assert!(outcome.logs.is_empty());
        assert!(outcome.successes.is_empty());
        assert!(outcome.failures.is_empty());
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
            ResolverMode::Destination,
            &[(TOKEN_A, BalanceMode::Normal, TransferMode::True)],
            None,
            true,
        );
        revoked.set_sweep_execution_mode(SweepExecutionMode::Disabled);

        let mut later = make_evm(
            ResolverMode::Destination,
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
