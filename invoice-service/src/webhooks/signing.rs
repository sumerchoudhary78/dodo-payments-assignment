//! Webhook signatures (decisions.md D4).
//!
//! Header: `Dodo-Signature: t=<unix seconds>,v1=<hex HMAC-SHA256(secret, "<t>.<raw body>")>`
//!
//! The timestamp is inside the signed string, so a captured webhook can't be replayed later
//! with a fresh timestamp. Receivers recompute the HMAC over the raw body bytes, compare in
//! constant time, and reject if |now - t| > 5 minutes.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

pub const HEADER: &str = "Dodo-Signature";

pub fn sign(secret: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("t={timestamp},v1={}", hex::encode(mac.finalize().into_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_changes_if_body_timestamp_or_secret_changes() {
        let base = sign("whsec_a", 1_700_000_000, b"{\"x\":1}");
        assert!(base.starts_with("t=1700000000,v1="));
        assert_ne!(base, sign("whsec_a", 1_700_000_000, b"{\"x\":2}"));
        assert_ne!(base, sign("whsec_a", 1_700_000_001, b"{\"x\":1}"));
        assert_ne!(base, sign("whsec_b", 1_700_000_000, b"{\"x\":1}"));
        assert_eq!(base, sign("whsec_a", 1_700_000_000, b"{\"x\":1}"));
    }
}
