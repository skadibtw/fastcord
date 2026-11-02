//! Numeric Gregorian patterns compiled on the startup worker; no native handles.
use super::DateOrder;
use std::fmt::Write;

const MAX_INPUT: usize = 256;
const LITERAL_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Day(Pad),
    Month(Pad),
    Year(u8), // 2: last two digits; otherwise minimum full-year width.
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pad {
    None,
    Zero,
    Space,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Literal {
    bytes: [u8; LITERAL_BYTES],
    len: u8,
}
impl Literal {
    fn push(&mut self, ch: char) -> Option<()> {
        if ch.is_control()
            || matches!(ch, '\u{200e}'..='\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{206f}' | '\u{061c}')
        {
            return None;
        }
        let start = usize::from(self.len);
        let end = start.checked_add(ch.len_utf8())?;
        ch.encode_utf8(self.bytes.get_mut(start..end)?);
        self.len = u8::try_from(end).ok()?;
        Some(())
    }
    fn text(&self) -> &str {
        // Only complete UTF-8 characters enter through push.
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or_default()
    }
}

/// Exactly three numeric fields and four literal chunks of at most 16 bytes.
/// Native calendar/numbering-system checks precede caching this pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatePattern {
    fields: [Field; 3],
    literals: [Literal; 4],
}
struct Builder {
    pattern: DatePattern,
    fields: usize,
}
impl Builder {
    fn new() -> Self {
        Self {
            pattern: DatePattern {
                fields: [Field::Year(4); 3],
                literals: [Literal::default(); 4],
            },
            fields: 0,
        }
    }
    fn field(&mut self, field: Field) -> Option<()> {
        *self.pattern.fields.get_mut(self.fields)? = field;
        self.fields += 1;
        Some(())
    }
    fn literal(&mut self, ch: char) -> Option<()> {
        self.pattern.literals.get_mut(self.fields)?.push(ch)
    }
    fn finish(self) -> Option<DatePattern> {
        if self.fields != 3 || self.pattern.order().is_none() {
            return None;
        }
        Some(self.pattern)
    }
}
impl DatePattern {
    /// Compile supported POSIX numeric fields without consulting the OS.
    /// Era/alternate digits, named months and recursive locale forms are refused.
    pub fn from_posix(format: &str) -> Option<Self> {
        if format.is_empty() || format.len() > MAX_INPUT {
            return None;
        }
        let mut out = Builder::new();
        let mut chars = format.chars();
        while let Some(ch) = chars.next() {
            if ch != '%' {
                out.literal(ch)?;
                continue;
            }
            match chars.next()? {
                '%' => out.literal('%')?,
                'd' => out.field(Field::Day(Pad::Zero))?,
                'e' => out.field(Field::Day(Pad::Space))?,
                'm' => out.field(Field::Month(Pad::Zero))?,
                'y' => out.field(Field::Year(2))?,
                'Y' => out.field(Field::Year(1))?,
                'D' => {
                    out.field(Field::Month(Pad::Zero))?;
                    out.literal('/')?;
                    out.field(Field::Day(Pad::Zero))?;
                    out.literal('/')?;
                    out.field(Field::Year(2))?;
                }
                'F' => {
                    out.field(Field::Year(4))?;
                    out.literal('-')?;
                    out.field(Field::Month(Pad::Zero))?;
                    out.literal('-')?;
                    out.field(Field::Day(Pad::Zero))?;
                }
                // One GNU padding flag on day/month, never arbitrary widths.
                flag @ ('-' | '_' | '0') => {
                    let pad = match flag {
                        '-' => Pad::None,
                        '_' => Pad::Space,
                        _ => Pad::Zero,
                    };
                    out.field(match chars.next()? {
                        'd' | 'e' => Field::Day(pad),
                        'm' => Field::Month(pad),
                        _ => return None,
                    })?;
                }
                _ => return None,
            }
        }
        out.finish()
    }

