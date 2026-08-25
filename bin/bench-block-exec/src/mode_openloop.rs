//! Tempo-inspired open-loop benchmark mode.
//!
//! Unlike the block-closed-loop modes, this mode keeps two pipelines active:
//! one continuously submits transactions at a target TPS, and the other
//! continuously assembles/imports blocks from the txpool.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::mode_e2e::{
    DEFAULT_SUBMIT_BATCH_SIZE, DEFAULT_SUBMIT_CONCURRENCY, SubmitOptions, build_http_client,
    build_submit_bodies, ensure_submit_batch_success,
};
use crate::tx_factory::{self, BenchSender, Workload};

use alloy_primitives::Bytes;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

pub(crate) const DEFAULT_OPEN_LOOP_SUBMIT_TICK_MS: u64 = 100;
pub(crate) const DEFAULT_OPEN_LOOP_PRODUCER_IDLE_MS: u64 = 10;
pub(crate) const DEFAULT_OPEN_LOOP_DRAIN_SECS: u64 = 10;
pub(crate) const DEFAULT_OPEN_LOOP_SUBMIT_BUFFER_TICKS: usize = 32;

#[derive(clap::Args)]
pub struct OpenLoopArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,

    #[arg(long)]
    pub jwt_secret: String,

    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,

    #[arg(long = "submit-http-rpc", value_delimiter = ',')]
    pub submit_http_rpcs: Vec<String>,

    #[arg(long)]
    pub workload: String,

    #[arg(long)]
    pub target_tps: u64,

    #[arg(long, default_value = "30")]
    pub duration_secs: u64,

    #[arg(long, default_value = "100")]
    pub senders: u64,

    #[arg(long)]
    pub output: String,

    #[arg(long, default_value = "99999")]
    pub chain_id: u64,

    #[arg(long, default_value_t = DEFAULT_SUBMIT_BATCH_SIZE)]
    pub submit_batch_size: usize,

    #[arg(long, default_value_t = DEFAULT_SUBMIT_CONCURRENCY)]
    pub submit_concurrency: usize,

    #[arg(long, default_value_t = DEFAULT_OPEN_LOOP_SUBMIT_TICK_MS)]
    pub submit_tick_ms: u64,

    #[arg(long, default_value_t = DEFAULT_OPEN_LOOP_PRODUCER_IDLE_MS)]
    pub producer_idle_ms: u64,

    #[arg(long, default_value_t = DEFAULT_OPEN_LOOP_DRAIN_SECS)]
    pub drain_secs: u64,

    #[arg(long, default_value_t = DEFAULT_OPEN_LOOP_SUBMIT_BUFFER_TICKS)]
    pub submit_buffer_ticks: usize,
}

#[derive(Debug, Default, Clone, Copy)]
struct SubmitSummary {
    submitted_txs: u64,
    http_calls: u64,
}

#[derive(Debug, Default, Clone, Copy)]
struct ProducerSummary {
    imported_blocks: u64,
    imported_txs: u64,
}

#[derive(Debug)]
struct PrebuiltSubmitBatch {
    tx_count: u64,
    bodies: Vec<Vec<u8>>,
}

struct SubmitLoopConfig {
    duration_secs: u64,
    submit_tick_ms: u64,
    http_client: reqwest::Client,
    submit_targets: Vec<String>,
    submit_options: SubmitOptions,
    sender_finished: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    submitted_txs: Arc<AtomicU64>,
}

struct ProducerLoopConfig {
    client: EngineClient,
    engine_rpc: String,
    http_rpc: String,
    pool_client: reqwest::Client,
    workload: Workload,
    target_tps: u64,
    senders: u64,
    producer_idle_ms: u64,
    drain_secs: u64,
    sender_finished: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    submitted_txs: Arc<AtomicU64>,
}

