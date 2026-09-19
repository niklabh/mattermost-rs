//! The slice of `shared/i18n` this port uses.
//!
//! Two things read it:
//!
//! - `users.CreateUser` replaces a submitted `Locale` that the server does not ship translations
//!   for with [`crate::config::Config::default_client_locale`] (app/users/users.go:57) —
//!   [`is_supported_locale`].
//! - The **web error page** (`mm_api::web_error`), whose URL carries the error's *translated*
//!   message under an ECDSA signature, so an untranslated id there is not a cosmetic divergence
//!   but a different signed document. [`Translations`] is the bundle `TranslationsPreInit` loads,
//!   and [`Translations::request_translator`] the translate function `web.Handler` picks from
//!   `Accept-Language` (web/handlers.go:191).
//!
//! JSON error bodies elsewhere still carry the id as their message ([D-092]); nothing here
//! changes that.

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

/// `defaultLocale` (shared/i18n/i18n.go:26) — the **package constant** `tfuncWithFallback` falls
/// back to, not the configured `DefaultServerLocale`.
const FALLBACK_LOCALE: &str = "en";

/// Why the bundle could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("unable to find i18n directory at \"i18n\"")]
    NoDirectory,
    #[error("reading {path}: {source}")]
    Read {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to unmarshal {path}: {source}")]
    Parse {
        path: std::path::PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

/// One entry of a translation file: `{"id": …, "translation": …}`.
#[derive(serde::Deserialize)]
struct Entry {
    id: String,
    translation: serde_json::Value,
}

/// Port of the go-i18n v1 bundle `initTranslationsWithDir` fills (shared/i18n/i18n.go:161), plus
/// the package's `locales` map.
///
/// # What is kept of each file
///
/// Only **plain-string** translations. go-i18n's `translate` with no arguments renders a plural
/// entry (a `{"one": …, "other": …}` object) with no plural form selected, which yields no
/// template and so the id itself — the same thing a missing entry yields, which is how they are
/// stored. An **empty** string is dropped for the same reason: `translate` returns the id when the
/// rendered text is empty (bundle.go:396). A string containing `{{` is a Go template that go-i18n
/// would execute against nil data; it is kept raw, and no error this port renders through here
/// has one (`web_error` asserts that over every supported file).
///
/// # Tags
///
/// go-i18n keys each file by its **normalised** tag (`pt-BR.json` is `pt-br`) and, through
/// `AddTranslation`, also registers it under every shorter prefix as a *fallback* (`pt`), later
/// files overwriting earlier ones. `os.ReadDir` sorts by name, so `zh-TW` is the fallback for
/// `zh`, and `en.json` (after `en-AU.json`) for `en`.
#[derive(Debug, Default)]
pub struct Translations {
    /// The loaded files' contents, indexed by the two maps below.
    files: Vec<std::collections::HashMap<String, String>>,
    /// `b.translations`: normalised tag → file.
    by_tag: std::collections::HashMap<String, usize>,
    /// `b.fallbackTranslations`: every prefix of every tag → the last file that claimed it.
    fallback: std::collections::HashMap<String, usize>,
    /// `locales`: the supported locales, spelled as their file names, that have a file.
    locales: std::collections::HashSet<String>,
}

impl Translations {
    /// Port of `initTranslationsWithDir` (i18n.go:161): every `*.json` in `dir` whose stem is a
    /// supported locale, in directory order. An unreadable directory loads nothing, as
    /// `os.ReadDir`'s ignored error does; an unreadable or malformed *file* is the error Go
    /// returns from `LoadTranslationFile`.
    pub fn load_dir(dir: &std::path::Path) -> Result<Self, I18nError> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let mut out = Translations::default();
        for name in names {
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            // `strings.Split(filename, ".")[0]`.
            let locale = stem.split('.').next().unwrap_or_default();
            if !is_supported_locale(locale) {
                continue;
            }
            let path = dir.join(&name);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(source) => return Err(I18nError::Read { path, source }),
            };
            out.add_file(locale, &bytes)
                .map_err(|source| I18nError::Parse { path, source })?;
        }
        Ok(out)
    }

    /// One file's `LoadTranslationFile` + `AddTranslation`.
    fn add_file(&mut self, locale: &str, bytes: &[u8]) -> Result<(), serde_json::Error> {
        let entries: Vec<Entry> = if bytes.is_empty() {
            Vec::new()
        } else {
            serde_json::from_slice(bytes)?
        };
        let tag = normalize_tag(locale);
        let index = match self.by_tag.get(&tag) {
            Some(&index) => index,
            None => {
                self.files.push(std::collections::HashMap::new());
                self.files.len() - 1
            }
        };
        if let Some(file) = self.files.get_mut(index) {
            for entry in entries {
                if let serde_json::Value::String(text) = entry.translation {
                    if !text.is_empty() {
                        file.insert(entry.id, text);
                    }
                }
            }
        }
        // `lang.MatchingTags()`: "zh-hans-cn" → zh, zh-hans, zh-hans-cn.
        for (at, _) in tag.match_indices('-') {
            self.fallback.insert(tag[..at].to_owned(), index);
        }
        self.fallback.insert(tag.to_owned(), index);
        self.by_tag.insert(tag, index);
        self.locales.insert(locale.to_owned());
        Ok(())
    }

    /// `b.supportedLanguage(pref)` then `b.translation(lang, …)`'s map choice: the first tag
    /// `language.Parse(pref)` yields that has translations, direct or fallback.
    ///
    /// `language.Parse` also drops a tag with no CLDR plural spec. Every tag that has a file here
    /// is a real language with one, so the only tags that check could drop are tags this lookup
    /// would not find anyway, and the plural table is not ported.
    fn file_for(&self, pref: &str) -> Option<&std::collections::HashMap<String, String>> {
        parse_language_tags(pref).into_iter().find_map(|tag| {
            self.by_tag
                .get(&tag)
                .or_else(|| self.fallback.get(&tag))
                .and_then(|&index| self.files.get(index))
        })
    }

    /// Port of `tfuncWithFallback(pref)(id)` (i18n.go:295) for an id with no arguments: `pref`'s
    /// translation, else the `en` one, else the id.
    pub fn translate(&self, pref: &str, id: &str) -> String {
        self.file_for(pref)
            .and_then(|file| file.get(id))
            .or_else(|| self.file_for(FALLBACK_LOCALE).and_then(|file| file.get(id)))
            .map_or_else(|| id.to_owned(), String::to_owned)
    }

    /// Port of the locale choice in `GetTranslationsAndLocaleFromRequest` (i18n.go:264): the
    /// first `Accept-Language` entry **exactly** as written when it is a loaded locale, else its
    /// part before the first `-`, else the default client locale. Case-sensitive and untrimmed —
    /// `pt-br`, `PT-BR` and ` es` all miss — and the weights are ignored.
    ///
    /// Go reads `defaultClientLocale`, which `InitTranslations` sets at startup and when
    /// `PUT /config` is served by that process (api4/config.go:205); this takes the configured
    /// value, the same thing once either has run.
    pub fn request_locale<'a>(
        &self,
        accept_language: &'a str,
        default_client_locale: &'a str,
    ) -> &'a str {
        let full = accept_language.split(',').next().unwrap_or_default();
        let short = full.split('-').next().unwrap_or_default();
        if self.locales.contains(full) {
            full
        } else if self.locales.contains(short) {
            short
        } else {
            default_client_locale
        }
    }

    /// `GetTranslationsAndLocaleFromRequest`'s translate function, applied to one id.
    pub fn translate_for_request(
        &self,
        accept_language: &str,
        default_client_locale: &str,
        id: &str,
    ) -> String {
        self.translate(
            self.request_locale(accept_language, default_client_locale),
            id,
        )
    }
}

