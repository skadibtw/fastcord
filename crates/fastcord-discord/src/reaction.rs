//! Reacting to messages (SPEC §9, §5.2): the account's own normal reaction is
//! added with `PUT /channels/{channel}/messages/{message}/reactions/{emoji}/@me`
//! and taken back with the user client's typed
//! `DELETE .../reactions/{emoji}/0/@me` (`0` is a normal reaction), and who
//! reacted is read with `GET .../reactions/{emoji}?limit={n}&type=0`, only when
//! the user asks to see them.
//!
//! Each operation has exactly one route; none falls back to another. Only
//! normal reactions are ever requested. The emoji is one percent-encoded path
//! segment ([`ReactionEmoji`]): a Unicode emoji as its UTF-8 bytes, a custom one
//! as `name:id`. The transport sends each request once (only a 429 is
//! rescheduled), so a failure reaches the caller, who owns any retry.

use std::fmt;

use fastcord_model::{Emoji, MAX_REACTION_EMOJI_NAME_BYTES, Snowflake};
use serde::de::{DeserializeSeed, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};

use crate::{Clock, Method, Priority, RestClient, RestError, RestRequest, Route};

/// Most users Discord lists in one page of reactions.
const MAX_USERS: u8 = 100;

/// Characters of a reacting user's name the inspection list keeps.
const MAX_USER_NAME_CHARS: usize = 64;

/// Uppercase digits of a percent-encoded byte.
const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// A reaction emoji validated and percent-encoded for one URL path segment.
/// `Debug` never prints the emoji.
#[derive(Clone, PartialEq, Eq)]
pub struct ReactionEmoji {
    encoded: String,
}

impl ReactionEmoji {
    /// Validates `emoji` and encodes it for the reaction routes: a Unicode
    /// emoji as its name's UTF-8 bytes, a custom emoji as `name:id`, every byte
    /// but ASCII letters, digits, and `-._~` as an uppercase `%XX`. A custom
    /// emoji needs a name and a nonzero ID, and no name may be empty or longer
    /// than [`MAX_REACTION_EMOJI_NAME_BYTES`] bytes.
    pub fn new(emoji: &Emoji) -> Result<Self, ReactionEmojiError> {
        let name = emoji.name.as_deref().unwrap_or_default();
        if emoji.id.is_none() && name.is_empty() {
            return Err(ReactionEmojiError::NoIdentity);
        }
        if name.is_empty()
            || name.len() > MAX_REACTION_EMOJI_NAME_BYTES
            || emoji.id.is_some_and(|id| id.0 == 0)
        {
            return Err(ReactionEmojiError::Invalid);
        }
        // Every byte may become three; a custom emoji adds `%3A` and an ID of
        // at most 20 digits.
        let mut encoded = String::with_capacity(name.len() * 3 + 3 + 20);
        push_encoded(&mut encoded, name);
        if let Some(id) = emoji.id {
            encoded.push_str("%3A");
            encoded.push_str(&id.0.to_string());
        }
        Ok(Self { encoded })
    }

    /// The percent-encoded path segment.
    pub fn as_path_segment(&self) -> &str {
        &self.encoded
    }
}

fn push_encoded(encoded: &mut String, text: &str) {
    for &byte in text.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
}

impl fmt::Debug for ReactionEmoji {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReactionEmoji")
            .field("encoded_bytes", &self.encoded.len())
            .finish_non_exhaustive()
    }
}

/// Why an emoji cannot be reacted with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReactionEmojiError {
    /// It has neither a name nor an ID: nothing says which reaction it is.
    NoIdentity,
    /// A custom emoji without a name or with a zero ID, or a name outside
    /// `1..=`[`MAX_REACTION_EMOJI_NAME_BYTES`] bytes.
    Invalid,
}

impl fmt::Display for ReactionEmojiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoIdentity => "an emoji needs a name or an ID to react with",
            Self::Invalid => "this emoji cannot be used as a reaction",
        })
    }
}

