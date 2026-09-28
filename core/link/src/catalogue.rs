//! The demo markets: titles and outcome names the engine never sees, the accounts, and the inputs that create them.
//!
//! IDs encode where they came from, so any reader can label them after a restart: event
//! `template × 1 000 000 + round`, instrument `event × 100 + index`. A calendar lists a round every few minutes for as
//! long as a venue runs, so a template has a million rounds, and an event up to a hundred markets. Journals recorded
//! under the first scheme, event `template × 1000 + round` and instrument `event × 10 + index`, are still labelled:
//! every event of the current scheme is at least a million, and every earlier one below it.
//!
//! Every seeding command has a fixed ID, so seeding twice within seven days changes nothing (E-7.1). After that the
//! IDs have expired (D-25), and seeding again would grant the accounts' money again.

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

/// How long a demo venue keeps a settled event before the engine releases it (E-8.4, D-29): half an hour of
/// sequencer time, long enough for the page's followers to see a round settle and for the parlay service to settle
/// parlays on its legs, and short enough that a calendar listing rounds every few minutes runs indefinitely on the
/// `small` profile. Production keeps the engine's default of seven days.
pub const DEMO_SETTLED_RETENTION_SECONDS: u64 = 1_800;

/// D-12's window, restated here because E-7.4 replaces both group parameters together: see `group_parameters`.
///
/// This is half of a sizing rule, and the half a deployment chooses. The engine records one entry per new order
/// for the whole window and the table does not evict early, so a group can take
/// `Capacities::duplicates / window` new orders a second and no more before it fail-stops (spec §18). At this
/// default that is about 36/s on `small`, 583/s on `launch` and 2,330/s on `scale` — so a venue expecting more
/// new orders a second than that cannot get there by picking a larger profile, and has to shorten its window
/// instead (`window_for`). Shortening is supported: E-7.4 takes a lower value and the engine reorders its
/// expiry queue for it.
pub const CLIENT_ORDER_ID_WINDOW_SECONDS: u64 = 7_200;

/// The longest client order ID window a group of this capacity can keep while taking `orders_per_second` new
/// orders, which is what `duplicates / rate` comes to, clamped to one second and never above the default. For
/// sizing a test environment or a group against the order rate it is expected to see, rather than finding out
/// from a fail-stop.
pub fn window_for(profile: &Capacities, orders_per_second: u64) -> u64 {
    let rate = orders_per_second.max(1);
    (profile.duplicates as u64 / rate).clamp(1, CLIENT_ORDER_ID_WINDOW_SECONDS)
}

/// The demo default fee rates, in millionths: see `Catalogue::fees`.
pub const DEMO_MAKER_RATE_MICROS: i64 = 17_500;
pub const DEMO_TAKER_RATE_MICROS: i64 = 70_000;

/// The catalogue's synthetic maker, whose demo override makes it a rebated market maker.
const SYNTHETIC_MAKER: u64 = 10;

pub struct Catalogue {
    accounts: Vec<Value>,
    markets: Vec<Value>,
}

/// Rounds a template can have, and so the factor between a template and its events' IDs.
pub const ROUNDS: u64 = 1_000_000;
/// Markets an event can have, and so the factor between an event and its instruments' IDs.
pub const MARKETS: u64 = 100;

pub const fn event_id(template: u64, round: u64) -> u64 {
    template * ROUNDS + round
}

pub const fn instrument_id(event: u64, index: u64) -> u64 {
    event * MARKETS + index
}

/// The template and round an event ID encodes, under whichever scheme it was listed.
pub const fn split_event(event: u64) -> (u64, u64) {
    if event < ROUNDS {
        (event / 1000, event % 1000)
    } else {
        (event / ROUNDS, event % ROUNDS)
    }
}

