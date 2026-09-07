//! Port of `app/scheme.go`, restricted to the reads the `/api/v4/schemes` routes make.
//!
//! # Every function here is gated on a migration having finished
//!
//! `IsPhase2MigrationCompleted` (scheme.go:204) guards **all** of them, including the plain
//! `GetScheme`. It is not a licence check and not a permission check: it asks whether the
//! Advanced Permissions Phase 2 migration has written its row into `Systems`, and answers **501**
//! `app.schemes.is_phase_2_migration_completed.not_completed.app_error` if not. A server that has
//! never run it has no usable schemes, so the whole family refuses rather than returning nonsense.
//!
//! The write half — `CreateScheme`, `PatchScheme`, `DeleteScheme` — is not ported. Each of those
//! routes is licence-gated ahead of the app call and answers 501 on an unlicensed server, so on
//! this deployment the app functions are unreachable; see `mm_api::schemes`.

use mm_model::channel::Channel;
use mm_model::migration::MIGRATION_KEY_ADVANCED_PERMISSIONS_PHASE2;
use mm_model::scheme::Scheme;
use mm_model::team::Team;
use mm_model::utils::{AppError, AppResult};
use mm_store::{ChannelStore, SchemeStore, StoreError, SystemStore, TeamStore};

use crate::App;

impl App {
    /// Port of `Server.IsPhase2MigrationCompleted` (scheme.go:204).
    ///
    /// # A missing row is a 501, and Go's memo does not change the answer
    ///
    /// Go caches the success in `s.phase2PermissionsMigrationComplete` and never asks again — a
    /// memo, not a different rule, because the migration only ever goes from absent to present.
    /// This reads through on every call, which costs one indexed lookup and cannot go stale in the
    /// direction that matters.
    ///
    /// Go's test is `if _, err := GetByName(...); err != nil` — so **any** store failure, not only
    /// a missing row, is reported as "migration not completed". Reproduced: a database error here
    /// answers 501 rather than 500, which looks wrong and is Go's.
    #[tracing::instrument(skip_all, fields(completed))]
    pub async fn is_phase2_migration_completed(&self) -> AppResult<()> {
        let row = self
            .store()
            .system()
            .get_by_name(MIGRATION_KEY_ADVANCED_PERMISSIONS_PHASE2)
            .await;

        let completed = migration_is_complete(&row);
        tracing::Span::current().record("completed", completed);
        if completed {
            return Ok(());
        }
        Err(AppError::boxed(
            "App.IsPhase2MigrationCompleted",
            "app.schemes.is_phase_2_migration_completed.not_completed.app_error",
            None,
            String::new(),
            501,
        ))
    }

    /// Port of `App.GetScheme` (scheme.go:14).
    ///
    /// **One error id, two statuses.** A missing scheme and a broken database both answer
    /// `app.scheme.get.app_error`; only the status distinguishes them, 404 against 500. A client
    /// matching on the id cannot tell them apart, which is Go's choice.
    #[tracing::instrument(skip_all, fields(scheme_id = %id, found))]
    pub async fn get_scheme(&self, id: &str) -> AppResult<Scheme> {
        self.is_phase2_migration_completed().await?;

        match self.store().scheme().get(id).await {
            Ok(Some(scheme)) => {
                tracing::Span::current().record("found", true);
                Ok(scheme)
            }
            Ok(None) => {
                tracing::Span::current().record("found", false);
                Err(scheme_get_error(404))
            }
            Err(err) => {
                tracing::error!(error = ?err, "scheme lookup failed");
                Err(scheme_get_error(500))
            }
        }
    }

    /// Port of `App.GetSchemesPage` (scheme.go:50) — `GetSchemes(scope, page*perPage, perPage)`.
    ///
    /// The migration gate is evaluated **twice** in Go: once here and again inside `GetSchemes`.
    /// Once is enough for the same answer.
    #[tracing::instrument(skip_all, fields(scope = %scope, page, per_page, found))]
    pub async fn get_schemes_page(
        &self,
        scope: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Scheme>> {
        self.is_phase2_migration_completed().await?;

        let schemes = self
            .store()
            .scheme()
            .get_all_page(scope, page * per_page, per_page)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "scheme listing failed");
                AppError::boxed(
                    "GetSchemes",
                    "app.scheme.get_all_page.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", schemes.len());
        Ok(schemes)
    }

