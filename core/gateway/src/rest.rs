//! The REST half of interface 1 (interfaces/gateway/openapi.yaml): what a request comes to. `reactor.rs` reads the
//! request and writes the response; nothing here waits, and a request that goes to the node is answered when the
//! node answers it.

use crate::api::{parse_id, ApiError, Gateway, Half, Pending};
use crate::http::{self, Request};
use crate::identity::{self, Credential};
use crate::treasury::Call;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use venue_identity::key::Permissions;
use venue_link::submit::Notify;

/// What a request comes to: its answer now, or a request given to the node whose answer `Gateway::finish` has once
/// the notify it was given is called.
pub enum Routed {
    Now(Result<Value, ApiError>),
    Later(Pending),
    /// A request passed to the treasury or the identity service, whose whole response `Call::take` has once the notify
    /// is called.
    Service(Call),
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

/// The request's credential, authenticated, within its flood limit, and holding `permission`.
fn credential(gateway: &Gateway, request: &Request, permission: Permissions) -> Result<Credential, ApiError> {
    let token = request
        .headers
        .get("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .ok_or_else(ApiError::unauthorized)?;
    let credential = gateway.authenticate(token.trim())?;
    gateway.rest_rate(credential.account)?;
    credential.require(permission)?;
    Ok(credential)
}

/// The account of a request whose credential holds `permission`.
fn account(gateway: &Gateway, request: &Request, permission: Permissions) -> Result<u64, ApiError> {
    credential(gateway, request, permission).map(|c| c.account)
}

/// Deposits and withdrawals: the account authenticated and, to move money, allowed to (reading takes only `read`),
/// then the request passed to the treasury as it came, which validates it and answers in interface 1's terms.
fn transfer(gateway: &Gateway, request: &Request, notify: &Notify) -> Result<Routed, ApiError> {
    let needs = if request.method == "GET" { Permissions::READ } else { Permissions::TRANSFERS };
    let account = account(gateway, request, needs)?;
    let treasury = gateway.treasury()?;
    if let Some(id) = request.path.trim_matches('/').split('/').nth(2) {
        parse_id(id, "transferId")?;
    }
    let query: Vec<String> = request.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let call =
        treasury.call(account, &request.method, &request.path, &query.join("&"), &request.body, Arc::clone(notify))?;
    Ok(Routed::Service(call))
}

fn body(request: &Request) -> Result<Value, ApiError> {
    serde_json::from_slice(&request.body).map_err(|e| ApiError::bad(format!("the body is not JSON: {e}")))
}

/// What `request` comes to. A request for the node is given `notify` to call once its answer can be taken.
pub fn route(gateway: &Gateway, request: &Request, notify: &Notify) -> Routed {
    route_or_refuse(gateway, request, notify).unwrap_or_else(|e| Routed::Now(Err(e)))
}

fn route_or_refuse(gateway: &Gateway, request: &Request, notify: &Notify) -> Result<Routed, ApiError> {
    // A path of the other half is not served here, as if it did not exist (README, public and private).
    if Half::of(&request.path).is_some_and(|half| half != gateway.half) {
        return Err(ApiError::not_found("no such route"));
    }
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let later = || Some(Arc::clone(notify));
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["healthz"]) => Ok(Routed::Now(if gateway.hub().ready() {
            Ok(json!({"ready": true}))
        } else {
            Err(ApiError::unavailable("catching up with the feed"))
        })),
        ("GET", ["v4", "markets"]) => {
            let resolved = match request.query.get("resolved").map(String::as_str) {
                None => None,
                Some("true") => Some(true),
                Some("false") => Some(false),
                Some(_) => return Err(ApiError::bad("resolved must be true or false")),
            };
            Ok(Routed::Now(Ok(gateway.hub().markets(resolved))))
        }
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
            let account = account(gateway, request, Permissions::READ)?;
            Ok(Routed::Now(Ok(gateway.hub().account(account))))
        }
        ("POST", ["v4", "orders"]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            Ok(Routed::Later(gateway.place_pending(account, rest_session(account), &body(request)?, later())?))
        }
        ("DELETE", ["v4", "orders"]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            let scope: Map<String, Value> =
                request.query.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect();
            let body = Value::Object(scope);
            Ok(Routed::Later(gateway.cancel_all_pending(account, rest_session(account), &body, later())?))
        }
        ("PATCH", ["v4", "orders", order]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            let order = parse_id(order, "orderId")?;
            Ok(Routed::Later(gateway.amend_pending(account, rest_session(account), order, &body(request)?, later())?))
        }
        ("DELETE", ["v4", "orders", order]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            let order = parse_id(order, "orderId")?;
            Ok(Routed::Later(gateway.cancel_pending(account, rest_session(account), order, later())?))
        }
        ("POST" | "GET", ["v4", "transfers"]) | ("GET" | "DELETE", ["v4", "transfers", _]) => {
            transfer(gateway, request, notify)
        }
        ("POST", ["v4", "rfqs"]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            Ok(Routed::Later(gateway.request_quote_pending(account, &body(request)?, later())?))
        }
        ("GET", ["v4", "rfqs"]) => {
            let account = account(gateway, request, Permissions::READ)?;
            Ok(Routed::Later(gateway.open_rfqs_pending(account, later())?))
        }
        ("GET", ["v4", "rfqs", rfq]) => {
            let account = account(gateway, request, Permissions::READ)?;
            Ok(Routed::Later(gateway.rfq_pending(account, rfq, later())?))
        }
        ("POST", ["v4", "rfqs", rfq, "quotes"]) => {
            let account = account(gateway, request, Permissions::TRADE)?;
            let mut body = body(request)?;
            if let Some(fields) = body.as_object_mut() {
                if fields.contains_key("requestId") {
                    return Err(ApiError::bad("unknown field requestId: the request is named by the path"));
                }
                fields.insert("requestId".into(), Value::String((*rfq).to_string()));
            }
            Ok(Routed::Later(gateway.attach_quote_pending(account, &body, later())?))
        }
        ("GET" | "POST", ["v4", "keys"]) | ("DELETE", ["v4", "keys", _]) => {
            let credential = credential(gateway, request, Permissions::KEYS)?;
            Ok(Routed::Service(identity::manage(gateway, &credential, request, notify)?))
        }
        _ => Err(ApiError::not_found("no such route")),
    }
}

