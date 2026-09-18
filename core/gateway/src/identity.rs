//! Who a credential is and what it may do (interfaces/gateway/README.md, API keys and permissions).
//!
//! A dev token is looked up in the catalogue and holds every permission. An API key is the identity service's to judge
//! (`core/identity`): the gateway asks it and remembers the answer, valid or not, for `REMEMBER`, so a key in use
//! costs one round trip every few seconds rather than one a request, and a failover loses nothing that matters. A
//! thread asks again about every credential still in use before its answer grows old, and the event loops close the
//! sessions of keys that stop being valid (`still_valid`), which is what bounds how long a revoked key keeps working:
//! at most `REMEMBER` on REST, and `REMEMBER` plus one check and one sweep for a session, under the 10 seconds the
//! contract promises.
//!
//! Managing keys (`/v4/keys`) is passed to the identity service as it came, once the gateway has checked the caller may
//! and that a key is not making a key with more than it holds; the answer is interface 1's already.

use crate::api::{only, ApiError, Gateway};
use crate::http::Request;
use crate::limits::monotonic;
use crate::metrics::{HalfMetrics, Outcome, Service};
use crate::treasury::{self, Call};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};
use venue_identity::clock;
use venue_identity::key::{self, Permissions};
use venue_identity::member;
use venue_link::submit::Notify;
use venue_telemetry::log;

/// How long an answer about a credential is taken as true.
pub const REMEMBER: Duration = Duration::from_secs(5);
/// How long an identity service that did not answer is left alone before it is asked again.
const UNREACHABLE: Duration = Duration::from_secs(1);
/// How often credentials in use are asked about again.
const CHECK_EVERY: Duration = Duration::from_secs(1);
/// A credential nobody has used for this long is forgotten.
const FORGET: Duration = Duration::from_secs(60);
/// Answers remembered at most. Past this those not used lately are forgotten, so a flood of made-up keys costs memory
/// that is bounded.
const MAX_REMEMBERED: usize = 65_536;
/// How long a verification may take. It is asked on the event loop for a credential not remembered, so it is short;
/// the service is on the same network as the gateway.
const VERIFY_TIMEOUT: Duration = Duration::from_millis(1000);
/// Calls managing keys outstanding at once, across every client of this gateway.
const MAX_CALLS: usize = 16;

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// An authenticated credential: whose it is and what it may do.
#[derive(Clone, Debug)]
pub struct Credential {
    pub account: u64,
    /// The ID of the credential that authenticated: an API key's (`ak_`) or a member session's (`vs_`), which their
    /// prefixes tell apart. None for a dev token, which has no ID.
    pub key: Option<String>,
    /// Which member, when a session authenticated. An API key belongs to an account rather than to a person, so it
    /// has none, and neither does a dev token. A member's own routes are addressed with it.
    pub member: Option<String>,
    pub permissions: Permissions,
    /// When the key expires, in milliseconds since the epoch.
    expires_at: Option<u64>,
    /// What the key's answer is remembered under, for a session to look it up without asking again.
    fingerprint: Option<[u8; 32]>,
}

impl Credential {
    /// A dev token: its account, with every permission.
    pub const fn dev_token(account: u64) -> Self {
        Self { account, key: None, member: None, permissions: Permissions::ALL, expires_at: None, fingerprint: None }
    }

    /// Refuses with 403 `FORBIDDEN` unless the credential holds `permission`.
    pub fn require(&self, permission: Permissions) -> Result<(), ApiError> {
        if self.permissions.contains(permission) {
            Ok(())
        } else {
            Err(ApiError::forbidden(format!("this API key does not hold the {} permission", permission.name())))
        }
    }
}

#[derive(Clone)]
enum Verdict {
    Valid(Credential),
    /// Not a valid key, with why.
    Refused(String),
    /// The identity service did not answer.
    Unreachable,
}

impl Verdict {
    fn answer(&self) -> Result<Credential, ApiError> {
        match self {
            Self::Valid(credential) => Ok(credential.clone()),
            Self::Refused(why) => Err(ApiError::unauthorized_because(why)),
            Self::Unreachable => Err(ApiError::unavailable("the identity service did not answer; try again")),
        }
    }
}

struct Entry {
    credential: String,
    verdict: Verdict,
    checked: Instant,
    used: Instant,
}

impl Entry {
    fn fresh(&self, now: Instant) -> bool {
        let lasts = if matches!(self.verdict, Verdict::Unreachable) { UNREACHABLE } else { REMEMBER };
        now.duration_since(self.checked) < lasts
    }
}

/// The identity service, by address, and what it has said lately.
pub struct Identity {
    address: Option<String>,
    /// Whether the catalogue's dev tokens are taken (`--dev-tokens`).
    pub dev_tokens: bool,
    remembered: Mutex<BTreeMap<[u8; 32], Entry>>,
    calls: Arc<AtomicUsize>,
    metrics: Arc<HalfMetrics>,
}

impl Identity {
    /// A gateway's identity: API keys through the service at `address`, if any, and dev tokens if allowed.
    pub fn new(address: Option<String>, dev_tokens: bool, metrics: Arc<HalfMetrics>) -> Self {
        let (remembered, calls) = (Mutex::new(BTreeMap::new()), Arc::new(AtomicUsize::new(0)));
        Self { address, dev_tokens, remembered, calls, metrics }
    }

