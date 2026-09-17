//! The WebSocket half of interface 1 (interfaces/gateway/asyncapi.yaml). One connection is one client session.
//!
//! This is the protocol: what a session holds and how it answers what its client sends. It never touches a socket
//! and never waits: `reactor.rs` serves the connections, reads what arrives, writes what this returns, and comes
//! back when a session's `notify` says a reply or a message is waiting.

use crate::api::{only, parse_id, ApiError, Gateway, Half, Pending};
use crate::hub::{id, Outbox};
use crate::identity::Credential;
use crate::limits::{monotonic, Bucket};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use venue_identity::key::Permissions;
use venue_link::submit::Notify;

/// Requests one session may have outstanding at once. At this many, whatever serves the connection stops reading
/// from it until the oldest is answered, which is where one client is stopped from queueing work at every other
/// client's expense.
pub const MAX_IN_FLIGHT: usize = 64;

pub struct Session {
    key: u64,
    engine_session: u64,
    account: Option<u64>,
    /// What the session logged in with, and so what it may do.
    credential: Option<Credential>,
    cancel_on_disconnect: bool,
    heartbeat: Option<Duration>,
    last_heard: Instant,
    bucket: Bucket,
    /// Called once a reply to one of this session's requests can be taken.
    notify: Notify,
    /// Requests given to the node and not yet answered, oldest first, each with the id to answer it under. A node
    /// answers a connection in commit order, so the oldest is always the first to be ready and replies leave in
    /// the order they were asked for. A `subscribeRfqs` is marked: the requests its answer catches up with are sent
    /// as `rfq` messages ahead of its reply.
    in_flight: VecDeque<(Value, Pending, bool)>,
}

impl Session {
    /// A session registered with the hub, whose messages go to `outbox` and whose replies are signalled through
    /// `notify`. None if no session ID could be drawn.
    pub fn open(gateway: &Gateway, outbox: Outbox, notify: Notify) -> Option<Self> {
        let engine_session = session_id()?;
        let key = gateway.hub().add_session(outbox);
        Some(Self {
            key,
            engine_session,
            account: None,
            credential: None,
            cancel_on_disconnect: false,
            heartbeat: None,
            last_heard: monotonic(),
            bucket: Bucket::per_second(gateway.limits.per_second, gateway.limits.burst),
            notify,
            in_flight: VecDeque::new(),
        })
    }

    /// The account logged in on this session, once one has.
    pub const fn account(&self) -> Option<u64> {
        self.account
    }

    /// The API key the session logged in with, if it did with one.
    pub fn api_key(&self) -> Option<&str> {
        self.credential.as_ref().and_then(|c| c.key.as_deref())
    }

    /// Refuses unless the session is logged in with a credential holding `permission`, and says whose account it is.
    fn allowed(&self, permission: Permissions) -> Result<u64, ApiError> {
        let (Some(account), Some(credential)) = (self.account, &self.credential) else {
            return Err(ApiError::unauthorized());
        };
        credential.require(permission)?;
        Ok(account)
    }

    /// The engine session ID this session's orders carry.
    pub const fn id(&self) -> u64 {
        self.engine_session
    }

    /// The client said something, so it is still there.
    pub fn heard(&mut self) {
        self.last_heard = monotonic();
    }

    /// Whether this session may not take another request until one it has outstanding is answered.
    pub fn at_limit(&self) -> bool {
        self.in_flight.len() >= MAX_IN_FLIGHT
    }

    /// Whether a logged-in client has been silent for longer than the heartbeat it asked for.
    pub fn heartbeat_missed(&self, now: Instant) -> bool {
        match (self.heartbeat, self.account) {
            (Some(limit), Some(_)) => now.duration_since(self.last_heard) > limit,
            _ => false,
        }
    }
}

/// A session ID unique across gateway restarts, below 2^53 so every client holds it exactly. A mass cancel by session
/// must never reach an older session's orders (spec E-12.6).
fn session_id() -> Option<u64> {
    let mut bytes = [0u8; 8];
    File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).ok()?;
    Some((u64::from_le_bytes(bytes) & ((1 << 53) - 1)).max(1))
}

/// Cancels a session's orders. The cancel waits on the node, so it runs on a thread of its own rather than holding
/// up whatever serves every other connection. It still reaches the node after everything the session sent before
/// it, because those were all handed to the ingress queue first.
fn cancel_session_orders(gateway: &Arc<Gateway>, session: &Session, cause: &'static str) {
    let Some(account) = session.account else { return };
    let (gateway, engine_session) = (Arc::clone(gateway), session.engine_session);
    thread::spawn(move || {
        if let Err(e) = gateway.cancel_session(account, engine_session, cause) {
            eprintln!("gateway: {cause} cancel for session {engine_session}: {}", e.message);
        }
    });
}

/// The client has been silent past its heartbeat: its orders are cancelled whatever it said at logon, and the
/// connection is about to be closed.
pub fn heartbeat_expired(gateway: &Arc<Gateway>, session: &mut Session) {
    session.cancel_on_disconnect = false;
    cancel_session_orders(gateway, session, "HEARTBEAT_MISSED");
}

/// The connection has gone: the session leaves the hub and, if it asked at logon, its orders are cancelled.
pub fn end(gateway: &Arc<Gateway>, session: &Session) {
    gateway.hub().remove_session(session.key);
    gateway.end_rfq_subscription(session.key);
    if session.cancel_on_disconnect {
        cancel_session_orders(gateway, session, "SESSION_DISCONNECT");
    }
}

/// Whether the session is logged in with an API key that has since been revoked or has expired, by what the gateway
/// last heard from the identity service. Such a session is told and closed (interfaces/gateway/README.md, API keys
/// and permissions), which cancels its orders if it asked at logon.
pub fn key_withdrawn(gateway: &Gateway, session: &Session) -> bool {
    session.credential.as_ref().is_some_and(|c| !gateway.identity.still_valid(c))
}

