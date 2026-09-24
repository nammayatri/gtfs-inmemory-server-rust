//! Verification of the `X-Pomerium-Jwt-Assertion` header.
//!
//! Pomerium signs an ES256 JWT for every request it forwards. The editor trusts
//! the email in it only after checking the signature against Pomerium's JWKS,
//! the audience (the dashboard host) and expiry. `alg` must be ES256 - `none`,
//! HS256 and anything else is rejected before a key is even looked up.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// Clock skew tolerated on exp / nbf / iat.
const LEEWAY_SECS: i64 = 30;
/// An unknown kid may trigger a JWKS refetch at most this often.
const MIN_REFRESH: Duration = Duration::from_secs(30);
/// Keys are refetched in the background of a request at least this often.
const MAX_AGE: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub email: String,
    pub subject: Option<String>,
    pub expires_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum JwtError {
    Malformed(&'static str),
    UnsupportedAlg,
    UnknownKey,
    BadSignature,
    Expired,
    NotYetValid,
    WrongAudience,
    NoEmail,
}

impl JwtError {
    pub fn code(&self) -> &'static str {
        match self {
            JwtError::Malformed(_) => "jwt_malformed",
            JwtError::UnsupportedAlg => "jwt_unsupported_alg",
            JwtError::UnknownKey => "jwt_unknown_key",
            JwtError::BadSignature => "jwt_bad_signature",
            JwtError::Expired => "jwt_expired",
            JwtError::NotYetValid => "jwt_not_yet_valid",
            JwtError::WrongAudience => "jwt_wrong_audience",
            JwtError::NoEmail => "jwt_no_email",
        }
    }
}

#[derive(Deserialize)]
struct Header {
    alg: String,
    kid: Option<String>,
}

pub struct ParsedToken<'a> {
    pub kid: Option<String>,
    signing_input: &'a str,
    signature: Vec<u8>,
    claims: Value,
}

/// Split and decode, checking only structure and `alg`. No trust yet.
pub fn parse(token: &str) -> Result<ParsedToken<'_>, JwtError> {
    let token = token.trim();
    let mut parts = token.splitn(3, '.');
    let (h, p, s) = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => (h, p, s),
        _ => return Err(JwtError::Malformed("expected three segments")),
    };
    let header: Header = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(h)
            .map_err(|_| JwtError::Malformed("header is not base64url"))?,
    )
    .map_err(|_| JwtError::Malformed("header is not JSON"))?;
    if header.alg != "ES256" {
        return Err(JwtError::UnsupportedAlg);
    }
    let claims: Value = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(p)
            .map_err(|_| JwtError::Malformed("payload is not base64url"))?,
    )
    .map_err(|_| JwtError::Malformed("payload is not JSON"))?;
    let signature = URL_SAFE_NO_PAD
        .decode(s)
        .map_err(|_| JwtError::Malformed("signature is not base64url"))?;
    if signature.len() != 64 {
        return Err(JwtError::BadSignature);
    }
    Ok(ParsedToken {
        kid: header.kid,
        signing_input: &token[..h.len() + 1 + p.len()],
        signature,
        claims,
    })
}

/// RFC 7519 NumericDate: "a JSON numeric value representing the number of
/// seconds" - not necessarily an integer. Pomerium emits `1.790144277e+09`,
/// which `Value::as_i64` rejects outright, so an exp read with it comes back
/// absent and a perfectly valid token is reported expired.
fn numeric_date(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

/// Verify with a known public key (uncompressed SEC1 point, 65 bytes).
pub fn verify(
    parsed: &ParsedToken<'_>,
    public_key: &[u8],
    audience: &str,
    now_unix: i64,
) -> Result<Claims, JwtError> {
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, public_key)
        .verify(parsed.signing_input.as_bytes(), &parsed.signature)
        .map_err(|_| JwtError::BadSignature)?;

    let c = &parsed.claims;
    let exp = c
        .get("exp")
        .and_then(numeric_date)
        .ok_or(JwtError::Expired)?;
    if exp + LEEWAY_SECS < now_unix {
        return Err(JwtError::Expired);
    }
    if let Some(nbf) = c.get("nbf").and_then(numeric_date) {
        if nbf - LEEWAY_SECS > now_unix {
            return Err(JwtError::NotYetValid);
        }
    }
    if let Some(iat) = c.get("iat").and_then(numeric_date) {
        if iat - LEEWAY_SECS > now_unix {
            return Err(JwtError::NotYetValid);
        }
    }
    let aud_ok = match c.get("aud") {
        Some(Value::String(a)) => a == audience,
        Some(Value::Array(list)) => list.iter().any(|a| a.as_str() == Some(audience)),
        _ => false,
    };
    if !aud_ok {
        return Err(JwtError::WrongAudience);
    }
    let email = c
        .get("email")
        .and_then(Value::as_str)
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.contains('@'))
        .ok_or(JwtError::NoEmail)?;
    Ok(Claims {
        email,
        subject: c.get("sub").and_then(Value::as_str).map(str::to_string),
        expires_at: exp,
    })
}

