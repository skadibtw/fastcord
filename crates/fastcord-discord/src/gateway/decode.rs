//! Typed decoding and normalization of dispatch payloads.
//!
//! Every payload is deserialized straight from the borrowed event text into
//! typed structs that name only the fields this client uses, so unknown fields
//! are skipped as they are read and no untyped JSON tree is ever built. READY's
//! arrays (guilds, users, members, channels) therefore stream element by
//! element into their final typed form.
//!
//! Normalization turns the deduplicated READY shape (`users` plus ID references,
//! `merged_members` parallel to `guilds`) into entities stored once. A payload
//! that still embeds user objects (no dedupe) normalizes to the same result.

use std::collections::HashMap;

use fastcord_model::{
    Channel, Guild, GuildMember, Message, MessageUpdate, Role, Snowflake, User, VoiceState,
};
use serde::{Deserialize, Deserializer};

use super::event::{
    ChannelUnread, Dispatch, GuildCreate, GuildDelete, MessageDelete, PassiveUpdate, Ready,
    ReadySupplemental, SessionId, SupplementalGuild,
};

/// A known payload that did not decode into its typed form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Malformed;

/// A "partial user" as READY and member payloads carry them: the ID is
/// guaranteed, everything else is best effort. One odd user object must not
/// make the whole account state undecodable.
#[derive(Deserialize)]
struct WireUser {
    id: Snowflake,
    #[serde(default, deserialize_with = "null_default")]
    username: String,
    #[serde(default)]
    global_name: Option<String>,
    #[serde(default)]
    avatar: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    bot: bool,
}

impl From<WireUser> for User {
    fn from(wire: WireUser) -> Self {
        Self {
            id: wire.id,
            username: wire.username,
            global_name: wire.global_name,
            avatar: wire.avatar,
            bot: wire.bot,
        }
    }
}

/// Discord documents several of these arrays as "may be empty, omitted, or
/// null"; all three mean empty.
fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct MemberWire {
    #[serde(default)]
    user: Option<WireUser>,
    #[serde(default)]
    user_id: Option<Snowflake>,
    #[serde(default)]
    nick: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    roles: Vec<Snowflake>,
    #[serde(default)]
    communication_disabled_until: Option<String>,
}

/// Guild fields nested under `properties` when CLIENT_STATE_V2 is on; merged
/// into the guild object itself otherwise. Both are accepted.
#[derive(Deserialize)]
struct GuildProperties {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    owner_id: Option<Snowflake>,
}

#[derive(Deserialize)]
struct GuildWire {
    id: Snowflake,
    #[serde(default)]
    unavailable: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    owner_id: Option<Snowflake>,
    #[serde(default)]
    properties: Option<GuildProperties>,
    #[serde(default, deserialize_with = "null_default")]
    roles: Vec<Role>,
    #[serde(default, deserialize_with = "null_default")]
    channels: Vec<Channel>,
    #[serde(default, deserialize_with = "null_default")]
    members: Vec<MemberWire>,
    #[serde(default)]
    member_count: u32,
}

#[derive(Deserialize)]
struct ReadyWire {
    session_id: String,
    resume_gateway_url: String,
    user: User,
    #[serde(default, deserialize_with = "null_default")]
    users: Vec<WireUser>,
    #[serde(default, deserialize_with = "null_default")]
    guilds: Vec<GuildWire>,
    #[serde(default, deserialize_with = "null_default")]
    merged_members: Vec<Vec<MemberWire>>,
    #[serde(default, deserialize_with = "null_default")]
    private_channels: Vec<Channel>,
    #[serde(default)]
    required_action: Option<String>,
}

#[derive(Deserialize)]
struct SupplementalGuildWire {
    id: Snowflake,
    #[serde(default, deserialize_with = "null_default")]
    voice_states: Vec<VoiceState>,
}

#[derive(Deserialize)]
struct SupplementalWire {
    #[serde(default, deserialize_with = "null_default")]
    guilds: Vec<SupplementalGuildWire>,
    #[serde(default, deserialize_with = "null_default")]
    merged_members: Vec<Vec<MemberWire>>,
    #[serde(default, deserialize_with = "null_default")]
    lazy_private_channels: Vec<Channel>,
}

