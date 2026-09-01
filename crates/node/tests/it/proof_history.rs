//! Historical-proof correctness tests.
//!
//! These exercise `eth_getProof`, `eth_getMultiProof` and `debug_executionWitness`
//! against the durable proof-history index, not just RPC wiring. Each successful proof is checked
//! against that block's canonical `stateRoot`. Reference-index coverage lives
//! in `reference_index.rs`.

use std::{sync::Arc, time::Duration};

use alloy_consensus::{SignableTransaction, TxEip1559, constants::KECCAK_EMPTY};
use alloy_eips::{Encodable2718, NumHash, eip1898::BlockWithParent};
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_rpc_types_debug::ExecutionWitness;
use alloy_rpc_types_eth::{Block, EIP1186AccountProofResponse};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use jsonrpsee::{core::client::ClientT, http_client::HttpClient, rpc_params};
use morph_chainspec::MorphChainSpec;
use morph_node::{
    MorphAddOns, MorphNode,
    test_utils::{
        MorphTestNode, MorphTxBuilder, make_deploy_tx, morph_payload_attributes, wallet_at_index,
    },
};
use morph_payload_types::ExecutableL2Data;
use morph_primitives::MorphHeader;
use morph_proofs::{
    DEFAULT_PROOFS_HISTORY_WINDOW, InitializationJob, MdbxProofsStorage, MorphProofsStorage,
    MorphProofsStore, ProofDbIdentity,
};
use morph_proofs_exex::MorphProofsExEx;
use morph_rpc::{ProofsSyncStatus, eth::proofs::DEFAULT_MAX_MULTI_PROOF_TARGETS};
use reth_chainspec::EthChainSpec;
use reth_e2e_test_utils::node::NodeTestContext;
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use reth_payload_primitives::BuiltPayload;
use reth_provider::{
    AccountReader, BlockReaderIdExt, DBProvider, DatabaseProviderFactory, ReceiptProvider,
    providers::BlockchainProvider,
};
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;
use reth_transaction_pool::TransactionPool;
use reth_trie::{AccountProof, EMPTY_ROOT_HASH};
use serde::Serialize;

const CHAIN_ID: u64 = 2910;
const ACCOUNT0: Address = alloy_primitives::address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const ACCOUNT1: Address = alloy_primitives::address!("70997970C51812dc3A010C7d01b50e0d17dc79C8");
const ACCOUNT2: Address = alloy_primitives::address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC");
const GENESIS_CONTRACT: Address =
    alloy_primitives::address!("530000000000000000000000000000000000000f");
const GENESIS_SLOT: B256 = B256::with_last_byte(0x06);
/// Never funded and never a sender, so it has no account-trie leaf at any tested height.
const ABSENT: Address = alloy_primitives::address!("00000000000000000000000000000000000abbe1");

/// Runtime: `PUSH1 0; CALLDATALOAD; PUSH1 0; SSTORE; STOP`.
///
/// Init code copies those 7 bytes and returns them as the deployed account.
const SETTER_INIT: &[u8] = &[
    0x60, 0x07, // PUSH1 7
    0x60, 0x0c, // PUSH1 12
    0x60, 0x00, // PUSH1 0
    0x39, // CODECOPY
    0x60, 0x07, // PUSH1 7
    0x60, 0x00, // PUSH1 0
    0xf3, // RETURN
    0x60, 0x00, 0x35, 0x60, 0x00, 0x55, 0x00,
];

#[derive(Clone, Copy)]
struct LaunchOpts {
    window: u64,
    prune_interval: Duration,
    verification_interval: u64,
    max_multi_proof_targets: usize,
}

impl Default for LaunchOpts {
    fn default() -> Self {
        Self {
            window: DEFAULT_PROOFS_HISTORY_WINDOW,
            // Keep the background pruner idle unless a test is specifically
            // asserting prune behavior.
            prune_interval: Duration::from_secs(3600),
            verification_interval: 0,
            max_multi_proof_targets: DEFAULT_MAX_MULTI_PROOF_TARGETS,
        }
    }
}

struct ProofHistoryHarness {
    node: MorphTestNode,
    storage: MorphProofsStorage<Arc<MdbxProofsStorage>>,
    _proofs_dir: tempfile::TempDir,
    chain_spec: Arc<MorphChainSpec>,
}

fn signer(index: u32) -> PrivateKeySigner {
    wallet_at_index(index, CHAIN_ID)
}

fn signed_transfer(from: u32, nonce: u64, to: Address, value: u64) -> eyre::Result<Bytes> {
    let tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: 21_000,
        max_fee_per_gas: 20_000_000_000u128,
        max_priority_fee_per_gas: 20_000_000_000u128,
        to: TxKind::Call(to),
        value: U256::from(value),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig = signer(from)
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|error| eyre::eyre!("signing transfer failed: {error}"))?;
    Ok(tx.into_signed(sig).encoded_2718().into())
}

fn signed_call(from: u32, nonce: u64, to: Address, data: Bytes) -> eyre::Result<Bytes> {
    MorphTxBuilder::new(CHAIN_ID, signer(from), nonce)
        .with_v1_eth_fee()
        .with_to(to)
        .with_data(data)
        .with_gas_limit(100_000)
        .build_signed()
}

fn slot_calldata(value: u64) -> Bytes {
    Bytes::from(B256::from(U256::from(value)).to_vec())
}

