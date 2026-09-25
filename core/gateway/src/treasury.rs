//! Deposits and withdrawals (interfaces/gateway/README.md, deposits and withdrawals). The treasury owns every transfer;
//! the private half authenticates the account, passes the request on, and passes the answer back untouched. It also
//! follows the treasury's updates and sends each to the account's private sessions. Nothing about a transfer is kept
//! here, so a gateway that fails over loses nothing.
//!
//! A call to the treasury waits on the network, so it runs on a thread of its own and tells the event loop when its
//! answer is there, as the ingress does for an order. Transfers are a person's deposits, not a market's orders: a
//! thread each is plenty, and a bound on how many at once keeps a flood from costing more than refusals.

use crate::api::{ApiError, Gateway};
use crate::metrics::{HalfMetrics, Outcome, Service};
use crate::{http, rest};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use venue_link::submit::Notify;
use venue_telemetry::log;

/// Calls to the treasury outstanding at once, across every client of this gateway.
const MAX_CALLS: usize = 64;
const CONNECT: Duration = Duration::from_secs(2);
/// The treasury answers a request within five seconds, however slow the sequencer group is, so an answer that takes
/// longer than this is not coming.
const ANSWER: Duration = Duration::from_secs(10);
/// The treasury sends an empty line on an update stream every 15 s; one silent for this long has gone.
const UPDATES_SILENCE: Duration = Duration::from_secs(45);

fn unavailable() -> ApiError {
    ApiError::unavailable(
        "the treasury did not answer; the outcome is unknown, so resend with the same client transfer ID",
    )
}

/// The treasury, by address.
pub struct Treasury {
    address: String,
    calls: Arc<AtomicUsize>,
    metrics: Arc<HalfMetrics>,
}

/// A request passed to the treasury or the identity service, whose response `take` has once the notify it was given is
/// called.
pub struct Call {
    response: Receiver<Vec<u8>>,
}

impl Call {
    /// A call whose whole response will arrive on `response`.
    pub const fn new(response: Receiver<Vec<u8>>) -> Self {
        Self { response }
    }

    /// The whole response, if it has arrived.
    pub fn take(&self) -> Option<Vec<u8>> {
        match self.response.try_recv() {
            Ok(response) => Some(response),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(rest::response(Err(unavailable()))),
        }
    }
}

impl Treasury {
    pub fn new(address: &str, metrics: Arc<HalfMetrics>) -> Self {
        Self { address: address.to_string(), calls: Arc::new(AtomicUsize::new(0)), metrics }
    }

    /// Passes a request under `/v4` for an authenticated account on to the treasury, as the same request under the
    /// account's own path there: `POST /v4/transfers` becomes `POST /v1/accounts/{account}/transfers`.
    pub fn call(
        &self,
        account: u64,
        method: &str,
        path: &str,
        query: &str,
        body: &[u8],
        notify: Notify,
    ) -> Result<Call, ApiError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) >= MAX_CALLS {
            self.calls.fetch_sub(1, Ordering::SeqCst);
            self.metrics.upstream(Service::Treasury, Outcome::Busy, None);
            return Err(ApiError::unavailable("too many transfer requests at once; nothing was sent, so try again"));
        }
        let suffix = path.strip_prefix("/v4").unwrap_or(path);
        let mut target = format!("/v1/accounts/{account}{suffix}");
        if !query.is_empty() {
            target.push('?');
            target.push_str(query);
        }
        let request = format!(
            "{method} {target} HTTP/1.1\r\nHost: treasury\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut request = request.into_bytes();
        request.extend_from_slice(body);
        let (tx, response) = sync_channel(1);
        let (address, calls, metrics) = (self.address.clone(), Arc::clone(&self.calls), Arc::clone(&self.metrics));
        thread::spawn(move || {
            let started = HalfMetrics::started();
            let exchanged = exchange(&address, &request);
            let outcome = if exchanged.is_ok() { Outcome::Ok } else { Outcome::Error };
            metrics.upstream(Service::Treasury, outcome, Some(started.elapsed()));
            let answer = exchanged.unwrap_or_else(|e| {
                log::warn("treasury call failed", &[("treasury", &address), ("error", &e)]);
                rest::response(Err(unavailable()))
            });
            calls.fetch_sub(1, Ordering::SeqCst);
            let _ = tx.send(answer);
            notify();
        });
        Ok(Call::new(response))
    }
}

