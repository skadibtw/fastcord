//! Server expression assets (SPEC §16, issue #99): upload, rename or edit, and
//! delete custom emoji, stickers and soundboard sounds. Each function sends
//! exactly one request on the user's click and never retries; the lists change
//! only when Discord's `GUILD_EMOJIS_UPDATE`, `GUILD_STICKERS_UPDATE` and
//! `GUILD_SOUNDBOARD_SOUND_*` events arrive. The permission and the free slots
//! are decided before a click reaches here (`GuildAssets`, [`expression_limits`]);
//! Discord stays the authority and its refusal is shown verbatim. Files are
//! checked by signature and size before anything is encoded. Purchases, boosts
//! and premium cross-server use are not offered. Wire shapes come from
//! community documentation of the official client and are unverified against a
//! live account (`docs/TESTING.md`).

use fastcord_model::Snowflake;
use reqwest::Method;
use serde_json::json;

use crate::image_data::{ImageError, emoji_data_uri, sound_data_uri};
use crate::{
    AdminError, AdminRequestError, Clock, Priority, RestClient, RestError, RestRequest, Route,
};

pub const MIN_EMOJI_NAME_CHARS: usize = 2;
pub const MAX_EMOJI_NAME_CHARS: usize = 32;
pub const MIN_STICKER_NAME_CHARS: usize = 2;
/// Longest name Discord accepts for a new or edited sticker.
pub const MAX_STICKER_EDIT_NAME_CHARS: usize = 30;
pub const MIN_TAGS_CHARS: usize = 2;
/// Largest sticker file (PNG, APNG, GIF or Lottie JSON).
pub const MAX_STICKER_BYTES: usize = 512 * 1024;
/// Largest soundboard file.
pub const MAX_SOUND_BYTES: usize = 512 * 1024;
/// Longest soundboard clip Discord accepts, in milliseconds.
pub const MAX_SOUND_MS: u64 = 5_200;

/// Free slots a server has from its boost tier, per emoji kind (static and
/// animated each), stickers and soundboard sounds. Higher tiers are capped at 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExpressionLimits {
    pub emoji: usize,
    pub stickers: usize,
    pub sounds: usize,
}

/// The slots Discord gives a server of `premium_tier`: shown, never bought.
pub const fn expression_limits(tier: u8) -> ExpressionLimits {
    const EMOJI: [usize; 4] = [50, 100, 150, 250];
    const STICKERS: [usize; 4] = [5, 15, 30, 60];
    const SOUNDS: [usize; 4] = [8, 24, 36, 48];
    let at = if tier > 3 { 3 } else { tier as usize };
    ExpressionLimits {
        emoji: EMOJI[at],
        stickers: STICKERS[at],
        sounds: SOUNDS[at],
    }
}

fn map_image(error: ImageError) -> AdminError {
    match error {
        ImageError::Unsupported => AdminError::UnsupportedImage,
        ImageError::TooLarge => AdminError::ImageTooLarge,
    }
}

/// A validated emoji name: 2 to 32 of letters, digits and underscore.
pub fn emoji_name(name: &str) -> Result<&str, AdminError> {
    let name = name.trim();
    let count = name.chars().count();
    if !(MIN_EMOJI_NAME_CHARS..=MAX_EMOJI_NAME_CHARS).contains(&count)
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(AdminError::BadName);
    }
    Ok(name)
}

/// A name of `min` to `max` characters; stickers and sounds take any text.
fn length(name: &str, min: usize, max: usize) -> Result<&str, AdminError> {
    let name = name.trim();
    if !(min..=max).contains(&name.chars().count()) {
        return Err(AdminError::BadName);
    }
    Ok(name)
}

fn sound_name(name: &str) -> Result<&str, AdminError> {
    length(name, MIN_EMOJI_NAME_CHARS, crate::MAX_SOUND_NAME_CHARS)
}

fn sticker_name(name: &str) -> Result<&str, AdminError> {
    length(name, MIN_STICKER_NAME_CHARS, MAX_STICKER_EDIT_NAME_CHARS)
}

