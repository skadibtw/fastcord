//! Static option emoji: visible interests only, four workers, <=32 32px images.
use crate::attachments::{AttachmentCache, DecodedImage};
use fastcord_model::Snowflake;
use iced::widget::{image, sensor, text};
use iced::{Element, Task};
use std::collections::{HashMap, HashSet};

type Key = (Snowflake, Snowflake);
const VISIBLE: usize = 64;
const IMAGES: usize = 32;
const WORKERS: usize = 4;

#[derive(Clone, Debug)]
pub enum Event {
    Visible(u64, Key, Snowflake, bool),
    Loaded(u64, Snowflake, u64, Result<DecodedImage, ()>),
}
#[derive(Debug, Default)]
pub struct Pool {
    generation: u64,
    ticket: u64,
    visible: HashMap<Key, Snowflake>,
    views: HashMap<Snowflake, image::Handle>,
    failed: HashSet<Snowflake>,
    workers: HashMap<Snowflake, (u64, iced::task::Handle)>,
}
impl Drop for Pool {
    fn drop(&mut self) {
        // Closing/resetting the pool cancels work immediately, even when a
        // diagnostic holds a handle clone. Guards remain an extra backstop.
        for (_, handle) in self.workers.values() {
            handle.abort();
        }
    }
}

impl Pool {
    #[cfg(test)]
    pub(super) fn interests(&self) -> usize {
        self.visible.len()
    }
    pub fn clear(&mut self) {
        *self = Self {
            generation: self.generation.wrapping_add(1),
            ticket: 0,
            visible: HashMap::new(),
            views: HashMap::new(),
            failed: HashSet::new(),
            workers: HashMap::new(),
        };
    }
    pub fn update(&mut self, event: Event) {
        match event {
            Event::Visible(generation, key, id, visible) if generation == self.generation => {
                if !visible {
                    if self.visible.get(&key) == Some(&id) {
                        self.visible.remove(&key);
                    }
                } else if self.visible.len() < VISIBLE || self.visible.contains_key(&key) {
                    self.visible.insert(key, id);
                }
            }
            Event::Loaded(generation, id, ticket, result)
                if generation == self.generation
                    && self
                        .workers
                        .get(&id)
                        .is_some_and(|(current, _)| *current == ticket) =>
            {
                self.workers.remove(&id);
                match result {
                    Ok(frame)
                        if frame.width > 0
                            && frame.width <= 32
                            && frame.height > 0
                            && frame.height <= 32
                            && frame.rgba.len()
                                == frame.width as usize * frame.height as usize * 4 =>
                    {
                        self.views.insert(
                            id,
                            image::Handle::from_rgba(frame.width, frame.height, frame.rgba),
                        );
                    }
                    _ => {
                        self.failed.insert(id);
                    }
                }
            }
            _ => {}
        }
    }
    pub fn sync(&mut self, cache: Option<&AttachmentCache>) -> Task<Event> {
        let mut wanted: Vec<_> = self.visible.values().copied().collect();
        wanted.sort_unstable();
        wanted.dedup();
        wanted.truncate(IMAGES);
        self.views.retain(|id, _| wanted.contains(id));
        self.failed.retain(|id| wanted.contains(id));
        self.workers.retain(|id, (_, handle)| {
            let keep = wanted.contains(id);
            if !keep {
                handle.abort();
            }
            keep
        });
        let Some(cache) = cache else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for id in wanted {
            if self.workers.len() == WORKERS {
                break;
            }
            if self.views.contains_key(&id)
                || self.failed.contains(&id)
                || self.workers.contains_key(&id)
            {
                continue;
            }
            self.ticket = self.ticket.wrapping_add(1);
            let ticket = self.ticket;
            let generation = self.generation;
            let cache = cache.clone();
            let (task, handle) = Task::perform(
                async move {
                    let bytes = cache.custom_emoji(id, false).await.map_err(|_| ())?;
                    let decoded = cache.decode_emoji(bytes).await.map_err(|_| ())?;
                    decoded.frames.into_iter().next().ok_or(())
                },
                move |result| Event::Loaded(generation, id, ticket, result),
            )
            .abortable();
            self.workers.insert(id, (ticket, handle.abort_on_drop()));
            tasks.push(task);
        }
        Task::batch(tasks)
    }
    pub fn view<'a>(&'a self, key: Key, id: Snowflake, fallback: &'a str) -> Element<'a, Event> {
        let content: Element<'a, Event> = if let Some(handle) = self.views.get(&id) {
            image(handle.clone()).width(24).height(24).into()
        } else {
            text(fallback).size(12).into()
        };
        let generation = self.generation;
        sensor(content)
            .key((generation, key, id))
            .on_show(move |_| Event::Visible(generation, key, id, true))
            .on_hide(Event::Visible(generation, key, id, false))
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cache() -> AttachmentCache {
        AttachmentCache::new(
            &fastcord_net::NetworkContext::off(),
            Snowflake(1),
            std::env::temp_dir().join(format!(
                "fastcord-onboarding-preview-{}",
                std::process::id()
            )),
            "test",
        )
        .unwrap()
    }
    fn frame() -> DecodedImage {
        DecodedImage {
            width: 32,
            height: 32,
            rgba: vec![255; 4096].into(),
        }
    }
    #[test]
    fn only_visible_assets_start_bounded_work_and_offscreen_cancels_and_releases() {
        let mut pool = Pool::default();
        let cache = cache();
        let _ = pool.sync(Some(&cache));
        assert!(pool.workers.is_empty());
        for id in 1..=100 {
            pool.update(Event::Visible(
                0,
                (Snowflake(1), Snowflake(id)),
                Snowflake(id),
                true,
            ));
        }
        let _ = pool.sync(Some(&cache)); // tasks deliberately never polled
        assert_eq!(pool.visible.len(), VISIBLE);
        assert_eq!(pool.workers.len(), WORKERS);
        let id = Snowflake(1);
        let (ticket, handle) = pool.workers[&id].clone();
        pool.update(Event::Visible(0, (Snowflake(1), id), id, false));
        let _ = pool.sync(Some(&cache));
        assert!(handle.is_aborted());
        pool.update(Event::Loaded(0, id, ticket, Ok(frame())));
        assert!(
            !pool.views.contains_key(&id),
            "late offscreen result discarded"
        );
        let id = Snowflake(2);
        let ticket = pool.workers[&id].0;
        pool.update(Event::Loaded(0, id, ticket, Ok(frame())));
        assert_eq!(pool.views.len(), 1);
        pool.update(Event::Visible(0, (Snowflake(1), id), id, false));
        let _ = pool.sync(Some(&cache));
        assert!(pool.views.is_empty());
        let handles: Vec<_> = pool
            .workers
            .values()
            .map(|(_, handle)| handle.clone())
            .collect();
        pool.clear();
        assert!(handles.iter().all(|handle| handle.is_aborted()));
        pool.update(Event::Visible(0, (Snowflake(1), id), id, true));
        assert!(
            pool.visible.is_empty(),
            "old panel sensor cannot create new interest"
        );
    }
    #[test]
    fn dropping_account_pool_cancels_admitted_task_with_an_observer_present() {
        let mut pool = Pool::default();
        let cache = cache();
        let id = Snowflake(1);
        pool.update(Event::Visible(0, (id, id), id, true));
        let _ = pool.sync(Some(&cache)); // never poll the network future
        let observer = pool.workers[&id].1.clone();
        assert!(!observer.is_aborted());
        drop(pool);
        assert!(observer.is_aborted());
    }

