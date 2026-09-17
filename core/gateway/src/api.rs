//! Order entry shared by REST and WebSocket: total validation, price and stake conversion, and the sequenced outcome
//! as an `OrderResult` (interfaces/gateway/openapi.yaml).

use crate::audit::Audit;
use crate::hub::{self, id, Hub};
use crate::identity::{Credential, Identity};
use crate::limits::{monotonic, Bucket};
use crate::parlays::{self, Parlays};
use crate::price::{self, PriceError};
use crate::treasury::Treasury;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;
use venue_identity::key;
use venue_link::submit::{self, Notify, Submit};
use venue_link::wire;

/// What a caller is told when the node did not answer: the outcome is genuinely unknown, so the only safe advice
/// is to send it again under the same client order ID, which the engine deduplicates (E-9.2, E-17.4).
fn unavailable() -> ApiError {
    ApiError::unavailable(
        "the sequencer group did not answer; the outcome is unknown, so resend with the same client order ID",
    )
}

/// The input for a mass cancel of one account's orders in `scope` (spec E-12.6).
fn mass_cancel_input(account: u64, session: u64, scope: &str, reference: u64, cause: &str) -> Value {
    json!({
        "msg": "MassCancel", "sessionId": session, "accountId": account, "scope": scope, "scopeRef": reference,
        "cause": cause
    })
}

/// What a request's results are made into for the client.
#[derive(Clone, Copy)]
enum Answer {
    /// An `OrderResult`: an order, an amendment or a cancel.
    Order,
    /// A mass cancel's sequence and executions.
    MassCancel,
}

/// A request given to the node or the parlay service and not answered yet, with the account whose result it will be.
pub struct Pending {
    account: u64,
    inner: Inner,
}

enum Inner {
    /// Given to the node, with what kind of answer its results make.
    Node(Answer, submit::Pending),
    /// Given to the parlay service, which answers in interface 1's terms already.
    Parlays(parlays::Pending),
}

#[derive(Clone, Copy)]
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

    /// A credential that was refused for a reason its holder may know, such as a revoked key.
    pub fn unauthorized_because(message: &str) -> Self {
        Self::new(401, "UNAUTHORIZED", message)
    }

    /// A valid credential without the permission a request needs.
    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(403, "FORBIDDEN", message)
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

/// A request for quote's ID, as the parlay service chooses them.
fn request_id(text: &str) -> Result<&str, ApiError> {
    let ok = (1..=32).contains(&text.len()) && text.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase());
    ok.then_some(text).ok_or_else(|| ApiError::bad("requestId must be 1 to 32 digits and lower-case letters"))
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

/// The two halves of interface 1 (interfaces/gateway/README.md, public and private), told apart by path alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Half {
    /// Market data for everyone: no login, and never a request to the node.
    Public,
    /// One account's orders and everything that happens to them.
    Private,
}

impl Half {
    /// The half's name, as the audit log gives it.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }

    /// Where this half's WebSocket is served.
    pub const fn websocket(self) -> &'static str {
        match self {
            Self::Public => "/v4/ws_public",
            Self::Private => "/v4/ws_private",
        }
    }

    /// The half a request path belongs to, or `None` for one both halves answer (`/healthz`) or neither does.
    pub fn of(path: &str) -> Option<Self> {
        let path = path.split('?').next().unwrap_or("");
        match path {
            "/v4/ws_public" | "/v4/markets" => Some(Self::Public),
            "/v4/ws_private" | "/v4/account" | "/v4/orders" | "/v4/transfers" | "/v4/rfqs" | "/v4/keys" => {
                Some(Self::Private)
            }
            p if p.starts_with("/v4/markets/") => Some(Self::Public),
            p if ["/v4/orders/", "/v4/transfers/", "/v4/rfqs/", "/v4/keys/"]
                .iter()
                .any(|under| p.starts_with(under)) =>
            {
                Some(Self::Private)
            }
            _ => None,
        }
    }
}

