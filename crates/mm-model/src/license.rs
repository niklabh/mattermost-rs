//! Port of `model/license.go` — the enterprise licence, its feature flags, and the tier ladder.
//!
//! # `entry` outranks `enterprise`
//!
//! `EntryTier` and `EnterpriseAdvancedTier` are **both 30**, and `LicenseToLicenseTier` maps
//! `entry` to 30. So `MinimumEnterpriseLicense` and `MinimumEnterpriseAdvancedLicense` both
//! return true for a Mattermost Entry licence, which is the cheapest SKU in the list. That reads
//! like a bug and may well be one — it is reproduced exactly, because the gate it controls is
//! server behaviour a client can observe.
//!
//! # The trial windows are exact equalities, not ranges
//!
//! `IsTrialLicense` compares `ExpiresAt - StartsAt` to two **exact** durations. A licence one
//! millisecond off either is not a trial. And the "sanctioned trial" bounds are named backwards:
//! `sanctionedTrialDurationLowerBound` (31d 23:59:59) is *larger* than
//! `sanctionedTrialDurationUpperBound` (29d 23:59:59), and the test is
//! `duration >= lower || duration <= upper` — i.e. **outside** the ordinary trial band.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, StringInterface, get_millis, is_valid_id};

/// Port of `model.DayInSeconds` (license.go:14).
pub const DAY_IN_SECONDS: i64 = 24 * 60 * 60;
/// Port of `model.DayInMilliseconds` (license.go:15).
pub const DAY_IN_MILLISECONDS: i64 = DAY_IN_SECONDS * 1000;

pub const EXPIRED_LICENSE_ERROR: &str = "api.license.add_license.expired.app_error";
pub const INVALID_LICENSE_ERROR: &str = "api.license.add_license.invalid.app_error";
/// A production licence uploaded to a test or dev service environment.
pub const WRONG_ENVIRONMENT_PRODUCTION_LICENSE_ERROR: &str =
    "api.license.add_license.wrong_environment_production.app_error";
/// A test or dev licence uploaded to a production service environment.
pub const WRONG_ENVIRONMENT_TEST_LICENSE_ERROR: &str =
    "api.license.add_license.wrong_environment_test.app_error";
/// Port of `model.LicenseGracePeriod` (license.go:24) — 10 days past expiry.
pub const LICENSE_GRACE_PERIOD: i64 = DAY_IN_MILLISECONDS * 10;
pub const LICENSE_RENEWAL_LINK: &str = "https://mattermost.com/renew/";

pub const LICENSE_SHORT_SKU_E10: &str = "E10";
pub const LICENSE_SHORT_SKU_E20: &str = "E20";
pub const LICENSE_SHORT_SKU_PROFESSIONAL: &str = "professional";
pub const LICENSE_SHORT_SKU_ENTERPRISE: &str = "enterprise";
/// The SKU string is **`advanced`**, not `enterprise_advanced`.
pub const LICENSE_SHORT_SKU_ENTERPRISE_ADVANCED: &str = "advanced";
pub const LICENSE_SHORT_SKU_MATTERMOST_ENTRY: &str = "entry";

pub const PROFESSIONAL_TIER: i64 = 10;
pub const ENTERPRISE_TIER: i64 = 20;
/// **Equal to [`ENTERPRISE_ADVANCED_TIER`]** — see the module docs.
pub const ENTRY_TIER: i64 = 30;
pub const ENTERPRISE_ADVANCED_TIER: i64 = 30;

/// Port of `model.LicenseUpForRenewalEmailSent` (license.go:51) — a `Systems` key.
pub const LICENSE_UP_FOR_RENEWAL_EMAIL_SENT: &str = "LicenseUpForRenewalEmailSent";

/// Port of `model.LicenseToLicenseTier` (license.go:42).
///
/// A lookup miss is `0` in Go (the zero value of the map's `int`), which is below every tier — so
/// `E10`, `E20` and an empty SKU are all below professional by this measure, even though
/// `HasEnterpriseMarketplacePlugins` special-cases `E20`.
pub fn license_tier(sku_short_name: &str) -> i64 {
    match sku_short_name {
        LICENSE_SHORT_SKU_PROFESSIONAL => PROFESSIONAL_TIER,
        LICENSE_SHORT_SKU_ENTERPRISE => ENTERPRISE_TIER,
        LICENSE_SHORT_SKU_ENTERPRISE_ADVANCED => ENTERPRISE_ADVANCED_TIER,
        LICENSE_SHORT_SKU_MATTERMOST_ENTRY => ENTRY_TIER,
        _ => 0,
    }
}

