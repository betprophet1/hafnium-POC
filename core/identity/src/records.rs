//! The identity service's records: accounts and API keys, in PostgreSQL (the schema is `migrations/`) or, for tests
//! and throwaway local venues, in memory.
//!
//! `accounts` holds each account of the environment: its ID, display name and, in test environments, the SHA-256 of
//! its dev token, seeded from the catalogue at start. It is the future home of the mapping from pseudonymous account
//! IDs to people, which the blueprint keeps in the accounts database and nowhere else (§12, no personal data); that
//! mapping belongs in tables of its own beside it, referring to it, with retention of their own. Nothing else about a
//! person is kept yet.
//!
//! `api_keys` holds each key: its hashed secret, permissions, expiry, revocation and last use. The database refuses a
//! key's deletion, a change to its terms, and a revocation undone; only its last use moves freely.

use crate::key;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::SystemTime;
use venue_store::postgres::Row;
pub use venue_store::Error;
use venue_store::{big, millis, time, unsigned, Database, Migration};

/// The identity service's schema, in order. A released migration is never edited: a change is a new one.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "20260917000001",
        name: "accounts_and_keys",
        sql: include_str!("../migrations/20260917000001_accounts_and_keys.sql"),
    },
    Migration {
        version: "20260918000001",
        name: "members_and_sessions",
        sql: include_str!("../migrations/20260918000001_members_and_sessions.sql"),
    },
    Migration {
        version: "20260918000002",
        name: "member_security",
        sql: include_str!("../migrations/20260918000002_member_security.sql"),
    },
];

/// An account as the catalogue describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: u64,
    pub name: String,
    /// The dev token that acts as the account in test environments, if it has one. Only its hash is kept.
    pub dev_token: Option<String>,
}

/// What a dev token is kept as: tagged like a key's hash (decision G-2), so neither can stand for the other.
pub fn dev_token_hash(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"venue-dev-token/1:");
    hasher.update(token.as_bytes());
    hasher.finalize().into()
}

/// A key as the records hold it: everything but its secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRow {
    pub id: String,
    pub account: u64,
    pub label: String,
    pub permissions: Vec<String>,
    pub hash: [u8; 32],
    pub created_at: u64,
    pub expires_at: Option<u64>,
    pub revoked_at: Option<u64>,
    pub created_by: Option<String>,
    pub last_used: Option<u64>,
}

/// A member: the person behind an account (`member.rs`). The password is here only as its Argon2id hash.
#[derive(Clone, Debug)]
pub struct MemberRow {
    pub id: String,
    pub account: u64,
    pub email: String,
    pub password_phc: String,
    pub display_name: String,
    pub created_at: u64,
    pub disabled_at: Option<u64>,
    /// When the member proved the address; none until they have.
    pub email_verified_at: Option<u64>,
    /// Failed logins in a row, and until when those failures have locked the member out.
    pub failed_logins: u32,
    pub locked_until: Option<u64>,
}

/// A session a member logged in with. The token is here only as its SHA-256.
#[derive(Clone, Debug)]
pub struct SessionRow {
    pub id: String,
    pub member: String,
    pub account: u64,
    /// Whether the member had proved their email when this was read: what a session may do depends on it.
    pub verified: bool,
    /// What the app calls the thing it runs on, so a member can tell their own sessions apart in a list and end the
    /// one they do not recognise. A name the app gives; never anything the venue works out about the device.
    pub device: Option<String>,
    pub hash: [u8; 32],
    pub created_at: u64,
    pub expires_at: u64,
    pub ended_at: Option<u64>,
    pub last_used: Option<u64>,
}

/// A one-time token emailed to a member: proving an address, or setting a new password.
#[derive(Clone, Debug)]
pub struct TokenRow {
    pub hash: [u8; 32],
    pub member: String,
    pub purpose: String,
    pub created_at: u64,
    pub expires_at: u64,
    pub used_at: Option<u64>,
}

/// A member's second factor, before and after they have proved it works.
#[derive(Clone, Debug)]
pub struct TotpRow {
    pub member: String,
    pub secret: Vec<u8>,
    pub created_at: u64,
    pub confirmed_at: Option<u64>,
    pub last_step: Option<u64>,
}

/// Mail the venue means to send.
#[derive(Clone, Debug)]
pub struct MailRow {
    pub id: i64,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub created_at: u64,
    pub sent_at: Option<u64>,
}

pub trait KeyRecords: Send + Sync {
    /// Makes the accounts table say what the catalogue says: new accounts added, names and dev tokens brought up to
    /// date, accounts no longer listed kept (a mapping outlives the records that refer to it).
    fn seed_accounts(&self, accounts: &[Account], now: u64) -> Result<(), Error>;
    /// Every key, in the order they were made.
    fn load_keys(&self) -> Result<Vec<KeyRow>, Error>;
    fn create(&self, key: &KeyRow) -> Result<(), Error>;
    /// Revokes a key, and returns when it was revoked: `at`, or an earlier revocation the records already hold.
    fn revoke(&self, key_id: &str, at: u64) -> Result<u64, Error>;
    fn used(&self, key_id: &str, at: u64) -> Result<(), Error>;
    fn ping(&self) -> Result<(), Error>;

