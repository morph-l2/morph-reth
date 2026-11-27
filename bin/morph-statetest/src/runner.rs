use crate::{
    schema::{MorphTestSuite, MorphTestUnit, SchemaError, Test, parse_fork},
    trace::Eip3155Tracer,
};
use alloy_consensus::transaction::Recovered;
use alloy_primitives::{Address, B256, Bytes, Log, U256, address, keccak256};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount, root::storage_root_unhashed};
use evm2::TxResult;
use morph_chainspec::hardfork::MorphHardfork;
use morph_revm::{FeeFrameFilter, MorphEvm, MorphEvmTypes, morph_evm, morph_evm_env};
use serde::Serialize;
use std::{collections::BTreeSet, fs, io::stderr, path::Path};
use thiserror::Error;

const MORPH_STATE_TEST_FEE_VAULT_ADDRESS: Address =
    address!("48442aa154897eef141df231cc1517fc8c1d170f");

/// A transaction's outcome, or why it was rejected.
type ExecResult = Result<TxResult<MorphEvmTypes>, String>;

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
    let chain_id = unit
        .env
        .current_chain_id
        .unwrap_or(U256::ONE)
        .try_into()
        .unwrap_or(1);

    let mut block = unit.block_env();
    if fork.is_curie() {
        block.beneficiary = MORPH_STATE_TEST_FEE_VAULT_ADDRESS;
    }
    let block_gas_limit = block.gas_limit.saturating_to::<u64>();

    let mut evm = morph_evm(
        unit.state(),
        morph_evm_env(fork, chain_id, block_gas_limit, block),
    );

    let tx = match unit.morph_tx_env(test, fork) {
        Ok(tx) => tx,
        Err(error) if test.expect_exception.is_some() => {
            return Ok(build_outcome(
                name,
                fork_name,
                unit,
                test,
                &Err(error.to_string()),
                &mut evm,
            ));
        }
        Err(error) => return Err(error.into()),
    };
    let tx = Recovered::new_unchecked(tx.env, tx.caller);

    let exec_result: ExecResult = if options.trace {
        let mut tracer = FeeFrameFilter::new(Eip3155Tracer::new(stderr()));
        evm.transact_with_inspector(&tx, &mut tracer)
            .map(|executed| executed.commit())
            .map_err(|error| error.to_string())
    } else {
        evm.transact(&tx)
            .map(|executed| executed.commit())
            .map_err(|error| error.to_string())
    };

    Ok(build_outcome(
        name,
        fork_name,
        unit,
        test,
        &exec_result,
        &mut evm,
    ))
}

fn build_outcome(
    name: &str,
    fork_name: &str,
    unit: &MorphTestUnit,
    test: &Test,
    exec_result: &ExecResult,
    evm: &mut MorphEvm<'_>,
) -> Outcome {
    let no_logs = Vec::new();
    let logs = exec_result.as_ref().map_or(&no_logs, |result| &result.logs);
    let logs_root = log_rlp_hash(logs);
    let state_root = state_merkle_trie_root(unit, evm);
    let error_msg = validation_error(test, exec_result, unit.out.as_ref(), state_root, logs_root);
    let output = exec_result
        .as_ref()
        .ok()
        .and_then(result_output)
        .cloned()
        .unwrap_or_default();
    let gas_used = exec_result
        .as_ref()
        .ok()
        .map(|result| result.tx_gas_used())
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

/// The output of a transaction that ran to a result; a halt has none.
fn result_output(result: &TxResult<MorphEvmTypes>) -> Option<&Bytes> {
    (!result.is_halt()).then_some(&result.output)
}

fn validation_error(
    test: &Test,
    exec_result: &ExecResult,
    expected_output: Option<&Bytes>,
    state_root: B256,
    logs_root: B256,
) -> Option<String> {
    // `expectException` is checked for presence, deliberately not for its text.
    // go-ethereum's own statetest harness does the same -- `tests/state_test.go`
    // returns early on `len(ExpectException) > 0` under a standing
    // "TODO check error string" -- so matching on the text here would make this
    // runner stricter than the client the fixtures are generated from, and a
    // fixture imported from go-ethereum could fail on wording alone. The string
    // stays in the JSON as documentation of which failure the case is meant to
    // provoke; the assertion is that the transaction is *rejected*, which is what
    // both clients agree on.
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
        && result_output(result) != Some(expected)
    {
        return Some(format!(
            "unexpected output: got {:?}, expected {expected:?}",
            result_output(result)
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

fn format_evm_result(exec_result: &ExecResult) -> String {
    match exec_result {
        Ok(result) if result.is_success() => format!("Success: {:?}", result.stop),
        Ok(result) if result.is_revert() => "Revert".to_string(),
        Ok(result) => format!("Halt: {:?}", result.stop),
        Err(error) => error.clone(),
    }
}

fn log_rlp_hash(logs: &[Log]) -> B256 {
    let mut out = Vec::with_capacity(alloy_rlp::list_length(logs));
    alloy_rlp::encode_list(logs, &mut out);
    keccak256(&out)
}

/// Computes the state root over the prestate accounts and everything execution accepted.
fn state_merkle_trie_root(unit: &MorphTestUnit, evm: &mut MorphEvm<'_>) -> B256 {
    let cache = &evm.overlay_db().cache;
    let addresses: BTreeSet<Address> = unit
        .pre
        .keys()
        .chain(cache.accounts.keys())
        .copied()
        .collect();
    let written_slots: Vec<(Address, Vec<U256>)> = cache
        .storage
        .iter()
        .map(|(address, storage)| (*address, storage.slots.keys().copied().collect()))
        .collect();

    let state = evm.state_mut();
    let mut accounts = Vec::with_capacity(addresses.len());
    for address in addresses {
        let info = state
            .account_info_untracked(&address)
            .expect("the in-memory database cannot fail");
        let Some(info) = info else {
            continue;
        };

        let mut slots: BTreeSet<U256> = unit
            .pre
            .get(&address)
            .map(|account| account.storage.keys().copied().collect())
            .unwrap_or_default();
        slots.extend(
            written_slots
                .iter()
                .filter(|(written, _)| *written == address)
                .flat_map(|(_, keys)| keys.iter().copied()),
        );
        let storage: Vec<(B256, U256)> = slots
            .into_iter()
            .map(|key| {
                let value = state
                    .storage_slot_untracked(&address, &key)
                    .expect("the in-memory database cannot fail");
                (B256::from(key), value)
            })
            .filter(|(_, value)| !value.is_zero())
            .collect();

        accounts.push((
            keccak256(address),
            TrieAccount {
                nonce: info.nonce,
                balance: info.balance,
                storage_root: storage_root_unhashed(storage),
                code_hash: info.code_hash,
            },
        ));
    }
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
