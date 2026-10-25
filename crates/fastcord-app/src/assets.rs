//! Server emoji, stickers and soundboard sounds (SPEC §16, issue #99): list
//! them with the free slots the server's boost level gives, upload one, rename
//! or edit one, and delete one after a Confirm.
//!
//! Every write is one explicit click and one request: it is checked again
//! against the reducer's read model (`GuildAssets`: Create Expressions to
//! upload and to change one's own, Manage Expressions for anyone's) and the free
//! slots immediately before it is sent, at most one is in flight, and none is
//! ever retried. Files are checked by signature, size and (sounds) duration
//! before anything is sent, off the UI thread; a chosen emoji or sticker is
//! previewed through the bounded emoji decoder and a chosen sound through the
//! #87 preview player. The emoji list follows `GUILD_EMOJIS_UPDATE`; stickers
//! and sounds are listed by REST and listed again when `GUILD_STICKERS_UPDATE`
//! or `GUILD_SOUNDBOARD_SOUND_*` arrive. Discord's refusal (30008 maximum
//! emojis, 50045 file too large, ...) is shown verbatim. Slots are shown from
//! the boost tier and never bought; with no tier reported no slot is assumed.

use std::sync::Arc;

use fastcord_audio::{DeviceChoice, FilePlayer, PlayerState};
use fastcord_discord::state::navigation::{GuildAssets, NavigationSnapshot};
use fastcord_discord::{
    AdminError, AdminRequestError, EmojiImage, ExpressionLimits, GuildSticker,
    MAX_EMOJI_NAME_CHARS, MAX_SOUND_BYTES, MAX_SOUND_MS, MAX_STICKER_BYTES, RestClient, RestError,
    SoundFile, SoundboardSound, StickerFile,
};
use fastcord_model::Snowflake;
use iced::widget::{
    button, column, container, image, row, scrollable, slider, space, text, text_input,
};
use iced::{Element, Length, Task};

use crate::attachments::{AttachmentCache, DecodedImage};

const EMOJI_FILE_LIMIT: u64 = 256 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Emoji,
    Stickers,
    Sounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Emoji(Snowflake),
    Sticker(Snowflake),
    Sound(Snowflake),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Description,
    Tags,
    Emoji,
    EditName,
    EditDescription,
    EditTags,
    EditEmoji,
}

/// A chosen file, already checked.
#[derive(Clone)]
pub enum Picked {
    Emoji {
        image: EmojiImage,
        bytes: bytes::Bytes,
        animated: bool,
    },
    Sticker {
        file: StickerFile,
        bytes: bytes::Bytes,
    },
    Sound {
        file: SoundFile,
        bytes: Arc<Vec<u8>>,
        ms: u64,
    },
}

impl std::fmt::Debug for Picked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Emoji { .. } => "Picked::Emoji",
            Self::Sticker { .. } => "Picked::Sticker",
            Self::Sound { .. } => "Picked::Sound",
        })
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Open(Snowflake),
    Close,
    Tab(Tab),
    Input(Field, String),
    NewVolume(u8),
    EditVolume(u8),
    PickFile,
    Picked(u64, Result<Option<Picked>, String>),
    Previewed(u64, u64, Option<DecodedImage>),
    Upload,
    StartEdit(Target),
    CancelEdit,
    Save,
    AskDelete(Target),
    ConfirmDelete,
    CancelDelete,
    /// A sound of the list (`Some`) or the chosen file (`None`).
    Preview(Option<Snowflake>),
    Loaded(u64, Result<crate::soundboard::Handle, &'static str>),
    Done(u64, Result<(), AdminRequestError>),
    Stickers(u64, u64, Result<Vec<GuildSticker>, RestError>),
    Sounds(u64, u64, Result<Vec<SoundboardSound>, RestError>),
    /// Discord reported a change of the server's stickers or sounds.
    Changed(Snowflake, Tab),
}

/// What the owner of the state lends to one update.
pub struct Env<'a> {
    pub client: &'a RestClient,
    pub navigation: &'a NavigationSnapshot,
    pub cache: Option<&'a AttachmentCache>,
    pub output: DeviceChoice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Busy {
    Pick,
    Upload,
    Save,
    Delete,
}

struct Draft {
    name: String,
    description: String,
    tags: String,
    emoji: String,
    volume: u8,
    picked: Option<Picked>,
    preview: Option<image::Handle>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            tags: String::new(),
            emoji: String::new(),
            volume: 100,
            picked: None,
            preview: None,
        }
    }
}

struct Edit {
    target: Target,
    name: String,
    description: String,
    tags: String,
    emoji: String,
    volume: u8,
}

struct Listing<T> {
    items: Option<Vec<T>>,
    failed: Option<String>,
    ticket: u64,
}

impl<T> Default for Listing<T> {
    fn default() -> Self {
        Self {
            items: None,
            failed: None,
            ticket: 0,
        }
    }
}

struct Preview {
    /// The list sound it plays; `None` for the chosen file.
    sound: Option<Snowflake>,
    /// `None` while the sound loads.
    player: Option<Arc<FilePlayer>>,
}

impl Preview {
    /// Loading or still playing.
    fn active(&self) -> bool {
        self.player.as_ref().is_none_or(|p| {
            !matches!(
                p.snapshot().state,
                PlayerState::Ended | PlayerState::Failed(_)
            )
        })
    }
}

pub struct State {
    guild: Option<Snowflake>,
    tab: Tab,
    serial: u64,
    busy: Option<Busy>,
    notice: Option<String>,
    emoji: Draft,
    sticker: Draft,
    sound: Draft,
    stickers: Listing<GuildSticker>,
    sounds: Listing<SoundboardSound>,
    edit: Option<Edit>,
    confirm: Option<Target>,
    preview: Option<Preview>,
    ticket: u64,
    seen_stickers: u64,
    seen_sounds: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            guild: None,
            tab: Tab::Emoji,
            serial: 0,
            busy: None,
            notice: None,
            emoji: Draft::default(),
            sticker: Draft::default(),
            sound: Draft::default(),
            stickers: Listing::default(),
            sounds: Listing::default(),
            edit: None,
            confirm: None,
            preview: None,
            ticket: 0,
            seen_stickers: 0,
            seen_sounds: 0,
        }
    }
}