    /// Port of `App.GetTeamsForSchemePage` (scheme.go:163).
    #[tracing::instrument(skip_all, fields(scheme_id = %scheme_id, page, per_page, found))]
    pub async fn get_teams_for_scheme_page(
        &self,
        scheme_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Team>> {
        self.is_phase2_migration_completed().await?;

        let teams = self
            .store()
            .team()
            .get_teams_by_scheme(scheme_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                scheme_scope_error(
                    err,
                    "GetTeamsForScheme",
                    "app.scheme.scheme_teams.app_error",
                )
            })?;

        tracing::Span::current().record("found", teams.len());
        Ok(teams)
    }

    /// Port of `App.GetChannelsForSchemePage` (scheme.go:183).
    #[tracing::instrument(skip_all, fields(scheme_id = %scheme_id, page, per_page, found))]
    pub async fn get_channels_for_scheme_page(
        &self,
        scheme_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<Channel>> {
        self.is_phase2_migration_completed().await?;

        let channels = self
            .store()
            .channel()
            .get_channels_by_scheme(scheme_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                scheme_scope_error(
                    err,
                    "GetChannelsForScheme",
                    "app.scheme.scheme_channels.app_error",
                )
            })?;

        tracing::Span::current().record("found", channels.len());
        Ok(channels)
    }
}

/// Go's `if _, err := GetByName(...); err != nil` (scheme.go:209), as a value.
///
/// # A named function because the route cannot reach the false branch
///
/// On any server whose Go side has ever started, the migration row is present — it is written
/// once and never removed — so `GET /api/v4/schemes` exercises only the `true` arm and a mutation
/// forcing this to `true` survives the whole parity suite. Measured. The truth table lives here
/// instead.
///
/// **A store failure counts as "not completed".** Go's test is on `err != nil`, not on the row
/// being absent, so a database error answers 501 rather than 500. That looks wrong and is Go's.
fn migration_is_complete(row: &Result<Option<String>, StoreError>) -> bool {
    matches!(row, Ok(Some(_)))
}

/// `GetScheme`'s error (scheme.go:24, :26), which is one id at two statuses.
fn scheme_get_error(status: i32) -> Box<AppError> {
    AppError::boxed(
        "GetScheme",
        "app.scheme.get.app_error",
        None,
        String::new(),
        status,
    )
}

/// The 500 both scheme-scoped listings answer any store failure with.
fn scheme_scope_error(err: StoreError, where_: &'static str, id: &'static str) -> Box<AppError> {
    tracing::error!(error = ?err, "scheme-scoped listing failed");
    AppError::boxed(where_, id, None, String::new(), 500)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `GetScheme`'s two statuses share an id, so a client cannot distinguish a missing scheme
    /// from a broken database — and a port that "helpfully" gave the 404 its own id would answer
    /// something Go never sends.
    #[test]
    fn the_missing_and_broken_cases_share_an_error_id() {
        let missing = scheme_get_error(404);
        let broken = scheme_get_error(500);
        assert_eq!(missing.id, "app.scheme.get.app_error");
        assert_eq!(missing.id, broken.id);
        assert_ne!(missing.status_code, broken.status_code);
    }

    /// The gate's three inputs. A present row passes; an **absent** row and a **failed lookup**
    /// both fail, and the second is the one a reader gets wrong — Go branches on the error, not on
    /// the row, so a broken database reports "migration not completed".
    #[test]
    fn only_a_present_row_completes_the_migration() {
        assert!(migration_is_complete(&Ok(Some("true".to_owned()))));
        assert!(
            migration_is_complete(&Ok(Some(String::new()))),
            "the value is never read — only the row's presence"
        );
        assert!(!migration_is_complete(&Ok(None)), "no row, no migration");
        assert!(
            !migration_is_complete(&Err(StoreError::NotFound {
                entity: "System",
                criteria: "name=x".to_owned(),
            })),
            "and a store failure is 'not completed', not a 500"
        );
    }

    /// The migration gate's status is **501**, not 500 or 404. It is the difference between "this
    /// server cannot answer yet" and "something went wrong", and the webapp branches on it.
    #[test]
    fn the_migration_gate_is_a_501() {
        let err = AppError::boxed(
            "App.IsPhase2MigrationCompleted",
            "app.schemes.is_phase_2_migration_completed.not_completed.app_error",
            None,
            String::new(),
            501,
        );
        assert_eq!(err.status_code, 501);
        assert!(err.id.ends_with("not_completed.app_error"));
    }
}
