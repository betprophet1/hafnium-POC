//! The identity service's own HTTP API, for the gateways and for operators. It is not interface 1: it trusts the
//! account in the path, because only the private half of the gateway reaches it, and only after authenticating the
//! account and checking it may manage keys (interfaces/gateway/README.md, API keys and permissions). Its answers to
//! the key calls are interface 1's `Key`, `KeyCreated`, `Keys` and `Error`, so the gateway passes them on untouched.
//!
//! - `POST /v1/accounts/{accountId}/keys`: a `KeyRequest`, and `createdBy` when a key asked
//! - `GET /v1/accounts/{accountId}/keys`
//! - `DELETE /v1/accounts/{accountId}/keys/{keyId}`
//! - `POST /v1/verify`: `{"credential"}`, answered `{"keyId", "accountId", "permissions", "expiresAt"}` or 401
//! - `GET /healthz`: `{"ready": true, "database": "ok"}`, or 503 when the key store cannot be reached (the pod is then
//!   not ready, and gateways answer keys they have not verified recently with 503)
//!
//! One thread per connection and one request per connection, like the treasury's: gateways ask about a key once every
//! few seconds, and people manage keys by hand.

use crate::clock;
use crate::store::{Refusal, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long a request has to arrive whole.
const ARRIVAL: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 16 * 1024;

pub fn serve(listener: &TcpListener, store: &Arc<Store>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let store = Arc::clone(store);
        thread::spawn(move || {
            if let Err(e) = handle(&store, &stream) {
                eprintln!("identity: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    body: Vec<u8>,
}

fn read(stream: &TcpStream) -> Result<Request, String> {
    stream.set_read_timeout(Some(ARRIVAL)).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("").to_string();
    let mut length = 0;
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(|_| "a bad content-length".to_string())?;
            }
        }
    }
    if length > MAX_BODY {
        return Err("the body is too large".into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(Request { method, path, body })
}

fn respond(mut stream: &TcpStream, status: u16, body: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let text = body.to_string();
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .map_err(|e| e.to_string())
}

fn account(text: &str) -> Result<u64, Refusal> {
    let digits = !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten().ok_or_else(|| Refusal::bad("accountId must be a decimal string"))
}

fn json_body(request: &Request) -> Result<Value, Refusal> {
    serde_json::from_slice(&request.body).map_err(|e| Refusal::bad(format!("the body is not JSON: {e}")))
}

fn handle(store: &Store, stream: &TcpStream) -> Result<(), String> {
    let answer = match read(stream) {
        Ok(request) => route(store, &request),
        Err(e) => Err(Refusal::bad(format!("unreadable request: {e}"))),
    };
    match answer {
        Ok(body) => respond(stream, 200, &body),
        Err(refusal) => respond(stream, refusal.status, &refusal.body()),
    }
}

fn route(store: &Store, request: &Request) -> Result<Value, Refusal> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let now = clock::wall_ms();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["healthz"]) => match store.health() {
            Ok(()) => Ok(json!({"ready": true, "database": "ok"})),
            Err(e) => Err(Refusal::unavailable(format!("the key store is unavailable: {e}"))),
        },
        ("POST", ["v1", "verify"]) => {
            let body = json_body(request)?;
            let credential = body["credential"].as_str().ok_or_else(|| Refusal::bad("credential is required"))?;
            Ok(store.verify(credential, now)?.json())
        }
        // Members and their sessions (interface 6, in draft): signing up, logging in, logging out.
        ("POST", ["v1", "members"]) => store.sign_up(&json_body(request)?, now),
        ("POST", ["v1", "sessions"]) => store.log_in(&json_body(request)?, now),
        ("DELETE", ["v1", "sessions", session_id]) => store.log_out(session_id, now),
        ("POST", ["v1", "accounts", id, "keys"]) => store.create(account(id)?, &json_body(request)?, now),
        ("GET", ["v1", "accounts", id, "keys"]) => Ok(store.list(account(id)?)),
        ("DELETE", ["v1", "accounts", id, "keys", key_id]) => store.revoke(account(id)?, key_id, now),
        _ => Err(Refusal::not_found("no such route")),
    }
}

#[cfg(test)]
mod tests {
    use crate::Settings;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn call(address: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).expect("connected");
        let body = body.map(Value::to_string).unwrap_or_default();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: identity\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).expect("sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("answered");
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status");
        (status, response.split_once("\r\n\r\n").and_then(|(_, b)| serde_json::from_str(b).ok()).unwrap_or(Value::Null))
    }

    #[test]
    fn a_member_signs_up_logs_in_trades_as_their_account_and_logs_out() {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let a = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");

        let signing = json!({"email": "Someone@Example.com", "password": "a good long password"});
        let (status, member) = call(&a, "POST", "/v1/members", Some(&signing));
        assert_eq!(status, 200, "{member}");
        assert_eq!(member["email"], "someone@example.com", "the email is kept as it is looked up: {member}");
        assert_eq!(member["displayName"], "someone", "{member}");
        let account = member["accountId"].as_str().expect("an account").to_string();
        assert!(account.parse::<u64>().expect("a number") >= 100_000, "an account of their own: {account}");
        assert!(!member.to_string().contains("a good long password"), "the password is not echoed: {member}");

        // The session it answers with is a credential the gateway can verify, for that account.
        let credential = member["session"]["credential"].as_str().expect("a credential").to_string();
        let (status, verified) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": credential})));
        assert_eq!((status, &verified["accountId"]), (200, &json!(account)), "{verified}");
        assert_eq!(verified["sessionId"], member["session"]["sessionId"], "{verified}");
        assert_eq!(verified["keyId"], Value::Null, "a session is not a key: {verified}");
        assert_eq!(verified["permissions"], json!(["read", "trade", "transfers", "keys"]), "{verified}");

        // The same email and password again is the same account, with a session of its own.
        let (status, again) = call(&a, "POST", "/v1/sessions", Some(&signing));
        assert_eq!((status, &again["accountId"]), (200, &json!(account)), "{again}");
        assert_ne!(again["session"]["sessionId"], member["session"]["sessionId"], "a second session");

        // Signing up twice with one email does not make a second account.
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 400);

        // The wrong password, and an email nobody has, are refused the same way.
        let wrong = json!({"email": "someone@example.com", "password": "not the password"});
        let (status, refused) = call(&a, "POST", "/v1/sessions", Some(&wrong));
        assert_eq!((status, refused["error"]["message"].as_str()), (401, Some("the email or password is wrong")));
        let nobody = json!({"email": "nobody@example.com", "password": "not the password"});
        let (status, unknown) = call(&a, "POST", "/v1/sessions", Some(&nobody));
        assert_eq!((status, unknown["error"]["message"].as_str()), (401, Some("the email or password is wrong")));

        // A password too short to be one is refused before it is ever hashed.
        let short = json!({"email": "other@example.com", "password": "short"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&short)).0, 400);

        // Logging out ends that session and no other.
        let path = format!("/v1/sessions/{}", member["session"]["sessionId"].as_str().expect("an ID"));
        assert_eq!(call(&a, "DELETE", &path, None).0, 200);
        let (status, ended) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": credential})));
        assert_eq!((status, ended["error"]["code"].as_str()), (401, Some("UNAUTHORIZED")), "{ended}");
        let still = again["session"]["credential"].as_str().expect("a credential");
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&json!({"credential": still}))).0, 200);
        // Logging out of a session that is not there says so rather than pretending.
        assert_eq!(call(&a, "DELETE", "/v1/sessions/vs_0000000000000000", None).0, 404);
    }

    #[test]
    fn keys_are_made_verified_listed_and_revoked_over_http() {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let a = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");

        assert_eq!(call(&a, "GET", "/healthz", None).0, 200);
        let (status, created) =
            call(&a, "POST", "/v1/accounts/2/keys", Some(&json!({"label": "script", "permissions": ["read"]})));
        assert_eq!(status, 200, "{created}");
        let (status, verified) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": created["credential"]})));
        assert_eq!((status, &verified["accountId"], &verified["permissions"]), (200, &json!("2"), &json!(["read"])));
        assert_eq!(call(&a, "GET", "/v1/accounts/2/keys", None).1["keys"][0]["keyId"], created["keyId"]);
        let path = format!("/v1/accounts/2/keys/{}", created["keyId"].as_str().expect("an ID"));
        assert_eq!(call(&a, "DELETE", "/v1/accounts/1/keys/ak_0000000000000000", None).0, 404);
        assert_eq!(call(&a, "DELETE", &path, None).0, 200);
        let (status, refused) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": created["credential"]})));
        assert_eq!((status, refused["error"]["code"].as_str()), (401, Some("UNAUTHORIZED")), "{refused}");
        assert_eq!(
            call(&a, "POST", "/v1/accounts/99/keys", Some(&json!({"label": "x", "permissions": ["read"]}))).0,
            404
        );
        assert_eq!(call(&a, "GET", "/v1/nowhere", None).0, 404);
    }
}