const HOUR_MS: i64 = 60 * 60 * 1000;
const MINUTE_MS: i64 = 60 * 1000;
const SECOND_MS: i64 = 1000;

/// Port of `trialDuration` (license.go:55) — 30 days **plus 8 hours**.
pub const TRIAL_DURATION_MS: i64 = 30 * 24 * HOUR_MS + 8 * HOUR_MS;
/// Port of `adminTrialDuration` (license.go:56) — 30 days plus 23:59:59.
pub const ADMIN_TRIAL_DURATION_MS: i64 =
    30 * 24 * HOUR_MS + 23 * HOUR_MS + 59 * MINUTE_MS + 59 * SECOND_MS;
/// Port of `sanctionedTrialDurationLowerBound` (license.go:60). Despite the name this is the
/// **larger** of the two — see the module docs.
pub const SANCTIONED_TRIAL_DURATION_LOWER_BOUND_MS: i64 =
    31 * 24 * HOUR_MS + 23 * HOUR_MS + 59 * MINUTE_MS + 59 * SECOND_MS;
/// Port of `sanctionedTrialDurationUpperBound` (license.go:61) — the **smaller** one.
pub const SANCTIONED_TRIAL_DURATION_UPPER_BOUND_MS: i64 =
    29 * 24 * HOUR_MS + 23 * HOUR_MS + 59 * MINUTE_MS + 59 * SECOND_MS;

/// Port of `model.LicenseRecord` (license.go:64) — the stored row.
///
/// **`Bytes` carries `json:"-"`**: the signed licence blob is never returned to a client, yet
/// `IsValid` requires it. A record decoded from JSON therefore cannot be valid — the same shape
/// of trap as `FileInfo.path`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LicenseRecord {
    #[serde(rename = "id")]
    pub id: String,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    /// `json:"-"` — the signed blob.
    #[serde(skip)]
    pub bytes: String,
}

/// The maximum size of a stored licence blob, inline in Go's `IsValid`.
pub const LICENSE_RECORD_BYTES_MAX_LENGTH: usize = 10000;

impl LicenseRecord {
    /// Port of `(*LicenseRecord).IsValid` (license.go:479).
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(record_err("id"));
        }

        if self.create_at == 0 {
            return Err(record_err("create_at"));
        }

        if self.bytes.is_empty() || self.bytes.len() > LICENSE_RECORD_BYTES_MAX_LENGTH {
            return Err(record_err("bytes"));
        }

        Ok(())
    }

    /// Port of `(*LicenseRecord).PreSave` (license.go:495) — sets `create_at`
    /// **unconditionally**.
    pub fn pre_save(&mut self) {
        self.create_at = get_millis();
    }
}

fn record_err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "LicenseRecord.IsValid",
        format!("model.license_record.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.LicenseLimits` (license.go:70).
///
/// **`CallDurationSeconds` is tagged `call_duration`** — the unit is in the field name, not the
/// key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LicenseLimits {
    #[serde(rename = "post_history")]
    pub post_history: i64,

    #[serde(rename = "board_cards")]
    pub board_cards: i64,

    #[serde(rename = "playbook_runs")]
    pub playbook_runs: i64,

    #[serde(rename = "call_duration")]
    pub call_duration_seconds: i64,

    #[serde(rename = "agents_prompts")]
    pub agents_prompts: i64,

    #[serde(rename = "push_notifications")]
    pub push_notifications: i64,
}

/// Port of `model.Customer` (license.go:101).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Customer {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "company")]
    pub company: String,
}

