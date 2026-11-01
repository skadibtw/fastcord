//! Date-specific OS timezone conversion. Call on workers, never in a view.
//! Production never changes TZ or the process locale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Civil {
    pub year: i32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalResult {
    Unique(i64),
    Ambiguous,
    Missing,
    Unsupported,
}
pub const fn supported() -> bool {
    cfg!(unix)
}
#[cfg(unix)]
pub use native::{offset_at, refresh, resolve};
#[cfg(not(unix))]
pub fn refresh() {}
#[cfg(not(unix))]
pub fn offset_at(_: i64) -> Option<i32> {
    None
}
#[cfg(not(unix))]
pub fn resolve(_: Civil) -> LocalResult {
    LocalResult::Unsupported
}

#[cfg(unix)]
#[allow(unsafe_code)]
mod native {
    use super::{Civil, LocalResult};
    /// Refresh libc's timezone rules once per worker batch; never changes TZ.
    pub fn refresh() {
        unsafe extern "C" {
            fn tzset();
        }
        // SAFETY: tzset takes no pointers and refreshes libc-owned timezone state.
        unsafe { tzset() };
    }
    fn local(seconds: i64) -> Option<libc::tm> {
        #[allow(clippy::useless_conversion)] // time_t can be narrower on Unix targets.
        let seconds: libc::time_t = seconds.try_into().ok()?;
        let mut output = std::mem::MaybeUninit::<libc::tm>::uninit();
        // SAFETY: input and output storage remain valid throughout the call.
        if unsafe { libc::localtime_r(&seconds, output.as_mut_ptr()) }.is_null() {
            return None;
        }
        // SAFETY: successful localtime_r initializes the full tm record.
        Some(unsafe { output.assume_init() })
    }
    pub fn offset_at(seconds: i64) -> Option<i32> {
        local(seconds)?.tm_gmtoff.try_into().ok()
    }
    fn matches(tm: &libc::tm, civil: Civil) -> bool {
        tm.tm_year.checked_add(1900) == Some(civil.year)
            && tm.tm_mon + 1 == i32::from(civil.month)
            && tm.tm_mday == i32::from(civil.day)
            && tm.tm_hour == i32::from(civil.hour)
            && tm.tm_min == i32::from(civil.minute)
            && tm.tm_sec == 0
    }
    pub fn resolve(civil: Civil) -> LocalResult {
        let Some(year) = civil.year.checked_sub(1900) else {
            return LocalResult::Missing;
        };
        let mut found = None;
        for daylight in [-1, 0, 1] {
            // SAFETY: zero is valid for all integer and pointer fields of tm.
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            tm.tm_year = year;
            tm.tm_mon = i32::from(civil.month) - 1;
            tm.tm_mday = i32::from(civil.day);
            tm.tm_hour = i32::from(civil.hour);
            tm.tm_min = i32::from(civil.minute);
            tm.tm_isdst = daylight;
            // SAFETY: mktime may normalize this exclusively owned record.
            let seconds = unsafe { libc::mktime(&mut tm) };
            #[allow(clippy::unnecessary_cast)] // time_t is 32-bit on some Unix targets.
            let seconds = seconds as i64;
            if seconds < 0 || !local(seconds).is_some_and(|value| matches(&value, civil)) {
                continue;
            }
            if found.is_some_and(|previous| previous != seconds) {
                return LocalResult::Ambiguous;
            }
            found = Some(seconds);
        }
        found.map_or(LocalResult::Missing, LocalResult::Unique)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn seasonal_offsets_and_gap_fold_are_checked_in_an_isolated_process() {
        if std::env::var_os("FASTCORD_TIMEZONE_FIXTURE").is_some() {
            refresh(); // actual libc batch refresh, in the isolated TZ child
            let civil = |month, day, hour, minute| Civil {
                year: 2026,
                month,
                day,
                hour,
                minute,
            };
            let LocalResult::Unique(winter) = resolve(civil(1, 15, 12, 0)) else {
                panic!("winter");
            };
            let LocalResult::Unique(summer) = resolve(civil(7, 15, 12, 0)) else {
                panic!("summer");
            };
            assert_eq!(offset_at(winter), Some(-18000));
            assert_eq!(offset_at(summer), Some(-14400));
            assert_eq!(resolve(civil(3, 8, 2, 30)), LocalResult::Missing);
            assert_eq!(resolve(civil(11, 1, 1, 30)), LocalResult::Ambiguous);
            assert_eq!(resolve(civil(2, 30, 12, 0)), LocalResult::Missing);
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "timezone::tests::seasonal_offsets_and_gap_fold_are_checked_in_an_isolated_process",
                "--nocapture",
            ])
            .env("FASTCORD_TIMEZONE_FIXTURE", "1")
            .env("TZ", "EST5EDT,M3.2.0/2,M11.1.0/2")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
