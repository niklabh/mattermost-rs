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

use axum::extract::{Path, RawQuery, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::channel::CHANNEL_TYPE_SPACE;
use mm_model::permission::{
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS, make_permission_error,
};
use mm_model::scheme::SchemeIDPatch;
use mm_model::user::UsersWithGroupsAndCount;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate, parse_page, parse_per_page, query_first};
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

/// Port of `channelMembersMinusGroupMembers` (api4/channel.go:2881) —
/// `GET /api/v4/channels/{channel_id}/members_minus_group_members`.
///
/// The System Console's "who would this group constraint remove?" preview: the channel's members
/// who are in **none** of the `group_ids`, one page at a time, plus the total.
///
/// # The one group route in this file with no licence gate
///
/// Every handler in `api4/group.go` opens with `requireLicense` and answers 501 unlicensed (see
/// [`crate::groups`]), and three handlers in `api4/channel.go` open with a licence check of their
/// own. This one has neither, so an unlicensed server reads the group tables and answers `200`.
/// Measured against the pinned Go server before it was ported, which is the only way that fact is
/// knowable — the surrounding code all says "enterprise".
///
/// # `group_ids` is validated twice, against two different strings
///
/// 1. `groupIDsQueryParamRegex.ReplaceAllString(param, "")` strips **everything outside
///    `[a-zA-Z0-9,]`** and the *stripped* string must be at least 26 characters. So
///    `group_ids=!!!!` is a 400 for length even though it has four characters, and a 30-character
///    run of letters passes the length test with no comma in it at all.
/// 2. The split is then over the **unstripped** parameter, and every element must satisfy
///    `IsValidId`. So the 30-character run fails here instead — same 400, a different clause.
///
/// Reproducing the two-string dance matters because collapsing it to one changes which inputs
/// pass: validating the stripped string would accept `a!b` wherever `ab` is valid.
///
/// # A space channel is rejected; a board channel is **not**
///
/// `rejectSpaceChannelByID` guards this route and `rejectBoardChannelByID` does not — unlike the
/// three `PUT …/members/{user_id}/…` handlers, which carry both. A board id therefore reaches the
/// store here and answers an empty page rather than the board guard's 400.
///
/// # The body carries no trailing newline
///
/// `json.Marshal` then `w.Write`, not `json.NewEncoder(w).Encode` — so unlike the channel lists
/// ([D-086]) there is no `\n`. The two conventions sit four hundred lines apart in the same file.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, groups, page, per_page))]
pub async fn channel_members_minus_group_members(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if !is_valid_id(&channel_id) {
        return ApiError::invalid_url_param("channel_id").into_response();
    }

    match state
        .app
        .get_channel_of_type(&channel_id, CHANNEL_TYPE_SPACE)
        .await
    {
        Ok(_) => {
            return ApiError::from(AppError::new(
                "",
                "api.channel.space_channel.app_error",
                None,
                "space channels cannot be accessed via /channels endpoints".to_owned(),
                400,
            ))
            .into_response();
        }
        Err(err) if err.status_code == 404 => {}
        Err(err) => return ApiError::from(err).into_response(),
    }

    let raw = query_first(query.as_deref(), "group_ids").unwrap_or_default();
    let Some(group_ids) = parse_group_ids(&raw) else {
        return ApiError::invalid_param("group_ids").into_response();
    };
    tracing::Span::current().record("groups", group_ids.len());

    if !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS,
        )
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_CHANNELS],
        ))
        .into_response();
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let (users, count) = match state
        .app
        .channel_members_minus_group_members(&channel_id, &group_ids, page, per_page)
        .await
    {
        Ok(answer) => answer,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let body = UsersWithGroupsAndCount {
        users: Some(users),
        count,
    };
    match serde_json::to_vec(&body) {
        // `json.Marshal` then `w.Write` — **no trailing newline**, unlike the channel lists.
        Ok(bytes) => (
            axum::http::StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            bytes,
        )
            .into_response(),
        Err(err) => ApiError::from(AppError::new(
            "Api4.channelMembersMinusGroupMembers",
            "api.marshal_error",
            None,
            err.to_string(),
            500,
        ))
        .into_response(),
    }
}

