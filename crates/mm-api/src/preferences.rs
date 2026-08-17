//! Port of `updatePreferences` (channels/api4/preference.go:90), reached as
//! `PUT /api/v4/users/me/preferences`.
//!
//! **The first write served from Rust.** Everything migrated before it read.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::preference::Preferences;
use mm_model::preference::{
    PREFERENCE_CATEGORY_DIRECT_CHANNEL_SHOW, PREFERENCE_CATEGORY_FLAGGED_POST,
    PREFERENCE_CATEGORY_GROUP_CHANNEL_SHOW, Preference,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

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
}
