//! Port of the read half of `app/job.go` — three getters and the permission table behind them.
//!
//! # `SessionHasPermissionToReadJob` returns *two* values and the second one is a gate
//!
//! `(bool, *model.Permission)` — and the pointer is `nil` for a job type the switch does not
//! recognise. Every caller checks that **first**:
//!
//! ```go
//! hasPermission, permissionRequired := c.App.SessionHasPermissionToReadJob(session, jobType)
//! if permissionRequired == nil {
//!     c.Err = model.NewAppError(..., "api.job.retrieve.nopermissions", ..., http.StatusBadRequest)
//! ```
//!
//! So an unknown job type is a **400**, not a 403 — the server says "that type has no permission
//! attached", not "you may not". Measured: `GET /api/v4/jobs/type/bogus_type` answers 400 with
//! `api.job.retrieve.nopermissions` on the running server. Collapsing the pair into a plain
//! `bool` loses exactly that branch, which is why [`ReadJobPermission`] is an enum rather than a
//! tuple of `(bool, Option<…>)` a caller could destructure and ignore.
//!
//! # The table is not `AllJobTypes`
//!
//! Thirteen types share `read_jobs`, five have a permission of their own, two are access-control
//! types with their own rules, and **everything else falls off the end of the switch** — including
//! several members of `AllJobTypes`. `resend_invitation_email` is in neither, so it is
//! unreadable through the API by anyone. Reproduced as Go writes it; see
//! [`App::session_has_permission_to_read_job`].

use mm_model::job::{self, Job};
use mm_model::permission::{
    PERMISSION_CREATE_COMPLIANCE_EXPORT_JOB, PERMISSION_CREATE_DATA_RETENTION_JOB,
    PERMISSION_CREATE_ELASTICSEARCH_POST_AGGREGATION_JOB,
    PERMISSION_CREATE_ELASTICSEARCH_POST_INDEXING_JOB, PERMISSION_CREATE_LDAP_SYNC_JOB,
    PERMISSION_MANAGE_CHANNEL_ACCESS_RULES, PERMISSION_MANAGE_COMPLIANCE_EXPORT_JOB,
    PERMISSION_MANAGE_DATA_RETENTION_JOB, PERMISSION_MANAGE_ELASTICSEARCH_POST_AGGREGATION_JOB,
    PERMISSION_MANAGE_ELASTICSEARCH_POST_INDEXING_JOB, PERMISSION_MANAGE_JOBS,
    PERMISSION_MANAGE_LDAP_SYNC_JOB, PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM_ACCESS_RULES,
    PERMISSION_READ_COMPLIANCE_EXPORT_JOB, PERMISSION_READ_DATA_RETENTION_JOB,
    PERMISSION_READ_ELASTICSEARCH_POST_AGGREGATION_JOB,
    PERMISSION_READ_ELASTICSEARCH_POST_INDEXING_JOB, PERMISSION_READ_JOBS,
    PERMISSION_READ_LDAP_SYNC_JOB, Permission,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult, StringMap, get_millis, is_valid_id, new_id};
use mm_store::{JobStore, StoreError};

use crate::App;

/// The answer `SessionHasPermissionToReadJob` gives, with Go's nil case made unignorable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadJobPermission {
    /// The switch had no case for this job type — Go's `(false, nil)`. Every caller turns this
    /// into a **400** `api.job.retrieve.nopermissions`, never a 403.
    NoPermissionDefined,
    /// The type maps to a permission, and whether the session holds it.
    Defined {
        granted: bool,
        required: &'static Permission,
    },
}

impl ReadJobPermission {
    /// True only for `Defined { granted: true, .. }`. `NoPermissionDefined` is not a grant.
    pub fn granted(&self) -> bool {
        matches!(self, Self::Defined { granted: true, .. })
    }

    /// The permission to name in a 403, or `None` when Go would have answered 400 instead.
    pub fn required(&self) -> Option<&'static Permission> {
        match self {
            Self::NoPermissionDefined => None,
            Self::Defined { required, .. } => Some(required),
        }
    }
}

/// The verdict of `SessionHasPermissionToCreateJob` and `SessionHasPermissionToManageJob`,
/// which answer the same `(bool, *Permission)` pair as the read check.
pub type JobPermission = ReadJobPermission;

