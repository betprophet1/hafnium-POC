//! A reader's picture of the venue, built only from the node's feed: books, orders, positions, sleeves, trades and
//! payouts. The web page shows it and the bots decide from it.
//!
//! Like the engine, it forgets a settled event and its markets when the engine releases them (`EventReleased`, spec
//! E-8.4), so it holds what the venue carries rather than everything it ever listed. It keeps two small things past a
//! release, each bounded: the latest round of each template (`released_rounds`), so a calendar never lists a round
//! again, and how a released event resolved while an unsettled parlay still names it as a leg (`released_legs`).

use crate::catalogue::Catalogue;
use crate::named;
use crate::wire::{self, Frame, Unit};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use venue_journal::message_header_codec::MessageHeaderDecoder;
use venue_journal::side::Side;
use venue_journal::{
    book_level_changed_codec, command_applied_codec, fill_codec, order_accepted_codec, order_amended_codec,
    order_cancelled_codec, order_reduced_codec, position_changed_codec, sleeve_changed_codec, ReadBuf,
};

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

#[derive(Clone, Default, Serialize, Deserialize)]
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

/// What a `Fill` says, however it was read. Both paths build one so that `View::filled` is the only place that
/// decides what a fill does.
struct Filled {
    instrument: u64,
    tick: u64,
    quantity: u64,
    maker: u64,
    taker: u64,
    maker_remaining: u64,
    taker_remaining: u64,
    maker_account: u64,
    taker_account: u64,
    taker_side: String,
    at: u64,
}

/// A frame's SBE header, for handing to a generated decoder.
fn header_of(f: &Frame) -> MessageHeaderDecoder<ReadBuf<'_>> {
    MessageHeaderDecoder::default().wrap(ReadBuf::new(f.bytes()), 0)
}