fn connect(address: &str) -> Result<TcpStream, String> {
    let socket = address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("{address} resolves to nothing"))?;
    TcpStream::connect_timeout(&socket, CONNECT).map_err(|e| e.to_string())
}

/// One request and its response, which is interface 1's already and is passed on as it is: the status, and the body
/// as the gateway writes any other.
pub fn exchange(address: &str, request: &[u8]) -> Result<Vec<u8>, String> {
    let mut stream = connect(address)?;
    stream.set_read_timeout(Some(ANSWER)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(ANSWER)).map_err(|e| e.to_string())?;
    stream.write_all(request).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("a response with no head")?;
    let status: u16 = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("a response with no status")?;
    let body: Value = serde_json::from_str(body).map_err(|e| format!("a response that is not JSON: {e}"))?;
    Ok(http::response(status, &body, &[]))
}

/// Follows the treasury's updates for ever, sending each to its account's private sessions, and reconnecting when the
/// stream ends. Updates made while it was not connected are not sent: clients read `GET /v4/transfers` again when they
/// reconnect themselves (interfaces/gateway/README.md), and a session that stayed connected across a treasury restart
/// sees the next change.
pub fn follow_updates(gateway: &Arc<Gateway>, address: String) {
    let gateway = Arc::clone(gateway);
    thread::spawn(move || loop {
        if let Err(e) = follow(&gateway, &address) {
            log::warn("treasury updates ended; reconnecting", &[("treasury", &address), ("error", &e)]);
        }
        thread::sleep(Duration::from_secs(1));
    });
}

fn follow(gateway: &Gateway, address: &str) -> Result<(), String> {
    let mut stream = connect(address)?;
    stream.set_read_timeout(Some(UPDATES_SILENCE)).map_err(|e| e.to_string())?;
    stream.write_all(b"GET /v1/updates HTTP/1.1\r\nHost: treasury\r\n\r\n").map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut in_head = true;
    loop {
        line.clear();
        if reader.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("the stream ended".into());
        }
        let text = line.trim();
        if in_head {
            in_head = !text.is_empty();
            continue;
        }
        if text.is_empty() {
            continue;
        }
        let update: Value = serde_json::from_str(text).map_err(|e| format!("an update that is not JSON: {e}"))?;
        let Some(account) = update["accountId"].as_str().and_then(|a| a.parse().ok()) else { continue };
        let message = json!({"type": "transfer", "transfer": update["transfer"]});
        gateway.hub().send_transfer(account, &message.to_string());
    }
}

/// What a client can rely on for deposits and withdrawals, against a gateway, a treasury and a node, all real.
#[cfg(test)]
mod tests {
    use crate::testing::{self, eventually, MAKER, MAKER_TOKEN, RETAIL_TOKEN};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::Message;