fn describe(error: AdminRequestError) -> String {
    match error {
        AdminRequestError::Invalid(AdminError::BadName) => {
            "That name is not allowed: emoji need 2 to 32 letters, digits or underscores; stickers 2 to 30 characters; sounds 2 to 32.".to_owned()
        }
        AdminRequestError::Invalid(AdminError::DescriptionTooLong) => {
            "A sticker description is empty or 2 to 100 characters.".to_owned()
        }
        AdminRequestError::Invalid(AdminError::UnsupportedImage) => {
            "That file type is not supported.".to_owned()
        }
        AdminRequestError::Invalid(AdminError::ImageTooLarge) => {
            "That file is larger than Discord allows.".to_owned()
        }
        AdminRequestError::Invalid(_) => {
            "That value is not allowed (sticker tags need 2 to 200 characters, a sound emoji at most 16).".to_owned()
        }
        AdminRequestError::Rest(error) => describe_rest(error),
    }
}

/// Discord's own refusal text is kept verbatim.
fn describe_rest(error: RestError) -> String {
    match error {
        RestError::PermissionDenied => {
            "Discord refused: you lack Create or Manage Expressions here.".to_owned()
        }
        RestError::ResourceGone => "That server or asset no longer exists.".to_owned(),
        RestError::Retryable(_) => {
            "Discord did not confirm. It may have been applied; check the list before trying again."
                .to_owned()
        }
        other => crate::friends::failure(other),
    }
}

/// Why an upload cannot go out now, or `None`: permission, known tier, loaded
/// list and a free slot.
fn upload_block(
    assets: Option<&GuildAssets>,
    guild: Snowflake,
    used: Option<usize>,
    slots: impl Fn(ExpressionLimits) -> usize,
) -> Option<&'static str> {
    let Some(assets) = assets.filter(|a| a.id == guild) else {
        return Some("Your permissions in this server are not known yet.");
    };
    if !assets.can_create {
        return Some("You need Create Expressions to upload here.");
    }
    let Some(limits) = assets.limits() else {
        return Some(
            "Discord did not report this server's boost level, so its free slots are unknown.",
        );
    };
    let Some(used) = used else {
        return Some("The list has not loaded yet.");
    };
    (used >= slots(limits)).then_some("No free slot is left. Delete one first.")
}

fn change_block(
    assets: Option<&GuildAssets>,
    guild: Snowflake,
    creator: Option<Snowflake>,
) -> Option<&'static str> {
    let Some(assets) = assets.filter(|a| a.id == guild) else {
        return Some("Your permissions in this server are not known yet.");
    };
    (!assets.may_change(creator))
        .then_some("You may change only what you uploaded, unless you have Manage Expressions.")
}

fn read_limited(path: &std::path::Path, limit: u64) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|_| "The file could not be read.".to_owned())?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "The file could not be read.".to_owned())?;
    Ok(bytes)
}

fn pick_emoji(path: &std::path::Path) -> Result<Picked, String> {
    let bytes = read_limited(path, EMOJI_FILE_LIMIT)?;
    let image = EmojiImage::new(&bytes).map_err(|e| {
        if e == AdminError::ImageTooLarge {
            "An emoji file is at most 256 KiB.".to_owned()
        } else {
            "Choose a PNG, JPEG, GIF or WebP image.".to_owned()
        }
    })?;
    let animated = bytes.starts_with(b"GIF8");
    Ok(Picked::Emoji {
        image,
        bytes: bytes.into(),
        animated,
    })
}

fn pick_sticker(path: &std::path::Path) -> Result<Picked, String> {
    let bytes = read_limited(path, MAX_STICKER_BYTES as u64)?;
    let file = StickerFile::new(bytes.clone()).map_err(|e| {
        if e == AdminError::ImageTooLarge {
            "A sticker file is at most 512 KiB.".to_owned()
        } else {
            "Choose a PNG, APNG, GIF or Lottie JSON file.".to_owned()
        }
    })?;
    Ok(Picked::Sticker {
        file,
        bytes: bytes.into(),
    })
}

/// The duration comes from the audio decoder, which stops at its own bound.
fn pick_sound(path: &std::path::Path) -> Result<Picked, String> {
    let bytes = read_limited(path, MAX_SOUND_BYTES as u64)?;
    let file = SoundFile::new(&bytes).map_err(|e| {
        if e == AdminError::ImageTooLarge {
            "A sound file is at most 512 KiB.".to_owned()
        } else {
            "Choose an MP3 or Ogg file.".to_owned()
        }
    })?;
    use fastcord_audio::soundboard::SoundError;
    let clip = fastcord_audio::soundboard::decode(&bytes).map_err(|e| match e {
        SoundError::TooLarge | SoundError::TooLong => "A sound is at most 5.2 seconds.".to_owned(),
        SoundError::Unsupported => "Only MP3 and Ogg Opus sounds can be checked here.".to_owned(),
        SoundError::Damaged => "That sound file is damaged.".to_owned(),
    })?;
    let ms = clip.len_ms();
    if ms > MAX_SOUND_MS {
        return Err("A sound is at most 5.2 seconds.".to_owned());
    }
    Ok(Picked::Sound {
        file,
        bytes: Arc::new(bytes),
        ms,
    })
}

impl State {
    pub fn clear(&mut self) {
        *self = Self {
            serial: self.serial.wrapping_add(1),
            ..Self::default()
        };
    }

    /// The server whose read model the panel needs.
    pub fn wanted(&self) -> Option<Snowflake> {
        self.guild
    }

    pub fn is_rejected(event: &Event) -> bool {
        matches!(
            event,
            Event::Done(
                _,
                Err(AdminRequestError::Rest(RestError::AuthenticationRequired))
            ) | Event::Stickers(_, _, Err(RestError::AuthenticationRequired))
                | Event::Sounds(_, _, Err(RestError::AuthenticationRequired))
        )
    }

    /// Changes of the open server's stickers or sounds not seen before.
    pub fn new_changes(
        &mut self,
        stickers: Option<(Snowflake, u64)>,
        sounds: Option<(Snowflake, u64)>,
    ) -> Vec<Event> {
        let mut events = Vec::new();
        if let Some((guild, seq)) = stickers.filter(|&(_, seq)| seq > self.seen_stickers) {
            self.seen_stickers = seq;
            events.push(Event::Changed(guild, Tab::Stickers));
        }
        if let Some((guild, seq)) = sounds.filter(|&(_, seq)| seq > self.seen_sounds) {
            self.seen_sounds = seq;
            events.push(Event::Changed(guild, Tab::Sounds));
        }
        events
    }

    fn draft(&mut self) -> &mut Draft {
        match self.tab {
            Tab::Emoji => &mut self.emoji,
            Tab::Stickers => &mut self.sticker,
            Tab::Sounds => &mut self.sound,
        }
    }