/// A fixed byte array as the string in it: the schema pads with zeroes, as `venue_harness::codec` also trims.
pub fn trimmed(b: &[u8]) -> String {
    let end = b.iter().rposition(|x| *x != 0).map_or(0, |i| i + 1);
    String::from_utf8_lossy(&b[..end]).into_owned()
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
        // The template first: this runs over every result, and asking each one through the JSON shape decodes
        // frames nothing else here reads (ADR-0002).
        let applied = unit
            .results
            .iter()
            .any(|r| r.template() == command_applied_codec::SBE_TEMPLATE_ID && r["outcome"] == "APPLIED");
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
            if !self.result_typed(r) {
                self.result(r);
            }
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

    /// The results a trading feed is mostly made of, read straight out of the generated SBE decoders instead of
    /// through `serde_json::Value`: about 857ns a result saved, on the one thread per half that applies every
    /// unit (ADR-0002, prototypes/feedapply). False for anything not handled here, which `result` then takes
    /// through the JSON shape as before -- the rest of the schema is listing and settlement traffic, rare
    /// enough that what it costs does not show.
    ///
    /// `view_results_agree` checks the two against each other for every message this handles, because a field
    /// read from the wrong place here would not fail: it would quietly give the view a different answer from
    /// the engine's.
    fn result_typed(&mut self, f: &Frame) -> bool {
        match f.template() {
            book_level_changed_codec::SBE_TEMPLATE_ID => {
                let mut dec = book_level_changed_codec::BookLevelChangedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                let Some(i) = self.instruments.get_mut(&dec.instrument_id()) else { return true };
                let side = if dec.side() == Side::BUY { &mut i.bids } else { &mut i.asks };
                let (tick, quantity) = (u64::from(dec.tick()), u64::from(dec.quantity()));
                if quantity == 0 {
                    side.remove(&tick);
                } else {
                    side.insert(tick, (quantity, u64::from(dec.order_count())));
                }
                true
            }
            order_accepted_codec::SBE_TEMPLATE_ID => {
                let mut dec = order_accepted_codec::OrderAcceptedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                let order = OrderView {
                    account: dec.account_id(),
                    instrument: dec.instrument_id(),
                    side: dec.side().to_string(),
                    tick: u64::from(dec.tick()),
                    remaining: u64::from(dec.quantity()),
                    client_order_id: trimmed(&dec.client_order_id()),
                    filled: 0,
                    filled_yes_cents: 0,
                    expires_at: None,
                };
                self.orders.insert(dec.order_id(), order);
                true
            }
            // The order lifecycle, which `executions` reads too -- both had to be typed for either to save
            // anything, since one decode serves the whole unit (ADR-0002).
            order_cancelled_codec::SBE_TEMPLATE_ID => {
                let mut dec = order_cancelled_codec::OrderCancelledDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                self.orders.remove(&dec.order_id());
                true
            }
            order_reduced_codec::SBE_TEMPLATE_ID => {
                let mut dec = order_reduced_codec::OrderReducedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                self.set_remaining(dec.order_id(), u64::from(dec.remaining_quantity()));
                true
            }
            order_amended_codec::SBE_TEMPLATE_ID => {
                let mut dec = order_amended_codec::OrderAmendedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                if let Some(o) = self.orders.get_mut(&dec.order_id()) {
                    o.tick = u64::from(dec.tick());
                    o.remaining = u64::from(dec.quantity());
                }
                true
            }
            // The two most numerous results the hub does not want at all: `executions` skips both, so before
            // this the only thing decoding them was falling through to `result`. `sleeve_changed` alone was
            // 2.24M decodes a minute, for two integers (ADR-0002, docs/adr/measurements/feed-mix.txt).
            sleeve_changed_codec::SBE_TEMPLATE_ID => {
                let mut dec = sleeve_changed_codec::SleeveChangedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                self.sleeves.insert(dec.account_id(), (dec.available_cents(), dec.reserved_cents()));
                true
            }
            position_changed_codec::SBE_TEMPLATE_ID => {
                let mut dec = position_changed_codec::PositionChangedDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                dec = group.parent().expect("the result group");
                let key = (dec.account_id(), dec.instrument_id());
                match dec.position() {
                    0 => self.positions.remove(&key),
                    p => self.positions.insert(key, p),
                };
                true
            }
            fill_codec::SBE_TEMPLATE_ID => {
                let mut dec = fill_codec::FillDecoder::default().header(header_of(f), 0);
                let mut group = dec.result_decoder();
                let at = group.sequenced_at();
                dec = group.parent().expect("the result group");
                let filled = Filled {
                    instrument: dec.instrument_id(),
                    tick: u64::from(dec.tick()),
                    quantity: u64::from(dec.quantity()),
                    maker: dec.maker_order_id(),
                    taker: dec.taker_order_id(),
                    maker_remaining: u64::from(dec.maker_remaining()),
                    taker_remaining: u64::from(dec.taker_remaining()),
                    maker_account: dec.maker_account_id(),
                    taker_account: dec.taker_account_id(),
                    taker_side: dec.taker_side().to_string(),
                    at,
                };
                self.filled(&filled);
                true
            }
            _ => false,
        }
    }

    /// Everything a fill does, in one place: the typed path and the JSON one both call this, so neither can
    /// come to a different answer than the other about what a fill means.
    fn filled(&mut self, f: &Filled) {
        // E-2.4: whole cents for a quantity on the lot step, which every fill is.
        let yes_cents = self
            .instruments
            .get(&f.instrument)
            .and_then(|i| self.ladders.get(&i.table))
            .and_then(|ladder| ladder.get(usize::try_from(f.tick).ok()?))
            .map_or(0, |row| {
                u128::from(f.quantity) * 100 * u128::from(row.numerator) / u128::from(row.denominator.max(1))
            });
        for order in [f.maker, f.taker] {
            if let Some(o) = self.orders.get_mut(&order) {
                o.filled = o.filled.saturating_add(f.quantity);
                o.filled_yes_cents = o.filled_yes_cents.saturating_add(u64::try_from(yes_cents).unwrap_or(u64::MAX));
            }
        }
        self.set_remaining(f.maker, f.maker_remaining);
        self.set_remaining(f.taker, f.taker_remaining);
        if let Some(i) = self.instruments.get_mut(&f.instrument) {
            i.last = Some(f.tick);
        }
        let trade = json!({
            "sequence": self.sequence, "atMs": f.at / 1_000_000,
            "instrument": f.instrument, "tick": f.tick, "quantity": f.quantity, "takerSide": f.taker_side,
            "maker": f.maker_account, "taker": f.taker_account
        });
        keep(&mut self.trades, trade);
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
            "Fill" => self.filled(&Filled {
                instrument: u(&r["instrumentId"]),
                tick: u(&r["tick"]),
                quantity: u(&r["quantity"]),
                maker: u(&r["makerOrderId"]),
                taker: u(&r["takerOrderId"]),
                maker_remaining: u(&r["makerRemaining"]),
                taker_remaining: u(&r["takerRemaining"]),
                maker_account: u(&r["makerAccountId"]),
                taker_account: u(&r["takerAccountId"]),
                taker_side: s(&r["takerSide"]),
                at: u(&r["result"]["sequencedAt"]),
            }),
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
    /// Follows `feed` from the beginning: a reader with nothing, which asks for everything.
    pub fn follow(feed: &str) -> Arc<Self> {
        Self::spawn(feed, View::default())
    }

    /// Follows `feed` starting from the view snapshot at `snapshot`, rather than from the beginning.
    ///
    /// Nothing in the protocol changes for this: the view arrives already knowing the sequence it holds, and
    /// the resume handshake a reconnect already uses says so, so the node sends only what followed it. That
    /// matters in an environment founded on a snapshot rather than on genesis — there the node's journal holds
    /// no inputs at all, so a reader asking for everything would be sent nothing and come up empty.
    pub fn follow_from(feed: &str, snapshot: &std::path::Path) -> Result<Arc<Self>, wire::Error> {
        Ok(Self::spawn(feed, View::load(snapshot)?))
    }

    fn spawn(feed: &str, view: View) -> Arc<Self> {
        let live = Arc::new(Self { view: Mutex::new(view), changed: Condvar::new() });
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
    use crate::wire::Frame;

    fn unit(msg: Value, at: u64, applied: bool) -> Unit {
        let mut input = msg;
        input["record"] = json!({"sequence": at, "sequencedAt": at, "sequencerGroup": 1});
        let results =
            if applied { vec![Frame::of(&json!({"msg": "CommandApplied", "outcome": "APPLIED"}))] } else { Vec::new() };
        Unit { input: Frame::of(&input), results }
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
        view.apply(&Unit { input: Frame::of(&order), results: vec![Frame::of(&accepted)] });
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
            Unit {
                input: Frame::of(&input),
                results: vec![Frame::of(&json!({"msg": "EventReleased", "eventId": event}))],
            }
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

    /// The typed reader and the JSON one must make the same view out of the same bytes.
    ///
    /// This is the check that makes taking `serde_json::Value` off the feed path safe to do at all. A field
    /// read from the wrong place in `result_typed` does not fail: the view simply carries a different number
    /// from the engine's, and nothing says so until someone reconciles a position. So every result the typed
    /// path claims is applied both ways, to two views seeded alike, and the whole of both is compared.
    #[test]
    fn the_typed_reader_and_the_json_one_agree() {
        // A market with a cent ladder, so a fill's `filled_yes_cents` is a real conversion and not a zero.
        let seeded = || {
            let mut view = View { sequence: 4_242, ..View::default() };
            view.ladders
                .insert(1, (0..=100).map(|t| Row { numerator: t, denominator: 100, complement: 100 - t }).collect());
            view.instruments.insert(
                9_500_000,
                InstrumentView { event: 7, table: 1, lot: 1, state: "OPEN".into(), ..InstrumentView::default() },
            );
            view
        };

        let at = 1_727_000_000_123_456_789_u64;
        let result = |sequence: u64| {
            json!({"inputSequence": sequence, "resultIndex": 0, "sequencedAt": at,
                                            "sequencerGroup": 1})
        };
        let results = [
            json!({"msg": "OrderAccepted", "result": result(1), "orderId": 900_123, "accountId": 9_600_007,
                   "sessionId": 42, "clientOrderId": "abc-123", "instrumentId": 9_500_000, "side": "BUY",
                   "tick": 40, "quantity": 1_000, "reservedCents": 40_000}),
            json!({"msg": "OrderAccepted", "result": result(2), "orderId": 900_124, "accountId": 9_600_008,
                   "sessionId": 43, "clientOrderId": "", "instrumentId": 9_500_000, "side": "SELL",
                   "tick": 60, "quantity": 500, "reservedCents": 20_000}),
            json!({"msg": "BookLevelChanged", "result": result(3), "instrumentId": 9_500_000, "side": "BUY",
                   "tick": 40, "quantity": 1_000, "orderCount": 1}),
            json!({"msg": "BookLevelChanged", "result": result(4), "instrumentId": 9_500_000, "side": "SELL",
                   "tick": 60, "quantity": 500, "orderCount": 1}),
            // A level going away, which is the zero-quantity case.
            json!({"msg": "BookLevelChanged", "result": result(5), "instrumentId": 9_500_000, "side": "SELL",
                   "tick": 60, "quantity": 0, "orderCount": 0}),
            // A partial fill, so both sides keep an order with a remaining quantity and a cent conversion.
            json!({"msg": "Fill", "result": result(6), "tradeId": 55_001, "instrumentId": 9_500_000, "tick": 40,
                   "quantity": 7, "makerOrderId": 900_123, "makerAccountId": 9_600_007, "makerRemaining": 993,
                   "takerOrderId": 900_124, "takerAccountId": 9_600_008, "takerRemaining": 493,
                   "takerSide": "SELL"}),
            // The two the hub skips entirely, so `result_typed` is the only thing that reads them: a sleeve
            // both ways round, and a position both set and cleared, since zero is the branch that removes.
            json!({"msg": "SleeveChanged", "result": result(6), "accountId": 9_600_007,
                   "availableCents": 123_456, "reservedCents": 7_890}),
            json!({"msg": "PositionChanged", "result": result(6), "accountId": 9_600_007,
                   "instrumentId": 9_500_000, "position": 42}),
            json!({"msg": "PositionChanged", "result": result(6), "accountId": 9_600_008,
                   "instrumentId": 9_500_000, "position": -17}),
            json!({"msg": "PositionChanged", "result": result(6), "accountId": 9_600_008,
                   "instrumentId": 9_500_000, "position": 0}),
            // And one that empties the maker, which is the branch that removes an order rather than reducing it.
            json!({"msg": "Fill", "result": result(7), "tradeId": 55_002, "instrumentId": 9_500_000, "tick": 40,
                   "quantity": 993, "makerOrderId": 900_123, "makerAccountId": 9_600_007, "makerRemaining": 0,
                   "takerOrderId": 900_124, "takerAccountId": 9_600_008, "takerRemaining": 0,
                   "takerSide": "SELL"}),
        ];
        let frames: Vec<Frame> = results.iter().map(Frame::of).collect();

        // Compared after every result, not only at the end: a wrong field often shows for one result and is
        // then overwritten by a later one, so an end-state check quietly passes. (It did, until this did not.)
        let (mut typed, mut through_json) = (seeded(), seeded());
        for (i, f) in frames.iter().enumerate() {
            assert!(typed.result_typed(f), "the typed path claims every result this test uses: {:?}", &**f);
            through_json.result(f);
            let (a, b) =
                (serde_json::to_value(&typed).expect("a view"), serde_json::to_value(&through_json).expect("a view"));
            assert_eq!(a, b, "the two readers disagree after result {i}: {:?}", &**f);
        }
        // And that the corpus did something, so an agreement of two empty views cannot pass for one.
        assert_eq!(typed.trades.len(), 2, "both fills were recorded");
        assert!(typed.orders.is_empty(), "the second fill emptied both orders");
        assert_eq!(typed.instruments[&9_500_000].last, Some(40), "the last traded tick");
    }
}
