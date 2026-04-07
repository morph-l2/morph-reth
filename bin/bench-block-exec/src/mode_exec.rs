//! Mode A: Pure execution benchmark.
//!
//! Bypasses the txpool entirely and injects transactions directly via
//! `engine_assembleL2Block`. This isolates block-building + import cost
//! from transaction propagation / pool overhead.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
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
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: ExecArgs) -> eyre::Result<()> {
    // 1. Parse workload, create client & single sender.
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;
    let mut senders = tx_factory::generate_senders(1);

    // 2. Pre-generate ALL transactions for ALL blocks upfront (no allocation
    //    during timing).
    println!(
        "Pre-generating {} blocks x {} txs ({} workload) ...",
        args.blocks, args.txs_per_block, workload
    );
    let mut all_block_txs: Vec<Vec<alloy_primitives::Bytes>> =
        Vec::with_capacity(args.blocks as usize);
    for _ in 0..args.blocks {
        let txs =
            tx_factory::build_block_txs(&mut senders, workload, args.txs_per_block, args.chain_id)?;
        all_block_txs.push(txs);
    }
    println!("Pre-generation complete.");

    // 3. Open output file.
    let mut out_file = std::fs::File::create(&args.output)?;
    let mut consecutive_errors: u64 = 0;

    // 4. Loop through blocks.
    for (i, txs) in all_block_txs.into_iter().enumerate() {
        let block_number = (i as u64) + 1;

        let params = AssembleL2BlockParams {
            number: block_number,
            transactions: txs,
            timestamp: Some(block_number),
        };

        let expected_tx_count = args.txs_per_block;

        // --- assemble ---
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

        // --- import ---
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
            cumulative_txs: 0, // not tracked in exec mode
            rolling_avg_tps_100: None,
            error,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(out_file, "{line}")?;

        // --- progress ---
        if block_number % 10 == 0 || block_number == args.blocks {
            println!(
                "block {block_number}/{}: assemble={:.1}ms import={:.1}ms total={:.1}ms tps={:.0} included={}/{}",
                args.blocks,
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
            eprintln!("5 consecutive errors — aborting.");
            break;
        }
    }

    println!("Results written to {}", args.output);
    Ok(())
}