async fn launch_node(
    chain_spec: Arc<MorphChainSpec>,
    storage: MorphProofsStorage<Arc<MdbxProofsStorage>>,
    opts: LaunchOpts,
) -> eyre::Result<MorphTestNode> {
    let runtime = Runtime::test();
    let network = NetworkArgs {
        discovery: DiscoveryArgs {
            disable_discovery: true,
            ..DiscoveryArgs::default()
        },
        ..NetworkArgs::default()
    };
    let node_config = NodeConfig::new(chain_spec.clone())
        .with_network(network)
        .with_unused_ports()
        .with_rpc(
            RpcServerArgs::default()
                .with_unused_ports()
                .with_http()
                .with_http_api(RpcModuleSelection::All),
        );
    let node = MorphNode::default();
    let exex_storage = storage.clone();

    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime.clone())
        .with_types_and_provider::<MorphNode, BlockchainProvider<_>>()
        .with_components(node.components_builder())
        .with_add_ons(MorphAddOns::new().with_proof_history(storage, opts.max_multi_proof_targets))
        .install_exex("morph-proof-history", async move |ctx| {
            let head = ctx.head;
            let provider = ctx
                .provider()
                .database_provider_ro()?
                .disable_long_read_transaction_safety();
            InitializationJob::new(exex_storage.clone(), provider.into_tx())
                .run(head.number, head.hash)?;

            let exex = MorphProofsExEx::builder(ctx, exex_storage)
                .with_proofs_history_window(opts.window)
                .with_proofs_history_prune_interval(opts.prune_interval)
                .with_verification_interval(opts.verification_interval)
                .build();
            Ok(async move { exex.run().await })
        })
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                reth_node_api::TreeConfig::default().with_cross_block_cache_size(1024 * 1024),
            );
            builder.launch_with(launcher)
        })
        .await?;

    let node = NodeTestContext::new(node, morph_payload_attributes).await?;
    let genesis_hash = chain_spec.genesis_hash();
    node.update_forkchoice(genesis_hash, genesis_hash).await?;
    Ok(node)
}

async fn setup(opts: LaunchOpts) -> eyre::Result<ProofHistoryHarness> {
    let genesis: Genesis = serde_json::from_str(include_str!("../assets/test-genesis.json"))?;
    let chain_spec = Arc::new(MorphChainSpec::from_genesis(genesis));
    let proofs_dir = tempfile::tempdir()?;
    let storage = Arc::new(MdbxProofsStorage::open(
        proofs_dir.path(),
        ProofDbIdentity::new(chain_spec.chain().id(), chain_spec.genesis_hash()),
    )?);
    let node = launch_node(chain_spec.clone(), storage.clone(), opts).await?;
    Ok(ProofHistoryHarness {
        node,
        storage,
        _proofs_dir: proofs_dir,
        chain_spec,
    })
}

fn rpc_client(node: &MorphTestNode) -> eyre::Result<HttpClient> {
    node.rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))
}

async fn wait_for_proof_status(
    client: &HttpClient,
    expected: impl Fn(&ProofsSyncStatus) -> bool,
    context: &str,
) -> eyre::Result<ProofsSyncStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last_status = None;
    while tokio::time::Instant::now() < deadline {
        if let Ok(status) = client
            .request::<ProofsSyncStatus, _>("debug_proofsSyncStatus", rpc_params![])
            .await
        {
            if expected(&status) {
                return Ok(status);
            }
            last_status = Some(status);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre::eyre!(
        "proof history did not reach {context}: {last_status:?}"
    ))
}

async fn wait_for_proof_tip(client: &HttpClient, expected: u64) -> eyre::Result<ProofsSyncStatus> {
    wait_for_proof_status(
        client,
        |status| status.latest == Some(expected),
        &format!("tip {expected}"),
    )
    .await
}

async fn wait_for_window(
    client: &HttpClient,
    earliest: u64,
    latest: u64,
) -> eyre::Result<ProofsSyncStatus> {
    wait_for_proof_status(
        client,
        |status| status.earliest == Some(earliest) && status.latest == Some(latest),
        &format!("window [{earliest}, {latest}]"),
    )
    .await
}

/// Waits for proof history to anchor on an exact `(number, hash)` tip.
///
/// `debug_proofsSyncStatus` only reports heights, so a reorg that keeps the height
/// but swaps the branch is invisible to [`wait_for_proof_tip`].
async fn wait_for_proof_latest(
    storage: &MorphProofsStorage<Arc<MdbxProofsStorage>>,
    number: u64,
    hash: B256,
) -> eyre::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut last = None;
    while tokio::time::Instant::now() < deadline {
        let latest = storage.get_latest_block_number()?;
        if latest == Some((number, hash)) {
            return Ok(());
        }
        last = latest;
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    Err(eyre::eyre!(
        "proof history latest never became ({number}, {hash}); last seen {last:?}"
    ))
}

async fn get_block<B: Serialize>(client: &HttpClient, block: B) -> eyre::Result<Block> {
    client
        .request("eth_getBlockByNumber", rpc_params![block, false])
        .await
        .map_err(|error| eyre::eyre!("eth_getBlockByNumber failed: {error}"))
}

async fn get_proof<B: Serialize>(
    client: &HttpClient,
    address: Address,
    slots: Vec<B256>,
    block: B,
) -> eyre::Result<EIP1186AccountProofResponse> {
    client
        .request("eth_getProof", rpc_params![address, slots, block])
        .await
        .map_err(|error| eyre::eyre!("eth_getProof failed: {error}"))
}

async fn get_multi_proof<B: Serialize>(
    client: &HttpClient,
    targets: Vec<(Address, Vec<B256>)>,
    block: B,
) -> eyre::Result<Vec<EIP1186AccountProofResponse>> {
    client
        .request("eth_getMultiProof", rpc_params![targets, block])
        .await
        .map_err(|error| eyre::eyre!("eth_getMultiProof failed: {error}"))
}

async fn execution_witness<B: Serialize>(
    client: &HttpClient,
    block: B,
) -> eyre::Result<ExecutionWitness> {
    client
        .request("debug_executionWitness", rpc_params![block])
        .await
        .map_err(|error| eyre::eyre!("debug_executionWitness failed: {error}"))
}

async fn execution_witness_with_mode<B: Serialize>(
    client: &HttpClient,
    block: B,
    mode: &str,
) -> eyre::Result<ExecutionWitness> {
    client
        .request("debug_executionWitness", rpc_params![block, mode])
        .await
        .map_err(|error| eyre::eyre!("debug_executionWitness({mode}) failed: {error}"))
}

async fn execution_witness_by_hash(
    client: &HttpClient,
    hash: B256,
) -> eyre::Result<ExecutionWitness> {
    client
        .request("debug_executionWitnessByBlockHash", rpc_params![hash])
        .await
        .map_err(|error| eyre::eyre!("debug_executionWitnessByBlockHash failed: {error}"))
}

async fn execution_witness_by_hash_with_mode(
    client: &HttpClient,
    hash: B256,
    mode: &str,
) -> eyre::Result<ExecutionWitness> {
    client
        .request("debug_executionWitnessByBlockHash", rpc_params![hash, mode])
        .await
        .map_err(|error| eyre::eyre!("debug_executionWitnessByBlockHash({mode}) failed: {error}"))
}

