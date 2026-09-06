//! Port of the `model.Config` settings something ported actually reads — and nothing else.
//!
//! `config.go` is 5,795 lines across 53 structs and this models sixteen settings from six
//! sections. That is not a staging post on the way to translating the rest: the document is read
//! whole from the database and the unmodelled sections are simply ignored, so a field appears here
//! when a reader needs it and its doc comment names that reader. **A config field with no reader
//! is a guess about a wire format nothing can falsify** — the same rule the rest of the project
//! applies to model files.
//!
//! Growing it costs one `Option<T>` in [`Document`], one line in [`Config::from_document`], one
//! key in `scripts/dump-config-fixture.sh`, and the count in
//! `the_fixture_covers_every_document_sourced_setting`.
//!
//! # Where these values come from
//!
//! From the **shared database**, then overlaid with the environment — in that order, which is Go's
//! own order and not an arbitrary one.
//!
//! This used to be environment-only, because Go's default backing store is `config.FileStore`
//! over `config.json` on a Docker volume this process cannot see: the two servers shared a
//! database but not a configuration, and every setting read here was an assumption. That was
//! [D-156]. `docker-compose.yml` now sets `MM_CONFIG` to the shared Postgres DSN, which makes Go
//! select `config.DatabaseStore` (config/store.go:91) and keep the whole `model.Config` as one
//! JSON document in `Configurations.Value`. [`mm_store::ConfigStore`] reads that document and
//! [`Config::load`] layers the environment on top of it.
//!
//! **The document is the configuration Go persists, not the one it runs on.** `Store.Load` builds
//! two configs and writes back the one *without* the environment applied (store.go:321), so a
//! reader that stops at the document disagrees with the running server on precisely the settings
//! an operator bothered to change. Measured against the live stack: the row says
//! `ServiceSettings.SiteURL == ""` while the server beside it is running on
//! `MM_SERVICESETTINGS_SITEURL=http://localhost:8065`. Hence [`Config::apply_env`], and hence its
//! being applied after the document rather than as a fallback for it.
//!
//! **`FeatureFlags` is not in the document at all.** Go clears the section before persisting when
//! `readOnlyFF` is set, which is the default (store.go:306-310) — confirmed against the live row,
//! which has no `FeatureFlags` key. So [`Config::feature_flag_burn_on_read`] can only ever come
//! from the environment or from the compiled-in default, and a future flag must not be given a
//! database source it does not have.
//!
//! # Which direction each default fails
//!
//! This matters more than the defaults themselves, because the two settings fail in *opposite*
//! directions when we are wrong about them:
//!
//! - `restrict_system_admin = false` makes `SessionHasPermissionToAndNotRestrictedAdmin`
//!   behave exactly like `SessionHasPermissionTo`. Being wrong here **over-grants**: we would
//!   admit a restricted system admin that Go denies.
//! - `compliance_enable = false` takes the public-channel fallback branch in
//!   `HasPermissionToReadChannel`, which is the *permissive* one. Being wrong here
//!   **over-grants** too: we would let a non-member read a public channel that Go, with
//!   compliance on, confines to members.
//!
//! Both are `false` in Go and both over-grant if that is wrong, so neither is a safe assumption
//! to bury. Note also that `authorization.go:475` reads `ComplianceSettings.Enable` **without**
//! consulting the licence, even though every compliance *feature* is licence-gated
//! (`app/compliance.go:18`). So "Team Edition cannot enable compliance" is not a proof that this
//! branch is unreachable — the setting alone moves it.

