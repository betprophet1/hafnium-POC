//! A reader's picture of the venue, built only from the node's feed: books, orders, positions, sleeves, trades and
//! payouts. The web page shows it and the bots decide from it.

use crate::catalogue::Catalogue;
use crate::wire::{self, Unit};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

const KEEP: usize = 60;

#[derive(Clone, Default)]
pub struct EventView {
    pub state_count: u64,
    pub started: bool,
    pub suspended: bool,
    pub ended: bool,
    pub resolved: bool,
    pub instruments: Vec<u64>,
    /// Sequencer times, in nanoseconds since the epoch, of the inputs that defined, started and ended the event, and
    /// the start `DefineEvent` scheduled (0 for none). A calendar counts its stages from these.
    pub defined_at: u64,
    pub scheduled_start: u64,
    pub started_at: Option<u64>,
    pub ended_at: Option<u64>,
    /// Whether the event has ever been suspended, so a calendar suspends a round at most once.
    pub suspended_ever: bool,
    /// How the event was resolved, once it has been: what a follower of settled events (a parlay's legs, say) needs.
    pub resolution: Option<Resolution>,
}

/// An applied `ResolveEvent` (E-15.4), as the feed recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// `RESOLVED` or `VOID`.
    pub kind: String,
    /// The state that occurred; 0 for a void.
    pub winning_state: u64,
    pub case_id: u64,
    pub at: u64,
}

#[derive(Clone, Default)]
pub struct InstrumentView {
    pub event: u64,
    pub table: u64,
    pub lot: u64,
    pub mask: u64,
    pub class: String,
    pub state: String,
    /// Tick to (quantity, order count).
    pub bids: BTreeMap<u64, (u64, u64)>,
    pub asks: BTreeMap<u64, (u64, u64)>,
    pub last: Option<u64>,
}

impl InstrumentView {
    pub fn best_bid(&self) -> Option<u64> {
        self.bids.keys().next_back().copied()
    }

    pub fn best_ask(&self) -> Option<u64> {
        self.asks.keys().next().copied()
    }
}

/// One row of a tick table, exactly as `DefineTickTable` gave it (spec E-6.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Row {
    pub numerator: u64,
    pub denominator: u64,
    pub complement: u64,
}

#[derive(Clone)]
pub struct OrderView {
    pub account: u64,
    pub instrument: u64,
    pub side: String,
    pub tick: u64,
    pub remaining: u64,
}

#[derive(Default)]
pub struct View {
    /// The last input applied; a reader has seen an input's effects once this reaches its sequence.
    pub sequence: u64,
    /// The sequencer's time at that input, in nanoseconds since the epoch. The node ticks (E-17.3), so this keeps
    /// moving while nothing else happens: it is the clock anything scheduled on sequencer time reads.
    pub now: u64,
    pub version: u64,
    /// Each table's ticks as basis points of probability, for display.
    pub tables: BTreeMap<u64, Vec<u64>>,
    /// Each table's rows as exact rationals, for anything that converts prices.
    pub ladders: BTreeMap<u64, Vec<Row>>,
    pub events: BTreeMap<u64, EventView>,
    pub instruments: BTreeMap<u64, InstrumentView>,
    pub orders: BTreeMap<u64, OrderView>,
    pub positions: BTreeMap<(u64, u64), i64>,
    /// Account to (available, reserved) cents.
    pub sleeves: BTreeMap<u64, (i64, i64)>,
    pub trades: VecDeque<Value>,
    pub payouts: VecDeque<Value>,
    /// The event whose markets were last settled or voided: a void pays with instrument 0, so its payouts name it.
    settled_event: u64,
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or("").to_string()
}

fn keep(list: &mut VecDeque<Value>, v: Value) {
    list.push_front(v);
    list.truncate(KEEP);
}

