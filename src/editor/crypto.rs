//! Primitives for the editor's second factor and sessions, all on `ring`:
//! RFC 4648 base32, RFC 4226/6238 HOTP/TOTP (HMAC-SHA1), AES-256-GCM for TOTP
//! secrets at rest, SHA-256, and random tokens.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::rand::{SecureRandom, SystemRandom};

pub const TOTP_PERIOD: u64 = 30;
pub const TOTP_DIGITS: u32 = 6;

const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(5) * 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            out.push(B32[((buf >> (bits - 5)) & 31) as usize] as char);
            bits -= 5;
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Accepts lower case, spaces and `=` padding, as authenticator apps display them.
pub fn base32_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in text.chars().filter(|c| !c.is_whitespace() && *c != '=') {
        let v = B32
            .iter()
            .position(|&x| x as char == c.to_ascii_uppercase())? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            out.push(((buf >> (bits - 8)) & 0xff) as u8);
            bits -= 8;
        }
    }
    Some(out)
}

pub fn hotp(secret: &[u8], counter: u64, digits: u32) -> u32 {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, secret);
    let tag = ring::hmac::sign(&key, &counter.to_be_bytes());
    let h = tag.as_ref();
    let offset = (h[h.len() - 1] & 0x0f) as usize;
    let bin = ((h[offset] as u32 & 0x7f) << 24)
        | ((h[offset + 1] as u32) << 16)
        | ((h[offset + 2] as u32) << 8)
        | h[offset + 3] as u32;
    bin % 10u32.pow(digits)
}

#[derive(Debug, PartialEq, Eq)]
pub enum TotpCheck {
    /// The code matched this time step, which is newer than the last one used.
    Valid(i64),
    /// The code matched a step already consumed: a replay.
    Reused,
    Invalid,
}

/// Verify a 6-digit code within ±1 step of `now_unix`, refusing any step at or
/// before `last_step`.
pub fn totp_check(secret: &[u8], code: &str, now_unix: u64, last_step: Option<i64>) -> TotpCheck {
    let code = code.trim();
    if code.len() != TOTP_DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return TotpCheck::Invalid;
    }
    let Ok(given) = code.parse::<u32>() else {
        return TotpCheck::Invalid;
    };
    let current = (now_unix / TOTP_PERIOD) as i64;
    let mut reused = false;
    for step in [current - 1, current, current + 1] {
        if step < 0 {
            continue;
        }
        if hotp(secret, step as u64, TOTP_DIGITS) == given {
            if last_step.is_some_and(|last| step <= last) {
                reused = true;
                continue;
            }
            return TotpCheck::Valid(step);
        }
    }
    if reused {
        TotpCheck::Reused
    } else {
        TotpCheck::Invalid
    }
}

pub fn totp_now(secret: &[u8], now_unix: u64) -> String {
    format!("{:06}", hotp(secret, now_unix / TOTP_PERIOD, TOTP_DIGITS))
}

pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    SystemRandom::new()
        .fill(&mut buf)
        .expect("system randomness is available");
    buf
}

pub fn random_token() -> String {
    URL_SAFE_NO_PAD.encode(random_bytes(32))
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .to_vec()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

/// AES-256-GCM with a random nonce; output is `nonce || ciphertext || tag`.
/// `aad` binds the ciphertext to its owner so a secret cannot be moved between
/// users by copying a column value.
pub struct SecretBox {
    key: LessSafeKey,
}

impl SecretBox {
    pub fn from_base64(key_b64: &str) -> Result<Self, String> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(key_b64.trim())
            .map_err(|_| "gtfs_editor_totp_key is not valid base64".to_string())?;
        Self::new(&raw)
    }

    pub fn new(raw: &[u8]) -> Result<Self, String> {
        if raw.len() != 32 {
            return Err("gtfs_editor_totp_key must decode to 32 bytes".to_string());
        }
        let key = UnboundKey::new(&AES_256_GCM, raw).map_err(|_| "invalid AES key".to_string())?;
        Ok(Self {
            key: LessSafeKey::new(key),
        })
    }

    pub fn seal(&self, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
        let nonce_bytes = random_bytes(NONCE_LEN);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&nonce_bytes);
        let mut in_out = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .expect("AES-GCM seal cannot fail for in-memory buffers");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&in_out);
        out
    }

    pub fn open(&self, sealed: &[u8], aad: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < NONCE_LEN + AES_256_GCM.tag_len() {
            return None;
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&sealed[..NONCE_LEN]);
        let mut in_out = sealed[NONCE_LEN..].to_vec();
        let plain = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut in_out,
            )
            .ok()?;
        Some(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 6238 appendix B, SHA-1, 8 digits.
    #[test]
    fn rfc6238_sha1_vectors() {
        let secret = b"12345678901234567890";
        for (t, want) in [
            (59u64, 94287082u32),
            (1111111109, 7081804),
            (1111111111, 14050471),
            (1234567890, 89005924),
            (2000000000, 69279037),
            (20000000000, 65353130),
        ] {
            assert_eq!(hotp(secret, t / 30, 8), want, "t={t}");
        }
    }

    /// RFC 4226 appendix D.
    #[test]
    fn rfc4226_hotp_vectors() {
        let secret = b"12345678901234567890";
        let want = [
            755224, 287082, 359152, 969429, 338314, 254676, 287922, 162583, 399871, 520489,
        ];
        for (counter, code) in want.iter().enumerate() {
            assert_eq!(hotp(secret, counter as u64, 6), *code);
        }
    }

    #[test]
    fn totp_window_and_replay() {
        let secret = b"12345678901234567890";
        let now = 1_700_000_000u64;
        let step = (now / 30) as i64;
        let code = totp_now(secret, now);
        assert_eq!(totp_check(secret, &code, now, None), TotpCheck::Valid(step));
        // the same code again is a replay
        assert_eq!(
            totp_check(secret, &code, now, Some(step)),
            TotpCheck::Reused
        );
        // previous step still accepted once
        let prev = totp_now(secret, now - 30);
        assert_eq!(
            totp_check(secret, &prev, now, Some(step - 2)),
            TotpCheck::Valid(step - 1)
        );
        // two steps away is outside the window
        let old = totp_now(secret, now - 90);
        assert_eq!(totp_check(secret, &old, now, None), TotpCheck::Invalid);
        assert_eq!(totp_check(secret, "12ab56", now, None), TotpCheck::Invalid);
        assert_eq!(totp_check(secret, "1234567", now, None), TotpCheck::Invalid);
    }

    #[test]
    fn base32_round_trip_and_rfc4648_vectors() {
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_encode(b"f"), "MY");
        assert_eq!(base32_decode("mzxw 6ytb oi======").unwrap(), b"foobar");
        let raw = random_bytes(20);
        assert_eq!(base32_decode(&base32_encode(&raw)).unwrap(), raw);
        assert!(base32_decode("not base32!").is_none());
    }

    #[test]
    fn secret_box_round_trip_and_tamper() {
        let key = random_bytes(32);
        let sb = SecretBox::new(&key).unwrap();
        let sealed = sb.seal(b"totp secret", b"user-1");
        assert_eq!(sb.open(&sealed, b"user-1").unwrap(), b"totp secret");
        assert!(sb.open(&sealed, b"user-2").is_none(), "aad binds the owner");
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(sb.open(&tampered, b"user-1").is_none());
        assert!(SecretBox::new(&key[..16]).is_err());
        let other = SecretBox::new(&random_bytes(32)).unwrap();
        assert!(other.open(&sealed, b"user-1").is_none());
    }
}
