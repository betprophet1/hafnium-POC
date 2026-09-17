//! The parlay service's operator API, over HTTP on an address of its own (`--operator`), apart from the gateways'
//! port so that a deployment can keep it off every network a gateway is on. It trusts whoever reaches it.
//!
//! - `GET /v1/responders`: the approved responder accounts, each with its name and how many sessions it has subscribed
//! - `PUT /v1/responders/{accountId}`: approves a catalogue account as a responder
//! - `DELETE /v1/responders/{accountId}`: revokes one, ending its subscriptions
//! - `GET /healthz`: `{"ready": true}` once the service has caught up with the feed, 503 until then
//!
//! One thread per connection and one request per connection, like the treasury's: an operator's traffic is a few
//! calls a day.

use crate::service::{Failure, Service};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long a request has to arrive whole.
const ARRIVAL: Duration = Duration::from_secs(10);

/// Serves operators on `listener` for as long as the process runs.
pub fn serve(listener: TcpListener, service: Arc<Service>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let service = Arc::clone(&service);
            thread::spawn(move || {
                if let Err(e) = handle(&service, &stream) {
                    eprintln!("parlay service: operator: {e}");
                }
            });
        }
    });
}

/// The method and path of a request; its headers and any body are read past and ignored.
fn read(stream: &TcpStream) -> std::io::Result<(String, String)> {
    stream.set_read_timeout(Some(ARRIVAL))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").split('?').next().unwrap_or("/").to_string();
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
    }
    Ok((method, path))
}

fn handle(service: &Service, mut stream: &TcpStream) -> std::io::Result<()> {
    let (method, path) = read(stream)?;
    let (status, body) = match route(service, &method, &path) {
        Ok(body) => (200, body),
        Err(f) => (f.status, json!({"error": {"code": f.code, "message": f.message}})),
    };
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
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
}

fn failure(status: u16, code: &'static str, message: impl Into<String>) -> Failure {
    Failure { status, code, message: message.into() }
}

fn route(service: &Service, method: &str, path: &str) -> Result<Value, Failure> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["healthz"]) => {
            if service.ready() {
                Ok(json!({"ready": true}))
            } else {
                Err(failure(503, "UNAVAILABLE", "catching up with the feed"))
            }
        }
        ("GET", ["v1", "responders"]) => Ok(service.responders()),
        ("PUT" | "DELETE", ["v1", "responders", account]) => {
            let digits = !account.is_empty() && account.len() <= 20 && account.bytes().all(|b| b.is_ascii_digit());
            let account = digits
                .then(|| account.parse::<u64>().ok())
                .flatten()
                .ok_or_else(|| failure(400, "BAD_REQUEST", "accountId must be a decimal string"))?;
            if method == "PUT" {
                service.approve(account)
            } else {
                service.revoke(account)
            }
        }
        _ => Err(failure(404, "NOT_FOUND", "no such route")),
    }
}
