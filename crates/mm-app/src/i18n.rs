//! The slice of `shared/i18n` this port uses.
//!
//! Three things read it:
//!
//! - `users.CreateUser` replaces a submitted `Locale` that the server does not ship translations
//!   for with [`crate::config::Config::default_client_locale`] (app/users/users.go:57) —
//!   [`is_supported_locale`].
//! - **Every error body this server writes.** `web.Handler.handleContextError` (web/handlers.go:431)
//!   runs `c.Err.Translate(c.AppContext.T)` before writing, so `message` is prose in the caller's
//!   language and never the id. [`Translations::translate_app_error`] is that step; `mm_api::error`
//!   applies it with the locale [`Translations::request_locale`] picks from `Accept-Language`
//!   (web/handlers.go:191), and the websocket applies it with [`Translations::server_locale`],
//!   because `model.NewAppError` translates at construction with `i18n.T` and nothing
//!   re-translates a frame.
//! - The **web error page** (`mm_api::web_error`), whose URL carries the translated message under
//!   an ECDSA signature, so a wrong message there is not a cosmetic divergence but a different
//!   signed document.
//!
//! # What is not rendered
//!
//! A translation is a Go `text/template` and go-i18n executes it against the `AppError`'s params
//! ([`Template`]). Two constructs the shipped files contain are **not** rendered here:
//!
//! - `{{if}}`/`{{else}}`/`{{end}}`, which only `app.bot.get_disable_bot_sysadmin_message` uses —
//!   a post body, not an error, and not an id this port raises. [D-940].
//! - A **plural** translation (`{"one": …, "other": …}`), which needs CLDR plural specs per
//!   language. Ten ids have one and none is an id this port raises;
//!   `go_parity_no_reachable_id_needs_a_construct_we_skip` fails the moment that stops being true.
//!
//! Both answer as though the entry were absent, which is the id — never text Go would not have
//! written. [`Translations::can_render`] reports the difference for the signed page.

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

/// The params an `AppError` carries into its translation: Go's `map[string]any`
/// (model/utils.go:240), which go-i18n hands to `text/template` as the template's data.
pub type Params = std::collections::HashMap<String, serde_json::Value>;

/// Why the bundle could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("unable to find i18n directory at \"i18n\"")]
    NoDirectory,
    #[error("loading the translations did not finish: {0}")]
    Join(String),
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

/// One piece of a parsed translation.
#[derive(Debug)]
enum Node {
    Text(String),
    Field(String),
}

/// What a translation's source parsed into.
#[derive(Debug)]
enum Body {
    /// No `{{` anywhere. go-i18n leaves the template unparsed and `Execute` returns the source
    /// verbatim whatever the data is (go-i18n translation/template.go:36) — most entries.
    Literal,
    /// Text and `{{.Field}}` actions in order.
    Actions(Vec<Node>),
    /// A construct this port does not render; see the module docs.
    Unsupported,
}

/// Port of go-i18n's `translation.template` (translation/template.go), which is Go's
/// `text/template` with the translation's own source as both the template's name and its text.
#[derive(Debug)]
struct Template {
    src: String,
    body: Body,
}

impl Template {
    /// `parseTemplate` (template.go:55): a source with no `{{` is never handed to the parser, so
    /// it can never fail and never substitutes anything.
    fn parse(src: String) -> Self {
        if !src.contains("{{") {
            return Template {
                src,
                body: Body::Literal,
            };
        }
        let mut nodes = Vec::new();
        let mut rest = src.as_str();
        let body = loop {
            let Some(open) = rest.find("{{") else {
                if !rest.is_empty() {
                    nodes.push(Node::Text(rest.to_owned()));
                }
                break Body::Actions(nodes);
            };
            if open > 0 {
                nodes.push(Node::Text(rest[..open].to_owned()));
            }
            let after = &rest[open + 2..];
            let Some(close) = after.find("}}") else {
                break Body::Unsupported;
            };
            let Some(field) = field_action(&after[..close]) else {
                break Body::Unsupported;
            };
            nodes.push(Node::Field(field.to_owned()));
            rest = &after[close + 2..];
        };
        Template { src, body }
    }

