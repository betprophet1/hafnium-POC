//! What an API key is: its ID, its secret, how the secret is kept, and what the key may do (interfaces/gateway/README.md,
//! API keys and permissions; the reasons are decisions G-1 to G-3 in interfaces/gateway/decisions.md).

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read};

/// Every key ID starts with this, so a credential can be told from a dev token by its first characters.
pub const ID_PREFIX: &str = "ak_";
/// Hex digits after the prefix: 64 random bits, enough that IDs never need coordinating.
const ID_DIGITS: usize = 16;
/// Hex digits of a secret: 256 random bits.
const SECRET_DIGITS: usize = 64;
/// What a stored hash is of, besides the ID and the secret: a version, so a later scheme can never collide with this one.
const HASH_TAG: &str = "venue-api-key/1:";

/// A set of permissions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Permissions(u8);

impl Permissions {
    pub const NONE: Self = Self(0);
    /// The account, its transfers and requests, and private sessions. Every key holds it.
    pub const READ: Self = Self(1);
    /// Orders, amendments, cancels, requests for quote and quotes.
    pub const TRADE: Self = Self(2);
    /// Deposits, withdrawals and their cancellation.
    pub const TRANSFERS: Self = Self(4);
    /// The account's keys.
    pub const KEYS: Self = Self(8);
    /// Answering requests for quote: subscribing to them, reading those open, and attaching quotes (decision G-12).
    /// The account must also be an approved responder, which is the parlay service's to say.
    pub const RESPOND: Self = Self(16);
    /// What a dev token holds.
    pub const ALL: Self = Self(31);

    /// Each permission with its name, in the order lists give them.
    const NAMED: [(Self, &'static str); 5] = [
        (Self::READ, "read"),
        (Self::TRADE, "trade"),
        (Self::TRANSFERS, "transfers"),
        (Self::KEYS, "keys"),
        (Self::RESPOND, "respond"),
    ];

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The name of a single permission, as a refusal names what was missing.
    pub fn name(self) -> &'static str {
        Self::NAMED.iter().find(|(p, _)| *p == self).map_or("", |(_, n)| n)
    }

    pub fn names(self) -> Vec<&'static str> {
        Self::NAMED.iter().filter(|(p, _)| self.contains(*p)).map(|(_, n)| *n).collect()
    }

    /// The set a list of names makes. An unknown or repeated name is refused, so a typo never quietly grants less.
    pub fn from_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<Self, String> {
        let mut set = Self::NONE;
        for name in names {
            let (permission, _) = Self::NAMED.iter().find(|(_, n)| *n == name).ok_or_else(|| {
                format!("unknown permission {name}: permissions are read, trade, transfers, keys and respond")
            })?;
            if set.contains(*permission) {
                return Err(format!("permission {name} is given twice"));
            }
            set = set.with(*permission);
        }
        Ok(set)
    }
}

