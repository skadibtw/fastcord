use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use reqwest::Method;
use reqwest::header::{HeaderName, HeaderValue};

use super::*;
use crate::Route;

#[derive(Default)]
struct Time {
    nanos: AtomicU64,
    changed: Notify,
}

#[derive(Clone, Default)]
struct TestClock(Arc<Time>);

impl TestClock {
    fn advance(&self, duration: Duration) {
        self.0.nanos.fetch_add(
            u64::try_from(duration.as_nanos()).unwrap(),
            Ordering::SeqCst,
        );
        self.0.changed.notify_waiters();
    }
}

impl Clock for TestClock {
    fn now(&self) -> Duration {
        Duration::from_nanos(self.0.nanos.load(Ordering::SeqCst))
    }

    async fn sleep_until(&self, deadline: Duration) {
        loop {
            let changed = self.0.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.now() >= deadline {
                return;
            }
            changed.await;
        }
    }
}

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn ready<F: Future>(future: Pin<&mut F>) -> F::Output {
    match poll(future) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("expected scheduler readiness"),
    }
}

fn route(method: Method, path: &str) -> RouteKey {
    Route::new(method, path).unwrap().key().clone()
}

fn acquire(limiter: &RateLimiter<TestClock>, route: &RouteKey) -> Reservation<TestClock> {
    let mut request = Box::pin(limiter.acquire(route.clone(), Priority::UserRead, 1024));
    ready(request.as_mut()).unwrap()
}

fn limits(
    limiter: &RateLimiter<TestClock>,
    remaining: u32,
    limit: u32,
    reset: Duration,
) -> ResponseLimits {
    ResponseLimits {
        bucket: Some(Arc::from("fixture-shared-bucket")),
        limit: Some(limit),
        remaining: Some(remaining),
        reset_after: Some(reset),
        observed_at: limiter.now(),
        ..ResponseLimits::default()
    }
}

fn seed(
    limiter: &RateLimiter<TestClock>,
    route: &RouteKey,
    remaining: u32,
    limit: u32,
    reset: Duration,
) {
    acquire(limiter, route).complete(limits(limiter, remaining, limit, reset), None);
}

fn fixture(name: &str, at: Duration) -> (ResponseLimits, Vec<u8>) {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("../../../../fixtures/rest/rate-limits.json")).unwrap();
    let mut headers = HeaderMap::new();
    for (key, value) in fixtures[name]["headers"].as_object().unwrap() {
        headers.insert(
            HeaderName::from_bytes(key.as_bytes()).unwrap(),
            HeaderValue::from_str(value.as_str().unwrap()).unwrap(),
        );
    }
    let body = serde_json::to_vec(&fixtures[name]["body"]).unwrap();
    (ResponseLimits::from_headers(&headers, at), body)
}

#[test]
fn shared_hash_shares_routes_and_methods_but_never_different_major_ids() {
    let limiter = RateLimiter::new(TestClock::default());
    let one = route(Method::GET, "/channels/1/messages");
    let write_one = route(Method::POST, "/channels/1/messages");
    let two = route(Method::GET, "/channels/2/messages");
    seed(&limiter, &one, 2, 3, Duration::from_secs(1));
    seed(&limiter, &write_one, 1, 3, Duration::from_secs(1));
    seed(&limiter, &two, 2, 3, Duration::from_secs(1));
    let _one = acquire(&limiter, &one);
    let mut blocked_write = Box::pin(limiter.acquire(write_one, Priority::UserWrite, 1024));
    assert!(poll(blocked_write.as_mut()).is_pending());
    let _two_first = acquire(&limiter, &two);
    let _two_second = acquire(&limiter, &two);
    let mut blocked_two = Box::pin(limiter.acquire(two, Priority::UserRead, 1024));
    assert!(poll(blocked_two.as_mut()).is_pending());
    let state = limiter.inner.lock();
    assert_eq!(state.buckets.len(), 2);
    assert_eq!(state.learned.len(), 2);
}

#[test]
fn fractional_reset_uses_only_injected_monotonic_time() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let key = route(Method::GET, "/channels/1/messages");
    let (limits, _) = fixture("fractional", limiter.now());
    acquire(&limiter, &key).complete(limits, None);
    let mut waiting = Box::pin(limiter.acquire(key, Priority::UserRead, 1024));
    assert!(poll(waiting.as_mut()).is_pending());
    clock.advance(Duration::from_millis(124));
    assert!(poll(waiting.as_mut()).is_pending());
    clock.advance(Duration::from_millis(1));
    let _reservation = ready(waiting.as_mut()).unwrap();
}