fn granted(permission: &'static Permission) -> JobPermission {
    JobPermission::Defined {
        granted: true,
        required: permission,
    }
}

fn denied(permission: &'static Permission) -> JobPermission {
    JobPermission::Defined {
        granted: false,
        required: permission,
    }
}

/// The job types this deployment registers a worker for — `JobServer.RegisterJobType` calls in
/// `Server.initJobs` (app/server.go:1585) that are **unconditional**. The eight behind an
/// enterprise interface (`data_retention`, `message_export`, both Elasticsearch types,
/// `ldap_sync`, both access-control syncs, `push_proxy_auth`) are nil on every build from this
/// tree ([D-571]), and `auto_translation_recovery` needs the licensed auto-translation service.
/// `_createJob` refuses a type with no worker as `model.job.is_valid.type.app_error`, so a job
/// of a listed-but-unregistered type is created by nobody.
pub const REGISTERED_JOB_TYPES: [&str; 29] = [
    job::JOB_TYPE_MIGRATIONS,
    job::JOB_TYPE_PLUGINS,
    job::JOB_TYPE_EXPIRY_NOTIFY,
    job::JOB_TYPE_PRODUCT_NOTICES,
    job::JOB_TYPE_IMPORT_PROCESS,
    job::JOB_TYPE_IMPORT_DELETE,
    job::JOB_TYPE_S3_PATH_MIGRATION,
    job::JOB_TYPE_DELETE_EMPTY_DRAFTS_MIGRATION,
    job::JOB_TYPE_DELETE_ORPHAN_DRAFTS_MIGRATION,
    job::JOB_TYPE_EXPORT_DELETE,
    job::JOB_TYPE_EXPORT_PROCESS,
    job::JOB_TYPE_ACTIVE_USERS,
    job::JOB_TYPE_MOBILE_SESSION_METADATA,
    job::JOB_TYPE_RESEND_INVITATION_EMAIL,
    job::JOB_TYPE_EXTRACT_CONTENT,
    job::JOB_TYPE_LAST_ACCESSIBLE_POST,
    job::JOB_TYPE_LAST_ACCESSIBLE_FILE,
    job::JOB_TYPE_UPGRADE_NOTIFY_ADMIN,
    job::JOB_TYPE_TRIAL_NOTIFY_ADMIN,
    job::JOB_TYPE_POST_PERSISTENT_NOTIFICATIONS,
    job::JOB_TYPE_INSTALL_PLUGIN_NOTIFY_ADMIN,
    job::JOB_TYPE_HOSTED_PURCHASE_SCREENING,
    job::JOB_TYPE_CLEANUP_DESKTOP_TOKENS,
    job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS,
    job::JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS,
    job::JOB_TYPE_REFRESH_MATERIALIZED_VIEWS,
    job::JOB_TYPE_EXPORT_USERS_TO_CSV,
    job::JOB_TYPE_DELETE_DMS_PREFERENCES_MIGRATION,
    job::JOB_TYPE_RECAP,
];