    /// The account ID a member signing up is given: one above every account the environment holds, and never below
    /// `floor`, so that accounts people made are told apart from the catalogue's at a glance.
    fn next_account(&self, floor: u64) -> Result<u64, Error>;
    /// Writes an account and the member of it together: either both or neither.
    fn create_member(&self, member: &MemberRow, name: &str, now: u64) -> Result<(), Error>;
    /// The member who logs in with `email`, if there is one. Read when someone logs in, not on every request.
    fn member_by_email(&self, email: &str) -> Result<Option<MemberRow>, Error>;
    /// Every session that has not ended and has not expired at `now`, read once at start.
    fn load_sessions(&self, now: u64) -> Result<Vec<SessionRow>, Error>;
    fn create_session(&self, session: &SessionRow) -> Result<(), Error>;
    /// Ends a session, and returns when it ended: `at`, or an earlier end the records already hold.
    fn end_session(&self, session_id: &str, at: u64) -> Result<u64, Error>;
    fn session_used(&self, session_id: &str, at: u64) -> Result<(), Error>;
    /// Every session of a member, newest first, ended ones included: what a member is shown of their own sessions.
    fn sessions_of(&self, member_id: &str) -> Result<Vec<SessionRow>, Error>;
    /// Ends every session of a member: what setting a new password does.
    fn end_sessions_of(&self, member_id: &str, at: u64) -> Result<Vec<String>, Error>;

    /// The member an ID names, for the calls a signed-in member makes about themselves.
    fn member(&self, member_id: &str) -> Result<Option<MemberRow>, Error>;
    /// Marks an email proved, and returns whether it had not been already.
    fn verify_email(&self, member_id: &str, at: u64) -> Result<bool, Error>;
    /// Replaces a member's password hash.
    fn set_password(&self, member_id: &str, phc: &str, at: u64) -> Result<(), Error>;
    /// Counts a failed login, and returns how many have failed in a row and until when the member is locked out.
    fn login_failed(
        &self,
        member_id: &str,
        at: u64,
        lock_after: u32,
        lock_for_ms: u64,
    ) -> Result<(u32, Option<u64>), Error>;
    /// Forgets the failures: a login that worked.
    fn login_worked(&self, member_id: &str) -> Result<(), Error>;

    fn create_token(&self, token: &TokenRow) -> Result<(), Error>;
    /// Takes a token: returns it and marks it used, or nothing if it is unknown, used or expired.
    fn take_token(&self, hash: &[u8; 32], purpose: &str, at: u64) -> Result<Option<TokenRow>, Error>;

    fn totp(&self, member_id: &str) -> Result<Option<TotpRow>, Error>;
    fn start_totp(&self, totp: &TotpRow) -> Result<(), Error>;
    fn confirm_totp(&self, member_id: &str, at: u64, step: u64) -> Result<(), Error>;
    fn totp_step_used(&self, member_id: &str, step: u64) -> Result<(), Error>;
    fn remove_totp(&self, member_id: &str) -> Result<(), Error>;

    /// Replaces a member's recovery codes with these hashes.
    fn set_recovery_codes(&self, member_id: &str, hashes: &[[u8; 32]], at: u64) -> Result<(), Error>;
    /// Takes one recovery code, if it is one of this member's and unused.
    fn take_recovery_code(&self, member_id: &str, hash: &[u8; 32], at: u64) -> Result<bool, Error>;
    /// How many are left.
    fn recovery_codes_left(&self, member_id: &str) -> Result<usize, Error>;

    /// Queues mail. Sending it is somebody else's (`mail.rs`).
    fn queue_mail(&self, to: &str, subject: &str, body: &str, at: u64) -> Result<(), Error>;
    /// Mail that has not been sent, oldest first.
    fn unsent_mail(&self, limit: usize) -> Result<Vec<MailRow>, Error>;
    fn mail_sent(&self, id: i64, at: u64) -> Result<(), Error>;
    /// The last mail this environment would have sent, newest first: how a demo shows what a member was told.
    fn recent_mail(&self, to: Option<&str>, limit: usize) -> Result<Vec<MailRow>, Error>;
}

/// Records in memory: shared by clones, so a store opened again on a clone is a restarted one; can be told to be
/// unreachable.
/// The accounts and keys held in memory.
/// What the in-memory records hold. Named rather than a tuple: there are too many of them now to count positions.
#[derive(Default)]
struct Held {
    accounts: BTreeMap<u64, Account>,
    keys: Vec<KeyRow>,
    members: Vec<MemberRow>,
    sessions: Vec<SessionRow>,
    tokens: Vec<TokenRow>,
    totp: Vec<TotpRow>,
    /// A member's recovery codes: the member, the hash, and when it was used.
    recovery: Vec<(String, [u8; 32], Option<u64>)>,
    mail: Vec<MailRow>,
}

#[derive(Clone, Default)]
pub struct Memory {
    data: Arc<Mutex<Held>>,
    down: Arc<AtomicBool>,
}

