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
    PERMISSION_MANAGE_SYSTEM, PERMISSION_MANAGE_TEAM_ACCESS_RULES,
    PERMISSION_READ_COMPLIANCE_EXPORT_JOB, PERMISSION_READ_DATA_RETENTION_JOB,
    PERMISSION_READ_ELASTICSEARCH_POST_AGGREGATION_JOB,
    PERMISSION_READ_ELASTICSEARCH_POST_INDEXING_JOB, PERMISSION_READ_JOBS,
    PERMISSION_READ_LDAP_SYNC_JOB, Permission,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, AppResult};
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

impl App {
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