/// Both of `channelMembersMinusGroupMembers`'s `group_ids` gates, as one function returning the
/// ids Go would have passed to the store.
///
/// `None` is the single 400 the two gates share. Separate from the handler because the two
/// strings they run against — stripped for the length, raw for the split — are the whole subtlety.
fn parse_group_ids(raw: &str) -> Option<Vec<String>> {
    // `groupIDsParamPattern = "[^a-zA-Z0-9,]*"` (api4/team.go:24), replaced with "". A `*` regex
    // deletes every run of non-matching characters, so this is simply "keep the alphanumerics and
    // the commas".
    let stripped_len = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == ',')
        .count();
    if stripped_len < 26 {
        return None;
    }

    // ...and the split is over `c.Params.GroupIDs`, the **raw** parameter, not the stripped one.
    let mut ids = Vec::new();
    for id in raw.split(',') {
        if !is_valid_id(id) {
            return None;
        }
        ids.push(id.to_owned());
    }
    Some(ids)
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

    /// The two `group_ids` gates, one branch at a time. The pairs that look alike and answer
    /// differently are the point: `a!b…` and `ab…` differ only in characters the *length* gate
    /// deletes and the *split* gate keeps.
    #[test]
    fn parse_group_ids_runs_the_length_gate_on_the_stripped_string() {
        // 25 alphanumerics — one short, whatever else is in the string.
        assert_eq!(parse_group_ids("abcdefghijklmnopqrstuvwxy"), None);
        // Four characters, none of which survive the strip: the length gate, not the split.
        assert_eq!(parse_group_ids("!!!!"), None);
        // 40 characters, of which only 20 survive the strip.
        assert_eq!(parse_group_ids(&"a!".repeat(20)), None);
        // Nothing at all — the absent parameter arrives here as "".
        assert_eq!(parse_group_ids(""), None);
    }

    #[test]
    fn parse_group_ids_runs_the_split_gate_on_the_raw_string() {
        // 30 alphanumerics: past the length gate, and not a valid 26-character id.
        assert_eq!(parse_group_ids(&"a".repeat(30)), None);
        // The strip keeps commas, so this clears 26 — and then the first element is 13 long.
        let two_short = format!("{},{}", "a".repeat(13), "b".repeat(13));
        assert_eq!(parse_group_ids(&two_short), None);
        // A separator that is not a comma: the strip deletes it, the split does not see it, and
        // the single 53-character element fails `IsValidId`.
        assert_eq!(parse_group_ids(&format!("{VALID};{VALID}")), None);
    }

    #[test]
    fn parse_group_ids_accepts_what_go_accepts() {
        assert_eq!(
            parse_group_ids(VALID),
            Some(vec![VALID.to_owned()]),
            "one id"
        );
        assert_eq!(
            parse_group_ids(&format!("{VALID},{VALID}")),
            Some(vec![VALID.to_owned(), VALID.to_owned()]),
            "two ids, duplicates and all — Go de-duplicates nowhere"
        );
        // `IsValidId` is letters-or-numbers, so upper case passes the split gate as well.
        assert_eq!(
            parse_group_ids("ABCDEFGHIJKLMNOPQRSTUVWXYZ"),
            Some(vec!["ABCDEFGHIJKLMNOPQRSTUVWXYZ".to_owned()])
        );
    }

    /// A trailing comma is one empty element, and the empty string is not a valid id — so
    /// `"<valid>,"` is a 400 while `"<valid>"` is a 200.
    #[test]
    fn a_trailing_comma_is_an_empty_element_and_fails() {
        assert_eq!(parse_group_ids(&format!("{VALID},")), None);
        assert_eq!(parse_group_ids(&format!(",{VALID}")), None);
    }

    /// The reverse: the trailing garbage cannot rescue a body whose *first* value is bad.
    #[test]
    fn a_trailing_value_does_not_rescue_a_bad_first_value() {
        assert!(!body_names_a_valid_scheme_id(
            format!(r#"{{}} {{"scheme_id":"{VALID}"}}"#).as_bytes()
        ));
    }
}