pub async fn run(args: OpenLoopArgs) -> eyre::Result<()> {
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;
    let http_client = build_http_client(args.submit_concurrency)?;
    let submit_options = SubmitOptions {
        batch_size: args.submit_batch_size,
        concurrency: args.submit_concurrency,
    };
    let sender_finished = Arc::new(AtomicBool::new(false));
    let abort = Arc::new(AtomicBool::new(false));
    let submitted_txs = Arc::new(AtomicU64::new(0));
    let submit_targets = resolve_submit_targets(&args.http_rpc, &args.submit_http_rpcs);
    let tick_plan = build_tick_plan(args.target_tps, args.duration_secs, args.submit_tick_ms);
    let total_txs: u64 = tick_plan.iter().sum();
    let num_ticks = tick_plan.len();

    // ── Pre-generate ALL transactions and serialize to JSON-RPC bodies ──
    // This moves ECDSA signing out of the hot submit loop so the submit
    // cadence is limited only by HTTP throughput, not by signing speed.
    println!(
        "Mode D (openloop): target={} TPS, duration={}s, {} senders, {} workload, {} submit target(s)",
        args.target_tps,
        args.duration_secs,
        args.senders,
        workload,
        submit_targets.len(),
    );
    println!("  Pre-generating {total_txs} txs across {num_ticks} ticks...");

    let batch_size = args.submit_batch_size;
    let senders_count = args.senders as usize;
    let chain_id = args.chain_id;
    let pregen_start = Instant::now();

    let prebuilt_ticks: Vec<PrebuiltSubmitBatch> = tokio::task::spawn_blocking(move || {
        let mut senders = tx_factory::generate_senders(senders_count);
        let mut next_sender_idx = 0usize;
        let mut ticks = Vec::with_capacity(num_ticks);

        for total in tick_plan {
            let txs = build_tick_txs(
                &mut senders,
                workload,
                total,
                chain_id,
                &mut next_sender_idx,
            )
            .expect("tx generation should not fail");
            let bodies = build_submit_bodies(&txs, batch_size);
            ticks.push(PrebuiltSubmitBatch {
                tx_count: txs.len() as u64,
                bodies,
            });
        }
        ticks
    })
    .await
    .map_err(|e| eyre::eyre!("pre-generation failed: {e}"))?;

    let pregen_ms = pregen_start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "  Pre-generated {total_txs} txs in {pregen_ms:.0}ms ({:.0} txs/s)",
        total_txs as f64 / (pregen_ms / 1000.0)
    );

    // Feed pre-built ticks into a channel for the submit loop.
    let (tick_tx, tick_batches) = mpsc::channel(args.submit_buffer_ticks.max(1));
    tokio::spawn(async move {
        for batch in prebuilt_ticks {
            if tick_tx.send(Ok(batch)).await.is_err() {
                break;
            }
        }
    });

    let mut out_file = std::fs::File::create(&args.output)?;
    let pool_client = build_http_client(1)?;

    let submit_future = drive_submit_loop(
        tick_batches,
        SubmitLoopConfig {
            duration_secs: args.duration_secs,
            submit_tick_ms: args.submit_tick_ms,
            http_client: http_client.clone(),
            submit_targets,
            submit_options,
            sender_finished: Arc::clone(&sender_finished),
            abort: Arc::clone(&abort),
            submitted_txs: Arc::clone(&submitted_txs),
        },
    );
    let produce_future = drive_producer_loop(
        &mut out_file,
        ProducerLoopConfig {
            client,
            engine_rpc: args.engine_rpc.clone(),
            http_rpc: args.http_rpc.clone(),
            pool_client,
            workload,
            target_tps: args.target_tps,
            senders: args.senders,
            producer_idle_ms: args.producer_idle_ms,
            drain_secs: args.drain_secs,
            sender_finished: Arc::clone(&sender_finished),
            abort: Arc::clone(&abort),
            submitted_txs: Arc::clone(&submitted_txs),
        },
    );

    let (submit_res, produce_res) = tokio::join!(submit_future, produce_future);
    abort.store(true, Ordering::SeqCst);
    sender_finished.store(true, Ordering::SeqCst);

    let submit_summary = submit_res?;
    let produce_summary = produce_res?;

    eyre::ensure!(
        produce_summary.imported_blocks > 0,
        "openloop mode did not import any non-empty blocks"
    );

    println!(
        "Openloop complete: submitted={} txs over {} HTTP calls, imported={} blocks / {} txs",
        submit_summary.submitted_txs,
        submit_summary.http_calls,
        produce_summary.imported_blocks,
        produce_summary.imported_txs,
    );
    println!("Results written to {}", args.output);
    Ok(())
}

