//! Port of `model/notify_admin.go` — "ask an admin to upgrade" requests.

use serde::{Deserialize, Serialize};

use crate::license::{LICENSE_SHORT_SKU_ENTERPRISE, LICENSE_SHORT_SKU_PROFESSIONAL};
use crate::serde_helpers::is_zero_i64;
use crate::utils::{AppError, AppResult, get_millis};

/// Port of `model.MattermostFeature` (notify_admin.go:12) — a `string` newtype whose values are
/// dotted feature paths.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MattermostFeature(pub String);

impl MattermostFeature {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for MattermostFeature {
    fn from(s: &str) -> Self {
        MattermostFeature(s.to_string())
    }
}

impl std::fmt::Display for MattermostFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub const PAID_FEATURE_GUEST_ACCOUNTS: &str = "mattermost.feature.guest_accounts";
/// The constant is `PaidFeatureCustomUsergroups`; the value says `custom_user_groups`.
pub const PAID_FEATURE_CUSTOM_USERGROUPS: &str = "mattermost.feature.custom_user_groups";
pub const PAID_FEATURE_CREATE_MULTIPLE_TEAMS: &str = "mattermost.feature.create_multiple_teams";
pub const PAID_FEATURE_STARTCALL: &str = "mattermost.feature.start_call";
/// The value abbreviates to `playbooks_retro`.
pub const PAID_FEATURE_PLAYBOOKS_RETROSPECTIVE: &str = "mattermost.feature.playbooks_retro";
pub const PAID_FEATURE_UNLIMITED_MESSAGES: &str = "mattermost.feature.unlimited_messages";
pub const PAID_FEATURE_UNLIMITED_FILE_STORAGE: &str = "mattermost.feature.unlimited_file_storage";
pub const PAID_FEATURE_ALL_PROFESSIONAL_FEATURES: &str = "mattermost.feature.all_professional";
pub const PAID_FEATURE_ALL_ENTERPRISE_FEATURES: &str = "mattermost.feature.all_enterprise";
pub const UPGRADE_DOWNGRADED_WORKSPACE: &str = "mattermost.feature.upgrade_downgraded_workspace";
/// The **prefix** for plugin features, not a feature in its own right — see
/// [`NotifyAdminData::is_valid`].
pub const PLUGIN_FEATURE: &str = "mattermost.feature.plugin";
pub const PAID_FEATURE_HIGHLIGHT_WITHOUT_NOTIFICATION: &str =
    "mattermost.feature.highlight_without_notification";

/// Port of `validSKUs` (notify_admin.go:29) — only two plans may be requested.
pub const VALID_SKUS: [&str; 2] = [LICENSE_SHORT_SKU_PROFESSIONAL, LICENSE_SHORT_SKU_ENTERPRISE];

/// Port of `paidFeatures` (notify_admin.go:35) — the eleven features a non-admin may ping about.
///
/// **[`PLUGIN_FEATURE`] is deliberately not in this set**: a plugin feature is matched by prefix
/// instead, which is why `IsValid` returns early for it.
pub const PAID_FEATURES: [&str; 11] = [
    PAID_FEATURE_GUEST_ACCOUNTS,
    PAID_FEATURE_CUSTOM_USERGROUPS,
    PAID_FEATURE_CREATE_MULTIPLE_TEAMS,
    PAID_FEATURE_STARTCALL,
    PAID_FEATURE_PLAYBOOKS_RETROSPECTIVE,
    PAID_FEATURE_UNLIMITED_MESSAGES,
    PAID_FEATURE_UNLIMITED_FILE_STORAGE,
    PAID_FEATURE_ALL_PROFESSIONAL_FEATURES,
    PAID_FEATURE_ALL_ENTERPRISE_FEATURES,
    UPGRADE_DOWNGRADED_WORKSPACE,
    PAID_FEATURE_HIGHLIGHT_WITHOUT_NOTIFICATION,
];

/// Port of `model.NotifyAdminToUpgradeRequest` (notify_admin.go:49).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyAdminToUpgradeRequest {
    #[serde(rename = "trial_notification")]
    pub trial_notification: bool,

    #[serde(rename = "required_plan")]
    pub required_plan: String,

    #[serde(rename = "required_feature")]
    pub required_feature: MattermostFeature,
}

/// Port of `model.NotifyAdminData` (notify_admin.go:55).
///
/// `SentAt` is a `sql.NullInt64`, which `encoding/json` marshals as the **struct**
/// `{"Int64":…,"Valid":…}` — capitalised keys and no tags, because `NullInt64` has none. That is
/// reproduced by [`NullInt64`]; a bare `Option<i64>` would emit a number or `null` instead and be
/// a wire break.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyAdminData {
    /// Epoch milliseconds. The only field with `omitempty`.
    #[serde(rename = "create_at", skip_serializing_if = "is_zero_i64")]
    pub create_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "required_plan")]
    pub required_plan: String,

    #[serde(rename = "required_feature")]
    pub required_feature: MattermostFeature,

    #[serde(rename = "trial")]
    pub trial: bool,

    #[serde(rename = "sent_at")]
    pub sent_at: NullInt64,
}

/// Go's `database/sql.NullInt64` on the wire: an object with `Int64` and `Valid`, both
/// capitalised, because the type carries no `json:` tags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NullInt64 {
    #[serde(rename = "Int64")]
    pub int64: i64,

    #[serde(rename = "Valid")]
    pub valid: bool,
}

impl NotifyAdminData {
    /// Port of `(*NotifyAdminData).IsValid` (notify_admin.go:64).
    ///
    /// A feature whose name **starts with** [`PLUGIN_FEATURE`] short-circuits to valid: plugin
    /// features are open-ended, so neither the plan nor the feature is checked for them.
    ///
    /// The two error messages are passed to `NewAppError` as the **id**, already interpolated —
    /// so they are never translated and appear verbatim in the response body.
    pub fn is_valid(&self) -> AppResult {
        if self.required_feature.as_str().starts_with(PLUGIN_FEATURE) {
            return Ok(());
        }

        if !VALID_SKUS.contains(&self.required_plan.as_str()) {
            return Err(err(format!(
                "Invalid plan, {} provided",
                self.required_plan
            )));
        }

        if !PAID_FEATURES.contains(&self.required_feature.as_str()) {
            return Err(err(format!(
                "Invalid feature, {} provided",
                self.required_feature
            )));
        }

        Ok(())
    }

    /// Port of `(*NotifyAdminData).PreSave` (notify_admin.go:78) — sets `create_at`
    /// unconditionally.
    pub fn pre_save(&mut self) {
        self.create_at = get_millis();
    }
}

fn err(message: String) -> Box<AppError> {
    Box::new(AppError::new("NotifyAdmin.IsValid", message, None, "", 400))
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
    fn notify_admin_to_upgrade_request_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            NotifyAdminToUpgradeRequest,
            "notify_admin_to_upgrade_request"
        );
    }
    #[test]
    fn notify_admin_data_round_trips_the_fixture() {
        assert_fixture_round_trips!(NotifyAdminData, "notify_admin_data");
    }
}
