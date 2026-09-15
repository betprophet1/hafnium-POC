//! Just enough HTTP/1.1 for the REST half: one request per connection, then close.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpStream;

const MAX_LINE: u64 = 8192;
const MAX_HEADERS: usize = 100;
const MAX_BODY: usize = 1 << 20;

pub struct Request {
    pub method: String,
    pub path: String,
    pub query: BTreeMap<String, String>,
    /// Header names in lower case.
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message.to_string())
}

pub fn read(stream: &TcpStream) -> io::Result<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.by_ref().take(MAX_LINE).read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/"));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query =
        query.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    let mut headers = BTreeMap::new();
    for _ in 0..MAX_HEADERS {
        let mut header = String::new();
        if reader.by_ref().take(MAX_LINE).read_line(&mut header)? == 0 || header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length = match headers.get("content-length") {
        Some(v) => v.parse::<usize>().map_err(|_| invalid("bad content-length"))?,
        None => 0,
    };
    if length > MAX_BODY {
        return Err(invalid("the body is too large"));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    Ok(Request { method, path: path.to_string(), query, headers, body })
}

pub fn respond(mut stream: &TcpStream, status: u16, body: &Value, headers: &[(&str, String)]) -> io::Result<()> {
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
    stream.write_all(head.as_bytes())?;
    stream.write_all(text.as_bytes())
}
