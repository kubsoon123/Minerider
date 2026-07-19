//! Vanilla's client-side chunk-batch pacing (`ChunkBatchSizeCalculator`).
//!
//! After each chunk batch the client tells the server how many chunks per
//! tick it wants next, via `chunk_batch_received`'s `chunks_per_tick`. The
//! value is not the batch size — it is `7_000_000 / aggregated_nanos_per_chunk`,
//! a running estimate that adapts to how long this client actually took to
//! process recent batches, so a fast client is fed chunks faster and a slow
//! one is throttled. The constants and formula match
//! `net.minecraft.client.multiplayer.ChunkBatchSizeCalculator` in 1.21.4.

use std::time::Duration;

/// Nanoseconds of chunk-processing budget per tick vanilla targets: ~7ms of
/// a 50ms tick. `desired_chunks_per_tick = NANOS_BUDGET / aggregated_nanos_per_chunk`.
const NANOS_PER_TICK_BUDGET: f64 = 7_000_000.0;

/// Initial per-chunk cost assumption before any batch has been measured: 2ms.
const INITIAL_NANOS_PER_CHUNK: f64 = 2_000_000.0;

/// A new sample may not pull the aggregate below `1/CLAMP` or above `CLAMP`
/// times its current value — one anomalous batch can't swing the estimate.
const CLAMP_COEFFICIENT: f64 = 3.0;

/// The rolling average weights the existing aggregate by the number of prior
/// samples, capped here so it stays responsive to sustained change rather
/// than freezing after a long session.
const MAX_OLD_SAMPLES_WEIGHT: u32 = 49;

/// Client-side estimate of how fast this client can accept chunks, updated
/// once per completed chunk batch. See the module docs.
#[derive(Debug, Clone)]
pub struct ChunkBatchSizeCalculator {
    aggregated_nanos_per_chunk: f64,
    old_samples_weight: u32,
}

impl Default for ChunkBatchSizeCalculator {
    fn default() -> Self {
        Self {
            aggregated_nanos_per_chunk: INITIAL_NANOS_PER_CHUNK,
            old_samples_weight: 1,
        }
    }
}

impl ChunkBatchSizeCalculator {
    /// Folds one finished batch's timing into the running estimate. `elapsed`
    /// is the wall-clock time from `chunk_batch_start` to `chunk_batch_finished`;
    /// `batch_size` is the finished packet's chunk count. A zero-size batch is
    /// ignored (no timing signal), exactly as vanilla does.
    pub fn record_batch(&mut self, elapsed: Duration, batch_size: u32) {
        if batch_size == 0 {
            return;
        }
        let nanos_per_chunk = elapsed.as_nanos() as f64 / batch_size as f64;
        let clamped = nanos_per_chunk.clamp(
            self.aggregated_nanos_per_chunk / CLAMP_COEFFICIENT,
            self.aggregated_nanos_per_chunk * CLAMP_COEFFICIENT,
        );
        self.aggregated_nanos_per_chunk =
            (self.aggregated_nanos_per_chunk * self.old_samples_weight as f64 + clamped)
                / (self.old_samples_weight as f64 + 1.0);
        self.old_samples_weight = (self.old_samples_weight + 1).min(MAX_OLD_SAMPLES_WEIGHT);
    }

    /// The `chunks_per_tick` to send in `chunk_batch_received`, given the
    /// current estimate. Always finite and positive.
    pub fn desired_chunks_per_tick(&self) -> f32 {
        (NANOS_PER_TICK_BUDGET / self.aggregated_nanos_per_chunk) as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_estimate_matches_vanilla_two_ms_per_chunk() {
        // Before any batch: 7ms budget / 2ms per chunk = 3.5 chunks/tick.
        let calc = ChunkBatchSizeCalculator::default();
        assert!((calc.desired_chunks_per_tick() - 3.5).abs() < 1e-6);
    }

    #[test]
    fn a_zero_size_batch_is_ignored() {
        let mut calc = ChunkBatchSizeCalculator::default();
        let before = calc.desired_chunks_per_tick();
        calc.record_batch(Duration::from_millis(100), 0);
        assert_eq!(calc.desired_chunks_per_tick(), before);
    }

    #[test]
    fn a_faster_client_is_fed_more_chunks_per_tick_than_a_slower_one() {
        // 0.5ms/chunk (fast) ends up wanting more chunks/tick than 5ms/chunk
        // (slow), after the same number of batches. Both are clamped per
        // batch, so this converges over several samples.
        let mut fast = ChunkBatchSizeCalculator::default();
        let mut slow = ChunkBatchSizeCalculator::default();
        for _ in 0..20 {
            fast.record_batch(Duration::from_micros(500 * 10), 10); // 0.5ms/chunk
            slow.record_batch(Duration::from_micros(5000 * 10), 10); // 5ms/chunk
        }
        assert!(
            fast.desired_chunks_per_tick() > slow.desired_chunks_per_tick(),
            "fast={} slow={}",
            fast.desired_chunks_per_tick(),
            slow.desired_chunks_per_tick()
        );
    }

    #[test]
    fn one_batch_cannot_swing_the_estimate_past_the_clamp() {
        // A single absurdly slow batch is clamped to 3x the current per-chunk
        // cost before averaging, so the aggregate can at most roughly double
        // toward it, never jump to the raw outlier.
        let mut calc = ChunkBatchSizeCalculator::default();
        let before = calc.aggregated_nanos_per_chunk;
        calc.record_batch(Duration::from_secs(10), 1); // wildly slow
        assert!(
            calc.aggregated_nanos_per_chunk <= before * CLAMP_COEFFICIENT,
            "a single batch must not exceed the clamp bound"
        );
    }

    #[test]
    fn desired_chunks_per_tick_is_always_finite_and_positive() {
        let mut calc = ChunkBatchSizeCalculator::default();
        for _ in 0..100 {
            calc.record_batch(Duration::from_micros(1), 8);
        }
        let v = calc.desired_chunks_per_tick();
        assert!(v.is_finite() && v > 0.0, "got {v}");
    }
}
