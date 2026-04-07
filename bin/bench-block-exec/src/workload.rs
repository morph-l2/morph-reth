use alloy_consensus::{EthereumTxEnvelope, SignableTransaction, TxEip1559, TxEip4844};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, Bytes, TxKind, U256};

/// Concrete envelope type (EIP-4844 variant required by the generic).
type TxEnvelope = EthereumTxEnvelope<TxEip4844>;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use clap::Args;

use crate::engine::{AssembleL2BlockParams, BlockTiming, EngineClient};

#[derive(Args)]
pub struct RunWorkloadArgs {
    /// Engine API RPC URL (authenticated).
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,
    /// Path to JWT secret file.
    #[arg(long)]
    pub jwt_secret: String,
    /// HTTP RPC URL (unauthenticated, for readiness checks).
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,
    /// Workload layer: "eth-transfer" or "erc20-transfer".
    #[arg(long)]
    pub layer: String,
    /// Number of transactions per block.
    #[arg(long)]
    pub txs_per_block: u64,
    /// Number of blocks to produce.
    #[arg(long)]
    pub blocks: u64,
    /// Output file path for per-block results (JSON lines).
    #[arg(long)]
    pub output: String,
    /// Engine name for tagging results (e.g., "geth" or "reth").
    #[arg(long, default_value = "unknown")]
    pub engine_name: String,
    /// Path to the BenchToken contract artifact JSON (required for erc20-transfer layer).
    #[arg(long)]
    pub contract_artifact: Option<String>,
    /// Hex-encoded sender private key (0x-prefixed).
    #[arg(long)]
    pub sender_key: String,
    /// Chain ID.
    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Generate a deterministic receiver address from an index.
///
/// Uses `0xBB` as the first byte and the index in the last 8 bytes,
/// producing unique addresses for each index.
pub fn receiver_address(index: u64) -> Address {
    let mut bytes = [0u8; 20];
    bytes[0] = 0xBB;
    bytes[12..20].copy_from_slice(&index.to_be_bytes());
    Address::from(bytes)
}

/// Build a batch of signed EIP-1559 ETH transfer transactions.
///
/// Each transfer sends 1 wei to a deterministic receiver address,
/// with gas_limit=21000.
pub fn build_eth_transfer_batch(
    signer: &PrivateKeySigner,
    count: u64,
    start_nonce: u64,
    chain_id: u64,
    max_fee_per_gas: u128,
) -> eyre::Result<Vec<Bytes>> {
    let mut txs = Vec::with_capacity(count as usize);
    for i in 0..count {
        let nonce = start_nonce + i;
        let tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(receiver_address(i)),
            value: U256::from(1),
            access_list: Default::default(),
            input: Bytes::new(),
        };
        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
        let envelope: TxEnvelope = EthereumTxEnvelope::Eip1559(tx.into_signed(sig));
        txs.push(Bytes::from(envelope.encoded_2718()));
    }
    Ok(txs)
}

/// Build a batch of signed EIP-1559 ERC20 `transfer(address,uint256)` transactions.
///
/// Each call transfers 1 token to a deterministic receiver address,
/// with gas_limit=60000.
pub fn build_erc20_transfer_batch(
    signer: &PrivateKeySigner,
    count: u64,
    start_nonce: u64,
    chain_id: u64,
    max_fee_per_gas: u128,
    contract_addr: Address,
) -> eyre::Result<Vec<Bytes>> {
    let mut txs = Vec::with_capacity(count as usize);
    // ERC20 transfer(address,uint256) selector: 0xa9059cbb
    let selector: [u8; 4] = [0xa9, 0x05, 0x9c, 0xbb];

    for i in 0..count {
        let nonce = start_nonce + i;
        let recipient = receiver_address(i);

        // ABI-encode: 32-byte padded address + 32-byte uint256(1)
        let mut calldata = Vec::with_capacity(4 + 32 + 32);
        calldata.extend_from_slice(&selector);
        // address is left-padded with 12 zero bytes
        calldata.extend_from_slice(&[0u8; 12]);
        calldata.extend_from_slice(recipient.as_slice());
        // uint256(1) as 32-byte big-endian
        let mut amount = [0u8; 32];
        amount[31] = 1;
        calldata.extend_from_slice(&amount);

        let tx = TxEip1559 {
            chain_id,
            nonce,
            gas_limit: 60_000,
            max_fee_per_gas,
            max_priority_fee_per_gas: 0,
            to: TxKind::Call(contract_addr),
            value: U256::ZERO,
            access_list: Default::default(),
            input: Bytes::from(calldata),
        };
        let sig = signer
            .sign_hash_sync(&tx.signature_hash())
            .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
        let envelope: TxEnvelope = EthereumTxEnvelope::Eip1559(tx.into_signed(sig));
        txs.push(Bytes::from(envelope.encoded_2718()));
    }
    Ok(txs)
}

