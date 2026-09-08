//! Test utilities for Morph node E2E testing.
//!
//! Provides helpers for setting up ephemeral Morph nodes, creating payload
//! attributes, building test transactions, and advancing the chain.
//!
//! # Quick Start
//!
//! ```ignore
//! use morph_node::test_utils::{TestNodeBuilder, HardforkSchedule, advance_chain};
//!
//! // Spin up a node with all forks active at t=0
//! let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
//! let mut node = nodes.pop().unwrap();
//!
//! // Advance 10 blocks with transfer transactions
//! let wallet = Arc::new(Mutex::new(wallet));
//! let payloads = advance_chain(10, &mut node, wallet).await?;
//! ```

use crate::{MorphArgs, MorphNode};
use alloy_consensus::BlockHeader;
use alloy_eips::eip2718::Encodable2718;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_rpc_types_engine::PayloadAttributes;
use alloy_rpc_types_eth::TransactionRequest;
use alloy_signer_local::PrivateKeySigner;
use morph_payload_types::MorphBuiltPayload;
use morph_primitives::{
    MorphTxEnvelope, TxL1Msg, TxMorph, transaction::l1_transaction::L1_TX_TYPE_ID,
};
use reth_e2e_test_utils::{
    NodeHelperType, TmpDB, node::NodeTestContext, transaction::TransactionTestContext,
    wallet::Wallet,
};
use reth_node_api::{NodeTypesWithDBAdapter, TreeConfig};
use reth_node_builder::{EngineNodeLauncher, Node, NodeBuilder, NodeConfig, NodeHandle};
use reth_node_core::args::{DiscoveryArgs, NetworkArgs, RpcServerArgs};
use reth_payload_builder::BuildNewPayload;
use reth_provider::providers::BlockchainProvider;
use reth_rpc_server_types::RpcModuleSelection;
use reth_tasks::Runtime;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{Instrument, Level, span};

/// Morph Node Helper type alias for E2E tests.
pub type MorphTestNode =
    NodeHelperType<MorphNode, BlockchainProvider<NodeTypesWithDBAdapter<MorphNode, TmpDB>>>;

// =============================================================================
// HardforkSchedule
// =============================================================================

/// Hardfork activation schedule presets for integration tests.
///
/// Controls which Morph hardforks are active at genesis time (t=0).
/// Use `#[test_case]` or similar to parametrize tests across schedules.
///
/// # Example
///
/// ```ignore
/// #[test_case(HardforkSchedule::AllActive ; "all active")]
/// #[test_case(HardforkSchedule::PreJade  ; "pre jade")]
/// #[tokio::test(flavor = "multi_thread")]
/// async fn test_block_building(schedule: HardforkSchedule) -> eyre::Result<()> {
///     let (mut nodes, wallet) = TestNodeBuilder::new().with_schedule(schedule).build().await?;
///     // ...
/// }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub enum HardforkSchedule {
    /// All Morph hardforks active at genesis (block 0 / timestamp 0).
    ///
    /// This is the default. Tests run against the latest protocol version.
    #[default]
    AllActive,

    /// Jade is NOT active; all other forks are active at t=0.
    ///
    /// Use this to test pre-Jade behavior: state root validation skipped,
    /// MorphTx v1 rejected, etc.
    PreJade,

    /// Viridian, Emerald, and Jade are NOT active; all earlier forks are at t=0.
    ///
    /// Use this to test pre-Viridian behavior: EIP-7702 rejected, etc.
    PreViridian,

    /// Forks that are currently active on Hoodi testnet are set to t=0;
    /// forks not yet activated on Hoodi are disabled (u64::MAX).
    ///
    /// Use this to ensure your test exercises the same rules as Hoodi.
    Hoodi,

    /// Forks that are currently active on mainnet are set to t=0;
    /// forks not yet activated on mainnet are disabled (u64::MAX).
    ///
    /// Use this to ensure your test exercises the same rules as mainnet.
    Mainnet,
}

