//! Port of the channel **administration** handlers of `channels/api4/channel.go`:
//! `updateChannelScheme` (:2817), `patchChannelModerations` (:3011), `moveChannel` (:3063),
//! `convertGroupMessageToChannel` (:3186), `channelMembersMinusGroupMembers` (:2881) and
//! `updateChannelMemberAutotranslation` (:2269).
//!
//! In its own module rather than in `channels.rs`, which is 4,700 lines of read handlers already.
//!
//! # Three of these six are a gate and nothing else, and the gate is *where* it is
//!
//! `patchChannelModerations` opens with the licence check — **before** `RequireChannelId` — so on
//! an unlicensed server a request naming a channel that does not exist still gets the licence
//! 403, never a 404. `updateChannelMemberAutotranslation` opens with
//! `AutoTranslation() == nil || !IsFeatureAvailable()`, an enterprise interface that is nil in the
//! build this project compares against.
//!
//! `updateChannelScheme` is the one whose order carries information: `RequireChannelId`, then the
//! **body validation**, and only then the licence check. Measured against the pinned Go server on
//! an unlicensed stack: `{"scheme_id":"nope"}` is a 400 and `{"scheme_id":"<26 chars>"}` is a 403.
//! A port that gated first would answer 403 to both and lose the 400.
//!
//! # Why the licensed branch forwards
//!
//! Everything past each gate — scheme assignment, the moderation patch, the autotranslation
//! member flag — reads enterprise tables this store does not port. A licensed installation is
//! forwarded whole, exactly as `channels::get_channel_moderations` and its two siblings do.

use axum::extract::{Path, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::scheme::SchemeIDPatch;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate};
use crate::error::ApiError;

