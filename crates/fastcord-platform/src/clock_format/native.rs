//! Parse native UTF-16 patterns without retaining locale text or guessing from
//! AM/PM labels. Windows uses h/H and a preferred first alternative; Unicode
//! date patterns additionally use K/k and treat semicolons as literal text.

pub(super) const MAX_UNITS: usize = 256;

pub(super) fn parse(format: &[u16], unicode: bool) -> Option<bool> {
    if format.is_empty()
        || format.len() > MAX_UNITS
        || format.contains(&0)
        || char::decode_utf16(format.iter().copied()).any(|value| value.is_err())
    {
        return None;
    }
    let mut quoted = false;
    let mut result = None;
    let mut units = format.iter().copied().peekable();
    while let Some(unit) = units.next() {
        if unit == u16::from(b'\'') {
            if units.peek() == Some(&u16::from(b'\'')) {
                units.next(); // A doubled apostrophe is literal, inside or outside quotes.
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted {
            continue;
        }
        if !unicode && unit == u16::from(b';') {
            break; // LOCALE_SSHORTTIME's first alternative is the preferred format.
        }
        let hour12 = match unit {
            0x68 => true,             // h: 1..12
            0x48 => false,            // H: 0..23
            0x4b if unicode => true,  // K: 0..11
            0x6b if unicode => false, // k: 1..24
            _ => continue,
        };
        if result.is_some_and(|old| old != hour12) {
            return None;
        }
        result = Some(hour12);
    }
    if quoted { None } else { result }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(text: &str, unicode: bool) -> Option<bool> {
        parse(&text.encode_utf16().collect::<Vec<_>>(), unicode)
    }

    #[test]
    fn quoted_literals_and_day_periods_do_not_determine_hour_cycle() {
        for unicode in [false, true] {
            for text in [
                "h:mm",
                "hh:mm tt",
                "h 'H'",
                "h 'o''clock'",
                "h '' 'H'",
                "h 'час 🕐'",
            ] {
                assert_eq!(pattern(text, unicode), Some(true), "{text}");
            }
            for text in ["HH:mm", "H 'h'", "HH:mm 'AM/PM'", "H ''"] {
                assert_eq!(pattern(text, unicode), Some(false), "{text}");
            }
            for text in ["", "tt", "a b B", "'h'", "'H'", "h H", "H 'literal", "'h''"] {
                assert_eq!(pattern(text, unicode), None, "{text}");
            }
        }
    }

    #[test]
    fn preferred_windows_alternative_and_unicode_hour_symbols_are_distinct() {
        for (text, expected) in [
            ("h:mm;HH:mm", Some(true)),
            ("HH:mm;h:mm", Some(false)),
            ("'H;';h:mm", None),
            ("h ';H';HH", Some(true)),
            (";H", None),
            ("K:mm", None),
            ("k:mm", None),
        ] {
            assert_eq!(pattern(text, false), expected, "{text}");
        }
        for (text, expected) in [
            ("K:mm", Some(true)),
            ("k:mm", Some(false)),
            ("h K", Some(true)),
            ("H k", Some(false)),
            ("K H", None),
            ("h:mm;H:mm", None),
            ("K 'k'", Some(true)),
        ] {
            assert_eq!(pattern(text, true), expected, "{text}");
        }
    }

    #[test]
    fn malformed_and_oversized_utf16_fail_without_truncation() {
        for unicode in [false, true] {
            assert_eq!(parse(&[0x68, 0, 0x48], unicode), None);
            assert_eq!(parse(&[0x68, 0xd800], unicode), None);
            assert_eq!(parse(&[0x68, 0xdc00], unicode), None);
            assert_eq!(parse(&vec![0x68; MAX_UNITS], unicode), Some(true));
            assert_eq!(parse(&vec![0x68; MAX_UNITS + 1], unicode), None);
        }
    }
}
