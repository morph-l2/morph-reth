use crate::engine::{BlockTiming, BlockTimingV2};
use clap::Args;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

#[derive(Args)]
pub(crate) struct SummarizeArgs {
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
pub(crate) fn percentile(sorted: &[f64], p: f64) -> f64 {
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

fn is_raw_sweep_result(path: &Path) -> bool {
    path.components()
        .any(|component| component.as_os_str() == "sweep")
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn mean_tps(data: &[&BlockTimingV2]) -> f64 {
    if data.is_empty() {
        0.0
    } else {
        data.iter().map(|t| t.tps).sum::<f64>() / data.len() as f64
    }
}

fn realized_tps(data: &[&BlockTimingV2]) -> f64 {
    let total_ms: f64 = data.iter().map(|t| t.total_ms).sum();
    let total_txs: u64 = data.iter().map(|t| t.tx_count).sum();

    if total_ms > 0.0 {
        total_txs as f64 / (total_ms / 1000.0)
    } else {
        0.0
    }
}

fn openloop_phase_split_stats(
    mode: &str,
    data: &[&BlockTimingV2],
) -> (usize, f64, f64, usize, f64, f64) {
    if mode != "openloop" {
        return (0, 0.0, 0.0, 0, 0.0, 0.0);
    }

    let mut active = Vec::new();
    let mut drain = Vec::new();

    for timing in data {
        match timing.phase.as_deref() {
            Some("active") => active.push(*timing),
            Some("drain") => drain.push(*timing),
            Some(_) => {}
            None if timing.tx_count > 0 => active.push(*timing),
            None => drain.push(*timing),
        }
    }

    (
        active.len(),
        mean_tps(&active),
        realized_tps(&active),
        drain.len(),
        mean_tps(&drain),
        realized_tps(&drain),
    )
}

fn tps_window_stats(data: &[&BlockTimingV2]) -> (f64, f64, f64, f64) {
    if data.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }

    let window_len = (data.len() / 3).max(1);
    let early: Vec<f64> = data.iter().take(window_len).map(|t| t.tps).collect();
    let late: Vec<f64> = data.iter().rev().take(window_len).map(|t| t.tps).collect();

    let mid_start = data.len().saturating_sub(window_len) / 2;
    let mid_end = (mid_start + window_len).min(data.len());
    let mid: Vec<f64> = data[mid_start..mid_end].iter().map(|t| t.tps).collect();

    let early_tps = mean(&early);
    let mid_tps = mean(&mid);
    let late_tps = mean(&late);
    let late_vs_early_pct = if early_tps > 0.0 {
        (late_tps / early_tps - 1.0) * 100.0
    } else {
        0.0
    };

    (early_tps, mid_tps, late_tps, late_vs_early_pct)
}

pub(crate) fn summarize(args: SummarizeArgs) -> eyre::Result<()> {
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
    let header = "engine\tlayer\ttx/blk\tavg_asm_ms\tavg_imp_ms\tavg_tot_ms\tp50_ms\tp95_ms\tp99_ms\teff_tps\t<300ms%";

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
/// Groups entries by benchmark configuration and computes aggregate stats.
/// Warmup samples are excluded by the benchmark modes themselves.
fn summarize_v2(results_dir: &str, output: Option<&str>) -> eyre::Result<()> {
    let dir = Path::new(results_dir);

    // 1. Collect all .jsonl files.
    let jsonl_files: Vec<PathBuf> = walkdir(dir)?
        .into_iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "jsonl" || ext == "json")
                && !is_raw_sweep_result(p)
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

    // 3. Keep fixed-block load and open-loop target as separate dimensions.
    type GroupKey = (String, String, String, u64, u64, u64, Option<u64>);
    let mut groups: BTreeMap<GroupKey, Vec<BlockTimingV2>> = BTreeMap::new();
    for t in all_timings {
        groups
            .entry((
                t.engine.clone(),
                t.mode.clone(),
                t.workload.clone(),
                t.senders,
                t.warmup_blocks,
                t.expected_tx_count,
                t.target_tps,
            ))
            .or_default()
            .push(t);
    }

    // 4. Build TSV output.
    let header = "engine\tmode\tworkload\tsenders\twarmup\ttxs_per_block\ttarget_tps\tblocks\tavg_txs\tinclusion%\tavg_asm_ms\tavg_imp_ms\tavg_tot_ms\tp50_ms\tp95_ms\tp99_ms\tpeak_tps\tavg_tps\trealized_tps\tavg_mgas_s\tearly_tps\tmid_tps\tlate_tps\tlate_vs_early%\tactive_blocks\tactive_avg_tps\tactive_realized_tps\tdrain_blocks\tdrain_avg_tps\tdrain_realized_tps\tdegradation%\terrors";

    let mut rows: Vec<String> = vec![header.to_string()];

    for ((engine, mode, workload, senders, warmup, txs_per_block, target_tps), entries) in &groups {
        let data: Vec<&BlockTimingV2> = entries.iter().collect();
        if data.is_empty() {
            continue;
        }

        let n = data.len() as f64;
        let blocks = data.len();

        // Averages
        let avg_txs: f64 = data.iter().map(|t| t.tx_count as f64).sum::<f64>() / n;
        let avg_inclusion: f64 = data.iter().map(|t| t.inclusion_rate).sum::<f64>() / n * 100.0;
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
        let peak_tps = tps_values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let avg_tps: f64 = tps_values.iter().sum::<f64>() / n;
        let realized_tps = realized_tps(&data);

        // Mgas/s
        let avg_mgas_s: f64 = data.iter().map(|t| t.mgas_per_sec).sum::<f64>() / n;

        let (early_tps, mid_tps, late_tps, late_vs_early_pct) = tps_window_stats(&data);
        let (
            active_blocks,
            active_avg_tps,
            active_realized_tps,
            drain_blocks,
            drain_avg_tps,
            drain_realized_tps,
        ) = openloop_phase_split_stats(mode, &data);

        // Degradation: for runs with 200+ entries, compare first 100 vs last 100
        let degradation_pct = if data.len() >= 200 {
            let first100_avg: f64 = data[..100].iter().map(|t| t.tps).sum::<f64>() / 100.0;
            let last100_avg: f64 =
                data[data.len() - 100..].iter().map(|t| t.tps).sum::<f64>() / 100.0;
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
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.1}\t{:.1}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.2}\t{:.1}\t{:.1}\t{:.1}\t{:.2}\t{:.1}\t{:.1}\t{:.1}\t{:.1}\t{}\t{:.1}\t{:.1}\t{}\t{:.1}\t{:.1}\t{}\t{}",
            engine,
            mode,
            workload,
            senders,
            warmup,
            txs_per_block,
            target_tps.map_or_else(|| "N/A".to_string(), |value| value.to_string()),
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
            realized_tps,
            avg_mgas_s,
            early_tps,
            mid_tps,
            late_tps,
            late_vs_early_pct,
            active_blocks,
            active_avg_tps,
            active_realized_tps,
            drain_blocks,
            drain_avg_tps,
            drain_realized_tps,
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
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

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

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bench-block-exec-{name}-{nanos}"))
    }

    fn write_rows(path: &Path, txs_per_block: u64, tps: f64) {
        write_rows_with_tps(path, txs_per_block, vec![tps; 12]);
    }

    fn write_rows_with_tps(path: &Path, txs_per_block: u64, tps_values: Vec<f64>) {
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();

        let rows: Vec<String> = tps_values
            .into_iter()
            .enumerate()
            .map(|(idx, tps)| {
                serde_json::to_string(&BlockTimingV2 {
                    block_number: idx as u64 + 1,
                    tx_count: txs_per_block,
                    expected_tx_count: txs_per_block,
                    target_tps: None,
                    engine: "reth".to_string(),
                    mode: "exec".to_string(),
                    workload: "eth-transfer".to_string(),
                    senders: 1,
                    warmup_blocks: 0,
                    phase: None,
                    submit_ms: 0.0,
                    pool_wait_ms: 0.0,
                    assemble_ms: 10.0,
                    import_ms: 5.0,
                    total_ms: 15.0,
                    gas_used: txs_per_block * 21_000,
                    tps,
                    mgas_per_sec: 21.0,
                    inclusion_rate: 1.0,
                    cumulative_blocks: idx as u64 + 1,
                    cumulative_txs: (idx as u64 + 1) * txs_per_block,
                    rolling_avg_tps_100: None,
                    error: false,
                })
                .unwrap()
            })
            .collect();

        fs::write(path, rows.join("\n")).unwrap();
    }

    #[test]
    fn summarize_v2_skips_sweep_inputs_and_separates_tx_counts() {
        let dir = temp_dir("summary-v2");
        let output = dir.join("summary.tsv");

        write_rows(
            &dir.join("exec/reth-eth-transfer-1000.jsonl"),
            1000,
            10_000.0,
        );
        write_rows(
            &dir.join("exec/reth-eth-transfer-2000.jsonl"),
            2000,
            20_000.0,
        );
        write_rows(
            &dir.join("sweep/reth-eth-transfer-3000.jsonl"),
            3000,
            30_000.0,
        );

        summarize_v2(dir.to_str().unwrap(), Some(output.to_str().unwrap())).unwrap();

        let summary = fs::read_to_string(&output).unwrap();
        let lines: Vec<&str> = summary.lines().collect();

        assert!(lines[0].contains("txs_per_block"));
        assert!(lines[0].contains("early_tps"));
        assert!(lines[0].contains("mid_tps"));
        assert!(lines[0].contains("late_tps"));
        assert!(lines[0].contains("late_vs_early%"));
        assert_eq!(
            lines.len(),
            3,
            "expected header plus two exec rows: {summary}"
        );
        assert!(summary.contains("\t1000\t"));
        assert!(summary.contains("\t2000\t"));
        assert!(
            !summary.contains("\t3000\t"),
            "raw sweep rows must stay out of summary"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summarize_v2_reports_early_mid_late_tps_windows() {
        let dir = temp_dir("summary-v2-windows");
        let output = dir.join("summary.tsv");

        let mut tps_values = vec![1000.0; 10];
        tps_values.extend(vec![900.0; 10]);
        tps_values.extend(vec![700.0; 10]);
        write_rows_with_tps(
            &dir.join("e2e/reth-eth-transfer-4000.jsonl"),
            4000,
            tps_values,
        );

        summarize_v2(dir.to_str().unwrap(), Some(output.to_str().unwrap())).unwrap();

        let summary = fs::read_to_string(&output).unwrap();
        let row = summary.lines().nth(1).unwrap();
        assert!(
            row.contains("\t1000.0\t"),
            "expected early window avg TPS in row: {row}"
        );
        assert!(
            row.contains("\t700.0\t"),
            "expected late window avg TPS in row: {row}"
        );
        assert!(
            row.contains("\t-30.0"),
            "expected late-vs-early drop in row: {row}"
        );

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn summarize_v2_reports_openloop_active_and_drain_splits() {
        let dir = temp_dir("summary-v2-openloop");
        let output = dir.join("summary.tsv");
        let path = dir.join("openloop/reth-eth-transfer.jsonl");
        let parent = path.parent().unwrap();
        fs::create_dir_all(parent).unwrap();

        let mut rows = Vec::new();
        for idx in 0..10 {
            rows.push(
                serde_json::json!({
                    "block_number": idx + 1,
                    "tx_count": 100,
                    "expected_tx_count": 1000,
                    "engine": "reth",
                    "mode": "openloop",
                    "workload": "eth-transfer",
                    "senders": 2000,
                    "warmup_blocks": 0,
                    "phase": "active",
                    "submit_ms": 0.0,
                    "pool_wait_ms": 0.0,
                    "assemble_ms": 5.0,
                    "import_ms": 5.0,
                    "total_ms": 100.0,
                    "gas_used": 2_100_000,
                    "tps": 1000.0,
                    "mgas_per_sec": 21.0,
                    "inclusion_rate": 1.0,
                    "cumulative_blocks": idx + 1,
                    "cumulative_txs": (idx + 1) * 100,
                    "error": false
                })
                .to_string(),
            );
        }

        for idx in 0..3 {
            rows.push(
                serde_json::json!({
                    "block_number": 11 + idx,
                    "tx_count": 120,
                    "expected_tx_count": 1000,
                    "engine": "reth",
                    "mode": "openloop",
                    "workload": "eth-transfer",
                    "senders": 2000,
                    "warmup_blocks": 0,
                    "phase": "active",
                    "submit_ms": 0.0,
                    "pool_wait_ms": 0.0,
                    "assemble_ms": 5.0,
                    "import_ms": 5.0,
                    "total_ms": 100.0,
                    "gas_used": 2_520_000,
                    "tps": 1200.0,
                    "mgas_per_sec": 25.2,
                    "inclusion_rate": 1.0,
                    "cumulative_blocks": 11 + idx,
                    "cumulative_txs": 1000 + ((idx + 1) * 120),
                    "error": false
                })
                .to_string(),
            );
        }

        for idx in 0..3 {
            rows.push(
                serde_json::json!({
                    "block_number": 14 + idx,
                    "tx_count": 0,
                    "expected_tx_count": 1000,
                    "engine": "reth",
                    "mode": "openloop",
                    "workload": "eth-transfer",
                    "senders": 2000,
                    "warmup_blocks": 0,
                    "phase": "drain",
                    "submit_ms": 0.0,
                    "pool_wait_ms": 0.0,
                    "assemble_ms": 2.0,
                    "import_ms": 1.0,
                    "total_ms": 10.0,
                    "gas_used": 0,
                    "tps": 0.0,
                    "mgas_per_sec": 0.0,
                    "inclusion_rate": 1.0,
                    "cumulative_blocks": 14 + idx,
                    "cumulative_txs": 1360,
                    "error": false
                })
                .to_string(),
            );
        }

        fs::write(&path, rows.join("\n")).unwrap();

        summarize_v2(dir.to_str().unwrap(), Some(output.to_str().unwrap())).unwrap();

        let summary = fs::read_to_string(&output).unwrap();
        let header: Vec<&str> = summary.lines().next().unwrap().split('\t').collect();
        let row: Vec<&str> = summary.lines().nth(1).unwrap().split('\t').collect();

        let active_blocks_idx = header
            .iter()
            .position(|field| *field == "active_blocks")
            .expect("missing active_blocks column");
        let active_avg_tps_idx = header
            .iter()
            .position(|field| *field == "active_avg_tps")
            .expect("missing active_avg_tps column");
        let drain_blocks_idx = header
            .iter()
            .position(|field| *field == "drain_blocks")
            .expect("missing drain_blocks column");
        let drain_avg_tps_idx = header
            .iter()
            .position(|field| *field == "drain_avg_tps")
            .expect("missing drain_avg_tps column");
        let realized_tps_idx = header
            .iter()
            .position(|field| *field == "realized_tps")
            .expect("missing realized_tps column");
        let active_realized_tps_idx = header
            .iter()
            .position(|field| *field == "active_realized_tps")
            .expect("missing active_realized_tps column");
        let drain_realized_tps_idx = header
            .iter()
            .position(|field| *field == "drain_realized_tps")
            .expect("missing drain_realized_tps column");

        assert_eq!(row[active_blocks_idx], "13");
        assert_eq!(row[active_avg_tps_idx], "1046.2");
        assert_eq!(row[drain_blocks_idx], "3");
        assert_eq!(row[drain_avg_tps_idx], "0.0");
        assert_eq!(row[realized_tps_idx], "1022.6");
        assert_eq!(row[active_realized_tps_idx], "1046.2");
        assert_eq!(row[drain_realized_tps_idx], "0.0");

        fs::remove_dir_all(dir).unwrap();
    }
}
