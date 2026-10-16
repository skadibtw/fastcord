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
//!
//! The one exception to typed streaming is the account's read cursors and mute
//! settings, which are read from a transient JSON value so that an unexpected
//! shape drops the unread markers instead of failing the whole session.

use std::collections::HashMap;

use fastcord_model::{
    CallCreate, CallDelete, CallUpdate, Channel, Emoji, Guild, GuildEmoji, GuildMember,
    MAX_REACTION_EMOJI_NAME_BYTES, Message, MessageUpdate, Relationship, RelationshipKind, Role,
    Snowflake, StreamCreate, StreamDelete, StreamServerUpdate, StreamUpdate, User,
    VoiceServerUpdate, VoiceState,
};
use serde::{Deserialize, Deserializer};

use super::event::{
    ChannelRecipientAdd, ChannelRecipientRemove, ChannelUnread, Dispatch, GroupId, GuildCreate,
    GuildDelete, GuildMemberEvent, GuildMemberRemove, GuildMuteUpdate, GuildRoleDelete,
    GuildRoleEvent, GuildUpdate, ListGroup, ListRow, MAX_RELATIONSHIPS, MemberListId, MemberListOp,
    MemberListUpdate, MessageAck, MessageDelete, MessageDeleteBulk, MessageReaction,
    MessageReactionRemoveAll, MessageReactionRemoveEmoji, MuteSettings, PassiveUpdate,
    ReadStateEntry, Ready, ReadyRead, ReadySupplemental, RelationshipUpdate, SessionId,
    SupplementalGuild, VoiceStateUpdate,
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
    #[serde(default)]
    roles: Option<Vec<Snowflake>>,
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
    emojis: Vec<GuildEmoji>,
    #[serde(default, deserialize_with = "null_default")]
    members: Vec<MemberWire>,
    #[serde(default)]
    member_count: u32,
}

#[derive(Deserialize)]
struct GuildUpdateWire {
    id: Snowflake,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    icon: Option<String>,
    #[serde(default)]
    owner_id: Option<Snowflake>,
    #[serde(default, deserialize_with = "null_default")]
    roles: Vec<Role>,
    #[serde(default)]
    member_count: Option<u32>,
}

#[derive(Deserialize)]
struct GuildUpdatePresence {
    #[serde(default, deserialize_with = "field_present")]
    name: bool,
    #[serde(default, deserialize_with = "field_present")]
    icon: bool,
    #[serde(default, deserialize_with = "field_present")]
    owner_id: bool,
    #[serde(default, deserialize_with = "field_present")]
    roles: bool,
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
    /// Read entry by entry (see `relationship_of`): one odd entry costs only
    /// itself, never the session.
    #[serde(default, deserialize_with = "null_default")]
    relationships: Vec<serde_json::Value>,
    /// Read cursors and mutes are read leniently (see `read_entries`): a
    /// surprising shape costs the markers, never the session.
    #[serde(default)]
    read_state: Option<serde_json::Value>,
    #[serde(default)]
    user_guild_settings: Option<serde_json::Value>,
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

/// GUILD_MEMBER_ADD / GUILD_MEMBER_UPDATE: the member fields sit beside `guild_id`.
#[derive(Deserialize)]
struct GuildIdWire {
    guild_id: Snowflake,
    #[serde(default)]
    user: UserPresence,
    #[serde(default, deserialize_with = "field_present")]
    nick: bool,
    #[serde(default, deserialize_with = "field_present")]
    roles: bool,
    #[serde(default, deserialize_with = "field_present")]
    communication_disabled_until: bool,
}

fn field_present<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    serde::de::IgnoredAny::deserialize(deserializer)?;
    Ok(true)
}

#[derive(Default, Deserialize)]
struct UserPresence {
    #[serde(default, deserialize_with = "field_present")]
    username: bool,
    #[serde(default, deserialize_with = "field_present")]
    global_name: bool,
    #[serde(default, deserialize_with = "field_present")]
    avatar: bool,
    #[serde(default, deserialize_with = "field_present")]
    bot: bool,
}

#[derive(Deserialize)]
struct GuildMemberRemoveWire {
    guild_id: Snowflake,
    user: WireUser,
}
#[derive(Deserialize)]
struct ChannelRecipientRemoveWire {
    channel_id: Snowflake,
    user: WireUser,
}

/// The partial emoji of a reaction event.
#[derive(Deserialize)]
struct ReactionEmojiWire {
    #[serde(default)]
    id: Option<Snowflake>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    animated: Option<bool>,
}

/// MESSAGE_REACTION_ADD and MESSAGE_REACTION_REMOVE. The reacting `member`,
/// `message_author_id`, `burst_colors`, and everything else are skipped as read.
#[derive(Deserialize)]
struct ReactionWire {
    user_id: Snowflake,
    channel_id: Snowflake,
    message_id: Snowflake,
    #[serde(default)]
    guild_id: Option<Snowflake>,
    emoji: ReactionEmojiWire,
    /// The reaction type: `0` (or absent) is a normal reaction, anything else
    /// a burst reaction.
    #[serde(default, rename = "type")]
    kind: Option<i64>,
    #[serde(default)]
    burst: Option<bool>,
}

#[derive(Deserialize)]
struct ReactionEmojiRemovedWire {
    channel_id: Snowflake,
    message_id: Snowflake,
    #[serde(default)]
    guild_id: Option<Snowflake>,
    emoji: ReactionEmojiWire,
}

#[derive(Deserialize)]
struct VoiceMemberWire {
    #[serde(default)]
    member: Option<MemberWire>,
}

#[derive(Deserialize)]
struct ListGroupWire {
    id: String,
    #[serde(default)]
    count: u32,
}

impl From<ListGroupWire> for ListGroup {
    fn from(wire: ListGroupWire) -> Self {
        let id = match wire.id.as_str() {
            "online" => GroupId::Online,
            "offline" => GroupId::Offline,
            other => other
                .parse::<Snowflake>()
                .map_or(GroupId::Other, GroupId::Role),
        };
        Self {
            id,
            count: wire.count,
        }
    }
}

/// A row is `{"group": {...}}` or `{"member": {...}}`.
#[derive(Default, Deserialize)]
struct ListItemWire {
    #[serde(default)]
    group: Option<ListGroupWire>,
    #[serde(default)]
    member: Option<MemberWire>,
}

#[derive(Deserialize)]
#[serde(tag = "op")]
enum ListOpWire {
    #[serde(rename = "SYNC")]
    Sync {
        range: (u32, u32),
        #[serde(default, deserialize_with = "null_default")]
        items: Vec<ListItemWire>,
    },
    #[serde(rename = "INSERT")]
    Insert {
        index: u32,
        #[serde(default)]
        item: ListItemWire,
    },
    #[serde(rename = "UPDATE")]
    Update {
        index: u32,
        #[serde(default)]
        item: ListItemWire,
    },
    #[serde(rename = "DELETE")]
    Delete { index: u32 },
    #[serde(rename = "INVALIDATE")]
    Invalidate { range: (u32, u32) },
    #[serde(other)]
    Unknown,
}

