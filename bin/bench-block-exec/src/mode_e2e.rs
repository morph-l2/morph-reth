//! Mode B: End-to-end pipeline benchmark.
//!
//! Submits transactions to the txpool, assembles blocks from the pool
//! (empty `transactions` array in `assembleL2Block`), and imports.
//! This measures the full lifecycle: pool acceptance, block assembly, and import.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::tx_factory::{self, BenchSender, Workload};

use alloy_primitives::Bytes;
use std::io::Write;
use std::time::Instant;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct E2eArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,

    #[arg(long)]
    pub jwt_secret: String,

    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,

    #[arg(long)]
    pub workload: String,

    #[arg(long)]
    pub txs_per_block: u64,

    #[arg(long, default_value = "200")]
    pub blocks: u64,

    #[arg(long, default_value = "100")]
    pub senders: u64,

    #[arg(long)]
    pub output: String,

    #[arg(long, default_value = "unknown")]
    pub engine_name: String,

    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

// ---------------------------------------------------------------------------
// Hex encoding helper
// ---------------------------------------------------------------------------

fn hex_encode(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------------------
// Txpool helpers
// ---------------------------------------------------------------------------

/// Send raw transactions to the txpool via batched JSON-RPC.
///
/// Splits `txs` into chunks of 500 and sends them concurrently with a
/// concurrency limit controlled by a semaphore. Returns elapsed time in
/// milliseconds.
pub async fn submit_to_txpool(
    http_rpc: &str,
    txs: &[Bytes],
    concurrency: usize,
) -> eyre::Result<f64> {
    let client = reqwest::Client::new();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let start = Instant::now();

    let chunk_size = 500;
    let mut handles = Vec::new();

    for (chunk_idx, chunk) in txs.chunks(chunk_size).enumerate() {
        let sem = semaphore.clone();
        let client = client.clone();
        let url = http_rpc.to_string();

        let batch: Vec<serde_json::Value> = chunk
            .iter()
            .enumerate()
            .map(|(i, tx)| {
                let tx_hex = format!("0x{}", hex_encode(tx));
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": "eth_sendRawTransaction",
                    "params": [tx_hex],
                    "id": chunk_idx * chunk_size + i + 1
                })
            })
            .collect();

        let handle = tokio::spawn(async move {
            let _permit = sem
                .acquire()
                .await
                .map_err(|e| eyre::eyre!("semaphore error: {e}"))?;

            let resp = client
                .post(&url)
                .json(&batch)
                .send()
                .await
                .map_err(|e| eyre::eyre!("batch send failed: {e}"))?;

            let results: Vec<serde_json::Value> = resp
                .json()
                .await
                .map_err(|e| eyre::eyre!("batch response parse failed: {e}"))?;

            // Check for errors in the batch response.
            for result in &results {
                if let Some(err) = result.get("error") {
                    return Err(eyre::eyre!("eth_sendRawTransaction error: {}", err));
                }
            }

            Ok::<(), eyre::Report>(())
        });

        handles.push(handle);
    }

    for handle in handles {
        handle.await.map_err(|e| eyre::eyre!("join error: {e}"))??;
    }

    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    Ok(elapsed_ms)
}

