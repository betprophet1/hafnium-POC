//! Latency distributions at full resolution (blueprint §14: per-hop histograms retained at full resolution, not
//! pre-averaged), in HdrHistogram, exported to Prometheus at fixed bounds.
//!
//! One thread records into a [`Recorder`] it owns, with no lock and no allocation, and merges what it has recorded
//! into the shared [`Hdr`] when it is next due and nobody else holds it; a scrape reads the shared one. The shared
//! histogram keeps every value since the process started at 3 significant figures, from 1 µs to 60 s, so its
//! percentiles are exact to 0.1% (`percentiles`), while Prometheus gets cumulative buckets it can rate and aggregate.

use crate::histogram::{exported, NANOS_PER_SECOND};
use hdrhistogram::Histogram as HdrHistogram;
use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::{MetricType, TypedMetric};
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

/// The smallest value told apart, in nanoseconds: a microsecond.
const LOWEST: u64 = 1_000;
/// The largest value kept exactly; anything above is recorded as this.
const HIGHEST: u64 = 60 * NANOS_PER_SECOND;
const SIGNIFICANT_FIGURES: u8 = 3;

/// Bounds of the exported buckets, in nanoseconds: 5 µs to 60 s, finer than the operational histograms' because these
/// are the distributions quoted.
pub const HOP_BOUNDS: &[u64] = &[
    5_000,
    10_000,
    20_000,
    50_000,
    100_000,
    200_000,
    500_000,
    1_000_000,
    2_000_000,
    5_000_000,
    10_000_000,
    20_000_000,
    50_000_000,
    100_000_000,
    200_000_000,
    500_000_000,
    1_000_000_000,
    2_000_000_000,
    5_000_000_000,
    10_000_000_000,
    30_000_000_000,
    60_000_000_000,
];

fn empty() -> HdrHistogram<u64> {
    HdrHistogram::new_with_bounds(LOWEST, HIGHEST, SIGNIFICANT_FIGURES).expect("fixed bounds are valid")
}

/// The shared distribution. Cheap to clone; every clone is the same histogram.
#[derive(Clone, Debug)]
pub struct Hdr(Arc<Mutex<HdrHistogram<u64>>>);

impl Default for Hdr {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(empty())))
    }
}

impl Hdr {
    /// A recorder that merges into this histogram at most every `every`.
    pub fn recorder(&self, every: Duration) -> Recorder {
        Recorder { shared: self.clone(), local: empty(), every, merged: None }
    }

    /// Records one value directly, for a thread that records rarely.
    pub fn record(&self, nanos: u64) {
        let mut h = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        h.saturating_record(nanos.clamp(LOWEST, HIGHEST));
    }

    /// Count, then the value at each quantile given (0.5, 0.99, ...) and the maximum, in nanoseconds.
    pub fn percentiles(&self, quantiles: &[f64]) -> (u64, Vec<u64>, u64) {
        let h = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        (h.len(), quantiles.iter().map(|&q| h.value_at_quantile(q)).collect(), h.max())
    }
}

/// One thread's recordings not yet merged.
#[derive(Debug)]
pub struct Recorder {
    shared: Hdr,
    local: HdrHistogram<u64>,
    every: Duration,
    merged: Option<Instant>,
}

impl Recorder {
    /// Records a value in nanoseconds, clamped to the histogram's range, and merges if due. `now` is the caller's
    /// monotonic clock (the workspace has one helper per process for it).
    pub fn record(&mut self, nanos: u64, now: Instant) {
        self.local.saturating_record(nanos.clamp(LOWEST, HIGHEST));
        if self.merged.is_none_or(|at| now.duration_since(at) >= self.every) {
            self.merge(now);
        }
    }

    /// Adds what has been recorded to the shared histogram unless a scrape holds it, in which case it waits for the
    /// next time.
    pub fn merge(&mut self, now: Instant) {
        if self.local.is_empty() {
            self.merged = Some(now);
            return;
        }
        let mut shared = match self.shared.0.try_lock() {
            Ok(shared) => shared,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        // Same bounds on both sides, so adding cannot fail.
        let _ = shared.add(&self.local);
        drop(shared);
        self.local.reset();
        self.merged = Some(now);
    }
}

impl TypedMetric for Hdr {
    const TYPE: MetricType = MetricType::Histogram;
}

impl EncodeMetric for Hdr {
    #[allow(clippy::float_arithmetic)]
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        let (buckets, count, sum) = {
            let h = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            let mut below = 0u64;
            let mut counts = Vec::with_capacity(HOP_BOUNDS.len() + 1);
            for &bound in HOP_BOUNDS {
                let upto = h.count_between(0, bound);
                counts.push(upto - below.min(upto));
                below = upto;
            }
            counts.push(h.len() - below.min(h.len()));
            let sum = h.mean() * h.len() as f64 / NANOS_PER_SECOND as f64;
            (exported(HOP_BOUNDS, NANOS_PER_SECOND, counts.into_iter()), h.len(), sum)
        };
        encoder.encode_histogram::<NoLabelSet>(sum, count, &buckets, None)
    }

    fn metric_type(&self) -> MetricType {
        Self::TYPE
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;
    use prometheus_client::registry::Registry;

    #[allow(clippy::disallowed_methods)]
    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_recorder_merges_into_the_shared_histogram_which_exports_cumulative_buckets() {
        let hdr = Hdr::default();
        let mut recorder = hdr.recorder(Duration::from_secs(3600));
        let t = now();
        recorder.record(3_000, t); // merged at once: nothing merged before
        recorder.record(150_000, t);
        recorder.record(90_000_000_000, t); // above the range: kept as 60 s
        assert_eq!(hdr.percentiles(&[0.5]).0, 1, "the later two wait for the next merge");
        recorder.merge(t);
        let (count, p, max) = hdr.percentiles(&[0.5]);
        assert_eq!(count, 3);
        assert!((149_000..=151_000).contains(&p[0]), "{p:?}");
        assert!(max >= HIGHEST, "{max}");
        let mut registry = Registry::default();
        registry.register("hop_latency_seconds", "a hop", hdr);
        let mut text = String::new();
        encode(&mut text, &registry).expect("encoded");
        assert!(text.contains("hop_latency_seconds_bucket{le=\"0.000005\"} 1\n"), "{text}");
        assert!(text.contains("hop_latency_seconds_bucket{le=\"0.1\"} 2\n"), "{text}");
        assert!(text.contains("hop_latency_seconds_bucket{le=\"+Inf\"} 3\n"), "{text}");
        assert!(text.contains("hop_latency_seconds_count 3\n"), "{text}");
    }
}
