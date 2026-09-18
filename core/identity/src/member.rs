//! What a member is: the person behind an account, their email and password, and the session an app holds after
//! logging in (core/identity/README.md, members; interface 6, in draft).
//!
//! A password is kept as an Argon2id hash and nothing else. A session token is kept as a SHA-256, as a key's secret
//! is (decision G-2): it carries 256 random bits of its own, so a fast hash over it is as safe as a slow one, and
//! verifying a session — which happens on every gateway cache miss — costs nothing. A password is not like that:
//! people choose them, so the hash over one is deliberately slow.

use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use sha2::{Digest, Sha256};
use std::io;

/// Every member ID starts with this.
pub const MEMBER_PREFIX: &str = "m_";
/// Every session ID starts with this, so a credential says which kind it is before anything looks it up.
pub const SESSION_PREFIX: &str = "vs_";
/// Hex digits after either prefix: 64 random bits.
const ID_DIGITS: usize = 16;
/// Hex digits of a session's secret: 256 random bits.
const SECRET_DIGITS: usize = 64;
/// What a stored session hash is over, besides the ID and the secret: a version, so a later scheme cannot collide.
const HASH_TAG: &str = "venue-session/1:";
/// The same, for an emailed token or a recovery code, so a hash of one can never be mistaken for a hash of the other.
const TOKEN_TAG: &str = "venue-token/1:";

/// How long a session lasts before an app must log in again. Long enough that an app used daily is not asked every
/// time, short enough that a token taken from a device stops working (interfaces/gateway/decisions.md, G-16).
pub const SESSION_SECONDS: u64 = 30 * 24 * 60 * 60;

/// The shortest password taken. Length is the only rule: a list of composition rules makes weaker passwords, and
/// checking a password against the breached-password corpora belongs with the real identity service.
pub const MIN_PASSWORD: usize = 10;
/// The longest, so that hashing one can never be made expensive on purpose.
pub const MAX_PASSWORD: usize = 256;

/// Whether a credential is meant as a session's: it starts as a session ID does.
pub fn looks_like_session(credential: &str) -> bool {
    credential.starts_with(SESSION_PREFIX)
}

