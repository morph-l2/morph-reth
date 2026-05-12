use crate::schema::{MorphTestSuite, MorphTestUnit, SchemaError, parse_fork};
use alloy_evm::{Evm, EvmEnv};
use alloy_primitives::{B256, Bytes};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount, root::storage_root_unhashed};
use morph_chainspec::hardfork::MorphHardfork;
use morph_evm::{MorphBlockEnv, evm::MorphEvm};
use revm::{
    context::{CfgEnv, result::ExecutionResult},
    database::{EmptyDB, PlainAccount, State},
    inspector::inspectors::TracerEip3155,
    primitives::{Address, Log, U256, keccak256},
};
use revm_statetest_types::Test;
use serde::Serialize;
use std::{fs, io::stderr, path::Path};
use thiserror::Error;

#[derive(Clone, Copy, Debug, Default)]
pub struct RunnerOptions {
    pub trace: bool,
    pub validate: bool,
}

#[derive(Debug, Error)]
pub enum RunnerError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Serde(#[from] serde_json::Error),
    #[error(transparent)]
    Schema(#[from] SchemaError),
    #[error("state test validation failed: {0}")]
    Validation(String),
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    #[serde(rename = "stateRoot")]
    pub state_root: B256,
    #[serde(rename = "logsRoot")]
    pub logs_root: B256,
    pub output: Bytes,
    #[serde(rename = "gasUsed")]
    pub gas_used: u64,
    pub pass: bool,
    #[serde(rename = "errorMsg")]
    pub error_msg: String,
    #[serde(rename = "evmResult")]
    pub evm_result: String,
    #[serde(rename = "postLogsHash")]
    pub post_logs_hash: B256,
    pub fork: String,
    pub test: String,
    pub d: usize,
    pub g: usize,
    pub v: usize,
}

pub fn run_suite_str(input: &str) -> Result<Vec<Outcome>, RunnerError> {
    run_suite_str_with_options(input, RunnerOptions::default())
}

pub fn run_suite_str_with_options(
    input: &str,
    options: RunnerOptions,
) -> Result<Vec<Outcome>, RunnerError> {
    let suite: MorphTestSuite = serde_json::from_str(input)?;
    run_suite(suite, options)
}

pub fn run_path(path: &Path, options: RunnerOptions) -> Result<Vec<Outcome>, RunnerError> {
    let input = fs::read_to_string(path)?;
    run_suite_str_with_options(&input, options)
}

fn run_suite(suite: MorphTestSuite, options: RunnerOptions) -> Result<Vec<Outcome>, RunnerError> {
    let mut outcomes = Vec::new();
    for (name, unit) in suite.0 {
        for (fork_name, tests) in &unit.post {
            let fork = parse_fork(fork_name)?;
            for test in tests {
                let outcome = execute_case(&name, &unit, fork_name, fork, test, options)?;
                if options.validate && !outcome.pass {
                    return Err(RunnerError::Validation(outcome.error_msg));
                }
                outcomes.push(outcome);
            }
        }
    }
    Ok(outcomes)
}

fn execute_case(
    name: &str,
    unit: &MorphTestUnit,
    fork_name: &str,
    fork: MorphHardfork,
    test: &Test,
    options: RunnerOptions,
) -> Result<Outcome, RunnerError> {
    let cache = unit.state();
    let mut state = State::builder()
        .with_cached_prestate(cache)
        .with_bundle_update()
        .build();
    let mut cfg = CfgEnv::<MorphHardfork>::default()
        .with_chain_id(
            unit.env
                .current_chain_id
                .unwrap_or(U256::ONE)
                .try_into()
                .unwrap_or(1),
        )
        .with_spec_and_mainnet_gas_params(fork);
    cfg.disable_eip7623 = true;

    let block = unit.block_env(&mut cfg);
    cfg.tx_gas_limit_cap = Some(block.gas_limit);

    let tx = match unit.morph_tx_env(test) {
        Ok(tx) => tx,
        Err(error) if test.expect_exception.is_some() => {
            let exec_result: Result<ExecutionResult<morph_revm::MorphHaltReason>, String> =
                Err(error.to_string());
            return Ok(build_outcome(
                name,
                fork_name,
                test,
                &exec_result,
                &state,
                unit.out.as_ref(),
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let env = EvmEnv {
        cfg_env: cfg,
        block_env: MorphBlockEnv { inner: block },
    };

    if options.trace {
        let mut evm = MorphEvm::new(&mut state, env)
            .with_inspector(TracerEip3155::buffered(stderr()).without_summary());
        evm.enable_inspector();
        let exec_result = evm.transact_commit(tx);
        return Ok(build_outcome(
            name,
            fork_name,
            test,
            &exec_result,
            &state,
            unit.out.as_ref(),
        ));
    }

    let mut evm = MorphEvm::new(&mut state, env);
    let exec_result = evm.transact_commit(tx);
    Ok(build_outcome(
        name,
        fork_name,
        test,
        &exec_result,
        &state,
        unit.out.as_ref(),
    ))
}

fn build_outcome<E>(
    name: &str,
    fork_name: &str,
    test: &Test,
    exec_result: &Result<ExecutionResult<morph_revm::MorphHaltReason>, E>,
    db: &State<EmptyDB>,
    expected_output: Option<&Bytes>,
) -> Outcome
where
    E: std::fmt::Display,
{
    let logs_root = log_rlp_hash(
        exec_result
            .as_ref()
            .map(|result| result.logs())
            .unwrap_or(&[]),
    );
    let state_root = state_merkle_trie_root(db.cache.trie_account());
    let error_msg = validation_error(test, exec_result, expected_output, state_root, logs_root);
    let output = exec_result
        .as_ref()
        .ok()
        .and_then(|result| result.output().cloned())
        .unwrap_or_default();
    let gas_used = exec_result
        .as_ref()
        .ok()
        .map(ExecutionResult::tx_gas_used)
        .unwrap_or_default();

    Outcome {
        state_root,
        logs_root,
        output,
        gas_used,
        pass: error_msg.is_none(),
        error_msg: error_msg.unwrap_or_default(),
        evm_result: format_evm_result(exec_result),
        post_logs_hash: logs_root,
        fork: fork_name.to_string(),
        test: name.to_string(),
        d: test.indexes.data,
        g: test.indexes.gas,
        v: test.indexes.value,
    }
}

fn validation_error<E>(
    test: &Test,
    exec_result: &Result<ExecutionResult<morph_revm::MorphHaltReason>, E>,
    expected_output: Option<&Bytes>,
    state_root: B256,
    logs_root: B256,
) -> Option<String>
where
    E: std::fmt::Display,
{
    match (&test.expect_exception, exec_result) {
        (Some(_), Err(_)) => return None,
        (Some(expected), Ok(_)) => {
            return Some(format!(
                "expected exception {expected:?}, but execution succeeded"
            ));
        }
        (None, Err(error)) => return Some(format!("unexpected execution error: {error}")),
        (None, Ok(_)) => {}
    }

    if let (Some(expected), Ok(result)) = (expected_output, exec_result)
        && result.output() != Some(expected)
    {
        return Some(format!(
            "unexpected output: got {:?}, expected {expected:?}",
            result.output()
        ));
    }
    if logs_root != test.logs {
        return Some(format!(
            "logs root mismatch: got {logs_root}, expected {}",
            test.logs
        ));
    }
    if state_root != test.hash {
        return Some(format!(
            "state root mismatch: got {state_root}, expected {}",
            test.hash
        ));
    }
    None
}

fn format_evm_result<E>(
    exec_result: &Result<ExecutionResult<morph_revm::MorphHaltReason>, E>,
) -> String
where
    E: std::fmt::Display,
{
    match exec_result {
        Ok(ExecutionResult::Success { reason, .. }) => format!("Success: {reason:?}"),
        Ok(ExecutionResult::Revert { .. }) => "Revert".to_string(),
        Ok(ExecutionResult::Halt { reason, .. }) => format!("Halt: {reason:?}"),
        Err(error) => error.to_string(),
    }
}

fn log_rlp_hash(logs: &[Log]) -> B256 {
    let mut out = Vec::with_capacity(alloy_rlp::list_length(logs));
    alloy_rlp::encode_list(logs, &mut out);
    keccak256(&out)
}

fn state_merkle_trie_root<'a>(
    accounts: impl IntoIterator<Item = (Address, &'a PlainAccount)>,
) -> B256 {
    let mut accounts: Vec<_> = accounts
        .into_iter()
        .map(|(address, account)| {
            let storage_root = storage_root_unhashed(
                account
                    .storage
                    .iter()
                    .filter(|&(_, &value)| !value.is_zero())
                    .map(|(key, value)| (B256::from(*key), *value)),
            );
            (
                keccak256(address),
                TrieAccount {
                    nonce: account.info.nonce,
                    balance: account.info.balance,
                    storage_root,
                    code_hash: account.info.code_hash,
                },
            )
        })
        .collect();
    accounts.sort_unstable_by_key(|(key, _)| *key);

    let mut trie = HashBuilder::default();
    let mut account_rlp = Vec::new();
    for (hashed_key, account) in accounts {
        account_rlp.clear();
        alloy_rlp::Encodable::encode(&account, &mut account_rlp);
        trie.add_leaf(Nibbles::unpack(hashed_key), &account_rlp);
    }
    trie.root()
}
