//! Sweep — Automatic Inflection Point Discovery
//!
//! Runs `mode_exec` at increasing transaction counts per block, measuring
//! per-transaction assembly cost. When the marginal cost degrades beyond
//! 1.5x the best observed rate the scan stops. The point with the highest
//! median TPS is reported as the peak.

use crate::engine::BlockTimingV2;
use crate::mode_exec;
use crate::report::percentile;

use std::io::{BufRead, BufReader};

// ---------------------------------------------------------------------------
// CLI args
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct SweepArgs {
    #[arg(long, default_value = "http://127.0.0.1:8551")]
    pub engine_rpc: String,

    #[arg(long)]
    pub jwt_secret: String,

    #[arg(long, default_value = "http://127.0.0.1:8545")]
    pub http_rpc: String,

    #[arg(long)]
    pub workload: String,

    #[arg(long, default_value = "30")]
    pub blocks_per_step: u64,

    #[arg(long)]
    pub output_dir: String,

    #[arg(long, default_value = "unknown")]
    pub engine_name: String,

    #[arg(long, default_value = "99999")]
    pub chain_id: u64,
}

// ---------------------------------------------------------------------------
// Output types
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct SweepResult {
    pub engine: String,
    pub workload: String,
    pub peak_tps: f64,
    pub peak_mgas_s: f64,
    pub inflection_txs: u64,
    pub points: Vec<SweepPoint>,
}

#[derive(Debug, serde::Serialize)]
pub struct SweepPoint {
    pub txs_per_block: u64,
    pub median_assemble_ms: f64,
    pub median_total_ms: f64,
    pub p95_total_ms: f64,
    pub median_tps: f64,
    pub median_mgas_s: f64,
    pub per_tx_assemble_us: f64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read a JSONL file of `BlockTimingV2` records, filtering out error rows.
fn read_timings(path: &str) -> eyre::Result<Vec<BlockTimingV2>> {
    let file = std::fs::File::open(path)?;
    let reader = BufReader::new(file);
    let mut timings = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(t) = serde_json::from_str::<BlockTimingV2>(trimmed) {
            if !t.error {
                timings.push(t);
            }
        }
    }
    Ok(timings)
}

/// Compute a `SweepPoint` from a set of per-block timings at a given txs_per_block.
fn compute_point(txs_per_block: u64, timings: &[BlockTimingV2]) -> Option<SweepPoint> {
    if timings.is_empty() {
        return None;
    }

    let mut assemble: Vec<f64> = timings.iter().map(|t| t.assemble_ms).collect();
    let mut total: Vec<f64> = timings.iter().map(|t| t.total_ms).collect();
    let mut tps: Vec<f64> = timings.iter().map(|t| t.tps).collect();
    let mut mgas: Vec<f64> = timings.iter().map(|t| t.mgas_per_sec).collect();

    assemble.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    total.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    tps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    mgas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let median_assemble_ms = percentile(&assemble, 50.0);
    let median_total_ms = percentile(&total, 50.0);
    let p95_total_ms = percentile(&total, 95.0);
    let median_tps = percentile(&tps, 50.0);
    let median_mgas_s = percentile(&mgas, 50.0);

    // Per-transaction assemble time in microseconds.
    let per_tx_assemble_us = if txs_per_block > 0 {
        (median_assemble_ms * 1000.0) / txs_per_block as f64
    } else {
        0.0
    };

    Some(SweepPoint {
        txs_per_block,
        median_assemble_ms,
        median_total_ms,
        p95_total_ms,
        median_tps,
        median_mgas_s,
        per_tx_assemble_us,
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run(args: SweepArgs) -> eyre::Result<()> {
    let coarse_points: &[u64] = &[1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000];

    // Ensure output directory exists.
    std::fs::create_dir_all(&args.output_dir)?;

    let mut points: Vec<SweepPoint> = Vec::new();
    let mut min_per_tx_us: f64 = f64::MAX;

    for &txs in coarse_points {
        let output_path = format!(
            "{}/{}-{}-{}.jsonl",
            args.output_dir, args.engine_name, args.workload, txs
        );

        println!(
            "\n=== Sweep: {} txs/block x {} blocks ===",
            txs, args.blocks_per_step
        );

        let exec_args = mode_exec::ExecArgs {
            engine_rpc: args.engine_rpc.clone(),
            jwt_secret: args.jwt_secret.clone(),
            http_rpc: args.http_rpc.clone(),
            workload: args.workload.clone(),
            txs_per_block: txs,
            blocks: args.blocks_per_step,
            output: output_path.clone(),
            engine_name: args.engine_name.clone(),
            chain_id: args.chain_id,
        };

        mode_exec::run(exec_args).await?;

        // Read results and compute stats.
        let timings = read_timings(&output_path)?;

        let point = match compute_point(txs, &timings) {
            Some(p) => p,
            None => {
                println!("  No valid timings at {} txs/block, skipping.", txs);
                continue;
            }
        };

        println!(
            "  median_tps={:.0}  median_total={:.1}ms  per_tx_asm={:.2}us",
            point.median_tps, point.median_total_ms, point.per_tx_assemble_us
        );

        // Track minimum per-tx assemble cost.
        if point.per_tx_assemble_us < min_per_tx_us {
            min_per_tx_us = point.per_tx_assemble_us;
        }

        // Check for degradation: current > min * 1.5
        let degraded = point.per_tx_assemble_us > min_per_tx_us * 1.5;

        points.push(point);

        if degraded {
            println!(
                "  Degradation detected at {} txs/block (per-tx {:.2}us > {:.2}us threshold). Stopping.",
                txs,
                points.last().unwrap().per_tx_assemble_us,
                min_per_tx_us * 1.5
            );
            break;
        }
    }

    // Find the point with the highest median TPS.
    let (peak_tps, peak_mgas_s, inflection_txs) = points
        .iter()
        .max_by(|a, b| {
            a.median_tps
                .partial_cmp(&b.median_tps)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|p| (p.median_tps, p.median_mgas_s, p.txs_per_block))
        .unwrap_or((0.0, 0.0, 0));

    let result = SweepResult {
        engine: args.engine_name.clone(),
        workload: args.workload.clone(),
        peak_tps,
        peak_mgas_s,
        inflection_txs,
        points,
    };

    // Write summary JSON.
    let summary_path = format!(
        "{}/{}-{}-sweep-summary.json",
        args.output_dir, args.engine_name, args.workload
    );
    let summary_json = serde_json::to_string_pretty(&result)?;
    std::fs::write(&summary_path, &summary_json)?;

    println!("\n=== Sweep Complete ===");
    println!("  Peak TPS:       {:.0}", result.peak_tps);
    println!("  Peak Mgas/s:    {:.2}", result.peak_mgas_s);
    println!("  Inflection at:  {} txs/block", result.inflection_txs);
    println!("  Summary:        {}", summary_path);

    Ok(())
}
