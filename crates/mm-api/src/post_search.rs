//! Port of `searchPostsInTeam` (api4/post.go:952), `searchPostsInAllTeams` (:966) and the shared
//! `searchPosts` (:970) — `POST /api/v4/teams/{team_id}/posts/search` and
//! `POST /api/v4/posts/search`, the search box.
//!
//! # The body is decoded the way Go decodes it
//!
//! `json.NewDecoder(r.Body).Decode(&params)` reads **one** JSON value and stops, so trailing
//! bytes are not an error; a literal `null` is a zero `SearchParameter` (every field is a
//! pointer) and fails the `terms` check rather than the decode; an empty body is `EOF`, which is
//! the decode error. All three are reproduced through a `StreamDeserializer` over
//! `Option<SearchParameter>`, as the scheme routes do.
//!
//! # `per_page` is read, defaulted to 60, and then ignored
//!
//! Go threads it through to the store, whose database branch has no paging: page 0 is up to
//! 100 rows per params element and any later page is empty. It is parsed here only so that a
//! non-integer value is the 400 Go gives it.
//!
//! # The audit record and the metrics are not ported
//!
//! `allPostHaveMembership` and `isMemberForAllPreviews` are computed and dropped ([D-028]);
//! `IncrementPostsSearchCounter` has no counterpart.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::post::PrepareError;
use mm_app::post_search::PostSearchError;
use mm_model::permission::{PERMISSION_VIEW_TEAM, make_permission_error};
use mm_model::post::SearchParameter;
use mm_model::post_list::PostList;
use mm_model::post_search_results::PostSearchResults;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `w.Header().Set("Cache-Control", ...)` on the 200 (api4/post.go:1046).
const CACHE_CONTROL: &str = "no-cache, no-store, must-revalidate";

/// `perPage := 60` (api4/post.go:997).
const DEFAULT_PER_PAGE: i64 = 60;

/// What the handler decided to do, before any of it is written.
enum Outcome {
    Served(Response),
    Failed(ApiError),
    /// The Go server has to answer this one — a metadata shape [`mm_app::post`] does not
    /// reproduce, or an `in:@user` whose direct channel [`mm_app::App::get_or_create_direct_channel`]
    /// declines to open.
    Forward,
}

/// Port of `searchPostsInTeam` (api4/post.go:952) — `POST /api/v4/teams/{team_id}/posts/search`.
///
/// `RequireTeamId` and the `view_team` check come **before** the body is read, so a caller
/// outside the team gets the 403 for a body that would not decode.
#[tracing::instrument(skip_all, fields(team_id = %team_id))]
pub async fn search_posts_in_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `APISessionRequiredDisableWhenBusy`: the busy check precedes the handler.
    if let Err(err) = crate::system::refuse_when_busy() {
        return err.into_response();
    }

    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
        .await
    {
        return ApiError::from(make_permission_error(&session.0, &[&PERMISSION_VIEW_TEAM]))
            .into_response();
    }

    search_posts(state, &team_id, &session, request).await
}

/// Port of `searchPostsInAllTeams` (api4/post.go:966) — `POST /api/v4/posts/search`. No
/// permission check of its own: the channel-membership sub-query and
/// `FilterPostsByChannelPermissions` are the whole of the access control.
#[tracing::instrument(skip_all)]
pub async fn search_posts_in_all_teams(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    // `APISessionRequiredDisableWhenBusy`: the busy check precedes the handler.
    if let Err(err) = crate::system::refuse_when_busy() {
        return err.into_response();
    }
    search_posts(state, "", &session, request).await
}

