//! Source sweep integration tests.

use alloy_consensus::{BlockHeader, TxReceipt, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Bytes, U256, address, b256, logs_bloom};
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{
    HardforkSchedule, L1MessageBuilder, MorphTestNode, MorphTxBuilder, SLOT1_ERC20_RUNTIME_CODE,
    TEST_TOKEN_ADDRESS, TestNodeBuilder,
};
use morph_payload_types::{
    AssembleL2BlockV2Params, ExecutableL2Data, MorphBuiltPayload, MorphPayloadTypes,
};
use morph_primitives::receipt::calculate_receipt_root_no_memo;
use reth_node_api::PayloadTypes;
use reth_payload_primitives::BuiltPayload;
use reth_provider::{HeaderProvider, ReceiptProvider, StateProviderFactory};

use super::helpers::advance_block_with_l1_messages;

const REGISTRY: Address = address!("0fF2Ea62eBca29E70aE2b0551a54eFFa4ea7DeEa");
const SOURCE: Address = address!("1000000000000000000000000000000000000001");
const SOURCE_TWO: Address = address!("1000000000000000000000000000000000000002");
const DESTINATION: Address = address!("2000000000000000000000000000000000000002");
const RECIPIENT: Address = address!("3000000000000000000000000000000000000003");
const ROUTER: Address = address!("4000000000000000000000000000000000000004");
const CANDIDATE_EMITTER: Address = address!("5000000000000000000000000000000000000005");
const SENDER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const AMOUNT: u64 = 123;
const PROD_REGISTRY: Address = address!("0fF2Ea62eBca29E70aE2b0551a54eFFa4ea7DeEa");
const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const REQUEST_TOPIC: B256 =
    b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");
const SWEEP_TOPIC: B256 = b256!("035b37215a69e14a80883933d6aa84f0919a67af9410a4a73e8a23baeca011f0");
const SWEEP_FAILED_TOPIC: B256 =
    b256!("0f64fa58e4261d8832b5ea6c262c691ef36e73cb21998c4fb01a83997940797c");

/// Runtime produced by solc 0.8.30 (optimizer runs=200, metadata disabled) from
/// `contracts/contracts/test/MockSweepRegistryEL.sol` in the morph repository.
///
/// It is a deterministic test double, not production Registry bytecode. The EL
/// resolves candidates through the production `resolveSweep(address,address)` ABI.
/// Regenerate with:
///   jq -r '.deployedBytecode.object' \
///     forge-artifacts/MockSweepRegistryEL.sol/MockSweepRegistryEL.json
const TEST_REGISTRY_RUNTIME: &str = "0x608060405234801561000f575f80fd5b5060043610610064575f3560e01c80639faa2f2f1161004d5780639faa2f2f146100b4578063b750bdde146100ec578063ba1b6c8414610178575f80fd5b8063663a375c14610068578063753d75631461007d575b5f80fd5b61007b610076366004610366565b610230565b005b61009f61008b366004610397565b60fe6020525f908152604090205460ff1681565b60405190151581526020015b60405180910390f35b6100c76100c2366004610366565b61028e565b60405173ffffffffffffffffffffffffffffffffffffffff90911681526020016100ab565b6101466100fa366004610397565b60fd6020525f90815260409020805460019091015473ffffffffffffffffffffffffffffffffffffffff82169174010000000000000000000000000000000000000000900460ff169083565b6040805173ffffffffffffffffffffffffffffffffffffffff90941684529115156020840152908201526060016100ab565b61007b6101863660046103b7565b73ffffffffffffffffffffffffffffffffffffffff9283165f90815260fe60209081526040808320805460017fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff00909116179055938516825260fd90529190912080547fffffffffffffffffffffff00000000000000000000000000000000000000000016919092169081179015157401000000000000000000000000000000000000000002179055565b8073ffffffffffffffffffffffffffffffffffffffff168273ffffffffffffffffffffffffffffffffffffffff167f24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff60405160405180910390a35050565b73ffffffffffffffffffffffffffffffffffffffff8082165f90815260fd60209081526040808320938616835260fe90915281205490919060ff1680156102ee5750805474010000000000000000000000000000000000000000900460ff165b80156103105750805473ffffffffffffffffffffffffffffffffffffffff1615155b15610333575473ffffffffffffffffffffffffffffffffffffffff169050610338565b5f9150505b92915050565b803573ffffffffffffffffffffffffffffffffffffffff81168114610361575f80fd5b919050565b5f8060408385031215610377575f80fd5b6103808361033e565b915061038e6020840161033e565b90509250929050565b5f602082840312156103a7575f80fd5b6103b08261033e565b9392505050565b5f805f606084860312156103c9575f80fd5b6103d28461033e565b92506103e06020850161033e565b91506103ee6040850161033e565b9050925092509256fea164736f6c6343000818000a";

