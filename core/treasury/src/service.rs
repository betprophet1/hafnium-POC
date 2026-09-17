//! The treasury's own HTTP API, for the gateway and for operators. It is not interface 1: it trusts the account in the
//! path, because only the private half of the gateway reaches it, and only after authenticating the account
//! (interfaces/gateway/README.md, deposits and withdrawals). Its answers are interface 1's `Transfer`, `Transfers`
//! and `Error`, so the gateway passes them on untouched.
//!
//! - `POST /v1/accounts/{accountId}/transfers`, `GET` the same with `?limit=`
//! - `GET` and `DELETE /v1/accounts/{accountId}/transfers/{transferId}`
//! - `GET /v1/updates`: every transfer change from now on, one JSON line each, `{"accountId", "transfer"}`, with an
//!   empty line every 15 s so a follower can tell a quiet treasury from a gone one
//! - `GET /v1/ledger`: the trial balance
//! - `GET /v1/bank/statement?from=&to=`: the fake bank's statement of the omnibus, wall-clock milliseconds
//! - `GET /v1/reconciliation`: the schedule, lost messages being injected, the last run, open and recent breaks
//! - `GET /v1/reconciliation/runs?limit=`, and `POST` the same to run now (waits up to ten seconds for it)
//! - `GET /v1/reconciliation/breaks?state=open|resolved|all&limit=`
//! - `POST /v1/reconciliation/breaks/{breakId}/resolve` with `{"note"}`: an operator marks a break resolved
//! - `GET /healthz`: `{"ready": true, "database": "ok"}`, or 503 when the records cannot be reached (the pod is then
//!   not ready, and the gateway answers transfers with 503 until they can)
//!
//! One thread per connection and one request per connection, like the web page's server: a treasury's traffic is a
//! person's deposits, not a market's orders.

use crate::clock;
use crate::treasury::{Refusal, Treasury, MAX_LIST, QUICK};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long a request has to arrive whole.
const ARRIVAL: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 64 * 1024;
/// How often an update stream with nothing to say says so.
const KEEPALIVE: Duration = Duration::from_secs(15);
/// How long an operator's request for a run waits for it: a run is in memory, but waits for the driver to finish
/// whatever send it is in the middle of.
const RUN_WAIT: Duration = Duration::from_secs(10);

pub fn serve(listener: &TcpListener, treasury: &Arc<Treasury>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let treasury = Arc::clone(treasury);
        thread::spawn(move || {
            if let Err(e) = handle(&treasury, stream) {
                eprintln!("treasury: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
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
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query =
        query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
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
    Ok(Request { method, path: path.to_string(), query, body })
}

fn respond(mut stream: &TcpStream, status: u16, body: &Value) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
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

fn param<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    request.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

fn limit(request: &Request) -> Result<usize, Refusal> {
    match param(request, "limit") {
        None => Ok(100),
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|l| (1..=MAX_LIST).contains(l))
            .ok_or_else(|| Refusal::bad(format!("limit must be from 1 to {MAX_LIST}"))),
    }
}

fn id(text: &str, name: &str) -> Result<u64, Refusal> {
    let digits = !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten().ok_or_else(|| Refusal::bad(format!("{name} must be a decimal string")))
}

fn handle(treasury: &Arc<Treasury>, stream: TcpStream) -> Result<(), String> {
    let request = match read(&stream) {
        Ok(r) => r,
        Err(e) => return respond(&stream, 400, &Refusal::bad(format!("unreadable request: {e}")).body()),
    };
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    if let ("GET", ["v1", "updates"]) = (request.method.as_str(), segments.as_slice()) {
        return updates(treasury, stream);
    }
    let answer = route(treasury, &request, &segments);
    match answer {
        Ok(body) => respond(&stream, 200, &body),
        Err(refusal) => respond(&stream, refusal.status, &refusal.body()),
    }
}

fn route(treasury: &Treasury, request: &Request, segments: &[&str]) -> Result<Value, Refusal> {
    let now = clock::wall_ms;
    match (request.method.as_str(), segments) {
        ("GET", ["healthz"]) => match treasury.health() {
            Ok(()) => Ok(json!({"ready": true, "database": "ok"})),
            Err(e) => Err(Refusal::unavailable(format!("the treasury's records are unavailable: {e}"))),
        },
        ("GET", ["v1", "ledger"]) => Ok(treasury.trial_balance()),
        ("GET", ["v1", "bank", "statement"]) => {
            let to = match param(request, "to") {
                None => now(),
                Some(v) => id(v, "to")?,
            };
            let from = param(request, "from").map_or(Ok(0), |v| id(v, "from"))?;
            Ok(treasury.statement(from, to))
        }
        ("GET", ["v1", "reconciliation"]) => Ok(treasury.reconciliation()),
        ("GET", ["v1", "reconciliation", "runs"]) => Ok(treasury.runs(limit(request)?)),
        ("POST", ["v1", "reconciliation", "runs"]) => Ok(treasury.request_run(RUN_WAIT)),
        ("GET", ["v1", "reconciliation", "breaks"]) => treasury.breaks(param(request, "state"), limit(request)?),
        ("POST", ["v1", "reconciliation", "breaks", b, "resolve"]) => {
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|e| Refusal::bad(format!("the body is not JSON: {e}")))?;
            treasury.resolve_break(id(b, "breakId")?, &body, now())
        }
        ("POST", ["v1", "accounts", account, "transfers"]) => {
            let account = id(account, "accountId")?;
            let body: Value = serde_json::from_slice(&request.body)
                .map_err(|e| Refusal::bad(format!("the body is not JSON: {e}")))?;
            let transfer = treasury.request(account, &body, now())?;
            Ok(treasury.wait_quick(transfer, QUICK))
        }
        ("GET", ["v1", "accounts", account, "transfers"]) => {
            let account = id(account, "accountId")?;
            treasury.list(account, limit(request)?)
        }
        ("GET", ["v1", "accounts", account, "transfers", transfer]) => {
            treasury.get(id(account, "accountId")?, id(transfer, "transferId")?)
        }
        ("DELETE", ["v1", "accounts", account, "transfers", transfer]) => {
            let account = id(account, "accountId")?;
            let transfer = treasury.cancel(account, id(transfer, "transferId")?, now())?;
            Ok(treasury.wait_quick(transfer, QUICK))
        }
        _ => Err(Refusal::not_found("no such route")),
    }
}

/// Streams every transfer change from now on, until the follower goes.
fn updates(treasury: &Treasury, mut stream: TcpStream) -> Result<(), String> {
    let head =
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
    stream.write_all(head.as_bytes()).map_err(|e| e.to_string())?;
    let mut next = None;
    loop {
        let (changes, after) = treasury.updates_from(next, KEEPALIVE);
        next = Some(after);
        let mut text = String::new();
        for (account, transfer) in changes {
            text.push_str(&json!({"accountId": account.to_string(), "transfer": transfer}).to_string());
            text.push('\n');
        }
        if text.is_empty() {
            text.push('\n');
        }
        stream.write_all(text.as_bytes()).map_err(|e| format!("an update stream ended: {e}"))?;
    }
}
