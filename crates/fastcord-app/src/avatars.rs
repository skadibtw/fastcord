//! User avatars and server icons from Discord's public CDN. The fetch, disk LRU
//! and decode run in [`AttachmentCache::avatar`]; this module owns the small
//! render-side store: at most [`MAX_VIEWS`] 64x64 RGBA handles (about 4 MiB),
//! loaded only for the keys the visible rows currently ask for. A missing,
//! failed or still-loading image leaves the letter circle in place.
use std::collections::{HashMap, HashSet, VecDeque};

use fastcord_model::Snowflake;
use iced::task::Handle;
use iced::widget::image;
use iced::{Element, Task};

use crate::Message;
use crate::attachments::{AttachmentCache, DecodedImage};

pub const MAX_VIEWS: usize = 256;
/// Most keys asked for at once; a window of rows never needs more.
pub const MAX_WANTED: usize = 128;
const MAX_FAILED: usize = 512;
const WORKERS: usize = 4;
const CDN: &str = "https://cdn.discordapp.com";

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
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

    pub fn guild(id: Snowflake, icon: Option<&str>) -> Option<Self> {
        icon.filter(|hash| valid_hash(hash))
            .map(|hash| Self::Guild {
                id,
                hash: hash.to_owned(),
            })
    }

    /// Animated hashes (`a_...`) get the static PNG: first frame only.
    pub fn url(&self) -> String {
        match self {
            Self::User { id, hash } => format!("{CDN}/avatars/{id}/{hash}.png?size=64"),
            Self::Guild { id, hash } => format!("{CDN}/icons/{id}/{hash}.png?size=64"),
            Self::Default(n) => format!("{CDN}/embed/avatars/{n}.png"),
        }
    }

    async fn load(self, cache: AttachmentCache) -> Option<DecodedImage> {
        let url = self.url();
        let (kind, id, hash) = match &self {
            Self::User { id, hash } => (2, *id, hash.as_str()),
            Self::Guild { id, hash } => (3, *id, hash.as_str()),
            Self::Default(n) => (4, Snowflake(u64::from(*n)), "default"),
        };
        cache.avatar(kind, id, hash, &url).await.ok()
    }
}

#[derive(Default)]
pub struct State {
    pub views: HashMap<AvatarKey, image::Handle>,
    order: VecDeque<AvatarKey>,
    workers: HashMap<AvatarKey, Handle>,
    failed: HashSet<AvatarKey>,
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
        self.start(account, cache)
    }

    fn start(&mut self, account: u64, cache: Option<&AttachmentCache>) -> Task<Message> {
        let Some(cache) = cache else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for key in &self.wanted {
            if self.workers.len() == WORKERS {
                break;
            }
            if self.views.contains_key(key)
                || self.workers.contains_key(key)
                || self.failed.contains(key)
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
        image: Option<DecodedImage>,
        cache: Option<&AttachmentCache>,
    ) -> Task<Message> {
        if self.workers.remove(&key).is_none() || !self.wanted.contains(&key) {
            return Task::none();
        }
        match image {
            Some(image) => {
                while self.views.len() >= MAX_VIEWS {
                    let Some(oldest) = self.order.pop_front() else {
                        break;
                    };
                    self.views.remove(&oldest);
                }
                self.order.push_back(key.clone());
                self.views.insert(
                    key,
                    image::Handle::from_rgba(image.width, image.height, image.rgba),
                );
            }
            None => {
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
            let _ = state.loaded(1, key(n), Some(decoded()), None);
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
    fn failures_keep_the_fallback_and_are_not_retried() {
        let mut state = State::default();
        let key = AvatarKey::Default(1);
        let _ = state.synchronize(vec![key.clone()], 1, None);
        state.workers.insert(
            key.clone(),
            Task::<Message>::none().abortable().1.abort_on_drop(),
        );
        let _ = state.loaded(1, key.clone(), None, None);
        assert!(!state.views.contains_key(&key));
        assert!(state.failed.contains(&key));
    }
}