/// Where an instrument sits among its event's markets, under the scheme its event was listed with.
pub fn market_index(event: u64, instrument: u64) -> Option<usize> {
    let factor = if event < ROUNDS { 10 } else { MARKETS };
    instrument.checked_sub(event.checked_mul(factor)?).filter(|i| *i < factor).map(|i| i as usize)
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
        let (template, round) = split_event(event);
        match self.market(template) {
            Some(m) if round > 1 => format!("{} (round {round})", m["title"].as_str().unwrap_or("")),
            Some(m) => m["title"].as_str().unwrap_or("").to_string(),
            None => format!("Event {event}"),
        }
    }

    pub fn states(&self, event: u64, count: u64) -> Vec<String> {
        let named = self.market(split_event(event).0).and_then(|m| m["states"].as_array());
        (0..count as usize)
            .map(|s| named.and_then(|n| n.get(s)).and_then(Value::as_str).map_or(format!("State {s}"), str::to_string))
            .collect()
    }

    pub fn label(&self, event: u64, instrument: u64) -> String {
        let index = market_index(event, instrument);
        self.market(split_event(event).0)
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

    /// The templates, in catalogue order.
    pub fn templates(&self) -> Vec<u64> {
        self.markets.iter().filter_map(|m| m["template"].as_u64()).collect()
    }

    /// How many markets one round of a template lists.
    pub fn market_count(&self, template: u64) -> usize {
        self.market(template).and_then(|m| m["instruments"].as_array()).map_or(0, Vec::len)
    }

    /// The inputs that list one round of a market and open it. `scheduled_start` is the sequencer time the event is
    /// expected to start, in nanoseconds since the epoch, or 0 for none.
    pub fn listing(&self, template: u64, round: u64, scheduled_start: u64) -> Result<Vec<Value>, Error> {
        let market = self.market(template).ok_or_else(|| format!("no market template {template}"))?;
        let states = market["states"].as_array().map_or(0, Vec::len);
        let instruments = market["instruments"].as_array().map_or(&[][..], Vec::as_slice);
        if !(1..ROUNDS).contains(&round) || instruments.len() as u64 > MARKETS {
            return Err(
                format!("template {template} cannot list round {round} of {} markets", instruments.len()).into()
            );
        }
        let event = event_id(template, round);
        let mut inputs = vec![command(
            "DefineEvent",
            format!("ev-{event}"),
            json!({"eventId": event, "stateCount": states, "scheduledStart": scheduled_start}),
        )];
        for (index, inst) in instruments.iter().enumerate() {
            let id = instrument_id(event, index as u64);
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
    fn group_parameters(order_ceiling: usize, id_window_seconds: u64) -> Value {
        command(
            "SetGroupParameters",
            "seed-limits".into(),
            json!({
                "activeOrderLimit": active_order_limit(order_ceiling),
                "clientOrderIdWindowSeconds": id_window_seconds
            }),
        )
    }

    /// The retention and the listing limits (E-7.12, D-30). The limits are the profile's own tables, so a listing past
    /// them is refused with `LISTING_CAPACITY` rather than fail-stopping the core; they must therefore be the profile
    /// the node runs with. Listers still stop short of them (the oracle's and the parlay service's `no_room`), so a
    /// round or a parlay is refused whole rather than listed in part.
    fn listing_parameters(profile: &Capacities) -> Value {
        let limit = |n: usize| u64::try_from(n).unwrap_or(u64::MAX).min(u64::from(u32::MAX));
        command(
            "SetListingParameters",
            "seed-listing".into(),
            json!({
                "settledRetentionSeconds": DEMO_SETTLED_RETENTION_SECONDS,
                "maxEvents": limit(profile.events),
                "maxInstruments": limit(profile.instruments),
                "maxPriceLevels": limit(profile.levels)
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

    /// Everything a fresh venue needs: the group settings, the listing parameters, the fee schedule, the tick table,
    /// funded accounts and round 1 of every market, for a node running `profile`. The settings come first, so no
    /// order or listing can exist before the limit that bounds it, and no fill before the fees that apply to it.
    pub fn seed(&self, profile: &Capacities) -> Result<Vec<Value>, Error> {
        self.seed_with(profile, CLIENT_ORDER_ID_WINDOW_SECONDS)
    }

    /// The same, with the client order ID window the group is to start on. A group that will take more new
    /// orders a second than `duplicates / CLIENT_ORDER_ID_WINDOW_SECONDS` has to be founded on a shorter one,
    /// since the window can only be shortened by replacing both group parameters (E-7.4) and the table
    /// fail-stops when it fills: see `window_for`.
    pub fn seed_with(&self, profile: &Capacities, id_window_seconds: u64) -> Result<Vec<Value>, Error> {
        let mut inputs =
            vec![Self::group_parameters(profile.orders, id_window_seconds), Self::listing_parameters(profile)];
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
            inputs.extend(self.listing(m["template"].as_u64().ok_or("a market needs a template")?, 1, 0)?);
        }
        Ok(inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_id_names_its_template_and_round_under_either_scheme() {
        assert_eq!(event_id(3, 1), 3_000_001);
        assert_eq!(split_event(event_id(3, 1)), (3, 1));
        assert_eq!(split_event(event_id(2, 999_999)), (2, 999_999));
        // Recorded before the scheme widened.
        assert_eq!(split_event(2_017), (2, 17));
    }

    #[test]
    fn rounds_go_past_a_thousand() {
        let event = event_id(1, 1_000);
        assert_eq!(split_event(event), (1, 1_000));
        assert_eq!(market_index(event, instrument_id(event, 2)), Some(2));
    }

    #[test]
    fn an_instrument_names_its_place_in_the_event_under_either_scheme() {
        assert_eq!(market_index(2_001, 20_012), Some(2));
        assert_eq!(market_index(event_id(2, 1), instrument_id(event_id(2, 1), 99)), Some(99));
        assert_eq!(market_index(event_id(2, 1), 7), None);
        assert_eq!(market_index(2_001, 20_010 + 10), None);
    }
}
