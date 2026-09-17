//! A Prometheus histogram of integer observations that takes no lock: one atomic per bucket and one for the sum.
//!
//! The Prometheus client's own histogram locks on every observation, which is not a price an event loop turn or a
//! journal append should pay. Values are integers in a base unit (nanoseconds, bytes, messages) and exported divided
//! by `scale` (nanoseconds as seconds). A scrape reads the atomics without stopping writers, so the count it reports
//! is the sum of the buckets it read, and `_sum` may be a moment ahead of them.

use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::{MetricType, TypedMetric};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Upper bounds for durations, in nanoseconds: 10 µs to 10 s.
pub const DURATION_BOUNDS: &[u64] = &[
    10_000,
    25_000,
    50_000,
    100_000,
    250_000,
    500_000,
    1_000_000,
    2_500_000,
    5_000_000,
    10_000_000,
    25_000_000,
    50_000_000,
    100_000_000,
    250_000_000,
    500_000_000,
    1_000_000_000,
    2_500_000_000,
    5_000_000_000,
    10_000_000_000,
];

/// Upper bounds for small counts, such as messages waiting: 0, then powers of two to 65 536.
pub const COUNT_BOUNDS: &[u64] =
    &[0, 1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16_384, 32_768, 65_536];

pub const NANOS_PER_SECOND: u64 = 1_000_000_000;

#[derive(Debug)]
struct Inner {
    bounds: &'static [u64],
    /// One per bound, then one for everything above the last.
    counts: Box<[AtomicU64]>,
    sum: AtomicU64,
    scale: u64,
}

/// Cheap to clone; every clone records into the same buckets.
#[derive(Clone, Debug)]
pub struct Histogram(Arc<Inner>);

impl Histogram {
    pub fn new(bounds: &'static [u64], scale: u64) -> Self {
        let counts = (0..=bounds.len()).map(|_| AtomicU64::new(0)).collect();
        Self(Arc::new(Inner { bounds, counts, sum: AtomicU64::new(0), scale: scale.max(1) }))
    }

    /// Durations, recorded in nanoseconds and exported in seconds.
    pub fn durations() -> Self {
        Self::new(DURATION_BOUNDS, NANOS_PER_SECOND)
    }

    /// Small counts, exported as they are.
    pub fn counts() -> Self {
        Self::new(COUNT_BOUNDS, 1)
    }

    pub fn observe(&self, value: u64) {
        let inner = &self.0;
        // A dozen or two bounds: a scan is as quick as a search and simpler.
        let bucket = inner.bounds.iter().position(|&b| value <= b).unwrap_or(inner.bounds.len());
        inner.counts[bucket].fetch_add(1, Ordering::Relaxed);
        inner.sum.fetch_add(value, Ordering::Relaxed);
    }

    pub fn observe_duration(&self, duration: Duration) {
        self.observe(u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX));
    }

    /// Observations so far.
    pub fn count(&self) -> u64 {
        self.0.counts.iter().map(|c| c.load(Ordering::Relaxed)).sum()
    }
}

impl TypedMetric for Histogram {
    const TYPE: MetricType = MetricType::Histogram;
}

/// Buckets as the encoder takes them: each bound in the exported unit with its own (not cumulative) count, the last
/// bound `f64::MAX` for `+Inf`.
#[allow(clippy::float_arithmetic)]
pub(crate) fn exported(bounds: &[u64], scale: u64, counts: impl Iterator<Item = u64>) -> Vec<(f64, u64)> {
    let uppers = bounds.iter().map(|&b| b as f64 / scale as f64).chain(std::iter::once(f64::MAX));
    uppers.zip(counts).collect()
}

impl EncodeMetric for Histogram {
    #[allow(clippy::float_arithmetic)]
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        let inner = &self.0;
        let buckets = exported(inner.bounds, inner.scale, inner.counts.iter().map(|c| c.load(Ordering::Relaxed)));
        let count = buckets.iter().map(|(_, c)| c).sum();
        let sum = inner.sum.load(Ordering::Relaxed) as f64 / inner.scale as f64;
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

    #[test]
    fn observations_land_in_the_first_bucket_that_holds_them_and_export_in_seconds() {
        let h = Histogram::durations();
        h.observe(10_000);
        h.observe(10_001);
        h.observe_duration(Duration::from_secs(60));
        assert_eq!(h.count(), 3);
        let mut registry = Registry::default();
        registry.register("turn_duration_seconds", "a turn", h);
        let mut text = String::new();
        encode(&mut text, &registry).expect("encoded");
        assert!(text.contains("turn_duration_seconds_bucket{le=\"0.00001\"} 1\n"), "{text}");
        assert!(text.contains("turn_duration_seconds_bucket{le=\"0.000025\"} 2\n"), "{text}");
        assert!(text.contains("turn_duration_seconds_bucket{le=\"+Inf\"} 3\n"), "{text}");
        assert!(text.contains("turn_duration_seconds_count 3\n"), "{text}");
        assert!(text.contains("turn_duration_seconds_sum 60.00002"), "{text}");
    }
}