    /// `template.Execute` (template.go:35) against an `AppError`'s params.
    ///
    /// `None` is [`Body::Unsupported`] — the caller then answers as though the entry were
    /// absent, rather than emitting text Go would not have written.
    fn execute<'a>(&'a self, params: Option<&Params>) -> Option<std::borrow::Cow<'a, str>> {
        match &self.body {
            Body::Literal => Some(std::borrow::Cow::Borrowed(self.src.as_str())),
            Body::Unsupported => None,
            Body::Actions(nodes) => {
                let mut out = String::with_capacity(self.src.len());
                for node in nodes {
                    match node {
                        Node::Text(text) => out.push_str(text),
                        Node::Field(name) => {
                            out.push_str(&render_value(params.and_then(|p| p.get(name))));
                        }
                    }
                }
                Some(std::borrow::Cow::Owned(out))
            }
        }
    }
}

/// The field name of a `{{ .Name }}` action body, or `None` for every other action.
///
/// Go's lexer takes a field name to be a run of letters, digits and underscores with
/// `unicode.IsLetter`/`IsDigit`, not the ASCII ones — `ro.json` really does ship
/// `{{.Invitație}}` — so the test here is `char::is_alphanumeric`. Surrounding spaces are the
/// lexer's to drop; `en.json` has both `{{.Name}}` and `{{ .Domain }}`.
fn field_action(body: &str) -> Option<&str> {
    let name = body.trim().strip_prefix('.')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    if !(first.is_alphabetic() || first == '_') {
        return None;
    }
    chars
        .all(|c| c.is_alphanumeric() || c == '_')
        .then_some(name)
}

/// Go's `fmt.Fprint` of one template argument, which is `%v`.
///
/// A **missing** map key and an explicit `nil` print the same way — the literal `<no value>` —
/// so an `AppError` whose params do not carry the field its sentence names puts that on the
/// wire. Measured rather than assumed: `fixtures/behaviour_i18n.json`'s `template_render`.
fn render_value(value: Option<&serde_json::Value>) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match value {
        None | Some(serde_json::Value::Null) => Cow::Borrowed("<no value>"),
        Some(serde_json::Value::Bool(flag)) => Cow::Borrowed(if *flag { "true" } else { "false" }),
        Some(serde_json::Value::String(text)) => Cow::Borrowed(text.as_str()),
        Some(serde_json::Value::Number(number)) => Cow::Owned(render_number(number)),
        Some(serde_json::Value::Array(items)) => Cow::Owned(format!(
            "[{}]",
            items
                .iter()
                .map(|item| render_value(Some(item)))
                .collect::<Vec<_>>()
                .join(" ")
        )),
        // `fmt` prints a map with its keys sorted, and `serde_json`'s object is a `BTreeMap`
        // without `preserve_order`, so iterating it is already that order.
        Some(serde_json::Value::Object(map)) => Cow::Owned(format!(
            "map[{}]",
            map.iter()
                .map(|(key, item)| format!("{key}:{}", render_value(Some(item))))
                .collect::<Vec<_>>()
                .join(" ")
        )),
    }
}

/// `%v` of a JSON number: an integer keeps its digits, and anything else goes through Go's
/// shortest float form, which is not Rust's (`1e21` is `1e+21`, not twenty-one digits).
fn render_number(number: &serde_json::Number) -> String {
    if let Some(signed) = number.as_i64() {
        return signed.to_string();
    }
    if let Some(unsigned) = number.as_u64() {
        return unsigned.to_string();
    }
    number
        .as_f64()
        .map_or_else(|| number.to_string(), mm_model::utils::go_format_float)
}