/// What a session whose key was withdrawn is told before it is closed.
pub fn key_withdrawn_error() -> String {
    error(&Value::Null, &ApiError::unauthorized_because("the session's API key has been revoked or has expired"))
}

/// The replies whose results have arrived, oldest first, stopping at the first that has not.
pub fn ready_replies(gateway: &Gateway, session: &mut Session) -> Vec<String> {
    let mut out = Vec::new();
    // The result is taken before the request leaves the queue, so nothing is dropped between the two.
    while let Some(result) = session.in_flight.front().and_then(|(_, pending, _)| gateway.finish(pending)) {
        if let Some((id, _, catch_up)) = session.in_flight.pop_front() {
            let result = match result {
                Ok(mut value) if catch_up => {
                    let requests = value.as_object_mut().and_then(|v| v.remove("requests"));
                    for mut rfq in requests.and_then(|r| r.as_array().cloned()).unwrap_or_default() {
                        rfq["type"] = json!("rfq");
                        out.push(rfq.to_string());
                    }
                    Ok(value)
                }
                other => other,
            };
            out.push(answer(&id, result));
        }
    }
    out
}

fn answer(id: &Value, result: Result<Value, ApiError>) -> String {
    match result {
        Ok(value) => reply(id, value),
        Err(e) => error(id, &e),
    }
}

fn error(request_id: &Value, e: &ApiError) -> String {
    let mut message = Map::new();
    message.insert("type".into(), json!("error"));
    if !request_id.is_null() {
        message.insert("id".into(), request_id.clone());
    }
    message.insert("error".into(), e.body()["error"].clone());
    Value::Object(message).to_string()
}

fn reply(request_id: &Value, result: Value) -> String {
    let mut message = Map::new();
    message.insert("type".into(), json!("reply"));
    if !request_id.is_null() {
        message.insert("id".into(), request_id.clone());
    }
    message.insert("result".into(), result);
    Value::Object(message).to_string()
}

/// What a client is told when it sends a binary frame.
pub fn binary_refused() -> String {
    error(&Value::Null, &ApiError::bad("send JSON as text frames"))
}

/// The messages that answer one text frame straight away. Replies to requests given to the node come later, from
/// `ready_replies`.
pub fn handle(gateway: &Gateway, session: &mut Session, text: &str) -> Vec<String> {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return vec![error(&Value::Null, &ApiError::bad("not JSON"))];
    };
    let request_id = message.get("id").cloned().unwrap_or(Value::Null);
    if let Err(wait) = session.bucket.take(monotonic()) {
        return vec![error(&request_id, &ApiError::rate_limited(wait))];
    }
    respond(gateway, session, &message).unwrap_or_else(|e| vec![error(&request_id, &e)])
}

/// The request without `op` and `id`, as the REST body would be.
fn body_of(message: &Value) -> Value {
    let mut body = message.as_object().cloned().unwrap_or_default();
    body.remove("op");
    body.remove("id");
    Value::Object(body)
}

fn topics(message: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    let list = message.get("topics").and_then(Value::as_array).filter(|t| !t.is_empty() && t.len() <= 100);
    let list = list.ok_or_else(|| ApiError::bad("topics must be a list of 1 to 100 topics"))?;
    list.iter()
        .map(|t| {
            let t = t.as_str().unwrap_or("");
            let instrument = t.strip_prefix("book.").or_else(|| t.strip_prefix("trades."));
            let valid = t == "markets" || instrument.is_some_and(|i| parse_id(i, "instrumentId").is_ok());
            if valid {
                Ok(t.to_string())
            } else {
                Err(ApiError::bad(format!("unknown topic {t}")))
            }
        })
        .collect()
}