/// The text fields of a sticker, checked: a name, a description of 0 or 2 to
/// 100 characters, and tags of 2 to 200 characters.
fn sticker_fields<'a>(
    name: &'a str,
    description: &'a str,
    tags: &'a str,
) -> Result<(&'a str, &'a str, &'a str), AdminError> {
    let name = sticker_name(name)?;
    let description = description.trim();
    let described = description.chars().count();
    if described == 1 || described > crate::MAX_STICKER_DESCRIPTION_CHARS {
        return Err(AdminError::DescriptionTooLong);
    }
    let tags = tags.trim();
    let tagged = tags.chars().count();
    if !(MIN_TAGS_CHARS..=crate::MAX_STICKER_TAGS_CHARS).contains(&tagged) {
        return Err(AdminError::BadValue);
    }
    Ok((name, description, tags))
}

/// A validated emoji image in the `data:` form the body carries.
#[derive(Clone, PartialEq, Eq)]
pub struct EmojiImage(String);

impl std::fmt::Debug for EmojiImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EmojiImage({} bytes)", self.0.len())
    }
}

impl EmojiImage {
    /// PNG, JPEG, GIF or WebP of at most 256 KiB, judged by signature.
    pub fn new(bytes: &[u8]) -> Result<Self, AdminError> {
        emoji_data_uri(bytes).map(Self).map_err(map_image)
    }
}

/// A validated soundboard file in the `data:` form the body carries. The
/// duration is checked by the caller with the audio decoder.
#[derive(Clone, PartialEq, Eq)]
pub struct SoundFile(String);

impl std::fmt::Debug for SoundFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SoundFile({} bytes)", self.0.len())
    }
}

impl SoundFile {
    /// MP3 or Ogg of at most 512 KiB, judged by signature.
    pub fn new(bytes: &[u8]) -> Result<Self, AdminError> {
        sound_data_uri(bytes, MAX_SOUND_BYTES)
            .map(Self)
            .map_err(map_image)
    }
}

/// A validated sticker file: PNG (APNG included), GIF or Lottie JSON of at
/// most 512 KiB, with the content type and name the multipart part carries.
#[derive(Clone, PartialEq, Eq)]
pub struct StickerFile {
    bytes: Vec<u8>,
    content_type: &'static str,
    filename: &'static str,
}

impl std::fmt::Debug for StickerFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "StickerFile({} bytes)", self.bytes.len())
    }
}

impl StickerFile {
    pub fn new(bytes: Vec<u8>) -> Result<Self, AdminError> {
        let (content_type, filename) = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            ("image/png", "sticker.png")
        } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            ("image/gif", "sticker.gif")
        } else if lottie(&bytes) {
            ("application/json", "sticker.json")
        } else {
            return Err(AdminError::UnsupportedImage);
        };
        if bytes.len() > MAX_STICKER_BYTES {
            return Err(AdminError::ImageTooLarge);
        }
        Ok(Self {
            bytes,
            content_type,
            filename,
        })
    }

    /// Whether this is a Lottie animation (Discord limits those to verified or
    /// partnered servers; its answer is shown if the server is neither).
    pub fn is_lottie(&self) -> bool {
        self.content_type == "application/json"
    }
}

/// A JSON object, which is what a Lottie animation is; nothing else is read.
fn lottie(bytes: &[u8]) -> bool {
    bytes.len() <= MAX_STICKER_BYTES
        && matches!(
            serde_json::from_slice::<serde_json::Value>(bytes),
            Ok(serde_json::Value::Object(_))
        )
}

/// A multipart boundary no part contains.
fn boundary(parts: &[&[u8]]) -> Option<String> {
    use std::hash::{BuildHasher, Hasher};
    (0..4).find_map(|_| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u8(1);
        let value = format!("fastcord{:016x}", hasher.finish());
        let clear = parts
            .iter()
            .all(|part| !part.windows(value.len()).any(|w| w == value.as_bytes()));
        clear.then_some(value)
    })
}

/// `multipart/form-data` for a sticker: the three text fields and the file.
fn sticker_multipart(
    name: &str,
    description: &str,
    tags: &str,
    file: &StickerFile,
) -> Result<(String, Vec<u8>), RestError> {
    let text = [("name", name), ("description", description), ("tags", tags)];
    let mut parts: Vec<&[u8]> = text.iter().map(|(_, value)| value.as_bytes()).collect();
    parts.push(&file.bytes);
    let boundary = boundary(&parts).ok_or(RestError::InvalidJson)?;
    let mut body = Vec::with_capacity(file.bytes.len() + 512);
    for (field, value) in text {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: {}\r\n\r\n",
            file.filename, file.content_type
        )
        .as_bytes(),
    );
    body.extend_from_slice(&file.bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Ok((format!("multipart/form-data; boundary={boundary}"), body))
}

