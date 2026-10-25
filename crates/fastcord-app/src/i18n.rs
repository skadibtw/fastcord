//! Interface language. English text in the source is the lookup key, as in
//! gettext: `tr("Send")` gives the chosen language's text or the English one
//! when it has none, so an untranslated string still shows something sensible.
//!
//! Adding a language: copy the `i18n/ru` directory, translate the right-hand
//! sides (plural rows hold `|`-separated forms, see [`plural_index`]), add a
//! [`Lang`] variant and wire it in [`Lang::catalog`], [`Lang::ALL`] and
//! [`plural_index`].
//!
//! Placeholders: `{}` takes the next argument, `{0}`, `{1}` a given one, so a
//! translation may reorder them. `{{` and `}}` are literal braces.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

mod ru;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Lang {
    #[default]
    English,
    Russian,
}

impl Lang {
    pub const ALL: [Self; 2] = [Self::English, Self::Russian];

    fn catalog(self) -> Option<&'static HashMap<&'static str, &'static str>> {
        static RU: OnceLock<HashMap<&str, &str>> = OnceLock::new();
        match self {
            Self::English => None,
            Self::Russian => Some(RU.get_or_init(|| ru::entries().collect())),
        }
    }

    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::Russian => "ru",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        let code = code.split(['-', '_']).next().unwrap_or("");
        Self::ALL
            .into_iter()
            .find(|lang| lang.code().eq_ignore_ascii_case(code))
    }

    /// The operating system's interface language, English when unknown.
    /// Tests always get English, whatever machine runs them.
    pub fn system() -> Self {
        if cfg!(test) {
            return Self::English;
        }
        sys_locale::get_locale()
            .as_deref()
            .and_then(Self::from_code)
            .unwrap_or_default()
    }
}

/// Every language names itself in its own language.
impl Display for Lang {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::English => "English",
            Self::Russian => "Русский",
        })
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set(lang: Lang) {
    CURRENT.store(lang as u8, Ordering::Relaxed);
}

pub fn current() -> Lang {
    match CURRENT.load(Ordering::Relaxed) {
        1 => Lang::Russian,
        _ => Lang::English,
    }
}

fn lookup(key: &str) -> Option<&'static str> {
    current().catalog()?.get(key).copied()
}

/// `text` in the current language. Works for runtime text too (an error's
/// `Display`, for one): a string with no entry comes back unchanged.
pub fn tr(text: &str) -> &str {
    lookup(text).unwrap_or(text)
}

/// `template` in the current language with its placeholders filled.
pub fn trf(template: &str, args: &[&dyn Display]) -> String {
    fill(tr(template), args)
}

/// A count with its noun: `one` for 1 in English, `other` otherwise. The
/// catalog row is keyed by `other` and lists every form of the language.
pub fn trn(count: u64, one: &str, other: &str) -> String {
    let template = match lookup(other) {
        Some(forms) => forms
            .split('|')
            .nth(plural_index(current(), count))
            .or_else(|| forms.split('|').last())
            .unwrap_or(other),
        None if count == 1 => one,
        None => other,
    };
    fill(template, &[&count])
}

/// Which `|`-separated form of a plural row a count takes.
fn plural_index(lang: Lang, n: u64) -> usize {
    match lang {
        Lang::English => usize::from(n != 1),
        // одно, два, пять: 1 21 101 / 2-4 22-24 / the rest.
        Lang::Russian => match (n % 10, n % 100) {
            (1, m) if m != 11 => 0,
            (2..=4, m) if !(12..=14).contains(&m) => 1,
            _ => 2,
        },
    }
}

fn fill(template: &str, args: &[&dyn Display]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(template.len() + 16 * args.len());
    let mut next = 0;
    let mut rest = template;
    while let Some(start) = rest.find(['{', '}']) {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push_str(&tail[..1]);
            rest = &tail[2..];
            continue;
        }
        if let Some(end) = tail.find('}').filter(|_| tail.starts_with('{')) {
            let inside = &tail[1..end];
            let index = if inside.is_empty() {
                next += 1;
                Some(next - 1)
            } else {
                inside.parse::<usize>().ok()
            };
            if let Some(arg) = index.and_then(|i| args.get(i)) {
                let _ = write!(out, "{arg}");
                rest = &tail[end + 1..];
                continue;
            }
        }
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(text: &str) -> usize {
        let unescaped = text.replace("{{", "").replace("}}", "");
        unescaped.matches('{').count()
    }

    #[test]
    fn fills_in_order_by_index_and_keeps_literal_braces() {
        assert_eq!(fill("{} of {}", &[&1, &2]), "1 of 2");
        assert_eq!(fill("{1} before {0}", &[&"a", &"b"]), "b before a");
        assert_eq!(fill("{{x}} {}", &[&5]), "{x} 5");
        assert_eq!(fill("{} {}", &[&1]), "1 {}");
    }

    #[test]
    fn russian_plural_forms() {
        let forms = [0, 1, 2, 5, 11, 12, 21, 22, 25, 101, 111, 1004];
        let picked: Vec<_> = forms
            .iter()
            .map(|n| plural_index(Lang::Russian, *n))
            .collect();
        assert_eq!(picked, [2, 0, 1, 2, 2, 2, 0, 1, 2, 0, 2, 1]);
    }

    #[test]
    fn locale_codes() {
        assert_eq!(Lang::from_code("ru-RU"), Some(Lang::Russian));
        assert_eq!(Lang::from_code("en_US"), Some(Lang::English));
        assert_eq!(Lang::from_code("de"), None);
    }

    /// Every row keeps its placeholders, plural rows have three forms, and no
    /// key appears twice with different translations.
    #[test]
    fn russian_catalog_is_consistent() {
        let mut seen: HashMap<&str, &str> = HashMap::new();
        for (key, value) in ru::entries() {
            assert!(!value.is_empty(), "empty translation for {key:?}");
            if let Some(previous) = seen.insert(key, value) {
                assert_eq!(previous, value, "conflicting rows for {key:?}");
            }
            let forms: Vec<_> = value.split('|').collect();
            assert!(
                forms.len() == 1 || forms.len() == 3,
                "{key:?} needs 1 or 3 forms"
            );
            for form in forms {
                assert_eq!(
                    placeholders(form),
                    placeholders(key),
                    "placeholders differ in {key:?} -> {form:?}"
                );
            }
        }
    }
}
