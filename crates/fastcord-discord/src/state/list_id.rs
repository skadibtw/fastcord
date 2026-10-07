//! Discord's channel member-list key, not the channel ID. Algorithm follows
//! discord.py-self's GuildChannel.member_list_id (docs/PROTOCOL.md).
use fastcord_model::{Channel, Permissions, Role, Snowflake};

use crate::gateway::MemberListId;

pub(super) fn for_channel(guild: Snowflake, roles: &[Role], channel: &Channel) -> MemberListId {
    let view = Permissions::VIEW_CHANNEL;
    if roles
        .iter()
        .any(|role| role.id == guild && role.permissions.contains(view))
        && !channel
            .permission_overwrites
            .iter()
            .any(|overwrite| overwrite.deny.contains(view))
    {
        return MemberListId("everyone".to_owned());
    }
    let mut entries: Vec<String> = channel
        .permission_overwrites
        .iter()
        .filter_map(|overwrite| {
            if overwrite.allow.contains(view) {
                Some(format!("allow:{}", overwrite.id))
            } else if overwrite.deny.contains(view) {
                Some(format!("deny:{}", overwrite.id))
            } else {
                None
            }
        })
        .collect();
    entries.sort_unstable();
    MemberListId(murmur3(entries.join(",").as_bytes()).to_string())
}

// MurmurHash3 x86_32, seed zero. Wrapping arithmetic is part of the hash.
fn murmur3(bytes: &[u8]) -> u32 {
    fn mix(mut word: u32) -> u32 {
        word = word.wrapping_mul(0xcc9e2d51);
        word = word.rotate_left(15);
        word.wrapping_mul(0x1b873593)
    }
    let mut hash = 0u32;
    let (chunks, tail) = bytes.as_chunks::<4>();
    for chunk in chunks {
        hash ^= mix(u32::from_le_bytes(*chunk));
        hash = hash
            .rotate_left(13)
            .wrapping_mul(5)
            .wrapping_add(0xe6546b64);
    }
    let word = tail
        .iter()
        .enumerate()
        .fold(0, |word, (i, byte)| word | (u32::from(*byte) << (i * 8)));
    if !tail.is_empty() {
        hash ^= mix(word);
    }
    hash ^= bytes.len() as u32;
    hash ^= hash >> 16;
    hash = hash.wrapping_mul(0x85ebca6b);
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(0xc2b2ae35);
    hash ^ (hash >> 16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_murmur3_vectors_cover_blocks_and_all_tail_sizes() {
        assert_eq!(murmur3(b""), 0);
        assert_eq!(murmur3(b"foo"), 0xf6a5c420);
        assert_eq!(murmur3(b"hello"), 0x248bfa47);
        assert_eq!(murmur3(b"abc"), 0xb3dd93fa);
        assert_eq!(murmur3(b"ab"), 0x9bbfd75f);
        assert_eq!(murmur3(b"a"), 0x3c2569b2);
    }
}