/// Port of `model.License` (license.go:79).
///
/// Not one field carries `omitempty`, so every key is always present — the four pointers as
/// `null` when unset.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct License {
    #[serde(rename = "id")]
    pub id: String,

    /// Epoch milliseconds.
    #[serde(rename = "issued_at")]
    pub issued_at: i64,

    #[serde(rename = "starts_at")]
    pub starts_at: i64,

    #[serde(rename = "expires_at")]
    pub expires_at: i64,

    #[serde(rename = "customer")]
    pub customer: Option<Customer>,

    #[serde(rename = "features")]
    pub features: Option<Features>,

    #[serde(rename = "sku_name")]
    pub sku_name: String,

    /// The tier key — see [`license_tier`].
    #[serde(rename = "sku_short_name")]
    pub sku_short_name: String,

    #[serde(rename = "is_trial")]
    pub is_trial: bool,

    #[serde(rename = "is_gov_sku")]
    pub is_gov_sku: bool,

    #[serde(rename = "is_non_production")]
    pub is_non_production: bool,

    #[serde(rename = "is_seat_count_enforced")]
    pub is_seat_count_enforced: bool,

    /// Users allowed **beyond** the licensed count before creation is blocked; `null` means 0.
    #[serde(rename = "extra_users")]
    pub extra_users: Option<i64>,

    #[serde(rename = "signup_jwt")]
    pub signup_jwt: Option<String>,

    #[serde(rename = "limits")]
    pub limits: Option<LicenseLimits>,
}

impl License {
    /// Port of `(*License).IsMattermostEntry` (license.go:96).
    pub fn is_mattermost_entry(&self) -> bool {
        self.sku_short_name == LICENSE_SHORT_SKU_MATTERMOST_ENTRY
    }

    /// Port of `(*License).IsExpired` (license.go:356). Strictly `<`, so a licence expiring this
    /// exact millisecond is not yet expired.
    pub fn is_expired(&self) -> bool {
        self.expires_at < get_millis()
    }

    /// Port of `(*License).IsPastGracePeriod` (license.go:360).
    pub fn is_past_grace_period(&self) -> bool {
        get_millis() - self.expires_at > LICENSE_GRACE_PERIOD
    }

    /// Port of `(*License).IsWithinExpirationPeriod` (license.go:365) — the **three-day window**
    /// 58..=60 days out, when the renewal email is sent.
    pub fn is_within_expiration_period(&self) -> bool {
        let days = self.days_to_expiration();
        (58..=60).contains(&days)
    }

    /// Port of `(*License).DaysToExpiration` (license.go:370).
    ///
    /// Go converts the millisecond difference to a `time.Duration`, takes `Hours()` — a
    /// `float64` — divides by 24 and truncates with `int(...)`. Truncation is **toward zero**, so
    /// an already-expired licence reports a negative, rounded-up number of days.
    pub fn days_to_expiration(&self) -> i64 {
        let dif = self.expires_at - get_millis();
        let hours = dif as f64 / HOUR_MS as f64;
        (hours / 24.0) as i64
    }

    /// Port of `(*License).IsStarted` (license.go:377).
    pub fn is_started(&self) -> bool {
        self.starts_at < get_millis()
    }

    /// Port of `(*License).IsCloud` (license.go:386).
    pub fn is_cloud(&self) -> bool {
        self.features
            .as_ref()
            .and_then(|f| f.cloud)
            .unwrap_or(false)
    }

    /// Port of `(*License).IsCloudPreview` (license.go:382) — a cloud trial whose window is
    /// **exactly** one hour.
    pub fn is_cloud_preview(&self) -> bool {
        self.is_cloud() && self.is_trial_license() && self.expires_at - self.starts_at == HOUR_MS
    }

    /// Port of `(*License).IsTrialLicense` (license.go:390).
    pub fn is_trial_license(&self) -> bool {
        let duration = self.expires_at - self.starts_at;
        self.is_trial || duration == TRIAL_DURATION_MS || duration == ADMIN_TRIAL_DURATION_MS
    }

    /// Port of `(*License).IsSanctionedTrial` (license.go:394) — a trial whose window falls
    /// *outside* the ordinary band. See the module docs on the bound names.
    pub fn is_sanctioned_trial(&self) -> bool {
        let duration = self.expires_at - self.starts_at;
        self.is_trial_license()
            && (duration >= SANCTIONED_TRIAL_DURATION_LOWER_BOUND_MS
                || duration <= SANCTIONED_TRIAL_DURATION_UPPER_BOUND_MS)
    }

    /// Port of `(*License).HasEnterpriseMarketplacePlugins` (license.go:401).
    ///
    /// **Go dereferences `l.Features.EnterprisePlugins` unguarded and panics** on a licence with
    /// no features or an unset flag. Here a missing flag is `false`, which is the safe direction
    /// and the only difference — `E20` and any professional-or-better licence still qualify.
    pub fn has_enterprise_marketplace_plugins(&self) -> bool {
        self.features
            .as_ref()
            .and_then(|f| f.enterprise_plugins)
            .unwrap_or(false)
            || self.sku_short_name == LICENSE_SHORT_SKU_E20
            || minimum_professional_license(Some(self))
    }

