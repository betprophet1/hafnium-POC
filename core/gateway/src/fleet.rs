//! What a client can rely on when a venue runs several gateways (interfaces/gateway/README.md, public and private):
//! each scenario spans gateway processes that share one node and one set of services (`testing::Fleet`), and looks
//! for anything that only holds with a single gateway — state kept in one gateway, a notification sent only through
//! the gateway that started something, identifiers that collide, or a gap when a connection is made again.

use crate::api::Limits;
use crate::limits::monotonic;
use crate::testing::{eventually, Fleet, Serve, MAKER_TOKEN, MARKET, RESPONDER, RESPONDER_TOKEN, RETAIL_TOKEN};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

/// Sends one request and returns the status and the JSON body. The gateway answers once and closes.
fn request(address: &str, method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> (u16, Value) {
    let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
    stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
    let body = body.map(Value::to_string).unwrap_or_default();
    let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
    let head = format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Content-Length: {}\r\n\r\n", body.len());
    stream.write_all(format!("{head}{body}").as_bytes()).expect("the request is sent");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("a whole response before the timeout");
    let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
    let json = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(json).unwrap_or(Value::Null))
}

fn order_body(client_order_id: &str, action: &str, tick: u64, quantity: u64) -> Value {
    json!({"clientOrderId": client_order_id, "instrumentId": MARKET.to_string(), "action": action, "outcome": "yes",
           "price": {"tick": tick}, "quantity": quantity})
}

/// Places an order over REST and returns its order ID, as a string.
fn place(address: &str, token: &str, body: &Value) -> String {
    let (status, placed) = request(address, "POST", "/v4/orders", Some(token), Some(body));
    assert_eq!((status, placed["status"].as_str()), (200, Some("accepted")), "{placed}");
    placed["orderId"].as_str().expect("an order ID").to_string()
}

