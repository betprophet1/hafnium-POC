//! `venue-sim gwload`: end-to-end throughput and round-trip latency through the gateway's WebSocket private
//! endpoint (`/v4/ws_private`) — the gateway-facing counterpart to `venue-node load`'s raw-ingress measurement,
//! same shape and same reporting style, so the two numbers are directly comparable.
//!
//! Each connection logs in once, places one resting order, then amends it back and forth between two
//! quantities, one request at a time, until the deadline. What is measured here is the gateway's own added
//! cost on top of the raw-ingress path: WebSocket framing, login/session bookkeeping, JSON parsing,
//! flood-limit checks and audit logging, before the input ever reaches the core.

use crate::session::Session;
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};
use venue_link::wire::{Error, Ids};

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
}

/// One connection's run: log in, place its order, then amend until `deadline`. Returns each round trip in
/// nanoseconds.
fn drive(cfg: &Config, index: usize, deadline: Instant) -> Result<Vec<u64>, Error> {
    let (mut session, _welcome) = Session::open(&cfg.gateway, &cfg.token, 30)?;
    let mut ids = Ids::new('g')?;
    // A tick nothing else is likely to be quoting at, so the order never trades and the amend loop measures
    // round trips rather than fills.
    let mut place = json!({
        "op": "order", "clientOrderId": ids.fresh(), "instrumentId": cfg.instrument.to_string(), "action": "buy",
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
    let started = now();
    let deadline = started + Duration::from_secs(cfg.seconds);
    let handles: Vec<_> = (0..cfg.connections)
        .map(|i| {
            let cfg = Config {
                gateway: cfg.gateway.clone(),
                token: cfg.token.clone(),
                instrument: cfg.instrument,
                connections: cfg.connections,
                seconds: cfg.seconds,
            };
            thread::spawn(move || drive(&cfg, i, deadline).map_err(|e| e.to_string()))
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
        "{} connection(s), {} amendments in {elapsed_ms} ms: {} messages/s; round trip \u{b5}s p50 {} p90 {} p99 {} p99.9 {} max {}",
        cfg.connections,
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