/// Port of `searchPosts` (api4/post.go:970), the body shared by the two handlers.
async fn search_posts(
    state: AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return invalid_body().into_response();
        }
    };

    match serve_search(&state, team_id, session, &bytes).await {
        Outcome::Served(response) => response,
        Outcome::Failed(err) => err.into_response(),
        Outcome::Forward => {
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// `api.post.search_posts.invalid_body.app_error`, 400 — the decode failure (api4/post.go:973).
fn invalid_body() -> ApiError {
    ApiError::from(AppError::new(
        "searchPosts",
        "api.post.search_posts.invalid_body.app_error",
        None,
        String::new(),
        400,
    ))
}

/// The first JSON value in the body, Go-style: see the module docs.
pub(crate) fn decode_search_parameter(bytes: &[u8]) -> Result<SearchParameter, ApiError> {
    let mut values =
        serde_json::Deserializer::from_slice(bytes).into_iter::<Option<SearchParameter>>();
    match values.next() {
        Some(Ok(params)) => Ok(params.unwrap_or_default()),
        Some(Err(err)) => {
            tracing::debug!(error = %err, "search body did not decode");
            Err(invalid_body())
        }
        None => Err(invalid_body()),
    }
}

async fn serve_search(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    bytes: &[u8],
) -> Outcome {
    let params = match decode_search_parameter(bytes) {
        Ok(params) => params,
        Err(err) => return Outcome::Failed(err),
    };

    // `params.Terms == nil || *params.Terms == ""` — one 400 for both.
    let terms = match params.terms.as_deref() {
        Some(terms) if !terms.is_empty() => terms,
        _ => return Outcome::Failed(ApiError::invalid_param("terms")),
    };

    let time_zone_offset = params.time_zone_offset.unwrap_or(0);
    let is_or_search = params.is_or_search.unwrap_or(false);
    let page = params.page.unwrap_or(0);
    let _per_page = params.per_page.unwrap_or(DEFAULT_PER_PAGE);
    let include_deleted_channels = params.include_deleted_channels.unwrap_or(false);

    let user_id = session.0.user_id.as_str();
    let (results, _all_post_have_membership) = match state
        .app
        .search_posts_for_user(
            terms,
            user_id,
            team_id,
            is_or_search,
            include_deleted_channels,
            time_zone_offset,
            page,
        )
        .await
    {
        Ok(found) => found,
        Err(PostSearchError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding the search to Go");
            return Outcome::Forward;
        }
        Err(PostSearchError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // The embedded list is never nil out of `SearchPostsForUser` — both of its answers build
    // one — so this default is a type-level courtesy, not a branch.
    let PostSearchResults { post_list, matches } = results;
    let list = post_list.unwrap_or_else(PostList::new);

    let prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding the search to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let (sanitized, _is_member_for_all_previews) = match state
        .app
        .sanitize_post_list_metadata_for_user(prepared, user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding the search to Go");
            return Outcome::Forward;
        }
        Err(PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // `model.MakePostSearchResults(clientPostList, results.Matches)` then `EncodeJSON`, which
    // strips the action integrations on the way out.
    let mut results = PostSearchResults::new(Some(sanitized), matches);
    let mut body = Vec::new();
    if let Err(err) = results.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise PostSearchResults");
        return Outcome::Failed(ApiError::from(AppError::new(
            "searchPosts",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }

    Outcome::Served(
        (
            StatusCode::OK,
            [
                ("Cache-Control", CACHE_CONTROL),
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three decoder shapes the module docs claim: one value with trailing bytes, `null`,
    /// and an empty body.
    #[test]
    fn the_body_is_decoded_like_gos_decoder() {
        let decoded = decode_search_parameter(br#"{"terms":"x","page":2} trailing"#).unwrap();
        assert_eq!(decoded.terms.as_deref(), Some("x"));
        assert_eq!(decoded.page, Some(2));

        let null = decode_search_parameter(b"null").unwrap();
        assert_eq!(null, SearchParameter::default());

        assert!(decode_search_parameter(b"").is_err());
        assert!(decode_search_parameter(b"{").is_err());
        assert!(decode_search_parameter(br#"{"terms":5}"#).is_err());
        assert!(decode_search_parameter(br#"{"page":1.5}"#).is_err());
        assert!(decode_search_parameter(br#"{"page":"1"}"#).is_err());
        // Unknown keys are ignored, as `encoding/json` ignores them.
        assert!(decode_search_parameter(br#"{"terms":"x","unknown":true}"#).is_ok());
    }
}
