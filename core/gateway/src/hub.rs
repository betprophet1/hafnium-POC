//! The gateway's picture of the venue and its sessions. The feed updates the view; each input's results become
//! private messages for the accounts they concern and public messages for topic subscribers.

use crate::limits::monotonic;
use crate::price;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SendError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_journal::message_header_codec::MessageHeaderDecoder;
use venue_journal::side::Side;
use venue_journal::{
    book_level_changed_codec, command_applied_codec, event_released_codec, market_state_changed_codec,
    order_accepted_codec, position_changed_codec, sleeve_changed_codec, ReadBuf,
};
use venue_link::catalogue::Catalogue;
use venue_link::submit::Notify;
use venue_link::view::{InstrumentView, Row, View};
use venue_link::wire::{Frame, Unit};

/// Private messages kept per account for sessions resuming by sequence.
const HISTORY: usize = 2_000;

/// Requests at most waiting for an order the feed has not delivered yet (`Hub::await_order`); past this one is refused
/// rather than queued.
pub const MAX_AWAITING: usize = 4_096;

/// Accepted orders remembered while the feed catches up. A bound, not a working size: entries leave as soon as the
/// feed passes them, so this only matters if the feed stalls.
const ACCEPTED: usize = 65_536;

pub fn id(v: u64) -> Value {
    Value::String(v.to_string())
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

pub fn lower(v: &Value) -> Value {
    Value::String(v.as_str().unwrap_or("").to_lowercase())
}

/// Wall-clock nanoseconds, only to judge whether the feed has caught up.
#[allow(clippy::disallowed_methods)]
fn wall_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

pub fn ladder(view: &View, instrument: u64) -> &[Row] {
    view.instruments.get(&instrument).and_then(|i| view.ladders.get(&i.table)).map_or(&[], Vec::as_slice)
}

/// The executions an input's results mean for each account, from the view as it was before the input.
/// Takes the results as an iterator of `&Value` rather than a slice, so a caller holding feed frames can hand
/// over what they decode to without collecting them first (`wire::Frame`).
/// A frame's SBE header, for handing to a generated decoder.
fn header_of(f: &Frame) -> MessageHeaderDecoder<ReadBuf<'_>> {
    MessageHeaderDecoder::default().wrap(ReadBuf::new(f.bytes()), 0)
}

/// Whether a result is a `CommandApplied` saying so. The template is checked first, so a scan for one does not
/// decode every other frame of the unit on the way past (ADR-0002).
fn applied_ok(f: &Frame) -> bool {
    f.template() == command_applied_codec::SBE_TEMPLATE_ID && f["outcome"] == "APPLIED"
}

/// The order id of an `OrderAccepted` frame, without decoding the rest of it.
fn order_accepted_order_id(f: &Frame) -> u64 {
    let mut dec = order_accepted_codec::OrderAcceptedDecoder::default().header(header_of(f), 0);
    let mut group = dec.result_decoder();
    dec = group.parent().expect("the result group");
    dec.order_id()
}

/// Results `executions` has nothing to say about, skipped before they are looked at rather than after: a frame
/// is kept decoded once anything asks for it, so filtering here is what lets a book level cost nothing at all
/// (ADR-0002).
///
/// Listed the way round that fails safely. A result missing from here costs a decode and nothing else. The
/// first cut of this was an allow-list of the results `executions` does answer for, which is the same idea
/// with the opposite failure: it omitted `OrderAmended`, and an amend confirmation stopped reaching the client
/// with every test still passing.
const NOT_EXECUTIONS: [u16; 4] = [
    book_level_changed_codec::SBE_TEMPLATE_ID,
    position_changed_codec::SBE_TEMPLATE_ID,
    sleeve_changed_codec::SBE_TEMPLATE_ID,
    command_applied_codec::SBE_TEMPLATE_ID,
];

pub fn executions_of(view: &View, results: &[Frame], sequence: u64) -> Vec<(u64, Value)> {
    let wanted = results.iter().filter(|f| !NOT_EXECUTIONS.contains(&f.template()));
    executions(view, wanted.map(|f| &**f), sequence)
}

pub fn executions<'a>(view: &View, results: impl IntoIterator<Item = &'a Value>, sequence: u64) -> Vec<(u64, Value)> {
    let mut out = Vec::new();
    let mut accepted: BTreeMap<u64, (String, u64)> = BTreeMap::new();
    for r in results {
        let instrument = u(&r["instrumentId"]);
        let quote = |tick: u64| price::quote(ladder(view, instrument), tick);
        let base = |kind: &str, order: Value| json!({"type": kind, "sequence": id(sequence), "orderId": order, "instrumentId": id(instrument)});
        match r["msg"].as_str().unwrap_or("") {
            "OrderAccepted" => {
                let (order, tick) = (u(&r["orderId"]), u(&r["tick"]));
                accepted.insert(order, (r["side"].as_str().unwrap_or("").to_string(), tick));
                let mut e = base("accepted", id(order));
                e["clientOrderId"] = r["clientOrderId"].clone();
                e["side"] = lower(&r["side"]);
                e["price"] = quote(tick);
                e["quantity"] = r["quantity"].clone();
                e["remaining"] = r["quantity"].clone();
                out.push((u(&r["accountId"]), e));
            }
            "OrderRejected" => {
                let mut e = base("rejected", r["originalOrderId"].as_u64().map_or(Value::Null, id));
                e["clientOrderId"] = r["clientOrderId"].clone();
                e["reason"] = lower(&r["reason"]);
                out.push((u(&r["accountId"]), e));
            }
            "Fill" => {
                let taker_buys = r["takerSide"] == "BUY";
                let parties = [
                    ("maker", &r["makerAccountId"], &r["makerOrderId"], &r["makerRemaining"], !taker_buys),
                    ("taker", &r["takerAccountId"], &r["takerOrderId"], &r["takerRemaining"], taker_buys),
                ];
                for (role, account, order, remaining, buys) in parties {
                    let mut e = base("fill", id(u(order)));
                    e["side"] = json!(if buys { "buy" } else { "sell" });
                    e["price"] = quote(u(&r["tick"]));
                    e["quantity"] = r["quantity"].clone();
                    e["remaining"] = remaining.clone();
                    e["tradeId"] = id(u(&r["tradeId"]));
                    e["role"] = json!(role);
                    e["cashCents"] = if buys { r["buyerCashCents"].clone() } else { r["sellerCashCents"].clone() };
                    // Charged on top of the cash change, to the maker and taker at their own rates (spec E-10.11).
                    let fee = if role == "maker" { &r["makerFeeCents"] } else { &r["takerFeeCents"] };
                    e["feeCents"] = fee.clone();
                    out.push((u(account), e));
                }
            }
            "OrderCancelled" => {
                let order = u(&r["orderId"]);
                let mut e = base("cancelled", id(order));
                e["quantity"] = r["cancelledQuantity"].clone();
                e["reason"] = lower(&r["reason"]);
                let known =
                    view.orders.get(&order).map(|o| (o.side.clone(), o.tick)).or_else(|| accepted.get(&order).cloned());
                if let Some((side, tick)) = known {
                    e["side"] = json!(side.to_lowercase());
                    e["price"] = quote(tick);
                }
                out.push((u(&r["accountId"]), e));
            }
            "OrderReduced" => {
                let mut e = base("reduced", id(u(&r["orderId"])));
                e["remaining"] = r["remainingQuantity"].clone();
                out.push((u(&r["accountId"]), e));
            }
            "OrderAmended" => {
                let order = u(&r["orderId"]);
                let mut e = base("amended", id(order));
                if let Some(o) = view.orders.get(&order) {
                    e["side"] = json!(o.side.to_lowercase());
                }
                e["price"] = quote(u(&r["tick"]));
                e["quantity"] = r["quantity"].clone();
                e["remaining"] = r["quantity"].clone();
                out.push((u(&r["accountId"]), e));
            }
            _ => {}
        }
    }
    out
}

