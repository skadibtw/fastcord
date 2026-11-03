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
    parse(&buffer[..count], true)
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
}