    /// Port of `(*License).HasRemoteClusterService` (license.go:407).
    ///
    /// Shared channels imply the remote-cluster service, checked first.
    pub fn has_remote_cluster_service(&self) -> bool {
        if self.has_shared_channels() {
            return true;
        }

        self.features
            .as_ref()
            .and_then(|f| f.remote_cluster_service)
            .unwrap_or(false)
            || minimum_professional_license(Some(self))
    }

    /// Port of `(*License).HasSharedChannels` (license.go:420).
    pub fn has_shared_channels(&self) -> bool {
        self.features
            .as_ref()
            .and_then(|f| f.shared_channels)
            .unwrap_or(false)
            || minimum_professional_license(Some(self))
    }
}

/// Port of `model.MinimumProfessionalLicense` (license.go:503).
pub fn minimum_professional_license(license: Option<&License>) -> bool {
    license.is_some_and(|l| license_tier(&l.sku_short_name) >= PROFESSIONAL_TIER)
}

/// Port of `model.MinimumEnterpriseLicense` (license.go:509). See the module docs: `entry`
/// satisfies this.
pub fn minimum_enterprise_license(license: Option<&License>) -> bool {
    license.is_some_and(|l| license_tier(&l.sku_short_name) >= ENTERPRISE_TIER)
}

/// Port of `model.MinimumEnterpriseAdvancedLicense` (license.go:514).
pub fn minimum_enterprise_advanced_license(license: Option<&License>) -> bool {
    license.is_some_and(|l| license_tier(&l.sku_short_name) >= ENTERPRISE_ADVANCED_TIER)
}

/// Port of `model.TrialLicenseRequest` (license.go:108).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrialLicenseRequest {
    #[serde(rename = "server_id")]
    pub server_id: String,

    #[serde(rename = "email")]
    pub email: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "site_url")]
    pub site_url: String,

    #[serde(rename = "site_name")]
    pub site_name: String,

    #[serde(rename = "users")]
    pub users: i64,

    #[serde(rename = "terms_accepted")]
    pub terms_accepted: bool,

    #[serde(rename = "receive_emails_accepted")]
    pub receive_emails_accepted: bool,

    #[serde(rename = "contact_name")]
    pub contact_name: String,

    #[serde(rename = "contact_email")]
    pub contact_email: String,

    #[serde(rename = "company_name")]
    pub company_name: String,

    #[serde(rename = "company_country")]
    pub company_country: String,

    #[serde(rename = "company_size")]
    pub company_size: String,

    #[serde(rename = "server_version")]
    pub server_version: String,
}

impl TrialLicenseRequest {
    /// Port of `(*TrialLicenseRequest).IsLegacy` (license.go:125) — none of the four newer fields
    /// set. `ContactEmail` is **not** one of the four, so setting it alone keeps the request
    /// legacy.
    pub fn is_legacy(&self) -> bool {
        self.company_country.is_empty()
            && self.company_name.is_empty()
            && self.company_size.is_empty()
            && self.contact_name.is_empty()
    }

    /// Port of `(*TrialLicenseRequest).IsValid` (license.go:129).
    ///
    /// Returns a plain `bool` — no error id, so a caller cannot tell *which* field failed.
    /// Applies to non-legacy requests only; a legacy one is checked elsewhere.
    pub fn is_valid(&self) -> bool {
        self.terms_accepted
            && !self.email.is_empty()
            && self.users > 0
            && !self.company_country.is_empty()
            && !self.company_name.is_empty()
            && !self.company_size.is_empty()
            && !self.contact_name.is_empty()
    }
}