impl std::error::Error for ReactionEmojiError {}

/// One reacting user, reduced to what an inspection list shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReactionUser {
    pub id: Snowflake,
    /// The global display name, else the username, cut to 64 characters. Empty
    /// only if Discord sent neither.
    pub name: String,
}

/// The few fields of a partial user object the list needs; the rest (avatar,
/// discriminator, flags, ...) is skipped as it is read.
#[derive(Deserialize)]
struct WireUser {
    id: Snowflake,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    global_name: Option<String>,
}

impl From<WireUser> for ReactionUser {
    fn from(wire: WireUser) -> Self {
        let mut name = wire
            .global_name
            .filter(|name| !name.is_empty())
            .or(wire.username)
            .unwrap_or_default();
        if let Some((end, _)) = name.char_indices().nth(MAX_USER_NAME_CHARS) {
            name.truncate(end);
            name.shrink_to_fit();
        }
        Self { id: wire.id, name }
    }
}

/// Decodes the array of users, keeping at most `limit` of them. Elements past
/// the limit are skipped without being decoded, so a response with more users
/// than asked for never becomes a longer list.
pub(crate) struct UserList {
    pub(crate) limit: usize,
}

impl<'de> DeserializeSeed<'de> for UserList {
    type Value = Vec<ReactionUser>;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for UserList {
    type Value = Vec<ReactionUser>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a list of users")
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut users = Vec::new();
        while users.len() < self.limit {
            match sequence.next_element::<WireUser>()? {
                Some(user) => users.push(user.into()),
                None => return Ok(users),
            }
        }
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(users)
    }
}

fn parse_users(body: &[u8], limit: usize) -> Result<Vec<ReactionUser>, RestError> {
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let users = UserList { limit }
        .deserialize(&mut deserializer)
        .map_err(|_| RestError::InvalidJson)?;
    deserializer.end().map_err(|_| RestError::InvalidJson)?;
    Ok(users)
}

/// The page size actually requested: Discord lists 1 to 100 users.
fn page_size(limit: u8) -> u8 {
    limit.clamp(1, MAX_USERS)
}

fn reaction_route(
    method: Method,
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
    tail: &str,
) -> Result<Route, RestError> {
    Route::new(
        method,
        &format!(
            "/channels/{channel}/messages/{message}/reactions/{}{tail}",
            emoji.as_path_segment()
        ),
    )
}

fn add_route(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
) -> Result<Route, RestError> {
    reaction_route(Method::PUT, channel, message, emoji, "/@me")
}

/// The user-client typed route: `0` names a normal reaction.
fn remove_own_route(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
) -> Result<Route, RestError> {
    reaction_route(Method::DELETE, channel, message, emoji, "/0/@me")
}

fn users_route(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
    limit: u8,
) -> Result<Route, RestError> {
    reaction_route(
        Method::GET,
        channel,
        message,
        emoji,
        &format!("?limit={limit}&type=0"),
    )
}

fn add_request(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        add_route(channel, message, emoji)?,
        Priority::UserWrite,
    ))
}

fn remove_own_request(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        remove_own_route(channel, message, emoji)?,
        Priority::UserWrite,
    ))
}

fn users_request(
    channel: Snowflake,
    message: Snowflake,
    emoji: &ReactionEmoji,
    limit: u8,
) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(
        users_route(channel, message, emoji, limit)?,
        Priority::UserRead,
    ))
}

impl<C: Clock> RestClient<C> {
    /// Adds the account's normal reaction with `emoji` to a message. Sent once
    /// at user-write priority; a failure (a permission Discord refuses, a
    /// deleted message) is returned to the caller.
    pub async fn add_reaction(
        &self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &ReactionEmoji,
    ) -> Result<(), RestError> {
        self.execute(add_request(channel, message, emoji)?)
            .await
            .map(drop)
    }

