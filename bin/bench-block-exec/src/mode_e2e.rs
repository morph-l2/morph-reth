//! Mode B: End-to-end pipeline benchmark.
//!
//! Submits transactions to the txpool, assembles blocks from the pool
//! (empty `transactions` array in `assembleL2Block`), and imports.
//! This measures the full lifecycle: pool acceptance, block assembly, and import.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::tx_factory::{self, BenchSender, Workload};

use alloy_primitives::{Bytes, hex};
use rayon::prelude::*;
use std::{cmp, io::Write, time::Instant};

pub(crate) const DEFAULT_SUBMIT_BATCH_SIZE: usize = 64;
pub(crate) const DEFAULT_SUBMIT_CONCURRENCY: usize = 64;
pub(crate) const DEFAULT_POOL_POLL_BATCH_SIZE: usize = 256;
pub(crate) const DEFAULT_POOL_POLL_INTERVAL_MS: u64 = 10;

#[derive(Clone, Copy, Debug)]
pub(crate) struct SubmitOptions {
    pub batch_size: usize,
    pub concurrency: usize,
}

impl SubmitOptions {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            batch_size: self.batch_size.max(1),
            concurrency: self.concurrency.max(1),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PoolWaitOptions {
    pub batch_size: usize,
    pub poll_interval_ms: u64,
}

impl PoolWaitOptions {
    pub(crate) fn sanitized(self) -> Self {
        Self {
            batch_size: self.batch_size.max(1),
            poll_interval_ms: self.poll_interval_ms.max(1),
        }
    }
}

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

    #[arg(long, default_value_t = DEFAULT_SUBMIT_BATCH_SIZE)]
    pub submit_batch_size: usize,

    #[arg(long, default_value_t = DEFAULT_SUBMIT_CONCURRENCY)]
    pub submit_concurrency: usize,

    #[arg(long, default_value_t = DEFAULT_POOL_POLL_BATCH_SIZE)]
    pub pool_poll_batch_size: usize,

    #[arg(long, default_value_t = DEFAULT_POOL_POLL_INTERVAL_MS)]
    pub pool_poll_interval_ms: u64,
}

pub(crate) fn build_submit_bodies(txs: &[Bytes], batch_size: usize) -> Vec<Vec<u8>> {
    let chunks: Vec<(usize, &[Bytes])> = txs.chunks(batch_size).enumerate().collect();
    chunks
        .par_iter()
        .map(|(ci, chunk)| {
            let mut body = Vec::with_capacity(chunk.len() * 256);
            body.push(b'[');
            for (i, tx) in chunk.iter().enumerate() {
                if i > 0 {
                    body.push(b',');
                }
                let id = ci * batch_size + i + 1;
                write!(
                    body,
                    r#"{{"jsonrpc":"2.0","method":"eth_sendRawTransaction","params":["{}"],"id":{}}}"#,
                    hex::encode_prefixed(tx),
                    id
                )
                .expect("json serialization");
            }
            body.push(b']');
            body
        })
        .collect()
}

fn build_pending_nonce_batches(
    expected: &[(String, u64)],
    batch_size: usize,
) -> Vec<Vec<(String, u64)>> {
    expected
        .chunks(batch_size)
        .map(|chunk| chunk.to_vec())
        .collect()
}

pub(crate) fn build_http_client(pool_max_idle_per_host: usize) -> eyre::Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(pool_max_idle_per_host.max(1))
        .tcp_nodelay(true)
        .build()
        .map_err(Into::into)
}

