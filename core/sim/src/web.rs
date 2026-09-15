//! The mini venue's web page, its API and the dummy oracle's controls. Development only: there is no
//! authentication, so never expose it outside a laptop or a test cluster.
//!
//! The oracle here sequences commands directly. Blueprint §16 routes a dummy resolution through a proposal, the
//! resolution console and an auto-approver; those do not exist yet.

use crate::catalogue::Catalogue;
use crate::view::Live;
use crate::wire::{self, Error, Ids, Ingress};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const INDEX: &str = include_str!("index.html");

pub struct Web {
    catalogue: Catalogue,
    live: Arc<Live>,
    ingress: Mutex<(Ingress, Ids)>,
}

fn poisoned<T>(_: T) -> Error {
    "a lock is poisoned".into()
}

impl Web {
    pub fn new(catalogue: Catalogue, live: Arc<Live>, ingress: Ingress, ids: Ids) -> Self {
        Self { catalogue, live, ingress: Mutex::new((ingress, ids)) }
    }

    fn snapshot(&self) -> Result<Value, Error> {
        Ok(self.live.view.lock().map_err(poisoned)?.snapshot(&self.catalogue))
    }

    fn action(&self, name: &str, r: &Value) -> Result<Value, Error> {
        let account = r["account"].as_u64().unwrap_or(0);
        let mut guard = self.ingress.lock().map_err(poisoned)?;
        let (ingress, ids) = &mut *guard;
        let inputs = match name {
            "order" => {
                let flags = if r["postOnly"] == true { json!(["POST_ONLY"]) } else { json!([]) };
                vec![wire::order(
                    account,
                    ids.fresh(),
                    json!({
                        "instrumentId": r["instrument"], "side": r["side"], "tick": r["tick"], "quantity": r["quantity"],
                        "enteredContract": r.get("contract").cloned().unwrap_or(json!("YES")),
                        "timeInForce": r.get("tif").cloned().unwrap_or(json!("GTC")),
                        "protectionTicks": r.get("protectionTicks").cloned().unwrap_or(json!(0)), "flags": flags
                    }),
                )]
            }
            "cancel" => vec![wire::cancel(
                account,
                r["instrument"].as_u64().ok_or("an instrument is required")?,
                r["orderId"].as_u64().ok_or("an order ID is required")?,
            )],
            "cancel-all" => vec![json!({
                "msg": "MassCancel", "sessionId": account * 10, "accountId": account, "scope": "ACCOUNT", "scopeRef": 0,
                "cause": "CLIENT_REQUEST"
            })],
            "grant" => vec![wire::command(
                "SleeveAdjust",
                ids.fresh(),
                json!({"accountId": account, "deltaCents": r["cents"], "reason": "GRANT"}),
            )],
            "oracle" => self.oracle(r, ids)?,
            _ => return Err(format!("no action {name}").into()),
        };
        let mut results = Vec::new();
        for input in inputs {
            results.extend(ingress.send(input)?);
        }
        Ok(json!({"results": results}))
    }

    /// The dummy oracle's per-event commands (blueprint §16): start, suspend, resume, end, close, resolve, void,
    /// and a new round of the same market.
    fn oracle(&self, r: &Value, ids: &mut Ids) -> Result<Vec<Value>, Error> {
        let event = r["event"].as_u64().ok_or("an event is required")?;
        let (tradable, next_round) = {
            let view = self.live.view.lock().map_err(poisoned)?;
            let e = view.events.get(&event).ok_or("no such event")?;
            let tradable: Vec<u64> = e
                .instruments
                .iter()
                .copied()
                .filter(|i| view.instruments.get(i).is_some_and(|i| matches!(i.state.as_str(), "OPEN" | "HALTED")))
                .collect();
            let template = event / 1000;
            let round = view.events.keys().filter(|id| **id / 1000 == template).map(|id| id % 1000).max();
            (tradable, round.unwrap_or(0) + 1)
        };
        let phase = |phase: &str, ids: &mut Ids| {
            wire::command("SetEventPhase", ids.fresh(), json!({"eventId": event, "phase": phase, "source": "OPERATOR"}))
        };
        let resolve = |kind: &str, state: u64, ids: &mut Ids| {
            wire::command(
                "ResolveEvent",
                ids.fresh(),
                json!({"eventId": event, "kind": kind, "caseId": 0, "winningState": state, "voidWeights": []}),
            )
        };
        Ok(match r["action"].as_str().unwrap_or("") {
            "start" => vec![phase("STARTED", ids)],
            "suspend" => vec![phase("SUSPENDED", ids)],
            "resume" => vec![phase("RESUMED", ids)],
            "end" => vec![phase("ENDED", ids)],
            "close" => tradable
                .iter()
                .map(|i| wire::command("SetMarketState", ids.fresh(), json!({"instrumentId": i, "state": "CLOSED"})))
                .collect(),
            "resolve" => vec![resolve("RESOLVED", r["state"].as_u64().ok_or("a winning state is required")?, ids)],
            "void" => vec![resolve("VOID", 0, ids)],
            "relist" => self.catalogue.listing(event / 1000, next_round)?,
            other => return Err(format!("no oracle action {other}").into()),
        })
    }
}

pub fn serve(listen: &str, web: Arc<Web>) -> Result<(), Error> {
    let listener = TcpListener::bind(listen)?;
    eprintln!("web page on http://{}", listener.local_addr()?);
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let web = Arc::clone(&web);
        thread::spawn(move || {
            if let Err(e) = handle(&web, stream) {
                eprintln!("web: {e}");
            }
        });
    }
    Ok(())
}

fn handle(web: &Web, stream: TcpStream) -> Result<(), Error> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            }
        }
    }
    if length > 1 << 20 {
        return respond(stream, 413, "text/plain", b"too large");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    match (method, path) {
        ("GET", "/") => respond(stream, 200, "text/html; charset=utf-8", INDEX.as_bytes()),
        ("GET", "/api/state") => respond(stream, 200, "application/json", web.snapshot()?.to_string().as_bytes()),
        ("GET", "/api/events") => events(web, stream),
        ("POST", p) if p.starts_with("/api/") => {
            let request: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(&body)? };
            match web.action(&p[5..], &request) {
                Ok(v) => respond(stream, 200, "application/json", v.to_string().as_bytes()),
                Err(e) => {
                    respond(stream, 400, "application/json", json!({"error": e.to_string()}).to_string().as_bytes())
                }
            }
        }
        _ => respond(stream, 404, "text/plain", b"not found"),
    }
}

fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<(), Error> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    Ok(())
}

/// Server-sent events: a snapshot whenever the view changes, at most five a second, and a comment every 15 s
/// otherwise so proxies keep the connection open.
fn events(web: &Web, mut stream: TcpStream) -> Result<(), Error> {
    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-store\r\n\r\n")?;
    let mut seen = u64::MAX;
    loop {
        let snapshot = {
            let guard = web.live.view.lock().map_err(poisoned)?;
            let (view, _) = web
                .live
                .changed
                .wait_timeout_while(guard, Duration::from_secs(15), |v| v.version == seen)
                .map_err(poisoned)?;
            (view.version != seen).then(|| (view.version, view.snapshot(&web.catalogue)))
        };
        match snapshot {
            Some((version, s)) => {
                write!(stream, "data: {s}\n\n")?;
                seen = version;
            }
            None => write!(stream, ": keep-alive\n\n")?,
        }
        stream.flush()?;
        thread::sleep(Duration::from_millis(200));
    }
}