/// Port of `model.Config` (config.go), restricted to the fields a migrated code path reads.
///
/// Deliberately not a lazily-grown mirror of the whole struct: a field appears here when
/// something ported consults it, and its doc comment names the caller. A config field with no
/// reader is a guess about the wire format that nothing can falsify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// `ExperimentalSettings.RestrictSystemAdmin` (config.go:1247).
    ///
    /// When true, Go's `SessionHasPermissionToAndNotRestrictedAdmin` denies **every** caller
    /// that is not unrestricted — it does not fall through to a role check. Read by
    /// [`crate::App::session_has_permission_to_and_not_restricted_admin`].
    pub restrict_system_admin: bool,

    /// `ComplianceSettings.Enable` (config.go:2874).
    ///
    /// When true, `HasPermissionToReadChannel` stops falling back to `read_public_channel` for
    /// open channels, confining reads to members so the compliance export sees every access.
    /// Read by [`crate::App::has_permission_to_read_channel`].
    pub compliance_enable: bool,

    /// `ImageProxySettings.Enable` (config.go:3996). Go default `false`.
    ///
    /// When true, `PostWithProxyAddedToImageURLs` rewrites every markdown image destination in
    /// the message and may set `message_source`. That needs the markdown parser ([D-044]), so
    /// [`crate::post::prepare_post_for_client_with_embeds_and_images`] refuses the post and the
    /// handler forwards to Go instead.
    pub image_proxy_enable: bool,

    /// `ServiceSettings.EnablePostIconOverride` (config.go:848). Go default `false`.
    ///
    /// When true, `OverrideIconURLIfEmoji` resolves an `override_icon_emoji` prop to a static
    /// emoji URL and writes it back into the post's props. Same treatment: a post carrying that
    /// prop is refused, and forwarded, while this is on.
    pub enable_post_icon_override: bool,

    /// `ServiceSettings.EnableCustomEmoji` (config.go:849). Go default **`true`**.
    ///
    /// Gates `metadata.emojis` entirely — `getCustomEmojisForPost` returns an empty slice
    /// without touching the store when this is off.
    pub enable_custom_emoji: bool,

    /// `ServiceSettings.PostPriority` (config.go:992). Go default **`true`**.
    ///
    /// Gates `metadata.priority` *and* `metadata.acknowledgements`. `IsPostPriorityEnabled`
    /// (app/post_priority.go:46) reads this and nothing else — there is **no licence check**,
    /// so the branch is live on Team Edition.
    pub post_priority: bool,

    /// `ServiceSettings.AllowSyncedDrafts` (config.go:488). Go default **`true`**.
    ///
    /// Gates the whole drafts feature. Every one of `getDrafts`, `upsertDraft` and `deleteDraft`
    /// checks it *first* and answers **501** `api.drafts.disabled.app_error` when it is off —
    /// before any permission check, so a caller with no rights at all still gets the 501 rather
    /// than a 403.
    pub allow_synced_drafts: bool,

    /// `ServiceSettings.EnableBurnOnRead` (config.go:472). Go default **`true`**.
    pub enable_burn_on_read: bool,

    /// `FeatureFlags.BurnOnRead` (feature_flags.go:90). Go default **`true`**.
    ///
    /// Kept apart from the setting above because `isBurnOnReadEnabled` (app/post_helpers.go:270)
    /// ands the two, and either one alone turns the feature off. Folding them into a single
    /// field here would make a deployment that disables only the flag indistinguishable from one
    /// that disables only the setting — the same value, reached two ways, is exactly the sort of
    /// coincidence that hides a wrong read.
    pub feature_flag_burn_on_read: bool,

    /// `FileSettings.DriverName` (config.go:1814). Go default **`"local"`**
    /// (`model.ImageDriverLocal`, config.go:1900).
    ///
    /// Read by [`crate::App::get_emoji`] and [`crate::App::get_emoji_by_name`], which refuse
    /// with `api.emoji.storage.app_error` (403) when it is the **empty string** — not when it
    /// is some driver we do not implement. A `String` rather than a `bool` because the value is
    /// what Go compares, and because `FileSettings.isValid` (config.go:4645) restricts it to
    /// `local`/`amazons3`/`azure`: an empty driver only ever arrives through a config that
    /// would fail Go's own validation, which is why the branch it gates is close to
    /// unreachable and is ported for fidelity rather than for coverage.
    pub file_driver_name: String,

    /// `ServiceSettings.EnableIncomingWebhooks` (config.go:388, defaulted at :607). Go default
    /// **`true`**.
    ///
    /// Gates all three of `GetIncomingWebhooksForTeamPageByUser`,
    /// `GetIncomingWebhooksPageByUser` and `GetIncomingWebhooksCount` (app/webhook.go:646, :659,
    /// :676), each answering **501** `api.incoming_webhook.disabled.app_error` — checked *after*
    /// the handler's permission gate, so a caller with no rights gets the 403 and only an
    /// authorised one ever sees the 501. Read by [`crate::App::get_incoming_webhooks_count`] and
    /// its two neighbours.
    pub enable_incoming_webhooks: bool,

    /// `ServiceSettings.EnableOutgoingWebhooks` (config.go:389, defaulted at :611). Go default
    /// **`true`**.
    ///
    /// The outgoing counterpart of [`Config::enable_incoming_webhooks`], gating the three
    /// functions behind `getOutgoingHooks` with **its own** error id,
    /// `api.outgoing_webhook.disabled.app_error`. Two settings, two ids, one status.
    pub enable_outgoing_webhooks: bool,

    /// `ServiceSettings.EnableOAuthServiceProvider` (config.go, defaulted at :595). Go default
    /// **`true`**.
    ///
    /// Gates all three OAuth **app** reads with `api.oauth.allow_oauth.turn_off.app_error` at
    /// **501** (app/oauth.go:75, :138, :151) — a third feature toggle with a third error id at the
    /// same status. Read by [`crate::App::get_oauth_apps`] and its two neighbours.
    pub enable_oauth_service_provider: bool,

    /// `PrivacySettings.ShowFullName` (config.go, defaulted `true`).
    ///
    /// Read by `getUser` and every route that sanitizes another user, and folded into
    /// `User.Etag`. Hardcoded in `mm-api`'s `AppState` until this module had a source of truth —
    /// see [D-085], which this closes: an admin turning it off now moves both servers together.
    pub show_full_name: bool,

    /// `PrivacySettings.ShowEmailAddress` (config.go, defaulted `true`).
    ///
    /// The companion of [`Config::show_full_name`], with the same readers and the same history.
    pub show_email_address: bool,

    /// The `MM_LICENSE` environment variable (`platform.LicenseEnv`, platform/license.go:26).
    ///
    /// Not an `MM_<SECTION>_<SETTING>` config overlay — it is its own variable, holding a whole
    /// signed licence rather than a setting, and Go reads it **before** the database
    /// (`LoadLicense`, platform/license.go:52). Kept here as the raw string because the only
    /// question anything ported asks of it is whether it is empty: validating a licence needs the
    /// signing key, which is not ported. Read by [`crate::App::license_state`].
    pub license: String,
}

impl Config {
    /// Port of `app.App.isBurnOnReadEnabled` (post_helpers.go:270).
    ///
    /// **Both halves default to true**, so on a stock server this is on — which is why
    /// `getCursorPostId` reaches the read-receipt-aware cursor query rather than the plain one.
    pub fn burn_on_read(&self) -> bool {
        self.feature_flag_burn_on_read && self.enable_burn_on_read
    }
}

