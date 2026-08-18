//! Port of `model/job.go` — the background-job record.
//!
//! # `AllJobTypes` is not all the job types
//!
//! job.go declares **42** `JobType*` constants. `AllJobTypes` lists **24**. `IsValidJobType` is a
//! linear scan of that array, so **eighteen declared job types fail the model's own validator** —
//! `cli_message_export`, `recap`, `push_proxy_auth`, every `*_notify_admin`, all four migration
//! jobs, and more. Whether that is an oversight or a deliberate "these are the schedulable ones"
//! list is not something the source settles, so the table is **enumerated from Go by name** rather
//! than transcribed, and [`ALL_JOB_TYPES`] carries the answer instead of a guess.
//!
//! # `IsValid` never checks the type
//!
//! [`is_valid_job_type`] exists and [`Job::is_valid`] does not call it. A job with a nonsense type
//! is valid as far as the model is concerned — including the eighteen above, which is what makes
//! the gap survivable in practice.
//!
//! # `IsValidStatusChange` is a partial transition table
//!
//! Three source states have outgoing transitions and four do not, so `success`, `error`,
//! `warning` and `canceled` are terminal — as is any status not in the list at all. The one that
//! reads oddly is real: **`in_progress` may go back to `pending`**, which is how a worker hands a
//! job back.
//!
//! # Not ported
//!
//! `MarshalYAML`/`UnmarshalYAML` ([D-119]) — the workspace has no YAML codec and nothing needs
//! one yet. The hard half of that pair *is* ported and measured: see [`crate::timeutils`], which
//! is where the timezone dependence and the trailing-zero elision live. `Worker` is an app-layer
//! interface over `*Config`, which is out of scope.

use serde::{Deserialize, Serialize};

use crate::utils::{self, AppResult, StringMap, is_valid_id};

// ---------------------------------------------------------------------------
// Job types (job.go:13-54)
// ---------------------------------------------------------------------------

