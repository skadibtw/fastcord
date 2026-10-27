//! Credentials never reach the request log: the log is meant to be read and
//! shared when auditing what the client sends.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

pub const REDACTED: &str = "<redacted>";

/// Keys whose values are replaced: `token`, `password`, `captcha_key`, the
/// `authorization` and `cookie` headers, the login `ticket` and TOTP `code`, and
/// any key mentioning a token, a password or a captcha.
pub fn sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "token"
            | "password"
            | "captcha_key"
            | "x-captcha-key"
            | "authorization"
            | "cookie"
            | "ticket"
            | "code"
    ) || key.contains("captcha")
        || key.contains("token")
        || key.contains("password")
}

/// A copy of `value` with every sensitive key's value replaced, at any depth.
pub fn json(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let value = if sensitive(key) {
                        Value::String(REDACTED.to_owned())
                    } else {
                        json(value)
                    };
                    (key.clone(), value)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(json).collect()),
        other => other.clone(),
    }
}

pub fn headers(headers: &BTreeMap<String, String>) -> Value {
    Value::Object(
        headers
            .iter()
            .map(|(name, value)| {
                let value = if sensitive(name) { REDACTED } else { value };
                (name.clone(), Value::String(value.to_owned()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokens_passwords_and_captcha_keys_are_replaced_at_any_depth() {
        let input = json!({
            "token": "t", "password": "p", "captcha_key": "c", "login": "me",
            "d": {"Token": "t2", "auth_token": "t3", "refresh_tokens": ["a"], "keep": 1,
                  "list": [{"password_confirm": "x", "name": "n"}]},
        });
        let out = super::json(&input);
        assert_eq!(out["token"], REDACTED);
        assert_eq!(out["password"], REDACTED);
        assert_eq!(out["captcha_key"], REDACTED);
        assert_eq!(out["login"], "me");
        assert_eq!(out["d"]["Token"], REDACTED);
        assert_eq!(out["d"]["auth_token"], REDACTED);
        assert_eq!(out["d"]["refresh_tokens"], REDACTED);
        assert_eq!(out["d"]["keep"], 1);
        assert_eq!(out["d"]["list"][0]["password_confirm"], REDACTED);
        assert_eq!(out["d"]["list"][0]["name"], "n");
        assert!(!out.to_string().contains("t2"));
    }

    #[test]
    fn login_secrets_are_replaced() {
        let out = super::json(&json!({"login": "a@fake.test", "ticket": "t", "code": "000000"}));
        assert_eq!(out["login"], "a@fake.test");
        assert_eq!(out["ticket"], REDACTED);
        assert_eq!(out["code"], REDACTED);
        let mut map = BTreeMap::new();
        map.insert("x-captcha-session-id".to_owned(), "s".to_owned());
        map.insert("x-captcha-rqtoken".to_owned(), "r".to_owned());
        let out = headers(&map);
        assert_eq!(out["x-captcha-session-id"], REDACTED);
        assert_eq!(out["x-captcha-rqtoken"], REDACTED);
    }

    #[test]
    fn credential_headers_are_replaced() {
        let mut map = BTreeMap::new();
        map.insert("authorization".to_owned(), "fake-token".to_owned());
        map.insert("cookie".to_owned(), "a=b".to_owned());
        map.insert("x-captcha-key".to_owned(), "k".to_owned());
        map.insert("x-super-properties".to_owned(), "props".to_owned());
        let out = headers(&map);
        assert_eq!(out["authorization"], REDACTED);
        assert_eq!(out["cookie"], REDACTED);
        assert_eq!(out["x-captcha-key"], REDACTED);
        assert_eq!(out["x-super-properties"], "props");
    }
}
