//! The demo markets: titles and outcome names the engine never sees, the accounts, and the inputs that create them.
//!
//! IDs encode where they came from, so any reader can label them after a restart: event `template × 1000 + round`,
//! instrument `event × 10 + index`. Every seeding command has a fixed ID, so seeding twice within seven days changes
//! nothing (E-7.1). After that the IDs have expired (D-25), and seeding again would grant the accounts' money again.

use crate::wire::{command, Error};
use serde_json::{json, Value};
use std::fs;
use venue_engine::state::Capacities;

pub const TICK_TABLE: u64 = 1;

/// Resting orders a venue allows at once, which E-9.3 check 17 refuses past with `ORDER_CAPACITY`, for a node whose
/// capacity profile holds `order_ceiling` orders. Zero means none, and none is what a venue gets unless someone sets
/// this: with no limit nothing checks the group's resting orders, so a venue that fills its books reaches
/// `alloc_order`, finds the slab's free list empty and fail-stops instead of rejecting the order. A seeded venue
/// therefore sets one at three quarters of the ceiling, with room to spare for the slot an order takes before check
/// 17 runs and gives back when it rejects.
pub const fn active_order_limit(order_ceiling: usize) -> u64 {
    order_ceiling as u64 / 4 * 3
}

/// A capacity profile by name, which must be the profile the node was started with: operator tools size what they
/// set from it.
pub fn capacity_profile(profile: &str) -> Result<Capacities, Error> {
    Capacities::named(profile)
        .ok_or_else(|| format!("unknown capacity profile {profile:?}: small, launch or scale").into())
}

/// D-12's window, restated here because E-7.4 replaces both group parameters together: see `group_parameters`.
pub const CLIENT_ORDER_ID_WINDOW_SECONDS: u64 = 7_200;

/// The demo default fee rates, in millionths: see `Catalogue::fees`.
pub const DEMO_MAKER_RATE_MICROS: i64 = 17_500;
pub const DEMO_TAKER_RATE_MICROS: i64 = 70_000;

/// The catalogue's synthetic maker, whose demo override makes it a rebated market maker.
const SYNTHETIC_MAKER: u64 = 10;

pub struct Catalogue {
    accounts: Vec<Value>,
    markets: Vec<Value>,
}

pub const fn event_id(template: u64, round: u64) -> u64 {
    template * 1000 + round
}

impl Catalogue {
    pub fn load(path: &str) -> Result<Self, Error> {
        let v: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
        let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
        Ok(Self { accounts: list("accounts"), markets: list("markets") })
    }

    pub fn accounts(&self) -> &[Value] {
        &self.accounts
    }

    fn market(&self, template: u64) -> Option<&Value> {
        self.markets.iter().find(|m| m["template"].as_u64() == Some(template))
    }

    pub fn title(&self, event: u64) -> String {
        match self.market(event / 1000) {
            Some(m) if event % 1000 > 1 => format!("{} (round {})", m["title"].as_str().unwrap_or(""), event % 1000),
            Some(m) => m["title"].as_str().unwrap_or("").to_string(),
            None => format!("Event {event}"),
        }
    }

    pub fn states(&self, event: u64, count: u64) -> Vec<String> {
        let named = self.market(event / 1000).and_then(|m| m["states"].as_array());
        (0..count as usize)
            .map(|s| named.and_then(|n| n.get(s)).and_then(Value::as_str).map_or(format!("State {s}"), str::to_string))
            .collect()
    }

    pub fn label(&self, event: u64, instrument: u64) -> String {
        let index = instrument.checked_sub(event * 10).map(|i| i as usize);
        self.market(event / 1000)
            .and_then(|m| m["instruments"].get(index?))
            .and_then(|i| i["label"].as_str())
            .map_or(format!("Instrument {instrument}"), str::to_string)
    }

    /// A tick table of whole cents, 1¢ to 99¢: tick `t` is `t + 1` cents.
    fn cent_table() -> Value {
        let ticks: Vec<Value> =
            (0..99).map(|t| json!({"tick": t, "numerator": t + 1, "denominator": 100, "complement": 98 - t})).collect();
        command("DefineTickTable", "seed-cents".into(), json!({"tickTableId": TICK_TABLE, "ticks": ticks}))
    }

