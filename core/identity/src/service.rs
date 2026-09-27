//! The identity service's own HTTP API, for the gateways and for operators. It is not interface 1: it trusts the
//! account in the path, because only the private half of the gateway reaches it, and only after authenticating the
//! account and checking it may manage keys (interfaces/gateway/README.md, API keys and permissions). Its answers to
//! the key calls are interface 1's `Key`, `KeyCreated`, `Keys` and `Error`, so the gateway passes them on untouched.
//!
//! - `POST /v1/accounts/{accountId}/keys`: a `KeyRequest`, and `createdBy` when a key asked
//! - `GET /v1/accounts/{accountId}/keys`
//! - `DELETE /v1/accounts/{accountId}/keys/{keyId}`
//! - `POST /v1/verify`: `{"credential"}`, answered `{"keyId", "accountId", "permissions", "expiresAt"}` or 401
//! - `GET /healthz`: `{"ready": true, "database": "ok"}`, or 503 when the key store cannot be reached (the pod is then
//!   not ready, and gateways answer keys they have not verified recently with 503)
//!
//! One thread per connection and one request per connection, like the treasury's: gateways ask about a key once every
//! few seconds, and people manage keys by hand.

use crate::clock;
use crate::store::{Refusal, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// How long a request has to arrive whole.
const ARRIVAL: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 16 * 1024;

pub fn serve(listener: &TcpListener, store: &Arc<Store>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let store = Arc::clone(store);
        thread::spawn(move || {
            if let Err(e) = handle(&store, &stream) {
                eprintln!("identity: {e}");
            }
        });
    }
}

struct Request {
    method: String,
    path: String,
    /// What followed the `?`, as it arrived. Only the mailbox has one.
    query: String,
    body: Vec<u8>,
}

fn read(stream: &TcpStream) -> Result<Request, String> {
    stream.set_read_timeout(Some(ARRIVAL)).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (path, query) = (path.to_string(), query.to_string());
    let mut length = 0;
    for _ in 0..100 {
        let mut header = String::new();
        if reader.read_line(&mut header).map_err(|e| e.to_string())? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                length = value.trim().parse().map_err(|_| "a bad content-length".to_string())?;
            }
        }
    }
    if length > MAX_BODY {
        return Err("the body is too large".into());
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).map_err(|e| e.to_string())?;
    Ok(Request { method, path, query, body })
}

fn respond(mut stream: &TcpStream, status: u16, body: &Value) -> Result<(), String> {
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
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
    .map_err(|e| e.to_string())
}

fn account(text: &str) -> Result<u64, Refusal> {
    let digits = !text.is_empty() && text.len() <= 20 && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten().ok_or_else(|| Refusal::bad("accountId must be a decimal string"))
}

/// One field of a query string, percent-decoded enough for an email address (`%40` for the `@`). Only the mailbox
/// takes one, and only where mail is shown at all.
fn query(query: &str, name: &str) -> Option<String> {
    let raw = query.split('&').find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))?;
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn json_body(request: &Request) -> Result<Value, Refusal> {
    serde_json::from_slice(&request.body).map_err(|e| Refusal::bad(format!("the body is not JSON: {e}")))
}

fn handle(store: &Store, stream: &TcpStream) -> Result<(), String> {
    let answer = match read(stream) {
        Ok(request) => route(store, &request),
        Err(e) => Err(Refusal::bad(format!("unreadable request: {e}"))),
    };
    match answer {
        Ok(body) => respond(stream, 200, &body),
        Err(refusal) => respond(stream, refusal.status, &refusal.body()),
    }
}

