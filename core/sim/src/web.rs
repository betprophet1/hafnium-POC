//! The mini venue's web page. Development only: nothing here is authenticated, so never expose it outside a laptop
//! or a test cluster.
//!
//! Trading goes through the gateway: this server passes `/v4` through to it untouched, WebSocket upgrades included,
//! and hands the page the dev tokens of the accounts it shows. Settlement and grants are operator actions, sequenced
//! as commands through the core's ingress. The dummy oracle here sequences directly; blueprint §16 routes a dummy
//! resolution through a proposal, the resolution console and an auto-approver, which do not exist yet.

use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::view::Live;
use venue_link::wire::{self, Error, Ids, Ingress};

const INDEX: &str = include_str!("index.html");

/// How long a `/v4` pass-through may stay silent before it is closed, so a gateway that vanished without a reset
/// cannot leave a request hanging or a WebSocket that looks open. A REST answer comes at once; the page's WebSocket
/// sessions heartbeat every 15 s and the gateway answers each one, so a healthy session is never silent for 45 s.
const REST_SILENCE: Duration = Duration::from_secs(30);
const SESSION_SILENCE: Duration = Duration::from_secs(45);

pub struct Web {
    catalogue: Catalogue,
    live: Arc<Live>,
    ingress: Mutex<(Ingress, Ids)>,
    gateway: String,
}

fn poisoned<T>(_: T) -> Error {
    "a lock is poisoned".into()
}

impl Web {
    pub fn new(catalogue: Catalogue, live: Arc<Live>, ingress: Ingress, ids: Ids, gateway: String) -> Self {
        Self { catalogue, live, ingress: Mutex::new((ingress, ids)), gateway }
    }

    fn snapshot(&self) -> Result<Value, Error> {
        Ok(self.live.view.lock().map_err(poisoned)?.snapshot(&self.catalogue))
    }

    /// The accounts the page's trading tabs act as, with their dev tokens.
    fn config(&self) -> Value {
        let accounts: Vec<Value> = self
            .catalogue
            .accounts()
            .iter()
            .filter(|a| !a["tab"].is_null())
            .map(|a| json!({"id": a["id"], "name": a["name"], "tab": a["tab"], "token": a["token"]}))
            .collect();
        json!({"accounts": accounts})
    }

    /// Operator actions: funding an account, and the dummy oracle.
    fn action(&self, name: &str, r: &Value) -> Result<Value, Error> {
        let mut guard = self.ingress.lock().map_err(poisoned)?;
        let (ingress, ids) = &mut *guard;
        let inputs = match name {
            "grant" => vec![wire::command(
                "SleeveAdjust",
                ids.fresh(),
                json!({"accountId": r["account"], "deltaCents": r["cents"], "reason": "GRANT"}),
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
    eprintln!("web page on http://{}, gateway at {}", listener.local_addr()?, web.gateway);
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
    let mut head = String::new();
    reader.read_line(&mut head)?;
    let (method, path) = {
        let mut parts = head.split_whitespace();
        (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string())
    };
    let mut length = 0usize;
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        head.push_str(&header);
        if header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse()?;
            }
        }
    }
    if path.starts_with("/v4") {
        return proxy(&web.gateway, &head, reader, stream);
    }
    if length > 1 << 20 {
        return respond(stream, 413, "text/plain", b"too large");
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    match (method.as_str(), path.as_str()) {
        ("GET", "/") => respond(stream, 200, "text/html; charset=utf-8", INDEX.as_bytes()),
        ("GET", "/api/config") => respond(stream, 200, "application/json", web.config().to_string().as_bytes()),
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

/// Passes a request and everything after it through to the gateway, in both directions, until either side closes.
fn proxy(
    gateway: &str,
    head: &str,
    mut from_client: BufReader<TcpStream>,
    mut to_client: TcpStream,
) -> Result<(), Error> {
    // Connecting is bounded, like the bots' connections.
    let upstream = match crate::session::connect(gateway) {
        Ok(s) => s,
        Err(e) => {
            let body =
                json!({"error": {"code": "UNAVAILABLE", "message": format!("the gateway is not reachable: {e}")}});
            return respond(to_client, 502, "application/json", body.to_string().as_bytes());
        }
    };
    let websocket = head.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("upgrade") && value.trim().eq_ignore_ascii_case("websocket")
        })
    });
    let silence = if websocket { SESSION_SILENCE } else { REST_SILENCE };
    let _ = upstream.set_nodelay(true);
    upstream.set_read_timeout(Some(silence))?;
    from_client.get_ref().set_read_timeout(Some(silence))?;
    to_client.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut from_gateway = upstream.try_clone()?;
    let mut to_gateway = upstream;
    to_gateway.write_all(head.as_bytes())?;
    let forward = thread::spawn(move || {
        let _ = io::copy(&mut from_client, &mut to_gateway);
        let _ = to_gateway.shutdown(Shutdown::Write);
    });
    let _ = io::copy(&mut from_gateway, &mut to_client);
    let _ = to_client.shutdown(Shutdown::Both);
    let _ = forward.join();
    Ok(())
}

fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<(), Error> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        413 => "Payload Too Large",
        502 => "Bad Gateway",
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

/// Server-sent events for the settlement tab: a snapshot whenever the view changes, at most five a second, and a
/// comment every 15 s otherwise so proxies keep the connection open.
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
