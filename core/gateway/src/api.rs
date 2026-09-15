//! Order entry shared by REST and WebSocket: total validation, price and stake conversion, and the sequenced outcome
//! as an `OrderResult` (interfaces/gateway/openapi.yaml).

use crate::hub::{self, id, Hub};
use crate::limits::{monotonic, Bucket};
use crate::price::{self, PriceError};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use venue_link::submit::Submit;
use venue_link::wire;

pub struct Limits {
    pub per_second: u32,
    pub burst: u32,
    pub logons_per_minute: u32,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
    pub details: Option<Value>,
    pub retry_after: Option<u64>,
}

impl ApiError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), details: None, retry_after: None }
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "BAD_REQUEST", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(401, "UNAUTHORIZED", "a valid token is required")
    }

    pub fn not_found(message: &str) -> Self {
        Self::new(404, "NOT_FOUND", message)
    }

    pub fn unavailable(message: &str) -> Self {
        Self::new(503, "UNAVAILABLE", message)
    }

    pub fn rate_limited(wait: Duration) -> Self {
        let mut e = Self::new(429, "RATE_LIMITED", "flood limit reached; nothing was sent");
        e.retry_after = Some(wait.as_secs().max(1));
        e.details = Some(json!({"retryAfterMillis": wait.as_millis() as u64}));
        e
    }

    pub fn body(&self) -> Value {
        let mut error = json!({"code": self.code, "message": self.message});
        if let Some(details) = &self.details {
            error["details"] = details.clone();
        }
        json!({"error": error})
    }
}

fn price_error(e: PriceError) -> ApiError {
    match e {
        PriceError::Malformed(message) => ApiError::bad(message),
        PriceError::NotOnLadder => ApiError::new(
            400,
            "PRICE_NOT_ON_LADDER",
            "the price is not a row of this market's ladder; nothing is rounded",
        ),
    }
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn parse_id(text: &str, name: &str) -> Result<u64, ApiError> {
    let ok = !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit());
    ok.then(|| text.parse().ok()).flatten().ok_or_else(|| ApiError::bad(format!("{name} must be a decimal string")))
}

fn id_field(body: &Map<String, Value>, name: &str) -> Result<u64, ApiError> {
    parse_id(body.get(name).and_then(Value::as_str).unwrap_or(""), name)
}

fn optional_id(body: &Map<String, Value>, name: &str) -> Result<Option<u64>, ApiError> {
    body.get(name).map(|_| id_field(body, name)).transpose()
}

fn integer(body: &Map<String, Value>, name: &str, min: u64, max: u64) -> Result<Option<u64>, ApiError> {
    body.get(name)
        .map(|v| v.as_u64().filter(|n| (min..=max).contains(n)))
        .map(|n| n.ok_or_else(|| ApiError::bad(format!("{name} must be an integer from {min} to {max}"))))
        .transpose()
}

fn flag(body: &Map<String, Value>, name: &str) -> Result<bool, ApiError> {
    match body.get(name) {
        None => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(ApiError::bad(format!("{name} must be true or false"))),
    }
}

/// The one of `allowed` a field holds, or `default` when it is absent.
fn choice(
    body: &Map<String, Value>,
    name: &str,
    allowed: &[&'static str],
    default: &'static str,
) -> Result<&'static str, ApiError> {
    match body.get(name) {
        None => Ok(default),
        Some(v) => allowed
            .iter()
            .find(|a| v.as_str() == Some(**a))
            .copied()
            .ok_or_else(|| ApiError::bad(format!("{name} must be one of {}", allowed.join(", ")))),
    }
}

/// Refuses any field the schema does not allow (`additionalProperties: false`).
pub fn only<'b>(body: &'b Value, allowed: &[&str]) -> Result<&'b Map<String, Value>, ApiError> {
    let fields = body.as_object().ok_or_else(|| ApiError::bad("the body must be a JSON object"))?;
    match fields.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(ApiError::bad(format!("unknown field {k}"))),
        None => Ok(fields),
    }
}

const ORDER_FIELDS: [&str; 15] = [
    "clientOrderId",
    "instrumentId",
    "action",
    "outcome",
    "price",
    "quantity",
    "stakeCents",
    "timeInForce",
    "expiresAt",
    "postOnly",
    "reduceOnly",
    "protectionTicks",
    "spendCapCents",
    "clientOrderGroup",
    "atEventStart",
];