/// The account's resting order IDs, as one gateway sees them.
fn resting(address: &str, token: &str) -> BTreeSet<String> {
    let (status, account) = request(address, "GET", "/v4/account", Some(token), None);
    assert_eq!(status, 200, "{account}");
    account["orders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|o| o["orderId"].as_str().map(str::to_string))
        .collect()
}

fn sequence(message: &Value) -> u64 {
    message["sequence"].as_str().and_then(|s| s.parse().ok()).unwrap_or_else(|| panic!("no sequence in {message}"))
}

/// The sequence a private message derives from, if it is one: an execution carries it inside.
fn private_sequence(message: &Value) -> Option<u64> {
    let sequence =
        if message["type"] == "execution" { &message["execution"]["sequence"] } else { &message["sequence"] };
    sequence.as_str().and_then(|s| s.parse().ok())
}

struct Client {
    socket: WebSocket<MaybeTlsStream<TcpStream>>,
    /// Streamed messages seen while waiting for an answer, in arrival order.
    streamed: Vec<Value>,
}

impl Client {
    fn connect(url: &str) -> Self {
        let (socket, _) = tungstenite::connect(url).expect("the gateway accepts a WebSocket");
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream.set_read_timeout(Some(Duration::from_secs(30))).expect("a read timeout");
        }
        Self { socket, streamed: Vec::new() }
    }

    fn send(&mut self, message: &Value) {
        self.socket.send(Message::text(message.to_string())).expect("sent");
    }

    fn next(&mut self) -> Value {
        loop {
            match self.socket.read().expect("a message before the timeout") {
                Message::Text(text) => return serde_json::from_str(text.as_str()).expect("JSON"),
                Message::Close(_) => panic!("the gateway closed the connection"),
                _ => {}
            }
        }
    }

    fn next_answer(&mut self) -> Value {
        loop {
            let m = self.next();
            if matches!(m["type"].as_str(), Some("reply" | "error" | "welcome" | "resync")) {
                return m;
            }
            self.streamed.push(m);
        }
    }

    fn ask(&mut self, message: &Value) -> Value {
        self.send(message);
        self.next_answer()
    }

    /// Asks again while the answer is that the parlay service is unavailable, as it is while a gateway reconnects.
    fn ask_parlays(&mut self, message: &Value) -> Value {
        for _ in 0..300 {
            let answer = self.ask(message);
            if answer["error"]["code"] != "UNAVAILABLE" {
                return answer;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the parlay service stayed unavailable for {message}");
    }

    /// Waits for a streamed message `wanted` accepts, whether it has already arrived or not.
    fn streamed_message(&mut self, wanted: impl Fn(&Value) -> bool) -> Value {
        loop {
            if let Some(found) = self.streamed.iter().find(|m| wanted(m)) {
                return found.clone();
            }
            let m = self.next();
            self.streamed.push(m);
        }
    }

    fn execution(&mut self, order: &str, kind: &str) -> Value {
        self.streamed_message(|m| {
            m["type"] == "execution" && m["execution"]["orderId"] == order && m["execution"]["type"] == kind
        })
    }

    fn login(&mut self, token: &str, extra: &Value) -> Value {
        let mut login = json!({"op": "login", "id": "login", "token": token});
        if let (Some(login), Some(extra)) = (login.as_object_mut(), extra.as_object()) {
            login.extend(extra.clone());
        }
        let welcome = self.ask(&login);
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        welcome
    }

    /// Whether the gateway closes the connection within `seconds`.
    fn closed_within(&mut self, seconds: u64) -> bool {
        if let MaybeTlsStream::Plain(stream) = self.socket.get_ref() {
            let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
        }
        for _ in 0..seconds * 10 {
            match self.socket.read() {
                Ok(Message::Close(_)) => return true,
                Ok(Message::Text(text)) => {
                    if let Ok(m) = serde_json::from_str(text.as_str()) {
                        self.streamed.push(m);
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return true,
            }
        }
        false
    }
}

/// Two private gateways and a session of the same account on each: an order placed through one is in the account and
/// can be cancelled through the other, and each of its executions reaches both sessions.
#[test]
fn an_order_placed_through_one_private_gateway_is_seen_and_cancelled_through_another() {
    let mut fleet = Fleet::start("fleet-orders");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let mut on_a = Client::connect(&a.ws_private);
    on_a.login(MAKER_TOKEN, &json!({}));
    let mut on_b = Client::connect(&b.ws_private);
    on_b.login(MAKER_TOKEN, &json!({}));

    let order = place(&a.address, MAKER_TOKEN, &order_body("across-1", "buy", 4, 10));
    for session in [&mut on_a, &mut on_b] {
        session.execution(&order, "accepted");
    }
    eventually("gateway B's account shows the order", || resting(&b.address, MAKER_TOKEN).contains(&order));
    let (status, cancelled) = request(&b.address, "DELETE", &format!("/v4/orders/{order}"), Some(MAKER_TOKEN), None);
    assert_eq!((status, cancelled["status"].as_str()), (200, Some("cancelled")), "{cancelled}");
    for session in [&mut on_a, &mut on_b] {
        session.execution(&order, "cancelled");
    }
    assert!(!resting(&a.address, MAKER_TOKEN).contains(&order), "gateway A still shows the cancelled order");

    // Placed through B's WebSocket and filled by the retail account through A: both parties hear on both gateways.
    let placed = on_b
        .ask(&json!({"op": "order", "id": "o", "clientOrderId": "across-2", "instrumentId": MARKET.to_string(),
                                  "action": "sell", "outcome": "yes", "price": {"tick": 40}, "quantity": 5}));
    let resting_order = placed["result"]["orderId"].as_str().expect("an order ID").to_string();
    let mut retail_on_b = Client::connect(&b.ws_private);
    retail_on_b.login(RETAIL_TOKEN, &json!({}));
    let taken = place(&a.address, RETAIL_TOKEN, &order_body("across-take", "buy", 40, 5));
    for session in [&mut on_a, &mut on_b] {
        session.execution(&resting_order, "fill");
    }
    retail_on_b.execution(&taken, "fill");
}

/// An order just placed through one gateway, amended and cancelled through another straight away, without waiting to
/// see it there: the other gateway may not have had it from the feed yet, and waits for it rather than saying there is
/// no such order. An order that has gone is still not found at once, and one no feed ever delivers is refused as
/// unavailable, with nothing sent.
#[test]
fn an_order_placed_through_one_gateway_can_be_amended_and_cancelled_through_another_at_once() {
    let mut fleet = Fleet::start("fleet-at-once");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let mut on_b = Client::connect(&b.ws_private);
    on_b.login(MAKER_TOKEN, &json!({}));
    let mut last: u64 = 0;
    for i in 0..20 {
        let order = place(&a.address, MAKER_TOKEN, &order_body(&format!("now-{i}"), "buy", 4, 10));
        let amended = on_b.ask(&json!({"op": "amend", "id": format!("a{i}"), "orderId": order, "quantity": 5}));
        assert_eq!(amended["result"]["status"], "amended", "order {i}: {amended}");
        let path = format!("/v4/orders/{order}");
        let (status, cancelled) = request(&b.address, "DELETE", &path, Some(MAKER_TOKEN), None);
        assert_eq!((status, cancelled["status"].as_str()), (200, Some("cancelled")), "order {i}: {cancelled}");
        last = order.parse().expect("a numeric order ID");
    }
    let gone = request(&b.address, "DELETE", &format!("/v4/orders/{last}"), Some(MAKER_TOKEN), None);
    assert_eq!((gone.0, gone.1["error"]["code"].as_str()), (404, Some("NOT_FOUND")), "{}", gone.1);
    let never = last + 1_000_000;
    let started = monotonic();
    let unseen = request(&b.address, "DELETE", &format!("/v4/orders/{never}"), Some(MAKER_TOKEN), None);
    assert_eq!((unseen.0, unseen.1["error"]["code"].as_str()), (503, Some("UNAVAILABLE")), "{}", unseen.1);
    assert!(monotonic().duration_since(started) >= Duration::from_secs(2), "refused before waiting for the feed");
}

/// Market data is the same whichever public gateway serves it: book changes caused through a private gateway reach
/// subscribers on every public gateway, the trades topic carries the same trades under the same sequences everywhere,
/// and once each has caught up every gateway's book is the same book at the same sequence.
#[test]
fn book_changes_made_through_a_private_gateway_reach_every_public_gateway_in_step() {
    let mut fleet = Fleet::start("fleet-books");
    let a = fleet.add(Serve::Private);
    let publics = [fleet.add(Serve::Public), fleet.add(Serve::Public), fleet.add(Serve::Both)];
    let topics = json!([format!("book.{MARKET}"), format!("trades.{MARKET}")]);
    let mut watchers: Vec<Client> = publics
        .iter()
        .map(|&i| {
            let mut watcher = Client::connect(&fleet.gateways[i].ws_public);
            watcher.send(&json!({"op": "subscribe", "id": "s", "topics": topics}));
            let image = watcher.next();
            assert_eq!((image["type"].as_str(), &image["snapshot"]), (Some("book"), &json!(true)), "{image}");
            watcher
        })
        .collect();
    let a = &fleet.gateways[a].address;
    place(a, MAKER_TOKEN, &order_body("book-sell", "sell", 40, 10));
    place(a, RETAIL_TOKEN, &order_body("book-take", "buy", 40, 4));
    place(a, MAKER_TOKEN, &order_body("book-buy", "buy", 30, 7));
    place(a, RETAIL_TOKEN, &order_body("book-take-2", "buy", 40, 1));
    let expected = BTreeSet::from([(false, 40, 5), (true, 30, 7)]);

    let mut trades = Vec::new();
    for watcher in &mut watchers {
        let mut book: BTreeSet<(bool, u64, u64)> = BTreeSet::new();
        let (mut seen, mut last_sequence) = (Vec::new(), 0);
        while book != expected || seen.len() < 2 {
            let m = watcher.next();
            match m["type"].as_str() {
                Some("book") => {
                    assert!(sequence(&m) >= last_sequence, "a book went back in sequence: {m}");
                    last_sequence = sequence(&m);
                    for change in m["changes"].as_array().into_iter().flatten() {
                        let (buy, tick) = (change["side"] == "buy", change["price"]["tick"].as_u64().unwrap_or(0));
                        book.retain(|(b, t, _)| (*b, *t) != (buy, tick));
                        if let Some(quantity) = change["quantity"].as_u64().filter(|q| *q > 0) {
                            book.insert((buy, tick, quantity));
                        }
                    }
                }
                Some("trade") => seen.push((m["tradeId"].clone(), m["sequence"].clone(), m["quantity"].clone())),
                _ => {}
            }
        }
        trades.push(seen);
    }
    assert!(trades.windows(2).all(|w| w[0] == w[1]), "the public gateways carried different trades: {trades:?}");
    let books = || {
        publics
            .iter()
            .map(|&i| request(&fleet.gateways[i].address, "GET", &format!("/v4/markets/{MARKET}/book"), None, None).1)
            .collect::<Vec<Value>>()
    };
    eventually("every public gateway shows the same book", || books().windows(2).all(|w| w[0] == w[1]));
}

/// Cancel-on-disconnect is the session's: a session with it that ends on one gateway cancels its own orders, and not
/// those a concurrent session of the same account placed on another gateway, nor the account's REST orders.
#[test]
fn a_session_ending_on_one_gateway_cancels_only_its_own_orders() {
    let mut fleet = Fleet::start("fleet-disconnect");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let mut leaving = Client::connect(&a.ws_private);
    let leaving_id = leaving.login(MAKER_TOKEN, &json!({"cancelOnDisconnect": true}))["sessionId"].clone();
    let mut staying = Client::connect(&b.ws_private);
    let staying_id = staying.login(MAKER_TOKEN, &json!({"cancelOnDisconnect": true}))["sessionId"].clone();
    assert_ne!(leaving_id, staying_id);
    let mut watching = Client::connect(&b.ws_private);
    watching.login(MAKER_TOKEN, &json!({}));
    let ws_order = |client: &mut Client, id: &str| {
        let placed = client
            .ask(&json!({"op": "order", "id": id, "clientOrderId": id, "instrumentId": MARKET.to_string(),
                                        "action": "buy", "outcome": "yes", "price": {"tick": 4}, "quantity": 1}));
        placed["result"]["orderId"].as_str().expect("an order ID").to_string()
    };
    let doomed = ws_order(&mut leaving, "cod-leaving");
    let kept = ws_order(&mut staying, "cod-staying");
    let rest = place(&a.address, MAKER_TOKEN, &order_body("cod-rest", "buy", 4, 1));
    watching.execution(&rest, "accepted");

    drop(leaving);
    watching.execution(&doomed, "cancelled");
    // Anything else that mass cancel took would have been sent with it; two round trips see it all through.
    for id in ["h1", "h2"] {
        assert_eq!(watching.ask(&json!({"op": "heartbeat", "id": id}))["id"], id);
    }
    let taken: Vec<&Value> = watching.streamed.iter().filter(|m| m["execution"]["type"] == "cancelled").collect();
    assert_eq!(taken.len(), 1, "more than the ended session's order was cancelled: {taken:?}");
    for gateway in [a, b] {
        let orders = resting(&gateway.address, MAKER_TOKEN);
        assert!(orders.contains(&kept) && orders.contains(&rest) && !orders.contains(&doomed), "{orders:?}");
    }
    assert_eq!(staying.ask(&json!({"op": "heartbeat", "id": "still"}))["id"], "still");
}

/// Session IDs are what a mass cancel by session is keyed to (spec E-12.6), so they must never collide: not between
/// gateways running side by side, and not with a gateway started in place of one that went.
#[test]
fn session_ids_never_collide_across_gateways_or_a_restart() {
    let mut fleet = Fleet::start("fleet-session-ids");
    let first = [fleet.add(Serve::Private), fleet.add(Serve::Private), fleet.add(Serve::Both)];
    let mut ids = BTreeSet::new();
    let mut sessions = Vec::new();
    let mut log_in = |url: &str, ids: &mut BTreeSet<String>| {
        for _ in 0..15 {
            let mut client = Client::connect(url);
            let id = client.login(RETAIL_TOKEN, &json!({}))["sessionId"].as_str().expect("a session ID").to_string();
            assert!(ids.insert(id.clone()), "session ID {id} was given twice");
            sessions.push(client);
        }
    };
    for &i in &first {
        log_in(&fleet.gateways[i].ws_private, &mut ids);
    }
    fleet.gateways[first[0]].stop();
    let replacement = fleet.add(Serve::Private);
    log_in(&fleet.gateways[replacement].ws_private, &mut ids);
    assert_eq!(ids.len(), 60);
}

/// A heartbeat is kept per session: one that goes silent on one gateway is cancelled and closed there, while a session
/// of the same account on another gateway that keeps beating keeps its orders and its connection.
#[test]
fn a_missed_heartbeat_on_one_gateway_leaves_sessions_on_another_alone() {
    let mut fleet = Fleet::start("fleet-heartbeat");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let order = |client: &mut Client, id: &str| {
        let placed = client
            .ask(&json!({"op": "order", "id": id, "clientOrderId": id, "instrumentId": MARKET.to_string(),
                                        "action": "buy", "outcome": "yes", "price": {"tick": 4}, "quantity": 1}));
        placed["result"]["orderId"].as_str().expect("an order ID").to_string()
    };
    let mut silent = Client::connect(&a.ws_private);
    silent.login(MAKER_TOKEN, &json!({"heartbeatSeconds": 1}));
    let silent_order = order(&mut silent, "hb-silent");
    let mut beating = Client::connect(&b.ws_private);
    beating.login(MAKER_TOKEN, &json!({"heartbeatSeconds": 1}));
    let beating_order = order(&mut beating, "hb-beating");
    for beat in 0.. {
        assert!(beat < 300, "the silent session's order was never cancelled");
        assert_eq!(beating.ask(&json!({"op": "heartbeat", "id": "beat"}))["id"], "beat");
        let cancelled = |m: &Value| m["execution"]["type"] == "cancelled";
        if beating.streamed.iter().any(|m| cancelled(m) && m["execution"]["orderId"] == silent_order.as_str()) {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(silent.closed_within(10), "the silent session was not closed");
    for id in ["after-1", "after-2"] {
        assert_eq!(beating.ask(&json!({"op": "heartbeat", "id": id}))["id"], id);
    }
    assert!(
        !beating
            .streamed
            .iter()
            .any(|m| m["execution"]["orderId"] == beating_order.as_str() && m["execution"]["type"] == "cancelled"),
        "the beating session's order was cancelled"
    );
    for gateway in [a, b] {
        assert!(resting(&gateway.address, MAKER_TOKEN).contains(&beating_order));
    }
}

/// Transfers are the treasury's, and every private gateway follows its updates: a deposit requested through one
/// gateway is streamed to the account's sessions on the others, and can be read and cancelled through any of them.
#[test]
fn a_transfer_requested_through_one_gateway_reaches_the_accounts_sessions_on_the_others() {
    let mut fleet = Fleet::start("fleet-transfers");
    let (a, b, e) = (fleet.add(Serve::Private), fleet.add(Serve::Private), fleet.add(Serve::Both));
    let (a, b, e) = (&fleet.gateways[a], &fleet.gateways[b], &fleet.gateways[e]);
    let mut on_b = Client::connect(&b.ws_private);
    on_b.login(RETAIL_TOKEN, &json!({}));
    let mut on_e = Client::connect(&e.ws_private);
    on_e.login(RETAIL_TOKEN, &json!({}));
    let heard = |client: &mut Client, id: &str, state: &str| {
        client.streamed_message(|m| {
            m["type"] == "transfer" && m["transfer"]["clientTransferId"] == id && m["transfer"]["state"] == state
        })
    };
    // A gateway hears the treasury's updates from when it has connected to their stream, which it does in the
    // background: small deposits until one has reached both sessions show that both are listening.
    for probe in 0.. {
        assert!(probe < 100, "the gateways never heard the treasury's updates");
        let id = format!("probe-{probe}");
        let body = json!({"clientTransferId": id, "kind": "deposit", "amountCents": 1, "method": "dummy_instant"});
        assert_eq!(request(&a.address, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&body)).0, 200);
        let marker = format!("probe-mark-{probe}");
        let flushed = |client: &mut Client| {
            assert_eq!(client.ask(&json!({"op": "heartbeat", "id": marker}))["id"], marker.as_str());
            client.streamed.iter().any(|m| m["transfer"]["clientTransferId"] == id.as_str())
        };
        let (on_b_heard, on_e_heard) = (flushed(&mut on_b), flushed(&mut on_e));
        if on_b_heard && on_e_heard {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    let before = request(&b.address, "GET", "/v4/account", Some(RETAIL_TOKEN), None).1["availableCents"].clone();
    let deposit =
        json!({"clientTransferId": "across", "kind": "deposit", "amountCents": 1234, "method": "dummy_instant"});
    let (status, made) = request(&a.address, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&deposit));
    assert_eq!((status, made["state"].as_str()), (200, Some("settled")), "{made}");
    for client in [&mut on_b, &mut on_e] {
        heard(client, "across", "pending");
        heard(client, "across", "settled");
    }
    let before = before.as_i64().expect("available cash");
    eventually("gateway B's account shows the deposit", || {
        request(&b.address, "GET", "/v4/account", Some(RETAIL_TOKEN), None).1["availableCents"].as_i64()
            == Some(before + 1234)
    });

    let slow = json!({"clientTransferId": "slow", "kind": "deposit", "amountCents": 500, "method": "dummy_bank"});
    let (status, pending) = request(&b.address, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&slow));
    assert_eq!((status, pending["state"].as_str()), (200, Some("pending")), "{pending}");
    let path = format!("/v4/transfers/{}", pending["transferId"].as_str().expect("a transfer ID"));
    assert_eq!(request(&e.address, "GET", &path, Some(RETAIL_TOKEN), None).1["clientTransferId"], "slow");
    let (status, cancelled) = request(&a.address, "DELETE", &path, Some(RETAIL_TOKEN), None);
    assert_eq!((status, cancelled["state"].as_str()), (200, Some("cancelled")), "{cancelled}");
    for client in [&mut on_b, &mut on_e] {
        heard(client, "slow", "cancelled");
    }
}

/// A request for quote needs no gateway in common: the requester's and the responder's are different, the request
/// reaches the responder, its quote reaches the requester, and taking it fills. A restarted parlay service has every
/// gateway's responder sessions subscribed again, so the next request reaches them all.
#[test]
fn a_request_for_quote_crosses_gateways_and_survives_a_parlay_service_restart() {
    let mut fleet = Fleet::start("fleet-rfq");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let body = json!({"label": "quoting", "permissions": ["read", "trade", "respond"]});
    let (status, key) = request(&b.address, "POST", "/v4/keys", Some(RESPONDER_TOKEN), Some(&body));
    assert_eq!(status, 200, "{key}");
    let key = key["credential"].as_str().expect("a credential").to_string();
    let mut responder_b = Client::connect(&b.ws_private);
    responder_b.login(&key, &json!({}));
    let subscribed = responder_b.ask_parlays(&json!({"op": "subscribeRfqs", "id": "sub"}));
    assert_eq!(subscribed["result"]["subscribed"], true, "{subscribed}");
    let mut responder_a = Client::connect(&a.ws_private);
    responder_a.login(RESPONDER_TOKEN, &json!({}));
    let subscribed = responder_a.ask_parlays(&json!({"op": "subscribeRfqs", "id": "sub"}));
    assert_eq!(subscribed["result"]["subscribed"], true, "{subscribed}");
    let mut requester = Client::connect(&a.ws_private);
    requester.login(RETAIL_TOKEN, &json!({}));

    let (arsenal, bitcoin) = (crate::testing::market(2, 0), crate::testing::market(3, 0));
    let legs = json!([{"instrumentId": bitcoin.to_string(), "outcome": "yes"},
                      {"instrumentId": MARKET.to_string(), "outcome": "no"}]);
    let opened = requester.ask_parlays(&json!({"op": "requestQuote", "id": "rq", "legs": legs, "quantity": 20}));
    assert_eq!(opened["result"]["responders"], 1, "{opened}");
    let rfq = &opened["result"];
    let (request_id, parlay) = (rfq["requestId"].clone(), rfq["instrumentId"].clone());
    let expires = rfq["expiresAt"].clone();
    responder_b.streamed_message(|m| m["type"] == "rfq" && m["requestId"] == request_id);
    let listed: u64 = parlay.as_str().and_then(|p| p.parse().ok()).expect("a numeric instrument ID");
    eventually("both gateways list the parlay", || {
        a.private().hub().market(listed).is_some() && b.private().hub().market(listed).is_some()
    });
    let sell =
        responder_b.ask(&json!({"op": "order", "id": "sell", "clientOrderId": "quote-1", "instrumentId": parlay,
                                       "action": "sell", "outcome": "yes", "price": {"cents": "0.8"}, "quantity": 20,
                                       "timeInForce": "GTD", "expiresAt": expires}));
    let quote_order = sell["result"]["orderId"].as_str().expect("an order ID").to_string();
    let attached = responder_b
        .ask_parlays(&json!({"op": "attachQuote", "id": "attach", "requestId": request_id, "orderId": quote_order}));
    assert_eq!(attached["result"]["state"], "open", "{attached}");
    let quote = requester.streamed_message(|m| m["type"] == "quote" && m["orderId"] == quote_order.as_str());
    let price = quote["price"]["probability"].clone();
    let taken = requester.ask(&json!({"op": "order", "id": "take", "clientOrderId": "take-1", "instrumentId": parlay,
                                      "action": "buy", "outcome": "yes", "price": {"probability": price},
                                      "quantity": 20, "timeInForce": "FOK"}));
    assert_eq!(taken["result"]["status"], "accepted", "{taken}");
    let filled: u64 = taken["result"]["executions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "fill")
        .filter_map(|e| e["quantity"].as_u64())
        .sum();
    assert_eq!(filled, 20, "{taken}");
    requester.streamed_message(|m| m["type"] == "quote" && m["state"] == "filled");
    responder_b.execution(&quote_order, "fill");

    // The service restarts: it knows no subscriptions until the gateways, each on its own, subscribe theirs again.
    let service = fleet.restart_parlays();
    eventually("both gateways subscribe their responder sessions again", || {
        service.responders()["responders"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|r| r["accountId"].as_str().and_then(|a| a.parse().ok()) == Some(RESPONDER) && r["subscriptions"] == 2)
    });
    let legs = json!([{"instrumentId": arsenal.to_string(), "outcome": "yes"},
                      {"instrumentId": bitcoin.to_string(), "outcome": "no"}]);
    let again = requester.ask_parlays(&json!({"op": "requestQuote", "id": "rq2", "legs": legs, "quantity": 10}));
    assert_eq!(again["result"]["responders"], 1, "{again}");
    for responder in [&mut responder_a, &mut responder_b] {
        responder.streamed_message(|m| m["type"] == "rfq" && m["requestId"] == again["result"]["requestId"]);
    }
}

/// API keys are the identity service's: a key made through one gateway works on another, and revoking it through that
/// other ends the sessions using it on the first within the contract's ten seconds, cancelling their orders if they
/// asked.
#[test]
fn a_key_made_through_one_gateway_works_on_another_and_revoking_it_ends_its_sessions_everywhere() {
    let mut fleet = Fleet::start("fleet-keys");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);
    let body = json!({"label": "across", "permissions": ["read", "trade"]});
    let (status, created) = request(&a.address, "POST", "/v4/keys", Some(MAKER_TOKEN), Some(&body));
    assert_eq!(status, 200, "{created}");
    let key = created["credential"].as_str().expect("a credential").to_string();
    assert_eq!(request(&b.address, "GET", "/v4/account", Some(&key), None).0, 200);
    place(&b.address, &key, &order_body("key-rest", "buy", 4, 1));
    let mut on_a = Client::connect(&a.ws_private);
    on_a.login(&key, &json!({"cancelOnDisconnect": true}));
    let placed = on_a
        .ask(&json!({"op": "order", "id": "o", "clientOrderId": "key-ws", "instrumentId": MARKET.to_string(),
                                  "action": "buy", "outcome": "yes", "price": {"tick": 4}, "quantity": 1}));
    let order = placed["result"]["orderId"].as_str().expect("an order ID").to_string();
    let mut on_b = Client::connect(&b.ws_private);
    on_b.login(&key, &json!({}));

    let path = format!("/v4/keys/{}", created["keyId"].as_str().expect("a key ID"));
    assert_eq!(request(&b.address, "DELETE", &path, Some(MAKER_TOKEN), None).0, 200);
    let revoked = monotonic();
    assert!(on_a.closed_within(15), "gateway A kept a session whose key was revoked through gateway B");
    let closed = monotonic().duration_since(revoked);
    assert!(closed < Duration::from_secs(10), "closed after {closed:?}");
    let told = on_a.streamed.iter().find(|m| m["type"] == "error").cloned().unwrap_or_default();
    assert_eq!(told["error"]["code"], "UNAUTHORIZED", "{told}");
    assert!(on_b.closed_within(15), "gateway B kept a session whose key it revoked");
    assert_eq!(request(&a.address, "GET", "/v4/account", Some(&key), None).0, 401);
    eventually("the closed session's order is cancelled", || !resting(&b.address, MAKER_TOKEN).contains(&order));
}

/// Flood and logon limits are kept in each gateway's memory (blueprint §12; README, flood limits), so a client meets
/// them per gateway: an account refused on one gateway is still served by another, and N gateways let an account
/// through up to N times each limit.
#[test]
fn flood_and_logon_limits_are_kept_per_gateway() {
    let mut fleet = Fleet::start("fleet-limits");
    let limits = Limits { per_second: 1, burst: 2, logons_per_minute: 2 };
    let (a, b) = (fleet.add_with(Serve::Private, limits), fleet.add_with(Serve::Private, limits));
    let (a, b) = (&fleet.gateways[a], &fleet.gateways[b]);

    let mut sessions = Vec::new();
    for _ in 0..2 {
        let mut client = Client::connect(&a.ws_private);
        client.login(RETAIL_TOKEN, &json!({}));
        sessions.push(client);
    }
    let mut third = Client::connect(&a.ws_private);
    let refused = third.ask(&json!({"op": "login", "id": "login", "token": RETAIL_TOKEN}));
    assert_eq!(refused["error"]["code"], "RATE_LIMITED", "{refused}");
    let mut elsewhere = Client::connect(&b.ws_private);
    elsewhere.login(RETAIL_TOKEN, &json!({}));

    // Messages on a session: its own bucket, wherever the account's other sessions are.
    let limited =
        (0..50).any(|_| sessions[0].ask(&json!({"op": "heartbeat", "id": "h"}))["error"]["code"] == "RATE_LIMITED");
    assert!(limited, "a session was never limited");
    assert_eq!(elsewhere.ask(&json!({"op": "heartbeat", "id": "b"}))["id"], "b");

    // REST: per account, per gateway.
    let limited = (0..50).any(|_| request(&a.address, "GET", "/v4/account", Some(RETAIL_TOKEN), None).0 == 429);
    assert!(limited, "REST was never limited");
    assert_eq!(request(&b.address, "GET", "/v4/account", Some(RETAIL_TOKEN), None).0, 200);
}

/// Each gateway process writes its own audit instance, even into one directory: what a client did through one gateway
/// is recorded in that gateway's files and no other's, and every file's chain verifies.
#[test]
fn each_gateway_audits_only_its_own_traffic_and_every_chain_verifies() {
    let mut fleet = Fleet::start("fleet-audit");
    let members = [fleet.add(Serve::Private), fleet.add(Serve::Public), fleet.add(Serve::Both)];
    let marks = ["audit-only-private", "audit-only-public", "audit-only-both"];
    place(&fleet.gateways[members[0]].address, MAKER_TOKEN, &order_body(marks[0], "buy", 4, 1));
    let mut viewer = Client::connect(&fleet.gateways[members[1]].ws_public);
    let refused = viewer.ask(&json!({"op": "subscribe", "id": marks[1], "topics": ["nonsense"]}));
    assert_eq!(refused["type"], "error", "{refused}");
    place(&fleet.gateways[members[2]].address, MAKER_TOKEN, &order_body(marks[2], "buy", 4, 1));

    let files_of = |instance: &str| -> Vec<std::path::PathBuf> {
        let prefix = format!("gateway.{instance}.");
        let mut files: Vec<_> = std::fs::read_dir(&fleet.audit_dir)
            .expect("the audit directory")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(&prefix)))
            .collect();
        files.sort();
        files
    };
    let text_of = |instance: &str| -> String {
        files_of(instance).iter().map(|f| std::fs::read_to_string(f).unwrap_or_default()).collect()
    };
    for (&member, mark) in members.iter().zip(marks) {
        let instance = &fleet.gateways[member].instance;
        eventually("the gateway's audit log records its traffic", || text_of(instance).contains(mark));
    }
    for (&member, mark) in members.iter().zip(marks) {
        let instance = &fleet.gateways[member].instance;
        let text = text_of(instance);
        for other in marks.iter().filter(|m| **m != mark) {
            assert!(!text.contains(other), "{instance} recorded another gateway's traffic: {other}");
        }
        for line in text.lines() {
            let record: Value = serde_json::from_str(line).expect("a JSON record");
            assert_eq!(record["instance"], instance.as_str(), "{record}");
        }
        for file in files_of(instance) {
            assert!(crate::audit::verify(&file).is_ok(), "{}: {:?}", file.display(), crate::audit::verify(&file));
        }
    }
}

/// A client whose gateway goes away reconnects to another and resumes from the last sequence it applied: what
/// happened to its account meanwhile is replayed, nothing it had is sent again, and the same holds on a gateway
/// started in place of the one that went, which has the venue from the feed alone.
#[test]
fn a_client_whose_gateway_goes_resumes_on_another_without_missing_executions() {
    let mut fleet = Fleet::start("fleet-resume");
    let (a, b) = (fleet.add(Serve::Private), fleet.add(Serve::Private));
    let mut client = Client::connect(&fleet.gateways[a].ws_private);
    client.login(MAKER_TOKEN, &json!({}));
    let placed = client
        .ask(&json!({"op": "order", "id": "o", "clientOrderId": "before", "instrumentId": MARKET.to_string(),
                                    "action": "buy", "outcome": "yes", "price": {"tick": 4}, "quantity": 1}));
    let before = placed["result"]["orderId"].as_str().expect("an order ID").to_string();
    client.execution(&before, "accepted");
    let last = client.streamed.iter().filter_map(private_sequence).max().expect("a sequence");

    fleet.gateways[a].stop();
    assert!(client.closed_within(10), "the client did not see its gateway go");
    let b_address = fleet.gateways[b].address.clone();
    let missed = place(&b_address, MAKER_TOKEN, &order_body("while-away", "buy", 5, 1));
    let (status, _) = request(&b_address, "DELETE", &format!("/v4/orders/{missed}"), Some(MAKER_TOKEN), None);
    assert_eq!(status, 200);
    let also = place(&b_address, MAKER_TOKEN, &order_body("while-away-2", "buy", 6, 1));

    let replacement = fleet.add(Serve::Private);
    for url in [&fleet.gateways[b].ws_private, &fleet.gateways[replacement].ws_private] {
        let mut resumed = Client::connect(url);
        let welcome = resumed.login(MAKER_TOKEN, &json!({"resumeFrom": last.to_string()}));
        assert_eq!(welcome["resumed"], true, "{welcome}");
        resumed.execution(&missed, "accepted");
        resumed.execution(&missed, "cancelled");
        resumed.execution(&also, "accepted");
        let repeated: Vec<&Value> =
            resumed.streamed.iter().filter(|m| private_sequence(m).is_some_and(|s| s <= last)).collect();
        assert!(repeated.is_empty(), "sent again what the client had applied: {repeated:?}");
    }
}
