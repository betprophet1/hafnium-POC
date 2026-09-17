//! The mini venue's web page. Development only: nothing here is authenticated, so never expose it outside a laptop
//! or a test cluster.
//!
//! Trading goes through the gateway: this server passes `/v4` through to it untouched, WebSocket upgrades included,
//! and hands the page the dev tokens of the accounts it shows, so deposits and withdrawals go through the gateway to
//! the treasury too. Settlement goes through the dummy oracle (`venue-sim oracle`), whose control API is passed
//! through under `/oracle` the same way, so a resolution from the page takes the oracle's proposal, sign-off and
//! dispute window like any other. Nothing here reaches the core's ingress: accounts are funded by deposits (blueprint
//! §16: demo balances come from a fake provider, never a ledger shortcut). The treasury's reconciliation summary, and
//! its request to run now, are passed through under `/treasury`; nothing else of the treasury's own API is, because it
//! trusts the account a request names and only the gateway may speak for one.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::catalogue::Catalogue;
use venue_link::view::Live;
use venue_link::wire::Error;

const INDEX: &str = include_str!("index.html");

/// How long a `/v4` pass-through may stay silent before it is closed, so a gateway that vanished without a reset
/// cannot leave a request hanging or a WebSocket that looks open. A REST answer comes at once; the page's WebSocket
/// sessions heartbeat every 15 s and the gateway answers each one, so a healthy session is never silent for 45 s.
const REST_SILENCE: Duration = Duration::from_secs(30);
const SESSION_SILENCE: Duration = Duration::from_secs(45);

pub struct Web {
    catalogue: Catalogue,
    live: Arc<Live>,
    gateway: String,
    oracle: String,
    treasury: String,
}

/// Settled rounds of each market the page's event stream still carries, latest first: enough to label recent payouts,
/// while a venue that settles rounds all day does not send every one it ever listed five times a second.
const RECENT_SETTLED: usize = 3;

fn poisoned<T>(_: T) -> Error {
    "a lock is poisoned".into()
}

impl Web {
    pub fn new(catalogue: Catalogue, live: Arc<Live>, gateway: String, oracle: String, treasury: String) -> Self {
        Self { catalogue, live, gateway, oracle, treasury }
    }

    fn snapshot(&self) -> Result<Value, Error> {
        Ok(trimmed(self.live.view.lock().map_err(poisoned)?.snapshot(&self.catalogue)))
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
}

/// A snapshot with only the unresolved events and each market's latest few settled rounds.
fn trimmed(mut snapshot: Value) -> Value {
    if let Some(events) = snapshot["events"].as_array_mut() {
        let settled = |e: &Value| (e["resolved"] == true).then(|| (e["template"].as_u64(), e["round"].as_u64()));
        // Each template's oldest settled round still kept.
        let mut rounds: BTreeMap<Option<u64>, Vec<Option<u64>>> = BTreeMap::new();
        for (template, round) in events.iter().filter_map(settled) {
            rounds.entry(template).or_default().push(round);
        }
        let oldest: BTreeMap<Option<u64>, Option<u64>> = rounds
            .into_iter()
            .map(|(template, mut r)| {
                r.sort_unstable_by(|a, b| b.cmp(a));
                (template, r.get(RECENT_SETTLED - 1).copied().flatten())
            })
            .collect();
        events.retain(|e| {
            settled(e).is_none_or(|(template, round)| {
                oldest.get(&template).copied().flatten().is_none_or(|o| round >= Some(o))
            })
        });
    }
    snapshot
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
        return proxy(&web.gateway, "gateway", &head, reader, stream);
    }
    if path == "/oracle" || path.starts_with("/oracle/") {
        return proxy(&web.oracle, "dummy oracle", &head, reader, stream);
    }
    // Only the reconciliation summary and a run on demand, with /treasury taken off the path.
    if let ("GET", "/treasury/v1/reconciliation") | ("POST", "/treasury/v1/reconciliation/runs") =
        (method.as_str(), path.as_str())
    {
        let head = head.replacen("/treasury/v1/", "/v1/", 1);
        return proxy(&web.treasury, "treasury", &head, reader, stream);
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
        _ => respond(stream, 404, "text/plain", b"not found"),
    }
}

/// Passes a request and everything after it through to `upstream` (the gateway or the dummy oracle), in both
/// directions, until either side closes.
fn proxy(
    gateway: &str,
    name: &str,
    head: &str,
    mut from_client: BufReader<TcpStream>,
    mut to_client: TcpStream,
) -> Result<(), Error> {
    // Connecting is bounded, like the bots' connections.
    let upstream = match crate::session::connect(gateway) {
        Ok(s) => s,
        Err(e) => {
            let body =
                json!({"error": {"code": "UNAVAILABLE", "message": format!("the {name} is not reachable: {e}")}});
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

pub(crate) fn respond(mut stream: TcpStream, status: u16, content_type: &str, body: &[u8]) -> Result<(), Error> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_stream_keeps_open_events_and_each_markets_latest_settled_rounds() {
        let event = |template: u64, round: u64, resolved: bool| json!({"template": template, "round": round, "resolved": resolved});
        let mut events: Vec<Value> = (1..=6).map(|r| event(1, r, true)).collect();
        events.extend((1..=2).map(|r| event(2, r, true)));
        events.push(event(1, 7, false));
        events.push(event(2, 3, false));
        let kept = trimmed(json!({"events": events}));
        let rounds: Vec<(u64, u64)> = kept["events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| (e["template"].as_u64().unwrap_or(0), e["round"].as_u64().unwrap_or(0)))
            .collect();
        assert_eq!(rounds, vec![(1, 4), (1, 5), (1, 6), (2, 1), (2, 2), (1, 7), (2, 3)]);
    }
}
