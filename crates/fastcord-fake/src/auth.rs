//! Fake email/password login. The scenario is chosen by the login (email) the
//! client sends; every value is an obvious fake. Nothing here is a real
//! credential: any non-empty password is accepted, the TOTP code is `000000`.

use serde_json::{Value, json};

use crate::http::{Request, Response};
use crate::world::{World, text};

pub const MFA_TICKET: &str = "fake-ticket-1";
pub const TOTP_CODE: &str = "000000";
pub const CAPTCHA_KEY: &str = "fake-captcha-key";

/// Logins and what they do: `direct@` a token, `mfa@` a TOTP step, `captcha@`
/// a CAPTCHA first (then a token once `x-captcha-key` is sent), `wrong@` a
/// wrong password, `verify@` the new-location e-mail refusal, `limited@` 429.
pub const LOGINS: &[&str] = &[
    "direct@fake.test",
    "mfa@fake.test",
    "captcha@fake.test",
    "wrong@fake.test",
    "verify@fake.test",
    "limited@fake.test",
];

fn invalid_form(code: &str, message: &str) -> Response {
    Response::json(
        400,
        &json!({"code": 50035, "message": "Invalid Form Body",
            "errors": {"login": {"_errors": [{"code": code, "message": message}]}}}),
    )
}

pub fn login(world: &World, req: &Request) -> Response {
    let body = req.json().unwrap_or(Value::Null);
    let login = text(&body, "login");
    let token = json!({"token": world.token, "user_id": text(&world.me, "id")});
    if text(&body, "password").is_empty() {
        return invalid_form("BASE_TYPE_REQUIRED", "This field is required");
    }
    match login {
        "direct@fake.test" => Response::json(200, &token),
        "mfa@fake.test" => Response::json(
            200,
            &json!({"mfa": true, "sms": false, "totp": true, "ticket": MFA_TICKET}),
        ),
        "captcha@fake.test" if req.header("x-captcha-key").is_some_and(|k| !k.is_empty()) => {
            Response::json(200, &token)
        }
        "captcha@fake.test" => Response::json(
            400,
            &json!({"captcha_key": ["captcha-required"], "captcha_service": "hcaptcha",
                "captcha_sitekey": "fake-sitekey", "captcha_rqdata": "fake-rqdata",
                "captcha_rqtoken": "fake-rqtoken", "captcha_session_id": "fake-session"}),
        ),
        "verify@fake.test" => invalid_form(
            "ACCOUNT_LOGIN_VERIFICATION_EMAIL",
            "New login location detected, please check your e-mail.",
        ),
        "limited@fake.test" => Response::json(
            429,
            &json!({"message": "You are being rate limited.", "retry_after": 30.0, "global": false}),
        ),
        _ => invalid_form("INVALID_LOGIN", "Login or password is invalid."),
    }
}

pub fn totp(world: &World, req: &Request) -> Response {
    let body = req.json().unwrap_or(Value::Null);
    if text(&body, "ticket") == MFA_TICKET && text(&body, "code") == TOTP_CODE {
        Response::json(200, &json!({"token": world.token}))
    } else {
        Response::json(
            400,
            &json!({"code": 60008, "message": "Invalid two-factor code"}),
        )
    }
}

/// `GET /experiments`: only the fingerprint the login flow reads.
pub fn experiments() -> Response {
    Response::json(
        200,
        &json!({"fingerprint": "1000.fakefingerprint", "assignments": []}),
    )
}
