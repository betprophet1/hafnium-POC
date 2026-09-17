//! A client of the gateway's REST API (interfaces/gateway/openapi.yaml), as the synthetic traders use it: one
//! connection per call, a bearer token, JSON in and out.

use serde_json::Value;
use std::io::{Read, Write};
use venue_link::wire::Error;

pub struct Api {
    addr: String,
    /// Where market data (`/v4/markets` and under it) is asked for: the public half, which may be another gateway.
    public: String,
    token: String,
}

pub struct Response {
    pub status: u16,
    pub body: Value,
}

impl Response {
    /// The gateway's refusal, for a response other than 200.
    pub fn refusal(&self) -> String {
        let error = &self.body["error"];
        format!("{} {}: {}", self.status, error["code"].as_str().unwrap_or(""), error["message"].as_str().unwrap_or(""))
    }
}

impl Api {
    pub fn new(addr: &str, token: &str) -> Self {
        Self { addr: addr.to_string(), public: addr.to_string(), token: token.to_string() }
    }

    /// The same, asking the gateway at `public` for market data: a venue may serve the public half apart from the
    /// private one (interfaces/gateway/README.md, public and private).
    pub fn with_public(mut self, public: &str) -> Self {
        self.public = public.to_string();
        self
    }

    pub fn get(&self, path: &str) -> Result<Response, Error> {
        self.call("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &Value) -> Result<Response, Error> {
        self.call("POST", path, Some(body))
    }

    pub fn patch(&self, path: &str, body: &Value) -> Result<Response, Error> {
        self.call("PATCH", path, Some(body))
    }

    pub fn delete(&self, path: &str) -> Result<Response, Error> {
        self.call("DELETE", path, None)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Response, Error> {
        let addr = if path.starts_with("/v4/markets") { &self.public } else { &self.addr };
        let mut stream = crate::session::connect(addr)?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: gateway\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            self.token,
            body.len()
        );
        stream.write_all(request.as_bytes())?;
        let mut response = String::new();
        stream.read_to_string(&mut response)?;
        let (head, payload) = response.split_once("\r\n\r\n").ok_or("the gateway sent a malformed response")?;
        let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("the gateway sent no status")?;
        let body = if payload.trim().is_empty() { Value::Null } else { serde_json::from_str(payload)? };
        Ok(Response { status, body })
    }
}
