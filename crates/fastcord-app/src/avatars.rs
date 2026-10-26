//! User avatars and server icons from Discord's public CDN. The fetch, disk LRU
//! and decode run in [`AttachmentCache::avatar`]; this module owns the small
//! render-side store: at most [`MAX_VIEWS`] handles and [`MAX_VIEW_BYTES`] of RGBA
//! (avatars are 64x64; link embed pictures are larger), loaded only for the keys the visible rows currently ask for. A missing,
//! failed or still-loading image leaves the letter circle in place.
use std::collections::{HashMap, HashSet, VecDeque};

use fastcord_model::Snowflake;
use iced::task::Handle;
use iced::widget::image;
use iced::{Element, Task};
use sha2::{Digest, Sha256};

use crate::Message;
use crate::attachments::{AttachmentCache, AttachmentError, DecodedImage};

pub const MAX_VIEWS: usize = 256;
/// Decoded pixels the store holds, whatever the handle count.
pub const MAX_VIEW_BYTES: usize = 24 * 1024 * 1024;
/// Most keys asked for at once; a window of rows never needs more.
pub const MAX_WANTED: usize = 128;
const MAX_FAILED: usize = 512;
const WORKERS: usize = 4;
/// A load that failed for a passing reason (network, 429/5xx, the cache)
/// is tried again this long after, the next time the store runs.
const RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(30);
const CDN: &str = "https://cdn.discordapp.com";

#[derive(Clone, Hash, PartialEq, Eq)]
pub enum AvatarKey {
    User {
        id: Snowflake,
        hash: String,
    },
    Guild {
        id: Snowflake,
        hash: String,
    },
    /// One of the six default avatars.
    Default(u8),
    /// A sticker's first frame (`gif` selects the `.gif` rendition of format 4).
    Sticker {
        id: Snowflake,
        gif: bool,
    },
    /// A link embed's thumbnail or image, from Discord's media proxy only.
    /// `hash` names the picture and box without the signed query.
    Embed {
        hash: String,
        url: String,
        fit: (u16, u16),
    },
}

/// Never prints a signed URL.
impl std::fmt::Debug for AvatarKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Embed { hash, fit, .. } => write!(f, "Embed({hash}, {fit:?})"),
            Self::User { id, hash } => write!(f, "User({id}, {hash})"),
            Self::Guild { id, hash } => write!(f, "Guild({id}, {hash})"),
            Self::Default(n) => write!(f, "Default({n})"),
            Self::Sticker { id, gif } => write!(f, "Sticker({id}, {gif})"),
        }
    }
}

/// Whether `url` is a plain https Discord media-proxy or CDN address.
fn discord_media_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    url.len() <= 2048
        && !url.contains(|c: char| c.is_whitespace() || c.is_control())
        && !host.contains(['@', ':'])
        && (host.ends_with(".discordapp.net") || host.ends_with(".discordapp.com"))
}

