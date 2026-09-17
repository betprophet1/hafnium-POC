//! The internal HTTP port a process serves its metrics on, and optionally read-only operator endpoints beside them.
//!
//! Never the client-facing listener: a separate address, which a deployment keeps to its monitoring network. One
//! thread answers one request at a time (a scrape every few seconds, an operator now and then) and closes each
//! connection; a slow client holds it for at most `TIMEOUT`.

use crate::log;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);
const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// An answer to a path this module does not serve itself: status, content type and body.
pub struct Answer {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Answer {
    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self { status, content_type: "application/json", body: body.to_string().into_bytes() }
    }
}

/// Answers `GET path?query`, or `None` for a path it does not serve (a 404).
pub type Handler = Arc<dyn Fn(&str, &str) -> Option<Answer> + Send + Sync>;

/// Binds `address` and serves `GET /metrics` from `registry`, `GET /healthz`, and whatever `handler` answers, on a
/// thread of its own. Binding happens before this returns, so a process given an address it cannot have says so.
pub fn spawn(address: &str, registry: Registry, handler: Option<Handler>) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(address)?;
    let bound = listener.local_addr()?;
    thread::Builder::new().name("metrics".into()).spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            if let Err(e) = answer(stream, &registry, handler.as_ref()) {
                if e.kind() != io::ErrorKind::WouldBlock && e.kind() != io::ErrorKind::TimedOut {
                    log::warn("metrics request failed", &[("error", &e)]);
                }
            }
        }
    })?;
    Ok(bound)
}

fn answer(stream: TcpStream, registry: &Registry, handler: Option<&Handler>) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.by_ref().take(8192).read_line(&mut line)?;
    // The rest of the head is not needed; read it so the client is not reset before it has our answer.
    let mut header = String::new();
    while reader.by_ref().take(8192).read_line(&mut header)? > 2 {
        header.clear();
    }
    let mut words = line.split_whitespace();
    let (method, target) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let reply = if method != "GET" {
        Answer { status: 405, content_type: "text/plain", body: b"only GET\n".to_vec() }
    } else if path == "/metrics" {
        let mut text = String::new();
        match encode(&mut text, registry) {
            Ok(()) => Answer { status: 200, content_type: CONTENT_TYPE, body: text.into_bytes() },
            Err(_) => Answer { status: 500, content_type: "text/plain", body: b"encoding failed\n".to_vec() },
        }
    } else if path == "/healthz" {
        Answer { status: 200, content_type: "text/plain", body: b"ok\n".to_vec() }
    } else {
        handler.and_then(|h| h(path, query)).unwrap_or(Answer {
            status: 404,
            content_type: "text/plain",
            body: b"not found\n".to_vec(),
        })
    };
    let reason = match reply.status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let mut out = stream;
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reply.status,
        reply.content_type,
        reply.body.len()
    );
    out.write_all(head.as_bytes())?;
    out.write_all(&reply.body)?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus_client::metrics::counter::Counter;

    fn get(address: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(address).expect("connected");
        write!(stream, "GET {target} HTTP/1.1\r\nHost: test\r\n\r\n").expect("sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    #[test]
    fn metrics_health_and_a_handlers_paths_are_served_and_nothing_else() {
        let mut registry = Registry::default();
        let counter = Counter::<u64>::default();
        counter.inc_by(7);
        registry.register("things", "things seen", counter);
        let handler: Handler = Arc::new(|path, query| {
            (path == "/v1/echo").then(|| Answer::json(200, &serde_json::json!({"query": query})))
        });
        let address = spawn("127.0.0.1:0", registry, Some(handler)).expect("bound");
        let metrics = get(address, "/metrics");
        assert!(metrics.starts_with("HTTP/1.1 200 OK\r\n"), "{metrics}");
        assert!(metrics.contains("application/openmetrics-text"), "{metrics}");
        assert!(metrics.contains("things_total 7\n"), "{metrics}");
        assert!(get(address, "/healthz").ends_with("ok\n"));
        assert!(get(address, "/v1/echo?depth=3").ends_with("{\"query\":\"depth=3\"}"));
        assert!(get(address, "/v1/other").starts_with("HTTP/1.1 404"));
    }
}
