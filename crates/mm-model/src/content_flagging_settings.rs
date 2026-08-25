//! Port of `model/content_flagging_settings.go` — the admin configuration for content flagging.
//!
//! These are config structs: **no `json:` tags**, so the wire keys are the Go field names. See
//! `ai_recap_settings.rs` for the same rule stated at length. The one exception is
//! [`ContentFlaggingReportingConfig`], which *is* tagged — it is the client-facing projection.
//!
//! # Reviewers must always be notified when content is flagged
//!
//! `ContentFlaggingNotificationSettings::set_defaults` **appends** `reviewers` to the `flagged`
//! event if an existing mapping omits it, and `is_valid` rejects a mapping that lacks it. Go's
//! comment says the UI does not allow disabling it and the check exists "for safety and
//! consistency" — so it is a server-side invariant, not a UI convenience.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::serde_helpers::is_none;
use crate::utils::{AppError, AppResult};

/// Port of `model.ContentFlaggingEvent` (content_flagging_settings.go:9) — a `string` newtype.
///
/// Note the constant names and values diverge: `EventContentRemoved` is **`removed`** and
/// `EventContentDismissed` is **`dismissed`**, without the `content` prefix.
pub const EVENT_FLAGGED: &str = "flagged";
pub const EVENT_ASSIGNED: &str = "assigned";
pub const EVENT_CONTENT_REMOVED: &str = "removed";
pub const EVENT_CONTENT_DISMISSED: &str = "dismissed";

/// Port of `model.NotificationTarget` (content_flagging_settings.go:18).
pub const TARGET_REVIEWERS: &str = "reviewers";
pub const TARGET_AUTHOR: &str = "author";
pub const TARGET_REPORTER: &str = "reporter";

/// Port of `model.ContentFlaggingDefaultReasons` (content_flagging_settings.go:26).
///
/// Free-text sentences, not identifiers, and they are what a reporter picks from — so they are
/// user-visible strings that an admin may replace wholesale.
pub const CONTENT_FLAGGING_DEFAULT_REASONS: [&str; 7] = [
    "Classification mismatch",
    "Need-to-know violation",
    "Personally identifiable information (PII) exposure",
    "Operational security (OPSEC) concern",
    "Controlled Unclassified Information (CUI) violation",
    "Unauthorized disclosure",
    "Other",
];

fn default_reasons() -> Vec<String> {
    CONTENT_FLAGGING_DEFAULT_REASONS
        .iter()
        .map(|r| (*r).to_string())
        .collect()
}

/// Port of `model.ContentFlaggingNotificationSettings` (content_flagging_settings.go:36).
///
/// A `BTreeMap` rather than a `HashMap`, so the marshalled key order matches Go's — Go sorts map
/// keys when marshalling, and this map reaches the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentFlaggingNotificationSettings {
    #[serde(rename = "EventTargetMapping")]
    pub event_target_mapping: BTreeMap<String, Vec<String>>,
}

impl ContentFlaggingNotificationSettings {
    /// Port of `(*ContentFlaggingNotificationSettings).SetDefaults`
    /// (content_flagging_settings.go:40).
    ///
    /// `flagged` is the only event whose **existing** mapping is modified: `reviewers` is appended
    /// if missing. The other three are filled only when absent, so an admin may empty them.
    pub fn set_defaults(&mut self) {
        match self.event_target_mapping.get_mut(EVENT_FLAGGED) {
            None => {
                self.event_target_mapping.insert(
                    EVENT_FLAGGED.to_string(),
                    vec![TARGET_REVIEWERS.to_string()],
                );
            }
            Some(targets) => {
                if !targets.iter().any(|t| t == TARGET_REVIEWERS) {
                    targets.push(TARGET_REVIEWERS.to_string());
                }
            }
        }

        self.event_target_mapping
            .entry(EVENT_ASSIGNED.to_string())
            .or_insert_with(|| vec![TARGET_REVIEWERS.to_string()]);

        self.event_target_mapping
            .entry(EVENT_CONTENT_REMOVED.to_string())
            .or_insert_with(|| {
                vec![
                    TARGET_REVIEWERS.to_string(),
                    TARGET_AUTHOR.to_string(),
                    TARGET_REPORTER.to_string(),
                ]
            });

        self.event_target_mapping
            .entry(EVENT_CONTENT_DISMISSED.to_string())
            .or_insert_with(|| vec![TARGET_REVIEWERS.to_string(), TARGET_REPORTER.to_string()]);
    }

