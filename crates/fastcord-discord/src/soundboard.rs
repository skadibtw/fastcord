//! The open server's soundboard (issue #87): the sounds the account may play
//! there, and playing one into the voice channel.
//!
//! Only the server's own sounds are listed and played (`GET /guilds/{id}/soundboard-sounds`,
//! `POST /channels/{id}/send-soundboard-sound` without `source_guild_id`): playing another
//! server's sound is a Nitro privilege and default sounds are not offered. Discord stays
//! authoritative about the permission to play. A play is sent once and never retried by the
//! transport (only a 429, which Discord did not process, is rescheduled); an ambiguous failure
//! reaches the caller, who owns any retry.

use fastcord_model::Snowflake;
use serde::{Deserialize, Serialize};

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Most sounds kept from one answer (a boosted server holds at most 48).
pub const GUILD_SOUNDS: usize = 64;
/// Characters of a sound's name kept (Discord allows 2 to 32).
pub const MAX_SOUND_NAME_CHARS: usize = 32;
/// Characters of a Unicode emoji name kept.
const MAX_EMOJI_NAME_CHARS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SoundboardSound {
    pub id: Snowflake,
    pub name: String,
    /// Discord's per-sound volume, 0 to 100.
    pub volume_percent: u8,
    /// The sound's Unicode emoji, when it has one (custom emoji are not drawn here).
    pub emoji: Option<String>,
    /// `false` when the server lost the boost level that kept it usable.
    pub available: bool,
    /// The member who uploaded it, when Discord says.
    pub creator: Option<Snowflake>,
}

#[derive(Deserialize)]
struct Wire {
    sound_id: Snowflake,
    #[serde(default)]
    name: String,
    #[serde(default = "full")]
    volume: f64,
    #[serde(default)]
    emoji_name: Option<String>,
    #[serde(default = "yes")]
    available: bool,
    #[serde(
        default,
        rename = "user",
        deserialize_with = "fastcord_model::creator_id"
    )]
    creator: Option<Snowflake>,
    #[serde(default)]
    user_id: Option<Snowflake>,
}

#[derive(Deserialize)]
struct Items {
    items: Vec<Wire>,
}

fn yes() -> bool {
    true
}

fn full() -> f64 {
    1.0
}

impl From<Wire> for SoundboardSound {
    fn from(wire: Wire) -> Self {
        let volume = if wire.volume.is_finite() {
            wire.volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        Self {
            id: wire.sound_id,
            name: wire.name.chars().take(MAX_SOUND_NAME_CHARS).collect(),
            volume_percent: (volume * 100.0).round() as u8,
            emoji: wire
                .emoji_name
                .filter(|name| !name.is_empty())
                .map(|name| name.chars().take(MAX_EMOJI_NAME_CHARS).collect()),
            available: wire.available,
            creator: wire.creator.or(wire.user_id),
        }
    }
}

/// The answer is `{"items": [...]}`; a bare array is accepted as well. Sorted by name.
pub(crate) fn parse_sounds(body: &[u8]) -> Result<Vec<SoundboardSound>, RestError> {
    let wires = match serde_json::from_slice::<Items>(body) {
        Ok(items) => items.items,
        Err(_) => serde_json::from_slice::<Vec<Wire>>(body).map_err(|_| RestError::InvalidJson)?,
    };
    let mut sounds: Vec<SoundboardSound> = wires
        .into_iter()
        .take(GUILD_SOUNDS)
        .map(SoundboardSound::from)
        .collect();
    sounds.sort_by_cached_key(|sound| (sound.name.to_lowercase(), sound.id));
    Ok(sounds)
}

#[derive(Serialize)]
struct PlayBody {
    sound_id: Snowflake,
}

fn play_route(channel: Snowflake) -> Result<Route, RestError> {
    Route::new(
        Method::POST,
        &format!("/channels/{channel}/send-soundboard-sound"),
    )
}

fn play_request(channel: Snowflake, sound: Snowflake) -> Result<RestRequest, RestError> {
    RestRequest::new(play_route(channel)?, Priority::UserWrite).json(&PlayBody { sound_id: sound })
}

impl<C: Clock> RestClient<C> {
    /// The sounds of `guild`, sorted by name. Sent once; a refusal means the account
    /// may not list them.
    pub async fn guild_soundboard_sounds(
        &self,
        guild: Snowflake,
    ) -> Result<Vec<SoundboardSound>, RestError> {
        let route = Route::new(Method::GET, &format!("/guilds/{guild}/soundboard-sounds"))?;
        let response = self
            .execute(RestRequest::new(route, Priority::UserRead))
            .await?;
        parse_sounds(response.body())
    }

    /// Plays `sound` of the server into the voice channel `channel` the account is in.
    /// Sent once at user-write priority and never repeated here: Discord answers 204 and
    /// the play reaches the participants as a `VOICE_CHANNEL_EFFECT_SEND` event.
    pub async fn play_soundboard_sound(
        &self,
        channel: Snowflake,
        sound: Snowflake,
    ) -> Result<(), RestError> {
        self.execute(play_request(channel, sound)?).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sounds_parse_sorted_clamped_and_garbage_is_refused() {
        let body = br#"{"items":[
            {"sound_id":"2","name":"Zed","volume":0.5,"emoji_name":"\ud83d\udd14","available":false,"guild_id":"9","user":{"id":"1"}},
            {"sound_id":"1","name":"alpha","volume":7,"emoji_name":null},
            {"sound_id":"3","name":"beta","volume":-1}]}"#;
        let sounds = parse_sounds(body).unwrap();
        assert_eq!(
            sounds.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "beta", "Zed"]
        );
        assert_eq!(sounds[0].volume_percent, 100, "volume is clamped");
        assert_eq!(sounds[1].volume_percent, 0);
        assert!(sounds[0].available, "available defaults to true");
        assert!(!sounds[2].available);
        assert_eq!(sounds[2].volume_percent, 50);
        assert_eq!(sounds[2].emoji.as_deref(), Some("\u{1f514}"));
        assert_eq!(sounds[0].emoji, None);
        assert!(parse_sounds(b"{}").is_err());
        assert!(parse_sounds(b"nope").is_err());
        assert_eq!(parse_sounds(b"[]").unwrap(), vec![]);
        assert_eq!(parse_sounds(br#"[{"sound_id":"4"}]"#).unwrap().len(), 1);
    }

    #[test]
    fn a_long_list_is_cut() {
        let items: Vec<String> = (1..=GUILD_SOUNDS + 20)
            .map(|id| format!(r#"{{"sound_id":"{id}","name":"s{id}"}}"#))
            .collect();
        let body = format!(r#"{{"items":[{}]}}"#, items.join(","));
        assert_eq!(parse_sounds(body.as_bytes()).unwrap().len(), GUILD_SOUNDS);
    }

    #[test]
    fn play_is_one_post_of_the_sound_id_and_never_names_another_server() {
        let route = play_route(Snowflake(500)).unwrap();
        assert_eq!(route.key().method(), Method::POST);
        assert_eq!(
            route.url.as_str(),
            "https://discord.com/api/v9/channels/500/send-soundboard-sound"
        );
        assert_eq!(
            serde_json::to_string(&PlayBody {
                sound_id: Snowflake(77)
            })
            .unwrap(),
            r#"{"sound_id":"77"}"#
        );
        assert!(play_request(Snowflake(500), Snowflake(77)).is_ok());
    }
}
