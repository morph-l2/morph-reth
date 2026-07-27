use crate::schema::{MorphTestSuite, MorphTestUnit, SchemaError, parse_fork};
use alloy_evm::{Evm, EvmEnv};
use alloy_primitives::{B256, Bytes};
use alloy_trie::{HashBuilder, Nibbles, TrieAccount, root::storage_root_unhashed};
use morph_chainspec::hardfork::MorphHardfork;
use morph_evm::{MorphBlockEnv, evm::MorphEvm};
use morph_revm::{MAX_CANDIDATES_PER_TX, SweepConfig, SweepExecutionMode, SweepTxPlan};
use revm::{
    context::{CfgEnv, result::ExecutionResult},
    database::{EmptyDB, PlainAccount, State},
    inspector::inspectors::TracerEip3155,
    primitives::{Address, Log, U256, address, keccak256},
};
use revm_statetest_types::Test;
use serde::Serialize;
use std::{fs, io::stderr, path::Path};
use thiserror::Error;

const MORPH_STATE_TEST_FEE_VAULT_ADDRESS: Address =
    address!("48442aa154897eef141df231cc1517fc8c1d170f");

/// Canonical sweep registry address for Onyx statetest vectors.
///
/// Mirrors the `sweepRegistryAddress` used by the test chain
/// config and the morph-node e2e suite, so the same fixtures resolve against a
/// registry the `pre` state deploys at this address. A fixture may override it
/// via `sweepRegistry`.
const MORPH_STATE_TEST_SWEEP_REGISTRY: Address =
    address!("5300000000000000000000000000000000000023");

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

    let mut block = unit.block_env(&mut cfg);
    if fork.is_curie() {
        block.beneficiary = MORPH_STATE_TEST_FEE_VAULT_ADDRESS;
    }
    cfg.tx_gas_limit_cap = Some(block.gas_limit);

    let tx = match unit.morph_tx_env(test, fork) {
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
                &[],
            ));
        }
        Err(error) => return Err(error.into()),
    };
    // From Onyx onward the EL sweeps whitelisted ERC-20 inflows to the
    // registered master after the main transaction. A single statetest runs one
    // transaction, so the per-transaction candidate cap is the whole allowance.
    let sweep = fork.is_onyx().then(|| SweepConfig {
        registry_address: unit
            .sweep_registry
            .unwrap_or(MORPH_STATE_TEST_SWEEP_REGISTRY),
    });
    let env = EvmEnv {
        cfg_env: cfg,
        block_env: MorphBlockEnv {
            inner: block,
            sweep,
        },
    };

    if options.trace {
        let mut evm = MorphEvm::new(&mut state, env)
            .with_inspector(TracerEip3155::buffered(stderr()).without_summary());
        evm.enable_inspector();
        evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
            SweepTxPlan::single_transaction(MAX_CANDIDATES_PER_TX),
        ));
        let exec_result = evm.transact_commit(tx);
        let receipt_logs = collect_receipt_logs(&mut evm, &exec_result);
        return Ok(build_outcome(
            name,
            fork_name,
            test,
            &exec_result,
            &state,
            unit.out.as_ref(),
            &receipt_logs,
        ));
    }

    let mut evm = MorphEvm::new(&mut state, env);
    evm.set_sweep_execution_mode(SweepExecutionMode::Canonical(
        SweepTxPlan::single_transaction(MAX_CANDIDATES_PER_TX),
    ));
    let exec_result = evm.transact_commit(tx);
    let receipt_logs = collect_receipt_logs(&mut evm, &exec_result);
    Ok(build_outcome(
        name,
        fork_name,
        test,
        &exec_result,
        &state,
        unit.out.as_ref(),
        &receipt_logs,
    ))
}