/// Takes a session out of one entry of an index, dropping the entry once it is empty.
fn unindex<K, Q>(index: &mut BTreeMap<Q, BTreeSet<u64>>, name: &K, key: u64)
where
    K: Ord + ?Sized,
    Q: Ord + std::borrow::Borrow<K>,
{
    if let Some(keys) = index.get_mut(name) {
        keys.remove(&key);
        if keys.is_empty() {
            index.remove(name);
        }
    }
}

/// What the gateway rendered a message from: the kind of frame it is, and the sequenced input whose results it was
/// rendered from. The audit log records such a frame by reference to the journal rather than keeping a second copy of
/// it (interfaces/audit/README.md, frames derived from the journal).
#[derive(Clone, Copy)]
pub struct Derived {
    pub kind: &'static str,
    pub sequence: u64,
}

/// A message on its way to a session, and where it came from. A message with no `derived` was not rendered from the
/// journal — an answer, an error, a transfer, a quote — and the audit log keeps it in full.
pub struct Queued {
    pub text: String,
    pub derived: Option<Derived>,
}

/// Where a session's messages go: its queue, and a way to tell whatever serves the session that something is waiting
/// there, so nothing has to keep looking.
#[derive(Clone)]
pub struct Outbox {
    queue: Sender<Queued>,
    notify: Notify,
    /// Messages sent and not yet taken off the queue by whatever serves the session, which takes one off as it takes
    /// each message (`taken`): the backlog the gateway's metrics report.
    waiting: Arc<AtomicU64>,
    /// Bytes on the queue and not yet taken. `max_write_buffer_size` bounds only what has already reached the
    /// socket, so without this a session the loops cannot drain fast enough grows this queue without limit --
    /// the gateway then holds gigabytes for one client rather than dropping it (`reactor::serve_ws`).
    behind: Arc<AtomicU64>,
    /// Set the first time the queue would go past `max_behind`, and never cleared: whatever serves the session
    /// closes it. Sticky because `put` stops queueing at the limit, so the backlog itself falls back under it and
    /// a reader watching only `behind` would never see that the session had overrun.
    over: Arc<AtomicBool>,
    /// How many bytes may wait here before the session is closed. Unbounded unless `with_limit` says otherwise,
    /// for the outboxes that serve no live client (`reconstruct`, tests).
    max_behind: u64,
}

impl Outbox {
    pub fn new(queue: Sender<Queued>, notify: Notify) -> Self {
        Self {
            queue,
            notify,
            waiting: Arc::new(AtomicU64::new(0)),
            behind: Arc::new(AtomicU64::new(0)),
            over: Arc::new(AtomicBool::new(false)),
            max_behind: u64::MAX,
        }
    }

    /// The same, closed once `max_behind` bytes wait on it: what a client serving a real connection gets.
    pub fn with_limit(mut self, max_behind: usize) -> Self {
        self.max_behind = max_behind as u64;
        self
    }

    pub fn send(&self, message: String) -> Result<(), SendError<Queued>> {
        self.put(Queued { text: message, derived: None })
    }

    /// A frame rendered from the results of `sequence`, which the journal holds.
    pub fn send_derived(&self, message: String, kind: &'static str, sequence: u64) -> Result<(), SendError<Queued>> {
        self.put(Queued { text: message, derived: Some(Derived { kind, sequence }) })
    }

    fn put(&self, message: Queued) -> Result<(), SendError<Queued>> {
        // Past the limit the session is already on its way out, so holding more for it only grows what the
        // gateway cannot send. The message is dropped rather than queued; a client that reconnects resumes from
        // `history`, which says through what it missed (`since`, `dropped_through`).
        let bytes = message.text.len() as u64;
        if self.over.load(Ordering::Relaxed)
            || self.behind.load(Ordering::Relaxed).saturating_add(bytes) > self.max_behind
        {
            self.over.store(true, Ordering::Relaxed);
            (self.notify)();
            return Ok(());
        }
        self.queue.send(message)?;
        self.waiting.fetch_add(1, Ordering::Relaxed);
        self.behind.fetch_add(bytes, Ordering::Relaxed);
        (self.notify)();
        Ok(())
    }

    /// The count of messages waiting, shared with whatever takes them off the queue.
    pub fn waiting(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.waiting)
    }

    /// The bytes waiting, shared with whatever takes them off the queue so its metrics can report the backlog.
    pub fn behind(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.behind)
    }

    /// Whether this session has overrun its limit, shared with whatever serves it so it can close the connection.
    pub fn over(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.over)
    }
}

struct Session {
    account: Option<u64>,
    tx: Outbox,
    topics: BTreeSet<String>,
    depth: usize,
}

#[derive(Default)]
struct History {
    entries: VecDeque<(u64, String)>,
    dropped_through: u64,
}

/// Book levels by `(buy, tick)`, each with its quantity and order count. A quantity of zero means the level has
/// gone, which is how the engine reports it and how a client applies it.
type Levels = BTreeMap<(bool, u64), (u64, u64)>;

pub struct Hub {
    pub view: View,
    catalogue: Catalogue,
    tokens: BTreeMap<String, u64>,
    sessions: BTreeMap<u64, Session>,
    /// Session keys by the account they are logged in as, and by topic, so a message reaches its sessions without
    /// looking at every session: at tens of thousands of connections a scan per message is most of the work.
    by_account: BTreeMap<u64, BTreeSet<u64>>,
    by_topic: BTreeMap<String, BTreeSet<u64>>,
    next_key: u64,
    /// Book levels that have moved since the last flush: instrument, then `(buy, tick)` to that level's latest
    /// quantity and order count. Writing the same key again is what conflates — a level that moves several times
    /// inside one flush is sent once, at its final value — and the whole thing is bounded by the ladder.
    dirty: BTreeMap<u64, Levels>,
    history: BTreeMap<u64, History>,
    connected: bool,
    last_unit: Option<(Instant, u64)>,
    /// Orders the node has accepted but the feed has not delivered yet: order id to (sequence, account, instrument).
    accepted: BTreeMap<u64, (u64, u64, u64)>,
    /// The same order ids in sequence order, so the oldest can be dropped without scanning the map.
    accepted_order: VecDeque<u64>,
    /// The highest input sequence the view has applied.
    last_sequence: u64,
    /// The highest order ID the feed has delivered. The engine numbers a group's orders in the order it accepts them
    /// (spec E-2.8, E-9.8), so an order ID above this is one the feed has not delivered yet, not one that has gone.
    highest_order: u64,
    /// Requests waiting for the feed to deliver an order, each with its deadline and whom to tell (`await_order`).
    awaiting: Vec<(u64, Instant, Notify)>,
}

impl Hub {
    pub fn new(catalogue: Catalogue) -> Self {
        Self::founded_on(catalogue, View::default())
    }

    /// A hub holding `view` already, rather than nothing — the reader's half of a snapshot pair
    /// (`venue_link::pair`), for a gateway serving a venue founded on a snapshot rather than on genesis.
    ///
    /// Everything else a hub holds is either bounded and derived from the units it goes on to see (`history`,
    /// `accepted`) or comes from the catalogue and its live connections rather than the feed, so the view is the
    /// whole of what it needs restoring. `last_sequence` follows the view's, so the feed's own resume handshake
    /// then asks for what came after it — no part of the protocol changes for this.
    pub fn founded_on(catalogue: Catalogue, view: View) -> Self {
        let tokens = catalogue
            .accounts()
            .iter()
            .filter_map(|a| Some((a["token"].as_str()?.to_string(), a["id"].as_u64()?)))
            .collect();
        let sequence = view.sequence;
        Self {
            view,
            catalogue,
            tokens,
            sessions: BTreeMap::new(),
            by_account: BTreeMap::new(),
            by_topic: BTreeMap::new(),
            next_key: 0,
            dirty: BTreeMap::new(),
            history: BTreeMap::new(),
            connected: false,
            last_unit: None,
            accepted: BTreeMap::new(),
            accepted_order: VecDeque::new(),
            last_sequence: sequence,
            highest_order: 0,
            awaiting: Vec::new(),
        }
    }