impl View {
    pub fn apply(&mut self, unit: &Unit) {
        let input = &unit.input;
        self.sequence = u(&input["record"]["sequence"]);
        self.now = u(&input["record"]["sequencedAt"]);
        let now = self.now;
        // A tick changes nothing a reader shows, so it does not wake anyone waiting for a change.
        if input["msg"] == "TimeTick" && unit.results.is_empty() {
            return;
        }
        self.version += 1;
        let applied = unit.results.iter().any(|r| r["msg"] == "CommandApplied" && r["outcome"] == "APPLIED");
        match input["msg"].as_str().unwrap_or("") {
            "DefineTickTable" if applied => {
                let rows: Vec<Row> = input["ticks"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|r| Row {
                        numerator: u(&r["numerator"]),
                        denominator: u(&r["denominator"]),
                        complement: u(&r["complement"]),
                    })
                    .collect();
                let bp = rows.iter().map(|r| 10_000 * r.numerator / r.denominator.max(1)).collect();
                let table = u(&input["tickTableId"]);
                self.tables.insert(table, bp);
                self.ladders.insert(table, rows);
            }
            "DefineEvent" if applied => {
                let event = EventView {
                    state_count: u(&input["stateCount"]),
                    defined_at: now,
                    scheduled_start: u(&input["scheduledStart"]),
                    ..EventView::default()
                };
                self.events.insert(u(&input["eventId"]), event);
            }
            "ListInstrument" if applied => {
                let (id, event) = (u(&input["instrumentId"]), u(&input["eventId"]));
                let inst = InstrumentView {
                    event,
                    table: u(&input["tickTableId"]),
                    lot: u(&input["lotStep"]),
                    mask: u(&input["stateMask"]),
                    class: s(&input["instrumentClass"]),
                    state: "LISTED".into(),
                    ..InstrumentView::default()
                };
                self.instruments.insert(id, inst);
                if let Some(e) = self.events.get_mut(&event) {
                    e.instruments.push(id);
                    e.instruments.sort_unstable();
                }
            }
            "SetEventPhase" if applied => {
                if let Some(e) = self.events.get_mut(&u(&input["eventId"])) {
                    match input["phase"].as_str() {
                        Some("STARTED") => {
                            e.started = true;
                            e.started_at = Some(now);
                        }
                        Some("SUSPENDED") => {
                            e.suspended = true;
                            e.suspended_ever = true;
                        }
                        Some("RESUMED") => e.suspended = false,
                        Some("ENDED") => {
                            e.ended = true;
                            e.ended_at.get_or_insert(now);
                        }
                        _ => {}
                    }
                }
            }
            "ResolveEvent" if applied => {
                if let Some(e) = self.events.get_mut(&u(&input["eventId"])) {
                    e.resolution = Some(Resolution {
                        kind: s(&input["kind"]),
                        winning_state: if input["kind"] == "VOID" { 0 } else { u(&input["winningState"]) },
                        case_id: u(&input["caseId"]),
                        at: now,
                    });
                }
            }
            _ => {}
        }
        for r in &unit.results {
            self.result(r);
        }
    }

    fn set_remaining(&mut self, order: u64, remaining: u64) {
        if remaining == 0 {
            self.orders.remove(&order);
        } else if let Some(o) = self.orders.get_mut(&order) {
            o.remaining = remaining;
        }
    }

