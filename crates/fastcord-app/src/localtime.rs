//! Local time for message headers and `<t:…>` markup. The offset is read once
//! at process start (see `init`); there is no time-zone database, so a message
//! sent across a DST change shows today's offset, not the offset of its day.

use std::sync::OnceLock;

use fastcord_model::markup::TimeStyle;
use time::{OffsetDateTime, UtcOffset};

static OFFSET: OnceLock<Option<UtcOffset>> = OnceLock::new();

/// Call from `main` before any thread is spawned: `time` refuses to read the
/// local offset once the process is multi-threaded on Unix.
pub fn init() {
    let _ = OFFSET.set(UtcOffset::current_local_offset().ok());
}

/// `None` when the local offset could not be determined (shown as UTC).
pub fn offset() -> Option<UtcOffset> {
    OFFSET.get().copied().flatten()
}

pub fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const DAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];

/// `<t:unix:style>` as the official client shows it. `None` if `unix` is out
/// of the representable range.
pub fn markup(unix: i64, style: TimeStyle, offset: Option<UtcOffset>, now: i64) -> Option<String> {
    if style == TimeStyle::Relative {
        return relative(unix, now);
    }
    let at = OffsetDateTime::from_unix_timestamp(unix)
        .ok()?
        .to_offset(offset.unwrap_or(UtcOffset::UTC));
    let (y, m, d) = (at.year(), at.month() as usize, at.day());
    let (h, min, s) = (at.hour(), at.minute(), at.second());
    let long_date = format!("{d} {} {y}", MONTHS[m - 1]);
    Some(match style {
        TimeStyle::ShortTime => format!("{h:02}:{min:02}"),
        TimeStyle::LongTime => format!("{h:02}:{min:02}:{s:02}"),
        TimeStyle::ShortDate => format!("{d:02}/{m:02}/{y}"),
        TimeStyle::LongDate => long_date,
        TimeStyle::ShortDateTime => format!("{long_date} {h:02}:{min:02}"),
        TimeStyle::LongDateTime => format!(
            "{}, {long_date} {h:02}:{min:02}",
            DAYS[at.weekday().number_days_from_monday() as usize]
        ),
        TimeStyle::Relative => unreachable!(),
    })
}

fn relative(unix: i64, now: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(unix).ok()?;
    let diff = unix.saturating_sub(now);
    let secs = diff.unsigned_abs();
    // Rounded like the official client: 6.7 days reads "7 days".
    let (minutes, hours, days) = (
        (secs + 30) / 60,
        (secs + 1_800) / 3_600,
        (secs + 43_200) / 86_400,
    );
    let span = if secs < 45 {
        "a few seconds".to_owned()
    } else if secs < 90 {
        "a minute".to_owned()
    } else if minutes < 45 {
        format!("{minutes} minutes")
    } else if minutes < 90 {
        "an hour".to_owned()
    } else if hours < 22 {
        format!("{hours} hours")
    } else if hours < 36 {
        "a day".to_owned()
    } else if days < 26 {
        format!("{days} days")
    } else if days < 45 {
        "a month".to_owned()
    } else if days < 320 {
        format!("{} months", (days * 100 / 3044).max(2))
    } else if days < 548 {
        "a year".to_owned()
    } else {
        format!("{} years", days * 100 / 36_525)
    };
    Some(if diff >= 0 {
        format!("in {span}")
    } else {
        format!("{span} ago")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 2021-04-20 16:20:30 UTC
    const AT: i64 = 1_618_935_630;

    fn plus3() -> Option<UtcOffset> {
        Some(UtcOffset::from_hms(3, 0, 0).unwrap())
    }

    #[test]
    fn every_style_at_a_fixed_offset() {
        let f = |style, off| markup(AT, style, off, AT).unwrap();
        assert_eq!(f(TimeStyle::ShortTime, None), "16:20");
        assert_eq!(f(TimeStyle::ShortTime, plus3()), "19:20");
        assert_eq!(f(TimeStyle::LongTime, plus3()), "19:20:30");
        assert_eq!(f(TimeStyle::ShortDate, None), "20/04/2021");
        assert_eq!(f(TimeStyle::LongDate, None), "20 April 2021");
        assert_eq!(f(TimeStyle::ShortDateTime, None), "20 April 2021 16:20");
        assert_eq!(
            f(TimeStyle::LongDateTime, None),
            "Tuesday, 20 April 2021 16:20"
        );
        // The offset can move the date.
        let late = Some(UtcOffset::from_hms(9, 0, 0).unwrap());
        assert_eq!(f(TimeStyle::ShortDate, late), "21/04/2021");
        assert_eq!(markup(i64::MAX, TimeStyle::ShortTime, None, 0), None);
    }

    #[test]
    fn relative_text_at_a_fixed_now() {
        let r = |delta: i64| markup(AT + delta, TimeStyle::Relative, None, AT).unwrap();
        assert_eq!(r(10), "in a few seconds");
        assert_eq!(r(-120), "2 minutes ago");
        assert_eq!(r(3 * 3600), "in 3 hours");
        assert_eq!(r(-3 * 86_400), "3 days ago");
        assert_eq!(r(-(6 * 86_400 + 16 * 3_600)), "7 days ago");
        assert_eq!(r(61 * 86_400), "in 2 months");
        assert_eq!(r(-800 * 86_400), "2 years ago");
    }
}