impl Memory {
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn data(&self) -> Result<MutexGuard<'_, Held>, Error> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("the records are down (a test said so)".into()));
        }
        Ok(self.data.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl KeyRecords for Memory {
    fn seed_accounts(&self, accounts: &[Account], _now: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        for a in accounts {
            data.accounts.insert(a.id, a.clone());
        }
        Ok(())
    }

    fn load_keys(&self) -> Result<Vec<KeyRow>, Error> {
        Ok(self.data()?.keys.clone())
    }

    fn create(&self, key: &KeyRow) -> Result<(), Error> {
        let mut data = self.data()?;
        if data.keys.iter().any(|k| k.id == key.id) {
            return Err(Error::refused(format!("key {} exists", key.id)));
        }
        data.keys.push(key.clone());
        Ok(())
    }

    fn revoke(&self, key_id: &str, at: u64) -> Result<u64, Error> {
        let mut data = self.data()?;
        let key = data.keys.iter_mut().find(|k| k.id == key_id).ok_or_else(|| Error::refused("no such key"))?;
        Ok(*key.revoked_at.get_or_insert(at))
    }

    fn used(&self, key_id: &str, at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(key) = data.keys.iter_mut().find(|k| k.id == key_id) {
            key.last_used = Some(key.last_used.map_or(at, |t| t.max(at)));
        }
        Ok(())
    }

    fn ping(&self) -> Result<(), Error> {
        self.data().map(|_| ())
    }

    fn next_account(&self, floor: u64) -> Result<u64, Error> {
        let data = self.data()?;
        Ok(data.accounts.keys().copied().max().map_or(floor, |top| top.saturating_add(1).max(floor)))
    }

    fn create_member(&self, member: &MemberRow, name: &str, _now: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if data.members.iter().any(|m| m.email == member.email) {
            return Err(Error::refused("that email already has an account"));
        }
        data.accounts.insert(member.account, Account { id: member.account, name: name.to_string(), dev_token: None });
        data.members.push(member.clone());
        Ok(())
    }

    fn member_by_email(&self, email: &str) -> Result<Option<MemberRow>, Error> {
        Ok(self.data()?.members.iter().find(|m| m.email == email).cloned())
    }

    fn load_sessions(&self, now: u64) -> Result<Vec<SessionRow>, Error> {
        Ok(self.data()?.sessions.iter().filter(|s| s.ended_at.is_none() && s.expires_at > now).cloned().collect())
    }

    fn create_session(&self, session: &SessionRow) -> Result<(), Error> {
        self.data()?.sessions.push(session.clone());
        Ok(())
    }

    fn end_session(&self, session_id: &str, at: u64) -> Result<u64, Error> {
        let mut data = self.data()?;
        let session =
            data.sessions.iter_mut().find(|s| s.id == session_id).ok_or_else(|| Error::refused("no such session"))?;
        Ok(*session.ended_at.get_or_insert(at))
    }

    fn session_used(&self, session_id: &str, at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(session) = data.sessions.iter_mut().find(|s| s.id == session_id) {
            session.last_used = Some(session.last_used.map_or(at, |t| t.max(at)));
        }
        Ok(())
    }

    fn sessions_of(&self, member_id: &str) -> Result<Vec<SessionRow>, Error> {
        let mut found: Vec<SessionRow> =
            self.data()?.sessions.iter().filter(|s| s.member == member_id).cloned().collect();
        found.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        Ok(found)
    }

    fn end_sessions_of(&self, member_id: &str, at: u64) -> Result<Vec<String>, Error> {
        let mut data = self.data()?;
        let mut ended = Vec::new();
        for session in data.sessions.iter_mut().filter(|s| s.member == member_id && s.ended_at.is_none()) {
            session.ended_at = Some(at);
            ended.push(session.id.clone());
        }
        Ok(ended)
    }

    fn member(&self, member_id: &str) -> Result<Option<MemberRow>, Error> {
        Ok(self.data()?.members.iter().find(|m| m.id == member_id).cloned())
    }

    fn verify_email(&self, member_id: &str, at: u64) -> Result<bool, Error> {
        let mut data = self.data()?;
        let member = data.members.iter_mut().find(|m| m.id == member_id).ok_or_else(|| Error::refused("no member"))?;
        if member.email_verified_at.is_some() {
            return Ok(false);
        }
        member.email_verified_at = Some(at);
        Ok(true)
    }

    fn set_password(&self, member_id: &str, phc: &str, _at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        let member = data.members.iter_mut().find(|m| m.id == member_id).ok_or_else(|| Error::refused("no member"))?;
        member.password_phc = phc.to_string();
        member.failed_logins = 0;
        member.locked_until = None;
        Ok(())
    }

    fn login_failed(
        &self,
        member_id: &str,
        at: u64,
        lock_after: u32,
        lock_for_ms: u64,
    ) -> Result<(u32, Option<u64>), Error> {
        let mut data = self.data()?;
        let member = data.members.iter_mut().find(|m| m.id == member_id).ok_or_else(|| Error::refused("no member"))?;
        member.failed_logins += 1;
        if member.failed_logins >= lock_after {
            member.locked_until = Some(at.saturating_add(lock_for_ms));
        }
        Ok((member.failed_logins, member.locked_until))
    }

    fn login_worked(&self, member_id: &str) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(member) = data.members.iter_mut().find(|m| m.id == member_id) {
            member.failed_logins = 0;
            member.locked_until = None;
        }
        Ok(())
    }

    fn create_token(&self, token: &TokenRow) -> Result<(), Error> {
        self.data()?.tokens.push(token.clone());
        Ok(())
    }

    fn take_token(&self, hash: &[u8; 32], purpose: &str, at: u64) -> Result<Option<TokenRow>, Error> {
        let mut data = self.data()?;
        let Some(token) = data
            .tokens
            .iter_mut()
            .find(|t| t.hash == *hash && t.purpose == purpose && t.used_at.is_none() && t.expires_at > at)
        else {
            return Ok(None);
        };
        token.used_at = Some(at);
        Ok(Some(token.clone()))
    }

    fn totp(&self, member_id: &str) -> Result<Option<TotpRow>, Error> {
        Ok(self.data()?.totp.iter().find(|t| t.member == member_id).cloned())
    }

    fn start_totp(&self, totp: &TotpRow) -> Result<(), Error> {
        let mut data = self.data()?;
        data.totp.retain(|t| t.member != totp.member);
        data.totp.push(totp.clone());
        Ok(())
    }

    fn confirm_totp(&self, member_id: &str, at: u64, step: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        let totp = data.totp.iter_mut().find(|t| t.member == member_id).ok_or_else(|| Error::refused("no totp"))?;
        totp.confirmed_at = Some(at);
        totp.last_step = Some(step);
        Ok(())
    }

    fn totp_step_used(&self, member_id: &str, step: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(totp) = data.totp.iter_mut().find(|t| t.member == member_id) {
            totp.last_step = Some(totp.last_step.map_or(step, |t| t.max(step)));
        }
        Ok(())
    }

    fn remove_totp(&self, member_id: &str) -> Result<(), Error> {
        let mut data = self.data()?;
        data.totp.retain(|t| t.member != member_id);
        data.recovery.retain(|(member, _, _)| member != member_id);
        Ok(())
    }

    fn set_recovery_codes(&self, member_id: &str, hashes: &[[u8; 32]], _at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        data.recovery.retain(|(member, _, _)| member != member_id);
        for hash in hashes {
            data.recovery.push((member_id.to_string(), *hash, None));
        }
        Ok(())
    }

    fn take_recovery_code(&self, member_id: &str, hash: &[u8; 32], at: u64) -> Result<bool, Error> {
        let mut data = self.data()?;
        let Some(code) =
            data.recovery.iter_mut().find(|(member, kept, used)| member == member_id && kept == hash && used.is_none())
        else {
            return Ok(false);
        };
        code.2 = Some(at);
        Ok(true)
    }

    fn recovery_codes_left(&self, member_id: &str) -> Result<usize, Error> {
        Ok(self.data()?.recovery.iter().filter(|(member, _, used)| member == member_id && used.is_none()).count())
    }

    fn queue_mail(&self, to: &str, subject: &str, body: &str, at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        let id = data.mail.len() as i64 + 1;
        data.mail.push(MailRow {
            id,
            to: to.to_string(),
            subject: subject.to_string(),
            body: body.to_string(),
            created_at: at,
            sent_at: None,
        });
        Ok(())
    }

    fn unsent_mail(&self, limit: usize) -> Result<Vec<MailRow>, Error> {
        Ok(self.data()?.mail.iter().filter(|m| m.sent_at.is_none()).take(limit).cloned().collect())
    }

    fn mail_sent(&self, id: i64, at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(mail) = data.mail.iter_mut().find(|m| m.id == id) {
            mail.sent_at = Some(at);
        }
        Ok(())
    }

    fn recent_mail(&self, to: Option<&str>, limit: usize) -> Result<Vec<MailRow>, Error> {
        let data = self.data()?;
        let mut found: Vec<MailRow> =
            data.mail.iter().filter(|m| to.is_none_or(|address| m.to == address)).cloned().collect();
        found.sort_by_key(|m| std::cmp::Reverse(m.id));
        found.truncate(limit);
        Ok(found)
    }
}