pub(crate) fn ensure_submit_batch_success(body: &[u8]) -> eyre::Result<()> {
    if !body
        .windows(br#""error""#.len())
        .any(|w| w == br#""error""#)
    {
        return Ok(());
    }

    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| eyre::eyre!("parse failed: {e}"))?;

    let results = match value {
        serde_json::Value::Array(results) => results,
        value => vec![value],
    };

    for result in results {
        if let Some(err) = result.get("error") {
            return Err(eyre::eyre!("eth_sendRawTransaction error: {}", err));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Txpool helpers
// ---------------------------------------------------------------------------

/// Send raw transactions to the txpool via concurrent individual JSON-RPC calls.
///
/// Each transaction is sent as its own HTTP POST request with high concurrency,
/// matching Tempo's approach. This maximizes reth's validation worker parallelism
/// since each individual `eth_sendRawTransaction` becomes an independent validation
/// job that can be processed by a separate worker.
///
/// Pre-encodes all request bodies before timing starts to isolate HTTP throughput.
pub async fn submit_to_txpool(
    http_rpc: &str,
    txs: &[Bytes],
    concurrency: usize,
) -> eyre::Result<f64> {
    let options = SubmitOptions {
        batch_size: DEFAULT_SUBMIT_BATCH_SIZE,
        concurrency,
    };
    let client = build_http_client(options.concurrency)?;
    submit_to_txpool_with_client(&client, http_rpc, txs, options).await
}

pub(crate) async fn submit_to_txpool_with_options(
    http_rpc: &str,
    txs: &[Bytes],
    options: SubmitOptions,
) -> eyre::Result<f64> {
    let client = build_http_client(options.concurrency)?;
    submit_to_txpool_with_client(&client, http_rpc, txs, options).await
}

pub(crate) async fn submit_to_txpool_with_client(
    client: &reqwest::Client,
    http_rpc: &str,
    txs: &[Bytes],
    options: SubmitOptions,
) -> eyre::Result<f64> {
    let mut next_target_idx = 0usize;
    submit_to_txpool_with_target_cursor(
        client,
        &[http_rpc.to_string()],
        txs,
        options,
        &mut next_target_idx,
    )
    .await
}

pub(crate) async fn submit_to_txpool_with_target_cursor(
    client: &reqwest::Client,
    http_rpcs: &[String],
    txs: &[Bytes],
    options: SubmitOptions,
    next_target_idx: &mut usize,
) -> eyre::Result<f64> {
    let options = options.sanitized();
    eyre::ensure!(!http_rpcs.is_empty(), "submit target list must not be empty");

    let bodies = build_submit_bodies(txs, options.batch_size);
    if bodies.is_empty() {
        return Ok(0.0);
    }

    submit_prebuilt_bodies_with_target_cursor(client, http_rpcs, bodies, options, next_target_idx).await
}

pub(crate) async fn submit_prebuilt_bodies_with_target_cursor(
    client: &reqwest::Client,
    http_rpcs: &[String],
    bodies: Vec<Vec<u8>>,
    options: SubmitOptions,
    next_target_idx: &mut usize,
) -> eyre::Result<f64> {
    let options = options.sanitized();
    eyre::ensure!(!http_rpcs.is_empty(), "submit target list must not be empty");
    if bodies.is_empty() {
        return Ok(0.0);
    }

    let start_target_idx = *next_target_idx % http_rpcs.len();
    let batch_count = bodies.len();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(options.concurrency));
    let start = Instant::now();

    let mut handles = Vec::with_capacity(batch_count);

    for (offset, body) in bodies.into_iter().enumerate() {
        let sem = semaphore.clone();
        let client = client.clone();
        let url = http_rpcs[(start_target_idx + offset) % http_rpcs.len()].clone();

        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.map_err(|e| eyre::eyre!("{e}"))?;

            // Fire-and-forget: send the request, check HTTP status, but skip
            // reading/parsing the response body. The pool_wait phase will
            // confirm all txs landed via nonce polling.
            client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| eyre::eyre!("send failed: {e}"))?
                .error_for_status()
                .map_err(|e| eyre::eyre!("http error: {e}"))?;

            Ok::<(), eyre::Report>(())
        }));
    }

    for handle in handles {
        handle.await.map_err(|e| eyre::eyre!("join: {e}"))??;
    }

    *next_target_idx = (start_target_idx + batch_count) % http_rpcs.len();
    Ok(start.elapsed().as_secs_f64() * 1000.0)
}