fn is_lower_hex(text: &str, digits: usize) -> bool {
    text.len() == digits && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `text` is a key ID.
pub fn is_id(text: &str) -> bool {
    text.strip_prefix(ID_PREFIX).is_some_and(|hex| is_lower_hex(hex, ID_DIGITS))
}

/// Whether a credential is meant as a key's rather than a dev token: it starts as a key ID does. A dev token never
/// does, so a malformed key is refused as a key rather than looked up as a token.
pub fn looks_like_key(credential: &str) -> bool {
    credential.starts_with(ID_PREFIX)
}

/// A credential's key ID and secret, if it is `<keyId>.<secret>` in the right form.
pub fn split(credential: &str) -> Option<(&str, &str)> {
    let (id, secret) = credential.split_once('.')?;
    (is_id(id) && is_lower_hex(secret, SECRET_DIGITS)).then_some((id, secret))
}

fn random_hex(bytes: usize) -> io::Result<String> {
    let mut buffer = vec![0u8; bytes];
    File::open("/dev/urandom")?.read_exact(&mut buffer)?;
    Ok(hex(&buffer))
}

/// A fresh key ID.
pub fn new_id() -> io::Result<String> {
    Ok(format!("{ID_PREFIX}{}", random_hex(ID_DIGITS / 2)?))
}

/// A fresh secret.
pub fn new_secret() -> io::Result<String> {
    random_hex(SECRET_DIGITS / 2)
}

/// What is stored in place of a key's secret (decision G-2): a SHA-256 over a version tag, the ID and the secret. The
/// secret carries 256 random bits, so a fast hash is as safe as a slow one and costs a verification nothing.
pub fn hash(id: &str, secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(HASH_TAG.as_bytes());
    hasher.update(id.as_bytes());
    hasher.update(b":");
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

/// Whether two hashes are equal, taking the same time wherever they first differ, so how long a refusal took says
/// nothing about how close a guess was.
pub fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let difference = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(difference) == 0
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 32 bytes from 64 hex digits.
pub fn unhex(text: &str) -> Option<[u8; 32]> {
    if !is_lower_hex(text, 64) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Entries an allow-list may hold at once (decision G-23): far more than a firm's set of egress addresses needs, and
/// bounded for the same reason a key's permissions are a fixed set rather than an open one.
pub const MAX_ALLOWED_IPS: usize = 20;

/// Whether `text` is an address or a network in the form the gateway's `--trusted-proxies` and a key's allow-list
/// both take (`core/gateway/src/forwarded.rs`): `192.0.2.7`, `10.42.0.0/16`, `2001:db8::/32`. Checked here so a
/// malformed entry is refused when the key is made, not silently ignored when the gateway later tries to use it.
pub fn is_ip_or_network(text: &str) -> bool {
    let (address, prefix) = match text.split_once('/') {
        Some((address, prefix)) => (address, Some(prefix)),
        None => (text, None),
    };
    let Ok(base) = address.parse::<std::net::IpAddr>() else { return false };
    let bits = if base.to_canonical().is_ipv4() { 32 } else { 128 };
    match prefix {
        None => true,
        Some(p) => p.parse::<u8>().is_ok_and(|p| p <= bits),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_secrets_and_credentials_have_one_form() {
        let (id, secret) = (new_id().expect("an ID"), new_secret().expect("a secret"));
        assert!(is_id(&id), "{id}");
        assert_eq!((id.len(), secret.len()), (19, 64));
        assert_ne!(new_secret().expect("another"), secret, "two secrets alike");
        let credential = format!("{id}.{secret}");
        assert_eq!(split(&credential), Some((id.as_str(), secret.as_str())));
        assert!(looks_like_key(&credential));
        assert!(!looks_like_key("dev-market-maker"));
        for bad in [
            id.clone(),
            format!("{id}.{}", &secret[1..]),
            format!("{id}.{}", secret.to_uppercase()),
            format!("ak_{}.{secret}", "0".repeat(15)),
            format!("bk_{}.{secret}", "0".repeat(16)),
        ] {
            assert_eq!(split(&bad), None, "{bad}");
        }
    }

    #[test]
    fn a_hash_depends_on_the_id_and_the_secret_and_compares_exactly() {
        let secret = "ab".repeat(32);
        let a = hash("ak_0000000000000001", &secret);
        assert!(same(&a, &hash("ak_0000000000000001", &secret)));
        assert!(!same(&a, &hash("ak_0000000000000002", &secret)), "the ID is part of the hash");
        assert!(!same(&a, &hash("ak_0000000000000001", &"ac".repeat(32))));
        let mut one_bit = a;
        one_bit[31] ^= 1;
        assert!(!same(&a, &one_bit));
        assert_eq!(unhex(&hex(&a)), Some(a));
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn permissions_are_named_sets() {
        let set = Permissions::from_names(["trade", "read"]).expect("valid");
        assert_eq!(set.names(), ["read", "trade"]);
        assert!(set.contains(Permissions::READ) && !set.contains(Permissions::TRANSFERS));
        assert!(Permissions::ALL.contains(set));
        assert!(Permissions::from_names(["read", "read"]).is_err());
        assert!(Permissions::from_names(["admin"]).is_err());
        assert_eq!(Permissions::KEYS.name(), "keys");
        assert_eq!(Permissions::RESPOND.name(), "respond");
        assert_eq!(Permissions::ALL.names(), ["read", "trade", "transfers", "keys", "respond"]);
        assert!(!Permissions::from_names(["read", "trade"]).expect("valid").contains(Permissions::RESPOND));
    }
}