/// What a REST client can rely on, against a gateway on a real node (`testing`). Like the WebSocket tests, these say
/// nothing about how requests are served.
#[cfg(test)]
mod tests {
    use crate::testing::{self, MAKER_TOKEN, MARKET, RETAIL_TOKEN};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    /// Sends one request and returns the status and the JSON body. The gateway answers once and closes.
    fn request(address: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
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

    /// POST /v4/rfqs as the retail customer. A parlay service refuses requests as unavailable while it is catching up
    /// with the feed, which on a loaded machine it can fall behind for a moment: that is asked again.
    fn ask_for_quotes(address: &str, body: &str) -> (u16, Value) {
        let mut answer = request(address, "POST", "/v4/rfqs", Some(RETAIL_TOKEN), Some(body));
        for _ in 0..300 {
            if answer.0 != 503 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            answer = request(address, "POST", "/v4/rfqs", Some(RETAIL_TOKEN), Some(body));
        }
        answer
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

    /// A settled event stays listed, says how it was resolved, and can be left out by a client that only trades.
    #[test]
    fn markets_say_how_an_event_was_resolved_and_can_leave_resolved_events_out() {
        let venue = testing::venue("rest-resolved");
        let event = venue_link::catalogue::event_id(1, 1);
        let resolve = venue_link::wire::command(
            "ResolveEvent",
            "test-resolve".into(),
            json!({"eventId": event, "kind": "RESOLVED", "caseId": 7, "winningState": 1, "voidWeights": []}),
        );
        venue_link::submit::Submit::spawn(&venue.ingress).send(resolve).expect("resolved");
        let listed = |query: &str| {
            let (status, markets) = request(&venue.address, "GET", &format!("/v4/markets{query}"), None, None);
            assert_eq!(status, 200, "{markets}");
            markets["events"].as_array().cloned().unwrap_or_default()
        };
        let named = event.to_string();
        let has = |events: &[Value]| events.iter().any(|e| e["eventId"].as_str() == Some(named.as_str()));
        testing::eventually("the feed shows the resolution", || has(&listed("?resolved=true")));
        let settled = listed("?resolved=true")
            .into_iter()
            .find(|e| e["eventId"].as_str() == Some(named.as_str()))
            .expect("listed");
        assert_eq!(settled["resolution"]["kind"], "resolved", "{settled}");
        assert_eq!(
            (settled["resolution"]["winningState"].as_u64(), settled["resolution"]["outcome"].as_str()),
            (Some(1), Some("Rain"))
        );
        assert!(!has(&listed("?resolved=false")));
        assert!(has(&listed("")));
        assert!(listed("?resolved=false").iter().all(|e| e["resolution"].is_null()));
        assert_eq!(request(&venue.address, "GET", "/v4/markets?resolved=maybe", None, None).0, 400);
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
    fn a_resting_order_in_the_account_says_how_it_was_placed_and_what_filled() {
        let venue = testing::venue("rest-account-orders");
        let a = &venue.address;
        let (status, placed) = request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order("ra1")));
        assert_eq!((status, placed["status"].as_str()), (200, Some("accepted")), "{placed}");
        let id = placed["orderId"].as_str().expect("an order ID").to_string();
        let sell = json!({"clientOrderId": "ra2", "instrumentId": MARKET.to_string(), "action": "sell",
                          "outcome": "yes", "price": {"tick": 4}, "quantity": 1, "timeInForce": "IOC"});
        let (status, sold) = request(a, "POST", "/v4/orders", Some(RETAIL_TOKEN), Some(&sell.to_string()));
        assert_eq!((status, sold["status"].as_str()), (200, Some("accepted")), "{sold}");
        let mut resting = Value::Null;
        testing::eventually("the fill is in the account", || {
            let (_, account) = request(a, "GET", "/v4/account", Some(MAKER_TOKEN), None);
            let found = account["orders"].as_array().and_then(|o| o.iter().find(|o| o["orderId"] == id.as_str()));
            resting = found.cloned().unwrap_or(Value::Null);
            resting["filled"] == 1
        });
        assert_eq!(resting["clientOrderId"], "ra1", "{resting}");
        assert_eq!((resting["quantity"].as_u64(), resting["remaining"].as_u64()), (Some(2), Some(1)), "{resting}");
        assert!(resting["filledYesCents"].as_u64().is_some_and(|c| c > 0), "{resting}");
        assert!(resting["eventId"].as_str().is_some_and(|e| e != "0"), "{resting}");
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
    fn a_gateway_serving_only_the_public_half_refuses_the_private_one() {
        let venue = testing::venue_serving("rest-public-only", true, false);
        let a = &venue.address;
        assert_eq!(request(a, "GET", "/healthz", None, None).0, 200);
        assert_eq!(request(a, "GET", "/v4/markets", None, None).0, 200);
        assert_eq!(request(a, "GET", "/v4/account", Some(MAKER_TOKEN), None).0, 404);
        assert_eq!(request(a, "POST", "/v4/orders", Some(MAKER_TOKEN), Some(&order("public-only"))).0, 404);
        assert_eq!(request(a, "GET", "/v4/transfers", Some(MAKER_TOKEN), None).0, 404);
        assert!(tungstenite::connect(&venue.ws_private).is_err(), "a public-only gateway took a private session");
        assert!(tungstenite::connect(&venue.ws_public).is_ok(), "a public-only gateway refused a public session");
    }

    #[test]
    fn a_gateway_serving_only_the_private_half_refuses_the_public_one() {
        let venue = testing::venue_serving("rest-private-only", false, true);
        let a = &venue.address;
        assert_eq!(request(a, "GET", "/healthz", None, None).0, 200);
        assert_eq!(request(a, "GET", "/v4/markets", None, None).0, 404);
        assert_eq!(request(a, "GET", &format!("/v4/markets/{MARKET}/book"), None, None).0, 404);
        assert_eq!(request(a, "GET", "/v4/account", Some(MAKER_TOKEN), None).0, 200);
        assert!(tungstenite::connect(&venue.ws_public).is_err(), "a private-only gateway took a public session");
        assert!(tungstenite::connect(&venue.ws_private).is_ok(), "a private-only gateway refused a private session");
    }

    fn legs(legs: &[(u64, &str)], quantity: u64) -> String {
        let legs: Vec<Value> = legs
            .iter()
            .map(|(market, outcome)| json!({"instrumentId": market.to_string(), "outcome": outcome}))
            .collect();
        json!({"legs": legs, "quantity": quantity}).to_string()
    }

    /// A market in `GET /v4/markets` by instrument, with its event.
    fn listed(address: &str, instrument: &str) -> Option<(Value, Value)> {
        let (_, markets) = request(address, "GET", "/v4/markets", None, None);
        markets["events"].as_array()?.iter().find_map(|e| {
            let m = e["markets"].as_array()?.iter().find(|m| m["instrumentId"].as_str() == Some(instrument))?;
            Some((e.clone(), m.clone()))
        })
    }

    #[test]
    fn a_request_names_one_parlay_whatever_the_leg_order_and_bad_ones_are_refused() {
        let venue = testing::venue("rest-rfq");
        let a = &venue.address;
        let (arsenal, draw, bitcoin) = (testing::market(2, 0), testing::market(2, 1), testing::market(3, 0));
        let ask = |body: &str| ask_for_quotes(a, body);
        for (why, body, status) in [
            ("two legs on one event", legs(&[(arsenal, "yes"), (draw, "no")], 10), 400),
            ("one leg", legs(&[(arsenal, "yes")], 10), 400),
            ("a quantity off the lot step", legs(&[(arsenal, "yes"), (MARKET, "yes")], 15), 400),
            ("a market that does not exist", legs(&[(arsenal, "yes"), (99_999, "yes")], 10), 404),
            ("an outcome that is neither", r#"{"legs": [{"instrumentId": "1", "outcome": "maybe"}, {"instrumentId": "2", "outcome": "yes"}], "quantity": 10}"#.to_string(), 400),
        ] {
            let (got, body) = ask(&body);
            assert_eq!(got, status, "{why}: {body}");
        }
        assert_eq!(request(a, "POST", "/v4/rfqs", None, Some(&legs(&[(arsenal, "yes"), (MARKET, "yes")], 10))).0, 401);

        let (status, first) = ask(&legs(&[(bitcoin, "no"), (arsenal, "yes"), (MARKET, "yes")], 10));
        assert_eq!((status, &first["listed"]), (200, &json!(true)), "{first}");
        let (status, again) = ask(&legs(&[(MARKET, "yes"), (bitcoin, "no"), (arsenal, "yes")], 30));
        assert_eq!((status, &again["listed"]), (200, &json!(false)), "{again}");
        assert_eq!(first["instrumentId"], again["instrumentId"]);
        assert_ne!(first["requestId"], again["requestId"]);
        let parlay = first["instrumentId"].as_str().expect("an instrument").to_string();

        let (event, market) = listed(a, &parlay).expect("the parlay is listed");
        assert_eq!(
            (market["class"].as_str(), market["state"].as_str(), market["lotStep"].as_u64()),
            (Some("parlay"), Some("open"), Some(10))
        );
        assert_eq!(market["legs"].as_array().map(Vec::len), Some(3), "{market}");
        assert_eq!(event["outcomes"], json!(["Loses", "Wins"]));
        assert!(event["title"].as_str().is_some_and(|t| t.starts_with("Parlay: ")), "{event}");
        assert_eq!(market["ladder"][0]["cents"], "0.1", "{market}");

        let request_id = first["requestId"].as_str().expect("a request ID");
        let (status, own) = request(a, "GET", &format!("/v4/rfqs/{request_id}"), Some(RETAIL_TOKEN), None);
        assert_eq!((status, &own["open"], &own["quotes"]), (200, &json!(true), &json!([])), "{own}");
        assert_eq!(request(a, "GET", &format!("/v4/rfqs/{request_id}"), Some(MAKER_TOKEN), None).0, 404);
        let attach = request(
            a,
            "POST",
            &format!("/v4/rfqs/{request_id}/quotes"),
            Some(MAKER_TOKEN),
            Some(r#"{"orderId": "5"}"#),
        );
        assert_eq!(attach.0, 403, "the market maker tab's account is not a responder: {}", attach.1);
        // Only an approved responder lists the open requests, and never sees its own among them.
        assert_eq!(request(a, "GET", "/v4/rfqs", Some(MAKER_TOKEN), None).0, 403);
        let (status, open) = request(a, "GET", "/v4/rfqs", Some(testing::RESPONDER_TOKEN), None);
        assert_eq!(status, 200, "{open}");
        let ids: Vec<&Value> = open["requests"].as_array().into_iter().flatten().map(|r| &r["requestId"]).collect();
        assert_eq!(ids, vec![&first["requestId"], &again["requestId"]], "{open}");
        assert!(open["requests"][0]["expiresAt"].as_str().is_some(), "{open}");
    }

    /// Each settlement rule against a real node: a lost leg expires a parlay, a voided leg voids one, and a parlay
    /// whose legs all won resolves to Wins once the last of them does, having closed when the first one stopped.
    #[test]
    fn parlays_settle_from_their_legs() {
        let venue = testing::venue("rest-rfq-settle");
        let a = &venue.address;
        let (arsenal, bitcoin) = (testing::market(2, 0), testing::market(3, 0));
        let parlay = |legs_asked: &[(u64, &str)]| {
            let (status, rfq) = ask_for_quotes(a, &legs(legs_asked, 10));
            assert_eq!(status, 200, "{rfq}");
            rfq["instrumentId"].as_str().expect("an instrument").to_string()
        };
        let loses = parlay(&[(MARKET, "yes"), (bitcoin, "yes")]);
        let voided = parlay(&[(arsenal, "yes"), (bitcoin, "no")]);
        let wins = parlay(&[(MARKET, "no"), (bitcoin, "yes")]);
        let operator = venue_link::submit::Submit::spawn(&venue.ingress);
        let resolve = |template: u64, kind: &str, state: u64| {
            let event = venue_link::catalogue::event_id(template, 1);
            let input = venue_link::wire::command(
                "ResolveEvent",
                format!("t-{template}-{kind}"),
                json!({"eventId": event, "kind": kind, "caseId": 0, "winningState": state, "voidWeights": []}),
            );
            operator.send(input).expect("resolved");
        };
        let resolution = |parlay: &str| listed(a, parlay).map(|(e, m)| (e["resolution"].clone(), m["state"].clone()));
        // No rain: the first parlay's rain leg lost, and the third's won.
        resolve(1, "RESOLVED", 0);
        testing::eventually("the lost parlay expires", || resolution(&loses).is_some_and(|(r, _)| !r.is_null()));
        let (expired, _) = resolution(&loses).expect("listed");
        assert_eq!(
            (expired["kind"].as_str(), expired["outcome"].as_str()),
            (Some("resolved"), Some("Loses")),
            "{expired}"
        );
        testing::eventually("the winning parlay closes", || resolution(&wins).is_some_and(|(_, s)| s == "closed"));
        resolve(2, "VOID", 0);
        testing::eventually("the voided parlay voids", || {
            resolution(&voided).is_some_and(|(r, _)| r["kind"] == "void")
        });
        resolve(3, "RESOLVED", 1);
        testing::eventually("the last parlay wins", || resolution(&wins).is_some_and(|(r, _)| !r.is_null()));
        let (won, state) = resolution(&wins).expect("listed");
        assert_eq!((won["outcome"].as_str(), state.as_str()), (Some("Wins"), Some("settled")), "{won}");
        assert!(
            resolution(&loses).is_some_and(|(r, _)| r["outcome"] == "Loses"),
            "a later leg changed a settled parlay"
        );
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
