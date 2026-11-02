//! The language of what open_mess shows people: the configuration window, notifications, dialogs
//! and the tray menu, in Polish or English (spec 2026-10-30-ui-languages). Tool descriptions, the
//! CLI and logs stay English: agents and scripts read them. Every text has both versions side by
//! side where it is used: `pick` for fixed ones, a `match` on [`Lang`] for ones with data.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::store::Store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    Pl,
    En,
}

impl Lang {
    /// `pl` or `en`, as the window names it.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Pl => "pl",
            Lang::En => "en",
        }
    }

    /// The text (or whatever else has a version per language) in this language.
    pub fn pick<T>(self, pl: T, en: T) -> T {
        match self {
            Lang::Pl => pl,
            Lang::En => en,
        }
    }
}

/// The setting in the `config` table: `auto` (Windows's language, also when it is missing), `pl`
/// or `en`.
pub const SETTING: &str = "language";

/// The process's language: English until the daemon settles it.
static CURRENT: AtomicU8 = AtomicU8::new(EN);
const EN: u8 = 0;
const PL: u8 = 1;

/// The language texts are built in now.
pub fn current() -> Lang {
    if CURRENT.load(Ordering::Relaxed) == PL { Lang::Pl } else { Lang::En }
}

/// Sets the process's language; true when it changed.
pub fn set(lang: Lang) -> bool {
    let value = match lang {
        Lang::Pl => PL,
        Lang::En => EN,
    };
    CURRENT.swap(value, Ordering::Relaxed) != value
}

/// The text in the current language.
pub fn pick(pl: &'static str, en: &'static str) -> &'static str {
    current().pick(pl, en)
}

/// The Polish form for `n`: `one` for 1, `few` for numbers ending in 2-4 but not 12-14, `many`
/// for the rest.
pub fn plural_pl<'a>(n: usize, one: &'a str, few: &'a str, many: &'a str) -> &'a str {
    if n == 1 {
        one
    } else if (2..=4).contains(&(n % 10)) && !(12..=14).contains(&(n % 100)) {
        few
    } else {
        many
    }
}

/// The language a BCP 47 tag names, the first of Windows's preferred languages: Polish when its
/// first subtag is `pl` (any letter case), English for anything else or nothing.
pub fn from_tag(first: Option<&str>) -> Lang {
    match first.and_then(|tag| tag.split('-').next()) {
        Some(primary) if primary.eq_ignore_ascii_case("pl") => Lang::Pl,
        _ => Lang::En,
    }
}

/// The stored setting: `pl`, `en`, or `auto` for anything else, a missing one included.
pub fn setting(store: &Store) -> &'static str {
    match store.get_config(SETTING).ok().flatten().as_deref() {
        Some("pl") => "pl",
        Some("en") => "en",
        _ => "auto",
    }
}

/// Settles the process's language from the setting, and for `auto` from Windows's: at the daemon's
/// start, whenever the window opens (Windows's language may have changed) and after the setting
/// changes. Returns it, and whether it changed; what shows the old language then is the caller's.
pub fn apply_language(store: &Store) -> (Lang, bool) {
    let lang = match setting(store) {
        "pl" => Lang::Pl,
        "en" => Lang::En,
        _ => from_tag(windows_language().as_deref()),
    };
    (lang, set(lang))
}

/// The first of the user's preferred languages in Windows; None when Windows says none.
#[cfg(windows)]
fn windows_language() -> Option<String> {
    use windows::System::UserProfile::GlobalizationPreferences;
    GlobalizationPreferences::Languages().and_then(|languages| languages.GetAt(0)).ok().map(|tag| tag.to_string())
}

/// No Windows, no language of its own: English.
#[cfg(not(windows))]
fn windows_language() -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polish_only_for_a_polish_tag() {
        for tag in ["pl", "pl-PL", "PL-pl"] {
            assert_eq!(from_tag(Some(tag)), Lang::Pl, "{tag}");
        }
        for tag in ["en-US", "de-DE", "", "plk", "-pl", "en-PL"] {
            assert_eq!(from_tag(Some(tag)), Lang::En, "{tag}");
        }
        assert_eq!(from_tag(None), Lang::En);
    }

    #[test]
    fn polish_plural_forms() {
        let form = |n| plural_pl(n, "one", "few", "many");
        for (n, expected) in [
            (1, "one"),
            (2, "few"),
            (4, "few"),
            (5, "many"),
            (11, "many"),
            (12, "many"),
            (14, "many"),
            (21, "many"),
            (22, "few"),
            (25, "many"),
            (102, "few"),
            (112, "many"),
            (0, "many"),
        ] {
            assert_eq!(form(n), expected, "{n}");
        }
    }

    #[test]
    fn setting_is_auto_unless_pl_or_en() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.sqlite3")).unwrap();
        assert_eq!(setting(&store), "auto", "missing");
        for (stored, expected) in [("pl", "pl"), ("en", "en"), ("auto", "auto"), ("PL", "auto"), ("de", "auto"), ("", "auto")] {
            store.set_config(SETTING, stored).unwrap();
            assert_eq!(setting(&store), expected, "{stored:?}");
        }
    }

    #[test]
    fn pick_by_language() {
        assert_eq!(Lang::Pl.pick("tak", "yes"), "tak");
        assert_eq!(Lang::En.pick("tak", "yes"), "yes");
        assert_eq!((Lang::Pl.code(), Lang::En.code()), ("pl", "en"));
    }
}