impl HardforkSchedule {
    /// Reference genesis JSON for this schedule (if any).
    ///
    /// Returns the raw genesis JSON string for Hoodi/Mainnet networks,
    /// used to determine which forks are currently active on those networks.
    fn reference_genesis_json(&self) -> Option<&'static str> {
        match self {
            Self::AllActive | Self::PreJade | Self::PreViridian => None,
            Self::Hoodi => Some(include_str!("../../chainspec/res/genesis/hoodi.json")),
            Self::Mainnet => Some(include_str!("../../chainspec/res/genesis/mainnet.json")),
        }
    }

    /// Apply this schedule's fork timestamps to a mutable genesis JSON value.
    ///
    /// - `AllActive`: no changes (test genesis already has all forks at 0)
    /// - `PreJade`: set `jadeForkTime` to `u64::MAX`
    /// - `Hoodi`/`Mainnet`: compare each `*Time` key against the reference network;
    ///   forks active now → 0, forks not yet active → `u64::MAX`.
    ///   Block-based forks (`*Block`) are always kept at 0.
    pub fn apply(&self, genesis: &mut serde_json::Value) {
        match self {
            Self::AllActive => {
                // nothing to do — test genesis has all forks at 0
            }
            Self::PreJade => {
                // Disable only Jade; all other forks remain at 0.
                let config = genesis["config"].as_object_mut().expect("genesis.config");
                config.insert("jadeForkTime".to_string(), serde_json::json!(u64::MAX));
            }
            Self::PreViridian => {
                let config = genesis["config"].as_object_mut().expect("genesis.config");
                config.insert("viridianTime".to_string(), serde_json::json!(u64::MAX));
                config.insert("emeraldTime".to_string(), serde_json::json!(u64::MAX));
                config.insert("jadeForkTime".to_string(), serde_json::json!(u64::MAX));
            }
            Self::Hoodi | Self::Mainnet => {
                let reference_json = self.reference_genesis_json().unwrap();
                let reference: serde_json::Value =
                    serde_json::from_str(reference_json).expect("reference genesis must parse");

                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system time after epoch")
                    .as_secs();

                let config = genesis["config"]
                    .as_object_mut()
                    .expect("genesis.config must be object");

                // For each *Time key in the test genesis, override based on reference network.
                for (key, value) in config.iter_mut() {
                    if !key.ends_with("Time") {
                        continue;
                    }
                    let new_ts = match reference["config"][key.as_str()].as_u64() {
                        // Fork already active on reference network → activate at t=0 in test.
                        Some(ts) if ts <= now => 0u64,
                        // Fork not yet active or absent → disable in test.
                        _ => u64::MAX,
                    };
                    *value = serde_json::json!(new_ts);
                }
            }
        }
    }
}

// =============================================================================
// TestNodeBuilder
// =============================================================================

/// Builder for configuring and launching ephemeral Morph test nodes.
///
/// # Example
///
/// ```ignore
/// // Single node with all forks active (default)
/// let (mut nodes, wallet) = TestNodeBuilder::new().build().await?;
///
/// // Single node with Jade disabled
/// let (mut nodes, wallet) = TestNodeBuilder::new()
///     .with_schedule(HardforkSchedule::PreJade)
///     .build()
///     .await?;
///
/// // Two nodes connected to each other
/// let (nodes, wallet) = TestNodeBuilder::new()
///     .with_num_nodes(2)
///     .build()
///     .await?;
///
/// // Single node with a non-default payload-builder limit
/// let (nodes, wallet) = TestNodeBuilder::new()
///     .with_max_tx_payload_bytes(3)
///     .build()
///     .await?;
/// ```
pub struct TestNodeBuilder {
    genesis_json: serde_json::Value,
    schedule: HardforkSchedule,
    num_nodes: usize,
    is_dev: bool,
    desired_gas_limit: Option<u64>,
    morph_args: Option<MorphArgs>,
}

impl Default for TestNodeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl TestNodeBuilder {
    /// Create a builder pre-loaded with the test genesis (`tests/assets/test-genesis.json`).
    ///
    /// The test genesis has all Morph hardforks active at block/timestamp 0 and
    /// funds four test accounts derived from the standard test mnemonic.
    pub fn new() -> Self {
        let genesis_json: serde_json::Value =
            serde_json::from_str(include_str!("../tests/assets/test-genesis.json"))
                .expect("test-genesis.json must be valid JSON");

        Self {
            genesis_json,
            schedule: HardforkSchedule::AllActive,
            num_nodes: 1,
            is_dev: false,
            desired_gas_limit: None,
            morph_args: None,
        }
    }

    /// Set the hardfork schedule.
    pub fn with_schedule(mut self, schedule: HardforkSchedule) -> Self {
        self.schedule = schedule;
        self
    }

