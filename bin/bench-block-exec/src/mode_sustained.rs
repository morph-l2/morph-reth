//! Mode C: Sustained block production benchmark.
//!
//! Two-phase flow:
//! - Phase 1 (warmup): Produce `warmup_blocks` blocks via txpool without timing.
//! - Phase 2 (measurement): Produce `blocks` blocks with full timing and rolling TPS.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::mode_e2e::{submit_to_txpool, wait_for_pool};
use crate::tx_factory::{self, Workload};

use std::collections::VecDeque;
use std::io::Write;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct SustainedArgs {
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

    #[arg(long, default_value = "1000")]
    pub blocks: u64,

    #[arg(long, default_value = "0")]
    pub warmup_blocks: u64,

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
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: SustainedArgs) -> eyre::Result<()> {
    let workload: Workload = args.workload.parse()?;
    let client = EngineClient::new(&args.engine_rpc, args.jwt_secret.clone())?;
    let mut senders = tx_factory::generate_senders(args.senders as usize);

    println!(
        "Mode C (sustained): {} warmup + {} measured blocks x {} txs/block, {} senders, {} workload",
        args.warmup_blocks, args.blocks, args.txs_per_block, args.senders, workload
    );

    // -----------------------------------------------------------------------
    // Phase 1: Warmup
    // -----------------------------------------------------------------------

    if args.warmup_blocks > 0 {
        println!("--- Phase 1: warmup ({} blocks) ---", args.warmup_blocks);
    }

    for block_idx in 0..args.warmup_blocks {
        let block_number = block_idx + 1;

        // Build transactions.
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;

        // Submit to txpool.
        submit_to_txpool(&args.http_rpc, &txs, 4).await?;

        // Wait for pool acceptance.
        wait_for_pool(&args.http_rpc, &senders, 60).await?;

        // Assemble block (pulls from pool).
        let assemble_params = AssembleL2BlockParams {
            number: block_number,
            transactions: vec![],
            timestamp: Some(block_number),
        };

        let (assembled, _assemble_ms) = client
            .assemble_l2_block(&args.engine_rpc, assemble_params)
            .await?;

        // Import block.
        client.new_l2_block(&args.engine_rpc, assembled).await?;

        // Progress every 50 blocks.
        if block_number % 50 == 0 || block_number == args.warmup_blocks {
            println!("warmup: {block_number}/{}", args.warmup_blocks);
        }
    }

    // -----------------------------------------------------------------------
    // Phase 2: Measurement
    // -----------------------------------------------------------------------

    println!("--- Phase 2: measurement ({} blocks) ---", args.blocks);

    let mut out_file = std::fs::File::create(&args.output)?;
    let mut consecutive_errors: u64 = 0;
    let mut cumulative_txs: u64 = 0;
    let mut rolling_tps: VecDeque<f64> = VecDeque::with_capacity(100);

    for block_idx in 0..args.blocks {
        let block_number = args.warmup_blocks + block_idx + 1;
        let measured_block = block_idx + 1;

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

        // --- update cumulative counters ---
        cumulative_txs += actual_tx_count;

        // --- record timing ---
        let mut timing = BlockTimingV2 {
            block_number,
            tx_count: actual_tx_count,
            expected_tx_count,
            engine: args.engine_name.clone(),
            mode: "sustained".to_string(),
            workload: workload.to_string(),
            senders: args.senders,
            warmup_blocks: args.warmup_blocks,
            submit_ms,
            pool_wait_ms,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: measured_block,
            cumulative_txs,
            rolling_avg_tps_100: None,
            error,
        };
        timing.finalize();

        // --- rolling TPS ---
        rolling_tps.push_back(timing.tps);
        if rolling_tps.len() > 100 {
            rolling_tps.pop_front();
        }
        if rolling_tps.len() >= 100 {
            let sum: f64 = rolling_tps.iter().sum();
            timing.rolling_avg_tps_100 = Some(sum / rolling_tps.len() as f64);
        }

        // --- write JSON line ---
        let line = serde_json::to_string(&timing)?;
        writeln!(out_file, "{line}")?;

        // --- progress every 100 measured blocks ---
        if measured_block % 100 == 0 || measured_block == args.blocks {
            let rolling_str = timing
                .rolling_avg_tps_100
                .map(|v| format!(" rolling_tps_100={v:.0}"))
                .unwrap_or_default();
            println!(
                "block {measured_block}/{}: submit={:.1}ms pool={:.1}ms assemble={:.1}ms import={:.1}ms total={:.1}ms tps={:.0} included={}/{}{}",
                args.blocks,
                timing.submit_ms,
                timing.pool_wait_ms,
                timing.assemble_ms,
                timing.import_ms,
                timing.total_ms,
                timing.tps,
                timing.tx_count,
                timing.expected_tx_count,
                rolling_str,
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
