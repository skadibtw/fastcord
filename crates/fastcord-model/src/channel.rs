use crate::{PermissionOverwrite, Snowflake, User};

/// Discord channel type. Unknown future types are preserved, not rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    GuildText,
    Dm,
    GuildVoice,
    GroupDm,
    GuildCategory,
    GuildAnnouncement,
    AnnouncementThread,
    PublicThread,
    PrivateThread,
    GuildStageVoice,
    GuildDirectory,
    GuildForum,
    GuildMedia,
    Unknown(u8),
}

impl ChannelKind {
    pub const fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::GuildText,
            1 => Self::Dm,
            2 => Self::GuildVoice,
            3 => Self::GroupDm,
            4 => Self::GuildCategory,
            5 => Self::GuildAnnouncement,
            10 => Self::AnnouncementThread,
            11 => Self::PublicThread,
            12 => Self::PrivateThread,
            13 => Self::GuildStageVoice,
            14 => Self::GuildDirectory,
            15 => Self::GuildForum,
            16 => Self::GuildMedia,
            other => Self::Unknown(other),
        }
    }

    pub const fn to_u8(self) -> u8 {
        match self {
            Self::GuildText => 0,
            Self::Dm => 1,
            Self::GuildVoice => 2,
            Self::GroupDm => 3,
            Self::GuildCategory => 4,
            Self::GuildAnnouncement => 5,
            Self::AnnouncementThread => 10,
            Self::PublicThread => 11,
            Self::PrivateThread => 12,
            Self::GuildStageVoice => 13,
            Self::GuildDirectory => 14,
            Self::GuildForum => 15,
            Self::GuildMedia => 16,
            Self::Unknown(v) => v,
        }
    }

    pub const fn is_private(self) -> bool {
        matches!(self, Self::Dm | Self::GroupDm)
    }

    pub const fn is_thread(self) -> bool {
        matches!(
            self,
            Self::AnnouncementThread | Self::PublicThread | Self::PrivateThread
        )
    }

    /// A forum or media channel: its children are posts (threads).
    pub const fn is_forum(self) -> bool {
        matches!(self, Self::GuildForum | Self::GuildMedia)
    }

    pub const fn is_voice(self) -> bool {
        matches!(self, Self::GuildVoice | Self::GuildStageVoice)
    }

    /// Channels whose message permissions follow text-channel implicit rules.
    pub const fn is_text_like(self) -> bool {
        matches!(
            self,
            Self::GuildText | Self::GuildAnnouncement | Self::GuildVoice | Self::GuildStageVoice
        ) || self.is_thread()
            || self.is_forum()
    }
}

impl serde::Serialize for ChannelKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u8(self.to_u8())
    }
}

impl<'de> serde::Deserialize<'de> for ChannelKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        <u8 as serde::Deserialize>::deserialize(d).map(Self::from_u8)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Channel {
    pub id: Snowflake,
    #[serde(rename = "type")]
    pub kind: ChannelKind,
    #[serde(default)]
    pub guild_id: Option<Snowflake>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub position: Option<i32>,
    /// Category for guild channels; parent channel for threads.
    #[serde(default)]
    pub parent_id: Option<Snowflake>,
    #[serde(default)]
    pub permission_overwrites: Vec<PermissionOverwrite>,
    /// DM and group-DM participants (excluding the current user).
    #[serde(default)]
    pub recipients: Vec<User>,
    /// DM and group-DM participants by ID. Gateway READY with deduplicated user
    /// objects sends only IDs (the users arrive once, in the `users` array);
    /// normalized channels always reference participants this way.
    #[serde(default)]
    pub recipient_ids: Vec<Snowflake>,
    /// Group DM owner.
    #[serde(default)]
    pub owner_id: Option<Snowflake>,
    /// Group DM icon hash.
    #[serde(default)]
    pub icon: Option<String>,
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
    /// A direct conversation Discord holds as a message request until the
    /// account accepts it. `None`: Discord did not say, which is not "accepted".
    #[serde(default)]
    pub is_message_request: Option<bool>,
    /// The request was flagged as spam. `None`: not said.
    #[serde(default)]
    pub is_spam: Option<bool>,
    /// Present on threads only.
    #[serde(default)]
    pub thread_metadata: Option<ThreadMetadata>,
    /// Present on a thread the current user has joined (the thread-member
    /// object itself carries nothing this client keeps).
    #[serde(default)]
    pub member: Option<ThreadMember>,
    /// Forum and media channels: the tags a post may carry (at most
    /// [`MAX_FORUM_TAGS`] are kept).
    #[serde(default, deserialize_with = "tags_bounded")]
    pub available_tags: Vec<ForumTag>,
    /// A post: the tags it carries (at most [`MAX_APPLIED_TAGS`] are kept).
    #[serde(default, deserialize_with = "applied_bounded")]
    pub applied_tags: Vec<Snowflake>,
    /// Channel flags: [`FLAG_PINNED`] on a post, [`FLAG_REQUIRE_TAG`] on a forum.
    #[serde(default)]
    pub flags: u64,
    /// Messages in a thread, not counting the first one (approximate).
    #[serde(default)]
    pub message_count: Option<u32>,
    /// Forum sort order: 0 latest activity, 1 creation date.
    #[serde(default)]
    pub default_sort_order: Option<u8>,
    /// Text, announcement and forum topic (capped at [`MAX_TOPIC_BYTES`]).
    #[serde(default, deserialize_with = "topic_capped")]
    pub topic: Option<Box<str>>,
    #[serde(default)]
    pub nsfw: bool,
    /// Slowmode in seconds (0 = off).
    #[serde(default)]
    pub rate_limit_per_user: u16,
    /// Voice bitrate in bits per second, as the server reports it.
    #[serde(default)]
    pub bitrate: u32,
    /// Voice user limit (0 = none).
    #[serde(default)]
    pub user_limit: u16,
    /// Voice region override; `None` is automatic.
    #[serde(default, deserialize_with = "region_capped")]
    pub rtc_region: Option<Box<str>>,
    /// Voice video quality: 1 auto, 2 full.
    #[serde(default)]
    pub video_quality_mode: Option<u8>,
}

