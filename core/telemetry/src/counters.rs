//! A family of counters whose label sets are all known at start, held as one atomic each and found by index, so
//! counting is a `fetch_add` with no lookup and no lock. A label set that has never counted anything is left out of the
//! exposition, so a grid of every route and status costs nothing in Prometheus until it is used.

use crate::Labels;
use prometheus_client::encoding::{EncodeMetric, MetricEncoder, NoLabelSet};
use prometheus_client::metrics::{MetricType, TypedMetric};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug)]
struct Inner {
    labels: Vec<Labels>,
    values: Box<[AtomicU64]>,
}

/// Cheap to clone; every clone counts into the same atomics.
#[derive(Clone, Debug)]
pub struct Counters(Arc<Inner>);

impl Counters {
    pub fn new(labels: Vec<Labels>) -> Self {
        let values = labels.iter().map(|_| AtomicU64::new(0)).collect();
        Self(Arc::new(Inner { labels, values }))
    }

    /// Every combination of the dimensions given, the first dimension varying slowest: index with
    /// [`Counters::index`].
    pub fn grid(dimensions: &[(&'static str, &[&str])]) -> Self {
        let mut labels: Vec<Labels> = vec![Vec::new()];
        for (name, values) in dimensions {
            labels = labels
                .iter()
                .flat_map(|prefix| {
                    values.iter().map(move |v| {
                        let mut l = prefix.clone();
                        l.push((*name, (*v).to_string()));
                        l
                    })
                })
                .collect();
        }
        Self::new(labels)
    }

    /// The index in a grid of the positions given, one per dimension, with the sizes of the dimensions.
    pub fn index(positions: &[usize], sizes: &[usize]) -> usize {
        positions.iter().zip(sizes).fold(0, |at, (p, size)| at * size + p)
    }

    pub fn inc(&self, index: usize) {
        self.add(index, 1);
    }

    pub fn add(&self, index: usize, n: u64) {
        if let Some(v) = self.0.values.get(index) {
            v.fetch_add(n, Ordering::Relaxed);
        }
    }

    pub fn get(&self, index: usize) -> u64 {
        self.0.values.get(index).map_or(0, |v| v.load(Ordering::Relaxed))
    }
}

impl TypedMetric for Counters {
    const TYPE: MetricType = MetricType::Counter;
}

impl EncodeMetric for Counters {
    fn encode(&self, mut encoder: MetricEncoder) -> Result<(), std::fmt::Error> {
        for (labels, value) in self.0.labels.iter().zip(self.0.values.iter()) {
            let value = value.load(Ordering::Relaxed);
            if value > 0 {
                encoder.encode_family(labels)?.encode_counter::<NoLabelSet, _, u64>(&value, None)?;
            }
        }
        Ok(())
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
    fn a_grid_counts_by_index_and_exports_only_what_counted() {
        let grid = Counters::grid(&[("half", &["public", "private"]), ("status", &["200", "404", "503"])]);
        grid.inc(Counters::index(&[1, 2], &[2, 3]));
        grid.add(Counters::index(&[0, 0], &[2, 3]), 5);
        let mut registry = Registry::default();
        registry.register("requests", "requests", grid);
        let mut text = String::new();
        encode(&mut text, &registry).expect("encoded");
        assert!(text.contains("requests_total{half=\"private\",status=\"503\"} 1\n"), "{text}");
        assert!(text.contains("requests_total{half=\"public\",status=\"200\"} 5\n"), "{text}");
        assert!(!text.contains("404"), "{text}");
    }
}