async fn execution_witness_error<B: Serialize>(
    client: &HttpClient,
    block: B,
) -> eyre::Result<String> {
    match client
        .request::<ExecutionWitness, _>("debug_executionWitness", rpc_params![block])
        .await
    {
        Ok(witness) => Err(eyre::eyre!(
            "expected debug_executionWitness to fail, got {} state nodes",
            witness.state.len()
        )),
        Err(error) => Ok(error.to_string()),
    }
}

/// Sorts every witness field so two witnesses can be compared by content.
///
/// A legacy witness carries trie nodes and key preimages in map-iteration order, which is not
/// stable between calls, so an equality check on the raw response would be flaky.
fn witness_contents(
    witness: &ExecutionWitness,
) -> (Vec<Bytes>, Vec<Bytes>, Vec<Bytes>, Vec<Bytes>) {
    let mut state = witness.state.clone();
    let mut codes = witness.codes.clone();
    let mut keys = witness.keys.clone();
    state.sort_unstable();
    codes.sort_unstable();
    keys.sort_unstable();
    // Headers stay in the order the provider returned them: that order is part of the contract.
    (state, codes, keys, witness.headers.clone())
}

fn verify_against_state_root(
    proof: &EIP1186AccountProofResponse,
    state_root: B256,
) -> eyre::Result<()> {
    AccountProof::from_eip1186_proof(proof.clone())
        .verify(state_root)
        .map_err(|error| {
            eyre::eyre!(
                "proof for {} does not verify against state root {state_root}: {error}",
                proof.address
            )
        })
}

async fn get_verified_proof<B: Serialize + Clone>(
    client: &HttpClient,
    address: Address,
    slots: Vec<B256>,
    block: B,
) -> eyre::Result<EIP1186AccountProofResponse> {
    let header_block = get_block(client, block.clone()).await?;
    let proof = get_proof(client, address, slots, block).await?;
    verify_against_state_root(&proof, header_block.header.state_root)?;
    Ok(proof)
}

async fn proof_error<B: Serialize>(
    client: &HttpClient,
    address: Address,
    block: B,
) -> eyre::Result<String> {
    match client
        .request::<EIP1186AccountProofResponse, _>(
            "eth_getProof",
            rpc_params![address, Vec::<B256>::new(), block],
        )
        .await
    {
        Ok(proof) => Err(eyre::eyre!(
            "expected eth_getProof to fail, got proof for {}",
            proof.address
        )),
        Err(error) => Ok(error.to_string()),
    }
}

async fn multi_proof_error<B: Serialize>(
    client: &HttpClient,
    targets: Vec<(Address, Vec<B256>)>,
    block: B,
) -> eyre::Result<String> {
    match client
        .request::<Vec<EIP1186AccountProofResponse>, _>(
            "eth_getMultiProof",
            rpc_params![targets, block],
        )
        .await
    {
        Ok(proofs) => Err(eyre::eyre!(
            "expected eth_getMultiProof to fail, got {} proofs",
            proofs.len()
        )),
        Err(error) => Ok(error.to_string()),
    }
}

fn assert_outside_window(error: &str, requested: u64) {
    assert!(
        error.contains("outside the historical proof window")
            && error.contains(&requested.to_string()),
        "expected window error for block {requested}, got: {error}"
    );
}

fn sender_nonce(node: &MorphTestNode) -> eyre::Result<u64> {
    node.inner
        .provider
        .basic_account(&ACCOUNT0)?
        .map(|account| account.nonce)
        .ok_or_else(|| eyre::eyre!("missing genesis sender {ACCOUNT0}"))
}

async fn include_transfer(
    node: &mut MorphTestNode,
    to: Address,
    value: u64,
) -> eyre::Result<alloy_primitives::BlockHash> {
    let nonce = sender_nonce(node)?;
    include_tx(node, signed_transfer(0, nonce, to, value)?).await
}

async fn include_call(
    node: &mut MorphTestNode,
    to: Address,
    data: Bytes,
) -> eyre::Result<alloy_primitives::BlockHash> {
    let nonce = sender_nonce(node)?;
    include_tx(node, signed_call(0, nonce, to, data)?).await
}

