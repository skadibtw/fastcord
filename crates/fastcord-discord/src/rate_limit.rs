use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use reqwest::header::HeaderMap;
use serde::Deserialize;
use tokio::sync::Notify;

use crate::{Clock, MajorParameter, Priority, RestError, RouteKey};

const TRACKED_BYTE_BUDGET: usize = 2 * 1024 * 1024;
const WORK_BYTE_BUDGET: usize = 8 * 1024 * 1024;
const MAX_BUCKET_HASH_BYTES: usize = 256;

#[derive(Default)]
pub(crate) struct ResponseLimits {
    bucket: Option<Arc<str>>,
    limit: Option<u32>,
    remaining: Option<u32>,
    reset_after: Option<Duration>,
    retry_after: Option<Duration>,
    global: bool,
    observed_at: Duration,
}

fn seconds(value: f64) -> Option<Duration> {
    Duration::try_from_secs_f64(value).ok()
}

impl ResponseLimits {
    pub(crate) fn from_headers(headers: &HeaderMap, observed_at: Duration) -> Self {
        let text = |name| headers.get(name).and_then(|value| value.to_str().ok());
        let duration = |name| {
            text(name)
                .and_then(|value| value.parse().ok())
                .and_then(seconds)
        };
        Self {
            bucket: text("x-ratelimit-bucket")
                .filter(|value| !value.is_empty() && value.len() <= MAX_BUCKET_HASH_BYTES)
                .map(Arc::from),
            limit: text("x-ratelimit-limit").and_then(|value| value.parse().ok()),
            remaining: text("x-ratelimit-remaining").and_then(|value| value.parse().ok()),
            reset_after: duration("x-ratelimit-reset-after"),
            retry_after: duration("retry-after"),
            global: text("x-ratelimit-global")
                .is_some_and(|value| value.eq_ignore_ascii_case("true"))
                || text("x-ratelimit-scope")
                    .is_some_and(|value| value.eq_ignore_ascii_case("global")),
            observed_at,
        }
    }

    fn header_retry(&self) -> Option<Duration> {
        self.retry_after.max(self.reset_after)
    }

    pub(crate) fn throttle(&self, body: &[u8]) -> Result<Throttle, RestError> {
        #[derive(Deserialize)]
        struct Body {
            retry_after: Option<f64>,
            #[serde(default)]
            global: bool,
        }
        let decoded = serde_json::from_slice::<Body>(body).ok();
        let retry = decoded
            .as_ref()
            .and_then(|body| body.retry_after)
            .and_then(seconds);
        let delay = retry
            .max(self.header_retry())
            .ok_or(RestError::InvalidRateLimit)?;
        let until = self
            .observed_at
            .checked_add(delay)
            .ok_or(RestError::InvalidRateLimit)?;
        Ok(Throttle {
            until,
            global: self.global || decoded.is_some_and(|body| body.global),
        })
    }
}

pub(crate) struct Throttle {
    until: Duration,
    global: bool,
}

#[derive(Default)]
struct Bucket {
    limit: Option<u32>,
    remaining: Option<u32>,
    reset_at: Option<Duration>,
    blocked_until: Option<Duration>,
    in_flight: u32,
    generation: u64,
}

impl Bucket {
    fn refresh(&mut self, now: Duration) {
        if self.reset_at.is_some_and(|at| at <= now) {
            self.remaining = self.limit.map(|limit| limit.saturating_sub(self.in_flight));
            self.reset_at = None;
            self.generation += 1;
        }
        if self.blocked_until.is_some_and(|at| at <= now) {
            self.blocked_until = None;
        }
    }

    fn available(&self) -> bool {
        if self.blocked_until.is_some() {
            false
        } else if self.remaining.is_some_and(|remaining| remaining > 0) {
            true
        } else if self.reset_at.is_some() {
            false
        } else {
            // Missing headers (including exhausted post-reset windows with no
            // new deadline) fall back to a serial probe, never an invented rate.
            self.in_flight == 0
        }
    }

