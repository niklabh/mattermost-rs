//! Port of `model/content_flagging_setting_request.go` — the System Console's write shape for
//! content-flagging settings.
//!
//! # This is where the reviewer invariants are actually enforced
//!
//! `ContentFlaggingSettings::IsValid` in `content_flagging_settings.rs` validates only the base.
//! The rule that a configuration must be able to *reach* a reviewer lives here, on the request
//! type — so a config written straight to disk escapes it, while one submitted through the API
//! does not.

use serde::{Deserialize, Serialize};

use crate::content_flagging_settings::{
    ContentFlaggingSettingsBase, ReviewerIDsSettings, ReviewerSettings,
};
use crate::serde_helpers::is_none;
use crate::utils::{AppError, AppResult};

/// Port of `model.ReviewSettingsRequest` (content_flagging_setting_request.go:7) — the toggles
/// and the ids together. Both embedded structs are **inlined** on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewSettingsRequest {
    #[serde(flatten)]
    pub reviewer_settings: ReviewerSettings,

    #[serde(flatten)]
    pub reviewer_ids_settings: ReviewerIDsSettings,
}

impl ReviewSettingsRequest {
    /// Port of `(*ReviewSettingsRequest).SetDefaults` (content_flagging_setting_request.go:12).
    pub fn set_defaults(&mut self) {
        self.reviewer_settings.set_defaults();
        self.reviewer_ids_settings.set_defaults();
    }

    /// Port of `(*ReviewSettingsRequest).IsValid` (content_flagging_setting_request.go:17).
    ///
    /// The rule, in Go's words: if **common reviewers** are enabled there must be at least one
    /// named reviewer *or* an "additional reviewer" toggle (system or team admins) must be on.
    /// And if no additional-reviewer toggle is on, every **enabled** team must name at least one
    /// reviewer — a team with the feature switched off may have an empty list, and a team with an
    /// empty list is fine as long as an additional-reviewer toggle covers it.
    ///
    /// **Go dereferences four pointers unguarded**, so calling this before `SetDefaults` panics.
    /// Here an unset toggle reads as `false` — the safe direction, and unreachable after
    /// `set_defaults`.
    ///
    /// Both errors report `Where` as **`Config.IsValid`**, not this type's name.
    pub fn is_valid(&self) -> AppResult {
        let system_admins = self
            .reviewer_settings
            .system_admins_as_reviewers
            .unwrap_or(false);
        let team_admins = self
            .reviewer_settings
            .team_admins_as_reviewers
            .unwrap_or(false);
        let additional_reviewers_enabled = system_admins || team_admins;

        let common_reviewers = self.reviewer_settings.common_reviewers.unwrap_or(false);
        let common_reviewer_ids_empty = self
            .reviewer_ids_settings
            .common_reviewer_ids
            .as_ref()
            .is_none_or(|ids| ids.is_empty());

        if common_reviewers && common_reviewer_ids_empty && !additional_reviewers_enabled {
            return Err(err("common_reviewers_not_set"));
        }

        if !additional_reviewers_enabled {
            for setting in self
                .reviewer_ids_settings
                .team_reviewers_setting
                .iter()
                .flat_map(|m| m.values())
            {
                let enabled = setting.enabled.unwrap_or(false);
                let ids_empty = setting
                    .reviewer_ids
                    .as_ref()
                    .is_none_or(|ids| ids.is_empty());
                if enabled && ids_empty {
                    return Err(err("team_reviewers_not_set"));
                }
            }
        }

        Ok(())
    }
}

fn err(suffix: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Config.IsValid",
        format!("model.config.is_valid.content_flagging.{suffix}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.ContentFlaggingSettingsRequest` (content_flagging_setting_request.go:40).
///
/// The same shape as `ContentFlaggingSettings` except that its reviewer block is the **request**
/// type, which carries the ids as well as the toggles.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContentFlaggingSettingsRequest {
    #[serde(flatten)]
    pub base: ContentFlaggingSettingsBase,

    #[serde(rename = "ReviewerSettings", skip_serializing_if = "is_none")]
    pub reviewer_settings: Option<ReviewSettingsRequest>,
}

impl ContentFlaggingSettingsRequest {
    /// Port of `(*ContentFlaggingSettingsRequest).SetDefaults`
    /// (content_flagging_setting_request.go:45).
    ///
    /// Note it calls the base's `SetDefaults` **and then repeats two of its steps** — filling
    /// `EnableContentFlagging` and `NotificationSettings` again, and calling
    /// `NotificationSettings.SetDefaults()` a second time. Both are idempotent, so the duplication
    /// is harmless; it is reproduced as a single pass here because the observable result is
    /// identical.
    pub fn set_defaults(&mut self) {
        self.base.set_defaults();

        let reviewer = self
            .reviewer_settings
            .get_or_insert_with(ReviewSettingsRequest::default);
        reviewer.set_defaults();
    }

    /// Port of `(*ContentFlaggingSettingsRequest).IsValid`
    /// (content_flagging_setting_request.go:74).
    ///
    /// Unlike `ContentFlaggingSettings::is_valid`, this one **does** validate the reviewer block.
    pub fn is_valid(&self) -> AppResult {
        self.base.is_valid()?;

        if let Some(reviewer) = &self.reviewer_settings {
            reviewer.is_valid()?;
        }

        Ok(())
    }
}