async fn include_tx(
    node: &mut MorphTestNode,
    raw_tx: Bytes,
) -> eyre::Result<alloy_primitives::BlockHash> {
    use alloy_consensus::TxReceipt;
    use alloy_consensus::transaction::TxHashRef;

    let tx_hash = node.rpc.inject_tx(raw_tx).await?;
    // The payload builder can emit an empty block if it races the pool insert.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !node.inner.pool.contains(&tx_hash) {
        eyre::ensure!(
            tokio::time::Instant::now() < deadline,
            "injected transaction {tx_hash} never became visible in the pool"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let payload = node.advance_block().await?;
    eyre::ensure!(
        payload.block().body().transactions.len() == 1,
        "expected the injected transaction to be the sole tx in the block"
    );
    let tx_hash = *payload
        .block()
        .body()
        .transactions
        .first()
        .expect("checked non-empty")
        .tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .ok_or_else(|| eyre::eyre!("missing receipt for included tx {tx_hash}"))?;
    eyre::ensure!(receipt.status(), "included transaction reverted: {tx_hash}");
    Ok(payload.block().hash())
}

/// Reads one requested slot out of a proof response.
///
/// Deliberately panics on a missing entry: defaulting to zero would let a response
/// that carries no storage proof at all pass an `== U256::ZERO` assertion.
fn storage_entry(proof: &EIP1186AccountProofResponse, slot: B256) -> U256 {
    let entry = proof
        .storage_proof
        .iter()
        .find(|entry| entry.key.as_b256() == slot)
        .unwrap_or_else(|| {
            panic!(
                "no storage proof entry for slot {slot} on {} (got {} entries)",
                proof.address,
                proof.storage_proof.len()
            )
        });
    entry.value
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("time went backwards")
        .as_secs()
}

async fn reopen_proofs(
    path: &std::path::Path,
    identity: ProofDbIdentity,
) -> eyre::Result<Arc<MdbxProofsStorage>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut last_error = None;
    while tokio::time::Instant::now() < deadline {
        match MdbxProofsStorage::open(path, identity) {
            Ok(storage) => return Ok(Arc::new(storage)),
            Err(error) => {
                last_error = Some(error.to_string());
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    Err(eyre::eyre!(
        "failed to reopen proofs db: {}",
        last_error.unwrap_or_else(|| "no attempt".to_string())
    ))
}

// -----------------------------------------------------------------------------
// 1 + 2. Multi-block historical queries and independent cryptographic verification
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_multi_block_account_and_storage() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    wait_for_proof_tip(&client, 0).await?;

    let genesis_sender = get_verified_proof(&client, ACCOUNT0, vec![], "0x0").await?;
    let genesis_recipient = get_verified_proof(&client, ACCOUNT1, vec![], "0x0").await?;
    let genesis_storage =
        get_verified_proof(&client, GENESIS_CONTRACT, vec![GENESIS_SLOT], "0x0").await?;
    assert_eq!(storage_entry(&genesis_storage, GENESIS_SLOT), U256::from(1));

    include_transfer(&mut env.node, ACCOUNT1, 1_000).await?;
    wait_for_proof_tip(&client, 1).await?;

    let nonce = sender_nonce(&env.node)?;
    let setter = Address::create(&ACCOUNT0, nonce);
    include_tx(
        &mut env.node,
        make_deploy_tx(CHAIN_ID, signer(0), nonce, SETTER_INIT)?,
    )
    .await?;
    wait_for_proof_tip(&client, 2).await?;

    include_call(&mut env.node, setter, slot_calldata(11)).await?;
    wait_for_proof_tip(&client, 3).await?;

    include_call(&mut env.node, setter, slot_calldata(22)).await?;
    wait_for_proof_tip(&client, 4).await?;

    let at_1_sender = get_verified_proof(&client, ACCOUNT0, vec![], "0x1").await?;
    let at_1_recipient = get_verified_proof(&client, ACCOUNT1, vec![], "0x1").await?;
    assert_eq!(at_1_sender.nonce, 1);
    assert!(at_1_sender.balance < genesis_sender.balance);
    assert_eq!(
        at_1_recipient.balance,
        genesis_recipient.balance + U256::from(1_000)
    );

    let at_2_setter = get_verified_proof(&client, setter, vec![B256::ZERO], "0x2").await?;
    assert_ne!(
        at_2_setter.code_hash,
        alloy_primitives::KECCAK256_EMPTY,
        "setter must be deployed at block 2"
    );
    assert_eq!(storage_entry(&at_2_setter, B256::ZERO), U256::ZERO);

    let at_3_setter = get_verified_proof(&client, setter, vec![B256::ZERO], "0x3").await?;
    let at_4_setter = get_verified_proof(&client, setter, vec![B256::ZERO], "0x4").await?;
    assert_eq!(storage_entry(&at_3_setter, B256::ZERO), U256::from(11));
    assert_eq!(storage_entry(&at_4_setter, B256::ZERO), U256::from(22));

    // Historical queries must not leak later state.
    let historical = get_verified_proof(&client, setter, vec![B256::ZERO], "0x3").await?;
    assert_eq!(storage_entry(&historical, B256::ZERO), U256::from(11));
    let still_genesis =
        get_verified_proof(&client, GENESIS_CONTRACT, vec![GENESIS_SLOT], "0x4").await?;
    assert_eq!(storage_entry(&still_genesis, GENESIS_SLOT), U256::from(1));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_proofs_verify_against_block_state_root() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 5_000).await?;
    wait_for_proof_tip(&client, 1).await?;

    let block_0 = get_block(&client, "0x0").await?;
    let block_1 = get_block(&client, "0x1").await?;
    assert_ne!(block_0.header.state_root, block_1.header.state_root);

    let proof = get_proof(&client, ACCOUNT0, vec![], "0x1").await?;
    verify_against_state_root(&proof, block_1.header.state_root)?;
    assert!(
        AccountProof::from_eip1186_proof(proof.clone())
            .verify(block_0.header.state_root)
            .is_err(),
        "account proof must not verify against a different block's state root"
    );

    let storage_proof = get_proof(&client, GENESIS_CONTRACT, vec![GENESIS_SLOT], "0x1").await?;
    verify_against_state_root(&storage_proof, block_1.header.state_root)?;
    assert_eq!(storage_entry(&storage_proof, GENESIS_SLOT), U256::from(1));

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_multi_proof_uses_one_historical_state() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    let recipient_at_genesis = get_verified_proof(&client, ACCOUNT1, vec![], "0x0").await?;
    include_transfer(&mut env.node, ACCOUNT1, 1_234).await?;
    wait_for_proof_tip(&client, 1).await?;
    include_transfer(&mut env.node, ACCOUNT2, 5_678).await?;
    wait_for_proof_tip(&client, 2).await?;

    // Query block 1 after block 2 is canonical so the request cannot be served from latest state.
    let block_1 = get_block(&client, "0x1").await?;
    let empty_slot = B256::with_last_byte(0x42);
    let expected_addresses = [ACCOUNT1, GENESIS_CONTRACT, ACCOUNT0];
    let proofs = get_multi_proof(
        &client,
        vec![
            (ACCOUNT1, vec![]),
            (GENESIS_CONTRACT, vec![GENESIS_SLOT, empty_slot]),
            (ACCOUNT0, vec![]),
        ],
        "0x1",
    )
    .await?;

    assert_eq!(proofs.len(), expected_addresses.len());
    assert_eq!(
        proofs.iter().map(|proof| proof.address).collect::<Vec<_>>(),
        expected_addresses
    );
    for proof in &proofs {
        verify_against_state_root(proof, block_1.header.state_root)?;
    }
    assert_eq!(
        proofs[0].balance,
        recipient_at_genesis.balance + U256::from(1_234)
    );
    assert_eq!(storage_entry(&proofs[1], GENESIS_SLOT), U256::from(1));
    assert_eq!(storage_entry(&proofs[1], empty_slot), U256::ZERO);
    assert_eq!(proofs[2].nonce, 1);

    // Match upstream semantics for empty batches while still validating the requested block.
    assert!(get_multi_proof(&client, vec![], "0x1").await?.is_empty());
    let outside = multi_proof_error(&client, vec![(ACCOUNT0, Vec::<B256>::new())], "0x64").await?;
    assert_outside_window(&outside, 0x64);

    Ok(())
}

/// Absent accounts must produce a valid *non-existence* proof.
///
/// This is a distinct code path: `MultiProof::account_proof` leaves `info` at `None` and falls
/// back to `EMPTY_ROOT_HASH`, and `AccountProof::verify` only accepts a missing leaf when `info`
/// is `None` **and** `storage_root` is the empty root. A response that invented either field, or
/// an implementation that returned an existing neighbour's leaf, would fail verification here.
///
/// `KECCAK_EMPTY` / `EMPTY_ROOT_HASH` are asserted because they are the values the absent account
/// *would* hash to, which is what keeps the response self-consistent: a verifier can recompute the
/// leaf that must be missing. Zero hashes would carry no such meaning, and only signal absence
/// out-of-band.
#[tokio::test(flavor = "multi_thread")]
async fn proof_history_proves_absent_accounts() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1_000).await?;
    wait_for_proof_tip(&client, 1).await?;
    include_transfer(&mut env.node, ACCOUNT2, 2_000).await?;
    wait_for_proof_tip(&client, 2).await?;

    let block_1 = get_block(&client, "0x1").await?;
    let absent_slot = B256::with_last_byte(0x07);

    let single = get_proof(&client, ABSENT, vec![absent_slot], "0x1").await?;
    verify_against_state_root(&single, block_1.header.state_root)?;
    assert_eq!(single.nonce, 0);
    assert_eq!(single.balance, U256::ZERO);
    assert_eq!(single.code_hash, KECCAK_EMPTY);
    assert_eq!(single.storage_hash, EMPTY_ROOT_HASH);
    // A slot on a missing account still gets an entry, proving zero rather than omitting it.
    assert_eq!(storage_entry(&single, absent_slot), U256::ZERO);

    // Batching an absent target next to a present one must not contaminate either response.
    let batched = get_multi_proof(
        &client,
        vec![(ACCOUNT0, vec![]), (ABSENT, vec![absent_slot])],
        "0x1",
    )
    .await?;
    assert_eq!(batched.len(), 2);
    for proof in &batched {
        verify_against_state_root(proof, block_1.header.state_root)?;
    }
    assert_eq!(batched[0].address, ACCOUNT0);
    assert_eq!(batched[0].nonce, 1);
    assert_ne!(batched[0].balance, U256::ZERO);
    assert_eq!(
        batched[1], single,
        "eth_getMultiProof must return the same non-existence proof as eth_getProof"
    );

    Ok(())
}