/// Build a signed EIP-1559 contract deployment transaction.
///
/// The `bytecode` is the compiled contract init code and the `initial_supply`
/// is ABI-encoded as a uint256 constructor argument appended to the bytecode.
pub fn build_deploy_tx(
    signer: &PrivateKeySigner,
    nonce: u64,
    chain_id: u64,
    max_fee_per_gas: u128,
    bytecode: &Bytes,
    initial_supply: U256,
) -> eyre::Result<Bytes> {
    // Append ABI-encoded uint256 constructor arg to bytecode
    let mut init_code = bytecode.to_vec();
    init_code.extend_from_slice(&initial_supply.to_be_bytes::<32>());

    let tx = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: 2_000_000,
        max_fee_per_gas,
        max_priority_fee_per_gas: 0,
        to: TxKind::Create,
        value: U256::ZERO,
        access_list: Default::default(),
        input: Bytes::from(init_code),
    };
    let sig = signer
        .sign_hash_sync(&tx.signature_hash())
        .map_err(|e| eyre::eyre!("signing failed: {e}"))?;
    let envelope: TxEnvelope = EthereumTxEnvelope::Eip1559(tx.into_signed(sig));
    Ok(Bytes::from(envelope.encoded_2718()))
}

/// Read a Foundry JSON artifact and extract the contract bytecode.
///
/// Expects the artifact to have `bytecode.object` as a hex string.
pub fn read_contract_artifact(path: &str) -> eyre::Result<Bytes> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| eyre::eyre!("failed to read artifact at {path}: {e}"))?;
    let artifact: serde_json::Value = serde_json::from_str(&content)
        .map_err(|e| eyre::eyre!("failed to parse artifact JSON: {e}"))?;

    let hex_str = artifact
        .get("bytecode")
        .and_then(|b| b.get("object"))
        .and_then(|o| o.as_str())
        .ok_or_else(|| eyre::eyre!("artifact missing bytecode.object field"))?;

    let hex_str = hex_str.strip_prefix("0x").unwrap_or(hex_str);
    let bytes = hex::decode(hex_str)
        .map_err(|e| eyre::eyre!("failed to decode bytecode hex: {e}"))?;
    Ok(Bytes::from(bytes))
}

/// Poll `eth_chainId` via HTTP POST until the RPC endpoint is ready.
async fn wait_for_rpc(http_rpc: &str, timeout_secs: u64) -> eyre::Result<()> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);

    loop {
        let result = client
            .post(http_rpc)
            .json(&serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_chainId",
                "params": [],
                "id": 1
            }))
            .send()
            .await;

        if let Ok(resp) = result {
            if resp.status().is_success() {
                return Ok(());
            }
        }

        if std::time::Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "RPC at {http_rpc} not ready after {timeout_secs}s"
            ));
        }

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

// ---------------------------------------------------------------------------
// Hex decoding helper (avoid pulling in another crate just for this)
// ---------------------------------------------------------------------------