    /// Asks the service about a credential, counting the call.
    fn ask(&self, address: &str, credential: &str, fingerprint: [u8; 32]) -> Verdict {
        let started = monotonic();
        let verdict = ask(address, credential, fingerprint);
        let outcome = if matches!(verdict, Verdict::Unreachable) { Outcome::Error } else { Outcome::Ok };
        self.metrics.upstream(Service::Identity, outcome, Some(monotonic().duration_since(started)));
        verdict
    }

    /// Whether API keys are verified here at all.
    pub const fn serves_keys(&self) -> bool {
        self.address.is_some()
    }

    fn address(&self) -> Result<&str, ApiError> {
        self.address
            .as_deref()
            .ok_or_else(|| ApiError::unavailable("API keys are not served here: this gateway has no identity service"))
    }

    /// Who a credential is — an API key's or a member's session — remembered, or asked of the service.
    pub fn verify_credential(&self, credential: &str) -> Result<Credential, ApiError> {
        let address = self.address()?;
        let fingerprint: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
        let now = monotonic();
        if let Some(entry) = locked(&self.remembered).get_mut(&fingerprint) {
            entry.used = now;
            if entry.fresh(now) {
                return entry.verdict.answer();
            }
        }
        let verdict = self.ask(address, credential, fingerprint);
        let answer = verdict.answer();
        self.remember(fingerprint, credential, verdict, now);
        answer
    }

    /// Forgets what was remembered about a credential, so the next request asks the identity service again. Used
    /// when the gateway itself has just ended a session: waiting for the answer to go stale would leave a token that
    /// logged out working until it did.
    pub fn forget(&self, credential: &str) {
        let fingerprint: [u8; 32] = Sha256::digest(credential.as_bytes()).into();
        locked(&self.remembered).remove(&fingerprint);
    }

    fn remember(&self, fingerprint: [u8; 32], credential: &str, verdict: Verdict, now: Instant) {
        let mut remembered = locked(&self.remembered);
        if remembered.len() >= MAX_REMEMBERED && !remembered.contains_key(&fingerprint) {
            remembered.retain(|_, e| now.duration_since(e.used) < REMEMBER);
            // Still full of credentials in use: this one is answered without being remembered, rather than forgetting
            // one a session depends on for its revocation to be noticed.
            if remembered.len() >= MAX_REMEMBERED {
                return;
            }
        }
        let used = remembered.get(&fingerprint).map_or(now, |e| e.used);
        remembered.insert(fingerprint, Entry { credential: credential.to_string(), verdict, checked: now, used });
    }

    /// Whether a session's credential is still valid, by what was last heard, without asking. A dev token always is,
    /// and so is a key whose service has stopped answering: sessions are not ended because the service is down.
    pub fn still_valid(&self, credential: &Credential) -> bool {
        let Some(fingerprint) = credential.fingerprint else { return true };
        let mut remembered = locked(&self.remembered);
        let Some(entry) = remembered.get_mut(&fingerprint) else { return true };
        entry.used = monotonic();
        !matches!(entry.verdict, Verdict::Refused(_))
    }

    /// Asks again about every credential used lately whose answer is about to grow old, and forgets the rest.
    fn check_in_use(&self) {
        let now = monotonic();
        let due: Vec<(String, [u8; 32])> = {
            let mut remembered = locked(&self.remembered);
            remembered.retain(|_, e| now.duration_since(e.used) < FORGET);
            remembered
                .iter()
                .filter(|(_, e)| now.duration_since(e.used) < REMEMBER * 2)
                .filter(|(_, e)| now.duration_since(e.checked) + CHECK_EVERY >= REMEMBER)
                .map(|(f, e)| (e.credential.clone(), *f))
                .collect()
        };
        let Some(address) = self.address.as_deref() else { return };
        for (credential, fingerprint) in due {
            let verdict = self.ask(address, &credential, fingerprint);
            // A service that stopped answering leaves the last real answer in place until it answers again.
            if matches!(verdict, Verdict::Unreachable) {
                continue;
            }
            self.remember(fingerprint, &credential, verdict, monotonic());
        }
    }

    /// Passes a call managing keys to the service, as the same call under the account's own path there.
    fn pass(&self, account: u64, method: &str, suffix: &str, body: &[u8], notify: Notify) -> Result<Call, ApiError> {
        self.pass_to(method, &format!("/v1/accounts/{account}/keys{suffix}"), body, notify)
    }

    /// The same, to a path of the identity service's own: members and sessions are not an account's keys.
    fn pass_to(&self, method: &str, path: &str, body: &[u8], notify: Notify) -> Result<Call, ApiError> {
        let address = self.address()?.to_string();
        if self.calls.fetch_add(1, Ordering::SeqCst) >= MAX_CALLS {
            self.calls.fetch_sub(1, Ordering::SeqCst);
            self.metrics.upstream(Service::Identity, Outcome::Busy, None);
            return Err(ApiError::unavailable("too many key requests at once; nothing was sent, so try again"));
        }
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: identity\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut request = head.into_bytes();
        request.extend_from_slice(body);
        let (tx, response) = sync_channel(1);
        let (calls, metrics) = (Arc::clone(&self.calls), Arc::clone(&self.metrics));
        thread::spawn(move || {
            let started = HalfMetrics::started();
            let exchanged = treasury::exchange(&address, &request);
            let outcome = if exchanged.is_ok() { Outcome::Ok } else { Outcome::Error };
            metrics.upstream(Service::Identity, outcome, Some(started.elapsed()));
            let answer = exchanged.unwrap_or_else(|e| {
                log::warn("identity service call failed", &[("identity", &address), ("error", &e)]);
                crate::rest::response(Err(ApiError::unavailable(
                    "the identity service did not answer; list the keys to see whether anything changed",
                )))
            });
            calls.fetch_sub(1, Ordering::SeqCst);
            let _ = tx.send(answer);
            notify();
        });
        Ok(Call::new(response))
    }
}