/// A contract target with no requested slots must still report its real storage root.
///
/// `Proof::multiproof` pre-seeds every target with an empty `StorageMultiProof` and only
/// overwrites it once the account leaf is reached, so a regression here would silently return
/// `EMPTY_ROOT_HASH` for contracts that do have storage.
#[tokio::test(flavor = "multi_thread")]
async fn proof_history_multi_proof_reports_contract_storage_root_without_slots() -> eyre::Result<()>
{
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1).await?;
    wait_for_proof_tip(&client, 1).await?;

    let block_1 = get_block(&client, "0x1").await?;
    let with_slot = get_proof(&client, GENESIS_CONTRACT, vec![GENESIS_SLOT], "0x1").await?;
    let proofs = get_multi_proof(&client, vec![(GENESIS_CONTRACT, vec![])], "0x1").await?;

    assert_eq!(proofs.len(), 1);
    verify_against_state_root(&proofs[0], block_1.header.state_root)?;
    assert!(proofs[0].storage_proof.is_empty());
    assert_ne!(
        proofs[0].storage_hash, EMPTY_ROOT_HASH,
        "genesis contract has storage, so its storage root must not be the empty root"
    );
    assert_eq!(proofs[0].storage_hash, with_slot.storage_hash);
    assert_eq!(proofs[0].code_hash, with_slot.code_hash);

    Ok(())
}

/// Repeating an address must yield one response per request entry, each scoped to its own slots.
///
/// Proof generation consolidates the duplicates into a single account target, so the expansion
/// back to request order is the part that can regress.
#[tokio::test(flavor = "multi_thread")]
async fn proof_history_multi_proof_expands_duplicate_targets() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1).await?;
    wait_for_proof_tip(&client, 1).await?;

    let block_1 = get_block(&client, "0x1").await?;
    let empty_slot = B256::with_last_byte(0x42);
    let proofs = get_multi_proof(
        &client,
        vec![
            (GENESIS_CONTRACT, vec![GENESIS_SLOT]),
            (GENESIS_CONTRACT, vec![empty_slot]),
            (GENESIS_CONTRACT, vec![]),
        ],
        "0x1",
    )
    .await?;

    assert_eq!(proofs.len(), 3);
    for proof in &proofs {
        assert_eq!(proof.address, GENESIS_CONTRACT);
        verify_against_state_root(proof, block_1.header.state_root)?;
    }
    // Each entry carries exactly the slots it asked for, not the consolidated union.
    assert_eq!(proofs[0].storage_proof.len(), 1);
    assert_eq!(storage_entry(&proofs[0], GENESIS_SLOT), U256::from(1));
    assert_eq!(proofs[1].storage_proof.len(), 1);
    assert_eq!(storage_entry(&proofs[1], empty_slot), U256::ZERO);
    assert!(proofs[2].storage_proof.is_empty());

    Ok(())
}

/// The configured account-target limit and the fixed storage-key limit are both enforced at RPC.
///
/// Launching with a deliberately tiny target limit also proves the CLI value reaches the handler
/// instead of the compiled-in default.
#[tokio::test(flavor = "multi_thread")]
async fn proof_history_multi_proof_enforces_configured_limits() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts {
        max_multi_proof_targets: 2,
        ..LaunchOpts::default()
    })
    .await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1).await?;
    wait_for_proof_tip(&client, 1).await?;

    // At the configured limit the request still succeeds.
    let at_limit =
        get_multi_proof(&client, vec![(ACCOUNT0, vec![]), (ACCOUNT1, vec![])], "0x1").await?;
    assert_eq!(at_limit.len(), 2);

    let too_many_targets = multi_proof_error(
        &client,
        vec![(ACCOUNT0, vec![]), (ACCOUNT1, vec![]), (ACCOUNT2, vec![])],
        "0x1",
    )
    .await?;
    assert!(
        too_many_targets.contains("too many proof targets") && too_many_targets.contains("max 2"),
        "expected configured target-limit error, got: {too_many_targets}"
    );

    // The storage-key cap is fixed at 1024 and is independent of the target limit.
    let too_many_keys = multi_proof_error(
        &client,
        vec![(GENESIS_CONTRACT, vec![B256::ZERO; 1025])],
        "0x1",
    )
    .await?;
    assert!(
        too_many_keys.contains("too many storage keys") && too_many_keys.contains("got 1025"),
        "expected storage-key-limit error, got: {too_many_keys}"
    );

    // An empty batch is not short-circuited: it still range-checks the requested block.
    let empty_outside = multi_proof_error(&client, vec![], "0x64").await?;
    assert_outside_window(&empty_outside, 0x64);

    Ok(())
}