    /// Takes the account's own normal reaction with `emoji` back, through the
    /// user client's typed route. Sent once at user-write priority.
    pub async fn remove_own_reaction(
        &self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &ReactionEmoji,
    ) -> Result<(), RestError> {
        self.execute(remove_own_request(channel, message, emoji)?)
            .await
            .map(drop)
    }

    /// Removes another user's reaction (`DELETE .../reactions/{emoji}/{user}`),
    /// which needs Manage Messages. Unverified for the user client.
    pub async fn remove_user_reaction(
        &self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &ReactionEmoji,
        user: Snowflake,
    ) -> Result<(), RestError> {
        let route = reaction_route(Method::DELETE, channel, message, emoji, &format!("/{user}"))?;
        let request = RestRequest::new(route, Priority::UserWrite).with_error_detail();
        self.execute(request).await.map(drop)
    }

    /// Clears every reaction with one emoji (`DELETE .../reactions/{emoji}`),
    /// which needs Manage Messages. Unverified for the user client.
    pub async fn clear_emoji_reactions(
        &self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &ReactionEmoji,
    ) -> Result<(), RestError> {
        let route = reaction_route(Method::DELETE, channel, message, emoji, "")?;
        let request = RestRequest::new(route, Priority::UserWrite).with_error_detail();
        self.execute(request).await.map(drop)
    }