pub struct Gateway {
    pub half: Half,
    hub: Mutex<Hub>,
    /// Pipelined, so no client waits on another's round trip to the node. The public half has none: nothing it
    /// serves ever reaches the node (blueprint §9).
    ingress: Option<Submit>,
    /// Where requests for quote go (`parlays.rs`), for a private half given one.
    parlays: OnceLock<Parlays>,
    pub limits: Limits,
    rest_buckets: Mutex<BTreeMap<u64, Bucket>>,
    logon_buckets: Mutex<BTreeMap<u64, Bucket>>,
    /// Where deposits and withdrawals go, when this private half offers them.
    treasury: Option<Treasury>,
    /// Where what clients sent and were answered is recorded (`audit.rs`).
    pub audit: Audit,
    /// Who a credential is (`identity.rs`): dev tokens, and API keys through the identity service.
    pub identity: Identity,
}

impl Gateway {
    /// The public half: market data from `hub`, which follows the node's feed.
    pub fn public(hub: Hub, limits: Limits) -> Self {
        Self::new(Half::Public, hub, None, limits)
    }

    /// The private half: orders to the node through `ingress`, and each account's messages from `hub`.
    pub fn private(hub: Hub, ingress: Submit, limits: Limits) -> Self {
        Self::new(Half::Private, hub, Some(ingress), limits)
    }

    fn new(half: Half, hub: Hub, ingress: Option<Submit>, limits: Limits) -> Self {
        Self {
            half,
            hub: Mutex::new(hub),
            ingress,
            parlays: OnceLock::new(),
            limits,
            rest_buckets: Mutex::new(BTreeMap::new()),
            logon_buckets: Mutex::new(BTreeMap::new()),
            treasury: None,
            audit: Audit::disabled(),
            identity: Identity::new(None, true),
        }
    }

    /// The same half, recording what its clients send and are answered in `audit`.
    pub fn with_audit(mut self, audit: Audit) -> Self {
        self.audit = audit;
        self
    }

    /// The same half, passing deposits and withdrawals to the treasury at `address`.
    pub fn with_treasury(mut self, address: &str) -> Self {
        self.treasury = Some(Treasury::new(address));
        self
    }

    /// The same half, verifying API keys with the identity service at `address`, if any, and taking the catalogue's
    /// dev tokens only if `dev_tokens`.
    pub fn with_identity(mut self, address: Option<String>, dev_tokens: bool) -> Self {
        self.identity = Identity::new(address, dev_tokens);
        self
    }

