//! Deterministic backend selection (SPEC §8.5): hardware candidates in the
//! platform's order, then software. An explicit preference never falls back;
//! automatic mode reports every candidate it skipped.

use std::fmt;

use crate::codec::{BackendKind, BackendPreference, CodecError};

/// A backend that may be able to open.
// Windows (Media Foundation) is the first platform backend; VA-API and
// VideoToolbox use the same selection in milestones 30 and 31.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) struct Candidate<S> {
    pub kind: BackendKind,
    pub name: String,
    pub source: S,
}

/// A candidate that failed to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub kind: BackendKind,
    pub name: String,
    pub error: CodecError,
}

impl fmt::Display for Attempt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {}", self.kind, self.name, self.error)
    }
}

/// An opened codec and the candidates that were tried before it.
pub struct Selected<T> {
    pub codec: T,
    /// Earlier candidates that failed, in the order they were tried. Non-empty
    /// means automatic mode fell back.
    pub rejected: Vec<Attempt>,
}

impl<T> fmt::Debug for Selected<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Selected")
            .field("rejected", &self.rejected)
            .finish_non_exhaustive()
    }
}

/// No backend could be opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The request itself is invalid (for example the encoder settings); no
    /// backend was tried.
    Invalid(CodecError),
    /// Discovering backends failed before any could be tried.
    Discovery(CodecError),
    /// No backend of the preferred kind exists on this system.
    NoBackend(BackendPreference),
    /// Every allowed backend failed, in the order tried.
    Failed(Vec<Attempt>),
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(error) => error.fmt(f),
            Self::Discovery(error) => write!(f, "no video codec is available: {error}"),
            Self::NoBackend(BackendPreference::Hardware) => {
                f.write_str("no hardware video codec is available")
            }
            Self::NoBackend(BackendPreference::Software) => {
                f.write_str("no software video codec is available")
            }
            Self::NoBackend(BackendPreference::Auto) => f.write_str("no video codec is available"),
            Self::Failed(attempts) => {
                f.write_str("every video codec failed to open")?;
                for attempt in attempts {
                    write!(f, "; {attempt}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for OpenError {}

/// Opens the first allowed candidate that succeeds: hardware candidates in the
/// given order, then software candidates in the given order.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn select<S, T>(
    preference: BackendPreference,
    candidates: Vec<Candidate<S>>,
    mut open: impl FnMut(S) -> Result<T, CodecError>,
) -> Result<Selected<T>, OpenError> {
    let (hardware, software): (Vec<_>, Vec<_>) = candidates
        .into_iter()
        .filter(|candidate| preference.allows(candidate.kind))
        .partition(|candidate| candidate.kind == BackendKind::Hardware);
    if hardware.is_empty() && software.is_empty() {
        return Err(OpenError::NoBackend(preference));
    }
    let mut rejected = Vec::new();
    for candidate in hardware.into_iter().chain(software) {
        match open(candidate.source) {
            Ok(codec) => return Ok(Selected { codec, rejected }),
            Err(error) => rejected.push(Attempt {
                kind: candidate.kind,
                name: candidate.name,
                error,
            }),
        }
    }
    Err(OpenError::Failed(rejected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use BackendKind::{Hardware, Software};

    fn candidates(list: &[(BackendKind, &'static str)]) -> Vec<Candidate<&'static str>> {
        list.iter()
            .map(|&(kind, name)| Candidate {
                kind,
                name: name.to_owned(),
                source: name,
            })
            .collect()
    }

    const BROKEN: CodecError = CodecError::Unsupported("broken");

    fn opener(
        working: &'static [&'static str],
    ) -> impl FnMut(&'static str) -> Result<&'static str, CodecError> {
        move |name| {
            if working.contains(&name) {
                Ok(name)
            } else {
                Err(BROKEN)
            }
        }
    }

    #[test]
    fn auto_prefers_hardware_regardless_of_enumeration_order() {
        let list = candidates(&[(Software, "sw"), (Hardware, "gpu0"), (Hardware, "gpu1")]);
        let selected = select(
            BackendPreference::Auto,
            list,
            opener(&["sw", "gpu0", "gpu1"]),
        )
        .unwrap();
        assert_eq!(selected.codec, "gpu0");
        assert!(selected.rejected.is_empty());
    }

    #[test]
    fn auto_falls_back_through_hardware_to_software_and_reports_each_failure() {
        let list = candidates(&[(Hardware, "gpu0"), (Hardware, "gpu1"), (Software, "sw")]);
        let tried = std::cell::RefCell::new(Vec::new());
        let selected = select(BackendPreference::Auto, list, |name| {
            tried.borrow_mut().push(name);
            opener(&["sw"])(name)
        })
        .unwrap();
        assert_eq!(selected.codec, "sw");
        assert_eq!(*tried.borrow(), ["gpu0", "gpu1", "sw"]);
        let names: Vec<_> = selected.rejected.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["gpu0", "gpu1"]);
        assert_eq!(
            selected.rejected[0].to_string(),
            "hardware gpu0: unsupported: broken"
        );
    }

    #[test]
    fn explicit_hardware_never_falls_back_to_software() {
        let list = candidates(&[(Hardware, "gpu0"), (Software, "sw")]);
        let error = select(BackendPreference::Hardware, list, opener(&["sw"])).unwrap_err();
        assert_eq!(
            error,
            OpenError::Failed(vec![Attempt {
                kind: Hardware,
                name: "gpu0".to_owned(),
                error: BROKEN,
            }])
        );
        let only_software = candidates(&[(Software, "sw")]);
        assert_eq!(
            select(BackendPreference::Hardware, only_software, opener(&["sw"])).unwrap_err(),
            OpenError::NoBackend(BackendPreference::Hardware)
        );
    }

    #[test]
    fn explicit_software_skips_hardware_entirely() {
        let list = candidates(&[(Hardware, "gpu0"), (Software, "sw")]);
        let mut tried = Vec::new();
        let selected = select(BackendPreference::Software, list, |name| {
            tried.push(name);
            Ok::<_, CodecError>(name)
        })
        .unwrap();
        assert_eq!(selected.codec, "sw");
        assert_eq!(tried, ["sw"]);
    }

    #[test]
    fn no_candidates_is_distinct_from_all_failing() {
        assert_eq!(
            select(BackendPreference::Auto, candidates(&[]), opener(&[])).unwrap_err(),
            OpenError::NoBackend(BackendPreference::Auto)
        );
        let error = select(
            BackendPreference::Auto,
            candidates(&[(Software, "sw")]),
            opener(&[]),
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "every video codec failed to open; software sw: unsupported: broken"
        );
    }
}
