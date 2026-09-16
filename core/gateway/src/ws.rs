//! The WebSocket half of interface 1 (interfaces/gateway/asyncapi.yaml). One connection is one client session.

use crate::api::{only, parse_id, ApiError, Gateway, Pending};
use crate::hub::id;
use crate::limits::{monotonic, Bucket};
use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/// How long a read waits for a session being streamed to, which is every session that does not say otherwise.
/// This is what every session always waited before.
const POLL_MIN: Duration = Duration::from_millis(50);

/// The longest a session that asked not to be streamed to waits while its client says nothing. A connection costs
/// the gateway a wakeup every interval whether or not anything arrives, and across thousands of connections that
/// is much its largest cost: a thousand of them waiting 50ms at a time cost 25% of a core doing nothing else.
const POLL_IDLE_MAX: Duration = Duration::from_secs(1);

/// How long a session with work outstanding waits on its oldest reply before looking at its socket again.
///
/// This wait is on the reply's channel, never on the socket, and the difference is the whole reason a request no
/// longer costs milliseconds. A socket read timeout is rounded up to a whole kernel tick — a jiffy, 1ms on this
/// kernel — so asking a socket to wait 200us gets you 1 to 2ms, and a reply that arrived in 40us waits out the
/// rest of the tick. A channel wait is a futex with a timespec and honours microseconds.
const AWAIT_REPLY: Duration = Duration::from_millis(2);

/// Requests one session may have outstanding at once. Beyond this it waits for its oldest before reading more,
/// which is where one client is stopped from queueing work at every other client's expense.
const MAX_IN_FLIGHT: usize = 64;

struct Session {
    key: u64,
    engine_session: u64,
    account: Option<u64>,
    cancel_on_disconnect: bool,
    heartbeat: Option<Duration>,
    last_heard: Instant,
    bucket: Bucket,
    /// How long this session's next quiet read waits, and the most it may ever grow to (`stream` at logon).
    poll: Duration,
    poll_ceiling: Duration,
    /// Requests given to the node and not yet answered, oldest first, each with the id to answer it under. A node
    /// answers a connection in commit order, so the oldest is always the first to be ready and replies leave in
    /// the order they were asked for.
    in_flight: VecDeque<(Value, Pending)>,
}

