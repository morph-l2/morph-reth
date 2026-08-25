//! Mode A: Pure execution benchmark.
//!
//! Measures EVM execution + state commit performance by pre-loading
//! transactions into the txpool, then timing only the assembleL2Block
//! and newL2Block calls. Txpool submission time is excluded from TPS.

use crate::engine::{AssembleL2BlockParams, BlockTimingV2, EngineClient};
use crate::mode_e2e;
use crate::tx_factory::{self, BenchSender, Workload};

use std::cmp;
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

    #[arg(long, default_value = "99999")]
    pub chain_id: u64,

    /// Number of senders (more senders avoids txpool per-account limits).
    #[arg(long, default_value = "1")]
    pub senders: u64,

    #[arg(long, default_value_t = mode_e2e::DEFAULT_SUBMIT_BATCH_SIZE)]
    pub submit_batch_size: usize,

    #[arg(long, default_value_t = mode_e2e::DEFAULT_SUBMIT_CONCURRENCY)]
    pub submit_concurrency: usize,

    #[arg(long, default_value_t = mode_e2e::DEFAULT_POOL_POLL_BATCH_SIZE)]
    pub pool_poll_batch_size: usize,

    #[arg(long, default_value_t = mode_e2e::DEFAULT_POOL_POLL_INTERVAL_MS)]
    pub pool_poll_interval_ms: u64,
}

pub(crate) struct ExecRunState {
    senders: Vec<BenchSender>,
    next_block_number: u64,
    cumulative_txs: u64,
}

impl ExecRunState {
    pub(crate) fn new(sender_count: usize) -> Self {
        Self {
            senders: tx_factory::generate_senders(sender_count.max(1)),
            next_block_number: 1,
            cumulative_txs: 0,
        }
    }

    pub(crate) fn senders(&self) -> &[BenchSender] {
        &self.senders
    }

    pub(crate) fn senders_mut(&mut self) -> &mut [BenchSender] {
        &mut self.senders
    }

    pub(crate) fn current_block_number(&self) -> u64 {
        self.next_block_number
    }

    pub(crate) fn advance_block_number(&mut self) {
        self.next_block_number += 1;
    }

