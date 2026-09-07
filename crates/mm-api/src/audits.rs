//! The two routes over the `Audits` table: `getUserAudits` (channels/api4/user.go:2827) at
//! `GET /api/v4/users/{user_id}/audits`, and `getAudits` (channels/api4/system.go:44) at
//! `GET /api/v4/audits`.
//!
//! The first is the webapp's *Profile → Security → View Access History* panel; the second is the
//! system console's server-wide audit log. They are the **same app call** with a different user
//! id — the server-wide one passes the empty string, which the store reads as "no filter".
//!
//! # They agree on almost nothing else
//!
//! | | `getUserAudits` | `getAudits` |
//! |---|---|---|
//! | permission | `edit_other_users`, via `SessionHasPermissionToUser` | `read_audits`, system-scoped |
//! | user id | `RequireUserId`, `me` resolved | empty, always |
//! | empty page | `null` | `null` |
//!
//! The last row is the one worth checking rather than assuming: both go through
//! `SqlAuditStore.Get`, whose `var audits model.Audits` is a nil slice, so **both** answer `null`
//! and not `[]`. See [`encode_audits`].
//!
//! # No etag, unlike its neighbours
//!
//! `model.Audits` **has** an `Etag()` (model/audits.go:8) and this route does not call it. Two
//! routes away, `getUsers` does. So the absence is Go's, not an omission here: this handler goes
//! straight from the permission check to `json.NewEncoder(w).Encode`, and a client that sends
//! `If-None-Match` gets a 200 with a full body.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_READ_AUDITS, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, resolve_me};
use crate::error::ApiError;

