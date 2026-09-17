//! Metrics derived from the journal, as the feed delivers it: every input with its results (deploy/observability/
//! metrics.md, venue-observer). Nothing here asks the engine anything; it reads what the engine already published.
//!
//! Counters count what happened after this process started, so that a restart, or a reconnection that replays the
//! feed from the start of the journal, counts nothing twice: Prometheus sees a counter reset, not a spike. Gauges
//! describe the venue as the projection holds it, the replayed history included, and are recomputed at most once a
//! second.

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use venue_engine::state::Capacities;
use venue_link::catalogue::Catalogue;
use venue_link::view::View;
use venue_link::wire::Unit;
use venue_telemetry::hdr::{Hdr, Recorder};
use venue_telemetry::prometheus_client;
use venue_telemetry::{FloatGauge, Labels};

/// How often the state gauges are recomputed from the projection.
const GAUGES_EVERY: Duration = Duration::from_secs(1);
/// A unit sequenced longer ago than this when it arrives is history being replayed, not a hop.
const HOP_HORIZON_NS: u64 = 60_000_000_000;

type Counters = Family<Labels, Counter>;
type Gauges = Family<Labels, Gauge>;
type FloatGauges = Family<Labels, FloatGauge>;

fn l(pairs: &[(&'static str, &str)]) -> Labels {
    pairs.iter().map(|(k, v)| (*k, (*v).to_string())).collect()
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

/// The resources whose use is compared with the capacity profile and the sequenced limits.
const RESOURCES: &[&str] = &["events", "instruments", "levels", "orders", "accounts"];

pub struct Derived {
    started_ns: u64,
    /// The highest sequence counted, so a replay after reconnecting counts nothing again.
    counted_through: u64,
    max_market_series: usize,
    profile: Option<Capacities>,
    /// Sequenced limits (`SetListingParameters`, `SetGroupParameters`) by resource, when set above 0.
    listing_limits: BTreeMap<&'static str, u64>,
    /// Markets that have per-market series now, and the labels of each one's info series.
    market_series: BTreeSet<u64>,
    info_labels: BTreeMap<u64, Labels>,
    gauges_at: Option<Instant>,
    hop: Recorder,
    // Observer health.
    feed_sequence: Gauge,
    feed_behind: FloatGauge,
    pub feed_reconnects: Counter,
    // Flow.
    inputs: Counters,
    order_rejections: Counters,
    command_outcomes: Counters,
    orders_accepted: Counter,
    order_cancels: Counters,
    fills: Counter,
    traded_contracts: Counter,
    traded_notional: Counter,
    fees: Counters,
    payouts: Counters,
    payout_cents: Counters,
    void_debits: Counter,
    events_defined: Counter,
    events_resolved: Counters,
    events_released: Counter,
    phase_changes: Counters,
    markets_listed: Counters,
    market_state_changes: Counters,
    last_event_defined: FloatGauge,
    // State.
    markets: Gauges,
    events: Gauges,
    open_orders: Gauge,
    resting: Gauges,
    accounts_active: Gauge,
    capacity_used: Gauges,
    capacity_limit: Gauges,
    // Per market.
    market_info: Gauges,
    market_contracts: Counters,
    market_notional: Counters,
    market_fills: Counters,
    market_orders: Counters,
    market_best: FloatGauges,
    market_spread: FloatGauges,
    market_depth: Gauges,
    market_resting: Gauges,
    market_two_sided: Gauges,
}

macro_rules! register {
    ($registry:expr, $name:literal, $help:literal, $metric:expr) => {{
        let metric = $metric;
        $registry.register($name, $help, metric.clone());
        metric
    }};
}

impl Derived {
    pub fn register(
        registry: &mut Registry,
        started_ns: u64,
        profile: Option<Capacities>,
        max_market_series: usize,
        hops: &Hdr,
    ) -> Self {
        let r = registry;
        let derived = Self {
            started_ns,
            counted_through: 0,
            max_market_series,
            profile,
            listing_limits: BTreeMap::new(),
            market_series: BTreeSet::new(),
            info_labels: BTreeMap::new(),
            gauges_at: None,
            hop: hops.recorder(Duration::from_millis(250)),
            feed_sequence: register!(
                r,
                "venue_observer_feed_sequence",
                "Sequence of the last unit applied",
                Gauge::default()
            ),
            feed_behind: register!(
                r,
                "venue_observer_feed_behind_seconds",
                "Wall clock minus sequencedAt of the last unit, when applied",
                FloatGauge::default()
            ),
            feed_reconnects: register!(
                r,
                "venue_observer_feed_reconnects",
                "Connections to the feed",
                Counter::default()
            ),
            inputs: register!(r, "venue_inputs", "Inputs with results, by message name", Counters::default()),
            order_rejections: register!(r, "venue_order_rejections", "OrderRejected by reason", Counters::default()),
            command_outcomes: register!(
                r,
                "venue_command_outcomes",
                "CommandApplied by command, outcome and reason",
                Counters::default()
            ),
            orders_accepted: register!(r, "venue_orders_accepted", "OrderAccepted", Counter::default()),
            order_cancels: register!(r, "venue_order_cancels", "OrderCancelled by reason", Counters::default()),
            fills: register!(r, "venue_fills", "Fill results", Counter::default()),
            traded_contracts: register!(r, "venue_traded_contracts", "Contracts traded", Counter::default()),
            traded_notional: register!(
                r,
                "venue_traded_notional_cents",
                "YES cost of what traded, in cents",
                Counter::default()
            ),
            fees: register!(r, "venue_fees_cents", "Fees from fills, by role and kind, in cents", Counters::default()),
            payouts: register!(r, "venue_payouts", "SettlementPayout results by kind", Counters::default()),
            payout_cents: register!(r, "venue_payout_cents", "Cents paid out by kind", Counters::default()),
            void_debits: register!(r, "venue_void_debits_cents", "Negative void returns, in cents", Counter::default()),
            events_defined: register!(r, "venue_events_defined", "DefineEvent applied", Counter::default()),
            events_resolved: register!(r, "venue_events_resolved", "Events resolved, by kind", Counters::default()),
            events_released: register!(r, "venue_events_released", "EventReleased", Counter::default()),
            phase_changes: register!(
                r,
                "venue_event_phase_changes",
                "SetEventPhase applied, by phase and source",
                Counters::default()
            ),
            markets_listed: register!(
                r,
                "venue_markets_listed",
                "ListInstrument applied, by class",
                Counters::default()
            ),
            market_state_changes: register!(
                r,
                "venue_market_state_changes",
                "MarketStateChanged by state",
                Counters::default()
            ),
            last_event_defined: register!(
                r,
                "venue_last_event_defined_timestamp_seconds",
                "Sequencer time of the last DefineEvent",
                FloatGauge::default()
            ),
            markets: register!(r, "venue_markets", "Markets listed now, by state and class", Gauges::default()),
            events: register!(r, "venue_events", "Events carried now, by status", Gauges::default()),
            open_orders: register!(r, "venue_open_orders", "Resting orders", Gauge::default()),
            resting: register!(r, "venue_resting_contracts", "Resting contracts by side", Gauges::default()),
            accounts_active: register!(r, "venue_accounts_active", "Accounts with a sleeve", Gauge::default()),
            capacity_used: register!(r, "venue_capacity_used", "Capacity in use, by resource", Gauges::default()),
            capacity_limit: register!(
                r,
                "venue_capacity_limit",
                "Capacity limits by resource and source (profile or listing)",
                Gauges::default()
            ),
            market_info: register!(r, "venue_market_info", "1 for each listed market", Gauges::default()),
            market_contracts: register!(
                r,
                "venue_market_traded_contracts",
                "Contracts traded in the market",
                Counters::default()
            ),
            market_notional: register!(
                r,
                "venue_market_traded_notional_cents",
                "YES cost traded in the market, in cents",
                Counters::default()
            ),
            market_fills: register!(r, "venue_market_fills", "Fills in the market", Counters::default()),
            market_orders: register!(
                r,
                "venue_market_orders_accepted",
                "Orders accepted in the market",
                Counters::default()
            ),
            market_best: register!(
                r,
                "venue_market_best_price",
                "Best bid (BUY) or ask (SELL) as a probability",
                FloatGauges::default()
            ),
            market_spread: register!(
                r,
                "venue_market_spread",
                "Best ask minus best bid, as a probability",
                FloatGauges::default()
            ),
            market_depth: register!(
                r,
                "venue_market_depth_at_touch_contracts",
                "Contracts at the best price, by side",
                Gauges::default()
            ),
            market_resting: register!(
                r,
                "venue_market_resting_contracts",
                "Contracts resting, by side",
                Gauges::default()
            ),
            market_two_sided: register!(
                r,
                "venue_market_two_sided",
                "1 while the book has a bid and an ask",
                Gauges::default()
            ),
        };
        if let Some(profile) = &derived.profile {
            let limits = [profile.events, profile.instruments, profile.levels, profile.orders, profile.accounts];
            for (resource, limit) in RESOURCES.iter().zip(limits) {
                derived
                    .capacity_limit
                    .get_or_create(&l(&[("resource", resource), ("source", "profile")]))
                    .set(i64::try_from(limit).unwrap_or(i64::MAX));
            }
        }
        derived
    }

    /// The feed connected again and will replay from the start of the journal into a fresh projection: markets get
    /// their series again as the replay lists them, and none released meanwhile is left behind.
    pub fn reconnected(&mut self) {
        self.feed_reconnects.inc();
        let markets: Vec<u64> = self.market_series.iter().copied().collect();
        for market in markets {
            self.forget_market(market);
        }
    }

    /// Takes in one unit: counts what it did (if it is new), then applies it to the projection. `now_ns` is the wall
    /// clock, `now` the monotonic one.
    pub fn apply(&mut self, view: &mut View, catalogue: Option<&Catalogue>, unit: &Unit, now_ns: u64, now: Instant) {
        let input = &unit.input;
        let sequence = u(&input["record"]["sequence"]);
        let at = u(&input["record"]["sequencedAt"]);
        let behind = now_ns.saturating_sub(at);
        self.feed_sequence.set(i64::try_from(sequence).unwrap_or(i64::MAX));
        self.feed_behind.set(venue_telemetry::seconds(behind));
        let applied = unit.results.iter().any(|r| r["msg"] == "CommandApplied" && r["outcome"] == "APPLIED");
        let msg = s(&input["msg"]);
        if applied {
            match msg {
                "DefineEvent" => {
                    self.last_event_defined.set(venue_telemetry::seconds(at));
                }
                "SetListingParameters" => {
                    for (resource, field) in
                        [("events", "maxEvents"), ("instruments", "maxInstruments"), ("levels", "maxPriceLevels")]
                    {
                        self.listing_limit(resource, u(&input[field]));
                    }
                }
                "SetGroupParameters" => self.listing_limit("orders", u(&input["activeOrderLimit"])),
                _ => {}
            }
        }
        // Markets whose events the engine releases in this unit, found before the projection forgets them.
        let released: Vec<u64> = unit
            .results
            .iter()
            .filter(|r| r["msg"] == "EventReleased")
            .flat_map(|r| view.events.get(&u(&r["eventId"])).map(|e| e.instruments.clone()).unwrap_or_default())
            .collect();
        let fresh = sequence > self.counted_through && at >= self.started_ns;
        if fresh {
            self.counted_through = sequence;
            if behind < HOP_HORIZON_NS {
                self.hop.record(behind, now);
            }
            self.count(view, unit, msg, applied);
        }
        view.apply(unit);
        for market in released {
            self.forget_market(market);
        }
        if applied && msg == "ListInstrument" {
            self.remember_market(view, catalogue, u(&input["instrumentId"]));
        }
        if self.gauges_at.is_none_or(|t| now.duration_since(t) >= GAUGES_EVERY) {
            self.gauges_at = Some(now);
            self.refresh(view, catalogue);
        }
    }

    fn listing_limit(&mut self, resource: &'static str, limit: u64) {
        let labels = l(&[("resource", resource), ("source", "listing")]);
        if limit == 0 {
            self.listing_limits.remove(resource);
            self.capacity_limit.remove(&labels);
        } else {
            self.listing_limits.insert(resource, limit);
            self.capacity_limit.get_or_create(&labels).set(i64::try_from(limit).unwrap_or(i64::MAX));
        }
    }

    fn count(&self, view: &View, unit: &Unit, msg: &str, applied: bool) {
        let input = &unit.input;
        if !unit.results.is_empty() || msg != "TimeTick" {
            self.inputs.get_or_create(&l(&[("type", msg)])).inc();
        }
        if applied {
            match msg {
                "DefineEvent" => {
                    self.events_defined.inc();
                }
                "ResolveEvent" => {
                    self.events_resolved.get_or_create(&l(&[("kind", s(&input["kind"]))])).inc();
                }
                "ExpireParlay" => {
                    self.events_resolved.get_or_create(&l(&[("kind", "RESOLVED")])).inc();
                }
                "SetEventPhase" => {
                    let labels = l(&[("phase", s(&input["phase"])), ("source", s(&input["source"]))]);
                    self.phase_changes.get_or_create(&labels).inc();
                }
                "ListInstrument" => {
                    self.markets_listed.get_or_create(&l(&[("class", s(&input["instrumentClass"]))])).inc();
                }
                _ => {}
            }
        }
        for r in &unit.results {
            match s(&r["msg"]) {
                "OrderRejected" => {
                    self.order_rejections.get_or_create(&l(&[("reason", s(&r["reason"]))])).inc();
                }
                "CommandApplied" => {
                    let labels = l(&[("type", msg), ("outcome", s(&r["outcome"])), ("reason", s(&r["rejectReason"]))]);
                    self.command_outcomes.get_or_create(&labels).inc();
                }
                "OrderAccepted" => {
                    self.orders_accepted.inc();
                    self.per_market(&self.market_orders, u(&r["instrumentId"]), 1);
                }
                "OrderCancelled" => {
                    self.order_cancels.get_or_create(&l(&[("reason", s(&r["reason"]))])).inc();
                }
                "Fill" => self.fill(view, r),
                "SettlementPayout" => {
                    let kind = s(&r["kind"]);
                    self.payouts.get_or_create(&l(&[("kind", kind)])).inc();
                    let cents = r["cents"].as_i64().unwrap_or(0);
                    if cents >= 0 {
                        self.payout_cents.get_or_create(&l(&[("kind", kind)])).inc_by(cents.unsigned_abs());
                    } else {
                        self.void_debits.inc_by(cents.unsigned_abs());
                    }
                }
                "MarketStateChanged" => {
                    self.market_state_changes.get_or_create(&l(&[("state", s(&r["state"]))])).inc();
                }
                "EventReleased" => {
                    self.events_released.inc();
                }
                _ => {}
            }
        }
    }

    fn fill(&self, view: &View, r: &Value) {
        let (quantity, tick, instrument) = (u(&r["quantity"]), u(&r["tick"]), u(&r["instrumentId"]));
        // E-2.4: the YES cost of a quantity on the lot step is whole cents.
        let notional = view
            .instruments
            .get(&instrument)
            .and_then(|i| view.ladders.get(&i.table))
            .and_then(|ladder| ladder.get(usize::try_from(tick).ok()?))
            .map_or(0, |row| {
                u128::from(quantity) * 100 * u128::from(row.numerator) / u128::from(row.denominator.max(1))
            });
        let notional = u64::try_from(notional).unwrap_or(u64::MAX);
        self.fills.inc();
        self.traded_contracts.inc_by(quantity);
        self.traded_notional.inc_by(notional);
        self.per_market(&self.market_fills, instrument, 1);
        self.per_market(&self.market_contracts, instrument, quantity);
        self.per_market(&self.market_notional, instrument, notional);
        for (role, field) in [("maker", "makerFeeCents"), ("taker", "takerFeeCents")] {
            let fee = r[field].as_i64().unwrap_or(0);
            let kind = if fee >= 0 { "charged" } else { "rebate" };
            if fee != 0 {
                self.fees.get_or_create(&l(&[("role", role), ("kind", kind)])).inc_by(fee.unsigned_abs());
            }
        }
    }

    /// Adds to a per-market counter, for a market that has series.
    fn per_market(&self, family: &Counters, market: u64, n: u64) {
        if self.market_series.contains(&market) {
            family.get_or_create(&l(&[("market", &market.to_string())])).inc_by(n);
        }
    }

    fn remember_market(&mut self, view: &View, catalogue: Option<&Catalogue>, market: u64) {
        if self.market_series.len() >= self.max_market_series || self.market_series.contains(&market) {
            return;
        }
        let Some(i) = view.instruments.get(&market) else { return };
        self.market_series.insert(market);
        let title =
            catalogue.map_or_else(String::new, |c| format!("{} / {}", c.title(i.event), c.label(i.event, market)));
        let (id, event) = (market.to_string(), i.event.to_string());
        let labels = l(&[("market", &id), ("event", &event), ("class", &i.class), ("title", &title)]);
        self.market_info.get_or_create(&labels).set(1);
        self.info_labels.insert(market, labels);
        for family in [&self.market_contracts, &self.market_notional, &self.market_fills, &self.market_orders] {
            // Present at 0 from the listing, so a market's first trade is a rise Prometheus sees.
            let _ = family.get_or_create(&l(&[("market", &id)]));
        }
    }

    /// The engine released the market: its series end.
    fn forget_market(&mut self, market: u64) {
        if !self.market_series.remove(&market) {
            return;
        }
        let id = market.to_string();
        let one = l(&[("market", &id)]);
        for family in [&self.market_contracts, &self.market_notional, &self.market_fills, &self.market_orders] {
            family.remove(&one);
        }
        self.market_spread.remove(&one);
        self.market_two_sided.remove(&one);
        for side in ["BUY", "SELL"] {
            let sided = l(&[("market", &id), ("side", side)]);
            self.market_best.remove(&sided);
            self.market_depth.remove(&sided);
            self.market_resting.remove(&sided);
        }
        if let Some(labels) = self.info_labels.remove(&market) {
            self.market_info.remove(&labels);
        }
    }

    /// Recomputes the state gauges from the projection.
    fn refresh(&mut self, view: &View, catalogue: Option<&Catalogue>) {
        let mut markets: BTreeMap<(String, String), i64> = BTreeMap::new();
        let mut levels = 0u64;
        for (id, i) in &view.instruments {
            *markets.entry((i.state.clone(), i.class.clone())).or_default() += 1;
            levels += 2 * view.ladders.get(&i.table).map_or(0, |ladder| ladder.len() as u64);
            if !self.market_series.contains(id) {
                self.remember_market(view, catalogue, *id);
            }
        }
        self.markets.clear();
        for ((state, class), n) in markets {
            self.markets.get_or_create(&l(&[("state", &state), ("class", &class)])).set(n);
        }
        let mut events: BTreeMap<&str, i64> = BTreeMap::new();
        for e in view.events.values() {
            let status = if e.resolved {
                "resolved"
            } else if e.ended {
                "ended"
            } else if e.started {
                "started"
            } else {
                "scheduled"
            };
            *events.entry(status).or_default() += 1;
        }
        self.events.clear();
        for (status, n) in events {
            self.events.get_or_create(&l(&[("status", status)])).set(n);
        }
        let gauge = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        self.open_orders.set(gauge(view.orders.len()));
        let (mut buy, mut sell) = (0u64, 0u64);
        for o in view.orders.values() {
            if o.side == "BUY" {
                buy += o.remaining;
            } else {
                sell += o.remaining;
            }
        }
        self.resting.get_or_create(&l(&[("side", "BUY")])).set(i64::try_from(buy).unwrap_or(i64::MAX));
        self.resting.get_or_create(&l(&[("side", "SELL")])).set(i64::try_from(sell).unwrap_or(i64::MAX));
        self.accounts_active.set(gauge(view.sleeves.len()));
        let used = [
            view.events.len() as u64,
            view.instruments.len() as u64,
            levels,
            view.orders.len() as u64,
            view.sleeves.len() as u64,
        ];
        for (resource, n) in RESOURCES.iter().zip(used) {
            self.capacity_used.get_or_create(&l(&[("resource", resource)])).set(i64::try_from(n).unwrap_or(i64::MAX));
        }
        for &market in &self.market_series {
            let Some(i) = view.instruments.get(&market) else { continue };
            let id = market.to_string();
            let bp = |tick: u64| {
                view.tables.get(&i.table).and_then(|t| t.get(usize::try_from(tick).ok()?)).copied().unwrap_or(0)
            };
            let sides = [("BUY", i.best_bid(), &i.bids), ("SELL", i.best_ask(), &i.asks)];
            for (side, best, levels) in sides {
                let labels = l(&[("market", &id), ("side", side)]);
                match best {
                    Some(tick) => {
                        self.market_best.get_or_create(&labels).set(probability(bp(tick)));
                        let depth = levels.get(&tick).map_or(0, |(q, _)| *q);
                        self.market_depth.get_or_create(&labels).set(i64::try_from(depth).unwrap_or(i64::MAX));
                    }
                    None => {
                        self.market_best.remove(&labels);
                        self.market_depth.get_or_create(&labels).set(0);
                    }
                }
                let resting: u64 = levels.values().map(|(q, _)| q).sum();
                self.market_resting.get_or_create(&labels).set(i64::try_from(resting).unwrap_or(i64::MAX));
            }
            let one = l(&[("market", &id)]);
            match (i.best_bid(), i.best_ask()) {
                (Some(bid), Some(ask)) => {
                    self.market_spread.get_or_create(&one).set(probability(bp(ask).saturating_sub(bp(bid))));
                    self.market_two_sided.get_or_create(&one).set(1);
                }
                _ => {
                    self.market_spread.remove(&one);
                    self.market_two_sided.get_or_create(&one).set(0);
                }
            }
        }
    }
}

/// Basis points as a probability.
#[allow(clippy::float_arithmetic)]
fn probability(bp: u64) -> f64 {
    bp as f64 / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::encoding::text::encode;
    use serde_json::json;

    const START: u64 = 1_000_000_000_000;

    fn unit(sequence: u64, at: u64, mut input: Value, results: Vec<Value>) -> Unit {
        input["record"] = json!({"sequence": sequence, "sequencedAt": at, "sequencerGroup": 1});
        Unit { input, results }
    }

    fn applied() -> Value {
        json!({"msg": "CommandApplied", "outcome": "APPLIED", "rejectReason": "NONE"})
    }

    #[allow(clippy::disallowed_methods)]
    fn now() -> Instant {
        Instant::now()
    }

    fn text(registry: &Registry) -> String {
        let mut text = String::new();
        encode(&mut text, registry).expect("encoded");
        text
    }

    #[test]
    fn a_market_is_counted_while_listed_and_forgotten_when_released_and_history_is_not_counted() {
        let mut registry = Registry::default();
        let hop = Hdr::default();
        let profile = Capacities::named("small");
        let mut derived = Derived::register(&mut registry, START, profile, 500, &hop);
        let mut view = View::default();
        let t = now();
        let ticks: Vec<Value> =
            (0..99).map(|t| json!({"tick": t, "numerator": t + 1, "denominator": 100, "complement": 98 - t})).collect();
        let history = [
            unit(1, START - 5, json!({"msg": "DefineTickTable", "tickTableId": 1, "ticks": ticks}), vec![applied()]),
            unit(
                2,
                START - 4,
                json!({"msg": "DefineEvent", "eventId": 7, "stateCount": 2, "scheduledStart": 0}),
                vec![applied()],
            ),
            unit(
                3,
                START - 3,
                json!({"msg": "ListInstrument", "instrumentId": 700, "eventId": 7, "instrumentClass": "BINARY",
                       "tickTableId": 1, "lotStep": 1, "stateMask": 1, "legs": []}),
                vec![applied()],
            ),
            unit(4, START - 2, json!({"msg": "SetGroupParameters", "activeOrderLimit": 12000}), vec![applied()]),
        ];
        for u in &history {
            derived.apply(&mut view, None, u, START, t);
        }
        let book = |side: &str, tick: u64, quantity: u64| json!({"msg": "BookLevelChanged", "instrumentId": 700, "side": side, "tick": tick, "quantity": quantity, "orderCount": 1});
        let live = [
            unit(
                5,
                START + 10,
                json!({"msg": "NewOrder"}),
                vec![
                    json!({"msg": "OrderAccepted", "orderId": 1, "accountId": 3, "instrumentId": 700, "side": "BUY", "tick": 39, "quantity": 10, "clientOrderId": "a"}),
                    book("BUY", 39, 10),
                ],
            ),
            unit(6, START + 11, json!({"msg": "NewOrder"}), vec![book("SELL", 44, 5)]),
            unit(
                7,
                START + 12,
                json!({"msg": "NewOrder"}),
                vec![
                    json!({"msg": "Fill", "instrumentId": 700, "tick": 39, "quantity": 4, "makerOrderId": 1, "makerRemaining": 6,
                           "takerOrderId": 2, "takerRemaining": 0, "makerFeeCents": -1, "takerFeeCents": 3}),
                    json!({"msg": "OrderRejected", "reason": "INSUFFICIENT_SLEEVE"}),
                    book("BUY", 39, 6),
                ],
            ),
        ];
        for (k, u) in (1..).zip(&live) {
            derived.apply(&mut view, None, u, START + 20, t + GAUGES_EVERY * k);
        }
        let exported = text(&registry);
        for line in [
            "venue_fills_total 1\n",
            "venue_traded_contracts_total 4\n",
            "venue_traded_notional_cents_total 160\n",
            "venue_fees_cents_total{role=\"maker\",kind=\"rebate\"} 1\n",
            "venue_fees_cents_total{role=\"taker\",kind=\"charged\"} 3\n",
            "venue_order_rejections_total{reason=\"INSUFFICIENT_SLEEVE\"} 1\n",
            "venue_market_traded_contracts_total{market=\"700\"} 4\n",
            "venue_market_info{market=\"700\",event=\"7\",class=\"BINARY\",title=\"\"} 1\n",
            "venue_market_best_price{market=\"700\",side=\"BUY\"} 0.4\n",
            "venue_market_spread{market=\"700\"} 0.05\n",
            "venue_market_two_sided{market=\"700\"} 1\n",
            "venue_market_depth_at_touch_contracts{market=\"700\",side=\"BUY\"} 6\n",
            "venue_capacity_used{resource=\"levels\"} 198\n",
            "venue_capacity_limit{resource=\"orders\",source=\"listing\"} 12000\n",
            "venue_capacity_limit{resource=\"orders\",source=\"profile\"} 16384\n",
            "venue_markets{state=\"LISTED\",class=\"BINARY\"} 1\n",
        ] {
            assert!(exported.contains(line), "{line} in {exported}");
        }
        assert!(
            !exported.contains("venue_events_defined_total 1"),
            "history before the start is not counted: {exported}"
        );
        let release =
            unit(8, START + 30, json!({"msg": "TimeTick"}), vec![json!({"msg": "EventReleased", "eventId": 7})]);
        derived.apply(&mut view, None, &release, START + 40, t + GAUGES_EVERY * 9);
        let exported = text(&registry);
        assert!(exported.contains("venue_events_released_total 1\n"), "{exported}");
        assert!(!exported.contains("market=\"700\""), "a released market's series end: {exported}");
        // A replay after reconnecting counts nothing again.
        derived.reconnected();
        let mut replayed = View::default();
        for u in history.iter().chain(&live) {
            derived.apply(&mut replayed, None, u, START + 50, t + GAUGES_EVERY * 10);
        }
        assert!(text(&registry).contains("venue_fills_total 1\n"));
        assert_eq!(hop.percentiles(&[0.5]).0, 4, "one hop for each live unit, none for history or the replay");
    }
}
