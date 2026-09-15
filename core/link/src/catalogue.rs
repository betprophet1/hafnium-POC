//! The demo markets: titles and outcome names the engine never sees, the accounts, and the inputs that create them.
//!
//! IDs encode where they came from, so any reader can label them after a restart: event `template × 1000 + round`,
//! instrument `event × 10 + index`. Every seeding command has a fixed ID, so seeding twice changes nothing (E-7.1).

use crate::wire::{command, Error};
use serde_json::{json, Value};
use std::fs;

pub const TICK_TABLE: u64 = 1;

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

    /// Everything a fresh venue needs: the tick table, funded accounts and round 1 of every market.
    pub fn seed(&self) -> Result<Vec<Value>, Error> {
        let mut inputs = vec![Self::cent_table()];
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
