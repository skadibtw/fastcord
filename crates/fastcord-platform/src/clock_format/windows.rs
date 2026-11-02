//! Read the current user's short-time pattern, including their overrides.

use super::native::{MAX_UNITS, parse};

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub(super) fn read() -> Option<bool> {
    use ::windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_SSHORTTIME};
    use ::windows::core::PCWSTR;
    // LOCALE_NAME_USER_DEFAULT is NULL. Do not pass LOCALE_NOUSEROVERRIDE:
    // regional defaults can disagree with the user's selected hour cycle.
    read_with(|buffer| unsafe {
        // SAFETY: default locale sentinel and an exclusive bounded output slice.
        GetLocaleInfoEx(PCWSTR::null(), LOCALE_SSHORTTIME, Some(buffer))
    })
}

fn read_with(fetch: impl FnOnce(&mut [u16]) -> i32) -> Option<bool> {
    fetch_with(fetch, |units| parse(units, false))
}

fn fetch_with<T>(
    fetch: impl FnOnce(&mut [u16]) -> i32,
    parse: impl FnOnce(&[u16]) -> Option<T>,
) -> Option<T> {
    let mut buffer = [0xffff; MAX_UNITS];
    let count = usize::try_from(fetch(&mut buffer)).ok()?;
    // GetLocaleInfoEx counts the trailing NUL. Never parse partial data after
    // a failed/oversized result, or accept a missing/embedded terminator.
    if count < 2 || count > buffer.len() || buffer[count - 1] != 0 {
        return None;
    }
    parse(&buffer[..count - 1])
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
pub(super) fn read_date() -> Option<super::DatePattern> {
    use ::windows::core::PCWSTR;
    date_from_locale(PCWSTR::null(), 0)
}

#[cfg(target_os = "windows")]
#[allow(unsafe_code)]
fn date_from_locale(locale: ::windows::core::PCWSTR, flags: u32) -> Option<super::DatePattern> {
    use ::windows::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_ICALENDARTYPE, LOCALE_SNATIVEDIGITS, LOCALE_SSHORTDATE,
    };
    let fetch = |item, buffer: &mut [u16]| unsafe {
        // SAFETY: locale is either the user-default sentinel or a test-owned
        // static locale string; every call supplies an exclusive fixed buffer.
        GetLocaleInfoEx(locale, item | flags, Some(buffer))
    };
    let gregorian = fetch_with(
        |buffer| fetch(LOCALE_ICALENDARTYPE, buffer),
        |units| Some(units == [b'1' as u16]),
    )?;
    let latin = fetch_with(
        |buffer| fetch(LOCALE_SNATIVEDIGITS, buffer),
        |units| Some(units.iter().copied().eq("0123456789".encode_utf16())),
    )?;
    if !gregorian || !latin {
        return None;
    }
    fetch_with(
        |buffer| fetch(LOCALE_SSHORTDATE, buffer),
        |units| super::DatePattern::from_native(units, false),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_native_read_rejects_errors_and_missing_terminators() {
        for count in [-1, 0, 1, MAX_UNITS as i32 + 1] {
            assert_eq!(read_with(|_| count), None);
        }
        assert_eq!(
            read_with(|buffer| {
                buffer[0] = 0x68;
                2
            }),
            None
        );
        assert_eq!(
            read_with(|buffer| {
                buffer[..3].copy_from_slice(&[0x68, 0, 0]);
                3
            }),
            None
        );
        assert_eq!(
            read_with(|buffer| {
                buffer[..2].copy_from_slice(&[0x68, 0]);
                2
            }),
            Some(true)
        );
    }

    #[cfg(target_os = "windows")]
    #[allow(unsafe_code)]
    #[test]
    fn native_explicit_locales_use_the_same_bounded_reader_without_mutating_preferences() {
        use ::windows::Win32::Globalization::{
            GetLocaleInfoEx, LOCALE_NOUSEROVERRIDE, LOCALE_SSHORTTIME,
        };
        use ::windows::core::w;
        for (locale, expected) in [(w!("en-US"), true), (w!("en-GB"), false)] {
            assert_eq!(
                read_with(|buffer| unsafe {
                    // SAFETY: static NUL-terminated locale and bounded output.
                    // Test-only overrides flag isolates the fixture from host settings.
                    GetLocaleInfoEx(
                        locale,
                        LOCALE_SSHORTTIME | LOCALE_NOUSEROVERRIDE,
                        Some(buffer),
                    )
                }),
                Some(expected)
            );
        }
        let _ = read(); // Current-user smoke: custom unsupported patterns may fall back.
    }
    #[cfg(target_os = "windows")]
    #[test]
    fn native_numeric_dates_follow_explicit_locales_and_reject_alternate_calendars() {
        use ::windows::Win32::Globalization::LOCALE_NOUSEROVERRIDE;
        use ::windows::core::w;
        for (locale, expected) in [
            (w!("en-US"), "2/3/2001"),
            (w!("de-DE"), "03.02.2001"),
            (w!("ja-JP"), "2001/02/03"),
        ] {
            assert_eq!(
                date_from_locale(locale, LOCALE_NOUSEROVERRIDE)
                    .unwrap()
                    .format(2001, 2, 3)
                    .as_deref(),
                Some(expected)
            );
        }
        assert!(date_from_locale(w!("th-TH"), LOCALE_NOUSEROVERRIDE).is_none());
        assert!(date_from_locale(w!("ar-SA"), LOCALE_NOUSEROVERRIDE).is_none());
        let _ = read_date();
    }
}
