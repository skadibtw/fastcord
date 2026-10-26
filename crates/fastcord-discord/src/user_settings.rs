//! The account's server folders and order from its user settings (issue #106).
//!
//! Two sources describe the same list. The legacy JSON object
//! (`user_settings.guild_folders` in READY and `USER_SETTINGS_UPDATE`) is what
//! the Gateway sends because this client does not ask for protobuf settings;
//! `PreloadedUserSettings.guild_folders` (field 14 of the base64 protobuf in
//! `user_settings_proto` and `USER_SETTINGS_PROTO_UPDATE`) is read by a small
//! hand-written decoder of only the fields below. Both are bounded and
//! lenient: anything unexpected yields "unknown" and leaves the order as
//! Discord's guild order. Nothing is written back (docs/PROTOCOL.md).

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

/// One protobuf field: the number and its payload.
enum Wire<'a> {
    Varint(u64),
    Fixed(u64),
    Bytes(&'a [u8]),
}

fn varint(bytes: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = bytes.split_first()?;
        *bytes = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// The fields of one message; `None` at the first malformed byte. Groups and
/// 32-bit fields are not part of the schema read here, so they end the parse.
fn fields(mut bytes: &[u8]) -> Option<Vec<(u64, Wire<'_>)>> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let tag = varint(&mut bytes)?;
        let wire = match tag & 7 {
            0 => Wire::Varint(varint(&mut bytes)?),
            1 => {
                let (head, rest) = bytes.split_first_chunk::<8>()?;
                bytes = rest;
                Wire::Fixed(u64::from_le_bytes(*head))
            }
            2 => {
                let len = usize::try_from(varint(&mut bytes)?).ok()?;
                let (head, rest) = bytes.split_at_checked(len)?;
                bytes = rest;
                Wire::Bytes(head)
            }
            _ => return None,
        };
        out.push((tag >> 3, wire));
    }
    Some(out)
}

/// The value inside a `google.protobuf` wrapper message (field 1).
fn wrapped(bytes: &[u8]) -> Option<Wire<'_>> {
    fields(bytes)?
        .into_iter()
        .find_map(|(n, w)| (n == 1).then_some(w))
}

fn folder_of(bytes: &[u8]) -> Option<GuildFolder> {
    let mut folder = GuildFolder::default();
    for (number, wire) in fields(bytes)? {
        match (number, wire) {
            // repeated fixed64 guild_ids: packed (proto3) or one per field.
            (1, Wire::Fixed(id)) => folder.guilds.push(Snowflake(id)),
            (1, Wire::Bytes(packed)) => folder.guilds.extend(
                packed
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|c| Snowflake(u64::from_le_bytes(*c))),
            ),
            // Int64Value id, StringValue name, UInt64Value color.
            (2, Wire::Bytes(inner)) => {
                if let Some(Wire::Varint(id)) = wrapped(inner) {
                    folder.id = Some(id).filter(|id| *id != 0);
                }
            }
            (3, Wire::Bytes(inner)) => {
                if let Some(Wire::Bytes(name)) = wrapped(inner) {
                    folder.name = std::str::from_utf8(name)
                        .ok()
                        .filter(|name| !name.is_empty())
                        .map(clip);
                }
            }
            (4, Wire::Bytes(inner)) => {
                if let Some(Wire::Varint(color)) = wrapped(inner) {
                    folder.color = u32::try_from(color).ok().filter(|c| *c <= 0xFF_FFFF);
                }
            }
            _ => {}
        }
    }
    folder.guilds.retain(|guild| guild.0 != 0);
    folder.guilds.truncate(MAX_FOLDERED_GUILDS);
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
    for (number, wire) in fields(&bytes)? {
        let (14, Wire::Bytes(inner)) = (number, wire) else {
            continue;
        };
        present = true;
        for (number, wire) in fields(inner)? {
            if let (1, Wire::Bytes(folder)) = (number, wire) {
                folders.push(folder_of(folder)?, &mut taken);
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
        let mut out = vec![tag(number, 2), inner.len() as u8];
        out.extend_from_slice(inner);
        out
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
}