#[derive(Deserialize)]
struct GuildDeleteWire {
    id: Snowflake,
    #[serde(default)]
    unavailable: bool,
}

#[derive(Deserialize)]
struct PassiveUpdateWire {
    guild_id: Snowflake,
    #[serde(default, deserialize_with = "null_default")]
    updated_channels: Vec<ChannelUnread>,
    #[serde(default, deserialize_with = "null_default")]
    updated_voice_states: Vec<VoiceState>,
    #[serde(default, deserialize_with = "null_default")]
    removed_voice_states: Vec<Snowflake>,
    #[serde(default, deserialize_with = "null_default")]
    updated_members: Vec<MemberWire>,
}

/// Each user once, in first-seen order; a later sighting replaces the earlier.
#[derive(Default)]
struct UserTable {
    index: HashMap<Snowflake, usize>,
    users: Vec<User>,
}

impl UserTable {
    fn insert(&mut self, user: User) {
        match self.index.get(&user.id) {
            Some(&at) => self.users[at] = user,
            None => {
                self.index.insert(user.id, self.users.len());
                self.users.push(user);
            }
        }
    }

    fn into_users(self) -> Vec<User> {
        self.users
    }
}

#[derive(Default)]
struct Normalizer {
    users: UserTable,
}

enum NormalizedGuild {
    Available(Box<Guild>),
    Unavailable(Snowflake),
}

impl Normalizer {
    fn member(&mut self, wire: MemberWire) -> Option<GuildMember> {
        let user_id = match (wire.user, wire.user_id) {
            (Some(user), _) => {
                let id = user.id;
                self.users.insert(user.into());
                id
            }
            (None, Some(id)) => id,
            (None, None) => return None,
        };
        Some(GuildMember {
            user_id,
            nick: wire.nick,
            roles: wire.roles,
            communication_disabled_until: wire.communication_disabled_until,
        })
    }

    fn members(&mut self, wire: Vec<MemberWire>) -> Vec<GuildMember> {
        wire.into_iter().filter_map(|m| self.member(m)).collect()
    }

    /// Channels reference DM participants by ID; guild channels learn their guild.
    fn channel(&mut self, mut channel: Channel, guild_id: Option<Snowflake>) -> Channel {
        if channel.guild_id.is_none() {
            channel.guild_id = guild_id;
        }
        if !channel.recipients.is_empty() {
            channel.recipient_ids = std::mem::take(&mut channel.recipients)
                .into_iter()
                .map(|user| {
                    let id = user.id;
                    self.users.insert(user);
                    id
                })
                .collect();
        }
        channel
    }

    fn guild(
        &mut self,
        wire: GuildWire,
        merged_members: Option<Vec<MemberWire>>,
    ) -> NormalizedGuild {
        if wire.unavailable {
            return NormalizedGuild::Unavailable(wire.id);
        }
        let properties = wire.properties;
        let id = wire.id;
        let mut members = self.members(wire.members);
        if let Some(merged) = merged_members {
            members.extend(self.members(merged));
        }
        let channels = wire
            .channels
            .into_iter()
            .map(|channel| self.channel(channel, Some(id)))
            .collect();
        NormalizedGuild::Available(Box::new(Guild {
            id,
            name: wire
                .name
                .or_else(|| properties.as_ref().and_then(|p| p.name.clone()))
                .unwrap_or_default(),
            icon: wire
                .icon
                .or_else(|| properties.as_ref().and_then(|p| p.icon.clone())),
            owner_id: wire
                .owner_id
                .or(properties.as_ref().and_then(|p| p.owner_id)),
            roles: wire.roles,
            channels,
            members,
            member_count: wire.member_count,
        }))
    }
}

/// READY with the pieces the connection (not the reducer) needs.
pub(crate) struct DecodedReady {
    pub(crate) ready: Ready,
    pub(crate) resume_gateway_url: String,
    pub(crate) required_action: Option<String>,
}