pub struct Postgres {
    db: Database,
}

impl Postgres {
    pub const fn new(db: Database) -> Self {
        Self { db }
    }
}

fn key_from(row: &Row) -> Result<KeyRow, Error> {
    let hash: Vec<u8> = row.get("secret_sha256");
    Ok(KeyRow {
        id: row.get("key_id"),
        account: unsigned(row.get("account_id"))?,
        label: row.get("label"),
        permissions: row.get("permissions"),
        hash: hash.try_into().map_err(|_| Error::refused("a key hash that is not 32 bytes"))?,
        created_at: millis(row.get("created_at")),
        expires_at: row.get::<_, Option<SystemTime>>("expires_at").map(millis),
        revoked_at: row.get::<_, Option<SystemTime>>("revoked_at").map(millis),
        created_by: row.get("created_by"),
        last_used: row.get::<_, Option<SystemTime>>("last_used_at").map(millis),
    })
}

impl KeyRecords for Postgres {
    fn seed_accounts(&self, accounts: &[Account], now: u64) -> Result<(), Error> {
        self.db.transaction(|tx| {
            for a in accounts {
                let token = a.dev_token.as_deref().map(|t| dev_token_hash(t).to_vec());
                tx.execute(
                    "INSERT INTO accounts AS a (account_id, display_name, dev_token_sha256, created_at, updated_at)
                     VALUES ($1, $2, $3, $4, $4)
                     ON CONFLICT (account_id) DO UPDATE SET display_name = EXCLUDED.display_name,
                       dev_token_sha256 = EXCLUDED.dev_token_sha256, updated_at = EXCLUDED.updated_at
                     WHERE (a.display_name, a.dev_token_sha256) IS DISTINCT FROM
                           (EXCLUDED.display_name, EXCLUDED.dev_token_sha256)",
                    &[&big(a.id)?, &a.name, &token, &time(now)],
                )?;
            }
            Ok(())
        })
    }

    fn load_keys(&self) -> Result<Vec<KeyRow>, Error> {
        self.db.get()?.query("SELECT * FROM api_keys ORDER BY made", &[])?.iter().map(key_from).collect()
    }

