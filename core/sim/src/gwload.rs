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
    /// Set to give connection `i` the token `{prefix}{i}` -- one account each -- instead of putting every
    /// connection on `token`. A venue does not see many connections from one account, and the gateway's own cost
    /// is quadratic in how many it does: every message for an account is copied to each of its live sessions
    /// (`Hub::send_to_account`), so N connections sharing one account make N times the messages per request that
    /// N connections on their own accounts would. Measuring the gateway means not asking it to do that.
    /// `venue-sim catalogue --accounts N` writes a catalogue holding the accounts these tokens name.
    pub token_prefix: Option<String>,
    /// What the connections do. `Amend` is the original measurement: one resting order per connection at a tick
    /// nothing trades at, amended back and forth, so nothing ever matches. `Trade` is a venue's own shape --
    /// makers resting liquidity and takers crossing it -- so the matching, the executions and the position and
    /// sleeve updates a fill causes are all in the measurement rather than absent from it.
    pub mode: Mode,
    /// In `Trade`, how many of the connections rest liquidity; the rest take it. Defaults to a quarter.
    pub makers: Option<usize>,
    /// In `Trade`, how many orders a non-maker connection sends for each one that crosses. 1 means every order
    /// trades, which was the original behaviour and is far busier in fills than any real venue; 10 (the
    /// default) means nine rest near the touch and are cancelled again for every one that matches. This decides
    /// the mix of results the feed carries -- fills against accepts, book levels and cancels -- so it decides
    /// what a measurement taken on this load is a measurement of.
    pub orders_per_trade: u64,
    /// Connections that only watch: they subscribe to the book on the *public* half and read what comes. Market
    /// data is served by the other half, with its own hub, feed thread and loops, so a run without any of these
    /// leaves all of that idle and measures only order entry.
    pub watchers: usize,
    /// Retail clients: the population a venue actually has most of. Each holds a market-data subscription on
    /// the public half *and* a logged-in session on the private half, reads the book continuously, and sends
    /// one order every `retail_order_seconds`. Two sockets each, which is what a real client costs.
    ///
    /// This is a different question from `connections`. Order entry is dominated by a handful of makers, but
    /// conflation caps a topic's *rate*, not its *fan-out*: every one of the ten snapshots a second is copied
    /// to every subscriber, so the outbound work is linear in this number while the inbound work is not. At
    /// 256 subscribers market data looked free (ADR-0002); whether that holds at thousands is the thing this
    /// exists to answer rather than extrapolate.
    pub retail: usize,
    /// How often a retail client sends an order. Mostly listening means this is seconds, not milliseconds.
    pub retail_order_seconds: u64,
    /// Where the public half is, for `watchers`. Defaults to `gateway`, which is right when one process serves
    /// both halves.
    pub public_gateway: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Amend,
    Trade,
}

/// The tick makers rest on and takers cross at. Away from where anything else quotes, so the two sides meet each
/// other rather than whatever else the venue happens to be carrying.
const TRADE_TICK: u64 = 50;
/// What a maker keeps resting, in contracts. Amended back up to this, so the book never empties under takers.
///
/// Sized to what the takers actually consume between amends, not to "large". It was 1,000,000, which at tick
/// 50 reserves about half of a load account's 100,000,000 cents; a maker re-posting before the previous order
/// was fully out then hit `insufficient_sleeve` and killed the run. Eight-second runs never reached it, which
/// is its own argument for measuring over a minute.
const MAKER_SIZE: u64 = 10_000;

