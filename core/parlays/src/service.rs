//! The parlay service's process: it follows the node's feed, lists parlays when they are first requested, holds the
//! open requests for quote and the quotes attached to them, and sequences what the legs call for.
//!
//! Gateways connect to it over TCP and speak JSON, one object per line. A gateway sends a call,
//! `{"ref": 7, "op": "request" | "quote" | "status" | "open" | "subscribe" | "unsubscribe", "accountId": 2, ...}`,
//! and is answered `{"ref": 7, "result": {...}}` or
//! `{"ref": 7, "error": {"status": 400, "code": "BAD_REQUEST", "message": "..."}}`, in whatever order the answers are
//! ready, except that `subscribe` and `unsubscribe` are answered in the order they were sent, before anything the
//! gateway sent after them. Unasked, the service sends every gateway `{"to": [10], "message": {...}}`, a message for
//! the private sessions of those accounts, and one gateway `{"toSessions": [3, 8], "message": {...}}`, a message for
//! those of its sessions, named by the key the gateway gave them in `subscribe`. The gateway delivers either as it
//! is, apart from turning a `price` tick into a price quote and nanosecond times into timestamps, as it does for
//! everything it presents.
//!
//! A responder's session subscribes with `{"op": "subscribe", "accountId", "session", "filters"}` and is answered
//! with its filters and the requests open now that match (`requests`), so a session that subscribes mid-window can
//! still quote. Subscriptions belong to the gateway connection they came through and end with it: a gateway that
//! reconnects, to this service or to a restarted one, subscribes its sessions again.
//!
//! Requests are here rather than in a gateway because a gateway must hold nothing a failover could lose (interface 1
//! README), and because a requester and the responders are rarely on one gateway. They are not in the journal: the
//! engine has no RFQ state machine (spec §19), and the quotes that matter, the orders, are journaled already. A
//! restarted service forgets open requests and subscriptions, rereads its approved responders from its database, and
//! relearns everything else (parlays, their legs and how they stand) from the feed.

use crate::responders::{Approvals, Filter, Subscriptions};
use crate::rules::{self, Action, Refusal, Wanted, LOT_STEP};
use serde_json::{json, Value};
use std::collections::BTreeMap;
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
    /// The most parlays the venue carries at once. Every book reserves its levels until its event is released a
    /// retention after settlement (E-8.4), so the service refuses a request past this instead of using them up.
    pub max_parlays: usize,
    /// The node's capacity profile: no parlay is listed that would take the venue past 95% of it (`rules::no_room`).
    pub capacity: rules::Capacity,
    /// The database the approved responders are kept in (`responders::Approvals`); `None` keeps them in memory,
    /// seeded from the catalogue, for tests and throwaway local venues.
    pub responders: Option<venue_store::Database>,
    /// The fewest contracts a request may ask for: a multiple of the parlay lot step, and at least one lot.
    pub min_quantity: u64,
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

fn forbidden(account: u64) -> Failure {
    Failure {
        status: 403,
        code: "FORBIDDEN",
        message: format!("account {account} is not an approved parlay responder; an operator approves responders"),
    }
}

fn unavailable(message: impl Into<String>) -> Failure {
    Failure { status: 503, code: "UNAVAILABLE", message: message.into() }
}

struct Quote {
    tick: u64,
    remaining: u64,
    /// Contracts of the quote bought so far, by anyone. `remaining` alone read as a fill of nothing once the quote
    /// was taken in full: a filled quote with zero contracts.
    filled: u64,
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
    /// When the last unit arrived, by the wall clock, or 0 before any has. A feed that has gone quiet and a feed
    /// that is behind look the same from `caught_up` alone; this is what separates them (`ready`).
    last_unit_ns: u64,
    requests: BTreeMap<String, Request>,
    /// The request each open quote's order answers.
    by_order: BTreeMap<u64, String>,
    count: u64,
    /// The sessions listening for requests.
    subscriptions: Subscriptions,
    /// Parlays with a command sequenced that the view has not reached yet, with that command's sequence. Nothing more
    /// is decided for them until it has, or the service would send the same move again on every tick in between.
    in_flight: BTreeMap<u64, u64>,
}