/// Port of `model.Features` (license.go:159).
///
/// Every flag is a `*bool` with no `omitempty`: `null` means "not set", which
/// [`Features::set_defaults`] then resolves — mostly to `FutureFeatures`, but four flags have
/// their own default. A `Some(false)` and a `None` are therefore different documents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Features {
    #[serde(rename = "users")]
    pub users: Option<i64>,

    #[serde(rename = "ldap")]
    pub ldap: Option<bool>,

    #[serde(rename = "ldap_groups")]
    pub ldap_groups: Option<bool>,

    #[serde(rename = "mfa")]
    pub mfa: Option<bool>,

    #[serde(rename = "google_oauth")]
    pub google_oauth: Option<bool>,

    #[serde(rename = "office365_oauth")]
    pub office365_oauth: Option<bool>,

    #[serde(rename = "openid")]
    pub open_id: Option<bool>,

    #[serde(rename = "compliance")]
    pub compliance: Option<bool>,

    #[serde(rename = "cluster")]
    pub cluster: Option<bool>,

    #[serde(rename = "metrics")]
    pub metrics: Option<bool>,

    #[serde(rename = "mhpns")]
    pub mhpns: Option<bool>,

    #[serde(rename = "saml")]
    pub saml: Option<bool>,

    /// Tagged **`elastic_search`**, two words, while the field is `Elasticsearch`.
    #[serde(rename = "elastic_search")]
    pub elasticsearch: Option<bool>,

    #[serde(rename = "announcement")]
    pub announcement: Option<bool>,

    #[serde(rename = "theme_management")]
    pub theme_management: Option<bool>,

    #[serde(rename = "email_notification_contents")]
    pub email_notification_contents: Option<bool>,

    #[serde(rename = "data_retention")]
    pub data_retention: Option<bool>,

    #[serde(rename = "message_export")]
    pub message_export: Option<bool>,

    #[serde(rename = "custom_permissions_schemes")]
    pub custom_permissions_schemes: Option<bool>,

    #[serde(rename = "custom_terms_of_service")]
    pub custom_terms_of_service: Option<bool>,

    #[serde(rename = "guest_accounts")]
    pub guest_accounts: Option<bool>,

    #[serde(rename = "guest_accounts_permissions")]
    pub guest_accounts_permissions: Option<bool>,

    /// Tagged **`id_loaded`**, while the field is `IDLoadedPushNotifications`.
    #[serde(rename = "id_loaded")]
    pub id_loaded_push_notifications: Option<bool>,

    #[serde(rename = "lock_teammate_name_display")]
    pub lock_teammate_name_display: Option<bool>,

    #[serde(rename = "enterprise_plugins")]
    pub enterprise_plugins: Option<bool>,

    #[serde(rename = "advanced_logging")]
    pub advanced_logging: Option<bool>,

    #[serde(rename = "cloud")]
    pub cloud: Option<bool>,

    #[serde(rename = "shared_channels")]
    pub shared_channels: Option<bool>,

    #[serde(rename = "remote_cluster_service")]
    pub remote_cluster_service: Option<bool>,

    #[serde(rename = "outgoing_oauth_connections")]
    pub outgoing_oauth_connections: Option<bool>,

    #[serde(rename = "auto_translation")]
    pub auto_translation: Option<bool>,

    /// The fallback every other unset flag takes — see [`Features::set_defaults`].
    #[serde(rename = "future_features")]
    pub future_features: Option<bool>,
}

impl Features {
    /// Port of `(*Features).SetDefaults` (license.go:227).
    ///
    /// `FutureFeatures` is resolved **first** and defaults to `true`; every flag below then
    /// inherits it, except for the five that do not:
    ///
    /// - `Users` defaults to `0` (it is an `*int`, not a flag);
    /// - `Announcement` and `ThemeManagement` default to **`true`** regardless;
    /// - `Cloud` defaults to **`false`** regardless;
    /// - `AutoTranslation` is **never defaulted at all** — Go's `SetDefaults` has no branch for
    ///   it, so it stays `nil` and every reader must treat that as off.
    pub fn set_defaults(&mut self) {
        if self.future_features.is_none() {
            self.future_features = Some(true);
        }
        let future = self.future_features.unwrap_or(true);

        if self.users.is_none() {
            self.users = Some(0);
        }

        for flag in [
            &mut self.ldap,
            &mut self.ldap_groups,
            &mut self.mfa,
            &mut self.google_oauth,
            &mut self.office365_oauth,
            &mut self.open_id,
            &mut self.compliance,
            &mut self.cluster,
            &mut self.metrics,
            &mut self.mhpns,
            &mut self.saml,
            &mut self.elasticsearch,
            &mut self.email_notification_contents,
            &mut self.data_retention,
            &mut self.message_export,
            &mut self.custom_permissions_schemes,
            &mut self.guest_accounts,
            &mut self.guest_accounts_permissions,
            &mut self.custom_terms_of_service,
            &mut self.id_loaded_push_notifications,
            &mut self.lock_teammate_name_display,
            &mut self.enterprise_plugins,
            &mut self.advanced_logging,
            &mut self.shared_channels,
            &mut self.remote_cluster_service,
            &mut self.outgoing_oauth_connections,
        ] {
            if flag.is_none() {
                *flag = Some(future);
            }
        }

        if self.announcement.is_none() {
            self.announcement = Some(true);
        }

        if self.theme_management.is_none() {
            self.theme_management = Some(true);
        }

        if self.cloud.is_none() {
            self.cloud = Some(false);
        }
    }