fn respond(gateway: &Gateway, session: &mut Session, message: &Value) -> Result<Vec<String>, ApiError> {
    let request_id = message.get("id").cloned().unwrap_or(Value::Null);
    let op = message.get("op").and_then(Value::as_str).ok_or_else(|| ApiError::bad("every message needs an op"))?;
    // Market data and order entry are separate endpoints, possibly separate gateways (README, public and private).
    let belongs = match op {
        "subscribe" | "unsubscribe" => Some(Half::Public),
        "login" | "order" | "amend" | "cancel" | "cancelAll" | "requestQuote" | "attachQuote" | "subscribeRfqs"
        | "unsubscribeRfqs" => Some(Half::Private),
        _ => None,
    };
    if let Some(half) = belongs.filter(|half| *half != gateway.half) {
        return Err(ApiError::bad(format!("{op} is served on {}", half.websocket())));
    }
    match op {
        "heartbeat" => {
            only(message, &["op", "id"])?;
            // Answered when it carries an id, so a client can tell a gateway that has gone from a quiet market.
            Ok(if request_id.is_null() { Vec::new() } else { vec![reply(&request_id, json!({}))] })
        }
        "login" => login(gateway, session, message),
        "subscribe" => {
            let fields = only(message, &["op", "id", "topics", "depth"])?;
            let topics = topics(fields)?;
            let depth = match fields.get("depth") {
                None => 20,
                Some(d) => d
                    .as_u64()
                    .filter(|d| (1..=100).contains(d))
                    .ok_or_else(|| ApiError::bad("depth must be from 1 to 100"))? as usize,
            };
            gateway.hub().subscribe(session.key, &topics, depth);
            Ok(Vec::new())
        }
        "unsubscribe" => {
            let topics = topics(only(message, &["op", "id", "topics"])?)?;
            gateway.hub().unsubscribe(session.key, &topics);
            Ok(Vec::new())
        }
        "order" | "amend" | "cancel" | "cancelAll" => {
            let account = session.allowed(Permissions::TRADE)?;
            let s = session.engine_session;
            let notify = Some(Arc::clone(&session.notify));
            let mut body = body_of(message);
            // Every one of these is answered when the node answers, never before and never out of turn: a client
            // with several to send is not held to one at a time, and a mass cancel is answered after the orders
            // sent before it, which reached the node before it too.
            let pending = match op {
                "order" => gateway.place_pending(account, s, &body, notify)?,
                "cancelAll" => gateway.cancel_all_pending(account, s, &body, notify)?,
                _ => {
                    let order = parse_id(body.get("orderId").and_then(Value::as_str).unwrap_or(""), "orderId")?;
                    if let Some(fields) = body.as_object_mut() {
                        fields.remove("orderId");
                    }
                    if op == "cancel" {
                        only(&body, &[])?;
                        gateway.cancel_pending(account, s, order, notify)?
                    } else {
                        gateway.amend_pending(account, s, order, &body, notify)?
                    }
                }
            };
            session.in_flight.push_back((request_id, pending, false));
            Ok(Vec::new())
        }
        "subscribeRfqs" | "unsubscribeRfqs" => {
            let account = session.allowed(Permissions::READ)?;
            let notify = Some(Arc::clone(&session.notify));
            let body = body_of(message);
            let subscribe = op == "subscribeRfqs";
            let pending = if subscribe {
                gateway.subscribe_rfqs_pending(account, session.key, &body, notify)?
            } else {
                gateway.unsubscribe_rfqs_pending(account, session.key, &body, notify)?
            };
            session.in_flight.push_back((request_id, pending, subscribe));
            Ok(Vec::new())
        }
        "requestQuote" | "attachQuote" => {
            let account = session.allowed(Permissions::TRADE)?;
            let notify = Some(Arc::clone(&session.notify));
            let body = body_of(message);
            // Answered in turn with the session's orders, so a responder that rests an order and then attaches it
            // hears about the order first.
            let pending = if op == "requestQuote" {
                gateway.request_quote_pending(account, &body, notify)?
            } else {
                gateway.attach_quote_pending(account, &body, notify)?
            };
            session.in_flight.push_back((request_id, pending, false));
            Ok(Vec::new())
        }
        _ => Err(ApiError::bad(format!("unknown op {op}"))),
    }
}

fn login(gateway: &Gateway, session: &mut Session, message: &Value) -> Result<Vec<String>, ApiError> {
    let fields =
        only(message, &["op", "id", "token", "cancelOnDisconnect", "heartbeatSeconds", "resumeFrom", "stream"])?;
    if session.account.is_some() {
        return Err(ApiError::bad("this session is already logged in"));
    }
    let credential = gateway.authenticate(fields.get("token").and_then(Value::as_str).unwrap_or(""))?;
    let account = credential.account;
    gateway.logon_rate(account)?;
    // A private session streams the account, which is reading it.
    credential.require(Permissions::READ)?;
    let cancel_on_disconnect = match fields.get("cancelOnDisconnect") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ApiError::bad("cancelOnDisconnect must be true or false")),
    };
    let heartbeat = match fields.get("heartbeatSeconds") {
        None => 0,
        Some(v) => {
            v.as_u64().filter(|s| *s <= 300).ok_or_else(|| ApiError::bad("heartbeatSeconds must be from 0 to 300"))?
        }
    };
    let resume_from = fields.get("resumeFrom").map(|v| parse_id(v.as_str().unwrap_or(""), "resumeFrom")).transpose()?;
    // `stream: false` once asked the gateway to wake rarely for this session, trading promptness for the cost of
    // polling its socket. Nothing polls now — every session is woken only when something is there — so the field
    // is still accepted and changes nothing.
    if !matches!(fields.get("stream"), None | Some(Value::Bool(_))) {
        return Err(ApiError::bad("stream must be true or false"));
    }
    session.account = Some(account);
    session.cancel_on_disconnect = cancel_on_disconnect;
    let (key, permissions) = (credential.key.clone(), credential.permissions.names());
    session.credential = Some(credential);
    session.heartbeat = (heartbeat > 0).then(|| Duration::from_secs(heartbeat));
    let (sequence, resumed) = {
        let mut hub = gateway.hub();
        hub.log_in(session.key, account);
        (hub.view.sequence, resume_from.map(|from| hub.since(account, from)))
    };
    let mut welcome = Map::new();
    welcome.insert("type".into(), json!("welcome"));
    if let Some(request_id) = message.get("id") {
        welcome.insert("id".into(), request_id.clone());
    }
    welcome.insert("accountId".into(), id(account));
    welcome.insert("sessionId".into(), id(session.engine_session));
    welcome.insert("sequence".into(), id(sequence));
    welcome.insert("resumed".into(), json!(matches!(resumed, Some(Some(_)))));
    if let Some(key) = key {
        welcome.insert("keyId".into(), json!(key));
    }
    welcome.insert("permissions".into(), json!(permissions));
    let mut out = vec![Value::Object(welcome).to_string()];
    match resumed {
        Some(Some(messages)) => out.extend(messages),
        Some(None) => out.push(json!({"type": "resync", "sequence": id(sequence)}).to_string()),
        None => {}
    }
    Ok(out)
}

/// What a WebSocket client can rely on, against a gateway on a real node (`testing`). These describe the behaviour,
/// not how connections are served, so they hold whichever way the gateway waits on its sockets.
#[cfg(test)]
mod tests {
    use crate::testing::{self, eventually, MAKER_TOKEN, MARKET, RESPONDER_TOKEN, RETAIL_TOKEN};
    use serde_json::{json, Value};
    use std::net::TcpStream;
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    struct Client {
        socket: WebSocket<MaybeTlsStream<TcpStream>>,
        /// Streamed messages (market data, private executions and account updates) seen while waiting for an
        /// answer, in arrival order.
        streamed: Vec<Value>,
    }

