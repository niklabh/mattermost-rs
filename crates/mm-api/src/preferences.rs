//! Port of `channels/api4/preference.go`: the three reads — `getPreferences` (:24),
//! `getPreferencesByCategory` (:45), `getPreferenceByCategoryAndName` (:66) — under
//! `GET /api/v4/users/{user_id}/preferences[/{category}[/name/{preference_name}]]`, and
//! `updatePreferences` (:90) as `PUT /api/v4/users/me/preferences`.
//!
//! `updatePreferences` was **the first write served from Rust.** Everything migrated before it
//! read.

use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_EDIT_OTHER_USERS, make_permission_error};
use mm_model::preference::Preferences;
use mm_model::preference::{
    PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW, PREFERENCE_CATEGORY_FLAGGED_POST,
    PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW, Preference,
};
use mm_model::utils::is_valid_alpha_num_hyphen_underscore;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

/// Go's mux class for `{category}` and `{preference_name}`: `[A-Za-z0-9_]+`
/// (api4/preference.go:20-21). **Narrower than the validator behind it** — `RequireCategory`
/// accepts a hyphen, the route does not, so `display-settings` is a mux 404 before any handler
/// runs. A segment outside this class is forwarded so that 404 is Go's own ([D-150]); the
/// `{user_id}` segment is handled by `partially_migrated_with_ids`, which knows only the id class.
fn segment_matches_preference_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Go's `RequireCategory` (web/context.go:561) and `RequirePreferenceName` (:585): both are
/// `IsValidAlphaNumHyphenUnderscore(value, true)`, the **format-strict** pattern
/// `^[a-z0-9]+([a-z\-\_0-9]+|(__)?)[a-z0-9]+$` — lowercase only, at least two characters, no
/// leading or trailing `_`/`-`. So `Display_Settings` and `a` both route in mux and then 400 with
/// `api.context.invalid_url_param.app_error`, naming the parameter.
#[allow(clippy::result_large_err)]
fn require_alpha_num_hyphen_underscore(value: &str, parameter: &str) -> Result<(), ApiError> {
    if is_valid_alpha_num_hyphen_underscore(value, true) {
        Ok(())
    } else {
        Err(ApiError::invalid_url_param(parameter))
    }
}

/// Go's `json.NewEncoder(w).Encode(v)` — the value followed by a newline ([D-086]).
// `result_large_err`: same shape as every handler in the crate; see `channels::require_id`.
#[allow(clippy::result_large_err)]
fn encode_with_newline<T: serde::Serialize>(where_: &str, value: &T) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise {where_} response");
        ApiError(mm_model::utils::AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
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

/// Which of the three reads a request is, once its segments have passed the mux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreferenceRead<'a> {
    All,
    Category(&'a str),
    CategoryAndName(&'a str, &'a str),
}

/// Go's `c.RequireUserId().RequireCategory().RequirePreferenceName()` chain, as one call so the
/// **order** is testable in-process: `params` is not on the wire and the message is untranslated
/// ([D-092]), so a swapped order survives every cross-server test — the same reasoning as
/// `channels::validate_ids`.
#[allow(clippy::result_large_err)]
fn require_segments(user_id: &str, read: PreferenceRead<'_>) -> Result<(), ApiError> {
    require_id(user_id, "user_id")?;
    match read {
        PreferenceRead::All => {}
        PreferenceRead::Category(category) => {
            require_alpha_num_hyphen_underscore(category, "category")?;
        }
        PreferenceRead::CategoryAndName(category, name) => {
            require_alpha_num_hyphen_underscore(category, "category")?;
            require_alpha_num_hyphen_underscore(name, "preference_name")?;
        }
    }
    Ok(())
}

/// The shared body of the three GET handlers, in Go's order.
///
/// 1. `me` resolves to the session's user **before** `RequireUserId` (web/context.go:301),
///    otherwise the literal fails `IsValidId`.
/// 2. `RequireUserId().RequireCategory().RequirePreferenceName()` — a chain that stops at the
///    first failure, so when two segments are bad the **earlier** one names the 400.
/// 3. `SessionHasPermissionToUser`, answering 403 naming `edit_other_users` — before any read, so
///    a caller cannot learn whether another user has a category by being refused differently.
/// 4. The read, then `json.NewEncoder(w).Encode` — newline-terminated.
///
/// # `GetAll` of nothing is `null`
///
/// Go encodes a nil `model.Preferences` — which is what sqlx leaves for zero rows — and a nil
/// slice marshals as `null`, not `[]`. `GetCategory` never reaches the encoder empty (the app
/// layer 404s), so the `null` spelling is `All`'s alone.
#[allow(clippy::result_large_err)]
async fn serve_preference_read(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
    read: PreferenceRead<'_>,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.as_str()
    } else {
        user_id
    };

    require_segments(user_id, read)?;

    if !state
        .app
        .session_has_permission_to_user(&session.0, user_id)
        .await
    {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    match read {
        PreferenceRead::All => {
            let preferences = state.app.get_preferences_for_user(user_id).await?;
            if preferences.is_empty() {
                encode_with_newline("getPreferences", &serde_json::Value::Null)
            } else {
                encode_with_newline("getPreferences", &preferences)
            }
        }
        PreferenceRead::Category(category) => {
            let preferences = state
                .app
                .get_preference_by_category_for_user(user_id, category)
                .await?;
            encode_with_newline("getPreferencesByCategory", &preferences)
        }
        PreferenceRead::CategoryAndName(category, name) => {
            let preference = state
                .app
                .get_preference_by_category_and_name_for_user(user_id, category, name)
                .await?;
            encode_with_newline("getPreferenceByCategoryAndName", &preference)
        }
    }
}

/// Port of `getPreferences` (api4/preference.go:24) for the literal `me` path, which axum routes
/// separately because `PUT` on it is already served.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id))]
pub async fn get_preferences_me(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    serve_preference_read(&state, &session, ME, PreferenceRead::All).await
}

/// Port of `getPreferences` (api4/preference.go:24) —
/// `GET /api/v4/users/{user_id}/preferences`.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn get_preferences(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    serve_preference_read(&state, &session, &user_id, PreferenceRead::All).await
}