fn volume(percent: u8) -> Result<f64, AdminError> {
    if percent > 100 {
        return Err(AdminError::BadValue);
    }
    Ok(f64::from(percent) / 100.0)
}

/// A sound's Unicode emoji: empty means none; at most 16 characters.
fn sound_emoji(emoji: &str) -> Result<Option<&str>, AdminError> {
    let emoji = emoji.trim();
    if emoji.chars().count() > 16 {
        return Err(AdminError::BadValue);
    }
    Ok((!emoji.is_empty()).then_some(emoji))
}

fn emoji_create_body(name: &str, image: &EmojiImage) -> Result<serde_json::Value, AdminError> {
    Ok(json!({ "name": emoji_name(name)?, "image": image.0 }))
}

fn sound_create_body(
    name: &str,
    file: &SoundFile,
    percent: u8,
    emoji: &str,
) -> Result<serde_json::Value, AdminError> {
    let mut body = json!({
        "name": sound_name(name)?,
        "sound": file.0,
        "volume": volume(percent)?,
    });
    if let Some(emoji) = sound_emoji(emoji)? {
        body["emoji_name"] = emoji.into();
    }
    Ok(body)
}

fn sound_edit_body(name: &str, percent: u8, emoji: &str) -> Result<serde_json::Value, AdminError> {
    Ok(json!({
        "name": sound_name(name)?,
        "volume": volume(percent)?,
        "emoji_name": sound_emoji(emoji)?,
    }))
}

fn write(method: Method, path: &str) -> Result<RestRequest, RestError> {
    Ok(RestRequest::new(Route::new(method, path)?, Priority::UserWrite).with_error_detail())
}

