use super::*;
use ::windows::Win32::System::Time::{
    EnumDynamicTimeZoneInformation, SystemTimeToTzSpecificLocalTime,
};

fn civil(year: i32, month: u8, day: u8, hour: u8, minute: u8) -> Civil {
    Civil {
        year,
        month,
        day,
        hour,
        minute,
    }
}

/// Read installed Windows rules without changing the machine's selected zone.
fn zone(key: &str) -> DYNAMIC_TIME_ZONE_INFORMATION {
    for index in 0..512 {
        let mut zone = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // SAFETY: one exclusively owned output record, no retained pointer.
        let status = unsafe { EnumDynamicTimeZoneInformation(index, &mut zone) };
        assert_eq!(status, 0, "fixture zone {key} must be installed");
        let end = zone
            .TimeZoneKeyName
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(128);
        if String::from_utf16_lossy(&zone.TimeZoneKeyName[..end]) == key {
            return zone;
        }
    }
    panic!("fixture zone not found within bounded registry enumeration: {key}")
}

#[test]
fn dynamic_rules_use_the_event_year_including_historical_us_changes() {
    let eastern = zone("Eastern Standard Time");
    for (input, utc, offset) in [
        (civil(2026, 1, 15, 12, 0), 1_768_496_400, -18_000),
        (civil(2026, 7, 15, 12, 0), 1_784_131_200, -14_400),
        // The US started DST in April in 2006, but March from 2007.
        (civil(2006, 3, 20, 12, 0), 1_142_874_000, -18_000),
        (civil(2007, 3, 20, 12, 0), 1_174_406_400, -14_400),
        (civil(2035, 1, 15, 12, 0), 2_052_493_200, -18_000),
        (civil(2035, 7, 15, 12, 0), 2_068_128_000, -14_400),
    ] {
        assert_eq!(resolve_in(&eastern, input), LocalResult::Unique(utc));
        assert_eq!(offset_in(&eastern, utc), Some(offset));
    }
}

#[test]
fn dynamic_native_rules_reject_gaps_folds_and_invalid_calendar_fields() {
    let eastern = zone("Eastern Standard Time");
    assert_eq!(
        resolve_in(&eastern, civil(2026, 3, 8, 2, 30)),
        LocalResult::Missing
    );
    assert_eq!(
        resolve_in(&eastern, civil(2026, 11, 1, 1, 30)),
        LocalResult::Ambiguous
    );
    for input in [
        civil(2026, 2, 30, 12, 0),
        civil(2026, 2, 29, 12, 0),
        civil(2026, 13, 1, 12, 0),
        civil(2026, 1, 1, 24, 0),
        civil(2026, 1, 1, 12, 60),
        civil(i32::MAX, 1, 1, 12, 0),
    ] {
        assert_eq!(resolve_in(&eastern, input), LocalResult::Missing);
    }
    assert!(matches!(
        resolve_in(&eastern, civil(2028, 2, 29, 12, 0)),
        LocalResult::Unique(_)
    ));
    assert_eq!(offset_in(&eastern, i64::MAX), None);
    assert_eq!(offset_in(&eastern, i64::MIN), None);
}

#[test]
fn zone_snapshots_change_rendering_without_changing_the_instant_or_global_zone() {
    let eastern = zone("Eastern Standard Time");
    let india = zone("India Standard Time");
    let utc = zone("UTC");
    let instant = 1_767_211_200; // 2025-12-31 20:00Z -> 2026-01-01 01:30 India
    for (zone, offset) in [
        (&eastern, -18_000),
        (&india, 19_800),
        (&utc, 0),
        (&eastern, -18_000),
    ] {
        assert_eq!(offset_in(zone, instant), Some(offset));
        let local = system_time(local_wall(zone, instant).unwrap()).unwrap();
        assert_eq!(
            resolve_in(
                zone,
                civil(
                    local.wYear.into(),
                    local.wMonth as u8,
                    local.wDay as u8,
                    local.wHour as u8,
                    local.wMinute as u8
                )
            ),
            LocalResult::Unique(instant)
        );
    }
    assert_eq!(
        resolve_in(&eastern, civil(2025, 12, 31, 23, 30)),
        LocalResult::Unique(1_767_241_800)
    );
}

#[test]
fn explicit_native_half_hour_transitions_and_disabled_dst_are_respected() {
    // A fixed synthetic rule: +10:30 standard / +11 daylight, southern summer.
    let rules = TIME_ZONE_INFORMATION {
        Bias: -630,
        DaylightBias: -30,
        StandardDate: SYSTEMTIME {
            wMonth: 4,
            wDay: 1,
            wHour: 2,
            ..Default::default()
        },
        DaylightDate: SYSTEMTIME {
            wMonth: 10,
            wDay: 1,
            wHour: 2,
            ..Default::default()
        },
        ..Default::default()
    };
    let resolve_fixed = |input| {
        let wall = seconds(&civil_time(input).unwrap()).unwrap();
        candidates::resolve(wall, biases(&rules).unwrap(), |utc| {
            let mut local = SYSTEMTIME::default();
            // SAFETY: all fixed fixture/input/output records remain live.
            unsafe {
                SystemTimeToTzSpecificLocalTime(Some(&rules), &system_time(utc)?, &mut local)
            }
            .ok()?;
            seconds(&local)
        })
    };
    assert_eq!(
        resolve_fixed(civil(2026, 4, 5, 1, 45)),
        LocalResult::Ambiguous
    );
    assert_eq!(
        resolve_fixed(civil(2026, 10, 4, 2, 15)),
        LocalResult::Missing
    );

    // Disabling dynamic rules freezes the supplied transitions; it does not
    // itself disable DST. Use explicit modern US transitions even for 2006.
    let mut eastern = zone("Eastern Standard Time");
    eastern.DynamicDaylightTimeDisabled = true;
    eastern.StandardDate = SYSTEMTIME {
        wMonth: 11,
        wDay: 1,
        wHour: 2,
        ..Default::default()
    };
    eastern.DaylightDate = SYSTEMTIME {
        wMonth: 3,
        wDay: 2,
        wHour: 2,
        ..Default::default()
    };
    assert_eq!(offset_in(&eastern, 1_142_874_000), Some(-14_400));
    assert_eq!(
        resolve_in(&eastern, civil(2006, 3, 20, 12, 0)),
        LocalResult::Unique(1_142_870_400)
    );

    // Fully disabling DST also clears both transition dates, as documented.
    eastern.StandardDate = SYSTEMTIME::default();
    eastern.DaylightDate = SYSTEMTIME::default();
    assert_eq!(offset_in(&eastern, 1_782_907_200), Some(-18_000));
    assert_eq!(
        resolve_in(&eastern, civil(2026, 3, 8, 2, 30)),
        LocalResult::Unique(1_772_955_000)
    );
}

#[test]
fn native_epoch_arithmetic_preserves_seconds_and_rejects_out_of_range() {
    for value in [-1, 0, 1, 1_767_211_201, 2_147_483_648] {
        assert_eq!(seconds(&system_time(value).unwrap()), Some(value));
    }
    assert_eq!(system_time(i64::MIN), None);
    assert_eq!(system_time(i64::MAX), None);
    assert_eq!(
        biases(&TIME_ZONE_INFORMATION {
            Bias: -345,
            StandardBias: 900,
            DaylightBias: 500,
            ..Default::default()
        }),
        Some([-345; 2])
    );
}