// -----------------------------------------------------------------------------
// 3. earliest / latest / future / missing-block bounds
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_rpc_block_bounds() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1).await?;
    include_transfer(&mut env.node, ACCOUNT2, 2).await?;
    wait_for_proof_tip(&client, 2).await?;

    let earliest = get_verified_proof(&client, ACCOUNT0, vec![], "earliest").await?;
    let latest = get_verified_proof(&client, ACCOUNT0, vec![], "latest").await?;
    assert_eq!(earliest.nonce, 0);
    assert_eq!(latest.nonce, 2);

    let by_number = get_verified_proof(&client, ACCOUNT0, vec![], "0x2").await?;
    assert_eq!(by_number.balance, latest.balance);

    // A canonical hash must resolve to the same proof as its height.
    let head_hash = get_block(&client, "0x2").await?.header.hash;
    let by_hash = get_proof(&client, ACCOUNT0, vec![], head_hash).await?;
    assert_eq!(by_hash, by_number);

    // Numeric block ids resolve without an existence check, so a future height is
    // rejected by the window bounds rather than by header lookup.
    let future = proof_error(&client, ACCOUNT0, "0x64").await?;
    assert_outside_window(&future, 0x64);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_returns_every_requested_storage_slot() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 7).await?;
    wait_for_proof_tip(&client, 1).await?;

    // Three populated genesis slots plus one that was never written.
    let populated = [
        (GENESIS_SLOT, U256::from(1u64)),
        (B256::with_last_byte(0x07), U256::from(0x35ba5d7b55u64)),
        (B256::with_last_byte(0x09), U256::from(1u64)),
    ];
    let empty = B256::with_last_byte(0x42);
    let slots = populated
        .iter()
        .map(|(slot, _)| *slot)
        .chain(std::iter::once(empty))
        .collect::<Vec<_>>();

    let proof = get_verified_proof(&client, GENESIS_CONTRACT, slots.clone(), "0x1").await?;
    assert_eq!(
        proof.storage_proof.len(),
        slots.len(),
        "every requested slot must come back with its own proof"
    );
    for (slot, expected) in populated {
        assert_eq!(storage_entry(&proof, slot), expected, "slot {slot}");
    }
    // An unset slot must still be proven, as a zero value rather than a missing entry.
    assert_eq!(storage_entry(&proof, empty), U256::ZERO);
    assert!(
        !proof
            .storage_proof
            .iter()
            .find(|entry| entry.key.as_b256() == empty)
            .expect("entry for the unset slot")
            .proof
            .is_empty(),
        "an unset slot needs a non-empty exclusion proof"
    );

    Ok(())
}

// -----------------------------------------------------------------------------
// 4. Prune: earliest advances, in-window succeeds, out-of-window fails
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_prune_updates_window_bounds() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts {
        window: 2,
        prune_interval: Duration::from_millis(100),
        ..LaunchOpts::default()
    })
    .await?;
    let client = rpc_client(&env.node)?;

    for i in 0..5 {
        include_transfer(&mut env.node, ACCOUNT1, 100 + i).await?;
    }
    wait_for_proof_tip(&client, 5).await?;
    let status = wait_for_window(&client, 3, 5).await?;
    assert_eq!(status.earliest, Some(3));
    assert_eq!(status.latest, Some(5));

    let pruned = proof_error(&client, ACCOUNT0, "0x2").await?;
    assert_outside_window(&pruned, 2);

    let earliest_tag = proof_error(&client, ACCOUNT0, "earliest").await?;
    assert_outside_window(&earliest_tag, 0);

    let in_window = get_verified_proof(&client, ACCOUNT0, vec![], "0x3").await?;
    let latest = get_verified_proof(&client, ACCOUNT0, vec![], "latest").await?;
    assert_eq!(in_window.nonce, 3);
    assert_eq!(latest.nonce, 5);

    Ok(())
}

// -----------------------------------------------------------------------------
// 5. Unwind: delete target and later, latest becomes parent, hash is correct.
//     This pokes storage directly to pin the `unwind_history` contract in a live node.
//     Reorg-driven unwind is covered by `proof_history_reorg_replaces_old_branch`.
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_unwind_restores_parent_tip() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    let block1 = include_transfer(&mut env.node, ACCOUNT1, 10).await?;
    let block2 = include_transfer(&mut env.node, ACCOUNT2, 20).await?;
    include_transfer(&mut env.node, ACCOUNT1, 30).await?;
    wait_for_proof_tip(&client, 3).await?;

    let before = get_verified_proof(&client, ACCOUNT0, vec![], "0x1").await?;
    env.storage
        .unwind_history(BlockWithParent::new(block1, NumHash::new(2, block2)))?;

    let status = wait_for_proof_tip(&client, 1).await?;
    assert_eq!(status.earliest, Some(0));
    assert_eq!(status.latest, Some(1));
    assert_eq!(
        env.storage.get_latest_block_number()?,
        Some((1, block1)),
        "unwind must set latest to the target parent hash"
    );

    let after = get_verified_proof(&client, ACCOUNT0, vec![], "0x1").await?;
    assert_eq!(after, before);

    let unwound = proof_error(&client, ACCOUNT0, "0x2").await?;
    assert_outside_window(&unwound, 2);

    Ok(())
}