    /// Sends one request and returns the status and the JSON body.
    fn request(address: &str, method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).expect("the gateway accepts a connection");
        stream.set_read_timeout(Some(Duration::from_secs(15))).expect("a read timeout");
        let body = body.map(Value::to_string).unwrap_or_default();
        let auth = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let head =
            format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\n{auth}Content-Length: {}\r\n\r\n", body.len());
        stream.write_all(format!("{head}{body}").as_bytes()).expect("the request is sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("a whole response before the timeout");
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status line");
        let json = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, serde_json::from_str(json).unwrap_or(Value::Null))
    }

    fn transfer(client: &str, kind: &str, cents: u64, method: &str) -> Value {
        json!({"clientTransferId": client, "kind": kind, "amountCents": cents, "method": method})
    }

    fn available(address: &str, token: &str) -> i64 {
        request(address, "GET", "/v4/account", Some(token), None).1["availableCents"].as_i64().unwrap_or(i64::MIN)
    }

    #[test]
    fn transfers_need_a_token_and_a_gateway_with_a_treasury() {
        let venue = testing::venue("transfers-no-treasury");
        let a = &venue.address;
        let deposit = transfer("t", "deposit", 100, "dummy_instant");
        assert_eq!(request(a, "POST", "/v4/transfers", None, Some(&deposit)).0, 401);
        assert_eq!(request(a, "GET", "/v4/transfers", Some("not-a-token"), None).0, 401);
        let (status, refused) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&deposit));
        assert_eq!((status, refused["error"]["code"].as_str()), (503, Some("UNAVAILABLE")), "{refused}");
    }

    #[test]
    fn deposits_and_withdrawals_move_available_cash_through_the_treasury() {
        let venue = testing::venue_with_treasury("transfers");
        let a = &venue.address;
        let (mut session, _) = tungstenite::connect(&venue.ws_private).expect("a private session");
        if let MaybeTlsStream::Plain(s) = session.get_mut() {
            s.set_read_timeout(Some(Duration::from_secs(10))).expect("a read timeout");
        }
        let login = json!({"op": "login", "id": "login", "token": RETAIL_TOKEN}).to_string();
        session.send(Message::text(login)).expect("login sent");
        let start = available(a, RETAIL_TOKEN);

        // An instant deposit is granted before the answer, and resending it changes nothing.
        let deposit = transfer("dep-1", "deposit", 1_234, "dummy_instant");
        let (status, made) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&deposit));
        assert_eq!((status, made["state"].as_str()), (200, Some("settled")), "{made}");
        assert!(made["sequence"].is_string(), "{made}");
        let id = made["transferId"].as_str().expect("a transfer ID").to_string();
        eventually("the deposit reaches available cash", || available(a, RETAIL_TOKEN) == start + 1_234);
        let (status, again) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&deposit));
        assert_eq!((status, again["transferId"].as_str()), (200, Some(id.as_str())), "{again}");
        let reused = transfer("dep-1", "deposit", 1, "dummy_instant");
        let (status, conflict) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&reused));
        assert_eq!(
            (status, conflict["error"]["code"].as_str()),
            (409, Some("CLIENT_TRANSFER_ID_REUSED")),
            "{conflict}"
        );

        // A withdrawal beyond available cash is the engine's refusal, answered as a failed transfer.
        let greedy = transfer("wd-big", "withdrawal", 10_000_000, "dummy_instant");
        let (status, refused) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&greedy));
        assert_eq!((status, refused["state"].as_str()), (200, Some("failed")), "{refused}");
        assert_eq!(refused["reason"], "insufficient_available_cash");
        let withdrawal = transfer("wd-1", "withdrawal", 234, "dummy_instant");
        let (status, paid) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&withdrawal));
        assert_eq!((status, paid["state"].as_str()), (200, Some("settled")), "{paid}");
        eventually("the withdrawal leaves available cash", || available(a, RETAIL_TOKEN) == start + 1_000);

        // A deposit still at the bank can be cancelled, once.
        let slow = transfer("dep-slow", "deposit", 500, "dummy_bank");
        let (status, pending) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&slow));
        assert_eq!((status, pending["state"].as_str()), (200, Some("pending")), "{pending}");
        let slow_path = format!("/v4/transfers/{}", pending["transferId"].as_str().expect("a transfer ID"));
        let (status, cancelled) = request(a, "DELETE", &slow_path, Some(RETAIL_TOKEN), None);
        assert_eq!((status, cancelled["reason"].as_str()), (200, Some("cancelled_by_client")), "{cancelled}");
        let (status, late) = request(a, "DELETE", &slow_path, Some(RETAIL_TOKEN), None);
        assert_eq!((status, late["error"]["code"].as_str()), (409, Some("TRANSFER_NOT_CANCELLABLE")), "{late}");

        // Reads: the account's own transfers, newest first, and nobody else's.
        let (status, list) = request(a, "GET", "/v4/transfers?limit=10", Some(RETAIL_TOKEN), None);
        let states: Vec<&str> =
            list["transfers"].as_array().into_iter().flatten().filter_map(|t| t["state"].as_str()).collect();
        assert_eq!((status, states), (200, vec!["cancelled", "settled", "failed", "settled"]), "{list}");
        let path = format!("/v4/transfers/{id}");
        assert_eq!(request(a, "GET", &path, Some(RETAIL_TOKEN), None).1["clientTransferId"], "dep-1");
        assert_eq!(request(a, "GET", &path, Some(MAKER_TOKEN), None).0, 404);
        assert_eq!(request(a, "DELETE", &path, Some(MAKER_TOKEN), None).0, 404);
        assert_eq!(request(a, "GET", "/v4/transfers/nope", Some(RETAIL_TOKEN), None).0, 400);

        // Every change reached the account's private session, in the order it happened.
        let mut seen = Vec::new();
        while !seen.contains(&"dep-slow cancelled".to_string()) {
            let Ok(Message::Text(text)) = session.read() else { panic!("the session ended after {seen:?}") };
            let message: Value = serde_json::from_str(text.as_str()).expect("JSON");
            if message["type"] == "transfer" {
                let t = &message["transfer"];
                seen.push(format!(
                    "{} {}",
                    t["clientTransferId"].as_str().unwrap_or(""),
                    t["state"].as_str().unwrap_or("")
                ));
            }
        }
        let expected = [
            "dep-1 pending",
            "dep-1 settled",
            "wd-big pending",
            "wd-big failed",
            "wd-1 pending",
            // Reclaimed: still pending, now with the sequence that took the cash.
            "wd-1 pending",
            "wd-1 settled",
            "dep-slow pending",
            "dep-slow cancelled",
        ];
        assert_eq!(seen, expected);
    }

    #[test]
    fn an_internal_transfer_moves_money_within_a_family_and_nowhere_else() {
        let venue = testing::venue_with_treasury("internal-transfers");
        let a = &venue.address;

        let (status, sub) =
            request(a, "POST", "/v4/subaccounts", Some(MAKER_TOKEN), Some(&json!({"label": "scalping"})));
        assert_eq!(status, 200, "{sub}");
        let sub_id = sub["accountId"].as_str().expect("an id").to_string();
        let key_path = format!("/v4/subaccounts/{sub_id}/keys");
        let (status, key) = request(
            a,
            "POST",
            &key_path,
            Some(MAKER_TOKEN),
            Some(&json!({"label": "bot", "permissions": ["read", "trade", "transfers"]})),
        );
        assert_eq!(status, 200, "{key}");
        let sub_cred =
            format!("{}.{}", key["keyId"].as_str().expect("an id"), key["secret"].as_str().expect("a secret"));

        // The subaccount is unknown to the treasury until it has transacted; a deposit through its own key opens it,
        // the same way any account the catalogue never named opens.
        let opening = transfer("open", "deposit", 5_000, "dummy_instant");
        let (status, opened) = request(a, "POST", "/v4/transfers", Some(&sub_cred), Some(&opening));
        assert_eq!((status, opened["state"].as_str()), (200, Some("settled")), "{opened}");
        eventually("the opening deposit reaches available cash", || available(a, &sub_cred) == 5_000);

        let maker_start = available(a, MAKER_TOKEN);
        let move_in =
            json!({"clientTransferId": "in-1", "kind": "internal", "amountCents": 2_000, "toAccountId": sub_id});
        let (status, moved) = request(a, "POST", "/v4/transfers", Some(MAKER_TOKEN), Some(&move_in));
        assert_eq!((status, moved["state"].as_str()), (200, Some("settled")), "{moved}");
        assert_eq!(moved["method"], Value::Null, "an internal transfer names no bank method: {moved}");
        eventually("the source loses it", || available(a, MAKER_TOKEN) == maker_start - 2_000);
        eventually("the destination gains it", || available(a, &sub_cred) == 5_000 + 2_000);

        // An unrelated account is refused: it is not part of this subaccount's family.
        let elsewhere =
            json!({"clientTransferId": "in-2", "kind": "internal", "amountCents": 1, "toAccountId": sub_id});
        let (status, refused) = request(a, "POST", "/v4/transfers", Some(RETAIL_TOKEN), Some(&elsewhere));
        assert_eq!((status, refused["error"]["code"].as_str()), (403, Some("FORBIDDEN")), "{refused}");

        // The other direction works too: the subaccount can move money back to its master.
        let move_back = json!({"clientTransferId": "out-1", "kind": "internal", "amountCents": 1_000,
                                "toAccountId": MAKER.to_string()});
        let (status, back) = request(a, "POST", "/v4/transfers", Some(&sub_cred), Some(&move_back));
        assert_eq!((status, back["state"].as_str()), (200, Some("settled")), "{back}");
    }
}