    #[test]
    fn replacement_at_interest_limit_survives_the_old_sensor_hide() {
        let mut pool = Pool::default();
        for id in 1..=VISIBLE as u64 {
            pool.update(Event::Visible(
                0,
                (Snowflake(1), Snowflake(id)),
                Snowflake(id),
                true,
            ));
        }
        let key = (Snowflake(1), Snowflake(1));
        pool.update(Event::Visible(0, key, Snowflake(999), true));
        assert_eq!(pool.visible.len(), VISIBLE);
        assert_eq!(pool.visible[&key], Snowflake(999));
        pool.update(Event::Visible(0, key, Snowflake(1), false));
        assert_eq!(pool.visible[&key], Snowflake(999));
        pool.update(Event::Visible(0, key, Snowflake(999), false));
        assert!(!pool.visible.contains_key(&key));
    }

    #[test]
    fn pixels_items_and_decode_failures_are_bounded_and_old_tickets_rejected() {
        let mut pool = Pool::default();
        let cache = cache();
        for id in 1..=64 {
            pool.update(Event::Visible(
                0,
                (Snowflake(1), Snowflake(id)),
                Snowflake(id),
                true,
            ));
        }
        for _ in 0..IMAGES {
            let _ = pool.sync(Some(&cache));
            let completions: Vec<_> = pool
                .workers
                .iter()
                .map(|(id, (ticket, _))| (*id, *ticket))
                .collect();
            for (id, ticket) in completions {
                pool.update(Event::Loaded(0, id, ticket, Ok(frame())));
            }
        }
        assert_eq!(pool.views.len(), IMAGES);
        assert!(pool.workers.is_empty());
        assert_eq!(pool.views.len() * 4096, 128 * 1024);
        pool.clear();
        let id = Snowflake(1);
        pool.update(Event::Visible(1, (id, id), id, true));
        let _ = pool.sync(Some(&cache));
        let ticket = pool.workers[&id].0;
        pool.update(Event::Loaded(1, id, ticket.wrapping_sub(1), Ok(frame())));
        assert!(pool.views.is_empty() && pool.workers.contains_key(&id));
        let mut oversized = frame();
        oversized.width = 33;
        pool.update(Event::Loaded(1, id, ticket, Ok(oversized)));
        let _ = pool.sync(Some(&cache));
        assert!(pool.failed.contains(&id) && pool.views.is_empty() && pool.workers.is_empty());
    }
}