pub const JOB_TYPE_DATA_RETENTION: &str = "data_retention";
pub const JOB_TYPE_MESSAGE_EXPORT: &str = "message_export";
pub const JOB_TYPE_CLI_MESSAGE_EXPORT: &str = "cli_message_export";
pub const JOB_TYPE_ELASTICSEARCH_POST_INDEXING: &str = "elasticsearch_post_indexing";
pub const JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION: &str = "elasticsearch_post_aggregation";
pub const JOB_TYPE_LDAP_SYNC: &str = "ldap_sync";
pub const JOB_TYPE_MIGRATIONS: &str = "migrations";
pub const JOB_TYPE_PLUGINS: &str = "plugins";
pub const JOB_TYPE_EXPIRY_NOTIFY: &str = "expiry_notify";
pub const JOB_TYPE_PRODUCT_NOTICES: &str = "product_notices";
pub const JOB_TYPE_ACTIVE_USERS: &str = "active_users";
pub const JOB_TYPE_IMPORT_PROCESS: &str = "import_process";
pub const JOB_TYPE_IMPORT_DELETE: &str = "import_delete";
pub const JOB_TYPE_EXPORT_PROCESS: &str = "export_process";
pub const JOB_TYPE_EXPORT_DELETE: &str = "export_delete";
pub const JOB_TYPE_CLOUD: &str = "cloud";
pub const JOB_TYPE_RESEND_INVITATION_EMAIL: &str = "resend_invitation_email";
pub const JOB_TYPE_EXTRACT_CONTENT: &str = "extract_content";
pub const JOB_TYPE_LAST_ACCESSIBLE_POST: &str = "last_accessible_post";
pub const JOB_TYPE_LAST_ACCESSIBLE_FILE: &str = "last_accessible_file";
pub const JOB_TYPE_UPGRADE_NOTIFY_ADMIN: &str = "upgrade_notify_admin";
pub const JOB_TYPE_TRIAL_NOTIFY_ADMIN: &str = "trial_notify_admin";
pub const JOB_TYPE_POST_PERSISTENT_NOTIFICATIONS: &str = "post_persistent_notifications";
pub const JOB_TYPE_INSTALL_PLUGIN_NOTIFY_ADMIN: &str = "install_plugin_notify_admin";
pub const JOB_TYPE_HOSTED_PURCHASE_SCREENING: &str = "hosted_purchase_screening";
pub const JOB_TYPE_S3_PATH_MIGRATION: &str = "s3_path_migration";
pub const JOB_TYPE_CLEANUP_DESKTOP_TOKENS: &str = "cleanup_desktop_tokens";
pub const JOB_TYPE_DELETE_EMPTY_DRAFTS_MIGRATION: &str = "delete_empty_drafts_migration";
pub const JOB_TYPE_REFRESH_MATERIALIZED_VIEWS: &str = "refresh_materialized_views";
pub const JOB_TYPE_DELETE_ORPHAN_DRAFTS_MIGRATION: &str = "delete_orphan_drafts_migration";
pub const JOB_TYPE_EXPORT_USERS_TO_CSV: &str = "export_users_to_csv";
pub const JOB_TYPE_DELETE_DMS_PREFERENCES_MIGRATION: &str = "delete_dms_preferences_migration";
pub const JOB_TYPE_MOBILE_SESSION_METADATA: &str = "mobile_session_metadata";
pub const JOB_TYPE_ACCESS_CONTROL_SYNC: &str = "access_control_sync";
pub const JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC: &str = "access_control_team_sync";
pub const JOB_TYPE_PUSH_PROXY_AUTH: &str = "push_proxy_auth";
pub const JOB_TYPE_RECAP: &str = "recap";
pub const JOB_TYPE_SCHEDULED_RECAP: &str = "scheduled_recap";
pub const JOB_TYPE_DELETE_EXPIRED_POSTS: &str = "delete_expired_posts";
pub const JOB_TYPE_AUTO_TRANSLATION_RECOVERY: &str = "autotranslation_recovery";
pub const JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS: &str = "cleanup_expired_access_tokens";
pub const JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS: &str = "notify_expiring_access_tokens";

// ---------------------------------------------------------------------------
// Statuses (job.go:56-62)
// ---------------------------------------------------------------------------

pub const JOB_STATUS_PENDING: &str = "pending";
pub const JOB_STATUS_IN_PROGRESS: &str = "in_progress";
pub const JOB_STATUS_SUCCESS: &str = "success";
pub const JOB_STATUS_ERROR: &str = "error";
pub const JOB_STATUS_CANCEL_REQUESTED: &str = "cancel_requested";
pub const JOB_STATUS_CANCELED: &str = "canceled";
pub const JOB_STATUS_WARNING: &str = "warning";

/// Port of `model.AllJobTypes` (job.go:65).
///
/// **24 of the 42 declared types**, in Go's order. The omissions are not arbitrary-looking and
/// they are not obviously deliberate either, so they are recorded rather than reasoned about:
/// `all_job_types_is_a_strict_subset` asserts both the membership and the size of the gap, and
/// `job_type_constants_match_go` checks every one of the 42 constants by its Go identifier.
///
/// Go's is `[...]string`, a fixed-size array — a detail with no Rust consequence beyond it being
/// impossible to append to at runtime, which a `&'static [&str]` also is.
pub static ALL_JOB_TYPES: &[&str] = &[
    JOB_TYPE_DATA_RETENTION,
    JOB_TYPE_MESSAGE_EXPORT,
    JOB_TYPE_ELASTICSEARCH_POST_INDEXING,
    JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION,
    JOB_TYPE_LDAP_SYNC,
    JOB_TYPE_MIGRATIONS,
    JOB_TYPE_PLUGINS,
    JOB_TYPE_EXPIRY_NOTIFY,
    JOB_TYPE_PRODUCT_NOTICES,
    JOB_TYPE_ACTIVE_USERS,
    JOB_TYPE_IMPORT_PROCESS,
    JOB_TYPE_IMPORT_DELETE,
    JOB_TYPE_EXPORT_PROCESS,
    JOB_TYPE_EXPORT_DELETE,
    JOB_TYPE_CLOUD,
    JOB_TYPE_EXTRACT_CONTENT,
    JOB_TYPE_LAST_ACCESSIBLE_POST,
    JOB_TYPE_LAST_ACCESSIBLE_FILE,
    JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
    JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS,
    JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS,
    JOB_TYPE_REFRESH_MATERIALIZED_VIEWS,
    JOB_TYPE_MOBILE_SESSION_METADATA,
    JOB_TYPE_SCHEDULED_RECAP,
];