    fn deadline(&self) -> Option<Duration> {
        let exhausted = self.remaining == Some(0);
        self.blocked_until
            .max(if exhausted { self.reset_at } else { None })
    }

    fn idle_expired(&self) -> bool {
        self.in_flight == 0 && self.reset_at.is_none() && self.blocked_until.is_none()
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct LearnedKey {
    hash: Arc<str>,
    major: MajorParameter,
}

struct Window {
    bucket: u64,
    generation: u64,
}

struct Work {
    route: RouteKey,
    priority: Priority,
    bytes: usize,
    window: Option<Window>,
}

#[derive(Default)]
struct State {
    stopped: bool,
    global_until: Option<Duration>,
    routes: HashMap<RouteKey, u64>,
    learned: HashMap<LearnedKey, u64>,
    buckets: HashMap<u64, Bucket>,
    pending: BTreeMap<u64, Work>,
    active: HashMap<u64, Work>,
    next_work: u64,
    next_bucket: u64,
    tracked_bytes: usize,
    work_bytes: usize,
}

impl State {
    fn prune(&mut self, now: Duration) {
        for bucket in self.buckets.values_mut() {
            bucket.refresh(now);
        }
        let pending: HashSet<_> = self.pending.values().map(|work| &work.route).collect();
        self.routes.retain(|route, id| {
            let keep = pending.contains(route) || !self.buckets[id].idle_expired();
            if !keep {
                self.tracked_bytes -= route.tracked_bytes();
            }
            keep
        });
        let used: HashSet<_> = self.routes.values().copied().collect();
        self.buckets.retain(|id, _| used.contains(id));
        self.learned.retain(|_, id| used.contains(id));
    }

    fn ensure_route(&mut self, route: &RouteKey, now: Duration) -> Result<(), RestError> {
        if self.routes.contains_key(route) {
            return Ok(());
        }
        let bytes = route.tracked_bytes();
        if self.tracked_bytes + bytes > TRACKED_BYTE_BUDGET {
            self.prune(now);
        }
        if self.tracked_bytes + bytes > TRACKED_BYTE_BUDGET {
            return Err(RestError::SchedulerCapacityExceeded);
        }
        let id = self.next_bucket;
        self.next_bucket += 1;
        self.buckets.insert(id, Bucket::default());
        self.routes.insert(route.clone(), id);
        self.tracked_bytes += bytes;
        Ok(())
    }

    fn admit(
        &mut self,
        route: RouteKey,
        priority: Priority,
        bytes: usize,
        now: Duration,
    ) -> Result<u64, RestError> {
        if self.stopped {
            return Err(RestError::AuthenticationRequired);
        }
        if bytes > WORK_BYTE_BUDGET || self.work_bytes + bytes > WORK_BYTE_BUDGET {
            return Err(RestError::SchedulerCapacityExceeded);
        }
        self.ensure_route(&route, now)?;
        let id = self.next_work;
        self.next_work += 1;
        self.pending.insert(
            id,
            Work {
                route,
                priority,
                bytes,
                window: None,
            },
        );
        self.work_bytes += bytes;
        Ok(id)
    }

    fn reserve(
        &mut self,
        id: u64,
        now: Duration,
    ) -> Result<Result<(), Option<Duration>>, RestError> {
        if self.stopped {
            return Err(RestError::AuthenticationRequired);
        }
        if self.global_until.is_some_and(|at| at > now) {
            return Ok(Err(self.global_until));
        }
        self.global_until = None;
        let mut chosen = None;
        for (&ticket, work) in &self.pending {
            let bucket = self
                .buckets
                .get_mut(&self.routes[&work.route])
                .expect("registered route bucket");
            bucket.refresh(now);
            if bucket.available() {
                let rank = (work.priority, ticket);
                if chosen.is_none_or(|best| rank < best) {
                    chosen = Some(rank);
                }
            }
        }
        if chosen.is_some_and(|(_, ticket)| ticket == id) {
            let mut work = self.pending.remove(&id).expect("registered pending work");
            let bucket_id = self.routes[&work.route];
            let bucket = self
                .buckets
                .get_mut(&bucket_id)
                .expect("registered route bucket");
            bucket.in_flight += 1;
            if let Some(remaining) = &mut bucket.remaining {
                *remaining = remaining.saturating_sub(1);
            }
            work.window = Some(Window {
                bucket: bucket_id,
                generation: bucket.generation,
            });
            self.active.insert(id, work);
            Ok(Ok(()))
        } else {
            let work = &self.pending[&id];
            Ok(Err(self.buckets[&self.routes[&work.route]].deadline()))
        }
    }

    fn release(&mut self, id: u64) {
        if let Some(work) = self.pending.remove(&id) {
            self.work_bytes -= work.bytes;
        } else if let Some(work) = self.active.remove(&id) {
            let window = work.window.expect("active reservation window");
            self.buckets
                .get_mut(&window.bucket)
                .expect("active bucket")
                .in_flight -= 1;
            self.work_bytes -= work.bytes;
            // Do not refund remaining: a canceled request may have reached Discord.
        }
    }

    fn learn(&mut self, route: &RouteKey, source: u64, hash: Arc<str>) -> u64 {
        let key = LearnedKey {
            hash,
            major: route.major().clone(),
        };
        let target = if let Some(&id) = self.learned.get(&key) {
            id
        } else {
            // Reuse a private/provisional bucket; split just this route out of a
            // shared bucket when Discord changes its advertised bucket hash.
            let private = self.routes.values().filter(|&&id| id == source).count() == 1;
            let id = if private {
                self.learned.retain(|_, id| *id != source);
                let bucket = self.buckets.get_mut(&source).expect("source bucket");
                bucket.limit = None;
                bucket.remaining = None;
                bucket.reset_at = None;
                // Keep an already-observed 429 pause across a hash change.
                source
            } else {
                let id = self.next_bucket;
                self.next_bucket += 1;
                self.buckets.insert(id, Bucket::default());
                id
            };
            self.learned.insert(key, id);
            id
        };
        if source != target {
            self.routes.insert(route.clone(), target);
            let generation = self.buckets[&target].generation;
            let mut transferred = 0;
            for work in self.active.values_mut().filter(|work| work.route == *route) {
                work.window = Some(Window {
                    bucket: target,
                    generation,
                });
                transferred += 1;
            }
            let source_bucket = self.buckets.get_mut(&source).expect("source bucket");
            source_bucket.in_flight -= transferred;
            let pause = source_bucket.blocked_until;
            let target_bucket = self.buckets.get_mut(&target).expect("learned bucket");
            target_bucket.in_flight += transferred;
            target_bucket.blocked_until = target_bucket.blocked_until.max(pause);
            if let Some(remaining) = &mut target_bucket.remaining {
                *remaining = remaining.saturating_sub(transferred);
            }
            if !self.routes.values().any(|&id| id == source) {
                self.buckets.remove(&source);
                self.learned.retain(|_, id| *id != source);
            }
        }
        target
    }

    fn complete(
        &mut self,
        id: u64,
        limits: ResponseLimits,
        throttle: Option<Throttle>,
        now: Duration,
    ) {
        let work = self.active.remove(&id).expect("active reservation");
        self.work_bytes -= work.bytes;
        let window = work.window.expect("active reservation window");
        let source = self.buckets.get_mut(&window.bucket).expect("active bucket");
        source.in_flight -= 1;
        source.refresh(now);
        let fresh = source.generation == window.generation;
        let target = limits.bucket.map_or(window.bucket, |hash| {
            self.learn(&work.route, window.bucket, hash)
        });
        let bucket = self.buckets.get_mut(&target).expect("response bucket");
        bucket.refresh(now);
        if let Some(limit) = limits.limit {
            bucket.limit = Some(limit);
        }
        if (fresh || target != window.bucket)
            && let Some(reset_at) = limits
                .reset_after
                .and_then(|after| limits.observed_at.checked_add(after))
                .filter(|&at| at > now)
            && let Some(remaining) = limits.remaining
            && let Some(limit) = bucket.limit
        {
            let available = remaining.min(limit).saturating_sub(bucket.in_flight);
            bucket.remaining = Some(
                bucket
                    .remaining
                    .map_or(available, |current| current.min(available)),
            );
            bucket.reset_at = bucket.reset_at.max(Some(reset_at));
        }
        if let Some(throttle) = throttle {
            if throttle.global {
                self.global_until = self.global_until.max(Some(throttle.until));
            } else {
                bucket.blocked_until = bucket.blocked_until.max(Some(throttle.until));
            }
        }
    }
}

struct Inner<C> {
    clock: C,
    state: Mutex<State>,
    changed: Notify,
}

pub(crate) struct RateLimiter<C> {
    inner: Arc<Inner<C>>,
}

impl<C> Clone for RateLimiter<C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<C: Clock> RateLimiter<C> {
    pub(crate) fn new(clock: C) -> Self {
        Self {
            inner: Arc::new(Inner {
                clock,
                state: Mutex::new(State::default()),
                changed: Notify::new(),
            }),
        }
    }

    pub(crate) fn now(&self) -> Duration {
        self.inner.clock.now()
    }

    pub(crate) fn stop(&self) {
        self.inner.lock().stopped = true;
        self.inner.changed.notify_waiters();
    }

    pub(crate) async fn authentication_required(&self) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.inner.lock().stopped {
                return;
            }
            changed.await;
        }
    }