impl Default for Config {
    /// Go's `SetDefaults` for exactly these fields. **Two of them default to `true`** — copying
    /// the `false` of the two above would silently drop `metadata.emojis` and
    /// `metadata.priority` from every response.
    fn default() -> Self {
        Self {
            restrict_system_admin: false,
            compliance_enable: false,
            image_proxy_enable: false,
            enable_post_icon_override: false,
            enable_custom_emoji: true,
            post_priority: true,
            allow_synced_drafts: true,
            enable_burn_on_read: true,
            feature_flag_burn_on_read: true,
            file_driver_name: "local".to_owned(),
            enable_incoming_webhooks: true,
            enable_outgoing_webhooks: true,
            enable_oauth_service_provider: true,
            show_full_name: true,
            show_email_address: true,
            license: String::new(),
        }
    }
}

impl Config {
    /// Go's defaults with the environment overlaid — i.e. [`Config::load`] with no document.
    ///
    /// Kept as the constructor for tests and for a deployment whose Go server has never written a
    /// configuration row.
    pub fn from_env() -> Self {
        Self::default().apply_env()
    }

    /// Apply the `MM_<SECTION>_<SETTING>` overlay on top of `self`.
    ///
    /// Port of `applyEnvironmentMap` (config/store.go:292), restricted to the settings modelled
    /// here. Go applies this **after** unmarshalling the document and running `SetDefaults`, and
    /// deliberately persists the pre-overlay config (store.go:321) — so this is a layer over the
    /// stored values, never a fallback for them. An operator who sets a variable has overridden
    /// the database on the Go server, and must override it here too.
    ///
    /// The variable names are Mattermost's own convention, so this agrees with the neighbouring
    /// Go server for free whenever that server is configured by environment.
    #[must_use]
    pub fn apply_env(self) -> Self {
        self.apply_env_from(&|key| std::env::var(key).ok())
    }

