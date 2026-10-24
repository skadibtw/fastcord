//! Email/password login workers. Every success ends as a token handed to the
//! same `login::token_login` validation and storage path as paste and QR.

use std::sync::Arc;

use fastcord_discord::{
    CaptchaChallenge, CaptchaSolution, MfaChallenge, PasswordAuth, PasswordAuthError,
    PasswordLogin, UserToken,
};

/// `PasswordLogin` made cloneable for the message bus; the token stays redacted.
#[derive(Clone, Debug)]
pub enum Step {
    Token(Arc<UserToken>),
    Mfa(MfaChallenge),
    Captcha(CaptchaChallenge),
}

pub async fn attempt(
    network: fastcord_net::NetworkContext,
    login: String,
    password: Arc<UserToken>,
    captcha: Option<CaptchaSolution>,
) -> Result<Step, PasswordAuthError> {
    let auth = PasswordAuth::new(&network, &crate::login::client_properties(&network).await)?;
    Ok(
        match auth
            .login(login.trim(), password.expose_secret(), captcha.as_ref())
            .await?
        {
            PasswordLogin::Token(token) => Step::Token(Arc::new(token)),
            PasswordLogin::Mfa(challenge) => Step::Mfa(challenge),
            PasswordLogin::Captcha(challenge) => Step::Captcha(challenge),
        },
    )
}

pub async fn second_factor(
    network: fastcord_net::NetworkContext,
    challenge: MfaChallenge,
    code: Arc<UserToken>,
) -> Result<Arc<UserToken>, PasswordAuthError> {
    let auth = PasswordAuth::new(&network, &crate::login::client_properties(&network).await)?;
    auth.totp(&challenge, code.expose_secret().trim())
        .await
        .map(Arc::new)
}