/// Longest channel topic kept (Discord allows 1024 characters, 4096 in forums).
pub const MAX_TOPIC_BYTES: usize = 4096 * 4;
const MAX_REGION_BYTES: usize = 64;

fn capped<'de, D: serde::Deserializer<'de>>(
    d: D,
    max: usize,
) -> Result<Option<Box<str>>, D::Error> {
    let mut text = <Option<String> as serde::Deserialize>::deserialize(d)?;
    if let Some(text) = &mut text {
        text.truncate(text.floor_char_boundary(max));
    }
    Ok(text.map(String::into_boxed_str))
}

fn topic_capped<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Box<str>>, D::Error> {
    capped(d, MAX_TOPIC_BYTES)
}

fn region_capped<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Box<str>>, D::Error> {
    capped(d, MAX_REGION_BYTES)
}

/// Most tags a forum defines.
pub const MAX_FORUM_TAGS: usize = 20;
/// Most tags one post carries.
pub const MAX_APPLIED_TAGS: usize = 5;
/// `PINNED` in a post's `flags`.
pub const FLAG_PINNED: u64 = 1 << 1;
/// `REQUIRE_TAG` in a forum's `flags`.
pub const FLAG_REQUIRE_TAG: u64 = 1 << 4;

/// One tag of a forum.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ForumTag {
    pub id: Snowflake,
    #[serde(default)]
    pub name: String,
    /// Only members with Manage Threads may apply it.
    #[serde(default)]
    pub moderated: bool,
    /// The unicode emoji of the tag, if it has one.
    #[serde(default)]
    pub emoji_name: Option<String>,
}

fn bounded<'de, D, T>(d: D, max: usize) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    struct V<T>(usize, std::marker::PhantomData<T>);
    impl<'de, T: serde::Deserialize<'de>> serde::de::Visitor<'de> for V<T> {
        type Value = Vec<T>;
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a list or null")
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<Vec<T>, E> {
            Ok(Vec::new())
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<T>, A::Error> {
            let mut out = Vec::new();
            while out.len() < self.0 {
                match seq.next_element()? {
                    Some(item) => out.push(item),
                    None => return Ok(out),
                }
            }
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(out)
        }
    }
    d.deserialize_any(V(max, std::marker::PhantomData))
}

fn tags_bounded<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<ForumTag>, D::Error> {
    bounded(d, MAX_FORUM_TAGS)
}

fn applied_bounded<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Snowflake>, D::Error> {
    bounded(d, MAX_APPLIED_TAGS)
}

impl Channel {
    /// Still a message request: not accepted, so not an ordinary conversation.
    pub fn is_pending_request(&self) -> bool {
        self.is_message_request == Some(true)
    }
}

/// Archive/lock state of a thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadMetadata {
    #[serde(default)]
    pub archived: bool,
    #[serde(default)]
    pub locked: bool,
    /// When the thread was archived (ISO 8601): the paging cursor of the
    /// archived-thread listing.
    #[serde(default)]
    pub archive_timestamp: Option<String>,
}

/// Marks the current user as a member of a thread.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThreadMember {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_fields_default_off_and_texts_are_capped() {
        let plain: Channel = serde_json::from_str(r#"{"id":"1","type":0}"#).unwrap();
        assert_eq!(
            (
                plain.topic.as_deref(),
                plain.nsfw,
                plain.rate_limit_per_user
            ),
            (None, false, 0)
        );
        let long = "\u{e9}".repeat(MAX_TOPIC_BYTES);
        let raw = format!(
            r#"{{"id":"1","type":2,"topic":"{long}","rtc_region":"{}","bitrate":96000,"user_limit":5,"video_quality_mode":2}}"#,
            "r".repeat(200)
        );
        let channel: Channel = serde_json::from_str(&raw).unwrap();
        assert!(channel.topic.as_deref().unwrap().len() <= MAX_TOPIC_BYTES);
        assert_eq!(channel.rtc_region.as_deref().unwrap().len(), 64);
        assert_eq!(
            (
                channel.bitrate,
                channel.user_limit,
                channel.video_quality_mode
            ),
            (96_000, 5, Some(2))
        );
    }
}
