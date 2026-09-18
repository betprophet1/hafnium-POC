//! The six-digit codes an authenticator app shows (RFC 6238, over RFC 4226's HOTP), and the base32 an app reads its
//! secret in (RFC 4648).
//!
//! HMAC-SHA-1 is not a choice: it is what every authenticator app computes, and the alternatives are not what the
//! `otpauth://` URI's readers implement. The secret is 160 bits, which is what RFC 4226 asks for, and it is the only
//! thing here worth keeping secret — a code is worth one step of thirty seconds and is then refused as used.

use hmac::{Hmac, KeyInit, Mac};
use sha1::Sha1;

/// Seconds a code lasts. Every authenticator app uses this.
pub const STEP_SECONDS: u64 = 30;
/// How many steps either side of now are accepted, for clocks that disagree: one, so a code is good for at most a
/// minute and a half.
pub const DRIFT_STEPS: i64 = 1;
/// Digits in a code.
const DIGITS: u32 = 6;

/// Which step of the clock a moment falls in. The step, not the time, is what a code is made from and what is
/// remembered, so the same code cannot be used twice.
pub const fn step_at(unix_seconds: u64) -> u64 {
    unix_seconds / STEP_SECONDS
}

/// The code for a step, as six digits with leading zeros.
pub fn code(secret: &[u8], step: u64) -> String {
    let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(secret).expect("HMAC takes a key of any length");
    mac.update(&step.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    // RFC 4226 §5.3: the low nibble of the last byte says where to read four bytes from.
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let binary =
        u32::from_be_bytes([digest[offset], digest[offset + 1], digest[offset + 2], digest[offset + 3]]) & 0x7fff_ffff;
    format!("{:0width$}", binary % 10_u32.pow(DIGITS), width = DIGITS as usize)
}

/// Whether `given` is a code for `secret` at or near `step`, and which step it was. `after` is the last step already
/// used: a code from it or earlier is refused, so one code cannot be used twice.
pub fn matches(secret: &[u8], given: &str, step: u64, after: Option<u64>) -> Option<u64> {
    let given = given.trim().replace(' ', "");
    if given.len() != DIGITS as usize || !given.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    for drift in -DRIFT_STEPS..=DRIFT_STEPS {
        let Some(candidate) = step.checked_add_signed(drift) else { continue };
        if after.is_some_and(|used| candidate <= used) {
            continue;
        }
        // Compared without giving away where they first differ.
        if same(code(secret, candidate).as_bytes(), given.as_bytes()) {
            return Some(candidate);
        }
    }
    None
}

fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |seen, (x, y)| seen | (x ^ y)) == 0
}

/// A secret as an authenticator app reads it: base32 without padding (RFC 4648), which is what the `otpauth://` URI
/// carries and what a person types when a camera will not read the code.
pub fn base32(secret: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::new();
    let (mut buffer, mut bits) = (0u16, 0u32);
    for byte in secret {
        buffer = (buffer << 8) | u16::from(*byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 0x1f) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 0x1f) as usize] as char);
    }
    out
}

/// Base32 read back, which is what an authenticator app does with what it was given. Nothing in the service needs
/// it — a secret is only ever written out — but a test that plays the part of the app does, and a decoder beside its
/// encoder is what keeps the two honest.
pub fn from_base32(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buffer, mut bits) = (0u16, 0u32);
    for c in text.chars().filter(|c| *c != '=') {
        let value = match c {
            'A'..='Z' => c as u16 - 'A' as u16,
            'a'..='z' => c as u16 - 'a' as u16,
            '2'..='7' => c as u16 - '2' as u16 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}

/// The URI an authenticator app takes, by camera or by hand.
pub fn uri(issuer: &str, account: &str, secret: &[u8]) -> String {
    let escape = |text: &str| text.replace('%', "%25").replace(' ', "%20").replace(':', "%3A");
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits={DIGITS}&period={STEP_SECONDS}",
        escape(issuer),
        escape(account),
        base32(secret),
        escape(issuer)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238's own test vectors, for the SHA-1 secret "12345678901234567890". If these pass, an authenticator app
    /// and this agree, which is the only thing that matters here.
    #[test]
    fn the_rfc_6238_vectors() {
        let secret = b"12345678901234567890";
        for (seconds, expected) in [
            (59u64, "287082"),
            (1_111_111_109, "081804"),
            (1_111_111_111, "050471"),
            (1_234_567_890, "005924"),
            (2_000_000_000, "279037"),
        ] {
            assert_eq!(code(secret, step_at(seconds)), expected, "at {seconds}");
        }
    }

    #[test]
    fn a_code_is_taken_once_and_only_near_its_own_time() {
        let secret = b"12345678901234567890";
        let step = step_at(1_111_111_111);
        let now = code(secret, step);
        assert_eq!(matches(secret, &now, step, None), Some(step));
        // Spaces as an app shows them, and either side of the step for a clock that disagrees.
        assert_eq!(matches(secret, &format!("{} {}", &now[..3], &now[3..]), step, None), Some(step));
        assert_eq!(matches(secret, &code(secret, step - 1), step, None), Some(step - 1));
        assert_eq!(matches(secret, &code(secret, step + 1), step, None), Some(step + 1));
        // Further away, and it is nobody's code.
        assert_eq!(matches(secret, &code(secret, step + 5), step, None), None);
        // Used once: the same code does not work again.
        assert_eq!(matches(secret, &now, step, Some(step)), None);
        // Nonsense is refused without pretending to compare it.
        assert_eq!(matches(secret, "12345", step, None), None);
        assert_eq!(matches(secret, "abcdef", step, None), None);
    }

    #[test]
    fn a_secret_is_written_as_an_app_reads_it() {
        // RFC 4648's vectors.
        assert_eq!(base32(b""), "");
        assert_eq!(base32(b"f"), "MY");
        assert_eq!(base32(b"fo"), "MZXQ");
        assert_eq!(base32(b"foobar"), "MZXW6YTBOI");
        let uri = uri("Venue", "someone@example.com", b"12345678901234567890");
        assert!(
            uri.starts_with("otpauth://totp/Venue:someone@example.com?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ"),
            "{uri}"
        );
        assert!(uri.contains("algorithm=SHA1&digits=6&period=30"), "{uri}");
    }
}