    fn list(&mut self, tab: Tab, guild: Snowflake, client: &RestClient) -> Task<Event> {
        let (client, serial) = (client.clone(), self.serial);
        match tab {
            Tab::Emoji => Task::none(),
            Tab::Stickers => {
                self.stickers.ticket = self.stickers.ticket.wrapping_add(1);
                let ticket = self.stickers.ticket;
                Task::perform(
                    async move { client.guild_stickers(guild).await },
                    move |result| Event::Stickers(serial, ticket, result),
                )
            }
            Tab::Sounds => {
                self.sounds.ticket = self.sounds.ticket.wrapping_add(1);
                let ticket = self.sounds.ticket;
                Task::perform(
                    async move { client.guild_soundboard_sounds(guild).await },
                    move |result| Event::Sounds(serial, ticket, result),
                )
            }
        }
    }

    fn start(&mut self, busy: Busy) {
        self.busy = Some(busy);
        self.notice = None;
    }

    fn creator_of(&self, target: Target, assets: &GuildAssets) -> Option<Snowflake> {
        match target {
            Target::Emoji(id) => assets.emojis.iter().find(|e| e.id == id)?.creator,
            Target::Sticker(id) => {
                self.stickers
                    .items
                    .as_ref()?
                    .iter()
                    .find(|s| s.id == id)?
                    .creator
            }
            Target::Sound(id) => {
                self.sounds
                    .items
                    .as_ref()?
                    .iter()
                    .find(|s| s.id == id)?
                    .creator
            }
        }
    }

    fn known(&self, target: Target, assets: &GuildAssets) -> bool {
        match target {
            Target::Emoji(id) => assets.emojis.iter().any(|e| e.id == id),
            Target::Sticker(id) => self
                .stickers
                .items
                .as_ref()
                .is_some_and(|l| l.iter().any(|s| s.id == id)),
            Target::Sound(id) => self
                .sounds
                .items
                .as_ref()
                .is_some_and(|l| l.iter().any(|s| s.id == id)),
        }
    }

    pub fn update(&mut self, event: Event, env: &Env) -> Task<Event> {
        let serial = self.serial;
        match event {
            Event::Open(guild) => {
                self.clear();
                self.guild = Some(guild);
                let (a, b) = (
                    self.list(Tab::Stickers, guild, env.client),
                    self.list(Tab::Sounds, guild, env.client),
                );
                return Task::batch([a, b]);
            }
            Event::Close if self.busy.is_none() => {
                self.clear();
            }
            Event::Close => {}
            Event::Tab(tab) => {
                self.tab = tab;
                self.edit = None;
                self.confirm = None;
            }
            Event::Input(field, value) => {
                let value: String = value.chars().take(200).collect();
                match field {
                    Field::Name => self.draft().name = value,
                    Field::Description => self.draft().description = value,
                    Field::Tags => self.draft().tags = value,
                    Field::Emoji => self.draft().emoji = value,
                    Field::EditName => self.edit_with(|e| e.name = value),
                    Field::EditDescription => self.edit_with(|e| e.description = value),
                    Field::EditTags => self.edit_with(|e| e.tags = value),
                    Field::EditEmoji => self.edit_with(|e| e.emoji = value),
                }
            }
            Event::NewVolume(v) => self.draft().volume = v.min(100),
            Event::EditVolume(v) => self.edit_with(|e| e.volume = v.min(100)),
            Event::Picked(s, result) if s == serial => {
                self.busy = None;
                match result {
                    Ok(Some(picked)) => return self.adopt(picked, env),
                    Ok(None) => {}
                    Err(reason) => self.notice = Some(reason),
                }
            }
            Event::Previewed(s, ticket, image) if s == serial && ticket == self.ticket => {
                self.draft().preview =
                    image.map(|i| image::Handle::from_rgba(i.width, i.height, i.rgba));
            }
            Event::Picked(..) | Event::Previewed(..) => {}
            Event::Done(s, result) if s == serial => {
                let was = self.busy.take();
                match result {
                    Ok(()) => {
                        match was {
                            Some(Busy::Upload) => *self.draft() = Draft::default(),
                            Some(Busy::Save) => self.edit = None,
                            _ => self.confirm = None,
                        }
                        self.notice =
                            Some("Sent. The list changes when Discord confirms.".to_owned());
                    }
                    Err(error) => self.notice = Some(describe(error)),
                }
            }
            Event::Done(..) => {}
            Event::Stickers(s, ticket, result) if s == serial && ticket == self.stickers.ticket => {
                match result {
                    Ok(items) => (self.stickers.items, self.stickers.failed) = (Some(items), None),
                    Err(error) => self.stickers.failed = Some(describe_rest(error)),
                }
            }
            Event::Sounds(s, ticket, result) if s == serial && ticket == self.sounds.ticket => {
                match result {
                    Ok(items) => (self.sounds.items, self.sounds.failed) = (Some(items), None),
                    Err(error) => self.sounds.failed = Some(describe_rest(error)),
                }
            }
            Event::Stickers(..) | Event::Sounds(..) => {}
            Event::Changed(guild, tab) => {
                let loaded = match tab {
                    Tab::Stickers => self.stickers.items.is_some(),
                    Tab::Sounds => self.sounds.items.is_some(),
                    Tab::Emoji => false,
                };
                if self.guild == Some(guild) && loaded {
                    return self.list(tab, guild, env.client);
                }
            }
            Event::Loaded(ticket, result) if ticket == self.ticket => match result {
                Ok(handle) => {
                    if let Some(p) = &mut self.preview {
                        p.player = Some(handle.0);
                    }
                }
                Err(reason) => {
                    self.preview = None;
                    self.notice = Some(reason.to_owned());
                }
            },
            Event::Loaded(..) => {}
            Event::Preview(sound) => return self.preview_sound(sound, env),
            Event::CancelEdit => self.edit = None,
            Event::CancelDelete => self.confirm = None,
            _ if self.busy.is_some() => {
                self.notice = Some("Wait for the previous request to finish.".to_owned());
            }
            Event::PickFile => {
                self.start(Busy::Pick);
                let tab = self.tab;
                return Task::perform(
                    async move {
                        let dialog = rfd::AsyncFileDialog::new();
                        let dialog = match tab {
                            Tab::Emoji => dialog
                                .set_title("Emoji image")
                                .add_filter("Image", &["png", "jpg", "jpeg", "gif", "webp"]),
                            Tab::Stickers => dialog
                                .set_title("Sticker")
                                .add_filter("Sticker", &["png", "apng", "gif", "json"]),
                            Tab::Sounds => dialog
                                .set_title("Sound")
                                .add_filter("Sound", &["mp3", "ogg", "opus"]),
                        };
                        let Some(file) = dialog.pick_file().await else {
                            return Ok(None);
                        };
                        let path = file.path().to_owned();
                        tokio::task::spawn_blocking(move || {
                            match tab {
                                Tab::Emoji => pick_emoji(&path),
                                Tab::Stickers => pick_sticker(&path),
                                Tab::Sounds => pick_sound(&path),
                            }
                            .map(Some)
                        })
                        .await
                        .unwrap_or_else(|_| Err("The file could not be read.".to_owned()))
                    },
                    move |result| Event::Picked(serial, result),
                );
            }
            Event::Upload => return self.upload(env),
            Event::StartEdit(target) => {
                let Some(guild) = self.guild else {
                    return Task::none();
                };
                let assets = env.navigation.assets.as_ref();
                let found = assets.filter(|a| self.known(target, a));
                if let Some(reason) = change_block(
                    assets,
                    guild,
                    found.and_then(|a| self.creator_of(target, a)),
                ) {
                    self.notice = Some(reason.to_owned());
                } else if let Some(a) = found {
                    self.edit = Some(self.edit_for(target, a));
                    self.confirm = None;
                }
            }
            Event::Save => return self.save(env),
            Event::AskDelete(target) => {
                self.edit = None;
                self.confirm = Some(target);
            }
            Event::ConfirmDelete => return self.delete(env),
        }
        Task::none()
    }

