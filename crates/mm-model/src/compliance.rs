//! Port of `model/compliance.go` — a compliance export job.

use serde::{Deserialize, Serialize};

use crate::user::normalize_email;
use crate::utils::{AppError, AppResult, get_millis, go_to_lower, is_valid_id, new_id};

pub const COMPLIANCE_STATUS_CREATED: &str = "created";
pub const COMPLIANCE_STATUS_RUNNING: &str = "running";
pub const COMPLIANCE_STATUS_FINISHED: &str = "finished";
pub const COMPLIANCE_STATUS_FAILED: &str = "failed";
pub const COMPLIANCE_STATUS_REMOVED: &str = "removed";

pub const COMPLIANCE_TYPE_DAILY: &str = "daily";
pub const COMPLIANCE_TYPE_ADHOC: &str = "adhoc";

/// The `Desc` cap, inline in Go's `IsValid`.
pub const COMPLIANCE_DESC_MAX_LENGTH: usize = 512;

/// Port of `model.Compliance` (compliance.go:22).
///
/// `Keywords` and `Emails` are **comma-separated strings**, not lists — which is why `PreSave`
/// runs `NormalizeEmail` over the whole `Emails` field rather than per address.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compliance {
    #[serde(rename = "id")]
    pub id: String,

    /// Epoch milliseconds.
    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "user_id")]
    pub user_id: String,

    /// One of the five `COMPLIANCE_STATUS_*` values.
    #[serde(rename = "status")]
    pub status: String,

    /// Rows exported. Reset to zero by `PreSave`.
    #[serde(rename = "count")]
    pub count: i64,

    /// Required, and capped at [`COMPLIANCE_DESC_MAX_LENGTH`].
    #[serde(rename = "desc")]
    pub desc: String,

    /// [`COMPLIANCE_TYPE_DAILY`] or [`COMPLIANCE_TYPE_ADHOC`]. **Not validated** by `IsValid`.
    #[serde(rename = "type")]
    pub type_: String,

    /// Epoch milliseconds.
    #[serde(rename = "start_at")]
    pub start_at: i64,

    #[serde(rename = "end_at")]
    pub end_at: i64,

    #[serde(rename = "keywords")]
    pub keywords: String,

    #[serde(rename = "emails")]
    pub emails: String,
}

impl Compliance {
    /// Port of `(*Compliance).PreSave` (compliance.go:68).
    ///
    /// Does four things beyond filling the id: forces `count` to **zero**, normalises the whole
    /// `emails` string, lower-cases `keywords`, and sets `create_at` **unconditionally**.
    ///
    /// `strings.ToLower` is [`go_to_lower`], not `str::to_lowercase` — the two disagree on `İ`
    /// and on final sigma, and a keyword list is user input.
    pub fn pre_save(&mut self) {
        if self.id.is_empty() {
            self.id = new_id();
        }

        if self.status.is_empty() {
            self.status = COMPLIANCE_STATUS_CREATED.to_string();
        }

        self.count = 0;
        self.emails = normalize_email(&self.emails);
        self.keywords = go_to_lower(&self.keywords);

        self.create_at = get_millis();
    }

    /// Port of `(*Compliance).DeepCopy` (compliance.go:84) — a struct copy; every field is a
    /// scalar, so `Clone` is the same thing.
    pub fn deep_copy(&self) -> Compliance {
        self.clone()
    }

    /// Port of `(*Compliance).JobName` (compliance.go:89).
    ///
    /// `<type>-<desc>-<id>` for a daily job, `<type>-<id>` for an ad-hoc one — so a daily job's
    /// name embeds free text the user supplied.
    pub fn job_name(&self) -> String {
        let mut job_name = self.type_.clone();
        if self.type_ == COMPLIANCE_TYPE_DAILY {
            job_name.push('-');
            job_name.push_str(&self.desc);
        }

        job_name.push('-');
        job_name.push_str(&self.id);

        job_name
    }

    /// Port of `(*Compliance).IsValid` (compliance.go:100).
    ///
    /// Neither `user_id`, `status`, `type` nor `emails` is checked. The window rules are: both
    /// ends non-zero, and `end_at` strictly **after** `start_at` — a zero-length window is
    /// invalid.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.id) {
            return Err(err("id"));
        }

        if self.create_at == 0 {
            return Err(err("create_at"));
        }

        if self.desc.len() > COMPLIANCE_DESC_MAX_LENGTH || self.desc.is_empty() {
            return Err(err("desc"));
        }

        if self.start_at == 0 {
            return Err(err("start_at"));
        }

        if self.end_at == 0 {
            return Err(err("end_at"));
        }

        if self.end_at <= self.start_at {
            return Err(err("start_end_at"));
        }

        Ok(())
    }
}

fn err(field: &str) -> Box<AppError> {
    Box::new(AppError::new(
        "Compliance.IsValid",
        format!("model.compliance.is_valid.{field}.app_error"),
        None,
        "",
        400,
    ))
}

/// Port of `model.Compliances` (compliance.go:51) — a `[]Compliance` of **values**.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Compliances(pub Vec<Compliance>);

/// Port of `model.ComplianceExportCursor` (compliance.go:57).
///
/// Two independent cursors — channel posts and direct messages are exported by separate queries —
/// and each keeps the **post id alongside the timestamp** to break ties when two posts share a
/// `CreateAt`. Dropping the id would silently skip or repeat posts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ComplianceExportCursor {
    pub last_channels_query_post_create_at: i64,
    pub last_channels_query_post_id: String,
    pub channels_query_completed: bool,
    pub last_direct_messages_query_post_create_at: i64,
    pub last_direct_messages_query_post_id: String,
    pub direct_messages_query_completed: bool,
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
    fn compliance_round_trips_the_fixture() {
        assert_fixture_round_trips!(Compliance, "compliance");
    }
}