impl<C: Clock> RestClient<C> {
    /// Uploads an emoji (`POST /guilds/{id}/emojis`); the list follows
    /// `GUILD_EMOJIS_UPDATE`.
    pub async fn create_emoji(
        &self,
        guild: Snowflake,
        name: &str,
        image: &EmojiImage,
    ) -> Result<(), AdminRequestError> {
        let body = emoji_create_body(name, image).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::POST, &format!("/guilds/{guild}/emojis"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Renames an emoji (`PATCH /guilds/{id}/emojis/{emoji}` with `{name}`).
    pub async fn rename_emoji(
        &self,
        guild: Snowflake,
        emoji: Snowflake,
        name: &str,
    ) -> Result<(), AdminRequestError> {
        let name = emoji_name(name).map_err(AdminRequestError::Invalid)?;
        let request = write(Method::PATCH, &format!("/guilds/{guild}/emojis/{emoji}"))?
            .json(&json!({ "name": name }))?;
        self.execute(request).await?;
        Ok(())
    }

    /// Deletes an emoji (`DELETE /guilds/{id}/emojis/{emoji}`).
    pub async fn delete_emoji(&self, guild: Snowflake, emoji: Snowflake) -> Result<(), RestError> {
        let request = write(Method::DELETE, &format!("/guilds/{guild}/emojis/{emoji}"))?;
        self.execute(request).await.map(drop)
    }

    /// Uploads a sticker (`POST /guilds/{id}/stickers`, multipart); the list
    /// follows `GUILD_STICKERS_UPDATE`.
    pub async fn create_sticker(
        &self,
        guild: Snowflake,
        name: &str,
        description: &str,
        tags: &str,
        file: &StickerFile,
    ) -> Result<(), AdminRequestError> {
        let (name, description, tags) =
            sticker_fields(name, description, tags).map_err(AdminRequestError::Invalid)?;
        let (content_type, body) = sticker_multipart(name, description, tags, file)?;
        let request =
            write(Method::POST, &format!("/guilds/{guild}/stickers"))?.raw(content_type, body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Changes a sticker's name, description and tags
    /// (`PATCH /guilds/{id}/stickers/{sticker}`).
    pub async fn edit_sticker(
        &self,
        guild: Snowflake,
        sticker: Snowflake,
        name: &str,
        description: &str,
        tags: &str,
    ) -> Result<(), AdminRequestError> {
        let (name, description, tags) =
            sticker_fields(name, description, tags).map_err(AdminRequestError::Invalid)?;
        let request = write(
            Method::PATCH,
            &format!("/guilds/{guild}/stickers/{sticker}"),
        )?
        .json(&json!({ "name": name, "description": description, "tags": tags }))?;
        self.execute(request).await?;
        Ok(())
    }

    /// Deletes a sticker (`DELETE /guilds/{id}/stickers/{sticker}`).
    pub async fn delete_sticker(
        &self,
        guild: Snowflake,
        sticker: Snowflake,
    ) -> Result<(), RestError> {
        let request = write(
            Method::DELETE,
            &format!("/guilds/{guild}/stickers/{sticker}"),
        )?;
        self.execute(request).await.map(drop)
    }

    /// Uploads a soundboard sound (`POST /guilds/{id}/soundboard-sounds`);
    /// the list follows `GUILD_SOUNDBOARD_SOUND_CREATE`. `percent` is the
    /// volume, 0 to 100; an empty `emoji` sends none.
    pub async fn create_sound(
        &self,
        guild: Snowflake,
        name: &str,
        file: &SoundFile,
        percent: u8,
        emoji: &str,
    ) -> Result<(), AdminRequestError> {
        let body =
            sound_create_body(name, file, percent, emoji).map_err(AdminRequestError::Invalid)?;
        let request =
            write(Method::POST, &format!("/guilds/{guild}/soundboard-sounds"))?.json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Changes a sound's name, volume and emoji
    /// (`PATCH /guilds/{id}/soundboard-sounds/{sound}`); an empty `emoji`
    /// clears it.
    pub async fn edit_sound(
        &self,
        guild: Snowflake,
        sound: Snowflake,
        name: &str,
        percent: u8,
        emoji: &str,
    ) -> Result<(), AdminRequestError> {
        let body = sound_edit_body(name, percent, emoji).map_err(AdminRequestError::Invalid)?;
        let request = write(
            Method::PATCH,
            &format!("/guilds/{guild}/soundboard-sounds/{sound}"),
        )?
        .json(&body)?;
        self.execute(request).await?;
        Ok(())
    }

    /// Deletes a sound (`DELETE /guilds/{id}/soundboard-sounds/{sound}`).
    pub async fn delete_sound(&self, guild: Snowflake, sound: Snowflake) -> Result<(), RestError> {
        let request = write(
            Method::DELETE,
            &format!("/guilds/{guild}/soundboard-sounds/{sound}"),
        )?;
        self.execute(request).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_data::MAX_EMOJI_BYTES;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    #[test]
    fn slots_follow_the_boost_tier_and_cap_at_three() {
        assert_eq!(expression_limits(0).emoji, 50);
        assert_eq!(expression_limits(1).stickers, 15);
        assert_eq!(expression_limits(2).sounds, 36);
        assert_eq!(
            expression_limits(3),
            ExpressionLimits {
                emoji: 250,
                stickers: 60,
                sounds: 48
            }
        );
        assert_eq!(expression_limits(9), expression_limits(3));
    }

    #[test]
    fn names_are_checked_before_anything_is_built() {
        assert_eq!(emoji_name(" party_1 "), Ok("party_1"));
        for bad in ["a", "has space", "dash-ed", "ü", &"x".repeat(33)] {
            assert_eq!(emoji_name(bad), Err(AdminError::BadName), "{bad}");
        }
        assert!(sticker_name("two words ok").is_ok());
        assert!(sticker_name(&"x".repeat(MAX_STICKER_EDIT_NAME_CHARS + 1)).is_err());
        assert!(sound_name("boom!").is_ok());
        assert!(sticker_fields("ab", "", "wave").is_ok());
        assert!(sticker_fields("ab", "x", "wave").is_err(), "one character");
        assert!(sticker_fields("ab", &"x".repeat(101), "wave").is_err());
        assert!(sticker_fields("ab", "", "w").is_err(), "tags are required");
        assert!(sticker_fields("ab", "", &"t".repeat(201)).is_err());
    }

    #[test]
    fn files_are_judged_by_signature_and_size() {
        assert!(EmojiImage::new(PNG).is_ok());
        assert!(EmojiImage::new(b"GIF89a..").is_ok());
        assert!(EmojiImage::new(b"RIFF\x00\x00\x00\x00WEBPVP8 ").is_ok());
        assert_eq!(
            EmojiImage::new(b"BM....."),
            Err(AdminError::UnsupportedImage)
        );
        let mut big = PNG.to_vec();
        big.resize(MAX_EMOJI_BYTES + 1, 0);
        assert_eq!(EmojiImage::new(&big), Err(AdminError::ImageTooLarge));
        big.truncate(MAX_EMOJI_BYTES);
        assert!(EmojiImage::new(&big).is_ok());

        assert!(StickerFile::new(PNG.to_vec()).is_ok());
        assert!(StickerFile::new(br#"{"v":"5.5","layers":[]}"#.to_vec()).is_ok());
        assert!(
            StickerFile::new(br#"{"v":1}"#.to_vec())
                .unwrap()
                .is_lottie()
        );
        assert!(StickerFile::new(b"[1]".to_vec()).is_err(), "not an object");
        assert!(StickerFile::new(b"<svg/>".to_vec()).is_err());
        let mut big = PNG.to_vec();
        big.resize(MAX_STICKER_BYTES + 1, 0);
        assert_eq!(StickerFile::new(big), Err(AdminError::ImageTooLarge));

        assert!(SoundFile::new(b"ID3\x04rest").is_ok());
        assert!(SoundFile::new(&[0xFF, 0xFB, 0x90]).is_ok());
        assert!(SoundFile::new(b"OggSrest").is_ok());
        assert_eq!(
            SoundFile::new(b"RIFFWAVE"),
            Err(AdminError::UnsupportedImage)
        );
        let mut big = b"OggS".to_vec();
        big.resize(MAX_SOUND_BYTES + 1, 0);
        assert_eq!(SoundFile::new(&big), Err(AdminError::ImageTooLarge));
    }

    #[test]
    fn bodies_carry_exactly_the_documented_fields() {
        let image = EmojiImage::new(PNG).unwrap();
        let body = emoji_create_body("party", &image).unwrap();
        assert_eq!(body["name"], "party");
        assert!(
            body["image"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(body.as_object().unwrap().len(), 2);
        assert!(emoji_create_body("no way", &image).is_err());

        let sound = SoundFile::new(b"OggSrest").unwrap();
        let body = sound_create_body("boom", &sound, 80, "").unwrap();
        assert_eq!(body["volume"], 0.8);
        assert!(body.get("emoji_name").is_none());
        assert!(
            body["sound"]
                .as_str()
                .unwrap()
                .starts_with("data:audio/ogg;base64,")
        );
        let body = sound_create_body("boom", &sound, 100, "\u{1f514}").unwrap();
        assert_eq!(body["emoji_name"], "\u{1f514}");
        assert!(sound_create_body("boom", &sound, 101, "").is_err());
        let edit = sound_edit_body("boom", 50, "").unwrap();
        assert!(edit["emoji_name"].is_null(), "an empty emoji clears it");
        assert_eq!(edit["volume"], 0.5);
    }

    #[test]
    fn a_sticker_is_a_multipart_of_three_fields_and_the_file() {
        let file = StickerFile::new(PNG.to_vec()).unwrap();
        let (content_type, body) = sticker_multipart("wave", "", "wave", &file).unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.ends_with(&format!("\r\n--{boundary}--\r\n")));
        for field in ["name", "description", "tags"] {
            assert!(
                text.contains(&format!("name=\"{field}\"\r\n\r\n")),
                "{field}"
            );
        }
        assert!(
            text.contains("name=\"file\"; filename=\"sticker.png\"\r\nContent-Type: image/png")
        );
        assert!(
            body.windows(PNG.len()).any(|w| w == PNG),
            "the file bytes are sent unchanged"
        );
    }

    #[test]
    fn requests_use_the_documented_routes_and_methods() {
        let route = |method, path: &str| Route::new(method, path).unwrap();
        assert_eq!(
            route(Method::POST, "/guilds/5/emojis").url.as_str(),
            "https://discord.com/api/v9/guilds/5/emojis"
        );
        assert!(write(Method::PATCH, "/guilds/5/emojis/6").is_ok());
        assert!(write(Method::DELETE, "/guilds/5/soundboard-sounds/7").is_ok());
    }
}
