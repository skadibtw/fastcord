use super::LocalResult;

/// Windows biases are minutes added to a local wall clock to obtain UTC.
/// Validate both possible offsets rather than trusting an inverse conversion
/// that chooses one side of a fold or normalizes a missing civil time.
pub(super) fn resolve(
    wall: i64,
    biases: [i32; 2],
    mut local_wall: impl FnMut(i64) -> Option<i64>,
) -> LocalResult {
    let mut found = None;
    for (index, bias) in biases.into_iter().enumerate() {
        if index == 1 && bias == biases[0] {
            continue;
        }
        let Some(utc) = wall.checked_add(i64::from(bias) * 60) else {
            return LocalResult::Missing;
        };
        // Match the Unix resolver's supported event-instant range.
        if utc < 0 {
            continue;
        }
        let Some(round_trip) = local_wall(utc) else {
            // A failed second lookup must not turn a possible fold into Unique.
            // Unsupported would make the app silently submit this field as UTC.
            return LocalResult::Missing;
        };
        if round_trip == wall {
            if found.is_some_and(|previous| previous != utc) {
                return LocalResult::Ambiguous;
            }
            found = Some(utc);
        }
    }
    found.map_or(LocalResult::Missing, LocalResult::Unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_gap_and_fold_require_exact_round_trips() {
        let wall = 100_000;
        assert_eq!(
            resolve(wall, [300, 240], |utc| Some(utc - 18_000)),
            LocalResult::Unique(118_000)
        );
        assert_eq!(
            resolve(wall, [300, 240], |utc| Some(if utc == 118_000 {
                wall + 3600
            } else {
                wall - 3600
            })),
            LocalResult::Missing
        );
        assert_eq!(
            resolve(wall, [300, 240], |_| Some(wall)),
            LocalResult::Ambiguous
        );
    }

    #[test]
    fn duplicate_fixed_offsets_are_unique_and_half_hour_folds_are_ambiguous() {
        let mut calls = 0;
        assert_eq!(
            resolve(100_000, [-345, -345], |utc| {
                calls += 1;
                Some(utc + 20_700)
            }),
            LocalResult::Unique(79_300)
        );
        assert_eq!(calls, 1);
        // No assumption about hour-sized or positive daylight adjustments.
        assert_eq!(
            resolve(100_000, [-630, -660], |_| Some(100_000)),
            LocalResult::Ambiguous
        );
        assert_eq!(
            resolve(100_000, [0, 60], |_| Some(100_000)),
            LocalResult::Ambiguous
        );
    }

    #[test]
    fn failures_never_become_utc_or_a_partially_proven_unique_result() {
        assert_eq!(
            resolve(100_000, [300, 240], |utc| (utc == 118_000)
                .then_some(100_000)),
            LocalResult::Missing
        );
        assert_eq!(
            resolve(i64::MAX, [1, 1], |_| panic!(
                "overflow must not call native API"
            )),
            LocalResult::Missing
        );
        assert_eq!(
            resolve(-1, [0, 0], |_| panic!("negative instant")),
            LocalResult::Missing
        );
        assert_eq!(resolve(0, [0, 0], Some), LocalResult::Unique(0));
    }
}
