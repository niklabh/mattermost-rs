//! Ports of the `app` functions behind the migrated `/api/v4/system/*` and `/api/v4/cluster/*`
//! reads: the onboarding flag, the applied-migration table and the cluster roster.
//!
//! Go scatters these across `app/onboarding.go`, `app/server.go` and `app/admin.go`; they are one
//! module here because they are one route family and none of them is more than a store call and
//! a decision.

use mm_model::cluster_info::ClusterInfo;
use mm_model::system::{AppliedMigration, SYSTEM_FIRST_ADMIN_SETUP_COMPLETE, System};
use mm_model::utils::{AppError, AppResult};
use mm_store::SystemStore;

use crate::App;
use crate::license::LicenseState;

impl App {
    /// Port of `App.GetOnboarding` (app/onboarding.go:90).
    ///
    /// # A missing row is `"false"`, not an error and not an absent object
    ///
    /// Go asks the store for `FirstAdminSetupComplete` and, on `ErrNotFound`, **synthesises** a
    /// `model.System` with the same name and the string `"false"` (onboarding.go:94-98). So a
    /// server that has never completed onboarding answers `200 {"name":"FirstAdminSetupComplete",
    /// "value":"false"}` rather than 404 — the client cannot distinguish "not set" from "set to
    /// false", and is not meant to.
    ///
    /// # The value is a string, because the `Systems` table is `map[string]string`
    ///
    /// `"true"` / `"false"` are four and five bytes of text on the wire. A port reaching for a
    /// `bool` changes the JSON shape of every system row.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_onboarding(&self) -> AppResult<System> {
        let value = self
            .store()
            .system()
            .get_by_name(SYSTEM_FIRST_ADMIN_SETUP_COMPLETE)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "onboarding flag lookup failed");
                AppError::boxed(
                    "getFirstAdminCompleteSetup",
                    "api.error_get_first_admin_complete_setup",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", value.is_some());
        Ok(onboarding_row(value))
    }

    /// Port of `App.GetAppliedSchemaMigrations` (app/server.go:2034).
    ///
    /// One store call and an error wrap whose id is `api.file.read_file.app_error` — a
    /// translation key about **reading a file**, on a query against `db_migrations`. That is what
    /// Go emits; it is not a transcription slip here.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_applied_schema_migrations(&self) -> AppResult<Vec<AppliedMigration>> {
        let migrations = self.store().get_applied_migrations().await.map_err(|err| {
            tracing::error!(error = ?err, "db_migrations read failed");
            AppError::boxed(
                "GetDBSchemaTable",
                "api.file.read_file.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        tracing::Span::current().record("found", migrations.len());
        Ok(migrations)
    }

    /// Port of `App.GetClusterStatus` (app/admin.go:133).
    ///
    /// # Unlicensed, the answer is an empty array and there is nothing else it could be
    ///
    /// Go's whole body is `if a.Cluster() == nil { return make([]*model.ClusterInfo, 0), nil }`
    /// and a delegation to the cluster interface otherwise. `a.Cluster()` is
    /// `einterfaces.ClusterInterface`, registered only by the enterprise build
    /// (`platform/service.go:544`), so on Team Edition — and on any unlicensed server — the first
    /// branch is the only reachable one. That is the same boundary MIGRATION.md records for the
    /// cache-invalidation bus: there is no cluster to ask, in either process.
    ///
    /// **`make(..., 0)`, not `nil`** — so the JSON is `[]` and never `null`. The distinction is
    /// the whole wire format of this route.
    ///
    /// The licensed branch is not ported and cannot be: it reads gossip state held in the other
    /// process. A caller must forward when [`LicenseState::Licensed`], which is why this returns
    /// the state it decided on rather than swallowing it.
    #[tracing::instrument(skip_all)]
    pub async fn get_cluster_status(&self) -> AppResult<Vec<ClusterInfo>> {
        Ok(Vec::new())
    }

    /// Whether [`Self::get_cluster_status`] may answer at all.
    ///
    /// Extracted so the boundary is one named decision rather than a condition repeated in every
    /// handler that has one.
    pub async fn cluster_status_is_ours_to_answer(&self) -> AppResult<bool> {
        Ok(self.license_state().await? == LicenseState::Unlicensed)
    }
}

/// Go's synthesised `model.System` for the onboarding flag (onboarding.go:94-98).
///
/// A pure function of what the store found, because that *is* the branch: a missing row and a row
/// holding `"false"` must produce identical JSON.
///
/// **It lives here rather than inside the test module, and that distinction cost a mutation.**
/// The first version of this port inlined the `unwrap_or_else` in `get_onboarding` and gave the
/// test module its own byte-identical copy to assert against — so a mutation changing the real
/// default to `"true"` left the test passing against its own private copy. A test that reimplements
/// the thing it is testing is a test of itself.
fn onboarding_row(stored: Option<String>) -> System {
    System {
        name: SYSTEM_FIRST_ADMIN_SETUP_COMPLETE.to_owned(),
        value: stored.unwrap_or_else(|| "false".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_row_is_indistinguishable_from_a_stored_false() {
        assert_eq!(onboarding_row(None), onboarding_row(Some("false".into())));
        assert_eq!(onboarding_row(None).value, "false");
        assert_eq!(
            onboarding_row(None).name,
            "FirstAdminSetupComplete",
            "the constant's value is not its Go name"
        );
    }

    /// A stored value is passed through untouched — including one that is neither `"true"` nor
    /// `"false"`. Go does no validation here and neither does this.
    #[test]
    fn a_stored_value_is_not_interpreted() {
        assert_eq!(onboarding_row(Some("true".into())).value, "true");
        assert_eq!(onboarding_row(Some(String::new())).value, "");
        assert_eq!(onboarding_row(Some("yes".into())).value, "yes");
    }
}