    fn edit_with(&mut self, change: impl FnOnce(&mut Edit)) {
        if let Some(edit) = &mut self.edit {
            change(edit);
        }
    }

    fn edit_for(&self, target: Target, assets: &GuildAssets) -> Edit {
        let mut edit = Edit {
            target,
            name: String::new(),
            description: String::new(),
            tags: String::new(),
            emoji: String::new(),
            volume: 100,
        };
        match target {
            Target::Emoji(id) => {
                if let Some(e) = assets.emojis.iter().find(|e| e.id == id) {
                    edit.name = e.name.clone();
                }
            }
            Target::Sticker(id) => {
                if let Some(s) = self.stickers.items.iter().flatten().find(|s| s.id == id) {
                    edit.name = s.name.clone();
                    edit.description = s.description.clone();
                    edit.tags = s.tags.clone();
                }
            }
            Target::Sound(id) => {
                if let Some(s) = self.sounds.items.iter().flatten().find(|s| s.id == id) {
                    edit.name = s.name.clone();
                    edit.emoji = s.emoji.clone().unwrap_or_default();
                    edit.volume = s.volume_percent;
                }
            }
        }
        edit
    }

    /// Keeps a checked file and starts its preview off the UI thread.
    fn adopt(&mut self, picked: Picked, env: &Env) -> Task<Event> {
        self.ticket = self.ticket.wrapping_add(1);
        self.preview = None;
        let (serial, ticket) = (self.serial, self.ticket);
        let preview_bytes = match &picked {
            Picked::Emoji { bytes, .. } | Picked::Sticker { bytes, .. } => Some(bytes.clone()),
            Picked::Sound { .. } => None,
        };
        let draft = self.draft();
        draft.preview = None;
        draft.picked = Some(picked);
        let (Some(bytes), Some(cache)) = (preview_bytes, env.cache.cloned()) else {
            return Task::none();
        };
        Task::perform(
            async move {
                // A file the bounded decoder refuses simply has no preview.
                cache
                    .decode_emoji(bytes)
                    .await
                    .ok()
                    .and_then(|animation| animation.frames.into_iter().next())
            },
            move |image| Event::Previewed(serial, ticket, image),
        )
    }

    fn preview_sound(&mut self, sound: Option<Snowflake>, env: &Env) -> Task<Event> {
        let playing = self
            .preview
            .take()
            .is_some_and(|p| p.sound == sound && p.active());
        if playing {
            return Task::none();
        }
        self.ticket = self.ticket.wrapping_add(1);
        let ticket = self.ticket;
        let output = env.output.clone();
        let task = match sound {
            Some(id) => {
                let Some(cache) = env.cache.cloned() else {
                    self.notice = Some("Sounds cannot be fetched right now.".to_owned());
                    return Task::none();
                };
                let percent = self
                    .sounds
                    .items
                    .iter()
                    .flatten()
                    .find(|s| s.id == id && s.available)
                    .map(|s| s.volume_percent);
                let Some(percent) = percent else {
                    return Task::none();
                };
                Task::perform(
                    crate::soundboard::start(cache, id, output, f32::from(percent) / 100.0),
                    move |r| Event::Loaded(ticket, r),
                )
            }
            None => {
                let Some(Picked::Sound { bytes, .. }) = &self.sound.picked else {
                    return Task::none();
                };
                let (bytes, gain) = (bytes.as_ref().clone(), f32::from(self.sound.volume) / 100.0);
                Task::perform(
                    crate::soundboard::start_bytes(bytes, output, gain),
                    move |r| Event::Loaded(ticket, r),
                )
            }
        };
        // The player arrives with `Loaded`; until then the row shows "Stop".
        self.preview = Some(Preview {
            sound,
            player: None,
        });
        task
    }

