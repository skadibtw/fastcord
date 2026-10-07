//! Wire shapes of the remote-auth gateway (v2). Unknown ops are tolerated;
//! payload fields that carry secrets are never printed by `Debug`.

use std::fmt;

use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::RemoteAuthError;

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Incoming {
    Hello {
        heartbeat_interval: u64,
        timeout_ms: u64,
    },
    HeartbeatAck,
    NonceProof {
        encrypted_nonce: String,
    },
    PendingRemoteInit {
        fingerprint: String,
    },
    PendingTicket {
        encrypted_user_payload: String,
    },
    PendingLogin {
        ticket: Zeroizing<String>,
    },
    Cancel,
    #[serde(other)]
    Unknown,
}

impl fmt::Debug for Incoming {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Hello { .. } => "Hello",
            Self::HeartbeatAck => "HeartbeatAck",
            Self::NonceProof { .. } => "NonceProof",
            Self::PendingRemoteInit { .. } => "PendingRemoteInit",
            Self::PendingTicket { .. } => "PendingTicket",
            Self::PendingLogin { .. } => "PendingLogin",
            Self::Cancel => "Cancel",
            Self::Unknown => "Unknown",
        })
    }
}

impl Incoming {
    pub(crate) fn parse(text: &str) -> Result<Self, RemoteAuthError> {
        serde_json::from_str(text).map_err(|_| RemoteAuthError::Protocol)
    }
}

#[derive(Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Outgoing<'a> {
    Init { encoded_public_key: &'a str },
    NonceProof { nonce: &'a str },
    Heartbeat,
}

impl Outgoing<'_> {
    pub(crate) fn to_text(&self) -> Result<Zeroizing<String>, RemoteAuthError> {
        serde_json::to_string(self)
            .map(Zeroizing::new)
            .map_err(|_| RemoteAuthError::Protocol)
    }
}

/// The account that scanned the QR code, as revealed (encrypted to our key) by
/// the gateway before the phone user confirms.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteUser {
    pub id: Snowflake,
    pub username: String,
    pub avatar: Option<String>,
}

impl fmt::Debug for RemoteUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteUser")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl RemoteUser {
    /// Format: `id:discriminator:avatar_hash:username`; the avatar hash is `0`
    /// when absent. Usernames never contain `:`, but split on the first three
    /// separators only so an unexpected one cannot truncate the name.
    pub(crate) fn parse(payload: &[u8]) -> Result<Self, RemoteAuthError> {
        let text = std::str::from_utf8(payload).map_err(|_| RemoteAuthError::Protocol)?;
        let mut parts = text.splitn(4, ':');
        let (Some(id), Some(_discriminator), Some(avatar), Some(username)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(RemoteAuthError::Protocol);
        };
        let id = id
            .parse::<u64>()
            .ok()
            .filter(|id| *id != 0)
            .ok_or(RemoteAuthError::Protocol)?;
        if username.is_empty() || username.chars().any(char::is_control) {
            return Err(RemoteAuthError::Protocol);
        }
        Ok(Self {
            id: Snowflake(id),
            username: username.to_owned(),
            avatar: (avatar != "0" && !avatar.is_empty()).then(|| avatar.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_documented_gateway_payloads() {
        let hello =
            Incoming::parse(include_str!("../../../../fixtures/remote-auth/hello.json")).unwrap();
        assert!(matches!(
            hello,
            Incoming::Hello {
                heartbeat_interval: 41250,
                timeout_ms: 142637
            }
        ));
        assert!(matches!(
            Incoming::parse(r#"{"op":"heartbeat_ack"}"#).unwrap(),
            Incoming::HeartbeatAck
        ));
        assert!(matches!(
            Incoming::parse(r#"{"op":"cancel"}"#).unwrap(),
            Incoming::Cancel
        ));
        let Incoming::PendingLogin { ticket } =
            Incoming::parse(r#"{"op":"pending_login","ticket":"dummy-ticket"}"#).unwrap()
        else {
            panic!("expected pending_login");
        };
        assert_eq!(&*ticket, "dummy-ticket");
        let Incoming::PendingRemoteInit { fingerprint } = Incoming::parse(
            r#"{"op":"pending_remote_init","fingerprint":"UZ0-kOVzXDZTFVV5_QlpURSO2BQHrtkKWHNpIGoDI0k"}"#,
        )
        .unwrap() else {
            panic!("expected pending_remote_init");
        };
        assert_eq!(fingerprint.len(), 43);
    }

    #[test]
    fn unknown_ops_and_extra_fields_are_tolerated_but_garbage_is_not() {
        assert!(matches!(
            Incoming::parse(r#"{"op":"future_op","x":1}"#).unwrap(),
            Incoming::Unknown
        ));
        assert!(matches!(
            Incoming::parse(r#"{"op":"cancel","extra":true}"#).unwrap(),
            Incoming::Cancel
        ));
        for bad in [
            "",
            "[]",
            r#"{"no_op":1}"#,
            r#"{"op":"hello"}"#,
            r#"{"op":"pending_login"}"#,
            r#"{"op":"nonce_proof","encrypted_nonce":7}"#,
        ] {
            assert_eq!(
                Incoming::parse(bad).unwrap_err(),
                RemoteAuthError::Protocol,
                "{bad}"
            );
        }
    }

    #[test]
    fn debug_never_prints_payload_fields() {
        let ticket = Incoming::parse(r#"{"op":"pending_login","ticket":"dummy-ticket"}"#).unwrap();
        assert_eq!(format!("{ticket:?}"), "PendingLogin");
        let nonce =
            Incoming::parse(r#"{"op":"nonce_proof","encrypted_nonce":"dummy-nonce"}"#).unwrap();
        assert!(!format!("{nonce:?}").contains("dummy"));
    }

    #[test]
    fn outgoing_frames_match_the_documented_shapes() {
        assert_eq!(
            &*Outgoing::Heartbeat.to_text().unwrap(),
            r#"{"op":"heartbeat"}"#
        );
        assert_eq!(
            &*Outgoing::Init {
                encoded_public_key: "AAAA"
            }
            .to_text()
            .unwrap(),
            r#"{"op":"init","encoded_public_key":"AAAA"}"#
        );
        assert_eq!(
            &*Outgoing::NonceProof { nonce: "bm9uY2U" }.to_text().unwrap(),
            r#"{"op":"nonce_proof","nonce":"bm9uY2U"}"#
        );
    }

    #[test]
    fn user_payload_parsing() {
        let user =
            RemoteUser::parse(b"852892297661906993:0:05145cc5646fbcba277b6d5ea2030610:dolfies")
                .unwrap();
        assert_eq!(user.id, Snowflake(852_892_297_661_906_993));
        assert_eq!(user.username, "dolfies");
        assert_eq!(
            user.avatar.as_deref(),
            Some("05145cc5646fbcba277b6d5ea2030610")
        );
        let no_avatar = RemoteUser::parse(b"175928847299117063:0:0:alt_fixture").unwrap();
        assert_eq!(no_avatar.avatar, None);
        // Extra separators stay in the username rather than shifting fields.
        assert_eq!(RemoteUser::parse(b"1:0:0:a:b").unwrap().username, "a:b");
        for bad in [
            &b""[..],
            b"1:0:0",
            b"x:0:0:name",
            b"0:0:0:name",
            b"1:0:0:",
            b"1:0:0:bad\nname",
            &[0xff, 0xfe],
        ] {
            assert_eq!(
                RemoteUser::parse(bad).unwrap_err(),
                RemoteAuthError::Protocol
            );
        }
        assert!(!format!("{user:?}").contains("dolfies"));
    }
}