    fn create(&self, k: &KeyRow) -> Result<(), Error> {
        let expires = k.expires_at.map(time);
        self.db.get()?.execute(
            "INSERT INTO api_keys (key_id, account_id, label, permissions, secret_sha256, created_at, expires_at,
               created_by, revoked_at, last_used_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &k.id,
                &big(k.account)?,
                &k.label,
                &k.permissions,
                &k.hash.to_vec(),
                &time(k.created_at),
                &expires,
                &k.created_by,
                &k.revoked_at.map(time),
                &k.last_used.map(time),
            ],
        )?;
        Ok(())
    }

    fn revoke(&self, key_id: &str, at: u64) -> Result<u64, Error> {
        let mut conn = self.db.get()?;
        conn.execute(
            "UPDATE api_keys SET revoked_at = $2 WHERE key_id = $1 AND revoked_at IS NULL",
            &[&key_id, &time(at)],
        )?;
        let row = conn
            .query_opt("SELECT revoked_at FROM api_keys WHERE key_id = $1", &[&key_id])?
            .ok_or_else(|| Error::refused(format!("no key {key_id}")))?;
        Ok(row.get::<_, Option<SystemTime>>(0).map_or(at, millis))
    }

    fn used(&self, key_id: &str, at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "UPDATE api_keys SET last_used_at = $2 WHERE key_id = $1 AND (last_used_at IS NULL OR last_used_at < $2)",
            &[&key_id, &time(at)],
        )?;
        Ok(())
    }

    fn ping(&self) -> Result<(), Error> {
        self.db.ping()
    }

    fn next_account(&self, floor: u64) -> Result<u64, Error> {
        let row = self.db.get()?.query_one("SELECT coalesce(max(account_id), 0) + 1 FROM accounts", &[])?;
        Ok(unsigned(row.get(0))?.max(floor))
    }

    fn create_member(&self, m: &MemberRow, name: &str, now: u64) -> Result<(), Error> {
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO accounts (account_id, display_name, dev_token_sha256, created_at, updated_at)
                 VALUES ($1, $2, NULL, $3, $3)",
                &[&big(m.account)?, &name, &time(now)],
            )?;
            tx.execute(
                "INSERT INTO members (member_id, account_id, email, password_phc, display_name, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[&m.id, &big(m.account)?, &m.email, &m.password_phc, &m.display_name, &time(m.created_at)],
            )?;
            Ok(())
        })
    }

    fn member_by_email(&self, email: &str) -> Result<Option<MemberRow>, Error> {
        let row = self.db.get()?.query_opt("SELECT * FROM members WHERE email = $1", &[&email])?;
        row.as_ref().map(member_from).transpose()
    }

    fn load_sessions(&self, now: u64) -> Result<Vec<SessionRow>, Error> {
        self.db
            .get()?
            .query(
                "SELECT s.*, m.account_id, m.email_verified_at IS NOT NULL AS verified
                 FROM member_sessions s JOIN members m USING (member_id)
                 WHERE s.ended_at IS NULL AND s.expires_at > $1",
                &[&time(now)],
            )?
            .iter()
            .map(session_from)
            .collect()
    }

    fn create_session(&self, s: &SessionRow) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO member_sessions (session_id, member_id, token_sha256, created_at, expires_at, device)
             VALUES ($1, $2, $3, $4, $5, $6)",
            &[&s.id, &s.member, &s.hash.to_vec(), &time(s.created_at), &time(s.expires_at), &s.device],
        )?;
        Ok(())
    }

    fn end_session(&self, session_id: &str, at: u64) -> Result<u64, Error> {
        let mut conn = self.db.get()?;
        conn.execute(
            "UPDATE member_sessions SET ended_at = $2 WHERE session_id = $1 AND ended_at IS NULL",
            &[&session_id, &time(at)],
        )?;
        let row = conn
            .query_opt("SELECT ended_at FROM member_sessions WHERE session_id = $1", &[&session_id])?
            .ok_or_else(|| Error::refused(format!("no session {session_id}")))?;
        Ok(row.get::<_, Option<SystemTime>>(0).map_or(at, millis))
    }

    fn session_used(&self, session_id: &str, at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "UPDATE member_sessions SET last_used_at = $2
             WHERE session_id = $1 AND (last_used_at IS NULL OR last_used_at < $2)",
            &[&session_id, &time(at)],
        )?;
        Ok(())
    }

    fn sessions_of(&self, member_id: &str) -> Result<Vec<SessionRow>, Error> {
        self.db
            .get()?
            .query(
                "SELECT s.*, m.account_id, m.email_verified_at IS NOT NULL AS verified
                 FROM member_sessions s JOIN members m USING (member_id)
                 WHERE s.member_id = $1 ORDER BY s.created_at DESC",
                &[&member_id],
            )?
            .iter()
            .map(session_from)
            .collect()
    }

    fn end_sessions_of(&self, member_id: &str, at: u64) -> Result<Vec<String>, Error> {
        let rows = self.db.get()?.query(
            "UPDATE member_sessions SET ended_at = $2 WHERE member_id = $1 AND ended_at IS NULL
             RETURNING session_id",
            &[&member_id, &time(at)],
        )?;
        Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
    }

    fn member(&self, member_id: &str) -> Result<Option<MemberRow>, Error> {
        let row = self.db.get()?.query_opt("SELECT * FROM members WHERE member_id = $1", &[&member_id])?;
        row.as_ref().map(member_from).transpose()
    }

    fn verify_email(&self, member_id: &str, at: u64) -> Result<bool, Error> {
        let changed = self.db.get()?.execute(
            "UPDATE members SET email_verified_at = $2 WHERE member_id = $1 AND email_verified_at IS NULL",
            &[&member_id, &time(at)],
        )?;
        Ok(changed == 1)
    }

    fn set_password(&self, member_id: &str, phc: &str, _at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "UPDATE members SET password_phc = $2, failed_logins = 0, locked_until = NULL WHERE member_id = $1",
            &[&member_id, &phc],
        )?;
        Ok(())
    }

    fn login_failed(
        &self,
        member_id: &str,
        at: u64,
        lock_after: u32,
        lock_for_ms: u64,
    ) -> Result<(u32, Option<u64>), Error> {
        let after = i32::try_from(lock_after).unwrap_or(i32::MAX);
        let row = self.db.get()?.query_one(
            "UPDATE members SET failed_logins = failed_logins + 1,
               locked_until = CASE WHEN failed_logins + 1 >= $2 THEN $3 ELSE locked_until END
             WHERE member_id = $1 RETURNING failed_logins, locked_until",
            &[&member_id, &after, &time(at.saturating_add(lock_for_ms))],
        )?;
        Ok((u32::try_from(row.get::<_, i32>(0)).unwrap_or(0), row.get::<_, Option<SystemTime>>(1).map(millis)))
    }

    fn login_worked(&self, member_id: &str) -> Result<(), Error> {
        self.db
            .get()?
            .execute("UPDATE members SET failed_logins = 0, locked_until = NULL WHERE member_id = $1", &[&member_id])?;
        Ok(())
    }

    fn create_token(&self, token: &TokenRow) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO member_tokens (token_sha256, member_id, purpose, created_at, expires_at)
             VALUES ($1, $2, $3, $4, $5)",
            &[&token.hash.to_vec(), &token.member, &token.purpose, &time(token.created_at), &time(token.expires_at)],
        )?;
        Ok(())
    }

    fn take_token(&self, hash: &[u8; 32], purpose: &str, at: u64) -> Result<Option<TokenRow>, Error> {
        // Taken and marked used in one statement: two apps racing on the same emailed link cannot both win.
        let row = self.db.get()?.query_opt(
            "UPDATE member_tokens SET used_at = $3
             WHERE token_sha256 = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > $3
             RETURNING *",
            &[&hash.to_vec(), &purpose, &time(at)],
        )?;
        row.as_ref().map(token_from).transpose()
    }

    fn totp(&self, member_id: &str) -> Result<Option<TotpRow>, Error> {
        let row = self.db.get()?.query_opt("SELECT * FROM member_totp WHERE member_id = $1", &[&member_id])?;
        row.as_ref().map(totp_from).transpose()
    }

    fn start_totp(&self, totp: &TotpRow) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO member_totp (member_id, secret, created_at) VALUES ($1, $2, $3)
             ON CONFLICT (member_id) DO UPDATE SET secret = EXCLUDED.secret, created_at = EXCLUDED.created_at,
               confirmed_at = NULL, last_step = NULL",
            &[&totp.member, &totp.secret, &time(totp.created_at)],
        )?;
        Ok(())
    }

    fn confirm_totp(&self, member_id: &str, at: u64, step: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "UPDATE member_totp SET confirmed_at = $2, last_step = $3 WHERE member_id = $1",
            &[&member_id, &time(at), &big(step)?],
        )?;
        Ok(())
    }

    fn totp_step_used(&self, member_id: &str, step: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "UPDATE member_totp SET last_step = $2 WHERE member_id = $1 AND (last_step IS NULL OR last_step < $2)",
            &[&member_id, &big(step)?],
        )?;
        Ok(())
    }

    fn remove_totp(&self, member_id: &str) -> Result<(), Error> {
        self.db.transaction(|tx| {
            tx.execute("DELETE FROM member_recovery_codes WHERE member_id = $1", &[&member_id])?;
            tx.execute("DELETE FROM member_totp WHERE member_id = $1", &[&member_id])?;
            Ok(())
        })
    }

    fn set_recovery_codes(&self, member_id: &str, hashes: &[[u8; 32]], at: u64) -> Result<(), Error> {
        self.db.transaction(|tx| {
            tx.execute("DELETE FROM member_recovery_codes WHERE member_id = $1", &[&member_id])?;
            for hash in hashes {
                tx.execute(
                    "INSERT INTO member_recovery_codes (member_id, code_sha256, created_at) VALUES ($1, $2, $3)",
                    &[&member_id, &hash.to_vec(), &time(at)],
                )?;
            }
            Ok(())
        })
    }

    fn take_recovery_code(&self, member_id: &str, hash: &[u8; 32], at: u64) -> Result<bool, Error> {
        let taken = self.db.get()?.execute(
            "UPDATE member_recovery_codes SET used_at = $3
             WHERE member_id = $1 AND code_sha256 = $2 AND used_at IS NULL",
            &[&member_id, &hash.to_vec(), &time(at)],
        )?;
        Ok(taken == 1)
    }

    fn recovery_codes_left(&self, member_id: &str) -> Result<usize, Error> {
        let row = self.db.get()?.query_one(
            "SELECT count(*) FROM member_recovery_codes WHERE member_id = $1 AND used_at IS NULL",
            &[&member_id],
        )?;
        Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or(0))
    }

    fn queue_mail(&self, to: &str, subject: &str, body: &str, at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO outbox_mail (to_email, subject, body, created_at) VALUES ($1, $2, $3, $4)",
            &[&to, &subject, &body, &time(at)],
        )?;
        Ok(())
    }

    fn unsent_mail(&self, limit: usize) -> Result<Vec<MailRow>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.db
            .get()?
            .query("SELECT * FROM outbox_mail WHERE sent_at IS NULL ORDER BY mail_id LIMIT $1", &[&limit])?
            .iter()
            .map(mail_from)
            .collect()
    }

    fn mail_sent(&self, id: i64, at: u64) -> Result<(), Error> {
        self.db.get()?.execute("UPDATE outbox_mail SET sent_at = $2 WHERE mail_id = $1", &[&id, &time(at)])?;
        Ok(())
    }

    fn recent_mail(&self, to: Option<&str>, limit: usize) -> Result<Vec<MailRow>, Error> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = match to {
            Some(address) => self.db.get()?.query(
                "SELECT * FROM outbox_mail WHERE to_email = $1 ORDER BY mail_id DESC LIMIT $2",
                &[&address, &limit],
            )?,
            None => self.db.get()?.query("SELECT * FROM outbox_mail ORDER BY mail_id DESC LIMIT $1", &[&limit])?,
        };
        rows.iter().map(mail_from).collect()
    }
}