#[test]
fn global_429_pauses_every_route_including_unknown_routes() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let one = route(Method::GET, "/channels/1/messages");
    let two = route(Method::GET, "/users/@me");
    let reservation = acquire(&limiter, &one);
    let (limits, body) = fixture("global_429", limiter.now());
    let throttle = limits.throttle(&body).unwrap();
    reservation.complete(limits, Some(throttle));
    let mut second = Box::pin(limiter.acquire(two, Priority::UserRead, 1024));
    let mut first = Box::pin(limiter.acquire(one, Priority::UserRead, 1024));
    assert!(poll(second.as_mut()).is_pending());
    assert!(poll(first.as_mut()).is_pending());
    clock.advance(Duration::from_millis(499));
    assert!(poll(second.as_mut()).is_pending());
    assert!(poll(first.as_mut()).is_pending());
    clock.advance(Duration::from_millis(1));
    let _second = ready(second.as_mut()).unwrap();
    let _first = ready(first.as_mut()).unwrap();
}

#[test]
fn per_route_429_honors_longest_retry_without_pausing_another_major() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let one = route(Method::GET, "/channels/1/messages");
    let two = route(Method::GET, "/channels/2/messages");
    let (limits, body) = fixture("route_429", limiter.now());
    let throttle = limits.throttle(&body).unwrap();
    acquire(&limiter, &one).complete(limits, Some(throttle));
    let mut first = Box::pin(limiter.acquire(one, Priority::UserWrite, 1024));
    assert!(poll(first.as_mut()).is_pending());
    let _other_major = acquire(&limiter, &two);
    clock.advance(Duration::from_millis(374));
    assert!(poll(first.as_mut()).is_pending());
    clock.advance(Duration::from_millis(1));
    let _retry = ready(first.as_mut()).unwrap();
}

#[test]
fn concurrent_reservations_and_out_of_order_responses_cannot_restore_spent_capacity() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let key = route(Method::GET, "/channels/1/messages");
    seed(&limiter, &key, 3, 4, Duration::from_secs(1));
    let first = acquire(&limiter, &key);
    let second = acquire(&limiter, &key);
    let third = acquire(&limiter, &key);
    let mut fourth = Box::pin(limiter.acquire(key, Priority::UserRead, 1024));
    assert!(poll(fourth.as_mut()).is_pending());
    third.complete(limits(&limiter, 0, 4, Duration::from_secs(1)), None);
    first.complete(limits(&limiter, 2, 4, Duration::from_secs(1)), None);
    second.complete(limits(&limiter, 1, 4, Duration::from_secs(1)), None);
    assert!(poll(fourth.as_mut()).is_pending());
    clock.advance(Duration::from_secs(1));
    let _fourth = ready(fourth.as_mut()).unwrap();
}

#[test]
fn reset_deducts_outstanding_reservations_and_ignores_old_window_responses() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let key = route(Method::GET, "/channels/1/messages");
    seed(&limiter, &key, 2, 3, Duration::from_millis(125));
    let first = acquire(&limiter, &key);
    let second = acquire(&limiter, &key);
    let old_first = limits(&limiter, 2, 3, Duration::from_millis(125));
    let old_second = limits(&limiter, 1, 3, Duration::from_millis(125));
    clock.advance(Duration::from_millis(125));
    let _new_window = acquire(&limiter, &key);
    let mut excess = Box::pin(limiter.acquire(key, Priority::UserRead, 1024));
    assert!(poll(excess.as_mut()).is_pending());
    first.complete(old_first, None);
    second.complete(old_second, None);
    assert!(poll(excess.as_mut()).is_pending());
}

#[test]
fn unknown_routes_allow_only_one_in_flight_and_cancellation_releases_them() {
    let limiter = RateLimiter::new(TestClock::default());
    let key = route(Method::GET, "/users/@me");
    let first = acquire(&limiter, &key);
    let mut second = Box::pin(limiter.acquire(key, Priority::UserRead, 1024));
    assert!(poll(second.as_mut()).is_pending());
    drop(first);
    let second = ready(second.as_mut()).unwrap();
    second.complete(ResponseLimits::default(), None);
    assert_eq!(limiter.inner.lock().work_bytes, 0);
}