    fn result(&mut self, r: &Value) {
        match r["msg"].as_str().unwrap_or("") {
            "MarketStateChanged" => {
                if let Some(i) = self.instruments.get_mut(&u(&r["instrumentId"])) {
                    i.state = s(&r["state"]);
                    if matches!(i.state.as_str(), "SETTLED" | "VOIDED") {
                        let event = i.event;
                        self.settled_event = event;
                        if let Some(e) = self.events.get_mut(&event) {
                            e.resolved = true;
                        }
                    }
                }
            }
            "BookLevelChanged" => {
                if let Some(i) = self.instruments.get_mut(&u(&r["instrumentId"])) {
                    let side = if r["side"] == "BUY" { &mut i.bids } else { &mut i.asks };
                    let (tick, quantity) = (u(&r["tick"]), u(&r["quantity"]));
                    if quantity == 0 {
                        side.remove(&tick);
                    } else {
                        side.insert(tick, (quantity, u(&r["orderCount"])));
                    }
                }
            }
            "OrderAccepted" => {
                let order = OrderView {
                    account: u(&r["accountId"]),
                    instrument: u(&r["instrumentId"]),
                    side: s(&r["side"]),
                    tick: u(&r["tick"]),
                    remaining: u(&r["quantity"]),
                };
                self.orders.insert(u(&r["orderId"]), order);
            }
            "Fill" => {
                self.set_remaining(u(&r["makerOrderId"]), u(&r["makerRemaining"]));
                self.set_remaining(u(&r["takerOrderId"]), u(&r["takerRemaining"]));
                let instrument = u(&r["instrumentId"]);
                if let Some(i) = self.instruments.get_mut(&instrument) {
                    i.last = Some(u(&r["tick"]));
                }
                let trade = json!({
                    "sequence": self.sequence, "atMs": u(&r["result"]["sequencedAt"]) / 1_000_000,
                    "instrument": instrument, "tick": r["tick"], "quantity": r["quantity"], "takerSide": r["takerSide"],
                    "maker": r["makerAccountId"], "taker": r["takerAccountId"]
                });
                keep(&mut self.trades, trade);
            }
            "OrderCancelled" => {
                self.orders.remove(&u(&r["orderId"]));
            }
            "OrderReduced" => self.set_remaining(u(&r["orderId"]), u(&r["remainingQuantity"])),
            "OrderAmended" => {
                if let Some(o) = self.orders.get_mut(&u(&r["orderId"])) {
                    o.tick = u(&r["tick"]);
                    o.remaining = u(&r["quantity"]);
                }
            }
            "PositionChanged" => {
                let key = (u(&r["accountId"]), u(&r["instrumentId"]));
                match r["position"].as_i64().unwrap_or(0) {
                    0 => self.positions.remove(&key),
                    p => self.positions.insert(key, p),
                };
            }
            "SleeveChanged" => {
                let sleeve = (r["availableCents"].as_i64().unwrap_or(0), r["reservedCents"].as_i64().unwrap_or(0));
                self.sleeves.insert(u(&r["accountId"]), sleeve);
            }
            "SettlementPayout" => {
                let instrument = u(&r["instrumentId"]);
                let event = match instrument {
                    0 => self.settled_event,
                    i => self.instruments.get(&i).map_or(0, |i| i.event),
                };
                let payout = json!({
                    "sequence": self.sequence, "account": r["accountId"], "event": event, "instrument": instrument,
                    "kind": r["kind"], "positionClosed": r["positionClosed"], "cents": r["cents"]
                });
                keep(&mut self.payouts, payout);
            }
            _ => {}
        }
    }

    /// Everything the web page draws, labelled from the catalogue.
    pub fn snapshot(&self, catalogue: &Catalogue) -> Value {
        let events: Vec<Value> = self
            .events
            .iter()
            .map(|(&id, e)| {
                let instruments: Vec<Value> = e
                    .instruments
                    .iter()
                    .filter_map(|iid| {
                        let i = self.instruments.get(iid)?;
                        Some(json!({
                            "id": iid, "label": catalogue.label(id, *iid), "class": i.class, "mask": i.mask,
                            "state": i.state, "table": i.table, "lot": i.lot, "last": i.last,
                            "bids": i.bids.iter().rev().map(|(t, (q, c))| json!([t, q, c])).collect::<Vec<_>>(),
                            "asks": i.asks.iter().map(|(t, (q, c))| json!([t, q, c])).collect::<Vec<_>>()
                        }))
                    })
                    .collect();
                let (template, round) = crate::catalogue::split_event(id);
                json!({
                    "id": id, "title": catalogue.title(id), "template": template, "round": round,
                    "states": catalogue.states(id, e.state_count), "started": e.started, "suspended": e.suspended,
                    "ended": e.ended, "resolved": e.resolved, "instruments": instruments
                })
            })
            .collect();
        let accounts: Vec<Value> = catalogue
            .accounts()
            .iter()
            .map(|a| {
                let id = u(&a["id"]);
                let (available, reserved) = self.sleeves.get(&id).copied().unwrap_or((0, 0));
                let positions: Vec<Value> = self
                    .positions
                    .iter()
                    .filter(|((account, _), _)| *account == id)
                    .map(|((_, instrument), p)| json!({"instrument": instrument, "position": p}))
                    .collect();
                let orders: Vec<Value> = self
                    .orders
                    .iter()
                    .filter(|(_, o)| o.account == id)
                    .map(|(oid, o)| {
                        json!({"orderId": oid, "instrument": o.instrument, "side": o.side, "tick": o.tick, "remaining": o.remaining})
                    })
                    .collect();
                json!({
                    "id": id, "name": a["name"], "tab": a["tab"], "available": available, "reserved": reserved,
                    "positions": positions, "orders": orders
                })
            })
            .collect();
        json!({
            "sequence": self.sequence, "tables": self.tables, "events": events, "accounts": accounts,
            "trades": self.trades, "payouts": self.payouts
        })
    }
}

