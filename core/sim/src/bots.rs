//! Synthetic makers and takers (blueprint §16). Ordinary clients of the gateway with dev tokens, exercising both of
//! its flows: the maker holds a WebSocket session, as market makers do, and the taker uses the REST API, as
//! programmatic retail does. Both decide only from what the gateway publishes. The maker is also a registered
//! responder: it answers requests for quote on parlays with resting sells (interfaces/gateway/README.md, parlays).

use crate::client::{Api, Response};
use crate::session::{self, Session};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::thread;
use std::time::Duration;
use venue_link::wire::{Error, Ids};

/// The maker's session heartbeat: silence this long and the gateway pulls its quotes.
const HEARTBEAT_SECONDS: u64 = 10;

/// A small seeded generator (splitmix64), so a run can be repeated.
pub struct Rng(u64);

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn chance(&mut self, per_mille: u64) -> bool {
        self.below(1000) < per_mille
    }
}

fn id(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0)
}

/// A market as `GET /v4/markets` lists it.
struct Market {
    id: u64,
    event: u64,
    /// `binary`, `categorical_outcome`, `scalar_band` or `parlay`.
    class: String,
    state: String,
    lot: u64,
    /// Each ladder row's probability in basis points.
    bp: Vec<u64>,
    started: bool,
    suspended: bool,
    ended: bool,
    resolved: bool,
}

fn basis_points(quote: &Value) -> u64 {
    let parts = quote["probability"].as_str().and_then(|p| p.split_once('/'));
    parts.and_then(|(n, d)| Some(10_000 * n.parse::<u64>().ok()? / d.parse::<u64>().ok()?.max(1))).unwrap_or(0)
}

fn ok(response: Response) -> Result<Value, Error> {
    if response.status == 200 {
        Ok(response.body)
    } else {
        Err(response.refusal().into())
    }
}

fn markets(api: &Api) -> Result<Vec<Market>, Error> {
    // Settled events have no open markets, and a venue on a calendar accumulates them all day.
    let body = ok(api.get("/v4/markets?resolved=false")?)?;
    let mut out = Vec::new();
    for e in body["events"].as_array().into_iter().flatten() {
        for m in e["markets"].as_array().into_iter().flatten() {
            out.push(Market {
                id: id(&m["instrumentId"]),
                event: id(&e["eventId"]),
                class: m["class"].as_str().unwrap_or("").to_string(),
                state: m["state"].as_str().unwrap_or("").to_string(),
                lot: m["lotStep"].as_u64().unwrap_or(1).max(1),
                bp: m["ladder"].as_array().into_iter().flatten().map(basis_points).collect(),
                started: e["started"] == true,
                suspended: e["suspended"] == true,
                ended: e["ended"] == true,
                resolved: e["resolved"] == true,
            });
        }
    }
    Ok(out)
}

/// A book's levels as `(tick, quantity)`, best first.
#[derive(Clone, Default, Debug, PartialEq)]
struct Book {
    bids: Vec<(u64, u64)>,
    asks: Vec<(u64, u64)>,
}

impl Book {
    /// Applies a WebSocket `book` message (interfaces/gateway/asyncapi.yaml, `book`): a snapshot replaces every level,
    /// and each change sets its level's quantity, a quantity of zero removing the level. Mirrors the web page's
    /// `applyBook`.
    fn apply(&mut self, message: &Value) {
        if message["snapshot"] == true {
            self.bids.clear();
            self.asks.clear();
        }
        for change in message["changes"].as_array().into_iter().flatten() {
            let tick = change["price"]["tick"].as_u64().unwrap_or(0);
            let quantity = change["quantity"].as_u64().unwrap_or(0);
            let levels = if change["side"] == "buy" { &mut self.bids } else { &mut self.asks };
            levels.retain(|&(t, _)| t != tick);
            if quantity > 0 {
                levels.push((tick, quantity));
            }
        }
        // Best first: the highest bid, the lowest ask.
        self.bids.sort_by_key(|b| std::cmp::Reverse(b.0));
        self.asks.sort_by_key(|a| a.0);
    }
}

/// A whole book from REST `GET /v4/markets/{id}/book`, which lists `bids` and `asks` best first.
fn book_of(body: &Value) -> Book {
    let levels = |side: &str| -> Vec<(u64, u64)> {
        body[side]
            .as_array()
            .into_iter()
            .flatten()
            .map(|l| (l["price"]["tick"].as_u64().unwrap_or(0), l["quantity"].as_u64().unwrap_or(0)))
            .collect()
    };
    Book { bids: levels("bids"), asks: levels("asks") }
}

