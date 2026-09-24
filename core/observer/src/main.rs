//! `venue-observer`: follows a node's feed like any downstream reader and derives, from the journal and its results,
//! what the venue's dashboards and alerts need: per-hop latency, flow, trading, fees, lifecycle, capacity headroom and
//! market quality (deploy/observability/metrics.md). It also answers read-only operator questions from its projection.
//! It never talks to the engine and never writes to the node: the engine gains no metrics code (blueprint §14).

mod derive;
mod operator;

use derive::Derived;
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::Instant;
use venue_link::catalogue::{capacity_profile, Catalogue};
use venue_link::view::View;
use venue_link::wire::{self, Error};
use venue_telemetry::hdr::Hdr;
use venue_telemetry::log;
use venue_telemetry::prometheus_client::metrics::family::Family;
use venue_telemetry::prometheus_client::registry::Registry;

const USAGE: &str = "usage:
  venue-observer --feed ADDR [--listen 127.0.0.1:9103] [--catalogue FILE] [--capacity small|launch|scale]
                 [--max-market-series 500] [--from-view FILE]

Follows the node's feed at --feed and serves, on --listen, Prometheus metrics at /metrics and read-only operator
endpoints: /v1/markets, /v1/markets/{id}?depth=N, /v1/accounts/{id}, /v1/latency and /healthz. --listen is an internal
address for monitoring and operators, never one clients reach. --catalogue labels markets with their titles.
--capacity names the node's capacity profile, whose table sizes capacity use is measured against. Per-market series
stop at --max-market-series markets. --from-view names the reader's half of a snapshot pair (venue-view snapshot),
so the projection starts holding the venue as it was then rather than nothing; a venue founded on a snapshot has
no inputs in its journal to send one that asks for everything. Every flag can also be given as VENUE_<FLAG>, for example VENUE_FEED;
VENUE_LOG_FORMAT=json writes the log as JSON lines.";

/// The projection the operator endpoints read, and the catalogue that names its markets.
pub struct State {
    pub view: View,
    pub catalogue: Option<Catalogue>,
}

/// The monotonic clock, for pacing gauges and merging histograms. The workspace bans clocks because the engine's only
/// clock is the journal; the observer is outside it and nothing it measures is sequenced.
#[allow(clippy::disallowed_methods)]
fn monotonic() -> Instant {
    Instant::now()
}

fn flags(args: &[String]) -> Result<BTreeMap<String, String>, Error> {
    let mut values = BTreeMap::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let key = flag.strip_prefix("--").ok_or_else(|| format!("unexpected argument {flag}\n{USAGE}"))?;
        let value = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
        values.insert(key.to_string(), value.clone());
    }
    Ok(values)
}

fn main() -> ExitCode {
    log::init("venue-observer");
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match flags(&args).and_then(|f| run(&f)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error("stopped", &[("reason", &e)]);
            ExitCode::FAILURE
        }
    }
}

fn run(flags: &BTreeMap<String, String>) -> Result<(), Error> {
    let get = |key: &str| {
        flags
            .get(key)
            .cloned()
            .or_else(|| std::env::var(format!("VENUE_{}", key.to_uppercase().replace('-', "_"))).ok())
    };
    let feed = get("feed").ok_or_else(|| format!("--feed (or VENUE_FEED) is required\n{USAGE}"))?;
    let listen = get("listen").unwrap_or_else(|| "127.0.0.1:9103".to_string());
    let catalogue = get("catalogue").filter(|c| !c.is_empty()).map(|c| Catalogue::load(&c)).transpose()?;
    let capacity = get("capacity").unwrap_or_else(|| "small".to_string());
    let profile = capacity_profile(&capacity)?;
    let max_market_series: usize = get("max-market-series").map_or(Ok(500), |m| m.parse())?;

    let mut registry = Registry::default();
    venue_telemetry::build_info(&mut registry, "venue_observer", env!("CARGO_PKG_VERSION"), &[("capacity", &capacity)]);
    let hops = Family::<Vec<(&'static str, String)>, Hdr>::default();
    registry.register("venue_hop_latency_seconds", "Journal-derived hops (HdrHistogram)", hops.clone());
    let hop = hops.get_or_create_owned(&vec![("hop", "sequenced_to_feed".into()), ("clock", "software".into())]);
    let mut derived =
        Derived::register(&mut registry, venue_telemetry::wall_ns(), Some(profile), max_market_series, &hop);
    // The reader's half of a snapshot pair, for an observer watching a venue founded on a snapshot rather than
    // on genesis: that venue's journal holds no inputs at all, so a projection that asked for everything would
    // be sent nothing and report on an empty venue. Its sequence is where the resume handshake carries on from.
    let view = match get("from-view").filter(|p| !p.is_empty()) {
        Some(path) => View::load(std::path::Path::new(&path)).map_err(|e| format!("--from-view {path}: {e}"))?,
        None => View::default(),
    };
    let state = Arc::new(Mutex::new(State { view, catalogue }));
    let bound = venue_telemetry::serve::spawn(&listen, registry, Some(operator::handler(Arc::clone(&state), hop)))?;
    log::info("serving metrics and operator endpoints", &[("listen", &bound), ("feed", &feed)]);

    let follower = thread::Builder::new().name("feed".into()).spawn(move || {
        let reconnect = Arc::new(Mutex::new(false));
        let reconnecting = Arc::clone(&reconnect);
        wire::follow_reporting(
            &feed,
            || {
                *reconnecting.lock().unwrap_or_else(PoisonError::into_inner) = true;
                // Carries on from the sequence this projection already holds, so a reconnect that finds the
                // same journal still there costs only what happened while disconnected.
                state.lock().unwrap_or_else(PoisonError::into_inner).view.sequence
            },
            || {
                // The node could not honour that position — this projection is of a history that never happened.
                state.lock().unwrap_or_else(PoisonError::into_inner).view = View::default();
            },
            |unit| {
                if std::mem::take(&mut *reconnect.lock().unwrap_or_else(PoisonError::into_inner)) {
                    derived.reconnected();
                }
                let mut state = state.lock().unwrap_or_else(PoisonError::into_inner);
                let State { view, catalogue } = &mut *state;
                derived.apply(view, catalogue.as_ref(), &unit, venue_telemetry::wall_ns(), monotonic());
            },
            |e| log::warn("feed connection", &[("feed", &feed), ("error", &e)]),
        )
    })?;
    let _ = follower.join();
    Err("the feed follower stopped".into())
}