/// Wait until each sender's pending nonce reaches its expected value.
///
/// Polls `eth_getTransactionCount` with `"pending"` tag in JSON-RPC batches
/// until each sender's pending nonce is >= its expected nonce. Returns elapsed
/// time in milliseconds.
pub async fn wait_for_pool(
    http_rpc: &str,
    senders: &[BenchSender],
    timeout_secs: u64,
) -> eyre::Result<f64> {
    let options = PoolWaitOptions {
        batch_size: DEFAULT_POOL_POLL_BATCH_SIZE,
        poll_interval_ms: DEFAULT_POOL_POLL_INTERVAL_MS,
    };
    let client = build_http_client(options.batch_size)?;
    wait_for_pool_with_client(&client, http_rpc, senders, timeout_secs, options).await
}

async fn pending_nonce_batch_ready(
    client: &reqwest::Client,
    http_rpc: &str,
    batch: &[(String, u64)],
) -> bool {
    let body: Vec<serde_json::Value> = batch
        .iter()
        .enumerate()
        .map(|(idx, (addr_hex, _))| {
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "eth_getTransactionCount",
                "params": [addr_hex, "pending"],
                "id": idx
            })
        })
        .collect();

    let resp = match client.post(http_rpc).json(&body).send().await {
        Ok(resp) => resp,
        Err(_) => return false,
    };

    let values = match resp.json::<serde_json::Value>().await {
        Ok(serde_json::Value::Array(values)) => values,
        Ok(value) => vec![value],
        Err(_) => return false,
    };

    let mut pending_by_id = std::collections::BTreeMap::new();
    for value in values {
        let id = value.get("id").and_then(|id| id.as_u64());
        let nonce = value
            .get("result")
            .and_then(|r| r.as_str())
            .and_then(|result| {
                let nonce_hex = result.strip_prefix("0x").unwrap_or(result);
                u64::from_str_radix(nonce_hex, 16).ok()
            });
        if let (Some(id), Some(nonce)) = (id, nonce) {
            pending_by_id.insert(id as usize, nonce);
        }
    }

    batch.iter().enumerate().all(|(idx, (_, expected_nonce))| {
        pending_by_id.get(&idx).copied().unwrap_or_default() >= *expected_nonce
    })
}

async fn pending_nonce_batches_ready(
    client: &reqwest::Client,
    http_rpc: &str,
    batches: &[Vec<(String, u64)>],
) -> eyre::Result<bool> {
    let mut join_set = tokio::task::JoinSet::new();

    for batch in batches {
        let client = client.clone();
        let http_rpc = http_rpc.to_string();
        let batch = batch.clone();
        join_set.spawn(async move { pending_nonce_batch_ready(&client, &http_rpc, &batch).await });
    }

    let mut all_ready = true;
    while let Some(result) = join_set.join_next().await {
        let ready = result.map_err(|e| eyre::eyre!("join: {e}"))?;
        all_ready &= ready;
    }

    Ok(all_ready)
}

pub(crate) async fn wait_for_pool_with_options(
    http_rpc: &str,
    senders: &[BenchSender],
    timeout_secs: u64,
    options: PoolWaitOptions,
) -> eyre::Result<f64> {
    let client = build_http_client(options.batch_size)?;
    wait_for_pool_with_client(&client, http_rpc, senders, timeout_secs, options).await
}

