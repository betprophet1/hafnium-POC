//! A retail population large enough to find the gateway's limit, rather than the harness's.
//!
//! The obvious shape — one thread per customer, each blocking on a read with a timeout — costs a wake-up per
//! customer per poll interval whether or not anything arrived. At 24,576 customers on a 32-vCPU box that was a
//! load average of 6,465 on the *client* while the venue sat at 16%, so every number above about a thousand
//! customers measured the load generator (ADR-0002, the retail measurements). Threads are the wrong axis.
//!
//! So: a few workers, each owning thousands of sockets through `epoll`. A worker wakes only for sockets that
//! have something, which is what makes the cost follow the traffic instead of the population. `libc` rather
//! than a new dependency, because the whole of it is three calls.
//!
//! Each customer is two sockets, which is what one really is: a market-data subscription on the public half
//! and a logged-in session on the private half. They read the book continuously and send an order every so
//! often.

use crate::session::{monotonic, Session};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use serde_json::json;
use std::thread;
use std::time::{Duration, Instant};
use venue_link::wire::{Error, Ids};

/// What one retail population did.
#[derive(Default)]
pub struct Outcome {
    /// Market-data messages the population was pushed. The number this whole role exists to produce.
    pub received: u64,
    pub orders: u64,
    pub fills: u64,
    /// Round trips of the orders they sent, in nanoseconds.
    pub times: Vec<u64>,
    /// Customers whose session failed, and the first reason.
    pub dropped: usize,
    pub first_error: Option<String>,
}

pub struct Config {
    pub public: String,
    pub gateway: String,
    /// One token per customer: each is its own account, as a venue's customers are.
    pub tokens: Vec<String>,
    pub instruments: Vec<u64>,
    pub deadline: Instant,
    pub order_every: Duration,
    pub tick: u64,
    /// How many workers share the population. Sockets per worker is `2 * tokens / workers`.
    pub workers: usize,
}

/// One customer, as a worker sees it.
struct Client {
    market: Session,
    session: Session,
    ids: Ids,
    next_order: Instant,
    /// When the outstanding order was sent, if there is one. At most one at a time: a customer waits for the
    /// reply like any other client, and a pipelining one would be measuring something else.
    pending: Option<Instant>,
    next_heartbeat: Instant,
}

/// Which socket of which customer a readiness event refers to, packed into the token.
const MARKET: usize = 0;
const PRIVATE: usize = 1;
const fn key(index: usize, which: usize) -> Token {
    Token(index << 1 | which)
}

