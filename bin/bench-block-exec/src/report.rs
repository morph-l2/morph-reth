use crate::engine::{BlockTiming, BlockTimingV2};
use clap::Args;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
pub struct SummarizeArgs {
    /// Directory containing per-round result files.
    #[arg(long)]
    pub results_dir: String,
    /// Output file path for the summary.
    #[arg(long)]
    pub output: Option<String>,
    /// Use V2 format (for new benchmark modes).
    #[arg(long, default_value = "false")]
    pub v2: bool,
}

/// Compute the `p`-th percentile (0..100) from a **pre-sorted** slice using
/// linear interpolation between adjacent values.
///
/// Returns `0.0` for an empty slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }

    // Map percentile [0, 100] to a fractional index in [0, n-1].
    let n = sorted.len() as f64;
    let rank = (p / 100.0) * (n - 1.0);
    let lower = rank.floor() as usize;
    let upper = rank.ceil().min(n - 1.0) as usize;
    let frac = rank - lower as f64;

    sorted[lower] + frac * (sorted[upper] - sorted[lower])
}

/// Recursively list all files (not directories) under `dir`.
fn walkdir(dir: &Path) -> eyre::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    if !dir.is_dir() {
        return Ok(files);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            files.extend(walkdir(&path)?);
        } else {
            files.push(path);
        }
    }
    Ok(files)
}