fn decode_ready(raw: &str) -> Result<DecodedReady, Malformed> {
    let wire: ReadyWire = serde_json::from_str(raw).map_err(|_| Malformed)?;
    if !wire.merged_members.is_empty() && wire.merged_members.len() != wire.guilds.len() {
        return Err(Malformed);
    }
    let mut normalizer = Normalizer::default();
    normalizer.users.insert(wire.user.clone());
    for user in wire.users {
        normalizer.users.insert(user.into());
    }
    let mut merged = wire.merged_members.into_iter();
    let mut guilds = Vec::new();
    let mut unavailable_guilds = Vec::new();
    for guild in wire.guilds {
        match normalizer.guild(guild, merged.next()) {
            NormalizedGuild::Available(guild) => guilds.push(*guild),
            NormalizedGuild::Unavailable(id) => unavailable_guilds.push(id),
        }
    }
    let private_channels = wire
        .private_channels
        .into_iter()
        .map(|channel| normalizer.channel(channel, None))
        .collect();
    Ok(DecodedReady {
        ready: Ready {
            session_id: SessionId(wire.session_id),
            user: wire.user,
            users: normalizer.users.into_users(),
            guilds,
            unavailable_guilds,
            private_channels,
        },
        resume_gateway_url: wire.resume_gateway_url,
        required_action: wire.required_action.filter(|action| !action.is_empty()),
    })
}

fn decode_supplemental(raw: &str) -> Result<ReadySupplemental, Malformed> {
    let wire: SupplementalWire = serde_json::from_str(raw).map_err(|_| Malformed)?;
    if !wire.merged_members.is_empty() && wire.merged_members.len() != wire.guilds.len() {
        return Err(Malformed);
    }
    let mut normalizer = Normalizer::default();
    let mut merged = wire.merged_members.into_iter();
    let guilds = wire
        .guilds
        .into_iter()
        .map(|guild| SupplementalGuild {
            id: guild.id,
            voice_states: guild.voice_states,
            members: merged
                .next()
                .map(|members| normalizer.members(members))
                .unwrap_or_default(),
        })
        .collect();
    let lazy_private_channels = wire
        .lazy_private_channels
        .into_iter()
        .map(|channel| normalizer.channel(channel, None))
        .collect();
    Ok(ReadySupplemental {
        guilds,
        users: normalizer.users.into_users(),
        lazy_private_channels,
    })
}

fn decode_guild_create(raw: &str) -> Result<GuildCreate, Malformed> {
    let wire: GuildWire = serde_json::from_str(raw).map_err(|_| Malformed)?;
    let mut normalizer = Normalizer::default();
    Ok(match normalizer.guild(wire, None) {
        NormalizedGuild::Available(guild) => GuildCreate::Available {
            guild: *guild,
            users: normalizer.users.into_users(),
        },
        NormalizedGuild::Unavailable(id) => GuildCreate::Unavailable(id),
    })
}

fn decode_passive_update(raw: &str) -> Result<PassiveUpdate, Malformed> {
    let wire: PassiveUpdateWire = serde_json::from_str(raw).map_err(|_| Malformed)?;
    let mut normalizer = Normalizer::default();
    let members = normalizer.members(wire.updated_members);
    Ok(PassiveUpdate {
        guild_id: wire.guild_id,
        channels: wire.updated_channels,
        updated_voice_states: wire.updated_voice_states,
        removed_voice_states: wire.removed_voice_states,
        members,
        users: normalizer.users.into_users(),
    })
}

/// The outcome of decoding one dispatch payload.
pub(crate) enum Decoded {
    Ready(Box<DecodedReady>),
    Event(Dispatch),
    /// An event this client has no handler for.
    Unhandled,
    /// A handled event whose payload did not decode.
    Malformed,
}

fn json<'a, T: Deserialize<'a>>(raw: &'a str) -> Result<T, Malformed> {
    serde_json::from_str(raw).map_err(|_| Malformed)
}