    /// The inputs that list one round of a market and open it.
    pub fn listing(&self, template: u64, round: u64) -> Result<Vec<Value>, Error> {
        let market = self.market(template).ok_or_else(|| format!("no market template {template}"))?;
        let states = market["states"].as_array().map_or(0, Vec::len);
        let event = event_id(template, round);
        let mut inputs = vec![command(
            "DefineEvent",
            format!("ev-{event}"),
            json!({"eventId": event, "stateCount": states, "scheduledStart": 0}),
        )];
        for (index, inst) in market["instruments"].as_array().into_iter().flatten().enumerate() {
            let id = event * 10 + index as u64;
            inputs.push(command(
                "ListInstrument",
                format!("in-{id}"),
                json!({
                    "instrumentId": id, "eventId": event, "instrumentClass": inst["class"], "stateMask": inst["mask"],
                    "tickTableId": TICK_TABLE, "lotStep": 1, "allocationPolicy": "PRICE_TIME", "positionLimit": 0,
                    "legs": []
                }),
            ));
            inputs.push(command("SetMarketState", format!("op-{id}"), json!({"instrumentId": id, "state": "OPEN"})));
        }
        Ok(inputs)
    }

    /// The venue-wide settings. E-7.4 replaces both of them together, so setting the order limit here also sets
    /// the client order ID window, and the window D-12 publishes has to be restated rather than left out — leaving
    /// it out would reset it to whatever this command carried.
    fn group_parameters(order_ceiling: usize) -> Value {
        command(
            "SetGroupParameters",
            "seed-limits".into(),
            json!({
                "activeOrderLimit": active_order_limit(order_ceiling),
                "clientOrderIdWindowSeconds": CLIENT_ORDER_ID_WINDOW_SECONDS
            }),
        )
    }

    /// The demo fee schedule (spec E-10.11, D-2), in the form prediction-market exchanges publish: a taker pays
    /// 7% and a maker 1.75% of `contracts × P × (1 − P)` dollars. The synthetic maker, standing in for a market
    /// maker on a programme, instead earns a 0.5% maker rebate and pays no taker fee, for as long as the venue runs
    /// (an override with no expiry, D-27), when the catalogue has it. Rates are millionths.
    fn fees(&self) -> Vec<Value> {
        let mut inputs = vec![command(
            "SetFeeSchedule",
            "seed-fees".into(),
            json!({"makerRateMicros": DEMO_MAKER_RATE_MICROS, "takerRateMicros": DEMO_TAKER_RATE_MICROS}),
        )];
        if self.accounts.iter().any(|a| a["id"].as_u64() == Some(SYNTHETIC_MAKER)) {
            inputs.push(command(
                "SetFeeOverride",
                "seed-fee-maker-programme".into(),
                json!({
                    "overrideId": 1, "priority": 10, "accountId": SYNTHETIC_MAKER, "instrumentId": 0, "eventId": 0,
                    "makerRateMicros": -5_000, "takerRateMicros": 0, "expiresAt": null
                }),
            ));
        }
        inputs
    }

    /// Everything a fresh venue needs: the group settings, the fee schedule, the tick table, funded accounts and
    /// round 1 of every market. The settings come first, so no order can exist before the limit that bounds them,
    /// and no fill before the fees that apply to it.
    pub fn seed(&self, order_ceiling: usize) -> Result<Vec<Value>, Error> {
        let mut inputs = vec![Self::group_parameters(order_ceiling)];
        inputs.extend(self.fees());
        inputs.push(Self::cent_table());
        for a in &self.accounts {
            let id = a["id"].as_u64().ok_or("an account needs an id")?;
            inputs.push(command(
                "SleeveAdjust",
                format!("grant-{id}"),
                json!({"accountId": id, "deltaCents": a["cents"], "reason": "GRANT"}),
            ));
        }
        for m in &self.markets {
            inputs.extend(self.listing(m["template"].as_u64().ok_or("a market needs a template")?, 1)?);
        }
        Ok(inputs)
    }
}
