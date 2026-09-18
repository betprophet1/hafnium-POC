//! The key store: every key of every account, kept in the identity service's records (`records.rs`: PostgreSQL, or
//! memory for tests) and held in memory for verification.
//!
//! Each change is written to the records before anyone is told it happened: a key made (everything about it but its
//! secret, which is only ever hashed), a key revoked, or a key's last use (at most once a minute per key, decision G-8).
//! A key's terms never change once made, so its row is also its history.
//!
//! One lock serves every call. Verifications are a few a second per key in use, since gateways remember the answer
//! (decision G-7), and a create or revoke is a person's action; neither is a market's traffic.

use crate::clock;
use crate::key::{self, Permissions};
use crate::member;
use crate::records::{KeyRecords, KeyRow, MemberRow, SessionRow};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Keys an account may hold that are neither revoked nor expired (decision G-6).
pub const MAX_ACTIVE: usize = 20;
/// How often at most a key's last use is written down.
pub const USED_EVERY_MS: u64 = 60_000;
const MAX_LABEL: usize = 64;

/// A call the store refused, in interface 1's `Error` terms.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn bad(message: impl Into<String>) -> Self {
        Self { status: 400, code: "BAD_REQUEST", message: message.into() }
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self { status: 401, code: "UNAUTHORIZED", message: message.into() }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self { status: 404, code: "NOT_FOUND", message: message.into() }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self { status: 503, code: "UNAVAILABLE", message: message.into() }
    }

    pub fn body(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

struct Key {
    id: String,
    account: u64,
    label: String,
    permissions: Permissions,
    hash: [u8; 32],
    created_at: u64,
    expires_at: Option<u64>,
    revoked_at: Option<u64>,
    created_by: Option<String>,
    last_used: Option<u64>,
    /// The last use written to the records.
    written_use: Option<u64>,
}

impl Key {
    /// Interface 1's `Key`.
    fn json(&self) -> Value {
        let time = |t: Option<u64>| t.map_or(Value::Null, |ms| json!(clock::rfc3339(ms)));
        json!({
            "keyId": self.id, "label": self.label, "permissions": self.permissions.names(),
            "createdAt": clock::rfc3339(self.created_at), "expiresAt": time(self.expires_at),
            "revokedAt": time(self.revoked_at), "lastUsedAt": time(self.last_used), "createdBy": self.created_by
        })
    }

    fn active(&self, now: u64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|t| t > now)
    }

    fn row(&self) -> KeyRow {
        KeyRow {
            id: self.id.clone(),
            account: self.account,
            label: self.label.clone(),
            permissions: self.permissions.names().iter().map(|n| (*n).to_string()).collect(),
            hash: self.hash,
            created_at: self.created_at,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            created_by: self.created_by.clone(),
            last_used: self.last_used,
        }
    }

    fn from_row(row: KeyRow) -> Result<Self, String> {
        Ok(Self {
            permissions: Permissions::from_names(row.permissions.iter().map(String::as_str))?,
            id: row.id,
            account: row.account,
            label: row.label,
            hash: row.hash,
            created_at: row.created_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            created_by: row.created_by,
            last_used: row.last_used,
            written_use: row.last_used,
        })
    }
}

/// A key a credential was verified as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The API key's ID, or the session's: both are IDs a log can name, and their prefixes tell them apart.
    pub key_id: String,
    pub account: u64,
    pub permissions: Permissions,
    pub expires_at: Option<u64>,
}

impl Verified {
    /// The answer to `POST /v1/verify`.
    pub fn json(&self) -> Value {
        let session = self.key_id.starts_with(member::SESSION_PREFIX);
        json!({
            "keyId": (!session).then(|| self.key_id.clone()), "sessionId": session.then(|| self.key_id.clone()),
            "accountId": self.account.to_string(), "permissions": self.permissions.names(),
            "expiresAt": self.expires_at.map(clock::rfc3339)
        })
    }
}

#[derive(Default)]
struct State {
    keys: BTreeMap<String, Key>,
    /// Sessions that have not ended, by their ID: read at start and kept current, as the keys are, so that verifying
    /// one costs a lock and a hash.
    sessions: BTreeMap<String, SessionRow>,
    /// The accounts this environment has: the catalogue's, and one for each member who has signed up.
    accounts: BTreeSet<u64>,
    /// Each account's key IDs, oldest first.
    by_account: BTreeMap<u64, Vec<String>>,
    /// A write failed, so the records may hold something memory does not (a revocation whose acknowledgement was
    /// lost): memory is read again from them first.
    stale: bool,
}