/// Port of `model.Job` (job.go:92).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    #[serde(rename = "id")]
    pub id: String,

    /// A plain `String`, not an enum: Go's field is `string` and [`Job::is_valid`] never narrows
    /// it, so any value round-trips.
    #[serde(rename = "type")]
    pub job_type: String,

    #[serde(rename = "priority")]
    pub priority: i64,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "start_at")]
    pub start_at: i64,

    #[serde(rename = "last_activity_at")]
    pub last_activity_at: i64,

    #[serde(rename = "status")]
    pub status: String,

    #[serde(rename = "progress")]
    pub progress: i64,

    /// `StringMap` with **no** `omitempty`, so nil is `null` and empty is `{}` — two distinct
    /// documents, hence the `Option`.
    #[serde(rename = "data")]
    pub data: Option<StringMap>,
}

impl Job {
    /// Port of `(*Job).Auditable` (job.go:104).
    ///
    /// All nine fields, **including `data`** — which carries an upstream `// TODO do we want this
    /// here` beside it. Reproduced with the payload in place: dropping it would make the Rust
    /// audit log narrower than the Go one for the same job. Contrast [`crate::view::View`], whose
    /// projection deliberately omits its payload.
    pub fn auditable(&self) -> utils::StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("type".into(), self.job_type.clone().into());
        out.insert("priority".into(), self.priority.into());
        out.insert("create_at".into(), self.create_at.into());
        out.insert("start_at".into(), self.start_at.into());
        out.insert("last_activity_at".into(), self.last_activity_at.into());
        out.insert("status".into(), self.status.clone().into());
        out.insert("progress".into(), self.progress.into());
        out.insert(
            "data".into(),
            match &self.data {
                Some(data) => serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
                None => serde_json::Value::Null,
            },
        );
        out
    }

    /// Port of `(*Job).LogClone` (job.go:245) — literally `return j.Auditable()`.
    pub fn log_clone(&self) -> utils::StringInterface {
        self.auditable()
    }

    /// Port of `(*Job).IsValid` (job.go:187).
    ///
    /// Three checks: the id, a non-zero `CreateAt`, and a known status. **Not** the type — see the
    /// module docs — and not `Priority` or `Progress`, so a job can be 1000% complete at priority
    /// −1 and still validate.
    ///
    /// Every branch carries `id=`, including the one that fires *because* the id is bad.
    pub fn is_valid(&self) -> AppResult {
        let detail = format!("id={}", self.id);

        if !is_valid_id(&self.id) {
            return Err(job_error("model.job.is_valid.id.app_error", &detail));
        }

        if self.create_at == 0 {
            return Err(job_error("model.job.is_valid.create_at.app_error", &detail));
        }

        if !is_valid_job_status(&self.status) {
            return Err(job_error("model.job.is_valid.status.app_error", &detail));
        }

        Ok(())
    }

    /// Port of `(*Job).IsValidStatusChange` (job.go:204).
    ///
    /// | from | to |
    /// |---|---|
    /// | `in_progress` | `pending` **or** `cancel_requested` |
    /// | `pending` | `cancel_requested` |
    /// | `cancel_requested` | `canceled` |
    /// | anything else | nothing |
    ///
    /// `in_progress → pending` is the transition that reads like a mistake and is not: it is how a
    /// worker returns a job to the queue. And the fall-through means `success`, `error`,
    /// `warning`, `canceled` and any unrecognised status are **terminal**.
    pub fn is_valid_status_change(&self, new_status: &str) -> bool {
        match self.status.as_str() {
            JOB_STATUS_IN_PROGRESS => {
                new_status == JOB_STATUS_PENDING || new_status == JOB_STATUS_CANCEL_REQUESTED
            }
            JOB_STATUS_PENDING => new_status == JOB_STATUS_CANCEL_REQUESTED,
            JOB_STATUS_CANCEL_REQUESTED => new_status == JOB_STATUS_CANCELED,
            _ => false,
        }
    }
}

