//! Port of `model/system.go` — the `Systems` key/value table and the odds and ends that hang off
//! it.
//!
//! # `SystemPostActionCookieSecret.Secret` is tagged `key`
//!
//! Not `secret`. The struct is stored as the JSON *value* of the `PostActionCookieSecret` row, so
//! renaming the tag would orphan every existing installation's cookie secret.
//!
//! # The ECDSA key is three `*big.Int`s
//!
//! Go marshals `big.Int` as an unquoted JSON **number** of arbitrary precision. `serde_json`
//! without `arbitrary_precision` parses any integer wider than 64 bits into an `f64` and loses
//! digits — which for a P-256 private scalar means the key no longer round-trips. So the three
//! components are held as their exact decimal digits and moved through
//! [`serde_json::value::RawValue`], which copies the token verbatim in both directions. This is
//! the one field in this file where a "tidier" type is a correctness bug.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::{is_empty_str, is_none};

pub const SYSTEM_SERVER_ID: &str = "DiagnosticId";
pub const SYSTEM_RAN_UNIT_TESTS: &str = "RanUnitTests";
pub const SYSTEM_LAST_SECURITY_TIME: &str = "LastSecurityTime";
pub const SYSTEM_ACTIVE_LICENSE_ID: &str = "ActiveLicenseId";
pub const SYSTEM_LAST_COMPLIANCE_TIME: &str = "LastComplianceTime";
pub const SYSTEM_ASYMMETRIC_SIGNING_KEY_KEY: &str = "AsymmetricSigningKey";
pub const SYSTEM_POST_ACTION_COOKIE_SECRET_KEY: &str = "PostActionCookieSecret";
pub const SYSTEM_INSTALLATION_DATE_KEY: &str = "InstallationDate";
pub const SYSTEM_ORGANIZATION_NAME: &str = "OrganizationName";
pub const SYSTEM_FIRST_ADMIN_ROLE: &str = "FirstAdminRole";
pub const SYSTEM_FIRST_SERVER_RUN_TIMESTAMP_KEY: &str = "FirstServerRunTimestamp";
pub const SYSTEM_CLUSTER_ENCRYPTION_KEY: &str = "ClusterEncryptionKey";
pub const SYSTEM_PUSH_PROXY_AUTH_TOKEN: &str = "PushProxyAuthToken";
/// The value is `UpgradedFromTE`, not `UpgradedFromTeId`.
pub const SYSTEM_UPGRADED_FROM_TE_ID: &str = "UpgradedFromTE";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_TEAMS_5: &str = "warn_metric_number_of_teams_5";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_CHANNELS_50: &str = "warn_metric_number_of_channels_50";
pub const SYSTEM_WARN_METRIC_MFA: &str = "warn_metric_mfa";
pub const SYSTEM_WARN_METRIC_EMAIL_DOMAIN: &str = "warn_metric_email_domain";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_ACTIVE_USERS_100: &str =
    "warn_metric_number_of_active_users_100";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_ACTIVE_USERS_200: &str =
    "warn_metric_number_of_active_users_200";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_ACTIVE_USERS_300: &str =
    "warn_metric_number_of_active_users_300";
pub const SYSTEM_WARN_METRIC_NUMBER_OF_ACTIVE_USERS_500: &str =
    "warn_metric_number_of_active_users_500";
/// **Capital `M`** — `warn_metric_number_of_posts_2M`, the only warn-metric value that is not
/// entirely lower-case.
pub const SYSTEM_WARN_METRIC_NUMBER_OF_POSTS_2M: &str = "warn_metric_number_of_posts_2M";
pub const SYSTEM_WARN_METRIC_LAST_RUN_TIMESTAMP_KEY: &str = "LastWarnMetricRunTimestamp";
pub const SYSTEM_FIRST_ADMIN_VISIT_MARKETPLACE: &str = "FirstAdminVisitMarketplace";
pub const SYSTEM_FIRST_ADMIN_SETUP_COMPLETE: &str = "FirstAdminSetupComplete";
pub const SYSTEM_LAST_ACCESSIBLE_POST_TIME: &str = "LastAccessiblePostTime";
pub const SYSTEM_LAST_ACCESSIBLE_FILE_TIME: &str = "LastAccessibleFileTime";
pub const SYSTEM_HOSTED_PURCHASE_NEEDS_SCREENING: &str = "HostedPurchaseNeedsScreening";
pub const SYSTEM_POST_CHANNEL_TYPE_BACKFILL_COMPLETE: &str = "PostChannelTypeBackfillComplete";
pub const AWS_METERING_REPORT_INTERVAL: i64 = 1;
pub const AWS_METERING_DIMENSION_USAGE_HRS: &str = "UsageHrs";
pub const CLOUD_RENEWAL_EMAIL: &str = "CloudRenewalEmail";