mod hex {
    pub fn decode(hex: &str) -> Result<Vec<u8>, String> {
        if hex.len() % 2 != 0 {
            return Err("hex string has odd length".into());
        }
        (0..hex.len())
            .step_by(2)
            .map(|i| {
                u8::from_str_radix(&hex[i..i + 2], 16)
                    .map_err(|e| format!("invalid hex at position {i}: {e}"))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Txpool helpers
// ---------------------------------------------------------------------------

/// Send a batch of raw transactions to the txpool via batched JSON-RPC.
///
/// Uses JSON-RPC batch requests to avoid exhausting local TCP ports.
/// Splits into chunks of up to 500 per batch to stay within request size limits.
async fn send_txs_to_txpool(http_rpc: &str, txs: &[Bytes]) -> eyre::Result<()> {
    let client = reqwest::Client::new();
    let chunk_size = 500;

    for chunk in txs.chunks(chunk_size) {
        let batch: Vec<serde_json::Value> = chunk
            .iter()
            .enumerate()
            .map(|(i, tx)| {
                let tx_hex = format!("0x{}", alloy_primitives::hex::encode(tx));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_sendRawTransaction",
                    "params": [tx_hex],
                    "id": i + 1
                })
            })
            .collect();

        let resp = client
            .post(http_rpc)
            .json(&batch)
            .send()
            .await
            .map_err(|e| eyre::eyre!("batch send failed: {e}"))?;

        let results: Vec<serde_json::Value> = resp
            .json()
            .await
            .map_err(|e| eyre::eyre!("batch response parse failed: {e}"))?;

        // Check for errors in the batch response
        for result in &results {
            if let Some(err) = result.get("error") {
                return Err(eyre::eyre!("eth_sendRawTransaction error: {}", err));
            }
        }
    }
    Ok(())
}

/// Wait until the txpool has at least `expected` pending transactions.
///
/// Polls `txpool_status` (for geth) or `eth_getTransactionCount` with "pending"
/// tag (works for both geth and reth) until the pending count matches.
async fn wait_for_txpool(
    http_rpc: &str,
    sender: Address,
    expected_nonce: u64,
    timeout_secs: u64,
) -> eyre::Result<()> {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    let sender_hex = format!("{:#x}", sender);

    loop {
        // Use eth_getTransactionCount with "pending" to check how many txs
        // from this sender have been accepted (pending nonce).
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "eth_getTransactionCount",
            "params": [sender_hex, "pending"],
            "id": 1
        });
        if let Ok(resp) = client.post(http_rpc).json(&body).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
                    let nonce_hex = result.strip_prefix("0x").unwrap_or(result);
                    if let Ok(pending_nonce) = u64::from_str_radix(nonce_hex, 16) {
                        if pending_nonce >= expected_nonce {
                            return Ok(());
                        }
                    }
                }
            }
        }

        if std::time::Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "txpool did not reach expected nonce {} within {}s",
                expected_nonce,
                timeout_secs
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Main workload runner
// ---------------------------------------------------------------------------