    pub fn account_for(&self, token: &str) -> Option<u64> {
        self.tokens.get(token).copied()
    }

    /// Before each feed connection's backlog: the view starts again from the beginning of the journal.
    pub fn restart(&mut self) {
        self.view = View::default();
        self.dirty.clear();
        self.connected = true;
        self.last_unit = None;
        // The replay starts from the beginning, so accepted orders stay remembered until it reaches them again.
        self.last_sequence = 0;
    }

    /// Before a connection's backlog when this gateway means to carry on: what it keeps is the view, and what it
    /// answers is the sequence that view has reached.
    ///
    /// Nothing else has to be undone, and the one mechanism that might have needed it turns out to prefer this.
    /// `accepted` remembers orders the node acknowledged before the feed delivered them, keyed by the sequence
    /// they were accepted at, and entries are forgotten once `last_sequence` passes them. `restart` sets that to
    /// zero so they survive a replay from the beginning; resuming leaves it where it was, which is already
    /// consistent with the entries still held.
    ///
    /// Safe because the feed is unit-atomic: a connection that drops part-way through a unit never delivers it,
    /// so the view always stands at a unit boundary — the one this answers with.
    pub fn resume(&mut self) -> u64 {
        self.connected = true;
        self.last_unit = None;
        self.last_sequence
    }

    /// The feed has caught up: its last input is recent, or nothing has arrived for a moment.
    pub fn ready(&self) -> bool {
        self.connected
            && self.last_unit.is_none_or(|(at, sequenced_at)| {
                monotonic().duration_since(at) > Duration::from_millis(500)
                    || wall_ns().saturating_sub(sequenced_at) < 3_000_000_000
            })
    }

    pub fn market(&self, instrument: u64) -> Option<(Vec<Row>, u64)> {
        let i = self.view.instruments.get(&instrument)?;
        Some((self.view.ladders.get(&i.table)?.clone(), i.lot))
    }

    /// The owner and instrument of a resting order, from the feed or, until the feed catches up, from what the node
    /// itself said when it accepted the order.
    pub fn order(&self, order: u64) -> Option<(u64, u64)> {
        self.view
            .orders
            .get(&order)
            .map(|o| (o.account, o.instrument))
            .or_else(|| self.accepted.get(&order).map(|&(_, account, instrument)| (account, instrument)))
    }

    /// Remembers an order the node has just accepted. A client learns its order id from the node's reply, which
    /// comes straight back, while the feed arrives separately; without this it could amend or cancel an order the
    /// gateway has not heard of yet and be told there is no such order. The entry is consulted only until the view
    /// has applied that sequence, after which the feed is authoritative again, so an order that has since been
    /// cancelled or filled is never resurrected.
    pub fn accept(&mut self, order: u64, sequence: u64, account: u64, instrument: u64) {
        if sequence <= self.last_sequence {
            return;
        }
        if self.accepted.insert(order, (sequence, account, instrument)).is_none() {
            self.accepted_order.push_back(order);
        }
        while self.accepted_order.len() > ACCEPTED {
            if let Some(oldest) = self.accepted_order.pop_front() {
                self.accepted.remove(&oldest);
            }
        }
    }

    /// Whether `order` is past every order ID the feed has delivered: accepted by the node, as far as this gateway
    /// can tell, but not delivered here yet. Another gateway's client learns an order ID from its own gateway's reply,
    /// which can reach it before this gateway's feed has the order.
    pub fn unseen(&self, order: u64) -> bool {
        order > self.highest_order && !self.accepted.contains_key(&order)
    }

    /// Calls `notify` once the feed has delivered `order`, or once `deadline` has passed (`expire_awaiting`), whichever
    /// is first. False, and nothing remembered, when too many requests are waiting already.
    pub fn await_order(&mut self, order: u64, deadline: Instant, notify: Notify) -> bool {
        if self.awaiting.len() >= MAX_AWAITING {
            return false;
        }
        self.awaiting.push((order, deadline, notify));
        true
    }

    /// Tells the requests whose deadline has passed, so they can be answered.
    pub fn expire_awaiting(&mut self) {
        let now = monotonic();
        self.awaiting.retain(|(_, deadline, notify)| {
            let expired = *deadline <= now;
            if expired {
                notify();
            }
            !expired
        });
    }

    fn quote(&self, instrument: u64, tick: u64) -> Value {
        price::quote(ladder(&self.view, instrument), tick)
    }

