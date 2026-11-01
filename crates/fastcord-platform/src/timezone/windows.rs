//! Dynamic Windows rules, read on workers only. No process timezone mutation.
use super::{Civil, LocalResult, candidates};
use ::windows::Win32::{
    Foundation::{FILETIME, SYSTEMTIME},
    System::Time::{
        DYNAMIC_TIME_ZONE_INFORMATION, FileTimeToSystemTime, GetDynamicTimeZoneInformation,
        GetTimeZoneInformationForYear, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
        SystemTimeToTzSpecificLocalTimeEx, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
    },
};

const UNIX_EPOCH: i64 = 11_644_473_600;
const TICKS_PER_SECOND: u64 = 10_000_000;

/// Unlike libc, these APIs read the current Windows zone on every operation.
/// The caller's bounded worker cache still refreshes at its existing deadline.
pub fn refresh() {}

fn current_zone() -> Option<DYNAMIC_TIME_ZONE_INFORMATION> {
    let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
    // SAFETY: the OS writes one fully allocated, exclusively owned record.
    (unsafe { GetDynamicTimeZoneInformation(&mut zone) } != TIME_ZONE_ID_INVALID).then_some(zone)
}

fn system_time(seconds: i64) -> Option<SYSTEMTIME> {
    let ticks = u64::try_from(seconds.checked_add(UNIX_EPOCH)?)
        .ok()?
        .checked_mul(TICKS_PER_SECOND)?;
    let file = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    let mut value = SYSTEMTIME::default();
    // SAFETY: both pointers refer to initialized storage for the entire call.
    unsafe { FileTimeToSystemTime(&file, &mut value) }.ok()?;
    Some(value)
}

fn seconds(value: &SYSTEMTIME) -> Option<i64> {
    let mut file = FILETIME::default();
    // SAFETY: the input and writable output records remain valid for the call.
    unsafe { SystemTimeToFileTime(value, &mut file) }.ok()?;
    let ticks = (u64::from(file.dwHighDateTime) << 32) | u64::from(file.dwLowDateTime);
    i64::try_from(ticks / TICKS_PER_SECOND)
        .ok()?
        .checked_sub(UNIX_EPOCH)
}

fn civil_time(civil: Civil) -> Option<SYSTEMTIME> {
    let value = SYSTEMTIME {
        wYear: civil.year.try_into().ok()?,
        wMonth: civil.month.into(),
        wDay: civil.day.into(),
        wHour: civil.hour.into(),
        wMinute: civil.minute.into(),
        ..Default::default()
    };
    // Reject invalid calendar fields even if an OS implementation normalizes.
    let back = system_time(seconds(&value)?)?;
    (back.wYear == value.wYear
        && back.wMonth == value.wMonth
        && back.wDay == value.wDay
        && back.wHour == value.wHour
        && back.wMinute == value.wMinute)
        .then_some(value)
}

/// The disabled-dynamic snapshot owns its transition dates. Looking it up by
/// registry key again can restore DST even after both dates were cleared.
fn fixed_rules(zone: &DYNAMIC_TIME_ZONE_INFORMATION) -> TIME_ZONE_INFORMATION {
    TIME_ZONE_INFORMATION {
        Bias: zone.Bias,
        StandardName: zone.StandardName,
        StandardDate: zone.StandardDate,
        StandardBias: zone.StandardBias,
        DaylightName: zone.DaylightName,
        DaylightDate: zone.DaylightDate,
        DaylightBias: zone.DaylightBias,
    }
}

fn local_wall(zone: &DYNAMIC_TIME_ZONE_INFORMATION, utc: i64) -> Option<i64> {
    let utc = system_time(utc)?;
    let mut local = SYSTEMTIME::default();
    if zone.DynamicDaylightTimeDisabled {
        let rules = fixed_rules(zone);
        // SAFETY: initialized, owned records live throughout the call. The
        // fixed API uses these exact transitions without a registry re-read.
        unsafe { SystemTimeToTzSpecificLocalTime(Some(&rules), &utc, &mut local) }.ok()?;
    } else {
        // SAFETY: a single zone snapshot and both time records live through
        // this call. Ex applies historical rules, including year crossings.
        unsafe { SystemTimeToTzSpecificLocalTimeEx(Some(zone), &utc, &mut local) }.ok()?;
    }
    seconds(&local)
}

fn offset_in(zone: &DYNAMIC_TIME_ZONE_INFORMATION, utc: i64) -> Option<i32> {
    local_wall(zone, utc)?.checked_sub(utc)?.try_into().ok()
}

pub fn offset_at(utc: i64) -> Option<i32> {
    offset_in(&current_zone()?, utc)
}

fn biases(rules: &TIME_ZONE_INFORMATION) -> Option<[i32; 2]> {
    if rules.StandardDate.wMonth == 0 && rules.DaylightDate.wMonth == 0 {
        return Some([rules.Bias; 2]);
    }
    Some([
        rules.Bias.checked_add(rules.StandardBias)?,
        rules.Bias.checked_add(rules.DaylightBias)?,
    ])
}

fn resolve_in(zone: &DYNAMIC_TIME_ZONE_INFORMATION, civil: Civil) -> LocalResult {
    let Some(value) = civil_time(civil) else {
        return LocalResult::Missing;
    };
    let rules = if zone.DynamicDaylightTimeDisabled {
        fixed_rules(zone)
    } else {
        let mut rules = TIME_ZONE_INFORMATION::default();
        // SAFETY: all records remain valid for the call. The year is LOCAL,
        // as required by this API. Only dynamic snapshots consult year rules.
        if unsafe { GetTimeZoneInformationForYear(value.wYear, Some(zone), &mut rules) }.is_err() {
            return LocalResult::Missing;
        }
        rules
    };
    let Some(biases) = biases(&rules) else {
        return LocalResult::Missing;
    };
    let Some(wall) = seconds(&value) else {
        return LocalResult::Missing;
    };
    candidates::resolve(wall, biases, |utc| local_wall(zone, utc))
}

pub fn resolve(civil: Civil) -> LocalResult {
    current_zone().map_or(LocalResult::Missing, |zone| resolve_in(&zone, civil))
}

#[cfg(test)]
mod tests;
