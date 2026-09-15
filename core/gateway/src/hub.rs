//! The gateway's picture of the venue and its sessions. The feed updates the view; each input's results become
//! private messages for the accounts they concern and public messages for topic subscribers.

use crate::limits::monotonic;
use crate::price;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use venue_link::catalogue::Catalogue;
use venue_link::view::{Row, View};
use venue_link::wire::Unit;

/// Private messages kept per account for sessions resuming by sequence.
const HISTORY: usize = 2_000;

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
pub fn executions(view: &View, results: &[Value], sequence: u64) -> Vec<(u64, Value)> {
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

struct Session {
    account: Option<u64>,
    tx: Sender<String>,
    topics: BTreeSet<String>,
    depth: usize,
}

#[derive(Default)]
struct History {
    entries: VecDeque<(u64, String)>,
    dropped_through: u64,
}

pub struct Hub {
    pub view: View,
    catalogue: Catalogue,
    tokens: BTreeMap<String, u64>,
    sessions: BTreeMap<u64, Session>,
    next_key: u64,
    dirty: BTreeSet<u64>,
    history: BTreeMap<u64, History>,
    connected: bool,
    last_unit: Option<(Instant, u64)>,
}

impl Hub {
    pub fn new(catalogue: Catalogue) -> Self {
        let tokens = catalogue
            .accounts()
            .iter()
            .filter_map(|a| Some((a["token"].as_str()?.to_string(), a["id"].as_u64()?)))
            .collect();
        Self {
            view: View::default(),
            catalogue,
            tokens,
            sessions: BTreeMap::new(),
            next_key: 0,
            dirty: BTreeSet::new(),
            history: BTreeMap::new(),
            connected: false,
            last_unit: None,
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

    /// The owner and instrument of a resting order.
    pub fn order(&self, order: u64) -> Option<(u64, u64)> {
        self.view.orders.get(&order).map(|o| (o.account, o.instrument))
    }

    fn quote(&self, instrument: u64, tick: u64) -> Value {
        price::quote(ladder(&self.view, instrument), tick)
    }

    pub fn apply(&mut self, unit: &Unit) {
        let input = &unit.input;
        let sequence = u(&input["record"]["sequence"]);
        let executions = executions(&self.view, &unit.results, sequence);
        self.view.apply(unit);
        self.last_unit = Some((monotonic(), u(&input["record"]["sequencedAt"])));
        for (account, execution) in executions {
            self.send_to_account(account, sequence, json!({"type": "execution", "execution": execution}).to_string());
        }
        let mut accounts: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
        let event_of = |view: &View, instrument: u64| view.instruments.get(&instrument).map_or(0, |i| i.event);
        // A void pays per account with instrument 0; its event is the one whose markets this unit settled.
        let settled_event = unit
            .results
            .iter()
            .find(|r| r["msg"] == "MarketStateChanged")
            .map_or(0, |r| event_of(&self.view, u(&r["instrumentId"])));
        for r in &unit.results {
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
                    self.send_to_account(u(&r["accountId"]), sequence, payout.to_string());
                }
                // A set burn: complete sets the account held went back as cash (spec E-14.3), which would
                // otherwise show only as positions falling and cash rising.
                "CollateralReleased" => {
                    let release = json!({
                        "type": "release", "sequence": id(sequence), "eventId": id(u(&r["eventId"])),
                        "kind": lower(&r["kind"]), "cents": r["cents"]
                    });
                    self.send_to_account(u(&r["accountId"]), sequence, release.to_string());
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
                "BookLevelChanged" => {
                    self.dirty.insert(instrument);
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
            self.send_to_account(account, sequence, update.to_string());
        }
        if input["msg"] == "SetEventPhase" && unit.results.iter().any(|r| r["outcome"] == "APPLIED") {
            let phase = json!({
                "type": "market", "sequence": id(sequence), "eventId": id(u(&input["eventId"])),
                "phase": lower(&input["phase"])
            });
            self.send_to_topic("markets", &phase.to_string());
        }
    }

    fn send_to_account(&mut self, account: u64, sequence: u64, message: String) {
        let history = self.history.entry(account).or_default();
        history.entries.push_back((sequence, message.clone()));
        while history.entries.len() > HISTORY {
            if let Some((dropped, _)) = history.entries.pop_front() {
                history.dropped_through = dropped;
            }
        }
        for s in self.sessions.values().filter(|s| s.account == Some(account)) {
            let _ = s.tx.send(message.clone());
        }
    }

    fn send_to_topic(&self, topic: &str, message: &str) {
        for s in self.sessions.values().filter(|s| s.topics.contains(topic)) {
            let _ = s.tx.send(message.to_string());
        }
    }

    pub fn add_session(&mut self, tx: Sender<String>) -> u64 {
        self.next_key += 1;
        self.sessions.insert(self.next_key, Session { account: None, tx, topics: BTreeSet::new(), depth: 20 });
        self.next_key
    }

    pub fn remove_session(&mut self, key: u64) {
        self.sessions.remove(&key);
    }

    pub fn log_in(&mut self, key: u64, account: u64) {
        if let Some(s) = self.sessions.get_mut(&key) {
            s.account = Some(account);
        }
    }

    /// The account's private messages after `from`, or `None` if some of them are no longer held.
    pub fn since(&self, account: u64, from: u64) -> Option<Vec<String>> {
        let Some(h) = self.history.get(&account) else { return Some(Vec::new()) };
        (from >= h.dropped_through)
            .then(|| h.entries.iter().filter(|(s, _)| *s > from).map(|(_, m)| m.clone()).collect())
    }

    pub fn subscribe(&mut self, key: u64, topics: &[String], depth: usize) {
        let snapshots: Vec<String> = topics
            .iter()
            .filter_map(|t| t.strip_prefix("book.")?.parse().ok())
            .filter_map(|instrument| self.book(instrument, depth))
            .map(|book| typed("book", book))
            .collect();
        if let Some(s) = self.sessions.get_mut(&key) {
            s.topics.extend(topics.iter().cloned());
            s.depth = depth;
            for snapshot in snapshots {
                let _ = s.tx.send(snapshot);
            }
        }
    }

    pub fn unsubscribe(&mut self, key: u64, topics: &[String]) {
        if let Some(s) = self.sessions.get_mut(&key) {
            for t in topics {
                s.topics.remove(t);
            }
        }
    }

    /// Sends a snapshot of every book that changed since the last call to its subscribers. Calling this at a fixed
    /// rate is the conflation.
    pub fn flush_books(&mut self) {
        for instrument in std::mem::take(&mut self.dirty) {
            let topic = format!("book.{instrument}");
            let wanted: Vec<(usize, Sender<String>)> =
                self.sessions.values().filter(|s| s.topics.contains(&topic)).map(|s| (s.depth, s.tx.clone())).collect();
            let mut by_depth: BTreeMap<usize, Option<String>> = BTreeMap::new();
            for (depth, tx) in wanted {
                let snapshot =
                    by_depth.entry(depth).or_insert_with(|| self.book(instrument, depth).map(|b| typed("book", b)));
                if let Some(snapshot) = snapshot {
                    let _ = tx.send(snapshot.clone());
                }
            }
        }
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

    pub fn markets(&self) -> Value {
        let events: Vec<Value> = self
            .view
            .events
            .iter()
            .map(|(&event, e)| {
                let markets: Vec<Value> = e
                    .instruments
                    .iter()
                    .filter_map(|&instrument| {
                        let i = self.view.instruments.get(&instrument)?;
                        let ladder = ladder(&self.view, instrument);
                        Some(json!({
                            "instrumentId": id(instrument), "label": self.catalogue.label(event, instrument),
                            "class": i.class.to_lowercase(), "state": i.state.to_lowercase(), "lotStep": i.lot,
                            "ladder": (0..ladder.len() as u64).map(|t| price::quote(ladder, t)).collect::<Vec<_>>()
                        }))
                    })
                    .collect();
                json!({
                    "eventId": id(event), "title": self.catalogue.title(event),
                    "outcomes": self.catalogue.states(event, e.state_count), "started": e.started,
                    "suspended": e.suspended, "ended": e.ended, "resolved": e.resolved, "markets": markets
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
                json!({
                    "orderId": id(*order), "instrumentId": id(o.instrument), "side": o.side.to_lowercase(),
                    "price": self.quote(o.instrument, o.tick), "remaining": o.remaining
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