    impl Client {
        fn connect(url: &str) -> Self {
            let (socket, _) = tungstenite::connect(url).expect("the gateway accepts a WebSocket");
            // Long, because tests share a machine with other builds; it costs time only when an answer never comes.
            if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
                stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
            }
            Self { socket, streamed: Vec::new() }
        }

        fn send(&mut self, message: Value) {
            self.socket.send(Message::text(message.to_string())).expect("sent");
        }

        /// The next text message, failing the test if none arrives within the read timeout.
        fn next(&mut self) -> Value {
            loop {
                match self.socket.read().expect("a message before the timeout") {
                    Message::Text(text) => return serde_json::from_str(text.as_str()).expect("JSON"),
                    Message::Close(_) => panic!("the gateway closed the connection"),
                    _ => {}
                }
            }
        }

        /// The next message that answers something the client sent. Everything else a session is sent (market
        /// data, and for a logged-in session its own executions and account updates) arrives interleaved with the
        /// answers, and is kept for the test to look at.
        fn next_answer(&mut self) -> Value {
            loop {
                let m = self.next();
                if matches!(m["type"].as_str(), Some("reply" | "error" | "welcome" | "resync")) {
                    return m;
                }
                self.streamed.push(m);
            }
        }

        /// Waits for a private execution of `kind` for the order with `client_order_id`.
        fn execution(&mut self, client_order_id: &str, kind: &str) -> Value {
            self.streamed_message(|m| {
                m["type"] == "execution"
                    && m["execution"]["clientOrderId"] == client_order_id
                    && m["execution"]["type"] == kind
            })
        }

        /// Waits for a streamed message `wanted` accepts, whether it has already arrived or not.
        fn streamed_message(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
            loop {
                if let Some(found) = self.streamed.iter().find(|m| wanted(m)) {
                    return found.clone();
                }
                let m = self.next();
                self.streamed.push(m);
            }
        }

        /// The next book message that `wanted` accepts, skipping others.
        fn next_book(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
            loop {
                let m = self.next();
                if m["type"] == "book" && wanted(&m) {
                    return m;
                }
            }
        }