impl Identity {
    /// Whether a member is one of the venue's own people, asked of the identity service each time rather than kept
    /// with the credential. Being made one of them, or ceasing to be, should take effect when it is done and not at
    /// whatever later moment a cached answer expires or a member next signs in — and this is asked once when an app
    /// opens, so there is nothing to save by remembering it.
    ///
    /// False whenever the answer is not plainly yes: no identity service, no such member, a service that did not
    /// answer. The failure worth avoiding is naming an internal address to somebody who should not have it, so the
    /// direction to fail in is obvious.
    pub fn staff(&self, member_id: &str) -> bool {
        let Ok(address) = self.address() else { return false };
        if !member::is_member_id(member_id) {
            return false;
        }
        let request = format!(
            "GET /v1/members/{member_id} HTTP/1.1\r\nHost: identity\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        match treasury::exchange(address, request.as_bytes()) {
            Ok(response) => {
                let text = String::from_utf8_lossy(&response);
                let Some((_, body)) = text.split_once("\r\n\r\n") else { return false };
                serde_json::from_str::<Value>(body).is_ok_and(|v| v["staff"] == Value::Bool(true))
            }
            Err(e) => {
                log::warn("asking whether a member is staff failed", &[("identity", &address), ("error", &e)]);
                false
            }
        }
    }
}

/// Keeps asking about the credentials a half's clients are using, for as long as the half exists.
pub fn keep_checking(gateway: &Arc<Gateway>) {
    let half = Arc::downgrade(gateway);
    thread::spawn(move || loop {
        thread::sleep(CHECK_EVERY);
        let Some(gateway) = half.upgrade() else { return };
        gateway.identity.check_in_use();
    });
}

/// One `POST /v1/verify`.
fn ask(address: &str, credential: &str, fingerprint: [u8; 32]) -> Verdict {
    match verify_call(address, credential) {
        Ok((200, answer)) => match valid(&answer, fingerprint) {
            Some(credential) => Verdict::Valid(credential),
            None => {
                log::warn("identity service answer unreadable", &[("identity", &address), ("answer", &answer)]);
                Verdict::Unreachable
            }
        },
        Ok((401, answer)) => {
            Verdict::Refused(answer["error"]["message"].as_str().unwrap_or("the API key is not valid").to_string())
        }
        Ok((status, answer)) => {
            log::warn("identity service refused", &[("identity", &address), ("status", &status), ("answer", &answer)]);
            Verdict::Unreachable
        }
        Err(e) => {
            log::warn("identity service unreachable", &[("identity", &address), ("error", &e)]);
            Verdict::Unreachable
        }
    }
}

fn valid(answer: &Value, fingerprint: [u8; 32]) -> Option<Credential> {
    let names = answer["permissions"].as_array()?.iter().map(Value::as_str).collect::<Option<Vec<&str>>>()?;
    Some(Credential {
        account: answer["accountId"].as_str()?.parse().ok()?,
        // A key or a session: the identity service answers with whichever it verified.
        key: Some(
            answer["keyId"]
                .as_str()
                .filter(|id| key::is_id(id))
                .or_else(|| answer["sessionId"].as_str().filter(|id| member::is_session_id(id)))?
                .to_string(),
        ),
        member: answer["memberId"].as_str().map(str::to_string),
        permissions: Permissions::from_names(names).ok()?,
        expires_at: match &answer["expiresAt"] {
            Value::Null => None,
            v => Some(clock::parse_rfc3339(v.as_str()?)?),
        },
        fingerprint: Some(fingerprint),
    })
}

fn verify_call(address: &str, credential: &str) -> Result<(u16, Value), String> {
    let socket = address
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("{address} resolves to nothing"))?;
    let mut stream = TcpStream::connect_timeout(&socket, VERIFY_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(VERIFY_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(VERIFY_TIMEOUT)).map_err(|e| e.to_string())?;
    let body = json!({"credential": credential}).to_string();
    let request = format!(
        "POST /v1/verify HTTP/1.1\r\nHost: identity\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text.split_once("\r\n\r\n").ok_or("a response with no head")?;
    let status = head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).ok_or("a response with no status")?;
    let answer = serde_json::from_str(body).map_err(|e| format!("a response that is not JSON: {e}"))?;
    Ok((status, answer))
}