/// Runs the population and returns what it did. Blocks until `deadline`.
pub fn run(cfg: Config) -> Result<Outcome, Error> {
    if cfg.tokens.is_empty() {
        return Ok(Outcome::default());
    }
    let workers = cfg.workers.max(1).min(cfg.tokens.len());
    let per = cfg.tokens.len().div_ceil(workers);

    let handles: Vec<_> = cfg
        .tokens
        .chunks(per)
        .map(|chunk| {
            let tokens: Vec<String> = chunk.to_vec();
            let (public, gateway) = (cfg.public.clone(), cfg.gateway.clone());
            let instruments = cfg.instruments.clone();
            let (deadline, order_every, tick) = (cfg.deadline, cfg.order_every, cfg.tick);
            // Small stacks: a worker holds thousands of sessions but needs almost no stack itself.
            thread::Builder::new()
                .stack_size(1024 * 1024)
                .spawn(move || worker(&public, &gateway, &tokens, &instruments, deadline, order_every, tick))
        })
        .collect::<std::io::Result<Vec<_>>>()?;

    let mut out = Outcome::default();
    for h in handles {
        match h.join().map_err(|_| Error::from("a retail worker panicked"))? {
            Ok(part) => {
                out.received += part.received;
                out.orders += part.orders;
                out.fills += part.fills;
                out.times.extend(part.times);
                out.dropped += part.dropped;
                if out.first_error.is_none() {
                    out.first_error = part.first_error;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// One worker: connect its share, then read whatever epoll says is ready until the deadline.
#[allow(clippy::too_many_lines)]
fn worker(
    public: &str,
    gateway: &str,
    tokens: &[String],
    instruments: &[u64],
    deadline: Instant,
    order_every: Duration,
    tick: u64,
) -> Result<Outcome, Error> {
    let mut out = Outcome::default();
    let mut poll = Poll::new()?;
    let mut clients: Vec<Option<Client>> = Vec::with_capacity(tokens.len());
    let topics: Vec<String> = instruments.iter().map(|i| format!("book.{i}")).collect();
    // The most popular book is where retail is, which is the point of the exponential weighting elsewhere.
    let instrument = instruments.first().copied().unwrap_or_default();

    // Connecting is blocking: the handshake, the subscribe and the login are all request/response, and
    // spreading the starts a little keeps thousands of logins from arriving as one burst the gateway's
    // logon limit would refuse.
    for (i, token) in tokens.iter().enumerate() {
        let connect = || -> Result<Client, Error> {
            let mut market = Session::watch(public)?;
            market.send(&json!({"op": "subscribe", "topics": topics, "depth": 10}))?;
            let (session, _welcome) = Session::open(gateway, token, 120)?;
            Ok(Client {
                market,
                session,
                ids: Ids::new('r')?,
                // Spread across the interval, so the population's orders arrive evenly rather than in waves.
                next_order: monotonic() + order_every * (i as u32 % 16) / 16,
                pending: None,
                next_heartbeat: monotonic() + Duration::from_secs(30),
            })
        };
        match connect() {
            Ok(mut c) => {
                c.market.set_nonblocking(true)?;
                c.session.set_nonblocking(true)?;
                let index = clients.len();
                let registry = poll.registry();
                registry.register(&mut SourceFd(&c.market.fd()), key(index, MARKET), Interest::READABLE)?;
                registry.register(&mut SourceFd(&c.session.fd()), key(index, PRIVATE), Interest::READABLE)?;
                clients.push(Some(c));
            }
            Err(e) => {
                out.dropped += 1;
                out.first_error.get_or_insert_with(|| e.to_string());
                clients.push(None);
            }
        }
    }

    let mut events = Events::with_capacity(1024);
    let mut next_sweep = monotonic();
    while monotonic() < deadline {
        if let Err(e) = poll.poll(&mut events, Some(Duration::from_millis(50))) {
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(format!("poll: {e}").into());
        }
        for event in &events {
            let token = event.token().0;
            let (index, which) = (token >> 1, token & 1);
            let Some(Some(client)) = clients.get_mut(index) else { continue };
            let mut failed = None;
            if which == MARKET {
                // Drain: one readiness notification can cover several frames, and edge cases aside, a frame
                // left in the buffer would not be reported again.
                loop {
                    match client.market.try_read() {
                        Ok(Some(_)) => out.received += 1,
                        Ok(None) => break,
                        Err(e) => {
                            failed = Some(e.to_string());
                            break;
                        }
                    }
                }
            } else {
                loop {
                    match client.session.try_read() {
                        Ok(Some(m)) => {
                            if m["type"] == "reply" {
                                if let Some(sent) = client.pending.take() {
                                    out.times.push(sent.elapsed().as_nanos() as u64);
                                }
                                out.fills += m["result"]["executions"]
                                    .as_array()
                                    .map_or(0, |e| e.iter().filter(|x| x["type"] == json!("fill")).count())
                                    as u64;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            failed = Some(e.to_string());
                            break;
                        }
                    }
                }
            }
            if let Some(e) = failed {
                out.dropped += 1;
                out.first_error.get_or_insert(e);
                clients[index] = None;
            }
        }

        // Orders and heartbeats are time-driven, not readiness-driven, so they get their own pass. Every
        // quarter second over the whole share, which is nothing next to the reads.
        let now = monotonic();
        if now < next_sweep {
            continue;
        }
        next_sweep = now + Duration::from_millis(250);
        for slot in &mut clients {
            let Some(client) = slot else { continue };
            let mut failed = None;
            if now >= client.next_heartbeat {
                client.next_heartbeat = now + Duration::from_secs(30);
                if let Err(e) = client.market.send(&json!({"op": "heartbeat"})) {
                    failed = Some(e.to_string());
                }
                if let Err(e) = client.session.send(&json!({"op": "heartbeat"})) {
                    failed = Some(e.to_string());
                }
            }
            if failed.is_none() && client.pending.is_none() && now >= client.next_order {
                client.next_order = now + order_every;
                let order = json!({
                    "op": "order", "clientOrderId": client.ids.fresh(), "instrumentId": instrument.to_string(),
                    "action": "buy", "outcome": "yes", "price": {"tick": tick}, "quantity": 1,
                    "timeInForce": "IOC", "id": "retail"
                });
                match client.session.send(&order) {
                    Ok(()) => {
                        client.pending = Some(now);
                        out.orders += 1;
                    }
                    Err(e) => failed = Some(e.to_string()),
                }
            }
            if let Some(e) = failed {
                out.dropped += 1;
                out.first_error.get_or_insert(e);
                *slot = None;
            }
        }
    }
    Ok(out)
}