/// `WarnMetricStatusLimitReached` is the **string** `"true"`, not a bool — the value is stored in
/// the `Systems` table, which is `map[string]string`.
pub const WARN_METRIC_STATUS_LIMIT_REACHED: &str = "true";
pub const WARN_METRIC_STATUS_RUNONCE: &str = "runonce";
pub const WARN_METRIC_STATUS_ACK: &str = "ack";
pub const WARN_METRIC_STATUS_STORE_PREFIX: &str = "warn_metric_";
/// Hours — `24 * 7`, not milliseconds, unlike [`WARN_METRIC_JOB_WAIT_TIME`] beside it.
pub const WARN_METRIC_JOB_INTERVAL: i64 = 24 * 7;
pub const WARN_METRIC_NUMBER_OF_ACTIVE_USERS_25: i64 = 25;
/// Milliseconds — 7 days.
pub const WARN_METRIC_JOB_WAIT_TIME: i64 = 1000 * 3600 * 24 * 7;

/// Port of `model.System` (system.go:53) — one row of the `Systems` table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct System {
    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "value")]
    pub value: String,
}

/// Port of `model.SystemPostActionCookieSecret` (system.go:58).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemPostActionCookieSecret {
    /// Tagged **`key`** — see the module docs. base64 on the wire.
    #[serde(
        rename = "key",
        with = "crate::go_bytes",
        skip_serializing_if = "is_none"
    )]
    pub secret: Option<Vec<u8>>,
}

/// Port of `model.SystemAsymmetricSigningKey` (system.go:62).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemAsymmetricSigningKey {
    #[serde(rename = "ecdsa_key", skip_serializing_if = "is_none")]
    pub ecdsa_key: Option<SystemECDSAKey>,
}

/// Port of `model.SystemECDSAKey` (system.go:66).
///
/// `x` and `y` have no `omitempty` and so are always written — as `null` when absent, which is
/// what a nil `*big.Int` marshals to. `d` (the private scalar) has `omitempty`, which is how the
/// public half of the key is published without a separate type.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemECDSAKey {
    #[serde(rename = "curve")]
    pub curve: String,

    /// Exact decimal digits of a `*big.Int`; see the module docs.
    #[serde(rename = "x", with = "go_big_int")]
    pub x: Option<String>,

    #[serde(rename = "y", with = "go_big_int")]
    pub y: Option<String>,

    /// The private scalar. `omitempty`.
    #[serde(rename = "d", with = "go_big_int", skip_serializing_if = "is_none")]
    pub d: Option<String>,
}

/// Go's `*big.Int` as an unquoted, arbitrary-precision JSON number, carried as its exact decimal
/// digits so no `f64` ever touches it.
mod go_big_int {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use serde_json::value::RawValue;

    pub fn serialize<S: Serializer>(value: &Option<String>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(digits) => RawValue::from_string(digits.clone())
                .map_err(serde::ser::Error::custom)?
                .serialize(s),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
        let raw = Option::<Box<RawValue>>::deserialize(d)?;
        Ok(raw.map(|r| r.get().to_string()))
    }
}

/// Port of `model.ServerBusyState` (system.go:74).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerBusyState {
    #[serde(rename = "busy")]
    pub busy: bool,

    /// Epoch **seconds** here, not milliseconds — it is set from `time.Time.Unix()`.
    #[serde(rename = "expires")]
    pub expires: i64,

    /// The same instant as a formatted string, for humans. `omitempty`.
    #[serde(rename = "expires_ts", skip_serializing_if = "is_empty_str")]
    pub expires_ts: String,
}

/// Port of `model.AppliedMigration` (system.go:80).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppliedMigration {
    #[serde(rename = "version")]
    pub version: i64,

    #[serde(rename = "name")]
    pub name: String,
}

/// Port of `model.LogFilter` (system.go:85).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogFilter {
    #[serde(rename = "server_names")]
    pub server_names: Option<Vec<String>>,

    #[serde(rename = "log_levels")]
    pub log_levels: Option<Vec<String>>,

    #[serde(rename = "date_from")]
    pub date_from: String,

    #[serde(rename = "date_to")]
    pub date_to: String,
}

/// Port of `model.LogEntry` (system.go:92). No `json:` tags — parsed out of a log line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
}

/// Port of `model.SystemPingOptions` (system.go:99).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemPingOptions {
    /// Include the detailed status breakdown.
    pub full_status: bool,
    /// Answer `200` even when the server is unhealthy. The Go field is `RESTSemantics` while its
    /// doc comment calls it `RestSemantics`.
    pub rest_semantics: bool,
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
    fn system_round_trips_the_fixture() {
        assert_fixture_round_trips!(System, "system");
    }
    #[test]
    fn system_post_action_cookie_secret_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            SystemPostActionCookieSecret,
            "system_post_action_cookie_secret"
        );
    }
    #[test]
    fn system_asymmetric_signing_key_round_trips_the_fixture() {
        assert_fixture_round_trips!(SystemAsymmetricSigningKey, "system_asymmetric_signing_key");
    }
    #[test]
    fn system_ecdsa_key_round_trips_the_fixture() {
        assert_fixture_round_trips!(SystemECDSAKey, "system_ecdsa_key");
    }
    #[test]
    fn server_busy_state_round_trips_the_fixture() {
        assert_fixture_round_trips!(ServerBusyState, "server_busy_state");
    }
    #[test]
    fn applied_migration_round_trips_the_fixture() {
        assert_fixture_round_trips!(AppliedMigration, "applied_migration");
    }
    #[test]
    fn log_filter_round_trips_the_fixture() {
        assert_fixture_round_trips!(LogFilter, "log_filter");
    }
}
