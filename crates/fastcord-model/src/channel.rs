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

    pub const fn is_voice(self) -> bool {
        matches!(self, Self::GuildVoice | Self::GuildStageVoice)
    }

    /// Channels whose message permissions follow text-channel implicit rules.
    pub const fn is_text_like(self) -> bool {
        matches!(
            self,
            Self::GuildText | Self::GuildAnnouncement | Self::GuildVoice | Self::GuildStageVoice
        ) || self.is_thread()
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
    #[serde(default)]
    pub last_message_id: Option<Snowflake>,
}
