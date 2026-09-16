//! The parlay service's process: it follows the node's feed, lists parlays when they are first requested, holds the
//! open requests for quote and the quotes attached to them, and sequences what the legs call for.
//!
//! Gateways connect to it over TCP and speak JSON, one object per line. A gateway sends a call,
//! `{"ref": 7, "op": "request" | "quote" | "status", "accountId": 2, ...}`, and is answered
//! `{"ref": 7, "result": {...}}` or `{"ref": 7, "error": {"status": 400, "code": "BAD_REQUEST", "message": "..."}}`,
//! in whatever order the answers are ready. Unasked, the service sends every gateway `{"to": [10], "message": {...}}`:
//! a message for the private sessions of those accounts, which the gateway delivers as it is, apart from turning a
//! `price` tick into a price quote and nanosecond times into timestamps, as it does for everything it presents.
//!
//! Requests are here rather than in a gateway because a gateway must hold nothing a failover could lose (interface 1
//! README), and because a requester and the responders are rarely on one gateway. They are not in the journal: the
//! engine has no RFQ state machine (spec §19), and the quotes that matter, the orders, are journaled already. A
//! restarted service forgets open requests and relearns everything else from the feed.

use crate::rules::{self, Action, Refusal, Wanted, LOT_STEP};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use venue_link::catalogue::Catalogue;
use venue_link::view::{Leg, View};
use venue_link::wire::{self, command, Error, Ids, Ingress, Unit};

/// How close to the wall clock a unit's sequencer time must be for the service to count as caught up. Until then it
/// is replaying history, and acting on a picture of the past would halt or close a parlay that is fine now.
const CAUGHT_UP_NANOS: u64 = 2_000_000_000;

/// How long a request is remembered after its window closes and its last quote has left the book, so a requester
/// can still read how it went.
const RETAIN_NANOS: u64 = 3_600 * 1_000_000_000;

/// How long an answer may wait for the feed: to show a parlay just listed, or an order a responder has just rested.
const FEED_PATIENCE: Duration = Duration::from_secs(5);