    /// Port of `(*Features).ToMap` (license.go:196) — the shape sent to clients in the client
    /// config.
    ///
    /// **Its keys are not the `json:` tags.** `google_oauth` becomes `google`,
    /// `office365_oauth` becomes `office365`, and `future_features` becomes `future`. Five
    /// tagged fields are missing entirely: `users`, `announcement`, `theme_management`,
    /// `custom_terms_of_service` and `auto_translation`.
    ///
    /// **Divergence:** Go dereferences all 26 pointers unguarded, so calling this before
    /// `SetDefaults` panics. Library code here may not panic, so an unset flag is `false` — the
    /// same value `SetDefaults` would most often have produced, and the safe direction.
    pub fn to_map(&self) -> StringInterface {
        let mut map = StringInterface::new();
        let mut put = |key: &str, value: Option<bool>| {
            map.insert(
                key.to_string(),
                serde_json::Value::Bool(value.unwrap_or(false)),
            );
        };

        put("ldap", self.ldap);
        put("ldap_groups", self.ldap_groups);
        put("mfa", self.mfa);
        put("google", self.google_oauth);
        put("office365", self.office365_oauth);
        put("openid", self.open_id);
        put("compliance", self.compliance);
        put("cluster", self.cluster);
        put("metrics", self.metrics);
        put("mhpns", self.mhpns);
        put("saml", self.saml);
        put("elastic_search", self.elasticsearch);
        put(
            "email_notification_contents",
            self.email_notification_contents,
        );
        put("data_retention", self.data_retention);
        put("message_export", self.message_export);
        put(
            "custom_permissions_schemes",
            self.custom_permissions_schemes,
        );
        put("guest_accounts", self.guest_accounts);
        put(
            "guest_accounts_permissions",
            self.guest_accounts_permissions,
        );
        put("id_loaded", self.id_loaded_push_notifications);
        put(
            "lock_teammate_name_display",
            self.lock_teammate_name_display,
        );
        put("enterprise_plugins", self.enterprise_plugins);
        put("advanced_logging", self.advanced_logging);
        put("cloud", self.cloud);
        put("shared_channels", self.shared_channels);
        put("remote_cluster_service", self.remote_cluster_service);
        put("future", self.future_features);
        put(
            "outgoing_oauth_connections",
            self.outgoing_oauth_connections,
        );

        map
    }
}

/// Port of `model.NewTestLicense` (license.go:437) — expires in 90 days, defaults applied, then
/// the named features forced **on**.
///
/// The feature names are the `json:` tags, not the Go field names, because Go applies them by
/// marshalling a `map[string]bool` and unmarshalling it over the struct. That is reproduced here
/// rather than mapped by hand, so an unknown name is silently ignored exactly as it is in Go.
pub fn new_test_license(features: &[&str]) -> License {
    let mut license = License {
        expires_at: get_millis() + 90 * DAY_IN_MILLISECONDS,
        customer: Some(Customer {
            id: "some ID".to_string(),
            email: "admin@example.com".to_string(),
            name: "Main Contact Person".to_string(),
            company: "My awesome Company".to_string(),
        }),
        features: Some(Features::default()),
        ..Default::default()
    };

    if let Some(f) = license.features.as_mut() {
        f.set_defaults();
        apply_feature_overrides(f, features, true);
    }

    license
}

/// Port of `model.NewTestLicenseWithFalseDefaults` (license.go:461) — as [`new_test_license`],
/// but the named features are forced **off** and the customer is empty.
pub fn new_test_license_with_false_defaults(features: &[&str]) -> License {
    let mut license = License {
        expires_at: get_millis() + 90 * DAY_IN_MILLISECONDS,
        customer: Some(Customer::default()),
        features: Some(Features::default()),
        ..Default::default()
    };

    if let Some(f) = license.features.as_mut() {
        f.set_defaults();
        apply_feature_overrides(f, features, false);
    }

    license
}

