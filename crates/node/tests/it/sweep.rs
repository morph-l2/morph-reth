//! Deposit sweep integration tests.

use alloy_consensus::{BlockHeader, TxReceipt, transaction::TxHashRef};
use alloy_primitives::{Address, B256, Bytes, U256, address, b256, logs_bloom};
use jsonrpsee::core::client::ClientT;
use morph_node::test_utils::{
    HardforkSchedule, L1MessageBuilder, MorphTestNode, MorphTxBuilder, SLOT1_ERC20_RUNTIME_CODE,
    TEST_TOKEN_ADDRESS, TEST_TOKEN_ID, TestNodeBuilder, make_sponsored_eip7702_call,
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
const PROD_REGISTRY: Address = address!("71C95911E9a5D330f4D621842EC243EE1343292e");
const SLOT1_ERC20_CODE_HASH: B256 =
    b256!("e71da1ef1d982047e78309f44c426e0870ac83a406380f3a2251d64d8cec943e");
const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");
const REQUEST_TOPIC: B256 =
    b256!("24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff");
const SWEEP_TOPIC: B256 = b256!("035b37215a69e14a80883933d6aa84f0919a67af9410a4a73e8a23baeca011f0");

/// Runtime produced by solc 0.8.30 (optimizer runs=200, metadata disabled) from
/// `tests/assets/SweepFixtures.sol::TestSweepRegistry`.
///
/// It is a deterministic test double, not production Registry bytecode.
const TEST_REGISTRY_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b5060043610610055575f3560e01c8063663a375c14610059578063753d75631461006e5780639faa2f2f146100a5578063ba1b6c84146100dd578063fcdd97bf1461013a575b5f5ffd5b61006c61006736600461025f565b610167565b005b61009061007c366004610290565b60016020525f908152604090205460ff1681565b60405190151581526020015b60405180910390f35b6100b86100b336600461025f565b6101ab565b604080516001600160a01b03909416845260208401929092529082015260600161009c565b61006c6100eb3660046102b0565b6001600160a01b039283165f908152600160208181526040808420805460ff19169093179092558281528183209486168352939093529190912080546001600160a01b03191691909216179055565b610159610148366004610290565b60026020525f908152604090205481565b60405190815260200161009c565b806001600160a01b0316826001600160a01b03167f24e3f180db341974dcd99a5e223d9d944422e303230ddde6659302f8620bbcff60405160405180910390a35050565b6001600160a01b038083165f8181526020818152604080832086861684528252808320549383526001909152812054919092169190819060ff1615806101f857506001600160a01b038316155b1561020a57505f91508190508061023d565b6001600160a01b0385165f818152600260205260409020549084903f82156102325782610235565b60015b935093509350505b9250925092565b80356001600160a01b038116811461025a575f5ffd5b919050565b5f5f60408385031215610270575f5ffd5b61027983610244565b915061028760208401610244565b90509250929050565b5f602082840312156102a0575f5ffd5b6102a982610244565b9392505050565b5f5f5f606084860312156102c2575f5ffd5b6102cb84610244565b92506102d960208501610244565b91506102e760408501610244565b9050925092509256";


/// Runtime for `SweepFixtures.sol::TestSweepRouter`.
const TEST_ROUTER_RUNTIME: &str = "0x608060405234801561000f575f5ffd5b5060043610610034575f3560e01c806341f47c231461003857806393d1fef41461004d575b5f5ffd5b61004b61004636600461024a565b610060565b005b61004b61005b366004610292565b610149565b604051632e86db2160e21b81526001600160a01b0380861660048301528085166024830152831660448201526023605360981b019063ba1b6c84906064015f604051808303815f87803b1580156100b5575f5ffd5b505af11580156100c7573d5f5f3e3d5ffd5b505060405163a9059cbb60e01b81526001600160a01b038681166004830152602482018590528716925063a9059cbb91506044016020604051808303815f875af1158015610117573d5f5f3e3d5ffd5b505050506040513d601f19601f8201168201806040525081019061013b91906102cc565b610143575f5ffd5b50505050565b60405163a9059cbb60e01b81526001600160a01b0383811660048301526024820183905284169063a9059cbb906044016020604051808303815f875af1158015610195573d5f5f3e3d5ffd5b505050506040513d601f19601f820116820180604052508101906101b991906102cc565b6101c1575f5ffd5b604051632e86db2160e21b81526001600160a01b038085166004830152831660248201525f60448201526023605360981b019063ba1b6c84906064015f604051808303815f87803b158015610214575f5f3d5ffd5b505af1158015610226573d5f5f3e3d5ffd5b50505050505050565b80356001600160a01b0381168114610245575f5ffd5b919050565b5f5f5f5f6080858703121561025d575f5ffd5b6102668561022f565b93506102746020860161022f565b92506102826040860161022f565b9396929550929360600135925050565b5f5f5f606084860312156102a4575f5ffd5b6102ad8461022f565b92506102bb6020850161022f565b929592945050506040919091013590565b5f602082840312156102dc575f5ffd5b815180151581146102eb575f5ffd5b939250505056";


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

async fn transfer_to_deposit(schedule: HardforkSchedule) -> eyre::Result<(U256, U256, usize)> {
    let (mut nodes, wallet) = stateful_builder().with_schedule(schedule).build().await?;
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
async fn onyx_activation_gates_sweep() -> eyre::Result<()> {
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
            [0x66, 0x3a, 0x37, 0x5c],
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
async fn unwhitelisted_fake_logs_do_not_exhaust_the_execution_quota() -> eyre::Result<()> {
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
            "each leading transaction emits sixteen bounded preflight candidates"
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
        3,
        "unwhitelisted fake logs must not suppress the eligible sweep"
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
        U256::ZERO
    );
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(MASTER))?
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
    let deposit_slot = token_balance_slot(DEPOSIT).to_string();
    let master_slot = token_balance_slot(MASTER).to_string();
    let amount_word = B256::from(U256::from(AMOUNT).to_be_bytes::<32>()).to_string();
    assert!(
        trace["post"][&token_key]["storage"][&deposit_slot].is_null(),
        "net-zero deposit storage should be omitted from the target diff"
    );
    assert_eq!(
        trace["post"][&token_key]["storage"][&master_slot],
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
        block_trace[4]["result"]["post"][&token_key]["storage"][&deposit_slot].is_null(),
        "full debug replay should omit the net-zero deposit slot"
    );
    assert_eq!(
        block_trace[4]["result"]["post"][&token_key]["storage"][&master_slot],
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
        parity_replay[4]["stateDiff"][&token_key]["storage"][&deposit_slot].is_null(),
        "parity replay should omit the net-zero deposit slot"
    );
    assert!(
        !parity_replay[4]["stateDiff"][&token_key]["storage"][&master_slot].is_null(),
        "parity block replay must include the eligible sweep"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn synthetic_trace_calls_do_not_run_sweep() -> eyre::Result<()> {
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

/// Left-pads an address into a 32-byte ABI word.
fn addr_word(a: Address) -> [u8; 32] {
    let mut w = [0u8; 32];
    w[12..].copy_from_slice(a.as_slice());
    w
}

/// Produces the deposit's EIP-712 `SweepAuthorization` signature
/// (65-byte r||s||v) exactly as the production Registry verifies it (§5.3).
#[derive(Debug, Clone, Copy)]
struct DepositAuthorization {
    deposit: Address,
    master: Address,
    registry: Address,
    chain_id: u64,
    nonce: u64,
    deadline: u64,
}

fn sign_deposit_auth(
    deposit_signer: &alloy_signer_local::PrivateKeySigner,
    authorization: DepositAuthorization,
) -> eyre::Result<Vec<u8>> {
    use alloy_primitives::keccak256;
    use alloy_signer::SignerSync;

    let DepositAuthorization {
        deposit,
        master,
        registry,
        chain_id,
        nonce,
        deadline,
    } = authorization;

    let domain_typehash = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let auth_typehash = keccak256(
        "SweepAuthorization(address deposit,address master,address registry,uint256 chainId,uint256 nonce,uint64 deadline,bytes32 mode,bytes32 sweepScope)",
    );
    let mode = keccak256("MORPH_SWEEP_V1");
    let sweep_scope = keccak256("WHITELISTED_ERC20_TO_MASTER_ONLY");

    let mut dom = Vec::new();
    dom.extend_from_slice(domain_typehash.as_slice());
    dom.extend_from_slice(keccak256("SweepRegistry").as_slice());
    dom.extend_from_slice(keccak256("2").as_slice());
    dom.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    dom.extend_from_slice(&addr_word(registry));
    let domain_sep = keccak256(&dom);

    let mut sh = Vec::new();
    sh.extend_from_slice(auth_typehash.as_slice());
    sh.extend_from_slice(&addr_word(deposit));
    sh.extend_from_slice(&addr_word(master));
    sh.extend_from_slice(&addr_word(registry));
    sh.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    sh.extend_from_slice(&U256::from(nonce).to_be_bytes::<32>());
    sh.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    sh.extend_from_slice(mode.as_slice());
    sh.extend_from_slice(sweep_scope.as_slice());
    let struct_hash = keccak256(&sh);

    let mut pre = Vec::with_capacity(66);
    pre.extend_from_slice(&[0x19, 0x01]);
    pre.extend_from_slice(domain_sep.as_slice());
    pre.extend_from_slice(struct_hash.as_slice());
    let digest = keccak256(&pre);

    // OZ ECDSA.recover expects v in {27,28}; alloy may return y_parity {0,1}.
    let mut bytes = deposit_signer.sign_hash_sync(&digest)?.as_bytes().to_vec();
    if bytes.len() == 65 && bytes[64] < 27 {
        bytes[64] += 27;
    }
    Ok(bytes)
}

/// ABI-encodes `registerSweep(address,address,uint256,uint64,bytes)`.
fn register_calldata(
    deposit: Address,
    master: Address,
    nonce: u64,
    deadline: u64,
    sig: &[u8],
) -> Bytes {
    let mut data = vec![0xd7, 0x1b, 0x77, 0xe8];
    data.extend_from_slice(&addr_word(deposit));
    data.extend_from_slice(&addr_word(master));
    data.extend_from_slice(&U256::from(nonce).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(deadline).to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(160u64).to_be_bytes::<32>()); // offset to bytes arg
    data.extend_from_slice(&U256::from(sig.len()).to_be_bytes::<32>());
    data.extend_from_slice(sig);
    let pad = (32 - sig.len() % 32) % 32;
    data.extend(std::iter::repeat_n(0u8, pad));
    data.into()
}

/// Drives the EL against the production `SweepRegistry` (not the test double):
/// owner policy setup + V2 EIP-712 registration + a pinned-codehash token
/// inflow swept through the contract's real fail-closed `resolveSweep`.
#[tokio::test(flavor = "multi_thread")]
async fn onyx_production_registry_resolves_and_sweeps() -> eyre::Result<()> {
    reth_tracing::init_test_tracing();

    const PRELOADED_FEE_BALANCE: u128 = 100_000_000_000_000_000_000;
    let deposit_signer = morph_node::test_utils::wallet_at_index(9, 2910);
    let deposit = deposit_signer.address();
    let fee_deposit_signer = morph_node::test_utils::wallet_at_index(8, 2910);
    let fee_deposit = fee_deposit_signer.address();
    let (mut nodes, wallet) = TestNodeBuilder::new()
        .with_sweep_config(PROD_REGISTRY)
        .with_account_code(TEST_TOKEN_ADDRESS, SLOT1_ERC20_RUNTIME_CODE)
        .with_account_code(PROD_REGISTRY, PROD_REGISTRY_RUNTIME.trim())
        .with_account_storage(PROD_REGISTRY, B256::ZERO, address_topic(SENDER))
        .with_account_storage(
            TEST_TOKEN_ADDRESS,
            token_balance_slot(fee_deposit),
            B256::from(U256::from(PRELOADED_FEE_BALANCE).to_be_bytes::<32>()),
        )
        .build()
        .await?;
    let mut node = nodes.pop().unwrap();

    let owner = wallet.inner.address();
    assert_eq!(owner, SENDER);
    let master = owner;
    let deadline = u64::MAX;
    let deposit_before = node.inner.provider.latest()?.basic_account(&deposit)?;
    assert_eq!(
        deposit_before
            .as_ref()
            .map(|account| account.balance)
            .unwrap_or_default(),
        U256::ZERO,
        "sponsored deposit must start with no native balance"
    );
    assert_eq!(
        deposit_before
            .map(|account| account.nonce)
            .unwrap_or_default(),
        0,
        "sponsored deposit authorization nonce must start at zero"
    );

    // setMasterApproval(master, true)
    let mut master_approval = vec![0x80, 0x53, 0xd0, 0xca];
    master_approval.extend_from_slice(&addr_word(master));
    master_approval.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    let tx0 = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 0)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(master_approval)
        .build_signed()?;
    node.rpc.inject_tx(tx0).await?;
    let approval_payload = node.advance_block().await?;
    let approval_hash = *approval_payload.block().body().transactions[0].tx_hash();
    assert!(
        node.inner
            .provider
            .receipt_by_hash(approval_hash)?
            .expect("master approval receipt")
            .status(),
        "setMasterApproval must succeed"
    );

    // setTokenPolicy(token, true, token.codehash, minimumAmount)
    let mut token_policy = vec![0x6c, 0x8c, 0x33, 0xf4];
    token_policy.extend_from_slice(&addr_word(TEST_TOKEN_ADDRESS));
    token_policy.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    token_policy.extend_from_slice(SLOT1_ERC20_CODE_HASH.as_slice());
    token_policy.extend_from_slice(&U256::from(1).to_be_bytes::<32>());
    let tx1 = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 1)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(token_policy)
        .build_signed()?;
    node.rpc.inject_tx(tx1).await?;
    let policy_payload = node.advance_block().await?;
    let policy_hash = *policy_payload.block().body().transactions[0].tx_hash();
    assert!(
        node.inner
            .provider
            .receipt_by_hash(policy_hash)?
            .expect("token policy receipt")
            .status(),
        "setTokenPolicy must succeed"
    );

    // Register a second, genesis-delegated zero-native deposit. Keeping this
    // authority out of the preceding Type-4 authorization transaction isolates
    // fee-refund sweeping from txpool authority-reservation timing.
    let fee_deposit_sig = sign_deposit_auth(
        &fee_deposit_signer,
        DepositAuthorization {
            deposit: fee_deposit,
            master,
            registry: PROD_REGISTRY,
            chain_id: wallet.chain_id,
            nonce: 0,
            deadline,
        },
    )?;
    let fee_registration = MorphTxBuilder::new(wallet.chain_id, wallet.inner.clone(), 2)
        .with_v1_eth_fee()
        .with_gas_limit(5_000_000)
        .with_to(PROD_REGISTRY)
        .with_data(register_calldata(
            fee_deposit,
            master,
            0,
            deadline,
            &fee_deposit_sig,
        ))
        .build_signed()?;
    node.rpc.inject_tx(fee_registration).await?;
    let fee_registration_payload = node.advance_block().await?;
    let fee_registration_hash = *fee_registration_payload.block().body().transactions[0].tx_hash();
    assert!(
        node.inner
            .provider
            .receipt_by_hash(fee_registration_hash)?
            .expect("fee deposit registration receipt")
            .status(),
        "genesis-delegated fee deposit registration must succeed"
    );

    // The registered zero-native deposit pays for a successful MorphTx with
    // the storage-slot fee token. An intrinsic-only gas limit leaves no unused
    // gas to reimburse, so the configured token-fee caller itself must trigger
    // settlement of the balance left after the maximum fee deduction.
    assert_eq!(
        node.inner
            .provider
            .latest()?
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(fee_deposit))?
            .unwrap_or_default(),
        U256::from(PRELOADED_FEE_BALANCE)
    );
    let fee_tx = MorphTxBuilder::new(wallet.chain_id, fee_deposit_signer, 0)
        .with_v0_token_fee(TEST_TOKEN_ID)
        .with_gas_limit(21_000)
        .with_to(RECIPIENT)
        .build_signed()?;
    node.rpc.inject_tx(fee_tx).await?;
    let fee_payload = node.advance_block().await?;
    assert_eq!(
        fee_payload.block().body().transactions.len(),
        1,
        "registered fee-token transaction must be executable"
    );
    let fee_tx_hash = *fee_payload.block().body().transactions[0].tx_hash();
    let fee_receipt = node
        .inner
        .provider
        .receipt_by_hash(fee_tx_hash)?
        .expect("fee-token receipt");
    assert!(fee_receipt.status(), "fee-token MorphTx must succeed");
    assert_eq!(
        fee_receipt.logs().len(),
        2,
        "slot fee accounting emits no receipt logs; only sweep settlement should remain"
    );
    assert_eq!(
        fee_receipt.logs()[0].topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(fee_deposit),
            address_topic(master)
        ]
    );
    let post_fee_sweep_amount = U256::from_be_slice(&fee_receipt.logs()[0].data.data);
    assert!(
        post_fee_sweep_amount > U256::ZERO
            && post_fee_sweep_amount < U256::from(PRELOADED_FEE_BALANCE),
        "sweep must settle the post-fee balance after retaining the charged fee"
    );
    assert_eq!(fee_receipt.logs()[1].address, PROD_REGISTRY);
    assert_eq!(fee_receipt.logs()[1].topics()[0], SWEEP_TOPIC);
    let state_after_fee_sweep = node.inner.provider.latest()?;
    assert_eq!(
        state_after_fee_sweep
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(fee_deposit))?
            .unwrap_or_default(),
        U256::ZERO,
        "post-fee balance must be swept to zero even when reimbursement is zero"
    );
    assert_eq!(
        state_after_fee_sweep
            .basic_account(&fee_deposit)?
            .expect("delegated fee deposit account")
            .balance,
        U256::ZERO,
        "fee-token execution must not require native balance"
    );

    // registerSweep with the deposit's real EIP-712 signature
    let sig = sign_deposit_auth(
        &deposit_signer,
        DepositAuthorization {
            deposit,
            master,
            registry: PROD_REGISTRY,
            chain_id: wallet.chain_id,
            nonce: 0,
            deadline,
        },
    )?;
    let tx3 = make_sponsored_eip7702_call(
        wallet.chain_id,
        wallet.inner.clone(),
        3,
        deposit_signer,
        0,
        PROD_REGISTRY,
        PROD_REGISTRY,
        register_calldata(deposit, master, 0, deadline, &sig),
    )?;
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
        "registerSweep must succeed with a valid EIP-712 deposit signature"
    );
    assert!(
        reg_receipt
            .logs()
            .iter()
            .any(|l| l.address == PROD_REGISTRY),
        "registration must emit a SweepRegistered event from the Registry"
    );
    let deposit_after_registration = node
        .inner
        .provider
        .latest()?
        .basic_account(&deposit)?
        .expect("EIP-7702 authorization must create the deposit account");
    assert_eq!(
        deposit_after_registration.balance,
        U256::ZERO,
        "sponsored onboarding must not pre-fund the deposit"
    );
    assert_eq!(
        deposit_after_registration.nonce, 1,
        "authorization must consume the deposit EOA nonce"
    );

    // Pinned-codehash inflow -> EL sweeps via the real resolveSweep.
    let tx4 = MorphTxBuilder::new(wallet.chain_id, wallet.inner, 4)
        .with_v1_eth_fee()
        .with_to(TEST_TOKEN_ADDRESS)
        .with_data(transfer_calldata(deposit, U256::from(AMOUNT)))
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

    // [main Transfer(owner->deposit)] ... [sweep Transfer(deposit->master)] [Swept]
    assert!(
        logs.len() >= 3,
        "expected main + sweep + Swept logs, got {}",
        logs.len()
    );
    assert_eq!(
        logs[0].topics(),
        &[TRANSFER_TOPIC, address_topic(owner), address_topic(deposit)]
    );
    let sweep_transfer = &logs[logs.len() - 2];
    assert_eq!(
        sweep_transfer.topics(),
        &[
            TRANSFER_TOPIC,
            address_topic(deposit),
            address_topic(master)
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
            address_topic(deposit),
            address_topic(master)
        ]
    );

    // Deposit fully drained through the production atomic policy path.
    let state = node.inner.provider.latest()?;
    assert_eq!(
        state
            .storage(TEST_TOKEN_ADDRESS, token_balance_slot(deposit))?
            .unwrap_or_default(),
        U256::ZERO,
        "deposit must be swept to zero via the production Registry resolveSweep"
    );

    Ok(())
}
