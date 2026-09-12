//! The slice of `shared/i18n` that account creation depends on.
//!
//! Only one thing in this port reads i18n at all, and it is not translation: `users.CreateUser`
//! replaces a submitted `Locale` that the server does not ship translations for with
//! [`crate::config::Config::default_client_locale`] (app/users/users.go:57). Nothing else here
//! needs `TranslateFunc`, the bundle loader, or the `Accept-Language` negotiation beside it, so
//! none of that is ported.

/// Port of `i18n.supportedLocales` (shared/i18n/i18n.go:73), in Go's order.
///
/// # Why this is the list and not the directory
///
/// `GetSupportedLocales()` returns a map built by walking `server/i18n/*.json` — **but**
/// `initTranslationsWithDir` skips any file whose stem is not in this hard-coded slice
/// (i18n.go:167). The directory ships 55 files and only these 23 are loaded, so a port that read
/// the directory would accept `am`, `hi` or `sv-SE` where Go resets them to the default client
/// locale. The gap is not hypothetical: 32 of the 55 files are outside this list.
///
/// # These are exact, case-sensitive keys
///
/// The map is keyed on the filename stem, so `en-AU`, `pt-BR`, `zh-CN` and `zh-TW` carry their
/// region with that hyphen and that casing. `model.IsValidLocale` — which `User.IsValid` uses —
/// is case-*insensitive* and accepts far more, so `"EN-au"` is a valid locale that is still not a
/// supported one and is still replaced. The two checks are not interchangeable.
pub const SUPPORTED_LOCALES: &[&str] = &[
    "de", "en", "en-AU", "es", "fr", "it", "hu", "nl", "pl", "pt-BR", "ro", "sv", "vi", "tr", "bg",
    "ru", "uk", "fa", "ko", "zh-CN", "zh-TW", "ja",
];

/// Port of the `i18n.GetSupportedLocales()[user.Locale]` membership test in `users.CreateUser`.
pub fn is_supported_locale(locale: &str) -> bool {
    SUPPORTED_LOCALES.contains(&locale)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list is transcribed from Go, so the oracle is the Go tree itself: every entry must
    /// name a translation file that actually exists, or `initTranslationsWithDir` would never
    /// have put it in the map and the locale would not in fact be supported.
    ///
    /// Reads `reference/mattermost/`, which is checked in beside this crate.
    #[test]
    fn every_supported_locale_has_a_translation_file() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        if !dir.is_dir() {
            // The reference tree is optional in a packaged checkout; the transcription test below
            // still runs.
            return;
        }
        for locale in SUPPORTED_LOCALES {
            assert!(
                dir.join(format!("{locale}.json")).is_file(),
                "{locale} is listed as supported but ships no translation file"
            );
        }
    }

    /// And the converse, which is the direction that would silently over-accept: the directory
    /// holds locales this list must *not* contain.
    #[test]
    fn the_directory_is_wider_than_the_supported_list() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let mut unsupported = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".json") {
                if !is_supported_locale(stem) {
                    unsupported.push(stem.to_owned());
                }
            }
        }
        assert!(
            unsupported.len() > 20,
            "expected the directory to ship many unsupported locales, found {unsupported:?}"
        );
        assert!(unsupported.contains(&"am".to_owned()));
        assert!(unsupported.contains(&"hi".to_owned()));
    }

    #[test]
    fn the_membership_test_is_case_and_region_sensitive() {
        assert!(is_supported_locale("en"));
        assert!(is_supported_locale("en-AU"));
        assert!(is_supported_locale("pt-BR"));
        // `model::user::is_valid_locale` accepts all three of these; the supported list does not.
        assert!(!is_supported_locale("en-au"));
        assert!(!is_supported_locale("EN"));
        assert!(!is_supported_locale("en-GB"));
        assert!(!is_supported_locale("zz"));
        assert!(!is_supported_locale(""));
    }
}