    fn upload(&mut self, env: &Env) -> Task<Event> {
        let Some(guild) = self.guild else {
            return Task::none();
        };
        let assets = env.navigation.assets.as_ref();
        let emoji_kind =
            matches!(&self.emoji.picked, Some(Picked::Emoji { animated, .. }) if *animated);
        let block = match self.tab {
            Tab::Emoji => {
                let used =
                    assets.map(|a| a.emojis.iter().filter(|e| e.animated == emoji_kind).count());
                upload_block(assets, guild, used, |l| l.emoji)
            }
            Tab::Stickers => upload_block(
                assets,
                guild,
                self.stickers.items.as_ref().map(Vec::len),
                |l| l.stickers,
            ),
            Tab::Sounds => upload_block(
                assets,
                guild,
                self.sounds.items.as_ref().map(Vec::len),
                |l| l.sounds,
            ),
        };
        if let Some(reason) = block {
            self.notice = Some(reason.to_owned());
            return Task::none();
        }
        let (serial, client) = (self.serial, env.client.clone());
        let draft = match self.tab {
            Tab::Emoji => &self.emoji,
            Tab::Stickers => &self.sticker,
            Tab::Sounds => &self.sound,
        };
        let (name, description, tags, emoji, volume) = (
            draft.name.clone(),
            draft.description.clone(),
            draft.tags.clone(),
            draft.emoji.clone(),
            draft.volume,
        );
        let task = match draft.picked.clone() {
            Some(Picked::Emoji { image, .. }) if self.tab == Tab::Emoji => Task::perform(
                async move { client.create_emoji(guild, &name, &image).await },
                move |r| Event::Done(serial, r),
            ),
            Some(Picked::Sticker { file, .. }) if self.tab == Tab::Stickers => Task::perform(
                async move {
                    client
                        .create_sticker(guild, &name, &description, &tags, &file)
                        .await
                },
                move |r| Event::Done(serial, r),
            ),
            Some(Picked::Sound { file, .. }) if self.tab == Tab::Sounds => Task::perform(
                async move {
                    client
                        .create_sound(guild, &name, &file, volume, &emoji)
                        .await
                },
                move |r| Event::Done(serial, r),
            ),
            _ => {
                self.notice = Some("Choose a file first.".to_owned());
                return Task::none();
            }
        };
        self.start(Busy::Upload);
        task
    }

    fn save(&mut self, env: &Env) -> Task<Event> {
        let (Some(guild), Some(edit)) = (self.guild, &self.edit) else {
            return Task::none();
        };
        let assets = env.navigation.assets.as_ref();
        let creator = assets
            .filter(|a| self.known(edit.target, a))
            .and_then(|a| self.creator_of(edit.target, a));
        let known = assets.is_some_and(|a| self.known(edit.target, a));
        if !known {
            self.notice = Some("That asset no longer exists.".to_owned());
            self.edit = None;
            return Task::none();
        }
        if let Some(reason) = change_block(assets, guild, creator) {
            self.notice = Some(reason.to_owned());
            return Task::none();
        }
        let (serial, client) = (self.serial, env.client.clone());
        let (name, description, tags, emoji, volume) = (
            edit.name.clone(),
            edit.description.clone(),
            edit.tags.clone(),
            edit.emoji.clone(),
            edit.volume,
        );
        let task = match edit.target {
            Target::Emoji(id) => Task::perform(
                async move { client.rename_emoji(guild, id, &name).await },
                move |r| Event::Done(serial, r),
            ),
            Target::Sticker(id) => Task::perform(
                async move {
                    client
                        .edit_sticker(guild, id, &name, &description, &tags)
                        .await
                },
                move |r| Event::Done(serial, r),
            ),
            Target::Sound(id) => Task::perform(
                async move { client.edit_sound(guild, id, &name, volume, &emoji).await },
                move |r| Event::Done(serial, r),
            ),
        };
        self.start(Busy::Save);
        task
    }

    fn delete(&mut self, env: &Env) -> Task<Event> {
        let (Some(guild), Some(target)) = (self.guild, self.confirm) else {
            return Task::none();
        };
        let assets = env.navigation.assets.as_ref();
        if !assets.is_some_and(|a| self.known(target, a)) {
            self.notice = Some("That asset no longer exists.".to_owned());
            self.confirm = None;
            return Task::none();
        }
        let creator = assets.and_then(|a| self.creator_of(target, a));
        if let Some(reason) = change_block(assets, guild, creator) {
            self.notice = Some(reason.to_owned());
            return Task::none();
        }
        let (serial, client) = (self.serial, env.client.clone());
        let task = match target {
            Target::Emoji(id) => Task::perform(
                async move { client.delete_emoji(guild, id).await },
                move |r| Event::Done(serial, r.map_err(AdminRequestError::Rest)),
            ),
            Target::Sticker(id) => Task::perform(
                async move { client.delete_sticker(guild, id).await },
                move |r| Event::Done(serial, r.map_err(AdminRequestError::Rest)),
            ),
            Target::Sound(id) => Task::perform(
                async move { client.delete_sound(guild, id).await },
                move |r| Event::Done(serial, r.map_err(AdminRequestError::Rest)),
            ),
        };
        self.start(Busy::Delete);
        task
    }
}

fn small<'a>(line: impl text::IntoFragment<'a>) -> iced::widget::Text<'a> {
    text(line).size(12).style(text::secondary)
}

fn slots(used: usize, limit: usize) -> String {
    format!(
        "{used} of {limit} used, {} free",
        limit.saturating_sub(used)
    )
}

pub fn view<'a>(
    state: &'a State,
    navigation: &'a NavigationSnapshot,
) -> Option<Element<'a, Event>> {
    let guild = state.guild?;
    let idle = state.busy.is_none();
    let assets = navigation.assets.as_ref().filter(|a| a.id == guild);
    let title = assets.map_or_else(
        || "Emoji, stickers and sounds".to_owned(),
        |a| format!("Emoji, stickers and sounds: {}", a.name),
    );
    let tab = |label: &'static str, which: Tab| {
        button(text(label).size(13))
            .on_press(Event::Tab(which))
            .style(if state.tab == which {
                button::primary
            } else {
                button::secondary
            })
    };
    let mut panel = column![
        row![
            text(title).size(13),
            space::horizontal(),
            crate::icons::close(idle.then_some(Event::Close)),
        ]
        .align_y(iced::Alignment::Center),
        row![
            tab("Emoji", Tab::Emoji),
            tab("Stickers", Tab::Stickers),
            tab("Soundboard", Tab::Sounds)
        ]
        .spacing(6),
    ]
    .spacing(6);
    let Some(assets) = assets else {
        return Some(
            container(panel.push(small(
                "This server is not available, or your roles in it are not known yet.",
            )))
            .padding(8)
            .into(),
        );
    };
    if !assets.can_create && !assets.can_manage {
        panel = panel.push(small(
            "You need Create or Manage Expressions to change these.",
        ));
    }
    if let Some(notice) = &state.notice {
        panel = panel.push(text(notice).size(12).style(text::danger));
    }
    panel = match state.tab {
        Tab::Emoji => panel.push(emoji_view(state, assets, idle)),
        Tab::Stickers => panel.push(sticker_view(state, assets, idle)),
        Tab::Sounds => panel.push(sound_view(state, assets, idle)),
    };
    Some(container(panel).padding(8).into())
}

