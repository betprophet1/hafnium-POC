//! The REST half of interface 1 (interfaces/gateway/openapi.yaml): what a request comes to. `reactor.rs` reads the
//! request and writes the response; nothing here waits, and a request that goes to the node is answered when the
//! node answers it.

use crate::api::{parse_id, ApiError, Gateway, Pending};
use crate::http::{self, Request};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use venue_link::submit::Notify;

/// What a request comes to: its answer now, or a request given to the node whose answer `Gateway::finish` has once
/// the notify it was given is called.
pub enum Routed {
    Now(Result<Value, ApiError>),
    Later(Pending),
}

/// The response for an answer.
pub fn response(answer: Result<Value, ApiError>) -> Vec<u8> {
    match answer {
        Ok(body) => http::response(200, &body, &[]),
        Err(e) => {
            let retry: Vec<(&str, String)> =
                e.retry_after.map(|s| ("Retry-After", s.to_string())).into_iter().collect();
            http::response(e.status, &e.body(), &retry)
        }
    }
}

/// The response to bytes that can never be a request.
pub fn unreadable(reason: &str) -> Vec<u8> {
    response(Err(ApiError::bad(format!("unreadable request: {reason}"))))
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

/// What `request` comes to. A request for the node is given `notify` to call once its answer can be taken.
pub fn route(gateway: &Gateway, request: &Request, notify: &Notify) -> Routed {
    route_or_refuse(gateway, request, notify).unwrap_or_else(|e| Routed::Now(Err(e)))
}

fn route_or_refuse(gateway: &Gateway, request: &Request, notify: &Notify) -> Result<Routed, ApiError> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let later = || Some(Arc::clone(notify));
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["healthz"]) => Ok(Routed::Now(if gateway.hub().ready() {
            Ok(json!({"ready": true}))
        } else {
            Err(ApiError::unavailable("catching up with the feed"))
        })),
        ("GET", ["v4", "markets"]) => Ok(Routed::Now(Ok(gateway.hub().markets()))),
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
            Ok(Routed::Now(gateway.hub().book(instrument, depth).ok_or_else(|| ApiError::not_found("no such market"))))
        }
        ("GET", ["v4", "account"]) => {
            let account = account(gateway, request)?;
            Ok(Routed::Now(Ok(gateway.hub().account(account))))
        }
        ("POST", ["v4", "orders"]) => {
            let account = account(gateway, request)?;
            Ok(Routed::Later(gateway.place_pending(account, rest_session(account), &body(request)?, later())?))
        }
        ("DELETE", ["v4", "orders"]) => {
            let account = account(gateway, request)?;
            let scope: Map<String, Value> =
                request.query.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
            let body = Value::Object(scope);
            Ok(Routed::Later(gateway.cancel_all_pending(account, rest_session(account), &body, later())?))
        }
        ("PATCH", ["v4", "orders", order]) => {
            let account = account(gateway, request)?;
            let order = parse_id(order, "orderId")?;
            Ok(Routed::Later(gateway.amend_pending(account, rest_session(account), order, &body(request)?, later())?))
        }
        ("DELETE", ["v4", "orders", order]) => {
            let account = account(gateway, request)?;
            let order = parse_id(order, "orderId")?;
            Ok(Routed::Later(gateway.cancel_pending(account, rest_session(account), order, later())?))
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
    fn a_request_that_arrives_in_pieces_is_answered_once_whole() {
        let venue = testing::venue("rest-pieces");
        let mut stream = TcpStream::connect(&venue.address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(5))).expect("a read timeout");
        let body = order("pieces");
        let whole = format!(
            "POST /v4/orders HTTP/1.1\r\nAuthorization: Bearer {MAKER_TOKEN}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        // Cut inside what could still be a WebSocket upgrade's first bytes, inside the head, and inside the body.
        let cuts = [0, 3, 40, whole.len() - 5, whole.len()];
        for piece in cuts.windows(2) {
            stream.write_all(&whole.as_bytes()[piece[0]..piece[1]]).expect("a piece is sent");
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("a whole response before the timeout");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"accepted\""), "{response}");
    }

    #[test]
    fn many_orders_at_once_are_each_answered() {
        let venue = testing::venue("rest-many");
        let answers: Vec<_> = (0..40)
            .map(|i| {
                let address = venue.address.clone();
                std::thread::spawn(move || {
                    request(&address, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order(&format!("many{i}"))))
                })
            })
            .collect();
        for answer in answers {
            let (status, placed) = answer.join().expect("a request thread");
            assert_eq!((status, placed["status"].as_str()), (200, Some("accepted")), "{placed}");
        }
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