pub struct Config {
    pub ingress: String,
    pub feed: String,
    pub listen: String,
    /// How long a request accepts quotes, in seconds of sequencer time.
    pub window_seconds: u64,
    /// The most parlays the service lists. Every book reserves its levels at listing for as long as the node runs
    /// (E-6.3), and running out of them is a fail-stop, so the service refuses a request past this instead.
    pub max_parlays: usize,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wall-clock nanoseconds, only to judge whether the feed has caught up; nothing decided from it is sequenced.
#[allow(clippy::disallowed_methods)]
fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn id(v: u64) -> Value {
    Value::String(v.to_string())
}

/// A refusal, as a gateway passes it on.
#[derive(Debug)]
pub struct Failure {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

fn bad(message: impl Into<String>) -> Failure {
    Failure { status: 400, code: "BAD_REQUEST", message: message.into() }
}

fn not_found(message: impl Into<String>) -> Failure {
    Failure { status: 404, code: "NOT_FOUND", message: message.into() }
}

fn unavailable(message: impl Into<String>) -> Failure {
    Failure { status: 503, code: "UNAVAILABLE", message: message.into() }
}

struct Quote {
    tick: u64,
    remaining: u64,
    expires_at: Option<u64>,
    /// `open`, `filled`, `expired` or `cancelled`.
    state: &'static str,
}

struct Request {
    requester: u64,
    parlay: u64,
    legs: Vec<Leg>,
    quantity: u64,
    listed: bool,
    opened_at: u64,
    expires_at: u64,
    quotes: BTreeMap<u64, Quote>,
}

#[derive(Default)]
struct State {
    view: View,
    caught_up: bool,
    requests: BTreeMap<String, Request>,
    /// The request each open quote's order answers.
    by_order: BTreeMap<u64, String>,
    count: u64,
    /// Parlays with a command sequenced that the view has not reached yet, with that command's sequence. Nothing more
    /// is decided for them until it has, or the service would send the same move again on every tick in between.
    in_flight: BTreeMap<u64, u64>,
}

/// A change to one order: its order ID, then what it now has remaining, its new tick, and how it left the book, each
/// if the result says.
type Update = (u64, Option<u64>, Option<u64>, Option<&'static str>);

impl State {
    /// Follows the orders that are quotes through one unit's results, and returns the requester's news.
    fn track_quotes(&mut self, results: &[Value]) -> Vec<(u64, Value)> {
        let mut changed: Vec<u64> = Vec::new();
        for r in results {
            let u = |name: &str| r[name].as_u64().unwrap_or(0);
            let updates: Vec<Update> = match r["msg"].as_str() {
                Some("Fill") => vec![
                    (u("makerOrderId"), Some(u("makerRemaining")), None, None),
                    (u("takerOrderId"), Some(u("takerRemaining")), None, None),
                ],
                Some("OrderCancelled") => {
                    let state = if r["reason"] == "EXPIRED" { "expired" } else { "cancelled" };
                    vec![(u("orderId"), None, None, Some(state))]
                }
                Some("OrderReduced") => vec![(u("orderId"), Some(u("remainingQuantity")), None, None)],
                Some("OrderAmended") => vec![(u("orderId"), Some(u("quantity")), Some(u("tick")), None)],
                _ => continue,
            };
            for (order, remaining, tick, gone) in updates {
                let Some(request) = self.by_order.get(&order) else { continue };
                let Some(quote) = self.requests.get_mut(request).and_then(|r| r.quotes.get_mut(&order)) else {
                    continue;
                };
                if let Some(remaining) = remaining {
                    quote.remaining = remaining;
                    if remaining == 0 {
                        quote.state = "filled";
                    }
                }
                quote.tick = tick.unwrap_or(quote.tick);
                if let Some(state) = gone {
                    quote.state = state;
                    quote.remaining = 0;
                }
                if !changed.contains(&order) {
                    changed.push(order);
                }
            }
        }
        let mut news = Vec::new();
        for order in changed {
            let Some(name) = self.by_order.get(&order).cloned() else { continue };
            let Some(request) = self.requests.get(&name) else { continue };
            let Some(quote) = request.quotes.get(&order) else { continue };
            let mut message = quote_json(&name, request.parlay, order, quote);
            message["type"] = json!("quote");
            news.push((request.requester, message));
            if quote.state != "open" {
                self.by_order.remove(&order);
            }
        }
        news
    }
}

fn quote_json(request: &str, parlay: u64, order: u64, quote: &Quote) -> Value {
    json!({
        "requestId": request, "orderId": id(order), "instrumentId": id(parlay), "price": {"tick": quote.tick},
        "quantity": quote.remaining, "expiresAt": quote.expires_at, "state": quote.state
    })
}

pub struct Service {
    catalogue: Catalogue,
    responders: BTreeSet<u64>,
    window: u64,
    max_parlays: usize,
    nonce: String,
    state: Mutex<State>,
    changed: Condvar,
    ingress: Mutex<(Ingress, Ids)>,
    gateways: Mutex<BTreeMap<u64, Sender<String>>>,
    next_gateway: Mutex<u64>,
}

/// A few random bytes as hex, so request IDs from a restarted service never repeat an earlier one's.
fn nonce() -> Result<String, Error> {
    let mut bytes = [0u8; 4];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(format!("{:08x}", u32::from_le_bytes(bytes)))
}

/// Starts the service: follows the feed, accepts gateways on `config.listen`, and returns once it is listening.
pub fn start(catalogue: Catalogue, config: &Config) -> Result<Arc<Service>, Error> {
    let responders =
        catalogue.accounts().iter().filter(|a| a["responder"] == true).filter_map(|a| a["id"].as_u64()).collect();
    let service = Arc::new(Service {
        catalogue,
        responders,
        window: config.window_seconds.max(1) * 1_000_000_000,
        max_parlays: config.max_parlays,
        nonce: nonce()?,
        state: Mutex::new(State::default()),
        changed: Condvar::new(),
        ingress: Mutex::new((Ingress::new(&config.ingress), Ids::new('p')?)),
        gateways: Mutex::new(BTreeMap::new()),
        next_gateway: Mutex::new(0),
    });
    let listener = TcpListener::bind(&config.listen)?;
    eprintln!("parlay service on {}, following {}", listener.local_addr()?, config.feed);
    let follower = Arc::clone(&service);
    let feed = config.feed.clone();
    thread::spawn(move || {
        wire::follow(
            &feed,
            || {
                let mut state = locked(&follower.state);
                state.view = View::default();
                state.caught_up = false;
                state.in_flight.clear();
            },
            |unit| follower.on_unit(&unit),
        );
    });
    let acceptor = Arc::clone(&service);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let service = Arc::clone(&acceptor);
            thread::spawn(move || {
                if let Err(e) = service.gateway(stream) {
                    eprintln!("parlay service: gateway connection: {e}");
                }
            });
        }
    });
    Ok(service)
}

