//! Cached native clock preference. Initialize on a blocking worker before
//! applying UI preferences. Reading the cache never calls an OS API.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateOrder {
    DayMonthYear,
    MonthDayYear,
    YearMonthDay,
}

static DATE_ORDER: OnceLock<Option<DateOrder>> = OnceLock::new();
static HOUR12: OnceLock<Option<bool>> = OnceLock::new();

pub fn initialize() {
    HOUR12.get_or_init(detect);
    DATE_ORDER.get_or_init(|| {
        #[cfg(target_os = "linux")]
        {
            linux::read_date(c"")
        }
        #[cfg(not(target_os = "linux"))]
        {
            None
        }
    });
}

/// Cached ordering only; separators, calendar and digits are not localized.
pub fn date_order() -> Option<DateOrder> {
    DATE_ORDER.get().copied().flatten()
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

#[cfg(not(target_os = "linux"))]
fn detect() -> Option<bool> {
    None
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod linux {
    use std::ffi::CStr;

    pub(super) fn read(name: &CStr) -> Option<bool> {
        read_item(name, libc::T_FMT, super::parse)
    }

    pub(super) fn read_date(name: &CStr) -> Option<super::DateOrder> {
        read_item(name, libc::D_FMT, super::parse_date)
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

#[cfg(any(target_os = "linux", test))]
fn parse_date(format: &[u8]) -> Option<DateOrder> {
    let mut fields = Vec::with_capacity(3);
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
        let added: &[u8] = match directive {
            b'd' | b'e' => b"d",
            b'm' | b'b' | b'B' | b'h' => b"m",
            b'y' | b'Y' => b"y",
            b'D' => b"mdy",
            b'F' => b"ymd",
            // Composite/era forms not understood here must use the fallback.
            _ => return None,
        };
        fields.extend_from_slice(added);
        if fields.len() > 3 {
            return None;
        }
    }
    match fields.as_slice() {
        b"dmy" => Some(DateOrder::DayMonthYear),
        b"mdy" => Some(DateOrder::MonthDayYear),
        b"ymd" => Some(DateOrder::YearMonthDay),
        _ => None,
    }
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
        assert_eq!(linux::read_date(c"C"), Some(DateOrder::MonthDayYear));
        assert_eq!(linux::read(c"fastcord-nonexistent-locale"), None);
    }
    #[test]
    fn date_order_uses_directives_not_region_guesses() {
        for format in [b"%d.%m.%Y".as_slice(), b"%e %b %Y", b"%Od/%Om/%EY"] {
            assert_eq!(parse_date(format), Some(DateOrder::DayMonthYear));
        }
        for format in [b"%m/%d/%y".as_slice(), b"%D", b"%%d %m %e %Y"] {
            assert_eq!(parse_date(format), Some(DateOrder::MonthDayYear));
        }
        for format in [b"%Y-%m-%d".as_slice(), b"%F", "%Y年%m月%d日".as_bytes()] {
            assert_eq!(parse_date(format), Some(DateOrder::YearMonthDay));
        }
        for format in [
            b"%x".as_slice(),
            b"%Y %m",
            b"%d %m %Y %Y",
            b"%m %Y %d",
            b"%",
        ] {
            assert_eq!(parse_date(format), None);
        }
    }
}