pub(crate) fn decode_dispatch(name: &str, raw: &str) -> Decoded {
    let event = match name {
        "READY" => {
            return decode_ready(raw)
                .map_or(Decoded::Malformed, |ready| Decoded::Ready(Box::new(ready)));
        }
        "READY_SUPPLEMENTAL" => {
            decode_supplemental(raw).map(|event| Dispatch::ReadySupplemental(Box::new(event)))
        }
        "RESUMED" => Ok(Dispatch::Resumed),
        "MESSAGE_CREATE" => json::<Message>(raw).map(|m| Dispatch::MessageCreate(Box::new(m))),
        "MESSAGE_UPDATE" => {
            json::<MessageUpdate>(raw).map(|m| Dispatch::MessageUpdate(Box::new(m)))
        }
        "MESSAGE_DELETE" => json::<MessageDelete>(raw).map(Dispatch::MessageDelete),
        "GUILD_CREATE" => decode_guild_create(raw).map(|g| Dispatch::GuildCreate(Box::new(g))),
        "GUILD_DELETE" => json::<GuildDeleteWire>(raw).map(|g| {
            Dispatch::GuildDelete(GuildDelete {
                id: g.id,
                unavailable: g.unavailable,
            })
        }),
        "CHANNEL_CREATE" => json::<Channel>(raw).map(|c| Dispatch::ChannelCreate(Box::new(c))),
        "CHANNEL_UPDATE" => json::<Channel>(raw).map(|c| Dispatch::ChannelUpdate(Box::new(c))),
        "CHANNEL_DELETE" => json::<Channel>(raw).map(|c| Dispatch::ChannelDelete(Box::new(c))),
        "PASSIVE_UPDATE_V2" => {
            decode_passive_update(raw).map(|p| Dispatch::PassiveUpdate(Box::new(p)))
        }
        _ => return Decoded::Unhandled,
    };
    event.map_or(Decoded::Malformed, Decoded::Event)
}

pub(crate) fn is_ready_family(name: &str) -> bool {
    matches!(name, "READY" | "READY_SUPPLEMENTAL")
}

#[cfg(test)]
mod tests {
    use fastcord_model::ChannelKind;

    use super::*;

    const READY: &str = include_str!("../../../../fixtures/gateway/ready.json");
    const SUPPLEMENTAL: &str = include_str!("../../../../fixtures/gateway/ready_supplemental.json");
    const READY_NON_DEDUPED: &str =
        include_str!("../../../../fixtures/gateway/ready_non_deduped.json");
    const PASSIVE: &str = include_str!("../../../../fixtures/gateway/passive_update_v2.json");

    fn id(value: u64) -> Snowflake {
        Snowflake(value)
    }

    fn ready(raw: &str) -> DecodedReady {
        match decode_dispatch("READY", raw) {
            Decoded::Ready(ready) => *ready,
            _ => panic!("READY did not decode"),
        }
    }