    pub(crate) fn record_transactions(&mut self, count: u64) {
        self.cumulative_txs += count;
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: ExecArgs) -> eyre::Result<()> {
    let sender_count = std::cmp::max(args.senders, 1) as usize;
    let mut state = ExecRunState::new(sender_count);
    run_with_state(&args, &mut state).await
}

pub(crate) async fn run_with_state(args: &ExecArgs, state: &mut ExecRunState) -> eyre::Result<()> {
    // 1. Parse workload, create client & senders.
    let workload: Workload = args.workload.parse()?;
    let jwt_hex = std::fs::read_to_string(&args.jwt_secret)
        .map_err(|e| eyre::eyre!("failed to read JWT secret file: {e}"))?
        .trim()
        .to_string();
    let client = EngineClient::new(&args.engine_rpc, jwt_hex)?;
    let submit_options = mode_e2e::SubmitOptions {
        batch_size: args.submit_batch_size,
        concurrency: args.submit_concurrency,
    };
    let pool_wait_options = mode_e2e::PoolWaitOptions {
        batch_size: args.pool_poll_batch_size,
        poll_interval_ms: args.pool_poll_interval_ms,
    };
    let http_client = mode_e2e::build_http_client(cmp::max(
        submit_options.concurrency,
        pool_wait_options.batch_size,
    ))?;

    // 2. Open output file.
    let mut out_file = std::fs::File::create(&args.output)?;

    println!(
        "Mode exec: {} blocks x {} txs ({} workload), txpool path",
        args.blocks, args.txs_per_block, workload,
    );

    // 3. Loop through blocks.
    for i in 0..args.blocks {
        let display_block_number = i + 1;
        let engine_block_number = state.current_block_number();
        let expected_tx_count = args.txs_per_block;

        // Build transactions for this block.
        let txs = tx_factory::build_block_txs(
            state.senders_mut(),
            workload,
            args.txs_per_block,
            args.chain_id,
        )?;

        // Submit to txpool in waves (NOT timed — we only care about execution).
        // Fire all waves as fast as possible, only wait for pool at the end.
        const WAVE_SIZE: usize = 10_000;
        let num_waves = txs.len().div_ceil(WAVE_SIZE);
        for (wi, wave) in txs.chunks(WAVE_SIZE).enumerate() {
            eprint!(
                "\r  submitting wave {}/{} ({} txs)...",
                wi + 1,
                num_waves,
                wave.len()
            );
            mode_e2e::submit_to_txpool_with_client(
                &http_client,
                &args.http_rpc,
                wave,
                submit_options,
            )
            .await?;
        }
        eprintln!("\r  waiting for pool to accept all {} txs...", txs.len());
        mode_e2e::wait_for_pool_with_client(
            &http_client,
            &args.http_rpc,
            state.senders(),
            600,
            pool_wait_options,
        )
        .await?;

        // --- assemble (TIMED) ---
        let params = AssembleL2BlockParams {
            number: engine_block_number,
            transactions: vec![], // empty — pull from txpool
            timestamp: Some(engine_block_number),
        };

        let assemble_result = client.assemble_l2_block(&args.engine_rpc, params).await;

        let (assembled, assemble_ms, gas_used, actual_tx_count, error) = match assemble_result {
            Ok((data, ms)) => {
                let gas = data.gas_used;
                let count = data.transactions.len() as u64;
                (Some(data), ms, gas, count, false)
            }
            Err(e) => {
                eprintln!("block {display_block_number}: assemble error: {e}");
                (None, 0.0, 0, 0, true)
            }
        };

        // --- import (TIMED) ---
        let full_inclusion = actual_tx_count == expected_tx_count;
        if !error && !full_inclusion {
            eprintln!(
                "block {display_block_number}: assembled {actual_tx_count}/{expected_tx_count} transactions"
            );
        }

        let (import_ms, error) = if !full_inclusion {
            (0.0, true)
        } else if let Some(data) = assembled {
            match client.new_l2_block(&args.engine_rpc, data).await {
                Ok(ms) => (ms, error),
                Err(e) => {
                    eprintln!("block {display_block_number}: import error: {e}");
                    (0.0, true)
                }
            }
        } else {
            (0.0, true)
        };

        // --- record timing (only assemble + import, no submit/pool_wait) ---
        let mut timing = BlockTimingV2 {
            block_number: display_block_number,
            tx_count: actual_tx_count,
            expected_tx_count,
            target_tps: None,
            engine: "reth".to_string(),
            mode: "exec".to_string(),
            workload: workload.to_string(),
            senders: args.senders.max(1),
            warmup_blocks: 0,
            phase: None,
            submit_ms: 0.0,
            pool_wait_ms: 0.0,
            assemble_ms,
            import_ms,
            total_ms: 0.0,
            gas_used,
            tps: 0.0,
            mgas_per_sec: 0.0,
            inclusion_rate: 0.0,
            cumulative_blocks: display_block_number,
            cumulative_txs: state.cumulative_txs + actual_tx_count,
            rolling_avg_tps_100: None,
            error,
        };
        timing.finalize();

        let line = serde_json::to_string(&timing)?;
        writeln!(out_file, "{line}")?;

        // --- progress ---
        if display_block_number % 10 == 0 || display_block_number == args.blocks {
            println!(
                "block {display_block_number}/{}: asm={:.1}ms imp={:.1}ms total={:.1}ms | {:.0} TPS, {:.0} MGas/s | {}/{}",
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

        if error {
            eyre::bail!(
                "block {display_block_number}: exec run failed; refusing to summarize a partial run"
            );
        }

        state.record_transactions(actual_tx_count);
        state.advance_block_number();
    }

    println!("Results written to {}", args.output);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_run_state_only_advances_block_number_after_success() {
        let mut state = ExecRunState::new(1);

        assert_eq!(state.current_block_number(), 1);
        let txs =
            tx_factory::build_block_txs(state.senders_mut(), Workload::EthTransfer, 1_000, 99_999)
                .unwrap();
        assert_eq!(txs.len(), 1_000);
        assert_eq!(state.current_block_number(), 1);

        state.advance_block_number();
        assert_eq!(state.current_block_number(), 2);
    }

    #[test]
    fn exec_run_state_preserves_block_and_nonce_progression_across_steps() {
        let mut state = ExecRunState::new(1);

        assert_eq!(state.current_block_number(), 1);
        let txs =
            tx_factory::build_block_txs(state.senders_mut(), Workload::EthTransfer, 1_000, 99_999)
                .unwrap();
        assert_eq!(txs.len(), 1_000);
        state.advance_block_number();
        assert_eq!(state.current_block_number(), 2);

        let txs =
            tx_factory::build_block_txs(state.senders_mut(), Workload::EthTransfer, 2_000, 99_999)
                .unwrap();
        assert_eq!(txs.len(), 2_000);
        state.advance_block_number();

        assert_eq!(state.senders()[0].nonce, 3_000);
        assert_eq!(state.current_block_number(), 3);
    }
}