    /// Port of `(*ContentFlaggingNotificationSettings).IsValid`
    /// (content_flagging_settings.go:68).
    ///
    /// Unknown events and targets are rejected, and the `flagged` event must both be non-empty
    /// **and** contain `reviewers`. Only the invalid-target error carries details, as
    /// `target: <value>`.
    pub fn is_valid(&self) -> AppResult {
        for (event, targets) in &self.event_target_mapping {
            if !matches!(
                event.as_str(),
                EVENT_FLAGGED | EVENT_ASSIGNED | EVENT_CONTENT_REMOVED | EVENT_CONTENT_DISMISSED
            ) {
                return Err(notification_err("invalid_event", String::new()));
            }

            for target in targets {
                if !matches!(
                    target.as_str(),
                    TARGET_REVIEWERS | TARGET_AUTHOR | TARGET_REPORTER
                ) {
                    return Err(notification_err(
                        "invalid_target",
                        format!("target: {target}"),
                    ));
                }
            }
        }

        let flagged = self.event_target_mapping.get(EVENT_FLAGGED);
        let has_reviewers =
            flagged.is_some_and(|targets| targets.iter().any(|t| t == TARGET_REVIEWERS));

        if flagged.is_none_or(|targets| targets.is_empty()) || !has_reviewers {
            return Err(notification_err(
                "reviewer_flagged_notification_disabled",
                String::new(),
            ));
        }

        Ok(())
    }
}

/// These ids have **no `.app_error` suffix**, unlike the content-flagging ones below.
fn notification_err(suffix: &str, details: String) -> Box<AppError> {
    Box::new(AppError::new(
        "Config.IsValid",
        format!("model.config.is_valid.notification_settings.{suffix}"),
        None,
        details,
        400,
    ))
}

fn content_flagging_err(suffix: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Config.IsValid",
        format!("model.config.is_valid.content_flagging.{suffix}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.TeamReviewerSetting` (content_flagging_settings.go:97).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TeamReviewerSetting {
    #[serde(rename = "Enabled", skip_serializing_if = "is_none")]
    pub enabled: Option<bool>,

    #[serde(rename = "ReviewerIds")]
    pub reviewer_ids: Option<Vec<String>>,
}

/// Port of `model.ReviewerSettings` (content_flagging_settings.go:102).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewerSettings {
    /// Defaults to **true**.
    #[serde(rename = "CommonReviewers", skip_serializing_if = "is_none")]
    pub common_reviewers: Option<bool>,

    /// Defaults to **false** — the only one of the three that does.
    #[serde(rename = "SystemAdminsAsReviewers", skip_serializing_if = "is_none")]
    pub system_admins_as_reviewers: Option<bool>,

    /// Defaults to true.
    #[serde(rename = "TeamAdminsAsReviewers", skip_serializing_if = "is_none")]
    pub team_admins_as_reviewers: Option<bool>,
}

impl ReviewerSettings {
    /// Port of `(*ReviewerSettings).SetDefaults` (content_flagging_settings.go:108).
    pub fn set_defaults(&mut self) {
        if self.common_reviewers.is_none() {
            self.common_reviewers = Some(true);
        }
        if self.system_admins_as_reviewers.is_none() {
            self.system_admins_as_reviewers = Some(false);
        }
        if self.team_admins_as_reviewers.is_none() {
            self.team_admins_as_reviewers = Some(true);
        }
    }
}

/// Port of `model.ReviewerIDsSettings` (content_flagging_settings.go:213).
///
/// Split from [`ReviewerSettings`] because the **ids** are stored separately from the toggles:
/// `ReviewSettingsRequest` in `content_flagging_setting_request.rs` embeds both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewerIDsSettings {
    #[serde(rename = "CommonReviewerIds")]
    pub common_reviewer_ids: Option<Vec<String>>,

    /// Keyed by team id.
    #[serde(rename = "TeamReviewersSetting")]
    pub team_reviewers_setting: Option<BTreeMap<String, TeamReviewerSetting>>,
}

impl ReviewerIDsSettings {
    /// Port of `(*ReviewerIDsSettings).SetDefaults` (content_flagging_settings.go:218).
    ///
    /// Both defaults are **empty, non-nil** containers, which marshal as `[]` and `{}` rather
    /// than `null`.
    pub fn set_defaults(&mut self) {
        if self.common_reviewer_ids.is_none() {
            self.common_reviewer_ids = Some(Vec::new());
        }
        if self.team_reviewers_setting.is_none() {
            self.team_reviewers_setting = Some(BTreeMap::new());
        }
    }
}

/// Port of `model.AdditionalContentFlaggingSettings` (content_flagging_settings.go:120).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdditionalContentFlaggingSettings {
    /// A `*[]string` — pointer **to a slice**, so "unset" and "explicitly empty" are different,
    /// and `is_valid` rejects the empty one.
    #[serde(rename = "Reasons", skip_serializing_if = "is_none")]
    pub reasons: Option<Vec<String>>,

    #[serde(rename = "ReporterCommentRequired", skip_serializing_if = "is_none")]
    pub reporter_comment_required: Option<bool>,

    #[serde(rename = "ReviewerCommentRequired", skip_serializing_if = "is_none")]
    pub reviewer_comment_required: Option<bool>,

    #[serde(rename = "HideFlaggedContent", skip_serializing_if = "is_none")]
    pub hide_flagged_content: Option<bool>,
}