impl App {
    /// Port of `app.App.SessionHasPermissionToCreateJob` (app/job.go:226) — also the cancel
    /// route's check ("if permission to create, permission to cancel").
    ///
    /// The two access-control types are the only arms that read the job's `data`: the team sync
    /// takes `policy_id` as a team id, and the sync takes it as a channel id and falls back to
    /// `team_id`. Their last arm — a team admin owning the policy, `ValidateTeamAdminPolicyOwnership`
    /// — is enterprise ABAC and is not consulted here: it can only grant, and only on a licensed
    /// server, so a caller who reaches it is answered `false` with `manage_system`, as an
    /// unlicensed Go answers.
    #[tracing::instrument(skip(self, session, job), fields(user_id = %session.user_id, job_type = %job.job_type))]
    pub async fn session_has_permission_to_create_job(
        &self,
        session: &Session,
        job: &Job,
    ) -> JobPermission {
        let single = |permission: &'static Permission| async move {
            JobPermission::Defined {
                granted: self.session_has_permission_to(session, permission).await,
                required: permission,
            }
        };
        let data = |key: &str| -> String {
            job.data
                .as_ref()
                .and_then(|d| d.get(key))
                .cloned()
                .unwrap_or_default()
        };
        match job.job_type.as_str() {
            job::JOB_TYPE_DATA_RETENTION => single(&PERMISSION_CREATE_DATA_RETENTION_JOB).await,
            job::JOB_TYPE_MESSAGE_EXPORT => single(&PERMISSION_CREATE_COMPLIANCE_EXPORT_JOB).await,
            job::JOB_TYPE_ELASTICSEARCH_POST_INDEXING => {
                single(&PERMISSION_CREATE_ELASTICSEARCH_POST_INDEXING_JOB).await
            }
            job::JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION => {
                single(&PERMISSION_CREATE_ELASTICSEARCH_POST_AGGREGATION_JOB).await
            }
            job::JOB_TYPE_LDAP_SYNC => single(&PERMISSION_CREATE_LDAP_SYNC_JOB).await,
            job::JOB_TYPE_MIGRATIONS
            | job::JOB_TYPE_PLUGINS
            | job::JOB_TYPE_PRODUCT_NOTICES
            | job::JOB_TYPE_EXPIRY_NOTIFY
            | job::JOB_TYPE_ACTIVE_USERS
            | job::JOB_TYPE_IMPORT_PROCESS
            | job::JOB_TYPE_IMPORT_DELETE
            | job::JOB_TYPE_EXPORT_PROCESS
            | job::JOB_TYPE_EXPORT_DELETE
            | job::JOB_TYPE_CLOUD
            | job::JOB_TYPE_EXTRACT_CONTENT
            | job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS
            | job::JOB_TYPE_NOTIFY_EXPIRING_ACCESS_TOKENS => single(&PERMISSION_MANAGE_JOBS).await,
            job::JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC => {
                if self
                    .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
                    .await
                {
                    return granted(&PERMISSION_MANAGE_SYSTEM);
                }
                // Team-type policies use the team ID as the policy ID.
                let policy_id = data("policy_id");
                if is_valid_id(&policy_id)
                    && self
                        .session_has_permission_to_team(
                            session,
                            &policy_id,
                            &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                        )
                        .await
                {
                    return granted(&PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                denied(&PERMISSION_MANAGE_SYSTEM)
            }
            job::JOB_TYPE_ACCESS_CONTROL_SYNC => {
                if self
                    .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
                    .await
                {
                    return granted(&PERMISSION_MANAGE_SYSTEM);
                }
                // `getChannelIDFromJobData`: a channel policy's id is the channel's id.
                let channel_id = data("policy_id");
                if !channel_id.is_empty() {
                    let (has_channel_permission, _) = self
                        .has_permission_to_channel(
                            &session.user_id,
                            &channel_id,
                            &PERMISSION_MANAGE_CHANNEL_ACCESS_RULES,
                        )
                        .await;
                    if has_channel_permission {
                        return granted(&PERMISSION_MANAGE_CHANNEL_ACCESS_RULES);
                    }
                }
                let team_id = data("team_id");
                if is_valid_id(&team_id)
                    && data("policy_id").is_empty()
                    && self
                        .session_has_permission_to_team(
                            session,
                            &team_id,
                            &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                        )
                        .await
                {
                    return granted(&PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                // `ValidateTeamAdminPolicyOwnership` — enterprise ABAC, see the doc comment.
                denied(&PERMISSION_MANAGE_SYSTEM)
            }
            _ => JobPermission::NoPermissionDefined,
        }
    }

    /// Port of `app.App.SessionHasPermissionToManageJob` (app/job.go:309) — the status route's
    /// check. The `manage_*` twin of the create matrix, with two differences: the `manage_jobs`
    /// list lacks `notify_expiring_access_tokens`, and `access_control_sync` is `manage_system`
    /// flat, with no channel or team arm.
    #[tracing::instrument(skip(self, session, job), fields(user_id = %session.user_id, job_type = %job.job_type))]
    pub async fn session_has_permission_to_manage_job(
        &self,
        session: &Session,
        job: &Job,
    ) -> JobPermission {
        let permission: &'static Permission = match job.job_type.as_str() {
            job::JOB_TYPE_DATA_RETENTION => &PERMISSION_MANAGE_DATA_RETENTION_JOB,
            job::JOB_TYPE_MESSAGE_EXPORT => &PERMISSION_MANAGE_COMPLIANCE_EXPORT_JOB,
            job::JOB_TYPE_ELASTICSEARCH_POST_INDEXING => {
                &PERMISSION_MANAGE_ELASTICSEARCH_POST_INDEXING_JOB
            }
            job::JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION => {
                &PERMISSION_MANAGE_ELASTICSEARCH_POST_AGGREGATION_JOB
            }
            job::JOB_TYPE_LDAP_SYNC => &PERMISSION_MANAGE_LDAP_SYNC_JOB,
            job::JOB_TYPE_MIGRATIONS
            | job::JOB_TYPE_PLUGINS
            | job::JOB_TYPE_PRODUCT_NOTICES
            | job::JOB_TYPE_EXPIRY_NOTIFY
            | job::JOB_TYPE_ACTIVE_USERS
            | job::JOB_TYPE_IMPORT_PROCESS
            | job::JOB_TYPE_IMPORT_DELETE
            | job::JOB_TYPE_EXPORT_PROCESS
            | job::JOB_TYPE_EXPORT_DELETE
            | job::JOB_TYPE_CLOUD
            | job::JOB_TYPE_EXTRACT_CONTENT
            | job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS => &PERMISSION_MANAGE_JOBS,
            job::JOB_TYPE_ACCESS_CONTROL_SYNC => &PERMISSION_MANAGE_SYSTEM,
            job::JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC => {
                if self
                    .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
                    .await
                {
                    return granted(&PERMISSION_MANAGE_SYSTEM);
                }
                let policy_id = job
                    .data
                    .as_ref()
                    .and_then(|d| d.get("policy_id"))
                    .cloned()
                    .unwrap_or_default();
                if is_valid_id(&policy_id)
                    && self
                        .session_has_permission_to_team(
                            session,
                            &policy_id,
                            &PERMISSION_MANAGE_TEAM_ACCESS_RULES,
                        )
                        .await
                {
                    return granted(&PERMISSION_MANAGE_TEAM_ACCESS_RULES);
                }
                return denied(&PERMISSION_MANAGE_SYSTEM);
            }
            _ => return JobPermission::NoPermissionDefined,
        };
        JobPermission::Defined {
            granted: self.session_has_permission_to(session, permission).await,
            required: permission,
        }
    }

    /// Port of `app.App.CreateJob` (app/job.go:86) for every type but the two access-control
    /// syncs, and of `JobServer.CreateJob`/`_createJob` (jobs/jobs.go:38, :78) behind it.
    ///
    /// A fresh id, `CreateAt` now, `pending`, the caller's `data` as sent (`null` for none);
    /// `IsValid`, then the worker check — a type this build registers no worker for is the 400
    /// `model.job.is_valid.type.app_error`, whose `detailed_error` names the id that was never
    /// written; then the `INSERT`, whose failure is `app.job.save.app_error`. The Go worker for
    /// the type picks the row up from the shared table, so a job created here runs there.
    #[tracing::instrument(skip(self, data), fields(job_type = %job_type))]
    pub async fn create_job(&self, job_type: &str, data: Option<StringMap>) -> AppResult<Job> {
        let job = Job {
            id: new_id(),
            job_type: job_type.to_owned(),
            create_at: get_millis(),
            status: job::JOB_STATUS_PENDING.to_owned(),
            data,
            ..Job::default()
        };
        job.is_valid()?;
        if !REGISTERED_JOB_TYPES.contains(&job.job_type.as_str()) {
            return Err(AppError::boxed(
                "Job.IsValid",
                "model.job.is_valid.type.app_error",
                None,
                format!("id={}", job.id),
                400,
            ));
        }
        self.store().job().save(&job).await.map_err(|err| {
            tracing::error!(error = %err, "job save failed");
            AppError::boxed(
                "CreateJob",
                "app.job.save.app_error",
                None,
                String::new(),
                500,
            )
        })
    }

    /// Port of `app.App.CancelJob` (app/job.go:209), which is `JobServer.RequestCancellation`
    /// (jobs/jobs.go:302): a `pending` job is `canceled` outright; an `in_progress` one becomes
    /// `cancel_requested` for its worker to notice; any other status is the 500
    /// `jobs.request_cancellation.status.error`. Each move publishes `job_updated`.
    #[tracing::instrument(skip(self), fields(job_id = %job_id))]
    pub async fn cancel_job(&self, job_id: &str) -> AppResult<()> {
        self.request_cancellation(job_id).await
    }

    /// Port of `app.App.UpdateJobStatus` (app/job.go:213): three statuses may be set —
    /// `pending` and `canceled` directly, `cancel_requested` through `RequestCancellation` —
    /// and any other is the 500 `app.job.update_status.app_error`, whatever `force` said.
    #[tracing::instrument(skip(self, job), fields(job_id = %job.id, new_status))]
    pub async fn update_job_status(&self, job: &Job, new_status: &str) -> AppResult<()> {
        match new_status {
            job::JOB_STATUS_PENDING => {
                self.set_job_status(&job.id, job::JOB_STATUS_PENDING, "SetJobPending")
                    .await
            }
            job::JOB_STATUS_CANCEL_REQUESTED => self.request_cancellation(&job.id).await,
            job::JOB_STATUS_CANCELED => {
                self.set_job_status(&job.id, job::JOB_STATUS_CANCELED, "SetJobCanceled")
                    .await
            }
            _ => Err(AppError::boxed(
                "UpdateJobStatus",
                "app.job.update_status.app_error",
                None,
                String::new(),
                500,
            )),
        }
    }

    async fn request_cancellation(&self, job_id: &str) -> AppResult<()> {
        let update_error = |err: StoreError| {
            tracing::error!(error = %err, job_id, "job status update failed");
            AppError::boxed(
                "RequestCancellation",
                "app.job.update.app_error",
                None,
                String::new(),
                500,
            )
        };
        if let Some(job) = self
            .store()
            .job()
            .update_status_optimistically(job_id, job::JOB_STATUS_PENDING, job::JOB_STATUS_CANCELED)
            .await
            .map_err(update_error)?
        {
            self.publish_job_status(&job, job::JOB_STATUS_CANCELED)
                .await;
            return Ok(());
        }
        if let Some(job) = self
            .store()
            .job()
            .update_status_optimistically(
                job_id,
                job::JOB_STATUS_IN_PROGRESS,
                job::JOB_STATUS_CANCEL_REQUESTED,
            )
            .await
            .map_err(update_error)?
        {
            self.publish_job_status(&job, job::JOB_STATUS_CANCEL_REQUESTED)
                .await;
            return Ok(());
        }
        Err(AppError::boxed(
            "RequestCancellation",
            "jobs.request_cancellation.status.error",
            None,
            format!("id={job_id}"),
            500,
        ))
    }

    /// `JobServer.SetJobPending` and `SetJobCanceled` (jobs/jobs.go:250, :236): an unconditional
    /// status write, then the event. A missing job is the same 500 as a failed write.
    pub(crate) async fn set_job_status(
        &self,
        job_id: &str,
        status: &str,
        where_: &'static str,
    ) -> AppResult<()> {
        let job = self
            .store()
            .job()
            .update_status(job_id, status)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, job_id, "job status update failed");
                AppError::boxed(where_, "app.job.update.app_error", None, String::new(), 500)
            })?;
        self.publish_job_status(&job, status).await;
        Ok(())
    }

    /// Port of `JobServer.publishJobStatus` (jobs/jobs.go:129): `job_updated` to everyone, with
    /// the job — its `status` overwritten by the one being announced — as a JSON string, and
    /// `ContainsSensitiveData` set so the hub delivers it only to `manage_system` sessions.
    pub(crate) async fn publish_job_status(&self, job: &Job, status: &str) {
        let mut announced = job.clone();
        announced.status = status.to_owned();
        let json = match serde_json::to_string(&announced) {
            Ok(json) => json,
            Err(err) => {
                tracing::warn!(error = %err, "Failed to marshal job for WebSocket event");
                return;
            }
        };
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_JOB_UPDATED,
            "",
            "",
            "",
            None,
            "",
        );
        message.add("job", serde_json::Value::String(json));
        if let Some(broadcast) = message.broadcast.as_mut() {
            broadcast.contains_sensitive_data = true;
        }
        self.publish(message).await;
    }

    /// Port of `App.SessionHasPermissionToReadJob` (app/job.go:358).
    ///
    /// The `access_control_team_sync` arm is the only one that consults **two** permissions:
    /// `manage_system` first, and `manage_team_access_rules` as the fallback — and when the first
    /// one grants, the permission it reports is `manage_system`, so a 403 on that type can name
    /// either permission depending on which check ran. Reproduced, including the order.
    #[tracing::instrument(skip(self, session), fields(user_id = %session.user_id, job_type))]
    pub async fn session_has_permission_to_read_job(
        &self,
        session: &Session,
        job_type: &str,
    ) -> ReadJobPermission {
        let single = |permission: &'static Permission| async move {
            ReadJobPermission::Defined {
                granted: self.session_has_permission_to(session, permission).await,
                required: permission,
            }
        };

        match job_type {
            job::JOB_TYPE_DATA_RETENTION => single(&PERMISSION_READ_DATA_RETENTION_JOB).await,
            job::JOB_TYPE_MESSAGE_EXPORT => single(&PERMISSION_READ_COMPLIANCE_EXPORT_JOB).await,
            job::JOB_TYPE_ELASTICSEARCH_POST_INDEXING => {
                single(&PERMISSION_READ_ELASTICSEARCH_POST_INDEXING_JOB).await
            }
            job::JOB_TYPE_ELASTICSEARCH_POST_AGGREGATION => {
                single(&PERMISSION_READ_ELASTICSEARCH_POST_AGGREGATION_JOB).await
            }
            job::JOB_TYPE_LDAP_SYNC => single(&PERMISSION_READ_LDAP_SYNC_JOB).await,
            job::JOB_TYPE_MIGRATIONS
            | job::JOB_TYPE_PLUGINS
            | job::JOB_TYPE_PRODUCT_NOTICES
            | job::JOB_TYPE_EXPIRY_NOTIFY
            | job::JOB_TYPE_ACTIVE_USERS
            | job::JOB_TYPE_IMPORT_PROCESS
            | job::JOB_TYPE_IMPORT_DELETE
            | job::JOB_TYPE_EXPORT_PROCESS
            | job::JOB_TYPE_EXPORT_DELETE
            | job::JOB_TYPE_CLOUD
            | job::JOB_TYPE_MOBILE_SESSION_METADATA
            | job::JOB_TYPE_EXTRACT_CONTENT
            | job::JOB_TYPE_CLEANUP_EXPIRED_ACCESS_TOKENS => single(&PERMISSION_READ_JOBS).await,
            job::JOB_TYPE_ACCESS_CONTROL_SYNC => single(&PERMISSION_MANAGE_SYSTEM).await,
            job::JOB_TYPE_ACCESS_CONTROL_TEAM_SYNC => {
                if self
                    .session_has_permission_to(session, &PERMISSION_MANAGE_SYSTEM)
                    .await
                {
                    return ReadJobPermission::Defined {
                        granted: true,
                        required: &PERMISSION_MANAGE_SYSTEM,
                    };
                }
                single(&PERMISSION_MANAGE_TEAM_ACCESS_RULES).await
            }
            _ => ReadJobPermission::NoPermissionDefined,
        }
    }

    /// Port of `App.GetJob` (app/job.go:38).
    ///
    /// **Both branches carry the same id** — `app.job.get.app_error` — and differ only in status:
    /// 404 for a miss, 500 for anything else. A client cannot tell them apart from the body.
    #[tracing::instrument(skip(self), fields(job_id = %id))]
    pub async fn get_job(&self, id: &str) -> AppResult<Job> {
        self.store().job().get(id).await.map_err(|err| {
            let status = if err.is_not_found() {
                404
            } else {
                tracing::error!(error = ?err, "job lookup failed");
                500
            };
            AppError::boxed(
                "GetJob",
                "app.job.get.app_error",
                None,
                String::new(),
                status,
            )
        })
    }

    /// Port of `App.GetJobsByTypesPage` (app/job.go:63) — the `getJobs` list.
    ///
    /// Returns the store's slice as it comes: an empty result must reach the wire as `null`, and
    /// the decision of how to encode it belongs to the handler. See [`mm_store::JobStore`].
    #[tracing::instrument(skip_all, fields(types = job_types.len(), page, per_page))]
    pub async fn get_jobs_by_types_page(
        &self,
        job_types: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Job>> {
        self.store()
            .job()
            .get_all_by_types_page(job_types, page, per_page)
            .await
            .map_err(|err| get_all_error("GetJobsByType", err))
    }

    /// Port of `App.GetJobsByTypePage` (app/job.go:55) — `getJobsByType`'s default branch.
    #[tracing::instrument(skip(self), fields(job_type = %job_type, page, per_page))]
    pub async fn get_jobs_by_type_page(
        &self,
        job_type: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Job>> {
        self.store()
            .job()
            .get_all_by_type_page(job_type, page, per_page)
            .await
            .map_err(|err| get_all_error("GetJobsByType", err))
    }

    /// Port of `App.GetJobsByTypesAndStatuses` (app/job.go:79).
    ///
    /// **The multiplication happens here**, not in the store: Go passes `page*perPage` as an
    /// offset (app/job.go:80), unlike its two page-taking neighbours. And the `where` field of the
    /// error is `GetAllByTypesAndStatusesPage` — the *store* method's name, not the app one's —
    /// which is Go's copy-paste and is reproduced because `where` is on the wire.
    #[tracing::instrument(skip_all, fields(types = job_types.len(), statuses = statuses.len(), page, per_page))]
    pub async fn get_jobs_by_types_and_statuses(
        &self,
        job_types: &[String],
        statuses: &[String],
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Job>> {
        self.store()
            .job()
            .get_all_by_types_and_statuses_page(job_types, statuses, page * per_page, per_page)
            .await
            .map_err(|err| get_all_error("GetAllByTypesAndStatusesPage", err))
    }

    /// Port of `App.GetJobsByTypeAndData` (app/job.go:71), narrowed to Go's single-pair callers.
    ///
    /// Go passes `useMaster: false`; there is one pool here, so the distinction has no effect.
    #[tracing::instrument(skip(self), fields(job_type = %job_type, data_key = %data_key))]
    pub async fn get_jobs_by_type_and_data(
        &self,
        job_type: &str,
        data_key: &str,
        data_value: &str,
    ) -> AppResult<Vec<Job>> {
        self.store()
            .job()
            .get_by_type_and_data(job_type, data_key, data_value)
            .await
            .map_err(|err| get_all_error("GetJobsByTypeAndData", err))
    }
}

/// The one error every list getter produces: a 500 with `app.job.get_all.app_error`. There is no
/// not-found branch — a query that matches nothing is an empty list, not an error.
fn get_all_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "job list lookup failed");
    AppError::boxed(
        where_,
        "app.job.get_all.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `NoPermissionDefined` is not a grant and names no permission — the pair that makes the
    /// caller answer 400 rather than 403.
    #[test]
    fn an_undefined_job_type_grants_nothing_and_names_nothing() {
        let answer = ReadJobPermission::NoPermissionDefined;
        assert!(!answer.granted());
        assert_eq!(answer.required(), None);
    }

    /// A defined-but-denied permission is also not a grant, yet it *does* name one. If these two
    /// cases collapsed, an unknown job type would start answering 403.
    #[test]
    fn a_denied_defined_permission_still_names_the_permission() {
        let answer = ReadJobPermission::Defined {
            granted: false,
            required: &PERMISSION_READ_JOBS,
        };
        assert!(!answer.granted());
        assert_eq!(answer.required().map(|p| p.id.as_ref()), Some("read_jobs"));
    }

    /// Every store failure on a list path is a 500 with the same id — there is no 404 here.
    #[test]
    fn list_errors_are_always_a_500() {
        let err = get_all_error(
            "GetJobsByType",
            StoreError::NotFound {
                entity: "Job",
                criteria: "id=x".to_owned(),
            },
        );
        assert_eq!(err.id, "app.job.get_all.app_error");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "GetJobsByType");
    }
}