/// Port of the go-i18n v1 bundle `initTranslationsWithDir` fills (shared/i18n/i18n.go:161), plus
/// the package's `locales` map.
///
/// # What is kept of each file
///
/// Only **plain-string** translations, each parsed into a [`Template`]. A **plural** entry (a
/// `{"one": …, "other": …}` object) is dropped: selecting its form needs the CLDR plural spec of
/// the language, which is not ported, and no id this port raises has one. An empty string is
/// *kept* — `translate` turns an empty rendering into the id (bundle.go:396), which is where that
/// belongs.
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
    files: Vec<std::collections::HashMap<String, Template>>,
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
                    file.insert(entry.id, Template::parse(text));
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
    fn file_for(&self, pref: &str) -> Option<&std::collections::HashMap<String, Template>> {
        parse_language_tags(pref).into_iter().find_map(|tag| {
            self.by_tag
                .get(&tag)
                .or_else(|| self.fallback.get(&tag))
                .and_then(|&index| self.files.get(index))
        })
    }

    /// `b.translate(lang, id, params)` (go-i18n bundle.go:341) for one language: the rendered
    /// template, or `None` wherever Go answers the id — no such language, no such id, an empty
    /// rendering, or a construct this port does not render.
    fn translate_once(&self, pref: &str, id: &str, params: Option<&Params>) -> Option<String> {
        let rendered = self.file_for(pref)?.get(id)?.execute(params)?;
        (!rendered.is_empty()).then(|| rendered.into_owned())
    }

    /// Port of `tfuncWithFallback(pref)(id, params…)` (i18n.go:295): `pref`'s rendering unless
    /// that *is* the id, then `en`'s, then the id itself.
    ///
    /// The comparison is against the rendered text, not "was the id found" — that is Go's own
    /// test, and it is why an entry that renders empty falls back to `en` rather than stopping.
    pub fn translate_with(&self, pref: &str, id: &str, params: Option<&Params>) -> String {
        match self.translate_once(pref, id, params) {
            Some(text) if text != id => text,
            _ => self
                .translate_once(FALLBACK_LOCALE, id, params)
                .unwrap_or_else(|| id.to_owned()),
        }
    }

    /// [`translate_with`](Self::translate_with) for an id with no params — `T(id)`.
    pub fn translate(&self, pref: &str, id: &str) -> String {
        self.translate_with(pref, id, None)
    }

    /// Port of `AppError.Translate` (model/utils.go:281) with the `T` of `pref`.
    ///
    /// Go branches on `params == nil` to call `T(id)` rather than `T(id, params)`, and go-i18n
    /// then executes the template against a nil `data` instead of the map. `text/template` prints
    /// a field of either as `<no value>`, so the two branches produce the same string and the
    /// `Option` passes straight through — pinned by the `none` and `empty` variants of
    /// `fixtures/behaviour_i18n.json`.
    ///
    /// `SkipTranslation` is Go's escape hatch and nothing in the reference tree sets it; the
    /// branch exists so that a port of something that does stays correct.
    pub fn translate_app_error(&self, pref: &str, err: &mut mm_model::utils::AppError) {
        if err.skip_translation {
            return;
        }
        err.message = self.translate_with(pref, &err.id, err.params.as_ref());
    }

    /// Whether this port renders `id` for `pref` the way Go would, or answers the id because the
    /// entry uses a construct it skips (see the module docs).
    ///
    /// Only one caller needs the difference: the **signed** web error page, whose URL carries the
    /// message, so a fallback there would be a different signed document rather than a cosmetic
    /// divergence. It forwards to Go instead.
    pub fn can_render(&self, pref: &str, id: &str) -> bool {
        let renderable = |pref: &str| {
            self.file_for(pref)
                .and_then(|file| file.get(id))
                .map(|template| !matches!(template.body, Body::Unsupported))
        };
        renderable(pref)
            .or_else(|| renderable(FALLBACK_LOCALE))
            .unwrap_or(true)
    }

    /// The locale `GetUserTranslations(locale)` (i18n.go:251) translates with: `locale` itself
    /// when a file was loaded for it — an **exact**, case-sensitive key lookup, with none of the
    /// tag fallback a request's `Accept-Language` gets — else `en`.
    pub fn user_locale<'a>(&self, locale: &'a str) -> &'a str {
        if self.locales.contains(locale) {
            locale
        } else {
            FALLBACK_LOCALE
        }
    }

    /// Port of `GetTranslationsBySystemLocale` (i18n.go:223): `DefaultServerLocale` when a file
    /// was loaded for it, else `en`.
    ///
    /// This is the `T` behind `i18n.T`, which `model.NewAppError` translates with at construction
    /// (model/utils.go:374). For an HTTP error that message is overwritten by the request's `T`
    /// before it is written; for a **websocket** error frame it is the message that goes on the
    /// wire, because nothing re-translates one (wsapi/websocket_handler.go:66).
    pub fn server_locale<'a>(&self, default_server_locale: &'a str) -> &'a str {
        if self.locales.contains(default_server_locale) {
            default_server_locale
        } else {
            FALLBACK_LOCALE
        }
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