    /// [`Config::apply_env`] against an arbitrary lookup.
    ///
    /// The indirection exists so the overlay can be tested at all. The process environment is
    /// global and `std::env::set_var` races every other test in the binary, so an overlay that
    /// read `std::env` directly could only ever be exercised with **nothing set** — under which
    /// it is indistinguishable from doing nothing, and a mutation deleting it survives. Measured:
    /// two did, before this existed.
    #[must_use]
    fn apply_env_from(self, lookup: &impl Fn(&str) -> Option<String>) -> Self {
        let default = self;
        Self {
            restrict_system_admin: lookup_bool(
                lookup,
                "MM_EXPERIMENTALSETTINGS_RESTRICTSYSTEMADMIN",
                default.restrict_system_admin,
            ),
            compliance_enable: lookup_bool(
                lookup,
                "MM_COMPLIANCESETTINGS_ENABLE",
                default.compliance_enable,
            ),
            image_proxy_enable: lookup_bool(
                lookup,
                "MM_IMAGEPROXYSETTINGS_ENABLE",
                default.image_proxy_enable,
            ),
            enable_post_icon_override: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEPOSTICONOVERRIDE",
                default.enable_post_icon_override,
            ),
            enable_custom_emoji: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLECUSTOMEMOJI",
                default.enable_custom_emoji,
            ),
            post_priority: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_POSTPRIORITY",
                default.post_priority,
            ),
            allow_synced_drafts: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ALLOWSYNCEDDRAFTS",
                default.allow_synced_drafts,
            ),
            enable_burn_on_read: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEBURNONREAD",
                default.enable_burn_on_read,
            ),
            feature_flag_burn_on_read: lookup_bool(
                lookup,
                "MM_FEATUREFLAGS_BURNONREAD",
                default.feature_flag_burn_on_read,
            ),
            // Not `env_bool`'s fallback rule: a string setting has no unparseable value, so an
            // override of `""` is a deliberate empty driver and must survive as one.
            file_driver_name: lookup("MM_FILESETTINGS_DRIVERNAME")
                .unwrap_or(default.file_driver_name),
            enable_incoming_webhooks: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEINCOMINGWEBHOOKS",
                default.enable_incoming_webhooks,
            ),
            enable_outgoing_webhooks: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEOUTGOINGWEBHOOKS",
                default.enable_outgoing_webhooks,
            ),
            enable_oauth_service_provider: lookup_bool(
                lookup,
                "MM_SERVICESETTINGS_ENABLEOAUTHSERVICEPROVIDER",
                default.enable_oauth_service_provider,
            ),
            show_full_name: lookup_bool(
                lookup,
                "MM_PRIVACYSETTINGS_SHOWFULLNAME",
                default.show_full_name,
            ),
            show_email_address: lookup_bool(
                lookup,
                "MM_PRIVACYSETTINGS_SHOWEMAILADDRESS",
                default.show_email_address,
            ),
            // Its own variable, not part of the `MM_<SECTION>_<SETTING>` overlay, and Go treats
            // any non-empty value as "a licence was supplied" before it ever tries to parse it.
            license: lookup("MM_LICENSE").unwrap_or(default.license),
        }
    }

    /// Parse a persisted `model.Config` document into the settings modelled here.
    ///
    /// Port of the `json.Unmarshal` plus `SetDefaults` pair in `Store.Load` (config/store.go:260
    /// and :285), for these fields only. Unknown sections and unknown keys are ignored, which is
    /// what makes growing this struct one field at a time safe: the document already holds all 47
    /// sections, and a field appears here when something ported reads it.
    ///
    /// # Absent means Go's default, which is not the zero value
    ///
    /// Every setting in `config.go` is a **pointer**, and `SetDefaults` fills the nil ones — so an
    /// absent key takes Go's default, and for `EnableCustomEmoji`, `PostPriority`,
    /// `AllowSyncedDrafts`, the two webhook toggles, the OAuth toggle and both privacy settings
    /// that default is `true`. This is why every field below is an `Option<T>` resolved against
    /// [`Config::default`] rather than a `#[serde(default)]`: the derived default for a `bool` is
    /// `false`, so the tidier-looking spelling would silently turn eight features off whenever the
    /// document omitted them. A JSON `null` lands in the same place as an absent key, which is
    /// also Go's behaviour — it unmarshals to a nil pointer that `SetDefaults` then fills.
    pub fn from_document(document: &str) -> Result<Self, ConfigError> {
        let parsed: Document =
            serde_json::from_str(document).map_err(|source| ConfigError::Malformed { source })?;
        let default = Self::default();

        let service = parsed.service_settings.unwrap_or_default();
        Ok(Self {
            restrict_system_admin: parsed
                .experimental_settings
                .unwrap_or_default()
                .restrict_system_admin
                .unwrap_or(default.restrict_system_admin),
            compliance_enable: parsed
                .compliance_settings
                .unwrap_or_default()
                .enable
                .unwrap_or(default.compliance_enable),
            image_proxy_enable: parsed
                .image_proxy_settings
                .unwrap_or_default()
                .enable
                .unwrap_or(default.image_proxy_enable),
            enable_post_icon_override: service
                .enable_post_icon_override
                .unwrap_or(default.enable_post_icon_override),
            enable_custom_emoji: service
                .enable_custom_emoji
                .unwrap_or(default.enable_custom_emoji),
            post_priority: service.post_priority.unwrap_or(default.post_priority),
            allow_synced_drafts: service
                .allow_synced_drafts
                .unwrap_or(default.allow_synced_drafts),
            enable_burn_on_read: service
                .enable_burn_on_read
                .unwrap_or(default.enable_burn_on_read),
            // Deliberately NOT read from the document: Go clears `FeatureFlags` before persisting
            // (store.go:306-310), so the section is absent from every row it writes. Sourcing it
            // here would read an absence as a deliberate `false` on the next `readOnlyFF` change.
            feature_flag_burn_on_read: default.feature_flag_burn_on_read,
            file_driver_name: parsed
                .file_settings
                .unwrap_or_default()
                .driver_name
                .unwrap_or(default.file_driver_name),
            enable_incoming_webhooks: service
                .enable_incoming_webhooks
                .unwrap_or(default.enable_incoming_webhooks),
            enable_outgoing_webhooks: service
                .enable_outgoing_webhooks
                .unwrap_or(default.enable_outgoing_webhooks),
            enable_oauth_service_provider: service
                .enable_oauth_service_provider
                .unwrap_or(default.enable_oauth_service_provider),
            show_full_name: parsed
                .privacy_settings
                .as_ref()
                .and_then(|p| p.show_full_name)
                .unwrap_or(default.show_full_name),
            show_email_address: parsed
                .privacy_settings
                .and_then(|p| p.show_email_address)
                .unwrap_or(default.show_email_address),
            // Not a config field on either server — `MM_LICENSE` is its own variable, read by
            // `apply_env`.
            license: default.license,
        })
    }

    /// Load the configuration the Go server is running on: the active document, then the
    /// environment overlay.
    ///
    /// The order is Go's (config/store.go:260 → :285 → :292) and it is the whole point of the
    /// function. An absent document is not an error — Go's own `DatabaseStore.Load` answers a
    /// missing row with a marshalled default config (database.go:232) — so this falls back to
    /// [`Config::default`] and still applies the overlay.
    pub async fn load(store: &impl mm_store::ConfigStore) -> Result<Self, ConfigError> {
        Self::load_with_env(store, &|key| std::env::var(key).ok()).await
    }

    /// [`Config::load`] against an arbitrary environment lookup, so the *composition* of document
    /// and overlay is testable and not only each half separately.
    async fn load_with_env(
        store: &impl mm_store::ConfigStore,
        lookup: &impl Fn(&str) -> Option<String>,
    ) -> Result<Self, ConfigError> {
        let document = store.load_active().await?;
        let base = match document.as_deref() {
            Some(raw) => Self::from_document(raw)?,
            None => {
                tracing::warn!(
                    "no active row in Configurations; falling back to Go's compiled-in defaults. \
                     Is MM_CONFIG pointed at this database?"
                );
                Self::default()
            }
        };
        Ok(base.apply_env_from(lookup))
    }
}

/// Failure modes of [`Config::load`].
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The row could not be read.
    #[error("failed to read the active configuration")]
    Store(#[from] mm_store::StoreError),

    /// The row exists but is not a `model.Config` document.
    ///
    /// Go treats this as fatal too (`HumanizeJSONError`, store.go:261) — a server running on a
    /// configuration it could not parse is worse than one that refuses to start.
    #[error("the active configuration is not valid JSON")]
    Malformed {
        #[source]
        source: serde_json::Error,
    },
}