/// A change to one order: its order ID, then what it now has remaining, its new tick, and how it left the book, each
/// if the result says, and the contracts it just traded.
type Update = (u64, Option<u64>, Option<u64>, Option<&'static str>, u64);

impl State {
    /// Whether the view is current enough to answer from: caught up with the feed, or at the head of a feed that has
    /// gone quiet.
    ///
    /// The second half is the fix to a deadlock, and it is worth saying why it is not just a weakened `caught_up`.
    /// The feed carries commands and nothing else — no heartbeat, no tick — so `caught_up`, which is worked out from
    /// the age of a unit as that unit arrives, stops being recomputed the moment the venue goes quiet. Start this
    /// service more than `CAUGHT_UP_NANOS` after a venue was last traded and the only units it will ever see are the
    /// replayed ones, already too old to count as current, with nothing behind them. It then refuses every request
    /// for as long as nobody trades — and nobody can trade a parlay, because that is what it is refusing. Waiting
    /// cannot mend it: the next unit would, and there is no next unit. Every idle venue starts this way.
    ///
    /// Silence is read as being at the head rather than behind it only because the alternative reading — a sequencer
    /// wedged with its socket still open — cannot be told apart from silence here, and is the less costly of the two
    /// to get wrong. Nothing is being sequenced during a wedge, so a quote offered against a stale view is one whose
    /// orders cannot fill either; they wait for the wedge to clear and are then checked by the engine, which is the
    /// authority on the book in any case. A dropped feed is a different matter and is handled where it happens, by
    /// clearing `last_unit_ns` along with the view.
    ///
    /// **Acting is still gated on `caught_up` alone** (`on_unit`). Answering that a parlay may be quoted is a claim
    /// about what is listed; halting or closing one is a move made on the venue's behalf, and making that move on a
    /// picture of the past would take down a parlay that is fine now. Only the first of those is relaxed here.
    fn current(&self) -> bool {
        self.caught_up || (self.last_unit_ns != 0 && wall_ns().saturating_sub(self.last_unit_ns) >= CAUGHT_UP_NANOS)
    }

    /// The requests still open that `account` may answer and `filter` selects, oldest first: never the account's own.
    fn open_for(&self, account: u64, filter: &Filter) -> Vec<(&String, &Request)> {
        let mut open: Vec<(&String, &Request)> = self
            .requests
            .iter()
            .filter(|(_, r)| self.view.now < r.expires_at && r.requester != account)
            .filter(|(_, r)| filter.matches(&r.legs, r.quantity))
            .collect();
        open.sort_by_key(|(name, r)| (r.opened_at, name.as_str()));
        open
    }

    /// Follows the orders that are quotes through one unit's results, and returns the requester's news.
    fn track_quotes(&mut self, results: &[Value]) -> Vec<(u64, Value)> {
        let mut changed: Vec<u64> = Vec::new();
        for r in results {
            let u = |name: &str| r[name].as_u64().unwrap_or(0);
            let updates: Vec<Update> = match r["msg"].as_str() {
                Some("Fill") => vec![
                    (u("makerOrderId"), Some(u("makerRemaining")), None, None, u("quantity")),
                    (u("takerOrderId"), Some(u("takerRemaining")), None, None, u("quantity")),
                ],
                Some("OrderCancelled") => {
                    let state = if r["reason"] == "EXPIRED" { "expired" } else { "cancelled" };
                    vec![(u("orderId"), None, None, Some(state), 0)]
                }
                Some("OrderReduced") => vec![(u("orderId"), Some(u("remainingQuantity")), None, None, 0)],
                Some("OrderAmended") => vec![(u("orderId"), Some(u("quantity")), Some(u("tick")), None, 0)],
                _ => continue,
            };
            for (order, remaining, tick, gone, traded) in updates {
                let Some(request) = self.by_order.get(&order) else { continue };
                let Some(quote) = self.requests.get_mut(request).and_then(|r| r.quotes.get_mut(&order)) else {
                    continue;
                };
                quote.filled += traded;
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
        "quantity": quote.remaining, "filled": quote.filled, "expiresAt": quote.expires_at, "state": quote.state
    })
}

pub struct Service {
    catalogue: Catalogue,
    /// Taken only after `state` when both are held.
    approvals: Mutex<Approvals>,
    window: u64,
    min_quantity: u64,
    max_parlays: usize,
    capacity: rules::Capacity,
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
    let seed = catalogue.accounts().iter().filter(|a| a["responder"] == true).filter_map(|a| a["id"].as_u64());
    let approvals = Approvals::load(config.responders.as_ref(), seed.collect::<Vec<u64>>())?;
    if config.min_quantity < LOT_STEP || !config.min_quantity.is_multiple_of(LOT_STEP) {
        return Err(format!("the minimum request must be a positive multiple of the lot step, {LOT_STEP}").into());
    }
    eprintln!("parlay service: approved responders {:?}", approvals.accounts());
    let service = Arc::new(Service {
        catalogue,
        approvals: Mutex::new(approvals),
        window: config.window_seconds.max(1) * 1_000_000_000,
        min_quantity: config.min_quantity,
        max_parlays: config.max_parlays,
        capacity: config.capacity,
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
                // The view has just been thrown away, so there is no last unit any more. Leaving the old stamp here
                // would make `current` read a dropped feed as a quiet one two seconds later and answer from an empty
                // view, which is the one way this could be worse than what it replaces.
                state.last_unit_ns = 0;
                state.in_flight.clear();
                // The view was just discarded, so everything is wanted again.
                0
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
            state.last_unit_ns = wall_ns();
            state.caught_up = state.last_unit_ns.saturating_sub(sequenced_at) < CAUGHT_UP_NANOS;
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

    /// Sends a message for some of one gateway's sessions to that gateway.
    fn push_sessions(&self, gateway: u64, sessions: &[u64], message: &Value) {
        if sessions.is_empty() {
            return;
        }
        let line = json!({"toSessions": sessions, "message": message}).to_string();
        let mut gateways = locked(&self.gateways);
        if gateways.get(&gateway).is_some_and(|g| g.send(line).is_err()) {
            gateways.remove(&gateway);
        }
    }

    /// Serves one gateway until it disconnects: its calls are answered on threads of their own, so a call waiting for
    /// the feed holds up nothing else. Subscribing and unsubscribing never wait, and are answered here, in order.
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
            if let Some(answer) = self.subscription_call(key, &call) {
                let answer = match answer {
                    Ok(result) => json!({"ref": call["ref"], "result": result}),
                    Err(f) => {
                        json!({"ref": call["ref"], "error": {"status": f.status, "code": f.code, "message": f.message}})
                    }
                };
                let _ = tx.send(answer.to_string());
                continue;
            }
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
        locked(&self.state).subscriptions.drop_gateway(key);
        Ok(())
    }

    /// The answer to a `subscribe` or `unsubscribe` from gateway connection `gateway`, or `None` for any other call.
    fn subscription_call(&self, gateway: u64, call: &Value) -> Option<Result<Value, Failure>> {
        let op = call["op"].as_str()?;
        if op != "subscribe" && op != "unsubscribe" {
            return None;
        }
        Some(self.subscription(gateway, op == "subscribe", call))
    }

    fn subscription(&self, gateway: u64, subscribe: bool, call: &Value) -> Result<Value, Failure> {
        let account = call["accountId"].as_u64().ok_or_else(|| bad("a call names its account"))?;
        let session = call["session"].as_u64().ok_or_else(|| bad("a subscription names its session"))?;
        if !subscribe {
            locked(&self.state).subscriptions.unsubscribe((gateway, session));
            return Ok(json!({"subscribed": false}));
        }
        self.subscribe(gateway, session, account, &call["filters"])
    }

    /// Subscribes a session of an approved responder, and answers with the requests open now that it would have been
    /// sent. Both happen under one lock with the requests, so a request opened meanwhile is in the answer or sent to
    /// the session afterwards, never both and never neither. A refusal leaves any subscription the session had as it
    /// was.
    fn subscribe(&self, gateway: u64, session: u64, account: u64, filters: &Value) -> Result<Value, Failure> {
        let filter = Filter::parse(filters).map_err(bad)?;
        let mut state = locked(&self.state);
        if !locked(&self.approvals).contains(account) {
            return Err(forbidden(account));
        }
        let requests: Vec<Value> = state
            .open_for(account, &filter)
            .into_iter()
            .map(|(name, request)| {
                let mut rfq = self.rfq_json(name, request);
                rfq["type"] = json!("rfq");
                rfq
            })
            .collect();
        let shown = filter.to_json();
        state.subscriptions.subscribe((gateway, session), account, filter);
        drop(state);
        eprintln!("parlay service: account {account} subscribed, {} requests open for it", requests.len());
        Ok(json!({"subscribed": true, "filters": shown, "openRequests": requests.len(), "requests": requests}))
    }

    /// The requests open now that an approved responder may quote.
    fn open(&self, account: u64) -> Result<Value, Failure> {
        let state = locked(&self.state);
        if !locked(&self.approvals).contains(account) {
            return Err(forbidden(account));
        }
        let requests: Vec<Value> = state
            .open_for(account, &Filter::default())
            .into_iter()
            .map(|(name, request)| self.rfq_json(name, request))
            .collect();
        Ok(json!({"requests": requests}))
    }

    /// Approves an account as a responder, for an operator. Only a catalogue account can be approved.
    pub fn approve(&self, account: u64) -> Result<Value, Failure> {
        if !self.catalogue.accounts().iter().any(|a| a["id"].as_u64() == Some(account)) {
            return Err(not_found(format!("no account {account}")));
        }
        let changed = locked(&self.approvals)
            .approve(account)
            .map_err(|e| unavailable(format!("the responder list could not be written: {e}")))?;
        if changed {
            eprintln!("parlay service: account {account} approved as a responder");
        }
        Ok(json!({"accountId": id(account), "approved": true, "changed": changed}))
    }

    /// Revokes an account's approval, for an operator: its subscriptions end at once, each session told, and it can
    /// attach no more quotes. Quotes it attached already are orders on the book, and stay there.
    pub fn revoke(&self, account: u64) -> Result<Value, Failure> {
        let (changed, ended) = {
            let mut state = locked(&self.state);
            let changed = locked(&self.approvals)
                .revoke(account)
                .map_err(|e| unavailable(format!("the responder list could not be written: {e}")))?;
            (changed, state.subscriptions.drop_account(account))
        };
        let message = json!({
            "type": "rfqSubscription", "subscribed": false,
            "reason": "this account's approval as a parlay responder was revoked"
        });
        let mut by_gateway: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for (gateway, session) in &ended {
            by_gateway.entry(*gateway).or_default().push(*session);
        }
        for (gateway, sessions) in by_gateway {
            self.push_sessions(gateway, &sessions, &message);
        }
        if changed {
            eprintln!("parlay service: account {account} revoked as a responder, {} subscriptions ended", ended.len());
        }
        Ok(json!({"accountId": id(account), "approved": false, "changed": changed, "subscriptionsEnded": ended.len()}))
    }

    /// The approved responders, each with its name and how many sessions it has subscribed, for an operator.
    pub fn responders(&self) -> Value {
        let state = locked(&self.state);
        let approvals = locked(&self.approvals);
        let list: Vec<Value> = approvals
            .accounts()
            .iter()
            .map(|&account| {
                let name = self
                    .catalogue
                    .accounts()
                    .iter()
                    .find(|a| a["id"].as_u64() == Some(account))
                    .map_or(Value::Null, |a| a["name"].clone());
                json!({"accountId": id(account), "name": name, "subscriptions": state.subscriptions.count(account)})
            })
            .collect();
        json!({"responders": list})
    }

    /// Whether the approved responders' database answers: `None` without one. Routing never depends on it.
    pub fn database_health(&self) -> Option<Result<(), String>> {
        locked(&self.approvals).health()
    }

    /// Whether the service is far enough along to answer requests. See `State::current`.
    pub fn ready(&self) -> bool {
        locked(&self.state).current()
    }

    fn call(&self, call: &Value) -> Result<Value, Failure> {
        let account = call["accountId"].as_u64().ok_or_else(|| bad("a call names its account"))?;
        match call["op"].as_str().unwrap_or("") {
            "request" => self.request(account, call),
            "quote" => self.quote(account, call),
            "status" => self.status(account, call),
            "open" => self.open(account),
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

    /// Opens a request: canonical legs, the parlay listed if it is new, and the request sent to every subscribed
    /// session of an approved responder whose filters match, except the requester's own, without naming the requester.
    fn request(&self, account: u64, call: &Value) -> Result<Value, Failure> {
        let quantity = call["quantity"].as_u64().unwrap_or(0);
        if quantity < LOT_STEP || !quantity.is_multiple_of(LOT_STEP) {
            return Err(bad(format!("quantity must be a positive multiple of the parlay lot step, {LOT_STEP}")));
        }
        if quantity < self.min_quantity {
            return Err(bad(format!("a request is for at least {} contracts", self.min_quantity)));
        }
        let wanted: Vec<Wanted> = call["legs"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| Wanted { instrument: l["instrumentId"].as_u64().unwrap_or(0), yes: l["yes"] == true })
            .collect();
        let (legs, parlay, inputs) = {
            let state = locked(&self.state);
            if !state.current() {
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
                    if let Some(reason) = rules::no_room(&state.view, &self.capacity) {
                        return Err(unavailable(reason));
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
        let (name, rfq, targets, reached) = {
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
            // Chosen while the request is inserted, under the same lock a subscription takes to read what is open.
            let (targets, reached) =
                state.subscriptions.targets(&locked(&self.approvals), account, &request.legs, quantity);
            state.requests.insert(name.clone(), request);
            (name, rfq, targets, reached)
        };
        let mut message = rfq.clone();
        message["type"] = json!("rfq");
        for (gateway, sessions) in &targets {
            self.push_sessions(*gateway, sessions, &message);
        }
        eprintln!(
            "parlay service: request {name} for parlay {parlay}, {quantity} contracts, sent to {reached} responders"
        );
        let mut result = rfq;
        result["listed"] = json!(listed);
        result["responders"] = json!(reached);
        Ok(result)
    }

    /// Attaches a responder's resting sell to a request.
    fn quote(&self, account: u64, call: &Value) -> Result<Value, Failure> {
        if !locked(&self.approvals).contains(account) {
            return Err(forbidden(account));
        }
        let name = call["requestId"].as_str().unwrap_or("").to_string();
        let order = call["orderId"].as_u64().unwrap_or(0);
        {
            let state = locked(&self.state);
            let request = state.requests.get(&name).ok_or_else(|| not_found("no such request"))?;
            if state.view.now >= request.expires_at {
                return Err(bad("the request's window has closed"));
            }
            if request.requester == account {
                return Err(bad("a responder cannot quote its own request"));
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
        let quote = Quote {
            tick: resting.tick,
            remaining: resting.remaining,
            filled: 0,
            expires_at: resting.expires_at,
            state: "open",
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(event: u64) -> Leg {
        Leg { event, instrument: event * 100, yes: true }
    }

    fn request(requester: u64, legs: &[u64], quantity: u64, opened_at: u64, expires_at: u64) -> Request {
        Request {
            requester,
            parlay: rules::ID_BASE + opened_at,
            legs: legs.iter().map(|&e| leg(e)).collect(),
            quantity,
            listed: true,
            opened_at,
            expires_at,
            quotes: BTreeMap::new(),
        }
    }

    /// What a responder subscribing now is caught up with: requests still open, oldest first, that its filter selects
    /// and that it did not make itself.
    #[test]
    fn a_subscriber_catches_up_with_the_open_requests_it_would_have_been_sent() {
        let mut state = State::default();
        state.view.now = 1_000;
        state.requests.insert("b".into(), request(2, &[1, 2], 10, 900, 2_000));
        state.requests.insert("a".into(), request(2, &[1, 2, 3], 50, 950, 2_000));
        state.requests.insert("closed".into(), request(2, &[1, 2], 10, 100, 1_000));
        state.requests.insert("own".into(), request(10, &[1, 2], 10, 990, 2_000));
        let names = |open: Vec<(&String, &Request)>| open.into_iter().map(|(n, _)| n.clone()).collect::<Vec<_>>();
        assert_eq!(names(state.open_for(10, &Filter::default())), vec!["b", "a"]);
        let three = Filter { min_legs: 3, ..Filter::default() };
        assert_eq!(names(state.open_for(10, &three)), vec!["a"]);
        let large = Filter { min_quantity: 20, ..Filter::default() };
        assert_eq!(names(state.open_for(10, &large)), vec!["a"]);
        assert_eq!(
            names(state.open_for(2, &Filter::default())),
            vec!["own"],
            "the requester sees only others' requests"
        );
        // A restarted service has forgotten every request: nothing to catch up with, and nothing wrongly offered.
        assert!(State::default().open_for(10, &Filter::default()).is_empty());
    }

    /// A venue nobody is trading must not look the same as a feed this service is behind on. It is the same feed
    /// either way — one with no heartbeat on it — so the two are told apart by how long it has been since a unit
    /// arrived, and being at the head of a quiet feed counts as current.
    #[test]
    fn a_quiet_feed_is_told_apart_from_one_the_service_is_behind_on() {
        let old = wall_ns() - CAUGHT_UP_NANOS * 2;

        assert!(!State::default().current(), "nothing has arrived yet: the view is empty, not current");

        let behind = State { caught_up: false, last_unit_ns: wall_ns(), ..State::default() };
        assert!(!behind.current(), "a unit just arrived carrying an old timestamp: there is a backlog, so wait");

        let quiet = State { caught_up: false, last_unit_ns: old, ..State::default() };
        assert!(quiet.current(), "the last unit is long past and no other came: this is the head of a quiet venue");

        let trading = State { caught_up: true, last_unit_ns: wall_ns(), ..State::default() };
        assert!(trading.current(), "keeping up with a busy feed");

        // What the feed thread leaves behind when the connection drops (`follow`'s on_connect). Without the cleared
        // stamp this would turn into "quiet" two seconds later and answer from a view that had just been discarded.
        let dropped = State { caught_up: false, last_unit_ns: 0, ..State::default() };
        assert!(!dropped.current(), "a dropped feed is not a quiet one");
    }
}
