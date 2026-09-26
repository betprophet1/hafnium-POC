//! `venue-sim gwload`: end-to-end throughput and round-trip latency through the gateway's WebSocket private
//! endpoint (`/v4/ws_private`) — the gateway-facing counterpart to `venue-node load`'s raw-ingress measurement,
//! same shape and same reporting style, so the two numbers are directly comparable.
//!
//! Each connection logs in once, places one resting order, then amends it back and forth between two
//! quantities, one request at a time, until the deadline. What is measured here is the gateway's own added
//! cost on top of the raw-ingress path: WebSocket framing, login/session bookkeeping, JSON parsing,
//! flood-limit checks and audit logging, before the input ever reaches the core.
//!
//! With `--instruments N` (N > 1), the load spreads across N synthetic markets instead of the one named by
//! `--instrument`: each connection picks one at the start of its run, weighted so instrument 0 gets roughly
//! twice instrument 1's share, which gets roughly twice instrument 2's, and so on -- the "few busy, most quiet"
//! shape real venues see (blueprint §1's power-law volume), not an even spread. This is for finding out whether
//! a bottleneck is instrument-specific (`Gateway`'s `hub: Mutex<Hub>`, `api.rs`, is not sharded by instrument at
//! all, so this is a real question to ask empirically rather than assume).

use crate::bots::Rng;
use crate::session::Session;
use serde_json::{json, Value};
use std::thread;
use std::time::{Duration, Instant};
use venue_link::wire::{self, Error, Ids, Ingress};

/// Benchmark timing only; nothing measured here reaches the journal.
#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

pub struct Config {
    pub gateway: String,
    pub token: String,
    pub instrument: u64,
    pub connections: usize,
    pub seconds: u64,
    /// Where to list synthetic instruments (bypassing the gateway for this one setup step, the way `venue-sim
    /// seed`/`workload` already do): the core's raw ingress. Required only when `instruments > 1`.
    pub ingress: Option<String>,
    /// How many synthetic markets to spread the load across. 1 (the default) changes nothing: every connection
    /// trades `instrument` exactly as before.
    pub instruments: usize,
}

/// Instrument rank `k`'s share of the connection population, out of a population where rank 0 is unscaled:
/// integer-only (the workspace forbids float arithmetic), halving every rank so the shape is exponential
/// without needing division to stay proportional.
fn weight(k: usize) -> u64 {
    (1_u64 << 32).checked_shr(k.min(32) as u32).unwrap_or(0).max(1)
}

/// Draws one instrument, weighted so index 0 is the most popular. `instruments` is assumed ranked
/// most-popular-first, which is exactly the order `list_instruments` returns them in.
fn pick_instrument(rng: &mut Rng, instruments: &[u64]) -> u64 {
    let total: u64 = (0..instruments.len()).map(weight).sum();
    let mut at = rng.below(total.max(1));
    for (k, &id) in instruments.iter().enumerate() {
        let w = weight(k);
        if at < w {
            return id;
        }
        at -= w;
    }
    instruments[instruments.len() - 1]
}

/// A `CommandApplied` result's own verdict, so a rejected setup command fails loudly here instead of surfacing
/// later as a confusing "no such market" from `drive`.
fn applied(results: &[Value], what: &str) -> Result<(), Error> {
    let outcome = results.iter().find(|r| r["msg"] == "CommandApplied").map(|r| r["outcome"].clone());
    match outcome {
        Some(Value::String(s)) if s == "APPLIED" => Ok(()),
        other => Err(format!("{what}: {other:?}").into()),
    }
}

/// Lists `n` synthetic binary markets directly on the core's ingress, reusing tick table 1 -- the cent ladder
/// `venue-sim seed` already defines with the same content, so nothing here redefines it (this assumes `seed`
/// ran first, same as `gwload`'s single-instrument mode already assumes the demo catalogue was seeded). Returns
/// each instrument's ID, ranked most-popular first: `pick_instrument`'s weighting assumes that order.
///
/// Idempotent across repeated calls against the same node (a sweep of runs, each asking for the same `n`, needs
/// this only once): a rejected `DefineEvent` is read as "a previous call already listed this one", and listing
/// and opening are skipped rather than retried, since nothing else in a test run claims this event range.
fn list_instruments(ingress_addr: &str, n: usize) -> Result<Vec<u64>, Error> {
    let mut ingress = Ingress::new(ingress_addr);
    let mut ids = Ids::new('x')?;
    let mut listed = Vec::new();
    for i in 0..n {
        let event = 950_000 + i as u64;
        let instrument = event * 10;
        let fields = json!({"eventId": event, "stateCount": 2, "scheduledStart": 0});
        let results = ingress.send(wire::command("DefineEvent", ids.fresh(), fields))?;
        let newly_defined = results.iter().any(|r| r["msg"] == "CommandApplied" && r["outcome"] == "APPLIED");
        if newly_defined {
            let listing = json!({
                "instrumentId": instrument, "eventId": event, "instrumentClass": "BINARY", "stateMask": 2,
                "tickTableId": 1, "lotStep": 1, "allocationPolicy": "PRICE_TIME", "positionLimit": 0, "legs": []
            });
            applied(&ingress.send(wire::command("ListInstrument", ids.fresh(), listing))?, "ListInstrument")?;
            let open = json!({"instrumentId": instrument, "state": "OPEN"});
            applied(&ingress.send(wire::command("SetMarketState", ids.fresh(), open))?, "SetMarketState")?;
        }
        listed.push(instrument);
    }
    Ok(listed)
}