/// Runtime for `SweepFixtures.sol::TestSweepRouter`.
const TEST_ROUTER_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806341f47c231461003857806393d1fef41461004d575b5f5ffd5b61004b610046366004610264565b610060565b005b61004b61005b3660046102ac565b610156565b604051632e86db2160e21b81526001600160a01b038086166004830152808516602483015283166044820152730ff2ea62ebca29e70ae2b0551a54effa4ea7deea9063ba1b6c84906064015f604051808303815f87803b1580156100c2575f5ffd5b505af11580156100d4573d5f5f3e3d5ffd5b505060405163a9059cbb60e01b81526001600160a01b038681166004830152602482018590528716925063a9059cbb91506044016020604051808303815f875af1158015610124573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061014891906102e6565b610150575f5ffd5b50505050565b60405163a9059cbb60e01b81526001600160a01b0383811660048301526024820183905284169063a9059cbb906044016020604051808303815f875af11580156101a2573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906101c691906102e6565b6101ce575f5ffd5b604051632e86db2160e21b81526001600160a01b038085166004830152831660248201525f6044820152730ff2ea62ebca29e70ae2b0551a54effa4ea7deea9063ba1b6c84906064015f604051808303815f87803b15801561022e575f5ffd5b505af1158015610240573d5f5f3e3d5ffd5b50505050505050565b80356001600160a01b038116811461025f575f5ffd5b919050565b5f5f5f5f60808587031215610277575f5ffd5b61028085610249565b935061028e60208601610249565b925061029c60408601610249565b9396929550929360600135925050565b5f5f5f606084860312156102be575f5ffd5b6102c784610249565b92506102d560208501610249565b929592945050506040919091013590565b5f602082840312156102f6575f5ffd5b81518015158114610305575f5ffd5b939250505056";

/// Runtime for `SweepFixtures.sol::TestCandidateEmitter`.
const TEST_CANDIDATE_EMITTER_RUNTIME: &str = "0x6080604052348015600e575f5ffd5b5060015b6010816001600160a01b031611607057604051600181526001600160a01b0382169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a3606a816072565b90506012565b005b5f6001600160a01b0382166002600160a01b03198101609f57634e487b7160e01b5f52601160045260245ffd5b6001019291505056";

/// Exact production-tuple runtime of `SweepRegistry`.
///
/// The constructor-set owner is restored in genesis storage. Source, compiler,
/// immutable, file, and runtime hashes are enforced by
/// `verify_sweep_fixtures.py` against the sibling `morph` checkout.
const PROD_REGISTRY_RUNTIME: &str = include_str!("../assets/SweepRegistry.deployed.hex");

fn token_balance_slot(account: Address) -> B256 {
    let mut preimage = [0_u8; 64];
    preimage[12..32].copy_from_slice(account.as_slice());
    preimage[63] = 1;
    alloy_primitives::keccak256(preimage)
}

fn transfer_calldata(to: Address, amount: U256) -> Bytes {
    let mut data = Vec::with_capacity(68);
    data.extend_from_slice(&[0xa9, 0x05, 0x9c, 0xbb]);
    data.extend_from_slice(&[0; 12]);
    data.extend_from_slice(to.as_slice());
    data.extend_from_slice(&amount.to_be_bytes::<32>());
    data.into()
}

fn address_topic(address: Address) -> B256 {
    B256::left_padding_from(address.as_slice())
}

fn encode_address_args(selector: [u8; 4], addresses: &[Address]) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * addresses.len());
    data.extend_from_slice(&selector);
    for address in addresses {
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(address.as_slice());
    }
    data.into()
}

fn encode_address_and_uint_args(
    selector: [u8; 4],
    addresses: &[Address],
    values: &[U256],
) -> Bytes {
    let mut data = Vec::with_capacity(4 + 32 * (addresses.len() + values.len()));
    data.extend_from_slice(&selector);
    for address in addresses {
        data.extend_from_slice(&[0; 12]);
        data.extend_from_slice(address.as_slice());
    }
    for value in values {
        data.extend_from_slice(&value.to_be_bytes::<32>());
    }
    data.into()
}

fn stateful_builder() -> TestNodeBuilder {
    TestNodeBuilder::new()
        .with_account_code(TEST_TOKEN_ADDRESS, SLOT1_ERC20_RUNTIME_CODE)
        .with_account_code(REGISTRY, TEST_REGISTRY_RUNTIME)
}

async fn import_payload(node: &MorphTestNode, payload: &MorphBuiltPayload) -> eyre::Result<()> {
    let hash = payload.block().hash();
    let execution_data = MorphPayloadTypes::block_to_payload(payload.block().clone(), None);
    let status = node
        .inner
        .add_ons_handle
        .beacon_engine_handle
        .new_payload(execution_data)
        .await?;
    assert!(
        status.is_valid(),
        "Engine API rejected replayed payload: {status:?}"
    );
    node.update_forkchoice(hash, hash).await?;
    node.sync_to(hash).await?;
    Ok(())
}

async fn transfer_to_source(schedule: HardforkSchedule) -> eyre::Result<(U256, U256, usize)> {
    let (mut nodes, wallet) = stateful_builder().with_schedule(schedule).build().await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let transaction = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(transaction).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload.block().body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("canonical receipt");
    assert!(receipt.status());

    let state = node.inner.provider.latest()?;
    let source = state
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
        .unwrap_or_default();
    let destination = state
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
        .unwrap_or_default();
    Ok((source, destination, receipt.logs().len()))
}

