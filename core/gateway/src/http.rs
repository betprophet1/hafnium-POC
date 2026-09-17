//! Just enough HTTP/1.1 for the REST half: one request per connection, answered, then the connection closes.
//!
//! A request is parsed from whatever bytes have arrived so far, because the event loops read it as it comes and
//! never wait for the rest of it.

use serde_json::Value;
use std::collections::BTreeMap;

/// The longest request line; `reactor.rs` reads no further than this to tell what a connection is.
pub const MAX_LINE: usize = 8192;
const MAX_HEADERS: usize = 100;
/// The most the request line and its headers may take together.
const MAX_HEAD: usize = MAX_LINE * (MAX_HEADERS + 1);
const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Header names in lower case.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

/// Where the head ends and the body starts: at the first empty line, whichever way lines end.
fn head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4));
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// A whole request from the bytes read so far: `Ok(None)` while more are needed, `Err` when these can never be one.
pub fn parse(buf: &[u8]) -> Result<Option<Request>, String> {
    let Some((end, body_start)) = head_end(buf) else {
        return if buf.len() > MAX_HEAD { Err("the request head is too large".into()) } else { Ok(None) };
    };
    if end > MAX_HEAD {
        return Err("the request head is too large".into());
    }
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| "the request head is not text".to_string())?;
    let mut lines = head.lines();
    let line = lines.next().unwrap_or("");
    if line.len() > MAX_LINE {
        return Err("the request line is too long".into());
    }
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query =
        query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for header in lines.take(MAX_HEADERS) {
        if let Some((name, value)) = header.split_once(':') {
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            match headers.get_mut(&name) {
                // A list a proxy may have added a line of its own to: every line counts, in order (`forwarded.rs`).
                Some(earlier) if name == "x-forwarded-for" || name == "forwarded" => {
                    earlier.push_str(", ");
                    earlier.push_str(value);
                }
                _ => {
                    headers.insert(name, value.to_string());
                }
            }
        }
    }
    let length = match headers.get("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| "bad content-length".to_string())?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err("the body is too large".into());
    }
    let Some(body) = buf.get(body_start..body_start + length) else { return Ok(None) };
    Ok(Some(Request { method, path: path.to_string(), query, headers, body: body.to_vec() }))
}

/// The methods and headers a browser is told it may use, which is what the API takes (interface 1 README, browsers).
const ALLOW_METHODS: &str = "GET, POST, PATCH, DELETE, OPTIONS";
const ALLOW_HEADERS: &str = "authorization, content-type";
/// How long a browser may keep a preflight answer: ten minutes, so a session costs one of them.
const PREFLIGHT_MAX_AGE: u64 = 600;

/// A response with the headers that let `origin` read it. The origin is echoed rather than `*` because a client
/// sends its token in a header, and an allow-list of one is easier to reason about than a wildcard; `Vary` keeps a
/// cache from answering one origin with another's response.
pub fn with_cors(response: Vec<u8>, origin: &str) -> Vec<u8> {
    let headers = format!(
        "Access-Control-Allow-Origin: {origin}\r\nAccess-Control-Expose-Headers: Retry-After\r\nVary: Origin\r\n"
    );
    let Some(end) = response.windows(2).position(|w| w == b"\r\n") else { return response };
    let mut out = Vec::with_capacity(response.len() + headers.len());
    out.extend_from_slice(&response[..end + 2]);
    out.extend_from_slice(headers.as_bytes());
    out.extend_from_slice(&response[end + 2..]);
    out
}

/// The answer to a browser's preflight: what it may send, and for how long it need not ask again.
pub fn preflight(origin: &str) -> Vec<u8> {
    let head = format!(
        "HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: {origin}\r\nAccess-Control-Allow-Methods: \
         {ALLOW_METHODS}\r\nAccess-Control-Allow-Headers: {ALLOW_HEADERS}\r\nAccess-Control-Max-Age: \
         {PREFLIGHT_MAX_AGE}\r\nVary: Origin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    head.into_bytes()
}

/// A whole response, ready to write.
pub fn response(status: u16, body: &Value, headers: &[(&str, String)]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let text = body.to_string();
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n",
        text.len()
    );
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(text.as_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn a_request_is_whole_only_once_its_head_and_body_have_arrived() {
        let full = b"POST /v4/orders?x=1 HTTP/1.1\r\nContent-Length: 4\r\nAuthorization: Bearer t\r\n\r\nbody";
        for cut in 0..full.len() {
            assert!(parse(&full[..cut]).expect("a prefix is not an error").is_none(), "whole at {cut} bytes");
        }
        let request = parse(full).expect("parsed").expect("whole");
        assert_eq!((request.method.as_str(), request.path.as_str()), ("POST", "/v4/orders"));
        assert_eq!(request.query.get("x").map(String::as_str), Some("1"));
        assert_eq!(request.headers.get("authorization").map(String::as_str), Some("Bearer t"));
        assert_eq!(request.body, b"body");
    }

    #[test]
    fn every_line_of_a_forwarding_header_is_kept_in_order() {
        let head =
            b"GET / HTTP/1.1\r\nX-Forwarded-For: 1.2.3.4\r\nx-forwarded-for: 10.0.0.1\r\nHost: a\r\nHost: b\r\n\r\n";
        let request = parse(head).expect("parsed").expect("whole");
        assert_eq!(request.headers.get("x-forwarded-for").map(String::as_str), Some("1.2.3.4, 10.0.0.1"));
        assert_eq!(request.headers.get("host").map(String::as_str), Some("b"), "any other header's last line counts");
    }

    #[test]
    fn requests_that_can_never_be_whole_are_refused() {
        assert!(parse(b"GET / HTTP/1.1\r\nContent-Length: nope\r\n\r\n").is_err());
        assert!(parse(b"GET / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n").is_err());
        assert!(parse(&vec![b'a'; 8192 * 102]).is_err());
    }
}
