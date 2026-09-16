//! The WebSocket half of interface 1 (interfaces/gateway/asyncapi.yaml). One connection is one client session.

use crate::api::{only, parse_id, ApiError, Gateway};
use crate::hub::id;
use crate::limits::{monotonic, Bucket};
use serde_json::{json, Map, Value};
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
    loop {
        // Only what the client sends shortens the wait again. What the gateway sends does not: a session that
        // asked not to be streamed to would rather collect its messages and take them in one go, and a session
        // that is streamed to has a ceiling of POLL_MIN, so its wait cannot grow whatever happens here.
        let mut heard = false;
        match socket.read() {
            Ok(Message::Text(text)) => {
                heard = true;
                session.last_heard = monotonic();
                for reply in handle(gateway, session, text.as_str()) {
                    if socket.send(Message::text(reply)).is_err() {
                        return;
                    }
                }
            }
            Ok(Message::Binary(_)) => {
                heard = true;
                let refusal = error(&Value::Null, &ApiError::bad("send JSON as text frames"));
                if socket.send(Message::text(refusal)).is_err() {
                    return;
                }
            }
            Ok(Message::Close(_)) => return,
            Ok(_) => {
                heard = true;
                session.last_heard = monotonic();
            }
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(_) => return,
        }
        while let Ok(message) = rx.try_recv() {
            if socket.write(Message::text(message)).is_err() {
                return;
            }
        }
        if socket.flush().is_err() {
            return;
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
        let next = if heard { POLL_MIN } else { (session.poll * 2).min(session.ceiling()) };
        if next != session.poll {
            session.poll = next;
            // Only when it actually changes: this is a syscall, and it would otherwise run on every quiet read.
            let _ = socket.get_ref().set_read_timeout(Some(next));
        }
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
            let result = match op {
                "order" => gateway.place(account, s, &body)?,
                "cancelAll" => gateway.cancel_all(account, s, &body)?,
                _ => {
                    let order = parse_id(body.get("orderId").and_then(Value::as_str).unwrap_or(""), "orderId")?;
                    if let Some(fields) = body.as_object_mut() {
                        fields.remove("orderId");
                    }
                    if op == "cancel" {
                        only(&body, &[])?;
                        gateway.cancel(account, s, order)?
                    } else {
                        gateway.amend(account, s, order, &body)?
                    }
                }
            };
            Ok(vec![reply(&request_id, result)])
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
