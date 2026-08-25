//! Port of `model/ai_recap_settings.go` — the admin-configurable AI Recap limits.
//!
//! # These are config structs, so the wire keys are the **Go field names**
//!
//! Not one field in this file carries a `json:` tag. `model.Config` and every settings struct
//! under it are marshalled with `encoding/json`'s default, which is the exported field name
//! verbatim — `Enable`, `DefaultLimits`, `MaxRecapsPerDay`. Snake-casing them would break every
//! existing `config.json` and every System Console request. The `access:"ai_recaps"` tags drive
//! the System Console's permission model and have no JSON effect.
//!
//! # `-1` is unlimited, `0` is not
//!
//! Every limit accepts `-1` for "off" and otherwise requires ≥ 1 — so `0` is invalid for six of
//! the seven. `CooldownMinutes` is the exception: it requires ≥ 0, and `0` means no cooldown.

use serde::{Deserialize, Serialize};

use crate::serde_helpers::is_none;
use crate::utils::{AppError, AppResult};

/// Port of `model.RecapLimitSettings` (ai_recap_settings.go:10).
///
/// All-pointer, like every config settings struct: `None` means "not configured", which
/// [`RecapLimitSettings::set_defaults`] then fills in. A configured `-1` and an unset field are
/// different states.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecapLimitSettings {
    /// Default 10.
    #[serde(rename = "MaxRecapsPerDay", skip_serializing_if = "is_none")]
    pub max_recaps_per_day: Option<i64>,

    /// Default 5.
    #[serde(rename = "MaxScheduledRecaps", skip_serializing_if = "is_none")]
    pub max_scheduled_recaps: Option<i64>,

    /// Default **-1** — unlimited, unlike its neighbours.
    #[serde(rename = "MaxChannelsPerRecap", skip_serializing_if = "is_none")]
    pub max_channels_per_recap: Option<i64>,

    /// Default 500.
    #[serde(rename = "MaxPostsPerRecap", skip_serializing_if = "is_none")]
    pub max_posts_per_recap: Option<i64>,

    /// Default 100000.
    #[serde(rename = "MaxTokensPerRecap", skip_serializing_if = "is_none")]
    pub max_tokens_per_recap: Option<i64>,

    /// Default 5000.
    #[serde(rename = "MaxPostsPerDay", skip_serializing_if = "is_none")]
    pub max_posts_per_day: Option<i64>,

    /// Default 60. `0` means no cooldown.
    #[serde(rename = "CooldownMinutes", skip_serializing_if = "is_none")]
    pub cooldown_minutes: Option<i64>,
}

impl RecapLimitSettings {
    /// Port of `(*RecapLimitSettings).SetDefaults` (ai_recap_settings.go:21).
    pub fn set_defaults(&mut self) {
        if self.max_recaps_per_day.is_none() {
            self.max_recaps_per_day = Some(10);
        }
        if self.max_scheduled_recaps.is_none() {
            self.max_scheduled_recaps = Some(5);
        }
        if self.max_channels_per_recap.is_none() {
            self.max_channels_per_recap = Some(-1);
        }
        if self.max_posts_per_recap.is_none() {
            self.max_posts_per_recap = Some(500);
        }
        if self.max_tokens_per_recap.is_none() {
            self.max_tokens_per_recap = Some(100_000);
        }
        if self.max_posts_per_day.is_none() {
            self.max_posts_per_day = Some(5000);
        }
        if self.cooldown_minutes.is_none() {
            self.cooldown_minutes = Some(60);
        }
    }