    pub fn apply(&mut self, unit: &Unit) {
        let input = &unit.input;
        let sequence = u(&input["record"]["sequence"]);
        let executions = executions_of(&self.view, &unit.results, sequence);
        // Markets the engine releases with their events (E-8.4), found before the view forgets them.
        let released: Vec<(u64, Vec<u64>)> = unit
            .results
            .iter()
            .filter(|r| r.template() == event_released_codec::SBE_TEMPLATE_ID)
            .map(|r| {
                let event = u(&r["eventId"]);
                (event, self.view.events.get(&event).map(|e| e.instruments.clone()).unwrap_or_default())
            })
            .collect();
        self.view.apply(unit);
        for (_, markets) in &released {
            for market in markets {
                self.dirty.remove(market);
            }
        }
        self.last_unit = Some((monotonic(), u(&input["record"]["sequencedAt"])));
        // The view has now applied this unit, so every order accepted at or before it can be forgotten here.
        self.last_sequence = self.last_sequence.max(sequence);
        // By template id and then straight out of the decoder: this runs over every result of every unit, and
        // asking it through the JSON shape means decoding frames nothing else reads (ADR-0002).
        let highest = unit
            .results
            .iter()
            .filter(|r| r.template() == order_accepted_codec::SBE_TEMPLATE_ID)
            .map(order_accepted_order_id)
            .max();
        if let Some(highest) = highest.filter(|h| *h > self.highest_order) {
            self.highest_order = highest;
            self.awaiting.retain(|(order, _, notify)| {
                let delivered = *order <= highest;
                if delivered {
                    notify();
                }
                !delivered
            });
        }
        while let Some(&oldest) = self.accepted_order.front() {
            if self.accepted.get(&oldest).is_some_and(|&(at, _, _)| at > self.last_sequence) {
                break;
            }
            self.accepted_order.pop_front();
            self.accepted.remove(&oldest);
        }
        for (account, execution) in executions {
            let execution = json!({"type": "execution", "execution": execution}).to_string();
            self.send_to_account(account, sequence, "execution", execution);
        }
        let mut accounts: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
        let event_of = |view: &View, instrument: u64| view.instruments.get(&instrument).map_or(0, |i| i.event);
        // A void pays per account with instrument 0; its event is the one whose markets this unit settled.
        let settled_event = unit
            .results
            .iter()
            .find(|r| r.template() == market_state_changed_codec::SBE_TEMPLATE_ID)
            .map_or(0, |r| event_of(&self.view, u(&r["instrumentId"])));
        for r in &unit.results {
            // The engine already said exactly what moved, so keep it rather than re-reading the book later.
            // Read typed: a book level moves on nearly every unit and nothing else here looks at the frame, so
            // this is the one result a busy feed never has to decode at all (ADR-0002).
            if r.template() == book_level_changed_codec::SBE_TEMPLATE_ID {
                let mut dec = book_level_changed_codec::BookLevelChangedDecoder::default().header(header_of(r), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                let level = (dec.side() == Side::BUY, u64::from(dec.tick()));
                let moved = (u64::from(dec.quantity()), u64::from(dec.order_count()));
                self.dirty.entry(dec.instrument_id()).or_default().insert(level, moved);
                continue;
            }
            let instrument = u(&r["instrumentId"]);
            match r["msg"].as_str().unwrap_or("") {
                "PositionChanged" => accounts
                    .entry(u(&r["accountId"]))
                    .or_default()
                    .push(json!({"instrumentId": id(instrument), "position": r["position"]})),
                "SleeveChanged" => {
                    accounts.entry(u(&r["accountId"])).or_default();
                }
                "SettlementPayout" if u(&r["accountId"]) != 0 => {
                    let event = if instrument == 0 { settled_event } else { event_of(&self.view, instrument) };
                    let payout = json!({
                        "type": "payout", "sequence": id(sequence), "eventId": id(event), "instrumentId": id(instrument),
                        "kind": lower(&r["kind"]), "positionClosed": r["positionClosed"], "cents": r["cents"]
                    });
                    self.send_to_account(u(&r["accountId"]), sequence, "payout", payout.to_string());
                }
                // A set burn: complete sets the account held went back as cash (spec E-14.3), which would
                // otherwise show only as positions falling and cash rising.
                "CollateralReleased" => {
                    let release = json!({
                        "type": "release", "sequence": id(sequence), "eventId": id(u(&r["eventId"])),
                        "kind": lower(&r["kind"]), "cents": r["cents"]
                    });
                    self.send_to_account(u(&r["accountId"]), sequence, "release", release.to_string());
                }
                "Fill" => {
                    let trade = json!({
                        "type": "trade", "sequence": id(sequence), "instrumentId": id(instrument),
                        "tradeId": id(u(&r["tradeId"])), "price": self.quote(instrument, u(&r["tick"])),
                        "quantity": r["quantity"], "takerSide": lower(&r["takerSide"]),
                        "at": price::format_timestamp(u(&r["result"]["sequencedAt"]))
                    });
                    self.send_to_topic(&format!("trades.{instrument}"), &trade.to_string());
                }
                "MarketStateChanged" => {
                    let event = self.view.instruments.get(&instrument).map_or(0, |i| i.event);
                    let market = json!({
                        "type": "market", "sequence": id(sequence), "eventId": id(event),
                        "instrumentId": id(instrument), "state": lower(&r["state"])
                    });
                    self.send_to_topic("markets", &market.to_string());
                }
                _ => {}
            }
        }
        for (account, positions) in accounts {
            let (available, reserved) = self.view.sleeves.get(&account).copied().unwrap_or((0, 0));
            let update = json!({
                "type": "account", "sequence": id(sequence), "availableCents": available, "reservedCents": reserved,
                "positions": positions
            });
            self.send_to_account(account, sequence, "account", update.to_string());
        }
        // A released event and its markets are gone from `GET /v4/markets` and their books; followers forget them.
        for (event, _) in &released {
            let gone = json!({"type": "market", "sequence": id(sequence), "eventId": id(*event), "released": true});
            self.send_to_topic("markets", &gone.to_string());
        }
        if input["msg"] == "SetEventPhase" && unit.results.iter().any(applied_ok) {
            let phase = json!({
                "type": "market", "sequence": id(sequence), "eventId": id(u(&input["eventId"])),
                "phase": lower(&input["phase"])
            });
            self.send_to_topic("markets", &phase.to_string());
        }
        // After each market's own state change, the event's resolution as a whole: a void and a settlement both
        // show as markets leaving trading, and only this says which, and which state occurred.
        let resolved = match input["msg"].as_str() {
            Some("ResolveEvent") => Some(u(&input["eventId"])),
            // Expiring a parlay resolves its event (E-15.7).
            Some("ExpireParlay") => self.view.instruments.get(&u(&input["instrumentId"])).map(|i| i.event),
            _ => None,
        };
        if let Some(event) = resolved.filter(|_| unit.results.iter().any(applied_ok)) {
            let resolved = json!({
                "type": "market", "sequence": id(sequence), "eventId": id(event), "resolution": self.resolution(event)
            });
            self.send_to_topic("markets", &resolved.to_string());
        }
    }

    /// The parlay an event was listed for, if it was. The catalogue knows nothing of parlays: they are listed on
    /// request, and their names come from their legs.
    fn parlay(&self, event: u64) -> Option<&InstrumentView> {
        let e = self.view.events.get(&event)?;
        e.instruments.iter().filter_map(|i| self.view.instruments.get(i)).find(|i| i.class == "PARLAY")
    }

    /// An event's outcome names. A parlay's event has two, and its YES pays in the second (spec D-22).
    fn outcomes(&self, event: u64, count: u64) -> Vec<String> {
        match self.parlay(event) {
            Some(_) => vec!["Loses".into(), "Wins".into()],
            None => self.catalogue.states(event, count),
        }
    }

    fn title(&self, event: u64) -> String {
        let Some(parlay) = self.parlay(event) else { return self.catalogue.title(event) };
        let legs: Vec<String> = parlay
            .legs
            .iter()
            .map(|l| format!("{} {}", self.catalogue.label(l.event, l.instrument), if l.yes { "yes" } else { "no" }))
            .collect();
        format!("Parlay: {}", legs.join(" + "))
    }

    /// A parlay's legs as `GET /v4/markets` shows them, each named from the catalogue.
    fn legs(&self, market: &InstrumentView) -> Vec<Value> {
        market
            .legs
            .iter()
            .map(|l| {
                json!({
                    "eventId": id(l.event), "instrumentId": id(l.instrument), "outcome": if l.yes { "yes" } else { "no" },
                    "title": self.catalogue.title(l.event), "label": self.catalogue.label(l.event, l.instrument)
                })
            })
            .collect()
    }

    /// A message from the parlay service for some accounts' private sessions, presented as everything else is. It is
    /// not kept for sessions resuming by sequence: it is not from the journal and has none.
    pub fn deliver(&self, accounts: &[u64], message: &Value) {
        let text = self.present(message.clone()).to_string();
        for account in accounts {
            for key in self.by_account.get(account).into_iter().flatten() {
                if let Some(s) = self.sessions.get(key) {
                    let _ = s.tx.send(text.clone());
                }
            }
        }
    }

    /// A message from the parlay service for some sessions, by their keys, presented as everything else is. Not kept
    /// for resuming either.
    pub fn deliver_to_sessions(&self, keys: &[u64], message: &Value) {
        let text = self.present(message.clone()).to_string();
        for key in keys {
            if let Some(s) = self.sessions.get(key) {
                let _ = s.tx.send(text.clone());
            }
        }
    }

    /// What the parlay service says, in interface 1's terms: a price it gives as a tick on an instrument becomes a
    /// price quote from that instrument's ladder, and a time in nanoseconds a timestamp. The service leaves both to
    /// the gateway, which owns how prices and times are shown.
    pub fn present(&self, mut value: Value) -> Value {
        let Some(object) = value.as_object_mut() else { return value };
        let instrument = object.get("instrumentId").and_then(Value::as_str).and_then(|s| s.parse::<u64>().ok());
        let tick = object.get("price").and_then(|p| p.get("tick")).and_then(Value::as_u64);
        if let (Some(instrument), Some(tick)) = (instrument, tick) {
            object.insert("price".into(), self.quote(instrument, tick));
        }
        for key in ["openedAt", "expiresAt"] {
            if let Some(ns) = object.get(key).and_then(Value::as_u64) {
                object.insert(key.into(), json!(price::format_timestamp(ns)));
            }
        }
        for list in ["quotes", "requests"] {
            if let Some(items) = object.get_mut(list).and_then(Value::as_array_mut) {
                for item in items.iter_mut() {
                    *item = self.present(item.take());
                }
            }
        }
        value
    }

    /// How an event was resolved, as `GET /v4/markets` and the `markets` topic show it; null while it is not.
    fn resolution(&self, event: u64) -> Value {
        let Some(r) = self.view.events.get(&event).and_then(|e| e.resolution.as_ref()) else { return Value::Null };
        if r.kind == "VOID" {
            return json!({"kind": "void", "at": price::format_timestamp(r.at)});
        }
        let outcome = self.outcomes(event, r.winning_state + 1).get(r.winning_state as usize).cloned();
        json!({
            "kind": "resolved", "winningState": r.winning_state, "outcome": outcome,
            "at": price::format_timestamp(r.at)
        })
    }

    /// A frame rendered from `sequence`'s results, to every session of the account, and kept for one resuming.
    /// `kind` is the frame's `type`, which the audit log records in place of the frame (`Derived`).
    fn send_to_account(&mut self, account: u64, sequence: u64, kind: &'static str, message: String) {
        let history = self.history.entry(account).or_default();
        history.entries.push_back((sequence, message.clone()));
        while history.entries.len() > HISTORY {
            if let Some((dropped, _)) = history.entries.pop_front() {
                history.dropped_through = dropped;
            }
        }
        for key in self.by_account.get(&account).into_iter().flatten() {
            if let Some(s) = self.sessions.get(key) {
                let _ = s.tx.send_derived(message.clone(), kind, sequence);
            }
        }
    }

    /// A transfer update from the treasury, to the account's sessions now. Not kept for resuming: it did not come from
    /// the journal and has no sequence, so a client that reconnects reads the transfers again instead. Every session
    /// may see them: seeing transfers takes `read`, which every credential holds.
    pub fn send_transfer(&self, account: u64, message: &str) {
        for key in self.by_account.get(&account).into_iter().flatten() {
            if let Some(s) = self.sessions.get(key) {
                let _ = s.tx.send(message.to_string());
            }
        }
    }

    fn send_to_topic(&self, topic: &str, message: &str) {
        for key in self.by_topic.get(topic).into_iter().flatten() {
            if let Some(s) = self.sessions.get(key) {
                let _ = s.tx.send(message.to_string());
            }
        }
    }

    pub fn add_session(&mut self, tx: Outbox) -> u64 {
        self.next_key += 1;
        self.sessions.insert(self.next_key, Session { account: None, tx, topics: BTreeSet::new(), depth: 20 });
        self.next_key
    }

    pub fn remove_session(&mut self, key: u64) {
        let Some(s) = self.sessions.remove(&key) else { return };
        if let Some(account) = s.account {
            unindex(&mut self.by_account, &account, key);
        }
        for topic in &s.topics {
            unindex(&mut self.by_topic, topic, key);
        }
    }

    pub fn log_in(&mut self, key: u64, account: u64) {
        if let Some(s) = self.sessions.get_mut(&key) {
            s.account = Some(account);
            self.by_account.entry(account).or_default().insert(key);
        }
    }

    /// The account's private messages after `from`, or `None` if some of them are no longer held.
    pub fn since(&self, account: u64, from: u64) -> Option<Vec<String>> {
        let Some(h) = self.history.get(&account) else { return Some(Vec::new()) };
        (from >= h.dropped_through)
            .then(|| h.entries.iter().filter(|(s, _)| *s > from).map(|(_, m)| m.clone()).collect())
    }

    pub fn subscribe(&mut self, key: u64, topics: &[String], depth: usize) {
        // The images are taken before the topics are registered, and both happen while this lock is held, so no
        // unit can land between a book's image and the first change sent after it.
        let images: Vec<String> =
            topics.iter().filter_map(|t| t.strip_prefix("book.")?.parse().ok()).map(|i| self.image(i)).collect();
        if let Some(s) = self.sessions.get_mut(&key) {
            s.topics.extend(topics.iter().cloned());
            for topic in topics {
                self.by_topic.entry(topic.clone()).or_default().insert(key);
            }
            s.depth = depth;
            for image in images {
                let _ = s.tx.send(image);
            }
        }
    }

    pub fn unsubscribe(&mut self, key: u64, topics: &[String]) {
        if let Some(s) = self.sessions.get_mut(&key) {
            for t in topics {
                if s.topics.remove(t) {
                    unindex(&mut self.by_topic, t, key);
                }
            }
        }
    }

    /// Sends the levels that moved since the last call to each book's subscribers. Calling this at a fixed rate is
    /// the conflation: one message per book per tick, carrying only what changed, whatever the churn.
    pub fn flush_books(&mut self) {
        for (instrument, levels) in std::mem::take(&mut self.dirty) {
            let topic = format!("book.{instrument}");
            // An entry leaves the index with its last session, so a topic in it has someone to send to.
            if !self.by_topic.contains_key(&topic) {
                continue;
            }
            let message = self.levels_message(instrument, &levels, false);
            self.send_to_topic(&topic, &message);
        }
    }

    /// Levels as a client applies them: a quantity of zero means the level has gone. `snapshot` says this is the
    /// whole book rather than what changed, so a client knows to start again from it.
    fn levels_message(&self, instrument: u64, levels: &Levels, snapshot: bool) -> String {
        let ladder = ladder(&self.view, instrument);
        let changes: Vec<Value> = levels
            .iter()
            .map(|(&(buy, tick), &(quantity, orders))| {
                json!({
                    "side": if buy { "buy" } else { "sell" },
                    "price": price::quote(ladder, tick), "quantity": quantity, "orders": orders
                })
            })
            .collect();
        let market = self.view.instruments.get(&instrument);
        // `last` is one price rather than a list, so sending where it stands costs nothing and saves a client
        // having to follow the trades topic to know it.
        typed(
            "book",
            json!({
                "instrumentId": id(instrument), "sequence": id(self.view.sequence),
                "state": market.map(|i| i.state.to_lowercase()),
                "last": market.and_then(|i| i.last).map(|t| price::quote(ladder, t)),
                "snapshot": snapshot, "changes": changes
            }),
        )
    }

    /// Every level of a book, flagged, for a client that has just asked for it.
    fn image(&self, instrument: u64) -> String {
        let mut levels = Levels::new();
        if let Some(i) = self.view.instruments.get(&instrument) {
            for (&tick, &level) in &i.bids {
                levels.insert((true, tick), level);
            }
            for (&tick, &level) in &i.asks {
                levels.insert((false, tick), level);
            }
        }
        self.levels_message(instrument, &levels, true)
    }

    pub fn book(&self, instrument: u64, depth: usize) -> Option<Value> {
        let i = self.view.instruments.get(&instrument)?;
        let ladder = ladder(&self.view, instrument);
        let level = |(tick, (quantity, orders)): (&u64, &(u64, u64))| json!({"price": price::quote(ladder, *tick), "quantity": quantity, "orders": orders});
        Some(json!({
            "instrumentId": id(instrument), "sequence": id(self.view.sequence), "state": i.state.to_lowercase(),
            "bids": i.bids.iter().rev().take(depth).map(level).collect::<Vec<_>>(),
            "asks": i.asks.iter().take(depth).map(level).collect::<Vec<_>>(),
            "last": i.last.map(|t| price::quote(ladder, t))
        }))
    }

    /// Every event, or with `resolved` only those that are (or are not) resolved: a venue that lists rounds on a
    /// calendar accumulates settled events, and a client that trades wants the live ones without every old ladder.
    pub fn markets(&self, resolved: Option<bool>) -> Value {
        let events: Vec<Value> = self
            .view
            .events
            .iter()
            .filter(|(_, e)| resolved.is_none_or(|r| e.resolved == r))
            .map(|(&event, e)| {
                let markets: Vec<Value> = e
                    .instruments
                    .iter()
                    .filter_map(|&instrument| {
                        let i = self.view.instruments.get(&instrument)?;
                        let ladder = ladder(&self.view, instrument);
                        let parlay = i.class == "PARLAY";
                        let label = if parlay { "Parlay".to_string() } else { self.catalogue.label(event, instrument) };
                        let mut market = json!({
                            "instrumentId": id(instrument), "label": label,
                            "class": i.class.to_lowercase(), "state": i.state.to_lowercase(), "lotStep": i.lot,
                            "ladder": (0..ladder.len() as u64).map(|t| price::quote(ladder, t)).collect::<Vec<_>>()
                        });
                        if parlay {
                            market["legs"] = json!(self.legs(i));
                        }
                        Some(market)
                    })
                    .collect();
                json!({
                    "eventId": id(event), "title": self.title(event),
                    "outcomes": self.outcomes(event, e.state_count), "started": e.started,
                    "suspended": e.suspended, "ended": e.ended, "resolved": e.resolved, "markets": markets,
                    "scheduledStart": (e.scheduled_start > 0).then(|| price::format_timestamp(e.scheduled_start)),
                    "resolution": self.resolution(event)
                })
            })
            .collect();
        json!({"sequence": id(self.view.sequence), "events": events})
    }

    pub fn account(&self, account: u64) -> Value {
        let (available, reserved) = self.view.sleeves.get(&account).copied().unwrap_or((0, 0));
        let positions: Vec<Value> = self
            .view
            .positions
            .iter()
            .filter(|((a, _), _)| *a == account)
            .map(|((_, instrument), position)| json!({"instrumentId": id(*instrument), "position": position}))
            .collect();
        let orders: Vec<Value> = self
            .view
            .orders
            .iter()
            .filter(|(_, o)| o.account == account)
            .map(|(order, o)| {
                // Enough for a client that lost its session to take the order up again as its own: the ID it
                // placed it under, its whole size and what has filled at what cost (the FIX adapter's order status).
                let event = self.view.instruments.get(&o.instrument).map_or(0, |i| i.event);
                json!({
                    "orderId": id(*order), "clientOrderId": o.client_order_id, "instrumentId": id(o.instrument),
                    "eventId": id(event), "side": o.side.to_lowercase(), "price": self.quote(o.instrument, o.tick),
                    "quantity": o.filled.saturating_add(o.remaining), "remaining": o.remaining, "filled": o.filled,
                    "filledYesCents": o.filled_yes_cents
                })
            })
            .collect();
        json!({
            "accountId": id(account), "sequence": id(self.view.sequence), "availableCents": available,
            "reservedCents": reserved, "positions": positions, "orders": orders
        })
    }
}

fn typed(kind: &str, mut body: Value) -> String {
    body["type"] = json!(kind);
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use venue_link::view::OrderView;
    use venue_link::wire::Frame;

    /// A session's outbox, and what arrives in it.
    fn outbox() -> (Outbox, mpsc::Receiver<Queued>) {
        let (tx, rx) = mpsc::channel();
        (Outbox::new(tx, Arc::new(|| {})), rx)
    }

    #[test]
    fn private_messages_reach_the_accounts_sessions_and_no_others() {
        let mut hub = hub();
        let (first, first_rx) = outbox();
        let (other, other_rx) = outbox();
        let (anonymous, anonymous_rx) = outbox();
        let first = hub.add_session(first);
        let other = hub.add_session(other);
        hub.add_session(anonymous);
        hub.log_in(first, 1);
        hub.log_in(other, 2);
        hub.apply(&unit(1, vec![accepted(7)]));
        assert!(first_rx.try_iter().count() > 0, "account 1's session was not told");
        assert_eq!(other_rx.try_iter().count(), 0, "another account's session was told");
        assert_eq!(anonymous_rx.try_iter().count(), 0, "a session logged in as nobody was told");
        // Removing a session leaves the account's entry usable for the next session to log in as it.
        hub.remove_session(first);
        let (next, next_rx) = outbox();
        let next = hub.add_session(next);
        hub.log_in(next, 1);
        hub.apply(&unit(2, vec![accepted(8)]));
        assert!(next_rx.try_iter().count() > 0, "a later session for the account was not told");
        assert_eq!(other_rx.try_iter().count(), 0);
    }

    /// A hub founded on a view holds it, and resumes from where that view had got to rather than from the
    /// beginning — which is the whole point: a venue founded on a snapshot has no inputs in its journal to send
    /// a hub that asks for everything, so asking for everything is how a gateway ends up serving an empty venue.
    #[test]
    fn a_hub_founded_on_a_view_resumes_from_where_that_view_had_got_to() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json");
        let catalogue = Catalogue::load(path).expect("the mini venue's catalogue");
        let mut view = View::default();
        view.sequence = 62;
        view.instruments.insert(1000, venue_link::view::InstrumentView::default());

        let mut founded = Hub::founded_on(catalogue, view);
        assert_eq!(founded.resume(), 62, "a founded hub asked for everything, not for what followed its view");
        assert!(founded.view.instruments.contains_key(&1000), "the view it was founded on was not kept");

        // And the ordinary path is unchanged: nothing held, so everything wanted.
        assert_eq!(hub().resume(), 0);
    }