/// A view kept current by a background thread following the feed.
pub struct Live {
    pub view: Mutex<View>,
    pub changed: Condvar,
}

impl Live {
    pub fn follow(feed: &str) -> Arc<Self> {
        let live = Arc::new(Self { view: Mutex::new(View::default()), changed: Condvar::new() });
        let (me, addr) = (Arc::clone(&live), feed.to_string());
        thread::spawn(move || {
            wire::follow(
                &addr,
                || {
                    if let Ok(mut v) = me.view.lock() {
                        *v = View::default();
                    }
                },
                |unit| {
                    if let Ok(mut v) = me.view.lock() {
                        v.apply(&unit);
                    }
                    me.changed.notify_all();
                },
            );
        });
        live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(msg: Value, at: u64, applied: bool) -> Unit {
        let mut input = msg;
        input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
        let results = if applied { vec![json!({"msg": "CommandApplied", "outcome": "APPLIED"})] } else { Vec::new() };
        Unit { input, results }
    }

    #[test]
    fn an_event_remembers_when_things_happened_to_it_and_how_it_was_resolved() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 3, "scheduledStart": 90}), 10, true));
        let phase = |p: &str| json!({"msg": "SetEventPhase", "eventId": 5, "phase": p, "source": "DUMMY_ORACLE"});
        view.apply(&unit(phase("STARTED"), 100, true));
        view.apply(&unit(phase("SUSPENDED"), 110, true));
        view.apply(&unit(phase("RESUMED"), 120, true));
        view.apply(&unit(phase("ENDED"), 200, true));
        let resolve =
            json!({"msg": "ResolveEvent", "eventId": 5, "kind": "RESOLVED", "caseId": 501, "winningState": 2});
        view.apply(&unit(resolve, 300, true));
        let e = &view.events[&5];
        assert_eq!((e.defined_at, e.scheduled_start, e.started_at, e.ended_at), (10, 90, Some(100), Some(200)));
        assert!(e.suspended_ever && !e.suspended);
        let expected = Resolution { kind: "RESOLVED".into(), winning_state: 2, case_id: 501, at: 300 };
        assert_eq!(e.resolution, Some(expected));
    }

    #[test]
    fn a_rejected_resolution_resolves_nothing() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0}), 1, true));
        view.apply(&unit(json!({"msg": "ResolveEvent", "eventId": 5, "kind": "VOID", "caseId": 0}), 2, false));
        assert_eq!(view.events[&5].resolution, None);
    }

    #[test]
    fn a_quiet_tick_moves_the_clock_but_changes_nothing_a_reader_shows() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0}), 1, true));
        let version = view.version;
        view.apply(&unit(json!({"msg": "TimeTick"}), 7, false));
        assert_eq!((view.now, view.sequence, view.version), (7, 7, version));
    }
}