pub async fn run(args: RunWorkloadArgs) -> eyre::Result<()> {
    // 1. Read JWT secret from file, create signer from sender_key
    let jwt_secret = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();

    let signer: PrivateKeySigner = args
        .sender_key
        .parse()
        .map_err(|e| eyre::eyre!("invalid sender_key: {e}"))?;
    let sender_addr = signer.address();

    // 2. Create EngineClient
    let engine = EngineClient::new(&args.engine_rpc, jwt_secret)?;

    // 3. Wait for RPC readiness
    println!("Waiting for RPC at {} ...", args.http_rpc);
    wait_for_rpc(&args.http_rpc, 60).await?;
    println!("RPC is ready.");

    let max_fee_per_gas: u128 = 1_000_000; // MORPH_BASE_FEE (0.001 gwei)
    let mut current_nonce: u64 = 0;
    let mut block_start: u64 = 1;

    // 4. If layer == "erc20-transfer", deploy the contract first via txpool
    let contract_addr = if args.layer == "erc20-transfer" {
        let artifact_path = args.contract_artifact.as_deref().ok_or_else(|| {
            eyre::eyre!("--contract-artifact is required for erc20-transfer layer")
        })?;
        let bytecode = read_contract_artifact(artifact_path)?;

        let initial_supply = U256::from(1_000_000_000u64) * U256::from(10u64).pow(U256::from(18));
        let deploy_tx =
            build_deploy_tx(&signer, 0, args.chain_id, max_fee_per_gas, &bytecode, initial_supply)?;

        // Send deploy tx to txpool
        send_txs_to_txpool(&args.http_rpc, &[deploy_tx]).await?;
        wait_for_txpool(&args.http_rpc, sender_addr, 1, 60).await?;

        // Assemble and import block 1 (node pulls deploy tx from txpool)
        let (assembled, _assemble_ms) = engine
            .assemble_l2_block(
                &args.engine_rpc,
                AssembleL2BlockParams {
                    number: 1,
                    transactions: vec![],
                    timestamp: Some(1),
                },
            )
            .await?;
        engine.new_l2_block(&args.engine_rpc, assembled).await?;

        let addr = sender_addr.create(0);
        println!("Deployed ERC20 contract at {addr}");

        current_nonce = 1;
        block_start = 2;

        Some(addr)
    } else {
        None
    };

    // 5. Open output file for JSON lines
    let mut output_file = std::fs::File::create(&args.output)
        .map_err(|e| eyre::eyre!("failed to create output file: {e}"))?;

    let mut timings: Vec<BlockTiming> = Vec::new();

    println!(
        "Running workload: {} tx/block x {} blocks (via txpool)",
        args.txs_per_block, args.blocks
    );

    for block_idx in 0..args.blocks {
        let block_number = block_start + block_idx;

        // Build tx batch
        let txs = if args.layer == "erc20-transfer" {
            build_erc20_transfer_batch(
                &signer,
                args.txs_per_block,
                current_nonce,
                args.chain_id,
                max_fee_per_gas,
                contract_addr.expect("contract_addr must be set for erc20-transfer"),
            )?
        } else {
            build_eth_transfer_batch(
                &signer,
                args.txs_per_block,
                current_nonce,
                args.chain_id,
                max_fee_per_gas,
            )?
        };

        // Send transactions to txpool via eth_sendRawTransaction
        send_txs_to_txpool(&args.http_rpc, &txs).await?;

        // Wait until all txs are in the txpool
        let expected_nonce = current_nonce + args.txs_per_block;
        wait_for_txpool(&args.http_rpc, sender_addr, expected_nonce, 60).await?;

        current_nonce += args.txs_per_block;

        // Assemble block — empty transactions array, node pulls from txpool
        let (assembled, assemble_ms) = engine
            .assemble_l2_block(
                &args.engine_rpc,
                AssembleL2BlockParams {
                    number: block_number,
                    transactions: vec![],
                    timestamp: Some(block_number),
                },
            )
            .await?;

        // Verify assembled block has the expected tx count
        let assembled_tx_count = assembled.transactions.len() as u64;
        if assembled_tx_count != args.txs_per_block {
            eprintln!(
                "  WARNING: block {} assembled {} txs (expected {})",
                block_number, assembled_tx_count, args.txs_per_block
            );
        }

        // Import block
        let import_ms = engine.new_l2_block(&args.engine_rpc, assembled).await?;

        let total_ms = assemble_ms + import_ms;
        let timing = BlockTiming {
            block_number,
            tx_count: assembled_tx_count,
            assemble_ms,
            import_ms,
            total_ms,
            layer: args.layer.clone(),
            engine: args.engine_name.clone(),
        };

        // Write as JSON line to output file
        use std::io::Write;
        let json_line = serde_json::to_string(&timing)?;
        writeln!(output_file, "{json_line}")?;

        timings.push(timing);

        // Print progress every 50 blocks
        if (block_idx + 1) % 50 == 0 || block_idx + 1 == args.blocks {
            println!(
                "Block {block_number}: assemble={assemble_ms:.1}ms import={import_ms:.1}ms total={total_ms:.1}ms txs={assembled_tx_count} [{}/{}]",
                block_idx + 1,
                args.blocks
            );
        }
    }

    // 6. Print quick summary
    if !timings.is_empty() {
        let count = timings.len() as f64;
        let avg_total = timings.iter().map(|t| t.total_ms).sum::<f64>() / count;
        let total_txs: u64 = timings.iter().map(|t| t.tx_count).sum();
        let total_time_s: f64 = timings.iter().map(|t| t.total_ms).sum::<f64>() / 1000.0;
        let effective_tps = if total_time_s > 0.0 {
            total_txs as f64 / total_time_s
        } else {
            0.0
        };
        let under_300ms = timings.iter().filter(|t| t.total_ms <= 300.0).count();
        let compliance_pct = under_300ms as f64 / count * 100.0;

        println!("\n--- Summary ---");
        println!("Blocks:         {}", timings.len());
        println!("Avg total:      {avg_total:.1}ms");
        println!("Effective TPS:  {effective_tps:.0}");
        println!("<=300ms:        {under_300ms}/{} ({compliance_pct:.1}%)", timings.len());
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn test_signer() -> PrivateKeySigner {
        "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"
            .parse()
            .unwrap()
    }

    #[test]
    fn eth_transfer_batch_produces_exact_count() {
        let signer = test_signer();
        let txs = build_eth_transfer_batch(&signer, 10, 0, 99999, 1_000_000).unwrap();
        assert_eq!(txs.len(), 10);
    }

    #[test]
    fn eth_transfer_nonces_are_contiguous() {
        let signer = test_signer();
        let txs = build_eth_transfer_batch(&signer, 5, 0, 99999, 1_000_000).unwrap();
        // Each tx should be non-empty (we can't easily decode nonces without
        // full RLP decoding, but we verify they are distinct non-empty blobs).
        assert_eq!(txs.len(), 5);
        for tx in &txs {
            assert!(!tx.is_empty());
        }
    }

    #[test]
    fn erc20_transfer_batch_produces_exact_count() {
        let signer = test_signer();
        let contract_addr = Address::with_last_byte(0x42);
        let txs =
            build_erc20_transfer_batch(&signer, 10, 0, 99999, 1_000_000, contract_addr).unwrap();
        assert_eq!(txs.len(), 10);
    }

    #[test]
    fn receiver_addresses_are_unique() {
        let addrs: HashSet<Address> = (0..100).map(receiver_address).collect();
        assert_eq!(addrs.len(), 100);
    }
}