/// Hashes go into a URL and a cache identity: letters, digits and `_` only.
fn valid_hash(hash: &str) -> bool {
    !hash.is_empty()
        && hash.len() <= 40
        && hash.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

impl AvatarKey {
    /// The user's avatar, or the default avatar when there is no usable hash.
    pub fn user(id: Snowflake, avatar: Option<&str>) -> Self {
        match avatar.filter(|hash| valid_hash(hash)) {
            Some(hash) => Self::User {
                id,
                hash: hash.to_owned(),
            },
            None => Self::Default(((id.0 >> 22) % 6) as u8),
        }
    }

    /// A renderable sticker (PNG, APNG, GIF); Lottie and unknown formats have none.
    pub fn sticker(id: Snowflake, format_type: u8) -> Option<Self> {
        matches!(format_type, 1 | 2 | 4).then_some(Self::Sticker {
            id,
            gif: format_type == 4,
        })
    }

    pub fn guild(id: Snowflake, icon: Option<&str>) -> Option<Self> {
        icon.filter(|hash| valid_hash(hash))
            .map(|hash| Self::Guild {
                id,
                hash: hash.to_owned(),
            })
    }

    /// An embed picture shown inside `fit` (width, height). Only Discord's own
    /// proxy is fetched: the original third-party URL never is.
    pub fn embed(proxy_url: &str, fit: (u16, u16)) -> Option<Self> {
        if !discord_media_url(proxy_url) || fit.0 == 0 || fit.1 == 0 {
            return None;
        }
        let path = proxy_url.split(['?', '#']).next().unwrap_or("");
        let digest = Sha256::digest(format!("{path}|{}x{}", fit.0, fit.1));
        Some(Self::Embed {
            hash: format!("{digest:x}")[..24].to_owned(),
            url: proxy_url.to_owned(),
            fit,
        })
    }

    /// Animated hashes (`a_...`) get the static PNG: first frame only.
    pub fn url(&self) -> String {
        match self {
            Self::User { id, hash } => format!("{CDN}/avatars/{id}/{hash}.png?size=64"),
            Self::Guild { id, hash } => format!("{CDN}/icons/{id}/{hash}.png?size=64"),
            Self::Default(n) => format!("{CDN}/embed/avatars/{n}.png"),
            Self::Sticker { id, gif: false } => format!("{CDN}/stickers/{id}.png?size=160"),
            Self::Sticker { id, gif: true } => {
                format!("https://media.discordapp.net/stickers/{id}.gif?size=160")
            }
            Self::Embed { url, .. } => url.clone(),
        }
    }

    async fn load(self, cache: AttachmentCache) -> Result<DecodedImage, AttachmentError> {
        let url = self.url();
        if let Self::Embed { hash, fit, .. } = &self {
            let fit = (u32::from(fit.0), u32::from(fit.1));
            return cache.embed_image(hash, &url, fit).await;
        }
        let (kind, id, hash) = match &self {
            Self::User { id, hash } => (2, *id, hash.as_str()),
            Self::Guild { id, hash } => (3, *id, hash.as_str()),
            Self::Default(n) => (4, Snowflake(u64::from(*n)), "default"),
            Self::Sticker { id, .. } => (6, *id, "sticker"),
            Self::Embed { .. } => return Err(AttachmentError::InvalidUrl),
        };
        cache.avatar(kind, id, hash, &url).await
    }
}

/// Whether a failed load may succeed later without anything else changing.
fn transient(error: &AttachmentError) -> bool {
    match error {
        AttachmentError::Network
        | AttachmentError::CacheUnavailable
        | AttachmentError::WorkerStopped
        | AttachmentError::RefreshFailed => true,
        AttachmentError::Http(status) => *status == 429 || *status >= 500,
        _ => false,
    }
}

#[derive(Default)]
pub struct State {
    pub views: HashMap<AvatarKey, image::Handle>,
    order: VecDeque<(AvatarKey, usize)>,
    bytes: usize,
    workers: HashMap<AvatarKey, Handle>,
    failed: HashSet<AvatarKey>,
    /// Keys whose last load failed for a passing reason, and when to retry.
    retry_at: HashMap<AvatarKey, std::time::Instant>,
    wanted: Vec<AvatarKey>,
}

impl State {
    /// Replaces the set of visible keys and starts missing loads. Work for keys
    /// that left the screen is cancelled; finished images stay until evicted.
    pub fn synchronize(
        &mut self,
        mut wanted: Vec<AvatarKey>,
        account: u64,
        cache: Option<&AttachmentCache>,
    ) -> Task<Message> {
        wanted.truncate(MAX_WANTED);
        if wanted == self.wanted {
            return Task::none();
        }
        self.wanted = wanted;
        let wanted = &self.wanted;
        self.workers.retain(|key, _| wanted.contains(key));
        // Failures are remembered only while their key is on screen, so a
        // picture that comes back later is asked for again (#165).
        self.failed.retain(|key| wanted.contains(key));
        self.retry_at.retain(|key, _| wanted.contains(key));
        // What is on screen is the most recently used: eviction takes what
        // left the screen first.
        let (mut kept, mut shown): (VecDeque<_>, VecDeque<_>) = std::mem::take(&mut self.order)
            .into_iter()
            .partition(|(key, _)| !wanted.contains(key));
        kept.append(&mut shown);
        self.order = kept;
        self.start(account, cache)
    }

    fn start(&mut self, account: u64, cache: Option<&AttachmentCache>) -> Task<Message> {
        let Some(cache) = cache else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        let now = std::time::Instant::now();
        for key in &self.wanted {
            if self.workers.len() == WORKERS {
                break;
            }
            if self.views.contains_key(key)
                || self.workers.contains_key(key)
                || self.failed.contains(key)
                || self.retry_at.get(key).is_some_and(|at| now < *at)
            {
                continue;
            }
            let (task, handle) = Task::perform(key.clone().load(cache.clone()), {
                let key = key.clone();
                move |image| Message::AvatarLoaded(account, key.clone(), image)
            })
            .abortable();
            self.workers.insert(key.clone(), handle.abort_on_drop());
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    pub fn loaded(
        &mut self,
        account: u64,
        key: AvatarKey,
        image: Result<DecodedImage, AttachmentError>,
        cache: Option<&AttachmentCache>,
    ) -> Task<Message> {
        if self.workers.remove(&key).is_none() || !self.wanted.contains(&key) {
            return Task::none();
        }
        match image {
            Ok(image) => {
                self.retry_at.remove(&key);
                let size = image.rgba.len();
                while self.views.len() >= MAX_VIEWS || self.bytes + size > MAX_VIEW_BYTES {
                    let Some((oldest, freed)) = self.order.pop_front() else {
                        break;
                    };
                    self.views.remove(&oldest);
                    self.bytes -= freed;
                }
                self.bytes += size;
                self.order.push_back((key.clone(), size));
                self.views.insert(
                    key,
                    image::Handle::from_rgba(image.width, image.height, image.rgba),
                );
            }
            Err(error) if transient(&error) => {
                self.retry_at
                    .insert(key, std::time::Instant::now() + RETRY_AFTER);
            }
            Err(_) => {
                if self.failed.len() >= MAX_FAILED {
                    self.failed.clear();
                }
                self.failed.insert(key);
            }
        }
        self.start(account, cache)
    }
}

/// The picture when it is loaded, otherwise whatever `fallback` builds.
pub fn view<'a, M: 'a>(
    views: &HashMap<AvatarKey, image::Handle>,
    key: &AvatarKey,
    size: f32,
    fallback: impl FnOnce() -> Element<'a, M>,
) -> Element<'a, M> {
    match views.get(key) {
        Some(handle) => image(handle.clone())
            .width(size)
            .height(size)
            .border_radius(size / 2.0)
            .content_fit(iced::ContentFit::Cover)
            .into(),
        None => fallback(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded() -> DecodedImage {
        DecodedImage {
            width: 1,
            height: 1,
            rgba: bytes::Bytes::from_static(&[0, 0, 0, 255]),
        }
    }

    #[test]
    fn urls_are_public_cdn_pngs_with_a_small_size() {
        let user = AvatarKey::user(Snowflake(42), Some("abc123"));
        assert_eq!(
            user.url(),
            "https://cdn.discordapp.com/avatars/42/abc123.png?size=64"
        );
        // Animated hashes request the static PNG.
        let animated = AvatarKey::user(Snowflake(42), Some("a_abc123"));
        assert_eq!(
            animated.url(),
            "https://cdn.discordapp.com/avatars/42/a_abc123.png?size=64"
        );
        let guild = AvatarKey::guild(Snowflake(7), Some("ff00")).unwrap();
        assert_eq!(
            guild.url(),
            "https://cdn.discordapp.com/icons/7/ff00.png?size=64"
        );
    }

    #[test]
    fn stickers_use_the_public_cdn_and_only_for_renderable_formats() {
        let id = Snowflake(9);
        assert_eq!(
            AvatarKey::sticker(id, 1).unwrap().url(),
            "https://cdn.discordapp.com/stickers/9.png?size=160"
        );
        assert_eq!(
            AvatarKey::sticker(id, 2).unwrap().url(),
            "https://cdn.discordapp.com/stickers/9.png?size=160"
        );
        assert_eq!(
            AvatarKey::sticker(id, 4).unwrap().url(),
            "https://media.discordapp.net/stickers/9.gif?size=160"
        );
        assert_eq!(AvatarKey::sticker(id, 3), None, "Lottie is not rendered");
        assert_eq!(AvatarKey::sticker(id, 0), None);
    }

    #[test]
    fn missing_or_unsafe_hashes_fall_back() {
        let id = Snowflake(5 << 22);
        assert_eq!(AvatarKey::user(id, None), AvatarKey::Default(5));
        assert_eq!(AvatarKey::user(id, Some("")), AvatarKey::Default(5));
        assert_eq!(
            AvatarKey::user(id, Some("../x?y")),
            AvatarKey::Default(5),
            "a hash is never spliced into a URL unless it is a plain token"
        );
        assert_eq!(
            AvatarKey::Default(5).url(),
            "https://cdn.discordapp.com/embed/avatars/5.png"
        );
        assert_eq!(AvatarKey::guild(Snowflake(1), None), None);
        assert_eq!(AvatarKey::guild(Snowflake(1), Some("a/b")), None);
        // (id >> 22) % 6 for a large id.
        assert_eq!(
            AvatarKey::user(Snowflake(175928847299117063), None),
            AvatarKey::Default(((175928847299117063u64 >> 22) % 6) as u8)
        );
    }

    #[test]
    fn the_store_is_bounded_and_ignores_what_left_the_screen() {
        let mut state = State::default();
        let key = |n: u64| AvatarKey::User {
            id: Snowflake(n),
            hash: "h".to_owned(),
        };
        // Simulates the worker bookkeeping; no cache, so nothing is started.
        let load = |state: &mut State, n: u64| {
            let handle = Task::<Message>::none().abortable().1;
            state.workers.insert(key(n), handle.abort_on_drop());
            let _ = state.loaded(1, key(n), Ok(decoded()), None);
        };
        for round in 0..3u64 {
            let wanted: Vec<_> = (round * 128..(round + 1) * 128).map(key).collect();
            let _ = state.synchronize(wanted, 1, None);
            for n in round * 128..(round + 1) * 128 {
                load(&mut state, n);
            }
        }
        assert_eq!(state.views.len(), MAX_VIEWS);
        assert!(!state.views.contains_key(&key(0)), "oldest is evicted");
        assert!(state.views.contains_key(&key(383)));
        // A late result for a key no longer wanted is dropped.
        let _ = state.synchronize(vec![key(1000)], 1, None);
        load(&mut state, 5);
        assert!(!state.views.contains_key(&key(5)));
        // An oversized request is truncated.
        let many: Vec<_> = (0..1000).map(key).collect();
        let _ = state.synchronize(many, 1, None);
        assert_eq!(state.wanted.len(), MAX_WANTED);
    }

    #[test]
    fn what_stays_on_screen_is_never_evicted_and_passing_failures_are_retried() {
        let mut state = State::default();
        let key = |n: u64| AvatarKey::User {
            id: Snowflake(n),
            hash: "h".to_owned(),
        };
        let finish = |state: &mut State, n: u64, image: Result<DecodedImage, AttachmentError>| {
            let handle = Task::<Message>::none().abortable().1;
            state.workers.insert(key(n), handle.abort_on_drop());
            let _ = state.loaded(1, key(n), image, None);
        };
        // Key 0 (say, the own avatar) is on screen the whole time while far
        // more than MAX_VIEWS other pictures come and go around it.
        for round in 0..4u64 {
            let mut wanted = vec![key(0)];
            wanted.extend((1 + round * 127..1 + (round + 1) * 127).map(key));
            let _ = state.synchronize(wanted, 1, None);
            if round == 0 {
                finish(&mut state, 0, Ok(decoded()));
            }
            for n in 1 + round * 127..1 + (round + 1) * 127 {
                finish(&mut state, n, Ok(decoded()));
            }
        }
        assert_eq!(state.views.len(), MAX_VIEWS);
        assert!(state.views.contains_key(&key(0)), "on screen: kept");
        assert!(
            !state.views.contains_key(&key(1)),
            "left the screen: evicted"
        );

        // A network failure is not final: it waits, then may be tried again.
        let _ = state.synchronize(vec![key(900), key(901)], 1, None);
        finish(&mut state, 900, Err(AttachmentError::Network));
        assert!(!state.failed.contains(&key(900)));
        assert!(state.retry_at.contains_key(&key(900)));
        // A 404 is final while the picture stays on screen ...
        finish(&mut state, 901, Err(AttachmentError::Http(404)));
        assert!(state.failed.contains(&key(901)));
        // ... and forgotten once it leaves, so it is asked for when it returns.
        let _ = state.synchronize(vec![key(2)], 1, None);
        assert!(state.failed.is_empty() && state.retry_at.is_empty());
    }

    #[test]
    fn failures_keep_the_fallback_and_are_not_retried() {
        let mut state = State::default();
        let key = AvatarKey::Default(1);
        let _ = state.synchronize(vec![key.clone()], 1, None);
        state.workers.insert(
            key.clone(),
            Task::<Message>::none().abortable().1.abort_on_drop(),
        );
        let _ = state.loaded(1, key.clone(), Err(AttachmentError::Http(404)), None);
        assert!(!state.views.contains_key(&key));
        assert!(state.failed.contains(&key));
    }

    #[test]
    fn embed_pictures_come_only_from_discord_and_hide_their_url() {
        let proxy = "https://images-ext-1.discordapp.net/external/abc/https/cdn.example/a.png?ex=1&hm=secret";
        let key = AvatarKey::embed(proxy, (80, 40)).unwrap();
        assert_eq!(key.url(), proxy);
        assert!(!format!("{key:?}").contains("secret"));
        // The signature is not part of the identity; the box is.
        let resigned = proxy.replace("hm=secret", "hm=other");
        let (AvatarKey::Embed { hash: a, .. }, Some(AvatarKey::Embed { hash: b, .. })) =
            (&key, AvatarKey::embed(&resigned, (80, 40)))
        else {
            panic!("embed keys");
        };
        assert_eq!(*a, b);
        let Some(AvatarKey::Embed { hash: c, .. }) = AvatarKey::embed(proxy, (80, 80)) else {
            panic!("embed key");
        };
        assert_ne!(*a, c);
        for bad in [
            "http://media.discordapp.net/a.png",
            "https://example.com/a.png",
            "https://media.discordapp.net.evil.test/a.png",
            "https://u:p@media.discordapp.net/a.png",
            "https://media.discordapp.net:8443/a.png",
            "https://media.discordapp.net/a b.png",
        ] {
            assert_eq!(AvatarKey::embed(bad, (80, 80)), None, "{bad}");
        }
        assert_eq!(AvatarKey::embed(proxy, (0, 80)), None);
    }

    #[test]
    fn pictures_are_bounded_by_bytes_and_a_failure_keeps_the_fallback() {
        let mut state = State::default();
        let key = |n: u64| {
            AvatarKey::embed(
                &format!("https://media.discordapp.net/a/{n}.png"),
                (400, 300),
            )
            .unwrap()
        };
        let big = || DecodedImage {
            width: 1_000,
            height: 1_000,
            rgba: bytes::Bytes::from(vec![0u8; 4_000_000]),
        };
        for n in 0..10 {
            let _ = state.synchronize(vec![key(n)], 1, None);
            state.workers.insert(
                key(n),
                Task::<Message>::none().abortable().1.abort_on_drop(),
            );
            let _ = state.loaded(1, key(n), Ok(big()), None);
        }
        assert!(state.bytes <= MAX_VIEW_BYTES && state.views.len() == 6);
        assert!(!state.views.contains_key(&key(0)) && state.views.contains_key(&key(9)));
        let _ = state.synchronize(vec![key(50)], 1, None);
        state.workers.insert(
            key(50),
            Task::<Message>::none().abortable().1.abort_on_drop(),
        );
        let _ = state.loaded(1, key(50), Err(AttachmentError::InvalidImage), None);
        assert!(!state.views.contains_key(&key(50)) && state.failed.contains(&key(50)));
    }
}
