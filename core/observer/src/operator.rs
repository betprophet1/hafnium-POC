//! Read-only operator endpoints (deploy/observability/metrics.md, operator endpoints), on the observer's internal
//! port beside its metrics. Every answer is read from the observer's projection of the feed at the sequence it names;
//! nothing here queries the engine or can change anything.

use crate::State;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex, PoisonError};
use venue_link::view::{InstrumentView, View};
use venue_telemetry::hdr::Hdr;
use venue_telemetry::serve::{Answer, Handler};

const DEFAULT_DEPTH: usize = 10;
const MAX_DEPTH: usize = 100;
/// Open orders listed for one account at most; the summary counts them all.
const MAX_ORDERS_LISTED: usize = 200;

pub fn handler(state: Arc<Mutex<State>>, hops: Hdr) -> Handler {
    Arc::new(move |path, query| {
        let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
        if segments.as_slice() == ["v1", "latency"] {
            return Some(latency(&hops));
        }
        let state = state.lock().unwrap_or_else(PoisonError::into_inner);
        match segments.as_slice() {
            ["v1", "markets"] => Some(markets(&state)),
            ["v1", "markets", id] => Some(match id.parse() {
                Ok(id) => market(&state, id, depth(query)),
                Err(_) => not_found("markets are named by number"),
            }),
            ["v1", "accounts", id] => Some(match id.parse() {
                Ok(id) => account(&state.view, id),
                Err(_) => not_found("accounts are named by number"),
            }),
            _ => None,
        }
    })
}

fn not_found(message: &str) -> Answer {
    Answer::json(404, &json!({"error": message}))
}

fn depth(query: &str) -> usize {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("depth="))
        .and_then(|d| d.parse().ok())
        .map_or(DEFAULT_DEPTH, |d: usize| d.clamp(1, MAX_DEPTH))
}

fn title(state: &State, event: u64, market: u64) -> Value {
    state.catalogue.as_ref().map_or(Value::Null, |c| json!(format!("{} / {}", c.title(event), c.label(event, market))))
}

/// A tick with its price in basis points of probability.
fn priced(view: &View, i: &InstrumentView, tick: u64) -> Value {
    let bp = view.tables.get(&i.table).and_then(|t| t.get(usize::try_from(tick).ok()?)).copied();
    json!({"tick": tick, "bp": bp})
}

fn phase(view: &View, event: u64) -> &'static str {
    match view.events.get(&event) {
        None => "unknown",
        Some(e) if e.resolved => "resolved",
        Some(e) if e.ended => "ended",
        Some(e) if e.suspended => "suspended",
        Some(e) if e.started => "started",
        Some(_) => "scheduled",
    }
}

fn markets(state: &State) -> Answer {
    let view = &state.view;
    let list: Vec<Value> = view
        .instruments
        .iter()
        .map(|(&id, i)| {
            json!({
                "market": id, "event": i.event, "title": title(state, i.event, id), "class": i.class, "state": i.state,
                "eventPhase": phase(view, i.event),
                "bestBid": i.best_bid().map(|t| priced(view, i, t)), "bestAsk": i.best_ask().map(|t| priced(view, i, t)),
                "last": i.last.map(|t| priced(view, i, t))
            })
        })
        .collect();
    Answer::json(200, &json!({"sequence": view.sequence, "markets": list}))
}

fn market(state: &State, id: u64, depth: usize) -> Answer {
    let view = &state.view;
    let Some(i) = view.instruments.get(&id) else { return not_found("no such market listed") };
    let level = |(tick, (quantity, orders)): (&u64, &(u64, u64))| {
        let mut l = priced(view, i, *tick);
        l["quantity"] = json!(quantity);
        l["orders"] = json!(orders);
        l
    };
    let event = view.events.get(&i.event).map(|e| {
        json!({
            "event": i.event, "phase": phase(view, i.event), "states": e.state_count, "definedAt": e.defined_at,
            "scheduledStart": e.scheduled_start, "startedAt": e.started_at, "endedAt": e.ended_at,
            "suspendedEver": e.suspended_ever,
            "resolution": e.resolution.as_ref().map(|r| json!({
                "kind": r.kind, "winningState": r.winning_state, "caseId": r.case_id, "at": r.at
            }))
        })
    });
    Answer::json(
        200,
        &json!({
            "sequence": view.sequence, "market": id, "title": title(state, i.event, id), "class": i.class,
            "state": i.state, "lotStep": i.lot, "stateMask": i.mask, "event": event,
            "legs": i.legs.iter().map(|l| json!({"event": l.event, "market": l.instrument, "yes": l.yes})).collect::<Vec<_>>(),
            "book": {
                "bids": i.bids.iter().rev().take(depth).map(level).collect::<Vec<_>>(),
                "asks": i.asks.iter().take(depth).map(level).collect::<Vec<_>>(),
            },
            "last": i.last.map(|t| priced(view, i, t))
        }),
    )
}

fn account(view: &View, id: u64) -> Answer {
    let sleeve = view
        .sleeves
        .get(&id)
        .map(|(available, reserved)| json!({"availableCents": available, "reservedCents": reserved}));
    let positions: Vec<Value> = view
        .positions
        .iter()
        .filter(|((account, _), _)| *account == id)
        .map(|((_, market), position)| json!({"market": market, "position": position}))
        .collect();
    let orders: Vec<_> = view.orders.iter().filter(|(_, o)| o.account == id).collect();
    let resting: u64 = orders.iter().map(|(_, o)| o.remaining).sum();
    if sleeve.is_none() && positions.is_empty() && orders.is_empty() {
        return not_found("the projection holds nothing for this account");
    }
    let listed: Vec<Value> = orders
        .iter()
        .take(MAX_ORDERS_LISTED)
        .map(|(order, o)| {
            json!({"orderId": order, "market": o.instrument, "side": o.side, "tick": o.tick, "remaining": o.remaining, "filled": o.filled})
        })
        .collect();
    Answer::json(
        200,
        &json!({
            "sequence": view.sequence, "account": id, "sleeve": sleeve, "positions": positions,
            "openOrders": {"count": orders.len(), "restingContracts": resting, "listed": listed}
        }),
    )
}

#[allow(clippy::float_arithmetic)]
fn latency(hops: &Hdr) -> Answer {
    let quantiles = [0.5, 0.9, 0.99, 0.999];
    let (count, values, max) = hops.percentiles(&quantiles);
    let micros = |ns: u64| ns as f64 / 1_000.0;
    Answer::json(
        200,
        &json!({
            "hop": "sequenced_to_feed", "clock": "software", "count": count,
            "p50Micros": micros(values[0]), "p90Micros": micros(values[1]), "p99Micros": micros(values[2]),
            "p999Micros": micros(values[3]), "maxMicros": micros(max),
            "note": "since this observer started, at full resolution; a functional profile's numbers are not latency figures"
        }),
    )
}
