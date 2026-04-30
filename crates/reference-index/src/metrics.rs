pub fn set_ready(ready: bool) {
    metrics::gauge!("morph_reference_index_ready").set(if ready { 1.0 } else { 0.0 });
}

pub fn set_lag_blocks(lag: u64) {
    metrics::gauge!("morph_reference_index_lag_blocks").set(lag as f64);
}

pub fn set_backfill_progress(progress: f64) {
    metrics::gauge!("morph_reference_index_backfill_progress").set(progress.clamp(0.0, 1.0));
}

pub fn set_backfill_state(state: u8) {
    metrics::gauge!("morph_reference_index_backfill_state").set(state as f64);
}

pub fn increment_entries(count: u64) {
    metrics::counter!("morph_reference_index_entries_total").increment(count);
}