/// One connection's run: log in, place its order on `instrument`, then amend until `deadline`. Returns each
/// round trip in nanoseconds.
fn drive(gateway: &str, token: &str, instrument: u64, index: usize, deadline: Instant) -> Result<Vec<u64>, Error> {
    let (mut session, _welcome) = Session::open(gateway, token, 30)?;
    let mut ids = Ids::new('g')?;
    // A tick nothing else is likely to be quoting at, so the order never trades and the amend loop measures
    // round trips rather than fills.
    let mut place = json!({
        "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(), "action": "buy",
        "outcome": "yes", "price": {"tick": 4}, "quantity": 10, "postOnly": true
    });
    place["id"] = json!(format!("place{index}"));
    let placed = session.request(place)?;
    if placed["result"]["status"] != "accepted" {
        return Err(format!("connection {index}: order not accepted: {placed}").into());
    }
    let order_id = placed["result"]["orderId"]
        .as_str()
        .ok_or_else(|| Error::from("no orderId in the accepted reply"))?
        .to_string();

    let mut times = Vec::new();
    let mut high = true;
    while now() < deadline {
        let quantity = if high { 11 } else { 10 };
        high = !high;
        let t0 = now();
        let reply =
            session.request(json!({"op": "amend", "orderId": order_id, "price": {"tick": 4}, "quantity": quantity}))?;
        times.push(t0.elapsed().as_nanos() as u64);
        if reply["type"] == "error" {
            return Err(format!("connection {index}: amend refused: {reply}").into());
        }
    }
    Ok(times)
}

fn micros(ns: u64) -> String {
    format!("{}.{:03}", ns / 1000, ns % 1000)
}

pub fn run(cfg: Config) -> Result<(), Error> {
    let instruments: Vec<u64> = if cfg.instruments > 1 {
        let ingress = cfg.ingress.as_deref().ok_or("--instruments > 1 needs --ingress to list them on")?;
        let listed = list_instruments(ingress, cfg.instruments)?;
        eprintln!("listed {} synthetic instruments: {:?}..{:?}", listed.len(), listed.first(), listed.last());
        listed
    } else {
        vec![cfg.instrument]
    };

    let started = now();
    let deadline = started + Duration::from_secs(cfg.seconds);
    let handles: Vec<_> = (0..cfg.connections)
        .map(|i| {
            let (gateway, token, instruments) = (cfg.gateway.clone(), cfg.token.clone(), instruments.clone());
            thread::spawn(move || {
                // Seeded from the connection index, so a run is reproducible.
                let mut rng = Rng::new(i as u64 + 1);
                let instrument = pick_instrument(&mut rng, &instruments);
                drive(&gateway, &token, instrument, i, deadline).map_err(|e| e.to_string())
            })
        })
        .collect();

    let mut all = Vec::new();
    for h in handles {
        all.extend(h.join().map_err(|_| Error::from("a connection thread panicked"))??);
    }
    let elapsed_ms = started.elapsed().as_millis().max(1);
    all.sort_unstable();
    let p = |q: usize| micros(all.get(all.len().saturating_sub(1) * q / 100_000).copied().unwrap_or(0));
    println!(
        "{} connection(s) over {} instrument(s), {} amendments in {elapsed_ms} ms: {} messages/s; round trip \u{b5}s p50 {} p90 {} p99 {} p99.9 {} max {}",
        cfg.connections,
        instruments.len(),
        all.len(),
        all.len() as u128 * 1000 / elapsed_ms,
        p(50_000),
        p(90_000),
        p(99_000),
        p(99_900),
        micros(all.last().copied().unwrap_or(0)),
    );
    Ok(())
}
