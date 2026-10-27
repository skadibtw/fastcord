//! The whole email/password login path against the fake Discord, offline:
//! every scenario of `fastcord_fake::auth`, ending with the token validated by
//! `GET /users/@me` and saved into an in-memory credential store (never the OS
//! keyring). The CAPTCHA window is replaced by a function returning a fake key.
//! One test per file: the fake origin is process-wide.

use fastcord_discord::gateway::{ClientProperties, HostOs};
use fastcord_discord::{
    CaptchaSolution, PasswordAuth, PasswordAuthError, PasswordLogin, Priority, RestClient,
    UserToken, fake_origin,
};
use fastcord_fake::{Config, auth, start};
use fastcord_model::User;
use fastcord_platform::NativeCredentialStore;

const PASSWORD: &str = "fake-password-not-real";

fn properties() -> ClientProperties {
    ClientProperties::bundled(HostOs::Linux, "en-US")
}

/// What the app does around `password::attempt`: a CAPTCHA is solved once by
/// `solver` (the window in the app) and a second one stops the flow.
async fn sign_in(
    auth: &PasswordAuth,
    login: &str,
    solver: impl Fn() -> CaptchaSolution,
) -> Result<UserToken, PasswordAuthError> {
    let mut solution = None;
    loop {
        match auth.login(login, PASSWORD, solution.as_ref()).await? {
            PasswordLogin::Token(token) => return Ok(token),
            PasswordLogin::Mfa(challenge) => return auth.totp(&challenge, auth::TOTP_CODE).await,
            PasswordLogin::Captcha(_) if solution.is_none() => solution = Some(solver()),
            PasswordLogin::Captcha(_) => return Err(PasswordAuthError::Protocol),
        }
    }
}

fn fake_key() -> CaptchaSolution {
    CaptchaSolution {
        key: auth::CAPTCHA_KEY.to_owned(),
        rqtoken: Some("fake-rqtoken".to_owned()),
        session_id: Some("fake-session".to_owned()),
    }
}

#[tokio::test]
async fn every_login_scenario_ends_as_the_app_would() {
    let dir = std::env::temp_dir().join(format!("fastcord-login-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("requests.jsonl");
    let fake = start(Config {
        port: 0,
        world: None,
        log: Some(log.clone()),
        echo: false,
        chatter: None,
        switches: Default::default(),
    })
    .await
    .unwrap();
    fake_origin::set_origin(&format!("127.0.0.1:{}", fake.port())).unwrap();
    let network = fastcord_net::NetworkContext::off();
    let auth = PasswordAuth::new(&network, &properties()).unwrap();

    // Direct token, MFA + TOTP, CAPTCHA then token: each ends in a token the
    // account validates with and the in-memory store keeps.
    for login in ["direct@fake.test", "mfa@fake.test", "captcha@fake.test"] {
        let token = sign_in(&auth, login, fake_key).await.unwrap();
        let client = RestClient::new(
            network.clone(),
            UserToken::new(token.expose_secret().to_owned()),
            properties(),
        )
        .unwrap();
        let user: User = client.get("/users/@me", Priority::UserRead).await.unwrap();
        let store = NativeCredentialStore::in_memory();
        store.save(user.id, token.expose_secret()).unwrap();
        let loaded = store.load(user.id).unwrap().expect("saved");
        assert_eq!(&*loaded.into_secret(), token.expose_secret(), "{login}");
    }

    // A CAPTCHA that is not answered with a key stays a challenge, and a
    // second one stops the flow (the app never loops windows).
    let empty = || CaptchaSolution {
        key: String::new(),
        rqtoken: None,
        session_id: None,
    };
    assert_eq!(
        sign_in(&auth, "captcha@fake.test", empty).await.err(),
        Some(PasswordAuthError::Protocol)
    );
    assert!(matches!(
        auth.login("captcha@fake.test", PASSWORD, None).await,
        Ok(PasswordLogin::Captcha(_))
    ));

    // Wrong password, wrong TOTP code, rate limit, e-mail verification.
    assert_eq!(
        auth.login("wrong@fake.test", PASSWORD, None).await.err(),
        Some(PasswordAuthError::Rejected)
    );
    let Ok(PasswordLogin::Mfa(challenge)) = auth.login("mfa@fake.test", PASSWORD, None).await
    else {
        panic!("expected an MFA challenge");
    };
    assert_eq!(
        auth.totp(&challenge, "111111").await.err(),
        Some(PasswordAuthError::Rejected)
    );
    assert_eq!(
        auth.login("limited@fake.test", PASSWORD, None).await.err(),
        Some(PasswordAuthError::RateLimited)
    );
    let Err(PasswordAuthError::Refused(refusal)) =
        auth.login("verify@fake.test", PASSWORD, None).await
    else {
        panic!("expected a refusal");
    };
    assert!(
        refusal
            .to_string()
            .contains("new login location (HTTP 400)")
    );

    // The request log holds the requests but no secret.
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("/auth/login") && text.contains("/auth/mfa/totp"));
    for secret in [
        PASSWORD,
        auth::MFA_TICKET,
        auth::TOTP_CODE,
        auth::CAPTCHA_KEY,
        "111111",
        "fake-rqtoken",
        "fake-session",
    ] {
        assert!(!text.contains(secret), "log leaks {secret}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