fn route(store: &Store, request: &Request) -> Result<Value, Refusal> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let now = clock::wall_ms();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["healthz"]) => match store.health() {
            Ok(()) => Ok(json!({"ready": true, "database": "ok"})),
            Err(e) => Err(Refusal::unavailable(format!("the key store is unavailable: {e}"))),
        },
        ("POST", ["v1", "verify"]) => {
            let body = json_body(request)?;
            let credential = body["credential"].as_str().ok_or_else(|| Refusal::bad("credential is required"))?;
            Ok(store.verify(credential, now)?.json())
        }
        // Members and their sessions (interface 6, in draft). The routes that take no member ID are the ones a
        // person reaches before they have a session: signing up, proving an address, getting a password back,
        // logging in. The rest name the member the gateway verified.
        ("POST", ["v1", "members"]) => store.sign_up(&json_body(request)?, now),
        ("POST", ["v1", "members", "verification"]) => store.resend_verification(&json_body(request)?, now),
        ("POST", ["v1", "members", "verify"]) => store.verify_email(&json_body(request)?, now),
        ("POST", ["v1", "members", "reset"]) => store.request_reset(&json_body(request)?, now),
        ("POST", ["v1", "members", "password"]) => store.set_password(&json_body(request)?, now),
        ("GET", ["v1", "members", member_id]) => store.about(member_id),
        ("GET", ["v1", "members", member_id, "sessions"]) => store.sessions_of(member_id, now),
        ("DELETE", ["v1", "members", member_id, "sessions", session_id]) => {
            store.end_session_of(member_id, session_id, now)
        }
        ("POST", ["v1", "members", member_id, "totp"]) => store.start_totp(member_id, now),
        ("PUT", ["v1", "members", member_id, "totp"]) => store.confirm_totp(member_id, &json_body(request)?, now),
        ("DELETE", ["v1", "members", member_id, "totp"]) => store.remove_totp(member_id, &json_body(request)?, now),
        ("POST", ["v1", "sessions"]) => store.log_in(&json_body(request)?, now),
        ("DELETE", ["v1", "sessions", session_id]) => store.log_out(session_id, now),
        // What this environment would have posted. Only where it is turned on, because the messages hold the very
        // codes they were sent to keep secret (core/identity/README.md, mail).
        ("GET", ["v1", "mail"]) => match store.shows_mail() {
            true => store.mailbox(query(&request.query, "to").as_deref(), 50),
            false => Err(Refusal::not_found("no such route")),
        },
        ("POST", ["v1", "accounts", id, "keys"]) => store.create(account(id)?, &json_body(request)?, now),
        ("GET", ["v1", "accounts", id, "keys"]) => Ok(store.list(account(id)?)),
        ("DELETE", ["v1", "accounts", id, "keys", key_id]) => store.revoke(account(id)?, key_id, now),
        // Subaccounts (decisions D-33, G-21, G-22): a family sharing an owner, its master, but not a book.
        ("POST", ["v1", "accounts", id, "subaccounts"]) => {
            store.create_subaccount(account(id)?, &json_body(request)?, now)
        }
        ("GET", ["v1", "accounts", id, "subaccounts"]) => store.list_subaccounts(account(id)?),
        ("GET", ["v1", "accounts", id]) => store.account_family(account(id)?),
        // Venue-wide, for the gateway's own reconciliation (`reconcile_subaccounts`), not a client route.
        ("GET", ["v1", "subaccounts"]) => store.all_subaccounts(),
        _ => Err(Refusal::not_found("no such route")),
    }
}