/// Port of `model.NewTestLicenseSKU` (license.go:474).
pub fn new_test_license_sku(sku_short_name: &str, features: &[&str]) -> License {
    let mut license = new_test_license(features);
    license.sku_short_name = sku_short_name.to_string();
    license
}

/// Go's `json.Marshal(map) + json.Unmarshal(over struct)` overlay, as one step.
///
/// A name that matches no tag changes nothing, and a failed round-trip leaves the features
/// untouched — Go ignores both errors too.
fn apply_feature_overrides(features: &mut Features, names: &[&str], value: bool) {
    let Ok(serde_json::Value::Object(mut map)) = serde_json::to_value(&*features) else {
        return;
    };
    for name in names {
        map.insert((*name).to_string(), serde_json::Value::Bool(value));
    }
    if let Ok(updated) = serde_json::from_value::<Features>(serde_json::Value::Object(map)) {
        *features = updated;
    }
}

#[cfg(test)]
mod wire_parity {
    use super::*;

    /// Round-trips the Go-generated fixture: decode into the port's type, re-encode, and compare
    /// the value graphs. The fixture is produced by `reference/dump`, whose reflective filler
    /// gives **every** field a distinctive non-zero value — so a dropped key, a renamed tag or a
    /// mis-typed field cannot pass. This is the parity oracle, not a smoke test.
    macro_rules! assert_fixture_round_trips {
        ($ty:ty, $fixture:literal) => {{
            let raw = include_str!(concat!("../../../fixtures/", $fixture, ".json"));
            let decoded: $ty =
                serde_json::from_str(raw).unwrap_or_else(|e| panic!("decoding {}: {e}", $fixture));
            let expected: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(
                serde_json::to_value(&decoded).unwrap(),
                expected,
                "re-encoding {} does not match Go",
                $fixture
            );
        }};
    }

    #[test]
    fn license_record_round_trips_the_fixture() {
        assert_fixture_round_trips!(LicenseRecord, "license_record");
    }
    #[test]
    fn license_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(LicenseLimits, "license_limits");
    }
    #[test]
    fn license_round_trips_the_fixture() {
        assert_fixture_round_trips!(License, "license");
    }
    #[test]
    fn customer_round_trips_the_fixture() {
        assert_fixture_round_trips!(Customer, "customer");
    }
    #[test]
    fn trial_license_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(TrialLicenseRequest, "trial_license_request");
    }
    #[test]
    fn features_round_trips_the_fixture() {
        assert_fixture_round_trips!(Features, "features");
    }
}

#[cfg(test)]
mod features_go_parity {
    use super::*;

    fn oracle() -> serde_json::Value {
        serde_json::from_str(include_str!(
            "../../../fixtures/behaviour_sweep_models.json"
        ))
        .expect("behaviour_sweep_models.json is generated by reference/dump")
    }

    /// `Features.SetDefaults` fills every `*bool` from `FutureFeatures` — **except** the handful
    /// it pins regardless. Asserting the serialized struct whole is what makes those exceptions
    /// visible: with `future_features` flipped to false, anything still true is one of them.
    #[test]
    fn set_defaults_and_to_map_match_go() {
        let oracle = oracle();
        let corpus = &oracle["license_features"];

        let mut f = Features::default();
        f.set_defaults();
        assert_eq!(
            serde_json::to_value(&f).unwrap(),
            corpus["defaults"],
            "Features after SetDefaults"
        );
        assert_eq!(
            serde_json::to_value(f.to_map()).unwrap(),
            corpus["defaults_map"],
            "ToMap after SetDefaults"
        );

        let mut off = Features {
            future_features: Some(false),
            ..Default::default()
        };
        off.set_defaults();
        assert_eq!(
            serde_json::to_value(&off).unwrap(),
            corpus["future_false"],
            "Features with FutureFeatures=false"
        );
        assert_eq!(
            serde_json::to_value(off.to_map()).unwrap(),
            corpus["future_false_map"],
            "ToMap with FutureFeatures=false"
        );

        // `SetDefaults` leaves this one alone entirely, so it is still `None` afterwards and
        // `ToMap` has no key for it.
        assert_eq!(
            f.auto_translation.is_none(),
            corpus["auto_translation_unset"].as_bool().unwrap()
        );
    }
}