#[tokio::test(flavor = "multi_thread")]
async fn onyx_activation_gates_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (source, destination, log_count) = transfer_to_source(HardforkSchedule::PreOnyx).await?;
    assert_eq!(source, U256::from(AMOUNT));
    assert_eq!(destination, U256::ZERO);
    assert_eq!(
        log_count, 1,
        "pre-Onyx receipt contains only inflow Transfer"
    );

    let (source, destination, log_count) = transfer_to_source(HardforkSchedule::AllActive).await?;
    assert_eq!(source, U256::ZERO);
    assert_eq!(destination, U256::from(AMOUNT));
    assert_eq!(
        log_count, 3,
        "Onyx receipt contains inflow, sweep, and settlement logs"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn l1_message_token_inflow_triggers_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let message = L1MessageBuilder::new(0)
        .with_sender(SENDER)
        .with_target(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .with_gas_limit(100_000)
        .build_encoded();
    let payload = advance_block_with_l1_messages(&mut node, vec![message]).await?;
    let tx_hash = *payload.block().body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("L1 message receipt");

    assert!(receipt.status());
    assert_eq!(receipt.logs().len(), 3);
    assert_eq!(receipt.logs()[2].topics()[0], SWEEP_TOPIC);
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn poke_sweeps_historical_balance_and_attaches_all_logs() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();

    let historical_inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(historical_inflow).await?;
    let inflow_payload = node.advance_block().await?;
    let inflow_hash = *inflow_payload.block().body().transactions[0].tx_hash();
    let inflow_receipt = node
        .inner
        .provider
        .receipt_by_hash(inflow_hash)?
        .expect("historical inflow receipt");
    assert_eq!(inflow_receipt.logs().len(), 1);

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let poke = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 2)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0x66, 0x3a, 0x37, 0x5c],
            &[TEST_TOKEN_ADDRESS, SOURCE],
        ))
        .build_signed()?;
    node.rpc.inject_tx(poke).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload.block().body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("poke receipt");
    let logs = receipt.logs();

    assert!(receipt.status());
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0].address, REGISTRY);
    assert_eq!(
        logs[0].topics(),
        &[
            REQUEST_TOPIC,
            address_topic(TEST_TOKEN_ADDRESS),
            address_topic(SOURCE)
        ]
    );
    assert!(logs[0].data.data.is_empty());
    assert_eq!(
        logs[1].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(SOURCE),
            address_topic(DESTINATION)
        ]
    );
    assert_eq!(U256::from_be_slice(&logs[1].data.data), U256::from(AMOUNT));
    assert_eq!(logs[2].topics()[0], SWEEP_TOPIC);
    assert_eq!(
        u32::from_be_bytes(logs[2].data.data[60..64].try_into().unwrap()),
        1
    );

    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn post_main_registry_state_controls_same_transaction_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder()
        .with_account_code(ROUTER, TEST_ROUTER_RUNTIME)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let fund_router = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(ROUTER, U256::from(2 * AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(fund_router).await?;
    node.advance_block().await?;

    let enable_then_inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(ROUTER)
        .with_data(encode_address_and_uint_args(
            [0x41, 0xf4, 0x7c, 0x23],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
            &[U256::from(AMOUNT)],
        ))
        .with_gas_limit(200_000)
        .build_signed()?;
    node.rpc.inject_tx(enable_then_inflow).await?;
    let enabled_payload = node.advance_block().await?;
    let enabled_hash = *enabled_payload.block().body().transactions[0].tx_hash();
    let enabled_receipt = node
        .inner
        .provider
        .receipt_by_hash(enabled_hash)?
        .expect("enable-then-inflow receipt");
    assert!(enabled_receipt.status());
    assert_eq!(enabled_receipt.logs().len(), 3);
    assert_eq!(enabled_receipt.logs()[2].topics()[0], SWEEP_TOPIC);

    let register_second = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 2)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE_TWO, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register_second).await?;
    node.advance_block().await?;

    let inflow_then_disable = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 3)
        .with_v1_eth_fee()
        .with_to(ROUTER)
        .with_data(encode_address_and_uint_args(
            [0x93, 0xd1, 0xfe, 0xf4],
            &[TEST_TOKEN_ADDRESS, SOURCE_TWO],
            &[U256::from(AMOUNT)],
        ))
        .with_gas_limit(200_000)
        .build_signed()?;
    node.rpc.inject_tx(inflow_then_disable).await?;
    let disabled_payload = node.advance_block().await?;
    let disabled_hash = *disabled_payload.block().body().transactions[0].tx_hash();
    let disabled_receipt = node
        .inner
        .provider
        .receipt_by_hash(disabled_hash)?
        .expect("inflow-then-disable receipt");
    assert!(disabled_receipt.status());
    assert_eq!(
        disabled_receipt.logs().len(),
        1,
        "final disabled Registry state suppresses settlement"
    );

    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE_TWO))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_resolver_and_code_present_source_leave_balance_unswept() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder()
        .with_account_code(SOURCE, "0x00")
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let disabled_inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE_TWO, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(disabled_inflow).await?;
    let disabled_payload = node.advance_block().await?;
    let disabled_hash = *disabled_payload.block().body().transactions[0].tx_hash();
    let disabled_receipt = node
        .inner
        .provider
        .receipt_by_hash(disabled_hash)?
        .expect("zero-resolver receipt");
    assert!(disabled_receipt.status());
    assert_eq!(
        disabled_receipt.logs().len(),
        1,
        "zero resolver must not add settlement logs"
    );

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 2)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(inflow).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload.block().body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("code-present receipt");

    assert!(receipt.status());
    // The Transfer, plus the protocol failure record: the Registry resolved a
    // destination, so the skip is attributable and reported on-chain.
    assert_eq!(receipt.logs().len(), 2);
    let failure = &receipt.logs()[1];
    assert_eq!(failure.address, REGISTRY);
    assert_eq!(
        failure.topics(),
        [
            SWEEP_FAILED_TOPIC,
            address_topic(TEST_TOKEN_ADDRESS),
            address_topic(SOURCE),
            address_topic(DESTINATION),
        ]
    );
    assert_eq!(
        &failure.data.data[..],
        &alloy_primitives::keccak256("source_has_code")[..],
        "failure reason must be the hashed metrics label"
    );
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE_TWO))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::ZERO
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn builder_and_engine_replay_match_and_trace_preserves_canonical_sweep_state()
-> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut source_nodes, wallet) = stateful_builder().build().await?;
    let mut source = source_nodes.pop().unwrap();
    let (mut replay_nodes, _) = stateful_builder().build().await?;
    let replay = replay_nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    source.rpc.inject_tx(register).await?;
    let setup_payload = source.advance_block().await?;
    import_payload(&replay, &setup_payload).await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    source.rpc.inject_tx(inflow).await?;
    let built = source.advance_block().await?;
    import_payload(&replay, &built).await?;

    let source_header = built.block().header();
    assert_ne!(
        source_header.state_root(),
        setup_payload.block().header().state_root(),
        "sweep state must change the parent state root"
    );
    let replay_header = replay
        .inner
        .provider
        .header_by_number(built.block().number())?
        .expect("replayed header");
    assert_eq!(source_header.state_root(), replay_header.state_root());
    assert_eq!(source_header.receipts_root(), replay_header.receipts_root());
    assert_eq!(source_header.logs_bloom(), replay_header.logs_bloom());
    assert_eq!(source_header.gas_used(), replay_header.gas_used());

    let tx_hash = *built.block().body().transactions[0].tx_hash();
    let source_receipt = source
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("source receipt");
    let replay_receipt = replay
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("replayed receipt");
    assert_eq!(source_receipt.logs(), replay_receipt.logs());

    let balance_before_trace = replay
        .inner
        .provider
        .latest()?
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
        .unwrap_or_default();
    let rpc = replay
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let trace: serde_json::Value = rpc
        .request(
            "debug_traceTransaction",
            (
                tx_hash,
                serde_json::json!({
                    "tracer": "prestateTracer",
                    "tracerConfig": { "diffMode": true }
                }),
            ),
        )
        .await?;
    let token_key = TEST_TOKEN_ADDRESS.to_string();
    let destination_slot = token_balance_slot(DESTINATION).to_string();
    let destination_value = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert_eq!(
        trace["post"][&token_key]["storage"][&destination_slot],
        serde_json::Value::String(destination_value),
        "prestate trace post-state must include the hidden sweep credit"
    );
    let source_slot = token_balance_slot(SOURCE).to_string();
    assert!(
        trace["post"][&token_key]["storage"][&source_slot].is_null(),
        "diff tracer omits the source slot because it starts and ends at zero"
    );
    let parity_trace = replay
        .rpc
        .inner
        .trace_api()
        .trace_transaction(tx_hash)
        .await?;
    assert!(parity_trace.is_some_and(|trace| !trace.is_empty()));
    let balance_after_trace = replay
        .inner
        .provider
        .latest()?
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
        .unwrap_or_default();
    assert_eq!(balance_before_trace, U256::from(AMOUNT));
    assert_eq!(balance_after_trace, balance_before_trace);

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn reorg_removes_sweep_receipt_logs_and_state() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    let parent = node.advance_block().await?;
    let parent_hash = parent.block().hash();

    let sweep_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    let canonical_sweep_tx_hash = alloy_primitives::keccak256(sweep_tx.as_ref());
    let replacement_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(RECIPIENT, U256::from(AMOUNT)))
        .build_signed()?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let client = node.auth_server_handle().http_client();

    let mut sweep_params = AssembleL2BlockV2Params::new(parent_hash, vec![sweep_tx]);
    sweep_params.timestamp = Some(now - 4);
    let sweep_block: ExecutableL2Data = client
        .request("engine_assembleL2BlockV2", (sweep_params,))
        .await?;

    let mut replacement_params = AssembleL2BlockV2Params::new(parent_hash, vec![replacement_tx]);
    replacement_params.timestamp = Some(now - 2);
    let replacement: ExecutableL2Data = client
        .request("engine_assembleL2BlockV2", (replacement_params,))
        .await?;
    let replacement_hash = replacement.hash;

    let _: morph_primitives::MorphHeader = client
        .request("engine_newL2BlockV2", (sweep_block.clone(),))
        .await?;
    let sweep_receipt = node
        .inner
        .provider
        .receipt_by_hash(canonical_sweep_tx_hash)?
        .expect("sweep receipt");
    assert_eq!(sweep_receipt.logs().len(), 3);

    let _: morph_primitives::MorphHeader = client
        .request("engine_newL2BlockV2", (replacement,))
        .await?;
    assert_eq!(node.block_hash(2), replacement_hash);
    assert!(
        node.inner
            .provider
            .receipt_by_hash(canonical_sweep_tx_hash)?
            .is_none(),
        "reorged canonical receipt must disappear"
    );
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(RECIPIENT))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn unwhitelisted_fake_logs_do_not_suppress_the_eligible_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder()
        .with_account_code(CANDIDATE_EMITTER, TEST_CANDIDATE_EMITTER_RUNTIME)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    for nonce in 1..=4 {
        let candidate_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), nonce)
            .with_v1_eth_fee()
            .with_to(CANDIDATE_EMITTER)
            .with_gas_limit(100_000)
            .build_signed()?;
        node.rpc.inject_tx(candidate_tx).await?;
    }
    let final_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 5)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    let final_hash = node.rpc.inject_tx(final_tx).await?;
    let payload = node.advance_block().await?;
    assert_eq!(payload.block().body().transactions.len(), 5);
    for transaction in &payload.block().body().transactions[..4] {
        let receipt = node
            .inner
            .provider
            .receipt_by_hash(*transaction.tx_hash())?
            .expect("candidate emitter receipt");
        assert!(receipt.status());
        assert_eq!(
            receipt.logs().len(),
            16,
            "each leading transaction emits sixteen Transfer logs"
        );
        assert!(
            receipt
                .logs()
                .iter()
                .all(|log| log.topics()[0] == TRANSFER_TOPIC)
        );
        // The fake Transfer logs are from an unwhitelisted token, so each
        // resolves to zero. `resolver_zero` is deliberately NOT reported
        // on-chain — no SweepFailed is appended (Onyx spec §6.3).
        assert!(
            receipt
                .logs()
                .iter()
                .all(|log| log.topics()[0] != SWEEP_FAILED_TOPIC),
            "unwhitelisted fake logs must not append SweepFailed"
        );
    }

    let receipt = node
        .inner
        .provider
        .receipt_by_hash(final_hash)?
        .expect("post-budget user receipt");
    assert!(receipt.status());
    assert_eq!(
        receipt.logs().len(),
        3,
        "unwhitelisted fake logs must not suppress the eligible sweep"
    );
    assert_eq!(
        receipt.logs()[0].topics(),
        &[TRANSFER_TOPIC, address_topic(SENDER), address_topic(SOURCE)]
    );
    assert_eq!(receipt.logs()[0].address, TEST_TOKEN_ADDRESS);
    assert_eq!(
        U256::from_be_slice(&receipt.logs()[0].data.data),
        U256::from(AMOUNT)
    );
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    let rpc = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let trace_options = serde_json::json!({
        "tracer": "prestateTracer",
        "tracerConfig": { "diffMode": true }
    });
    let trace: serde_json::Value = rpc
        .request(
            "debug_traceTransaction",
            (final_hash, trace_options.clone()),
        )
        .await?;
    let token_key = TEST_TOKEN_ADDRESS.to_string();
    let source_slot = token_balance_slot(SOURCE).to_string();
    let destination_slot = token_balance_slot(DESTINATION).to_string();
    let amount_word = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert!(
        trace["post"][&token_key]["storage"][&source_slot].is_null(),
        "net-zero source storage should be omitted from the target diff"
    );
    assert_eq!(
        trace["post"][&token_key]["storage"][&destination_slot],
        serde_json::Value::String(amount_word.clone()),
        "target transaction trace must include the eligible sweep"
    );

    let block_trace: serde_json::Value = rpc
        .request(
            "debug_traceBlockByHash",
            (payload.block().hash(), trace_options),
        )
        .await?;
    assert!(
        block_trace[4]["result"]["post"][&token_key]["storage"][&source_slot].is_null(),
        "full debug replay should omit the net-zero source slot"
    );
    assert_eq!(
        block_trace[4]["result"]["post"][&token_key]["storage"][&destination_slot],
        serde_json::Value::String(amount_word),
        "full debug block replay must include the eligible sweep"
    );

    let parity_trace = node
        .rpc
        .inner
        .trace_api()
        .trace_block(payload.block().hash().into())
        .await?;
    assert!(parity_trace.is_some_and(|traces| !traces.is_empty()));
    let parity_replay: serde_json::Value = rpc
        .request(
            "trace_replayBlockTransactions",
            (payload.block().hash(), vec!["stateDiff"]),
        )
        .await?;
    assert!(
        parity_replay[4]["stateDiff"][&token_key]["storage"][&source_slot].is_null(),
        "parity replay should omit the net-zero source slot"
    );
    assert!(
        !parity_replay[4]["stateDiff"][&token_key]["storage"][&destination_slot].is_null(),
        "parity block replay must include the eligible sweep"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_trace_calls_run_sweep_with_a_fresh_transaction_meter() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();
    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let call = serde_json::json!({
        "from": SENDER,
        "to": TEST_TOKEN_ADDRESS,
        "gas": "0x30d40",
        "gasPrice": "0x4a817c800",
        "data": transfer_calldata(SOURCE, U256::from(AMOUNT)),
    });
    let rpc = node
        .rpc_client()
        .ok_or_else(|| eyre::eyre!("HTTP RPC client not available"))?;
    let debug_trace: serde_json::Value = rpc
        .request(
            "debug_traceCall",
            (
                call.clone(),
                "latest",
                serde_json::json!({
                    "tracer": "prestateTracer",
                    "tracerConfig": { "diffMode": true }
                }),
            ),
        )
        .await?;
    let token_key = TEST_TOKEN_ADDRESS.to_string();
    let source_slot = token_balance_slot(SOURCE).to_string();
    let destination_slot = token_balance_slot(DESTINATION).to_string();
    // Onyx spec §8: a standalone simulation runs the full sweep with a fresh 1M
    // transaction transfer meter and an empty seen set, so the simulated inflow is
    // swept onward exactly as it would be in a canonical block. The source slot goes
    // 0 -> AMOUNT -> 0, which is a net-zero diff the tracer omits; the destination
    // slot is where the sweep becomes visible.
    assert!(
        debug_trace["post"][&token_key]["storage"][&source_slot].is_null(),
        "debug_traceCall must sweep the synthetic inflow back out of the source"
    );
    assert!(
        !debug_trace["post"][&token_key]["storage"][&destination_slot].is_null(),
        "debug_traceCall must execute the sweep and credit the destination"
    );

    let parity_trace: serde_json::Value = rpc
        .request("trace_call", (call, vec!["stateDiff"], "latest"))
        .await?;
    assert!(
        parity_trace["stateDiff"][&token_key]["storage"][&source_slot].is_null(),
        "trace_call state diff must show the source swept back to zero"
    );
    assert!(
        !parity_trace["stateDiff"][&token_key]["storage"][&destination_slot].is_null(),
        "trace_call must execute the sweep and credit the destination"
    );

    // Simulation must not touch the chain: neither balance moved on disk.
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::ZERO
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_onyx_receipt_commits_sweep_roots_bloom_and_user_gas() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, SOURCE, DESTINATION],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(SOURCE, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(inflow).await?;
    let payload = node.advance_block().await?;
    let block = payload.block();
    let tx_hash = *block.body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("canonical receipt");
    let logs = receipt.logs();

    assert!(receipt.status());
    assert_eq!(logs.len(), 3);
    assert_eq!(logs[0].address, TEST_TOKEN_ADDRESS);
    assert_eq!(
        logs[0].topics(),
        &[TRANSFER_TOPIC, address_topic(SENDER), address_topic(SOURCE)]
    );
    assert_eq!(U256::from_be_slice(&logs[0].data.data), U256::from(AMOUNT));
    assert_eq!(logs[1].address, TEST_TOKEN_ADDRESS);
    assert_eq!(
        logs[1].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(SOURCE),
            address_topic(DESTINATION)
        ]
    );
    assert_eq!(U256::from_be_slice(&logs[1].data.data), U256::from(AMOUNT));
    assert_eq!(logs[2].address, REGISTRY);
    assert_eq!(
        logs[2].topics(),
        &[
            SWEEP_TOPIC,
            address_topic(TEST_TOKEN_ADDRESS),
            address_topic(SOURCE),
            address_topic(DESTINATION)
        ]
    );
    assert_eq!(
        U256::from_be_slice(&logs[2].data.data[..32]),
        U256::from(AMOUNT)
    );
    assert_eq!(&logs[2].data.data[32..60], &[0_u8; 28]);
    assert_eq!(
        u32::from_be_bytes(logs[2].data.data[60..64].try_into().unwrap()),
        1,
        "settlement offset points to the sweep Transfer within this receipt"
    );

    assert_eq!(block.header().inner.gas_used, receipt.cumulative_gas_used());
    assert!(
        block.header().inner.gas_used < 350_000,
        "fixed system gas must not be charged to user/header gas"
    );
    assert_eq!(
        block.header().inner.receipts_root,
        calculate_receipt_root_no_memo(std::slice::from_ref(&receipt))
    );
    assert_eq!(block.header().inner.logs_bloom, logs_bloom(logs));
    let parent_header = node
        .inner
        .provider
        .header_by_number(block.number() - 1)?
        .expect("parent header");
    assert_ne!(
        block.header().inner.state_root,
        parent_header.inner.state_root,
        "sweep block state root must differ from its parent"
    );

    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(SOURCE))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DESTINATION))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}

