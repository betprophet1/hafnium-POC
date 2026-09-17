//! `venue-node load`: end-to-end throughput and round-trip latency through a node's ingress.
//!
//! Each connection places one resting order for the funded market-maker account, then amends it back and forth
//! between two quantities, one request at a time, until the time is up. An increase re-queues the order and a
//! reduction keeps its place, so both amendment paths (E-12.4, E-12.5) carry load, and no client order ID is used
//! after setup, so the duplicate table (E-9.2) does not fill. Both amendments are encoded once; replies are read
//! but not decoded, so the client costs almost nothing. What is measured is the node's whole path: stamping,
//! journaling, the engine, publishing to feed readers and the reply.

use crate::frames::{read_frame, write_frame, MAX_FRAME};
use crate::node::Error;
use serde_json::{json, Value};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpStream;
use std::thread;
use std::time::{Duration, Instant};
use venue_harness::codec;

/// 5¢ YES, below anything the bots quote, so the order never trades.
const TICK: u64 = 4;

/// Benchmark timing only; nothing measured here reaches a journal.
#[allow(clippy::disallowed_methods)]
fn now() -> Instant {
    Instant::now()
}

fn zeroed() -> Value {
    json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0})
}

struct Connection {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    frame: Vec<u8>,
}

impl Connection {
    fn open(addr: &str) -> Result<Self, Error> {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        Ok(Self { reader: BufReader::new(stream.try_clone()?), writer: BufWriter::new(stream), frame: Vec::new() })
    }

    /// Sends one input and reads its results up to the empty frame, keeping them only if asked.
    fn round_trip(&mut self, input: &[u8], keep: bool) -> Result<Vec<Value>, Error> {
        write_frame(&mut self.writer, input)?;
        self.writer.flush()?;
        let mut results = Vec::new();
        loop {
            match read_frame(&mut self.reader, &mut self.frame)? {
                Some(0) => return Ok(results),
                Some(n) if keep => results.push(codec::decode(&self.frame[..n])?),
                Some(_) => {}
                None => return Err("the node closed the connection".into()),
            }
        }
    }
}

fn encode(input: &Value) -> Result<Vec<u8>, Error> {
    let mut buf = vec![0u8; MAX_FRAME];
    let n = codec::encode(input, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// One connection's run: set up its order, then amend until `deadline`. Returns each round trip in nanoseconds.
fn drive(addr: &str, index: usize, deadline: Instant, account: u64, instrument: u64) -> Result<Vec<u64>, Error> {
    let mut c = Connection::open(addr)?;
    let session = 900_000 + index as u64;
    let order = json!({
        "msg": "NewOrder", "record": zeroed(), "sessionId": session, "accountId": account,
        "clientOrderId": format!("load-{}-{index}", std::process::id()), "instrumentId": instrument, "side": "BUY",
        "enteredContract": "YES", "tick": TICK, "quantity": 10, "timeInForce": "GTC", "expiresAt": null,
        "atEventStart": "KEEP", "flags": [], "protectionTicks": 0, "spendCapCents": null, "enteredStakeCents": null,
        "clientOrderGroup": null
    });
    let results = c.round_trip(&encode(&order)?, true)?;
    let accepted = results.iter().find(|r| r["msg"] == "OrderAccepted").ok_or_else(|| {
        format!(
            "connection {index}: the resting order was not accepted: {}\nthe account and market are --account {account} --instrument {instrument}; a seeded node generates its instrument IDs",
            Value::Array(results.clone())
        )
    })?;
    let order_id = accepted["orderId"].clone();
    let amend = |quantity: u64| {
        encode(&json!({
            "msg": "AmendOrder", "record": zeroed(), "sessionId": session, "accountId": account,
            "instrumentId": instrument, "orderId": order_id, "newTick": null, "newQuantity": quantity,
            "atEventStart": "UNCHANGED"
        }))
    };
    let (up, down) = (amend(20)?, amend(10)?);
    let first = c.round_trip(&up, true)?;
    if !first.iter().any(|r| r["msg"] == "OrderAmended") {
        return Err(format!("connection {index}: the amendment was not applied: {}", Value::Array(first)).into());
    }
    let mut samples = Vec::new();
    let mut increase = false;
    while now() < deadline {
        let input = if increase { &up } else { &down };
        let t = now();
        c.round_trip(input, false)?;
        samples.push(t.elapsed().as_nanos() as u64);
        increase = !increase;
    }
    Ok(samples)
}

fn micros(ns: u64) -> String {
    format!("{}.{:03}", ns / 1000, ns % 1000)
}

pub fn load(addr: &str, connections: usize, seconds: u64, account: u64, instrument: u64) -> Result<(), Error> {
    let started = now();
    let deadline = started + Duration::from_secs(seconds);
    let workers: Vec<_> = (0..connections)
        .map(|index| {
            let addr = addr.to_string();
            thread::spawn(move || drive(&addr, index, deadline, account, instrument).map_err(|e| e.to_string()))
        })
        .collect();
    let mut all = Vec::new();
    for worker in workers {
        all.extend(worker.join().map_err(|_| "a load connection panicked")??);
    }
    let elapsed = started.elapsed();
    all.sort_unstable();
    let p = |q: usize| micros(all.get(all.len().saturating_sub(1) * q / 100_000).copied().unwrap_or(0));
    println!(
        "{connections} connection(s), {} amendments in {} ms: {} messages/s; round trip µs p50 {} p90 {} p99 {} p99.9 {} max {}",
        all.len(),
        elapsed.as_millis(),
        all.len() as u128 * 1000 / elapsed.as_millis().max(1),
        p(50_000),
        p(90_000),
        p(99_000),
        p(99_900),
        micros(all.last().copied().unwrap_or(0))
    );
    Ok(())
}
