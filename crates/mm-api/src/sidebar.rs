//! Port of the **read** side of `BaseRoutes.ChannelCategories` (api4/api.go:231), handlers in
//! `api4/channel_category.go`:
//!
//! | route | Go handler |
//! |---|---|
//! | `GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories` | `getCategoriesForTeamForUser` (:14) |
//! | `GET …/channels/categories/order` | `getCategoryOrderForTeamForUser` (:95) |
//! | `GET …/channels/categories/{category_id}` | `getCategoryForTeamForUser` (:166) |
//!
//! The five writes on the same three paths (`POST`/`PUT` on the collection, `PUT` on `/order`,
//! `PUT`/`DELETE` on the singular) stay forwarded, through [`crate::partially_migrated`]'s method
//! fallback. `tests/parity_sidebar_router.rs` asserts that over HTTP, because "still forwarded"
//! is a claim about the router rather than about anything in this file.
//!
//! # `order` beside `{category_id}`: both routers agree, for different reasons
//!
//! `order` matches `[A-Za-z0-9_-]+`, so the literal and the parameter both match
//! `GET …/categories/order`. gorilla resolves that by **registration order** and
//! `api4/channel.go` registers `/order` at :80, before `/{category_id:…}` at :82 — so Go serves
//! `getCategoryOrderForTeamForUser`. axum resolves it by **specificity**: a static segment always
//! beats a parameter, regardless of the order the routes were added.
//!
//! Same answer, and the reasons being different is exactly why it is pinned by a test rather
//! than by a comment. Contrast `/teams/name/{team_name}` (see `teams::TEAM_BY_NAME_SHADOWED_
//! LITERALS`), where Go's registration order is the *reverse* of axum's preference and the
//! handler has to forward the difference by hand. There is no such case here: `/order` is the
//! only literal under `/categories/`, and it is registered first.
//!
//! # Wire framing differs between the three, in one Go file
//!
//! `getCategoriesForTeamForUser` and `getCategoryForTeamForUser` both end in `json.Marshal`
//! followed by `w.Write` — **no trailing newline**. `getCategoryOrderForTeamForUser` ends in
//! `json.NewEncoder(w).Encode` — **a trailing newline** ([D-086]). Three handlers, two framings,
//! forty lines apart. The parity suite compares raw bytes for exactly this reason.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::sidebar::SidebarCategoriesResult;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_VIEW_TEAM, Permission, make_permission_error,
};
use mm_model::sidebar_category::is_valid_category_id;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

/// Go's mux class for `{category_id}`: `[A-Za-z0-9_-]+` (api4/channel.go:82).
///
/// **Wider than the `[A-Za-z0-9]+` every `*_id` segment uses**, because a default category's id
/// is `{type}_{userId}_{teamId}` — underscores and all. That is why the route registers the
/// parameter as `{category}` rather than `{category_id}`: the shared id-charset middleware
/// ([`crate::parameter_is_id_shaped`]) keys off the `_id` suffix and would forward every default
/// category to Go, which is to say the common case of the whole route.
///
/// A segment outside this class is a gorilla mux 404 before any handler runs, so it is forwarded
/// and Go answers its own — the same rule `roles::segment_matches_role_name_mux` follows.
fn segment_matches_category_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `c.RequireUserId().RequireTeamId()` (channel_category.go:15) as one call, so the **order** is
/// testable.
///
/// User first, team second — the reverse of `teams::validate_team_and_user_ids`, which serves a
/// handler that chains them the other way round. Neither order is observable from a response
/// body ([D-092] leaves `message` as the raw id and `params` is not on the wire), so the only
/// place it can be pinned is a unit test on this function.
///
/// `me` is resolved by the caller before this runs, matching `RequireUserId` (web/context.go:301)
/// which substitutes the session's id *before* validating.
// See `channels::require_id` for why the large-error lint is allowed across this crate.
#[allow(clippy::result_large_err)]
fn validate_user_and_team_ids(user_id: &str, team_id: &str) -> Result<(), ApiError> {
    require_id(user_id, "user_id")?;
    require_id(team_id, "team_id")?;
    Ok(())
}