fn token_from(row: &Row) -> Result<TokenRow, Error> {
    let hash: Vec<u8> = row.get("token_sha256");
    Ok(TokenRow {
        hash: hash.try_into().map_err(|_| Error::refused("a token hash that is not 32 bytes"))?,
        member: row.get("member_id"),
        purpose: row.get("purpose"),
        created_at: millis(row.get("created_at")),
        expires_at: millis(row.get("expires_at")),
        used_at: row.get::<_, Option<SystemTime>>("used_at").map(millis),
    })
}

fn totp_from(row: &Row) -> Result<TotpRow, Error> {
    Ok(TotpRow {
        member: row.get("member_id"),
        secret: row.get("secret"),
        created_at: millis(row.get("created_at")),
        confirmed_at: row.get::<_, Option<SystemTime>>("confirmed_at").map(millis),
        last_step: row.get::<_, Option<i64>>("last_step").map(|s| unsigned(s).unwrap_or(0)),
    })
}

fn mail_from(row: &Row) -> Result<MailRow, Error> {
    Ok(MailRow {
        id: row.get("mail_id"),
        to: row.get("to_email"),
        subject: row.get("subject"),
        body: row.get("body"),
        created_at: millis(row.get("created_at")),
        sent_at: row.get::<_, Option<SystemTime>>("sent_at").map(millis),
    })
}

