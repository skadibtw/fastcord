use crate::User;

/// How the account stands with another user (`type` of a Discord relationship).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RelationshipKind {
    Friend,
    Blocked,
    IncomingRequest,
    OutgoingRequest,
}

impl RelationshipKind {
    /// Types 1 to 4; implicit relationships (5) and anything newer are not kept.
    pub fn from_wire(kind: u64) -> Option<Self> {
        match kind {
            1 => Some(Self::Friend),
            2 => Some(Self::Blocked),
            3 => Some(Self::IncomingRequest),
            4 => Some(Self::OutgoingRequest),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relationship {
    pub user: User,
    pub kind: RelationshipKind,
}