impl Session {
    /// The most a quiet read may wait: never so long that this session's own heartbeat would be missed late,
    /// however patient its client said it was willing to be.
    fn ceiling(&self) -> Duration {
        match self.heartbeat {
            Some(limit) => self.poll_ceiling.min(limit / 2),
            None => self.poll_ceiling,
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

pub fn serve(gateway: &Gateway, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(mut socket) = tungstenite::accept(stream) else { return };
    let _ = socket.get_ref().set_read_timeout(Some(POLL_MIN));
    let _ = socket.get_ref().set_write_timeout(Some(Duration::from_secs(5)));
    let Some(engine_session) = session_id() else { return };
    let (tx, rx) = mpsc::channel();
    let key = gateway.hub().add_session(tx);
    let mut session = Session {
        key,
        engine_session,
        account: None,
        cancel_on_disconnect: false,
        heartbeat: None,
        last_heard: monotonic(),
        bucket: Bucket::per_second(gateway.limits.per_second, gateway.limits.burst),
        // Until a client says otherwise it is streamed to, and its wait never grows past where it starts.
        poll: POLL_MIN,
        poll_ceiling: POLL_MIN,
        in_flight: VecDeque::new(),
    };
    run(gateway, &mut socket, &mut session, &rx);
    gateway.hub().remove_session(key);
    if let (Some(account), true) = (session.account, session.cancel_on_disconnect) {
        if let Err(e) = gateway.cancel_session(account, engine_session, "SESSION_DISCONNECT") {
            eprintln!("gateway: cancel on disconnect for session {engine_session}: {}", e.message);
        }
    }
}

fn run(gateway: &Gateway, socket: &mut WebSocket<TcpStream>, session: &mut Session, rx: &Receiver<String>) {
    // What the socket is actually set to, so it is only set again when it really changes.
    let mut applied = POLL_MIN;
    let mut nonblocking = false;
    loop {
        // A session at its limit answers its oldest before taking anything more: one client queueing work is not
        // allowed to become every other client's problem.
        while session.in_flight.len() >= MAX_IN_FLIGHT {
            let Some(message) = wait_for_oldest(gateway, session) else { break };
            if !queue(socket, message) {
                return;
            }
        }
        // Everything the client has already sent is taken before anything at all is waited for. While work is
        // outstanding the socket does not block, so a read that finds nothing returns at once and a burst is
        // drained in a single pass rather than one request per pass. With nothing outstanding the first read
        // blocks on this session's own interval, which is what makes an idle connection cost nothing.
        //
        // The waiting happens after this, on the reply's channel, which honours microseconds where a socket
        // timeout rounds up to a whole kernel tick.
        let mut heard = false;
        loop {
            let outstanding = !session.in_flight.is_empty();
            if outstanding != nonblocking {
                if socket.get_ref().set_nonblocking(outstanding).is_err() {
                    return;
                }
                nonblocking = outstanding;
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    heard = true;
                    session.last_heard = monotonic();
                    for reply in handle(gateway, session, text.as_str()) {
                        if !queue(socket, reply) {
                            return;
                        }
                    }
                }
                Ok(Message::Binary(_)) => {
                    heard = true;
                    if !queue(socket, error(&Value::Null, &ApiError::bad("send JSON as text frames"))) {
                        return;
                    }
                }
                Ok(Message::Close(_)) => return,
                Ok(_) => {
                    heard = true;
                    session.last_heard = monotonic();
                }
                Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    break
                }
                Err(_) => return,
            }
            // Not past the point where this session holds more than its share, however much it sent at once.
            if session.in_flight.len() >= MAX_IN_FLIGHT {
                break;
            }
        }
        let busy = !session.in_flight.is_empty();
        while let Ok(message) = rx.try_recv() {
            if !queue(socket, message) {
                return;
            }
        }
        if let (Some(limit), Some(account)) = (session.heartbeat, session.account) {
            if monotonic().duration_since(session.last_heard) > limit {
                session.cancel_on_disconnect = false;
                if let Err(e) = gateway.cancel_session(account, session.engine_session, "HEARTBEAT_MISSED") {
                    eprintln!("gateway: heartbeat cancel for session {}: {}", session.engine_session, e.message);
                }
                let _ = socket.close(None);
                let _ = socket.flush();
                return;
            }
        }
        // Replies to what the client asked for, in the order it asked. With nothing ready yet and work still
        // outstanding, the wait belongs here, on the reply, rather than back on the socket.
        let mut replies = ready_replies(gateway, session);
        if replies.is_empty() && !session.in_flight.is_empty() {
            if let Some(message) = await_oldest(gateway, session) {
                replies.push(message);
                replies.extend(ready_replies(gateway, session));
            }
        }
        for message in replies {
            if !queue(socket, message) {
                return;
            }
        }
        if !flushed(socket) {
            return;
        }
        // Anything the client sends makes the session attentive again; quiet lets its wait grow to its ceiling.
        session.poll = if heard { POLL_MIN } else { (session.poll * 2).min(session.ceiling()) };
        // Only meaningful while the socket blocks, and only set when it changes: this is a syscall.
        if !busy && session.poll != applied {
            applied = session.poll;
            let _ = socket.get_ref().set_read_timeout(Some(session.poll));
        }
    }
}

/// Queues a message for the client, and says whether the connection is still worth keeping. A would-block is not
/// a failure: the message stays in the socket's own buffer and goes out on a later flush. That distinction only
/// arises because this socket is non-blocking whenever the session has work outstanding, and treating it as a
/// failure would drop sessions whose client had briefly stopped reading.
fn queue(socket: &mut WebSocket<TcpStream>, message: String) -> bool {
    match socket.write(Message::text(message)) {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

/// Pushes what is queued towards the client, tolerating a would-block for the same reason.
fn flushed(socket: &mut WebSocket<TcpStream>) -> bool {
    match socket.flush() {
        Ok(()) => true,
        Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}

/// The replies whose results have arrived, oldest first, stopping at the first that has not.
fn ready_replies(gateway: &Gateway, session: &mut Session) -> Vec<String> {
    let mut out = Vec::new();
    // The result is taken before the request leaves the queue, so nothing is dropped between the two.
    while let Some(result) = session.in_flight.front().and_then(|(_, pending)| gateway.finish(pending)) {
        if let Some((id, _)) = session.in_flight.pop_front() {
            out.push(answer(&id, result));
        }
    }
    out
}

/// Every outstanding reply, waiting for each in turn. For when something has to be answered in one go and must
/// not overtake what the client asked for before it.
fn drain_in_flight(gateway: &Gateway, session: &mut Session) -> Vec<String> {
    let mut out = Vec::new();
    while let Some((id, pending)) = session.in_flight.pop_front() {
        out.push(answer(&id, gateway.wait_for(pending)));
    }
    out
}

/// The oldest reply, waited for briefly. This is where a session with work outstanding spends its time, so that a
/// reply goes out when the node answers rather than when the socket next times out.
fn await_oldest(gateway: &Gateway, session: &mut Session) -> Option<String> {
    let result = {
        let (_, pending) = session.in_flight.front()?;
        gateway.finish_within(pending, AWAIT_REPLY)?
    };
    let (id, _) = session.in_flight.pop_front()?;
    Some(answer(&id, result))
}

/// The oldest reply, waited for. How a session at its limit makes room before reading anything more.
fn wait_for_oldest(gateway: &Gateway, session: &mut Session) -> Option<String> {
    let (id, pending) = session.in_flight.pop_front()?;
    Some(answer(&id, gateway.wait_for(pending)))
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

fn handle(gateway: &Gateway, session: &mut Session, text: &str) -> Vec<String> {
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
            let account = session.account.ok_or_else(ApiError::unauthorized)?;
            let s = session.engine_session;
            let mut body = body_of(message);
            // A mass cancel answers with what it cancelled rather than with an order's outcome, and it is rare, so
            // it is still done in one go. Everything before it is answered first: it must not overtake orders the
            // client sent earlier.
            if op == "cancelAll" {
                let mut out = drain_in_flight(gateway, session);
                out.push(reply(&request_id, gateway.cancel_all(account, s, &body)?));
                return Ok(out);
            }
            let pending = match op {
                "order" => gateway.place_pending(account, s, &body)?,
                _ => {
                    let order = parse_id(body.get("orderId").and_then(Value::as_str).unwrap_or(""), "orderId")?;
                    if let Some(fields) = body.as_object_mut() {
                        fields.remove("orderId");
                    }
                    if op == "cancel" {
                        only(&body, &[])?;
                        gateway.cancel_pending(account, s, order)?
                    } else {
                        gateway.amend_pending(account, s, order, &body)?
                    }
                }
            };
            // Answered when the node answers, not before: a client with several to send is not held to one at a
            // time waiting for each round trip.
            session.in_flight.push_back((request_id, pending));
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
    let account = gateway.authenticate(fields.get("token").and_then(Value::as_str).unwrap_or(""))?;
    gateway.logon_rate(account)?;
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
    // A client that says it is not streamed to would rather be woken rarely than promptly: its messages collect
    // and arrive together. A screen does not mind; anything trading should leave this alone.
    let streamed = match fields.get("stream") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ApiError::bad("stream must be true or false")),
    };
    session.account = Some(account);
    session.cancel_on_disconnect = cancel_on_disconnect;
    session.heartbeat = (heartbeat > 0).then(|| Duration::from_secs(heartbeat));
    session.poll_ceiling = if streamed { POLL_MIN } else { POLL_IDLE_MAX };
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
    use crate::testing::{self, eventually, MAKER_TOKEN, MARKET, RETAIL_TOKEN};
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
            if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
                stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
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
            let matches = |m: &Value| {
                m["type"] == "execution"
                    && m["execution"]["clientOrderId"] == client_order_id
                    && m["execution"]["type"] == kind
            };
            loop {
                if let Some(found) = self.streamed.iter().find(|m| matches(m)) {
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
        let mut client = Client::connect(&venue.ws_url);
        client.send(json!({"op": "login", "id": "bad", "token": "no-such-token"}));
        let refused = client.next_answer();
        assert_eq!(refused["type"], "error", "{refused}");
        assert_eq!(refused["id"], "bad");
        let welcome = client.login(json!({}));
        assert_eq!(welcome["id"], "login");
        assert_eq!(welcome["accountId"], "1");
        assert!(welcome["sessionId"].as_str().is_some_and(|s| !s.is_empty()), "{welcome}");
    }

    #[test]
    fn orders_amendments_and_cancels_are_answered_under_their_ids() {
        let venue = testing::venue("ws-order");
        let mut client = Client::connect(&venue.ws_url);
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
        let mut client = Client::connect(&venue.ws_url);
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
        let mut client = Client::connect(&venue.ws_url);
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
        let mut watcher = Client::connect(&venue.ws_url);
        watcher.send(json!({"op": "subscribe", "id": "s", "topics": [format!("book.{MARKET}")]}));
        let image = watcher.next_book(|_| true);
        assert_eq!(image["snapshot"], true, "{image}");
        let mut trader = Client::connect(&venue.ws_url);
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
    }

    #[test]
    fn a_session_that_is_not_streamed_to_still_gets_its_changes() {
        let venue = testing::venue("ws-unstreamed");
        let mut watcher = Client::connect(&venue.ws_url);
        watcher.login(json!({"stream": false}));
        watcher.send(json!({"op": "subscribe", "id": "s", "topics": [format!("book.{MARKET}")]}));
        watcher.next_book(|m| m["snapshot"] == true);
        let mut trader = Client::connect(&venue.ws_url);
        trader.login(json!({}));
        trader.send(order("u1", 8));
        assert_eq!(trader.next_answer()["result"]["status"], "accepted");
        watcher.next_book(|m| m["changes"].as_array().is_some_and(|c| c.iter().any(|l| l["price"]["tick"] == 8)));
    }

    #[test]
    fn a_heartbeat_is_answered_and_a_missed_one_cancels_and_closes() {
        let venue = testing::venue("ws-heartbeat");
        let mut client = Client::connect(&venue.ws_url);
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
        let mut pulled = Client::connect(&venue.ws_url);
        pulled.login(json!({"cancelOnDisconnect": true}));
        pulled.send(order("d1", 4));
        let pulled_id: u64 = order_id(&pulled.next_answer()).parse().expect("a numeric order ID");
        let mut kept = Client::connect(&venue.ws_url);
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
    fn malformed_messages_get_errors_and_the_session_carries_on() {
        let venue = testing::venue("ws-malformed");
        let mut client = Client::connect(&venue.ws_url);
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