    /// Lists the users who reacted normally with `emoji`, for an explicit
    /// inspection. `limit` is clamped to 1 to 100 and the answer never holds
    /// more entries than that, whatever Discord sends; a body that is not an
    /// array of users is [`RestError::InvalidJson`]. User-read priority.
    pub async fn reaction_users(
        &self,
        channel: Snowflake,
        message: Snowflake,
        emoji: &ReactionEmoji,
        limit: u8,
    ) -> Result<Vec<ReactionUser>, RestError> {
        let limit = page_size(limit);
        let response = self
            .execute(users_request(channel, message, emoji, limit)?)
            .await?;
        parse_users(response.body(), usize::from(limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UserToken;
    use crate::gateway::{ClientProperties, HostOs};

    const ROUTES: &str = include_str!("../../../fixtures/rest/reaction-routes.json");
    const USERS: &str = include_str!("../../../fixtures/rest/reaction-users.json");
    const CHANNEL: Snowflake = Snowflake(500);
    const MESSAGE: Snowflake = Snowflake(1_290_000_000_000_000_010);

    fn unicode(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    fn custom(id: u64, name: &str) -> Emoji {
        Emoji {
            id: Some(Snowflake(id)),
            name: Some(name.to_owned()),
            animated: true,
        }
    }

    fn emoji(emoji: &Emoji) -> ReactionEmoji {
        ReactionEmoji::new(emoji).unwrap()
    }

    fn encoded(emoji: &Emoji) -> String {
        ReactionEmoji::new(emoji)
            .unwrap()
            .as_path_segment()
            .to_owned()
    }

    /// The emoji each entry of the route fixture describes.
    fn described(description: &str) -> Emoji {
        match description {
            "U+1F44D thumbs up" => unicode("\u{1f44d}"),
            "U+2764 U+FE0F red heart with variation selector" => unicode("\u{2764}\u{fe0f}"),
            "U+1F469 U+1F3FD U+200D U+1F4BB woman technologist, medium skin tone (ZWJ sequence)" => {
                unicode("\u{1f469}\u{1f3fd}\u{200d}\u{1f4bb}")
            }
            "custom wave:777" => custom(777, "wave"),
            other => panic!("unknown emoji in the route fixture: {other}"),
        }
    }

    #[test]
    fn moderator_routes_name_the_user_or_clear_the_emoji() {
        let heart = emoji(&unicode("\u{2764}"));
        let (user, clear) = (
            reaction_route(Method::DELETE, CHANNEL, MESSAGE, &heart, "/77").unwrap(),
            reaction_route(Method::DELETE, CHANNEL, MESSAGE, &heart, "").unwrap(),
        );
        assert_eq!(
            user.url.as_str(),
            format!(
                "https://discord.com/api/v10/channels/500/messages/{MESSAGE}/reactions/%E2%9D%A4/77"
            )
        );
        assert!(clear.url.as_str().ends_with("/reactions/%E2%9D%A4"));
        assert_eq!(clear.key().method(), &Method::DELETE);
    }

    #[test]
    fn the_route_fixture_pins_method_and_path_of_every_operation_for_every_kind_of_emoji() {
        #[derive(Deserialize)]
        struct Pin {
            operation: String,
            emoji: String,
            method: String,
            path: String,
        }
        let pins: Vec<Pin> = serde_json::from_str(ROUTES).unwrap();
        assert_eq!(pins.len(), 12);
        for pin in &pins {
            let emoji = emoji(&described(&pin.emoji));
            let route = match pin.operation.as_str() {
                "add" => add_route(CHANNEL, MESSAGE, &emoji),
                "remove_own" => remove_own_route(CHANNEL, MESSAGE, &emoji),
                "users" => users_route(CHANNEL, MESSAGE, &emoji, 25),
                other => panic!("unknown operation in the route fixture: {other}"),
            }
            .unwrap();
            let what = format!("{} {}", pin.operation, pin.emoji);
            assert_eq!(route.key().method().as_str(), pin.method, "{what}");
            assert_eq!(
                route.url.as_str(),
                format!("https://discord.com/api/v10{}", pin.path),
                "{what}"
            );
        }
        // Every operation is pinned for every emoji, once.
        let mut pinned: Vec<(&str, &str)> = pins
            .iter()
            .map(|pin| (pin.operation.as_str(), pin.emoji.as_str()))
            .collect();
        pinned.sort_unstable();
        pinned.dedup();
        assert_eq!(pinned.len(), 12);
        for operation in ["add", "remove_own", "users"] {
            assert_eq!(
                pinned
                    .iter()
                    .filter(|(pinned, _)| *pinned == operation)
                    .count(),
                4,
                "{operation}"
            );
        }
    }

    #[test]
    fn adding_ends_in_me_and_the_typed_removal_in_0_me_and_neither_takes_a_query() {
        for emoji in [
            unicode("\u{1f44d}"),
            unicode("\u{2764}\u{fe0f}"),
            unicode("\u{1f469}\u{1f3fd}\u{200d}\u{1f4bb}"),
            custom(777, "wave"),
        ] {
            let emoji = ReactionEmoji::new(&emoji).unwrap();
            let add = add_route(CHANNEL, MESSAGE, &emoji).unwrap();
            let remove = remove_own_route(CHANNEL, MESSAGE, &emoji).unwrap();
            let users = users_route(CHANNEL, MESSAGE, &emoji, 25).unwrap();
            let segment = emoji.as_path_segment();
            assert!(
                add.url
                    .path()
                    .ends_with(&format!("/reactions/{segment}/@me")),
                "{segment}"
            );
            assert!(
                !add.url.path().ends_with("/0/@me"),
                "adding has no reaction type segment"
            );
            assert!(
                remove
                    .url
                    .path()
                    .ends_with(&format!("/reactions/{segment}/0/@me")),
                "{segment}"
            );
            assert_eq!((add.url.query(), remove.url.query()), (None, None));
            assert!(users.url.path().ends_with(&format!("/reactions/{segment}")));
            assert_eq!(users.url.query(), Some("limit=25&type=0"));
            assert_eq!(add.key().method(), &Method::PUT);
            assert_eq!(remove.key().method(), &Method::DELETE);
            assert_eq!(users.key().method(), &Method::GET);
        }
    }

    #[test]
    fn route_keys_keep_the_channel_major_and_normalize_the_emoji_and_the_typed_zero() {
        let up = emoji(&unicode("\u{1f44d}"));
        let add = add_route(CHANNEL, MESSAGE, &up).unwrap();
        let remove = remove_own_route(CHANNEL, MESSAGE, &up).unwrap();
        let users = users_route(CHANNEL, MESSAGE, &up, 25).unwrap();
        assert_eq!(
            add.key().normalized(),
            "/channels/{channel_id}/messages/{id}/reactions/{emoji}/@me"
        );
        assert_eq!(
            remove.key().normalized(),
            "/channels/{channel_id}/messages/{id}/reactions/{emoji}/{id}/@me",
            "the typed `0` is normalized like any other ID"
        );
        assert_eq!(
            users.key().normalized(),
            "/channels/{channel_id}/messages/{id}/reactions/{emoji}"
        );
        for route in [&add, &remove, &users] {
            assert_eq!(route.key().major().channel_id(), Some(CHANNEL));
        }
        // Another emoji or message shares the operation's key (and so its rate
        // limit); another channel does not.
        let wave = emoji(&custom(777, "wave"));
        assert_eq!(
            add_route(CHANNEL, Snowflake(7), &wave).unwrap().key(),
            add.key()
        );
        assert_eq!(
            remove_own_route(CHANNEL, Snowflake(7), &wave)
                .unwrap()
                .key(),
            remove.key()
        );
        assert_eq!(
            users_route(CHANNEL, Snowflake(7), &wave, 100)
                .unwrap()
                .key(),
            users.key()
        );
        assert_ne!(
            add_route(Snowflake(501), MESSAGE, &up).unwrap().key(),
            add.key()
        );
        assert_ne!(add.key(), remove.key());
    }

    #[test]
    fn requests_carry_the_stated_priority_and_never_show_ids_or_emoji() {
        let up = emoji(&unicode("\u{1f44d}"));
        let add = format!("{:?}", add_request(CHANNEL, MESSAGE, &up).unwrap());
        let remove = format!("{:?}", remove_own_request(CHANNEL, MESSAGE, &up).unwrap());
        let users = format!("{:?}", users_request(CHANNEL, MESSAGE, &up, 25).unwrap());
        assert!(add.contains("priority: UserWrite") && add.contains("body_bytes: 0"));
        assert!(remove.contains("priority: UserWrite") && remove.contains("body_bytes: 0"));
        assert!(users.contains("priority: UserRead") && users.contains("body_bytes: 0"));
        for shown in [&add, &remove, &users] {
            assert!(shown.contains("/channels/{channel_id}/messages/{id}/reactions/{emoji}"));
            assert!(!shown.contains("1290000000000000010") && !shown.contains("%F0"));
        }
    }

    #[test]
    fn every_class_of_byte_is_encoded_as_the_routes_require() {
        // ASCII letters, digits, and `-._~` stay; every other ASCII byte is an
        // uppercase %XX.
        assert_eq!(encoded(&unicode("AZaz09-._~")), "AZaz09-._~");
        for byte in 0u8..=127 {
            let character = char::from(byte);
            let expected = if character.is_ascii_alphanumeric() || "-._~".contains(character) {
                character.to_string()
            } else {
                format!("%{byte:02X}")
            };
            assert_eq!(
                encoded(&unicode(&character.to_string())),
                expected,
                "{byte}"
            );
        }
        for (name, expected) in [
            (":", "%3A"),
            (" ", "%20"),
            ("%", "%25"),
            ("/", "%2F"),
            ("?", "%3F"),
            ("#", "%23"),
            ("@", "%40"),
            ("\\", "%5C"),
            ("+", "%2B"),
            ("a b", "a%20b"),
            ("100%", "100%25"),
            // Multi-byte characters are encoded byte by byte.
            ("\u{e9}", "%C3%A9"),
            ("\u{20ac}", "%E2%82%AC"),
            ("\u{1f600}", "%F0%9F%98%80"),
            ("\u{2764}\u{fe0f}", "%E2%9D%A4%EF%B8%8F"),
        ] {
            assert_eq!(encoded(&unicode(name)), expected, "{name}");
        }
        // A custom emoji is `name:id`, the colon encoded.
        assert_eq!(encoded(&custom(777, "wave")), "wave%3A777");
        assert_eq!(encoded(&custom(1, "a b:c")), "a%20b%3Ac%3A1");
        assert_eq!(encoded(&custom(u64::MAX, "x")), "x%3A18446744073709551615");
        let renamed = Emoji {
            animated: false,
            ..custom(777, "wave")
        };
        assert_eq!(
            encoded(&renamed),
            "wave%3A777",
            "animated is not part of it"
        );
    }

    #[test]
    fn names_of_up_to_64_bytes_are_accepted_and_longer_ones_are_not() {
        let longest = MAX_REACTION_EMOJI_NAME_BYTES;
        let ascii = "a".repeat(longest);
        assert_eq!(encoded(&unicode(&ascii)), ascii);
        assert_eq!(
            ReactionEmoji::new(&unicode(&"a".repeat(longest + 1))),
            Err(ReactionEmojiError::Invalid)
        );
        // The limit counts bytes: 32 two-byte characters fit, 33 do not.
        assert_eq!(
            encoded(&unicode(&"\u{e9}".repeat(longest / 2))).len(),
            longest / 2 * 6
        );
        assert_eq!(
            ReactionEmoji::new(&unicode(&"\u{e9}".repeat(longest / 2 + 1))),
            Err(ReactionEmojiError::Invalid)
        );
        // The same limit for the name of a custom emoji.
        assert!(ReactionEmoji::new(&custom(5, &ascii)).is_ok());
        assert_eq!(
            ReactionEmoji::new(&custom(5, &"a".repeat(longest + 1))),
            Err(ReactionEmojiError::Invalid)
        );
        // The longest segment possible is still a valid route.
        let worst = emoji(&custom(u64::MAX, &"\0".repeat(longest)));
        assert_eq!(worst.as_path_segment().len(), longest * 3 + 3 + 20);
        assert!(add_route(CHANNEL, MESSAGE, &worst).is_ok());
    }

    #[test]
    fn an_emoji_that_cannot_be_reacted_with_is_refused_before_any_request() {
        let nameless = Emoji {
            id: None,
            name: None,
            animated: false,
        };
        let id_only = |id: u64| Emoji {
            id: Some(Snowflake(id)),
            name: None,
            animated: false,
        };
        for refused in [nameless, unicode("")] {
            assert_eq!(
                ReactionEmoji::new(&refused),
                Err(ReactionEmojiError::NoIdentity)
            );
        }
        for refused in [
            id_only(777),
            id_only(0),
            custom(777, ""),
            custom(0, "wave"),
            unicode(&"x".repeat(MAX_REACTION_EMOJI_NAME_BYTES + 1)),
        ] {
            assert_eq!(
                ReactionEmoji::new(&refused),
                Err(ReactionEmojiError::Invalid)
            );
        }
        // The errors say nothing about the emoji and are real errors.
        for error in [ReactionEmojiError::NoIdentity, ReactionEmojiError::Invalid] {
            let shown = error.to_string();
            assert!(!shown.is_empty() && !shown.contains("777"));
            let _: &dyn std::error::Error = &error;
        }
        assert_ne!(
            ReactionEmojiError::NoIdentity.to_string(),
            ReactionEmojiError::Invalid.to_string()
        );
    }

    #[test]
    fn a_name_that_would_be_a_dot_segment_never_reaches_the_network() {
        for name in [".", ".."] {
            let emoji = emoji(&unicode(name));
            assert_eq!(emoji.as_path_segment(), name);
            assert!(matches!(
                add_route(CHANNEL, MESSAGE, &emoji),
                Err(RestError::InvalidRoute)
            ));
            assert!(matches!(
                remove_own_request(CHANNEL, MESSAGE, &emoji),
                Err(RestError::InvalidRoute)
            ));
            assert!(matches!(
                users_request(CHANNEL, MESSAGE, &emoji, 25),
                Err(RestError::InvalidRoute)
            ));
        }
    }

    #[test]
    fn debug_never_prints_the_emoji() {
        let wave = emoji(&custom(777, "wave"));
        let shown = format!("{wave:?}");
        assert!(shown.contains("ReactionEmoji"));
        for secret in ["wave", "777", "%3A"] {
            assert!(!shown.contains(secret), "{secret}");
        }
        let unicode = emoji(&unicode("\u{1f44d}"));
        let shown = format!("{unicode:?}");
        assert!(!shown.contains('\u{1f44d}') && !shown.contains("F0"));
    }

    #[test]
    fn the_page_size_is_clamped_to_what_discord_lists() {
        assert_eq!(
            [0, 1, 25, 100, 101, 255].map(page_size),
            [1, 1, 25, 100, 100, 100]
        );
        let up = emoji(&unicode("\u{1f44d}"));
        for (asked, sent) in [(0, 1), (100, 100), (255, 100)] {
            let route = users_route(CHANNEL, MESSAGE, &up, page_size(asked)).unwrap();
            assert_eq!(
                route.url.query(),
                Some(format!("limit={sent}&type=0").as_str())
            );
        }
    }

    fn user(id: u64, name: &str) -> ReactionUser {
        ReactionUser {
            id: Snowflake(id),
            name: name.to_owned(),
        }
    }

    #[test]
    fn the_users_fixture_decodes_only_what_the_list_shows_and_truncates_to_the_limit() {
        let users = parse_users(USERS.as_bytes(), 100).unwrap();
        assert_eq!(
            users,
            [
                user(2, "Alt Account"),
                user(3, "other_fixture"),
                user(1_290_000_000_000_000_444, "Zo\u{eb} Fixture"),
            ],
            "the global name, else the username"
        );
        assert_eq!(parse_users(USERS.as_bytes(), 2).unwrap(), users[..2]);
        assert_eq!(parse_users(USERS.as_bytes(), 1).unwrap(), users[..1]);
        assert!(parse_users(b"[]", 5).unwrap().is_empty());
        assert!(parse_users(USERS.as_bytes(), 0).unwrap().is_empty());

        // A response far larger than asked for is cut at the limit, and what
        // lies beyond it is skipped without being read as a user.
        let many: Vec<serde_json::Value> = (1..=1_000)
            .map(|n| serde_json::json!({"id": n.to_string(), "username": format!("u{n}")}))
            .collect();
        let body = serde_json::to_vec(&many).unwrap();
        let kept = parse_users(&body, 100).unwrap();
        assert_eq!(kept.len(), 100);
        assert_eq!((kept[0].id, kept[99].id), (Snowflake(1), Snowflake(100)));
        let junk = br#"[{"id":"1","username":"a"}, 5, "x", {"bad":true}, [[]]]"#;
        assert_eq!(parse_users(junk, 1).unwrap(), [user(1, "a")]);
    }

    #[test]
    fn display_names_are_cut_to_64_characters_and_fall_back_to_the_username() {
        let decode = |fields: &str| {
            let body = format!(r#"[{{"id":"1"{fields}}}]"#);
            parse_users(body.as_bytes(), 5).unwrap().remove(0).name
        };
        // Characters, not bytes: no multi-byte character is split.
        let long = "\u{e9}".repeat(200);
        let cut = decode(&format!(r#","username":"short","global_name":"{long}""#));
        assert_eq!(cut, "\u{e9}".repeat(MAX_USER_NAME_CHARS));
        let exact = "x".repeat(MAX_USER_NAME_CHARS);
        assert_eq!(decode(&format!(r#","username":"{exact}""#)), exact);
        assert_eq!(
            decode(&format!(r#","username":"{exact}y""#)),
            exact,
            "one character over is cut"
        );
        let emoji_name = "\u{1f600}".repeat(65);
        assert_eq!(
            decode(&format!(r#","username":"{emoji_name}""#)),
            "\u{1f600}".repeat(64)
        );
        // Null, blank, or absent global names fall back; nothing at all is empty.
        assert_eq!(decode(r#","username":"alt","global_name":null"#), "alt");
        assert_eq!(decode(r#","username":"alt","global_name":"""#), "alt");
        assert_eq!(decode(r#","username":"alt""#), "alt");
        assert_eq!(decode(r#","username":null,"global_name":"Name""#), "Name");
        assert_eq!(decode(""), "");
    }

    #[test]
    fn a_body_that_is_not_an_array_of_users_is_invalid_json() {
        let malformed: [&[u8]; 14] = [
            b"",
            b"{}",
            br#"{"message":"Unknown Message","code":10008}"#,
            b"null",
            b"[",
            b"[{",
            br#"[{"username":"no id"}]"#,
            br#"[{"id":"not a number","username":"a"}]"#,
            br#"[{"id":"1","username":5}]"#,
            b"[1]",
            br#"["a"]"#,
            b"[null]",
            br#"[{"id":"1"}] trailing"#,
            // Syntax errors past the limit still count: skipping needs valid JSON.
            br#"[{"id":"1","username":"a"}, {"#,
        ];
        for body in malformed {
            assert_eq!(
                parse_users(body, 1),
                Err(RestError::InvalidJson),
                "{}",
                String::from_utf8_lossy(body)
            );
        }
    }

    #[tokio::test]
    async fn a_stopped_account_never_adds_removes_or_lists_reactions() {
        let properties = ClientProperties::bundled(HostOs::Linux, "en-US");
        let client = RestClient::new(
            UserToken::new("offline-test-credential".to_owned()),
            properties,
        )
        .unwrap();
        client.stop_authenticated_work();
        let up = emoji(&unicode("\u{1f44d}"));
        assert_eq!(
            client.add_reaction(CHANNEL, MESSAGE, &up).await,
            Err(RestError::AuthenticationRequired)
        );
        assert_eq!(
            client.remove_own_reaction(CHANNEL, MESSAGE, &up).await,
            Err(RestError::AuthenticationRequired)
        );
        assert_eq!(
            client.reaction_users(CHANNEL, MESSAGE, &up, 25).await,
            Err(RestError::AuthenticationRequired)
        );
    }

    /// Alt account only: `FASTCORD_LIVE_TOKEN`, `FASTCORD_LIVE_CHANNEL` and
    /// `FASTCORD_LIVE_MESSAGE` (a message in a DM with another account).
    /// Adds, lists and takes back a normal reaction through the real routes.
    #[tokio::test]
    #[ignore = "contacts Discord with the alt account"]
    async fn live_reaction_add_list_remove() {
        let var = |k: &str| std::env::var(k).unwrap();
        let properties = ClientProperties::bundled(HostOs::current(), "en-US");
        let client =
            RestClient::new(UserToken::new(var("FASTCORD_LIVE_TOKEN")), properties).unwrap();
        let channel = Snowflake(var("FASTCORD_LIVE_CHANNEL").parse().unwrap());
        let message = Snowflake(var("FASTCORD_LIVE_MESSAGE").parse().unwrap());
        let up = emoji(&unicode("\u{1F44D}"));
        println!(
            "initial remove: {:?}",
            client.remove_own_reaction(channel, message, &up).await
        );
        println!(
            "initial users: {:?}",
            client.reaction_users(channel, message, &up, 25).await
        );
        let users = client.reaction_users(channel, message, &up, 25).await;
        assert!(users.unwrap().is_empty());
        client
            .add_reaction(channel, message, &up)
            .await
            .expect("add");
        let users = client
            .reaction_users(channel, message, &up, 25)
            .await
            .unwrap();
        println!("after add: {} user(s)", users.len());
        assert_eq!(users.len(), 1);
        client
            .remove_own_reaction(channel, message, &up)
            .await
            .unwrap();
        let users = client
            .reaction_users(channel, message, &up, 25)
            .await
            .unwrap();
        println!("after remove: {} user(s)", users.len());
        assert!(users.is_empty());
    }
}