/// `/v4/keys`: the caller authenticated and allowed to manage keys, then the call passed to the identity service.
/// Signing up, logging in and logging out, passed to the identity service (interface 1 README, members and
/// sessions). The first two are the only requests the gateway takes from nobody, so they are limited by where they
/// came from rather than by an account.
pub fn members(gateway: &Gateway, request: &Request, client: &str, notify: &Notify) -> Result<Call, ApiError> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let identity = &gateway.identity;
    let notify = Arc::clone(notify);
    match (request.method.as_str(), segments.as_slice()) {
        ("POST", ["v4", "members"]) => {
            gateway.anonymous_rate(client)?;
            identity.pass_to("POST", "/v1/members", &request.body, notify)
        }
        // Proving an address, and getting a password back. All four are reached by somebody with no session — that
        // is the point of them — so all four are limited by where they came from, and all four answer the same
        // whether or not the address is anybody's (interfaces/gateway/decisions.md, G-16).
        ("POST", ["v4", "members", step @ ("verification" | "verify" | "reset" | "password")]) => {
            gateway.anonymous_rate(client)?;
            identity.pass_to("POST", &format!("/v1/members/{step}"), &request.body, notify)
        }
        ("POST", ["v4", "sessions"]) => {
            gateway.anonymous_rate(client)?;
            identity.pass_to("POST", "/v1/sessions", &request.body, notify)
        }
        // The mail this environment would have sent. The identity service answers 404 unless it was started with
        // --show-mail, so a venue that can really post to an address never serves it.
        ("GET", ["v4", "mail"]) => {
            gateway.anonymous_rate(client)?;
            let to = request.path.split_once('?').map_or(String::new(), |(_, q)| format!("?{q}"));
            identity.pass_to("GET", &format!("/v1/mail{to}"), &[], notify)
        }
        ("DELETE", ["v4", "sessions", "current"]) => {
            // Only the session itself may end itself, which is what holding its credential proves.
            let credential = crate::rest::credential(gateway, request, Permissions::READ)?;
            let session = credential
                .key
                .as_deref()
                .filter(|id| member::is_session_id(id))
                .ok_or_else(|| ApiError::bad("this is not a member session"))?;
            // Forgotten here rather than when the answer comes: if the call fails the session stands, and the only
            // cost is that the next request verifies it again.
            if let Some(token) = crate::rest::bearer(request) {
                identity.forget(token);
            }
            identity.pass_to("DELETE", &format!("/v1/sessions/{session}"), &[], notify)
        }
        _ => Err(ApiError::not_found("no such route")),
    }
}

/// A member's own account: what they are, the sessions they hold elsewhere, and their second factor. Every route
/// here is addressed by the member the session belongs to, never by one named in the request, so a session can only
/// ever reach its own (interfaces/gateway/README.md, members and sessions).
pub fn me(gateway: &Gateway, request: &Request, notify: &Notify) -> Result<Call, ApiError> {
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let identity = &gateway.identity;
    let notify = Arc::clone(notify);
    let credential = crate::rest::credential(gateway, request, Permissions::READ)?;
    let member = credential
        .member
        .as_deref()
        .ok_or_else(|| ApiError::bad("these are a member's own routes, and this is an API key"))?;
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["v4", "members", "me"]) => identity.pass_to("GET", &format!("/v1/members/{member}"), &[], notify),
        ("GET", ["v4", "sessions"]) => identity.pass_to("GET", &format!("/v1/members/{member}/sessions"), &[], notify),
        ("DELETE", ["v4", "sessions", session]) => {
            if !member::is_session_id(session) {
                return Err(ApiError::bad("sessionId must be vs_ and 16 lower-case hex digits"));
            }
            identity.pass_to("DELETE", &format!("/v1/members/{member}/sessions/{session}"), &[], notify)
        }
        // Starting a second factor answers a secret, confirming it answers the recovery codes, and removing it wants
        // the password in the body: a session someone else is holding must not be able to take the factor off.
        ("POST", ["v4", "members", "me", "totp"]) => {
            identity.pass_to("POST", &format!("/v1/members/{member}/totp"), &[], notify)
        }
        ("PUT", ["v4", "members", "me", "totp"]) => {
            identity.pass_to("PUT", &format!("/v1/members/{member}/totp"), &request.body, notify)
        }
        ("DELETE", ["v4", "members", "me", "totp"]) => {
            identity.pass_to("DELETE", &format!("/v1/members/{member}/totp"), &request.body, notify)
        }
        _ => Err(ApiError::not_found("no such route")),
    }
}

pub fn manage(
    gateway: &Gateway,
    credential: &Credential,
    request: &Request,
    notify: &Notify,
) -> Result<Call, ApiError> {
    credential.require(Permissions::KEYS)?;
    let segments: Vec<&str> = request.path.trim_matches('/').split('/').collect();
    let identity = &gateway.identity;
    let notify = Arc::clone(notify);
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["v4", "keys"]) => identity.pass(credential.account, "GET", "", &[], notify),
        ("DELETE", ["v4", "keys", key_id]) => {
            if !key::is_id(key_id) {
                return Err(ApiError::bad("keyId must be ak_ and 16 lower-case hex digits"));
            }
            identity.pass(credential.account, "DELETE", &format!("/{key_id}"), &[], notify)
        }
        ("POST", ["v4", "keys"]) => {
            let mut body: Value = serde_json::from_slice(&request.body)
                .map_err(|e| ApiError::bad(format!("the body is not JSON: {e}")))?;
            let fields = only(&body, &["label", "permissions", "expiresAt"])?;
            let asked = fields
                .get("permissions")
                .and_then(Value::as_array)
                .map(|names| names.iter().map(Value::as_str).collect::<Option<Vec<&str>>>())
                .and_then(|names| Permissions::from_names(names?).ok())
                .ok_or_else(|| {
                    ApiError::bad("permissions must be a list of read, trade, transfers, keys and respond")
                })?;
            if credential.key.is_some() {
                beyond_the_creator(credential, asked, fields.get("expiresAt"))?;
                body["createdBy"] = json!(credential.key);
            }
            identity.pass(credential.account, "POST", "", body.to_string().as_bytes(), notify)
        }
        _ => Err(ApiError::not_found("no such route")),
    }
}

