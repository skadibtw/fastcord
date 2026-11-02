//! The account's server folders and order from its user settings (issue #106).
//!
//! Two sources describe the same list. The legacy JSON object
//! (`user_settings.guild_folders` in READY and `USER_SETTINGS_UPDATE`) is what
//! the Gateway sends because this client does not ask for protobuf settings;
//! `PreloadedUserSettings.guild_folders` (field 14 of the base64 protobuf in
//! `user_settings_proto` and `USER_SETTINGS_PROTO_UPDATE`) is read by a small
//! hand-written decoder of only the fields below. Both are bounded and
//! forward-compatible for supported unknown wire fields; malformed protobuf
//! yields "unknown" and cannot replace existing folders. Nothing is written
//! back (docs/PROTOCOL.md).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use fastcord_model::Snowflake;

/// Folders kept.
pub const MAX_FOLDERS: usize = 200;
/// Servers kept across all folders.
pub const MAX_FOLDERED_GUILDS: usize = 2_000;
/// Longest folder name kept, in bytes (Discord allows 12 characters).
pub const MAX_FOLDER_NAME_BYTES: usize = 64;
/// Longest base64 settings blob decoded.
const MAX_PROTO_BASE64: usize = 512 * 1024;

/// One entry of the account's server list: a real folder when `id` is set,
/// otherwise a lone server placed in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuildFolder {
    pub id: Option<u64>,
    pub name: Option<String>,
    /// 24-bit RGB.
    pub color: Option<u32>,
    pub guilds: Vec<Snowflake>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuildFolders {
    pub folders: Vec<GuildFolder>,
}

impl GuildFolders {
    fn push(&mut self, folder: GuildFolder, taken: &mut usize) {
        if self.folders.len() < MAX_FOLDERS && *taken + folder.guilds.len() <= MAX_FOLDERED_GUILDS {
            *taken += folder.guilds.len();
            if self.folders.len() == self.folders.capacity() {
                self.folders
                    .reserve_exact((MAX_FOLDERS - self.folders.len()).min(16));
            }
            self.folders.push(folder);
        }
    }

    /// Approximate bytes held, for the store's ledger.
    pub fn bytes(&self) -> usize {
        self.folders
            .iter()
            .map(|f| 64 + f.guilds.len() * 8 + f.name.as_ref().map_or(0, String::len))
            .sum()
    }
}

fn clip(name: &str) -> String {
    name[..name.floor_char_boundary(MAX_FOLDER_NAME_BYTES)].to_owned()
}

fn id_of(value: &serde_json::Value) -> Option<u64> {
    match value {
        serde_json::Value::String(text) => text.parse().ok(),
        serde_json::Value::Number(number) => number.as_u64(),
        _ => None,
    }
    .filter(|id| *id != 0)
}

/// `user_settings.guild_folders` / `USER_SETTINGS_UPDATE.guild_folders`:
/// `[{id, name, color, guild_ids}]`. `None` when the value is not an array.
pub fn from_json(value: &serde_json::Value) -> Option<GuildFolders> {
    let mut folders = GuildFolders::default();
    let mut taken = 0;
    for entry in value.as_array()? {
        let guilds = entry
            .get("guild_ids")
            .and_then(serde_json::Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(id_of)
                    .map(Snowflake)
                    .take(MAX_FOLDERED_GUILDS)
                    .collect()
            })
            .unwrap_or_default();
        folders.push(
            GuildFolder {
                id: entry.get("id").and_then(id_of),
                name: entry
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(clip),
                color: entry
                    .get("color")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|c| u32::try_from(c).ok())
                    .filter(|c| *c <= 0xFF_FFFF),
                guilds,
            },
            &mut taken,
        );
    }
    Some(folders)
}

/// One protobuf field; unknown fixed32 payloads need no retained value.
#[derive(Clone, Copy)]
enum Wire<'a> {
    Varint(u64),
    Fixed(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        // A u64's tenth byte can contain only bit 63 and cannot continue.
        if shift == 63 && byte > 1 {
            return None;
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Borrowed streaming scan. A malformed field is one error item, then EOF;
/// callers must consume the complete message before publishing any projection.
/// Deprecated groups remain explicitly unsupported (this is a proto3 reader).
struct Fields<'a>(&'a [u8]);

impl<'a> Iterator for Fields<'a> {
    type Item = Option<(u32, Wire<'a>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.0.is_empty() {
            return None;
        }
        let field = read_field(&mut self.0);
        if field.is_none() {
            self.0 = &[];
        }
        Some(field)
    }
}

