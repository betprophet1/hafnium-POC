//! The REST half of interface 1 (interfaces/gateway/openapi.yaml).

use crate::api::{parse_id, ApiError, Gateway};
use crate::http::{self, Request};
use serde_json::{json, Map, Value};
use std::net::TcpStream;

pub fn handle(gateway: &Gateway, stream: &TcpStream) {
    let outcome = http::read(stream)
        .map_err(|e| ApiError::bad(format!("unreadable request: {e}")))
        .and_then(|request| route(gateway, &request));
    let written = match outcome {
        Ok(body) => http::respond(stream, 200, &body, &[]),
        Err(e) => {
            let retry: Vec<(&str, String)> =
                e.retry_after.map(|s| ("Retry-After", s.to_string())).into_iter().collect();
            http::respond(stream, e.status, &e.body(), &retry)
        }
    };
    if let Err(e) = written {
        eprintln!("gateway: rest: {e}");
    }
}

/// REST calls have no session. Their orders carry a fixed session ID per account, which no disconnect ever cancels.
const fn rest_session(account: u64) -> u64 {
    account * 10
}

fn account(gateway: &Gateway, request: &Request) -> Result<u64, ApiError> {
    let token = request
        .headers
        .get("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let account = gateway.authenticate(token.trim())?;
    gateway.rest_rate(account)?;
    Ok(account)
}

fn body(request: &Request) -> Result<Value, ApiError> {
    serde_json::from_slice(&request.body).map_err(|e| ApiError::bad(format!("the body is not JSON: {e}")))
}

fn route(gateway: &Gateway, request: &Request) -> Result<Value, ApiError> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["healthz"]) => {
            if gateway.hub().ready() {
                Ok(json!({"ready": true}))
            } else {
                Err(ApiError::unavailable("catching up with the feed"))
            }
        }
        ("GET", ["v4", "markets"]) => Ok(gateway.hub().markets()),
        ("GET", ["v4", "markets", instrument, "book"]) => {
            let instrument = parse_id(instrument, "instrumentId")?;
            let depth = match request.query.get("depth") {
                None => 20,
                Some(d) => d
                    .parse::<usize>()
                    .ok()
                    .filter(|d| (1..=100).contains(d))
                    .ok_or_else(|| ApiError::bad("depth must be from 1 to 100"))?,
            };
            gateway.hub().book(instrument, depth).ok_or_else(|| ApiError::not_found("no such market"))
        }
        ("GET", ["v4", "account"]) => {
            let account = account(gateway, request)?;
            Ok(gateway.hub().account(account))
        }
        ("POST", ["v4", "orders"]) => {
            let account = account(gateway, request)?;
            gateway.place(account, rest_session(account), &body(request)?)
        }
        ("DELETE", ["v4", "orders"]) => {
            let account = account(gateway, request)?;
            let scope: Map<String, Value> =
                request.query.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
            gateway.cancel_all(account, rest_session(account), &Value::Object(scope))
        }
        ("PATCH", ["v4", "orders", order]) => {
            let account = account(gateway, request)?;
            gateway.amend(account, rest_session(account), parse_id(order, "orderId")?, &body(request)?)
        }
        ("DELETE", ["v4", "orders", order]) => {
            let account = account(gateway, request)?;
            gateway.cancel(account, rest_session(account), parse_id(order, "orderId")?)
        }
        _ => Err(ApiError::not_found("no such route")),
    }
}