/// Refuses a key made by a key that would hold a permission its maker does not, or outlive it (decision G-5).
fn beyond_the_creator(creator: &Credential, asked: Permissions, expires_at: Option<&Value>) -> Result<(), ApiError> {
    if let Some(missing) = Permissions::ALL.names().into_iter().find(|name| {
        let one = Permissions::from_names([*name]).unwrap_or(Permissions::NONE);
        asked.contains(one) && !creator.permissions.contains(one)
    }) {
        return Err(ApiError::forbidden(format!(
            "a key cannot make a key with a permission it does not hold itself: {missing}"
        )));
    }
    if let Some(limit) = creator.expires_at {
        // A malformed expiry is the identity service's to refuse, as for any caller.
        let expiry = match expires_at.and_then(Value::as_str) {
            Some(text) => match clock::parse_rfc3339(text) {
                Some(t) => Some(t),
                None => return Ok(()),
            },
            None => None,
        };
        if expiry.is_none_or(|t| t > limit) {
            return Err(ApiError::forbidden(format!(
                "a key cannot make a key that outlives it: expiresAt must be no later than {}",
                clock::rfc3339(limit)
            )));
        }
    }
    Ok(())
}

/// Who may do what with API keys, against a gateway, a node, a treasury and an identity service, all real.
#[cfg(test)]
mod tests {
    use super::Identity;
    use crate::api::{Gateway, Limits};
    use crate::hub::Hub;
    use crate::limits::monotonic;
    use crate::testing::{self, eventually, MAKER, MAKER_TOKEN, MARKET, RESPONDER_TOKEN, RETAIL_TOKEN};
    use serde_json::{json, Value};
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::path::Path;
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

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

    /// Makes a key with `token` and returns the answer, which carries its credential.
    fn make_key(address: &str, token: &str, permissions: &[&str]) -> Value {
        let body = json!({"label": format!("test {}", permissions.join(" ")), "permissions": permissions});
        let (status, created) = request(address, "POST", "/v4/keys", Some(token), Some(&body));
        assert_eq!(status, 200, "{created}");
        created
    }

    fn credential(created: &Value) -> String {
        created["credential"].as_str().expect("a credential").to_string()
    }

    fn order(client_order_id: &str) -> Value {
        json!({"clientOrderId": client_order_id, "instrumentId": MARKET.to_string(), "action": "buy",
               "outcome": "yes", "price": {"tick": 4}, "quantity": 2})
    }

    fn forbidden(answer: &(u16, Value)) -> bool {
        answer.0 == 403 && answer.1["error"]["code"] == "FORBIDDEN"
    }

    type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

    fn connect(url: &str) -> Socket {
        let (socket, _) = tungstenite::connect(url).expect("a WebSocket");
        if let MaybeTlsStream::Plain(stream) = socket.get_ref() {
            stream.set_read_timeout(Some(Duration::from_secs(15))).expect("a read timeout");
        }
        socket
    }

    /// Sends `message` and returns the next answer to anything (reply, error or welcome), skipping streamed messages.
    fn ask(socket: &mut Socket, message: &Value) -> Value {
        socket.send(Message::text(message.to_string())).expect("sent");
        next_answer(socket)
    }

    fn next_answer(socket: &mut Socket) -> Value {
        loop {
            match socket.read().expect("a message before the timeout") {
                Message::Text(text) => {
                    let m: Value = serde_json::from_str(text.as_str()).expect("JSON");
                    if matches!(m["type"].as_str(), Some("reply" | "error" | "welcome")) {
                        return m;
                    }
                }
                Message::Close(_) => panic!("the gateway closed the session"),
                _ => {}
            }
        }
    }

    #[test]
    fn keys_are_made_listed_and_revoked_and_a_key_cannot_make_more_than_it_holds() {
        let venue = testing::venue("keys-manage");
        let a = &venue.address;
        let created = make_key(a, MAKER_TOKEN, &["read", "trade"]);
        let id = created["keyId"].as_str().expect("an ID").to_string();
        let secret = created["secret"].as_str().expect("a secret").to_string();
        assert_eq!(credential(&created), format!("{id}.{secret}"));
        assert_eq!((created["createdBy"].clone(), created["revokedAt"].clone()), (Value::Null, Value::Null));

        let (status, listed) = request(a, "GET", "/v4/keys", Some(MAKER_TOKEN), None);
        assert_eq!((status, listed["keys"][0]["keyId"].as_str()), (200, Some(id.as_str())), "{listed}");
        assert!(!listed.to_string().contains(&secret), "a list showed a secret: {listed}");
        let (_, theirs) = request(a, "GET", "/v4/keys", Some(RETAIL_TOKEN), None);
        assert_eq!(theirs["keys"], json!([]), "another account's keys were listed");
        let path = format!("/v4/keys/{id}");
        assert_eq!(request(a, "DELETE", &path, Some(RETAIL_TOKEN), None).0, 404);
        assert_eq!(request(a, "DELETE", "/v4/keys/not-a-key", Some(MAKER_TOKEN), None).0, 400);
        assert_eq!(request(a, "GET", "/v4/keys", Some(&credential(&created)), None).0, 403, "a key without keys");
        let no_read = json!({"label": "blind", "permissions": ["trade"]});
        assert_eq!(request(a, "POST", "/v4/keys", Some(MAKER_TOKEN), Some(&no_read)).0, 400);

        // A key that manages keys, and expires in an hour, makes keys within what it holds and no longer-lived.
        let hour = venue_identity::clock::rfc3339(venue_identity::clock::wall_ms() + 3_600_000);
        let body = json!({"label": "delegate", "permissions": ["read", "keys"], "expiresAt": hour});
        let (status, delegate) = request(a, "POST", "/v4/keys", Some(MAKER_TOKEN), Some(&body));
        assert_eq!(status, 200, "{delegate}");
        let delegate = credential(&delegate);
        let wider = json!({"label": "wider", "permissions": ["read", "trade"], "expiresAt": hour});
        let refused = request(a, "POST", "/v4/keys", Some(&delegate), Some(&wider));
        assert!(forbidden(&refused) && refused.1.to_string().contains("trade"), "{}", refused.1);
        let forever = json!({"label": "forever", "permissions": ["read"]});
        assert!(forbidden(&request(a, "POST", "/v4/keys", Some(&delegate), Some(&forever))));
        let (status, child) = request(
            a,
            "POST",
            "/v4/keys",
            Some(&delegate),
            Some(&json!({"label": "child", "permissions": ["read"], "expiresAt": hour})),
        );
        assert_eq!(status, 200, "{child}");
        assert_eq!(child["createdBy"].as_str(), delegate.split('.').next());
        let smuggled = json!({"label": "x", "permissions": ["read"], "createdBy": id});
        assert_eq!(request(a, "POST", "/v4/keys", Some(MAKER_TOKEN), Some(&smuggled)).0, 400);

        let (status, revoked) = request(a, "DELETE", &path, Some(MAKER_TOKEN), None);
        assert_eq!(status, 200, "{revoked}");
        assert!(revoked["revokedAt"].is_string(), "{revoked}");
        assert_eq!(request(a, "DELETE", &path, Some(MAKER_TOKEN), None).1["revokedAt"], revoked["revokedAt"]);
    }

