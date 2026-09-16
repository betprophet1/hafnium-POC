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

/// What a REST client can rely on, against a gateway on a real node (`testing`). Like the WebSocket tests, these say
/// nothing about how requests are served.
#[cfg(test)]
mod tests {
    use crate::testing::{self, MAKER_TOKEN, MARKET};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// Sends one request and returns the status and the JSON body. The gateway answers once and closes.
    fn request(address: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n");
        if let Some(token) = token {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        let body = body.unwrap_or("");
        head.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
        stream.write_all(head.as_bytes()).expect("the request is sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("a whole response before the timeout");
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
        let json = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, serde_json::from_str(json).unwrap_or(Value::Null))
    }

    fn order(client_order_id: &str) -> String {
        json!({"clientOrderId": client_order_id, "instrumentId": MARKET.to_string(), "action": "buy",
               "outcome": "yes", "price": {"tick": 4}, "quantity": 2})
        .to_string()
    }

    #[test]
    fn public_reads_answer_without_a_token() {
        let venue = testing::venue("rest-reads");
        let (status, health) = request(&venue.address, "GET", "/healthz", None, None);
        assert_eq!((status, &health["ready"]), (200, &json!(true)), "{health}");
        let (status, markets) = request(&venue.address, "GET", "/v4/markets", None, None);
        assert_eq!(status, 200);
        assert!(markets["events"].as_array().is_some_and(|e| !e.is_empty()), "{markets}");
        let (status, book) = request(&venue.address, "GET", &format!("/v4/markets/{MARKET}/book?depth=5"), None, None);
        assert_eq!((status, book["instrumentId"].as_str()), (200, Some(MARKET.to_string().as_str())), "{book}");
        let (status, _) = request(&venue.address, "GET", "/v4/markets/99999/book", None, None);
        assert_eq!(status, 404);
    }

    #[test]
    fn the_account_needs_a_token() {
        let venue = testing::venue("rest-account");
        assert_eq!(request(&venue.address, "GET", "/v4/account", None, None).0, 401);
        assert_eq!(request(&venue.address, "GET", "/v4/account", Some("not-a-token"), None).0, 401);
        let (status, account) = request(&venue.address, "GET", "/v4/account", Some(MAKER_TOKEN), None);
        assert_eq!(status, 200, "{account}");
        assert!(account["availableCents"].as_str().is_some() || account["availableCents"].is_number(), "{account}");
    }

    #[test]
    fn an_order_is_placed_resent_amended_and_cancelled() {
        let venue = testing::venue("rest-orders");
        let a = &venue.address;
        let (status, placed) = request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order("r1")));
        assert_eq!((status, placed["status"].as_str()), (200, Some("accepted")), "{placed}");
        let id = placed["orderId"].as_str().expect("an order ID").to_string();
        let (status, resent) = request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order("r1")));
        assert_eq!((status, resent["status"].as_str()), (200, Some("duplicate")), "{resent}");
        assert_eq!(resent["orderId"].as_str(), Some(id.as_str()));
        let (status, amended) =
            request(a, "PATCH", &format!("/v4/orders/{id}"), Some(MAKER_TOKEN), Some(r#"{"quantity": 5}"#));
        assert_eq!((status, amended["status"].as_str()), (200, Some("amended")), "{amended}");
        let (status, cancelled) = request(a, "DELETE", &format!("/v4/orders/{id}"), Some(MAKER_TOKEN), None);
        assert_eq!((status, cancelled["status"].as_str()), (200, Some("cancelled")), "{cancelled}");
    }

    #[test]
    fn cancel_all_answers_with_what_it_cancelled() {
        let venue = testing::venue("rest-cancel-all");
        let a = &venue.address;
        for i in 0..3 {
            let (status, _) = request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order(&format!("ca{i}"))));
            assert_eq!(status, 200);
        }
        let (status, all) = request(a, "DELETE", &format!("/v4/orders?instrumentId={MARKET}"), Some(MAKER_TOKEN), None);
        assert_eq!(status, 200, "{all}");
        assert_eq!(all["executions"].as_array().map(Vec::len), Some(3), "{all}");
    }

    #[test]
    fn bad_requests_are_refused_with_reasons() {
        let venue = testing::venue("rest-bad");
        let a = &venue.address;
        assert_eq!(request(a, "GET", "/v4/nowhere", None, None).0, 404);
        let (status, not_json) = request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some("not json"));
        assert_eq!((status, not_json["error"]["code"].as_str()), (400, Some("BAD_REQUEST")), "{not_json}");
        let (status, _) = request(a, "POST", "/v4/orders", None, Some(&order("no-token")));
        assert_eq!(status, 401);
    }
}