fn book(api: &Api, instrument: u64) -> Result<Book, Error> {
    Ok(book_of(&ok(api.get(&format!("/v4/markets/{instrument}/book?depth=10"))?)?))
}

#[derive(Clone)]
struct Resting {
    id: u64,
    instrument: u64,
    side: String,
    tick: u64,
    remaining: u64,
}

struct Account {
    sequence: u64,
    orders: Vec<Resting>,
}

fn account(api: &Api) -> Result<Account, Error> {
    let body = ok(api.get("/v4/account")?)?;
    let orders = body["orders"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| Resting {
            id: id(&o["orderId"]),
            instrument: id(&o["instrumentId"]),
            side: o["side"].as_str().unwrap_or("").to_string(),
            tick: o["price"]["tick"].as_u64().unwrap_or(0),
            remaining: o["remaining"].as_u64().unwrap_or(0),
        })
        .collect();
    Ok(Account { sequence: id(&body["sequence"]), orders })
}

enum Action {
    Place(Value),
    Amend(u64, Value),
    Cancel(u64),
    CancelEvent(u64),
}

/// What became of one action.
enum Outcome {
    /// Sequenced, with the input's sequence when it changed something this bot will see.
    Sequenced(Option<u64>),
    /// Refused by the flood limit: wait this long before sending more.
    Throttled(Duration),
    /// The order an amend or cancel named no longer rests: it filled or was cancelled first.
    Gone(u64),
    /// Refused or failed, and already logged if it was worth logging.
    Failed,
}

fn retry_after(error: &Value) -> Duration {
    Duration::from_millis(error["details"]["retryAfterMillis"].as_u64().unwrap_or(1000).max(100))
}

/// An amend or cancel can lose a race with a fill, at the gateway (not found) or in the engine (unknown order). That is
/// the market working, not something to log.
fn lost_race(order_may_be_gone: bool, code: &Value) -> bool {
    order_may_be_gone && (code == "NOT_FOUND" || code == "unknown_order")
}

/// The order an amend or cancel names.
const fn named_order(action: &Action) -> Option<u64> {
    match action {
        Action::Amend(order, _) | Action::Cancel(order) => Some(*order),
        Action::Place(_) | Action::CancelEvent(_) => None,
    }
}

/// Performs one action over REST.
fn perform(api: &Api, who: &str, action: Action) -> Outcome {
    let order_may_be_gone = named_order(&action).is_some();
    let response = match action {
        Action::Place(body) => api.post("/v4/orders", &body),
        Action::Amend(order, body) => api.patch(&format!("/v4/orders/{order}"), &body),
        Action::Cancel(order) => api.delete(&format!("/v4/orders/{order}")),
        Action::CancelEvent(event) => api.delete(&format!("/v4/orders?eventId={event}")),
    };
    match response {
        Ok(r) if r.status == 200 => {
            if r.body["status"] == "rejected" && !lost_race(order_may_be_gone, &r.body["rejectReason"]) {
                eprintln!("{who}: rejected: {}", r.body["rejectReason"]);
            }
            Outcome::Sequenced(r.body["sequence"].as_str().and_then(|s| s.parse().ok()))
        }
        Ok(r) if lost_race(order_may_be_gone, &r.body["error"]["code"]) => Outcome::Failed,
        Ok(r) if r.status == 429 => Outcome::Throttled(retry_after(&r.body["error"])),
        Ok(r) => {
            eprintln!("{who}: refused: {}", r.refusal());
            Outcome::Failed
        }
        Err(e) => {
            eprintln!("{who}: {e}");
            Outcome::Failed
        }
    }
}

/// Performs one action as a WebSocket session op. `Err` means the session is gone.
fn perform_on(session: &mut Session, who: &str, action: Action) -> Result<Outcome, Error> {
    let named = named_order(&action);
    let order_may_be_gone = named.is_some();
    let with_op = |op: &str, mut body: Value| {
        body["op"] = json!(op);
        body
    };
    let message = match action {
        Action::Place(body) => with_op("order", body),
        Action::Amend(order, body) => {
            let mut m = with_op("amend", body);
            m["orderId"] = json!(order.to_string());
            m
        }
        Action::Cancel(order) => json!({"op": "cancel", "orderId": order.to_string()}),
        Action::CancelEvent(event) => json!({"op": "cancelAll", "eventId": event.to_string()}),
    };
    let reply = session.request(message)?;
    Ok(match reply["type"].as_str() {
        Some("reply") if lost_race(order_may_be_gone, &reply["result"]["rejectReason"]) => {
            Outcome::Gone(named.unwrap_or(0))
        }
        Some("reply") => {
            let result = &reply["result"];
            if result["status"] == "rejected" {
                eprintln!("{who}: rejected: {}", result["rejectReason"]);
            }
            // Only an input that produced executions for this account is sure to reach this session's stream.
            let seen = result["executions"].as_array().is_some_and(|e| !e.is_empty());
            Outcome::Sequenced(seen.then(|| result["sequence"].as_str().and_then(|s| s.parse().ok())).flatten())
        }
        Some("error") if lost_race(order_may_be_gone, &reply["error"]["code"]) => Outcome::Gone(named.unwrap_or(0)),
        Some("error") if reply["error"]["code"] == "RATE_LIMITED" => Outcome::Throttled(retry_after(&reply["error"])),
        _ => {
            eprintln!("{who}: refused: {}", reply["error"]);
            Outcome::Failed
        }
    })
}