/// `utils.TranslationsPreInit` (channels/utils/i18n.go:15) — `FindDirRelBinary("i18n")`, which
/// resolves against the **working directory** first, so this process finds the same `i18n/` the
/// Go server beside it reads (`scripts/mm-api-env.sh` launches it from Go's run directory).
async fn load() -> Result<Translations, I18nError> {
    tokio::task::spawn_blocking(|| {
        let (dir, found) = crate::logs::find_dir_rel_binary("i18n");
        if !found {
            return Err(I18nError::NoDirectory);
        }
        Translations::load_dir(&dir)
    })
    .await
    .map_err(|err| I18nError::Join(err.to_string()))?
}

/// Load the bundle at start-up, loudly.
///
/// Go's `TranslationsPreInit` error aborts the server (`cmd/mattermost/commands/server.go`), and
/// it must abort this one too: without translations every error body would carry an id where Go
/// writes a sentence, on every route at once, and nothing about the running server would say so.
/// Idempotent — a request that beat it to [`translations`] loaded the same directory.
pub async fn init() -> Result<(), I18nError> {
    let bundle = load().await?;
    let _already_loaded = TRANSLATIONS.set(Some(bundle));
    Ok(())
}

/// The process's bundle, loading it on first use. `None` when there is no such directory or a
/// file in it will not load — a server Go refuses to start, and one [`init`] refuses too.
pub async fn translations() -> Option<&'static Translations> {
    TRANSLATIONS
        .get_or_init(|| async {
            load()
                .await
                .map_err(|err| tracing::error!(error = %err, "could not load the translations"))
                .ok()
        })
        .await
        .as_ref()
}