fn job_error(id: &str, detail: &str) -> Box<utils::AppError> {
    Box::new(utils::AppError::new("Job.IsValid", id, None, detail, 400))
}

/// Port of `model.IsValidJobStatus` (job.go:219).
///
/// All seven declared statuses are accepted — unlike the job types, where the list and the
/// constants disagree.
pub fn is_valid_job_status(status: &str) -> bool {
    matches!(
        status,
        JOB_STATUS_PENDING
            | JOB_STATUS_IN_PROGRESS
            | JOB_STATUS_SUCCESS
            | JOB_STATUS_ERROR
            | JOB_STATUS_WARNING
            | JOB_STATUS_CANCEL_REQUESTED
            | JOB_STATUS_CANCELED
    )
}

/// Port of `model.IsValidJobType` (job.go:235) — a linear scan of [`ALL_JOB_TYPES`].
///
/// **Rejects eighteen of the 42 declared job types.** See the module docs.
pub fn is_valid_job_type(job_type: &str) -> bool {
    ALL_JOB_TYPES.contains(&job_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_job() -> Job {
        Job {
            id: "abcdefghijklmnopqrstuvwxyz".into(),
            job_type: JOB_TYPE_DATA_RETENTION.into(),
            priority: 10,
            create_at: 1_700_000_000_000,
            start_at: 1_700_000_001_000,
            last_activity_at: 1_700_000_002_000,
            status: JOB_STATUS_PENDING.into(),
            progress: 42,
            data: Some(StringMap::from([("key".to_owned(), "value".to_owned())])),
        }
    }

    #[test]
    fn a_valid_job_validates() {
        assert!(valid_job().is_valid().is_ok());
    }

    /// The gap between "declared" and "accepted", asserted rather than described.
    #[test]
    fn all_job_types_is_a_strict_subset_of_the_declared_constants() {
        assert_eq!(ALL_JOB_TYPES.len(), 24);

        for omitted in [
            JOB_TYPE_CLI_MESSAGE_EXPORT,
            JOB_TYPE_RECAP,
            JOB_TYPE_PUSH_PROXY_AUTH,
            JOB_TYPE_UPGRADE_NOTIFY_ADMIN,
            JOB_TYPE_S3_PATH_MIGRATION,
        ] {
            assert!(
                !is_valid_job_type(omitted),
                "{omitted} is declared and is NOT in AllJobTypes"
            );
        }

        // ...and yet a job carrying one validates, because IsValid never consults the list.
        let mut job = valid_job();
        job.job_type = JOB_TYPE_RECAP.into();
        assert!(job.is_valid().is_ok());
    }

    #[test]
    fn is_valid_does_not_check_the_type() {
        let mut job = valid_job();
        job.job_type = "not_a_job_type_at_all".into();
        assert!(job.is_valid().is_ok());
        assert!(!is_valid_job_type(&job.job_type));

        job.job_type = String::new();
        assert!(job.is_valid().is_ok());
    }

    #[test]
    fn nothing_bounds_priority_or_progress() {
        let mut job = valid_job();
        job.priority = -1;
        job.progress = 1000;
        assert!(job.is_valid().is_ok());
    }

    /// The transition that reads like a bug.
    #[test]
    fn in_progress_may_return_to_pending() {
        let mut job = valid_job();
        job.status = JOB_STATUS_IN_PROGRESS.into();
        assert!(job.is_valid_status_change(JOB_STATUS_PENDING));
        assert!(job.is_valid_status_change(JOB_STATUS_CANCEL_REQUESTED));
        assert!(!job.is_valid_status_change(JOB_STATUS_SUCCESS));
    }

    #[test]
    fn the_terminal_states_have_no_transitions() {
        let mut job = valid_job();
        for terminal in [
            JOB_STATUS_SUCCESS,
            JOB_STATUS_ERROR,
            JOB_STATUS_WARNING,
            JOB_STATUS_CANCELED,
            "",
            "unknown",
        ] {
            job.status = terminal.into();
            for target in [
                JOB_STATUS_PENDING,
                JOB_STATUS_IN_PROGRESS,
                JOB_STATUS_CANCEL_REQUESTED,
                JOB_STATUS_CANCELED,
            ] {
                assert!(
                    !job.is_valid_status_change(target),
                    "{terminal} -> {target} must be refused"
                );
            }
        }
    }

    /// `data` has no `omitempty`, so nil and empty are different documents.
    #[test]
    fn nil_and_empty_data_differ_on_the_wire() {
        let mut job = valid_job();
        job.data = None;
        assert!(
            utils::go_json_marshal(&job)
                .unwrap()
                .contains(r#""data":null"#)
        );

        job.data = Some(StringMap::new());
        assert!(
            utils::go_json_marshal(&job)
                .unwrap()
                .contains(r#""data":{}"#)
        );
    }
}

#[cfg(test)]
mod go_parity {
    use super::*;
    use serde_json::Value;
    use std::sync::OnceLock;

    fn oracle() -> &'static Value {
        static ORACLE: OnceLock<Value> = OnceLock::new();
        ORACLE.get_or_init(|| {
            let raw = include_str!("../../../fixtures/behaviour_job.json");
            serde_json::from_str(raw).expect("behaviour_job.json parses")
        })
    }

    const ID: &str = "abcdefghijklmnopqrstuvwxyz";

    fn base_job() -> Job {
        Job {
            id: ID.into(),
            job_type: JOB_TYPE_DATA_RETENTION.into(),
            priority: 10,
            create_at: 1_700_000_000_000,
            start_at: 1_700_000_001_000,
            last_activity_at: 1_700_000_002_000,
            status: JOB_STATUS_PENDING.into(),
            progress: 42,
            data: Some(StringMap::from([("key".to_owned(), "value".to_owned())])),
        }
    }

    /// Every constant, checked by its **Go identifier** rather than as an unordered set.
    ///
    /// A set comparison would pass if two constants were swapped; this cannot.
    fn rust_job_type_constants() -> Vec<(&'static str, &'static str)> {
        vec![
            ("JobTypeDataRetention", JOB_TYPE_DATA_RETENTION),
            ("JobTypeMessageExport", JOB_TYPE_MESSAGE_EXPORT),
            ("JobTypeCLIMessageExport", JOB_TYPE_CLI_MESSAGE_EXPORT),
            (
                "JobTypeElasticsearchPostIndexing",
                JOB_TYPE_ELASTICSEARCH_POST_INDEXING,
            ),
            (
                "JobTypeElasticsearchPostAggregation",
                JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION,
            ),
            ("JobTypeLdapSync", JOB_TYPE_LDAP_SYNC),
            ("JobTypeMigrations", JOB_TYPE_MIGRATIONS),
            ("JobTypePlugins", JOB_TYPE_PLUGINS),
            ("JobTypeExpiryNotify", JOB_TYPE_EXPIRY_NOTIFY),
            ("JobTypeProductNotices", JOB_TYPE_PRODUCT_NOTICES),
            ("JobTypeActiveUsers", JOB_TYPE_ACTIVE_USERS),
            ("JobTypeImportProcess", JOB_TYPE_IMPORT_PROCESS),
            ("JobTypeImportDelete", JOB_TYPE_IMPORT_DELETE),
            ("JobTypeExportProcess", JOB_TYPE_EXPORT_PROCESS),
            ("JobTypeExportDelete", JOB_TYPE_EXPORT_DELETE),
            ("JobTypeCloud", JOB_TYPE_CLOUD),
            (
                "JobTypeResendInvitationEmail",
                JOB_TYPE_RESEND_INVITATION_EMAIL,
            ),
            ("JobTypeExtractContent", JOB_TYPE_EXTRACT_CONTENT),
            ("JobTypeLastAccessiblePost", JOB_TYPE_LAST_ACCESSIBLE_POST),
            ("JobTypeLastAccessibleFile", JOB_TYPE_LAST_ACCESSIBLE_FILE),
            ("JobTypeUpgradeNotifyAdmin", JOB_TYPE_UPGRADE_NOTIFY_ADMIN),
            ("JobTypeTrialNotifyAdmin", JOB_TYPE_TRIAL_NOTIFY_ADMIN),
            (
                "JobTypePostPersistentNotifications",
                JOB_TYPE_POST_PERSISTENT_NOTIFICATIONS,
            ),
            (
                "JobTypeInstallPluginNotifyAdmin",
                JOB_TYPE_INSTALL_PLUGIN_NOTIFY_ADMIN,
            ),
            (
                "JobTypeHostedPurchaseScreening",
                JOB_TYPE_HOSTED_PURCHASE_SCREENING,
            ),
            ("JobTypeS3PathMigration", JOB_TYPE_S3_PATH_MIGRATION),
            (
                "JobTypeCleanupDesktopTokens",
                JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
            ),
            (
                "JobTypeDeleteEmptyDraftsMigration",
                JOB_TYPE_DELETE_EMPTY_DRAFTS_MIGRATION,
            ),
            (
                "JobTypeRefreshMaterializedViews",
                JOB_TYPE_REFRESH_MATERIALIZED_VIEWS,
            ),
            (
                "JobTypeDeleteOrphanDraftsMigration",
                JOB_TYPE_DELETE_ORPHAN_DRAFTS_MIGRATION,
            ),
            ("JobTypeExportUsersToCSV", JOB_TYPE_EXPORT_USERS_TO_CSV),
            (
                "JobTypeDeleteDmsPreferencesMigration",
                JOB_TYPE_DELETE_DMS_PREFERENCES_MIGRATION,
            ),
            (
                "JobTypeMobileSessionMetadata",
                JOB_TYPE_MOBILE_SESSION_METADATA,
            ),
            ("JobTypeAccessControlSync", JOB_TYPE_ACCESS_CONTROL_SYNC),
            (
                "JobTypeAccessControlTeamSync",
                JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC,
            ),
            ("JobTypePushProxyAuth", JOB_TYPE_PUSH_PROXY_AUTH),
            ("JobTypeRecap", JOB_TYPE_RECAP),
            ("JobTypeScheduledRecap", JOB_TYPE_SCHEDULED_RECAP),
            ("JobTypeDeleteExpiredPosts", JOB_TYPE_DELETE_EXPIRED_POSTS),
            (
                "JobTypeAutoTranslationRecovery",
                JOB_TYPE_AUTO_TRANSLATION_RECOVERY,
            ),
            (
                "JobTypeCleanupExpiredAccessTokens",
                JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS,
            ),
            (
                "JobTypeNotifyExpiringAccessTokens",
                JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS,
            ),
        ]
    }

    #[test]
    fn job_type_constants_match_go() {
        let want = oracle()["constants"]["job_types"].as_object().unwrap();
        let ours = rust_job_type_constants();

        assert_eq!(
            ours.len(),
            want.len(),
            "every declared constant must be transcribed"
        );
        assert_eq!(oracle()["constants"]["job_type_count"], ours.len());

        for (go_name, value) in ours {
            assert_eq!(
                want.get(go_name).and_then(Value::as_str),
                Some(value),
                "{go_name}"
            );
        }
    }

    #[test]
    fn job_status_constants_match_go() {
        let want = oracle()["constants"]["job_statuses"].as_object().unwrap();
        for (go_name, value) in [
            ("JobStatusPending", JOB_STATUS_PENDING),
            ("JobStatusInProgress", JOB_STATUS_IN_PROGRESS),
            ("JobStatusSuccess", JOB_STATUS_SUCCESS),
            ("JobStatusError", JOB_STATUS_ERROR),
            ("JobStatusCancelRequested", JOB_STATUS_CANCEL_REQUESTED),
            ("JobStatusCanceled", JOB_STATUS_CANCELED),
            ("JobStatusWarning", JOB_STATUS_WARNING),
        ] {
            assert_eq!(
                want.get(go_name).and_then(Value::as_str),
                Some(value),
                "{go_name}"
            );
        }
        assert_eq!(want.len(), 7);
    }

    /// `AllJobTypes`, in Go's order — the array, not a set.
    #[test]
    fn all_job_types_matches_go() {
        let want: Vec<&str> = oracle()["all_job_types"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();

        assert_eq!(ALL_JOB_TYPES, want.as_slice());
        assert_eq!(oracle()["constants"]["all_job_types_length"], want.len());
    }

    /// **The finding**: eighteen declared job types fail the validator.
    #[test]
    fn eighteen_declared_job_types_are_rejected() {
        let mut declared = 0;
        let mut rejected = Vec::new();

        for case in oracle()["is_valid_job_type"].as_array().unwrap() {
            let value = case["value"].as_str().unwrap();
            let want = case["valid"].as_bool().unwrap();
            assert_eq!(is_valid_job_type(value), want, "IsValidJobType({value:?})");

            if case["declared_constant"].as_bool().unwrap() {
                declared += 1;
                if !want {
                    rejected.push(case["go_name"].as_str().unwrap());
                }
            }
        }

        assert_eq!(declared, 42, "the corpus covers every declared constant");
        assert_eq!(
            rejected.len(),
            18,
            "declared but not in AllJobTypes: {rejected:?}"
        );
        assert_eq!(declared - rejected.len(), ALL_JOB_TYPES.len());
    }

    #[test]
    fn is_valid_job_status_matches_go() {
        for case in oracle()["is_valid_job_status"].as_array().unwrap() {
            let value = case["value"].as_str().unwrap();
            assert_eq!(
                is_valid_job_status(value),
                case["valid"].as_bool().unwrap(),
                "IsValidJobStatus({value:?})"
            );
        }
    }

    #[test]
    fn is_valid_matches_go() {
        for case in oracle()["is_valid"].as_array().unwrap() {
            let name = case["name"].as_str().unwrap();
            let mut j = base_job();

            match name {
                "valid" => {}
                "bad_id" => j.id = "nope".into(),
                "empty_id" => j.id = String::new(),
                "zero_create_at" => j.create_at = 0,
                "negative_create_at" => j.create_at = -1,
                "empty_status" => j.status = String::new(),
                "unknown_status" => j.status = "done".into(),
                "uppercase_status" => j.status = "PENDING".into(),
                "empty_type" => j.job_type = String::new(),
                "unknown_type" => j.job_type = "not_a_job_type".into(),
                "declared_but_unlisted_type" => j.job_type = JOB_TYPE_RECAP.into(),
                "negative_priority" => j.priority = -1,
                "negative_progress" => j.progress = -1,
                "progress_over_100" => j.progress = 1000,
                "nil_data" => j.data = None,
                "zero_start_at" => j.start_at = 0,
                "bad_id_and_zero_create_at" => {
                    j.id = "nope".into();
                    j.create_at = 0;
                }
                "zero_create_at_and_bad_status" => {
                    j.create_at = 0;
                    j.status = "done".into();
                }
                other => panic!("unmapped corpus case: {other}"),
            }

            match j.is_valid() {
                Ok(()) => assert!(case["ok"].as_bool().unwrap(), "{name}: Go rejected this"),
                Err(err) => {
                    assert!(!case["ok"].as_bool().unwrap(), "{name}: Go accepted this");
                    assert_eq!(err.id, case["id"].as_str().unwrap(), "{name}: id");
                    assert_eq!(err.where_, case["where"].as_str().unwrap(), "{name}: where");
                    assert_eq!(
                        err.status_code,
                        case["status"].as_i64().unwrap() as i32,
                        "{name}: status"
                    );
                    assert_eq!(
                        err.detailed_error,
                        case["detailed_error"].as_str().unwrap(),
                        "{name}: detailed_error"
                    );
                }
            }
        }
    }

    /// The full 9×9 transition matrix, including statuses the switch never names.
    #[test]
    fn is_valid_status_change_matches_go() {
        let cases = oracle()["is_valid_status_change"].as_array().unwrap();
        assert_eq!(cases.len(), 81, "9 source states x 9 targets");

        let mut allowed = 0;
        for case in cases {
            let mut j = base_job();
            j.status = case["current"].as_str().unwrap().to_owned();
            let next = case["new"].as_str().unwrap();
            let want = case["allowed"].as_bool().unwrap();

            assert_eq!(
                j.is_valid_status_change(next),
                want,
                "{:?} -> {next:?}",
                j.status
            );
            if want {
                allowed += 1;
            }
        }

        assert_eq!(
            allowed, 4,
            "only four of the 81 pairs are permitted — the table is very partial"
        );
    }

    #[test]
    fn auditable_matches_go() {
        let a = &oracle()["auditable"];

        assert_eq!(
            base_job().auditable().len() as u64,
            a["key_count"].as_u64().unwrap()
        );
        assert_eq!(
            utils::go_json_marshal(&base_job().auditable()).unwrap(),
            a["json"].as_str().unwrap()
        );

        let mut nil_data = base_job();
        nil_data.data = None;
        assert_eq!(
            utils::go_json_marshal(&nil_data.auditable()).unwrap(),
            a["nil_data_json"].as_str().unwrap()
        );

        // LogClone is Auditable, verbatim.
        assert_eq!(a["log_clone_equals_auditable"], true);
        assert_eq!(base_job().log_clone(), base_job().auditable());

        // Unlike View's projection, this one carries the payload — with a TODO beside it upstream.
        assert_eq!(a["includes_data"], true);
    }

    #[test]
    fn the_wire_format_matches_go() {
        for probe in oracle()["wire"].as_array().unwrap() {
            let name = probe["name"].as_str().unwrap();
            let job = match name {
                "zero" => Job::default(),
                "full" => base_job(),
                "nil_data" => Job {
                    data: None,
                    ..base_job()
                },
                "empty_data" => Job {
                    data: Some(StringMap::new()),
                    ..base_job()
                },
                "negative_numbers" => Job {
                    priority: -1,
                    progress: -100,
                    create_at: -1,
                    ..base_job()
                },
                "escapable_data" => Job {
                    data: Some(StringMap::from([
                        ("html".to_owned(), "<b>&</b>".to_owned()),
                        ("path".to_owned(), "a/b".to_owned()),
                    ])),
                    ..base_job()
                },
                other => panic!("unmapped wire probe: {other}"),
            };

            assert_eq!(
                utils::go_json_marshal(&job).unwrap(),
                probe["json"].as_str().unwrap(),
                "{name}"
            );
        }
    }

    /// The generated wire fixture, round-tripped.
    #[test]
    fn the_fixture_round_trips() {
        let raw = include_str!("../../../fixtures/job.json");
        let want: Value = serde_json::from_str(raw).expect("fixture parses");
        let decoded: Job = serde_json::from_str(raw).expect("decodes");
        let ours: Value =
            serde_json::from_str(&utils::go_json_marshal(&decoded).unwrap()).expect("re-parses");
        assert_eq!(ours, want);
    }
}