/// The slice of the persisted `model.Config` document this module reads.
///
/// Field-for-field a subset; the names are Go's own struct field names, because `config.go`
/// carries no `json:` tags on these and `encoding/json` therefore uses the Go identifier verbatim.
/// Confirmed against the live row rather than assumed.
#[derive(Debug, serde::Deserialize)]
struct Document {
    #[serde(rename = "ServiceSettings")]
    service_settings: Option<ServiceSettingsDocument>,
    #[serde(rename = "ComplianceSettings")]
    compliance_settings: Option<EnableOnlyDocument>,
    #[serde(rename = "ExperimentalSettings")]
    experimental_settings: Option<ExperimentalSettingsDocument>,
    #[serde(rename = "ImageProxySettings")]
    image_proxy_settings: Option<EnableOnlyDocument>,
    #[serde(rename = "FileSettings")]
    file_settings: Option<FileSettingsDocument>,
    #[serde(rename = "PrivacySettings")]
    privacy_settings: Option<PrivacySettingsDocument>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ServiceSettingsDocument {
    #[serde(rename = "EnablePostIconOverride")]
    enable_post_icon_override: Option<bool>,
    #[serde(rename = "EnableCustomEmoji")]
    enable_custom_emoji: Option<bool>,
    #[serde(rename = "PostPriority")]
    post_priority: Option<bool>,
    #[serde(rename = "AllowSyncedDrafts")]
    allow_synced_drafts: Option<bool>,
    #[serde(rename = "EnableBurnOnRead")]
    enable_burn_on_read: Option<bool>,
    #[serde(rename = "EnableIncomingWebhooks")]
    enable_incoming_webhooks: Option<bool>,
    #[serde(rename = "EnableOutgoingWebhooks")]
    enable_outgoing_webhooks: Option<bool>,
    #[serde(rename = "EnableOAuthServiceProvider")]
    enable_oauth_service_provider: Option<bool>,
}

/// `ComplianceSettings` and `ImageProxySettings` both contribute exactly one field, and it has the
/// same name in each.
#[derive(Debug, Default, serde::Deserialize)]
struct EnableOnlyDocument {
    #[serde(rename = "Enable")]
    enable: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct ExperimentalSettingsDocument {
    #[serde(rename = "RestrictSystemAdmin")]
    restrict_system_admin: Option<bool>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct FileSettingsDocument {
    #[serde(rename = "DriverName")]
    driver_name: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct PrivacySettingsDocument {
    #[serde(rename = "ShowFullName")]
    show_full_name: Option<bool>,
    #[serde(rename = "ShowEmailAddress")]
    show_email_address: Option<bool>,
}

/// `strconv.ParseBool` (Go strconv/atob.go:10) — the exact set of accepted spellings, and
/// `None` for everything else.
///
/// Go's list is closed and case-sensitive apart from the six forms below: `TRUE`, `True` and
/// `true` parse, but `tRuE` and `yes` do not. Widening it to `eq_ignore_ascii_case` would accept
/// values the Go server rejects, which is how the two configurations drift apart.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

/// [`parse_bool`] over an environment variable, with an absent *or* unparseable value falling
/// back to the default.
///
/// The fallback direction is Go's: viper leaves the setting at its configured default when an
/// override does not parse, so `MM_COMPLIANCESETTINGS_ENABLE=yes` is *not* true on either
/// server. Treating an unparseable value as `true` would silently diverge on a typo.
/// See [`Config::apply_env_from`] for why the lookup is a parameter rather than `std::env`.
fn lookup_bool(lookup: &impl Fn(&str) -> Option<String>, key: &str, default: bool) -> bool {
    lookup(key)
        .and_then(|raw| parse_bool(&raw))
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_gos_set_defaults() {
        let config = Config::default();
        assert!(!config.restrict_system_admin, "config.go:1269 — new(false)");
        assert!(!config.compliance_enable, "config.go:2875 — new(false)");
        assert!(!config.image_proxy_enable, "config.go:3996 — new(false)");
        assert!(
            !config.enable_post_icon_override,
            "config.go:848 — new(false)"
        );
        // The two that are **not** false. A port that assumed the pattern held would drop
        // `metadata.emojis` and `metadata.priority` from every post.
        assert!(config.enable_custom_emoji, "config.go:850 — new(true)");
        assert!(config.post_priority, "config.go:993 — new(true)");
        assert!(config.enable_burn_on_read, "config.go:1034 — new(true)");
        assert!(
            config.feature_flag_burn_on_read,
            "feature_flags.go:187 — f.BurnOnRead = true"
        );
        // And therefore the conjunction, which is what decides which cursor query runs.
        assert!(
            config.burn_on_read(),
            "post_helpers.go:270 — both halves true"
        );
        // The one string setting. `model.ImageDriverLocal` is the literal `"local"`; the emoji
        // reads compare it against `""`, so a default of `""` here would 403 every one of them.
        assert_eq!(
            config.file_driver_name, "local",
            "config.go:1900 — new(ImageDriverLocal)"
        );
    }

    /// Either half alone turns it off — the reason the two are separate fields.
    #[test]
    fn burn_on_read_needs_both_halves() {
        let config = Config {
            enable_burn_on_read: false,
            ..Config::default()
        };
        assert!(!config.burn_on_read(), "the setting alone disables it");

        let config = Config {
            feature_flag_burn_on_read: false,
            ..Config::default()
        };
        assert!(!config.burn_on_read(), "the flag alone disables it");
    }

    /// Exactly `strconv.ParseBool`'s twelve spellings, and nothing else.
    #[test]
    fn parse_bool_matches_go_strconv() {
        for raw in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(raw), Some(true), "{raw} should parse true");
        }
        for raw in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(raw), Some(false), "{raw} should parse false");
        }
    }