    #[test]
    fn each_permission_allows_what_it_names_and_nothing_more() {
        let venue = testing::venue_with_treasury("keys-permissions");
        let a = &venue.address;
        let reader = credential(&make_key(a, MAKER_TOKEN, &["read"]));
        let trader = credential(&make_key(a, MAKER_TOKEN, &["read", "trade"]));
        let banker = credential(&make_key(a, MAKER_TOKEN, &["read", "transfers"]));
        let deposit =
            json!({"clientTransferId": "k-1", "kind": "deposit", "amountCents": 100, "method": "dummy_instant"});
        let legs = json!({"legs": [{"instrumentId": MARKET.to_string(), "outcome": "yes"},
                                   {"instrumentId": testing::market(3, 0).to_string(), "outcome": "no"}], "quantity": 10});

        // read: the account and its transfers, nothing that acts.
        assert_eq!(request(a, "GET", "/v4/account", Some(&reader), None).0, 200);
        assert_eq!(request(a, "GET", "/v4/transfers", Some(&reader), None).0, 200);
        for (method, path, body) in [
            ("POST", "/v4/orders", Some(order("r-1"))),
            ("DELETE", "/v4/orders", None),
            ("DELETE", "/v4/orders/1", None),
            ("POST", "/v4/rfqs", Some(legs.clone())),
            ("POST", "/v4/transfers", Some(deposit.clone())),
            ("GET", "/v4/keys", None),
        ] {
            let answer = request(a, method, path, Some(&reader), body.as_ref());
            assert!(forbidden(&answer), "{method} {path} with a read key: {} {}", answer.0, answer.1);
        }

        // trade: orders and requests for quote, and still no money.
        let (status, placed) = request(a, "POST", "/v4/orders", Some(&trader), Some(&order("t-1")));
        assert_eq!((status, placed["status"].as_str()), (200, Some("accepted")), "{placed}");
        assert_eq!(request(a, "POST", "/v4/rfqs", Some(&trader), Some(&legs)).0, 200);
        assert!(forbidden(&request(a, "POST", "/v4/transfers", Some(&trader), Some(&deposit))));

        // transfers: money, and no orders.
        let (status, moved) = request(a, "POST", "/v4/transfers", Some(&banker), Some(&deposit));
        assert_eq!((status, moved["state"].as_str()), (200, Some("settled")), "{moved}");
        assert!(forbidden(&request(a, "POST", "/v4/orders", Some(&banker), Some(&order("b-1")))));

        // A wrong secret is not a key at all.
        let (id, _) = trader.split_once('.').expect("a credential");
        let wrong = format!("{id}.{}", "0".repeat(64));
        assert_eq!(request(a, "GET", "/v4/account", Some(&wrong), None).0, 401);

        // The same on a private session: a read key logs in and is refused every op that acts.
        let mut session = connect(&venue.ws_private);
        let welcome = ask(&mut session, &json!({"op": "login", "id": "l", "token": reader}));
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        assert_eq!((welcome["keyId"].as_str(), &welcome["permissions"]), (reader.split('.').next(), &json!(["read"])));
        let mut placing = order("w-1");
        placing["op"] = json!("order");
        placing["id"] = json!("o");
        for op in [placing.clone(), json!({"op": "cancelAll", "id": "c"}), {
            let mut rfq = legs.clone();
            rfq["op"] = json!("requestQuote");
            rfq["id"] = json!("q");
            rfq
        }] {
            let refused = ask(&mut session, &op);
            assert_eq!(
                (refused["type"].as_str(), refused["error"]["code"].as_str()),
                (Some("error"), Some("FORBIDDEN"))
            );
            assert_eq!(refused["id"], op["id"]);
        }
        let mut trading = connect(&venue.ws_private);
        let welcome = ask(&mut trading, &json!({"op": "login", "id": "l", "token": trader}));
        assert_eq!(welcome["permissions"], json!(["read", "trade"]), "{welcome}");
        let placed = ask(&mut trading, &placing);
        assert_eq!(placed["result"]["status"], "accepted", "{placed}");
        // A dev token's session holds everything, and names no key.
        let mut dev = connect(&venue.ws_private);
        let welcome = ask(&mut dev, &json!({"op": "login", "id": "l", "token": MAKER_TOKEN}));
        assert_eq!(
            (welcome.get("keyId"), &welcome["permissions"]),
            (None, &json!(["read", "trade", "transfers", "keys", "respond"]))
        );
    }

