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
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_VIEW_TEAM, Permission, make_permission_error,
};
use mm_model::sidebar_category::{SidebarCategoryWithChannels, is_valid_category_id};
use mm_model::utils::PAYLOAD_PARSE_ERROR;

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
    ApiError::from(mm_model::utils::AppError::new(
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
/// # This `GET` writes, on first contact
///
/// Zero categories means Go **creates** the three defaults inside this handler, migrating the
/// user's favourites into `SidebarChannels` as it goes — see
/// [`mm_app::App::get_sidebar_categories_for_team_for_user`]. Reachable only for an account whose
/// rows are missing, since joining a team creates them.
///
/// # Wire format
///
/// `json.Marshal` then `w.Write` (:41) — **no trailing newline**, unlike the `/order` sibling,
/// and with Go's HTML escaping, which `serde_json` does not apply. A category named `Q&A` is nine
/// bytes different between the two without [`marshal_or_500`].
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
pub async fn get_categories_for_team_for_user(
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
        return ApiError::from(make_permission_error(&session.0, &[permission])).into_response();
    }

    let categories = match state
        .app
        .get_sidebar_categories_for_team_for_user(&user_id, &team_id)
        .await
    {
        Ok(categories) => categories,
        Err(err) => return ApiError::from(err).into_response(),
    };

    marshal_or_500(&categories, "getCategoriesForTeamForUser")
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
        return ApiError::from(make_permission_error(&session.0, &[permission])).into_response();
    }

    let order = match state
        .app
        .get_sidebar_category_order(&user_id, &team_id)
        .await
    {
        Ok(order) => order,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("count", order.len());

    match mm_model::utils::go_json_marshal(&order) {
        Ok(mut body) => {
            // `json.NewEncoder(w).Encode` writes the newline; the two siblings' `w.Write` does not.
            body.push('\n');
            json_body(body.into_bytes())
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
        return ApiError::from(make_permission_error(&session.0, &[permission])).into_response();
    }

    let category = match state.app.get_sidebar_category(&category).await {
        Ok(category) => category,
        Err(err) => return ApiError::from(err).into_response(),
    };

    marshal_or_500(&category, "getCategoryForTeamForUser")
}

// ---------------------------------------------------------------------------
// The five writes
// ---------------------------------------------------------------------------

/// Read the whole body, or Go's `SetInvalidParamWithErr("category", err)`.
///
/// The parameter name is `category` on **four** of the five writes, including the one whose body
/// is an array; only `/order` names something else (see
/// [`update_category_order_for_team_for_user`]). So a client sending a malformed array to
/// `PUT …/categories` is told `category`, singular.
async fn read_body(body: axum::body::Body) -> Result<axum::body::Bytes, ApiError> {
    axum::body::to_bytes(body, usize::MAX).await.map_err(|err| {
        tracing::warn!(error = %err, "could not read the request body");
        ApiError::invalid_param("category")
    })
}

/// Port of `validateSidebarCategory` (api4/channel_category.go:259) and
/// `validateSidebarCategories` (:273), which differ only in fetching the channel list once.
///
/// # It is a filter, not a validator, and it mutates the request
///
/// Despite the name nothing is rejected: `validateSidebarCategoryChannels` (:301) **drops** every
/// channel id the user is not a member of, logs it, and de-duplicates the rest. So a request
/// naming somebody else's private channel succeeds and simply does not contain it — a port that
/// answered 400 here would refuse requests Go accepts.
///
/// # The one branch that *is* an error is a 400 with an unexpected id
///
/// A failure fetching the channel list becomes
/// `model.NewAppError("validateSidebarCategory", "api.invalid_channel", nil, "", 400)`. That is
/// reachable: `GetChannelsForTeamForUser` answers **404** when the user is a member of no channel
/// in the team, and this converts it to a 400 whose id says `api.invalid_channel` — nothing to do
/// with any channel the caller named.
///
/// # `channel_ids` is never `null` after this
///
/// `RemoveDuplicateStringsNonSort` returns `list := []string{}`, a non-nil slice, even for a nil
/// input. So every create/update answer carries `"channel_ids":[]` at worst, and the `null` that
/// `mm_model::sidebar_category` documents as reachable is not reachable through these routes.
async fn validate_sidebar_categories(
    state: &AppState,
    team_id: &str,
    user_id: &str,
    categories: &mut [SidebarCategoryWithChannels],
) -> Result<(), ApiError> {
    // `IncludeDeleted: true, LastDeleteAt: 0` — an archived channel may stay in a category.
    let opts = mm_model::channel::ChannelSearchOpts {
        include_deleted: true,
        last_delete_at: 0,
        ..Default::default()
    };
    let channels = state
        .app
        .get_channels_for_team_for_user(team_id, user_id, &opts)
        .await
        .map_err(|err| {
            tracing::debug!(error = %err, "the caller's channel list is unavailable");
            ApiError::from(mm_model::utils::AppError::new(
                "validateSidebarCategory",
                "api.invalid_channel",
                None,
                String::new(),
                400,
            ))
        })?;

    let allowed: std::collections::HashSet<&str> = channels
        .0
        .iter()
        .map(|channel| channel.id.as_str())
        .collect();

    for category in categories {
        let mut filtered: Vec<String> = Vec::new();
        for channel_id in category.channel_ids.as_deref().unwrap_or_default() {
            if allowed.contains(channel_id.as_str()) {
                filtered.push(channel_id.clone());
            } else {
                tracing::info!(
                    user_id = %user_id,
                    channel_id = %channel_id,
                    "Stopping user from adding channel to their sidebar when they are not a member"
                );
            }
        }
        category.channel_ids = Some(mm_model::utils::remove_duplicate_strings_non_sort(
            &filtered,
        ));
    }

    Ok(())
}

/// The preamble every write on `…/channels/categories` shares: resolve `me`, validate the two
/// ids, then the two permission gates with `SessionHasPermissionToUser` as the first.
///
/// Returns the resolved user id, or the response to send.
// The error is a whole `Response`; see `channels::require_id` for why the lint is allowed
// across this crate.
#[allow(clippy::result_large_err)]
async fn user_and_team_write_gate(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: String,
    team_id: &str,
) -> Result<String, Response> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_user_and_team_ids(&user_id, team_id) {
        return Err(err.into_response());
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
                .session_has_permission_to_team(&session.0, team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return Err(
            ApiError::from(make_permission_error(&session.0, &[permission])).into_response(),
        );
    }

    Ok(user_id)
}

/// `json.Marshal` a write's answer with Go's HTML escaping, or Go's `api.marshal_error` 500.
///
/// **`serde_json::to_vec` is not a substitute.** Go escapes `<`, `>` and `&` inside strings and
/// serde_json does not, and `display_name` is arbitrary user text — so a category called
/// `Q&A` differs between the two servers by nine bytes unless this is used. Asserted over HTTP in
/// `parity/sidebar_category_writes.rs`.
fn marshal_or_500<T: serde::Serialize>(value: &T, where_: &'static str) -> Response {
    match mm_model::utils::go_json_marshal(value) {
        Ok(body) => json_body(body.into_bytes()),
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the sidebar write's answer");
            marshal_error(where_)
        }
    }
}

/// Port of `createCategoryForTeamForUser` (api4/channel_category.go:47) —
/// `POST /api/v4/users/{user_id}/teams/{team_id}/channels/categories`.
///
/// # The body's `user_id` and `team_id` must equal the path's, and the failure is a 400
///
/// One `if`: `err != nil || c.Params.UserId != request.UserId || c.Params.TeamId != request.TeamId`
/// — so a decode failure and a mismatched id are the same
/// `api.context.invalid_body_param.app_error` naming `category`. Note that `me` has already been
/// resolved by `RequireUserId`, so a body carrying the caller's real id and a path saying `me` is
/// accepted.
///
/// # Three fields of the body are ignored outright
///
/// `id`, `type` and `collapsed`. The store mints a fresh `NewId()`, forces
/// `type: "custom"`, and leaves `collapsed` false — see
/// `mm_store::sidebar_category_store::create_sidebar_category`. `display_name`, `sorting`,
/// `muted` and `channel_ids` are the four that carry.
///
/// # `200`, and no trailing newline
///
/// `w.Write(categoryJSON)` after `json.Marshal` — the same framing as the two migrated `GET`s and
/// not the encoder framing `/order` uses.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id))]
pub async fn create_category_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = match user_and_team_write_gate(&state, &session, user_id, &team_id).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let mut category: SidebarCategoryWithChannels = match serde_json::from_slice(&bytes) {
        Ok(category) => category,
        Err(err) => {
            tracing::debug!(error = %err, "category body did not decode");
            return ApiError::invalid_param("category").into_response();
        }
    };
    if category.category.user_id != user_id || category.category.team_id != team_id {
        return ApiError::invalid_param("category").into_response();
    }

    if let Err(err) = validate_sidebar_categories(
        &state,
        &team_id,
        &user_id,
        std::slice::from_mut(&mut category),
    )
    .await
    {
        return err.into_response();
    }

    match state
        .app
        .create_sidebar_category(&user_id, &team_id, &category)
        .await
    {
        Ok(created) => marshal_or_500(&created, "createCategoryForTeamForUser"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateCategoriesForTeamForUser` (api4/channel_category.go:203) —
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/channels/categories`.
///
/// # A category the caller does not own is a **400**, not a 403
///
/// The per-category gate is `SessionHasPermissionToCategory` and its refusal is
/// `c.SetInvalidParam("category")` — `api.context.invalid_body_param.app_error`, 400. Every other
/// permission refusal in this file is a 403 with `api.context.permissions.app_error`, and this
/// one is not, which is also why a category id naming no row answers 400 here: the gate fetches
/// the row and denies when the lookup fails. The whole request is refused, not the one entry.
///
/// # The loop runs over every category before any of them is written
///
/// Permissions for all, then validation for all, then one transactional store call. So a request
/// whose fourth entry is somebody else's category changes nothing at all.
///
/// # A body of `null` is accepted and answers `[]`
///
/// `json.Decode` into a `[]*T` leaves the slice nil without erroring, the two loops then run zero
/// times, and the store returns its `[]*model.SidebarCategoryWithChannels{}` literal — so the
/// answer is `[]`, not `null` and not a 400.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn update_categories_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = match user_and_team_write_gate(&state, &session, user_id, &team_id).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    // `Option`, so a JSON `null` is Go's nil slice rather than a decode error.
    let mut categories: Vec<SidebarCategoryWithChannels> =
        match serde_json::from_slice::<Option<Vec<SidebarCategoryWithChannels>>>(&bytes) {
            Ok(categories) => categories.unwrap_or_default(),
            Err(err) => {
                tracing::debug!(error = %err, "categories body did not decode");
                return ApiError::invalid_param("category").into_response();
            }
        };
    tracing::Span::current().record("count", categories.len());

    for category in &categories {
        if !state
            .app
            .session_has_permission_to_category(
                &session.0,
                &user_id,
                &team_id,
                &category.category.id,
            )
            .await
        {
            return ApiError::invalid_param("category").into_response();
        }
    }

    if let Err(err) = validate_sidebar_categories(&state, &team_id, &user_id, &mut categories).await
    {
        return err.into_response();
    }

    match state
        .app
        .update_sidebar_categories(&user_id, &team_id, &categories)
        .await
    {
        Ok(updated) => marshal_or_500(&updated, "updateCategoriesForTeamForUser"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `updateCategoryOrderForTeamForUser` (api4/channel_category.go:125) —
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/channels/categories/order`.
///
/// # The only route in the family whose decode failure is not `invalid_body_param`
///
/// It goes through `model.NonSortedArrayFromJSON`, and the handler wraps the failure itself:
/// `model.NewAppError("updateCategoryOrderForTeamForUser", model.PayloadParseError, nil, "", 400)`
/// — id `api.payload.parse.error`, and **no `Name` in `params`**. Its siblings answer
/// `api.context.invalid_body_param.app_error` with `{"Name":"category"}`.
///
/// # A body of `null` decodes to nil *without an error*
///
/// `NonSortedArrayFromJSON` returns `(nil, nil)` when the decoded slice is nil, so `null` is not a
/// 400. The nil then flows to the store, whose length check fails against any non-empty existing
/// order and answers **500** — and `ArrayToJSON(nil)` would answer the four bytes `null` on the
/// success path, which is reachable only for a user with no categories at all.
///
/// # The response is the *de-duplicated* list, not the request
///
/// `RemoveDuplicateStringsNonSort` runs before the permission loop, and the same slice is both
/// stored and echoed. So sending an id twice answers with it once — and, because the store's
/// length check then sees one fewer id than the user has categories, answers 500 while doing so.
///
/// # `w.Write(ArrayToJSON(...))`: no trailing newline
///
/// The *read* on this same path uses `json.NewEncoder` and therefore does have one ([D-086]). Two
/// methods, one path, two framings.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn update_category_order_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let user_id = match user_and_team_write_gate(&state, &session, user_id, &team_id).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let category_order: Option<Vec<String>> =
        match serde_json::from_slice::<Option<Vec<String>>>(&bytes) {
            // `RemoveDuplicateStringsNonSort`, which also turns a nil into `[]` — but Go applies
            // it only on the non-nil path, so the `None` here stays `None`.
            Ok(Some(order)) => Some(mm_model::utils::remove_duplicate_strings_non_sort(&order)),
            Ok(None) => None,
            Err(err) => {
                tracing::debug!(error = %err, "category order body did not decode");
                return ApiError::from(mm_model::utils::AppError::new(
                    "updateCategoryOrderForTeamForUser",
                    PAYLOAD_PARSE_ERROR,
                    None,
                    String::new(),
                    400,
                ))
                .into_response();
            }
        };
    let order = category_order.as_deref().unwrap_or_default();
    tracing::Span::current().record("count", order.len());

    for category_id in order {
        if !state
            .app
            .session_has_permission_to_category(&session.0, &user_id, &team_id, category_id)
            .await
        {
            return ApiError::invalid_param("category").into_response();
        }
    }

    if let Err(err) = state
        .app
        .update_sidebar_category_order(&user_id, &team_id, order)
        .await
    {
        return ApiError::from(err).into_response();
    }

    // `model.ArrayToJSON` discards its error and renders a nil slice as `null`.
    match &category_order {
        Some(order) => marshal_or_500(order, "updateCategoryOrderForTeamForUser"),
        None => json_body(b"null".to_vec()),
    }
}

/// The preamble the two `{category_id}` writes share, mirroring
/// [`get_category_for_team_for_user`]'s: the mux charset, `me`, the three `Require*` calls in
/// Go's order, then `SessionHasPermissionToCategory` **before** the team gate.
///
/// `Ok(Some(user_id))` means proceed; `Ok(None)` means the request was forwarded and the caller
/// must return `forwarded`.
// The error is a whole `Response`; see `channels::require_id` for why the lint is allowed
// across this crate.
#[allow(clippy::result_large_err)]
async fn category_write_gate(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: String,
    team_id: &str,
    category: &str,
) -> Result<String, Response> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    if let Err(err) = validate_user_and_team_ids(&user_id, team_id) {
        return Err(err.into_response());
    }
    if !is_valid_category_id(category) {
        return Err(ApiError::invalid_url_param("category_id").into_response());
    }

    let denial = sidebar_denied(
        || async {
            state
                .app
                .session_has_permission_to_category(&session.0, &user_id, team_id, category)
                .await
        },
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
    )
    .await;
    if let Some(permission) = denial {
        return Err(
            ApiError::from(make_permission_error(&session.0, &[permission])).into_response(),
        );
    }

    Ok(user_id)
}

/// Port of `updateCategoryForTeamForUser` (api4/channel_category.go:317) —
/// `PUT /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}`.
///
/// # It is the collection route with a list of one, and the id comes from the **path**
///
/// `categoryUpdateRequest.Id = c.Params.CategoryId` is assigned *after* validation and before the
/// store call, so `id` in the body is ignored entirely. The store call is the same
/// `UpdateSidebarCategories`, which means this route shares the collection route's read-only-field
/// rules and its Favorites/`Preferences` mirroring — and its **500** for a store failure, where
/// its own permission gate has already turned a missing category into a 403.
///
/// # The gate is `SessionHasPermissionToCategory`, so a missing category is 403
///
/// Unlike the collection route, whose per-category refusal is a 400. Same underlying function,
/// two different wrappers, two statuses — see [`get_category_for_team_for_user`], which documents
/// the gate itself.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, category_id = %category, forwarded))]
pub async fn update_category_for_team_for_user(
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

    let user_id = match category_write_gate(&state, &session, user_id, &team_id, &category).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    let bytes = match read_body(request.into_body()).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let mut update: SidebarCategoryWithChannels = match serde_json::from_slice(&bytes) {
        Ok(update) => update,
        Err(err) => {
            tracing::debug!(error = %err, "category body did not decode");
            return ApiError::invalid_param("category").into_response();
        }
    };
    // Note the order: Go tests `TeamId` first here and `UserId` first in the create handler. Same
    // error either way, so nothing on the wire moves.
    if update.category.team_id != team_id || update.category.user_id != user_id {
        return ApiError::invalid_param("category").into_response();
    }

    if let Err(err) = validate_sidebar_categories(
        &state,
        &team_id,
        &user_id,
        std::slice::from_mut(&mut update),
    )
    .await
    {
        return err.into_response();
    }

    // After validation, before the store: the path wins over the body.
    update.category.id = category;

    match state
        .app
        .update_sidebar_categories(&user_id, &team_id, std::slice::from_ref(&update))
        .await
    {
        Ok(updated) => match updated.first() {
            Some(category) => marshal_or_500(category, "updateCategoryForTeamForUser"),
            // Go indexes `categories[0]` unguarded; one category in means one out, so this is
            // unreachable rather than a divergence — but it must not be a panic.
            None => {
                tracing::error!("UpdateSidebarCategories answered with no categories");
                marshal_error("updateCategoryForTeamForUser")
            }
        },
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `deleteCategoryForTeamForUser` (api4/channel_category.go:368) —
/// `DELETE /api/v4/users/{user_id}/teams/{team_id}/channels/categories/{category_id}`.
///
/// # Deleting a default category is a 400, and the id is not a delete-specific one
///
/// The refusal comes from the store (`ErrInvalidInput` for a type that is not `custom`) and
/// carries `app.channel.sidebar_categories.app_error` — the same id every other failure in this
/// family carries. So a client cannot distinguish "you may not delete Favorites" from "the query
/// broke" by the `id`; only the status separates them, 400 from 500.
///
/// # The channels are not lost and not moved by this request
///
/// Deleting the category deletes its `SidebarChannels` rows, which makes every channel in it an
/// orphan; the *next read* files orphans under Channels or Direct Messages. So the channels
/// reappear elsewhere without this handler doing anything, and a client that refetches sees them.
///
/// # `ReturnStatusOK`
///
/// `{"status":"OK"}` with no trailing newline — Go's `ReturnStatusOK` writes a marshalled
/// `map[string]string` through `w.Write`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, category_id = %category, forwarded))]
pub async fn delete_category_for_team_for_user(
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

    let user_id = match category_write_gate(&state, &session, user_id, &team_id, &category).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };

    match state
        .app
        .delete_sidebar_category(&user_id, &team_id, &category)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// `ReturnStatusOK` (web/handlers.go) — `{"status":"OK"}`, no trailing newline.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
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