type Plan<'a> = dyn FnMut(&[Market], &Account, &dyn Fn(u64) -> Option<Book>) -> Vec<Action> + 'a;

/// The taker's loop over REST. Every `interval` it reads the markets and the account and performs what `plan` decides,
/// planning again only once the account shows the effects of everything it sent.
fn run(api: &Api, interval: Duration, who: &str, plan: &mut Plan<'_>) -> ! {
    let (mut sent, mut stale) = (0u64, 0u32);
    loop {
        thread::sleep(interval);
        let (markets, account) = match (markets(api), account(api)) {
            (Ok(m), Ok(a)) => (m, a),
            (Err(e), _) | (_, Err(e)) => {
                eprintln!("{who}: {e}");
                continue;
            }
        };
        if account.sequence < sent {
            // A gateway that restarted on a fresh journal never reaches the old sequence; stop waiting for it.
            stale += 1;
            if stale < 50 {
                continue;
            }
            sent = 0;
        }
        stale = 0;
        let books = |instrument: u64| book(api, instrument).ok();
        for action in plan(&markets, &account, &books) {
            match perform(api, who, action) {
                Outcome::Sequenced(Some(sequence)) => sent = sent.max(sequence),
                Outcome::Throttled(wait) => {
                    // The rest of this pass was planned from a picture that will be stale after the wait.
                    eprintln!("{who}: flood limit reached; pausing {} ms", wait.as_millis());
                    thread::sleep(wait);
                    break;
                }
                Outcome::Sequenced(None) | Outcome::Gone(_) | Outcome::Failed => {}
            }
        }
    }
}

/// What a session has told the maker: its resting orders, the books it follows, and how far its picture reaches.
#[derive(Default)]
struct Stream {
    orders: BTreeMap<u64, Resting>,
    books: BTreeMap<u64, Book>,
    /// Every input at or below this sequence is reflected in `orders` and `books`.
    sequence: u64,
    /// The sequence of the account snapshot `orders` started from; executions at or below it are already in it.
    snapshot: u64,
    markets_changed: bool,
    resync: bool,
    /// Requests for quote received and not answered yet.
    rfqs: Vec<Value>,
}

impl Stream {
    fn restart(&mut self, snapshot: Account) {
        self.snapshot = snapshot.sequence;
        self.sequence = self.sequence.max(snapshot.sequence);
        self.orders = snapshot.orders.into_iter().map(|o| (o.id, o)).collect();
    }

    fn apply(&mut self, message: &Value) {
        let execution = &message["execution"];
        for sequence in [&message["sequence"], &execution["sequence"]] {
            if let Some(s) = sequence.as_str().and_then(|s| s.parse::<u64>().ok()) {
                self.sequence = self.sequence.max(s);
            }
        }
        match message["type"].as_str().unwrap_or("") {
            "execution" if id(&execution["sequence"]) <= self.snapshot => {}
            "execution" => {
                let order = id(&execution["orderId"]);
                let remaining = execution["remaining"].as_u64();
                match execution["type"].as_str().unwrap_or("") {
                    "accepted" => {
                        let resting = Resting {
                            id: order,
                            instrument: id(&execution["instrumentId"]),
                            side: execution["side"].as_str().unwrap_or("").to_string(),
                            tick: execution["price"]["tick"].as_u64().unwrap_or(0),
                            remaining: remaining.unwrap_or(0),
                        };
                        self.orders.insert(order, resting);
                    }
                    "fill" | "reduced" | "amended" => {
                        if remaining == Some(0) {
                            self.orders.remove(&order);
                        } else if let Some(o) = self.orders.get_mut(&order) {
                            o.remaining = remaining.unwrap_or(o.remaining);
                            if execution["type"] == "amended" {
                                o.tick = execution["price"]["tick"].as_u64().unwrap_or(o.tick);
                            }
                        }
                    }
                    "cancelled" => {
                        self.orders.remove(&order);
                    }
                    _ => {}
                }
            }
            "book" => {
                self.books.entry(id(&message["instrumentId"])).or_default().apply(message);
            }
            "market" => self.markets_changed = true,
            "resync" => self.resync = true,
            "rfq" => self.rfqs.push(message.clone()),
            _ => {}
        }
    }
}