/// Port of `patchChannelModerations` (api4/channel.go:3011) —
/// `PUT /api/v4/channels/{channel_id}/moderations/patch`.
///
/// The licence check is the handler's **first statement**, ahead of `RequireChannelId`, so an
/// unlicensed server answers 403 for every channel id including one that does not exist. The
/// error id is `Api4.patchChannelModerations` and the status a 403 — the same status as
/// `getChannelModerations` on the sibling path and a *different* id
/// (`patch_channel_moderations.license.error` against `get_channel_moderations.license.error`).
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, licensed))]
pub async fn patch_channel_moderations(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => {
            let _ = &channel_id;
            ApiError::from(AppError::new(
                "Api4.patchChannelModerations",
                "api.channel.patch_channel_moderations.license.error",
                None,
                String::new(),
                403,
            ))
            .into_response()
        }
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// Port of `updateChannelMemberAutotranslation` (api4/channel.go:2269) —
/// `PUT /api/v4/channels/{channel_id}/members/{user_id}/autotranslation`.
///
/// # The gate is an enterprise interface, not a licence field
///
/// Go tests `c.App.AutoTranslation() == nil || !c.App.AutoTranslation().IsFeatureAvailable()`.
/// `AutoTranslation()` returns `Srv().AutoTranslation`, populated only by
/// `RegisterAutoTranslationInterface` from the enterprise imports (app/enterprise.go:127), so the
/// build this project compares against leaves it nil and the route is a 403 with
/// `api.channel.update_channel_member_autotranslation.feature_not_available.app_error` — measured,
/// not inferred.
///
/// **`IsFeatureAvailable` is not visible from this tree** (it lives in the closed enterprise
/// repository), so what a licensed server answers here cannot be established from the source. That
/// is why the licensed branch forwards rather than reproducing the 403: the one case we can verify
/// is the unlicensed one, and guessing the other would be a confident wrong answer.
///
/// Note also the parameter order — Go is `RequireUserId().RequireChannelId()` here, the reverse of
/// every other handler on this path — which is unobservable while the gate precedes it.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id, licensed))]
pub async fn update_channel_member_autotranslation(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => {
            let _ = (&channel_id, &user_id);
            ApiError::from(AppError::new(
                "updateChannelMemberAutotranslation",
                "api.channel.update_channel_member_autotranslation.feature_not_available.app_error",
                None,
                String::new(),
                403,
            ))
            .into_response()
        }
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// Port of `updateChannelScheme` (api4/channel.go:2817) —
/// `PUT /api/v4/channels/{channel_id}/scheme`.
///
/// # Three gates, in this order, and the order is on the wire
///
/// 1. `RequireChannelId` — an id outside `[A-Za-z0-9]+` never reaches the handler at all
///    (gorilla 404s it, and `partially_migrated_with_ids` reproduces that); a 26-character-shaped
///    check failure is `invalid_url_param`.
/// 2. The body: `jsonErr != nil || p.SchemeID == nil || !model.IsValidId(*p.SchemeID)` collapses
///    to **one** 400, `invalid_body_param` naming `scheme_id`. So `{}`, `{"scheme_id":null}`,
///    `{"scheme_id":"nope"}` and a body that is not JSON at all are indistinguishable on the wire.
/// 3. The licence — 403 `api.channel.update_channel_scheme.license.error`.
///
/// [`is_valid_id`] is the whole of step 2's third clause: Go's `IsValidId` (utils.go:802) is 26
/// **bytes**, every rune of which is a Unicode letter or number. That is *looser* than the 26
/// lower-case base32 characters an id generator produces, so `ABCDEFGHIJKLMNOPQRSTUVWXYZ` and
/// `12345678901234567890123456` both pass it and reach the 403 — measured against the pinned Go
/// server, after this port initially assumed base32 and asserted the 400 that is not there.
///
/// # `Decode` stops at the first JSON value
///
/// Go uses `json.NewDecoder(r.Body).Decode(&p)`, which consumes one value and ignores whatever
/// follows it, so `{"scheme_id":"<valid>"} trailing garbage` passes where `json.Unmarshal` would
/// fail. Reproduced with [`serde_json::Deserializer::into_iter`] taking the first item.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, licensed))]
pub async fn update_channel_scheme(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&channel_id) {
        return ApiError::invalid_url_param("channel_id").into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("scheme_id").into_response();
        }
    };

    if !body_names_a_valid_scheme_id(&bytes) {
        return ApiError::invalid_param("scheme_id").into_response();
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            "Api4.UpdateChannelScheme",
            "api.channel.update_channel_scheme.license.error",
            None,
            String::new(),
            403,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// The whole of `updateChannelScheme`'s step 2, as one predicate because Go answers it as one.
///
/// Separate from the handler so the four ways to fail it can be asserted without a server.
fn body_names_a_valid_scheme_id(bytes: &[u8]) -> bool {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<SchemeIDPatch>();
    match values.next() {
        Some(Ok(patch)) => patch.scheme_id.as_deref().is_some_and(is_valid_id),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "abcdefghijklmnopqrstuvwxyz";

    #[test]
    fn a_valid_scheme_id_passes_the_body_gate() {
        assert!(body_names_a_valid_scheme_id(
            format!(r#"{{"scheme_id":"{VALID}"}}"#).as_bytes()
        ));
    }

    /// The four failures Go collapses into one 400. Each is a separate branch of the `||` chain:
    /// a decode error, a missing key, an explicit null, and an id that is not 26 base32 characters.
    #[test]
    fn every_other_shape_fails_the_body_gate() {
        for body in [
            "",
            "garbage",
            "[]",
            "{}",
            r#"{"scheme_id":null}"#,
            r#"{"scheme_id":"nope"}"#,
            r#"{"scheme_id":""}"#,
            // 25 characters, and 27.
            r#"{"scheme_id":"abcdefghijklmnopqrstuvwxy"}"#,
            r#"{"scheme_id":"abcdefghijklmnopqrstuvwxyza"}"#,
            // 26 characters, one of which is neither a letter nor a number.
            r#"{"scheme_id":"ab-defghijklmnopqrstuvwxyz"}"#,
        ] {
            assert!(
                !body_names_a_valid_scheme_id(body.as_bytes()),
                "{body} should not pass"
            );
        }
    }

    /// `Decode` takes one value and stops; `Unmarshal` would reject the trailer. Go uses the
    /// former here, so this is a 403 on an unlicensed server and not a 400.
    #[test]
    fn a_trailing_value_after_the_object_is_ignored_as_go_ignores_it() {
        assert!(body_names_a_valid_scheme_id(
            format!(r#"{{"scheme_id":"{VALID}"}} and then some"#).as_bytes()
        ));
    }

    /// `IsValidId` is letters-or-numbers, not base32: an all-upper-case and an all-digit id both
    /// pass it, which is why an unlicensed server answers **403** and not 400 for either. Both
    /// measured against the pinned Go server.
    #[test]
    fn upper_case_and_all_digit_ids_pass_because_go_tests_letters_or_numbers() {
        for id in ["ABCDEFGHIJKLMNOPQRSTUVWXYZ", "12345678901234567890123456"] {
            assert!(
                body_names_a_valid_scheme_id(format!(r#"{{"scheme_id":"{id}"}}"#).as_bytes()),
                "{id} should pass"
            );
        }
    }

    /// The reverse: the trailing garbage cannot rescue a body whose *first* value is bad.
    #[test]
    fn a_trailing_value_does_not_rescue_a_bad_first_value() {
        assert!(!body_names_a_valid_scheme_id(
            format!(r#"{{}} {{"scheme_id":"{VALID}"}}"#).as_bytes()
        ));
    }
}
