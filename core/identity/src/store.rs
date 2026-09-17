//! The key store: every key of every account, kept in an append-only file of JSON lines and replayed at start.
//!
//! The first line names the format. Each later line is one thing that happened to a key, written before anyone is
//! told it happened: `create` (everything about the key but its secret, which is only ever hashed), `revoke`, or
//! `used` (a key's last use, at most once a minute per key, decision G-8). Nothing is ever rewritten, so the file is
//! also the history of every key. A stop mid-write leaves at most a last line cut short, which the next start drops.
//!
//! One lock serves every call. Verifications are a few a second per key in use, since gateways remember the answer
//! (decision G-7), and a create or revoke is a person's action; neither is a market's traffic.

use crate::clock;
use crate::key::{self, Permissions};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

const FORMAT: &str = "venue-identity/1";
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

    fn unavailable(message: impl Into<String>) -> Self {
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
    /// The last use written to the file.
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
}

/// A key a credential was verified as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub key_id: String,
    pub account: u64,
    pub permissions: Permissions,
    pub expires_at: Option<u64>,
}

impl Verified {
    /// The answer to `POST /v1/verify`.
    pub fn json(&self) -> Value {
        json!({
            "keyId": self.key_id, "accountId": self.account.to_string(), "permissions": self.permissions.names(),
            "expiresAt": self.expires_at.map(clock::rfc3339)
        })
    }
}

struct State {
    keys: BTreeMap<String, Key>,
    /// Each account's key IDs, oldest first.
    by_account: BTreeMap<u64, Vec<String>>,
    file: File,
}

pub struct Store {
    state: Mutex<State>,
    fsync: bool,
    /// The accounts keys may be made for: the environment's catalogue.
    accounts: BTreeSet<u64>,
}