/// Account IDs for members who sign up start here, above anything a catalogue seeds, so an account a person made is
/// told from an environment's own at a glance.
const MEMBER_ACCOUNTS_FROM: u64 = 100_000;

/// What a member may do with their own account: everything but answering requests for quote, which needs an approval
/// the parlay service gives and is a market maker's business rather than a member's.
const MEMBER_PERMISSIONS: Permissions =
    Permissions::READ.with(Permissions::TRADE).with(Permissions::TRANSFERS).with(Permissions::KEYS);

/// An Argon2id hash of a password nobody has, verified against when no member holds the email that was given, so that
/// a refusal takes as long either way and says nothing about which emails this venue knows.
const NO_SUCH_MEMBER_PHC: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHRzb21lc2FsdA$7a9UvJ5BPrGDQJXsfoRBqH4LKgXUcrFMnwjWBxbGGfM";

fn records_refusal(e: venue_store::Error) -> Refusal {
    if e.is_unavailable() {
        Refusal::unavailable(e.to_string())
    } else {
        Refusal::bad(e.to_string())
    }
}

pub struct Store {
    state: Mutex<State>,
    records: Arc<dyn KeyRecords>,
}

fn locked(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn insert(state: &mut State, made: Key) {
    state.by_account.entry(made.account).or_default().push(made.id.clone());
    state.keys.insert(made.id.clone(), made);
}

fn read(records: &dyn KeyRecords, accounts: BTreeSet<u64>, now: u64) -> Result<State, String> {
    let mut state = State { accounts, ..State::default() };
    for row in records.load_keys()? {
        let key = Key::from_row(row)?;
        insert(&mut state, key);
    }
    for session in records.load_sessions(now)? {
        state.accounts.insert(session.account);
        state.sessions.insert(session.id.clone(), session);
    }
    Ok(state)
}

impl Store {
    /// The store whose keys are in `records`, read into memory.
    pub fn open(records: Arc<dyn KeyRecords>, accounts: BTreeSet<u64>) -> Result<Self, String> {
        let state = read(records.as_ref(), accounts, clock::wall_ms())?;
        Ok(Self { state: Mutex::new(state), records })
    }

    /// Reads the keys again if a write failed since they were last read. Verification carries on from memory if the
    /// records cannot be read; changes are refused.
    fn fresh(&self, state: &mut State) -> Result<(), Refusal> {
        if !state.stale {
            return Ok(());
        }
        match read(self.records.as_ref(), state.accounts.clone(), clock::wall_ms()) {
            Ok(mut again) => {
                again.stale = false;
                *state = again;
                eprintln!("identity: keys read again from the records");
                Ok(())
            }
            Err(e) => {
                eprintln!("identity: reading the records: {e}");
                Err(Refusal::unavailable("the key store could not be read; nothing was changed"))
            }
        }
    }

    /// Whether the records answer, for health checks.
    pub fn health(&self) -> Result<(), String> {
        self.records.ping().map_err(String::from)
    }

    /// Makes a key for `account` from a request body (interface 1's `KeyRequest`, and `createdBy` when a key asked),
    /// and answers with it, its secret and its credential. The gateway has already checked the caller may; this
    /// checks the key is well formed.
    pub fn create(&self, account: u64, body: &Value, now: u64) -> Result<Value, Refusal> {
        let fields = body.as_object().ok_or_else(|| Refusal::bad("the body must be a JSON object"))?;
        if let Some(unknown) =
            fields.keys().find(|k| !["label", "permissions", "expiresAt", "createdBy"].contains(&k.as_str()))
        {
            return Err(Refusal::bad(format!("unknown field {unknown}")));
        }
        if !locked(&self.state).accounts.contains(&account) {
            return Err(Refusal::not_found("no such account"));
        }
        let label = fields
            .get("label")
            .and_then(Value::as_str)
            .filter(|l| (1..=MAX_LABEL).contains(&l.len()) && l.bytes().all(|b| (0x20..=0x7e).contains(&b)))
            .ok_or_else(|| Refusal::bad(format!("label must be 1 to {MAX_LABEL} printable ASCII characters")))?;
        let names = fields
            .get("permissions")
            .and_then(Value::as_array)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| Refusal::bad("permissions must be a list of one or more permissions"))?;
        let names: Vec<&str> = names
            .iter()
            .map(|n| n.as_str().ok_or_else(|| Refusal::bad("a permission must be a string")))
            .collect::<Result<_, _>>()?;
        let permissions = Permissions::from_names(names).map_err(Refusal::bad)?;
        if !permissions.contains(Permissions::READ) {
            return Err(Refusal::bad("permissions must include read: every key can see its own account"));
        }
        let expires_at = match fields.get("expiresAt") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(clock::parse_rfc3339)
                    .filter(|t| *t > now)
                    .ok_or_else(|| Refusal::bad("expiresAt must be a UTC RFC 3339 time in the future"))?,
            ),
        };
        let created_by = match fields.get("createdBy") {
            None | Some(Value::Null) => None,
            Some(v) => Some(
                v.as_str()
                    .filter(|id| key::is_id(id))
                    .ok_or_else(|| Refusal::bad("createdBy must be a key ID"))?
                    .to_string(),
            ),
        };
        let random = |e: std::io::Error| {
            eprintln!("identity: the random source: {e}");
            Refusal::unavailable("no key could be made; nothing was changed")
        };
        let secret = key::new_secret().map_err(random)?;
        let mut state = locked(&self.state);
        self.fresh(&mut state)?;
        let active =
            state.by_account.get(&account).into_iter().flatten().filter(|id| state.keys[*id].active(now)).count();
        if active >= MAX_ACTIVE {
            return Err(Refusal::bad(format!(
                "an account may hold at most {MAX_ACTIVE} keys that are neither revoked nor expired"
            )));
        }
        let mut id = key::new_id().map_err(random)?;
        while state.keys.contains_key(&id) {
            id = key::new_id().map_err(random)?;
        }
        let made = Key {
            hash: key::hash(&id, &secret),
            id,
            account,
            label: label.to_string(),
            permissions,
            created_at: now,
            expires_at,
            revoked_at: None,
            created_by,
            last_used: None,
            written_use: None,
        };
        if let Err(e) = self.records.create(&made.row()) {
            state.stale = true;
            eprintln!("identity: writing key {}: {e}", made.id);
            return Err(Refusal::unavailable("the key store could not be written; nothing was changed"));
        }
        let mut answer = made.json();
        answer["secret"] = json!(secret);
        answer["credential"] = json!(format!("{}.{secret}", made.id));
        insert(&mut state, made);
        Ok(answer)
    }

    /// Every key of `account`, newest first.
    pub fn list(&self, account: u64) -> Value {
        let mut state = locked(&self.state);
        let _ = self.fresh(&mut state);
        let keys: Vec<Value> =
            state.by_account.get(&account).into_iter().flatten().rev().map(|id| state.keys[id].json()).collect();
        json!({"keys": keys})
    }

    /// Revokes one of `account`'s keys. Revoking a key already revoked answers it as it is.
    pub fn revoke(&self, account: u64, key_id: &str, now: u64) -> Result<Value, Refusal> {
        let mut state = locked(&self.state);
        self.fresh(&mut state)?;
        let found = state.keys.get(key_id).filter(|k| k.account == account);
        let found = found.ok_or_else(|| Refusal::not_found("no such key of this account"))?;
        if found.revoked_at.is_some() {
            return Ok(found.json());
        }
        let revoked_at = match self.records.revoke(key_id, now) {
            Ok(at) => at,
            Err(e) => {
                state.stale = true;
                eprintln!("identity: revoking key {key_id}: {e}");
                return Err(Refusal::unavailable("the key store could not be written; nothing was changed"));
            }
        };
        let revoked = state.keys.get_mut(key_id).ok_or_else(|| Refusal::not_found("no such key of this account"))?;
        revoked.revoked_at = Some(revoked_at);
        Ok(revoked.json())
    }

    /// Which key a credential is, if it is a valid one now, noting its use. Every refusal is `UNAUTHORIZED`; only a
    /// caller that knew the secret learns that the key was revoked or has expired (decision G-11).
    /// A member signing up: an account of their own, their email and a hash of their password, and a session so the
    /// app they signed up in does not have to log in again at once (interface 1 README, members and sessions).
    pub fn sign_up(&self, body: &Value, now: u64) -> Result<Value, Refusal> {
        let fields = body.as_object().ok_or_else(|| Refusal::bad("the body must be a JSON object"))?;
        if let Some(unknown) = fields.keys().find(|k| !["email", "password", "displayName"].contains(&k.as_str())) {
            return Err(Refusal::bad(format!("unknown field {unknown}")));
        }
        let email = member::normalise_email(fields.get("email").and_then(Value::as_str).unwrap_or_default());
        if !member::looks_like_email(&email) {
            return Err(Refusal::bad("email must be an email address"));
        }
        let password = fields.get("password").and_then(Value::as_str).unwrap_or_default();
        if !(member::MIN_PASSWORD..=member::MAX_PASSWORD).contains(&password.chars().count()) {
            return Err(Refusal::bad(format!(
                "password must be {} to {} characters",
                member::MIN_PASSWORD,
                member::MAX_PASSWORD
            )));
        }
        let display_name = match fields.get("displayName").and_then(Value::as_str) {
            Some(name) if (1..=64).contains(&name.chars().count()) => name.to_string(),
            Some(_) => return Err(Refusal::bad("displayName must be 1 to 64 characters")),
            None => email.split('@').next().unwrap_or("member").to_string(),
        };
        let account = self.records.next_account(MEMBER_ACCOUNTS_FROM).map_err(records_refusal)?;
        let row = MemberRow {
            id: member::new_member_id().map_err(|e| Refusal::unavailable(format!("an ID: {e}")))?,
            account,
            email,
            password_phc: member::hash_password(password).map_err(Refusal::unavailable)?,
            display_name: display_name.clone(),
            created_at: now,
            disabled_at: None,
        };
        self.records.create_member(&row, &display_name, now).map_err(records_refusal)?;
        locked(&self.state).accounts.insert(account);
        let session = self.start_session(&row, now)?;
        Ok(json!({
            "memberId": row.id, "accountId": account.to_string(), "email": row.email,
            "displayName": row.display_name, "session": session
        }))
    }

    /// Logging in: the email and password of a member, answered with a session for their app to hold.
    pub fn log_in(&self, body: &Value, now: u64) -> Result<Value, Refusal> {
        let fields = body.as_object().ok_or_else(|| Refusal::bad("the body must be a JSON object"))?;
        let email = member::normalise_email(fields.get("email").and_then(Value::as_str).unwrap_or_default());
        let password = fields.get("password").and_then(Value::as_str).unwrap_or_default();
        let wrong = || Refusal::unauthorized("the email or password is wrong");
        let found = self.records.member_by_email(&email).map_err(records_refusal)?;
        let Some(row) = found else {
            // A password is hashed either way, so that how long a refusal took says nothing about whether the email
            // is one this venue knows.
            let _ = member::password_matches(NO_SUCH_MEMBER_PHC, password);
            return Err(wrong());
        };
        if !member::password_matches(&row.password_phc, password) {
            return Err(wrong());
        }
        if row.disabled_at.is_some() {
            return Err(Refusal::unauthorized("this member cannot log in"));
        }
        let session = self.start_session(&row, now)?;
        Ok(json!({
            "memberId": row.id, "accountId": row.account.to_string(), "email": row.email,
            "displayName": row.display_name, "session": session
        }))
    }

    /// A session, written down and then kept in memory, and answered with its credential — the only time the secret
    /// exists anywhere but the app that asked for it.
    fn start_session(&self, member: &MemberRow, now: u64) -> Result<Value, Refusal> {
        let id = member::new_session_id().map_err(|e| Refusal::unavailable(format!("an ID: {e}")))?;
        let secret = member::new_secret().map_err(|e| Refusal::unavailable(format!("a secret: {e}")))?;
        let row = SessionRow {
            id: id.clone(),
            member: member.id.clone(),
            account: member.account,
            hash: member::hash(&id, &secret),
            created_at: now,
            expires_at: now.saturating_add(member::SESSION_SECONDS * 1000),
            ended_at: None,
            last_used: None,
        };
        self.records.create_session(&row).map_err(records_refusal)?;
        let expires = row.expires_at;
        locked(&self.state).sessions.insert(id.clone(), row);
        Ok(json!({
            "sessionId": id, "credential": format!("{id}.{secret}"), "expiresAt": clock::rfc3339(expires)
        }))
    }

    /// Logging out. A session that has already ended answers as it did, so an app repeating the call is not an error.
    pub fn log_out(&self, session_id: &str, now: u64) -> Result<Value, Refusal> {
        let ended = self.records.end_session(session_id, now).map_err(|e| match e {
            e if e.is_unavailable() => Refusal::unavailable(e.to_string()),
            _ => Refusal::not_found("no such session"),
        })?;
        let mut state = locked(&self.state);
        state.sessions.remove(session_id);
        Ok(json!({"sessionId": session_id, "endedAt": clock::rfc3339(ended)}))
    }

    /// Verifying a session: the same shape as a key's, and the same answer to the gateway, with the permissions a
    /// member holds over their own account.
    fn verify_session(&self, credential: &str, now: u64) -> Result<Verified, Refusal> {
        let invalid = || Refusal::unauthorized("the session is not valid");
        let (id, secret) = member::split(credential).ok_or_else(invalid)?;
        let presented = member::hash(id, secret);
        let mut state = locked(&self.state);
        let _ = self.fresh(&mut state);
        let found = state.sessions.get(id).ok_or_else(invalid)?;
        if !key::same(&found.hash, &presented) {
            return Err(invalid());
        }
        if found.ended_at.is_some() {
            return Err(Refusal::unauthorized("the session has ended"));
        }
        if found.expires_at <= now {
            return Err(Refusal::unauthorized("the session has expired"));
        }
        let verified = Verified {
            key_id: found.id.clone(),
            account: found.account,
            permissions: MEMBER_PERMISSIONS,
            expires_at: Some(found.expires_at),
        };
        let due = found.last_used.is_none_or(|t| now >= t.saturating_add(USED_EVERY_MS));
        if due && self.records.session_used(id, now).is_ok() {
            if let Some(s) = state.sessions.get_mut(id) {
                s.last_used = Some(now);
            }
        }
        Ok(verified)
    }

    pub fn verify(&self, credential: &str, now: u64) -> Result<Verified, Refusal> {
        if member::looks_like_session(credential) {
            return self.verify_session(credential, now);
        }
        let invalid = || Refusal::unauthorized("the API key is not valid");
        let (id, secret) = key::split(credential).ok_or_else(invalid)?;
        let presented = key::hash(id, secret);
        let mut state = locked(&self.state);
        // A revocation the records hold and memory might not: read them again if a write failed. If they cannot be
        // read, memory answers, as it would have before the failed write.
        let _ = self.fresh(&mut state);
        let found = state.keys.get(id).ok_or_else(invalid)?;
        if !key::same(&found.hash, &presented) {
            return Err(invalid());
        }
        if found.revoked_at.is_some() {
            return Err(Refusal::unauthorized("the API key has been revoked"));
        }
        if found.expires_at.is_some_and(|t| t <= now) {
            return Err(Refusal::unauthorized("the API key has expired"));
        }
        let verified = Verified {
            key_id: found.id.clone(),
            account: found.account,
            permissions: found.permissions,
            expires_at: found.expires_at,
        };
        let due = found.written_use.is_none_or(|t| now >= t.saturating_add(USED_EVERY_MS));
        // A use that cannot be written down is not a reason to refuse the key: it is only ever a minute out.
        if due && self.records.used(id, now).is_ok() {
            if let Some(k) = state.keys.get_mut(id) {
                k.written_use = Some(now);
            }
        }
        if let Some(k) = state.keys.get_mut(id) {
            k.last_used = Some(k.last_used.map_or(now, |t| t.max(now)));
        }
        Ok(verified)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::records::Memory;
    use serde_json::Map;

    /// A request body as the gateway passes it on, for tests.
    fn request(label: &str, permissions: &[&str], expires_at: Option<u64>) -> Value {
        let mut body = Map::new();
        body.insert("label".into(), json!(label));
        body.insert("permissions".into(), json!(permissions));
        if let Some(t) = expires_at {
            body.insert("expiresAt".into(), json!(clock::rfc3339(t)));
        }
        Value::Object(body)
    }

    /// 2026-09-17T00:00:00Z.
    const NOW: u64 = 1_789_603_200_000;

    fn open(records: &Arc<dyn KeyRecords>) -> Store {
        Store::open(Arc::clone(records), BTreeSet::from([1, 2])).expect("a store")
    }

    fn memory() -> Arc<dyn KeyRecords> {
        Arc::new(Memory::default())
    }

    fn credential(created: &Value) -> String {
        created["credential"].as_str().expect("a credential").to_string()
    }

    #[test]
    fn a_created_key_verifies_only_with_its_own_secret() {
        let store = open(&memory());
        let created = store.create(1, &request("bot", &["read", "trade"], None), NOW).expect("created");
        let id = created["keyId"].as_str().expect("an ID");
        assert_eq!(created["permissions"], json!(["read", "trade"]));
        assert_eq!((created["expiresAt"].clone(), created["createdBy"].clone()), (Value::Null, Value::Null));
        let secret = created["secret"].as_str().expect("a secret");
        assert_eq!(credential(&created), format!("{id}.{secret}"));

        let verified = store.verify(&credential(&created), NOW + 1).expect("valid");
        assert_eq!(
            verified,
            Verified {
                key_id: id.into(),
                account: 1,
                permissions: Permissions::READ.with(Permissions::TRADE),
                expires_at: None
            }
        );
        let wrong = format!("{id}.{}", "0".repeat(64));
        assert_eq!(store.verify(&wrong, NOW).expect_err("a wrong secret").message, "the API key is not valid");
        let unknown = format!("ak_{}.{secret}", "0".repeat(16));
        assert_eq!(store.verify(&unknown, NOW).expect_err("no such key").status, 401);
        assert_eq!(store.verify("dev-market-maker", NOW).expect_err("not a key").status, 401);

        // The list never shows a secret, or anything it could be recovered from.
        let listed = store.list(1).to_string();
        assert!(!listed.contains(secret) && !listed.contains("hash"), "{listed}");
        assert_eq!(store.list(2)["keys"], json!([]));
    }

    #[test]
    fn a_malformed_request_is_refused_and_makes_nothing() {
        let store = open(&memory());
        for (why, account, body) in [
            ("no read", 1, request("x", &["trade"], None)),
            ("an unknown permission", 1, request("x", &["read", "admin"], None)),
            ("no permissions", 1, request("x", &[], None)),
            ("an empty label", 1, request("", &["read"], None)),
            ("a long label", 1, request(&"x".repeat(65), &["read"], None)),
            ("an expiry in the past", 1, request("x", &["read"], Some(NOW - 1))),
            ("an unknown field", 1, json!({"label": "x", "permissions": ["read"], "secret": "mine"})),
        ] {
            assert_eq!(store.create(account, &body, NOW).expect_err(why).status, 400, "{why}");
        }
        assert_eq!(store.create(3, &request("x", &["read"], None), NOW).expect_err("no such account").status, 404);
        assert_eq!(store.list(1)["keys"], json!([]));
    }

    #[test]
    fn an_expired_key_is_refused_and_says_so_to_its_holder() {
        let store = open(&memory());
        let created = store.create(1, &request("short", &["read"], Some(NOW + 1000)), NOW).expect("created");
        assert!(store.verify(&credential(&created), NOW + 999).is_ok());
        let refused = store.verify(&credential(&created), NOW + 1000).expect_err("expired");
        assert_eq!((refused.status, refused.message.as_str()), (401, "the API key has expired"));
    }

    #[test]
    fn a_revoked_key_is_refused_for_good_and_only_its_account_can_revoke_it() {
        let store = open(&memory());
        let created = store.create(1, &request("gone", &["read", "transfers"], None), NOW).expect("created");
        let id = created["keyId"].as_str().expect("an ID");
        assert_eq!(store.revoke(2, id, NOW).expect_err("another account's").status, 404);
        let revoked = store.revoke(1, id, NOW + 5).expect("revoked");
        assert_eq!(revoked["revokedAt"], clock::rfc3339(NOW + 5));
        assert_eq!(store.revoke(1, id, NOW + 9).expect("again")["revokedAt"], clock::rfc3339(NOW + 5));
        assert_eq!(
            store.verify(&credential(&created), NOW + 6).expect_err("revoked").message,
            "the API key has been revoked"
        );
    }

    #[test]
    fn an_account_holds_a_bounded_number_of_active_keys() {
        let store = open(&memory());
        let made: Vec<Value> = (0..MAX_ACTIVE)
            .map(|i| store.create(2, &request(&format!("k{i}"), &["read"], None), NOW).expect("created"))
            .collect();
        assert_eq!(store.create(2, &request("one more", &["read"], None), NOW).expect_err("too many").status, 400);
        store.revoke(2, made[0]["keyId"].as_str().expect("an ID"), NOW).expect("revoked");
        store.create(2, &request("in its place", &["read"], None), NOW).expect("room again");
        let listed = store.list(2);
        assert_eq!(listed["keys"][0]["label"], "in its place", "newest first");
        assert_eq!(listed["keys"].as_array().map(Vec::len), Some(MAX_ACTIVE + 1));
    }

    /// Keys, revocations and uses outlive the store; a secret is never written anywhere.
    pub(crate) fn keys_revocations_and_uses_survive_a_restart(records: &Arc<dyn KeyRecords>) {
        let (kept, revoked, used) = {
            let store = open(records);
            let kept =
                store.create(1, &request("kept", &["read", "trade"], Some(NOW + 3_600_000)), NOW).expect("created");
            let mut child = request("child", &["read"], None);
            child["createdBy"] = kept["keyId"].clone();
            let revoked = store.create(1, &child, NOW).expect("created");
            store.revoke(1, revoked["keyId"].as_str().expect("an ID"), NOW + 10).expect("revoked");
            store.verify(&credential(&kept), NOW + 20).expect("used");
            // Within the minute: remembered, not written.
            store.verify(&credential(&kept), NOW + 30).expect("used again");
            (kept, revoked, store.list(1))
        };
        let store = open(records);
        let replayed = store.list(1);
        assert_eq!(replayed["keys"][0]["createdBy"], kept["keyId"]);
        assert_eq!(replayed["keys"][1]["lastUsedAt"], clock::rfc3339(NOW + 20), "the last use written survives");
        assert_eq!(used["keys"][0], replayed["keys"][0]);
        assert!(store.verify(&credential(&kept), NOW + 40).is_ok(), "a kept key verifies after a restart");
        assert_eq!(store.verify(&credential(&revoked), NOW + 40).expect_err("still revoked").status, 401);
        let rows = records.load_keys().expect("readable");
        let secret = kept["secret"].as_str().expect("a secret");
        assert!(rows.iter().all(|r| !format!("{r:?}").contains(secret)), "a secret was written");
    }

    #[test]
    fn keys_revocations_and_uses_survive_a_restart_in_memory() {
        keys_revocations_and_uses_survive_a_restart(&memory());
    }

    #[test]
    fn records_that_cannot_be_written_refuse_changes_and_verification_carries_on() {
        let memory = Memory::default();
        let records: Arc<dyn KeyRecords> = Arc::new(memory.clone());
        let store = open(&records);
        let created = store.create(1, &request("bot", &["read"], None), NOW).expect("created");
        memory.set_down(true);
        assert_eq!(store.create(1, &request("more", &["read"], None), NOW).expect_err("down").status, 503);
        let id = created["keyId"].as_str().expect("an ID");
        assert_eq!(store.revoke(1, id, NOW).expect_err("down").status, 503);
        assert!(store.verify(&credential(&created), NOW + 1).is_ok(), "verified from memory");
        assert!(store.health().is_err());
        memory.set_down(false);
        store.revoke(1, id, NOW + 2).expect("revoked once the records are back");
        assert!(store.verify(&credential(&created), NOW + 3).is_err());
    }

    pub(crate) fn many_threads_create_verify_and_revoke_at_once(records: &Arc<dyn KeyRecords>) {
        let store = Arc::new(open(records));
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    let account = 1 + t % 2;
                    let mut made = Vec::new();
                    for i in 0..5 {
                        let created = store
                            .create(account, &request(&format!("t{t}-{i}"), &["read"], None), NOW)
                            .expect("created");
                        for n in 0..20 {
                            store.verify(&credential(&created), NOW + n * USED_EVERY_MS).expect("valid");
                        }
                        made.push(created);
                    }
                    for created in &made[..2] {
                        store.revoke(account, created["keyId"].as_str().expect("an ID"), NOW).expect("revoked");
                        assert!(store.verify(&credential(created), NOW).is_err());
                    }
                    made
                })
            })
            .collect();
        let made: Vec<Value> = threads.into_iter().flat_map(|t| t.join().expect("a thread")).collect();
        drop(store);
        let store = open(records);
        let valid = made.iter().filter(|c| store.verify(&credential(c), NOW).is_ok()).count();
        assert_eq!((made.len(), valid), (40, 24));
    }

    #[test]
    fn many_threads_create_verify_and_revoke_at_once_in_memory() {
        many_threads_create_verify_and_revoke_at_once(&memory());
    }
}
