//! Just enough HTTP/1.1 for the REST half: one request per connection, answered, then the connection closes.
//!
//! A request is parsed from whatever bytes have arrived so far, because the event loops read it as it comes and
//! never wait for the rest of it.

use serde_json::Value;
use std::collections::BTreeMap;

const MAX_LINE: usize = 8192;
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
    let mut headers = BTreeMap::new();
    for header in lines.take(MAX_HEADERS) {
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
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

/// A whole response, ready to write.
pub fn response(status: u16, body: &Value, headers: &[(&str, String)]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
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
    fn requests_that_can_never_be_whole_are_refused() {
        assert!(parse(b"GET / HTTP/1.1\r\nContent-Length: nope\r\n\r\n").is_err());
        assert!(parse(b"GET / HTTP/1.1\r\nContent-Length: 99999999\r\n\r\n").is_err());
        assert!(parse(&vec![b'a'; 8192 * 102]).is_err());
    }
}
