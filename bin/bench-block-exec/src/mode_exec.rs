//! Mode A: Pure execution benchmark.
//!
//! Measures EVM execution + state commit performance by pre-loading
//! transactions into the txpool, then timing only the assembleL2Block
//! and newL2Block calls. Txpool submission time is excluded from TPS.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::mode_e2e;
use crate::tx_factory::{self, Workload};

use std::io::Write;

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct ExecArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,

    #[arg(long)]
    pub jwt_secret: String,

    /// HTTP RPC URL for txpool submission.
    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,

    #[arg(long)]
    pub workload: String,

    #[arg(long)]
    pub txs_per_block: u64,

    #[arg(long, default_value = "50")]
    pub blocks: u64,

    #[arg(long)]
    pub output: String,

    #[arg(long, default_value = "unknown")]
    pub engine_name: String,

    #[arg(long, default_value = "99999")]
    pub chain_id: u64,

    /// Number of senders (more senders avoids txpool per-account limits).
    #[arg(long, default_value = "1")]
    pub senders: u64,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: ExecArgs) -> eyre::Result<()> {
    // 1. Parse workload, create client & senders.
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;
    let sender_count = std::cmp::max(args.senders, 1) as usize;
    let mut senders = tx_factory::generate_senders(sender_count);

    // 2. Open output file.
    let mut out_file = std::fs::File::create(&args.output)?;
    let mut consecutive_errors: u64 = 0;

    println!(
        "Mode exec: {} blocks x {} txs ({} workload), txpool path",
        args.blocks, args.txs_per_block, workload,
    );

    // 3. Loop through blocks.
    for i in 0..args.blocks {
        let block_number = i + 1;
        let expected_tx_count = args.txs_per_block;

        // Build transactions for this block.
        let txs = tx_factory::build_block_txs(
            &mut senders,
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;

        // Submit to txpool in waves (NOT timed — we only care about execution).
        // Fire all waves as fast as possible, only wait for pool at the end.
        let concurrency = std::cmp::min(sender_count, 16);
        const WAVE_SIZE: usize = 10_000;
        let num_waves = (txs.len() + WAVE_SIZE - 1) / WAVE_SIZE;
        for (wi, wave) in txs.chunks(WAVE_SIZE).enumerate() {
            eprint!("\r  submitting wave {}/{} ({} txs)...", wi + 1, num_waves, wave.len());
            mode_e2e::submit_to_txpool(&args.http_rpc, wave, concurrency).await?;
        }
        eprintln!("\r  waiting for pool to accept all {} txs...", txs.len());
        mode_e2e::wait_for_pool(&args.http_rpc, &senders, 600).await?;

        // --- assemble (TIMED) ---
        let params = AssembleL2BlockParams {
            number: block_number,
            transactions: vec![], // empty — pull from txpool
            timestamp: Some(block_number),
        };

        let assemble_result = client.assemble_l2_block(&args.engine_rpc, params).await;

        let (assembled, assemble_ms, gas_used, actual_tx_count, error) = match assemble_result {
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

        // --- import (TIMED) ---
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

        // --- record timing (only assemble + import, no submit/pool_wait) ---
        let mut timing = BlockTimingV2 {
            block_number,
            tx_count: actual_tx_count,
            expected_tx_count,
            engine: args.engine_name.clone(),
            mode: "exec".to_string(),
            workload: workload.to_string(),
            senders: 1,
            warmup_blocks: 0,
            submit_ms: 0.0,
            pool_wait_ms: 0.0,
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
                "block {block_number}/{}: asm={:.1}ms imp={:.1}ms total={:.1}ms | {:.0} TPS, {:.0} MGas/s | {}/{}",
                args.blocks,
                timing.assemble_ms,
                timing.import_ms,
                timing.total_ms,
                timing.tps,
                timing.mgas_per_sec,
                timing.tx_count,
                timing.expected_tx_count,
            );
        }

        // --- bail on repeated failures ---
        if consecutive_errors >= 5 {
            eprintln!("5 consecutive errors — aborting.");
            break;
        }
    }

    println!("Results written to {}", args.output);
    Ok(())
}