/// The two permission gates every handler in `channel_category.go` runs, in Go's order,
/// returning the permission the refusal names — or `None` when both grant.
///
/// Lifted out of the handlers for the reason `teams::team_unread_denied` gives: **the order is
/// not observable over HTTP.** `WipeDetailed` empties `detailed_error` outside developer mode
/// (model/utils.go:339), so a caller who fails both gates gets a byte-identical 403 either way.
/// What the unit test on this function pins is that the team check is not even *evaluated* when
/// the first gate refuses — cheap as well as correct — and that the refusal names
/// `edit_other_users` for the first and `view_team` for the second.
///
/// **The first gate is not the same function on all three routes.** The two list handlers pass
/// `SessionHasPermissionToUser`; the singular one passes `SessionHasPermissionToCategory`, which
/// shares only the `edit_other_users` branch (see
/// `mm_app::App::session_has_permission_to_category`). Both refusals name
/// `model.PermissionEditOtherUsers`, which is why the two are so easy to confuse and why the
/// *difference* is asserted over HTTP instead: `parity_sidebar_category.rs` asks for a category
/// belonging to somebody else while naming oneself in the path, which the user gate would allow
/// and the category gate refuses.
async fn sidebar_denied<F, FFut, T, TFut>(
    first_gate_allowed: F,
    team_allowed: T,
) -> Option<&'static Permission>
where
    F: FnOnce() -> FFut,
    FFut: std::future::Future<Output = bool>,
    T: FnOnce() -> TFut,
    TFut: std::future::Future<Output = bool>,
{
    if !first_gate_allowed().await {
        return Some(&PERMISSION_EDIT_OTHER_USERS);
    }
    if !team_allowed().await {
        return Some(&PERMISSION_VIEW_TEAM);
    }
    None
}

/// `json.Marshal` + `w.Write`: a JSON body with **no** trailing newline.
fn json_body(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

fn marshal_error(where_: &'static str) -> Response {
    ApiError(mm_model::utils::AppError::new(
        where_,
        "api.marshal_error",
        None,
        String::new(),
        500,
    ))
    .into_response()
}

/// Port of `getCategoriesForTeamForUser` (api4/channel_category.go:14) —
/// `GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories`.
///
/// The webapp fetches this on every team load, so it is the busiest route in the family by a
/// wide margin, and its body is the sidebar a user sees.
///
/// # What the body contains that the database does not
///
/// Most of a normal user's Channels category is **not** in `SidebarChannels`. Joining a channel
/// writes a membership row and nothing else, and the store appends every such orphan on the way
/// out — see `mm_store::sidebar_category_store`. A port that returned the join alone would
/// answer with a nearly empty sidebar and look entirely plausible doing it.
///
/// # The empty case is forwarded, not answered
///
/// Zero categories means Go would **create** the three defaults inside this GET, migrating the
/// user's favourites into `SidebarChannels` as it goes. This server does not write here, so the
/// request is forwarded and Go performs its own migration; see
/// [`mm_app::sidebar::SidebarCategoriesResult`]. Reachable only for an account whose rows are
/// missing, since joining a team creates them.
///
/// # Wire format
///
/// `json.Marshal` then `w.Write` (:41) — **no trailing newline**, unlike the `/order` sibling.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, forwarded))]
pub async fn get_categories_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_user_and_team_ids(&user_id, &team_id) {
        return err.into_response();
    }

    let denial = sidebar_denied(
        || async {
            state
                .app
                .session_has_permission_to_user(&session.0, &user_id)
                .await
        },
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return ApiError(*make_permission_error(&session.0, &[permission])).into_response();
    }

    let categories = match state
        .app
        .get_sidebar_categories_for_team_for_user(&user_id, &team_id)
        .await
    {
        Ok(SidebarCategoriesResult::Found(categories)) => categories,
        Ok(SidebarCategoriesResult::NeedsInitialCategories) => {
            tracing::Span::current().record("forwarded", true);
            tracing::info!(
                "no sidebar categories for this user and team; forwarding so Go runs its migration"
            );
            return crate::proxy::forward_to_go(State(state), request).await;
        }
        Err(err) => return ApiError(err).into_response(),
    };
    tracing::Span::current().record("forwarded", false);

    match serde_json::to_vec(&*categories) {
        Ok(body) => json_body(body),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the sidebar categories");
            marshal_error("getCategoriesForTeamForUser")
        }
    }
}