/// Port of `getUserAudits` (user.go:2827).
///
/// # The permission is `edit_other_users`, not a read permission
///
/// `SessionHasPermissionToUser` is the self-or-admin check, and its failure reports
/// `PermissionEditOtherUsers` — a *write* permission gating a read. That is Go's choice and it is
/// reproduced: a role granting some hypothetical "read other users" would still be refused here.
///
/// # `me` is resolved before the id is validated
///
/// `RequireUserId` (web/context.go:301) substitutes the session's user id first and only then
/// calls `IsValidId`, so `/users/me/audits` is a 200 and not a 400. See [`resolve_me`].
///
/// # Pagination, and the bound that cannot be reached
///
/// `page` and `per_page` come from `web.ParamsFromRequest`, which floors `page` at 0, defaults
/// `per_page` to 60 and clamps it to **200**. The store refuses a limit above **1000**
/// (`audit_store.go:54`) with a 400 of its own — unreachable from here, and ported anyway; see
/// `mm_app::audit`.
///
/// # Wire format: an empty page is `null`, not `[]`
///
/// `json.NewEncoder(w).Encode(audits)`, so there **is** a trailing newline. And a user with no
/// audit rows gets the four bytes `null`: `SqlAuditStore.Get` declares `var audits model.Audits`
/// — a **nil** slice — and sqlx's `Select` appends into it, so a no-row query leaves it nil and
/// `json.Marshal` writes `null`. The bot store two files away does `bots := []*model.Bot{}` and
/// therefore answers `[]` for the same shape of query, which is how deliberate the distinction
/// is: it is the initialiser, not the query.
///
/// This was **measured, and it is the opposite of what the first version of this port assumed**
/// — a fresh user's audits really do come back as `null` on the running server.
#[tracing::instrument(skip_all, fields(user_id = %user_id, page, per_page, count))]
pub async fn get_user_audits(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `me` first, then `RequireUserId` (web/context.go:301).
    let user_id = resolve_me(&user_id, &session);
    is_valid_id(user_id)
        .then_some(())
        .ok_or_else(|| ApiError::invalid_url_param("user_id"))?;

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let audits = state.app.get_audits_page(user_id, page, per_page).await?;
    tracing::Span::current().record("count", audits.0.len());

    let mut body = encode_audits(&audits)?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// The body bytes, without the encoder's newline.
///
/// Split out so the nil-versus-empty decision is testable without a server; it is the one place
/// this route can silently change a JSON *type*.
fn encode_audits(audits: &mm_model::audit::Audits) -> Result<Vec<u8>, ApiError> {
    if audits.0.is_empty() {
        // Go's nil slice — see the note on the handler. `serde` would write `[]` here, which is a
        // different value of a different type to every client that checks.
        return Ok(b"null".to_vec());
    }
    serde_json::to_vec(audits).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the audits");
        ApiError::from(AppError::new(
            "getUserAudits",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// Port of `getAudits` (api4/system.go:44).
///
/// # `read_audits` is system-scoped, and it is not a sysconsole permission
///
/// `PermissionReadAudits` is one of the original coarse permissions, granted to `system_admin`
/// only on a stock server. A role holding `sysconsole_read_compliance_custom_policies` — which
/// *is* how the console usually gates this page — does not get in here.
///
/// # No user filter, and that is the difference from its sibling
///
/// `GetAuditsPage(c.AppContext, "", page, perPage)` — the empty id is deliberate and the store
/// drops its `WHERE` for it, returning every user's rows. See
/// [`mm_store::AuditStore::get`], where the earlier port of that branch was wrong.
///
/// # The audit record's two meta keys disagree about the page size
///
/// Go records `"page": c.Params.Page` and `"audits_per_page": c.Params.LogsPerPage` — the
/// **logs** per-page parameter, not the `PerPage` the query actually used (system.go:57-58). We
/// write no audit rows ([D-087]: the `Audits` table is Go's to write), so nothing here reproduces
/// it; it is recorded because a reader comparing the two servers' audit logs will notice.
///
/// Wire format is `getUserAudits`'s exactly: `json.NewEncoder(w).Encode`, trailing newline, and
/// `null` for an empty page.
#[tracing::instrument(skip_all, fields(page, per_page, count))]
pub async fn get_audits(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_AUDITS)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_AUDITS],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    // The empty user id is the route. See the store's `get`.
    let audits = state.app.get_audits_page("", page, per_page).await?;
    tracing::Span::current().record("count", audits.0.len());

    let mut body = encode_audits(&audits)?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::audit::{Audit, Audits};

    /// The nil slice. `Audits` is `#[serde(transparent)]` over a `Vec`, so serde alone would give
    /// `[]` — the handler has to choose, and this pins which way.
    #[test]
    fn an_empty_page_is_null_and_not_an_empty_array() {
        let body = encode_audits(&Audits(Vec::new())).expect("encodes");
        assert_eq!(body, b"null");
        assert_ne!(
            body, b"[]",
            "the initialiser in Go's store is nil, not a literal"
        );
    }

    /// Every key is present and unconditional — `model.Audit` carries no `omitempty`, so an audit
    /// row with an empty `session_id` still puts the key on the wire. The pre-login row that Go
    /// writes with `extra_info: "authenticated"` has exactly that shape.
    #[test]
    fn every_audit_key_is_written_even_when_empty() {
        let audits = Audits(vec![Audit {
            id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            create_at: 1_701_355_039_000,
            user_id: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            action: "/api/v4/users/login".to_owned(),
            extra_info: "authenticated".to_owned(),
            ip_address: "172.18.0.1".to_owned(),
            session_id: String::new(),
        }]);

        // Asserted on the **bytes**, not through `serde_json::Value`: a `Value`'s object is a
        // `BTreeMap`, so parsing and reading the keys back sorts them alphabetically and any
        // field order at all would pass. That mistake was made once in this suite already.
        let body = String::from_utf8(encode_audits(&audits).expect("encodes")).expect("utf-8");
        assert_eq!(
            body,
            concat!(
                r#"[{"id":"y9i4er48tt8bukijy7i3u5y9ar","create_at":1701355039000,"#,
                r#""user_id":"6rtg4qbe5bn55mw5t6gphxyaxa","action":"/api/v4/users/login","#,
                r#""extra_info":"authenticated","ip_address":"172.18.0.1","session_id":""}]"#
            ),
            "Go's field order, and `session_id` is present though empty"
        );
    }
}