    #[test]
    fn deduplicated_ready_is_normalized_into_entities_stored_once() {
        let decoded = ready(READY);
        let ready = &decoded.ready;
        assert_eq!(ready.session_id.as_str(), "fixture-session-id-0001");
        assert_eq!(
            decoded.resume_gateway_url,
            "wss://gateway-us-east1-b.discord.gg"
        );
        assert_eq!(decoded.required_action, None);
        assert_eq!(ready.user.id, id(175_928_847_299_117_063));
        assert_eq!(ready.user.display_name(), "Alt Fixture");

        // Each user exactly once, current user included, in first-seen order.
        let ids: Vec<u64> = ready.users.iter().map(|u| u.id.0).collect();
        assert_eq!(
            ids,
            [
                175_928_847_299_117_063,
                80_351_110_224_678_912,
                53_908_232_506_183_680,
                90_101_010_101_010_101
            ]
        );
        assert!(ready.users[3].bot);

        // The unavailable guild is known by ID only; order of the others kept.
        assert_eq!(ready.unavailable_guilds, [id(41_771_983_423_143_940)]);
        assert_eq!(ready.guilds.len(), 2);
        let first = &ready.guilds[0];
        assert_eq!(first.name, "Test Server");
        assert_eq!(first.owner_id, Some(id(80_351_110_224_678_912)));
        assert_eq!(first.member_count, 42);
        assert_eq!(first.roles.len(), 2);
        assert!(
            first.roles[1]
                .permissions
                .contains(fastcord_model::Permissions::ADMINISTRATOR)
        );
        // Channels learn their guild; unknown channel types survive.
        assert!(first.channels.iter().all(|c| c.guild_id == Some(first.id)));
        assert_eq!(first.channels[2].kind, ChannelKind::GuildForum);
        assert_eq!(first.channels[1].permission_overwrites.len(), 1);
        assert_eq!(
            first.channels[0].last_message_id,
            Some(id(175_928_847_299_117_100))
        );

        // merged_members is parallel to guilds: the unavailable guild's empty
        // slot must not shift the last guild's member onto the wrong guild.
        assert_eq!(first.members.len(), 1);
        assert_eq!(first.members[0].user_id, ready.user.id);
        assert_eq!(first.members[0].nick.as_deref(), Some("Altie"));
        assert_eq!(first.members[0].roles, [id(41_771_983_444_444_444)]);
        let second = &ready.guilds[1];
        assert_eq!(second.name, "Second Server");
        assert_eq!(second.icon.as_deref(), Some("a_icon_hash"));
        assert_eq!(second.members.len(), 1);
        assert_eq!(
            second.members[0].communication_disabled_until.as_deref(),
            Some("2099-01-01T00:00:00.000000+00:00")
        );

        // Private channels reference users by ID.
        assert_eq!(ready.private_channels.len(), 2);
        assert_eq!(ready.private_channels[0].kind, ChannelKind::Dm);
        assert_eq!(
            ready.private_channels[0].recipient_ids,
            [id(80_351_110_224_678_912)]
        );
        assert!(ready.private_channels[0].recipients.is_empty());
        assert_eq!(ready.private_channels[1].kind, ChannelKind::GroupDm);
        assert_eq!(ready.private_channels[1].recipient_ids.len(), 2);
    }

    #[test]
    fn ready_without_deduplication_normalizes_to_the_same_shape() {
        let decoded = ready(READY_NON_DEDUPED);
        let ready = &decoded.ready;
        assert_eq!(ready.guilds.len(), 1);
        // `properties` (client-state v2 shape) and merged-in properties both work.
        assert_eq!(ready.guilds[0].name, "Nested Server");
        assert_eq!(ready.guilds[0].owner_id, Some(id(80_351_110_224_678_912)));
        // Embedded user objects were moved into the table; members and DM
        // recipients keep only IDs.
        let ids: Vec<u64> = ready.users.iter().map(|u| u.id.0).collect();
        assert_eq!(ids, [175_928_847_299_117_063, 80_351_110_224_678_912]);
        assert_eq!(ready.guilds[0].members.len(), 2);
        assert_eq!(
            ready.private_channels[0].recipient_ids,
            [id(80_351_110_224_678_912)]
        );
        assert!(ready.private_channels[0].recipients.is_empty());
        assert_eq!(
            decoded.resume_gateway_url,
            "wss://gateway-us-east1-c.discord.gg"
        );
    }

    #[test]
    fn null_and_missing_arrays_are_empty_and_required_action_is_surfaced() {
        let raw = r#"{"user":{"id":"1","username":"a"},"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg","guilds":null,"users":null,"private_channels":null,"merged_members":null,"required_action":"AGREE_TO_TERMS"}"#;
        let decoded = ready(raw);
        assert!(decoded.ready.guilds.is_empty() && decoded.ready.private_channels.is_empty());
        assert_eq!(
            decoded.ready.users.len(),
            1,
            "the current user is always known"
        );
        assert_eq!(decoded.required_action.as_deref(), Some("AGREE_TO_TERMS"));
        let blank = raw.replace("AGREE_TO_TERMS", "");
        assert_eq!(ready(&blank).required_action, None);
    }