    /// Override the gas limit in the genesis block.
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.genesis_json["gasLimit"] = serde_json::json!(format!("{gas_limit:#x}"));
        self
    }

    /// Override an account's runtime bytecode in the test genesis.
    pub fn with_account_code(mut self, address: Address, code: impl Into<String>) -> Self {
        let alloc = self
            .genesis_json
            .get_mut("alloc")
            .and_then(serde_json::Value::as_object_mut)
            .expect("test genesis alloc must be an object");
        let address = address.to_string().to_ascii_lowercase();
        let entry = alloc
            .entry(address)
            .or_insert_with(|| serde_json::json!({ "balance": "0x0" }));
        let entry = entry
            .as_object_mut()
            .expect("test genesis account entry must be an object");
        entry.insert("code".to_string(), serde_json::json!(code.into()));
        self
    }

    /// Set the number of nodes to start.
    ///
    /// When `num_nodes > 1`, all nodes are interconnected via a simulated P2P network.
    pub fn with_num_nodes(mut self, n: usize) -> Self {
        self.num_nodes = n;
        self
    }

    /// Enable or disable dev mode (auto-sealing blocks every 100ms).
    pub fn with_dev(mut self, is_dev: bool) -> Self {
        self.is_dev = is_dev;
        self
    }

    /// Set the sequencer's header `gasLimit` target, as `--builder.gaslimit` would.
    ///
    /// Assembled blocks then ramp toward this value instead of copying the parent.
    pub fn with_desired_gas_limit(mut self, desired_gas_limit: u64) -> Self {
        self.desired_gas_limit = Some(desired_gas_limit);
        self
    }

    /// Override the maximum pool-transaction payload bytes included in a block.
    ///
    /// Restricted to single-node setups.
    pub fn with_max_tx_payload_bytes(mut self, max_bytes: u64) -> Self {
        self.morph_args
            .get_or_insert_with(MorphArgs::default)
            .max_tx_payload_bytes = max_bytes;
        self
    }

    /// Build and launch the configured nodes.
    ///
    /// Returns the node handles and a wallet derived from
    /// the standard test mnemonic (`test test test ... junk`).
    pub async fn build(mut self) -> eyre::Result<(Vec<MorphTestNode>, Wallet)> {
        // Apply the hardfork schedule to the genesis JSON before parsing.
        self.schedule.apply(&mut self.genesis_json);

        let genesis: Genesis = serde_json::from_value(self.genesis_json)?;
        let chain_spec = morph_chainspec::MorphChainSpec::from_genesis(genesis);

        if let Some(args) = self.morph_args {
            if self.num_nodes != 1 {
                return Err(eyre::eyre!(
                    "custom Morph arguments support one test node, but {} were requested; \
                     the upstream multi-node helper builds MorphNode::default() per node",
                    self.num_nodes
                ));
            }
            let (node, wallet) =
                launch_test_node_with_args(Arc::new(chain_spec), self.is_dev, args).await?;
            return Ok((vec![node], wallet));
        }

        // Built via `E2ETestSetupBuilder` rather than `setup_engine` so the node config
        // can carry `--builder.gaslimit`, which `setup_engine` gives no way to set.
        let is_dev = self.is_dev;
        let desired_gas_limit = self.desired_gas_limit;
        reth_e2e_test_utils::E2ETestSetupBuilder::<MorphNode, _>::new(
            self.num_nodes,
            Arc::new(chain_spec),
            morph_payload_attributes,
        )
        .with_node_config_modifier(move |mut config| {
            config.builder.gas_limit = desired_gas_limit;
            config.set_dev(is_dev)
        })
        .build()
        .await
    }
}

/// Launch one node from explicit [`MorphArgs`].
///
/// Mirrors `reth_e2e_test_utils::setup_engine` for a single node, including its
/// network, RPC, and tree-config defaults. It exists because that helper builds
/// the node as `N::default()` and exposes no hook to pass an instance, so tests
/// that need non-default payload-builder limits cannot go through it. Keep the
/// config here in sync with upstream so the two paths stay comparable.
async fn launch_test_node_with_args(
    chain_spec: Arc<morph_chainspec::MorphChainSpec>,
    is_dev: bool,
    args: MorphArgs,
) -> eyre::Result<(MorphTestNode, Wallet)> {
    use reth_chainspec::EthChainSpec;

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
        )
        .set_dev(is_dev);
    let node = MorphNode::new(args);
    let tree_config = TreeConfig::default().with_cross_block_cache_size(1024 * 1024);
    let span = span!(Level::INFO, "node", idx = 0);

    let NodeHandle {
        node,
        node_exit_future: _,
    } = NodeBuilder::new(node_config)
        .testing_node(runtime)
        .with_types_and_provider::<MorphNode, BlockchainProvider<_>>()
        .with_components(node.components_builder())
        .with_add_ons(node.add_ons())
        .launch_with_fn(|builder| {
            let launcher = EngineNodeLauncher::new(
                builder.task_executor().clone(),
                builder.config().datadir(),
                tree_config,
            );
            builder.launch_with(launcher)
        })
        .instrument(span)
        .await?;

    let node = NodeTestContext::new(node, morph_payload_attributes).await?;
    let genesis_number = chain_spec.genesis_header().number();
    let genesis_hash = node.block_hash(genesis_number);
    node.update_forkchoice(genesis_hash, genesis_hash).await?;

    Ok((
        node,
        Wallet::default().with_chain_id(chain_spec.chain().into()),
    ))
}