fn member_from(row: &Row) -> Result<MemberRow, Error> {
    Ok(MemberRow {
        id: row.get("member_id"),
        account: unsigned(row.get("account_id"))?,
        email: row.get("email"),
        password_phc: row.get("password_phc"),
        display_name: row.get("display_name"),
        created_at: millis(row.get("created_at")),
        disabled_at: row.get::<_, Option<SystemTime>>("disabled_at").map(millis),
        email_verified_at: row.get::<_, Option<SystemTime>>("email_verified_at").map(millis),
        failed_logins: u32::try_from(row.get::<_, i32>("failed_logins")).unwrap_or(0),
        locked_until: row.get::<_, Option<SystemTime>>("locked_until").map(millis),
    })
}

fn session_from(row: &Row) -> Result<SessionRow, Error> {
    let hash: Vec<u8> = row.get("token_sha256");
    Ok(SessionRow {
        id: row.get("session_id"),
        member: row.get("member_id"),
        account: unsigned(row.get("account_id"))?,
        verified: row.get("verified"),
        device: row.get("device"),
        hash: hash.try_into().map_err(|_| Error::refused("a session hash that is not 32 bytes"))?,
        created_at: millis(row.get("created_at")),
        expires_at: millis(row.get("expires_at")),
        ended_at: row.get::<_, Option<SystemTime>>("ended_at").map(millis),
        last_used: row.get::<_, Option<SystemTime>>("last_used_at").map(millis),
    })
}