#[derive(Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    kty: String,
    crv: Option<String>,
    x: Option<String>,
    y: Option<String>,
    kid: Option<String>,
}

/// Parse a JWKS document into `kid -> uncompressed P-256 point`. Keys that are
/// not EC P-256 are ignored; a key without a kid is stored under "".
pub fn parse_jwks(doc: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    let jwks: Jwks = serde_json::from_slice(doc).map_err(|e| format!("JWKS is not valid: {e}"))?;
    let mut out = HashMap::new();
    for k in jwks.keys {
        if k.kty != "EC" || k.crv.as_deref() != Some("P-256") {
            continue;
        }
        let (Some(x), Some(y)) = (k.x, k.y) else {
            continue;
        };
        let (Ok(x), Ok(y)) = (URL_SAFE_NO_PAD.decode(x), URL_SAFE_NO_PAD.decode(y)) else {
            continue;
        };
        if x.len() != 32 || y.len() != 32 {
            continue;
        }
        let mut point = vec![0x04];
        point.extend_from_slice(&x);
        point.extend_from_slice(&y);
        out.insert(k.kid.unwrap_or_default(), point);
    }
    if out.is_empty() {
        return Err("JWKS holds no EC P-256 key".to_string());
    }
    Ok(out)
}

pub struct JwksCache {
    url: String,
    keys: RwLock<HashMap<String, Vec<u8>>>,
    fetched_at: Mutex<Option<Instant>>,
    http: reqwest::Client,
}