        /// Sends a call for the parlay service and returns its answer, asking again while the answer is that the service
        /// is unavailable: it refuses requests while catching up with the feed, which on a loaded machine it can fall
        /// behind for a moment.
        fn ask_parlays(&mut self, message: Value) -> Value {
            for _ in 0..300 {
                self.send(message.clone());
                let answer = self.next_answer();
                if answer["error"]["code"] != "UNAVAILABLE" {
                    return answer;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("the parlay service stayed unavailable for {message}");
        }

        /// Logs in as the account whose dev token this is.
        fn login_as(&mut self, token: &str) {
            self.send(json!({"op": "login", "id": "login", "token": token}));
            let welcome = self.next_answer();
            assert_eq!(welcome["type"], "welcome", "{welcome}");
        }

        fn login(&mut self, extra: Value) -> Value {
            let mut login = json!({"op": "login", "id": "login", "token": MAKER_TOKEN});
            if let (Some(login), Some(extra)) = (login.as_object_mut(), extra.as_object()) {
                login.extend(extra.clone());
            }
            self.send(login);
            let welcome = self.next_answer();
            assert_eq!(welcome["type"], "welcome", "{welcome}");
            welcome
        }

        /// Whether the gateway closes the connection within `seconds`.
        fn closed_within(&mut self, seconds: u64) -> bool {
            for _ in 0..seconds * 10 {
                if let MaybeTlsStream::Plain(stream) = self.socket.get_ref() {
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                }
                match self.socket.read() {
                    Ok(Message::Close(_)) => return true,
                    Ok(_) => {}
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                    Err(_) => return true,
                }
            }
            false
        }
    }

    fn order(id: &str, tick: u64) -> Value {
        json!({"op": "order", "id": id, "clientOrderId": id, "instrumentId": MARKET.to_string(), "action": "buy",
               "outcome": "yes", "price": {"tick": tick}, "quantity": 1})
    }

    fn order_id(reply: &Value) -> String {
        reply["result"]["orderId"].as_str().unwrap_or_else(|| panic!("no order ID in {reply}")).to_string()
    }

    #[test]
    fn login_welcomes_the_account_and_a_bad_token_is_refused() {
        let venue = testing::venue("ws-login");
        let mut client = Client::connect(&venue.ws_private);
        client.send(json!({"op": "login", "id": "bad", "token": "no-such-token"}));
        let refused = client.next_answer();
        assert_eq!(refused["type"], "error", "{refused}");
        assert_eq!(refused["id"], "bad");
        // `stream` is deprecated and still accepted.
        let welcome = client.login(json!({"stream": false}));
        assert_eq!(welcome["id"], "login");
        assert_eq!(welcome["accountId"], "1");
        assert!(welcome["sessionId"].as_str().is_some_and(|s| !s.is_empty()), "{welcome}");
    }

    #[test]
    fn orders_amendments_and_cancels_are_answered_under_their_ids() {
        let venue = testing::venue("ws-order");
        let mut client = Client::connect(&venue.ws_private);
        client.login(json!({}));
        client.send(order("o1", 4));
        let placed = client.next_answer();
        assert_eq!((placed["type"].as_str(), placed["id"].as_str()), (Some("reply"), Some("o1")), "{placed}");
        assert_eq!(placed["result"]["status"], "accepted");
        let id = order_id(&placed);
        // The session is also told privately, as an execution, whichever of the two it happens to see first.
        let accepted = client.execution("o1", "accepted");
        assert_eq!(accepted["execution"]["orderId"], id.as_str(), "{accepted}");
        client.send(json!({"op": "amend", "id": "a1", "orderId": id, "quantity": 3}));
        let amended = client.next_answer();
        assert_eq!((amended["id"].as_str(), amended["result"]["status"].as_str()), (Some("a1"), Some("amended")));
        client.send(json!({"op": "cancel", "id": "c1", "orderId": id}));
        let cancelled = client.next_answer();
        assert_eq!((cancelled["id"].as_str(), cancelled["result"]["status"].as_str()), (Some("c1"), Some("cancelled")));
    }

    #[test]
    fn a_burst_sent_without_waiting_is_answered_in_the_order_it_was_sent() {
        let venue = testing::venue("ws-burst");
        let mut client = Client::connect(&venue.ws_private);
        client.login(json!({}));
        // More than a session may have outstanding at once, so the gateway has to hold some back and still keep order.
        let ids: Vec<String> = (0..100).map(|i| format!("b{i}")).collect();
        for id in &ids {
            client.send(order(id, 4));
        }
        for id in &ids {
            let reply = client.next_answer();
            assert_eq!(reply["id"].as_str(), Some(id.as_str()), "{reply}");
            assert_eq!(reply["result"]["status"], "accepted", "{reply}");
        }
    }

    #[test]
    fn cancel_all_is_answered_after_the_orders_sent_before_it() {
        let venue = testing::venue("ws-cancel-all");
        let mut client = Client::connect(&venue.ws_private);
        client.login(json!({}));
        for i in 0..10 {
            client.send(order(&format!("m{i}"), 4));
        }
        client.send(json!({"op": "cancelAll", "id": "all"}));
        for i in 0..10 {
            let reply = client.next_answer();
            assert_eq!(reply["id"].as_str(), Some(format!("m{i}").as_str()), "{reply}");
        }
        let all = client.next_answer();
        assert_eq!((all["type"].as_str(), all["id"].as_str()), (Some("reply"), Some("all")), "{all}");
        assert_eq!(all["result"]["executions"].as_array().map(Vec::len), Some(10), "{all}");
    }

    #[test]
    fn a_book_subscription_starts_with_a_snapshot_and_then_carries_changes() {
        let venue = testing::venue("ws-book");
        let mut watcher = Client::connect(&venue.ws_public);
        watcher.send(json!({"op": "subscribe", "id": "s", "topics": [format!("book.{MARKET}")]}));
        let image = watcher.next_book(|_| true);
        assert_eq!(image["snapshot"], true, "{image}");
        let mut trader = Client::connect(&venue.ws_private);
        trader.login(json!({}));
        trader.send(order("t1", 7));
        assert_eq!(trader.next_answer()["result"]["status"], "accepted");
        let change = watcher.next_book(|m| {
            m["snapshot"] == false
                && m["changes"]
                    .as_array()
                    .is_some_and(|c| c.iter().any(|l| l["side"] == "buy" && l["price"]["tick"] == 7))
        });
        assert_eq!(change["instrumentId"], MARKET.to_string(), "{change}");
        // The trader's own session carries its execution, and nothing public, however long it has been open.
        trader.send(json!({"op": "heartbeat", "id": "h"}));
        assert_eq!(trader.next_answer()["id"], "h");
        trader.execution("t1", "accepted");
        let public: Vec<&Value> = trader
            .streamed
            .iter()
            .filter(|m| matches!(m["type"].as_str(), Some("book" | "trade" | "market")))
            .collect();
        assert!(public.is_empty(), "a private session was sent market data: {public:?}");
    }

    #[test]
    fn each_endpoint_refuses_the_other_ones_ops_and_says_where_they_go() {
        let venue = testing::venue("ws-halves");
        let mut public = Client::connect(&venue.ws_public);
        for op in [json!({"op": "login", "id": "l", "token": MAKER_TOKEN}), order("p1", 4)] {
            public.send(op);
            let refused = public.next_answer();
            assert_eq!(refused["type"], "error", "{refused}");
            assert!(refused["error"]["message"].as_str().is_some_and(|m| m.contains("/v4/ws_private")), "{refused}");
        }
        public.send(json!({"op": "heartbeat", "id": "hp"}));
        assert_eq!(public.next_answer()["id"], "hp");
        let mut private = Client::connect(&venue.ws_private);
        private.login(json!({}));
        private.send(json!({"op": "subscribe", "id": "s", "topics": [format!("book.{MARKET}")]}));
        let refused = private.next_answer();
        assert_eq!((refused["type"].as_str(), refused["id"].as_str()), (Some("error"), Some("s")), "{refused}");
        assert!(refused["error"]["message"].as_str().is_some_and(|m| m.contains("/v4/ws_public")), "{refused}");
        // The session carries on after being refused.
        private.send(order("p2", 4));
        assert_eq!(private.next_answer()["result"]["status"], "accepted");
    }

    #[test]
    fn a_heartbeat_is_answered_and_a_missed_one_cancels_and_closes() {
        let venue = testing::venue("ws-heartbeat");
        let mut client = Client::connect(&venue.ws_private);
        client.login(json!({"heartbeatSeconds": 1}));
        client.send(json!({"op": "heartbeat", "id": "h"}));
        let beat = client.next_answer();
        assert_eq!((beat["type"].as_str(), beat["id"].as_str()), (Some("reply"), Some("h")), "{beat}");
        client.send(order("hb1", 4));
        let id: u64 = order_id(&client.next_answer()).parse().expect("a numeric order ID");
        // Silence past the heartbeat: the gateway pulls the session's orders and closes the connection.
        assert!(client.closed_within(5), "the gateway kept a silent session past its heartbeat");
        eventually("the silent session's order is cancelled", || venue.gateway.hub().order(id).is_none());
    }

    #[test]
    fn disconnecting_pulls_orders_only_when_the_session_asked() {
        let venue = testing::venue("ws-disconnect");
        let mut pulled = Client::connect(&venue.ws_private);
        pulled.login(json!({"cancelOnDisconnect": true}));
        pulled.send(order("d1", 4));
        let pulled_id: u64 = order_id(&pulled.next_answer()).parse().expect("a numeric order ID");
        let mut kept = Client::connect(&venue.ws_private);
        kept.send(json!({"op": "login", "id": "login", "token": RETAIL_TOKEN}));
        assert_eq!(kept.next_answer()["type"], "welcome");
        kept.send(order("d2", 4));
        let kept_id: u64 = order_id(&kept.next_answer()).parse().expect("a numeric order ID");
        drop(pulled);
        drop(kept);
        eventually("the order of the session that asked is cancelled", || {
            venue.gateway.hub().order(pulled_id).is_none()
        });
        assert!(venue.gateway.hub().order(kept_id).is_some(), "an order was cancelled though its session did not ask");
    }

    #[test]
    fn a_client_that_stops_reading_is_disconnected_rather_than_buffered_for() {
        let venue = testing::venue("ws-slow");
        let mut client = Client::connect(&venue.ws_private);
        client.login(json!({}));
        // Replies and executions for these come to megabytes, far past what the gateway lets a client fall behind
        // by (`testing::MAX_BEHIND`) and past the kernel's socket buffers too. The client sends and never reads.
        const ORDERS: usize = 40_000;
        let mut sent = 0;
        for i in 0..ORDERS {
            // A failed send means the gateway has already let the connection go.
            if client.socket.send(Message::text(order(&format!("s{i}"), 4).to_string())).is_err() {
                break;
            }
            sent += 1;
        }
        // Now it reads: what the kernel still holds arrives, and then the connection ends, well short of every reply.
        let mut replies = 0;
        let dropped = loop {
            match client.socket.read() {
                Ok(Message::Text(text)) => {
                    if text.as_str().contains(r#""type":"reply""#) {
                        replies += 1;
                    }
                }
                Ok(Message::Close(_)) => break true,
                Ok(_) => {}
                // Silence, with the connection still open: the gateway kept buffering for a client not reading.
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
                {
                    break false
                }
                Err(_) => break true,
            }
        };
        assert!(dropped, "a client that stopped reading was kept, having been sent {replies} of {sent} replies");
        assert!(replies < sent, "the client got all {replies} replies, so it was never behind");
    }

    /// The whole request for quote: the requester asks, a responder hears without learning who asked, rests a sell and
    /// attaches it, the requester sees the quote and takes it with an ordinary order, and sees it leave the book.
    #[test]
    fn a_request_reaches_responders_and_their_quotes_reach_the_requester() {
        let venue = testing::venue("ws-rfq");
        let mut responder = Client::connect(&venue.ws_private);
        responder.login_as(RESPONDER_TOKEN);
        let subscribed = responder.ask_parlays(json!({"op": "subscribeRfqs", "id": "sub"}));
        assert_eq!((subscribed["id"].as_str(), &subscribed["result"]["subscribed"]), (Some("sub"), &json!(true)));
        let mut requester = Client::connect(&venue.ws_private);
        requester.login_as(RETAIL_TOKEN);
        let bitcoin = testing::market(3, 0).to_string();
        let legs =
            json!([{"instrumentId": bitcoin, "outcome": "yes"}, {"instrumentId": MARKET.to_string(), "outcome": "no"}]);
        let opened = requester.ask_parlays(json!({"op": "requestQuote", "id": "rq", "legs": legs, "quantity": 20}));
        assert_eq!((opened["type"].as_str(), opened["id"].as_str()), (Some("reply"), Some("rq")), "{opened}");
        let rfq = &opened["result"];
        assert_eq!((&rfq["listed"], &rfq["responders"]), (&json!(true), &json!(1)), "{rfq}");
        // Canonical order is by event: rain (template 1) before bitcoin (template 3).
        assert_eq!(rfq["legs"][0]["instrumentId"], MARKET.to_string(), "{rfq}");
        assert_eq!((rfq["legs"][0]["outcome"].as_str(), rfq["lotStep"].as_u64()), (Some("no"), Some(10)));
        let (request, parlay) = (rfq["requestId"].as_str().expect("a request ID"), rfq["instrumentId"].clone());
        let expires = rfq["expiresAt"].as_str().expect("a window").to_string();

        let heard = responder.streamed_message(|m| m["type"] == "rfq");
        assert_eq!((heard["requestId"].as_str(), &heard["instrumentId"]), (Some(request), &parlay), "{heard}");
        assert!(!heard.to_string().contains("accountId"), "the requester was named: {heard}");
        let listed: u64 = parlay.as_str().and_then(|p| p.parse().ok()).expect("a numeric instrument ID");
        eventually("the private half lists the parlay", || venue.gateway.hub().market(listed).is_some());
        let sell = json!({"op": "order", "id": "sell", "clientOrderId": "quote-1", "instrumentId": parlay,
                          "action": "sell", "outcome": "yes", "price": {"cents": "0.8"}, "quantity": 20,
                          "timeInForce": "GTD", "expiresAt": expires});
        responder.send(sell);
        let placed = responder.next_answer();
        assert_eq!(placed["result"]["status"], "accepted", "{placed}");
        let order = order_id(&placed);
        let attached =
            responder.ask_parlays(json!({"op": "attachQuote", "id": "attach", "requestId": request, "orderId": order}));
        assert_eq!((attached["id"].as_str(), attached["result"]["state"].as_str()), (Some("attach"), Some("open")));
        assert_eq!(attached["result"]["price"]["cents"], "0.8", "{attached}");

        let quote = requester.streamed_message(|m| m["type"] == "quote");
        assert_eq!(
            (quote["orderId"].as_str(), quote["quantity"].as_u64()),
            (Some(order.as_str()), Some(20)),
            "{quote}"
        );
        assert_eq!(
            (quote["price"]["probability"].as_str(), quote["expiresAt"].as_str()),
            (Some("1/125"), Some(expires.as_str()))
        );
        let take = json!({"op": "order", "id": "take", "clientOrderId": "take-1", "instrumentId": parlay,
                          "action": "buy", "outcome": "yes", "price": quote["price"]["probability"].as_str().map(|p| json!({"probability": p})),
                          "quantity": 20, "timeInForce": "FOK"});
        requester.send(take);
        let taken = requester.next_answer();
        assert_eq!(taken["result"]["status"], "accepted", "{taken}");
        let filled: u64 = taken["result"]["executions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|e| e["type"] == "fill")
            .filter_map(|e| e["quantity"].as_u64())
            .sum();
        assert_eq!(filled, 20, "{taken}");
        let gone = requester.streamed_message(|m| m["type"] == "quote" && m["state"] != "open");
        assert_eq!(
            (gone["state"].as_str(), gone["quantity"].as_u64(), gone["filled"].as_u64()),
            (Some("filled"), Some(0), Some(20)),
            "{gone}"
        );
    }

    #[test]
    fn only_a_responders_own_resting_sell_on_the_parlay_is_a_quote() {
        let venue = testing::venue("ws-rfq-attach");
        let mut requester = Client::connect(&venue.ws_private);
        requester.login_as(RETAIL_TOKEN);
        let legs = json!([{"instrumentId": MARKET.to_string(), "outcome": "yes"},
                          {"instrumentId": testing::market(2, 1).to_string(), "outcome": "yes"}]);
        let rfq = requester.ask_parlays(json!({"op": "requestQuote", "id": "rq", "legs": legs, "quantity": 10}))
            ["result"]
            .clone();
        let (request, parlay) = (rfq["requestId"].clone(), rfq["instrumentId"].clone());
        // Orders on the parlay are validated against the private half's view, which learns of it from the feed.
        let listed: u64 = parlay.as_str().and_then(|p| p.parse().ok()).expect("a numeric instrument ID");
        eventually("the private half lists the parlay", || venue.gateway.hub().market(listed).is_some());
        // The requester is no responder, whatever it rests.
        requester.send(json!({"op": "order", "id": "own", "clientOrderId": "own-1", "instrumentId": parlay,
                              "action": "sell", "outcome": "yes", "price": {"cents": "50"}, "quantity": 10}));
        let own = order_id(&requester.next_answer());
        let refused =
            requester.ask_parlays(json!({"op": "attachQuote", "id": "a1", "requestId": request, "orderId": own}));
        assert_eq!(refused["error"]["code"], "FORBIDDEN");
        let mut responder = Client::connect(&venue.ws_private);
        responder.login_as(RESPONDER_TOKEN);
        // Someone else's order, a buy, and an order on another market are all refused.
        assert_eq!(
            responder.ask_parlays(json!({"op": "attachQuote", "id": "a2", "requestId": request, "orderId": own}))
                ["error"]["code"],
            "NOT_FOUND"
        );
        responder.send(json!({"op": "order", "id": "buy", "clientOrderId": "buy-1", "instrumentId": parlay,
                              "action": "buy", "outcome": "yes", "price": {"cents": "1"}, "quantity": 10}));
        let buy = order_id(&responder.next_answer());
        assert_eq!(
            responder.ask_parlays(json!({"op": "attachQuote", "id": "a3", "requestId": request, "orderId": buy}))
                ["error"]["code"],
            "BAD_REQUEST"
        );
        responder.send(order("elsewhere", 4));
        let elsewhere = order_id(&responder.next_answer());
        assert_eq!(
            responder.ask_parlays(json!({"op": "attachQuote", "id": "a4", "requestId": request, "orderId": elsewhere}))
                ["error"]["code"],
            "BAD_REQUEST"
        );
        assert_eq!(
            responder
                .ask_parlays(json!({"op": "attachQuote", "id": "a5", "requestId": "nosuchrequest", "orderId": buy}))
                ["error"]["code"],
            "NOT_FOUND"
        );
    }

    fn request_quote(client: &mut Client, id: &str, legs: &[(u64, &str)], quantity: u64) -> Value {
        let legs: Vec<Value> =
            legs.iter().map(|(m, outcome)| json!({"instrumentId": m.to_string(), "outcome": outcome})).collect();
        let opened = client.ask_parlays(json!({"op": "requestQuote", "id": id, "legs": legs, "quantity": quantity}));
        assert_eq!(opened["type"], "reply", "{opened}");
        opened["result"].clone()
    }

    /// Nothing `wanted` accepts has reached the client, of what was on its way before this was called. Two round trips:
    /// the gateway answers a heartbeat as it reads it, possibly ahead of messages already waiting for the session, but
    /// sends whatever was waiting before it reads the next.
    fn none_streamed(client: &mut Client, id: &str, wanted: impl Fn(&Value) -> bool) {
        for round in ["a", "b"] {
            let id = format!("{id}{round}");
            client.send(json!({"op": "heartbeat", "id": id}));
            assert_eq!(client.next_answer()["id"], id.as_str());
        }
        let seen: Vec<&Value> = client.streamed.iter().filter(|m| wanted(m)).collect();
        assert!(seen.is_empty(), "sent {seen:?}");
    }

    /// A responder hears requests only once it subscribes: first those still open, ahead of the reply, then each new
    /// one its filters select, until it unsubscribes. Its own requests are never sent to it.
    #[test]
    fn a_subscribed_responder_catches_up_then_hears_what_its_filters_select() {
        let venue = testing::venue("ws-rfq-subscribe");
        let (arsenal, bitcoin) = (testing::market(2, 0), testing::market(3, 0));
        let mut requester = Client::connect(&venue.ws_private);
        requester.login_as(RETAIL_TOKEN);
        let mut responder = Client::connect(&venue.ws_private);
        responder.login_as(RESPONDER_TOKEN);
        let early = request_quote(&mut requester, "early", &[(MARKET, "yes"), (bitcoin, "no")], 10);
        assert_eq!(early["responders"], 0, "nobody had subscribed: {early}");
        // Logged in but not subscribed: nothing arrives.
        none_streamed(&mut responder, "h0", |m| m["type"] == "rfq");

        responder.send(json!({"op": "subscribeRfqs", "id": "sub"}));
        // The request is open, so only the parlay service being unreachable could refuse this, and the venue has
        // reached it; nothing else is sent to this session meanwhile.
        let caught_up = responder.next();
        assert_eq!((caught_up["type"].as_str(), &caught_up["requestId"]), (Some("rfq"), &early["requestId"]));
        assert!(caught_up["expiresAt"].as_str().is_some_and(|t| t.ends_with('Z')), "{caught_up}");
        assert!(!caught_up.to_string().contains("accountId"), "the requester was named: {caught_up}");
        let reply = responder.next();
        assert_eq!((reply["type"].as_str(), reply["id"].as_str()), (Some("reply"), Some("sub")), "{reply}");
        assert_eq!(reply["result"]["openRequests"], 1, "{reply}");
        assert_eq!(reply["result"]["filters"]["minLegs"], 2, "{reply}");
        assert!(reply["result"].get("requests").is_none(), "{reply}");

        // A second session of the same responder, choosier: three legs or more.
        let mut choosy = Client::connect(&venue.ws_private);
        choosy.login_as(RESPONDER_TOKEN);
        let sub3 = choosy.ask_parlays(json!({"op": "subscribeRfqs", "id": "sub3", "filters": {"minLegs": 3}}));
        assert_eq!(sub3["result"]["openRequests"], 0, "{sub3}");
        let two = request_quote(&mut requester, "two", &[(arsenal, "yes"), (bitcoin, "yes")], 20);
        assert_eq!(two["responders"], 1, "two sessions of one account are one responder: {two}");
        responder.streamed_message(|m| m["type"] == "rfq" && m["requestId"] == two["requestId"]);
        let three = request_quote(&mut requester, "three", &[(MARKET, "no"), (arsenal, "yes"), (bitcoin, "yes")], 10);
        choosy.streamed_message(|m| m["type"] == "rfq" && m["requestId"] == three["requestId"]);
        none_streamed(&mut choosy, "h1", |m| m["requestId"] == two["requestId"]);

        let unsubscribed = responder.ask_parlays(json!({"op": "unsubscribeRfqs", "id": "unsub"}));
        assert_eq!(unsubscribed["result"]["subscribed"], false, "{unsubscribed}");
        responder.streamed.clear();
        let late = request_quote(&mut requester, "late", &[(MARKET, "yes"), (arsenal, "no")], 10);
        assert_eq!(late["responders"], 0, "{late}");
        none_streamed(&mut responder, "h2", |m| m["type"] == "rfq");

        // An approved responder that requests is not sent its own request.
        let own = request_quote(&mut choosy, "own", &[(MARKET, "yes"), (arsenal, "yes"), (bitcoin, "no")], 10);
        assert_eq!(own["responders"], 0, "{own}");
        none_streamed(&mut choosy, "h3", |m| m["requestId"] == own["requestId"]);
    }

    #[test]
    fn an_account_that_is_not_an_approved_responder_cannot_subscribe() {
        let venue = testing::venue("ws-rfq-refused");
        let mut client = Client::connect(&venue.ws_private);
        client.send(json!({"op": "subscribeRfqs", "id": "anonymous"}));
        assert_eq!(client.next_answer()["error"]["code"], "UNAUTHORIZED");
        client.login_as(RETAIL_TOKEN);
        let refused = client.ask_parlays(json!({"op": "subscribeRfqs", "id": "retail"}));
        assert_eq!((refused["id"].as_str(), refused["error"]["code"].as_str()), (Some("retail"), Some("FORBIDDEN")));
        assert!(refused["error"]["message"].as_str().is_some_and(|m| m.contains("approved")), "{refused}");
        let mut responder = Client::connect(&venue.ws_private);
        responder.login_as(RESPONDER_TOKEN);
        for (filters, why) in [
            (json!({"minLegs": 7}), "too many legs"),
            (json!({"minLegs": 4, "maxLegs": 3}), "an empty range"),
            (json!({"eventIds": [5]}), "an ID that is not a string"),
            (json!({"colour": "red"}), "an unknown filter"),
        ] {
            let refused = responder.ask_parlays(json!({"op": "subscribeRfqs", "id": why, "filters": filters}));
            assert_eq!(refused["error"]["code"], "BAD_REQUEST", "{why}: {refused}");
        }
        let mut public = Client::connect(&venue.ws_public);
        public.send(json!({"op": "subscribeRfqs", "id": "public"}));
        let wrong_half = public.next_answer();
        assert!(wrong_half["error"]["message"].as_str().is_some_and(|m| m.contains("/v4/ws_private")), "{wrong_half}");
    }

    #[test]
    fn malformed_messages_get_errors_and_the_session_carries_on() {
        let venue = testing::venue("ws-malformed");
        let mut client = Client::connect(&venue.ws_private);
        client.socket.send(Message::text("not json")).expect("sent");
        assert_eq!(client.next_answer()["type"], "error");
        client.socket.send(Message::binary(vec![1, 2, 3])).expect("sent");
        assert_eq!(client.next_answer()["type"], "error");
        client.send(json!({"op": "nonsense", "id": "x"}));
        assert_eq!(client.next_answer()["id"], "x");
        client.send(order("before-login", 4));
        let unauthorized = client.next_answer();
        assert_eq!((unauthorized["type"].as_str(), unauthorized["id"].as_str()), (Some("error"), Some("before-login")));
        client.send(json!({"op": "heartbeat", "id": "still-here"}));
        assert_eq!(client.next_answer()["id"], "still-here");
    }
}