    /// The mini venue's catalogue: a hub needs one, and these tests need nothing from it.
    fn hub() -> Hub {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json");
        Hub::new(Catalogue::load(path).expect("the mini venue's catalogue"))
    }

    /// One unit off the feed: the input with its record header, and the results it produced.
    fn unit(sequence: u64, results: Vec<Value>) -> Unit {
        let record = json!({"sequence": sequence, "sequencedAt": 0, "sequencerGroup": 1});
        Unit {
            input: Frame::of(&json!({"msg": "NewOrder", "record": record})),
            results: results.iter().map(Frame::of).collect(),
        }
    }

    fn accepted(order: u64) -> Value {
        json!({"msg": "OrderAccepted", "orderId": order, "accountId": 1, "instrumentId": 1000,
               "side": "BUY", "tick": 9, "quantity": 1})
    }

    #[test]
    fn an_accepted_order_resolves_before_the_feed_has_delivered_it() {
        let mut hub = hub();
        assert_eq!(hub.order(7), None);
        // The node has answered the client, and the feed has not arrived yet. Without the record the client would
        // be told there is no such order, moments after the node accepted it.
        hub.accept(7, 5, 1, 1000);
        assert_eq!(hub.order(7), Some((1, 1000)));
        // The feed catches up with that same input, and says the same thing.
        hub.apply(&unit(5, vec![accepted(7)]));
        assert_eq!(hub.order(7), Some((1, 1000)));
        // Once the feed says the order has gone, it has gone: the record must not bring it back.
        let cancelled = json!({"msg": "OrderCancelled", "orderId": 7, "accountId": 1, "instrumentId": 1000});
        hub.apply(&unit(6, vec![cancelled]));
        assert_eq!(hub.order(7), None);
    }

