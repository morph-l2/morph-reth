//! Recoverable deposit sweep integration tests.

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

const REGISTRY: Address = address!("5300000000000000000000000000000000000023");
const DEPOSIT: Address = address!("1000000000000000000000000000000000000001");
const DEPOSIT_TWO: Address = address!("1000000000000000000000000000000000000002");
const MASTER: Address = address!("2000000000000000000000000000000000000002");
const RECIPIENT: Address = address!("3000000000000000000000000000000000000003");
const ROUTER: Address = address!("4000000000000000000000000000000000000004");
const CANDIDATE_EMITTER: Address = address!("5000000000000000000000000000000000000005");
const SENDER: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
const AMOUNT: u64 = 123;
const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const REQUEST_TOPIC: B256 =
    b256!("346554ceae624a5906db47e5591fb8c2f586147cfa7524691bf09d284b167a34");
const SWEEP_TOPIC: B256 = b256!("4cb65f464c97b7cae979110960f2dba5a9447c795638563ad5f1e2b52c6f37dd");

/// Runtime produced by solc 0.8.30 (optimizer runs=200, metadata disabled) from
/// `tests/assets/RecoverableSweepFixtures.sol::TestRecoverableRegistry`.
///
/// It is a deterministic test double, not production Registry bytecode.
const TEST_REGISTRY_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b506004361061003f575f3560e01c80639faa2f2f14610043578063ba1b6c8414610094578063eb991b0e146100de575b5f5ffd5b61007861005136600461014b565b6001600160a01b039182165f90815260208181526040808320938516835292905220541690565b6040516001600160a01b03909116815260200160405180910390f35b6100dc6100a236600461017c565b6001600160a01b039283165f9081526020818152604080832094861683529390529190912080546001600160a01b03191691909216179055565b005b6100dc6100ec36600461014b565b806001600160a01b0316826001600160a01b03167f346554ceae624a5906db47e5591fb8c2f586147cfa7524691bf09d284b167a3460405160405180910390a35050565b80356001600160a01b0381168114610146575f5ffd5b919050565b5f5f6040838503121561015c575f5ffd5b61016583610130565b915061017360208401610130565b90509250929050565b5f5f5f6060848603121561018e575f5ffd5b61019784610130565b92506101a560208501610130565b91506101b360408501610130565b9050925092509256";

/// Runtime for `RecoverableSweepFixtures.sol::TestRecoverableRouter`.
const TEST_ROUTER_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806341f47c231461003857806393d1fef41461004d575b5f5ffd5b61004b61004636600461024a565b610060565b005b61004b61005b366004610292565b610149565b604051632e86db2160e21b81526001600160a01b0380861660048301528085166024830152831660448201526023605360981b019063ba1b6c84906064015f604051808303815f87803b1580156100b5575f5ffd5b505af11580156100c7573d5f5f3e3d5ffd5b505060405163a9059cbb60e01b81526001600160a01b038681166004830152602482018590528716925063a9059cbb91506044016020604051808303815f875af1158015610117573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061013b91906102cc565b610143575f5ffd5b50505050565b60405163a9059cbb60e01b81526001600160a01b0383811660048301526024820183905284169063a9059cbb906044016020604051808303815f875af1158015610195573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906101b991906102cc565b6101c1575f5ffd5b604051632e86db2160e21b81526001600160a01b038085166004830152831660248201525f60448201526023605360981b019063ba1b6c84906064015f604051808303815f87803b158015610214575f5ffd5b505af1158015610226573d5f5f3e3d5ffd5b50505050505050565b80356001600160a01b0381168114610245575f5ffd5b919050565b5f5f5f5f6080858703121561025d575f5ffd5b6102668561022f565b93506102746020860161022f565b92506102826040860161022f565b9396929550929360600135925050565b5f5f5f606084860312156102a4575f5ffd5b6102ad8461022f565b92506102bb6020850161022f565b929592945050506040919091013590565b5f602082840312156102dc575f5ffd5b815180151581146102eb575f5ffd5b939250505056";

/// Runtime for `RecoverableSweepFixtures.sol::TestCandidateEmitter`.
const TEST_CANDIDATE_EMITTER_RUNTIME: &str = "0x6080604052348015600e575f5ffd5b5060015b6010816001600160a01b031611607057604051600181526001600160a01b0382169033907fddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef9060200160405180910390a3606a816072565b90506012565b005b5f6001600160a01b0382166002600160a01b03198101609f57634e487b7160e01b5f52601160045260245ffd5b6001019291505056";

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