/// How a responder answers a request for quote: the body of the resting sell it quotes with, or `None` to let the
/// request pass.
type Respond<'a> = dyn FnMut(&[Market], &dyn Fn(u64) -> Option<Book>, &Value) -> Option<Value> + 'a;

/// The maker's loop over two WebSocket sessions: its own orders arrive on the private one, where its actions go out as
/// session ops, and books and market states on the public one. When the session ends the gateway cancels its quotes, and the loop logs in again, cancels whatever an earlier
/// session left resting, and starts from a fresh snapshot. Requests for quote arrive on the private session too, and
/// are answered as they come rather than on the planning interval: a request's window is short.
fn run_session(
    api: &Api,
    gateway: &str,
    token: &str,
    interval: Duration,
    who: &str,
    plan: &mut Plan<'_>,
    respond: &mut Respond<'_>,
) -> ! {
    loop {
        if let Err(e) = session_pass(api, gateway, token, interval, who, plan, respond) {
            eprintln!("{who}: session ended: {e}; logging in again");
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn session_pass(
    api: &Api,
    gateway: &str,
    token: &str,
    interval: Duration,
    who: &str,
    plan: &mut Plan<'_>,
    respond: &mut Respond<'_>,
) -> Result<(), Error> {
    let (mut session, welcome) = Session::open(gateway, token, HEARTBEAT_SECONDS)?;
    eprintln!("{who}: session {} logged in", welcome["sessionId"].as_str().unwrap_or("?"));
    // Books and market states come on a public session of their own: the private one carries only this account.
    let mut market = Session::watch(gateway)?;
    let mut markets = markets(api)?;
    // Cancel-on-disconnect covers one session's orders, and only while its gateway lives to send the MassCancel: a
    // gateway that died took an earlier session's orders' cover with it (interface 1 README, sessions). Cancelling
    // them here puts every resting order under this session.
    let events: BTreeSet<u64> = markets.iter().map(|m| m.event).collect();
    for event in events {
        if let Outcome::Throttled(wait) = perform_on(&mut session, who, Action::CancelEvent(event))? {
            thread::sleep(wait);
        }
    }
    let mut stream = Stream { sequence: id(&welcome["sequence"]), ..Stream::default() };
    stream.restart(account(api)?);
    market.send(&json!({"op": "subscribe", "topics": ["markets"]}))?;
    let mut followed: BTreeSet<u64> = BTreeSet::new();
    let (mut sent, mut waited) = (0u64, 0u32);
    let mut next_plan = session::monotonic();
    loop {
        let unfollowed: Vec<String> = markets
            .iter()
            .filter(|m| m.state == "open" && followed.insert(m.id))
            .map(|m| format!("book.{}", m.id))
            .collect();
        if !unfollowed.is_empty() {
            market.send(&json!({"op": "subscribe", "topics": unfollowed, "depth": 10}))?;
        }
        for message in
            session.poll(Duration::from_millis(50))?.into_iter().chain(market.poll(Duration::from_millis(50))?)
        {
            stream.apply(&message);
        }
        if stream.markets_changed {
            stream.markets_changed = false;
            markets = crate::bots::markets(api)?;
            // A resolved event's markets leave the list, and the engine later releases them (E-8.4): forget their
            // books and stop following them, so a maker on a calendar does not hold every round it ever quoted.
            let gone: Vec<u64> = followed.iter().copied().filter(|f| !markets.iter().any(|m| m.id == *f)).collect();
            if !gone.is_empty() {
                let topics: Vec<String> = gone.iter().map(|g| format!("book.{g}")).collect();
                market.send(&json!({"op": "unsubscribe", "topics": topics}))?;
                followed.retain(|f| !gone.contains(f));
            }
            stream.books.retain(|b, _| markets.iter().any(|m| m.id == *b));
        }
        if stream.resync {
            stream.resync = false;
            stream.restart(account(api)?);
        }
        for rfq in std::mem::take(&mut stream.rfqs) {
            // A parlay listed for this very request is not in the market list yet.
            if !markets.iter().any(|m| m.id == id(&rfq["instrumentId"])) {
                markets = crate::bots::markets(api)?;
            }
            // The legs' whole books, from REST, as they stand at the request.
            let books = |instrument: u64| book(api, instrument).ok();
            match respond(&markets, &books, &rfq) {
                Some(order) => answer_rfq(&mut session, who, &rfq, order)?,
                None => eprintln!("{who}: no price for request {}: a leg has no two-sided book", rfq["requestId"]),
            }
        }
        if session::monotonic() < next_plan {
            continue;
        }
        next_plan = session::monotonic() + interval;
        if stream.sequence < sent {
            // Wait for the session to show the last action, but not for ever.
            waited += 1;
            if waited < 20 {
                continue;
            }
        }
        waited = 0;
        let account = Account { sequence: stream.sequence, orders: stream.orders.values().cloned().collect() };
        let actions = {
            let books = |instrument: u64| stream.books.get(&instrument).cloned();
            plan(&markets, &account, &books)
        };
        for action in actions {
            match perform_on(&mut session, who, action)? {
                Outcome::Sequenced(Some(sequence)) => sent = sent.max(sequence),
                Outcome::Throttled(wait) => {
                    eprintln!("{who}: flood limit reached; pausing {} ms", wait.as_millis());
                    thread::sleep(wait);
                    break;
                }
                Outcome::Gone(order) => {
                    stream.orders.remove(&order);
                }
                Outcome::Sequenced(None) | Outcome::Failed => {}
            }
        }
    }
}

/// Rests the quote's sell and attaches it to the request. `Err` means the session is gone; a refusal is only logged,
/// since the request will have lapsed by the time anything could be done about it.
fn answer_rfq(session: &mut Session, who: &str, rfq: &Value, mut order: Value) -> Result<(), Error> {
    order["op"] = json!("order");
    let placed = session.request(order)?;
    let Some(order_id) = placed["result"]["orderId"].as_str().filter(|_| placed["result"]["status"] == "accepted")
    else {
        eprintln!(
            "{who}: quote not rested: {}",
            if placed["type"] == "error" { &placed["error"] } else { &placed["result"] }
        );
        return Ok(());
    };
    let attach = json!({"op": "attachQuote", "requestId": rfq["requestId"], "orderId": order_id});
    let attached = session.request(attach)?;
    if attached["type"] == "reply" {
        let price = &attached["result"]["price"]["cents"];
        eprintln!(
            "{who}: quoted request {} at {}c",
            rfq["requestId"].as_str().unwrap_or("?"),
            price.as_str().unwrap_or("?")
        );
    } else {
        eprintln!("{who}: quote not attached: {}", attached["error"]);
    }
    Ok(())
}

fn nearest_tick(bp: &[u64], target: u64) -> u64 {
    bp.iter().enumerate().min_by_key(|(_, b)| b.abs_diff(target)).map_or(0, |(t, _)| t as u64)
}

pub struct Maker {
    /// Contracts quoted at the touch on each side, in lots; each level further out quotes one more multiple.
    pub size: u64,
    /// Price levels quoted on each side.
    pub levels: u64,
    /// Ticks from fair value to each quote; doubled once an event has started.
    pub half_spread: u64,
    pub interval: Duration,
    pub seed: u64,
    /// What the maker adds to a parlay's price as a responder, in thousandths of it.
    pub margin_per_mille: u64,
}

/// A parlay's price as a responder quotes it, as a row of the parlay's ladder, or `None` if a leg has no two-sided
/// book to take a price from. Each leg's chance is the middle of its book (for a NO leg, one minus it), and the
/// parlay's is their product: the legs are on different events, which the maker takes as independent. Correlated
/// legs make that wrong, which is why the exchange leaves the pricing to responders (blueprint §6). The margin is on
/// top, plus a tenth of a cent, rounded up to the ladder, and never the top row.
fn parlay_tick(markets: &[Market], books: &dyn Fn(u64) -> Option<Book>, rfq: &Value, margin: u64) -> Option<u64> {
    let parlay = markets.iter().find(|m| m.id == id(&rfq["instrumentId"]))?;
    let mut ppm: u128 = 1_000_000;
    for leg in rfq["legs"].as_array()? {
        let market = markets.iter().find(|m| m.id == id(&leg["instrumentId"]))?;
        let book = books(market.id)?;
        let (bid, ask) = (book.bids.first()?.0, book.asks.first()?.0);
        let mid = (market.bp.get(bid as usize)? + market.bp.get(ask as usize)?) / 2;
        let chance = if leg["outcome"] == "yes" { mid } else { 10_000u64.saturating_sub(mid) };
        ppm = ppm * u128::from(chance) / 10_000;
    }
    let target_bp = u64::try_from((ppm * u128::from(1_000 + margin) / 1_000 + 1_000).div_ceil(100)).ok()?;
    let top = parlay.bp.len().checked_sub(2)?;
    Some(parlay.bp.iter().position(|&bp| bp >= target_bp).unwrap_or(top).min(top) as u64)
}

/// Quotes several levels on both sides of every open market around a seeded random-walk fair value, rescaled to sum
/// to 100¢ across an event's outcomes, over a WebSocket session with cancel-on-disconnect. Pulls its quotes when an
/// event ends or is suspended. Answers each request for quote with a sell of the whole size, good until the request's
/// window closes, priced by `parlay_tick`; parlays are otherwise left to their requesters and anyone who takes a quote.
pub fn maker(api: &Api, gateway: &str, token: &str, config: &Maker, mut ids: Ids, mut quote_ids: Ids) -> ! {
    let mut rng = Rng::new(config.seed);
    let mut fair: BTreeMap<u64, u64> = BTreeMap::new();
    let margin = config.margin_per_mille;
    let mut respond = |markets: &[Market], books: &dyn Fn(u64) -> Option<Book>, rfq: &Value| {
        let tick = parlay_tick(markets, books, rfq, margin)?;
        Some(json!({
            "clientOrderId": quote_ids.fresh(), "instrumentId": rfq["instrumentId"], "action": "sell", "outcome": "yes",
            "price": {"tick": tick}, "quantity": rfq["quantity"], "timeInForce": "GTD", "expiresAt": rfq["expiresAt"]
        }))
    };
    let mut plan = |markets: &[Market], account: &Account, books: &dyn Fn(u64) -> Option<Book>| {
        let mut out = Vec::new();
        let mut by_event: BTreeMap<u64, Vec<&Market>> = BTreeMap::new();
        for m in markets.iter().filter(|m| m.state == "open" && m.class != "parlay") {
            by_event.entry(m.event).or_default().push(m);
        }
        for (event, open) in by_event {
            let first = open[0];
            if first.resolved {
                continue;
            }
            if first.ended || first.suspended {
                if account.orders.iter().any(|o| open.iter().any(|m| m.id == o.instrument)) {
                    out.push(Action::CancelEvent(event));
                }
                continue;
            }
            let spread = if first.started { config.half_spread * 2 } else { config.half_spread };
            let mut quotes: Vec<(&Market, u64)> = Vec::new();
            for m in &open {
                let n = m.bp.len() as u64;
                if n < 2 * spread + 3 {
                    continue;
                }
                let f = fair.entry(m.id).or_insert_with(|| n / 4 + rng.below(n / 2));
                match rng.below(3) {
                    0 => *f = f.saturating_sub(1),
                    1 => *f += 1,
                    _ => {}
                }
                *f = (*f).clamp(spread + 1, n - 2 - spread);
                quotes.push((m, *f));
            }
            if quotes.len() > 1 {
                let total = quotes.iter().map(|(m, f)| m.bp[*f as usize]).sum::<u64>().max(1);
                for (m, f) in &mut quotes {
                    let target = m.bp[*f as usize] * 10_000 / total;
                    *f = nearest_tick(&m.bp, target).clamp(spread + 1, m.bp.len() as u64 - 2 - spread);
                    fair.insert(m.id, *f);
                }
            }
            for (m, f) in quotes {
                let Some(book) = books(m.id) else { continue };
                let top = m.bp.len() as u64 - 1;
                let (best_bid, best_ask) = (book.bids.first().map(|l| l.0), book.asks.first().map(|l| l.0));
                for side in ["buy", "sell"] {
                    // Level k sits k ticks behind the touch with k + 1 times the size. A level that would cross
                    // someone else's order is left out, since post-only would refuse it.
                    let desired: Vec<(u64, u64)> = (0..config.levels.max(1))
                        .filter_map(|k| {
                            let tick = if side == "buy" {
                                f.checked_sub(spread + k)?
                            } else {
                                (f + spread + k <= top).then_some(f + spread + k)?
                            };
                            // Deeper levels carry more, and every requote sizes them afresh: a real maker's
                            // ladder moves in size as well as in price, and a demo where it does not makes the
                            // book look frozen even while it is changing.
                            Some((tick, config.size * (k + 1) * m.lot * (1 + rng.below(4))))
                        })
                        .filter(|(tick, _)| {
                            if side == "buy" {
                                best_ask.is_none_or(|a| *tick < a)
                            } else {
                                best_bid.is_none_or(|b| *tick > b)
                            }
                        })
                        .collect();
                    let mut mine: Vec<&Resting> =
                        account.orders.iter().filter(|o| o.instrument == m.id && o.side == side).collect();
                    if side == "buy" {
                        mine.sort_by_key(|o| std::cmp::Reverse(o.tick));
                    } else {
                        mine.sort_by_key(|o| o.tick);
                    }
                    for (k, o) in mine.iter().enumerate() {
                        match desired.get(k) {
                            // Any difference in size, not only a shortfall: a level that shrinks has to be
                            // amended too, or half of the sizing never reaches the book.
                            Some(&(tick, size)) if o.tick != tick || o.remaining != size => {
                                out.push(Action::Amend(o.id, json!({"price": {"tick": tick}, "quantity": size})));
                            }
                            Some(_) => {}
                            None => out.push(Action::Cancel(o.id)),
                        }
                    }
                    for &(tick, size) in desired.iter().skip(mine.len()) {
                        out.push(Action::Place(json!({
                            "clientOrderId": ids.fresh(), "instrumentId": m.id.to_string(), "action": side,
                            "outcome": "yes", "price": {"tick": tick}, "quantity": size, "postOnly": true
                        })));
                    }
                }
            }
        }
        out
    };
    run_session(api, gateway, token, config.interval, "maker", &mut plan, &mut respond)
}

pub struct Taker {
    /// The largest ordinary order, in lots; a sweep is four times this.
    pub max_quantity: u64,
    /// Chance per market per interval of sending an order, in thousandths.
    pub per_mille: u64,
    /// Chance that an order is a sweep three ticks through the touch, in thousandths.
    pub sweep_per_mille: u64,
    pub interval: Duration,
    pub seed: u64,
}

/// Sends immediate-or-cancel orders at the touch at random over REST, as retail arrives: buying YES, buying NO or
/// selling YES, with prices as ticks or cents and buys often as stakes, and the occasional sweep.
pub fn taker(api: &Api, config: &Taker, mut ids: Ids) -> ! {
    let mut rng = Rng::new(config.seed);
    run(api, config.interval, "taker", &mut |markets, _account, books| {
        let mut out = Vec::new();
        // Parlays are left alone: their quotes are for their requesters to take first.
        for m in markets.iter().filter(|m| m.state == "open" && !m.suspended && !m.resolved && m.class != "parlay") {
            if !rng.chance(config.per_mille) {
                continue;
            }
            let Some(book) = books(m.id) else { continue };
            let n = m.bp.len() as u64;
            let sweep = rng.chance(config.sweep_per_mille);
            let lots = if sweep { config.max_quantity * 4 } else { 1 + rng.below(config.max_quantity.max(1)) };
            let (action, outcome, yes_tick) = match rng.below(3) {
                0 => {
                    let Some(&(ask, _)) = book.asks.first() else { continue };
                    ("buy", "yes", if sweep { (ask + 3).min(n - 1) } else { ask })
                }
                1 => {
                    let Some(&(bid, _)) = book.bids.first() else { continue };
                    ("buy", "no", if sweep { bid.saturating_sub(3) } else { bid })
                }
                _ => {
                    let Some(&(bid, _)) = book.bids.first() else { continue };
                    ("sell", "yes", if sweep { bid.saturating_sub(3) } else { bid })
                }
            };
            // A ladder closed under complement reverses: the NO price at YES tick t is row n - 1 - t.
            let (outcome_tick, outcome_bp) = if outcome == "yes" {
                (yes_tick, m.bp[yes_tick as usize])
            } else {
                (n - 1 - yes_tick, 10_000 - m.bp[yes_tick as usize])
            };
            let whole_cents = outcome_bp % 100 == 0;
            let mut body = json!({
                "clientOrderId": ids.fresh(), "instrumentId": m.id.to_string(), "action": action, "outcome": outcome,
                "timeInForce": "IOC"
            });
            body["price"] = if whole_cents && rng.below(2) == 0 {
                json!({"cents": (outcome_bp / 100).to_string()})
            } else {
                json!({"tick": outcome_tick})
            };
            if action == "buy" && whole_cents && m.lot == 1 && rng.below(2) == 0 {
                body["stakeCents"] = json!(lots * outcome_bp / 100);
            } else {
                body["quantity"] = json!(lots * m.lot);
            }
            out.push(Action::Place(body));
        }
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(id: u64, class: &str, bp: Vec<u64>) -> Market {
        Market {
            id,
            event: id,
            class: class.into(),
            state: "open".into(),
            lot: 1,
            bp,
            started: false,
            suspended: false,
            ended: false,
            resolved: false,
        }
    }

    /// The cent ladder's basis points, and the parlay ladder's: 0.1¢ to 10¢, whole cents, 90¢ to 99.9¢.
    fn ladders() -> (Vec<u64>, Vec<u64>) {
        let cents = (1..=99).map(|c| c * 100).collect();
        let parlay = (1..=100).chain((110..=890).step_by(10)).chain(900..=999).map(|n| n * 10).collect();
        (cents, parlay)
    }

    #[test]
    fn a_parlay_is_priced_from_the_middle_of_its_legs_books_with_a_margin_on_top() {
        let (cents, parlay) = ladders();
        let markets =
            vec![market(1, "binary", cents.clone()), market(2, "binary", cents), market(9, "parlay", parlay.clone())];
        // Both legs 49¢ bid, 51¢ offered (ticks 48 and 50): a middle of 50¢.
        let book = Book { bids: vec![(48, 10)], asks: vec![(50, 10)] };
        let books = |_: u64| Some(book.clone());
        let rfq = json!({"instrumentId": "9", "legs": [{"instrumentId": "1", "outcome": "yes"}, {"instrumentId": "2", "outcome": "no"}]});
        // 25% plus a tenth of it and 0.1¢ is 27.6¢, rounded up to the ladder's 28¢.
        let tick = parlay_tick(&markets, &books, &rfq, 100).expect("priced");
        assert_eq!(parlay[tick as usize], 2_800);
        // Without a book on a leg there is no price to quote.
        assert_eq!(parlay_tick(&markets, &|_| None, &rfq, 100), None);
    }

    fn level(side: &str, tick: u64, quantity: u64) -> Value {
        json!({"side": side, "price": {"tick": tick, "cents": "0"}, "quantity": quantity, "orders": 1})
    }

    #[test]
    fn a_session_book_starts_from_its_snapshot_and_follows_the_changes() {
        let mut stream = Stream::default();
        let book = |changes: Vec<Value>, snapshot: bool| json!({"type": "book", "instrumentId": "7", "sequence": "5", "snapshot": snapshot, "changes": changes});
        stream.apply(&book(
            vec![level("buy", 40, 5), level("sell", 45, 3), level("buy", 42, 2), level("sell", 44, 1)],
            true,
        ));
        assert_eq!(stream.books[&7], Book { bids: vec![(42, 2), (40, 5)], asks: vec![(44, 1), (45, 3)] });
        // A new level on each side, a changed one, and a removed one.
        stream.apply(&book(
            vec![level("buy", 43, 4), level("buy", 40, 9), level("sell", 44, 0), level("sell", 50, 6)],
            false,
        ));
        assert_eq!(stream.books[&7], Book { bids: vec![(43, 4), (42, 2), (40, 9)], asks: vec![(45, 3), (50, 6)] });
        stream.apply(&book(vec![level("buy", 42, 0), level("sell", 45, 7)], false));
        assert_eq!(stream.books[&7], Book { bids: vec![(43, 4), (40, 9)], asks: vec![(45, 7), (50, 6)] });
        // A later snapshot replaces every level, including an empty one.
        stream.apply(&book(vec![level("sell", 60, 1)], true));
        assert_eq!(stream.books[&7], Book { bids: vec![], asks: vec![(60, 1)] });
        stream.apply(&book(vec![], true));
        assert_eq!(stream.books[&7], Book::default());
    }

    #[test]
    fn a_rest_book_is_read_from_its_bids_and_asks() {
        let body = json!({
            "instrumentId": "7",
            "bids": [{"price": {"tick": 42}, "quantity": 2}, {"price": {"tick": 40}, "quantity": 5}],
            "asks": [{"price": {"tick": 44}, "quantity": 1}]
        });
        assert_eq!(book_of(&body), Book { bids: vec![(42, 2), (40, 5)], asks: vec![(44, 1)] });
    }

    #[test]
    fn a_long_shot_is_quoted_on_the_parlays_finest_rows() {
        let (cents, parlay) = ladders();
        let markets: Vec<Market> =
            (1..=4).map(|i| market(i, "binary", cents.clone())).chain([market(9, "parlay", parlay.clone())]).collect();
        // Four legs at 20¢ each: 0.16%, which a cent ladder could not tell from nothing.
        let book = Book { bids: vec![(18, 10)], asks: vec![(20, 10)] };
        let legs: Vec<Value> = (1..=4).map(|i| json!({"instrumentId": i.to_string(), "outcome": "yes"})).collect();
        let rfq = json!({"instrumentId": "9", "legs": legs});
        let tick = parlay_tick(&markets, &|_| Some(book.clone()), &rfq, 0).expect("priced");
        assert_eq!(parlay[tick as usize], 30, "0.16¢ and 0.1¢ is 0.26¢, rounded up to 0.3¢");
    }
}