    /// The near-misses. Each of these is a value an operator plausibly writes, and Go rejects
    /// every one — so each must fall back rather than being read as true.
    #[test]
    fn parse_bool_rejects_everything_go_rejects() {
        for raw in [
            "yes", "no", "on", "off", "tRuE", "TrUe", "y", "n", "2", "-1", "", " true", "true ",
            "enabled",
        ] {
            assert_eq!(parse_bool(raw), None, "{raw:?} must not parse");
        }
    }

    /// An absent key takes the default in **both** directions — a fallback that always returned
    /// `false` would look correct against Go's defaults while ignoring its argument entirely.
    ///
    /// `env_bool` is exercised through a key nothing sets rather than by mutating the
    /// environment: the process environment is global, and writing it from one test races every
    /// other test in the binary.
    #[test]
    fn an_absent_key_takes_the_default_either_way() {
        let empty = |_: &str| None;
        assert!(lookup_bool(&empty, "MM_ANYTHING", true));
        assert!(!lookup_bool(&empty, "MM_ANYTHING", false));
    }

    /// A value that does not parse falls back too — Go's viper leaves the setting at its
    /// configured value rather than treating a typo as `true`.
    #[test]
    fn an_unparseable_value_takes_the_default_either_way() {
        let yes = |_: &str| Some("yes".to_owned());
        assert!(lookup_bool(&yes, "MM_ANYTHING", true));
        assert!(
            !lookup_bool(&yes, "MM_ANYTHING", false),
            "`yes` is not `true`"
        );
    }

    /// And a value that *does* parse wins over the default in both directions.
    #[test]
    fn a_parseable_value_overrides_the_default() {
        assert!(lookup_bool(
            &|_: &str| Some("true".to_owned()),
            "MM_ANYTHING",
            false
        ));
        assert!(!lookup_bool(
            &|_: &str| Some("false".to_owned()),
            "MM_ANYTHING",
            true
        ));
    }
}

/// Parity tests against `fixtures/config_active.json` — the configuration a real Go server booted
/// itself on, projected to the modelled keys by `scripts/dump-config-fixture.sh`.
#[cfg(test)]
mod go_parity {
    use super::*;

    const ACTIVE: &str = include_str!("../../../fixtures/config_active.json");

    /// **The test this whole change exists for.** Every default in [`Config::default`] was
    /// transcribed by reading `SetDefaults` in a 5,795-line Go file; this asserts each one against
    /// what a Go server actually wrote after running it.
    ///
    /// Before the config document was reachable there was no way to check the transcription at
    /// all — `defaults_match_gos_set_defaults` asserts the same values against line numbers a
    /// human read, which catches a typo in the test and nothing in the world.
    #[test]
    fn every_default_matches_what_go_actually_wrote() {
        let from_go = Config::from_document(ACTIVE).expect("the fixture is a config document");
        let transcribed = Config::default();

        assert_eq!(
            from_go, transcribed,
            "a default transcribed from config.go disagrees with the one Go wrote"
        );
    }

    /// The fixture covers **every** setting read from the document.
    ///
    /// Without this the coverage rots silently. `scripts/dump-config-fixture.sh` carries its own
    /// copy of the key list, so a field added to [`Config`] but not to the script is simply absent
    /// from the fixture — and then `every_default_matches_what_go_actually_wrote` compares its
    /// *default* against its default and passes, having proved nothing about the new field. The
    /// count is the cheapest thing that fails instead.
    ///
    /// Fourteen, not sixteen: `feature_flag_burn_on_read` and `license` are the two settings that
    /// do not come from the document at all.
    #[test]
    fn the_fixture_covers_every_document_sourced_setting() {
        let fixture: serde_json::Value = serde_json::from_str(ACTIVE).expect("the fixture is JSON");
        let keys: usize = fixture
            .as_object()
            .expect("an object of sections")
            .values()
            .map(|section| section.as_object().expect("a section of settings").len())
            .sum();

        assert_eq!(
            keys, 14,
            "the fixture covers {keys} settings and Config reads 14 from the document. \
             Add the new key to scripts/dump-config-fixture.sh and re-run it — a modelled \
             setting the fixture does not carry is a setting Go's own output never checked"
        );
    }

    /// The document supplies values; it does not merely fail to override defaults. Flipping every
    /// modelled boolean away from its default proves each field is genuinely read — without this,
    /// a `from_document` that ignored its argument entirely would pass the test above.
    #[test]
    fn every_field_is_actually_read_from_the_document() {
        let inverted = r#"{
            "ServiceSettings": {
                "EnablePostIconOverride": true,
                "EnableCustomEmoji": false,
                "PostPriority": false,
                "AllowSyncedDrafts": false,
                "EnableBurnOnRead": false,
                "EnableIncomingWebhooks": false,
                "EnableOutgoingWebhooks": false,
                "EnableOAuthServiceProvider": false
            },
            "ComplianceSettings": { "Enable": true },
            "ExperimentalSettings": { "RestrictSystemAdmin": true },
            "ImageProxySettings": { "Enable": true },
            "FileSettings": { "DriverName": "amazons3" },
            "PrivacySettings": { "ShowFullName": false, "ShowEmailAddress": false }
        }"#;
        let config = Config::from_document(inverted).expect("valid document");