/// `language.NormalizeTag`: lower case, `_` → `-`.
fn normalize_tag(tag: &str) -> String {
    tag.to_lowercase().replace('_', "-")
}

/// The tags `language.Parse` (go-i18n language.go:40) reads from `src`: split on `,`, `;` and
/// `.` and trimmed when any of those occurs, the whole string **untrimmed** when none does,
/// normalised, first occurrence kept.
fn parse_language_tags(src: &str) -> Vec<String> {
    let candidates: Vec<String> = if src.contains([',', ';', '.']) {
        src.split([',', ';', '.'])
            .map(|tag| normalize_tag(tag.trim()))
            .collect()
    } else {
        vec![normalize_tag(src)]
    };
    let mut tags: Vec<String> = Vec::with_capacity(candidates.len());
    for tag in candidates {
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    tags
}

static TRANSLATIONS: tokio::sync::OnceCell<Option<Translations>> =
    tokio::sync::OnceCell::const_new();

/// The process's bundle: `utils.TranslationsPreInit` (channels/utils/i18n.go:15) —
/// `FindDirRelBinary("i18n")` — loaded on first use. `None` when there is no such directory or a
/// file in it will not load, which is a server Go refuses to start.
pub async fn translations() -> Option<&'static Translations> {
    TRANSLATIONS
        .get_or_init(|| async {
            let loaded = tokio::task::spawn_blocking(|| {
                let (dir, found) = crate::logs::find_dir_rel_binary("i18n");
                if !found {
                    return Err(I18nError::NoDirectory);
                }
                Translations::load_dir(&dir)
            })
            .await;
            match loaded {
                Ok(Ok(translations)) => Some(translations),
                Ok(Err(err)) => {
                    tracing::error!(error = %err, "could not load the translations");
                    None
                }
                Err(err) => {
                    tracing::error!(error = %err, "loading the translations did not finish");
                    None
                }
            }
        })
        .await
        .as_ref()
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

    fn reference_bundle() -> Option<Translations> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../reference/mattermost/server/i18n");
        dir.is_dir()
            .then(|| Translations::load_dir(&dir).expect("the pinned tree's files load"))
    }

    /// `GetTranslationsAndLocaleFromRequest` + `tfuncWithFallback` against Go's own, over the
    /// pinned tree's files — `fixtures/behaviour_web_error.json`'s `translations`, generated by
    /// `reference/dump/behaviour_web_error.go` with the real `shared/i18n` package.
    #[test]
    fn go_parity_translations_for_a_request() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_web_error.json"))
                .expect("behaviour_web_error.json is generated by reference/dump");
        let rows = fixture["translations"].as_array().expect("an array");
        assert!(rows.len() > 1000, "the corpus is all there: {}", rows.len());
        let mut failures = Vec::new();
        for row in rows {
            let accept = row["accept_language"].as_str().unwrap();
            let default = row["default_client_locale"].as_str().unwrap();
            let id = row["id"].as_str().unwrap();
            let got = bundle.translate_for_request(accept, default, id);
            if got != row["message"].as_str().unwrap() {
                failures.push(format!(
                    "Accept-Language {accept:?}, default {default:?}, {id}: got {got:?}, Go {:?}",
                    row["message"]
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    /// The branches the corpus pins, named — each answer below differs from its neighbours', so a
    /// port that took any one branch wrong reads a different language.
    #[test]
    fn the_request_locale_branches() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        // Exact first entry.
        assert_eq!(bundle.request_locale("pt-BR,en", "en"), "pt-BR");
        // Its language part — but the whole entry first when both are loaded.
        assert_eq!(bundle.request_locale("de-DE,en", "en"), "de");
        assert_eq!(bundle.request_locale("en-AU", "es"), "en-AU");
        // Neither, case-sensitively: the default.
        assert_eq!(bundle.request_locale("PT-BR", "es"), "es");
        assert_eq!(bundle.request_locale("", "es"), "es");
        // Only the first entry is read.
        assert_eq!(bundle.request_locale("xx,de", "es"), "es");
        // A fallback tag: `pt` has no file of its own, and `zh` falls to the file loaded last.
        let parse = "manaultesting.manual_test.parse.app_error";
        assert_eq!(
            bundle.translate("pt", parse),
            bundle.translate("pt-BR", parse)
        );
        assert_eq!(
            bundle.translate("zh", parse),
            bundle.translate("zh-TW", parse)
        );
        assert_ne!(
            bundle.translate("zh-CN", parse),
            bundle.translate("zh-TW", parse)
        );
        // A missing id falls back to `en`, then to itself.
        assert_eq!(
            bundle.translate("es", "basic_security_check.url.too_long_error"),
            "URL is too long"
        );
        assert_eq!(bundle.translate("es", "no.such.id"), "no.such.id");
    }

    #[test]
    fn language_parse_splits_only_when_a_separator_is_present() {
        assert_eq!(parse_language_tags(" es"), vec![" es".to_owned()]);
        assert_eq!(
            parse_language_tags(" es ;q=0.9, PT_br"),
            vec![
                "es".to_owned(),
                "q=0".to_owned(),
                "9".to_owned(),
                "pt-br".to_owned()
            ]
        );
        assert_eq!(parse_language_tags("de,de"), vec!["de".to_owned()]);
        assert!(parse_language_tags("").is_empty());
    }

    /// An empty string and a plural object both read as "no translation": Go's `translate` returns
    /// the id for either.
    #[test]
    fn empty_and_plural_entries_are_not_translations() {
        let mut bundle = Translations::default();
        bundle
            .add_file(
                "en",
                br#"[{"id":"a","translation":""},{"id":"b","translation":{"one":"x","other":"y"}},{"id":"c","translation":"C"}]"#,
            )
            .unwrap();
        assert_eq!(bundle.translate("en", "a"), "a");
        assert_eq!(bundle.translate("en", "b"), "b");
        assert_eq!(bundle.translate("en", "c"), "C");
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