/// Creates ephemeral Morph nodes for E2E testing.
///
/// Convenience wrapper around [`TestNodeBuilder`] for tests that don't need
/// custom hardfork schedules or payload-builder limits.
pub async fn setup(num_nodes: usize, is_dev: bool) -> eyre::Result<(Vec<MorphTestNode>, Wallet)> {
    TestNodeBuilder::new()
        .with_num_nodes(num_nodes)
        .with_dev(is_dev)
        .build()
        .await
}

// =============================================================================
// Chain advancement helpers
// =============================================================================

/// Advance the chain by `length` blocks, each containing one transfer transaction.
///
/// Returns the built payloads for inspection.
pub async fn advance_chain(
    length: usize,
    node: &mut MorphTestNode,
    wallet: Arc<Mutex<Wallet>>,
) -> eyre::Result<Vec<MorphBuiltPayload>> {
    node.advance(length as u64, |_| {
        let wallet = wallet.clone();
        Box::pin(async move {
            let mut wallet = wallet.lock().await;
            let nonce = wallet.inner_nonce;
            wallet.inner_nonce += 1;
            transfer_tx_with_nonce(wallet.chain_id, wallet.inner.clone(), nonce).await
        })
    })
    .await
}

/// Advance the chain by one block without injecting any transactions.
///
/// Uses the same direct `send_new_payload` + `resolve_kind` approach as the
/// L1 message helper to avoid polluting the payload event stream.
pub async fn advance_empty_block(node: &mut MorphTestNode) -> eyre::Result<MorphBuiltPayload> {
    use alloy_consensus::BlockHeader;
    use reth_node_api::PayloadKind;
    use reth_payload_primitives::BuiltPayload;
    use reth_provider::BlockReaderIdExt;

    let head = node
        .inner
        .provider
        .sealed_header_by_number_or_tag(alloy_rpc_types_eth::BlockNumberOrTag::Latest)
        .map_err(|e| eyre::eyre!("provider error: {e}"))?;

    let (head_hash, head_ts) = head
        .map(|h| (h.hash(), h.timestamp()))
        .unwrap_or((B256::ZERO, 0));

    let rpc_attrs = morph_payload_types::MorphPayloadAttributes {
        inner: alloy_rpc_types_engine::PayloadAttributes {
            timestamp: head_ts + 1,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: Some(vec![]),
        no_tx_pool: false,
        gas_limit: None,
        base_fee_per_gas: None,
    };

    let payload_id = node
        .inner
        .payload_builder_handle
        .send_new_payload(BuildNewPayload {
            attributes: rpc_attrs,
            parent_hash: head_hash,
            cache: None,
            state_root_handle: None,
        })
        .await?
        .map_err(|e| eyre::eyre!("payload build failed: {e}"))?;

    // Poll until the payload is available (or 10s timeout)
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let payload = loop {
        if tokio::time::Instant::now() > deadline {
            return Err(eyre::eyre!("timeout waiting for empty block payload"));
        }
        match node
            .inner
            .payload_builder_handle
            .resolve_kind(payload_id, PayloadKind::Earliest)
            .await
        {
            Some(Ok(p)) => break p,
            Some(Err(e)) => return Err(eyre::eyre!("payload build error: {e}")),
            None => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
        }
    };

    node.submit_payload(payload.clone()).await?;
    let block_hash = payload.block().hash();
    node.update_forkchoice(block_hash, block_hash).await?;
    node.sync_to(block_hash).await?;

    Ok(payload)
}

/// Standard test mnemonic phrase (Hardhat/Foundry default).
pub const TEST_MNEMONIC: &str = "test test test test test test test test test test test junk";

/// Return a signer for account at HD derivation index `idx` with the given chain ID.
///
/// Index 0 → `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` (has ETH + tokens in genesis)
/// Index 1 → `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` (has ETH + tokens in genesis)
/// Index 2 → `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` (has ETH only, no tokens)
/// Index 3 → `0x90F79bf6EB2c4f870365E785982E1f101E93b906` (has ETH only, no tokens)
pub fn wallet_at_index(idx: u32, chain_id: u64) -> PrivateKeySigner {
    use alloy_signer::Signer;
    use alloy_signer_local::coins_bip39::English;
    alloy_signer_local::MnemonicBuilder::<English>::default()
        .phrase(TEST_MNEMONIC)
        .derivation_path(format!("m/44'/60'/0'/0/{idx}"))
        .expect("valid derivation path")
        .build()
        .expect("wallet must build from test mnemonic")
        .with_chain_id(Some(chain_id))
}

/// Creates a signed EIP-1559 transfer transaction with an explicit nonce.
///
/// Public version for use in test helpers outside this module.
pub async fn make_transfer_tx(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> Bytes {
    transfer_tx_with_nonce(chain_id, signer, nonce).await
}

/// Creates a signed EIP-2930 (type 0x01) transaction.
pub fn make_eip2930_tx(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> eyre::Result<Bytes> {
    use alloy_consensus::{SignableTransaction, TxEip2930};
    use alloy_signer::SignerSync;

    let tx = TxEip2930 {
        chain_id,
        nonce,
        gas_price: 20_000_000_000u128,
        gas_limit: 21_000,
        to: TxKind::Call(Address::with_last_byte(0x42)),
        value: U256::from(100),
        access_list: Default::default(),
        input: Bytes::new(),
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let envelope = MorphTxEnvelope::Eip2930(tx.into_signed(sig));
    Ok(envelope.encoded_2718().into())
}

/// Creates a signed EIP-4844 (type 0x03) transaction.
pub fn make_eip4844_tx(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> eyre::Result<Bytes> {
    use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip4844};
    use alloy_signer::SignerSync;

    let tx = TxEip4844 {
        chain_id,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: 20_000_000_000u128,
        max_priority_fee_per_gas: 20_000_000_000u128,
        max_fee_per_blob_gas: 1u128,
        to: Address::with_last_byte(0x42),
        value: U256::from(100),
        access_list: Default::default(),
        input: Bytes::new(),
        blob_versioned_hashes: vec![B256::with_last_byte(0x01)],
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let envelope = EthereumTxEnvelope::Eip4844(tx.into_signed(sig));
    Ok(envelope.encoded_2718().into())
}

/// Creates a signed EIP-7702 (type 0x04) transaction.
pub fn make_eip7702_tx(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> eyre::Result<Bytes> {
    use alloy_consensus::{SignableTransaction, TxEip7702};
    use alloy_eips::eip7702::Authorization;
    use alloy_signer::SignerSync;

    let delegate_to = Address::with_last_byte(0x42);
    let authorization = Authorization {
        chain_id: U256::from(chain_id),
        address: delegate_to,
        nonce,
    };
    let auth_sig = signer
        .sign_hash_sync(&authorization.signature_hash())
        .map_err(|e| eyre::eyre!("auth signing failed: {e}"))?;
    let signed_auth = authorization.into_signed(auth_sig);

    let tx = TxEip7702 {
        chain_id,
        nonce,
        gas_limit: 100_000,
        max_fee_per_gas: 20_000_000_000u128,
        max_priority_fee_per_gas: 20_000_000_000u128,
        to: delegate_to,
        value: U256::ZERO,
        access_list: Default::default(),
        authorization_list: vec![signed_auth],
        input: Bytes::new(),
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("tx signing failed: {e}"))?;
    let envelope = MorphTxEnvelope::Eip7702(tx.into_signed(sig));
    Ok(envelope.encoded_2718().into())
}

/// Creates a signed EIP-1559 contract deployment transaction (CREATE).
///
/// The returned bytes can be injected into the pool via `node.rpc.inject_tx()`.
/// The deployed contract address is computed by `Address::create(sender, nonce)`.
pub fn make_deploy_tx(
    chain_id: u64,
    signer: PrivateKeySigner,
    nonce: u64,
    init_code: impl Into<Bytes>,
) -> eyre::Result<Bytes> {
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_signer::SignerSync;

    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 500_000,
        max_fee_per_gas: 20_000_000_000u128,
        max_priority_fee_per_gas: 20_000_000_000u128,
        to: TxKind::Create,
        value: U256::ZERO,
        access_list: Default::default(),
        input: init_code.into(),
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let envelope = MorphTxEnvelope::Eip1559(tx.into_signed(sig));
    Ok(envelope.encoded_2718().into())
}

/// Creates a signed EIP-1559 transfer transaction with an explicit nonce.
async fn transfer_tx_with_nonce(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> Bytes {
    let tx = TransactionRequest {
        nonce: Some(nonce),
        value: Some(U256::from(100)),
        to: Some(TxKind::Call(Address::random())),
        gas: Some(21_000),
        max_fee_per_gas: Some(20_000_000_000u128),
        max_priority_fee_per_gas: Some(20_000_000_000u128),
        chain_id: Some(chain_id),
        ..Default::default()
    };
    let signed = TransactionTestContext::sign_tx(signer, tx).await;
    signed.encoded_2718().into()
}

/// Creates Morph payload attributes for a given timestamp.
///
/// The attributes generator function passed to reth's E2E test framework.
/// Creates minimal attributes with no L1 messages, suitable for basic tests.
/// Use [`L1MessageBuilder`] + [`advance_block_with_l1_messages`] (in
/// `tests/it/helpers.rs`) for tests that need L1 messages.
pub fn morph_payload_attributes(timestamp: u64) -> morph_payload_types::MorphPayloadAttributes {
    morph_payload_types::MorphPayloadAttributes {
        inner: PayloadAttributes {
            timestamp,
            prev_randao: B256::ZERO,
            suggested_fee_recipient: Address::ZERO,
            withdrawals: Some(vec![]),
            parent_beacon_block_root: Some(B256::ZERO),
            slot_number: None,
            target_gas_limit: None,
        },
        transactions: None,
        no_tx_pool: false,
        gas_limit: None,
        base_fee_per_gas: None,
    }
}

// =============================================================================
// L1MessageBuilder
// =============================================================================

/// Builder for constructing test L1 message transactions.
///
/// L1 messages are deposit-style transactions that arrive from the L1 contract
/// queue. They must appear at the start of a block with strictly sequential
/// `queue_index` values.
///
/// # Example
///
/// ```ignore
/// use morph_node::test_utils::L1MessageBuilder;
///
/// // Build a simple ETH deposit L1 message
/// let l1_msg: Bytes = L1MessageBuilder::new(0)
///     .with_target(recipient_address)
///     .with_value(U256::from(1_000_000_000_000_000_000u128)) // 1 ETH
///     .build_encoded();
/// ```
#[derive(Debug, Clone)]
pub struct L1MessageBuilder {
    /// Queue index of this message in the L2MessageQueue contract.
    queue_index: u64,
    /// L1 address that originally sent this message.
    sender: Address,
    /// L2 target address to call.
    target: Address,
    /// Wei value to transfer to the target.
    value: U256,
    /// Gas limit for L2 execution (prepaid on L1).
    gas_limit: u64,
    /// Optional calldata.
    data: Bytes,
}

impl L1MessageBuilder {
    /// Create a new builder with the given queue index.
    ///
    /// Defaults: sender = `0x01`, target = zero address,
    /// value = 0, gas_limit = 100_000, data = empty.
    pub fn new(queue_index: u64) -> Self {
        Self {
            queue_index,
            sender: Address::with_last_byte(0x01),
            target: Address::ZERO,
            value: U256::ZERO,
            gas_limit: 100_000,
            data: Bytes::new(),
        }
    }

    /// Set the L1 sender address.
    pub fn with_sender(mut self, sender: Address) -> Self {
        self.sender = sender;
        self
    }

    /// Set the L2 target address to call.
    pub fn with_target(mut self, target: Address) -> Self {
        self.target = target;
        self
    }

    /// Set the Wei value to transfer.
    pub fn with_value(mut self, value: U256) -> Self {
        self.value = value;
        self
    }

    /// Set the gas limit for L2 execution.
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Set the calldata for this message.
    pub fn with_data(mut self, data: impl Into<Bytes>) -> Self {
        self.data = data.into();
        self
    }

    /// Build the L1 message and encode it as EIP-2718 bytes.
    ///
    /// The returned bytes can be passed directly as an element of
    /// `MorphPayloadAttributes::transactions`.
    pub fn build_encoded(self) -> Bytes {
        let tx = TxL1Msg {
            queue_index: self.queue_index,
            gas_limit: self.gas_limit,
            to: self.target,
            value: self.value,
            sender: self.sender,
            input: self.data,
        };

        // EIP-2718 encoding: type byte (0x7E) + RLP body
        let mut buf = Vec::with_capacity(1 + tx.fields_len() + 4);
        // TxL1Msg::encode_2718 writes the type byte followed by RLP list
        buf.put_u8(L1_TX_TYPE_ID);
        use alloy_rlp::{BufMut, Header};
        let header = Header {
            list: true,
            payload_length: tx.fields_len(),
        };
        header.encode(&mut buf);
        tx.encode_fields(&mut buf);

        buf.into()
    }

    /// Build a sequence of N sequential L1 messages starting at `start_index`.
    ///
    /// Convenience method for tests that need multiple consecutive L1 messages.
    pub fn build_sequential(start_index: u64, count: u64) -> Vec<Bytes> {
        (start_index..start_index + count)
            .map(|i| Self::new(i).build_encoded())
            .collect()
    }
}

// =============================================================================
// MorphTx test constants
// =============================================================================

/// Token ID of the test ERC20 token registered in the test genesis.
///
/// The token is registered in L2TokenRegistry at
/// `0x5300000000000000000000000000000000000021` with:
/// - token_id = 1
/// - token_address = `TEST_TOKEN_ADDRESS`
/// - price_ratio = 1e18 (1:1 with ETH)
/// - decimals = 18, isActive = true
pub const TEST_TOKEN_ID: u16 = 1;

/// Address of the test ERC20 token deployed in the test genesis.
///
/// Pre-funded with 1000 tokens (1e21 wei) for test accounts 0 and 1.
/// Address: `0x5300000000000000000000000000000000000022`
pub const TEST_TOKEN_ADDRESS: Address = Address::new([
    0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x22,
]);

/// Fee vault configured by the test genesis, which receives token fees.
///
/// Address: `0x530000000000000000000000000000000000000a`
pub const TEST_FEE_VAULT_ADDRESS: Address = Address::new([
    0x53, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x0a,
]);

/// Base slot of the test token's `balances` mapping.
///
/// The registry stores this one-based so that zero means "unknown", and
/// `morph_revm`'s token-fee reader subtracts one. The test genesis registers `2`
/// for `TEST_TOKEN_ID`, so the effective base slot is `1`.
const TEST_TOKEN_BALANCE_BASE_SLOT: u64 = 1;

/// Storage slot holding `account`'s balance of the test ERC20 token.
///
/// Derives the Solidity mapping slot independently from the token-fee
/// implementation so fee-accounting tests do not share their oracle with
/// production code.
pub fn test_token_balance_slot(account: Address) -> B256 {
    let mut preimage = [0u8; 64];
    preimage[12..32].copy_from_slice(account.as_slice());
    preimage[32..].copy_from_slice(&U256::from(TEST_TOKEN_BALANCE_BASE_SLOT).to_be_bytes::<32>());
    alloy_primitives::keccak256(preimage)
}

// =============================================================================
// MorphTxBuilder
// =============================================================================

/// Builder for constructing and signing MorphTx (type 0x7F) transactions.
///
/// MorphTx is Morph's custom transaction type that supports:
/// - v0: ERC20 fee payment (`fee_token_id > 0`)
/// - v1: ETH or ERC20 fee, with optional `reference` and `memo` fields
///
/// # Example — v0 ERC20 fee
///
/// ```ignore
/// use morph_node::test_utils::{MorphTxBuilder, TEST_TOKEN_ID};
///
/// let raw = MorphTxBuilder::new(chain_id, signer, nonce)
///     .with_v0_token_fee(TEST_TOKEN_ID)
///     .build_signed()?;
/// ```
///
/// # Example — v1 ETH fee
///
/// ```ignore
/// let raw = MorphTxBuilder::new(chain_id, signer, nonce)
///     .with_v1_eth_fee()
///     .build_signed()?;
/// ```
pub struct MorphTxBuilder {
    chain_id: u64,
    signer: PrivateKeySigner,
    nonce: u64,
    gas_limit: u64,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
    to: TxKind,
    value: U256,
    input: Bytes,
    version: u8,
    fee_token_id: u16,
    fee_limit: U256,
    reference: Option<B256>,
    memo: Option<Bytes>,
}

impl MorphTxBuilder {
    /// Create a new builder with sensible defaults.
    ///
    /// Defaults to v0, fee_token_id=0 (must call `with_v0_token_fee` or
    /// `with_v1_eth_fee` before building).
    pub fn new(chain_id: u64, signer: PrivateKeySigner, nonce: u64) -> Self {
        Self {
            chain_id,
            signer,
            nonce,
            gas_limit: 100_000,
            max_fee_per_gas: 20_000_000_000u128,
            max_priority_fee_per_gas: 20_000_000_000u128,
            to: TxKind::Call(Address::with_last_byte(0x42)),
            value: U256::ZERO,
            input: Bytes::new(),
            version: 0,
            fee_token_id: 0,
            fee_limit: U256::ZERO,
            reference: None,
            memo: None,
        }
    }

    /// Configure as MorphTx **v0** with ERC20 fee payment.
    ///
    /// - `fee_token_id` must be > 0 (v0 requires ERC20 fee)
    /// - Sets a generous `fee_limit` (1e20 tokens) to avoid rejection
    pub fn with_v0_token_fee(mut self, fee_token_id: u16) -> Self {
        assert!(fee_token_id > 0, "v0 MorphTx requires fee_token_id > 0");
        self.version = 0;
        self.fee_token_id = fee_token_id;
        self.fee_limit = U256::from(100_000_000_000_000_000_000u128); // 100 tokens
        self
    }

    /// Configure as MorphTx **v1** with ETH fee payment (fee_token_id = 0).
    ///
    /// This is the simplest MorphTx variant — fee is paid in ETH like EIP-1559,
    /// but the receipt preserves the MorphTx version/reference/memo fields.
    pub fn with_v1_eth_fee(mut self) -> Self {
        self.version = 1;
        self.fee_token_id = 0;
        self.fee_limit = U256::ZERO;
        self
    }

    /// Configure as MorphTx **v1** with ERC20 fee payment.
    pub fn with_v1_token_fee(mut self, fee_token_id: u16) -> Self {
        assert!(fee_token_id > 0, "v1 ERC20 fee requires fee_token_id > 0");
        self.version = 1;
        self.fee_token_id = fee_token_id;
        self.fee_limit = U256::from(100_000_000_000_000_000_000u128); // 100 tokens
        self
    }

    /// Configure raw MorphTx fields, bypassing version-specific assertions.
    ///
    /// Use this for testing structurally invalid MorphTx configurations
    /// (e.g., v0 + fee_token_id=0, v0 + reference, memo > 64 bytes).
    pub fn with_raw_morph_config(
        mut self,
        version: u8,
        fee_token_id: u16,
        fee_limit: U256,
    ) -> Self {
        self.version = version;
        self.fee_token_id = fee_token_id;
        self.fee_limit = fee_limit;
        self
    }

    /// Set the recipient address.
    pub fn with_to(mut self, to: Address) -> Self {
        self.to = TxKind::Call(to);
        self
    }

    /// Set the ETH value to transfer.
    pub fn with_value(mut self, value: U256) -> Self {
        self.value = value;
        self
    }

    /// Set calldata.
    pub fn with_data(mut self, data: impl Into<Bytes>) -> Self {
        self.input = data.into();
        self
    }

    /// Set an optional reference (v1 only).
    pub fn with_reference(mut self, reference: B256) -> Self {
        self.reference = Some(reference);
        self
    }

    /// Set an optional memo (v1 only, max 64 bytes).
    pub fn with_memo(mut self, memo: impl Into<Bytes>) -> Self {
        self.memo = Some(memo.into());
        self
    }

    /// Set gas limit.
    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = gas_limit;
        self
    }

    /// Set max and priority fees to control deterministic pool ordering.
    pub fn with_fees(mut self, max_fee_per_gas: u128, max_priority_fee_per_gas: u128) -> Self {
        self.max_fee_per_gas = max_fee_per_gas;
        self.max_priority_fee_per_gas = max_priority_fee_per_gas;
        self
    }

    /// Build and sign the MorphTx, returning EIP-2718 encoded bytes.
    pub fn build_signed(self) -> eyre::Result<Bytes> {
        use alloy_consensus::SignableTransaction;
        use alloy_signer::SignerSync;

        let tx = TxMorph {
            chain_id: self.chain_id,
            nonce: self.nonce,
            gas_limit: self.gas_limit,
            max_fee_per_gas: self.max_fee_per_gas,
            max_priority_fee_per_gas: self.max_priority_fee_per_gas,
            to: self.to,
            value: self.value,
            access_list: Default::default(),
            version: self.version,
            fee_token_id: self.fee_token_id,
            fee_limit: self.fee_limit,
            reference: self.reference,
            memo: self.memo,
            input: self.input,
        };

        let sig = self
            .signer
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
        let signed = tx.into_signed(sig);
        let envelope = MorphTxEnvelope::Morph(signed);
        Ok(envelope.encoded_2718().into())
    }
}