#[test]
fn user_writes_overtake_speculative_reads_when_a_bucket_resets() {
    let clock = TestClock::default();
    let limiter = RateLimiter::new(clock.clone());
    let read = route(Method::GET, "/channels/1/messages");
    let write = route(Method::POST, "/channels/1/messages");
    seed(&limiter, &read, 1, 1, Duration::from_millis(125));
    seed(&limiter, &write, 0, 1, Duration::from_millis(125));
    let mut speculative = Box::pin(limiter.acquire(read, Priority::SpeculativeRead, 1024));
    let mut user_write = Box::pin(limiter.acquire(write, Priority::UserWrite, 1024));
    assert!(poll(speculative.as_mut()).is_pending());
    assert!(poll(user_write.as_mut()).is_pending());
    clock.advance(Duration::from_millis(125));
    assert!(poll(speculative.as_mut()).is_pending());
    let _write = ready(user_write.as_mut()).unwrap();
    assert!(poll(speculative.as_mut()).is_pending());
}

#[test]
fn auth_stop_wakes_queued_work_active_cancellation_and_all_future_work() {
    let limiter = RateLimiter::new(TestClock::default());
    let key = route(Method::GET, "/users/@me");
    let active = acquire(&limiter, &key);
    let mut queued = Box::pin(limiter.acquire(key.clone(), Priority::UserRead, 1024));
    let mut stop_signal = Box::pin(limiter.authentication_required());
    assert!(poll(queued.as_mut()).is_pending());
    assert!(poll(stop_signal.as_mut()).is_pending());
    limiter.stop();
    assert!(matches!(
        ready(queued.as_mut()),
        Err(RestError::AuthenticationRequired)
    ));
    ready(stop_signal.as_mut());
    drop(active);
    let mut fresh = Box::pin(limiter.acquire(key, Priority::UserWrite, 1024));
    assert!(matches!(
        ready(fresh.as_mut()),
        Err(RestError::AuthenticationRequired)
    ));
    assert_eq!(limiter.inner.lock().work_bytes, 0);
}

#[test]
fn joining_a_learned_bucket_accounts_for_already_outstanding_work() {
    let limiter = RateLimiter::new(TestClock::default());
    let first_route = route(Method::GET, "/channels/1/messages");
    let second_route = route(Method::POST, "/channels/1/messages");
    seed(&limiter, &first_route, 3, 4, Duration::from_secs(1));
    let _already_active = acquire(&limiter, &first_route);
    acquire(&limiter, &second_route).complete(limits(&limiter, 2, 4, Duration::from_secs(1)), None);
    let _last = acquire(&limiter, &first_route);
    let mut excess = Box::pin(limiter.acquire(second_route, Priority::UserWrite, 1024));
    assert!(poll(excess.as_mut()).is_pending());
}

#[test]
fn global_flag_from_body_scope_or_header_is_respected_and_bad_timing_is_rejected() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("0.25"));
    headers.insert("x-ratelimit-scope", HeaderValue::from_static("global"));
    let parsed = ResponseLimits::from_headers(&headers, Duration::from_secs(7));
    let throttle = parsed.throttle(b"not JSON").unwrap();
    assert!(throttle.global);
    assert_eq!(throttle.until, Duration::from_millis(7250));
    let body_only = ResponseLimits::default()
        .throttle(br#"{"retry_after":0.125,"global":true}"#)
        .unwrap();
    assert!(body_only.global);
    assert_eq!(body_only.until, Duration::from_millis(125));
    for value in ["NaN", "inf", "-1", "1e100"] {
        headers.insert("retry-after", HeaderValue::from_str(value).unwrap());
        let parsed = ResponseLimits::from_headers(&headers, Duration::ZERO);
        assert!(matches!(
            parsed.throttle(b"{}"),
            Err(RestError::InvalidRateLimit)
        ));
    }
}

#[test]
fn byte_budgets_reject_excess_work_and_expired_cache_entries_are_reclaimed() {
    let limiter = RateLimiter::new(TestClock::default());
    let key = route(Method::GET, "/users/@me");
    let mut oversized = Box::pin(limiter.acquire(key, Priority::UserRead, WORK_BYTE_BUDGET + 1));
    assert!(matches!(
        ready(oversized.as_mut()),
        Err(RestError::SchedulerCapacityExceeded)
    ));
    let mut state = State::default();
    for id in 0..3000 {
        let key = route(Method::GET, &format!("/channels/{id}/messages"));
        state.ensure_route(&key, Duration::ZERO).unwrap();
        assert!(state.tracked_bytes <= TRACKED_BYTE_BUDGET);
    }
    assert!(state.routes.len() < 3000);
}