/// `venue-identity import`: moves a key store kept by earlier versions in a JSON-lines file into empty records, each
/// key with its revocation and last written use. Its accounts must already be in the records (seeded from the
/// catalogue). Returns how many keys it moved.
pub fn import(path: &std::path::Path, records: &dyn KeyRecords) -> Result<usize, String> {
    if !records.load_keys()?.is_empty() {
        return Err("the records already hold keys: an import only goes into empty ones".into());
    }
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let whole = text.rfind('\n').map_or(0, |i| i + 1);
    let mut keys: Vec<KeyRow> = Vec::new();
    for (n, line) in text[..whole].lines().enumerate() {
        let r: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("{shown}: line {}: {e}", n + 1))?;
        let at = |e: &str| format!("{shown}: line {}: {e}", n + 1);
        if n == 0 {
            if r["format"] != "venue-identity/1" {
                return Err(at("not a venue-identity/1 store"));
            }
            continue;
        }
        let id = r["keyId"].as_str().ok_or_else(|| at("no keyId"))?.to_string();
        match r["op"].as_str() {
            Some("create") => keys.push(KeyRow {
                hash: key::unhex(r["hash"].as_str().unwrap_or("")).ok_or_else(|| at("a malformed hash"))?,
                account: r["accountId"].as_u64().ok_or_else(|| at("no accountId"))?,
                label: r["label"].as_str().ok_or_else(|| at("no label"))?.to_string(),
                permissions: r["permissions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect(),
                created_at: r["createdAt"].as_u64().ok_or_else(|| at("no createdAt"))?,
                expires_at: r["expiresAt"].as_u64(),
                revoked_at: None,
                created_by: r["createdBy"].as_str().map(str::to_string),
                last_used: None,
                id,
            }),
            Some(op @ ("revoke" | "used")) => {
                let when = r["at"].as_u64().ok_or_else(|| at("no at"))?;
                let k = keys.iter_mut().find(|k| k.id == id).ok_or_else(|| at("a key never created"))?;
                if op == "revoke" {
                    k.revoked_at = Some(when);
                } else {
                    k.last_used = Some(when);
                }
            }
            _ => return Err(at("an unknown record")),
        }
    }
    for k in &keys {
        records.create(k).map_err(|e| format!("key {}: {e}", k.id))?;
    }
    Ok(keys.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tests;
    use venue_store::testing;

    fn postgres(name: &str) -> Option<(Database, Arc<dyn KeyRecords>)> {
        let config = testing::owned_config(name)?;
        let db = Database::connect(&config).expect("the test database");
        db.migrate(&venue_store::migrations_dir!()).expect("migrated");
        let records: Arc<dyn KeyRecords> = Arc::new(Postgres::new(db.clone()));
        let accounts = [1, 2].map(|id| Account { id, name: format!("account {id}"), dev_token: None });
        records.seed_accounts(&accounts, 1_000).expect("seeded");
        Some((db, records))
    }

    #[test]
    fn the_migration_files_are_the_ones_the_build_embeds() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        venue_store::check_files(&dir, MIGRATIONS).expect("the same migrations");
    }

    #[test]
    fn keys_revocations_and_uses_survive_a_restart_in_postgres() {
        let Some((db, records)) = postgres("identity_restart") else { return };
        tests::keys_revocations_and_uses_survive_a_restart(&records);
        let mut conn = db.get().expect("a connection");
        let row = conn
            .query_one("SELECT count(*), bool_and(octet_length(secret_sha256) = 32) FROM api_keys", &[])
            .expect("a row");
        assert_eq!((row.get::<_, i64>(0), row.get::<_, bool>(1)), (2, true));
    }

    #[test]
    fn many_threads_create_verify_and_revoke_at_once_in_postgres() {
        let Some((_, records)) = postgres("identity_threads") else { return };
        tests::many_threads_create_verify_and_revoke_at_once(&records);
    }

    #[test]
    fn accounts_are_seeded_idempotently_and_a_key_can_only_be_revoked() {
        let Some((db, records)) = postgres("identity_enforced") else { return };
        let catalogue = [
            Account { id: 1, name: "Market maker".into(), dev_token: Some("dev-market-maker".into()) },
            Account { id: 2, name: "account 2".into(), dev_token: None },
        ];
        records.seed_accounts(&catalogue, 2_000).expect("seeded");
        records.seed_accounts(&catalogue, 3_000).expect("seeded again");
        let mut conn = db.get().expect("a connection");
        let row = conn
            .query_one("SELECT display_name, dev_token_sha256, updated_at FROM accounts WHERE account_id = 1", &[])
            .expect("a row");
        assert_eq!(row.get::<_, String>(0), "Market maker");
        assert_eq!(row.get::<_, Vec<u8>>(1), dev_token_hash("dev-market-maker").to_vec(), "only the hash is kept");
        assert_eq!(millis(row.get(2)), 2_000, "an unchanged account is not written again");
        let key = KeyRow {
            id: "ak_00000000000000aa".into(),
            account: 1,
            label: "bot".into(),
            permissions: vec!["read".into(), "trade".into()],
            hash: [7; 32],
            created_at: 5_000,
            expires_at: None,
            revoked_at: None,
            created_by: None,
            last_used: None,
        };
        records.create(&key).expect("created");
        assert!(records.create(&key).is_err(), "made twice");
        records.used(&key.id, 6_000).expect("used");
        records.used(&key.id, 5_500).expect("an older use changes nothing");
        assert_eq!(records.revoke(&key.id, 7_000).expect("revoked"), 7_000);
        assert_eq!(records.revoke(&key.id, 8_000).expect("revoked again"), 7_000, "the first revocation stands");
        let loaded = records.load_keys().expect("keys");
        assert_eq!((loaded[0].last_used, loaded[0].revoked_at), (Some(6_000), Some(7_000)));
        for sql in [
            "UPDATE api_keys SET permissions = ARRAY['read', 'trade', 'transfers']",
            "UPDATE api_keys SET revoked_at = NULL",
            "UPDATE api_keys SET secret_sha256 = decode(repeat('00', 32), 'hex')",
            "DELETE FROM api_keys",
            "DELETE FROM accounts",
            "TRUNCATE api_keys",
            "INSERT INTO api_keys (key_id, account_id, label, permissions, secret_sha256, created_at)
               VALUES ('ak_00000000000000bb', 1, 'x', ARRAY['trade'], decode(repeat('00', 32), 'hex'), now())",
            "INSERT INTO api_keys (key_id, account_id, label, permissions, secret_sha256, created_at)
               VALUES ('ak_00000000000000cc', 99, 'x', ARRAY['read'], decode(repeat('00', 32), 'hex'), now())",
        ] {
            assert!(conn.batch_execute(sql).is_err(), "the database accepted: {sql}");
        }
    }

    #[test]
    fn a_key_file_imports_into_postgres() {
        let Some((_, records)) = postgres("identity_import") else { return };
        let path = std::env::temp_dir().join(format!("venue-identity-import-{}.jsonl", std::process::id()));
        let hash = "ab".repeat(32);
        let text = format!(
            "{{\"format\":\"venue-identity/1\"}}\n\
             {{\"op\":\"create\",\"keyId\":\"ak_0000000000000001\",\"accountId\":2,\"label\":\"bot\",\"permissions\":[\"read\"],\"hash\":\"{hash}\",\"createdAt\":1000,\"expiresAt\":null,\"createdBy\":null}}\n\
             {{\"op\":\"used\",\"keyId\":\"ak_0000000000000001\",\"at\":2000}}\n\
             {{\"op\":\"revoke\",\"keyId\":\"ak_0000000000000001\",\"at\":3000}}\n"
        );
        std::fs::write(&path, text).expect("written");
        assert_eq!(import(&path, records.as_ref()), Ok(1));
        let keys = records.load_keys().expect("keys");
        assert_eq!((keys[0].last_used, keys[0].revoked_at, keys[0].hash), (Some(2_000), Some(3_000), [0xab; 32]));
        assert!(import(&path, records.as_ref()).is_err(), "imported twice");
        let _ = std::fs::remove_file(&path);
    }
}