fn file_row<'a>(
    draft: &'a Draft,
    label: &'static str,
    idle: bool,
    ready: bool,
) -> Element<'a, Event> {
    let mut line =
        row![button(text("Choose file").size(12)).on_press_maybe(idle.then_some(Event::PickFile)),]
            .spacing(8)
            .align_y(iced::Alignment::Center);
    if let Some(handle) = &draft.preview {
        line = line.push(image(handle.clone()).width(32).height(32));
    }
    line = line.push(small(if draft.picked.is_some() {
        "file checked"
    } else {
        label
    }));
    line = line.push(
        button(text("Upload").size(12)).on_press_maybe((idle && ready).then_some(Event::Upload)),
    );
    line.into()
}

fn name_input<'a>(draft: &'a Draft, hint: &'static str) -> Element<'a, Event> {
    text_input(hint, &draft.name)
        .on_input(|v| Event::Input(Field::Name, v))
        .size(13)
        .into()
}

fn edit_buttons<'a>(idle: bool) -> Element<'a, Event> {
    row![
        button(text("Save").size(12)).on_press_maybe(idle.then_some(Event::Save)),
        button(text("Cancel").size(12)).on_press(Event::CancelEdit),
    ]
    .spacing(6)
    .into()
}

/// Rename and Delete (with its Confirm) of one row, or the editor in its place.
fn actions<'a>(
    state: &'a State,
    assets: &GuildAssets,
    target: Target,
    creator: Option<Snowflake>,
    idle: bool,
) -> Element<'a, Event> {
    if state.confirm == Some(target) {
        return row![
            text("Delete it?").size(12).style(text::danger),
            button(text("Delete").size(12))
                .on_press_maybe(idle.then_some(Event::ConfirmDelete))
                .style(button::danger),
            button(text("Cancel").size(12)).on_press(Event::CancelDelete),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center)
        .into();
    }
    let allowed = assets.may_change(creator);
    row![
        button(text("Edit").size(12))
            .on_press_maybe((idle && allowed).then_some(Event::StartEdit(target)))
            .style(button::secondary),
        button(text("Delete").size(12))
            .on_press_maybe((idle && allowed).then_some(Event::AskDelete(target)))
            .style(button::secondary),
    ]
    .spacing(6)
    .into()
}

fn creator_label(name: Option<&str>, creator: Option<Snowflake>) -> String {
    match (name, creator) {
        (Some(name), _) => format!("by {name}"),
        (None, Some(_)) => "by a member".to_owned(),
        (None, None) => String::new(),
    }
}

