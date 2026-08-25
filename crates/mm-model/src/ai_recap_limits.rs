//! Port of `model/ai_recap_limits.go` — resolved per-user recap limits and their live status.

use serde::{Deserialize, Serialize};

/// Port of `model.UnlimitedValue` (ai_recap_limits.go:4) — **`-1`, not `0`**. Zero is a real
/// limit meaning "none allowed"; `-1` means the limit is off.
pub const UNLIMITED_VALUE: i64 = -1;

/// Port of `model.LimitSource` (ai_recap_limits.go:7) — where a resolved limit came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LimitSource(pub String);

impl LimitSource {
    pub const SYSTEM: &'static str = "system";
    pub const GROUP: &'static str = "group";
    pub const USER: &'static str = "user";

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for LimitSource {
    fn from(s: &str) -> Self {
        LimitSource(s.to_string())
    }
}

/// Port of `model.IsLimitEnabled` (ai_recap_limits.go:57).
///
/// Only `-1` disables a limit — a negative value other than `-1` is *not* unlimited, it is simply
/// an unreachable limit.
pub fn is_limit_enabled(limit_value: i64) -> bool {
    limit_value != UNLIMITED_VALUE
}

/// Port of `model.EffectiveRecapLimits` (ai_recap_limits.go:18) — the resolved values.
///
/// Non-pointer fields **on purpose**: resolution has already happened, so there is no "unset"
/// state left. Contrast `RecapLimitSettings` in `ai_recap_settings.rs`, which is all pointers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffectiveRecapLimits {
    #[serde(rename = "max_recaps_per_day")]
    pub max_recaps_per_day: i64,

    #[serde(rename = "max_scheduled_recaps")]
    pub max_scheduled_recaps: i64,

    #[serde(rename = "max_channels_per_recap")]
    pub max_channels_per_recap: i64,

    #[serde(rename = "max_posts_per_recap")]
    pub max_posts_per_recap: i64,

    #[serde(rename = "max_tokens_per_recap")]
    pub max_tokens_per_recap: i64,

    #[serde(rename = "max_posts_per_day")]
    pub max_posts_per_day: i64,

    /// `0` means no cooldown — this one is **not** `-1`-gated.
    #[serde(rename = "cooldown_minutes")]
    pub cooldown_minutes: i64,

    #[serde(rename = "source")]
    pub source: LimitSource,

    /// The group or user id when overridden; empty for `system`.
    #[serde(rename = "source_id")]
    pub source_id: String,
}

/// Port of `model.RecapLimitStatus` (ai_recap_limits.go:36).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecapLimitStatus {
    #[serde(rename = "effective_limits")]
    pub effective_limits: EffectiveRecapLimits,

    #[serde(rename = "daily")]
    pub daily: DailyUsageStatus,

    #[serde(rename = "cooldown")]
    pub cooldown: CooldownStatus,
}

/// Port of `model.DailyUsageStatus` (ai_recap_limits.go:43).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyUsageStatus {
    #[serde(rename = "used")]
    pub used: i64,

    #[serde(rename = "limit")]
    pub limit: i64,

    /// Epoch milliseconds — **midnight in the user's timezone**, not UTC midnight.
    #[serde(rename = "reset_at")]
    pub reset_at: i64,
}

/// Port of `model.CooldownStatus` (ai_recap_limits.go:50).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CooldownStatus {
    #[serde(rename = "is_active")]
    pub is_active: bool,

    /// Epoch milliseconds.
    #[serde(rename = "available_at")]
    pub available_at: i64,

    /// Seconds — the same instant as `available_at`, expressed for an HTTP `Retry-After`.
    #[serde(rename = "retry_after_seconds")]
    pub retry_after_seconds: i64,
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
    fn effective_recap_limits_round_trips_the_fixture() {
        assert_fixture_round_trips!(EffectiveRecapLimits, "effective_recap_limits");
    }
    #[test]
    fn recap_limit_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(RecapLimitStatus, "recap_limit_status");
    }
    #[test]
    fn daily_usage_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(DailyUsageStatus, "daily_usage_status");
    }
    #[test]
    fn cooldown_status_round_trips_the_fixture() {
        assert_fixture_round_trips!(CooldownStatus, "cooldown_status");
    }
}
