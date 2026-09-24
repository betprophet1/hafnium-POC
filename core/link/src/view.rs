//! A reader's picture of the venue, built only from the node's feed: books, orders, positions, sleeves, trades and
//! payouts. The web page shows it and the bots decide from it.
//!
//! Like the engine, it forgets a settled event and its markets when the engine releases them (`EventReleased`, spec
//! E-8.4), so it holds what the venue carries rather than everything it ever listed. It keeps two small things past a
//! release, each bounded: the latest round of each template (`released_rounds`), so a calendar never lists a round
//! again, and how a released event resolved while an unsettled parlay still names it as a leg (`released_legs`).

use crate::catalogue::Catalogue;
use crate::named;
use crate::wire::{self, Unit};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};

const KEEP: usize = 60;

#[derive(Clone, Default, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    /// `RESOLVED` or `VOID`.
    pub kind: String,
    /// The state that occurred; 0 for a void.
    pub winning_state: u64,
    pub case_id: u64,
    pub at: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
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
    /// A parlay's legs, in the canonical order `ListInstrument` gave them; empty for every other class.
    pub legs: Vec<Leg>,
}

/// One leg of a parlay: the market, and whether the parlay needs its YES (rather than its NO) to pay.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leg {
    pub event: u64,
    pub instrument: u64,
    pub yes: bool,
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Row {
    pub numerator: u64,
    pub denominator: u64,
    pub complement: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct OrderView {
    pub account: u64,
    pub instrument: u64,
    pub side: String,
    pub tick: u64,
    pub remaining: u64,
    /// The client order ID it was placed with. An amendment keeps it.
    pub client_order_id: String,
    /// Contracts filled so far, and their YES cost in cents (E-2.4), so a client restoring its state after a
    /// disconnect can be told an order's filled quantity and average price, not only what still rests.
    pub filled: u64,
    pub filled_yes_cents: u64,
    /// When a GTD order lapses (E-8.1), from the `NewOrder` that placed it: `OrderAccepted` does not say.
    pub expires_at: Option<u64>,
}

/// `positions` is keyed by `(account, instrument)`, and JSON has no such key — a map key must be a string. The
/// pair travels as a flat triple instead, which is also what somebody reading the file would expect to see.
mod positions_as_triples {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(map: &BTreeMap<(u64, u64), i64>, s: S) -> Result<S::Ok, S::Error> {
        map.iter()
            .map(|(&(account, instrument), &position)| (account, instrument, position))
            .collect::<Vec<_>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<(u64, u64), i64>, D::Error> {
        let triples = Vec::<(u64, u64, i64)>::deserialize(d)?;
        Ok(triples.into_iter().map(|(account, instrument, position)| ((account, instrument), position)).collect())
    }
}

#[derive(Default, Serialize, Deserialize)]
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
    #[serde(with = "positions_as_triples")]
    pub positions: BTreeMap<(u64, u64), i64>,
    /// Account to (available, reserved) cents.
    pub sleeves: BTreeMap<u64, (i64, i64)>,
    pub trades: VecDeque<Value>,
    pub payouts: VecDeque<Value>,
    /// The event whose markets were last settled or voided: a void pays with instrument 0, so its payouts name it.
    settled_event: u64,
    /// For each template, the highest round the engine has released and when that round was defined: a template's
    /// rounds can all be released between two listings, and a calendar counts on from the latest one. Parlays' events
    /// are no template's rounds and are not counted.
    pub released_rounds: BTreeMap<u64, (u64, u64)>,
    /// Released events that a parlay the view still carries names as a leg: how each resolved, and the masks of its
    /// markets, so the parlay can still be settled from them (`resolution_of`, `mask_of`). Dropped once no carried
    /// parlay names the event.
    pub released_legs: BTreeMap<u64, ReleasedLeg>,
}

/// What a parlay needs of a leg's event once the engine has released it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedLeg {
    pub resolution: Option<Resolution>,
    /// Market to its state mask.
    pub masks: BTreeMap<u64, u64>,
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
                    legs: input["legs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|l| Leg {
                            event: u(&l["legEventId"]),
                            instrument: u(&l["legInstrumentId"]),
                            yes: l["legOutcome"] == "YES",
                        })
                        .collect(),
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
            // E-15.7: processed as `ResolveEvent` `RESOLVED` on the parlay's event, to the lowest state outside its
            // mask, so a follower of resolutions sees it as one.
            "ExpireParlay" if applied => {
                if let Some(parlay) = self.instruments.get(&u(&input["instrumentId"])) {
                    let losing = (0..64).find(|s| parlay.mask >> s & 1 == 0).unwrap_or(0);
                    if let Some(e) = self.events.get_mut(&parlay.event) {
                        e.resolution =
                            Some(Resolution { kind: "RESOLVED".into(), winning_state: losing, case_id: 0, at: now });
                    }
                }
            }
            _ => {}
        }
        for r in &unit.results {
            self.result(r);
        }
        if input["msg"] == "NewOrder" {
            if let Some(expires) = input["expiresAt"].as_u64() {
                for r in unit.results.iter().filter(|r| r["msg"] == "OrderAccepted") {
                    if let Some(o) = self.orders.get_mut(&u(&r["orderId"])) {
                        o.expires_at = Some(expires);
                    }
                }
            }
        }
    }

    /// How an event resolved, whether the view still carries it or it was released under a carried parlay's leg.
    pub fn resolution_of(&self, event: u64) -> Option<&Resolution> {
        match self.events.get(&event) {
            Some(e) => e.resolution.as_ref(),
            None => self.released_legs.get(&event).and_then(|r| r.resolution.as_ref()),
        }
    }

    /// A market's state mask, whether the view still carries it or it was released under a carried parlay's leg.
    pub fn mask_of(&self, event: u64, instrument: u64) -> Option<u64> {
        match self.instruments.get(&instrument) {
            Some(i) => Some(i.mask),
            None => self.released_legs.get(&event).and_then(|r| r.masks.get(&instrument).copied()),
        }
    }

    /// E-8.4: the event and its markets no longer exist. Its positions are already 0 and its orders gone (E-15.5);
    /// anything left of them here goes too.
    fn release(&mut self, event: u64) {
        let Some(e) = self.events.remove(&event) else { return };
        let mut markets: Vec<u64> = e.instruments.clone();
        markets.extend(self.instruments.iter().filter(|(_, i)| i.event == event).map(|(&id, _)| id));
        markets.sort_unstable();
        markets.dedup();
        let released: Vec<(u64, InstrumentView)> =
            markets.iter().filter_map(|id| self.instruments.remove(id).map(|i| (*id, i))).collect();
        self.positions.retain(|(_, instrument), _| !markets.contains(instrument));
        self.orders.retain(|_, o| !markets.contains(&o.instrument));
        let parlay = released.iter().any(|(_, i)| i.class == "PARLAY");
        if !parlay {
            let (template, round) = crate::catalogue::split_event(event);
            let latest = self.released_rounds.entry(template).or_insert((round, e.defined_at));
            if round >= latest.0 {
                *latest = (round, e.defined_at);
            }
        }
        let named = |view: &Self, event: u64| {
            view.instruments.values().any(|i| i.class == "PARLAY" && i.legs.iter().any(|l| l.event == event))
        };
        if named(self, event) {
            let masks = released.iter().map(|(id, i)| (*id, i.mask)).collect();
            self.released_legs.insert(event, ReleasedLeg { resolution: e.resolution, masks });
        }
        if parlay {
            let kept: Vec<u64> = self.released_legs.keys().copied().filter(|&leg| named(self, leg)).collect();
            self.released_legs.retain(|leg, _| kept.contains(leg));
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
                    client_order_id: s(&r["clientOrderId"]),
                    filled: 0,
                    filled_yes_cents: 0,
                    expires_at: None,
                };
                self.orders.insert(u(&r["orderId"]), order);
            }
            "Fill" => {
                let (quantity, tick) = (u(&r["quantity"]), u(&r["tick"]));
                let instrument = u(&r["instrumentId"]);
                // E-2.4: whole cents for a quantity on the lot step, which every fill is.
                let yes_cents = self
                    .instruments
                    .get(&instrument)
                    .and_then(|i| self.ladders.get(&i.table))
                    .and_then(|ladder| ladder.get(usize::try_from(tick).ok()?))
                    .map_or(0, |row| {
                        u128::from(quantity) * 100 * u128::from(row.numerator) / u128::from(row.denominator.max(1))
                    });
                for order in [u(&r["makerOrderId"]), u(&r["takerOrderId"])] {
                    if let Some(o) = self.orders.get_mut(&order) {
                        o.filled = o.filled.saturating_add(quantity);
                        o.filled_yes_cents =
                            o.filled_yes_cents.saturating_add(u64::try_from(yes_cents).unwrap_or(u64::MAX));
                    }
                }
                self.set_remaining(u(&r["makerOrderId"]), u(&r["makerRemaining"]));
                self.set_remaining(u(&r["takerOrderId"]), u(&r["takerRemaining"]));
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
            "EventReleased" => self.release(u(&r["eventId"])),
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

    /// The view a reader would hold after being sent everything in `journal`/`results` up to and including
    /// sequence `at` — or all of it, when `at` is `None`.
    ///
    /// This is a *view* snapshot, and it is deliberately built the way a live reader builds one: by applying the
    /// same units, through the same `apply`. The engine's own snapshot (spec §20) cannot stand in for it. That
    /// one is the engine's logical state, and §20 has it omit whatever the engine does not need — so it carries
    /// no parlay legs (only the mask the engine matches on), no resolution kind or winning state (only
    /// `resolved`), none of an event's timestamps, and no per-order filled total (only what still rests). A
    /// reader rebuilt from it would look right and be quietly wrong for parlays, for a calendar counting an
    /// event's stages, and for any client asking what an order has filled. Replaying the units loses nothing,
    /// because the units are what a reader's state is a function of in the first place.
    pub fn replayed(
        journal: &std::path::Path,
        results: &std::path::Path,
        at: Option<u64>,
    ) -> Result<Self, wire::Error> {
        let mut view = Self::default();
        for unit in wire::replayed_units(journal, results)? {
            if at.is_some_and(|stop| u(&unit.input["record"]["sequence"]) > stop) {
                break;
            }
            view.apply(&unit);
        }
        Ok(view)
    }

    /// Writes the view to `path`, durably and atomically: a sibling temp file, `fsync`ed, then renamed over the
    /// original — the same discipline the journal's own rewrites use, so a crash mid-write leaves whatever was
    /// there before rather than a half-written view.
    pub fn save(&self, path: &std::path::Path) -> Result<(), wire::Error> {
        let temp = path.with_extension("view-tmp");
        {
            let file = std::fs::File::create(&temp)?;
            serde_json::to_writer(std::io::BufWriter::new(&file), self)?;
            file.sync_all()?;
        }
        std::fs::rename(&temp, path)?;
        Ok(())
    }

    /// Reads back what `save` wrote.
    pub fn load(path: &std::path::Path) -> Result<Self, wire::Error> {
        let file = std::fs::File::open(path)?;
        Ok(serde_json::from_reader(std::io::BufReader::new(file))?)
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
                            "asks": i.asks.iter().map(|(t, (q, c))| json!([t, q, c])).collect::<Vec<_>>(),
                            "legs": i.legs.iter().map(|l| json!({"event": l.event, "instrument": l.instrument, "yes": l.yes})).collect::<Vec<_>>()
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
        named("link-view", move || {
            wire::follow(
                &addr,
                || {
                    // Carries on from the sequence this view already holds, so a reconnect that finds the same
                    // journal still there costs only what happened while disconnected, not the whole history.
                    me.view.lock().map(|v| v.sequence).unwrap_or(0)
                },
                || {
                    // The node could not honour that position (a different journal under the same address, or
                    // one that no longer goes back that far) — this view is of a history that never happened.
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
    fn a_parlay_keeps_its_legs_and_a_gtd_order_its_expiry() {
        let mut view = View::default();
        let legs = json!([{"legEventId": 1, "legInstrumentId": 100, "legOutcome": "YES"},
                          {"legEventId": 2, "legInstrumentId": 201, "legOutcome": "NO"}]);
        let list = json!({"msg": "ListInstrument", "instrumentId": 9, "eventId": 9, "instrumentClass": "PARLAY",
                          "stateMask": 2, "tickTableId": 2, "lotStep": 10, "legs": legs});
        view.apply(&unit(list, 1, true));
        let expected =
            vec![Leg { event: 1, instrument: 100, yes: true }, Leg { event: 2, instrument: 201, yes: false }];
        assert_eq!(view.instruments[&9].legs, expected);
        let mut order = json!({"msg": "NewOrder", "accountId": 10, "instrumentId": 9, "expiresAt": 5_000});
        order["record"] = json!({"sequence": 2, "sequencedAt": 2, "sequencerGroup": 1});
        let accepted = json!({"msg": "OrderAccepted", "orderId": 77, "accountId": 10, "instrumentId": 9,
                              "side": "SELL", "tick": 4, "quantity": 10});
        view.apply(&Unit { input: order, results: vec![accepted] });
        assert_eq!(view.orders[&77].expires_at, Some(5_000));
    }

    #[test]
    fn an_expired_parlay_is_resolved_to_its_losing_state() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 9, "stateCount": 2, "scheduledStart": 0}), 1, true));
        let list = json!({"msg": "ListInstrument", "instrumentId": 9, "eventId": 9, "instrumentClass": "PARLAY",
                          "stateMask": 2, "tickTableId": 2, "lotStep": 10, "legs": []});
        view.apply(&unit(list, 2, true));
        view.apply(&unit(json!({"msg": "ExpireParlay", "instrumentId": 9, "causingEventId": 1}), 3, true));
        let expected = Resolution { kind: "RESOLVED".into(), winning_state: 0, case_id: 0, at: 3 };
        assert_eq!(view.events[&9].resolution, Some(expected));
    }

    #[test]
    fn a_rejected_resolution_resolves_nothing() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0}), 1, true));
        view.apply(&unit(json!({"msg": "ResolveEvent", "eventId": 5, "kind": "VOID", "caseId": 0}), 2, false));
        assert_eq!(view.events[&5].resolution, None);
    }

    #[test]
    fn a_released_event_is_forgotten_but_its_round_and_a_parlays_leg_are_not() {
        let mut view = View::default();
        let round = crate::catalogue::event_id(3, 7);
        let leg = crate::catalogue::instrument_id(round, 0);
        view.apply(&unit(
            json!({"msg": "DefineEvent", "eventId": round, "stateCount": 2, "scheduledStart": 0}),
            1,
            true,
        ));
        let list = |id: u64, event: u64, class: &str, legs: Value| {
            json!({"msg": "ListInstrument", "instrumentId": id, "eventId": event, "instrumentClass": class,
                   "stateMask": 2, "tickTableId": 1, "lotStep": 1, "legs": legs})
        };
        view.apply(&unit(list(leg, round, "BINARY", json!([])), 2, true));
        let parlay = 1u64 << 52;
        view.apply(&unit(
            json!({"msg": "DefineEvent", "eventId": parlay, "stateCount": 2, "scheduledStart": 0}),
            3,
            true,
        ));
        let legs = json!([{"legEventId": round, "legInstrumentId": leg, "legOutcome": "YES"}]);
        view.apply(&unit(list(parlay, parlay, "PARLAY", legs), 4, true));
        let resolve =
            json!({"msg": "ResolveEvent", "eventId": round, "kind": "RESOLVED", "caseId": 1, "winningState": 1});
        view.apply(&unit(resolve, 5, true));
        let tick = |at: u64, event: u64| {
            let mut input = json!({"msg": "TimeTick"});
            input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
            Unit { input, results: vec![json!({"msg": "EventReleased", "eventId": event})] }
        };
        view.apply(&tick(6, round));
        assert!(!view.events.contains_key(&round) && !view.instruments.contains_key(&leg));
        assert_eq!(view.released_rounds.get(&3), Some(&(7, 1)));
        assert_eq!(view.resolution_of(round).map(|r| r.winning_state), Some(1));
        assert_eq!(view.mask_of(round, leg), Some(2));
        // Once the parlay goes too, nothing is kept for its leg, and a parlay's event is no template's round.
        view.apply(&tick(7, parlay));
        assert!(view.released_legs.is_empty() && view.instruments.is_empty());
        assert_eq!(view.released_rounds.len(), 1);
    }

    #[test]
    fn a_quiet_tick_moves_the_clock_but_changes_nothing_a_reader_shows() {
        let mut view = View::default();
        view.apply(&unit(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0}), 1, true));
        let version = view.version;
        view.apply(&unit(json!({"msg": "TimeTick"}), 7, false));
        assert_eq!((view.now, view.sequence, view.version), (7, 7, version));
    }

    /// Every field, populated, through `save` and `load`.
    ///
    /// The recorded fixture the integration tests replay is thin — it produces no positions, trades, payouts,
    /// parlay legs, resolutions or part-filled orders at all — so a round trip over *it* leaves most of this
    /// struct untouched and would not notice a field that silently failed to survive. `positions` is the one
    /// that cannot be derived: it is keyed by a pair, which JSON cannot use as a map key, so it is the one part
    /// of this serialisation written by hand rather than derived.
    #[test]
    fn every_field_survives_being_saved_and_loaded() {
        let resolution = Resolution { kind: "RESOLVED".into(), winning_state: 2, case_id: 7, at: 900 };
        let mut view = View {
            sequence: 500,
            now: 1_234,
            version: 9,
            tables: BTreeMap::from([(1, vec![100, 200])]),
            ladders: BTreeMap::from([(1, vec![Row { numerator: 1, denominator: 4, complement: 3 }])]),
            events: BTreeMap::from([(
                5,
                EventView {
                    state_count: 3,
                    started: true,
                    suspended: false,
                    ended: true,
                    resolved: true,
                    instruments: vec![50, 51],
                    defined_at: 10,
                    scheduled_start: 20,
                    started_at: Some(30),
                    ended_at: None,
                    suspended_ever: true,
                    resolution: Some(resolution.clone()),
                },
            )]),
            instruments: BTreeMap::from([(
                50,
                InstrumentView {
                    event: 5,
                    table: 1,
                    lot: 10,
                    mask: 6,
                    class: "PARLAY".into(),
                    state: "OPEN".into(),
                    bids: BTreeMap::from([(3, (40, 2))]),
                    asks: BTreeMap::from([(9, (11, 1))]),
                    last: Some(4),
                    legs: vec![Leg { event: 1, instrument: 100, yes: true }],
                },
            )]),
            orders: BTreeMap::from([(
                77,
                OrderView {
                    account: 10,
                    instrument: 50,
                    side: "BUY".into(),
                    tick: 3,
                    remaining: 5,
                    client_order_id: "abc".into(),
                    filled: 6,
                    filled_yes_cents: 150,
                    expires_at: Some(5_000),
                },
            )]),
            positions: BTreeMap::from([((10, 50), -3), ((11, 51), 8)]),
            sleeves: BTreeMap::from([(10, (900, 100))]),
            trades: VecDeque::from([json!({"sequence": 4, "quantity": 2})]),
            payouts: VecDeque::from([json!({"sequence": 5, "cents": 300})]),
            settled_event: 5,
            released_rounds: BTreeMap::from([(3, (7, 11))]),
            released_legs: BTreeMap::from([(
                1,
                ReleasedLeg { resolution: Some(resolution), masks: BTreeMap::from([(100, 2)]) },
            )]),
        };
        // A reader that has seen nothing of a field must round trip too, not only one that has seen something.
        view.events.insert(6, EventView::default());

        let dir = std::env::temp_dir().join(format!("venue-view-fields-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let path = dir.join("view.json");
        view.save(&path).expect("saves");
        let restored = View::load(&path).expect("loads");

        let (before, after) =
            (serde_json::to_value(&view).expect("serialises"), serde_json::to_value(&restored).expect("serialises"));
        assert_eq!(before, after, "a view did not survive being written and read back");
        // Named explicitly, because an equal serialisation of two views that both dropped the same field would
        // still compare equal.
        assert_eq!(restored.positions, view.positions, "positions did not survive");
        assert_eq!(restored.settled_event, 5, "settled_event did not survive");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