    pub(crate) fn pause_from_headers(&self, route: &RouteKey, limits: &ResponseLimits) {
        if let Some(until) = limits
            .header_retry()
            .and_then(|after| limits.observed_at.checked_add(after))
        {
            let mut state = self.inner.lock();
            if limits.global {
                state.global_until = state.global_until.max(Some(until));
            } else if let Some(&id) = state.routes.get(route) {
                let bucket = state.buckets.get_mut(&id).expect("registered route bucket");
                bucket.blocked_until = bucket.blocked_until.max(Some(until));
            }
            drop(state);
            self.inner.changed.notify_waiters();
        }
    }

    pub(crate) async fn acquire(
        &self,
        route: RouteKey,
        priority: Priority,
        bytes: usize,
    ) -> Result<Reservation<C>, RestError> {
        let id = self
            .inner
            .lock()
            .admit(route, priority, bytes, self.now())?;
        let reservation = Reservation {
            inner: self.inner.clone(),
            id,
        };
        self.inner.changed.notify_waiters();
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let decision = self.inner.lock().reserve(id, self.now())?;
            match decision {
                Ok(()) => {
                    self.inner.changed.notify_waiters();
                    return Ok(reservation);
                }
                Err(Some(deadline)) => {
                    tokio::select! {
                        () = changed => {},
                        () = self.inner.clock.sleep_until(deadline) => {},
                    }
                }
                Err(None) => changed.await,
            }
        }
    }
}

impl<C> Inner<C> {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub(crate) struct Reservation<C> {
    inner: Arc<Inner<C>>,
    id: u64,
}

impl<C: Clock> Reservation<C> {
    pub(crate) fn complete(self, limits: ResponseLimits, throttle: Option<Throttle>) {
        self.inner
            .lock()
            .complete(self.id, limits, throttle, self.inner.clock.now());
        self.inner.changed.notify_waiters();
    }
}

impl<C> Drop for Reservation<C> {
    fn drop(&mut self) {
        self.inner.lock().release(self.id);
        self.inner.changed.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