/// Port of `getCategoryOrderForTeamForUser` (api4/channel_category.go:95) —
/// `GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories/order`.
///
/// A bare JSON array of category ids, in `SortOrder` order — the same ids the collection route
/// puts in its `order` key, from the same table, by a query that does not touch
/// `SidebarChannels` at all.
///
/// # It does *not* share the collection route's create-on-empty branch
///
/// `GetSidebarCategoryOrder` has no `len(...) == 0` fallback (channel_category.go:73), so a user
/// whose rows are missing gets `[]` here and a Go-side migration from `/categories`. Nothing is
/// forwarded from this handler.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(order)` (:117) — an *encoder*, so the body carries a **trailing
/// newline** where its two siblings' `w.Write` does not. The store builds `[]string{}`, so an
/// empty answer is `[]\n` and never `null\n`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_category_order_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_user_and_team_ids(&user_id, &team_id) {
        return err.into_response();
    }

    let denial = sidebar_denied(
        || async {
            state
                .app
                .session_has_permission_to_user(&session.0, &user_id)
                .await
        },
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return ApiError(*make_permission_error(&session.0, &[permission])).into_response();
    }

    let order = match state
        .app
        .get_sidebar_category_order(&user_id, &team_id)
        .await
    {
        Ok(order) => order,
        Err(err) => return ApiError(err).into_response(),
    };
    tracing::Span::current().record("count", order.len());

    match serde_json::to_vec(&order) {
        Ok(mut body) => {
            // `json.NewEncoder(w).Encode` writes the newline; the two siblings' `w.Write` does not.
            body.push(b'\n');
            json_body(body)
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the sidebar category order");
            marshal_error("getCategoryOrderForTeamForUser")
        }
    }
}