/// Submit one planned tick at a time with bounded HTTP concurrency.
///
/// Waiting for each tick prevents a nominal load target from creating an
/// unbounded queue of client-side requests after the measurement deadline.
/// When the RPC path cannot sustain the target, fewer ticks are accepted and
/// the completion line reports the actual submitted transaction count.
async fn drive_submit_loop(
    mut tick_batches: mpsc::Receiver<eyre::Result<PrebuiltSubmitBatch>>,
    config: SubmitLoopConfig,
) -> eyre::Result<SubmitSummary> {
    let SubmitLoopConfig {
        duration_secs,
        submit_tick_ms,
        http_client,
        submit_targets,
        submit_options,
        sender_finished,
        abort,
        submitted_txs,
    } = config;
    let tick = Duration::from_millis(submit_tick_ms.max(1));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(duration_secs.max(1));
    let mut next_tick = tokio::time::Instant::now();
    let mut next_target_idx = 0usize;
    let mut summary = SubmitSummary::default();
    let options = submit_options.sanitized();

    let result: eyre::Result<SubmitSummary> = async {
        loop {
            if abort.load(Ordering::SeqCst) {
                break;
            }

            if tokio::time::Instant::now() >= deadline {
                break;
            }

            let batch = match tick_batches.recv().await {
                Some(result) => result?,
                None => break,
            };

            if batch.tx_count > 0 {
                let body_count = submit_prebuilt_batch(
                    &http_client,
                    &submit_targets,
                    batch.bodies,
                    options.concurrency,
                    &mut next_target_idx,
                )
                .await?;

                summary.submitted_txs += batch.tx_count;
                summary.http_calls += body_count as u64;
                submitted_txs.store(summary.submitted_txs, Ordering::SeqCst);
            }

            next_tick += tick;
            let now = tokio::time::Instant::now();
            if next_tick > now {
                tokio::time::sleep_until(next_tick).await;
            }
        }

        Ok(summary)
    }
    .await;

    sender_finished.store(true, Ordering::SeqCst);
    if result.is_err() {
        abort.store(true, Ordering::SeqCst);
    }
    result
}

async fn submit_prebuilt_batch(
    client: &reqwest::Client,
    submit_targets: &[String],
    bodies: Vec<Vec<u8>>,
    concurrency: usize,
    next_target_idx: &mut usize,
) -> eyre::Result<usize> {
    let target_count = submit_targets.len().max(1);
    let body_count = bodies.len();
    let mut inflight: tokio::task::JoinSet<eyre::Result<()>> = tokio::task::JoinSet::new();

    for (offset, body) in bodies.into_iter().enumerate() {
        while inflight.len() >= concurrency.max(1) {
            let result = inflight
                .join_next()
                .await
                .ok_or_else(|| eyre::eyre!("submit task set unexpectedly empty"))?;
            result.map_err(|e| eyre::eyre!("join: {e}"))??;
        }

        let client = client.clone();
        let url = submit_targets[(*next_target_idx + offset) % target_count].clone();
        inflight.spawn(async move {
            let resp = client
                .post(&url)
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|e| eyre::eyre!("send failed: {e}"))?
                .error_for_status()
                .map_err(|e| eyre::eyre!("http error: {e}"))?;
            let body = resp
                .bytes()
                .await
                .map_err(|e| eyre::eyre!("read failed: {e}"))?;
            ensure_submit_batch_success(body.as_ref())
        });
    }

    while let Some(result) = inflight.join_next().await {
        result.map_err(|e| eyre::eyre!("join: {e}"))??;
    }

    *next_target_idx = (*next_target_idx + body_count.max(1)) % target_count;
    Ok(body_count)
}