fn read_field<'a>(bytes: &mut &'a [u8]) -> Option<(u32, Wire<'a>)> {
    let tag = varint(bytes)?;
    let number = u32::try_from(tag >> 3).ok()?;
    if !(1..=(1 << 29) - 1).contains(&number) {
        return None;
    }
    let wire = match tag & 7 {
        0 => Wire::Varint(varint(bytes)?),
        1 => {
            let (head, rest) = bytes.split_first_chunk::<8>()?;
            *bytes = rest;
            Wire::Fixed(u64::from_le_bytes(*head))
        }
        2 => {
            let len = usize::try_from(varint(bytes)?).ok()?;
            let (head, rest) = bytes.split_at_checked(len)?;
            *bytes = rest;
            Wire::Bytes(head)
        }
        5 => {
            let (_, rest) = bytes.split_first_chunk::<4>()?;
            *bytes = rest;
            Wire::Fixed32
        }
        _ => return None,
    };
    Some((number, wire))
}

/// A wrapper's matching scalar field 1 is last-one-wins. Inspect trailing and
/// unknown fields too, so a malformed wrapper can never become a partial update.
fn wrapped<'a, T>(bytes: &'a [u8], value: impl Fn(Wire<'a>) -> Option<T>) -> Option<Option<T>> {
    let mut result = None;
    for field in Fields(bytes) {
        let (number, wire) = field?;
        if number == 1
            && let Some(value) = value(wire)
        {
            result = Some(value);
        }
    }
    Some(result)
}

fn wrapped_number(bytes: &[u8]) -> Option<Option<u64>> {
    wrapped(bytes, |wire| match wire {
        Wire::Varint(value) => Some(value),
        _ => None,
    })
}

fn wrapped_text(bytes: &[u8]) -> Option<Option<&str>> {
    let mut result = None;
    for field in Fields(bytes) {
        if let (1, Wire::Bytes(text)) = field? {
            // Every known string occurrence must be UTF-8, even if a later
            // scalar would otherwise replace it under last-one-wins semantics.
            result = Some(std::str::from_utf8(text).ok()?);
        }
    }
    Some(result)
}

fn packed_ids(bytes: &[u8]) -> Option<impl Iterator<Item = u64> + '_> {
    let (chunks, remainder) = bytes.as_chunks::<8>();
    remainder
        .is_empty()
        .then(|| chunks.iter().map(|chunk| u64::from_le_bytes(*chunk)))
}

fn folder_of(bytes: &[u8], guild_limit: usize, retain: bool) -> Option<GuildFolder> {
    // Count before allocating, so vector capacity across retained folders never
    // exceeds the guild budget. Both passes borrow the same bounded input.
    let mut count = 0usize;
    for field in Fields(bytes) {
        let added = match field? {
            (1, Wire::Fixed(id)) => usize::from(id != 0),
            (1, Wire::Bytes(packed)) => packed_ids(packed)?.filter(|id| *id != 0).count(),
            _ => 0,
        };
        count = count.checked_add(added)?;
    }
    let mut folder = GuildFolder {
        guilds: Vec::with_capacity(if retain { count.min(guild_limit) } else { 0 }),
        ..Default::default()
    };
    let mut color_seen = false;
    let mut push = |id| {
        if retain && id != 0 && folder.guilds.len() < guild_limit {
            folder.guilds.push(Snowflake(id));
        }
    };
    for field in Fields(bytes) {
        match field? {
            (1, Wire::Fixed(id)) => push(id),
            (1, Wire::Bytes(packed)) => {
                for id in packed_ids(packed)? {
                    push(id);
                }
            }
            // Repeated wrapper messages merge; absent scalar preserves prior
            // value; explicit empty/zero clears name/ID, but color zero is black.
            (2, Wire::Bytes(inner)) => {
                if let Some(id) = wrapped_number(inner)? {
                    folder.id = Some(id).filter(|id| *id != 0);
                }
            }
            (3, Wire::Bytes(inner)) => {
                if let Some(name) = wrapped_text(inner)?
                    && retain
                {
                    folder.name = (!name.is_empty()).then(|| clip(name));
                }
            }
            (4, Wire::Bytes(inner)) => {
                if let Some(color) = wrapped_number(inner)? {
                    folder.color = u32::try_from(color).ok().filter(|c| *c <= 0xFF_FFFF);
                } else if !color_seen {
                    // Present UInt64Value with absent scalar has default zero
                    // (black). Later empty messages merge without resetting it.
                    folder.color = Some(0);
                }
                color_seen = true;
            }
            _ => {}
        }
    }
    Some(folder)
}