    /// An order ID above every one the feed has delivered is one not delivered yet: whoever waits for it is told when it
    /// is, and one past its deadline is told too. An ID the feed has passed is not waited for, resting or not.
    #[test]
    fn an_order_the_feed_has_not_delivered_is_waited_for() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let mut hub = hub();
        hub.apply(&unit(1, vec![accepted(7)]));
        assert!(!hub.unseen(6) && !hub.unseen(7) && hub.unseen(8));
        let told = Arc::new(AtomicUsize::new(0));
        let counter = |told: &Arc<AtomicUsize>| -> Notify {
            let told = Arc::clone(told);
            Arc::new(move || {
                told.fetch_add(1, Ordering::SeqCst);
            })
        };
        let later = monotonic() + Duration::from_secs(60);
        assert!(hub.await_order(9, later, counter(&told)));
        assert!(hub.await_order(12, later, counter(&told)));
        hub.apply(&unit(2, vec![accepted(8)]));
        assert_eq!(told.load(Ordering::SeqCst), 0, "told before its order was delivered");
        hub.apply(&unit(3, vec![accepted(9), accepted(10)]));
        assert_eq!(told.load(Ordering::SeqCst), 1, "not told once its order was delivered");
        assert!(!hub.unseen(9) && hub.unseen(12));
        let expired = Arc::new(AtomicUsize::new(0));
        assert!(hub.await_order(40, monotonic(), counter(&expired)));
        hub.expire_awaiting();
        assert_eq!((told.load(Ordering::SeqCst), expired.load(Ordering::SeqCst)), (1, 1));
    }

    #[test]
    fn a_record_the_view_has_already_passed_is_not_kept() {
        let mut hub = hub();
        hub.apply(&unit(6, Vec::new()));
        // There is nothing to remember: the view has applied input 6, so it has already seen whatever input 3 did.
        hub.accept(8, 3, 1, 1000);
        assert_eq!(hub.order(8), None);
    }

    #[test]
    fn a_released_event_leaves_the_markets_and_its_followers_are_told() {
        let mut hub = hub();
        let (follower, follower_rx) = outbox();
        let follower = hub.add_session(follower);
        hub.subscribe(follower, &["markets".to_string()], 10);
        let applied = [json!({"msg": "CommandApplied", "outcome": "APPLIED"})];
        let command = |sequence: u64, input: Value| {
            let mut input = input;
            input["record"] = json!({"sequence": sequence, "sequencedAt": sequence, "sequencerGroup": 1});
            Unit { input: Frame::of(&input), results: applied.iter().map(Frame::of).collect() }
        };
        hub.apply(&command(1, json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0})));
        let list = json!({"msg": "ListInstrument", "instrumentId": 50, "eventId": 5, "instrumentClass": "BINARY",
                          "tickTableId": 1, "lotStep": 1, "stateMask": 2, "legs": []});
        hub.apply(&command(2, list));
        assert_eq!(hub.markets(None)["events"].as_array().map(Vec::len), Some(1));
        follower_rx.try_iter().count();
        let tick = Unit {
            input: Frame::of(&json!({"msg": "TimeTick",
                "record": {"sequence": 3, "sequencedAt": 0, "sequencerGroup": 1}})),
            results: vec![Frame::of(&json!({"msg": "EventReleased", "eventId": 5}))],
        };
        hub.apply(&tick);
        assert_eq!(hub.markets(None)["events"].as_array().map(Vec::len), Some(0));
        assert!(hub.market(50).is_none());
        let told: Vec<Value> = follower_rx.try_iter().filter_map(|m| serde_json::from_str(&m.text).ok()).collect();
        assert!(told.iter().any(|m| m["released"] == true && m["eventId"] == "5"), "{told:?}");
    }

    #[test]
    fn each_party_to_a_fill_sees_its_own_fee() {
        let fill = json!({
            "msg": "Fill", "tradeId": 5, "instrumentId": 1000, "tick": 29, "quantity": 10, "makerOrderId": 1,
            "makerAccountId": 1, "makerRemaining": 0, "takerOrderId": 2, "takerAccountId": 2, "takerRemaining": 0,
            "takerSide": "SELL", "mintedPairs": 10, "burnedPairs": 0, "buyerCashCents": -300, "sellerCashCents": -700,
            "makerFeeCents": -3, "takerFeeCents": 15
        });
        let out = executions(&View::default(), [&fill], 9);
        let fee = |account: u64| out.iter().find(|(a, _)| *a == account).map(|(_, e)| e["feeCents"].clone());
        assert_eq!(fee(1), Some(json!(-3)));
        assert_eq!(fee(2), Some(json!(15)));
    }

    /// A book level moving is the most frequent thing on a live feed, and nothing in the gateway needs its JSON
    /// shape: the hub keeps the moved level for the next conflate flush and the view updates its book, both
    /// straight out of the decoder. So applying one must leave the frame undecoded.
    ///
    /// This is asserted rather than timed because the decoded shape is kept once anything asks for it, so a
    /// single reader still reaching for it costs the whole decode and shows up only as a slightly slower feed
    /// thread. `Frame::decoded` makes that visible (ADR-0002).
    #[test]
    fn a_book_level_is_applied_without_decoding_it() {
        let mut hub = hub();
        let moved = Frame::of(&json!({
            "msg": "BookLevelChanged",
            "result": {"inputSequence": 9, "resultIndex": 0, "sequencedAt": 0, "sequencerGroup": 1},
            "instrumentId": 10_010, "side": "BUY", "tick": 40, "quantity": 250, "orderCount": 2
        }));
        let input = Frame::of(&json!({"msg": "NewOrder",
            "record": {"sequence": 9, "sequencedAt": 0, "sequencerGroup": 1}}));
        let unit = Unit { input, results: vec![moved] };

        hub.apply(&unit);

        assert!(!unit.results[0].decoded(), "a book level change should never need its JSON shape");
        assert_eq!(
            hub.dirty.get(&10_010).and_then(|levels| levels.get(&(true, 40))),
            Some(&(250, 2)),
            "and the moved level is still what the engine said"
        );
    }

    /// Every result that owes the client an execution must still produce one once the feed path stopped
    /// decoding frames it has no use for.
    ///
    /// `executions_of` skips results before looking at them, which is what makes a book level free. The first
    /// cut of that skipped by an allow-list of the results `executions` answers for, and the list omitted
    /// `OrderAmended`: amend confirmations stopped reaching clients and all 98 tests still passed, because
    /// nothing asserted this. So this asserts it, for every arm rather than the one that broke.
    #[test]
    fn every_result_that_owes_an_execution_still_produces_one() {
        let result = json!({"inputSequence": 3, "resultIndex": 0, "sequencedAt": 0, "sequencerGroup": 1});
        let owed = [
            (
                "accepted",
                json!({"msg": "OrderAccepted", "result": result, "orderId": 1, "accountId": 7,
                                "sessionId": 1, "clientOrderId": "c1", "instrumentId": 10_010, "side": "BUY",
                                "tick": 40, "quantity": 5, "reservedCents": 200}),
            ),
            (
                "rejected",
                json!({"msg": "OrderRejected", "result": result, "accountId": 7, "sessionId": 1,
                                "clientOrderId": "c2"}),
            ),
            (
                "amended",
                json!({"msg": "OrderAmended", "result": result, "orderId": 1, "accountId": 7,
                               "instrumentId": 10_010}),
            ),
            (
                "cancelled",
                json!({"msg": "OrderCancelled", "result": result, "orderId": 1, "accountId": 7,
                                 "instrumentId": 10_010}),
            ),
            (
                "reduced",
                json!({"msg": "OrderReduced", "result": result, "orderId": 1, "accountId": 7,
                               "instrumentId": 10_010}),
            ),
            (
                "fill",
                json!({"msg": "Fill", "result": result, "tradeId": 9, "instrumentId": 10_010, "tick": 40,
                            "quantity": 2, "makerOrderId": 1, "makerAccountId": 7, "makerRemaining": 3,
                            "takerOrderId": 2, "takerAccountId": 8, "takerRemaining": 0, "takerSide": "SELL"}),
            ),
        ];
        let view = View::default();
        for (kind, message) in owed {
            let frames = [Frame::of(&message)];
            let out = executions_of(&view, &frames, 3);
            assert!(
                out.iter().any(|(_, e)| e["type"] == json!(kind)),
                "a {kind} execution should reach the client, got {out:?} for {message}"
            );
            // And filtering frames before looking at them must change nothing against reading them all.
            // Compared against the decoded frame rather than the literal above, so that what is being checked
            // is the filtering and not the schema filling in fields the literal left out.
            let unfiltered = executions(&view, frames.iter().map(|f| &**f), 3);
            assert_eq!(out, unfiltered, "filtering frames changed what a {kind} produces");
        }
    }

    /// A view with enough in it that the arms which consult one have something to find: a cent ladder so
    /// `quote` converts a tick to a real price, and a resting order so the branches that look one up are taken
    /// rather than skipped.
    fn view_for_executions() -> View {
        let mut view = View::default();
        view.ladders
            .insert(1, (0..=100).map(|t| Row { numerator: t, denominator: 100, complement: 100 - t }).collect());
        view.instruments.insert(
            10_010,
            InstrumentView { event: 7, table: 1, lot: 1, state: "OPEN".into(), ..InstrumentView::default() },
        );
        view.orders.insert(
            700,
            OrderView {
                account: 7,
                instrument: 10_010,
                side: "SELL".into(),
                tick: 60,
                remaining: 40,
                client_order_id: "resting".into(),
                filled: 0,
                filled_yes_cents: 0,
                expires_at: None,
            },
        );
        view
    }

    /// Every arm of `executions`, in an order that exercises what it carries between them.
    ///
    /// Two things here are deliberate and easy to lose. `OrderAccepted` for order 701 is followed by an
    /// `OrderCancelled` for the same order, which is *not* in the view: that arm falls back to the `accepted`
    /// map the earlier result populated, so the two cannot be read independently of each other. And the `Fill`
    /// carries distinct non-zero `buyerCashCents`, `sellerCashCents`, `makerFeeCents` and `takerFeeCents`, so a
    /// reader that crossed two of them over would be caught rather than agreeing at zero.
    fn execution_corpus() -> Vec<Value> {
        let result = |n: u64| {
            json!({"inputSequence": n, "resultIndex": 0, "sequencedAt": 1_727_000_000_000_000_000u64,
                                     "sequencerGroup": 1})
        };
        vec![
            json!({"msg": "OrderAccepted", "result": result(1), "orderId": 701, "accountId": 11, "sessionId": 3,
                   "clientOrderId": "cl-701", "instrumentId": 10_010, "side": "BUY", "tick": 40, "quantity": 25,
                   "reservedCents": 1_000}),
            // Cancelled for an order the view has never seen: only the `accepted` map above can price it.
            json!({"msg": "OrderCancelled", "result": result(2), "orderId": 701, "accountId": 11,
                   "instrumentId": 10_010, "cancelledQuantity": 25, "reason": "CLIENT_REQUEST"}),
            // And one the view does know, which takes the other branch.
            json!({"msg": "OrderCancelled", "result": result(3), "orderId": 700, "accountId": 7,
                   "instrumentId": 10_010, "cancelledQuantity": 40, "reason": "EVENT_START"}),
            json!({"msg": "OrderAmended", "result": result(4), "orderId": 700, "accountId": 7,
                   "instrumentId": 10_010, "tick": 55, "quantity": 30}),
            json!({"msg": "OrderReduced", "result": result(5), "orderId": 700, "accountId": 7,
                   "instrumentId": 10_010, "remainingQuantity": 12}),
            json!({"msg": "OrderRejected", "result": result(6), "accountId": 11, "sessionId": 3,
                   "clientOrderId": "cl-702", "originalOrderId": 701, "reason": "DUPLICATE_CLIENT_ORDER_ID"}),
            json!({"msg": "Fill", "result": result(7), "tradeId": 900, "instrumentId": 10_010, "tick": 40,
                   "quantity": 9, "makerOrderId": 700, "makerAccountId": 7, "makerRemaining": 3,
                   "takerOrderId": 701, "takerAccountId": 11, "takerRemaining": 16, "takerSide": "BUY",
                   "buyerCashCents": 360, "sellerCashCents": 540, "makerFeeCents": 7, "takerFeeCents": 13}),
            // The other side of the trade, so a reader that has maker and taker the wrong way round shows up.
            json!({"msg": "Fill", "result": result(8), "tradeId": 901, "instrumentId": 10_010, "tick": 70,
                   "quantity": 4, "makerOrderId": 701, "makerAccountId": 11, "makerRemaining": 12,
                   "takerOrderId": 700, "takerAccountId": 7, "takerRemaining": 0, "takerSide": "SELL",
                   "buyerCashCents": 280, "sellerCashCents": 120, "makerFeeCents": 2, "takerFeeCents": 5}),
        ]
    }

    /// The differential harness for `executions`, built before anything in it is read typed.
    ///
    /// `executions` is the last of the feed path still going through `serde_json::Value`, and it is the worst
    /// of it to move: six arms, state carried between them, two callers giving it results from different
    /// places, and the cash and fee fields of a trade. A field taken from the wrong place there does not fail
    /// -- a client is told it paid a different amount, and nothing says so.
    ///
    /// So this compares the ways of reading the same results and fails if they differ. Today that is frames
    /// against the JSON they decode to, which guards the filtering in `executions_of`. When a typed reader
    /// lands, it is one more path through the same corpus, compared the same way.
    ///
    /// Both whole and one at a time: the batch is what exercises the `accepted` map that the cancel arm reads,
    /// and the singles are what name the result that disagreed when one does. Stage 2 taught that an
    /// end-of-run comparison hides a divergence a later result happens to overwrite.
    #[test]
    fn executions_agrees_however_the_results_are_read() {
        let view = view_for_executions();
        let corpus = execution_corpus();
        let frames: Vec<Frame> = corpus.iter().map(Frame::of).collect();
        // Against what the frames decode to, not the literals above: the schema fills in fields a literal
        // leaves out, and that difference is not the one being looked for.
        let decoded: Vec<Value> = frames.iter().map(|f| (**f).clone()).collect();

        let whole_frames = executions_of(&view, &frames, 99);
        let whole_json = executions(&view, decoded.iter(), 99);
        assert_eq!(whole_frames, whole_json, "the two readings of the whole unit disagree");

        for (i, (frame, value)) in frames.iter().zip(&decoded).enumerate() {
            let one_frame = executions_of(&view, std::slice::from_ref(frame), 99);
            let one_json = executions(&view, [value], 99);
            assert_eq!(one_frame, one_json, "result {i} reads differently as a frame than as JSON: {value}");
        }

        // The corpus has to be doing something, or two readings of nothing would agree.
        assert_eq!(whole_json.len(), 10, "six arms, with a fill counted for each side: {whole_json:?}");
        let kinds: BTreeSet<&str> = whole_json.iter().filter_map(|(_, e)| e["type"].as_str()).collect();
        assert_eq!(
            kinds,
            ["accepted", "amended", "cancelled", "fill", "rejected", "reduced"].into_iter().collect(),
            "every arm is covered"
        );
        // And the cancel that only the `accepted` map could price carries a price at all.
        let cancelled_701 = whole_json
            .iter()
            .find(|(_, e)| e["type"] == json!("cancelled") && e["orderId"] == json!("701"))
            .expect("the cancel of the order the view never saw");
        assert!(
            cancelled_701.1["price"].is_object(),
            "priced from what the accept said, since the view has no such order: {:?}",
            cancelled_701.1
        );
    }
}