        assert!(config.enable_post_icon_override);
        assert!(!config.enable_custom_emoji);
        assert!(!config.post_priority);
        assert!(!config.allow_synced_drafts);
        assert!(!config.enable_burn_on_read);
        assert!(!config.enable_incoming_webhooks);
        assert!(!config.enable_outgoing_webhooks);
        assert!(!config.enable_oauth_service_provider);
        assert!(config.compliance_enable);
        assert!(config.restrict_system_admin);
        assert!(config.image_proxy_enable);
        assert_eq!(config.file_driver_name, "amazons3");
        assert!(!config.show_full_name);
        assert!(!config.show_email_address);
    }

    /// The two privacy settings are read from **different** keys.
    ///
    /// Both default to `true` and the inversion test above sets both to `false`, so a port that
    /// wired `ShowFullName` into both fields would pass every other test in this module. They are
    /// separated here because they are also separated on the wire: `Sanitize` consults them
    /// independently, and a caller who may see names but not emails is an ordinary configuration.
    #[test]
    fn the_two_privacy_settings_are_not_the_same_key() {
        let config = Config::from_document(
            r#"{"PrivacySettings":{"ShowFullName":true,"ShowEmailAddress":false}}"#,
        )
        .expect("valid document");
        assert!(config.show_full_name, "names are shown");
        assert!(!config.show_email_address, "emails are not");

        let swapped = Config::from_document(
            r#"{"PrivacySettings":{"ShowFullName":false,"ShowEmailAddress":true}}"#,
        )
        .expect("valid document");
        assert!(!swapped.show_full_name);
        assert!(swapped.show_email_address);
    }

    /// The same asymmetry for the two webhook toggles, which are likewise both `true` by default
    /// and carry *different* error ids on the routes they gate.
    #[test]
    fn the_two_webhook_toggles_are_not_the_same_key() {
        let config = Config::from_document(
            r#"{"ServiceSettings":{"EnableIncomingWebhooks":false,"EnableOutgoingWebhooks":true}}"#,
        )
        .expect("valid document");
        assert!(!config.enable_incoming_webhooks);
        assert!(config.enable_outgoing_webhooks);
    }

    /// **The trap this module is shaped to avoid.** An absent key is Go's *default*, not the zero
    /// value: `config.go` makes every setting a pointer and `SetDefaults` fills the nil ones. A
    /// `#[serde(default)]` on these bools would read `{}` as eight features switched off.
    #[test]
    fn an_absent_section_takes_gos_default_not_the_zero_value() {
        let config = Config::from_document("{}").expect("an empty object is a partial config");
        assert_eq!(
            config,
            Config::default(),
            "an empty document is all defaults"
        );

        // Named individually, because these are the ones where default and zero disagree.
        assert!(config.enable_custom_emoji);
        assert!(config.post_priority);
        assert!(config.allow_synced_drafts);
        assert!(config.enable_burn_on_read);
        assert!(config.enable_incoming_webhooks);
        assert!(config.enable_outgoing_webhooks);
        assert!(config.enable_oauth_service_provider);
        assert!(config.show_full_name);
        assert!(config.show_email_address);
        assert_eq!(config.file_driver_name, "local");
    }

    /// A present section with an absent key is the same as an absent section — `SetDefaults` walks
    /// the whole struct, not only the sections the document mentioned.
    #[test]
    fn a_half_filled_section_defaults_the_rest() {
        let config = Config::from_document(r#"{"ServiceSettings":{"PostPriority":false}}"#)
            .expect("valid document");
        assert!(!config.post_priority, "the key that was present");
        assert!(
            config.enable_custom_emoji,
            "its neighbour keeps Go's default"
        );
    }

    /// `null` unmarshals to a nil pointer in Go, which `SetDefaults` then fills — so it is the
    /// same as absent, and specifically *not* `false`.
    #[test]
    fn an_explicit_null_is_the_default_not_false() {
        let config = Config::from_document(r#"{"ServiceSettings":{"EnableCustomEmoji":null}}"#)
            .expect("null is a nil pointer, not a parse failure");
        assert!(config.enable_custom_emoji);
    }

    /// `FeatureFlags` is never persisted (config/store.go:306-310), so it must not be sourced from
    /// the document even when something puts one there — a stray section must not be able to turn
    /// a flag off. `scripts/dump-config-fixture.sh` fails loudly if the live row ever grows one.
    #[test]
    fn feature_flags_are_not_read_from_the_document() {
        let config = Config::from_document(r#"{"FeatureFlags":{"BurnOnRead":false}}"#)
            .expect("valid document");
        assert!(
            config.feature_flag_burn_on_read,
            "the flag comes from the environment or the compiled-in default, never the row"
        );
    }

    /// The flag is not the setting. `isBurnOnReadEnabled` ands the two (app/post_helpers.go:270),
    /// which is why they are separate fields — and why sourcing the flag from
    /// `ServiceSettings.EnableBurnOnRead`, the nearest plausible confusion, has to be visible.
    #[test]
    fn the_burn_on_read_flag_is_not_the_burn_on_read_setting() {
        let config = Config::from_document(r#"{"ServiceSettings":{"EnableBurnOnRead":false}}"#)
            .expect("valid document");
        assert!(
            !config.enable_burn_on_read,
            "the setting is read from the row"
        );
        assert!(
            config.feature_flag_burn_on_read,
            "the flag is not, and must not follow it"
        );
        assert!(
            !config.burn_on_read(),
            "either half alone still disables it"
        );
    }

    /// The document holds all 47 sections and this struct models six of them. Ignoring the rest is
    /// what makes growing the struct one reader at a time safe.
    #[test]
    fn unknown_sections_and_keys_are_ignored() {
        let config = Config::from_document(
            r#"{"SqlSettings":{"DataSource":"secret"},"ServiceSettings":{"SiteURL":"x"}}"#,
        )
        .expect("valid document");
        assert_eq!(config, Config::default());
    }

    /// Go refuses to start on a configuration it cannot parse (`HumanizeJSONError`,
    /// config/store.go:261) and so do we — see the `?` in `main.rs`.
    #[test]
    fn a_malformed_document_is_an_error() {
        let err = Config::from_document("{not json").expect_err("must not be accepted");
        assert!(matches!(err, ConfigError::Malformed { .. }));
    }

    /// A [`mm_store::ConfigStore`] that answers with whatever it was built on, so [`Config::load`]
    /// can be tested without a database.
    struct FakeStore(Option<String>);

    impl mm_store::ConfigStore for FakeStore {
        async fn load_active(&self) -> Result<Option<String>, mm_store::StoreError> {
            Ok(self.0.clone())
        }
    }

    /// `load` reads the document. Without this, every assertion in this module could hold while
    /// `load` ignored its store and returned `Config::default().apply_env()` — the tests above all
    /// call `from_document` directly, and the wiring between the two is exactly what a reader
    /// would get wrong.
    #[tokio::test]
    async fn load_takes_its_values_from_the_document() {
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"PostPriority":false}}"#
                .to_owned(),
        ));
        let config = Config::load(&store).await.expect("loads");

        assert!(
            config.compliance_enable,
            "read from the document, not defaulted"
        );
        assert!(!config.post_priority, "and so is this one");
        assert!(
            config.enable_custom_emoji,
            "while the rest keep Go's defaults"
        );
    }

    /// An absent document is not an error — Go answers a missing row with a marshalled default
    /// config (config/database.go:232) rather than refusing to boot.
    #[tokio::test]
    async fn load_falls_back_to_defaults_when_no_row_is_active() {
        let config = Config::load(&FakeStore(None)).await.expect("loads");
        assert_eq!(config, Config::default().apply_env());
    }

    /// A malformed document *is* an error, and it reaches the caller rather than being swallowed
    /// into defaults. `main.rs` turns this into a refusal to start.
    #[tokio::test]
    async fn load_propagates_a_malformed_document() {
        let store = FakeStore(Some("{not json".to_owned()));
        let err = Config::load(&store)
            .await
            .expect_err("must not be accepted");
        assert!(
            matches!(err, ConfigError::Malformed { .. }),
            "a config we cannot parse must not silently become the defaults"
        );
    }

    /// **The overlay actually overrides the document.** Both survivors of the first mutation run
    /// lived here: with no `MM_` variable set — which is every test process — `apply_env` is
    /// indistinguishable from doing nothing, so deleting it from `load` changed no observable
    /// behaviour. A fake environment is the fixture that was missing.
    #[tokio::test]
    async fn load_lets_the_environment_override_the_document() {
        // The document says compliance is ON and custom emoji are OFF; the environment says the
        // reverse of each. Both directions, so an overlay that only ever forced `true` — or only
        // ever forced `false` — is still visible.
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"EnableCustomEmoji":false}}"#
                .to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_COMPLIANCESETTINGS_ENABLE" => Some("false".to_owned()),
            "MM_SERVICESETTINGS_ENABLECUSTOMEMOJI" => Some("true".to_owned()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert!(
            !config.compliance_enable,
            "the environment must win over the document, and it must be able to turn a setting off"
        );
        assert!(
            config.enable_custom_emoji,
            "and to turn one on — an overlay applied in the wrong order would leave this false"
        );
    }

    /// The environment does not reach settings it does not name, even when it names others. Guards
    /// against an overlay that resets the whole config to defaults whenever any variable is set.
    #[tokio::test]
    async fn the_environment_only_moves_what_it_names() {
        let store = FakeStore(Some(
            r#"{"ComplianceSettings":{"Enable":true},"ServiceSettings":{"PostPriority":false}}"#
                .to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_COMPLIANCESETTINGS_ENABLE" => Some("false".to_owned()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert!(!config.compliance_enable, "named, so it moved");
        assert!(
            !config.post_priority,
            "not named, so the document's value survives — not the default of true"
        );
    }

    /// The string setting takes the same route, and an override of `""` is a deliberate empty
    /// driver rather than an unparseable value to fall back from.
    #[tokio::test]
    async fn an_empty_string_override_survives_as_one() {
        let store = FakeStore(Some(
            r#"{"FileSettings":{"DriverName":"amazons3"}}"#.to_owned(),
        ));
        let env = |key: &str| match key {
            "MM_FILESETTINGS_DRIVERNAME" => Some(String::new()),
            _ => None,
        };

        let config = Config::load_with_env(&store, &env).await.expect("loads");

        assert_eq!(
            config.file_driver_name, "",
            "an empty driver is what the emoji reads 403 on; it must not fall back to the document"
        );
    }

    /// The overlay layers over the document rather than replacing it: a field the environment does
    /// not name keeps the document's value. Exercised with an environment that sets nothing, which
    /// is the only way to read the environment without racing every other test in the binary.
    #[test]
    fn the_env_overlay_preserves_document_values_it_does_not_name() {
        let from_doc = Config::from_document(r#"{"ComplianceSettings":{"Enable":true}}"#)
            .expect("valid document");
        let overlaid = from_doc.clone().apply_env();
        assert_eq!(
            overlaid, from_doc,
            "no MM_ variable is set in the test environment, so nothing may move"
        );
        assert!(
            overlaid.compliance_enable,
            "and the document's value survives"
        );
    }
}