    #[test]
    fn ready_with_misaligned_members_or_missing_session_is_malformed() {
        // Two guilds but one members list: attaching by position would be wrong.
        let raw = r#"{"user":{"id":"1","username":"a"},"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg","guilds":[{"id":"1"},{"id":"2"}],"merged_members":[[]]}"#;
        assert!(matches!(decode_dispatch("READY", raw), Decoded::Malformed));
        for raw in [
            "{}",
            r#"{"user":{"id":"1","username":"a"}}"#,
            r#"{"user":{"id":"x","username":"a"},"session_id":"s","resume_gateway_url":"u"}"#,
            "not json",
        ] {
            assert!(
                matches!(decode_dispatch("READY", raw), Decoded::Malformed),
                "{raw}"
            );
        }
    }

    #[test]
    fn ready_supplemental_carries_voice_states_members_and_lazy_channels() {
        let Decoded::Event(Dispatch::ReadySupplemental(supplemental)) =
            decode_dispatch("READY_SUPPLEMENTAL", SUPPLEMENTAL)
        else {
            panic!("READY_SUPPLEMENTAL did not decode");
        };
        assert_eq!(supplemental.guilds.len(), 3);
        let first = &supplemental.guilds[0];
        assert_eq!(first.id, id(41_771_983_423_143_937));
        assert_eq!(first.voice_states.len(), 1);
        let voice = &first.voice_states[0];
        assert_eq!(voice.user_id, id(80_351_110_224_678_912));
        assert_eq!(voice.channel_id, Some(id(41_771_983_423_143_939)));
        assert!(voice.self_mute && !voice.self_deaf);
        // A member that embeds its user gets its ID and the user is reported once.
        assert_eq!(first.members.len(), 3);
        assert_eq!(first.members[2].user_id, id(60_606_060_606_060_606));
        assert_eq!(supplemental.users.len(), 2);
        assert!(supplemental.users.iter().any(|u| u.username == "newcomer"));
        // The unavailable guild has neither.
        assert!(supplemental.guilds[1].voice_states.is_empty());
        assert!(supplemental.guilds[1].members.is_empty());
        assert_eq!(
            supplemental.guilds[2].members[0].user_id,
            id(80_351_110_224_678_912)
        );
        // Lazy private channels, ID-referenced like READY's.
        assert_eq!(supplemental.lazy_private_channels.len(), 2);
        assert_eq!(
            supplemental.lazy_private_channels[1].recipient_ids,
            [id(70_707_070_707_070_707)]
        );
    }

    #[test]
    fn supplemental_with_misaligned_members_is_malformed() {
        let raw = r#"{"guilds":[{"id":"1"},{"id":"2"}],"merged_members":[[],[],[]]}"#;
        assert!(matches!(
            decode_dispatch("READY_SUPPLEMENTAL", raw),
            Decoded::Malformed
        ));
    }

    #[test]
    fn passive_update_v2_is_decoded_for_unsubscribed_guilds() {
        let Decoded::Event(Dispatch::PassiveUpdate(update)) =
            decode_dispatch("PASSIVE_UPDATE_V2", PASSIVE)
        else {
            panic!("PASSIVE_UPDATE_V2 did not decode");
        };
        assert_eq!(update.guild_id, id(41_771_983_423_143_941));
        assert_eq!(update.channels.len(), 2);
        assert_eq!(
            update.channels[0].last_message_id,
            Some(id(175_928_847_299_200_000))
        );
        assert_eq!(update.channels[1].last_message_id, None);
        assert_eq!(update.updated_voice_states.len(), 1);
        assert_eq!(update.removed_voice_states, [id(53_908_232_506_183_680)]);
        assert_eq!(update.members[0].nick.as_deref(), Some("N"));
        assert_eq!(update.users.len(), 1);
    }