async fn drive_producer_loop(
    out_file: &mut std::fs::File,
    config: ProducerLoopConfig,
) -> eyre::Result<ProducerSummary> {
    let ProducerLoopConfig {
        client,
        engine_rpc,
        http_rpc,
        pool_client,
        workload,
        target_tps,
        senders,
        producer_idle_ms,
        drain_secs,
        sender_finished,
        abort,
        submitted_txs,
    } = config;
    let idle = Duration::from_millis(producer_idle_ms.max(1));
    let max_drain = Duration::from_secs(drain_secs.max(1));
    let mut next_block_number = 1u64;
    let mut imported_blocks = 0u64;
    let mut cumulative_txs = 0u64;
    let mut rolling_tps = VecDeque::with_capacity(100);
    let mut last_completed_at = Instant::now();
    let mut sender_finished_at: Option<Instant> = None;
    let mut consecutive_errors = 0u64;

    loop {
        if abort.load(Ordering::SeqCst) {
            break;
        }

        if sender_finished.load(Ordering::SeqCst) && sender_finished_at.is_none() {
            sender_finished_at = Some(Instant::now());
        }

        let assemble_params = AssembleL2BlockParams {
            number: next_block_number,
            transactions: vec![],
            timestamp: Some(next_block_number),
        };

        let (assembled, assemble_ms) =
            match client.assemble_l2_block(&engine_rpc, assemble_params).await {
                Ok(result) => result,
                Err(e) => {
                    consecutive_errors += 1;
                    eprintln!("openloop block {next_block_number}: assemble error: {e}");
                    if consecutive_errors >= 5 {
                        return Err(eyre::eyre!(
                            "openloop aborted after 5 consecutive assemble/import errors"
                        ));
                    }
                    tokio::time::sleep(idle).await;
                    continue;
                }
            };

        let actual_tx_count = assembled.transactions.len() as u64;
        let gas_used = assembled.gas_used;
        let import_ms = match client.new_l2_block(&engine_rpc, assembled).await {
            Ok(ms) => ms,
            Err(e) => {
                consecutive_errors += 1;
                eprintln!("openloop block {next_block_number}: import error: {e}");
                if consecutive_errors >= 5 {
                    return Err(eyre::eyre!(
                        "openloop aborted after 5 consecutive assemble/import errors"
                    ));
                }
                tokio::time::sleep(idle).await;
                continue;
            }
        };

        consecutive_errors = 0;
        imported_blocks += 1;
        cumulative_txs += actual_tx_count;

        let completed_at = Instant::now();
        let observed_interval_ms =
            completed_at.duration_since(last_completed_at).as_secs_f64() * 1000.0;
        last_completed_at = completed_at;

        let mut timing = BlockTimingV2 {
            block_number: next_block_number,
            tx_count: actual_tx_count,
            expected_tx_count: 0,
            target_tps: Some(target_tps),
            engine: "reth".to_string(),
            mode: "openloop".to_string(),
            workload: workload.to_string(),
            senders,
            warmup_blocks: 0,
            phase: Some(
                if sender_finished.load(Ordering::SeqCst) || sender_finished_at.is_some() {
                    "drain"
                } else {
                    "active"
                }
                .to_string(),
            ),
            submit_ms: 0.0,
            pool_wait_ms: 0.0,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: imported_blocks,
            cumulative_txs,
            rolling_avg_tps_100: None,
            error: false,
        };
        timing.finalize();
        apply_observed_interval(
            &mut timing,
            observed_interval_ms.max(assemble_ms + import_ms),
        );

        rolling_tps.push_back(timing.tps);
        if rolling_tps.len() > 100 {
            rolling_tps.pop_front();
        }
        if !rolling_tps.is_empty() {
            let sum: f64 = rolling_tps.iter().sum();
            timing.rolling_avg_tps_100 = Some(sum / rolling_tps.len() as f64);
        }

        let line = serde_json::to_string(&timing)?;
        writeln!(out_file, "{line}")?;

        if imported_blocks.is_multiple_of(10) {
            println!(
                "openloop block {imported_blocks}: total={:.1}ms assemble={:.1}ms import={:.1}ms tps={:.0} txs={}",
                timing.total_ms, timing.assemble_ms, timing.import_ms, timing.tps, timing.tx_count,
            );
        }

        if let Some(finished_at) = sender_finished_at {
            let (pending_txs, queued_txs) = txpool_status(&pool_client, &http_rpc).await?;
            let submitted = submitted_txs.load(Ordering::SeqCst);
            if drain_complete(submitted, cumulative_txs, pending_txs, queued_txs) {
                break;
            }
            if finished_at.elapsed() >= max_drain {
                return Err(eyre::eyre!(
                    "openloop drain timed out: submitted={submitted} imported={cumulative_txs} pending={pending_txs} queued={queued_txs}"
                ));
            }
        }

        next_block_number += 1;

        if actual_tx_count == 0 {
            tokio::time::sleep(idle).await;
        }
    }

    Ok(ProducerSummary {
        imported_blocks,
        imported_txs: cumulative_txs,
    })
}