/// What one connection did, so a run can report fills and messages as well as round trips.
#[derive(Default)]
struct Outcome {
    /// Round trips, in nanoseconds, of whatever requests this connection made.
    times: Vec<u64>,
    /// Executions this connection was told about in its own replies.
    fills: u64,
    /// Messages a watcher was pushed.
    received: u64,
    /// New orders this connection sent, and how many of them matched anything. Reported as a ratio so a run
    /// says what mix it actually achieved rather than what it was asked for.
    orders: u64,
    traded: u64,
    /// Cancels sent to pull a resting order before replacing it.
    cancels: u64,
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
    let record = results.iter().find(|r| r["msg"] == "CommandApplied");
    let outcome = record.map(|r| r["outcome"].clone());
    match outcome {
        Some(Value::String(s)) if s == "APPLIED" => Ok(()),
        // The reason as well as the outcome: "REJECTED" on its own says nothing about what to change.
        _ => Err(format!("{what}: {}", record.map_or_else(|| "no result".to_string(), ToString::to_string)).into()),
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

/// Fills among a reply's executions. `executions` carries every kind -- an `accepted` for an order that
/// merely rested, a `cancelled`, an `amended` -- so counting the array counts order acknowledgements as
/// trades. It reported 189,264 "fills" for 80,233 taker orders that can each match at most once.
fn fills_in(reply: &Value) -> u64 {
    reply["result"]["executions"]
        .as_array()
        .map_or(0, |e| e.iter().filter(|x| x["type"] == json!("fill")).count() as u64)
}

/// A maker's run: rest a sell the takers can hit, then keep amending it back up to size. The amend is both the
/// request being measured and how the book is replenished, so the liquidity never runs out from under the takers.
fn drive_maker(gateway: &str, token: &str, instrument: u64, index: usize, deadline: Instant) -> Result<Outcome, Error> {
    let (mut session, _welcome) = Session::open(gateway, token, 30)?;
    let mut ids = Ids::new('m')?;
    let mut place = json!({
        "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(), "action": "sell",
        "outcome": "yes", "price": {"tick": TRADE_TICK}, "quantity": MAKER_SIZE, "postOnly": true
    });
    place["id"] = json!(format!("make{index}"));
    let placed = session.request(place)?;
    if placed["result"]["status"] != "accepted" {
        return Err(format!("maker {index}: order not accepted: {placed}").into());
    }
    let order_id = placed["result"]["orderId"]
        .as_str()
        .ok_or_else(|| Error::from("no orderId in the accepted reply"))?
        .to_string();

    let mut out = Outcome::default();
    while now() < deadline {
        let t0 = now();
        let reply = session.request(
            json!({"op": "amend", "orderId": order_id, "price": {"tick": TRADE_TICK}, "quantity": MAKER_SIZE}),
        )?;
        out.times.push(t0.elapsed().as_nanos() as u64);
        // A maker whose order has been taken in full has nothing left to amend; rest another.
        if reply["type"] == "error" || reply["result"]["status"] == json!("rejected") {
            let mut again = json!({
                "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(),
                "action": "sell", "outcome": "yes", "price": {"tick": TRADE_TICK}, "quantity": MAKER_SIZE,
                "postOnly": true
            });
            again["id"] = json!(format!("remake{index}"));
            let _ = session.request(again)?;
        }
        out.fills += fills_in(&reply);
    }
    Ok(out)
}

/// A taker's run: mostly quoting near the touch without trading, crossing once every `orders_per_trade`.
///
/// The first cut of this crossed on *every* request, so the load was one order to one trade at a single price
/// level. That is not what a venue carries: the great majority of what reaches an exchange rests, is amended
/// or is cancelled, and only a small fraction ever matches. It also quietly decides what is being measured,
/// because the results differ per message -- a trade means two `Fill`s, two accounts and their positions and
/// sleeves, while a resting order means an `OrderAccepted` and a `BookLevelChanged`. Measuring the feed path
/// on a one-to-one load weights it towards fills far past anything real (ADR-0002).
///
/// So: `orders_per_trade - 1` post-only buys a tick or three behind the makers' ask, each cancelled before the
/// next goes on, then one immediate-or-cancel at the ask that actually trades. The passive orders sit at three
/// ticks rather than one so the book has depth to move rather than a single level toggling.
///
/// The achieved ratio is counted and reported rather than assumed, because whether an order rests or trades is
/// the venue's decision, not this harness's.
fn drive_taker(
    gateway: &str,
    token: &str,
    instrument: u64,
    index: usize,
    deadline: Instant,
    orders_per_trade: u64,
) -> Result<Outcome, Error> {
    let (mut session, _welcome) = Session::open(gateway, token, 30)?;
    let mut ids = Ids::new('t')?;
    let mut out = Outcome::default();
    let mut n: u64 = 0;
    // The passive order currently resting, to be pulled before the next one goes on.
    let mut resting: Option<String> = None;
    while now() < deadline {
        n += 1;
        let crossing = orders_per_trade <= 1 || n.is_multiple_of(orders_per_trade);
        let order = if crossing {
            json!({
                "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(),
                "action": "buy", "outcome": "yes", "price": {"tick": TRADE_TICK}, "quantity": 1,
                "timeInForce": "IOC"
            })
        } else {
            // A tick or three behind the ask, so it rests instead of matching. Post-only, so if it ever would
            // cross the venue rejects it rather than silently turning this into another trade.
            let tick = TRADE_TICK - 1 - (n % 3);
            json!({
                "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(),
                "action": "buy", "outcome": "yes", "price": {"tick": tick}, "quantity": 1, "postOnly": true
            })
        };
        let mut order = order;
        order["id"] = json!(format!("take{index}"));
        let t0 = now();
        let reply = session.request(order)?;
        out.times.push(t0.elapsed().as_nanos() as u64);
        if reply["type"] == "error" {
            return Err(format!("taker {index}: order refused: {reply}").into());
        }
        out.orders += 1;
        let filled = fills_in(&reply);
        out.fills += filled;
        if filled > 0 {
            out.traded += 1;
        }
        if !crossing {
            // Pull the previous one and remember this one, so the book churns near the touch instead of
            // growing until the group's order capacity stops the run.
            if let Some(previous) = resting.replace(reply["result"]["orderId"].as_str().unwrap_or_default().to_string())
            {
                if !previous.is_empty() {
                    let t1 = now();
                    let _ = session.request(json!({"op": "cancel", "orderId": previous}))?;
                    out.times.push(t1.elapsed().as_nanos() as u64);
                    out.cancels += 1;
                }
            }
        }
    }
    Ok(out)
}

/// A watcher's run: subscribe to the book on the public half and read what the gateway pushes. Sends nothing
/// after the subscription, so what it costs the gateway is the conflated fan-out rather than order entry.
/// A retail client: listens to the book, and now and then trades.
///
/// Both halves at once, from one thread, because that is what one customer is -- a market-data subscription
/// and a session that can send an order -- and because the interesting cost is the pair. The thread spends
/// almost all of its time blocked in the market-data read, so thousands of these are cheap on the client side
/// provided their stacks are small; `spawn_small` is what makes that true.
fn drive_retail(
    public: &str,
    gateway: &str,
    token: &str,
    instruments: &[u64],
    deadline: Instant,
    order_every: Duration,
    tick: u64,
) -> Result<Outcome, Error> {
    let mut market = Session::watch(public)?;
    let topics: Vec<String> = instruments.iter().map(|i| format!("book.{i}")).collect();
    market.send(&json!({"op": "subscribe", "topics": topics, "depth": 10}))?;
    let (mut session, _welcome) = Session::open(gateway, token, 30)?;
    let mut ids = Ids::new('r')?;
    let mut out = Outcome::default();
    let mut last_order = now();
    // One book each, the most popular by construction: retail follows the crowd, and spreading these evenly
    // would contradict the whole point of the exponential weighting.
    let instrument = instruments.first().copied().unwrap_or_default();
    while now() < deadline {
        out.received += market.poll(Duration::from_millis(50))?.len() as u64;
        if last_order.elapsed() < order_every {
            continue;
        }
        last_order = now();
        let mut order = json!({
            "op": "order", "clientOrderId": ids.fresh(), "instrumentId": instrument.to_string(),
            "action": "buy", "outcome": "yes", "price": {"tick": tick}, "quantity": 1, "timeInForce": "IOC"
        });
        order["id"] = json!("retail");
        let t0 = now();
        let reply = session.request(order)?;
        out.times.push(t0.elapsed().as_nanos() as u64);
        if reply["type"] == "error" {
            return Err(format!("retail order refused: {reply}").into());
        }
        out.orders += 1;
        let filled = fills_in(&reply);
        out.fills += filled;
        if filled > 0 {
            out.traded += 1;
        }
    }
    Ok(out)
}

/// A thread with a small stack. The default is 8 MiB of reserved address space each, which is fine for tens of
/// threads and not for tens of thousands; these block on a socket read and need almost none of it.
fn spawn_small<F, T>(f: F) -> std::io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new().stack_size(256 * 1024).spawn(f)
}

fn drive_watcher(public: &str, instruments: &[u64], deadline: Instant) -> Result<Outcome, Error> {
    let mut market = Session::watch(public)?;
    let topics: Vec<String> = instruments.iter().map(|i| format!("book.{i}")).collect();
    market.send(&json!({"op": "subscribe", "topics": topics, "depth": 10}))?;
    let mut out = Outcome::default();
    while now() < deadline {
        out.received += market.poll(Duration::from_millis(50))?.len() as u64;
    }
    Ok(out)
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

    // A quarter of the connections rest liquidity by default, which keeps the book stocked for the rest without
    // spending most of the run on it. At least one, or the takers have nothing to trade against.
    let makers = match cfg.mode {
        Mode::Amend => 0,
        Mode::Trade => cfg.makers.unwrap_or((cfg.connections / 4).max(1)).min(cfg.connections),
    };

    // Sharing one account across connections costs the gateway a copy of every message per session of that
    // account, so what a run like that measures is mostly that fan-out. Say so rather than report the number as
    // if it were the gateway's.
    if cfg.token_prefix.is_none() && cfg.connections > 2 {
        eprintln!(
            "warning: all {} connections log in as one account ({}), which a venue does not see. Every message \
             for it is copied to each of its {} sessions, so this measures that fan-out as much as the gateway. \
             Give each connection its own account with --token-prefix (see `venue-sim catalogue`).",
            cfg.connections, cfg.token, cfg.connections
        );
    }

    let started = now();
    let deadline = started + Duration::from_secs(cfg.seconds);
    let handles: Vec<_> = (0..cfg.connections)
        .map(|i| {
            let token = match &cfg.token_prefix {
                Some(prefix) => format!("{prefix}{i}"),
                None => cfg.token.clone(),
            };
            let (gateway, instruments) = (cfg.gateway.clone(), instruments.clone());
            let (mode, makers, orders_per_trade) = (cfg.mode, makers, cfg.orders_per_trade.max(1));
            thread::spawn(move || {
                // Seeded from the connection index, so a run is reproducible.
                let mut rng = Rng::new(i as u64 + 1);
                let instrument = pick_instrument(&mut rng, &instruments);
                match mode {
                    Mode::Amend => drive(&gateway, &token, instrument, i, deadline)
                        .map(|times| Outcome { times, ..Outcome::default() }),
                    // Makers first, so the liquidity is resting before the takers start on it.
                    Mode::Trade if i < makers => drive_maker(&gateway, &token, instrument, i, deadline),
                    Mode::Trade => drive_taker(&gateway, &token, instrument, i, deadline, orders_per_trade),
                }
                .map_err(|e| e.to_string())
            })
        })
        .collect();

    let public = cfg.public_gateway.clone().unwrap_or_else(|| cfg.gateway.clone());
    let watching: Vec<_> = (0..cfg.watchers)
        .map(|_| {
            let (public, instruments) = (public.clone(), instruments.clone());
            spawn_small(move || drive_watcher(&public, &instruments, deadline).map_err(|e| e.to_string()))
        })
        .collect::<std::io::Result<Vec<_>>>()?;

    // Retail accounts come after the trading connections in the catalogue, so the two populations never share
    // one: `venue-sim catalogue --accounts N` must have been given at least `connections + retail`.
    let retailing: Vec<_> = (0..cfg.retail)
        .map(|i| {
            let (public, gateway) = (public.clone(), cfg.gateway.clone());
            let instruments = instruments.clone();
            let token = match &cfg.token_prefix {
                Some(prefix) => format!("{prefix}{}", cfg.connections + i),
                None => cfg.token.clone(),
            };
            let every = Duration::from_secs(cfg.retail_order_seconds.max(1));
            spawn_small(move || {
                drive_retail(&public, &gateway, &token, &instruments, deadline, every, TRADE_TICK)
                    .map_err(|e| e.to_string())
            })
        })
        .collect::<std::io::Result<Vec<_>>>()?;

    let mut all = Vec::new();
    let (mut fills, mut received) = (0_u64, 0_u64);
    let (mut orders, mut traded, mut cancels) = (0_u64, 0_u64, 0_u64);
    // A connection the gateway hangs up on is a result, not a reason to throw the run away. It disconnects a
    // client that falls too far behind what it has been queued, and a maker taking thousands of executions a
    // second is a genuine slow consumer of its own fills. Count them, report them, and measure the rest --
    // failing the whole run here means a 60-second measurement is lost to one dropped session.
    let mut dropped = 0_usize;
    let mut first_error: Option<String> = None;
    for h in handles {
        match h.join().map_err(|_| Error::from("a connection thread panicked"))? {
            Ok(out) => {
                all.extend(out.times);
                fills += out.fills;
                orders += out.orders;
                traded += out.traded;
                cancels += out.cancels;
            }
            Err(e) => {
                dropped += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    if all.is_empty() {
        let why = first_error.unwrap_or_else(|| "no connection completed".into());
        return Err(format!("every connection failed; first: {why}").into());
    }
    for h in watching {
        let out = h.join().map_err(|_| Error::from("a watcher thread panicked"))??;
        received += out.received;
    }
    // Retail is reported apart from the trading connections: mixing a client that sends one order every few
    // seconds into the same latency pool as a maker sending thousands would hide both.
    let mut retail_times = Vec::new();
    let (mut retail_orders, mut retail_fills, mut retail_dropped) = (0_u64, 0_u64, 0_usize);
    let mut retail_received = 0_u64;
    for h in retailing {
        match h.join().map_err(|_| Error::from("a retail thread panicked"))? {
            Ok(out) => {
                retail_received += out.received;
                retail_times.extend(out.times);
                retail_orders += out.orders;
                retail_fills += out.fills;
            }
            Err(e) => {
                retail_dropped += 1;
                first_error.get_or_insert(e);
            }
        }
    }
    let elapsed_ms = started.elapsed().as_millis().max(1);
    all.sort_unstable();
    let p = |q: usize| micros(all.get(all.len().saturating_sub(1) * q / 100_000).copied().unwrap_or(0));
    let what = match cfg.mode {
        Mode::Amend => "amendments",
        Mode::Trade => "requests",
    };
    print!(
        "{} connection(s) over {} instrument(s), {} {what} in {elapsed_ms} ms: {} messages/s; round trip \u{b5}s p50 {} p90 {} p99 {} p99.9 {} max {}",
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
    if cfg.mode == Mode::Trade {
        print!("; {makers} maker(s), {fills} fills, {} fills/s", u128::from(fills) * 1000 / elapsed_ms);
        // What mix the run actually got, not what it was asked for: whether an order rests or matches is the
        // venue's decision. `1:N` reads as one order in N having traded.
        let ratio = orders.checked_div(traded).unwrap_or(orders);
        print!("; {orders} taker orders, {traded} traded (1:{ratio}), {cancels} cancels");
    }
    if cfg.retail > 0 {
        retail_times.sort_unstable();
        let rp = |q: usize| {
            micros(retail_times.get(retail_times.len().saturating_sub(1) * q / 100_000).copied().unwrap_or(0))
        };
        // The market data they were pushed is the number this role exists to produce: conflation caps a
        // topic's rate, not its fan-out, so this is the term that grows with the population.
        print!(
            "; {} retail took {retail_received} messages, {}/s ({} each), sent {retail_orders} orders, \
             {retail_fills} fills, p50 {} p99 {}, {retail_dropped} dropped",
            cfg.retail,
            u128::from(retail_received) * 1000 / elapsed_ms,
            retail_received / (cfg.retail.max(1) as u64),
            rp(50_000),
            rp(99_000)
        );
    }
    if dropped > 0 {
        print!("; {dropped} connection(s) dropped: {}", first_error.as_deref().unwrap_or(""));
    }
    if cfg.watchers > 0 {
        print!(
            "; {} watcher(s) took {received} messages, {}/s",
            cfg.watchers,
            u128::from(received) * 1000 / elapsed_ms
        );
    }
    println!();
    Ok(())
}
