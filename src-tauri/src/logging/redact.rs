// Secret masking applied to every record before it reaches disk. Producers
// redact at the source too; this second pass is the backstop so a producer
// bug can never persist a password or token. Customer data is kept on
// purpose (shop owner's decision) — only credentials/secrets are masked.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

pub const REDACTED: &str = "[REDACTED]";

/// Object keys whose value is always masked, matched case-insensitively
/// anywhere in the key (`adminPassword`, `x-internal-api-key`, `JWT_SECRET`).
fn sensitive_key_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(password|passwd|pwd|secret|token|authorization|cookie|api[_-]?key|jwt|cvv|card_?number|otp|proof|\bpin\b|^pin$|_pin$|pin_code|pincode|device[_-]?code|^payload$|^changes$|^records?$)",
        )
        .unwrap()
    })
}

/// Secret-shaped substrings masked inside free text: bearer/JWT tokens and
/// 64-hex secrets (the shape of the generated `jwt_secret` / API key).
fn secret_value_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(bearer\s+[A-Za-z0-9\-_\.=]+|eyJ[A-Za-z0-9\-_=]+\.[A-Za-z0-9\-_=]+\.[A-Za-z0-9\-_.+/=]*|\b[a-f0-9]{64}\b)",
        )
        .unwrap()
    })
}

pub fn is_sensitive_key(key: &str) -> bool {
    sensitive_key_re().is_match(key)
}

pub fn redact_text(text: &str) -> String {
    if !secret_value_re().is_match(text) {
        return text.to_string();
    }
    secret_value_re().replace_all(text, REDACTED).into_owned()
}

/// Recursively mask sensitive keys and secret-shaped strings in place.
pub fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if is_sensitive_key(key) && !v.is_null() && !is_redacted(v) {
                    // Whole value goes, nested objects included (`tokens: {...}`).
                    *v = Value::String(REDACTED.into());
                } else {
                    redact_value(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_value),
        Value::String(s) if secret_value_re().is_match(s) => *s = redact_text(s),
        _ => {}
    }
}

fn is_redacted(v: &Value) -> bool {
    v.as_str() == Some(REDACTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn masks_cloud_link_credentials() {
        let mut v = json!({
            "userCode": "ABCD-EFGH",
            "deviceCode": "dc_secret",
            "accessToken": "a",
            "refreshToken": "r"
        });
        redact_value(&mut v);
        assert_eq!(v["deviceCode"], REDACTED);
        assert_eq!(v["accessToken"], REDACTED);
        assert_eq!(v["refreshToken"], REDACTED);
        assert_eq!(v["userCode"], "ABCD-EFGH");
    }

    #[test]
    fn masks_the_phone_proof_and_otp_fields_but_not_the_masked_number() {
        let mut v = json!({
            "phoneProof": "ovp_abc",
            "otpId": "otp_1",
            "otpCode": "042817",
            "phone": "***4567"
        });
        redact_value(&mut v);
        assert_eq!(v["phoneProof"], REDACTED);
        assert_eq!(v["otpId"], REDACTED);
        assert_eq!(v["otpCode"], REDACTED);
        assert_eq!(v["phone"], "***4567");
    }

    #[test]
    fn masks_sync_record_bodies_but_not_counters() {
        let mut v = json!({
            "payload": {"customerName": "A. Perera"},
            "changes": [{"key": "cust_1"}],
            "record": {"key": "prod_1"},
            "pushed": 12,
            "pendingOut": 3,
            "serviceToken": "abc"
        });
        redact_value(&mut v);
        assert_eq!(v["payload"], REDACTED);
        assert_eq!(v["changes"], REDACTED);
        assert_eq!(v["record"], REDACTED);
        assert_eq!(v["serviceToken"], REDACTED);
        assert_eq!(v["pushed"], 12);
        assert_eq!(v["pendingOut"], 3);
    }

    #[test]
    fn masks_sensitive_keys_at_any_depth() {
        let mut v = json!({
            "email": "owner@shop.lk",
            "adminPassword": "hunter2",
            "headers": {"Authorization": "Bearer abc", "X-Internal-Api-Key": "k"},
            "items": [{"card_number": "4111", "qty": 2}],
            "env": {"JWT_SECRET": "s"}
        });
        redact_value(&mut v);
        assert_eq!(v["email"], "owner@shop.lk");
        assert_eq!(v["adminPassword"], REDACTED);
        assert_eq!(v["headers"]["Authorization"], REDACTED);
        assert_eq!(v["headers"]["X-Internal-Api-Key"], REDACTED);
        assert_eq!(v["items"][0]["card_number"], REDACTED);
        assert_eq!(v["items"][0]["qty"], 2);
        assert_eq!(v["env"]["JWT_SECRET"], REDACTED);
    }

    #[test]
    fn keeps_customer_data_and_innocent_keys() {
        let mut v =
            json!({"phone": "0771234567", "address": "Kandy", "shipping": "x", "spinner": true});
        let before = v.clone();
        redact_value(&mut v);
        assert_eq!(v, before);
    }

    #[test]
    fn masks_secret_shaped_text() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ1In0.sig_abc";
        let hex = "a".repeat(64);
        let out = redact_text(&format!("token {jwt} and {hex} and Bearer xyz.123"));
        assert!(!out.contains("eyJhbGci"));
        assert!(!out.contains(&hex));
        assert!(!out.contains("xyz.123"));
    }
}
