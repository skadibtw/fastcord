//! Cached native clock preference. Initialize on a blocking worker before
//! applying UI preferences. Reading the cache never calls an OS API.

use std::sync::OnceLock;

#[path = "clock_format/date.rs"]
mod date;
pub use date::DatePattern;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateOrder {
    DayMonthYear,
    MonthDayYear,
    YearMonthDay,
}

static DATE_PATTERN: OnceLock<Option<DatePattern>> = OnceLock::new();
static HOUR12: OnceLock<Option<bool>> = OnceLock::new();

pub fn initialize() {
    HOUR12.get_or_init(detect);
    DATE_PATTERN.get_or_init(detect_date);
}

/// The supported numeric pattern, initialized only by the blocking startup loader.
pub fn date_pattern() -> Option<&'static DatePattern> {
    DATE_PATTERN.get().and_then(Option::as_ref)
}

/// Field order for application-language named dates; no native formatting call.
pub fn date_order() -> Option<DateOrder> {
    date_pattern().and_then(DatePattern::order)
}

fn detect_date() -> Option<DatePattern> {
    #[cfg(target_os = "linux")]
    {
        linux::read_date(c"")
    }
    #[cfg(target_os = "windows")]
    {
        windows::read_date()
    }
    #[cfg(target_os = "macos")]
    {
        macos::read_date()
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        None
    }
}

/// `None` until initialization, or when no supported preference is available.
pub fn hour12() -> Option<bool> {
    HOUR12.get().copied().flatten()
}

#[cfg(target_os = "linux")]
fn detect() -> Option<bool> {
    // Empty locale name asks libc to honor LC_ALL, LC_TIME and LANG.
    linux::read(c"")
}

#[cfg(target_os = "windows")]
fn detect() -> Option<bool> {
    windows::read()
}

#[cfg(target_os = "macos")]
fn detect() -> Option<bool> {
    macos::read()
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn detect() -> Option<bool> {
    None
}

#[cfg(target_os = "macos")]
#[path = "clock_format/macos.rs"]
mod macos;
#[cfg(any(target_os = "windows", target_os = "macos", test))]
#[path = "clock_format/native.rs"]
mod native;
#[cfg(any(target_os = "windows", test))]
#[path = "clock_format/windows.rs"]
mod windows;

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::ffi::CStr;

    pub(super) fn read(name: &CStr) -> Option<bool> {
        read_item(name, libc::T_FMT, super::parse)
    }

    pub(super) fn read_date(name: &CStr) -> Option<super::DatePattern> {
        read_item(name, libc::D_FMT, |bytes| {
            super::DatePattern::from_posix(std::str::from_utf8(bytes).ok()?)
        })
    }

    fn read_item<T>(
        name: &CStr,
        item: libc::nl_item,
        parse: impl FnOnce(&[u8]) -> Option<T>,
    ) -> Option<T> {
        // SAFETY: name is NUL terminated; the newly allocated locale is private
        // to this call. No process-global or thread locale is changed.
        let locale =
            unsafe { libc::newlocale(libc::LC_TIME_MASK, name.as_ptr(), std::ptr::null_mut()) };
        if locale.is_null() {
            return None;
        }
        // SAFETY: locale is live and item is a valid T_FMT or D_FMT langinfo item. libc owns
        // the returned NUL-terminated string until freelocale below.
        let format = unsafe { libc::nl_langinfo_l(item, locale) };
        let result = if format.is_null() {
            None
        } else {
            // SAFETY: see above; parsing borrows only while locale is alive.
            parse(unsafe { CStr::from_ptr(format) }.to_bytes())
        };
        // SAFETY: this call exclusively owns the successful newlocale result.
        unsafe { libc::freelocale(locale) };
        result
    }
}

#[cfg(any(target_os = "linux", test))]
fn parse(format: &[u8]) -> Option<bool> {
    let mut result = None;
    let mut bytes = format.iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'%' {
            continue;
        }
        let mut directive = bytes.next()?;
        if directive == b'%' {
            continue;
        }
        while matches!(
            directive,
            b'E' | b'O' | b'_' | b'-' | b'0'..=b'9' | b'^' | b'#'
        ) {
            directive = bytes.next()?;
        }
        let hour12 = match directive {
            b'I' | b'l' | b'r' => true,
            b'H' | b'k' | b'R' | b'T' => false,
            _ => continue,
        };
        if result.is_some_and(|previous| previous != hour12) {
            return None;
        }
        result = Some(hour12);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_hour_directives_without_country_or_am_pm_guesses() {
        for format in [b"%I:%M:%S %p".as_slice(), b"%r", b"%-l:%M", b"%OI:%M"] {
            assert_eq!(parse(format), Some(true));
        }
        for format in [b"%H:%M:%S".as_slice(), b"%T", b"%R", b"%_k:%M", b"%%I %H"] {
            assert_eq!(parse(format), Some(false));
        }
        for format in [b"%p".as_slice(), b"%%I", b"%", b"%H %I"] {
            assert_eq!(parse(format), None);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn native_c_locale_is_24_hour_and_unknown_locale_is_unavailable() {
        assert_eq!(linux::read(c"C"), Some(false));
        assert_eq!(
            linux::read_date(c"C")
                .unwrap()
                .format(2001, 2, 3)
                .as_deref(),
            Some("02/03/01")
        );
        assert_eq!(linux::read(c"fastcord-nonexistent-locale"), None);
    }
}