    #[cfg(any(target_os = "windows", target_os = "macos", test))]
    pub(super) fn from_native(format: &[u16], unicode: bool) -> Option<Self> {
        if format.is_empty() || format.len() > MAX_INPUT || format.contains(&0) {
            return None;
        }
        let text = String::from_utf16(format).ok()?;
        let mut chars = text.chars().peekable();
        let mut quoted = false;
        let mut out = Builder::new();
        while let Some(ch) = chars.next() {
            if ch == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.literal('\'')?;
                } else {
                    quoted = !quoted;
                }
            } else if quoted {
                out.literal(ch)?;
            } else if matches!(ch, 'd' | 'M' | 'y') {
                let mut width = 1usize;
                while chars.peek() == Some(&ch) {
                    chars.next();
                    width += 1;
                }
                out.field(match (ch, width) {
                    ('d', 1) => Field::Day(Pad::None),
                    ('d', 2) => Field::Day(Pad::Zero),
                    ('M', 1) => Field::Month(Pad::None),
                    ('M', 2) => Field::Month(Pad::Zero),
                    ('y', 2 | 4) => Field::Year(width as u8),
                    ('y', 1 | 3) if unicode => Field::Year(width as u8),
                    _ => return None,
                })?;
            } else if ch.is_ascii_alphabetic() || ch == ';' {
                // No era/week-year, unknown field or alternative can become a literal.
                return None;
            } else {
                out.literal(ch)?;
            }
        }
        if quoted { None } else { out.finish() }
    }

    pub fn order(&self) -> Option<DateOrder> {
        match self.fields {
            [Field::Day(_), Field::Month(_), Field::Year(_)] => Some(DateOrder::DayMonthYear),
            [Field::Month(_), Field::Day(_), Field::Year(_)] => Some(DateOrder::MonthDayYear),
            [Field::Year(_), Field::Month(_), Field::Day(_)] => Some(DateOrder::YearMonthDay),
            _ => None,
        }
    }
    /// Format converted Gregorian civil fields. Nonpositive/extended years keep
    /// the app's existing signed/full-year fallback rather than losing an era.
    pub fn format(&self, year: i32, month: u8, day: u8) -> Option<String> {
        if !(1..=9999).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let mut out = String::with_capacity(4 * LITERAL_BYTES + 8);
        for (at, field) in self.fields.iter().enumerate() {
            out.push_str(self.literals[at].text());
            match *field {
                Field::Year(2) => write!(out, "{:02}", year % 100).ok()?,
                Field::Year(width) => {
                    write!(out, "{year:0width$}", width = usize::from(width)).ok()?
                }
                Field::Day(pad) | Field::Month(pad) => {
                    let n = if matches!(field, Field::Day(_)) {
                        day
                    } else {
                        month
                    };
                    match pad {
                        Pad::None => write!(out, "{n}").ok()?,
                        Pad::Zero => write!(out, "{n:02}").ok()?,
                        Pad::Space => write!(out, "{n:2}").ok()?,
                    }
                }
            }
        }
        out.push_str(self.literals[3].text());
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn posix_numeric_fields_padding_and_literals_are_preserved() {
        for (pattern, expected) in [
            ("%d.%m.%Y", "03.02.2001"),
            ("%D", "02/03/01"),
            ("%F", "2001-02-03"),
            ("%e / %-m / %y", " 3 / 2 / 01"),
            ("%Y年%m月%d日", "2001年02月03日"),
            ("%% %_d %0m %Y", "%  3 02 2001"),
        ] {
            let parsed = DatePattern::from_posix(pattern).unwrap();
            assert_eq!(
                parsed.format(2001, 2, 3).as_deref(),
                Some(expected),
                "{pattern}"
            );
        }
    }
    #[test]
    fn native_dialects_preserve_quotes_widths_and_separators() {
        for (pattern, expected) in [
            ("M/d/yy", "2/3/01"),
            ("dd.MM.yyyy", "03.02.2001"),
            ("yyyy'年'M'月'd'日'", "2001年2月3日"),
            ("'date: 'd'/'M'/'yyyy", "date: 3/2/2001"),
            ("d''M''yyyy", "3'2'2001"),
        ] {
            for unicode in [false, true] {
                let units: Vec<_> = pattern.encode_utf16().collect();
                assert_eq!(
                    DatePattern::from_native(&units, unicode)
                        .unwrap()
                        .format(2001, 2, 3)
                        .as_deref(),
                    Some(expected)
                );
            }
        }
        let units: Vec<_> = "y-M-d".encode_utf16().collect();
        assert_eq!(DatePattern::from_native(&units, false), None);
        assert_eq!(
            DatePattern::from_native(&units, true)
                .unwrap()
                .format(2001, 2, 3)
                .as_deref(),
            Some("2001-2-3")
        );
        assert_eq!(
            DatePattern::from_native(&"yyy-M-d".encode_utf16().collect::<Vec<_>>(), true)
                .unwrap()
                .format(7, 2, 3)
                .as_deref(),
            Some("007-2-3")
        );
    }
    #[test]
    fn unsupported_patterns_and_bounds_fail_whole_without_truncation() {
        for pattern in [
            "%x",
            "%Od/%Om/%EY",
            "%e %b %Y",
            "%Y %m",
            "%Y %m %d %Y",
            "%m %Y %d",
            "%999d/%m/%Y",
            "%",
            "%Y\n%m %d",
            "%Y\0%m %d",
        ] {
            assert_eq!(DatePattern::from_posix(pattern), None, "{pattern}");
        }
        for pattern in [
            "dd MMM yyyy",
            "ddd M yyyy",
            "dd/MM/YYYY",
            "gg y/M/d",
            "d/M/y",
            "d/M/yyy",
            "dd/MM/yyyy;M/d/yy",
            "'oops d/M/yyyy",
            "d/M/yyyy\n",
            "d/M/yyyy\0",
            "d/M/yyyy\u{202e}",
        ] {
            let units: Vec<_> = pattern.encode_utf16().collect();
            assert_eq!(DatePattern::from_native(&units, false), None, "{pattern}");
        }
        for unicode in [false, true] {
            assert_eq!(DatePattern::from_native(&[0xd800], unicode), None);
            assert_eq!(DatePattern::from_native(&[0x79; 256], unicode), None);
            assert_eq!(DatePattern::from_native(&[0x79; 257], unicode), None);
        }
        for control in [
            '\u{061c}', '\u{200e}', '\u{200f}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}',
            '\u{206f}',
        ] {
            assert!(DatePattern::from_posix(&format!("%d/%m/%Y{control}")).is_none());
            assert!(
                DatePattern::from_native(
                    &format!("d/M/yyyy{control}")
                        .encode_utf16()
                        .collect::<Vec<_>>(),
                    true
                )
                .is_none()
            );
        }
        assert_eq!(DatePattern::from_posix(&"x".repeat(257)), None);
        let literal = "x".repeat(LITERAL_BYTES);
        assert!(DatePattern::from_posix(&format!("{literal}%d/%m/%Y")).is_some());
        assert!(DatePattern::from_posix(&format!("{literal}x%d/%m/%Y")).is_none());
        assert!(DatePattern::from_posix("日日日日日%d/%m/%Y").is_some());
        assert!(DatePattern::from_posix("日日日日日日%d/%m/%Y").is_none());
        let pattern = DatePattern::from_posix("%D").unwrap();
        for year in [-1, 0, 10000] {
            assert_eq!(pattern.format(year, 2, 3), None);
        }
        assert!(std::mem::size_of::<DatePattern>() <= 80);
    }
}