impl AdditionalContentFlaggingSettings {
    /// Port of `(*AdditionalContentFlaggingSettings).SetDefaults`
    /// (content_flagging_settings.go:127).
    ///
    /// **Go points `Reasons` at the package-level `ContentFlaggingDefaultReasons` slice**, so
    /// every server that takes the default shares one backing array and a mutation through one
    /// config would be visible in another. Here each config gets its own copy, which is the safe
    /// direction and observably identical.
    pub fn set_defaults(&mut self) {
        if self.reasons.is_none() {
            self.reasons = Some(default_reasons());
        }
        if self.reporter_comment_required.is_none() {
            self.reporter_comment_required = Some(true);
        }
        if self.reviewer_comment_required.is_none() {
            self.reviewer_comment_required = Some(true);
        }
        if self.hide_flagged_content.is_none() {
            self.hide_flagged_content = Some(true);
        }
    }

    /// Port of `(*AdditionalContentFlaggingSettings).IsValid`
    /// (content_flagging_settings.go:145).
    pub fn is_valid(&self) -> AppResult {
        if self.reasons.as_ref().is_none_or(|r| r.is_empty()) {
            return Err(content_flagging_err("reasons_not_set"));
        }

        Ok(())
    }
}

/// Port of `model.ContentFlaggingSettingsBase` (content_flagging_settings.go:153).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentFlaggingSettingsBase {
    /// Defaults to **false** — the feature is off until an admin turns it on.
    #[serde(rename = "EnableContentFlagging", skip_serializing_if = "is_none")]
    pub enable_content_flagging: Option<bool>,

    #[serde(rename = "NotificationSettings", skip_serializing_if = "is_none")]
    pub notification_settings: Option<ContentFlaggingNotificationSettings>,

    #[serde(rename = "AdditionalSettings", skip_serializing_if = "is_none")]
    pub additional_settings: Option<AdditionalContentFlaggingSettings>,
}

impl ContentFlaggingSettingsBase {
    /// Port of `(*ContentFlaggingSettingsBase).SetDefaults`
    /// (content_flagging_settings.go:159).
    pub fn set_defaults(&mut self) {
        if self.enable_content_flagging.is_none() {
            self.enable_content_flagging = Some(false);
        }

        let notification = self
            .notification_settings
            .get_or_insert_with(ContentFlaggingNotificationSettings::default);
        notification.set_defaults();

        let additional = self
            .additional_settings
            .get_or_insert_with(AdditionalContentFlaggingSettings::default);
        additional.set_defaults();
    }

    /// Port of `(*ContentFlaggingSettingsBase).IsValid` (content_flagging_settings.go:178).
    ///
    /// **Go dereferences both sub-settings unguarded**, so calling this before `SetDefaults`
    /// panics. Here an absent block passes — the safe direction, and unreachable after
    /// `set_defaults`.
    pub fn is_valid(&self) -> AppResult {
        if let Some(notification) = &self.notification_settings {
            notification.is_valid()?;
        }

        if let Some(additional) = &self.additional_settings {
            additional.is_valid()?;
        }

        Ok(())
    }
}

/// Port of `model.ContentFlaggingSettings` (content_flagging_settings.go:190) — the base plus the
/// reviewer toggles, as stored in `model.Config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentFlaggingSettings {
    #[serde(flatten)]
    pub base: ContentFlaggingSettingsBase,

    #[serde(rename = "ReviewerSettings", skip_serializing_if = "is_none")]
    pub reviewer_settings: Option<ReviewerSettings>,
}

impl ContentFlaggingSettings {
    /// Port of `(*ContentFlaggingSettings).SetDefaults` (content_flagging_settings.go:195).
    pub fn set_defaults(&mut self) {
        self.base.set_defaults();

        let reviewer = self
            .reviewer_settings
            .get_or_insert_with(ReviewerSettings::default);
        reviewer.set_defaults();
    }

    /// Port of `(*ContentFlaggingSettings).IsValid` (content_flagging_settings.go:205).
    ///
    /// **Delegates to the base only** — the reviewer settings are not validated here. That check
    /// lives on `ReviewSettingsRequest`, which is the *request* type, so a config written directly
    /// escapes it.
    pub fn is_valid(&self) -> AppResult {
        self.base.is_valid()
    }
}

/// Port of `model.ContentFlaggingReportingConfig` (content_flagging_settings.go:205) — the
/// client-facing projection, and **the only tagged type in this file**.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentFlaggingReportingConfig {
    #[serde(rename = "reasons")]
    pub reasons: Option<Vec<String>>,

    #[serde(rename = "reporter_comment_required")]
    pub reporter_comment_required: Option<bool>,

    #[serde(rename = "reviewer_comment_required")]
    pub reviewer_comment_required: Option<bool>,

    #[serde(
        rename = "notify_reporter_on_dismissal",
        skip_serializing_if = "is_none"
    )]
    pub notify_reporter_on_dismissal: Option<bool>,

    #[serde(rename = "notify_reporter_on_removal", skip_serializing_if = "is_none")]
    pub notify_reporter_on_removal: Option<bool>,
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
    fn content_flagging_reporting_config_round_trips_the_fixture() {
        assert_fixture_round_trips!(
            ContentFlaggingReportingConfig,
            "content_flagging_reporting_config"
        );
    }
}
