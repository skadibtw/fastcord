//! Named Gateway capability flags (SPEC §4.3). The Identify `capabilities`
//! integer is derived from this list, never copied as a magic number.
//!
//! Only capabilities whose payload shapes the Gateway layer actually handles
//! are selected. Everything else stays off: protobuf user settings, reaction
//! debouncing, client-state v2, versioned read states, and token refresh would
//! each change payloads this client does not decode yet.

/// Flag values from the userdoccers capability table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    /// Omits the `notes` map from READY.
    LazyUserNotes,
    /// No member/presence syncing for implicit relationships.
    NoAffineUserIds,
    /// READY carries each user once in `users`; members and private channels
    /// reference them by ID and guild members are merged into `merged_members`.
    DedupeUserObjects,
    /// Splits READY from READY_SUPPLEMENTAL. Requires `DedupeUserObjects`.
    PrioritizedReadyPayload,
    /// Passive guild updates v2 (`PASSIVE_UPDATE_V2`) for unsubscribed guilds.
    PassiveGuildUpdateV2,
}

impl Capability {
    pub const fn bit(self) -> u64 {
        match self {
            Self::LazyUserNotes => 1 << 0,
            Self::NoAffineUserIds => 1 << 1,
            Self::DedupeUserObjects => 1 << 4,
            Self::PrioritizedReadyPayload => 1 << 5,
            Self::PassiveGuildUpdateV2 => 1 << 14,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::LazyUserNotes => "LAZY_USER_NOTES",
            Self::NoAffineUserIds => "NO_AFFINE_USER_IDS",
            Self::DedupeUserObjects => "DEDUPE_USER_OBJECTS",
            Self::PrioritizedReadyPayload => "PRIORITIZED_READY_PAYLOAD",
            Self::PassiveGuildUpdateV2 => "PASSIVE_GUILD_UPDATE_V2",
        }
    }
}

/// The capabilities requested on every Identify.
pub const SELECTED: [Capability; 5] = [
    Capability::LazyUserNotes,
    Capability::NoAffineUserIds,
    Capability::DedupeUserObjects,
    Capability::PrioritizedReadyPayload,
    Capability::PassiveGuildUpdateV2,
];

/// The Identify `capabilities` value for [`SELECTED`].
pub const fn selected_value() -> u64 {
    let mut value = 0;
    let mut index = 0;
    while index < SELECTED.len() {
        value |= SELECTED[index].bit();
        index += 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_value_is_the_union_of_the_named_flags() {
        assert_eq!(selected_value(), 1 + 2 + 16 + 32 + 16_384);
        let names: Vec<_> = SELECTED.iter().map(|c| c.name()).collect();
        assert_eq!(
            names,
            [
                "LAZY_USER_NOTES",
                "NO_AFFINE_USER_IDS",
                "DEDUPE_USER_OBJECTS",
                "PRIORITIZED_READY_PAYLOAD",
                "PASSIVE_GUILD_UPDATE_V2"
            ]
        );
    }

    #[test]
    fn prioritized_ready_always_comes_with_its_deduplication_prerequisite() {
        assert!(SELECTED.contains(&Capability::PrioritizedReadyPayload));
        assert!(SELECTED.contains(&Capability::DedupeUserObjects));
    }

    #[test]
    fn flags_are_distinct_single_bits() {
        let mut seen = 0u64;
        for capability in SELECTED {
            assert_eq!(capability.bit().count_ones(), 1);
            assert_eq!(seen & capability.bit(), 0);
            seen |= capability.bit();
        }
    }
}