// -----------------------------------------------------------------------------
// 6. Reorg-driven revert: the ExEx must drop the old branch and serve the new one
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_reorg_replaces_old_branch() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;
    let auth = env.node.auth_server_handle().http_client();
    let now = unix_now();
    let genesis_hash = env.chain_spec.genesis_hash();

    let nonce0 = sender_nonce(&env.node)?;
    let nonce1 = nonce0 + 1;
    let tx_b1 = signed_transfer(0, nonce0, ACCOUNT1, 1_000)?;
    let tx_b2a = signed_transfer(0, nonce1, ACCOUNT1, 2_000)?;
    let tx_b2b = signed_transfer(0, nonce1, ACCOUNT2, 3_000)?;

    let mut p1 = morph_payload_types::AssembleL2BlockV2Params::new(genesis_hash, vec![tx_b1]);
    p1.timestamp = Some(now - 6);
    let block1: ExecutableL2Data = auth.request("engine_assembleL2BlockV2", (p1,)).await?;
    let block1_hash = block1.hash;
    let _: MorphHeader = auth.request("engine_newL2BlockV2", (block1,)).await?;

    let mut p2a = morph_payload_types::AssembleL2BlockV2Params::new(block1_hash, vec![tx_b2a]);
    p2a.timestamp = Some(now - 4);
    let block2a: ExecutableL2Data = auth.request("engine_assembleL2BlockV2", (p2a,)).await?;
    let block2a_hash = block2a.hash;
    let _: MorphHeader = auth.request("engine_newL2BlockV2", (block2a,)).await?;
    wait_for_proof_tip(&client, 2).await?;

    let balance_at_1 = get_verified_proof(&client, ACCOUNT1, vec![], "0x1")
        .await?
        .balance;
    let balance_at_2a = get_verified_proof(&client, ACCOUNT1, vec![], "0x2")
        .await?
        .balance;
    assert_eq!(balance_at_2a, balance_at_1 + U256::from(2_000));

    // No manual `unwind_history` here on purpose: the reorg notification alone has to
    // drive the revert. Poking storage first would let this test pass even if the ExEx
    // reorg path were broken.
    let mut p2b = morph_payload_types::AssembleL2BlockV2Params::new(block1_hash, vec![tx_b2b]);
    p2b.timestamp = Some(now - 2);
    let block2b: ExecutableL2Data = auth.request("engine_assembleL2BlockV2", (p2b,)).await?;
    assert_ne!(block2b.hash, block2a_hash);
    let block2b_hash = block2b.hash;
    let _: MorphHeader = auth.request("engine_newL2BlockV2", (block2b,)).await?;

    let head = env
        .node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
        .expect("head after fork import");
    assert_eq!(head.hash(), block2b_hash);
    // The height stays 2 across the reorg, so only the hash proves the branch swapped.
    wait_for_proof_latest(&env.storage, 2, block2b_hash).await?;

    let account1_on_fork = get_verified_proof(&client, ACCOUNT1, vec![], "0x2").await?;
    let account2_on_fork = get_verified_proof(&client, ACCOUNT2, vec![], "0x2").await?;
    assert_eq!(
        account1_on_fork.balance, balance_at_1,
        "old-branch credit to ACCOUNT1 must not survive the fork"
    );
    assert_eq!(
        account2_on_fork.balance,
        get_verified_proof(&client, ACCOUNT2, vec![], "0x1")
            .await?
            .balance
            + U256::from(3_000)
    );

    match auth
        .request::<EIP1186AccountProofResponse, _>(
            "eth_getProof",
            rpc_params![ACCOUNT1, Vec::<B256>::new(), block2a_hash],
        )
        .await
    {
        Ok(_) => {
            return Err(eyre::eyre!(
                "old-branch block hash {block2a_hash} must not serve a proof"
            ));
        }
        Err(error) => {
            let message = error.to_string();
            assert!(
                message.to_lowercase().contains("not found")
                    || message.contains("HeaderNotFound")
                    || message.contains("not canonical"),
                "old fork hash should be rejected, got: {message}"
            );
        }
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// 7. verification-interval = 1: proofs stay correct on the forced re-execution path.
//     That the interval actually *selects* that path is pinned by the
//     `build_batch_entry_*` tests in `morph-proofs-exex`, which can observe the
//     chosen `BatchBlock` variant directly.
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_stays_correct_with_verification_enabled() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts {
        verification_interval: 1,
        ..LaunchOpts::default()
    })
    .await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 111).await?;
    include_transfer(&mut env.node, ACCOUNT2, 222).await?;
    include_transfer(&mut env.node, ACCOUNT1, 333).await?;
    wait_for_proof_tip(&client, 3).await?;

    let at_1 = get_verified_proof(&client, ACCOUNT1, vec![], "0x1").await?;
    let at_2 = get_verified_proof(&client, ACCOUNT2, vec![], "0x2").await?;
    let at_3 = get_verified_proof(&client, ACCOUNT1, vec![], "0x3").await?;
    assert_eq!(at_1.balance, at_3.balance - U256::from(333));
    assert!(at_2.balance > U256::ZERO);

    // Cached-path corruption would fail this: each height must still match that
    // block's independently fetched canonical state root.
    for block in ["0x1", "0x2", "0x3"] {
        let header = get_block(&client, block).await?;
        let proof = get_proof(&client, ACCOUNT0, vec![], block).await?;
        verify_against_state_root(&proof, header.header.state_root)?;
    }

    Ok(())
}

