/// A user's availability as the Gateway reports it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    Online,
    Idle,
    Dnd,
    /// Only ever the account's own choice; Discord reports others as offline.
    Invisible,
    #[default]
    Offline,
}

impl Status {
    /// The Gateway/settings spelling; anything else is not a status.
    pub fn from_wire(text: &str) -> Option<Self> {
        match text {
            "online" => Some(Self::Online),
            "idle" => Some(Self::Idle),
            "dnd" => Some(Self::Dnd),
            "invisible" => Some(Self::Invisible),
            "offline" => Some(Self::Offline),
            _ => None,
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Idle => "idle",
            Self::Dnd => "dnd",
            Self::Invisible => "invisible",
            Self::Offline => "offline",
        }
    }
}

/// Longest custom status text kept (Discord allows 128 characters).
pub const MAX_CUSTOM_STATUS_CHARS: usize = 128;

/// Status plus the text of a custom status. Other activities are not kept.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Presence {
    pub status: Status,
    pub custom: Option<String>,
}