/// `PreloadedUserSettings` (base64) → its `guild_folders` (field 14,
/// `GuildFolders { repeated GuildFolder folders = 1; }`). `None` when the blob
/// is malformed, too large or does not carry the field.
pub fn from_proto_base64(encoded: &str) -> Option<GuildFolders> {
    if encoded.len() > MAX_PROTO_BASE64 {
        return None;
    }
    let bytes = STANDARD
        .decode(encoded)
        .or_else(|_| STANDARD_NO_PAD.decode(encoded))
        .ok()?;
    let mut folders = GuildFolders::default();
    let mut taken = 0;
    let mut present = false;
    for field in Fields(&bytes) {
        let (number, wire) = field?;
        let (14, Wire::Bytes(inner)) = (number, wire) else {
            continue;
        };
        present = true;
        for field in Fields(inner) {
            if let (1, Wire::Bytes(folder)) = field? {
                let retain = folders.folders.len() < MAX_FOLDERS;
                let folder = folder_of(folder, MAX_FOLDERED_GUILDS - taken, retain)?;
                if retain {
                    folders.push(folder, &mut taken);
                }
            }
        }
    }
    present.then_some(folders)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tag(number: u64, wire: u64) -> u8 {
        (number << 3 | wire) as u8
    }

    fn message(number: u64, inner: &[u8]) -> Vec<u8> {
        let mut out = encoded_varint(number << 3 | 2);
        out.extend(encoded_varint(inner.len() as u64));
        out.extend_from_slice(inner);
        out
    }

    fn encoded_varint(mut value: u64) -> Vec<u8> {
        let mut out = Vec::new();
        while value >= 128 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
        out
    }

    fn number(field: u64, value: u64) -> Vec<u8> {
        let mut out = encoded_varint(field << 3);
        out.extend(encoded_varint(value));
        out
    }

    fn blob(folder: &[u8]) -> String {
        STANDARD.encode(message(14, &message(1, folder)))
    }

    /// Hand-assembled `PreloadedUserSettings` with one folder.
    fn proto() -> String {
        let mut folder = message(1, &[10u64.to_le_bytes(), 11u64.to_le_bytes()].concat());
        folder.extend(message(2, &[tag(1, 0), 77])); // Int64Value 77
        folder.extend(message(3, &message(1, b"Games"))); // StringValue
        folder.extend(message(4, &[tag(1, 0), 0xff, 0x01])); // UInt64Value 255
        let mut settings = message(1, b"other setting"); // unknown field kept out
        settings.extend(message(14, &message(1, &folder)));
        STANDARD.encode(settings)
    }

    #[test]
    fn protobuf_folders_decode_and_unknown_fields_are_skipped() {
        let folders = from_proto_base64(&proto()).unwrap();
        assert_eq!(
            folders.folders,
            [GuildFolder {
                id: Some(77),
                name: Some("Games".into()),
                color: Some(255),
                guilds: vec![Snowflake(10), Snowflake(11)],
            }]
        );
        // No guild_folders field: unknown, not "no folders".
        assert_eq!(from_proto_base64(&STANDARD.encode([tag(1, 2), 0])), None);
        // Truncated, not base64 and oversized blobs are unknown.
        assert_eq!(
            from_proto_base64(&STANDARD.encode([tag(14, 2), 9, 1])),
            None
        );
        assert_eq!(from_proto_base64("!!!"), None);
        assert_eq!(from_proto_base64(&"A".repeat(MAX_PROTO_BASE64 + 4)), None);
    }

    #[test]
    fn legacy_json_folders_decode_leniently_and_are_bounded() {
        let value = json!([
            {"id": 5, "name": "Work", "color": 16711680, "guild_ids": ["1", "2", 3, "bad"]},
            {"id": null, "name": null, "color": null, "guild_ids": ["9"]},
            {"id": "6", "name": "", "color": 99999999999u64}
        ]);
        let folders = from_json(&value).unwrap().folders;
        assert_eq!(folders.len(), 3);
        assert_eq!(
            folders[0].guilds,
            [Snowflake(1), Snowflake(2), Snowflake(3)]
        );
        assert_eq!((folders[0].id, folders[0].color), (Some(5), Some(0xFF0000)));
        assert_eq!((folders[1].id, folders[1].name.clone()), (None, None));
        assert_eq!((folders[2].id, folders[2].color), (Some(6), None));
        assert_eq!(from_json(&json!({})), None);
        let many = json!(
            (1..=300)
                .map(|n| json!({"id": n, "guild_ids": []}))
                .collect::<Vec<_>>()
        );
        assert_eq!(from_json(&many).unwrap().folders.len(), MAX_FOLDERS);
    }
    #[test]
    fn streaming_unknown_fixed32_and_wrapper_last_values_preserve_known_folders() {
        let unknown32 = [0x7d, 1, 2, 3, 4]; // field15, wire5
        let mut folder = message(1, &10u64.to_le_bytes());
        let mut name = message(1, b"old");
        name.extend(unknown32);
        name.extend(message(1, b"new"));
        folder.extend(message(3, &name));
        folder.extend(message(2, &[number(1, 77), number(1, 88)].concat()));
        folder.extend(message(2, &[])); // merged empty wrapper does not clear 88
        folder.extend(message(4, &[number(1, 255), number(1, 0)].concat()));
        folder.extend(unknown32);
        let mut inner = unknown32.to_vec();
        inner.extend(message(1, &folder));
        inner.extend(unknown32);
        let mut top = unknown32.to_vec();
        top.extend(message(14, &inner));
        top.extend(unknown32);
        let result = from_proto_base64(&STANDARD.encode(top)).unwrap();
        assert_eq!(
            result.folders,
            [GuildFolder {
                id: Some(88),
                name: Some("new".into()),
                color: Some(0),
                guilds: vec![Snowflake(10)]
            }]
        );
        folder.extend(message(2, &number(1, 0)));
        folder.extend(message(3, &message(1, b"")));
        let cleared = from_proto_base64(&blob(&folder)).unwrap();
        assert_eq!(
            (cleared.folders[0].id, cleared.folders[0].name.clone()),
            (None, None)
        );
    }

    #[test]
    fn malformed_tags_varints_lengths_and_known_nested_values_reject_whole_blob() {
        let valid = message(14, &message(1, &message(1, &10u64.to_le_bytes())));
        let mut overflow = vec![0x80; 9];
        overflow.push(2);
        let malformed = [
            vec![0, 1], // zero field number with otherwise valid scalar
            vec![8],
            vec![9, 1, 2],
            vec![10, 5, 1],
            vec![13, 1, 2, 3],
            vec![11, 12],
            vec![14],
            vec![15],
            [encoded_varint((1u64 << 29) << 3), vec![1]].concat(),
            [vec![8], overflow.clone()].concat(),
            [vec![10], overflow.clone()].concat(),
            overflow,
        ];
        for bad in malformed {
            let mut top = valid.clone();
            top.extend_from_slice(&bad);
            assert!(
                from_proto_base64(&STANDARD.encode(&top)).is_none(),
                "trailing {bad:?}"
            );
            assert!(from_proto_base64(&blob(&bad)).is_none(), "folder {bad:?}");
            // A previously found scalar does not hide malformed wrapper tails.
            for field in [2, 3, 4] {
                let prefix = if field == 3 {
                    message(1, b"known")
                } else {
                    number(1, 77)
                };
                assert!(
                    from_proto_base64(&blob(&message(field, &[prefix, bad.clone()].concat())))
                        .is_none(),
                    "wrapper {field}: {bad:?}"
                );
            }
        }
        let mut packed = 10u64.to_le_bytes().to_vec();
        packed.push(0xff);
        assert!(from_proto_base64(&blob(&message(1, &packed))).is_none());
        let invalid_then_valid = [message(1, &[0xff]), message(1, b"valid")].concat();
        assert!(from_proto_base64(&blob(&message(3, &invalid_then_valid))).is_none());
        let full = encoded_varint(u64::MAX);
        let mut slice = full.as_slice();
        assert_eq!(varint(&mut slice), Some(u64::MAX));
        assert!(slice.is_empty());
        assert!(
            Fields(&encoded_varint(((1u64 << 29) - 1) << 3 | 5))
                .next()
                .unwrap()
                .is_none(),
            "valid max tag still needs 4 payload bytes"
        );
    }

    #[test]
    fn present_empty_color_wrapper_uses_default_only_on_first_occurrence() {
        let folder = |values: &[&[u8]]| {
            let bytes: Vec<_> = values.iter().flat_map(|value| message(4, value)).collect();
            from_proto_base64(&blob(&bytes)).unwrap().folders.remove(0)
        };
        assert_eq!(folder(&[&[]]).color, Some(0));
        assert_eq!(folder(&[&number(1, 255), &[]]).color, Some(255));
        assert_eq!(folder(&[&number(1, 0x1000000), &[]]).color, None);
        assert_eq!(
            folder(&[&number(1, 0x1000000), &number(1, 0)]).color,
            Some(0)
        );
        assert_eq!(folder(&[]).color, None);
    }

    #[test]
    fn supported_unknown_wire_fields_and_mixed_repeated_ids_are_forward_compatible() {
        let mut folder = vec![9];
        folder.extend(10u64.to_le_bytes());
        folder.extend(message(1, &11u64.to_le_bytes()));
        folder.extend(message(1, &12u64.to_le_bytes()));
        let mut settings = message(14, &message(1, &folder));
        settings.extend(number(15, u64::MAX));
        settings.push(tag(15, 1));
        settings.extend(u64::MAX.to_le_bytes());
        settings.extend(message(15, &[0, 0xff])); // unknown bytes are opaque
        settings.extend(encoded_varint(((1u64 << 29) - 1) << 3 | 5));
        settings.extend([1, 2, 3, 4]);
        let result = from_proto_base64(&STANDARD_NO_PAD.encode(settings)).unwrap();
        assert_eq!(
            result.folders[0].guilds,
            [Snowflake(10), Snowflake(11), Snowflake(12)]
        );
    }

    #[test]
    fn missing_folders_and_explicit_empty_remain_distinct() {
        assert_eq!(
            from_proto_base64(&STANDARD.encode(message(15, b"unknown"))),
            None
        );
        assert_eq!(
            from_proto_base64(&STANDARD.encode(message(14, &[]))),
            Some(GuildFolders::default())
        );
        assert_eq!(
            from_proto_base64(&STANDARD.encode([0x75, 0, 0, 0, 0])),
            None,
            "field14 with a different wire type is not the known message"
        );
    }

    #[test]
    fn retention_capacities_are_bounded_but_discarded_entries_are_still_validated() {
        let ids: Vec<_> = (1..=2500u64).flat_map(u64::to_le_bytes).collect();
        let mut folder = message(1, &ids);
        folder.extend(message(3, &message(1, "日".repeat(100).as_bytes())));
        let mut inner = message(1, &folder);
        for _ in 0..MAX_FOLDERS + 8 {
            inner.extend(message(1, &message(1, &9u64.to_le_bytes())));
        }
        let result = from_proto_base64(&STANDARD.encode(message(14, &inner))).unwrap();
        assert_eq!(result.folders.len(), MAX_FOLDERS);
        assert!(result.folders.capacity() <= MAX_FOLDERS);
        assert_eq!(
            result.folders.iter().map(|f| f.guilds.len()).sum::<usize>(),
            MAX_FOLDERED_GUILDS
        );
        assert!(
            result
                .folders
                .iter()
                .map(|f| f.guilds.capacity())
                .sum::<usize>()
                <= MAX_FOLDERED_GUILDS
        );
        assert!(result.folders[0].name.as_ref().unwrap().len() <= MAX_FOLDER_NAME_BYTES);
        inner.extend(message(1, &message(2, &[0x08, 0x80])));
        assert!(
            from_proto_base64(&STANDARD.encode(message(14, &inner))).is_none(),
            "malformed wrapper beyond retention cap cannot publish partial state"
        );
        let packed_over_cap = [message(1, &folder), message(1, &message(1, &[0; 9]))].concat();
        assert!(from_proto_base64(&STANDARD.encode(message(14, &packed_over_cap))).is_none());
    }
}