pub(crate) async fn wait_for_pool_with_client(
    client: &reqwest::Client,
    http_rpc: &str,
    senders: &[BenchSender],
    timeout_secs: u64,
    options: PoolWaitOptions,
) -> eyre::Result<f64> {
    let options = options.sanitized();
    let start = Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);

    // For each sender, we need the pending nonce to reach sender.nonce.
    // (tx_factory advances nonce after building, so sender.nonce is already the
    //  expected value after all txs are accepted.)
    let expected: Vec<(String, u64)> = senders
        .iter()
        .map(|s| (format!("{:#x}", s.address), s.nonce))
        .collect();
    let expected_batches = build_pending_nonce_batches(&expected, options.batch_size);

    loop {
        let all_ready = pending_nonce_batches_ready(client, http_rpc, &expected_batches).await?;

        if all_ready {
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            return Ok(elapsed_ms);
        }

        if Instant::now() >= deadline {
            return Err(eyre::eyre!(
                "pool did not accept all transactions within {timeout_secs}s"
            ));
        }

        tokio::time::sleep(std::time::Duration::from_millis(options.poll_interval_ms)).await;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: E2eArgs) -> eyre::Result<()> {
    // 1. Parse workload, create engine client and senders.
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;
    let mut senders = tx_factory::generate_senders(args.senders as usize);
    let submit_options = SubmitOptions {
        batch_size: args.submit_batch_size,
        concurrency: args.submit_concurrency,
    };
    let pool_wait_options = PoolWaitOptions {
        batch_size: args.pool_poll_batch_size,
        poll_interval_ms: args.pool_poll_interval_ms,
    };
    let http_client = build_http_client(cmp::max(
        submit_options.concurrency,
        pool_wait_options.batch_size,
    ))?;

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
        let txs =
            tx_factory::build_block_txs(&mut senders, workload, args.txs_per_block, args.chain_id)?;

        let expected_tx_count = txs.len() as u64;

        // --- submit to txpool (timed) ---
        let submit_ms =
            match submit_to_txpool_with_client(&http_client, &args.http_rpc, &txs, submit_options)
                .await
            {
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
        let pool_wait_ms = match wait_for_pool_with_client(
            &http_client,
            &args.http_rpc,
            &senders,
            60,
            pool_wait_options,
        )
        .await
        {
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

        let (assembled, assemble_ms, gas_used, actual_tx_count, error) = match client
            .assemble_l2_block(&args.engine_rpc, assemble_params)
            .await
        {
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
            phase: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tx_factory;
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        time::{Duration, sleep},
    };

    async fn spawn_recording_server()
    -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(parts) => parts,
                    Err(_) => break,
                };
                let recorded = Arc::clone(&recorded);

                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    let header_end;

                    loop {
                        let n = socket.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            header_end = idx + 4;
                            break;
                        }
                    }

                    let headers = String::from_utf8_lossy(&buf[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);

                    while buf.len() < header_end + content_length {
                        let n = socket.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }

                    let body =
                        String::from_utf8_lossy(&buf[header_end..header_end + content_length])
                            .to_string();
                    recorded.lock().unwrap().push(body.clone());

                    let response_body = response_for_body(&body);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        response_body.len(),
                        response_body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                });
            }
        });

        (format!("http://{addr}"), requests, handle)
    }

    async fn spawn_keepalive_server() -> (
        String,
        Arc<Mutex<Vec<String>>>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let recorded = Arc::clone(&requests);
        let accepted = Arc::clone(&connections);

        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(parts) => parts,
                    Err(_) => break,
                };
                accepted.fetch_add(1, Ordering::SeqCst);
                let recorded = Arc::clone(&recorded);

                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];

                    loop {
                        let header_end = loop {
                            if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break idx + 4;
                            }

                            let n = socket.read(&mut tmp).await.unwrap();
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        };

                        let headers = String::from_utf8_lossy(&buf[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if name.eq_ignore_ascii_case("content-length") {
                                    value.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);

                        while buf.len() < header_end + content_length {
                            let n = socket.read(&mut tmp).await.unwrap();
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }

                        let body =
                            String::from_utf8_lossy(&buf[header_end..header_end + content_length])
                                .to_string();
                        buf.drain(..header_end + content_length);
                        recorded.lock().unwrap().push(body.clone());

                        let response_body = response_for_body(&body);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });

        (format!("http://{addr}"), requests, connections, handle)
    }

    async fn spawn_delayed_keepalive_server(
        response_delay: Duration,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let inflight = Arc::new(AtomicUsize::new(0));
        let max_inflight = Arc::new(AtomicUsize::new(0));
        let inflight_counter = Arc::clone(&inflight);
        let max_counter = Arc::clone(&max_inflight);

        let handle = tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(parts) => parts,
                    Err(_) => break,
                };
                let inflight_counter = Arc::clone(&inflight_counter);
                let max_counter = Arc::clone(&max_counter);

                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];

                    loop {
                        let header_end = loop {
                            if let Some(idx) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break idx + 4;
                            }

                            let n = socket.read(&mut tmp).await.unwrap();
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        };

                        let headers = String::from_utf8_lossy(&buf[..header_end]);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if name.eq_ignore_ascii_case("content-length") {
                                    value.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);

                        while buf.len() < header_end + content_length {
                            let n = socket.read(&mut tmp).await.unwrap();
                            if n == 0 {
                                return;
                            }
                            buf.extend_from_slice(&tmp[..n]);
                        }

                        let body =
                            String::from_utf8_lossy(&buf[header_end..header_end + content_length])
                                .to_string();
                        buf.drain(..header_end + content_length);

                        let current = inflight_counter.fetch_add(1, Ordering::SeqCst) + 1;
                        max_counter.fetch_max(current, Ordering::SeqCst);
                        sleep(response_delay).await;
                        inflight_counter.fetch_sub(1, Ordering::SeqCst);

                        let response_body = response_for_body(&body);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n{}",
                            response_body.len(),
                            response_body
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                    }
                });
            }
        });

        (format!("http://{addr}"), max_inflight, handle)
    }

    fn response_for_body(body: &str) -> String {
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        match value {
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(|item| {
                        let id = item.get("id").cloned().unwrap_or(serde_json::Value::Null);
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": "0x1"
                        })
                    })
                    .collect(),
            )
            .to_string(),
            serde_json::Value::Object(map) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": map.get("id").cloned().unwrap_or(serde_json::Value::Null),
                "result": "0x1"
            })
            .to_string(),
            _ => panic!("unexpected request body shape"),
        }
    }

    #[tokio::test]
    async fn submit_to_txpool_uses_large_default_batches() {
        let (url, requests, handle) = spawn_recording_server().await;
        let txs: Vec<Bytes> = (0..25).map(|b| Bytes::from(vec![b; 3])).collect();

        submit_to_txpool(&url, &txs, 1).await.unwrap();
        sleep(Duration::from_millis(50)).await;

        let recorded = requests.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "expected one batch request, got {recorded:?}"
        );
        let batch: Vec<serde_json::Value> = serde_json::from_str(&recorded[0]).unwrap();
        assert_eq!(batch.len(), 25);

        handle.abort();
    }

    #[test]
    fn build_submit_bodies_preserves_prefixed_hex_and_ids() {
        let txs = vec![Bytes::from(vec![0x12, 0x34]), Bytes::from(vec![0xab, 0xcd])];
        let bodies = build_submit_bodies(&txs, 1);

        assert_eq!(bodies.len(), 2);

        let first: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
        let second: serde_json::Value = serde_json::from_slice(&bodies[1]).unwrap();

        assert_eq!(first[0]["params"][0], "0x1234");
        assert_eq!(first[0]["id"], 1);
        assert_eq!(second[0]["params"][0], "0xabcd");
        assert_eq!(second[0]["id"], 2);
    }

    #[tokio::test]
    async fn wait_for_pool_batches_default_nonce_checks() {
        let (url, requests, handle) = spawn_recording_server().await;
        let mut senders = tx_factory::generate_senders(3);
        for sender in &mut senders {
            sender.nonce = 1;
        }

        wait_for_pool(&url, &senders, 1).await.unwrap();
        sleep(Duration::from_millis(50)).await;

        let recorded = requests.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            1,
            "expected one nonce poll request, got {recorded:?}"
        );
        let batch: Vec<serde_json::Value> = serde_json::from_str(&recorded[0]).unwrap();
        assert_eq!(batch.len(), 3);

        handle.abort();
    }

    #[test]
    fn submit_batch_response_accepts_success_bytes() {
        let body =
            br#"[{"jsonrpc":"2.0","id":1,"result":"0xabc"},{"jsonrpc":"2.0","id":2,"result":"0xdef"}]"#;
        ensure_submit_batch_success(body).unwrap();
    }

    #[test]
    fn submit_batch_response_reports_batch_error() {
        let body = br#"[{"jsonrpc":"2.0","id":1,"result":"0xabc"},{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"nonce too low"}}]"#;
        let err = ensure_submit_batch_success(body).unwrap_err();
        assert!(err.to_string().contains("nonce too low"));
    }

    #[tokio::test]
    async fn wait_for_pool_checks_multiple_nonce_batches_concurrently() {
        let (url, max_inflight, handle) =
            spawn_delayed_keepalive_server(Duration::from_millis(25)).await;
        let mut senders = tx_factory::generate_senders(4);
        for sender in &mut senders {
            sender.nonce = 1;
        }

        let client = build_http_client(4).unwrap();
        wait_for_pool_with_client(
            &client,
            &url,
            &senders,
            1,
            PoolWaitOptions {
                batch_size: 2,
                poll_interval_ms: 1,
            },
        )
        .await
        .unwrap();

        assert!(
            max_inflight.load(Ordering::SeqCst) >= 2,
            "expected concurrent nonce checks"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn shared_http_client_reuses_connection_across_submit_and_wait() {
        let (url, requests, connections, handle) = spawn_keepalive_server().await;
        let txs: Vec<Bytes> = (0..4).map(|b| Bytes::from(vec![b; 3])).collect();
        let mut senders = tx_factory::generate_senders(2);
        for sender in &mut senders {
            sender.nonce = 1;
        }

        let client = build_http_client(4).unwrap();

        submit_to_txpool_with_client(
            &client,
            &url,
            &txs,
            SubmitOptions {
                batch_size: 2,
                concurrency: 1,
            },
        )
        .await
        .unwrap();
        wait_for_pool_with_client(
            &client,
            &url,
            &senders,
            1,
            PoolWaitOptions {
                batch_size: 2,
                poll_interval_ms: 1,
            },
        )
        .await
        .unwrap();
        sleep(Duration::from_millis(50)).await;

        assert_eq!(connections.load(Ordering::SeqCst), 1);
        assert_eq!(requests.lock().unwrap().len(), 3);

        handle.abort();
    }

    #[tokio::test]
    async fn submit_to_txpool_round_robins_across_targets() {
        let (url_a, requests_a, handle_a) = spawn_recording_server().await;
        let (url_b, requests_b, handle_b) = spawn_recording_server().await;
        let txs: Vec<Bytes> = (0..6).map(|b| Bytes::from(vec![b; 3])).collect();
        let client = build_http_client(4).unwrap();
        let mut next_target_idx = 0usize;

        submit_to_txpool_with_target_cursor(
            &client,
            &[url_a.clone(), url_b.clone()],
            &txs,
            SubmitOptions {
                batch_size: 2,
                concurrency: 4,
            },
            &mut next_target_idx,
        )
        .await
        .unwrap();
        sleep(Duration::from_millis(50)).await;

        assert_eq!(next_target_idx, 1);
        assert_eq!(requests_a.lock().unwrap().len(), 2);
        assert_eq!(requests_b.lock().unwrap().len(), 1);

        submit_to_txpool_with_target_cursor(
            &client,
            &[url_a, url_b],
            &txs,
            SubmitOptions {
                batch_size: 2,
                concurrency: 4,
            },
            &mut next_target_idx,
        )
        .await
        .unwrap();
        sleep(Duration::from_millis(50)).await;

        assert_eq!(next_target_idx, 0);
        assert_eq!(requests_a.lock().unwrap().len(), 3);
        assert_eq!(requests_b.lock().unwrap().len(), 3);

        handle_a.abort();
        handle_b.abort();
    }
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
        phase: None,
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
