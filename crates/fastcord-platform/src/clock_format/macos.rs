//! Current-locale short-time formatter honors the user's clock preferences.
#![allow(unsafe_code)]

use super::native::{MAX_UNITS, parse};
use objc2_core_foundation::{CFDateFormatter, CFDateFormatterStyle, CFLocale, CFRange, CFString};

pub(super) fn read() -> Option<bool> {
    let locale = CFLocale::current()?;
    from_locale(&locale)
}

fn from_locale(locale: &CFLocale) -> Option<bool> {
    // SAFETY: NULL allocator requests the default allocator; locale is retained
    // for the call. The returned formatter and its format have owned CF retains.
    let formatter = unsafe {
        CFDateFormatter::new(
            None,
            Some(locale),
            CFDateFormatterStyle::NoStyle,
            CFDateFormatterStyle::ShortStyle,
        )
    }?;
    let format = formatter.format()?;
    from_format(&format)
}

fn from_format(format: &CFString) -> Option<bool> {
    with_units(format, |units| parse(units, true))
}

fn with_units<T>(format: &CFString, parse: impl FnOnce(&[u16]) -> Option<T>) -> Option<T> {
    let count = usize::try_from(format.length()).ok()?;
    if count == 0 || count > MAX_UNITS {
        return None;
    }
    let mut buffer = [0u16; MAX_UNITS];
    // SAFETY: the retained immutable CFString has exactly count UTF-16 units;
    // the checked range fits both the string and our fixed output buffer.
    unsafe {
        format.characters(
            CFRange {
                location: 0,
                length: count as _,
            },
            buffer.as_mut_ptr(),
        )
    };
    parse(&buffer[..count])
}

pub(super) fn read_date() -> Option<super::DatePattern> {
    let locale = CFLocale::current()?;
    date_from_locale(&locale)
}

fn date_from_locale(locale: &CFLocale) -> Option<super::DatePattern> {
    // SAFETY: default allocator, retained locale, owned formatter. Neither
    // formatter configuration nor UTC sample checks alter any system setting.
    let formatter = unsafe {
        CFDateFormatter::new(
            None,
            Some(locale),
            CFDateFormatterStyle::ShortStyle,
            CFDateFormatterStyle::NoStyle,
        )
    }?;
    date_from_formatter(&formatter)
}

fn date_from_formatter(formatter: &CFDateFormatter) -> Option<super::DatePattern> {
    use objc2_core_foundation::{
        CFTimeZone, kCFDateFormatterCalendarName, kCFDateFormatterTimeZone,
    };
    // SAFETY: the property key is the documented calendar-name key; downcast
    // checks the returned CF type before reading its bounded text.
    let calendar = unsafe { formatter.property(Some(kCFDateFormatterCalendarName?)) }?;
    let gregorian = with_units(calendar.downcast_ref::<CFString>()?, |units| {
        Some(units.iter().copied().eq("gregorian".encode_utf16()))
    })?;
    if !gregorian {
        return None;
    }
    let format = formatter.format()?;
    let pattern = with_units(&format, |units| {
        super::DatePattern::from_native(units, true)
    })?;
    let utc = CFTimeZone::with_time_interval_from_gmt(None, 0.0)?;
    // SAFETY: a CFTimeZone is the required value type, and both retained objects
    // remain alive. This changes only our private short-lived formatter.
    unsafe { formatter.set_property(Some(kCFDateFormatterTimeZone?), Some(&utc)) };
    // The two dates together exercise all decimal digits (even with yy).
    // Reject alternate numbering systems or native syntax our compiler cannot
    // reproduce; no native formatter is retained for UI use.
    for (at, year, month, day) in [
        (-426_902_400.0, 1987, 6, 23),
        (1_696_809_600.0, 2054, 10, 9),
    ] {
        // SAFETY: retained formatter, default allocator, finite CF absolute time
        // (seconds since 2001-01-01 UTC); output is owned and length-checked.
        let rendered =
            unsafe { CFDateFormatter::new_string_with_absolute_time(None, Some(formatter), at) }?;
        let expected = pattern.format(year, month, day)?;
        let agrees = with_units(&rendered, |units| {
            Some(units.iter().copied().eq(expected.encode_utf16()))
        })?;
        if !agrees {
            return None;
        }
    }
    Some(pattern)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_formatter_and_fixed_string_extraction_leave_system_preferences_unchanged() {
        // Explicit locale fixtures exercise the native formatter without
        // assuming a region overrides the user's selected hour cycle.
        for name in ["en_US", "en_GB"] {
            let identifier = CFString::from_str(name);
            let locale = CFLocale::new(None, Some(&identifier)).unwrap();
            assert!(from_locale(&locale).is_some());
        }
        let _ = read(); // Current-user smoke: unavailable/malformed is a valid fallback.
        for (text, expected) in [
            ("h:mm 'H'", Some(true)),
            ("HH:mm 'h'", Some(false)),
            ("K:mm", Some(true)),
            ("k:mm", Some(false)),
            ("h H", None),
            ("", None),
        ] {
            assert_eq!(from_format(&CFString::from_str(text)), expected);
        }
        assert_eq!(
            from_format(&CFString::from_str(&"h".repeat(MAX_UNITS + 1))),
            None
        );
    }
    #[test]
    fn native_numeric_dates_and_alternate_calendars_use_private_formatters() {
        for name in ["en_US", "en_GB", "de_DE", "ja_JP"] {
            let identifier = CFString::from_str(name);
            let locale = CFLocale::new(None, Some(&identifier)).unwrap();
            assert!(date_from_locale(&locale).is_some(), "{name}");
        }
        for name in [
            "th_TH@calendar=buddhist",
            "ja_JP@calendar=japanese",
            "en_US@numbers=arab",
        ] {
            let identifier = CFString::from_str(name);
            let locale = CFLocale::new(None, Some(&identifier)).unwrap();
            assert!(date_from_locale(&locale).is_none(), "{name}");
        }
        let _ = read_date();
    }
}