#[cfg(test)]
mod tests {
    use crate::Settings;
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn call(address: &str, method: &str, path: &str, body: Option<&Value>) -> (u16, Value) {
        let mut stream = TcpStream::connect(address).expect("connected");
        let body = body.map(Value::to_string).unwrap_or_default();
        let request =
            format!("{method} {path} HTTP/1.1\r\nHost: identity\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        stream.write_all(request.as_bytes()).expect("sent");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("answered");
        let status = response.split_whitespace().nth(1).and_then(|s| s.parse().ok()).expect("a status");
        (status, response.split_once("\r\n\r\n").and_then(|(_, b)| serde_json::from_str(b).ok()).unwrap_or(Value::Null))
    }

    /// A venue of its own on a free port, with its mail readable, so a test can follow a code out of a message the
    /// way a person follows it out of their inbox.
    fn venue() -> String {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
            show_mail: true,
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let address = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");
        address
    }

    /// The code out of the newest message to `to`: the one run of hex digits a token is written as.
    fn code(address: &str, to: &str) -> String {
        let (status, mail) = call(address, "GET", &format!("/v1/mail?to={}", to.replace('@', "%40")), None);
        assert_eq!(status, 200, "{mail}");
        let hex = |word: &&str| word.len() == 32 && word.bytes().all(|b| b.is_ascii_hexdigit());
        mail["mail"]
            .as_array()
            .expect("messages")
            .iter()
            .find_map(|m| m["body"].as_str()?.split_whitespace().find(hex).map(str::to_string))
            .unwrap_or_else(|| panic!("a code in {mail}"))
    }

    #[test]
    fn a_member_signs_up_proves_their_address_logs_in_and_logs_out() {
        let a = venue();
        let signing = json!({"email": "Someone@Example.com", "password": "a good long password"});

        // Signing up says nothing but that a message went out: not the account, not a session, and above all not
        // whether the address was already somebody's.
        let (status, answered) = call(&a, "POST", "/v1/members", Some(&signing));
        assert_eq!((status, &answered), (200, &json!({"checkYourEmail": true})), "{answered}");

        // Logging in works before the address is proved — with everything but moving money.
        let (status, member) = call(&a, "POST", "/v1/sessions", Some(&signing));
        assert_eq!(status, 200, "{member}");
        assert_eq!(member["email"], "someone@example.com", "the email is kept as it is looked up: {member}");
        assert_eq!(member["displayName"], "someone", "{member}");
        assert_eq!(member["emailVerified"], json!(false), "{member}");
        let account = member["accountId"].as_str().expect("an account").to_string();
        assert!(account.parse::<u64>().expect("a number") >= 100_000, "an account of their own: {account}");
        assert!(!member.to_string().contains("a good long password"), "the password is not echoed: {member}");

        let credential = member["session"]["credential"].as_str().expect("a credential").to_string();
        let verify = json!({"credential": credential});
        let (status, verified) = call(&a, "POST", "/v1/verify", Some(&verify));
        assert_eq!((status, &verified["accountId"]), (200, &json!(account)), "{verified}");
        assert_eq!(verified["sessionId"], member["session"]["sessionId"], "{verified}");
        assert_eq!(verified["keyId"], Value::Null, "a session is not a key: {verified}");
        assert_eq!(verified["permissions"], json!(["read", "trade", "keys"]), "no transfers yet: {verified}");

        // The code that went to the address proves it, and the session already open gains transfers from then on,
        // without the member logging in again.
        let token = code(&a, "someone@example.com");
        let (status, proved) = call(&a, "POST", "/v1/members/verify", Some(&json!({"token": token})));
        assert_eq!((status, &proved), (200, &json!({"verified": true})), "{proved}");
        let (_, now_verified) = call(&a, "POST", "/v1/verify", Some(&verify));
        assert_eq!(now_verified["permissions"], json!(["read", "trade", "transfers", "keys"]), "{now_verified}");
        // And the same code cannot be spent twice.
        assert_eq!(call(&a, "POST", "/v1/members/verify", Some(&json!({"token": token}))).0, 400);

        // The same email and password again is the same account, with a session of its own.
        let (status, again) = call(&a, "POST", "/v1/sessions", Some(&signing));
        assert_eq!((status, &again["accountId"]), (200, &json!(account)), "{again}");
        assert_ne!(again["session"]["sessionId"], member["session"]["sessionId"], "a second session");

        // Signing up twice with one email answers exactly as the first time did, and makes no second account. What
        // the second attempt does is tell the address that somebody tried.
        let (status, twice) = call(&a, "POST", "/v1/members", Some(&signing));
        assert_eq!((status, &twice), (200, &json!({"checkYourEmail": true})), "{twice}");
        let (_, mail) = call(&a, "GET", "/v1/mail?to=someone%40example.com", None);
        assert_eq!(mail["mail"][0]["subject"], "Someone tried to sign up with your address", "{mail}");

        // The wrong password, and an email nobody has, are refused the same way.
        let wrong = json!({"email": "someone@example.com", "password": "not the password"});
        let (status, refused) = call(&a, "POST", "/v1/sessions", Some(&wrong));
        assert_eq!((status, refused["error"]["message"].as_str()), (401, Some("the email or password is wrong")));
        let nobody = json!({"email": "nobody@example.com", "password": "not the password"});
        let (status, unknown) = call(&a, "POST", "/v1/sessions", Some(&nobody));
        assert_eq!((status, unknown["error"]["message"].as_str()), (401, Some("the email or password is wrong")));

        // A password too short to be one is refused before it is ever hashed.
        let short = json!({"email": "other@example.com", "password": "short"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&short)).0, 400);

        // Logging out ends that session and no other.
        let path = format!("/v1/sessions/{}", member["session"]["sessionId"].as_str().expect("an ID"));
        assert_eq!(call(&a, "DELETE", &path, None).0, 200);
        let (status, ended) = call(&a, "POST", "/v1/verify", Some(&verify));
        assert_eq!((status, ended["error"]["code"].as_str()), (401, Some("UNAUTHORIZED")), "{ended}");
        let still = again["session"]["credential"].as_str().expect("a credential");
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&json!({"credential": still}))).0, 200);
        // Logging out of a session that is not there says so rather than pretending.
        assert_eq!(call(&a, "DELETE", "/v1/sessions/vs_0000000000000000", None).0, 404);
    }

    #[test]
    fn a_forgotten_password_is_set_again_by_mail_and_every_session_ends() {
        let a = venue();
        let signing = json!({"email": "forgot@example.com", "password": "the first password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        let (_, member) = call(&a, "POST", "/v1/sessions", Some(&signing));
        let credential = member["session"]["credential"].as_str().expect("a credential").to_string();
        let verify = json!({"credential": credential});
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&verify)).0, 200);

        // Asking answers the same whether or not the address is anybody's, and writes to it either way.
        let asked = json!({"email": "forgot@example.com"});
        let (status, answer) = call(&a, "POST", "/v1/members/reset", Some(&asked));
        assert_eq!((status, &answer), (200, &json!({"checkYourEmail": true})), "{answer}");
        let (status, for_nobody) = call(&a, "POST", "/v1/members/reset", Some(&json!({"email": "no@example.com"})));
        assert_eq!((status, &for_nobody), (200, &json!({"checkYourEmail": true})), "{for_nobody}");
        let (_, theirs) = call(&a, "GET", "/v1/mail?to=no%40example.com", None);
        assert_eq!(theirs["mail"][0]["subject"], "Setting a new password", "even with no account: {theirs}");

        // The code sets the password, and everything signed in anywhere is signed out.
        let token = code(&a, "forgot@example.com");
        let setting = json!({"token": token, "password": "a second long password"});
        let (status, set) = call(&a, "POST", "/v1/members/password", Some(&setting));
        assert_eq!((status, &set["passwordSet"], &set["sessionsEnded"]), (200, &json!(true), &json!(1)), "{set}");
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&verify)).0, 401, "the old session is gone");
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&signing)).0, 401, "and so is the old password");
        let now = json!({"email": "forgot@example.com", "password": "a second long password"});
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&now)).0, 200);
        // A code is good once.
        assert_eq!(call(&a, "POST", "/v1/members/password", Some(&setting)).0, 400);
    }

    #[test]
    fn repeated_wrong_passwords_lock_the_member_out_and_say_so() {
        let a = venue();
        let signing = json!({"email": "guessed@example.com", "password": "the right password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        let wrong = json!({"email": "guessed@example.com", "password": "not the password"});
        for attempt in 1..=5 {
            let (status, refused) = call(&a, "POST", "/v1/sessions", Some(&wrong));
            assert_eq!((attempt, status, refused["error"]["code"].as_str()), (attempt, 401, Some("UNAUTHORIZED")));
        }
        // Now even the right password is refused, and with a code of its own, so an app can say what is wrong.
        let (status, locked) = call(&a, "POST", "/v1/sessions", Some(&signing));
        assert_eq!((status, locked["error"]["code"].as_str()), (429, Some("LOCKED_OUT")), "{locked}");
        let (_, mail) = call(&a, "GET", "/v1/mail?to=guessed%40example.com", None);
        assert_eq!(mail["mail"][0]["subject"], "Too many attempts to log in", "the member is told: {mail}");

        // Setting a new password is the way back in, and it lifts the lock.
        assert_eq!(call(&a, "POST", "/v1/members/reset", Some(&json!({"email": "guessed@example.com"}))).0, 200);
        let token = code(&a, "guessed@example.com");
        let setting = json!({"token": token, "password": "a brand new password"});
        assert_eq!(call(&a, "POST", "/v1/members/password", Some(&setting)).0, 200);
        let now = json!({"email": "guessed@example.com", "password": "a brand new password"});
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&now)).0, 200, "the lock lifted with the password");
    }

    #[test]
    fn a_second_factor_is_asked_for_at_the_next_login_and_a_recovery_code_also_gets_in() {
        let a = venue();
        let signing = json!({"email": "careful@example.com", "password": "a good long password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        let (_, member) = call(&a, "POST", "/v1/sessions", Some(&signing));
        let id = member["memberId"].as_str().expect("a member").to_string();

        // Starting one answers a secret for an authenticator app, and changes nothing until a code from it arrives.
        let (status, started) = call(&a, "POST", &format!("/v1/members/{id}/totp"), None);
        assert_eq!(status, 200, "{started}");
        let secret = crate::totp::from_base32(started["secret"].as_str().expect("a secret")).expect("base32");
        assert!(started["uri"].as_str().expect("a uri").starts_with("otpauth://totp/Venue:"), "{started}");
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&signing)).0, 200, "not asked for until it is confirmed");

        // A wrong code confirms nothing; the right one answers the recovery codes, which are shown this once.
        let step = crate::totp::step_at(crate::clock::wall_ms() / 1000);
        assert_eq!(call(&a, "PUT", &format!("/v1/members/{id}/totp"), Some(&json!({"code": "000000"}))).0, 400);
        let now = crate::totp::code(&secret, step);
        let (status, confirmed) = call(&a, "PUT", &format!("/v1/members/{id}/totp"), Some(&json!({"code": now})));
        assert_eq!((status, &confirmed["confirmed"]), (200, &json!(true)), "{confirmed}");
        let codes: Vec<String> = confirmed["recoveryCodes"]
            .as_array()
            .expect("codes")
            .iter()
            .map(|c| c.as_str().expect("a code").to_string())
            .collect();
        assert_eq!(codes.len(), 10, "{confirmed}");

        // From now on the password alone is not enough, and the refusal says which of the two is missing.
        let (status, asked) = call(&a, "POST", "/v1/sessions", Some(&signing));
        assert_eq!((status, asked["error"]["code"].as_str()), (401, Some("SECOND_FACTOR_REQUIRED")), "{asked}");
        // The code that confirmed the factor is spent, so logging in with it again does not work — the app shows
        // the next one, which does. (A test cannot wait half a minute for it, so it asks for the step ahead, which
        // is inside the drift a clock a little fast would need anyway.)
        let used = json!({"email": "careful@example.com", "password": "a good long password", "code": now});
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&used)).0, 401, "a code is good once");
        let next = crate::totp::code(&secret, step + 1);
        let with_code = json!({"email": "careful@example.com", "password": "a good long password", "code": next});
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&with_code)).0, 200);
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&with_code)).0, 401, "and that one is spent too");

        // A recovery code gets in instead, however it was copied down, and is itself good once.
        let shouted = codes[0].to_uppercase().replace('-', " ");
        let recovery = json!({"email": "careful@example.com", "password": "a good long password", "code": shouted});
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&recovery)).0, 200, "{shouted}");
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&recovery)).0, 401, "a recovery code is used once");
        let (_, about) = call(&a, "GET", &format!("/v1/members/{id}"), None);
        assert_eq!((&about["secondFactor"], &about["recoveryCodesLeft"]), (&json!(true), &json!(9)), "{about}");

        // Taking it off needs the password, not just a session: a session somebody else is holding must not be able
        // to take the second factor off the account.
        let path = format!("/v1/members/{id}/totp");
        assert_eq!(call(&a, "DELETE", &path, Some(&json!({"password": "not it"}))).0, 401);
        assert_eq!(call(&a, "DELETE", &path, Some(&json!({"password": "a good long password"}))).0, 200);
        assert_eq!(call(&a, "POST", "/v1/sessions", Some(&signing)).0, 200, "the password alone again");
    }

    #[test]
    fn a_member_sees_their_own_sessions_and_ends_the_one_they_do_not_know() {
        let a = venue();
        let signing = json!({"email": "many@example.com", "password": "a good long password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        let phone = json!({"email": "many@example.com", "password": "a good long password", "device": "a phone"});
        let (_, first) = call(&a, "POST", "/v1/sessions", Some(&phone));
        let (_, second) = call(&a, "POST", "/v1/sessions", Some(&signing));
        let id = first["memberId"].as_str().expect("a member").to_string();

        let (status, held) = call(&a, "GET", &format!("/v1/members/{id}/sessions"), None);
        assert_eq!(status, 200, "{held}");
        let sessions = held["sessions"].as_array().expect("sessions");
        assert_eq!(sessions.len(), 2, "{held}");
        assert!(sessions.iter().any(|s| s["device"] == json!("a phone")), "named by the app: {held}");
        assert!(sessions.iter().all(|s| s["live"] == json!(true)), "{held}");

        // Ending the one they do not recognise leaves the one they are using.
        let gone = second["session"]["sessionId"].as_str().expect("an ID").to_string();
        assert_eq!(call(&a, "DELETE", &format!("/v1/members/{id}/sessions/{gone}"), None).0, 200);
        let mine = first["session"]["credential"].as_str().expect("a credential");
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&json!({"credential": mine}))).0, 200);
        let theirs = second["session"]["credential"].as_str().expect("a credential");
        assert_eq!(call(&a, "POST", "/v1/verify", Some(&json!({"credential": theirs}))).0, 401);

        // And a session that is not theirs is not theirs to end, whoever holds its ID.
        let other = json!({"email": "someone.else@example.com", "password": "a good long password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&other)).0, 200);
        let (_, stranger) = call(&a, "POST", "/v1/sessions", Some(&other));
        let not_mine = stranger["session"]["sessionId"].as_str().expect("an ID").to_string();
        assert_eq!(call(&a, "DELETE", &format!("/v1/members/{id}/sessions/{not_mine}"), None).0, 404);
    }

    #[test]
    fn nobody_is_staff_by_signing_up_and_only_the_records_can_make_them() {
        let a = venue();
        let signing = json!({"email": "dev@example.com", "password": "a good long password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        let (_, member) = call(&a, "POST", "/v1/sessions", Some(&signing));
        let id = member["memberId"].as_str().expect("a member").to_string();

        // Signing up never grants it, whatever the request says. The field is not one of the ones sign-up reads, so
        // asking for it is a malformed request rather than a quietly ignored one.
        let (status, asked) = call(
            &a,
            "POST",
            "/v1/members",
            Some(&json!({"email": "sneak@example.com", "password": "a good long password", "staff": true})),
        );
        assert_eq!((status, asked["error"]["code"].as_str()), (400, Some("BAD_REQUEST")), "{asked}");

        let (status, about) = call(&a, "GET", &format!("/v1/members/{id}"), None);
        assert_eq!((status, &about["staff"]), (200, &json!(false)), "{about}");

        // There is no route that sets it: an operator does, against the records, and that is the whole point — no
        // session, no key and no request can reach it (decision G-20).
        assert_eq!(call(&a, "POST", &format!("/v1/members/{id}/staff"), Some(&json!({"staff": true}))).0, 404);
    }

    #[test]
    fn mail_is_only_shown_where_the_venue_was_told_to_show_it() {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
            show_mail: false,
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let a = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");

        let signing = json!({"email": "quiet@example.com", "password": "a good long password"});
        assert_eq!(call(&a, "POST", "/v1/members", Some(&signing)).0, 200);
        // The mail was still written down. It is the route that is not there.
        let (status, refused) = call(&a, "GET", "/v1/mail", None);
        assert_eq!((status, refused["error"]["code"].as_str()), (404, Some("NOT_FOUND")), "{refused}");
    }

    #[test]
    fn keys_are_made_verified_listed_and_revoked_over_http() {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
            show_mail: false,
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let a = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");

        assert_eq!(call(&a, "GET", "/healthz", None).0, 200);
        let (status, created) =
            call(&a, "POST", "/v1/accounts/2/keys", Some(&json!({"label": "script", "permissions": ["read"]})));
        assert_eq!(status, 200, "{created}");
        let (status, verified) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": created["credential"]})));
        assert_eq!((status, &verified["accountId"], &verified["permissions"]), (200, &json!("2"), &json!(["read"])));
        assert_eq!(call(&a, "GET", "/v1/accounts/2/keys", None).1["keys"][0]["keyId"], created["keyId"]);
        let path = format!("/v1/accounts/2/keys/{}", created["keyId"].as_str().expect("an ID"));
        assert_eq!(call(&a, "DELETE", "/v1/accounts/1/keys/ak_0000000000000000", None).0, 404);
        assert_eq!(call(&a, "DELETE", &path, None).0, 200);
        let (status, refused) = call(&a, "POST", "/v1/verify", Some(&json!({"credential": created["credential"]})));
        assert_eq!((status, refused["error"]["code"].as_str()), (401, Some("UNAUTHORIZED")), "{refused}");
        assert_eq!(
            call(&a, "POST", "/v1/accounts/99/keys", Some(&json!({"label": "x", "permissions": ["read"]}))).0,
            404
        );
        assert_eq!(call(&a, "GET", "/v1/nowhere", None).0, 404);
    }

    #[test]
    fn subaccounts_are_made_and_listed_over_http_and_cannot_nest() {
        let settings = Settings {
            catalogue: concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/charts/venue-mini/files/markets.json").into(),
            show_mail: false,
        };
        let records = crate::records(&crate::Storage::Memory(crate::records::Memory::default())).expect("records");
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
        let a = listener.local_addr().expect("an address").to_string();
        crate::start(&settings, records, listener).expect("started");

        assert_eq!(call(&a, "GET", "/v1/accounts/1", None).1["masterAccountId"], Value::Null);
        let (status, created) = call(&a, "POST", "/v1/accounts/1/subaccounts", Some(&json!({"label": "scalping"})));
        assert_eq!(status, 200, "{created}");
        assert_eq!(created["masterAccountId"], json!("1"));
        let sub = created["accountId"].as_str().expect("an id").to_string();

        assert_eq!(call(&a, "GET", &format!("/v1/accounts/{sub}"), None).1["masterAccountId"], json!("1"));
        let (status, listed) = call(&a, "GET", "/v1/accounts/1/subaccounts", None);
        assert_eq!((status, &listed["subaccounts"][0]["accountId"]), (200, &json!(sub)), "{listed}");

        // A subaccount cannot itself be a master.
        let path = format!("/v1/accounts/{sub}/subaccounts");
        assert_eq!(call(&a, "POST", &path, Some(&json!({"label": "nested"}))).0, 400);
        assert_eq!(
            call(&a, "POST", "/v1/accounts/99999/subaccounts", Some(&json!({"label": "x"}))).0,
            404,
            "no such master account"
        );

        // Venue-wide, for the gateway's own reconciliation: every subaccount, whoever its master is.
        let (status, all) = call(&a, "GET", "/v1/subaccounts", None);
        assert_eq!((status, &all["subaccounts"][0]["accountId"]), (200, &json!(sub)), "{all}");
        assert_eq!(all["subaccounts"][0]["masterAccountId"], json!("1"));
        assert_eq!(all["subaccounts"].as_array().map(Vec::len), Some(1), "account 1 is a master, not listed here");
    }
}