fn txs_for_window(target_tps: u64, window_secs: f64, carry: &mut f64) -> u64 {
    if target_tps == 0 || window_secs <= 0.0 {
        return 0;
    }

    let budget = *carry + target_tps as f64 * window_secs;
    let txs = budget.floor() as u64;
    *carry = budget - txs as f64;
    txs
}

fn build_tick_plan(target_tps: u64, duration_secs: u64, submit_tick_ms: u64) -> Vec<u64> {
    let tick = Duration::from_millis(submit_tick_ms.max(1));
    let total = Duration::from_secs(duration_secs.max(1));
    let mut remaining = total;
    let mut carry = 0.0;
    let mut plan = Vec::new();

    while !remaining.is_zero() {
        let window = remaining.min(tick);
        plan.push(txs_for_window(target_tps, window.as_secs_f64(), &mut carry));
        remaining = remaining.saturating_sub(window);
    }

    plan
}

fn resolve_submit_targets(http_rpc: &str, submit_http_rpcs: &[String]) -> Vec<String> {
    let targets: Vec<String> = submit_http_rpcs
        .iter()
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    if targets.is_empty() {
        vec![http_rpc.to_string()]
    } else {
        targets
    }
}

fn build_tick_txs(
    senders: &mut [BenchSender],
    workload: Workload,
    total_txs: u64,
    chain_id: u64,
    next_sender_idx: &mut usize,
) -> eyre::Result<Vec<Bytes>> {
    if senders.is_empty() || total_txs == 0 {
        return Ok(Vec::new());
    }

    let rotate_by = *next_sender_idx % senders.len();
    if rotate_by > 0 {
        senders.rotate_left(rotate_by);
    }

    let txs = tx_factory::build_block_txs(senders, workload, total_txs, chain_id)?;

    if rotate_by > 0 {
        senders.rotate_right(rotate_by);
    }

    *next_sender_idx = (*next_sender_idx + total_txs as usize) % senders.len();
    Ok(txs)
}

fn apply_observed_interval(timing: &mut BlockTimingV2, observed_interval_ms: f64) {
    timing.total_ms = observed_interval_ms;
    if timing.total_ms > 0.0 {
        let secs = timing.total_ms / 1000.0;
        timing.tps = timing.tx_count as f64 / secs;
        timing.mgas_per_sec = timing.gas_used as f64 / secs / 1_000_000.0;
    } else {
        timing.tps = 0.0;
        timing.mgas_per_sec = 0.0;
    }
    timing.inclusion_rate = 1.0;
}

async fn txpool_status(client: &reqwest::Client, http_rpc: &str) -> eyre::Result<(u64, u64)> {
    let resp = client
        .post(http_rpc)
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": "txpool_status",
            "params": [],
            "id": 1
        }))
        .send()
        .await?
        .error_for_status()?;

    let value: serde_json::Value = resp.json().await?;
    let result = value
        .get("result")
        .ok_or_else(|| eyre::eyre!("txpool_status missing result"))?;

    let pending = parse_hex_quantity(result.get("pending"))
        .ok_or_else(|| eyre::eyre!("txpool_status missing pending"))?;
    let queued = parse_hex_quantity(result.get("queued"))
        .ok_or_else(|| eyre::eyre!("txpool_status missing queued"))?;

    Ok((pending, queued))
}

