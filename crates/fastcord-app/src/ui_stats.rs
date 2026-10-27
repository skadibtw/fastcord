//! Debug-only redraw statistics (#212): with `FASTCORD_UI_STATS=1`, one line
//! per five seconds (written when a view, update or video draw runs, so a silent window
//! prints nothing) with the `view`, `update` and video-draw counts. Counts only; no user
//! data. Release builds compile it to nothing.

#[cfg(debug_assertions)]
mod imp {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static VIEWS: AtomicU64 = AtomicU64::new(0);
    static UPDATES: AtomicU64 = AtomicU64::new(0);
    static DRAWS: AtomicU64 = AtomicU64::new(0);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);

    fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| std::env::var_os("FASTCORD_UI_STATS").is_some_and(|v| v == "1"))
    }

    pub fn view() {
        if !enabled() {
            return;
        }
        VIEWS.fetch_add(1, Ordering::Relaxed);
        report();
    }

    fn report() {
        let Ok(mut last) = LAST.lock() else { return };
        let now = Instant::now();
        let since = *last.get_or_insert(now);
        if now.duration_since(since) >= Duration::from_secs(5) {
            *last = Some(now);
            eprintln!(
                "ui-stats: {} views, {} updates, {} video draws in 5 s",
                VIEWS.swap(0, Ordering::Relaxed),
                UPDATES.swap(0, Ordering::Relaxed),
                DRAWS.swap(0, Ordering::Relaxed)
            );
        }
    }

    /// One draw of a video widget (any window).
    pub fn video_draw() {
        if enabled() {
            DRAWS.fetch_add(1, Ordering::Relaxed);
            report();
        }
    }

    pub fn update() {
        if enabled() {
            UPDATES.fetch_add(1, Ordering::Relaxed);
            report();
        }
    }
}

#[cfg(debug_assertions)]
pub use imp::{update, video_draw, view};
#[cfg(not(debug_assertions))]
pub fn view() {}
#[cfg(not(debug_assertions))]
pub fn update() {}
#[cfg(not(debug_assertions))]
pub fn video_draw() {}