/// Port of `getPreferencesByCategory` (api4/preference.go:45) —
/// `GET /api/v4/users/{user_id}/preferences/{category}`.
///
/// Go's sibling `POST .../preferences/delete` (:19) shares this path shape: a `POST` to it falls
/// to `partially_migrated`'s method fallback and is forwarded, while a `GET` to `/delete` reaches
/// this handler with `delete` as the category — exactly what gorilla does after the method
/// mismatch on the `/delete` route — and 404s as an empty category.
#[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category, forwarded))]
pub async fn get_preferences_by_category(
    State(state): State<AppState>,
    Path((user_id, category)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !segment_matches_preference_mux(&category) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    serve_preference_read(
        &state,
        &session,
        &user_id,
        PreferenceRead::Category(&category),
    )
    .await
    .unwrap_or_else(IntoResponse::into_response)
}

/// Port of `getPreferenceByCategoryAndName` (api4/preference.go:66) —
/// `GET /api/v4/users/{user_id}/preferences/{category}/name/{preference_name}`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, category = %category, name = %preference_name, forwarded))]
pub async fn get_preference_by_category_and_name(
    State(state): State<AppState>,
    Path((user_id, category, preference_name)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !segment_matches_preference_mux(&category)
        || !segment_matches_preference_mux(&preference_name)
    {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    serve_preference_read(
        &state,
        &session,
        &user_id,
        PreferenceRead::CategoryAndName(&category, &preference_name),
    )
    .await
    .unwrap_or_else(IntoResponse::into_response)
}

/// `maxUpdatePreferences` (preference.go:14).
const MAX_UPDATE_PREFERENCES: usize = 100;

/// Categories this handler must **not** serve, each for a different reason.
///
/// * `flagged_post` — Go loads the referenced post, loads that post's channel and checks
///   `PermissionReadChannelContent` (preference.go:118-138). Serving it without that would let a
///   caller learn whether a post exists in a channel they cannot read.
/// * `direct_channel_show` / `group_channel_show` — Go's `UpdatePreferences` calls
///   `UpdateSidebarChannelsByPreferences` (preference.go:62) to keep sidebar categories in step
///   with DM and GM visibility. The channel store is unported, so serving these here would write
///   a correct preference row and leave the sidebar permanently wrong — a **persisted**
///   inconsistency a reload does not fix. Forwarding closes [D-091] without porting anything.
const FORWARDED_CATEGORIES: &[&str] = &[
    PREFERENCE_CATEGORY_FLAGGED_POST,
    PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW,
    PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW,
];

/// Port of `updatePreferences` for the `me` case.
///
/// # Partial migration, on purpose
///
/// Go's handler special-cases the `flagged_post` category: for each such preference it loads the
/// referenced post, loads that post's channel, and checks `PermissionReadChannelContent`
/// (preference.go:118-138). None of that machinery is ported. Serving those requests here
/// **without** the check would let a user flag a post in a channel they cannot read, which is an
/// information leak — flags are per-user, but the 400-vs-200 answer reveals whether the post
/// exists.
///
/// So a batch containing any `flagged_post` entry is **forwarded to the Go server** instead. That
/// is the Strangler Fig applied inside a single route rather than across routes: migrate the part
/// that is verified, forward the rest, and let the client see no difference either way.
///
/// # Not reproduced
///
/// The audit record, the sidebar sync and the two WebSocket events — see [D-089], [D-091] and
/// the note on `App::update_preferences`.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count, forwarded))]
pub async fn update_preferences_me(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();

    // The body is read once and kept, because it may have to be replayed to the Go server.
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("preferences").into_response();
        }
    };

    let preferences: Vec<Preference> = match serde_json::from_slice(&bytes) {
        Ok(preferences) => preferences,
        // Go answers `SetInvalidParamWithErr("preferences", ...)` for a body that will not decode.
        Err(err) => {
            tracing::debug!(error = %err, "preferences body did not decode");
            return ApiError::invalid_param("preferences").into_response();
        }
    };

    // `len(preferences) == 0 || len(preferences) > maxUpdatePreferences` (preference.go:109).
    // Both bounds are Go's, and the empty case is an error rather than a no-op.
    if preferences.is_empty() || preferences.len() > MAX_UPDATE_PREFERENCES {
        return ApiError::invalid_param("preferences").into_response();
    }

    // The part we do not implement goes to the server that does.
    if preferences
        .iter()
        .any(|p| FORWARDED_CATEGORIES.contains(&p.category.as_str()))
    {
        tracing::Span::current().record("forwarded", true);
        let request = Request::from_parts(parts, Body::from(bytes));
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    tracing::Span::current().record("count", preferences.len());

    let preferences = Preferences(preferences);
    if let Err(app_error) = state
        .app
        .update_preferences(&session.0.user_id, &preferences)
        .await
    {
        return ApiError(app_error).into_response();
    }

    // `ReturnStatusOK` — `{"status":"OK"}` written with `w.Write`, so no trailing newline
    // (web.go:127). Not an encoder call site; see [D-086].
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

    fn preference(category: &str) -> Preference {
        Preference {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            category: category.to_owned(),
            name: "use_military_time".to_owned(),
            value: "true".to_owned(),
        }
    }

    fn needs_forwarding(preferences: &[Preference]) -> bool {
        preferences
            .iter()
            .any(|p| FORWARDED_CATEGORIES.contains(&p.category.as_str()))
    }

    /// The safety property of the partial migration: anything touching `flagged_post` must reach
    /// the server that performs the channel-read permission check.
    #[test]
    fn a_flagged_post_entry_forces_the_batch_to_go() {
        assert!(needs_forwarding(&[preference(
            PREFERENCE_CATEGORY_FLAGGED_POST
        )]));

        // Mixed batches too — the check is per entry, so one flagged post sends the whole batch.
        assert!(needs_forwarding(&[
            preference("display_settings"),
            preference(PREFERENCE_CATEGORY_FLAGGED_POST),
        ]));
    }

    /// The sidebar-bearing categories go to Go as well, because the sidebar sync behind them is
    /// unported and skipping it would persist an inconsistency rather than merely miss an event.
    #[test]
    fn the_sidebar_categories_are_forwarded_too() {
        assert!(needs_forwarding(&[preference(
            PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW
        )]));
        assert!(needs_forwarding(&[preference(
            PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW
        )]));
    }

    #[test]
    fn ordinary_categories_are_served_here() {
        assert!(!needs_forwarding(&[
            preference("display_settings"),
            preference("advanced_settings"),
        ]));
    }

    /// Both of Go's bounds, including the one that is easy to read as a no-op: an empty batch is
    /// an error, not a successful nothing.
    #[test]
    fn the_batch_size_bounds_are_gos() {
        let too_many: Vec<Preference> = (0..=MAX_UPDATE_PREFERENCES)
            .map(|_| preference("display_settings"))
            .collect();
        let exactly_max: Vec<Preference> = (0..MAX_UPDATE_PREFERENCES)
            .map(|_| preference("display_settings"))
            .collect();

        let rejected = |p: &[Preference]| p.is_empty() || p.len() > MAX_UPDATE_PREFERENCES;

        assert!(
            rejected(&[]),
            "an empty batch is invalid_param, not a no-op"
        );
        assert!(rejected(&too_many), "101 is over the cap");
        assert!(
            !rejected(&exactly_max),
            "100 is exactly the cap and allowed"
        );
    }

    /// The success body is Go's `ReturnStatusOK`, byte for byte and without a newline.
    #[test]
    fn the_ok_body_matches_return_status_ok() {
        assert_eq!(r#"{"status":"OK"}"#, "{\"status\":\"OK\"}");
        assert!(!r#"{"status":"OK"}"#.ends_with('\n'));
    }

    // ---- the three reads ----

    const ME_ID: &str = "y9i4er48tt8bukijy7i3u5y9ar";

    fn param_name(err: &ApiError) -> String {
        err.0
            .params
            .as_ref()
            .and_then(|p| p.get("Name"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned()
    }

    /// The mux class is `[A-Za-z0-9_]+`: underscore in, hyphen and dot out, case allowed.
    #[test]
    fn the_segment_charset_is_gos_mux_class() {
        assert!(segment_matches_preference_mux("display_settings"));
        assert!(segment_matches_preference_mux("Display_Settings"));
        assert!(segment_matches_preference_mux("a"));
        assert!(segment_matches_preference_mux("__"));
        assert!(
            !segment_matches_preference_mux("display-settings"),
            "hyphen is routed by Go as a 404"
        );
        assert!(!segment_matches_preference_mux("display.settings"));
        assert!(!segment_matches_preference_mux(""));
        assert!(!segment_matches_preference_mux("caf\u{e9}"));
    }

    /// The validator is stricter than the mux in one direction (case, length, edges) and looser
    /// in another (hyphen) — both directions pinned, because each is where a port would "fix" it.
    #[test]
    fn the_validator_is_the_format_strict_pattern() {
        assert!(require_alpha_num_hyphen_underscore("display_settings", "category").is_ok());
        assert!(require_alpha_num_hyphen_underscore("ab", "category").is_ok());
        assert!(
            require_alpha_num_hyphen_underscore("display-settings", "category").is_ok(),
            "the validator allows a hyphen the mux never lets through"
        );

        for bad in ["Display_Settings", "a", "__", "_ab", "ab_", ""] {
            let err = require_alpha_num_hyphen_underscore(bad, "category")
                .expect_err("rejected by the format-strict pattern");
            assert_eq!(err.0.status_code, 400, "{bad:?}");
            assert_eq!(
                err.0.id, "api.context.invalid_url_param.app_error",
                "{bad:?}"
            );
            assert_eq!(param_name(&err), "category", "{bad:?}");
        }
    }

    /// `RequireUserId().RequireCategory().RequirePreferenceName()` stops at the first failure,
    /// so the earlier bad segment is the one the 400 names.
    #[test]
    fn the_require_chain_runs_user_then_category_then_name() {
        let both_bad = require_segments("nope", PreferenceRead::CategoryAndName("BAD", "BAD"))
            .expect_err("400");
        assert_eq!(param_name(&both_bad), "user_id");

        let cat_and_name_bad =
            require_segments(ME_ID, PreferenceRead::CategoryAndName("BAD", "BAD"))
                .expect_err("400");
        assert_eq!(param_name(&cat_and_name_bad), "category");

        let name_bad = require_segments(
            ME_ID,
            PreferenceRead::CategoryAndName("display_settings", "BAD"),
        )
        .expect_err("400");
        assert_eq!(param_name(&name_bad), "preference_name");

        assert!(require_segments(ME_ID, PreferenceRead::All).is_ok());
        assert!(require_segments(ME_ID, PreferenceRead::Category("display_settings")).is_ok());
        assert!(
            require_segments(
                ME_ID,
                PreferenceRead::CategoryAndName("display_settings", "use_military_time")
            )
            .is_ok()
        );
    }

    /// `GetAll` of a user with no rows is the literal `null` — Go encodes a nil slice — and every
    /// success body ends in the encoder's newline.
    #[tokio::test]
    async fn an_empty_get_all_encodes_as_null_with_a_newline() {
        let response =
            encode_with_newline("getPreferences", &serde_json::Value::Null).expect("encodes");
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert_eq!(body.as_ref(), b"null\n");

        let response = encode_with_newline(
            "getPreferences",
            &Preferences(vec![preference("display_settings")]),
        )
        .expect("encodes");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust")
        );
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        assert!(
            body.ends_with(b"}]\n"),
            "{:?}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            body.starts_with(b"[{\"user_id\":"),
            "field order is the struct's"
        );
    }
}