fn build_outcome<E>(
    name: &str,
    fork_name: &str,
    test: &Test,
    exec_result: &Result<ExecutionResult<morph_revm::MorphHaltReason>, E>,
    db: &State<EmptyDB>,
    expected_output: Option<&Bytes>,
    receipt_logs: &[Log],
) -> Outcome
where
    E: std::fmt::Display,
{
    let logs_root = log_rlp_hash(receipt_logs);
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

fn collect_receipt_logs<DB, I, E>(
    evm: &mut MorphEvm<DB, I>,
    exec_result: &Result<ExecutionResult<morph_revm::MorphHaltReason>, E>,
) -> Vec<Log>
where
    DB: alloy_evm::Database,
    I: revm::Inspector<morph_revm::evm::MorphContext<DB>>,
{
    let mut logs = evm.take_pre_fee_logs();
    if let Ok(result) = exec_result {
        logs.extend(result.logs().iter().cloned());
    }
    logs.extend(evm.take_post_fee_logs());
    // Sweep logs (token call logs + synthesized `Swept`)
    // are appended last, matching the block executor's receipt ordering.
    if let Some(outcome) = evm.take_sweep_outcome() {
        logs.extend(outcome.logs);
    }
    logs
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;

    // Minimal slot-1 ERC-20 (`balanceOf`/`transfer` with a Transfer event); the
    // balance mapping lives at slot 1. Shared with the morph-node e2e suite.
    const SLOT1_ERC20_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806370a0823114610038578063a9059cbb1461006a575b5f5ffd5b61005761004636600461015e565b60016020525f908152604090205481565b6040519081526020015b60405180910390f35b61007d61007836600461017e565b61008d565b6040519015158152602001610061565b335f90815260016020526040812054828110156100da5760405162461bcd60e51b815260206004820152600760248201526662616c616e636560c81b604482015260640160405180910390fd5b335f81815260016020908152604080832087860390556001600160a01b03881680845292819020805488019055518681529192917fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef910160405180910390a35060019392505050565b80356001600160a01b0381168114610159575f5ffd5b919050565b5f6020828403121561016e575f5ffd5b61017782610143565b9392505050565b5f5f6040838503121561018f575f5ffd5b61019883610143565b94602093909301359350505056";

    // Test-only Registry returns a single 32-byte `address master` from the
    // frozen `resolveSweep(token,deposit)` selector, matching the V1 EL ABI.
    const TEST_REGISTRY_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b506004361061004a575f3560e01c8063663a375c1461004e578063753d7563146100635780639faa2f2f1461009a578063ba1b6c84146100c5575b5f5ffd5b61006161005c3660046101d4565b610122565b005b610085610071366004610205565b60016020525f908152604090205460ff1681565b60405190151581526020015b60405180910390f35b6100ad6100a83660046101d4565b610166565b6040516001600160a01b039091168152602001610091565b6100616100d3366004610225565b6001600160a01b039283165f908152600160208181526040808420805460ff19169093179092558281528183209486168352939093529190912080546001600160a01b03191691909216179055565b806001600160a01b0316826001600160a01b03167f24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff60405160405180910390a35050565b6001600160a01b0382165f9081526001602052604081205460ff1661018c57505f6101b3565b506001600160a01b038083165f908152602081815260408083208585168452909152902054165b92915050565b80356001600160a01b03811681146101cf575f5ffd5b919050565b5f5f604083850312156101e5575f5ffd5b6101ee836101b9565b91506101fc602084016101b9565b90509250929050565b5f60208284031215610215575f5ffd5b61021e826101b9565b9392505050565b5f5f5f60608486031215610237575f5ffd5b610240846101b9565b925061024e602085016101b9565b915061025c604085016101b9565b9050925092509256";

    const SENDER: Address = address!("a94f5374fce5edbc8e2a8697c15331677e6ebf0b");
    const TOKEN: Address = address!("00000000000000000000000000000000000000aa");
    const DEPOSIT: Address = address!("00000000000000000000000000000000000000d0");
    const MASTER: Address = address!("00000000000000000000000000000000000000e0");
    const AMOUNT: u64 = 1000;

    fn erc20_balance_slot(account: Address) -> B256 {
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(account.as_slice());
        preimage[63] = 1;
        keccak256(preimage)
    }

    fn registry_master_slot(token: Address, deposit: Address) -> B256 {
        let mut inner_preimage = [0u8; 64];
        inner_preimage[12..32].copy_from_slice(token.as_slice());
        let inner = keccak256(inner_preimage);
        let mut outer_preimage = [0u8; 64];
        outer_preimage[12..32].copy_from_slice(deposit.as_slice());
        outer_preimage[32..64].copy_from_slice(inner.as_slice());
        keccak256(outer_preimage)
    }

    fn registry_whitelist_slot(token: Address) -> B256 {
        let mut preimage = [0u8; 64];
        preimage[12..32].copy_from_slice(token.as_slice());
        preimage[63] = 1;
        keccak256(preimage)
    }

    fn word(value: B256) -> String {
        hex::encode_prefixed(value)
    }

    /// A `transfer(deposit, AMOUNT)` inflow triggers the EL sweep only under
    /// Onyx: the deposit balance is swept to master and extra logs are appended,
    /// so both the state root and the logs root diverge from the Jade run.
    #[test]
    fn onyx_sweeps_inflow_to_master_and_diverges_from_jade() {
        let transfer_data = {
            let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
            data.extend_from_slice(B256::left_padding_from(DEPOSIT.as_slice()).as_slice());
            data.extend_from_slice(&U256::from(AMOUNT).to_be_bytes::<32>());
            hex::encode_prefixed(data)
        };
        let fixture = format!(
            r#"{{
              "case": {{
                "env": {{
                  "currentChainID": "0x1",
                  "currentCoinbase": "0x0000000000000000000000000000000000000000",
                  "currentDifficulty": "0x0",
                  "currentGasLimit": "0x989680",
                  "currentNumber": "0x1",
                  "currentTimestamp": "0x1",
                  "currentBaseFee": "0x0"
                }},
                "pre": {{
                  "{sender}": {{ "balance": "0x3635c9adc5dea00000", "nonce": "0x0", "code": "0x", "storage": {{}} }},
                  "{token}": {{ "balance": "0x0", "nonce": "0x0", "code": "{erc20}", "storage": {{ "{sender_bal_slot}": "{amount_word}" }} }},
                  "{deposit}": {{ "balance": "0x0", "nonce": "0x0", "code": "0x", "storage": {{}} }},
                  "{registry}": {{ "balance": "0x0", "nonce": "0x0", "code": "{registry_code}", "storage": {{ "{master_slot}": "{master_word}", "{whitelist_slot}": "0x01" }} }}
                }},
                "transaction": {{
                  "nonce": "0x0",
                  "gasPrice": "0x1",
                  "gasLimit": ["0x100000"],
                  "to": "{token}",
                  "value": ["0x0"],
                  "data": ["{data}"],
                  "sender": "{sender}",
                  "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
                }},
                "post": {{
                  "Jade": [{{ "indexes": {{ "data": 0, "gas": 0, "value": 0 }}, "hash": "0x0000000000000000000000000000000000000000000000000000000000000000", "logs": "0x0000000000000000000000000000000000000000000000000000000000000000", "expectException": null }}],
                  "Onyx": [{{ "indexes": {{ "data": 0, "gas": 0, "value": 0 }}, "hash": "0x0000000000000000000000000000000000000000000000000000000000000000", "logs": "0x0000000000000000000000000000000000000000000000000000000000000000", "expectException": null }}]
                }}
              }}
            }}"#,
            sender = hex::encode_prefixed(SENDER),
            token = hex::encode_prefixed(TOKEN),
            deposit = hex::encode_prefixed(DEPOSIT),
            registry = hex::encode_prefixed(MORPH_STATE_TEST_SWEEP_REGISTRY),
            erc20 = SLOT1_ERC20_RUNTIME,
            registry_code = TEST_REGISTRY_RUNTIME,
            sender_bal_slot = word(erc20_balance_slot(SENDER)),
            amount_word = word(B256::from(U256::from(AMOUNT).to_be_bytes())),
            master_slot = word(registry_master_slot(TOKEN, DEPOSIT)),
            whitelist_slot = word(registry_whitelist_slot(TOKEN)),
            master_word = word(B256::left_padding_from(MASTER.as_slice())),
            data = transfer_data,
        );

        let outcomes = run_suite_str(&fixture).expect("suite should execute");
        let jade = outcomes
            .iter()
            .find(|o| o.fork == "Jade")
            .expect("Jade outcome");
        let onyx = outcomes
            .iter()
            .find(|o| o.fork == "Onyx")
            .expect("Onyx outcome");

        assert!(
            jade.evm_result.starts_with("Success"),
            "Jade main tx must succeed: {}",
            jade.evm_result
        );
        assert!(
            onyx.evm_result.starts_with("Success"),
            "Onyx main tx must succeed: {}",
            onyx.evm_result
        );
        assert_ne!(
            jade.state_root, onyx.state_root,
            "Onyx sweep must move the deposit balance to master and change the state root"
        );
        assert_ne!(
            jade.logs_root, onyx.logs_root,
            "Onyx sweep must append the sweep Transfer and Swept logs"
        );
    }

    /// Without a registry deployed at the configured address, an Onyx inflow has
    /// nothing to resolve against, so the run matches Jade (no sweep).
    #[test]
    fn onyx_without_registry_matches_jade() {
        let transfer_data = {
            let mut data = vec![0xa9, 0x05, 0x9c, 0xbb];
            data.extend_from_slice(B256::left_padding_from(DEPOSIT.as_slice()).as_slice());
            data.extend_from_slice(&U256::from(AMOUNT).to_be_bytes::<32>());
            hex::encode_prefixed(data)
        };
        let fixture = format!(
            r#"{{
              "case": {{
                "env": {{
                  "currentChainID": "0x1",
                  "currentCoinbase": "0x0000000000000000000000000000000000000000",
                  "currentDifficulty": "0x0",
                  "currentGasLimit": "0x989680",
                  "currentNumber": "0x1",
                  "currentTimestamp": "0x1",
                  "currentBaseFee": "0x0"
                }},
                "pre": {{
                  "{sender}": {{ "balance": "0x3635c9adc5dea00000", "nonce": "0x0", "code": "0x", "storage": {{}} }},
                  "{token}": {{ "balance": "0x0", "nonce": "0x0", "code": "{erc20}", "storage": {{ "{sender_bal_slot}": "{amount_word}" }} }}
                }},
                "transaction": {{
                  "nonce": "0x0",
                  "gasPrice": "0x1",
                  "gasLimit": ["0x100000"],
                  "to": "{token}",
                  "value": ["0x0"],
                  "data": ["{data}"],
                  "sender": "{sender}",
                  "secretKey": "0x45a915e4d060149eb4365960e6a7a45f334393093061116b197e3240065ff2d8"
                }},
                "post": {{
                  "Jade": [{{ "indexes": {{ "data": 0, "gas": 0, "value": 0 }}, "hash": "0x0000000000000000000000000000000000000000000000000000000000000000", "logs": "0x0000000000000000000000000000000000000000000000000000000000000000", "expectException": null }}],
                  "Onyx": [{{ "indexes": {{ "data": 0, "gas": 0, "value": 0 }}, "hash": "0x0000000000000000000000000000000000000000000000000000000000000000", "logs": "0x0000000000000000000000000000000000000000000000000000000000000000", "expectException": null }}]
                }}
              }}
            }}"#,
            sender = hex::encode_prefixed(SENDER),
            token = hex::encode_prefixed(TOKEN),
            erc20 = SLOT1_ERC20_RUNTIME,
            sender_bal_slot = word(erc20_balance_slot(SENDER)),
            amount_word = word(B256::from(U256::from(AMOUNT).to_be_bytes())),
            data = transfer_data,
        );

        let outcomes = run_suite_str(&fixture).expect("suite should execute");
        let jade = outcomes.iter().find(|o| o.fork == "Jade").unwrap();
        let onyx = outcomes.iter().find(|o| o.fork == "Onyx").unwrap();

        assert_eq!(
            jade.state_root, onyx.state_root,
            "with no registry there is nothing to resolve, so Onyx must not sweep"
        );
        assert_eq!(
            jade.logs_root, onyx.logs_root,
            "no sweep means no extra logs"
        );
    }
}