fn parse_hex_quantity(value: Option<&serde_json::Value>) -> Option<u64> {
    value
        .and_then(|value| value.as_str())
        .and_then(|raw| u64::from_str_radix(raw.trim_start_matches("0x"), 16).ok())
}

fn drain_complete(
    submitted_txs: u64,
    imported_txs: u64,
    pending_txs: u64,
    queued_txs: u64,
) -> bool {
    imported_txs >= submitted_txs && pending_txs == 0 && queued_txs == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_timing() -> BlockTimingV2 {
        BlockTimingV2 {
            block_number: 1,
            tx_count: 100,
            expected_tx_count: 0,
            target_tps: Some(20_000),
            engine: "reth".to_string(),
            mode: "openloop".to_string(),
            workload: "eth-transfer".to_string(),
            senders: 100,
            warmup_blocks: 0,
            phase: Some("active".to_string()),
            submit_ms: 0.0,
            pool_wait_ms: 0.0,
            assemble_ms: 5.0,
            import_ms: 5.0,
            total_ms: 0.0,
            gas_used: 2_100_000,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: 1,
            cumulative_txs: 100,
            rolling_avg_tps_100: None,
            error: false,
        }
    }

    #[test]
    fn txs_for_window_preserves_fractional_budget() {
        let mut carry = 0.0;
        let mut total = 0;
        for _ in 0..10 {
            total += txs_for_window(25_001, 0.1, &mut carry);
        }

        assert_eq!(total, 25_001);
        assert!(carry < 1.0);
    }

    #[test]
    fn apply_observed_interval_overrides_total_and_tps() {
        let mut timing = sample_timing();
        timing.finalize();

        apply_observed_interval(&mut timing, 50.0);

        assert_eq!(timing.total_ms, 50.0);
        assert_eq!(timing.tps, 2_000.0);
        assert_eq!(timing.mgas_per_sec, 42.0);
        assert_eq!(timing.inclusion_rate, 1.0);
    }

    #[test]
    fn drain_complete_requires_pool_to_be_empty() {
        assert!(!drain_complete(5_995, 200, 5_795, 0));
        assert!(drain_complete(5_995, 5_995, 0, 0));
    }

    #[test]
    fn build_tick_txs_rotates_across_senders() {
        let mut senders = tx_factory::generate_senders(5);
        let mut next_sender_idx = 0;

        build_tick_txs(
            &mut senders,
            Workload::EthTransfer,
            2,
            99999,
            &mut next_sender_idx,
        )
        .unwrap();
        build_tick_txs(
            &mut senders,
            Workload::EthTransfer,
            2,
            99999,
            &mut next_sender_idx,
        )
        .unwrap();
        build_tick_txs(
            &mut senders,
            Workload::EthTransfer,
            2,
            99999,
            &mut next_sender_idx,
        )
        .unwrap();

        let nonces: Vec<u64> = senders.iter().map(|sender| sender.nonce).collect();
        assert_eq!(nonces, vec![2, 1, 1, 1, 1]);
    }

    #[test]
    fn resolve_submit_targets_defaults_to_http_rpc() {
        let targets = resolve_submit_targets("http://127.0.0.1:8545", &[]);
        assert_eq!(targets, vec!["http://127.0.0.1:8545".to_string()]);
    }

    #[test]
    fn resolve_submit_targets_accepts_csv_and_trims() {
        let targets = resolve_submit_targets(
            "http://127.0.0.1:8545",
            &[String::from(
                " http://a:8545, http://b:8545 ,,http://c:8545 ",
            )],
        );
        assert_eq!(
            targets,
            vec![
                "http://a:8545".to_string(),
                "http://b:8545".to_string(),
                "http://c:8545".to_string(),
            ]
        );
    }
}
