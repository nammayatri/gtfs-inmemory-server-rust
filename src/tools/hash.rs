use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Produces a stable HMAC-SHA256 hex digest for a phone number using the configured secret key.
/// Used to store and compare phone numbers without ever persisting plaintext values.
pub fn hash_phone_number(phone: &str, key: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(key.as_bytes()).expect("HMAC accepts keys of any size");
    mac.update(phone.trim().as_bytes());
    hex::encode(mac.finalize().into_bytes())
}