/// Wait until each sender's pending nonce reaches its expected value.
///
/// For each sender, polls `eth_getTransactionCount` with `"pending"` tag
/// every 50 ms until the nonce is >= the sender's current nonce. Returns
/// elapsed time in milliseconds.
pub async fn wait_for_pool(
    http_rpc: &str,
    senders: &[BenchSender],
    timeout_secs: u64,
) -> eyre::Result<f64> {
    let client = reqwest::Client::new();
    let start = Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    // For each sender, we need the pending nonce to reach sender.nonce.
    // (tx_factory advances nonce after building, so sender.nonce is already the
    //  expected value after all txs are accepted.)
    let expected: Vec<(String, u64)> = senders
        .iter()
        .map(|s| (format!("{:#x}", s.address), s.nonce))
        .collect();

    loop {
        let mut all_ready = true;

        for (addr_hex, expected_nonce) in &expected {
            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getTransactionCount",
                "params": [addr_hex, "pending"],
                "id": 1
            });

            if let Ok(resp) = client.post(http_rpc).json(&body).send().await {
                if let Ok(json) = resp.json::<serde_json::Value>().await {
                    if let Some(result) = json.get("result").and_then(|r| r.as_str()) {
                        let nonce_hex = result.strip_prefix("0x").unwrap_or(result);
                        if let Ok(pending_nonce) = u64::from_str_radix(nonce_hex, 16) {
                            if pending_nonce < *expected_nonce {
                                all_ready = false;
                                break;
                            }
                        } else {
                            all_ready = false;
                            break;
                        }
                    } else {
                        all_ready = false;
                        break;
                    }
                } else {
                    all_ready = false;
                    break;
                }
            } else {
                all_ready = false;
                break;
            }
        }

        if all_ready {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            return Ok(elapsed_ms);
        }

        if Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "pool did not accept all transactions within {timeout_secs}s"
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: E2eArgs) -> eyre::Result<()> {
    // 1. Parse workload, create engine client and senders.
    let workload: Workload = args.workload.parse()?;
    let client = EngineClient::new(&args.engine_rpc, args.jwt_secret.clone())?;
    let mut senders = tx_factory::generate_senders(args.senders as usize);

    println!(
        "Mode B (e2e): {} blocks x {} txs/block, {} senders, {} workload",
        args.blocks, args.txs_per_block, args.senders, workload
    );

    // 2. Open output file.
    let mut out_file = std::fs::File::create(&args.output)?;
    let mut consecutive_errors: u64 = 0;

    // 3. Main block loop.
    for block_idx in 0..args.blocks {
        let block_number = block_idx + 1;

        // --- build transactions ---
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;

        let expected_tx_count = txs.len() as u64;

        // --- submit to txpool (timed) ---
        let submit_ms = match submit_to_txpool(&args.http_rpc, &txs, 4).await {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("block {block_number}: submit error: {e}");
                consecutive_errors += 1;
                if consecutive_errors >= 5 {
                    eprintln!("5 consecutive errors - aborting.");
                    break;
                }
                write_error_timing(
                    &mut out_file,
                    block_number,
                    expected_tx_count,
                    &args.engine_name,
                    &workload,
                    args.senders,
                )?;
                continue;
            }
        };

        // --- wait for pool acceptance (timed) ---
        let pool_wait_ms = match wait_for_pool(&args.http_rpc, &senders, 60).await {
            Ok(ms) => ms,
            Err(e) => {
                eprintln!("block {block_number}: pool wait error: {e}");
                consecutive_errors += 1;
                if consecutive_errors >= 5 {
                    eprintln!("5 consecutive errors - aborting.");
                    break;
                }
                write_error_timing(
                    &mut out_file,
                    block_number,
                    expected_tx_count,
                    &args.engine_name,
                    &workload,
                    args.senders,
                )?;
                continue;
            }
        };

        // --- assemble block (pulls from pool) ---
        let assemble_params = AssembleL2BlockParams {
            number: block_number,
            transactions: vec![],
            timestamp: Some(block_number),
        };

        let (assembled, assemble_ms, gas_used, actual_tx_count, error) =
            match client.assemble_l2_block(&args.engine_rpc, assemble_params).await {
                Ok((data, ms)) => {
                    let gas = data.gas_used;
                    let count = data.transactions.len() as u64;
                    (Some(data), ms, gas, count, false)
                }
                Err(e) => {
                    eprintln!("block {block_number}: assemble error: {e}");
                    (None, 0.0, 0, 0, true)
                }
            };

        // --- import block ---
        let (import_ms, error) = if let Some(data) = assembled {
            match client.new_l2_block(&args.engine_rpc, data).await {
                Ok(ms) => (ms, error),
                Err(e) => {
                    eprintln!("block {block_number}: import error: {e}");
                    (0.0, true)
                }
            }
        } else {
            (0.0, true)
        };

        // --- error tracking ---
        if error {
            consecutive_errors += 1;
        } else {
            consecutive_errors = 0;
        }

        // --- record timing ---
        let mut timing = BlockTimingV2 {
            block_number,
            tx_count: actual_tx_count,
            expected_tx_count,
            engine: args.engine_name.clone(),
            mode: "e2e".to_string(),
            workload: workload.to_string(),
            senders: args.senders,
            warmup_blocks: 0,
            submit_ms,
            pool_wait_ms,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: block_number,
            cumulative_txs: 0,
            rolling_avg_tps_100: None,
            error,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(out_file, "{line}")?;

        // --- progress ---
        if block_number % 10 == 0 || block_number == args.blocks {
            println!(
                "block {block_number}/{}: submit={:.1}ms pool={:.1}ms assemble={:.1}ms import={:.1}ms total={:.1}ms tps={:.0} included={}/{}",
                args.blocks,
                timing.submit_ms,
                timing.pool_wait_ms,
                timing.assemble_ms,
                timing.import_ms,
                timing.total_ms,
                timing.tps,
                timing.tx_count,
                timing.expected_tx_count,
            );
        }

        // --- bail on repeated failures ---
        if consecutive_errors >= 5 {
            eprintln!("5 consecutive errors - aborting.");
            break;
        }
    }

    println!("Results written to {}", args.output);
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Write an error-only timing line (all timing fields zero, error=true).
fn write_error_timing(
    out: &mut std::fs::File,
    block_number: u64,
    expected_tx_count: u64,
    engine: &str,
    workload: &Workload,
    senders: u64,
) -> eyre::Result<()> {
    let mut timing = BlockTimingV2 {
        block_number,
        tx_count: 0,
        expected_tx_count,
        engine: engine.to_string(),
        mode: "e2e".to_string(),
        workload: workload.to_string(),
        senders,
        warmup_blocks: 0,
        submit_ms: 0.0,
        pool_wait_ms: 0.0,
        assemble_ms: 0.0,
        import_ms: 0.0,
        total_ms: 0.0,
        gas_used: 0,
        tps: 0.0,
        mgas_per_sec: 0.0,
        inclusion_rate: 0.0,
        cumulative_blocks: block_number,
        cumulative_txs: 0,
        rolling_avg_tps_100: None,
        error: true,
    };
    timing.finalize();

    let line = serde_json::to_string(&timing)?;
    writeln!(out, "{line}")?;
    Ok(())
}