/// The bundle if it is already loaded, for the callers that cannot await — the websocket's error
/// frames, which are built from a synchronous `returnWebSocketError`. [`init`] has run long
/// before one is reachable.
pub fn loaded() -> Option<&'static Translations> {
    TRANSLATIONS.get().and_then(Option::as_ref)
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

    /// `template_render` from `fixtures/behaviour_i18n.json`: Go's `text/template` over every
    /// value kind a params map can hold, plus the missing key and the nil datum. This is the
    /// oracle for [`Template`] itself, independent of any locale file.
    #[test]
    fn go_parity_template_rendering() {
        let fixture = i18n_fixture();
        let rows = fixture["template_render"].as_array().expect("an array");
        assert!(rows.len() > 20, "the corpus is all there: {}", rows.len());
        let mut failures = Vec::new();
        for row in rows {
            let src = row["src"].as_str().expect("a string").to_owned();
            let params: Option<Params> = match &row["data"] {
                serde_json::Value::Null => None,
                serde_json::Value::Object(map) => {
                    Some(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                }
                other => panic!("unexpected data {other}"),
            };
            let template = Template::parse(src.clone());
            let got = template
                .execute(params.as_ref())
                .map(std::borrow::Cow::into_owned);
            let want = row["output"].as_str().expect("a string");
            if got.as_deref() != Some(want) {
                failures.push(format!(
                    "{src:?} with {:?}: got {got:?}, Go {want:?}",
                    row["data"]
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

    /// `AppError.Translate` through the real bundle: every (default client locale,
    /// `Accept-Language`, id, params variant) row Go recorded, including the ids whose sentences
    /// carry `{{.Field}}` and the variants that leave a field out.
    #[test]
    fn go_parity_translate_app_error_for_a_request() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        let fixture = i18n_fixture();
        let rows = fixture["translations"].as_array().expect("an array");
        assert!(rows.len() > 4000, "the corpus is all there: {}", rows.len());
        let mut failures = Vec::new();
        for row in rows {
            let accept = row["accept_language"].as_str().expect("a string");
            let default = row["default_client_locale"].as_str().expect("a string");
            let id = row["id"].as_str().expect("a string");
            let mut err = mm_model::utils::AppError::new(
                "Oracle",
                id,
                params_of(&row["params"]),
                "detail",
                400,
            );
            bundle.translate_app_error(bundle.request_locale(accept, default), &mut err);
            if err.message != row["message"].as_str().expect("a string") {
                failures.push(format!(
                    "{accept:?}/{default:?} {id} ({}): got {:?}, Go {:?}",
                    row["variant"], err.message, row["message"]
                ));
            }
        }
        assert!(
            failures.is_empty(),
            "{} of {} mismatches, first 20:\n{}",
            failures.len(),
            rows.len(),
            failures
                .iter()
                .take(20)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }

    /// The same, through [`Translations::server_locale`] — the `i18n.T` a websocket error frame
    /// carries, which depends on `DefaultServerLocale` and never on the request.
    #[test]
    fn go_parity_translate_app_error_for_the_server_locale() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        let fixture = i18n_fixture();
        let rows = fixture["server_translations"].as_array().expect("an array");
        assert!(rows.len() > 200, "the corpus is all there: {}", rows.len());
        let mut failures = Vec::new();
        for row in rows {
            let server = row["default_server_locale"].as_str().expect("a string");
            let id = row["id"].as_str().expect("a string");
            let mut err = mm_model::utils::AppError::new(
                "Oracle",
                id,
                params_of(&row["params"]),
                "detail",
                400,
            );
            bundle.translate_app_error(bundle.server_locale(server), &mut err);
            if err.message != row["message"].as_str().expect("a string") {
                failures.push(format!(
                    "server {server:?} {id}: got {:?}, Go {:?}",
                    err.message, row["message"]
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

    /// `SkipTranslation` leaves the message alone — the one branch of `Translate` the corpus can
    /// only state, because nothing in the reference tree takes it.
    #[test]
    fn go_parity_skip_translation_is_a_no_op() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        let mut err = mm_model::utils::AppError::new(
            "Oracle",
            "api.context.session_expired.app_error",
            None,
            "",
            401,
        );
        err.message = "left alone".to_owned();
        err.skip_translation = true;
        bundle.translate_app_error("en", &mut err);
        assert_eq!(err.message, i18n_fixture()["skip_translation"]);
    }

    /// The two constructs this port skips are exactly the two the shipped files contain, and
    /// **no id reachable from this server's code** uses either.
    ///
    /// This is the guard that keeps the skip honest. `loaded_file_actions` is Go's own census of
    /// every `{{…}}` body and every plural id across the 22 loaded files; the reachable-id set is
    /// grepped out of `crates/` here rather than transcribed, so a new handler that raises a
    /// plural or `{{if}}` id fails this test rather than shipping a wrong sentence.
    #[test]
    fn go_parity_no_reachable_id_needs_a_construct_we_skip() {
        let Some(bundle) = reference_bundle() else {
            return;
        };
        let fixture = i18n_fixture();
        let census = &fixture["loaded_file_actions"];
        let unsupported: Vec<&str> = census["actions"]
            .as_object()
            .expect("an object")
            .keys()
            .filter(|action| field_action(action).is_none())
            .map(String::as_str)
            .collect();
        assert_eq!(
            unsupported,
            ["else", "end", "if .disableBotsSetting", "if .printAllBots"],
            "the shipped files grew a construct this port does not render"
        );

        // Every id this port can raise, read out of the source tree.
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut reachable = std::collections::BTreeSet::new();
        collect_translation_ids(&crates, &mut reachable);
        assert!(
            reachable.len() > 500,
            "the id sweep found only {}",
            reachable.len()
        );

        let plural: Vec<&str> = census["plural_ids"]
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter(|id| reachable.contains(*id))
            .collect();
        assert!(
            plural.is_empty(),
            "reachable ids with a plural entry: {plural:?}"
        );

        let skipped: Vec<&String> = reachable
            .iter()
            .filter(|id| !bundle.can_render("en", id))
            .collect();
        assert!(
            skipped.is_empty(),
            "reachable ids whose template this port skips: {skipped:?}"
        );
    }

    /// `fixtures/behaviour_i18n.json`, generated by `reference/dump/behaviour_i18n.go`.
    fn i18n_fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../../../fixtures/behaviour_i18n.json"))
            .expect("behaviour_i18n.json is generated by reference/dump")
    }

    /// A fixture row's `params` as an `AppError` would carry them — `null` is Go's nil map, which
    /// is a different branch of `Translate` from an empty one.
    fn params_of(value: &serde_json::Value) -> Option<Params> {
        value
            .as_object()
            .map(|map| map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    /// Every `"…"` string literal in `crates/*/src` that looks like a translation id. Deliberately
    /// over-inclusive: a false positive costs nothing (an id nothing raises still must not need a
    /// construct we skip), while a false negative would be the hole this test exists to close.
    fn collect_translation_ids(
        dir: &std::path::Path,
        out: &mut std::collections::BTreeSet<String>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                collect_translation_ids(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for piece in text.split('"').skip(1).step_by(2) {
                    if piece.len() > 8
                        && piece.contains('.')
                        && piece.split('.').count() > 2
                        && piece.chars().all(|c| {
                            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '_'
                        })
                    {
                        out.insert(piece.to_owned());
                    }
                }
            }
        }
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