    #[test]
    fn guild_create_available_and_unavailable() {
        let Decoded::Event(Dispatch::GuildCreate(created)) = decode_dispatch(
            "GUILD_CREATE",
            r#"{"id":"5","name":"G","owner_id":"6","roles":[],"channels":[{"id":"7","type":0}],"members":[{"user":{"id":"6","username":"o"},"roles":[]}]}"#,
        ) else {
            panic!("GUILD_CREATE did not decode");
        };
        let GuildCreate::Available { guild, users } = *created else {
            panic!("expected an available guild");
        };
        assert_eq!(guild.channels[0].guild_id, Some(id(5)));
        assert_eq!(guild.members[0].user_id, id(6));
        assert_eq!(users.len(), 1);
        let Decoded::Event(Dispatch::GuildCreate(outage)) =
            decode_dispatch("GUILD_CREATE", r#"{"id":"5","unavailable":true}"#)
        else {
            panic!("outage GUILD_CREATE did not decode");
        };
        assert_eq!(*outage, GuildCreate::Unavailable(id(5)));
    }

    #[test]
    fn live_events_decode_and_unknown_or_malformed_ones_are_contained() {
        let message = include_str!("../../../../fixtures/model/message_create.json");
        let Decoded::Event(Dispatch::MessageCreate(created)) =
            decode_dispatch("MESSAGE_CREATE", message)
        else {
            panic!("MESSAGE_CREATE did not decode");
        };
        assert_eq!(created.content, "hello <:wave:777>");
        let partial = include_str!("../../../../fixtures/model/message_update_partial.json");
        assert!(matches!(
            decode_dispatch("MESSAGE_UPDATE", partial),
            Decoded::Event(Dispatch::MessageUpdate(_))
        ));
        let Decoded::Event(Dispatch::MessageDelete(deleted)) = decode_dispatch(
            "MESSAGE_DELETE",
            r#"{"id":"1","channel_id":"2","guild_id":"3"}"#,
        ) else {
            panic!("MESSAGE_DELETE did not decode");
        };
        assert_eq!(deleted.guild_id, Some(id(3)));
        assert!(matches!(
            decode_dispatch("GUILD_DELETE", r#"{"id":"3"}"#),
            Decoded::Event(Dispatch::GuildDelete(GuildDelete {
                unavailable: false,
                ..
            }))
        ));
        assert!(matches!(
            decode_dispatch("CHANNEL_DELETE", r#"{"id":"3","type":0}"#),
            Decoded::Event(Dispatch::ChannelDelete(_))
        ));
        assert!(matches!(
            decode_dispatch("TYPING_START", "{}"),
            Decoded::Unhandled
        ));
        assert!(matches!(
            decode_dispatch("MESSAGE_CREATE", r#"{"id":"1"}"#),
            Decoded::Malformed
        ));
        assert!(is_ready_family("READY") && is_ready_family("READY_SUPPLEMENTAL"));
        assert!(!is_ready_family("MESSAGE_CREATE"));
    }

    #[test]
    fn debug_output_never_contains_content_or_session_identifiers() {
        let message = include_str!("../../../../fixtures/model/message_create.json");
        let Decoded::Event(event) = decode_dispatch("MESSAGE_CREATE", message) else {
            panic!("MESSAGE_CREATE did not decode");
        };
        let shown = format!("{event:?}");
        assert_eq!(shown, "Dispatch::MESSAGE_CREATE");
        assert!(!shown.contains("hello") && !shown.contains("SIGNED"));
        let decoded = ready(READY);
        let shown = format!("{:?} {:?}", decoded.ready, decoded.ready.session_id);
        assert!(!shown.contains("fixture-session-id-0001"));
        assert!(!shown.contains("alt_fixture"));
    }

    #[test]
    fn partial_user_objects_do_not_sink_the_whole_ready() {
        let raw = r#"{"user":{"id":"1","username":"a"},"session_id":"s","resume_gateway_url":"wss://gateway.discord.gg","users":[{"id":"2"},{"id":"3","username":null,"bot":null,"global_name":null,"avatar":null}],"guilds":[{"id":"9","name":"G","owner_id":"1","members":[{"user":{"id":"4"},"roles":null}]}]}"#;
        let decoded = ready(raw);
        let users = &decoded.ready.users;
        assert_eq!(users.len(), 4);
        assert_eq!((users[1].id, users[1].username.as_str()), (id(2), ""));
        assert!(!users[2].bot);
        assert_eq!(decoded.ready.guilds[0].members[0].user_id, id(4));
        assert!(decoded.ready.guilds[0].members[0].roles.is_empty());
    }
}