async fn transfer_to_deposit(schedule: HardforkSchedule) -> eyre::Result<(U256, U256, usize)> {
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_schedule(schedule)
        .with_account_code(TEST_TOKEN_ADDRESS, SLOT1_ERC20_RUNTIME_CODE)
        .with_account_code(REGISTRY, TEST_REGISTRY_RUNTIME)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let transaction = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
    let deposit = state
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
        .unwrap_or_default();
    let master = state
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
        .unwrap_or_default();
    Ok((deposit, master, receipt.logs().len()))
}

#[tokio::test(flavor = "multi_thread")]
async fn onyx_activation_gates_recoverable_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (deposit, master, log_count) = transfer_to_deposit(HardforkSchedule::PreOnyx).await?;
    assert_eq!(deposit, U256::from(AMOUNT));
    assert_eq!(master, U256::ZERO);
    assert_eq!(
        log_count, 1,
        "pre-Onyx receipt contains only inflow Transfer"
    );

    let (deposit, master, log_count) = transfer_to_deposit(HardforkSchedule::AllActive).await?;
    assert_eq!(deposit, U256::ZERO);
    assert_eq!(master, U256::from(AMOUNT));
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let message = L1MessageBuilder::new(0)
        .with_sender(SENDER)
        .with_target(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let poke = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 2)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xeb, 0x99, 0x1b, 0x0e],
            &[TEST_TOKEN_ADDRESS, DEPOSIT],
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
            address_topic(DEPOSIT)
        ]
    );
    assert!(logs[0].data.data.is_empty());
    assert_eq!(
        logs[1].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(DEPOSIT),
            address_topic(MASTER)
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
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT_TWO, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register_second).await?;
    node.advance_block().await?;

    let inflow_then_disable = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 3)
        .with_v1_eth_fee()
        .with_to(ROUTER)
        .with_data(encode_address_and_uint_args(
            [0x93, 0xd1, 0xfe, 0xf4],
            &[TEST_TOKEN_ADDRESS, DEPOSIT_TWO],
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
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT_TWO))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn zero_resolver_and_code_present_deposit_leave_balance_unswept() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder()
        .with_account_code(DEPOSIT, "0x00")
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let disabled_inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT_TWO, U256::from(AMOUNT)))
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 2)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
    assert_eq!(receipt.logs().len(), 1);
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT_TWO))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    source.rpc.inject_tx(register).await?;
    let setup_payload = source.advance_block().await?;
    import_payload(&replay, &setup_payload).await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
    let master_slot = token_balance_slot(MASTER).to_string();
    let master_value = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert_eq!(
        trace["post"][&token_key]["storage"][&master_slot],
        serde_json::Value::String(master_value),
        "prestate trace post-state must include the hidden sweep credit"
    );
    let deposit_slot = token_balance_slot(DEPOSIT).to_string();
    assert!(
        trace["post"][&token_key]["storage"][&deposit_slot].is_null(),
        "diff tracer omits the deposit slot because it starts and ends at zero"
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
        .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    let parent = node.advance_block().await?;
    let parent_hash = parent.block().hash();

    let sweep_tx = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
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
async fn user_transaction_after_block_sweep_budget_exhaustion_still_succeeds() -> eyre::Result<()> {
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
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
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
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
            "each leading transaction must consume sixteen candidates"
        );
        assert!(
            receipt
                .logs()
                .iter()
                .all(|log| log.topics()[0] == TRANSFER_TOPIC)
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
        1,
        "exhausted budget must leave only the eligible inflow Transfer"
    );
    assert_eq!(
        receipt.logs()[0].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(SENDER),
            address_topic(DEPOSIT)
        ]
    );
    assert_eq!(receipt.logs()[0].address, TEST_TOKEN_ADDRESS);
    assert_eq!(
        U256::from_be_slice(&receipt.logs()[0].data.data),
        U256::from(AMOUNT)
    );
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
            .unwrap_or_default(),
        U256::ZERO
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
    let deposit_slot = token_balance_slot(DEPOSIT).to_string();
    let master_slot = token_balance_slot(MASTER).to_string();
    let amount_word = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert_eq!(
        trace["post"][&token_key]["storage"][&deposit_slot],
        serde_json::Value::String(amount_word.clone()),
        "target transaction trace must preserve exhausted block budget"
    );
    assert!(
        trace["post"][&token_key]["storage"][&master_slot].is_null(),
        "target transaction trace must not credit master after budget exhaustion"
    );

    let block_trace: serde_json::Value = rpc
        .request(
            "debug_traceBlockByHash",
            (payload.block().hash(), trace_options),
        )
        .await?;
    assert_eq!(
        block_trace[4]["result"]["post"][&token_key]["storage"][&deposit_slot],
        serde_json::Value::String(amount_word),
        "full debug block replay must preserve exhausted block budget"
    );
    assert!(block_trace[4]["result"]["post"][&token_key]["storage"][&master_slot].is_null());

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
        !parity_replay[4]["stateDiff"][&token_key]["storage"][&deposit_slot].is_null(),
        "parity block replay must retain the final inflow at the deposit"
    );
    assert!(
        parity_replay[4]["stateDiff"][&token_key]["storage"][&master_slot].is_null(),
        "parity block replay must preserve exhausted block budget"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_trace_calls_do_not_run_recoverable_sweep() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = stateful_builder().build().await?;
    let mut node = nodes.pop().unwrap();
    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let call = serde_json::json!({
        "from": SENDER,
        "to": TEST_TOKEN_ADDRESS,
        "gas": "0x30d40",
        "gasPrice": "0x4a817c800",
        "data": transfer_calldata(DEPOSIT, U256::from(AMOUNT)),
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
    let deposit_slot = token_balance_slot(DEPOSIT).to_string();
    let master_slot = token_balance_slot(MASTER).to_string();
    let amount_word = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert_eq!(
        debug_trace["post"][&token_key]["storage"][&deposit_slot],
        serde_json::Value::String(amount_word),
        "debug_traceCall must retain the synthetic inflow at the deposit"
    );
    assert!(
        debug_trace["post"][&token_key]["storage"][&master_slot].is_null(),
        "debug_traceCall must not execute the canonical-only sweep hook"
    );

    let parity_trace: serde_json::Value = rpc
        .request("trace_call", (call, vec!["stateDiff"], "latest"))
        .await?;
    assert!(
        !parity_trace["stateDiff"][&token_key]["storage"][&deposit_slot].is_null(),
        "trace_call state diff must retain the synthetic inflow"
    );
    assert!(
        parity_trace["stateDiff"][&token_key]["storage"][&master_slot].is_null(),
        "trace_call must not execute the canonical-only sweep hook"
    );

    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
            .unwrap_or_default(),
        U256::ZERO
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn successful_onyx_receipt_commits_sweep_roots_bloom_and_user_gas() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_account_code(TEST_TOKEN_ADDRESS, SLOT1_ERC20_RUNTIME_CODE)
        .with_account_code(REGISTRY, TEST_REGISTRY_RUNTIME)
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let register = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_to(REGISTRY)
        .with_data(encode_address_args(
            [0xba, 0x1b, 0x6c, 0x84],
            &[TEST_TOKEN_ADDRESS, DEPOSIT, MASTER],
        ))
        .build_signed()?;
    node.rpc.inject_tx(register).await?;
    node.advance_block().await?;

    let inflow = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 1)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(DEPOSIT, U256::from(AMOUNT)))
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
        &[
            TRANSFER_TOPIC,
            address_topic(SENDER),
            address_topic(DEPOSIT)
        ]
    );
    assert_eq!(U256::from_be_slice(&logs[0].data.data), U256::from(AMOUNT));
    assert_eq!(logs[1].address, TEST_TOKEN_ADDRESS);
    assert_eq!(
        logs[1].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(DEPOSIT),
            address_topic(MASTER)
        ]
    );
    assert_eq!(U256::from_be_slice(&logs[1].data.data), U256::from(AMOUNT));
    assert_eq!(logs[2].address, REGISTRY);
    assert_eq!(
        logs[2].topics(),
        &[
            SWEEP_TOPIC,
            address_topic(TEST_TOKEN_ADDRESS),
            address_topic(DEPOSIT),
            address_topic(MASTER)
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
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(DEPOSIT))?
            .unwrap_or_default(),
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
            .unwrap_or_default(),
        U256::from(AMOUNT)
    );

    Ok(())
}
