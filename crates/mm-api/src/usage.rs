//! Port of `api4/usage.go` — `GET /api/v4/usage/{posts,storage,teams}`.
//!
//! Three counters the system console renders and the cloud billing job reads. All three are
//! **`APISessionRequired` with no permission check**: any authenticated session may ask how many
//! posts, bytes and teams this installation holds. That is Go's choice on all three, stated by
//! the absence of a `SessionHasPermissionTo` call rather than by a comment, and it is reproduced.
//!
//! # Two of the three round, and they round in different layers
//!
//! `posts` rounds in the *app* layer at resolution 3; `storage` rounds in the *handler* at
//! resolution 8; `teams` does not round at all. See [`mm_app::utils::round_off_to_zeroes_resolution`].
//!
//! # Wire format
//!
//! All three are `json.Marshal` plus a bare `w.Write`: **no trailing newline**, unlike most of
//! this API.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::utils::round_off_to_zeroes_resolution;
use mm_model::usage::{PostsUsage, StorageUsage};
use mm_model::utils::AppError;
use serde::Serialize;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// The storage resolution, applied by the **handler** (api4/usage.go:47).
///
/// Eight, against the app layer's three for posts — and it is an **upper bound**, not a fixed
/// width. `min(zeroes, resolution)` clamps it to the magnitude of the number, so a 5,200-byte
/// installation reports `5000` (three zeroes) rather than `0`, and only a total of nine digits or
/// more is rounded to the full eight. Getting this backwards is easy and this constant is where
/// it would happen; see [`tests::the_resolution_is_an_upper_bound_not_a_width`].
const STORAGE_RESOLUTION: i32 = 8;

/// Port of `getPostsUsage` (api4/usage.go:24).
///
/// The rounding happened in the app layer; this handler only encodes.
///
/// # This is the sharpest instance of [D-087] found so far, and we are the correct one
///
/// Go passes `AllowFromCache: true`, and the cache behind it is **size 1 with a thirty-minute
/// expiry** (`localcachelayer/layer.go:342`) that **nothing invalidates when a post is written**.
/// So Go's answer here is not "the count a moment ago" — it is the count at some point in the
/// last half hour, and a busy server's number can be wrong by any amount. Measured on the
/// development stack: Go answered `400` against a table holding `18`, and answered `10` — our
/// number, to the byte — the instant its caches were cleared.
///
/// [D-087]'s decision applies unchanged: we read through and are never staler than Go. It is
/// worth stating loudly here because the divergence is large, silent, and would otherwise look
/// like a bug in the port. The parity test clears Go's cache before comparing, for that reason;
/// see `common::invalidate_go_caches`.
#[tracing::instrument(skip_all, fields(count))]
pub async fn get_posts_usage(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let count = state.app.get_posts_usage().await?;
    tracing::Span::current().record("count", count);
    encode("Api4.getPostsUsage", &PostsUsage { count })
}

/// Port of `getStorageUsage` (api4/usage.go:41).
///
/// # The rounding is here, not in the app layer
///
/// `usage = utils.RoundOffToZeroesResolution(float64(usage), 8)` sits between the app call and
/// the marshal. A port that moved it into `App::get_storage_usage` would give the same answer to
/// this route and a different one to the next caller — and Go has one: the cloud usage job reads
/// the app function directly and wants the unrounded bytes.
#[tracing::instrument(skip_all, fields(raw, bytes))]
pub async fn get_storage_usage(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let raw = state.app.get_storage_usage().await?;
    let bytes = round_off_to_zeroes_resolution(raw as f64, STORAGE_RESOLUTION);
    tracing::Span::current().record("raw", raw);
    tracing::Span::current().record("bytes", bytes);
    encode("Api4.getStorageUsage", &StorageUsage { bytes })
}

/// Port of `getTeamsUsage` (api4/usage.go:61).
///
/// # The nil check below the error check is dead, and reproducing it would be wrong
///
/// Go writes `if teamsUsage == nil { c.Err = … }` **without returning** (usage.go:68-70), so on
/// that branch it sets an error and then marshals the nil anyway. `GetTeamsUsage` never returns
/// `(nil, nil)`, so the branch is unreachable — and a port that turned it into an early return
/// would be changing behaviour on a path that does not exist. `AppResult` makes it
/// unrepresentable instead.
#[tracing::instrument(skip_all, fields(active, cloud_archived))]
pub async fn get_teams_usage(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let usage = state.app.get_teams_usage().await?;
    tracing::Span::current().record("active", usage.active);
    tracing::Span::current().record("cloud_archived", usage.cloud_archived);
    encode("Api4.getTeamsUsage", &usage)
}

/// `json.Marshal` plus `w.Write` — no trailing newline — with Go's own `api.marshal_error` on the
/// failure the three handlers each spell out separately.
fn encode<T: Serialize>(where_: &'static str, value: &T) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the usage counter");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

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
    use mm_model::usage::TeamsUsage;

    /// The three bodies, on the bytes. Every key is present — none of the three types carries
    /// `omitempty` — and none of the bodies ends in a newline.
    #[test]
    fn the_three_bodies_are_gos_bytes() {
        let posts = serde_json::to_string(&PostsUsage { count: 12000 }).expect("encodes");
        assert_eq!(posts, r#"{"count":12000}"#);

        let storage = serde_json::to_string(&StorageUsage { bytes: 0 }).expect("encodes");
        assert_eq!(
            storage, r#"{"bytes":0}"#,
            "a zeroed total still writes the key"
        );

        let teams = serde_json::to_string(&TeamsUsage {
            active: 3,
            cloud_archived: 0,
        })
        .expect("encodes");
        assert_eq!(teams, r#"{"active":3,"cloud_archived":0}"#);
    }

    /// The handler-side rounding, at the resolution this route passes.
    ///
    /// **Written first as "8 zeroes the answer of a small server" and that was wrong** — the
    /// `min(zeroes, resolution)` clamp means the requested resolution never exceeds the
    /// magnitude, so a few kilobytes rounds to three zeroes and not to nothing. The only way to
    /// get a zero out of this function is the `-9..=9` window. Pinned here because the mistake
    /// survives every parity test: on the development stack both servers agree on whatever this
    /// returns, right or wrong.
    #[test]
    fn the_resolution_is_an_upper_bound_not_a_width() {
        assert_eq!(
            round_off_to_zeroes_resolution(5200.0, STORAGE_RESOLUTION),
            5000,
            "three zeroes, because the magnitude clamps the requested eight"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(999_999_999.0, STORAGE_RESOLUTION),
            900_000_000,
            "and only at nine digits is the full resolution reached"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(7.0, STORAGE_RESOLUTION),
            0,
            "the small-number window is the only path to a zero"
        );
        assert_eq!(
            round_off_to_zeroes_resolution(0.0, STORAGE_RESOLUTION),
            0,
            "an installation with no files reports no bytes either way"
        );
    }
}