#[derive(Deserialize)]
struct MemberListWire {
    id: String,
    guild_id: Snowflake,
    #[serde(default)]
    member_count: Option<u32>,
    #[serde(default)]
    online_count: Option<u32>,
    #[serde(default)]
    groups: Option<Vec<ListGroupWire>>,
    #[serde(default, deserialize_with = "null_default")]
    ops: Vec<ListOpWire>,
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
            roles_known: wire.roles.is_some(),
            roles: wire.roles.unwrap_or_default(),
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
            emojis: wire.emojis,
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
    let relationships = wire
        .relationships
        .iter()
        .filter_map(relationship_of)
        .take(MAX_RELATIONSHIPS)
        .collect();
    let read = ReadyRead {
        entries: wire
            .read_state
            .as_ref()
            .map(read_entries)
            .unwrap_or_default(),
        mutes: wire
            .user_guild_settings
            .as_ref()
            .map(mute_settings)
            .unwrap_or_default(),
    };
    Ok(DecodedReady {
        ready: Ready {
            read,
            relationships,
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

/// `{"id", "type", "user": {...}}`; the entry's `user` is required (a
/// relationship is shown by name), its `id` must agree, and types other than
/// 1 to 4 are not kept.
fn relationship_of(value: &serde_json::Value) -> Option<Relationship> {
    let kind = RelationshipKind::from_wire(value.get("type")?.as_u64()?)?;
    let user: User = WireUser::deserialize(value.get("user")?).ok()?.into();
    if user.id.0 == 0 || snowflake_of(value.get("id")?)? != user.id {
        return None;
    }
    Some(Relationship { user, kind })
}

fn snowflake_of(value: &serde_json::Value) -> Option<Snowflake> {
    match value {
        serde_json::Value::String(text) => text.parse().ok(),
        serde_json::Value::Number(number) => number.as_u64().map(Snowflake),
        _ => None,
    }
    .filter(|id: &Snowflake| id.0 != 0)
}

/// The array of entries of a READY settings object, which Discord has sent
/// both as `{"entries": [...]}` and as a bare array.
fn entries_of(value: &serde_json::Value) -> &[serde_json::Value] {
    value
        .get("entries")
        .unwrap_or(value)
        .as_array()
        .map_or(&[], Vec::as_slice)
}

/// Read cursors from `read_state`. An entry whose ids do not parse is skipped.
fn read_entries(value: &serde_json::Value) -> Vec<ReadStateEntry> {
    entries_of(value)
        .iter()
        .filter_map(|entry| {
            let channel_id = snowflake_of(entry.get("id")?)?;
            let last_acked = entry
                .get("last_acked_id")
                .filter(|id| !id.is_null())
                .or_else(|| entry.get("last_message_id"))
                .and_then(snowflake_of)?;
            let mention_count = entry
                .get("mention_count")
                .and_then(serde_json::Value::as_u64)
                .map_or(0, |n| n.min(u64::from(u32::MAX)) as u32);
            Some(ReadStateEntry {
                channel_id,
                last_acked,
                mention_count,
            })
        })
        .collect()
}

/// Whether a mute is in force: `muted` set and `mute_config.end_time`, when
/// there is one, not yet passed.
fn mute_active(entry: &serde_json::Value) -> bool {
    if entry.get("muted") != Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    let end = entry
        .get("mute_config")
        .and_then(|config| config.get("end_time"))
        .and_then(serde_json::Value::as_str)
        .and_then(|text| {
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).ok()
        });
    end.is_none_or(|end| end > time::OffsetDateTime::now_utc())
}

fn mute_settings(value: &serde_json::Value) -> MuteSettings {
    let mut mutes = MuteSettings::default();
    for entry in entries_of(value) {
        if let Some(update) = mute_update(entry) {
            if let Some(guild) = update.guild_id.filter(|_| update.guild_muted) {
                mutes.guilds.push(guild);
            }
            mutes.channels.extend(
                update
                    .channels
                    .iter()
                    .filter(|(_, m)| *m)
                    .map(|(id, _)| *id),
            );
        }
    }
    mutes
}

/// One settings entry (READY's or an update's) as a [`GuildMuteUpdate`].
fn mute_update(entry: &serde_json::Value) -> Option<GuildMuteUpdate> {
    let guild_id = entry.get("guild_id").and_then(snowflake_of);
    let channels = entry
        .get("channel_overrides")
        .and_then(serde_json::Value::as_array)
        .map(|overrides| {
            overrides
                .iter()
                .filter_map(|o| Some((snowflake_of(o.get("channel_id")?)?, mute_active(o))))
                .collect()
        })
        .unwrap_or_default();
    entry.is_object().then(|| GuildMuteUpdate {
        guild_id,
        guild_muted: mute_active(entry),
        channels,
    })
}

#[derive(Deserialize)]
struct MessageAckWire {
    channel_id: Snowflake,
    message_id: Snowflake,
    #[serde(default, deserialize_with = "null_default")]
    mention_count: u32,
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

fn decode_guild_update(raw: &str) -> Result<GuildUpdate, Malformed> {
    let wire: GuildUpdateWire = json(raw)?;
    let presence: GuildUpdatePresence = json(raw)?;
    Ok(GuildUpdate {
        id: wire.id,
        name: wire.name,
        name_present: presence.name,
        icon: wire.icon,
        icon_present: presence.icon,
        owner_id: wire.owner_id,
        owner_present: presence.owner_id,
        roles: wire.roles,
        roles_present: presence.roles,
        member_count: wire.member_count,
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

fn decode_member_event(raw: &str) -> Result<GuildMemberEvent, Malformed> {
    let guild: GuildIdWire = json(raw)?;
    let wire: MemberWire = json(raw)?;
    let mut normalizer = Normalizer::default();
    let member = normalizer.member(wire).ok_or(Malformed)?;
    Ok(GuildMemberEvent {
        guild_id: guild.guild_id,
        member,
        nick_present: guild.nick,
        roles_present: guild.roles,
        timeout_present: guild.communication_disabled_until,
        username_present: guild.user.username,
        global_name_present: guild.user.global_name,
        avatar_present: guild.user.avatar,
        bot_present: guild.user.bot,
        users: normalizer.users.into_users(),
    })
}

fn decode_member_remove(raw: &str) -> Result<GuildMemberRemove, Malformed> {
    let wire: GuildMemberRemoveWire = json(raw)?;
    Ok(GuildMemberRemove {
        guild_id: wire.guild_id,
        user_id: wire.user.id,
    })
}
fn decode_channel_recipient_remove(raw: &str) -> Result<ChannelRecipientRemove, Malformed> {
    let wire: ChannelRecipientRemoveWire = json(raw)?;
    Ok(ChannelRecipientRemove {
        channel_id: wire.channel_id,
        user_id: wire.user.id,
    })
}

fn decode_voice_state(raw: &str) -> Result<VoiceStateUpdate, Malformed> {
    let state: VoiceState = json(raw)?;
    let carried: VoiceMemberWire = json(raw)?;
    let mut normalizer = Normalizer::default();
    let member = carried.member.and_then(|member| normalizer.member(member));
    Ok(VoiceStateUpdate {
        state,
        member,
        users: normalizer.users.into_users(),
    })
}

fn decode_member_list(raw: &str) -> Result<MemberListUpdate, Malformed> {
    let wire: MemberListWire = json(raw)?;
    let mut normalizer = Normalizer::default();
    let mut members = Vec::new();
    let mut row = |item: ListItemWire, normalizer: &mut Normalizer| -> ListRow {
        if let Some(group) = item.group {
            return ListRow::Group(group.into());
        }
        match item.member.and_then(|member| normalizer.member(member)) {
            Some(member) => {
                let id = member.user_id;
                members.push(member);
                ListRow::Member(id)
            }
            None => ListRow::Unreadable,
        }
    };
    let ops = wire
        .ops
        .into_iter()
        .map(|op| match op {
            ListOpWire::Sync { range, items } => MemberListOp::Sync {
                start: range.0,
                end: range.1,
                rows: items
                    .into_iter()
                    .map(|item| row(item, &mut normalizer))
                    .collect(),
            },
            ListOpWire::Insert { index, item } => MemberListOp::Insert {
                index,
                row: row(item, &mut normalizer),
            },
            ListOpWire::Update { index, item } => MemberListOp::Update {
                index,
                row: row(item, &mut normalizer),
            },
            ListOpWire::Delete { index } => MemberListOp::Delete { index },
            ListOpWire::Invalidate { range } => MemberListOp::Invalidate {
                start: range.0,
                end: range.1,
            },
            ListOpWire::Unknown => MemberListOp::Unknown,
        })
        .collect();
    Ok(MemberListUpdate {
        guild_id: wire.guild_id,
        list_id: MemberListId(wire.id),
        member_count: wire.member_count,
        online_count: wire.online_count,
        groups: wire
            .groups
            .map(|groups| groups.into_iter().map(ListGroup::from).collect()),
        ops,
        members,
        users: normalizer.users.into_users(),
    })
}

/// A reaction emoji must identify a reaction (a custom emoji by a nonzero ID, a
/// Unicode emoji by its name) and carry a name short enough for the request
/// path it may be put in. Anything else cannot be applied or answered, so the
/// event is skipped as malformed rather than guessed at.
fn reaction_emoji(wire: ReactionEmojiWire) -> Result<Emoji, Malformed> {
    let identified = wire.id.is_some_and(|id| id.0 != 0)
        || wire.name.as_deref().is_some_and(|name| !name.is_empty());
    let fits = wire
        .name
        .as_ref()
        .is_none_or(|name| name.len() <= MAX_REACTION_EMOJI_NAME_BYTES);
    if !identified || !fits {
        return Err(Malformed);
    }
    Ok(Emoji {
        id: wire.id,
        name: wire.name,
        animated: wire.animated.unwrap_or(false),
    })
}

fn decode_reaction(raw: &str) -> Result<MessageReaction, Malformed> {
    let wire: ReactionWire = json(raw)?;
    Ok(MessageReaction {
        channel_id: wire.channel_id,
        message_id: wire.message_id,
        guild_id: wire.guild_id,
        user_id: wire.user_id,
        emoji: reaction_emoji(wire.emoji)?,
        burst: wire.kind.is_some_and(|kind| kind != 0) || wire.burst == Some(true),
    })
}

fn decode_reaction_emoji_removed(raw: &str) -> Result<MessageReactionRemoveEmoji, Malformed> {
    let wire: ReactionEmojiRemovedWire = json(raw)?;
    Ok(MessageReactionRemoveEmoji {
        channel_id: wire.channel_id,
        message_id: wire.message_id,
        guild_id: wire.guild_id,
        emoji: reaction_emoji(wire.emoji)?,
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
        "MESSAGE_DELETE_BULK" => json::<MessageDeleteBulk>(raw).map(Dispatch::MessageDeleteBulk),
        "MESSAGE_ACK" => json::<MessageAckWire>(raw).map(|ack| {
            Dispatch::MessageAck(MessageAck {
                channel_id: ack.channel_id,
                message_id: ack.message_id,
                mention_count: ack.mention_count,
            })
        }),
        "USER_GUILD_SETTINGS_UPDATE" => json::<serde_json::Value>(raw).and_then(|value| {
            mute_update(&value)
                .map(|update| Dispatch::UserGuildSettingsUpdate(Box::new(update)))
                .ok_or(Malformed)
        }),
        "RELATIONSHIP_ADD" => json::<serde_json::Value>(raw).and_then(|value| {
            relationship_of(&value)
                .map(|relationship| Dispatch::RelationshipAdd(Box::new(relationship)))
                .ok_or(Malformed)
        }),
        "RELATIONSHIP_UPDATE" => json::<serde_json::Value>(raw).and_then(|value| {
            Ok(Dispatch::RelationshipUpdate(RelationshipUpdate {
                id: snowflake_of(value.get("id").ok_or(Malformed)?).ok_or(Malformed)?,
                kind: value
                    .get("type")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(RelationshipKind::from_wire),
            }))
        }),
        "RELATIONSHIP_REMOVE" => json::<serde_json::Value>(raw).and_then(|value| {
            snowflake_of(value.get("id").ok_or(Malformed)?)
                .map(Dispatch::RelationshipRemove)
                .ok_or(Malformed)
        }),
        "MESSAGE_REACTION_ADD" => {
            decode_reaction(raw).map(|r| Dispatch::MessageReactionAdd(Box::new(r)))
        }
        "MESSAGE_REACTION_REMOVE" => {
            decode_reaction(raw).map(|r| Dispatch::MessageReactionRemove(Box::new(r)))
        }
        "MESSAGE_REACTION_REMOVE_ALL" => {
            json::<MessageReactionRemoveAll>(raw).map(Dispatch::MessageReactionRemoveAll)
        }
        "MESSAGE_REACTION_REMOVE_EMOJI" => decode_reaction_emoji_removed(raw)
            .map(|r| Dispatch::MessageReactionRemoveEmoji(Box::new(r))),
        "GUILD_CREATE" => decode_guild_create(raw).map(|g| Dispatch::GuildCreate(Box::new(g))),
        "GUILD_UPDATE" => decode_guild_update(raw).map(|g| Dispatch::GuildUpdate(Box::new(g))),
        "GUILD_EMOJIS_UPDATE" => json::<super::GuildEmojisUpdate>(raw)
            .map(|event| Dispatch::GuildEmojisUpdate(Box::new(event))),
        "GUILD_DELETE" => json::<GuildDeleteWire>(raw).map(|g| {
            Dispatch::GuildDelete(GuildDelete {
                id: g.id,
                unavailable: g.unavailable,
            })
        }),
        "CHANNEL_CREATE" => json::<Channel>(raw).map(|c| Dispatch::ChannelCreate(Box::new(c))),
        "CHANNEL_UPDATE" => json::<Channel>(raw).map(|c| Dispatch::ChannelUpdate(Box::new(c))),
        "CHANNEL_DELETE" => json::<Channel>(raw).map(|c| Dispatch::ChannelDelete(Box::new(c))),
        "CHANNEL_RECIPIENT_ADD" => {
            json::<ChannelRecipientAdd>(raw).map(|c| Dispatch::ChannelRecipientAdd(Box::new(c)))
        }
        "CHANNEL_RECIPIENT_REMOVE" => decode_channel_recipient_remove(raw)
            .map(|event| Dispatch::ChannelRecipientRemove(Box::new(event))),
        "PASSIVE_UPDATE_V2" => {
            decode_passive_update(raw).map(|p| Dispatch::PassiveUpdate(Box::new(p)))
        }
        "GUILD_MEMBER_LIST_UPDATE" => {
            decode_member_list(raw).map(|u| Dispatch::MemberListUpdate(Box::new(u)))
        }
        "GUILD_MEMBER_ADD" => {
            decode_member_event(raw).map(|m| Dispatch::GuildMemberAdd(Box::new(m)))
        }
        "GUILD_MEMBER_UPDATE" => {
            decode_member_event(raw).map(|m| Dispatch::GuildMemberUpdate(Box::new(m)))
        }
        "GUILD_MEMBER_REMOVE" => decode_member_remove(raw).map(Dispatch::GuildMemberRemove),
        "GUILD_ROLE_CREATE" => {
            json::<GuildRoleEvent>(raw).map(|r| Dispatch::GuildRoleCreate(Box::new(r)))
        }
        "GUILD_ROLE_UPDATE" => {
            json::<GuildRoleEvent>(raw).map(|r| Dispatch::GuildRoleUpdate(Box::new(r)))
        }
        "GUILD_ROLE_DELETE" => json::<GuildRoleDelete>(raw).map(Dispatch::GuildRoleDelete),
        "VOICE_STATE_UPDATE" => {
            decode_voice_state(raw).map(|v| Dispatch::VoiceStateUpdate(Box::new(v)))
        }
        "CALL_CREATE" => json::<CallCreate>(raw).map(|c| Dispatch::CallCreate(Box::new(c))),
        "CALL_UPDATE" => json::<CallUpdate>(raw).map(Dispatch::CallUpdate),
        "CALL_DELETE" => json::<CallDelete>(raw).map(Dispatch::CallDelete),
        "VOICE_SERVER_UPDATE" => {
            json::<VoiceServerUpdate>(raw).map(|v| Dispatch::VoiceServerUpdate(Box::new(v)))
        }
        "STREAM_CREATE" => json::<StreamCreate>(raw).map(|s| Dispatch::StreamCreate(Box::new(s))),
        "STREAM_SERVER_UPDATE" => {
            json::<StreamServerUpdate>(raw).map(|s| Dispatch::StreamServerUpdate(Box::new(s)))
        }
        "STREAM_UPDATE" => json::<StreamUpdate>(raw).map(|s| Dispatch::StreamUpdate(Box::new(s))),
        "STREAM_DELETE" => json::<StreamDelete>(raw).map(|s| Dispatch::StreamDelete(Box::new(s))),
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
    fn bulk_message_deletes_decode_ids_in_wire_order_and_redact_payloads() {
        let raw = include_str!("../../../../fixtures/gateway/message_delete_bulk.json");
        let Decoded::Event(event) = decode_dispatch("MESSAGE_DELETE_BULK", raw) else {
            panic!("MESSAGE_DELETE_BULK did not decode");
        };
        assert_eq!(event.name(), "MESSAGE_DELETE_BULK");
        assert_eq!(format!("{event:?}"), "Dispatch::MESSAGE_DELETE_BULK");
        let Dispatch::MessageDeleteBulk(deleted) = event else {
            unreachable!();
        };
        assert_eq!(deleted.channel_id, id(500));
        assert_eq!(deleted.guild_id, Some(id(100)));
        assert_eq!(deleted.ids, [id(10), id(12), id(11), id(10)]);
        assert!(matches!(
            decode_dispatch("MESSAGE_DELETE_BULK", r#"{"channel_id":"500","ids":null}"#),
            Decoded::Malformed
        ));
    }

    const REACTION_ADD: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_add.json");
    const REACTION_ADD_CUSTOM: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_add_custom.json");
    const REACTION_ADD_BURST: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_add_burst.json");
    const REACTION_REMOVE: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_remove.json");
    const REACTION_REMOVE_ALL: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_remove_all.json");
    const REACTION_REMOVE_EMOJI: &str =
        include_str!("../../../../fixtures/gateway/message_reaction_remove_emoji.json");
    const REACTED: u64 = 1_290_000_000_000_000_010;

    fn reaction_event(name: &str, raw: &str) -> Dispatch {
        let Decoded::Event(event) = decode_dispatch(name, raw) else {
            panic!("{name} did not decode");
        };
        assert_eq!(event.name(), name);
        event
    }

    fn unicode_emoji(name: &str) -> Emoji {
        Emoji {
            id: None,
            name: Some(name.to_owned()),
            animated: false,
        }
    }

    #[test]
    fn reaction_events_decode_their_fixtures_and_skip_what_they_do_not_use() {
        // A Unicode reaction by another user; the reacting `member` and
        // `message_author_id` are not kept.
        let Dispatch::MessageReactionAdd(added) =
            reaction_event("MESSAGE_REACTION_ADD", REACTION_ADD)
        else {
            panic!("not a reaction add");
        };
        assert_eq!(
            *added,
            MessageReaction {
                channel_id: id(500),
                message_id: id(REACTED),
                guild_id: Some(id(100)),
                user_id: id(3),
                emoji: unicode_emoji("\u{1f44d}"),
                burst: false,
            }
        );

        // An animated custom emoji, explicitly a normal reaction.
        let Dispatch::MessageReactionAdd(custom) =
            reaction_event("MESSAGE_REACTION_ADD", REACTION_ADD_CUSTOM)
        else {
            panic!("not a reaction add");
        };
        assert_eq!(custom.user_id, id(2));
        assert_eq!(
            custom.emoji,
            Emoji {
                id: Some(id(777)),
                name: Some("wave".to_owned()),
                animated: true
            }
        );
        assert!(!custom.burst);

        // A burst reaction (type 1, `burst_colors` skipped), variation selector kept.
        let Dispatch::MessageReactionAdd(burst) =
            reaction_event("MESSAGE_REACTION_ADD", REACTION_ADD_BURST)
        else {
            panic!("not a reaction add");
        };
        assert!(burst.burst);
        assert_eq!(burst.emoji, unicode_emoji("\u{2764}\u{fe0f}"));

        let Dispatch::MessageReactionRemove(removed) =
            reaction_event("MESSAGE_REACTION_REMOVE", REACTION_REMOVE)
        else {
            panic!("not a reaction removal");
        };
        assert_eq!(
            *removed,
            MessageReaction {
                channel_id: id(500),
                message_id: id(REACTED),
                guild_id: Some(id(100)),
                user_id: id(3),
                emoji: unicode_emoji("\u{1f44d}"),
                burst: false,
            }
        );

        let Dispatch::MessageReactionRemoveAll(cleared) =
            reaction_event("MESSAGE_REACTION_REMOVE_ALL", REACTION_REMOVE_ALL)
        else {
            panic!("not a remove-all");
        };
        assert_eq!(
            cleared,
            MessageReactionRemoveAll {
                channel_id: id(500),
                message_id: id(REACTED),
                guild_id: Some(id(100)),
            }
        );

        // An absent `animated` is false.
        let Dispatch::MessageReactionRemoveEmoji(emptied) =
            reaction_event("MESSAGE_REACTION_REMOVE_EMOJI", REACTION_REMOVE_EMOJI)
        else {
            panic!("not a remove-emoji");
        };
        assert_eq!(
            *emptied,
            MessageReactionRemoveEmoji {
                channel_id: id(500),
                message_id: id(REACTED),
                guild_id: Some(id(100)),
                emoji: Emoji {
                    id: Some(id(777)),
                    name: Some("wave".to_owned()),
                    animated: false
                },
            }
        );
    }

    #[test]
    fn a_reaction_is_a_burst_by_its_type_or_its_burst_flag() {
        for (extra, burst) in [
            ("", false),
            (r#","type":0"#, false),
            (r#","type":0,"burst":false"#, false),
            (r#","type":null,"burst":null"#, false),
            (r#","burst":false"#, false),
            (r#","type":1"#, true),
            (r#","type":1,"burst":false"#, true),
            (r#","burst":true"#, true),
            (r#","type":0,"burst":true"#, true),
        ] {
            let raw = format!(
                r#"{{"user_id":"3","channel_id":"500","message_id":"9","emoji":{{"name":"x"}}{extra}}}"#
            );
            for name in ["MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE"] {
                let (Dispatch::MessageReactionAdd(reaction)
                | Dispatch::MessageReactionRemove(reaction)) = reaction_event(name, &raw)
                else {
                    panic!("{name} is not a reaction");
                };
                assert_eq!(reaction.burst, burst, "{name}{extra}");
            }
        }
    }

    #[test]
    fn reaction_payloads_tolerate_null_and_absent_optional_fields_and_unknown_ones() {
        // A DM has no guild: absent and null both mean none.
        for guild in ["", r#","guild_id":null"#] {
            let raw = format!(
                r#"{{"user_id":"3","channel_id":"500","message_id":"9","emoji":{{"name":"x"}}{guild}}}"#
            );
            let Dispatch::MessageReactionAdd(reaction) =
                reaction_event("MESSAGE_REACTION_ADD", &raw)
            else {
                panic!("not a reaction add");
            };
            assert_eq!(reaction.guild_id, None, "{guild}");
        }
        // A deleted custom emoji has no name; null `animated` is false.
        let raw = r#"{"user_id":"3","channel_id":"500","message_id":"9","emoji":{"id":"777","name":null,"animated":null}}"#;
        let Dispatch::MessageReactionAdd(reaction) = reaction_event("MESSAGE_REACTION_ADD", raw)
        else {
            panic!("not a reaction add");
        };
        assert_eq!(
            reaction.emoji,
            Emoji {
                id: Some(id(777)),
                name: None,
                animated: false
            }
        );
        // An explicit null ID beside a name is a Unicode emoji.
        let raw = r#"{"channel_id":"500","message_id":"9","emoji":{"id":null,"name":"x","animated":false}}"#;
        let Dispatch::MessageReactionRemoveEmoji(emptied) =
            reaction_event("MESSAGE_REACTION_REMOVE_EMOJI", raw)
        else {
            panic!("not a remove-emoji");
        };
        assert_eq!(
            (emptied.guild_id, emptied.emoji),
            (None, unicode_emoji("x"))
        );
        // Fields this client does not use, however nested, are skipped.
        let noisy = r##"{"user_id":"3","channel_id":"500","message_id":"9","emoji":{"name":"x","extra":{"deep":[1,2,{"a":null}]}},"member":{"user":{"id":"3","username":"u"},"roles":["1","2"]},"message_author_id":"4","burst_colors":["#fff"],"future":[[[]]]}"##;
        assert!(matches!(
            reaction_event("MESSAGE_REACTION_ADD", noisy),
            Dispatch::MessageReactionAdd(_)
        ));
        let raw = r#"{"channel_id":"500","message_id":"9","reactions_removed":["x"]}"#;
        assert!(matches!(
            reaction_event("MESSAGE_REACTION_REMOVE_ALL", raw),
            Dispatch::MessageReactionRemoveAll(_)
        ));
    }

    #[test]
    fn a_reaction_emoji_that_cannot_identify_a_reaction_makes_the_event_malformed() {
        let long = "x".repeat(MAX_REACTION_EMOJI_NAME_BYTES + 1);
        let accents = "\u{e9}".repeat(MAX_REACTION_EMOJI_NAME_BYTES / 2 + 1);
        let bad = [
            "{}".to_owned(),
            "null".to_owned(),
            r#"{"id":null,"name":null}"#.to_owned(),
            r#"{"id":null,"name":""}"#.to_owned(),
            r#"{"id":"0"}"#.to_owned(),
            r#"{"id":"0","name":""}"#.to_owned(),
            r#"{"id":"0","name":null}"#.to_owned(),
            // The limit is in bytes, whatever the characters.
            format!(r#"{{"name":"{long}"}}"#),
            format!(r#"{{"id":"777","name":"{long}"}}"#),
            format!(r#"{{"name":"{accents}"}}"#),
            r#"{"id":"not a number","name":"x"}"#.to_owned(),
            r#"{"name":5}"#.to_owned(),
        ];
        for emoji in &bad {
            let reaction =
                format!(r#"{{"user_id":"3","channel_id":"500","message_id":"9","emoji":{emoji}}}"#);
            for name in ["MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE"] {
                assert!(
                    matches!(decode_dispatch(name, &reaction), Decoded::Malformed),
                    "{name} {emoji:.40}"
                );
            }
            let emptied = format!(r#"{{"channel_id":"500","message_id":"9","emoji":{emoji}}}"#);
            assert!(
                matches!(
                    decode_dispatch("MESSAGE_REACTION_REMOVE_EMOJI", &emptied),
                    Decoded::Malformed
                ),
                "{emoji:.40}"
            );
        }

        // The limit itself is fine, and so is any emoji that has some identity.
        let at_limit = "x".repeat(MAX_REACTION_EMOJI_NAME_BYTES);
        let accents_at_limit = "\u{e9}".repeat(MAX_REACTION_EMOJI_NAME_BYTES / 2);
        for emoji in [
            format!(r#"{{"name":"{at_limit}"}}"#),
            format!(r#"{{"name":"{accents_at_limit}"}}"#),
            r#"{"id":"777"}"#.to_owned(),
            r#"{"id":"0","name":"x"}"#.to_owned(),
            r#"{"name":"x"}"#.to_owned(),
        ] {
            let reaction =
                format!(r#"{{"user_id":"3","channel_id":"500","message_id":"9","emoji":{emoji}}}"#);
            assert!(
                matches!(
                    decode_dispatch("MESSAGE_REACTION_ADD", &reaction),
                    Decoded::Event(Dispatch::MessageReactionAdd(_))
                ),
                "{emoji:.40}"
            );
        }

        // Everything else the payload must carry.
        for raw in [
            r#"{"channel_id":"500","message_id":"9","emoji":{"name":"x"}}"#,
            r#"{"user_id":"3","message_id":"9","emoji":{"name":"x"}}"#,
            r#"{"user_id":"3","channel_id":"500","emoji":{"name":"x"}}"#,
            r#"{"user_id":"3","channel_id":"500","message_id":"9"}"#,
            r#"{"user_id":"3","channel_id":"500","message_id":"9","emoji":null}"#,
            r#"{"user_id":"x","channel_id":"500","message_id":"9","emoji":{"name":"x"}}"#,
            r#"{"user_id":"3","channel_id":"500","message_id":"9","emoji":{"name":"x"},"type":"1"}"#,
            "[]",
            "",
        ] {
            for name in ["MESSAGE_REACTION_ADD", "MESSAGE_REACTION_REMOVE"] {
                assert!(
                    matches!(decode_dispatch(name, raw), Decoded::Malformed),
                    "{name} {raw}"
                );
            }
        }
        for raw in [
            "{}",
            r#"{"channel_id":"500"}"#,
            r#"{"message_id":"9"}"#,
            r#"{"channel_id":"x","message_id":"9"}"#,
            r#"{"channel_id":"500","message_id":"9""#,
        ] {
            assert!(
                matches!(
                    decode_dispatch("MESSAGE_REACTION_REMOVE_ALL", raw),
                    Decoded::Malformed
                ),
                "{raw}"
            );
        }
        for raw in [
            "{}",
            r#"{"channel_id":"500","message_id":"9"}"#,
            r#"{"message_id":"9","emoji":{"name":"x"}}"#,
            r#"{"channel_id":"500","emoji":{"name":"x"}}"#,
        ] {
            assert!(
                matches!(
                    decode_dispatch("MESSAGE_REACTION_REMOVE_EMOJI", raw),
                    Decoded::Malformed
                ),
                "{raw}"
            );
        }
    }

    #[test]
    fn debounced_reaction_events_stay_unhandled_because_their_capability_is_not_enabled() {
        let raw = r#"{"channel_id":"500","message_id":"9","guild_id":"100","reactions":[{"users":["3","4"],"emoji":{"name":"x"}}]}"#;
        assert!(matches!(
            decode_dispatch("MESSAGE_REACTION_ADD_MANY", raw),
            Decoded::Unhandled
        ));
        assert!(
            crate::gateway::SELECTED_CAPABILITIES
                .iter()
                .all(|capability| !capability.name().contains("DEBOUNCE"))
        );
    }

    #[test]
    fn reaction_event_debug_output_names_the_event_only() {
        for (name, raw) in [
            ("MESSAGE_REACTION_ADD", REACTION_ADD),
            ("MESSAGE_REACTION_ADD", REACTION_ADD_CUSTOM),
            ("MESSAGE_REACTION_ADD", REACTION_ADD_BURST),
            ("MESSAGE_REACTION_REMOVE", REACTION_REMOVE),
            ("MESSAGE_REACTION_REMOVE_ALL", REACTION_REMOVE_ALL),
            ("MESSAGE_REACTION_REMOVE_EMOJI", REACTION_REMOVE_EMOJI),
        ] {
            let shown = format!("{:?}", reaction_event(name, raw));
            assert_eq!(shown, format!("Dispatch::{name}"));
        }
    }

    #[test]
    fn guild_updates_preserve_field_presence_without_decoding_member_or_channel_state() {
        let Decoded::Event(Dispatch::GuildUpdate(update)) = decode_dispatch(
            "GUILD_UPDATE",
            r#"{"id":"100","name":"changed","owner_id":null,"roles":null,"member_count":0,"channels":[{"ignored":true}]}"#,
        ) else {
            panic!("GUILD_UPDATE did not decode");
        };
        assert_eq!(update.id, id(100));
        assert_eq!(update.name.as_deref(), Some("changed"));
        assert!(update.name_present && update.owner_present && update.roles_present);
        assert!(!update.icon_present);
        assert!(update.owner_id.is_none() && update.roles.is_empty());
        assert_eq!(update.member_count, Some(0));
        let Decoded::Event(Dispatch::GuildUpdate(partial)) =
            decode_dispatch("GUILD_UPDATE", r#"{"id":"100"}"#)
        else {
            panic!("partial GUILD_UPDATE did not decode");
        };
        assert!(!partial.name_present && !partial.owner_present && !partial.roles_present);
        assert!(matches!(
            decode_dispatch("GUILD_UPDATE", "{}"),
            Decoded::Malformed
        ));
        assert_eq!(Dispatch::GuildUpdate(partial).name(), "GUILD_UPDATE");
    }

    #[test]
    fn role_events_decode_real_payload_shapes_and_debug_only_names_the_event() {
        let fixtures: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../../fixtures/gateway/guild_role_events.json"
        ))
        .unwrap();
        for fixture in fixtures {
            let name = fixture["t"].as_str().unwrap();
            let Decoded::Event(event) = decode_dispatch(name, &fixture["d"].to_string()) else {
                panic!("{name} did not decode");
            };
            assert_eq!(event.name(), name);
            assert_eq!(format!("{event:?}"), format!("Dispatch::{name}"));
            match event {
                Dispatch::GuildRoleCreate(event) => {
                    assert_eq!(event.guild_id, id(100));
                    assert_eq!(event.role.id, id(103));
                    assert_eq!(
                        event.role.permissions,
                        fastcord_model::Permissions::VIEW_CHANNEL
                    );
                }
                Dispatch::GuildRoleUpdate(event) => {
                    assert_eq!(event.guild_id, id(100));
                    assert_eq!(event.role.id, id(101));
                    assert_eq!(event.role.permissions, fastcord_model::Permissions::NONE);
                }
                Dispatch::GuildRoleDelete(event) => {
                    assert_eq!((event.guild_id, event.role_id), (id(100), id(101)));
                }
                _ => panic!("unexpected role fixture"),
            }
        }
        for name in [
            "GUILD_ROLE_CREATE",
            "GUILD_ROLE_UPDATE",
            "GUILD_ROLE_DELETE",
        ] {
            assert!(matches!(decode_dispatch(name, "{}"), Decoded::Malformed));
        }
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
        assert!(!decoded.ready.guilds[0].members[0].roles_known);
    }

    const LIST_SYNC: &str =
        include_str!("../../../../fixtures/gateway/member_list_update_sync.json");
    const LIST_OPS: &str = include_str!("../../../../fixtures/gateway/member_list_update_ops.json");
    const VOICE_UPDATE: &str = include_str!("../../../../fixtures/gateway/voice_state_update.json");
    const VOICE_SERVER: &str =
        include_str!("../../../../fixtures/gateway/voice_server_update.json");
    const MEMBER_UPDATE: &str =
        include_str!("../../../../fixtures/gateway/guild_member_update.json");

    fn member_list(raw: &str) -> MemberListUpdate {
        match decode_dispatch("GUILD_MEMBER_LIST_UPDATE", raw) {
            Decoded::Event(Dispatch::MemberListUpdate(update)) => *update,
            _ => panic!("GUILD_MEMBER_LIST_UPDATE did not decode"),
        }
    }

    #[test]
    fn member_list_sync_decodes_groups_rows_and_normalizes_members_once() {
        let update = member_list(LIST_SYNC);
        assert_eq!(update.guild_id, id(41_771_983_423_143_937));
        assert_eq!(update.list_id, MemberListId("everyone".to_owned()));
        assert_eq!(
            (update.member_count, update.online_count),
            (Some(4), Some(3))
        );
        let groups: Vec<(GroupId, u32)> = update
            .groups
            .as_ref()
            .unwrap()
            .iter()
            .map(|g| (g.id.clone(), g.count))
            .collect();
        assert_eq!(
            groups,
            [
                (GroupId::Role(id(41_771_983_444_444_444)), 1),
                (GroupId::Online, 2),
                (GroupId::Offline, 1)
            ]
        );
        let [MemberListOp::Sync { start, end, rows }] = update.ops.as_slice() else {
            panic!("expected one SYNC");
        };
        assert_eq!((*start, *end), (0, 99));
        assert_eq!(rows.len(), 7);
        assert!(
            matches!(&rows[0], ListRow::Group(g) if g.id == GroupId::Role(id(41_771_983_444_444_444)))
        );
        assert_eq!(rows[1], ListRow::Member(id(175_928_847_299_117_063)));
        assert!(matches!(&rows[2], ListRow::Group(g) if g.id == GroupId::Online && g.count == 2));
        assert_eq!(rows[6], ListRow::Member(id(60_606_060_606_060_606)));
        // Members once, users once, presence ignored.
        assert_eq!(update.members.len(), 4);
        assert_eq!(update.members[0].nick.as_deref(), Some("Altie"));
        assert_eq!(update.members[0].roles, [id(41_771_983_444_444_444)]);
        assert_eq!(update.users.len(), 4);
    }

    #[test]
    fn member_list_ops_decode_each_kind_and_unknown_ones_are_kept_as_unknown() {
        let update = member_list(LIST_OPS);
        assert!(matches!(update.ops[0], MemberListOp::Delete { index: 3 }));
        assert!(matches!(
            &update.ops[1],
            MemberListOp::Update { index: 3, row: ListRow::Member(user) }
                if *user == id(53_908_232_506_183_680)
        ));
        assert!(matches!(
            &update.ops[2],
            MemberListOp::Update { index: 4, row: ListRow::Group(g) }
                if g.id == GroupId::Offline && g.count == 2
        ));
        assert!(matches!(
            &update.ops[3],
            MemberListOp::Insert { index: 5, .. }
        ));
        assert_eq!(update.members.len(), 2, "UPDATE and INSERT carry members");

        let raw = r#"{"id":"h","guild_id":"1","ops":[{"op":"INVALIDATE","range":[0,99]},{"op":"SHUFFLE","index":1},{"op":"INSERT","index":2},{"op":"SYNC","range":[5,6],"items":[{},{"member":{"roles":[]}}]}]}"#;
        let update = member_list(raw);
        assert_eq!(
            update.ops[0],
            MemberListOp::Invalidate { start: 0, end: 99 }
        );
        assert_eq!(update.ops[1], MemberListOp::Unknown);
        // A row that cannot be read still occupies its index.
        assert_eq!(
            update.ops[2],
            MemberListOp::Insert {
                index: 2,
                row: ListRow::Unreadable
            }
        );
        assert_eq!(
            update.ops[3],
            MemberListOp::Sync {
                start: 5,
                end: 6,
                rows: vec![ListRow::Unreadable, ListRow::Unreadable]
            }
        );
        assert!(update.members.is_empty());
    }

    #[test]
    fn member_list_update_without_its_identity_is_malformed() {
        for raw in [
            r#"{"ops":[]}"#,
            r#"{"id":"x","ops":[]}"#,
            r#"{"id":"x","guild_id":"1","ops":[{"op":"DELETE"}]}"#,
        ] {
            assert!(
                matches!(
                    decode_dispatch("GUILD_MEMBER_LIST_UPDATE", raw),
                    Decoded::Malformed
                ),
                "{raw}"
            );
        }
        let empty = member_list(r#"{"id":"x","guild_id":"1","groups":null,"ops":null}"#);
        assert!(empty.ops.is_empty() && empty.groups.is_none());
    }

    #[test]
    fn guild_member_events_and_voice_state_decode_with_their_member() {
        let Decoded::Event(Dispatch::GuildMemberUpdate(update)) =
            decode_dispatch("GUILD_MEMBER_UPDATE", MEMBER_UPDATE)
        else {
            panic!("GUILD_MEMBER_UPDATE did not decode");
        };
        assert_eq!(update.guild_id, id(41_771_983_423_143_937));
        assert_eq!(update.member.user_id, id(80_351_110_224_678_912));
        assert_eq!(update.member.nick.as_deref(), Some("Nell"));
        assert_eq!(update.users.len(), 1);
        assert!(matches!(
            decode_dispatch("GUILD_MEMBER_ADD", MEMBER_UPDATE),
            Decoded::Event(Dispatch::GuildMemberAdd(_))
        ));
        let Decoded::Event(Dispatch::GuildMemberRemove(removed)) = decode_dispatch(
            "GUILD_MEMBER_REMOVE",
            r#"{"guild_id":"4","user":{"id":"5","username":"gone"}}"#,
        ) else {
            panic!("GUILD_MEMBER_REMOVE did not decode");
        };
        assert_eq!((removed.guild_id, removed.user_id), (id(4), id(5)));
        // A member event without a user cannot be applied to anything.
        assert!(matches!(
            decode_dispatch("GUILD_MEMBER_UPDATE", r#"{"guild_id":"4","roles":[]}"#),
            Decoded::Malformed
        ));

        let Decoded::Event(Dispatch::VoiceStateUpdate(voice)) =
            decode_dispatch("VOICE_STATE_UPDATE", VOICE_UPDATE)
        else {
            panic!("VOICE_STATE_UPDATE did not decode");
        };
        assert_eq!(voice.state.guild_id, Some(id(41_771_983_423_143_937)));
        assert_eq!(voice.state.channel_id, Some(id(41_771_983_423_143_939)));
        assert_eq!(
            voice.member.as_ref().map(|m| m.user_id),
            Some(id(53_908_232_506_183_680))
        );
        assert_eq!(voice.users.len(), 1);
        // Leaving a channel carries no member.
        let Decoded::Event(Dispatch::VoiceStateUpdate(left)) = decode_dispatch(
            "VOICE_STATE_UPDATE",
            r#"{"guild_id":"4","channel_id":null,"user_id":"5","session_id":"s"}"#,
        ) else {
            panic!("VOICE_STATE_UPDATE did not decode");
        };
        assert!(left.state.channel_id.is_none() && left.member.is_none());
    }

    #[test]
    fn channel_recipient_events_decode_their_distinct_wire_shapes() {
        let Decoded::Event(Dispatch::ChannelRecipientAdd(added)) = decode_dispatch(
            "CHANNEL_RECIPIENT_ADD",
            r#"{"channel_id":"300","user":{"id":"42","username":"new","global_name":null,"avatar":null}}"#,
        ) else {
            panic!("CHANNEL_RECIPIENT_ADD did not decode");
        };
        assert_eq!(added.channel_id, id(300));
        assert_eq!(added.user.id, id(42));
        assert_eq!(added.user.username, "new");

        let Decoded::Event(Dispatch::ChannelRecipientRemove(removed)) = decode_dispatch(
            "CHANNEL_RECIPIENT_REMOVE",
            r#"{"channel_id":"300","user":{"id":"42"}}"#,
        ) else {
            panic!("CHANNEL_RECIPIENT_REMOVE did not decode");
        };
        assert_eq!(removed.channel_id, id(300));
        assert_eq!(removed.user_id, id(42));
    }
    #[test]
    fn voice_server_updates_decode_and_never_show_the_token() {
        let Decoded::Event(event) = decode_dispatch("VOICE_SERVER_UPDATE", VOICE_SERVER) else {
            panic!("VOICE_SERVER_UPDATE did not decode");
        };
        assert_eq!(event.name(), "VOICE_SERVER_UPDATE");
        let Dispatch::VoiceServerUpdate(update) = &event else {
            panic!("unexpected event {event:?}");
        };
        assert_eq!(update.token.expose_secret(), "fixture-voice-token");
        assert_eq!(update.guild_id, Some(id(41_771_983_423_143_937)));
        assert_eq!(update.channel_id, None);
        assert_eq!(
            update.endpoint.as_deref(),
            Some("fixture.discord.media:443")
        );
        let shown = format!("{event:?} {update:?}");
        assert!(!shown.contains("fixture-voice-token"), "{shown}");

        // The voice server went away: disconnect and wait for the next one.
        let Decoded::Event(Dispatch::VoiceServerUpdate(reallocating)) = decode_dispatch(
            "VOICE_SERVER_UPDATE",
            r#"{"token":"fixture-voice-token","guild_id":"4","endpoint":null}"#,
        ) else {
            panic!("VOICE_SERVER_UPDATE did not decode");
        };
        assert_eq!(reallocating.endpoint, None);
        assert_eq!(reallocating.guild_id, Some(id(4)));
        assert!(!format!("{reallocating:?}").contains("fixture-voice-token"));

        // Without a token there is nothing to connect with.
        assert!(matches!(
            decode_dispatch("VOICE_SERVER_UPDATE", r#"{"guild_id":"4","endpoint":null}"#),
            Decoded::Malformed
        ));
    }

    #[test]
    fn stream_signal_events_decode_typed_and_redact_tokens() {
        let stream_key = "guild:41:127:1";
        let Decoded::Event(created) = decode_dispatch(
            "STREAM_CREATE",
            r#"{"stream_key":"guild:41:127:1","rtc_server_id":"300","rtc_channel_id":"400","region":"us-east","viewer_ids":["2"],"paused":false}"#,
        ) else {
            panic!("STREAM_CREATE did not decode");
        };
        let Dispatch::StreamCreate(stream_create) = &created else {
            panic!("unexpected stream event");
        };
        assert_eq!(stream_create.stream_key, stream_key);
        assert_eq!(stream_create.rtc_server_id, id(300));
        assert_eq!(stream_create.rtc_channel_id, id(400));

        let Decoded::Event(server) = decode_dispatch(
            "STREAM_SERVER_UPDATE",
            r#"{"token":"stream-secret","stream_key":"guild:41:127:1","guild_id":"41","endpoint":"rtc.example"}"#,
        ) else {
            panic!("STREAM_SERVER_UPDATE did not decode");
        };
        let Dispatch::StreamServerUpdate(server_update) = &server else {
            panic!("unexpected stream event");
        };
        assert_eq!(server_update.token.expose_secret(), "stream-secret");
        assert_eq!(server_update.endpoint.as_deref(), Some("rtc.example"));
        assert!(!format!("{server:?} {server_update:?}").contains("stream-secret"));
        let Decoded::Event(updated) =
            decode_dispatch("STREAM_UPDATE", r#"{"stream_key":"guild:41:127:1"}"#)
        else {
            panic!("STREAM_UPDATE did not decode");
        };
        assert!(
            matches!(&updated, Dispatch::StreamUpdate(update) if update.stream_key == stream_key)
        );
        let Decoded::Event(deleted) = decode_dispatch(
            "STREAM_DELETE",
            r#"{"stream_key":"guild:41:127:1","reason":"stream_ended"}"#,
        ) else {
            panic!("STREAM_DELETE did not decode");
        };
        assert!(matches!(&deleted, Dispatch::StreamDelete(delete)
            if delete.stream_key == stream_key
                && delete.reason == fastcord_model::StreamDeleteReason::StreamEnded));
        for name in [
            "STREAM_CREATE",
            "STREAM_SERVER_UPDATE",
            "STREAM_UPDATE",
            "STREAM_DELETE",
        ] {
            assert!(matches!(decode_dispatch(name, "{}"), Decoded::Malformed));
        }
    }

    #[test]
    fn relationship_events_decode_and_malformed_ones_are_rejected() {
        let add = r#"{"id":"5","type":3,"user":{"id":"5","username":"x"}}"#;
        assert!(matches!(
            decode_dispatch("RELATIONSHIP_ADD", add),
            Decoded::Event(Dispatch::RelationshipAdd(r)) if r.kind == RelationshipKind::IncomingRequest
        ));
        assert!(matches!(
            decode_dispatch("RELATIONSHIP_UPDATE", r#"{"id":"5","nickname":"n"}"#),
            Decoded::Event(Dispatch::RelationshipUpdate(RelationshipUpdate {
                kind: None,
                ..
            }))
        ));
        assert!(matches!(
            decode_dispatch("RELATIONSHIP_REMOVE", r#"{"id":"5","type":3}"#),
            Decoded::Event(Dispatch::RelationshipRemove(Snowflake(5)))
        ));
        for (name, raw) in [
            ("RELATIONSHIP_ADD", r#"{"id":"5","type":3}"#),
            (
                "RELATIONSHIP_ADD",
                r#"{"id":"6","type":3,"user":{"id":"5","username":"x"}}"#,
            ),
            (
                "RELATIONSHIP_ADD",
                r#"{"id":"5","type":9,"user":{"id":"5","username":"x"}}"#,
            ),
            ("RELATIONSHIP_UPDATE", "{}"),
            ("RELATIONSHIP_REMOVE", r#"{"id":"0"}"#),
        ] {
            assert!(
                matches!(decode_dispatch(name, raw), Decoded::Malformed),
                "{raw}"
            );
        }
    }
}