fn locked(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Store {
    /// Opens the store at `path`, creating it if there is none, and replays it. A line that is not a whole record,
    /// other than a last one cut short, is a store this service did not write, and it refuses to start on it.
    pub fn open(path: &Path, fsync: bool, accounts: BTreeSet<u64>) -> Result<Self, String> {
        let shown = path.display();
        let mut text = Vec::new();
        if path.exists() {
            File::open(path).and_then(|mut f| f.read_to_end(&mut text)).map_err(|e| format!("{shown}: {e}"))?;
        }
        // Everything after the last newline is a record a stop cut short: it was never acted on, so it goes.
        let whole = text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        if whole < text.len() {
            eprintln!("identity: dropping a last line cut short in {shown}");
            OpenOptions::new()
                .write(true)
                .open(path)
                .and_then(|f| f.set_len(whole as u64))
                .map_err(|e| format!("{shown}: {e}"))?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path).map_err(|e| format!("{shown}: {e}"))?;
        let mut state = State { keys: BTreeMap::new(), by_account: BTreeMap::new(), file };
        let lines = std::str::from_utf8(&text[..whole]).map_err(|_| format!("{shown}: not text"))?;
        for (n, line) in lines.lines().enumerate() {
            let record: Value =
                serde_json::from_str(line).map_err(|e| format!("{shown}: line {}: not JSON: {e}", n + 1))?;
            if n == 0 {
                if record["format"] != FORMAT {
                    return Err(format!("{shown}: not a {FORMAT} store"));
                }
                continue;
            }
            replay(&mut state, &record).map_err(|e| format!("{shown}: line {}: {e}", n + 1))?;
        }
        let store = Self { state: Mutex::new(state), fsync, accounts };
        if whole == 0 {
            store.write(&mut locked(&store.state), &json!({"format": FORMAT})).map_err(|e| e.message)?;
        }
        Ok(store)
    }

    /// Appends one record, durably when the store syncs, before the change it records is made.
    fn write(&self, state: &mut State, record: &Value) -> Result<(), Refusal> {
        let mut line = record.to_string();
        line.push('\n');
        let written =
            state
                .file
                .write_all(line.as_bytes())
                .and_then(|()| if self.fsync { state.file.sync_data() } else { Ok(()) });
        written.map_err(|e| {
            eprintln!("identity: writing the store: {e}");
            Refusal::unavailable("the key store could not be written; nothing was changed")
        })
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
        if !self.accounts.contains(&account) {
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
        let record = json!({
            "op": "create", "keyId": made.id, "accountId": account, "label": made.label,
            "permissions": permissions.names(), "hash": key::hex(&made.hash), "createdAt": now,
            "expiresAt": expires_at, "createdBy": made.created_by
        });
        self.write(&mut state, &record)?;
        let mut answer = made.json();
        answer["secret"] = json!(secret);
        answer["credential"] = json!(format!("{}.{secret}", made.id));
        insert(&mut state, made);
        Ok(answer)
    }

    /// Every key of `account`, newest first.
    pub fn list(&self, account: u64) -> Value {
        let state = locked(&self.state);
        let keys: Vec<Value> =
            state.by_account.get(&account).into_iter().flatten().rev().map(|id| state.keys[id].json()).collect();
        json!({"keys": keys})
    }

    /// Revokes one of `account`'s keys. Revoking a key already revoked answers it as it is.
    pub fn revoke(&self, account: u64, key_id: &str, now: u64) -> Result<Value, Refusal> {
        let mut state = locked(&self.state);
        let found = state.keys.get(key_id).filter(|k| k.account == account);
        let found = found.ok_or_else(|| Refusal::not_found("no such key of this account"))?;
        if found.revoked_at.is_some() {
            return Ok(found.json());
        }
        self.write(&mut state, &json!({"op": "revoke", "keyId": key_id, "at": now}))?;
        let revoked = state.keys.get_mut(key_id).ok_or_else(|| Refusal::not_found("no such key of this account"))?;
        revoked.revoked_at = Some(now);
        Ok(revoked.json())
    }

    /// Which key a credential is, if it is a valid one now, noting its use. Every refusal is `UNAUTHORIZED`; only a
    /// caller that knew the secret learns that the key was revoked or has expired (decision G-11).
    pub fn verify(&self, credential: &str, now: u64) -> Result<Verified, Refusal> {
        let invalid = || Refusal::unauthorized("the API key is not valid");
        let (id, secret) = key::split(credential).ok_or_else(invalid)?;
        let presented = key::hash(id, secret);
        let mut state = locked(&self.state);
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
        if due && self.write(&mut state, &json!({"op": "used", "keyId": id, "at": now})).is_ok() {
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

fn insert(state: &mut State, made: Key) {
    state.by_account.entry(made.account).or_default().push(made.id.clone());
    state.keys.insert(made.id.clone(), made);
}

/// Applies one record of the file to what has been replayed so far.
fn replay(state: &mut State, record: &Value) -> Result<(), String> {
    let text = |name: &str| record[name].as_str().map(str::to_string).ok_or_else(|| format!("no {name}"));
    let time = |name: &str| record[name].as_u64().ok_or_else(|| format!("no {name}"));
    let id = text("keyId")?;
    match record["op"].as_str() {
        Some("create") => {
            let names: Vec<&str> =
                record["permissions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let made = Key {
                hash: key::unhex(&text("hash")?).ok_or("a malformed hash")?,
                account: record["accountId"].as_u64().ok_or("no accountId")?,
                label: text("label")?,
                permissions: Permissions::from_names(names)?,
                created_at: time("createdAt")?,
                expires_at: record["expiresAt"].as_u64(),
                revoked_at: None,
                created_by: record["createdBy"].as_str().map(str::to_string),
                last_used: None,
                written_use: None,
                id,
            };
            if state.keys.contains_key(&made.id) {
                return Err(format!("key {} is created twice", made.id));
            }
            insert(state, made);
        }
        Some(op @ ("revoke" | "used")) => {
            let at = time("at")?;
            let found = state.keys.get_mut(&id).ok_or_else(|| format!("{op} of unknown key {id}"))?;
            if op == "revoke" {
                found.revoked_at = Some(at);
            } else {
                found.last_used = Some(at);
                found.written_use = Some(at);
            }
        }
        _ => return Err("an unknown record".into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;
    use std::path::PathBuf;
    use std::sync::Arc;

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

    fn path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("venue-identity-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn open(path: &Path) -> Store {
        Store::open(path, false, BTreeSet::from([1, 2])).expect("a store")
    }

    fn credential(created: &Value) -> String {
        created["credential"].as_str().expect("a credential").to_string()
    }

    #[test]
    fn a_created_key_verifies_only_with_its_own_secret() {
        let store = open(&path("verify"));
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
        let store = open(&path("malformed"));
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
        let store = open(&path("expiry"));
        let created = store.create(1, &request("short", &["read"], Some(NOW + 1000)), NOW).expect("created");
        assert!(store.verify(&credential(&created), NOW + 999).is_ok());
        let refused = store.verify(&credential(&created), NOW + 1000).expect_err("expired");
        assert_eq!((refused.status, refused.message.as_str()), (401, "the API key has expired"));
    }

    #[test]
    fn a_revoked_key_is_refused_for_good_and_only_its_account_can_revoke_it() {
        let store = open(&path("revoke"));
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
        let store = open(&path("bounded"));
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

    #[test]
    fn the_store_replays_keys_revocations_and_uses_and_survives_a_cut_off_line() {
        let file = path("replay");
        let (kept, revoked, used) = {
            let store = open(&file);
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
        let text = std::fs::read_to_string(&file).expect("read");
        assert_eq!(text.lines().filter(|l| l.contains("\"used\"")).count(), 1, "{text}");
        assert!(!text.contains(kept["secret"].as_str().expect("a secret")), "a secret was written");
        // A stop in the middle of a write.
        let mut cut = std::fs::OpenOptions::new().append(true).open(&file).expect("opened");
        cut.write_all(br#"{"op":"revoke","keyId":"#).expect("written");
        drop(cut);

        let store = open(&file);
        let replayed = store.list(1);
        assert_eq!(replayed["keys"][0]["createdBy"], kept["keyId"]);
        assert_eq!(replayed["keys"][1]["lastUsedAt"], clock::rfc3339(NOW + 20), "the last use written survives");
        assert_eq!(used["keys"][0], replayed["keys"][0]);
        assert!(store.verify(&credential(&kept), NOW + 40).is_ok(), "a kept key verifies after replay");
        assert_eq!(store.verify(&credential(&revoked), NOW + 40).expect_err("still revoked").status, 401);
        // The cut line went, and new records follow whole lines.
        store.create(1, &request("after", &["read"], None), NOW + 50).expect("created");
        drop(store);
        assert_eq!(open(&file).list(1)["keys"].as_array().map(Vec::len), Some(3));

        std::fs::write(&file, "{\"format\":\"something-else\"}\n").expect("written");
        assert!(Store::open(&file, false, BTreeSet::new()).is_err(), "a foreign file is refused");
    }

    #[test]
    fn many_threads_create_verify_and_revoke_at_once() {
        let file = path("concurrent");
        let store = Arc::new(open(&file));
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
        let store = open(&file);
        let valid = made.iter().filter(|c| store.verify(&credential(c), NOW).is_ok()).count();
        assert_eq!((made.len(), valid), (40, 24));
    }
}
