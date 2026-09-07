//! Port of `App.GetAuditsPage` (channels/app/audit.go:59).
//!
//! One store call and a two-way error split. The split is the whole content of the function, and
//! it is the thing a port gets wrong: `ErrOutOfBounds` is a **400** with its own id, and every
//! other store failure is a 500 with a different one.

use mm_model::audit::Audits;
use mm_model::utils::{AppError, AppResult};
use mm_store::{AuditStore, StoreError};

use crate::App;

impl App {
    /// Port of `App.GetAuditsPage` (audit.go:59).
    ///
    /// Go multiplies `page * perPage` into an offset in `int` arithmetic. Both values arrive from
    /// `web.ParamsFromRequest`, which clamps `per_page` to 200 and floors `page` at 0, so the
    /// product cannot overflow through the REST API; `i64` here removes even the theoretical
    /// question without changing any reachable answer.
    ///
    /// **An empty `user_id` is not a bug here.** `getAudits` (`GET /api/v4/audits`) passes one
    /// deliberately, and the store answers it with the unfiltered page — see
    /// [`mm_store::AuditStore::get`]. Only `getUserAudits` narrows by user.
    #[tracing::instrument(skip_all, fields(user_id, page, per_page, found))]
    pub async fn get_audits_page(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Audits> {
        let audits = self
            .store()
            .audit()
            .get(user_id, page * per_page, per_page)
            .await
            .map_err(audits_error)?;

        tracing::Span::current().record("found", audits.0.len());
        Ok(audits)
    }
}

/// Go's `switch` on the store error (audit.go:62-69), which is two ids and two statuses.
///
/// `app.audit.get.limit.app_error` / 400 is unreachable through either audit route — `per_page`
/// is clamped to 200 long before the store's own 1000 — so it is ported for fidelity and has no
/// test through a route.
fn audits_error(err: StoreError) -> Box<AppError> {
    if err.is_out_of_bounds() {
        return AppError::boxed(
            "GetAuditsPage",
            "app.audit.get.limit.app_error",
            None,
            String::new(),
            400,
        );
    }
    tracing::error!(error = ?err, "audit lookup failed");
    AppError::boxed(
        "GetAuditsPage",
        "app.audit.get.finding.app_error",
        None,
        String::new(),
        500,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two branches, and that they do **not** share an id or a status. A port that collapsed
    /// them would answer 500 to a client's oversized page size.
    #[test]
    fn out_of_bounds_is_a_400_and_everything_else_is_a_500() {
        let limit = audits_error(StoreError::OutOfBounds { limit: 1001 });
        assert_eq!(limit.id, "app.audit.get.limit.app_error");
        assert_eq!(limit.status_code, 400);

        let other = audits_error(StoreError::NotFound {
            entity: "Audit",
            criteria: "userId=x".to_owned(),
        });
        assert_eq!(other.id, "app.audit.get.finding.app_error");
        assert_eq!(other.status_code, 500);
    }

    /// `errors.As` finds `ErrOutOfBounds` and nothing else; a not-found is **not** a 404 here,
    /// because Go's `default` arm swallows it into the same 500 as a driver failure.
    #[test]
    fn a_not_found_is_not_promoted_to_a_404() {
        let err = audits_error(StoreError::NotFound {
            entity: "Audit",
            criteria: "userId=x".to_owned(),
        });
        assert_ne!(err.status_code, 404, "Go's default arm is a 500");
    }
}
