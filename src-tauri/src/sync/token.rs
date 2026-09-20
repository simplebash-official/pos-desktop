// Short-lived service token the sync agent presents to the LOCAL backend. The
// backend's `SyncAgent` extractor accepts HS256 tokens signed with its own
// `JWT_SECRET` (which the shell already owns via `config.json`) whose claims
// carry `scope: "sync"`. A plain HS256 JWT is a few lines, so it is built here
// instead of pulling in a JWT crate and its crypto backend.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64URL, Engine};
use hmac::{Hmac, Mac};
use serde_json::json;
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Lifetime of a minted token. The contract allows at most 5 minutes.
pub const TOKEN_TTL_SECS: i64 = 240;

pub fn mint_service_token(secret: &str, now_unix: i64) -> String {
    let header = B64URL.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let claims = json!({
        "sub": "sync-agent",
        "scope": "sync",
        "iat": now_unix,
        "exp": now_unix + TOKEN_TTL_SECS,
    });
    let payload = B64URL.encode(claims.to_string());
    let signing_input = format!("{header}.{payload}");
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length");
    mac.update(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        B64URL.encode(mac.finalize().into_bytes())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parts(token: &str) -> (String, Value, String) {
        let mut it = token.split('.');
        let header = it.next().unwrap().to_string();
        let payload = it.next().unwrap();
        let sig = it.next().unwrap().to_string();
        let claims: Value =
            serde_json::from_slice(&B64URL.decode(payload).unwrap()).expect("claims are JSON");
        (header, claims, sig)
    }

    #[test]
    fn token_carries_the_sync_scope_and_a_short_expiry() {
        let (_, claims, _) = parts(&mint_service_token("secret", 1_000));
        assert_eq!(claims["sub"], "sync-agent");
        assert_eq!(claims["scope"], "sync");
        let ttl = claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap();
        assert!(ttl > 0 && ttl <= 300, "ttl {ttl}");
    }

    #[test]
    fn signature_verifies_with_the_secret_and_not_with_another() {
        let token = mint_service_token("right-secret", 5);
        let (header, _, sig) = parts(&token);
        let payload = token.split('.').nth(1).unwrap();
        let expected = |secret: &str| {
            let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
            mac.update(format!("{header}.{payload}").as_bytes());
            B64URL.encode(mac.finalize().into_bytes())
        };
        assert_eq!(sig, expected("right-secret"));
        assert_ne!(sig, expected("wrong-secret"));
        assert_eq!(
            B64URL.decode(&header).unwrap(),
            br#"{"alg":"HS256","typ":"JWT"}"#
        );
    }
}