    /// Port of `(*RecapLimitSettings).isValid` (ai_recap_settings.go:46).
    ///
    /// Unexported in Go and reached only through [`AIRecapSettings::is_valid`]; `pub(crate)` here
    /// for the same reason. Every error's `Where` is **`Config.IsValid`**, not this type's name.
    pub(crate) fn is_valid(&self) -> AppResult {
        for (value, field) in [
            (self.max_recaps_per_day, "max_recaps_per_day"),
            (self.max_scheduled_recaps, "max_scheduled_recaps"),
            (self.max_channels_per_recap, "max_channels_per_recap"),
            (self.max_posts_per_recap, "max_posts_per_recap"),
            (self.max_tokens_per_recap, "max_tokens_per_recap"),
            (self.max_posts_per_day, "max_posts_per_day"),
        ] {
            if let Some(value) = value {
                if value != -1 && value < 1 {
                    return Err(err(field));
                }
            }
        }

        // The one field with a different rule: `>= 0`, where zero means no cooldown.
        if let Some(cooldown) = self.cooldown_minutes {
            if cooldown < 0 {
                return Err(err("cooldown_minutes"));
            }
        }

        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Config.IsValid",
        format!("model.config.is_valid.ai_recap.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.AIRecapSettings` (ai_recap_settings.go:98).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AIRecapSettings {
    /// Master toggle; defaults to **true**.
    #[serde(rename = "Enable", skip_serializing_if = "is_none")]
    pub enable: Option<bool>,

    #[serde(rename = "DefaultLimits", skip_serializing_if = "is_none")]
    pub default_limits: Option<RecapLimitSettings>,

    #[serde(rename = "EnforceRecapsPerDay", skip_serializing_if = "is_none")]
    pub enforce_recaps_per_day: Option<bool>,

    #[serde(rename = "EnforceScheduledRecaps", skip_serializing_if = "is_none")]
    pub enforce_scheduled_recaps: Option<bool>,

    #[serde(rename = "EnforceChannelsPerRecap", skip_serializing_if = "is_none")]
    pub enforce_channels_per_recap: Option<bool>,

    #[serde(rename = "EnforcePostsPerRecap", skip_serializing_if = "is_none")]
    pub enforce_posts_per_recap: Option<bool>,

    #[serde(rename = "EnforceTokensPerRecap", skip_serializing_if = "is_none")]
    pub enforce_tokens_per_recap: Option<bool>,

    #[serde(rename = "EnforcePostsPerDay", skip_serializing_if = "is_none")]
    pub enforce_posts_per_day: Option<bool>,

    #[serde(rename = "EnforceCooldown", skip_serializing_if = "is_none")]
    pub enforce_cooldown: Option<bool>,
}

impl AIRecapSettings {
    /// Port of `(*AIRecapSettings).SetDefaults` (ai_recap_settings.go:114).
    ///
    /// Materialises `DefaultLimits` if absent and then **always** calls its `SetDefaults`, so a
    /// partially configured limit block is completed rather than left half-set.
    pub fn set_defaults(&mut self) {
        if self.enable.is_none() {
            self.enable = Some(true);
        }

        let limits = self
            .default_limits
            .get_or_insert_with(RecapLimitSettings::default);
        limits.set_defaults();

        for flag in [
            &mut self.enforce_recaps_per_day,
            &mut self.enforce_scheduled_recaps,
            &mut self.enforce_channels_per_recap,
            &mut self.enforce_posts_per_recap,
            &mut self.enforce_tokens_per_recap,
            &mut self.enforce_posts_per_day,
            &mut self.enforce_cooldown,
        ] {
            if flag.is_none() {
                *flag = Some(true);
            }
        }
    }

    /// Port of `(*AIRecapSettings).IsEnabled` (ai_recap_settings.go:145).
    ///
    /// **Defaults to enabled**: a nil receiver and an unset `Enable` both answer true. Reproduced
    /// for the unset case; the nil receiver is unrepresentable on `&self`, and a caller holding an
    /// `Option` maps `None` to `true`.
    pub fn is_enabled(&self) -> bool {
        self.enable.unwrap_or(true)
    }

    /// Port of `(*AIRecapSettings).IsValid` (ai_recap_settings.go:155).
    ///
    /// Validates the limit block only; none of the eight toggles can be invalid.
    ///
    /// `Config.AIRecapsEnabled` — which ANDs this with the `EnableAIRecaps` feature flag — lives
    /// on `Config` and is ported in `config.rs`.
    pub fn is_valid(&self) -> AppResult {
        if let Some(limits) = &self.default_limits {
            limits.is_valid()?;
        }
        Ok(())
    }
}
