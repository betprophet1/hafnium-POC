//! Metrics and logs for the venue's processes (deploy/observability/metrics.md).
//!
//! Outside the engine, always: the engine is a pure function of its journal (blueprint §4) and gains no metrics. What
//! the processes around it measure, they measure here: counters and gauges from the Prometheus client, histograms
//! cheap enough for an event loop (`histogram.rs`), full-resolution latency distributions for the hops the blueprint
//! publishes (`hdr.rs`), a `/metrics` endpoint on an internal port (`serve.rs`), and one structured log line per event
//! (`log.rs`).
//!
//! Recording is never a lock on a hot path: counters, gauges and histograms are atomics behind handles made once at
//! start, and an HdrHistogram is recorded by one thread into its own copy and merged into the shared one now and then.

pub mod counters;
pub mod hdr;
pub mod histogram;
pub mod log;
pub mod serve;

pub use prometheus_client;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use std::time::{SystemTime, UNIX_EPOCH};

/// A label set: fixed names, values from bounded sets (metrics.md, bounded cardinality).
pub type Labels = Vec<(&'static str, String)>;

/// Wall-clock nanoseconds since the epoch. The workspace bans clocks because the engine's only clock is the journal;
/// a process outside the engine compares its own clock with a journal's `sequencedAt` to say how far behind it is,
/// and nothing it measures is sequenced.
#[allow(clippy::disallowed_methods)]
pub fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Registers `<prefix>_build_info`: 1, labelled with the package version and `extra`.
pub fn build_info(registry: &mut Registry, prefix: &str, version: &str, extra: &[(&'static str, &str)]) {
    let info = Family::<Labels, Gauge>::default();
    let mut labels: Labels = vec![("version", version.to_string())];
    labels.extend(extra.iter().map(|(k, v)| (*k, (*v).to_string())));
    info.get_or_create(&labels).set(1);
    registry.register(format!("{prefix}_build_info"), "1, labelled with what runs", info);
}

/// Nanoseconds as seconds, for gauges exported in seconds.
#[allow(clippy::float_arithmetic)]
pub fn seconds(nanos: u64) -> f64 {
    nanos as f64 / 1e9
}

/// A gauge of seconds (or any other fractional value).
pub type FloatGauge = Gauge<f64, std::sync::atomic::AtomicU64>;