fn emoji_view<'a>(state: &'a State, assets: &'a GuildAssets, idle: bool) -> Element<'a, Event> {
    let count = |animated| {
        assets
            .emojis
            .iter()
            .filter(|e| e.animated == animated)
            .count()
    };
    let summary = match assets.limits() {
        Some(l) => format!(
            "Static {}. Animated {}.",
            slots(count(false), l.emoji),
            slots(count(true), l.emoji)
        ),
        None => {
            "Free slots are unknown: Discord did not report this server's boost level.".to_owned()
        }
    };
    let mut panel = column![small(summary)].spacing(6);
    if assets.can_create {
        let ready = state.emoji.picked.is_some() && !state.emoji.name.trim().is_empty();
        panel = panel
            .push(small(format!(
                "PNG, JPEG, GIF or WebP up to 256 KiB; a name of 2 to {MAX_EMOJI_NAME_CHARS} letters, digits or underscores."
            )))
            .push(name_input(&state.emoji, "Emoji name"))
            .push(file_row(&state.emoji, "no file chosen", idle, ready));
    }
    let rows = assets.emojis.iter().map(|e| {
        let target = Target::Emoji(e.id);
        let mut line = row![].spacing(8).align_y(iced::Alignment::Center);
        match &state.edit {
            Some(edit) if edit.target == target => {
                line = line
                    .push(
                        text_input("Name", &edit.name)
                            .on_input(|v| Event::Input(Field::EditName, v))
                            .size(13),
                    )
                    .push(edit_buttons(idle));
            }
            _ => {
                line = line
                    .push(
                        text(format!(
                            ":{}:{}  {}",
                            e.name,
                            if e.animated { " (animated)" } else { "" },
                            creator_label(e.creator_name.as_deref(), e.creator)
                        ))
                        .size(13)
                        .width(Length::Fill),
                    )
                    .push(actions(state, assets, target, e.creator, idle));
            }
        }
        line.into()
    });
    panel = panel.push(
        scrollable(column(rows.collect::<Vec<Element<'a, Event>>>()).spacing(4))
            .height(Length::Fixed(200.0)),
    );
    panel.into()
}

fn sticker_view<'a>(state: &'a State, assets: &'a GuildAssets, idle: bool) -> Element<'a, Event> {
    let list = &state.stickers;
    let used = list.items.as_ref().map(Vec::len);
    let summary = match (assets.limits(), used) {
        (Some(l), Some(used)) => format!("Stickers {}.", slots(used, l.stickers)),
        (None, _) => {
            "Free slots are unknown: Discord did not report this server's boost level.".to_owned()
        }
        _ => "Loading...".to_owned(),
    };
    let mut panel = column![small(summary)].spacing(6);
    if let Some(reason) = &list.failed {
        panel = panel.push(text(reason).size(12).style(text::danger));
    }
    if assets.can_create {
        let d = &state.sticker;
        let ready = d.picked.is_some() && !d.name.trim().is_empty() && !d.tags.trim().is_empty();
        panel = panel
            .push(small(
                "PNG, APNG, GIF or Lottie JSON up to 512 KiB (Lottie needs a verified or partnered server). Tags: the related emoji name.",
            ))
            .push(name_input(d, "Sticker name"))
            .push(
                text_input("Description (optional)", &d.description)
                    .on_input(|v| Event::Input(Field::Description, v))
                    .size(13),
            )
            .push(
                text_input("Tags, for example the related emoji name", &d.tags)
                    .on_input(|v| Event::Input(Field::Tags, v))
                    .size(13),
            )
            .push(file_row(d, "no file chosen", idle, ready));
    }
    let rows = list.items.iter().flatten().map(|s| {
        let target = Target::Sticker(s.id);
        match &state.edit {
            Some(edit) if edit.target == target => column![
                text_input("Name", &edit.name)
                    .on_input(|v| Event::Input(Field::EditName, v))
                    .size(13),
                text_input("Description", &edit.description)
                    .on_input(|v| Event::Input(Field::EditDescription, v))
                    .size(13),
                text_input("Tags", &edit.tags)
                    .on_input(|v| Event::Input(Field::EditTags, v))
                    .size(13),
                edit_buttons(idle),
            ]
            .spacing(4)
            .into(),
            _ => row![
                text(format!(
                    "{}{}  {}",
                    s.name,
                    if s.available { "" } else { " (unavailable)" },
                    creator_label(None, s.creator)
                ))
                .size(13)
                .width(Length::Fill),
                actions(state, assets, target, s.creator, idle),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        }
    });
    panel
        .push(
            scrollable(column(rows.collect::<Vec<Element<'a, Event>>>()).spacing(4))
                .height(Length::Fixed(200.0)),
        )
        .into()
}

fn sound_view<'a>(state: &'a State, assets: &'a GuildAssets, idle: bool) -> Element<'a, Event> {
    let list = &state.sounds;
    let used = list.items.as_ref().map(Vec::len);
    let summary = match (assets.limits(), used) {
        (Some(l), Some(used)) => format!("Sounds {}.", slots(used, l.sounds)),
        (None, _) => {
            "Free slots are unknown: Discord did not report this server's boost level.".to_owned()
        }
        _ => "Loading...".to_owned(),
    };
    let mut panel = column![small(summary)].spacing(6);
    if let Some(reason) = &list.failed {
        panel = panel.push(text(reason).size(12).style(text::danger));
    }
    let playing = |sound: Option<Snowflake>| {
        state
            .preview
            .as_ref()
            .is_some_and(|p| p.sound == sound && p.active())
    };
    if assets.can_create {
        let d = &state.sound;
        let ready = d.picked.is_some() && !d.name.trim().is_empty();
        let length = match &d.picked {
            Some(Picked::Sound { ms, .. }) => format!("{:.1} s", *ms as f64 / 1000.0),
            _ => String::new(),
        };
        panel = panel
            .push(small(
                "MP3 or Ogg Opus up to 512 KiB and 5.2 seconds; a name of 2 to 32 characters.",
            ))
            .push(name_input(d, "Sound name"))
            .push(
                row![
                    text_input("Emoji (optional)", &d.emoji)
                        .on_input(|v| Event::Input(Field::Emoji, v))
                        .size(13),
                    small(format!("Volume {}%", d.volume)),
                    slider(0..=100u8, d.volume, Event::NewVolume).width(Length::Fixed(120.0)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .push(file_row(d, "no file chosen", idle, ready))
            .push(
                row![
                    button(
                        text(if playing(None) {
                            "Stop"
                        } else {
                            "Preview file"
                        })
                        .size(12)
                    )
                    .on_press_maybe(d.picked.is_some().then_some(Event::Preview(None)))
                    .style(button::secondary),
                    small(length),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            );
    }
    let rows = list.items.iter().flatten().map(|s| {
        let target = Target::Sound(s.id);
        match &state.edit {
            Some(edit) if edit.target == target => column![
                text_input("Name", &edit.name)
                    .on_input(|v| Event::Input(Field::EditName, v))
                    .size(13),
                row![
                    text_input("Emoji (optional)", &edit.emoji)
                        .on_input(|v| Event::Input(Field::EditEmoji, v))
                        .size(13),
                    small(format!("Volume {}%", edit.volume)),
                    slider(0..=100u8, edit.volume, Event::EditVolume).width(Length::Fixed(120.0)),
                ]
                .spacing(8),
                edit_buttons(idle),
            ]
            .spacing(4)
            .into(),
            _ => row![
                text(format!(
                    "{}{}{}",
                    s.emoji
                        .as_deref()
                        .map_or_else(String::new, |e| format!("{e} ")),
                    s.name,
                    if s.available { "" } else { " (unavailable)" }
                ))
                .size(13)
                .width(Length::Fill),
                button(
                    text(if playing(Some(s.id)) {
                        "Stop"
                    } else {
                        "Preview"
                    })
                    .size(12)
                )
                .on_press_maybe(s.available.then_some(Event::Preview(Some(s.id))))
                .style(button::secondary),
                actions(state, assets, target, s.creator, idle),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .into(),
        }
    });
    panel
        .push(
            scrollable(column(rows.collect::<Vec<Element<'a, Event>>>()).spacing(4))
                .height(Length::Fixed(200.0)),
        )
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastcord_discord::state::navigation::AssetEmoji;
    use fastcord_discord::{NetworkFailure, Refusal, RetryableFailure};
    use reqwest::StatusCode;

    const GUILD: Snowflake = Snowflake(1);
    const ME: Snowflake = Snowflake(10);
    const OTHER: Snowflake = Snowflake(11);
    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn client() -> RestClient {
        use fastcord_discord::UserToken;
        use fastcord_discord::gateway::{ClientProperties, HostOs};
        RestClient::new(
            UserToken::new("offline-assets-credential".into()),
            ClientProperties::bundled(HostOs::Linux, "en-US"),
        )
        .unwrap()
    }

    fn emoji(id: u64, animated: bool, creator: Snowflake) -> AssetEmoji {
        AssetEmoji {
            id: Snowflake(id),
            name: format!("e{id}"),
            animated,
            creator: Some(creator),
            creator_name: None,
        }
    }

    fn nav(
        create: bool,
        manage: bool,
        tier: Option<u8>,
        emojis: Vec<AssetEmoji>,
    ) -> NavigationSnapshot {
        NavigationSnapshot {
            assets: Some(GuildAssets {
                id: GUILD,
                name: "Guild".into(),
                me: ME,
                can_create: create,
                can_manage: manage,
                tier,
                emojis,
            }),
            ..NavigationSnapshot::default()
        }
    }

    fn send(state: &mut State, client: &RestClient, nav: &NavigationSnapshot, event: Event) {
        let env = Env {
            client,
            navigation: nav,
            cache: None,
            output: DeviceChoice::default(),
        };
        let _ = state.update(event, &env);
    }

    fn open(nav: &NavigationSnapshot) -> (State, RestClient) {
        let client = client();
        let mut state = State::default();
        send(&mut state, &client, nav, Event::Open(GUILD));
        (state, client)
    }

    fn picked(animated: bool) -> Option<Picked> {
        Some(Picked::Emoji {
            image: EmojiImage::new(PNG).unwrap(),
            bytes: PNG.to_vec().into(),
            animated,
        })
    }

    fn with_emoji_file(state: &mut State) {
        state.emoji.name = "wave".into();
        state.emoji.picked = picked(false);
    }

    #[test]
    fn upload_needs_create_permission_and_a_free_slot() {
        let denied = nav(false, false, Some(0), vec![]);
        let (mut state, client) = open(&denied);
        with_emoji_file(&mut state);
        send(&mut state, &client, &denied, Event::Upload);
        assert_eq!(state.busy, None);
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("Create Expressions")
        );

        let full = nav(
            true,
            false,
            Some(0),
            (0..50).map(|i| emoji(100 + i, false, ME)).collect(),
        );
        send(&mut state, &client, &full, Event::Upload);
        assert_eq!(state.busy, None);
        assert!(state.notice.as_deref().unwrap().contains("No free slot"));

        // Animated emoji have their own 50 slots.
        state.emoji.picked = picked(true);
        send(&mut state, &client, &full, Event::Upload);
        assert_eq!(state.busy, Some(Busy::Upload));
    }

    #[test]
    fn an_unknown_boost_level_assumes_no_slot() {
        let unknown = nav(true, false, None, vec![]);
        let (mut state, client) = open(&unknown);
        with_emoji_file(&mut state);
        send(&mut state, &client, &unknown, Event::Upload);
        assert_eq!(state.busy, None);
        assert!(state.notice.as_deref().unwrap().contains("boost level"));
    }

    #[test]
    fn one_upload_is_in_flight_and_a_second_click_is_refused() {
        let ok = nav(true, false, Some(0), vec![]);
        let (mut state, client) = open(&ok);
        with_emoji_file(&mut state);
        send(&mut state, &client, &ok, Event::Upload);
        assert_eq!(state.busy, Some(Busy::Upload));
        send(&mut state, &client, &ok, Event::Upload);
        assert_eq!(state.busy, Some(Busy::Upload));
        assert!(state.notice.as_deref().unwrap().contains("Wait"));
    }

    #[test]
    fn the_answer_clears_the_draft_or_shows_the_refusal() {
        let ok = nav(true, false, Some(0), vec![]);
        let (mut state, client) = open(&ok);
        with_emoji_file(&mut state);
        send(&mut state, &client, &ok, Event::Upload);
        let serial = state.serial;
        // A stale answer from another session is ignored.
        send(&mut state, &client, &ok, Event::Done(serial + 1, Ok(())));
        assert_eq!(state.busy, Some(Busy::Upload));
        let refusal = Refusal::new(StatusCode::BAD_REQUEST, Some(30008), "Maximum emojis");
        let error = AdminRequestError::Rest(RestError::Refused(refusal));
        send(&mut state, &client, &ok, Event::Done(serial, Err(error)));
        assert_eq!(state.busy, None);
        assert!(state.emoji.picked.is_some(), "a refused draft is kept");
        let notice = state.notice.clone().unwrap();
        assert!(notice.contains("30008") && notice.contains("Maximum emojis"));
        send(&mut state, &client, &ok, Event::Upload);
        send(&mut state, &client, &ok, Event::Done(serial, Ok(())));
        assert!(state.emoji.picked.is_none());
        assert!(
            state
                .notice
                .as_deref()
                .unwrap()
                .contains("Discord confirms")
        );
    }

    #[test]
    fn failures_are_described_and_ambiguous_ones_say_so() {
        let network = RestError::Retryable(RetryableFailure::Network(NetworkFailure::Connection));
        assert!(describe_rest(network).contains("may have been applied"));
        assert!(describe_rest(RestError::PermissionDenied).contains("Expressions"));
        assert!(describe_rest(RestError::ResourceGone).contains("no longer exists"));
        let limited = RestError::Http(StatusCode::TOO_MANY_REQUESTS);
        assert!(describe_rest(limited).contains("rate limiting"));
    }

    #[test]
    fn others_assets_need_manage_and_own_assets_need_create() {
        let list = vec![emoji(100, false, ME), emoji(101, false, OTHER)];
        let creator = nav(true, false, Some(1), list.clone());
        let (mut state, client) = open(&creator);
        let theirs = Target::Emoji(Snowflake(101));
        send(&mut state, &client, &creator, Event::StartEdit(theirs));
        assert!(state.edit.is_none());
        assert!(state.notice.is_some());
        let mine = Target::Emoji(Snowflake(100));
        send(&mut state, &client, &creator, Event::StartEdit(mine));
        assert!(state.edit.is_some());

        let manager = nav(false, true, Some(1), list);
        send(&mut state, &client, &manager, Event::StartEdit(theirs));
        assert_eq!(state.edit.as_ref().unwrap().target, theirs);
        // Permissions are checked again at the click: they were lost meanwhile.
        let lost = nav(false, false, Some(1), vec![emoji(101, false, OTHER)]);
        send(&mut state, &client, &lost, Event::Save);
        assert_eq!(state.busy, None);
        send(&mut state, &client, &manager, Event::Save);
        assert_eq!(state.busy, Some(Busy::Save));
    }

    #[test]
    fn delete_needs_the_confirm_and_the_asset_to_still_exist() {
        let n = nav(true, true, Some(1), vec![emoji(100, false, ME)]);
        let (mut state, client) = open(&n);
        let target = Target::Emoji(Snowflake(100));
        send(&mut state, &client, &n, Event::ConfirmDelete);
        assert_eq!(state.busy, None, "no Confirm was asked for");
        send(&mut state, &client, &n, Event::AskDelete(target));
        let gone = nav(true, true, Some(1), vec![]);
        send(&mut state, &client, &gone, Event::ConfirmDelete);
        assert_eq!(state.busy, None);
        assert!(state.confirm.is_none());
        send(&mut state, &client, &n, Event::AskDelete(target));
        send(&mut state, &client, &n, Event::ConfirmDelete);
        assert_eq!(state.busy, Some(Busy::Delete));
    }

    #[test]
    fn stickers_and_sounds_are_listed_again_once_per_change() {
        let mut state = State::default();
        assert_eq!(state.new_changes(Some((GUILD, 3)), None).len(), 1);
        assert!(state.new_changes(Some((GUILD, 3)), None).is_empty());
        assert_eq!(
            state.new_changes(Some((GUILD, 4)), Some((GUILD, 4))).len(),
            2
        );
    }

    #[test]
    fn only_a_rejected_token_stops_the_account() {
        let done = |error| Event::Done(0, Err(AdminRequestError::Rest(error)));
        assert!(State::is_rejected(&done(RestError::AuthenticationRequired)));
        assert!(!State::is_rejected(&done(RestError::PermissionDenied)));
    }

    #[test]
    fn closing_clears_and_invalidates_pending_answers() {
        let n = nav(true, false, Some(0), vec![]);
        let (mut state, client) = open(&n);
        let old = state.serial;
        send(&mut state, &client, &n, Event::Close);
        assert!(state.wanted().is_none());
        assert_ne!(state.serial, old);
    }
}