/// Left-pads an address into a 32-byte ABI word.
fn addr_word(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

/// Produces the source's EIP-712 `SweepAuthorization` signature
/// (65-byte r||s||v) exactly as the production Registry verifies it.
///
/// The v1 message struct is frozen as `SweepAuthorization(address source,
/// address controller, uint64 deadline)`; `registry` and `chain_id` are part of
/// the EIP-712 domain separator, not the message.
#[derive(Debug, Clone, Copy)]
struct SourceAuthorization {
    source: Address,
    controller: Address,
    registry: Address,
    chain_id: u64,
    deadline: u64,
}

fn sign_source_auth(
    source_signer: &alloy_signer_local::PrivateKeySigner,
    authorization: SourceAuthorization,
) -> eyre::Result<Vec<u8>> {
    use alloy_primitives::keccak256;
    use alloy_signer::SignerSync;

    let SourceAuthorization {
        source,
        controller,
        registry,
        chain_id,
        deadline,
    } = authorization;

    let domain_typehash = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let auth_typehash =
        keccak256("SweepAuthorization(address source,address controller,uint64 deadline)");

    let mut dom = Vec::new();
    dom.extend_from_slice(domain_typehash.as_slice());
    dom.extend_from_slice(keccak256("SweepRegistry").as_slice());
    dom.extend_from_slice(keccak256("1").as_slice());
    dom.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    dom.extend_from_slice(&addr_word(registry));
    let domain_sep = keccak256(&dom);

    let mut sh = Vec::new();
    sh.extend_from_slice(auth_typehash.as_slice());
    sh.extend_from_slice(&addr_word(source));
    sh.extend_from_slice(&addr_word(controller));
    sh.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    let struct_hash = keccak256(&sh);

    let mut pre = Vec::with_capacity(66);
    pre.extend_from_slice(&[0x19, 0x01]);
    pre.extend_from_slice(domain_sep.as_slice());
    pre.extend_from_slice(struct_hash.as_slice());
    let digest = keccak256(&pre);

    // OZ ECDSA.recover expects v in {27,28}; alloy may return y_parity {0,1}.
    let mut bytes = source_signer.sign_hash_sync(&digest)?.as_bytes().to_vec();
    if bytes.len() == 65 && bytes[64] < 27 {
        bytes[64] += 27;
    }
    Ok(bytes)
}

/// ABI-encodes `registerSweep(address,address,uint64,bytes)`.
fn register_calldata(source: Address, controller: Address, deadline: u64, sig: &[u8]) -> Bytes {
    let mut data = vec![0x47, 0x58, 0x0c, 0xee];
    data.extend_from_slice(&addr_word(source));
    data.extend_from_slice(&addr_word(controller));
    data.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(128u64).to_be_bytes::<32>()); // offset to bytes arg
    data.extend_from_slice(&U256::from(sig.len()).to_be_bytes::<32>());
    data.extend_from_slice(sig);
    let pad = (32 - sig.len() % 32) % 32;
    data.extend(std::iter::repeat_n(0u8, pad));
    data.into()
}