const MAX_CENTS: u64 = i64::MAX as u64;

pub struct Gateway {
    hub: Mutex<Hub>,
    /// Pipelined, so no client waits on another's round trip to the node.
    ingress: Submit,
    pub limits: Limits,
    rest_buckets: Mutex<BTreeMap<u64, Bucket>>,
    logon_buckets: Mutex<BTreeMap<u64, Bucket>>,
}

impl Gateway {
    pub fn new(hub: Hub, ingress: Submit, limits: Limits) -> Self {
        Self {
            hub: Mutex::new(hub),
            ingress,
            limits,
            rest_buckets: Mutex::new(BTreeMap::new()),
            logon_buckets: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn hub(&self) -> MutexGuard<'_, Hub> {
        locked(&self.hub)
    }

    pub fn authenticate(&self, token: &str) -> Result<u64, ApiError> {
        self.hub().account_for(token).ok_or_else(ApiError::unauthorized)
    }

    /// REST requests have no session, so their flood limit is per account.
    pub fn rest_rate(&self, account: u64) -> Result<(), ApiError> {
        let mut buckets = locked(&self.rest_buckets);
        let bucket =
            buckets.entry(account).or_insert_with(|| Bucket::per_second(self.limits.per_second, self.limits.burst));
        bucket.take(monotonic()).map_err(ApiError::rate_limited)
    }

    pub fn logon_rate(&self, account: u64) -> Result<(), ApiError> {
        let mut buckets = locked(&self.logon_buckets);
        let rate = self.limits.logons_per_minute;
        let bucket = buckets.entry(account).or_insert_with(|| Bucket::per_minute(rate, rate));
        bucket.take(monotonic()).map_err(ApiError::rate_limited)
    }

    fn send(&self, input: Value) -> Result<Vec<Value>, ApiError> {
        self.ingress.send(input).map_err(|e| {
            eprintln!("gateway: ingress: {e}");
            ApiError::unavailable(
                "the sequencer group did not answer; the outcome is unknown, so resend with the same client order ID",
            )
        })
    }

    /// POST /v4/orders and the WebSocket `order` op.
    pub fn place(&self, account: u64, session: u64, body: &Value) -> Result<Value, ApiError> {
        let fields = only(body, &ORDER_FIELDS)?;
        let client_order_id = fields
            .get("clientOrderId")
            .and_then(Value::as_str)
            .filter(|s| (1..=32).contains(&s.len()) && s.bytes().all(|b| (0x21..=0x7e).contains(&b)))
            .ok_or_else(|| ApiError::bad("clientOrderId must be 1 to 32 printable ASCII characters"))?;
        let instrument = id_field(fields, "instrumentId")?;
        let buy = choice(fields, "action", &["buy", "sell"], "")? == "buy";
        if !fields.contains_key("action") {
            return Err(ApiError::bad("action is required"));
        }
        let yes = match fields.get("outcome").and_then(Value::as_str) {
            Some("yes") => true,
            Some("no") => false,
            _ => return Err(ApiError::bad("outcome must be yes or no")),
        };
        let wanted = price::parse(fields.get("price").unwrap_or(&Value::Null)).map_err(price_error)?;
        let quantity = integer(fields, "quantity", 1, u64::from(u32::MAX))?;
        let stake = integer(fields, "stakeCents", 1, MAX_CENTS)?;
        let time_in_force = choice(fields, "timeInForce", &["GTC", "GTD", "IOC", "FOK"], "GTC")?;
        let expires_at = match (time_in_force, fields.get("expiresAt")) {
            ("GTD", Some(v)) => Some(
                v.as_str()
                    .and_then(price::parse_timestamp)
                    .ok_or_else(|| ApiError::bad("expiresAt must be an RFC 3339 timestamp"))?,
            ),
            ("GTD", None) => return Err(ApiError::bad("expiresAt is required for GTD")),
            (_, Some(_)) => return Err(ApiError::bad("expiresAt is only for GTD")),
            _ => None,
        };
        let post_only = flag(fields, "postOnly")?;
        let reduce_only = flag(fields, "reduceOnly")?;
        let protection = integer(fields, "protectionTicks", 0, 255)?.unwrap_or(0);
        let spend_cap = integer(fields, "spendCapCents", 1, MAX_CENTS)?;
        let group = integer(fields, "clientOrderGroup", 0, u64::from(u32::MAX))?;
        let at_event_start = match choice(fields, "atEventStart", &["cancel", "keep"], "cancel")? {
            "keep" => "KEEP",
            _ => "CANCEL",
        };
        if !buy && (stake.is_some() || spend_cap.is_some()) {
            return Err(ApiError::bad("stakeCents and spendCapCents are for buys only"));
        }
        let (ladder, lot) = self.hub().market(instrument).ok_or_else(|| ApiError::not_found("no such market"))?;
        let row = price::row_of(&ladder, &wanted).map_err(price_error)?;
        let chosen = &ladder[row as usize];
        let outcome_price = price::ratio(chosen).ok_or_else(|| price_error(PriceError::NotOnLadder))?;
        let contracts = match (quantity, stake) {
            (Some(q), None) => q,
            (None, Some(s)) => price::contracts_for_stake(s, outcome_price, lot).map_err(|(step, per_step)| {
                let mut e = ApiError::new(
                    400,
                    "STAKE_NOT_ON_STEP",
                    format!("at this price a stake must be a multiple of {step} cents"),
                );
                e.details = Some(json!({"stepCents": step as u64, "contractsPerStep": per_step as u64}));
                e
            })?,
            _ => return Err(ApiError::bad("give exactly one of quantity and stakeCents")),
        };
        if contracts > u64::from(u32::MAX) {
            return Err(ApiError::bad("the order is too large"));
        }
        // The journal is in YES terms: buying NO is selling YES at the complementary tick (spec E-2.5, E-2.7).
        let tick = if yes { row } else { chosen.complement };
        let (side, entered) = match (buy, yes) {
            (true, true) => ("BUY", "YES"),
            (true, false) => ("SELL", "NO"),
            (false, true) => ("SELL", "YES"),
            (false, false) => ("BUY", "NO"),
        };
        let flags: Vec<&str> =
            [("POST_ONLY", post_only), ("REDUCE_ONLY", reduce_only)].into_iter().filter(|f| f.1).map(|f| f.0).collect();
        let input = wire::order(
            account,
            client_order_id.to_string(),
            json!({
                "sessionId": session, "instrumentId": instrument, "side": side, "enteredContract": entered,
                "tick": tick, "quantity": contracts, "timeInForce": time_in_force, "expiresAt": expires_at,
                "atEventStart": at_event_start, "flags": flags, "protectionTicks": protection,
                "spendCapCents": spend_cap, "enteredStakeCents": stake, "clientOrderGroup": group
            }),
        );
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// PATCH /v4/orders/{orderId} and the WebSocket `amend` op.
    pub fn amend(&self, account: u64, session: u64, order: u64, body: &Value) -> Result<Value, ApiError> {
        let fields = only(body, &["price", "outcome", "quantity", "atEventStart"])?;
        let (instrument, ladder) = {
            let hub = self.hub();
            let (_, instrument) = hub
                .order(order)
                .filter(|(owner, _)| *owner == account)
                .ok_or_else(|| ApiError::not_found("no such resting order"))?;
            (instrument, hub.market(instrument).map(|(ladder, _)| ladder).unwrap_or_default())
        };
        let yes = choice(fields, "outcome", &["yes", "no"], "yes")? == "yes";
        let tick = match fields.get("price") {
            Some(p) => {
                let row = price::row_of(&ladder, &price::parse(p).map_err(price_error)?).map_err(price_error)?;
                Some(if yes { row } else { ladder[row as usize].complement })
            }
            None => None,
        };
        let quantity = integer(fields, "quantity", 1, u64::from(u32::MAX))?;
        let at_event_start = match fields.get("atEventStart") {
            None => "UNCHANGED",
            Some(_) => match choice(fields, "atEventStart", &["cancel", "keep"], "cancel")? {
                "keep" => "KEEP",
                _ => "CANCEL",
            },
        };
        if tick.is_none() && quantity.is_none() && at_event_start == "UNCHANGED" {
            return Err(ApiError::bad("give at least one of price, quantity and atEventStart"));
        }
        let input = json!({
            "msg": "AmendOrder", "sessionId": session, "accountId": account, "instrumentId": instrument,
            "orderId": order, "newTick": tick, "newQuantity": quantity, "atEventStart": at_event_start
        });
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// DELETE /v4/orders/{orderId} and the WebSocket `cancel` op.
    pub fn cancel(&self, account: u64, session: u64, order: u64) -> Result<Value, ApiError> {
        let (_, instrument) = self
            .hub()
            .order(order)
            .filter(|(owner, _)| *owner == account)
            .ok_or_else(|| ApiError::not_found("no such resting order"))?;
        let mut input = wire::cancel(account, instrument, order);
        input["sessionId"] = json!(session);
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// DELETE /v4/orders and the WebSocket `cancelAll` op.
    pub fn cancel_all(&self, account: u64, session: u64, body: &Value) -> Result<Value, ApiError> {
        let fields = only(body, &["instrumentId", "eventId"])?;
        let (scope, reference) = match (optional_id(fields, "instrumentId")?, optional_id(fields, "eventId")?) {
            (None, None) => ("ACCOUNT", 0),
            (Some(instrument), None) => ("INSTRUMENT", instrument),
            (None, Some(event)) => ("EVENT", event),
            _ => return Err(ApiError::bad("give at most one of instrumentId and eventId")),
        };
        self.mass_cancel(account, session, scope, reference, "CLIENT_REQUEST")
    }

    /// Cancel-on-disconnect and missed heartbeats, keyed to the client session (blueprint §5).
    pub fn cancel_session(&self, account: u64, session: u64, cause: &str) -> Result<Value, ApiError> {
        self.mass_cancel(account, session, "SESSION", 0, cause)
    }

    fn mass_cancel(
        &self,
        account: u64,
        session: u64,
        scope: &str,
        reference: u64,
        cause: &str,
    ) -> Result<Value, ApiError> {
        let input = json!({
            "msg": "MassCancel", "sessionId": session, "accountId": account, "scope": scope, "scopeRef": reference,
            "cause": cause
        });
        let results = self.send(input)?;
        let hub = self.hub();
        let sequence = results.iter().find_map(|r| r["result"]["inputSequence"].as_u64()).unwrap_or(hub.view.sequence);
        let executions: Vec<Value> = hub::executions(&hub.view, &results, sequence)
            .into_iter()
            .filter(|(a, _)| *a == account)
            .map(|(_, e)| e)
            .collect();
        Ok(json!({"sequence": id(sequence), "executions": executions}))
    }

    /// An `OrderResult` from one input's results.
    fn result(&self, account: u64, results: &[Value]) -> Value {
        let mut hub = self.hub();
        let sequence = results.iter().find_map(|r| r["result"]["inputSequence"].as_u64()).unwrap_or(0);
        let executions: Vec<Value> = hub::executions(&hub.view, results, sequence)
            .into_iter()
            .filter(|(a, _)| *a == account)
            .map(|(_, e)| e)
            .collect();
        // The client can amend or cancel as soon as it has this reply, which reaches it before the feed does.
        for r in results.iter().filter(|r| r["msg"] == "OrderAccepted") {
            let field = |name: &str| r[name].as_u64().unwrap_or(0);
            hub.accept(field("orderId"), sequence, field("accountId"), field("instrumentId"));
        }
        let outcome = results.iter().find_map(|r| {
            let order = || id(r["orderId"].as_u64().unwrap_or(0));
            match r["msg"].as_str()? {
                "OrderAccepted" => Some(("accepted", order(), Value::Null)),
                "OrderAmended" => Some(("amended", order(), Value::Null)),
                "OrderCancelled" => Some(("cancelled", order(), Value::Null)),
                "OrderRejected" if r["reason"] == "DUPLICATE_CLIENT_ORDER_ID" => Some((
                    "duplicate",
                    r["originalOrderId"].as_u64().map_or(Value::Null, id),
                    json!("duplicate_client_order_id"),
                )),
                "OrderRejected" => Some(("rejected", Value::Null, hub::lower(&r["reason"]))),
                _ => None,
            }
        });
        let (status, order, reason) = outcome.unwrap_or(("rejected", Value::Null, json!("no_outcome")));
        json!({"status": status, "sequence": id(sequence), "orderId": order, "rejectReason": reason, "executions": executions})
    }
}