impl Service {
    fn on_unit(&self, unit: &Unit) {
        let (news, actions) = {
            let mut state = locked(&self.state);
            state.view.apply(unit);
            let sequenced_at = unit.input["record"]["sequencedAt"].as_u64().unwrap_or(0);
            state.caught_up = wall_ns().saturating_sub(sequenced_at) < CAUGHT_UP_NANOS;
            let news = state.track_quotes(&unit.results);
            let (now, sequence) = (state.view.now, state.view.sequence);
            state.requests.retain(|_, r| {
                r.expires_at.saturating_add(RETAIN_NANOS) > now || r.quotes.values().any(|q| q.state == "open")
            });
            state.in_flight.retain(|_, at| *at > sequence);
            let actions: Vec<(u64, Action)> = if state.caught_up {
                rules::actions(&state.view).into_iter().filter(|(p, _)| !state.in_flight.contains_key(p)).collect()
            } else {
                Vec::new()
            };
            (news, actions)
        };
        self.changed.notify_all();
        for (requester, message) in news {
            self.push(&[requester], &message);
        }
        for (parlay, action) in actions {
            let input = match action {
                Action::Settle(settlement) => rules::settle_input(parlay, settlement),
                Action::Move(to) => {
                    let command_id = locked(&self.ingress).1.fresh();
                    command("SetMarketState", command_id, json!({"instrumentId": parlay, "state": to}))
                }
            };
            eprintln!("parlay service: {} for parlay {parlay}", input["msg"].as_str().unwrap_or(""));
            match self.send(input) {
                Ok(results) => {
                    let at = results.iter().find_map(|r| r["result"]["inputSequence"].as_u64());
                    let mut state = locked(&self.state);
                    let at = at.unwrap_or(state.view.sequence + 1);
                    state.in_flight.insert(parlay, at);
                    if let Some(refused) =
                        results.iter().find(|r| r["msg"] == "CommandApplied" && r["outcome"] == "REJECTED")
                    {
                        eprintln!("parlay service: parlay {parlay}: rejected {}", refused["rejectReason"]);
                    }
                }
                Err(e) => eprintln!("parlay service: ingress: {e}"),
            }
        }
    }

    fn send(&self, input: Value) -> Result<Vec<Value>, Error> {
        locked(&self.ingress).0.send(input)
    }

    /// Sends a message for some accounts' sessions to every gateway connected.
    fn push(&self, accounts: &[u64], message: &Value) {
        if accounts.is_empty() {
            return;
        }
        let line = json!({"to": accounts, "message": message}).to_string();
        locked(&self.gateways).retain(|_, gateway| gateway.send(line.clone()).is_ok());
    }

    /// Serves one gateway until it disconnects: its calls are answered on threads of their own, so a call waiting for
    /// the feed holds up nothing else.
    fn gateway(self: &Arc<Self>, stream: TcpStream) -> Result<(), Error> {
        stream.set_nodelay(true)?;
        let (tx, rx) = channel::<String>();
        let key = {
            let mut next = locked(&self.next_gateway);
            *next += 1;
            *next
        };
        locked(&self.gateways).insert(key, tx.clone());
        let mut writer = stream.try_clone()?;
        thread::spawn(move || {
            for line in rx {
                if writer.write_all(line.as_bytes()).and_then(|()| writer.write_all(b"\n")).is_err() {
                    break;
                }
            }
        });
        for line in BufReader::new(stream).lines() {
            let call: Value = match serde_json::from_str(&line?) {
                Ok(call) => call,
                Err(e) => {
                    let _ = tx.send(
                        json!({"error": {"status": 400, "code": "BAD_REQUEST", "message": e.to_string()}}).to_string(),
                    );
                    continue;
                }
            };
            let (service, tx) = (Arc::clone(self), tx.clone());
            thread::spawn(move || {
                let answer = match service.call(&call) {
                    Ok(result) => json!({"ref": call["ref"], "result": result}),
                    Err(f) => {
                        json!({"ref": call["ref"], "error": {"status": f.status, "code": f.code, "message": f.message}})
                    }
                };
                let _ = tx.send(answer.to_string());
            });
        }
        locked(&self.gateways).remove(&key);
        Ok(())
    }