pub fn summarize(args: SummarizeArgs) -> eyre::Result<()> {
    if args.v2 {
        return summarize_v2(&args.results_dir, args.output.as_deref());
    }
    let dir = Path::new(&args.results_dir);

    // 1. Recursively find all .json files.
    let json_files: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();

    // 2. Parse BlockTiming from each JSON-lines file.
    let mut all_timings: Vec<BlockTiming> = Vec::new();
    for path in &json_files {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(timing) = serde_json::from_str::<BlockTiming>(trimmed) {
                all_timings.push(timing);
            }
        }
    }

    // 3. Group by (engine, layer, tx_count) — BTreeMap for sorted output.
    let mut groups: BTreeMap<(String, String, u64), Vec<BlockTiming>> = BTreeMap::new();
    for t in all_timings {
        groups
            .entry((t.engine.clone(), t.layer.clone(), t.tx_count))
            .or_default()
            .push(t);
    }

    // 4. Build the TSV output.
    let header =
        "engine\tlayer\ttx/blk\tavg_asm_ms\tavg_imp_ms\tavg_tot_ms\tp50_ms\tp95_ms\tp99_ms\teff_tps\t<300ms%";

    let mut rows: Vec<String> = vec![header.to_string()];

    for ((engine, layer, tx_count), entries) in &groups {
        // Skip first 10 entries as warmup.
        let data: Vec<&BlockTiming> = entries.iter().skip(10).collect();
        if data.is_empty() {
            continue;
        }

        let n = data.len() as f64;

        let avg_assemble: f64 = data.iter().map(|t| t.assemble_ms).sum::<f64>() / n;
        let avg_import: f64 = data.iter().map(|t| t.import_ms).sum::<f64>() / n;
        let avg_total: f64 = data.iter().map(|t| t.total_ms).sum::<f64>() / n;

        let mut totals: Vec<f64> = data.iter().map(|t| t.total_ms).collect();
        totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let p50 = percentile(&totals, 50.0);
        let p95 = percentile(&totals, 95.0);
        let p99 = percentile(&totals, 99.0);

        let effective_tps = if avg_total > 0.0 {
            *tx_count as f64 / (avg_total / 1000.0)
        } else {
            0.0
        };

        let meets_300ms_count = data.iter().filter(|t| t.total_ms < 300.0).count();
        let meets_300ms_pct = meets_300ms_count as f64 / n * 100.0;

        rows.push(format!(
            "{}\t{}\t{}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.1}\t{:.1}",
            engine,
            layer,
            tx_count,
            avg_assemble,
            avg_import,
            avg_total,
            p50,
            p95,
            p99,
            effective_tps,
            meets_300ms_pct,
        ));
    }

    let output_text = rows.join("\n");

    // 5. Print to stdout.
    println!("{output_text}");

    // 6. Optionally write to file.
    if let Some(ref path) = args.output {
        let mut f = fs::File::create(path)?;
        writeln!(f, "{output_text}")?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// V2 summarization (new benchmark modes)
// ---------------------------------------------------------------------------

/// Summarize `.jsonl` files containing [`BlockTimingV2`] records.
///
/// Groups entries by `(engine, mode, workload, senders, warmup_blocks)`, skips
/// the first 10 entries per group as warmup, then computes aggregate stats.
fn summarize_v2(results_dir: &str, output: Option<&str>) -> eyre::Result<()> {
    let dir = Path::new(results_dir);

    // 1. Collect all .jsonl files.
    let jsonl_files: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "jsonl" || ext == "json")
        })
        .collect();

    // 2. Parse BlockTimingV2 from each file.
    let mut all_timings: Vec<BlockTimingV2> = Vec::new();
    for path in &jsonl_files {
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(timing) = serde_json::from_str::<BlockTimingV2>(trimmed) {
                all_timings.push(timing);
            }
        }
    }

    // 3. Group by (engine, mode, workload, senders, warmup_blocks).
    type GroupKey = (String, String, String, u64, u64);
    let mut groups: BTreeMap<GroupKey, Vec<BlockTimingV2>> = BTreeMap::new();
    for t in all_timings {
        groups
            .entry((
                t.engine.clone(),
                t.mode.clone(),
                t.workload.clone(),
                t.senders,
                t.warmup_blocks,
            ))
            .or_default()
            .push(t);
    }

    // 4. Build TSV output.
    let header = "engine\tmode\tworkload\tsenders\twarmup\tblocks\tavg_txs\tinclusion%\tavg_asm_ms\tavg_imp_ms\tavg_tot_ms\tp50_ms\tp95_ms\tp99_ms\tpeak_tps\tavg_tps\tavg_mgas_s\tdegradation%\terrors";

    let mut rows: Vec<String> = vec![header.to_string()];

    for ((engine, mode, workload, senders, warmup), entries) in &groups {
        // Skip first 10 entries as warmup.
        let data: Vec<&BlockTimingV2> = entries.iter().skip(10).collect();
        if data.is_empty() {
            continue;
        }

        let n = data.len() as f64;
        let blocks = data.len();

        // Averages
        let avg_txs: f64 = data.iter().map(|t| t.tx_count as f64).sum::<f64>() / n;
        let avg_inclusion: f64 =
            data.iter().map(|t| t.inclusion_rate).sum::<f64>() / n * 100.0;
        let avg_asm: f64 = data.iter().map(|t| t.assemble_ms).sum::<f64>() / n;
        let avg_imp: f64 = data.iter().map(|t| t.import_ms).sum::<f64>() / n;
        let avg_tot: f64 = data.iter().map(|t| t.total_ms).sum::<f64>() / n;

        // Percentiles on total_ms
        let mut totals: Vec<f64> = data.iter().map(|t| t.total_ms).collect();
        totals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = percentile(&totals, 50.0);
        let p95 = percentile(&totals, 95.0);
        let p99 = percentile(&totals, 99.0);

        // TPS
        let tps_values: Vec<f64> = data.iter().map(|t| t.tps).collect();
        let peak_tps = tps_values
            .iter()
            .cloned()
            .fold(f64::NEG_INFINITY, f64::max);
        let avg_tps: f64 = tps_values.iter().sum::<f64>() / n;

        // Mgas/s
        let avg_mgas_s: f64 = data.iter().map(|t| t.mgas_per_sec).sum::<f64>() / n;

        // Degradation: for runs with 200+ entries, compare first 100 vs last 100
        let degradation_pct = if data.len() >= 200 {
            let first100_avg: f64 =
                data[..100].iter().map(|t| t.tps).sum::<f64>() / 100.0;
            let last100_avg: f64 = data[data.len() - 100..]
                .iter()
                .map(|t| t.tps)
                .sum::<f64>()
                / 100.0;
            if first100_avg > 0.0 {
                (last100_avg / first100_avg - 1.0) * 100.0
            } else {
                0.0
            }
        } else {
            f64::NAN
        };

        // Error count
        let errors = data.iter().filter(|t| t.error).count();

        let deg_str = if degradation_pct.is_nan() {
            "N/A".to_string()
        } else {
            format!("{degradation_pct:.1}")
        };

        rows.push(format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.1}\t{:.1}\t{:.2}\t{}\t{}",
            engine,
            mode,
            workload,
            senders,
            warmup,
            blocks,
            avg_txs,
            avg_inclusion,
            avg_asm,
            avg_imp,
            avg_tot,
            p50,
            p95,
            p99,
            peak_tps,
            avg_tps,
            avg_mgas_s,
            deg_str,
            errors,
        ));
    }

    let output_text = rows.join("\n");

    // Print to stdout.
    println!("{output_text}");

    // Optionally write to file.
    if let Some(path) = output {
        let mut f = fs::File::create(path)?;
        writeln!(f, "{output_text}")?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_calculation_is_correct() {
        let values = vec![10.0, 20.0, 30.0, 40.0, 50.0, 60.0, 70.0, 80.0, 90.0, 100.0];
        assert!(
            (percentile(&values, 50.0) - 55.0).abs() < 1.0,
            "p50 should be ~55.0, got {}",
            percentile(&values, 50.0)
        );
        assert!(
            (percentile(&values, 95.0) - 95.5).abs() < 1.0,
            "p95 should be ~95.5, got {}",
            percentile(&values, 95.0)
        );
    }

    #[test]
    fn percentile_single_value() {
        assert_eq!(percentile(&[42.0], 50.0), 42.0);
    }

    #[test]
    fn percentile_empty() {
        assert_eq!(percentile(&[], 50.0), 0.0);
    }
}