// -----------------------------------------------------------------------------
// 8. Restart: the proofs MDBX reopens with its window and per-block rows intact
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_db_survives_node_restart() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let genesis: Genesis = serde_json::from_str(include_str!("../assets/test-genesis.json"))?;
    let chain_spec = Arc::new(MorphChainSpec::from_genesis(genesis));
    let proofs_dir = tempfile::tempdir()?;
    let identity = ProofDbIdentity::new(chain_spec.chain().id(), chain_spec.genesis_hash());
    let proofs_path = proofs_dir.path().to_path_buf();

    // First process on an owned runtime. `Runtime::test()` inside `#[tokio::test]`
    // attaches to the test handle and would keep the ExEx (and its MDBX lock) alive.
    let first_tip = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("owned runtime");
        let result = rt.block_on(async move {
            let storage = Arc::new(MdbxProofsStorage::open(&proofs_path, identity)?);
            let mut node = launch_node(chain_spec, storage.clone(), LaunchOpts::default()).await?;
            let client = rpc_client(&node)?;
            include_transfer(&mut node, ACCOUNT1, 10).await?;
            include_transfer(&mut node, ACCOUNT2, 20).await?;
            wait_for_proof_tip(&client, 2).await?;
            let first_tip = node
                .inner
                .provider
                .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)?
                .expect("tip before restart")
                .hash();
            drop(node);
            drop(storage);
            Ok::<_, eyre::Error>(first_tip)
        });
        rt.shutdown_timeout(Duration::from_secs(3));
        result
    })
    .join()
    .map_err(|_| eyre::eyre!("first node thread panicked"))??;

    let storage = reopen_proofs(proofs_dir.path(), identity).await?;
    assert_eq!(
        storage.get_latest_block_number()?,
        Some((2, first_tip)),
        "proof MDBX must keep the pre-restart tip after reopen"
    );
    assert_eq!(
        storage.get_earliest_block_number()?.map(|(n, _)| n),
        Some(0)
    );

    // Pointers alone would survive even if the per-block rows were lost, so read the
    // change sets back. The earliest block is the baseline snapshot rather than a diff,
    // so change sets start above it.
    assert!(
        storage.fetch_trie_updates(0).is_err(),
        "the initialization anchor is a baseline snapshot, not a change set"
    );
    for block in 1..=2 {
        storage
            .fetch_trie_updates(block)
            .unwrap_or_else(|error| panic!("block {block} trie updates must persist: {error}"));
    }
    assert!(
        storage.fetch_trie_updates(3).is_err(),
        "no history may exist past the reopened tip"
    );

    // Not covered here: continuing to append after the restart. That needs a chain DB
    // that outlives the node, and this harness drops its reth TempDatabase on node
    // drop. The ExEx-side restart guards are covered by `ensure_initialized*` tests in
    // `morph-proofs-exex`.
    Ok(())
}

// -----------------------------------------------------------------------------
// 9. `debug_executionWitness`: served from proof history, bounded by its window
// -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_serves_execution_witness() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let mut env = setup(LaunchOpts::default()).await?;
    let client = rpc_client(&env.node)?;

    include_transfer(&mut env.node, ACCOUNT1, 1_000).await?;

    let nonce = sender_nonce(&env.node)?;
    let setter = Address::create(&ACCOUNT0, nonce);
    include_tx(
        &mut env.node,
        make_deploy_tx(CHAIN_ID, signer(0), nonce, SETTER_INIT)?,
    )
    .await?;

    let call_block = include_call(&mut env.node, setter, slot_calldata(11)).await?;
    wait_for_proof_tip(&client, 3).await?;

    // Block 3 stores into the setter, so its witness must carry the trie nodes proving the
    // pre-state it wrote over, the contract's bytecode, and the parent header.
    let witness = execution_witness(&client, "0x3").await?;
    assert!(
        !witness.state.is_empty(),
        "witness must carry the trie nodes touched by the block"
    );
    assert!(
        !witness.codes.is_empty(),
        "witness must carry the bytecode executed by the block"
    );
    assert!(
        !witness.keys.is_empty(),
        "witness must carry the preimages of the hashed keys it touched"
    );
    assert!(
        !witness.headers.is_empty(),
        "witness must carry at least the parent header"
    );

    // Addressing the same block by hash must be the same request, not a second code path
    // left on reth's replay-based default.
    let by_hash = execution_witness_by_hash(&client, call_block).await?;
    assert_eq!(
        witness_contents(&by_hash),
        witness_contents(&witness),
        "executionWitnessByBlockHash must agree with executionWitness"
    );

    let legacy = execution_witness_with_mode(&client, "0x3", "legacy").await?;
    assert_eq!(
        witness_contents(&legacy),
        witness_contents(&witness),
        "explicit legacy mode must match the default"
    );

    let canonical = execution_witness_with_mode(&client, "0x3", "canonical").await?;
    assert!(
        !canonical.state.is_empty(),
        "canonical mode must produce a witness"
    );
    assert!(
        canonical.state.windows(2).all(|pair| pair[0] <= pair[1]),
        "canonical mode must return state nodes in canonical order"
    );
    assert!(
        canonical.codes.windows(2).all(|pair| pair[0] <= pair[1]),
        "canonical mode must return bytecodes in canonical order"
    );

    let canonical_by_hash =
        execution_witness_by_hash_with_mode(&client, call_block, "canonical").await?;
    assert_eq!(
        witness_contents(&canonical_by_hash),
        witness_contents(&canonical),
        "both witness entry points must honor canonical mode"
    );

    // Genesis has no parent state; clamping would silently witness the wrong state.
    let genesis = execution_witness_error(&client, "0x0").await?;
    assert!(
        genesis.contains("no parent state"),
        "expected genesis rejection, got: {genesis}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn proof_history_execution_witness_is_bounded_by_the_proof_window() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    // The point of this test: reth's default `debug_executionWitness` can serve any block the
    // chain DB still holds, so a pruned-out block failing here is what proves the RPC is bound
    // to proof history rather than to the historical-overlay slow path.
    let mut env = setup(LaunchOpts {
        window: 2,
        prune_interval: Duration::from_millis(100),
        ..LaunchOpts::default()
    })
    .await?;
    let client = rpc_client(&env.node)?;

    let mut block_hashes = Vec::new();
    for i in 0..6 {
        block_hashes.push(include_transfer(&mut env.node, ACCOUNT1, 100 + i).await?);
    }
    wait_for_proof_tip(&client, 6).await?;
    wait_for_window(&client, 4, 6).await?;

    // Leave block 6 canonical while moving only the proof tip back to block 5. This models the
    // normal one-block indexing lag and pins the upper bound: block 6 needs only block 5's state.
    env.storage.unwind_history(BlockWithParent::new(
        block_hashes[4],
        NumHash::new(6, block_hashes[5]),
    ))?;
    wait_for_window(&client, 4, 5).await?;

    // A witness needs the parent state, so block 4 fails because state 3 was pruned.
    let pruned = execution_witness_error(&client, "0x4").await?;
    assert_outside_window(&pruned, 3);

    let at_5 = execution_witness(&client, "0x5").await?;
    assert!(
        !at_5.state.is_empty(),
        "block 5 must be servable: its parent 4 is the earliest retained block"
    );
    let at_6 = execution_witness(&client, "0x6").await?;
    assert!(
        !at_6.state.is_empty(),
        "block 6 must be servable while the proof tip is 5 because its parent state is retained"
    );

    // A much later block does not exist.
    let future = execution_witness_error(&client, "0x64").await?;
    assert!(
        future.contains("not found") || future.contains("outside the historical proof window"),
        "expected a not-found or window error for a future block, got: {future}"
    );

    Ok(())
}
