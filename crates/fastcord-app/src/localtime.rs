//! Local time for message headers and `<t:…>` markup. The offset is read once
//! at process start (see `init`); there is no time-zone database, so a message
//! sent across a DST change shows today's offset, not the offset of its day.

use crate::i18n::{tr, trf, trn};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use fastcord_model::markup::TimeStyle;
use time::{OffsetDateTime, UtcOffset};

static OFFSET: OnceLock<Option<UtcOffset>> = OnceLock::new();
static HOUR12: AtomicBool = AtomicBool::new(false);

/// The clock of every time of day shown (see `chat_prefs::TimeFormat`).
pub fn set_hour12(on: bool) {
    HOUR12.store(on, Ordering::Relaxed);
}

pub fn hour12() -> bool {
    HOUR12.load(Ordering::Relaxed)
}

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
    // `to_offset` panics when the offset carries the date past year 9999;
    // anyone can post `<t:253402300799>`, so the checked form is required.
    let at = OffsetDateTime::from_unix_timestamp(unix)
        .ok()?
        .checked_to_offset(offset.unwrap_or(UtcOffset::UTC))?;
    let (y, m, d) = (at.year(), at.month() as usize, at.day());
    let (h, min, s) = (at.hour(), at.minute(), at.second());
    let long_date = trf("{} {} {}", &[&d, &tr(MONTHS[m - 1]), &y]);
    let hour12 = hour12();
    let short = crate::chat_prefs::clock(h, min, hour12);
    Some(match style {
        TimeStyle::ShortTime => short,
        TimeStyle::LongTime if hour12 => {
            // "4:20:30 PM": seconds go before the AM/PM suffix.
            let (hm, suffix) = short.split_once(' ').unwrap_or((&short, ""));
            format!("{hm}:{s:02} {suffix}")
        }
        TimeStyle::LongTime => format!("{short}:{s:02}"),
        TimeStyle::ShortDate => format!("{d:02}/{m:02}/{y}"),
        TimeStyle::LongDate => long_date,
        TimeStyle::ShortDateTime => format!("{long_date} {short}"),
        TimeStyle::LongDateTime => format!(
            "{}, {long_date} {short}",
            tr(DAYS[at.weekday().number_days_from_monday() as usize])
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
        tr("a few seconds").to_owned()
    } else if secs < 90 {
        tr("a minute").to_owned()
    } else if minutes < 45 {
        trn(minutes, "{} minute", "{} minutes")
    } else if minutes < 90 {
        tr("an hour").to_owned()
    } else if hours < 22 {
        trn(hours, "{} hour", "{} hours")
    } else if hours < 36 {
        tr("a day").to_owned()
    } else if days < 26 {
        trn(days, "{} day", "{} days")
    } else if days < 45 {
        tr("a month").to_owned()
    } else if days < 320 {
        trn((days * 100 / 3044).max(2), "{} month", "{} months")
    } else if days < 548 {
        tr("a year").to_owned()
    } else {
        trn(days * 100 / 36_525, "{} year", "{} years")
    };
    Some(if diff >= 0 {
        trf("in {}", &[&span])
    } else {
        trf("{} ago", &[&span])
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
        // The last representable second, moved past year 9999 by the offset.
        let east = UtcOffset::from_hms(3, 0, 0).ok();
        let west = UtcOffset::from_hms(-3, 0, 0).ok();
        assert_eq!(
            markup(253_402_300_799, TimeStyle::ShortDateTime, east, 0),
            None
        );
        assert!(markup(253_402_300_799, TimeStyle::ShortDateTime, west, 0).is_some());
        assert_eq!(
            markup(-377_705_116_800, TimeStyle::ShortDateTime, west, 0),
            None
        );
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