    /// The treasury. The caller has checked the credential's permission for what it asks (`rest.rs`).
    pub fn treasury(&self) -> Result<&Treasury, ApiError> {
        self.treasury
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("this gateway has no treasury; transfers are not offered"))
    }

    /// The node's ingress, which only the private half has.
    fn ingress(&self) -> Result<&Submit, ApiError> {
        self.ingress.as_ref().ok_or_else(|| ApiError::not_found("orders are served on the private half"))
    }

    pub fn hub(&self) -> MutexGuard<'_, Hub> {
        locked(&self.hub)
    }

    /// Who a credential is: an API key's, by the identity service (interfaces/gateway/README.md, API keys and
    /// permissions), or a dev token's, by the catalogue, holding every permission.
    pub fn authenticate(&self, token: &str) -> Result<Credential, ApiError> {
        if key::looks_like_key(token) {
            return self.identity.verify_key(token);
        }
        if !self.identity.dev_tokens {
            return Err(ApiError::unauthorized());
        }
        self.hub().account_for(token).map(Credential::dev_token).ok_or_else(ApiError::unauthorized)
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

    /// Hands an input to the node without waiting for it, so a client with several to send is not held to one at a
    /// time. The account travels with it because the results are only an answer once they have been made into one.
    /// `notify` is called once they can be taken, for a caller that waits on many requests at once.
    fn submit(&self, account: u64, input: Value, answer: Answer, notify: Option<Notify>) -> Result<Pending, ApiError> {
        let inner = self.ingress()?.submit_notifying(input, notify).map_err(|e| {
            eprintln!("gateway: ingress: {e}");
            unavailable()
        })?;
        Ok(Pending { account, inner: Inner::Node(answer, inner) })
    }

    /// The client's answer from a request's results.
    fn outcome(&self, account: u64, answer: Answer, results: &[Value]) -> Value {
        match answer {
            Answer::Order => self.result(account, results),
            Answer::MassCancel => self.mass_cancel_result(account, results),
        }
    }

    fn send(&self, input: Value) -> Result<Vec<Value>, ApiError> {
        self.ingress()?.send(input).map_err(|e| {
            eprintln!("gateway: ingress: {e}");
            unavailable()
        })
    }

    /// The `OrderResult` for a pending request, waiting however long the node takes. For when something must be
    /// answered in one go and cannot be left outstanding.
    #[cfg(test)]
    pub fn wait_for(&self, pending: Pending) -> Result<Value, ApiError> {
        match pending.inner {
            Inner::Node(answer, inner) => match inner.wait() {
                Ok(results) => Ok(self.outcome(pending.account, answer, &results)),
                Err(e) => {
                    eprintln!("gateway: ingress: {e}");
                    Err(unavailable())
                }
            },
            Inner::Parlays(inner) => self.parlay_answer(inner.wait()),
        }
    }

    /// The answer to a request the node has answered, or `None` while it has not. A caller that submitted with a
    /// notify asks again when it is told.
    pub fn finish(&self, pending: &Pending) -> Option<Result<Value, ApiError>> {
        Some(match &pending.inner {
            Inner::Node(answer, inner) => match inner.take()? {
                Ok(results) => Ok(self.outcome(pending.account, *answer, &results)),
                Err(e) => {
                    eprintln!("gateway: ingress: {e}");
                    Err(unavailable())
                }
            },
            Inner::Parlays(inner) => self.parlay_answer(inner.take()?),
        })
    }

    /// Connects this half to the parlay service at `addr`: requests for quote go there, and what it sends for an
    /// account's sessions is delivered to them. A half without one refuses requests for quote as unavailable.
    pub fn connect_parlays(gateway: &Arc<Self>, addr: &str) {
        let half = Arc::downgrade(gateway);
        let deliver: parlays::Deliver = Arc::new(move |accounts, message| {
            if let Some(gateway) = half.upgrade() {
                gateway.hub().deliver(accounts, message);
            }
        });
        let _ = gateway.parlays.set(Parlays::spawn(addr, deliver));
    }

    /// Gives a call to the parlay service for `account`, without waiting for its answer.
    fn ask_parlays(&self, account: u64, mut call: Value, notify: Option<Notify>) -> Result<Pending, ApiError> {
        let service = self.parlays.get().ok_or_else(|| {
            ApiError::unavailable("requests for quote are not served here: this gateway has no parlay service")
        })?;
        call["accountId"] = json!(account);
        let inner = service.call(call, notify).map_err(|e| {
            eprintln!("gateway: parlay service: {e}");
            ApiError::unavailable("the parlay service is not taking requests; nothing was requested")
        })?;
        Ok(Pending { account, inner: Inner::Parlays(inner) })
    }

    /// The client's answer from the parlay service's: its refusal is passed on as the edge refusal it is.
    fn parlay_answer(&self, answer: parlays::Answer) -> Result<Value, ApiError> {
        match answer {
            Ok(result) => Ok(self.hub().present(result)),
            Err((status, code, message)) => {
                let code = match code.as_str() {
                    "BAD_REQUEST" => "BAD_REQUEST",
                    "NOT_FOUND" => "NOT_FOUND",
                    _ => "UNAVAILABLE",
                };
                Err(ApiError::new(status, code, message))
            }
        }
    }

    /// POST /v4/rfqs and the WebSocket `requestQuote` op: the legs' shape is checked here, and whether they make a
    /// parlay by the service, which has the venue's picture to judge it by.
    pub fn request_quote_pending(
        &self,
        account: u64,
        body: &Value,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let fields = only(body, &["legs", "quantity"])?;
        let legs = fields
            .get("legs")
            .and_then(Value::as_array)
            .filter(|legs| (2..=6).contains(&legs.len()))
            .ok_or_else(|| ApiError::bad("legs must be a list of 2 to 6 legs"))?;
        let legs = legs
            .iter()
            .map(|leg| {
                let leg = only(leg, &["instrumentId", "outcome"])?;
                let yes = match leg.get("outcome").and_then(Value::as_str) {
                    Some("yes") => true,
                    Some("no") => false,
                    _ => return Err(ApiError::bad("a leg's outcome must be yes or no")),
                };
                Ok(json!({"instrumentId": id_field(leg, "instrumentId")?, "yes": yes}))
            })
            .collect::<Result<Vec<Value>, ApiError>>()?;
        let quantity = integer(fields, "quantity", 1, u64::from(u32::MAX))?
            .ok_or_else(|| ApiError::bad("quantity is required"))?;
        self.ask_parlays(account, json!({"op": "request", "legs": legs, "quantity": quantity}), notify)
    }

    /// GET /v4/rfqs/{requestId}: one of the account's own requests.
    pub fn rfq_pending(&self, account: u64, request: &str, notify: Option<Notify>) -> Result<Pending, ApiError> {
        let request = request_id(request)?;
        self.ask_parlays(account, json!({"op": "status", "requestId": request}), notify)
    }

    /// POST /v4/rfqs/{requestId}/quotes and the WebSocket `attachQuote` op.
    pub fn attach_quote_pending(
        &self,
        account: u64,
        body: &Value,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let fields = only(body, &["requestId", "orderId"])?;
        let request = request_id(fields.get("requestId").and_then(Value::as_str).unwrap_or(""))?;
        let order = id_field(fields, "orderId")?;
        self.ask_parlays(account, json!({"op": "quote", "requestId": request, "orderId": order}), notify)
    }

    /// The input for a new order, once it has passed every check the gateway makes.
    fn place_input(&self, account: u64, session: u64, body: &Value) -> Result<Value, ApiError> {
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
        Ok(input)
    }

    /// POST /v4/orders and the WebSocket `order` op.
    #[cfg(test)]
    pub fn place(&self, account: u64, session: u64, body: &Value) -> Result<Value, ApiError> {
        let input = self.place_input(account, session, body)?;
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// The same, given to the node without waiting for it: `finish` collects the result later, and `notify` is
    /// called once it can.
    pub fn place_pending(
        &self,
        account: u64,
        session: u64,
        body: &Value,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let input = self.place_input(account, session, body)?;
        self.submit(account, input, Answer::Order, notify)
    }

    /// The input for an amendment, once it has passed every check. The order is resolved here, before anything is
    /// sent, so that an amendment still lands on an order this account really has (see `Hub::accept`).
    fn amend_input(&self, account: u64, session: u64, order: u64, body: &Value) -> Result<Value, ApiError> {
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
        Ok(json!({
            "msg": "AmendOrder", "sessionId": session, "accountId": account, "instrumentId": instrument,
            "orderId": order, "newTick": tick, "newQuantity": quantity, "atEventStart": at_event_start
        }))
    }

    /// PATCH /v4/orders/{orderId} and the WebSocket `amend` op.
    #[cfg(test)]
    pub fn amend(&self, account: u64, session: u64, order: u64, body: &Value) -> Result<Value, ApiError> {
        let input = self.amend_input(account, session, order, body)?;
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// The same, given to the node without waiting for it: `finish` collects the result later.
    pub fn amend_pending(
        &self,
        account: u64,
        session: u64,
        order: u64,
        body: &Value,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let input = self.amend_input(account, session, order, body)?;
        self.submit(account, input, Answer::Order, notify)
    }

    /// The input for a cancellation, once the order has been resolved to this account's.
    fn cancel_input(&self, account: u64, session: u64, order: u64) -> Result<Value, ApiError> {
        let (_, instrument) = self
            .hub()
            .order(order)
            .filter(|(owner, _)| *owner == account)
            .ok_or_else(|| ApiError::not_found("no such resting order"))?;
        let mut input = wire::cancel(account, instrument, order);
        input["sessionId"] = json!(session);
        Ok(input)
    }

    /// DELETE /v4/orders/{orderId} and the WebSocket `cancel` op.
    #[cfg(test)]
    pub fn cancel(&self, account: u64, session: u64, order: u64) -> Result<Value, ApiError> {
        let input = self.cancel_input(account, session, order)?;
        let results = self.send(input)?;
        Ok(self.result(account, &results))
    }

    /// The same, given to the node without waiting for it.
    pub fn cancel_pending(
        &self,
        account: u64,
        session: u64,
        order: u64,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let input = self.cancel_input(account, session, order)?;
        self.submit(account, input, Answer::Order, notify)
    }

    /// The scope of a `cancelAll`: the whole account, one market, or one event.
    fn cancel_all_scope(body: &Value) -> Result<(&'static str, u64), ApiError> {
        let fields = only(body, &["instrumentId", "eventId"])?;
        match (optional_id(fields, "instrumentId")?, optional_id(fields, "eventId")?) {
            (None, None) => Ok(("ACCOUNT", 0)),
            (Some(instrument), None) => Ok(("INSTRUMENT", instrument)),
            (None, Some(event)) => Ok(("EVENT", event)),
            _ => Err(ApiError::bad("give at most one of instrumentId and eventId")),
        }
    }

    /// DELETE /v4/orders and the WebSocket `cancelAll` op, given to the node without waiting for it. Its answer is a
    /// mass cancel's, not an `OrderResult`.
    pub fn cancel_all_pending(
        &self,
        account: u64,
        session: u64,
        body: &Value,
        notify: Option<Notify>,
    ) -> Result<Pending, ApiError> {
        let (scope, reference) = Self::cancel_all_scope(body)?;
        let input = mass_cancel_input(account, session, scope, reference, "CLIENT_REQUEST");
        self.submit(account, input, Answer::MassCancel, notify)
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
        let results = self.send(mass_cancel_input(account, session, scope, reference, cause))?;
        Ok(self.mass_cancel_result(account, &results))
    }

    /// A mass cancel's answer: its sequence and the account's cancellations.
    fn mass_cancel_result(&self, account: u64, results: &[Value]) -> Value {
        let hub = self.hub();
        let sequence = results.iter().find_map(|r| r["result"]["inputSequence"].as_u64()).unwrap_or(hub.view.sequence);
        let executions: Vec<Value> = hub::executions(&hub.view, results, sequence)
            .into_iter()
            .filter(|(a, _)| *a == account)
            .map(|(_, e)| e)
            .collect();
        json!({"sequence": id(sequence), "executions": executions})
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

/// The request path end to end: order entry through `Gateway`, a real node's ingress, and that node's feed back into
/// the hub. A stub would say whatever the test told it; this says what the engine decides.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MAKER, MARKET, RETAIL};
    use std::sync::Arc;

    const SESSION: u64 = 42;

    /// A seeded node of the test's own, and a gateway that sends to it and follows its feed (`testing`).
    fn venue(name: &str) -> Arc<Gateway> {
        crate::testing::venue(name).gateway
    }

    fn order(client_order_id: &str, action: &str, tick: u64, quantity: u64) -> Value {
        json!({"clientOrderId": client_order_id, "instrumentId": MARKET.to_string(), "action": action,
               "outcome": "yes", "price": {"tick": tick}, "quantity": quantity})
    }

    fn order_id(result: &Value) -> u64 {
        result["orderId"].as_str().and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("no order ID in {result}"))
    }

    #[test]
    fn an_order_rests_and_its_resend_is_a_duplicate_of_it() {
        let gateway = venue("duplicate");
        let placed = gateway.place(MAKER, SESSION, &order("a", "buy", 4, 10)).expect("placed");
        assert_eq!(placed["status"], "accepted", "{placed}");
        let resent = gateway.place(MAKER, SESSION, &order("a", "buy", 4, 10)).expect("resent");
        assert_eq!(resent["status"], "duplicate", "{resent}");
        assert_eq!(resent["orderId"], placed["orderId"]);
        assert_eq!(resent["rejectReason"], "duplicate_client_order_id");
    }

    #[test]
    fn a_resting_order_can_be_amended_and_then_cancelled() {
        let gateway = venue("amend-cancel");
        let placed = gateway.place(MAKER, SESSION, &order("b", "buy", 4, 10)).expect("placed");
        let id = order_id(&placed);
        // Straight after the reply, before the feed need have delivered the order (see `Hub::accept`).
        let amended = gateway.amend(MAKER, SESSION, id, &json!({"quantity": 20})).expect("amended");
        assert_eq!(amended["status"], "amended", "{amended}");
        assert_eq!(order_id(&amended), id);
        let cancelled = gateway.cancel(MAKER, SESSION, id).expect("cancelled");
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        assert_eq!(order_id(&cancelled), id);
    }

    #[test]
    fn another_accounts_order_is_not_found() {
        let gateway = venue("ownership");
        let id = order_id(&gateway.place(MAKER, SESSION, &order("c", "buy", 4, 10)).expect("placed"));
        let amend = gateway.amend(RETAIL, SESSION, id, &json!({"quantity": 1})).expect_err("not the retail account's");
        assert_eq!(amend.status, 404);
        let cancel = gateway.cancel(RETAIL, SESSION, id).expect_err("not the retail account's");
        assert_eq!(cancel.status, 404);
    }

    #[test]
    fn the_engine_refuses_what_an_account_cannot_pay_for_or_fill() {
        let gateway = venue("engine-refusals");
        // 1 000 000 contracts at 99c is far beyond the retail account's 50 000 cents.
        let broke = gateway.place(RETAIL, SESSION, &order("d", "buy", 98, 1_000_000)).expect("answered");
        assert_eq!(broke["status"], "rejected", "{broke}");
        assert_eq!(broke["rejectReason"], "insufficient_sleeve");
        // Nothing rests on this fresh node, so a fill-or-kill buy has nothing to fill against.
        let mut fok = order("e", "buy", 50, 5);
        fok["timeInForce"] = json!("FOK");
        let unfilled = gateway.place(MAKER, SESSION, &fok).expect("answered");
        assert_eq!(unfilled["status"], "rejected", "{unfilled}");
        assert_eq!(unfilled["rejectReason"], "fok_not_fillable");
    }

    #[test]
    fn malformed_orders_are_refused_before_anything_is_sent() {
        let gateway = venue("malformed");
        let mut no_action = order("f", "buy", 4, 1);
        no_action.as_object_mut().expect("an object").remove("action");
        assert_eq!(gateway.place(MAKER, SESSION, &no_action).expect_err("no action").status, 400);
        let mut unknown = order("g", "buy", 4, 1);
        unknown["instrumentId"] = json!("99999");
        assert_eq!(gateway.place(MAKER, SESSION, &unknown).expect_err("no such market").status, 404);
        let off_ladder = order("h", "buy", 5_000, 1);
        assert_eq!(gateway.place(MAKER, SESSION, &off_ladder).expect_err("off the ladder").status, 400);
        // None of those reached the node, so the IDs are unused and can still place orders.
        let placed = gateway.place(MAKER, SESSION, &order("f", "buy", 4, 1)).expect("placed");
        assert_eq!(placed["status"], "accepted", "{placed}");
    }

    #[test]
    fn pipelined_orders_are_answered_in_the_order_they_were_sent() {
        let gateway = venue("pipelined");
        let pending: Vec<Pending> = (0..20)
            .map(|i| {
                gateway.place_pending(MAKER, SESSION, &order(&format!("p{i}"), "buy", 4, 1), None).expect("submitted")
            })
            .collect();
        let mut last = 0;
        for p in pending {
            let result = gateway.wait_for(p).expect("answered");
            assert_eq!(result["status"], "accepted", "{result}");
            let sequence: u64 = result["sequence"].as_str().and_then(|s| s.parse().ok()).expect("a sequence");
            assert!(sequence > last, "sequence {sequence} after {last}");
            last = sequence;
        }
    }
}