fn is_lower_hex(text: &str, digits: usize) -> bool {
    text.len() == digits && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `text` is a session ID.
pub fn is_session_id(text: &str) -> bool {
    text.strip_prefix(SESSION_PREFIX).is_some_and(|hex| is_lower_hex(hex, ID_DIGITS))
}

/// A credential's session ID and secret, if it is `<sessionId>.<secret>` in the right form.
pub fn split(credential: &str) -> Option<(&str, &str)> {
    let (id, secret) = credential.split_once('.')?;
    (is_session_id(id) && is_lower_hex(secret, SECRET_DIGITS)).then_some((id, secret))
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut buffer)?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn new_member_id() -> io::Result<String> {
    Ok(format!("{MEMBER_PREFIX}{}", random_hex(ID_DIGITS / 2)?))
}

pub fn new_session_id() -> io::Result<String> {
    Ok(format!("{SESSION_PREFIX}{}", random_hex(ID_DIGITS / 2)?))
}

pub fn new_secret() -> io::Result<String> {
    random_hex(SECRET_DIGITS / 2)
}

/// What is stored in place of a session's secret.
pub fn hash(id: &str, secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(HASH_TAG.as_bytes());
    hasher.update(id.as_bytes());
    hasher.update(b":");
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

/// A token emailed to a member: 128 random bits as hex. More than a code read once out of a message needs, and short
/// enough to go in a link or be typed by hand.
pub fn new_token() -> io::Result<String> {
    random_hex(16)
}

/// A recovery code as a person reads it out: ten hex digits in two groups, so it can be written down and typed back.
/// Matched case-insensitively and with the groups' dash optional, because how it was copied should not matter.
pub fn new_recovery_code() -> io::Result<String> {
    let hex = random_hex(5)?;
    Ok(format!("{}-{}", &hex[..5], &hex[5..]))
}

/// How a recovery code is written before it is hashed or compared, so that case and spacing cannot refuse a code the
/// member has right.
pub fn tidy_recovery_code(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

/// The secret an authenticator app is given: 160 bits, which is what RFC 4226 asks for.
pub fn new_totp_secret() -> io::Result<Vec<u8>> {
    let mut buffer = vec![0u8; 20];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut buffer)?;
    Ok(buffer)
}

/// What is stored in place of an emailed token or a recovery code. They carry their own randomness, as a session's
/// secret does, so a fast hash over one is as safe here as a slow one — and only a password needs the slow one.
pub fn token_hash(token: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TOKEN_TAG.as_bytes());
    hasher.update(token.trim().as_bytes());
    hasher.finalize().into()
}

/// An email as it is stored and looked up: trimmed and lower-cased, so the same address typed in any case is the
/// same member. Nothing else is normalised — how an address routes is its provider's business, not ours.
pub fn normalise_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// Whether an address is one this service will take. Deliberately loose: an address is proved by sending to it, which
/// the real identity service will do, and a stricter pattern only turns valid addresses away.
pub fn looks_like_email(email: &str) -> bool {
    let at = email.find('@');
    match at {
        Some(at) => {
            let (local, domain) = (&email[..at], &email[at + 1..]);
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && email.len() <= 254
                && !email.contains(char::is_whitespace)
        }
        None => false,
    }
}

/// The Argon2id hash of a password, as a PHC string that carries its own salt and parameters, so a later change of
/// parameters can still verify what this one wrote.
pub fn hash_password(password: &str) -> Result<String, String> {
    // The salt is the hasher's to make: a large random one, from the system, per password.
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| format!("hashing the password: {e}"))
}

/// Whether `password` is the one `phc` was made from. A hash this cannot parse verifies nothing, which refuses the
/// login rather than letting it through.
pub fn password_matches(phc: &str, password: &str) -> bool {
    PasswordHash::new(phc).is_ok_and(|parsed| Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_password_verifies_against_its_own_hash_and_nothing_else() {
        let phc = hash_password("correct horse battery").expect("hashed");
        assert!(phc.starts_with("$argon2id$"), "{phc}");
        assert!(password_matches(&phc, "correct horse battery"));
        assert!(!password_matches(&phc, "correct horse batterY"));
        assert!(!password_matches(&phc, ""));
        // Two hashes of the same password differ: each carries its own salt.
        assert_ne!(phc, hash_password("correct horse battery").expect("hashed"));
        // Anything that is not a hash verifies nothing.
        assert!(!password_matches("not a hash", "correct horse battery"));
    }

    #[test]
    fn a_session_credential_says_what_it_is() {
        let id = new_session_id().expect("an ID");
        let secret = new_secret().expect("a secret");
        let credential = format!("{id}.{secret}");
        assert!(looks_like_session(&credential));
        assert_eq!(split(&credential), Some((id.as_str(), secret.as_str())));
        assert!(!looks_like_session("ak_0123456789abcdef.00"), "a key is not a session");
        assert_eq!(split("vs_nothex.00"), None);
        assert_eq!(split(&id), None, "an ID alone is not a credential");
        assert_eq!(hash(&id, &secret), hash(&id, &secret));
        assert_ne!(hash(&id, &secret), hash(&id, "0"));
    }

    #[test]
    fn an_email_is_the_same_member_however_it_was_typed() {
        assert_eq!(normalise_email("  Someone@Example.COM "), "someone@example.com");
        assert!(looks_like_email("someone@example.com"));
        assert!(!looks_like_email("someone"));
        assert!(!looks_like_email("@example.com"));
        assert!(!looks_like_email("someone@example"));
        assert!(!looks_like_email("some one@example.com"));
    }
}