impl JwksCache {
    pub fn new(url: String) -> Self {
        Self {
            url,
            keys: RwLock::new(HashMap::new()),
            fetched_at: Mutex::new(None),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn fetch(&self) -> Result<HashMap<String, Vec<u8>>, String> {
        let body = if let Some(path) = self.url.strip_prefix("file://") {
            tokio::fs::read(path)
                .await
                .map_err(|e| format!("reading JWKS file failed: {e}"))?
        } else {
            let resp = self
                .http
                .get(&self.url)
                .send()
                .await
                .map_err(|e| format!("fetching JWKS failed: {e}"))?;
            if !resp.status().is_success() {
                return Err(format!("fetching JWKS returned {}", resp.status()));
            }
            resp.bytes()
                .await
                .map_err(|e| format!("reading JWKS failed: {e}"))?
                .to_vec()
        };
        parse_jwks(&body)
    }

    /// Refetch unless the last fetch was under `MIN_REFRESH` ago.
    async fn refresh(&self, force_age: Duration) {
        let mut fetched_at = self.fetched_at.lock().await;
        if fetched_at.is_some_and(|t| t.elapsed() < force_age) {
            return;
        }
        match self.fetch().await {
            Ok(keys) => {
                *self.keys.write().await = keys;
                *fetched_at = Some(Instant::now());
            }
            Err(e) => {
                // Keep the old keys; remember the attempt so a flood of bad kids
                // cannot turn into a flood of fetches.
                tracing::error!(tag = "[GTFS EDITOR JWKS]", error = %e);
                *fetched_at = Some(Instant::now());
            }
        }
    }

    pub async fn key_for(&self, kid: Option<&str>) -> Option<Vec<u8>> {
        self.refresh(MAX_AGE).await;
        if let Some(k) = self.lookup(kid).await {
            return Some(k);
        }
        self.refresh(MIN_REFRESH).await;
        self.lookup(kid).await
    }

    async fn lookup(&self, kid: Option<&str>) -> Option<Vec<u8>> {
        let keys = self.keys.read().await;
        match kid {
            Some(kid) => keys.get(kid).cloned(),
            None if keys.len() == 1 => keys.values().next().cloned(),
            None => None,
        }
    }

    pub async fn verify(&self, token: &str, audience: &str) -> Result<Claims, JwtError> {
        let parsed = parse(token)?;
        let key = self
            .key_for(parsed.kid.as_deref())
            .await
            .ok_or(JwtError::UnknownKey)?;
        verify(&parsed, &key, audience, chrono::Utc::now().timestamp())
    }
}

/// Test support: an ES256 signer and its JWKS, so tests exercise the real
/// verification path rather than a bypass.
pub mod testing {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_FIXED_SIGNING};

    pub struct TestSigner {
        pair: EcdsaKeyPair,
        pub kid: String,
    }

    impl TestSigner {
        pub fn generate(kid: &str) -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
                .expect("key generation");
            let pair =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
                    .expect("key parse");
            Self {
                pair,
                kid: kid.to_string(),
            }
        }

        pub fn public_point(&self) -> Vec<u8> {
            self.pair.public_key().as_ref().to_vec()
        }

        pub fn jwks(&self) -> String {
            let p = self.public_point();
            serde_json::json!({"keys": [{
                "kty": "EC", "crv": "P-256", "alg": "ES256", "use": "sig", "kid": self.kid,
                "x": URL_SAFE_NO_PAD.encode(&p[1..33]),
                "y": URL_SAFE_NO_PAD.encode(&p[33..65]),
            }]})
            .to_string()
        }

        pub fn sign_with_header(&self, header: Value, claims: Value) -> String {
            let input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header.to_string()),
                URL_SAFE_NO_PAD.encode(claims.to_string())
            );
            let sig = self
                .pair
                .sign(&SystemRandom::new(), input.as_bytes())
                .expect("signing");
            format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
        }

        pub fn sign(&self, claims: Value) -> String {
            self.sign_with_header(
                serde_json::json!({"alg": "ES256", "typ": "JWT", "kid": self.kid}),
                claims,
            )
        }

        pub fn token_for(&self, email: &str, audience: &str, ttl_secs: i64) -> String {
            let now = chrono::Utc::now().timestamp();
            self.sign(serde_json::json!({
                "iss": "pomerium-test", "aud": audience, "email": email, "sub": email,
                "iat": now, "exp": now + ttl_secs,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::TestSigner;
    use super::*;
    use serde_json::json;

    const AUD: &str = "gtfs.sso.example.test";

    #[test]
    fn accepts_a_valid_token() {
        let s = TestSigner::generate("k1");
        let now = chrono::Utc::now().timestamp();
        let tok = s.token_for("Ops@NammaYatri.in", AUD, 300);
        let parsed = parse(&tok).unwrap();
        assert_eq!(parsed.kid.as_deref(), Some("k1"));
        let claims = verify(&parsed, &s.public_point(), AUD, now).unwrap();
        assert_eq!(claims.email, "ops@nammayatri.in");
        // aud as an array
        let tok = s.sign(json!({"aud": ["other", AUD], "email": "a@b.c", "exp": now + 60}));
        assert!(verify(&parse(&tok).unwrap(), &s.public_point(), AUD, now).is_ok());
        // JWKS parse yields the same key
        let keys = parse_jwks(s.jwks().as_bytes()).unwrap();
        assert_eq!(keys.get("k1").unwrap(), &s.public_point());
    }

    /// Pomerium 0.17 writes NumericDate claims as JSON floats in scientific
    /// notation (`"exp": 1.790144277e+09`). RFC 7519 allows that - `exp` is "a
    /// JSON numeric value", not an integer - and `Value::as_i64` returns None
    /// for it, which read as a missing exp and reported every live token as
    /// expired. Real values from the prod assertion that exposed this.
    #[test]
    fn accepts_float_numeric_dates_as_pomerium_sends_them() {
        let s = TestSigner::generate("k1");
        let now: i64 = 1_790_143_996;
        let tok = s.sign(json!({
            "aud": AUD,
            "email": "ops@nammayatri.in",
            "iat": 1.790143954e+09,
            "exp": 1.790144277e+09,
        }));
        let claims = verify(&parse(&tok).unwrap(), &s.public_point(), AUD, now)
            .expect("a float exp that is still in the future must verify");
        assert_eq!(claims.email, "ops@nammayatri.in");

        // a float exp in the past is still rejected - the fix must not swallow expiry
        let stale = s.sign(json!({"aud": AUD, "email": "a@b.c", "exp": 1.790143000e+09}));
        assert_eq!(
            verify(&parse(&stale).unwrap(), &s.public_point(), AUD, now),
            Err(JwtError::Expired)
        );

        // a float nbf in the future is still rejected
        let early = s.sign(json!({"aud": AUD, "email": "a@b.c",
                                  "exp": 1.790144277e+09, "nbf": 1.790144200e+09}));
        assert_eq!(
            verify(&parse(&early).unwrap(), &s.public_point(), AUD, now),
            Err(JwtError::NotYetValid)
        );
    }

    #[test]
    fn rejects_bad_signature_wrong_key_and_tampering() {
        let s = TestSigner::generate("k1");
        let other = TestSigner::generate("k1");
        let now = chrono::Utc::now().timestamp();
        let tok = s.token_for("a@b.c", AUD, 300);
        assert_eq!(
            verify(&parse(&tok).unwrap(), &other.public_point(), AUD, now),
            Err(JwtError::BadSignature)
        );
        // swap in a payload with another email, keeping the signature
        let parts: Vec<&str> = tok.split('.').collect();
        let forged_payload = URL_SAFE_NO_PAD
            .encode(json!({"aud": AUD, "email": "admin@x.y", "exp": now + 300}).to_string());
        let forged = format!("{}.{}.{}", parts[0], forged_payload, parts[2]);
        assert_eq!(
            verify(&parse(&forged).unwrap(), &s.public_point(), AUD, now),
            Err(JwtError::BadSignature)
        );
    }

    #[test]
    fn rejects_wrong_audience_expired_and_missing_email() {
        let s = TestSigner::generate("k1");
        let now = chrono::Utc::now().timestamp();
        let key = s.public_point();
        let wrong_aud = s.token_for("a@b.c", "gims.sso.example.test", 300);
        assert_eq!(
            verify(&parse(&wrong_aud).unwrap(), &key, AUD, now),
            Err(JwtError::WrongAudience)
        );
        let expired = s.sign(json!({"aud": AUD, "email": "a@b.c", "exp": now - 120}));
        assert_eq!(
            verify(&parse(&expired).unwrap(), &key, AUD, now),
            Err(JwtError::Expired)
        );
        let no_exp = s.sign(json!({"aud": AUD, "email": "a@b.c"}));
        assert_eq!(
            verify(&parse(&no_exp).unwrap(), &key, AUD, now),
            Err(JwtError::Expired)
        );
        let future =
            s.sign(json!({"aud": AUD, "email": "a@b.c", "exp": now + 900, "nbf": now + 600}));
        assert_eq!(
            verify(&parse(&future).unwrap(), &key, AUD, now),
            Err(JwtError::NotYetValid)
        );
        let no_email = s.sign(json!({"aud": AUD, "exp": now + 300}));
        assert_eq!(
            verify(&parse(&no_email).unwrap(), &key, AUD, now),
            Err(JwtError::NoEmail)
        );
    }

    #[test]
    fn rejects_other_algorithms_and_garbage() {
        let s = TestSigner::generate("k1");
        let none = s.sign_with_header(json!({"alg": "none"}), json!({"email": "a@b.c"}));
        assert!(matches!(parse(&none), Err(JwtError::UnsupportedAlg)));
        let hs = s.sign_with_header(json!({"alg": "HS256"}), json!({"email": "a@b.c"}));
        assert!(matches!(parse(&hs), Err(JwtError::UnsupportedAlg)));
        assert!(matches!(parse("abc"), Err(JwtError::Malformed(_))));
        assert!(matches!(parse("a.b."), Err(JwtError::Malformed(_))));
        assert!(parse_jwks(br#"{"keys": [{"kty": "RSA", "n": "x", "e": "AQAB"}]}"#).is_err());
    }

    #[tokio::test]
    async fn cache_reads_file_jwks_and_verifies() {
        let s = TestSigner::generate("file-key");
        let dir =
            std::env::temp_dir().join(format!("jwks-{}", crate::editor::crypto::random_token()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("jwks.json");
        std::fs::write(&path, s.jwks()).unwrap();
        let cache = JwksCache::new(format!("file://{}", path.display()));
        let tok = s.token_for("ops@x.y", AUD, 300);
        assert_eq!(cache.verify(&tok, AUD).await.unwrap().email, "ops@x.y");
        let stranger = TestSigner::generate("unknown-kid");
        let tok = stranger.token_for("ops@x.y", AUD, 300);
        assert_eq!(cache.verify(&tok, AUD).await, Err(JwtError::UnknownKey));
        std::fs::remove_dir_all(dir).ok();
    }
}