/// Port of `getCategoryForTeamForUser` (api4/channel_category.go:166) —
/// `GET /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}`.
///
/// # A missing category is a 403, not a 404
///
/// The first gate is `SessionHasPermissionToCategory`, which **fetches the category itself** and
/// denies when the lookup fails (authorization.go:246). So `GetSidebarCategory`'s own 404 is
/// unreachable from this route for anyone without `edit_other_users`: a syntactically valid id
/// that names no row answers `api.context.permissions.app_error` with a 403. Measured against
/// the running Go server, not inferred — and it is the answer a client has to handle.
///
/// # The `user_id` in the path is checked against the row, not just against the session
///
/// The gate compares `category.UserId` twice: to `session.UserId` **and** to the path's
/// `user_id`. Naming yourself in the path does not get you somebody else's category, and naming
/// somebody else does not get you your own. `SessionHasPermissionToUser` — the first gate of the
/// two list routes — would allow both, and its self-shortcut makes the difference invisible in
/// any test where the caller asks about their own categories.
///
/// # Wire format
///
/// `json.Marshal` then `w.Write` (:190) — **no trailing newline**, like the collection route and
/// unlike `/order`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, category_id = %category, forwarded))]
pub async fn get_category_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id, category)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !segment_matches_category_mux(&category) {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!("category segment is outside Go's mux charset; forwarding for Go's 404");
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    // `c.RequireUserId().RequireTeamId().RequireCategoryId()` — the category is validated last,
    // so a bad user id and a bad category id together report `user_id`.
    if let Err(err) = validate_user_and_team_ids(&user_id, &team_id) {
        return err.into_response();
    }
    if !is_valid_category_id(&category) {
        return ApiError::invalid_url_param("category_id").into_response();
    }

    let denial = sidebar_denied(
        || async {
            state
                .app
                .session_has_permission_to_category(&session.0, &user_id, &team_id, &category)
                .await
        },
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return ApiError(*make_permission_error(&session.0, &[permission])).into_response();
    }

    let category = match state.app.get_sidebar_category(&category).await {
        Ok(category) => category,
        Err(err) => return ApiError(err).into_response(),
    };

    match serde_json::to_vec(&category) {
        Ok(body) => json_body(body),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the sidebar category");
            marshal_error("getCategoryForTeamForUser")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const USER: &str = "y9i4er48tt8bukijy7i3u5y9ar";
    const TEAM: &str = "n3ocs5fepw8qt1mb3psko5oq7y";

    /// Go's chain is `RequireUserId().RequireTeamId()`, and each link returns early once an error
    /// is set — so with both ids malformed the **user** one wins. `teams.rs`'s helper is the
    /// mirror image, because its handler chains them the other way; getting the two the same way
    /// round would be invisible outside a test like this one.
    #[test]
    fn the_user_id_is_validated_before_the_team_id() {
        let err = validate_user_and_team_ids("nope", "also-nope").expect_err("both are invalid");
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::Value::String("user_id".to_owned())),
            "with both malformed, Go reports user_id"
        );
        assert_eq!(err.0.status_code, 400);
        assert_eq!(err.0.id, "api.context.invalid_url_param.app_error");

        let err = validate_user_and_team_ids(USER, "nope").expect_err("the team id is invalid");
        assert_eq!(
            err.0.params.as_ref().and_then(|p| p.get("Name")),
            Some(&serde_json::Value::String("team_id".to_owned())),
        );

        assert!(validate_user_and_team_ids(USER, TEAM).is_ok());
    }

    /// The first gate refuses in `edit_other_users`' name and the team check is **not run**.
    #[tokio::test]
    async fn the_first_gate_refuses_before_the_team_gate_is_evaluated() {
        let team_calls = AtomicUsize::new(0);
        let denial = sidebar_denied(
            || async { false },
            || async {
                team_calls.fetch_add(1, Ordering::SeqCst);
                true
            },
        )
        .await;

        assert_eq!(denial.map(|p| &*p.id), Some("edit_other_users"));
        assert_eq!(
            team_calls.load(Ordering::SeqCst),
            0,
            "Go returns before SessionHasPermissionToTeam"
        );
    }

    #[tokio::test]
    async fn the_team_gate_refuses_in_view_teams_name() {
        let denial = sidebar_denied(|| async { true }, || async { false }).await;
        assert_eq!(denial.map(|p| &*p.id), Some("view_team"));
    }

    #[tokio::test]
    async fn both_gates_granting_is_no_denial() {
        assert!(
            sidebar_denied(|| async { true }, || async { true })
                .await
                .is_none()
        );
    }

    /// `[A-Za-z0-9_-]+`, wider than the id charset in both directions that matter: a default
    /// category id carries two underscores, and a hyphen is legal even though `NewId` never
    /// emits one.
    #[test]
    fn the_category_segment_charset_is_gos() {
        assert!(segment_matches_category_mux(USER));
        assert!(segment_matches_category_mux(&format!(
            "favorites_{USER}_{TEAM}"
        )));
        assert!(segment_matches_category_mux("a-b_C9"));

        assert!(!segment_matches_category_mux(""), "gorilla's + needs one");
        assert!(!segment_matches_category_mux("has.dot"));
        assert!(!segment_matches_category_mux("has space"));
        assert!(!segment_matches_category_mux("has/slash"));
    }

    /// The charset and `IsValidCategoryId` are **different checks with different answers**, and
    /// both run. `catgory` passes the mux and fails validation (a 400 we produce); `a.b` fails
    /// the mux and is forwarded (a 404 Go produces). Conflating them would swap a 400 for a 404.
    #[test]
    fn the_mux_charset_is_not_the_validity_check() {
        assert!(segment_matches_category_mux("notacategory"));
        assert!(!is_valid_category_id("notacategory"));

        let default_id = format!("favorites_{USER}_{TEAM}");
        assert!(segment_matches_category_mux(&default_id));
        assert!(
            is_valid_category_id(&default_id),
            "the id shape the webapp actually sends"
        );
    }
}