    #[test]
    fn a_revoked_key_is_refused_within_seconds_and_its_session_closed() {
        let venue = testing::venue("keys-revoke");
        let a = &venue.address;
        let created = make_key(a, MAKER_TOKEN, &["read", "trade"]);
        let key = credential(&created);
        let mut session = connect(&venue.ws_private);
        let welcome = ask(&mut session, &json!({"op": "login", "id": "l", "token": key, "cancelOnDisconnect": true}));
        assert_eq!(welcome["type"], "welcome", "{welcome}");
        let mut placing = order("rv-1");
        placing["op"] = json!("order");
        placing["id"] = json!("o");
        let placed = ask(&mut session, &placing);
        let order_id: u64 = placed["result"]["orderId"].as_str().and_then(|o| o.parse().ok()).expect("an order ID");
        assert_eq!(request(a, "GET", "/v4/account", Some(&key), None).0, 200);

        let path = format!("/v4/keys/{}", created["keyId"].as_str().expect("an ID"));
        assert_eq!(request(a, "DELETE", &path, Some(MAKER_TOKEN), None).0, 200);
        let revoked = monotonic();
        // REST: refused within what the gateway remembers an answer for.
        loop {
            let (status, answer) = request(a, "GET", "/v4/account", Some(&key), None);
            if status == 401 {
                assert!(answer["error"]["message"].as_str().is_some_and(|m| m.contains("revoked")), "{answer}");
                break;
            }
            assert!(
                monotonic().duration_since(revoked) < super::REMEMBER + Duration::from_secs(1),
                "a revoked key still works on REST"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        // The session: told, and closed, within the contract's ten seconds, which cancels its order as it asked.
        let told = loop {
            match session.read() {
                Ok(Message::Text(text)) => {
                    let m: Value = serde_json::from_str(text.as_str()).expect("JSON");
                    if m["type"] == "error" {
                        break m;
                    }
                }
                Ok(Message::Close(_)) | Err(_) => panic!("the session ended without being told"),
                Ok(_) => {}
            }
        };
        assert_eq!(told["error"]["code"], "UNAUTHORIZED", "{told}");
        let closed = loop {
            match session.read() {
                Ok(Message::Close(_)) | Err(_) => break monotonic(),
                Ok(_) => {}
            }
        };
        assert!(closed.duration_since(revoked) < Duration::from_secs(10), "closed after {:?}", closed - revoked);
        eventually("the session's order is cancelled", || venue.gateway.hub().order(order_id).is_none());
        let mut again = connect(&venue.ws_private);
        let refused = ask(&mut again, &json!({"op": "login", "id": "l", "token": key}));
        assert_eq!(refused["error"]["code"], "UNAUTHORIZED", "{refused}");
    }

    /// `ask`, again while the parlay service is unavailable, which on a loaded machine it can be for a moment.
    fn ask_parlays(socket: &mut Socket, message: &Value) -> Value {
        for _ in 0..300 {
            let answer = ask(socket, message);
            if answer["error"]["code"] != "UNAVAILABLE" {
                return answer;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the parlay service stayed unavailable for {message}");
    }

    /// `request`, again while the parlay service is unavailable.
    fn request_parlays(address: &str, method: &str, path: &str, token: &str, body: Option<&Value>) -> (u16, Value) {
        for _ in 0..300 {
            let answer = request(address, method, path, Some(token), body);
            if answer.0 != 503 {
                return answer;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("the parlay service stayed unavailable for {method} {path}");
    }

    fn names_respond(answer: &Value) -> bool {
        answer["error"]["message"].as_str().is_some_and(|m| m.contains("respond"))
    }

    #[test]
    fn answering_requests_for_quote_needs_the_respond_permission_and_an_approved_account() {
        let venue = testing::venue("keys-respond");
        let a = &venue.address;
        // The synthetic maker's account is an approved responder; the retail customer's is not.
        let trader = make_key(a, RESPONDER_TOKEN, &["read", "trade"]);
        let responder = make_key(a, RESPONDER_TOKEN, &["read", "trade", "respond"]);
        let unapproved = credential(&make_key(a, RETAIL_TOKEN, &["read", "trade", "respond"]));
        let (trader_id, trader) = (trader["keyId"].as_str().expect("an ID").to_string(), credential(&trader));
        let responder = credential(&responder);
        let quote = json!({"orderId": "1"});

        // An approved account's key without respond: refused over REST and on a session, naming the permission.
        for (method, path, body) in [("GET", "/v4/rfqs", None), ("POST", "/v4/rfqs/abc/quotes", Some(&quote))] {
            let answer = request(a, method, path, Some(&trader), body);
            assert!(forbidden(&answer) && names_respond(&answer.1), "{method} {path}: {} {}", answer.0, answer.1);
        }
        let mut session = connect(&venue.ws_private);
        assert_eq!(ask(&mut session, &json!({"op": "login", "id": "l", "token": trader}))["type"], "welcome");
        for op in [
            json!({"op": "subscribeRfqs", "id": "s"}),
            json!({"op": "unsubscribeRfqs", "id": "u"}),
            json!({"op": "attachQuote", "id": "q", "requestId": "abc", "orderId": "1"}),
        ] {
            let refused = ask(&mut session, &op);
            assert_eq!(
                (&refused["id"], refused["error"]["code"].as_str()),
                (&op["id"], Some("FORBIDDEN")),
                "{refused}"
            );
            assert!(names_respond(&refused), "{refused}");
        }

        // With respond, the same account subscribes and reads what is open; a quote for no request is refused for
        // that, not for the key.
        let mut session = connect(&venue.ws_private);
        assert_eq!(ask(&mut session, &json!({"op": "login", "id": "l", "token": responder}))["type"], "welcome");
        let subscribed = ask_parlays(&mut session, &json!({"op": "subscribeRfqs", "id": "s"}));
        assert_eq!(subscribed["result"]["subscribed"], true, "{subscribed}");
        let attached = ask_parlays(
            &mut session,
            &json!({"op": "attachQuote", "id": "q", "requestId": "nosuchrequest", "orderId": "1"}),
        );
        assert!(attached["type"] == "error" && attached["error"]["code"] != "FORBIDDEN", "{attached}");
        let (status, open) = request_parlays(a, "GET", "/v4/rfqs", &responder, None);
        assert_eq!(status, 200, "{open}");

        // With respond, an account the parlay service has not approved is still refused.
        let (status, refused) = request_parlays(a, "GET", "/v4/rfqs", &unapproved, None);
        assert_eq!((status, refused["error"]["code"].as_str()), (403, Some("FORBIDDEN")), "{refused}");
        assert!(refused["error"]["message"].as_str().is_some_and(|m| m.contains("approved")), "{refused}");
        let mut session = connect(&venue.ws_private);
        assert_eq!(ask(&mut session, &json!({"op": "login", "id": "l", "token": unapproved}))["type"], "welcome");
        let refused = ask_parlays(&mut session, &json!({"op": "subscribeRfqs", "id": "s"}));
        assert_eq!(refused["error"]["code"], "FORBIDDEN", "{refused}");
        assert!(refused["error"]["message"].as_str().is_some_and(|m| m.contains("approved")), "{refused}");

        // The audit log names the key a refused subscription came from.
        eventually("the refused subscription is recorded with its key", || {
            audit_text(&venue.audit_dir).lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).any(|r| {
                r["kind"] == "ws-frame"
                    && r["dir"] == "in"
                    && r["key"] == trader_id.as_str()
                    && r["msg"].as_str().is_some_and(|m| m.contains("subscribeRfqs"))
            })
        });
    }

    /// Every record in the directory's files.
    fn audit_text(dir: &Path) -> String {
        let mut files: Vec<_> =
            std::fs::read_dir(dir).expect("the audit directory").flatten().map(|e| e.path()).collect();
        files.sort();
        files.iter().map(|f| std::fs::read_to_string(f).unwrap_or_default()).collect()
    }

    #[test]
    fn audit_records_name_the_key_and_never_its_secret() {
        let venue = testing::venue("keys-audit");
        let a = &venue.address;
        let created = make_key(a, MAKER_TOKEN, &["read", "trade"]);
        let (id, secret) = (created["keyId"].as_str().expect("an ID"), created["secret"].as_str().expect("a secret"));
        let key = credential(&created);
        assert_eq!(request(a, "POST", "/v4/orders", Some(&key), Some(&order("au-1"))).0, 200);
        let deposit = json!({"clientTransferId": "au", "kind": "deposit", "amountCents": 1, "method": "dummy_instant"});
        assert_eq!(request(a, "POST", "/v4/transfers", Some(&key), Some(&deposit)).0, 403);
        let records =
            |text: &str| -> Vec<Value> { text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect() };
        eventually("the refusal is recorded with the key", || {
            records(&audit_text(&venue.audit_dir))
                .iter()
                .any(|r| r["kind"] == "http-response" && r["status"] == 403 && r["key"] == id && r["account"] == "1")
        });
        let text = audit_text(&venue.audit_dir);
        assert!(
            records(&text).iter().any(|r| r["kind"] == "http-request"
                && r["key"] == id
                && r["body"].as_str().is_some_and(|b| b.contains("au-1"))),
            "the order is recorded with the key"
        );
        assert!(!text.contains(secret), "a secret was written to the audit log");
        assert!(text.contains(r#"\"secret\":\"***\""#), "the answer that made the key is recorded, masked");
    }

    #[test]
    fn dev_tokens_can_be_refused_and_keys_need_an_identity_service_that_answers() {
        let limits = Limits { per_second: 100, burst: 100, logons_per_minute: 100 };
        let catalogue = || {
            venue_link::catalogue::Catalogue::load(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../deploy/charts/venue-mini/files/markets.json"
            ))
            .expect("the catalogue")
        };
        // Never connected to: nothing here sends an order.
        let ingress = || venue_link::submit::Submit::spawn("127.0.0.1:17999");
        let key = format!("ak_0123456789abcdef.{}", "0".repeat(64));
        let open = Gateway::private(Hub::new(catalogue()), ingress(), limits);
        assert_eq!(open.authenticate(MAKER_TOKEN).map(|c| c.account).ok(), Some(MAKER));
        assert_eq!(open.authenticate(&key).map(|_| ()).expect_err("no identity service").status, 503);
        let closed = Gateway::private(Hub::new(catalogue()), ingress(), limits).with_identity(None, false);
        assert_eq!(closed.authenticate(MAKER_TOKEN).map(|_| ()).expect_err("dev tokens refused").status, 401);
        let metrics = crate::metrics::Metrics::unregistered().half(crate::api::Half::Private);
        let away = Identity::new(Some("127.0.0.1:17998".into()), true, metrics);
        assert_eq!(away.verify_credential(&key).map(|_| ()).expect_err("nobody answers").status, 503);
    }
}