    fn call(&self, call: &Value) -> Result<Value, Failure> {
        let account = call["accountId"].as_u64().ok_or_else(|| bad("a call names its account"))?;
        match call["op"].as_str().unwrap_or("") {
            "request" => self.request(account, call),
            "quote" => self.quote(account, call),
            "status" => self.status(account, call),
            other => Err(bad(format!("no call {other}"))),
        }
    }

    /// Waits, up to `FEED_PATIENCE`, for the feed to make `ready` hold, and returns what it gives then.
    fn when<T>(&self, mut ready: impl FnMut(&State) -> Option<T>) -> Option<T> {
        let state = locked(&self.state);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, FEED_PATIENCE, |s| ready(s).is_none())
            .unwrap_or_else(PoisonError::into_inner);
        ready(&state)
    }

    fn rfq_json(&self, name: &str, request: &Request) -> Value {
        let legs: Vec<Value> = request
            .legs
            .iter()
            .map(|l| {
                json!({
                    "eventId": id(l.event), "instrumentId": id(l.instrument), "outcome": if l.yes { "yes" } else { "no" },
                    "title": self.catalogue.title(l.event), "label": self.catalogue.label(l.event, l.instrument)
                })
            })
            .collect();
        json!({
            "requestId": name, "instrumentId": id(request.parlay), "eventId": id(request.parlay), "legs": legs,
            "quantity": request.quantity, "lotStep": LOT_STEP, "openedAt": request.opened_at,
            "expiresAt": request.expires_at
        })
    }

    /// Opens a request: canonical legs, the parlay listed if it is new, and the request sent to every responder but
    /// the requester, who is not named.
    fn request(&self, account: u64, call: &Value) -> Result<Value, Failure> {
        let quantity = call["quantity"].as_u64().unwrap_or(0);
        if quantity < LOT_STEP || !quantity.is_multiple_of(LOT_STEP) {
            return Err(bad(format!("quantity must be a positive multiple of the parlay lot step, {LOT_STEP}")));
        }
        let wanted: Vec<Wanted> = call["legs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| Wanted { instrument: l["instrumentId"].as_u64().unwrap_or(0), yes: l["yes"] == true })
            .collect();
        let (legs, parlay, inputs) = {
            let state = locked(&self.state);
            if !state.caught_up {
                return Err(unavailable("the parlay service is catching up with the feed"));
            }
            let legs = rules::canonical(&state.view, &wanted).map_err(|r| match r {
                Refusal::Bad(message) => bad(message),
                Refusal::Unknown(instrument) => not_found(format!("no market {instrument}")),
            })?;
            let parlay = rules::parlay_id(&legs);
            let inputs = match state.view.instruments.get(&parlay) {
                Some(listed) if listed.legs != legs => {
                    return Err(bad(format!("parlay {parlay} was listed on other legs; these cannot be offered")));
                }
                Some(listed) if listed.state != "OPEN" => {
                    return Err(bad(format!(
                        "parlay {parlay} is {}: it no longer trades",
                        listed.state.to_lowercase()
                    )));
                }
                Some(_) => Vec::new(),
                None => {
                    let parlays = state.view.instruments.values().filter(|i| i.class == "PARLAY").count();
                    if parlays >= self.max_parlays {
                        return Err(unavailable(format!("this venue lists at most {} parlays", self.max_parlays)));
                    }
                    let table = (!state.view.ladders.contains_key(&rules::TICK_TABLE)).then(rules::tick_table);
                    table.into_iter().chain(rules::listing(&legs)).collect()
                }
            };
            (legs, parlay, inputs)
        };
        let listed = !inputs.is_empty();
        for input in inputs {
            let results = self.send(input).map_err(|e| unavailable(format!("the node did not answer: {e}")))?;
            let applied = results.iter().find(|r| r["msg"] == "CommandApplied");
            if applied.is_some_and(|r| r["outcome"] == "REJECTED") {
                let reason = applied.map_or(Value::Null, |r| r["rejectReason"].clone());
                return Err(bad(format!("the venue refused to list the parlay: {reason}")));
            }
        }
        self.when(|s| s.view.instruments.get(&parlay).filter(|i| i.state == "OPEN").map(|_| ()))
            .ok_or_else(|| unavailable("the parlay was listed but the feed has not shown it open yet; ask again"))?;
        let (name, rfq) = {
            let mut state = locked(&self.state);
            state.count += 1;
            let name = format!("{}{}", self.nonce, rules::command_id('r', state.count).split_off(1));
            let now = state.view.now;
            let request = Request {
                requester: account,
                parlay,
                legs,
                quantity,
                listed,
                opened_at: now,
                expires_at: now + self.window,
                quotes: BTreeMap::new(),
            };
            let rfq = self.rfq_json(&name, &request);
            state.requests.insert(name.clone(), request);
            (name, rfq)
        };
        let responders: Vec<u64> = self.responders.iter().copied().filter(|r| *r != account).collect();
        let mut message = rfq.clone();
        message["type"] = json!("rfq");
        self.push(&responders, &message);
        eprintln!("parlay service: request {name} for parlay {parlay}, {quantity} contracts");
        let mut result = rfq;
        result["listed"] = json!(listed);
        Ok(result)
    }

    /// Attaches a responder's resting sell to a request.
    fn quote(&self, account: u64, call: &Value) -> Result<Value, Failure> {
        if !self.responders.contains(&account) {
            return Err(bad("only registered responders can quote"));
        }
        let name = call["requestId"].as_str().unwrap_or("").to_string();
        let order = call["orderId"].as_u64().unwrap_or(0);
        {
            let state = locked(&self.state);
            let request = state.requests.get(&name).ok_or_else(|| not_found("no such request"))?;
            if state.view.now >= request.expires_at {
                return Err(bad("the request's window has closed"));
            }
        }
        // The order was accepted moments ago, and the feed can be a little behind the gateway's answer.
        let found = self.when(|s| s.view.orders.get(&order).map(|o| (o.account, o.instrument, o.side.clone())));
        let (owner, instrument, side) = found.ok_or_else(|| not_found("no such resting order"))?;
        let mut state = locked(&self.state);
        let parlay = state.requests.get(&name).map_or(0, |r| r.parlay);
        if owner != account {
            return Err(not_found("no such resting order"));
        }
        if instrument != parlay {
            return Err(bad("the order is not on this request's parlay"));
        }
        if side != "SELL" {
            return Err(bad("a quote sells YES on the parlay; this order buys"));
        }
        if let Some(other) = state.by_order.get(&order) {
            if *other != name {
                return Err(bad("the order already answers another request"));
            }
        }
        let Some(resting) = state.view.orders.get(&order).cloned() else {
            return Err(not_found("no such resting order"));
        };
        let quote =
            Quote { tick: resting.tick, remaining: resting.remaining, expires_at: resting.expires_at, state: "open" };
        let message = quote_json(&name, parlay, order, &quote);
        let requester = {
            let request = state.requests.get_mut(&name).ok_or_else(|| not_found("no such request"))?;
            request.quotes.insert(order, quote);
            request.requester
        };
        state.by_order.insert(order, name);
        drop(state);
        let mut pushed = message.clone();
        pushed["type"] = json!("quote");
        self.push(&[requester], &pushed);
        Ok(message)
    }

    /// A request and its quotes, for the account that made it.
    fn status(&self, account: u64, call: &Value) -> Result<Value, Failure> {
        let name = call["requestId"].as_str().unwrap_or("");
        let state = locked(&self.state);
        let request =
            state.requests.get(name).filter(|r| r.requester == account).ok_or_else(|| not_found("no such request"))?;
        let mut status = self.rfq_json(name, request);
        status["listed"] = json!(request.listed);
        status["open"] = json!(state.view.now < request.expires_at);
        status["quotes"] = request
            .quotes
            .iter()
            .map(|(order, q)| quote_json(name, request.parlay, *order, q))
            .collect::<Vec<_>>()
            .into();
        Ok(status)
    }
}