/// Drives the EL against the production `SweepRegistry` (not the test double).
///
/// The source accounts only sign EIP-712 authorizations; the destination submits
/// ordinary `registerSweep` transactions, so neither source needs native ETH.
#[tokio::test(flavor = "multi_thread")]
async fn onyx_production_registry_resolves_and_sweeps() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let source_signer = morph_node::test_utils::wallet_at_index(9, 2910);
    let source = source_signer.address();
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_code(TEST_TOKEN_ADDRESS, SLOT1_ERC20_RUNTIME_CODE)
        .with_account_code(PROD_REGISTRY, PROD_REGISTRY_RUNTIME.trim())
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let owner = wallet.inner.address();
    assert_eq!(owner, SENDER);
    let destination = owner;
    let deadline = u64::MAX;

    // Initialize the directly injected production runtime before using its
    // OwnableUpgradeable and EIP-712 state.
    let mut init = vec![0xc4, 0xd6, 0x6d, 0xe8];
    init.extend_from_slice(&addr_word(owner));
    let init_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(init)
        .build_signed()?;
    node.rpc.inject_tx(init_tx).await?;
    let init_payload = node.advance_block().await?;
    let init_hash = *init_payload.block().body().transactions[0].tx_hash();
    assert!(
        node.inner
            .provider
            .receipt_by_hash(init_hash)?
            .expect("init receipt")
            .status(),
        "initialize(owner) must succeed"
    );
    let source_before = node.inner.provider.latest()?.basic_account(&source)?;
    assert_eq!(
        source_before
            .as_ref()
            .map(|account| account.balance)
            .unwrap_or_default(),
        U256::ZERO,
        "source must start with no native balance"
    );
    assert_eq!(
        source_before
            .as_ref()
            .map(|account| account.nonce)
            .unwrap_or_default(),
        0,
        "source must not send a registration transaction"
    );

    // Establish the route: the controller (the destination/owner here) points its
    // single destination pointer at the recipient. Every registerSweep below
    // requires a configured route, or it reverts with DestinationNotConfigured.
    let mut route = vec![0xf0, 0x47, 0x57, 0x91];
    route.extend_from_slice(&addr_word(destination));
    let route_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(route)
        .build_signed()?;
    node.rpc.inject_tx(route_tx).await?;
    let route_payload = node.advance_block().await?;
    let route_hash = *route_payload.block().body().transactions[0].tx_hash();
    assert!(
        node.inner
            .provider
            .receipt_by_hash(route_hash)?
            .expect("route receipt")
            .status(),
        "setSweepDestination must succeed"
    );

    // Enable the token in the V1 Registry. The controller (the destination/owner)
    // submits every registerSweep below, so no EIP-7702 transaction or operator
    // is needed.
    let mut token_whitelist = vec![0xc9, 0xbc, 0xc9, 0x7e];
    token_whitelist.extend_from_slice(&addr_word(TEST_TOKEN_ADDRESS));
    token_whitelist.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    let whitelist_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 2)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(token_whitelist)
        .build_signed()?;
    node.rpc.inject_tx(whitelist_tx).await?;
    let whitelist_payload = node.advance_block().await?;
    let whitelist_hash = *whitelist_payload.block().body().transactions[0].tx_hash();
    let whitelist_receipt = node
        .inner
        .provider
        .receipt_by_hash(whitelist_hash)?
        .expect("token whitelist receipt");
    assert!(
        whitelist_receipt.status(),
        "setTokenWhitelist must succeed: {whitelist_receipt:?}"
    );

    // Register the plain-EOA source with the controller's authorization.
    let sig = sign_source_auth(
        &source_signer,
        SourceAuthorization {
            source,
            controller: destination,
            registry: PROD_REGISTRY,
            chain_id: wallet.chain_id,
            deadline,
        },
    )?;
    // The controller (the destination/owner here) submits the registration; the
    // source remains a plain EOA.
    let tx3 = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 3)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(register_calldata(source, destination, deadline, &sig))
        .build_signed()?;
    node.rpc.inject_tx(tx3).await?;
    let reg_payload = node.advance_block().await?;
    let reg_hash = *reg_payload.block().body().transactions[0].tx_hash();
    let reg_receipt = node
        .inner
        .provider
        .receipt_by_hash(reg_hash)?
        .expect("register receipt");
    assert!(
        reg_receipt.status(),
        "registerSweep must succeed with a valid EIP-712 source signature: {reg_receipt:?}"
    );
    assert!(
        reg_receipt
            .logs()
            .iter()
            .any(|l| l.address == PROD_REGISTRY),
        "registration must emit a SweepRegistered event from the Registry"
    );
    let source_after_registration = node.inner.provider.latest()?.basic_account(&source)?;
    assert_eq!(
        source_after_registration
            .as_ref()
            .map(|account| account.balance)
            .unwrap_or_default(),
        U256::ZERO,
        "registration must not require native balance from the source"
    );
    assert_eq!(
        source_after_registration
            .as_ref()
            .map(|account| account.nonce)
            .unwrap_or_default(),
        0,
        "source must not send a registration transaction"
    );

    // Pinned-codehash inflow -> EL sweeps via the real resolveSweep.
    let tx4 = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 4)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(source, U256::from(AMOUNT)))
        .build_signed()?;
    node.rpc.inject_tx(tx4).await?;
    let payload = node.advance_block().await?;
    let tx_hash = *payload.block().body().transactions[0].tx_hash();
    let receipt = node
        .inner
        .provider
        .receipt_by_hash(tx_hash)?
        .expect("transfer receipt");
    assert!(receipt.status());
    let logs = receipt.logs();

    // [main Transfer(owner->source)] ... [sweep Transfer(source->destination)] [Swept]
    assert!(
        logs.len() >= 3,
        "expected main + sweep + Swept logs, got {}",
        logs.len()
    );
    assert_eq!(
        logs[0].topics(),
        &[TRANSFER_TOPIC, address_topic(owner), address_topic(source)]
    );
    let sweep_transfer = &logs[logs.len() - 2];
    assert_eq!(
        sweep_transfer.topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(source),
            address_topic(destination)
        ]
    );
    assert_eq!(
        U256::from_be_slice(&sweep_transfer.data.data),
        U256::from(AMOUNT)
    );
    let sweep = &logs[logs.len() - 1];
    assert_eq!(sweep.address, PROD_REGISTRY);
    assert_eq!(
        sweep.topics(),
        &[
            SWEEP_TOPIC,
            address_topic(TEST_TOKEN_ADDRESS),
            address_topic(source),
            address_topic(destination)
        ]
    );

    // Source fully drained through the production atomic policy path.
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(source))?
            .unwrap_or_default(),
        U256::ZERO,
        "source must be swept to zero via the production Registry resolveSweep"
    );

    Ok(())
}
