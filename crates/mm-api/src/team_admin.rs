//! Port of what is left of `channels/api4/team.go` after the team reads, the team writes and the
//! membership writes: `teamMembersMinusGroupMembers` (:2222), `updateTeamScheme` (:2155),
//! `inviteUsersToTeam` (:1751), `inviteGuestsToChannels` (:1878) and `importTeam` (:1660).
//!
//! In its own module rather than in `teams.rs`, which is 3,400 lines already, and because these
//! five share one property the read handlers do not: **each one stops at a different point, and
//! where it stops is the whole of what this server can honestly answer.**
//!
//! # What serves and what hands over, measured rather than assumed
//!
//! | route | served here | forwarded |
//! |---|---|---|
//! | `GET …/members_minus_group_members` | everything | nothing |
//! | `PUT …/scheme` | the 400 and the unlicensed 501 | a licensed server |
//! | `POST …/invite/email` | two 403s and three 400s | every request that would send mail |
//! | `POST …/invite-guests/email` | the unlicensed 501 | a licensed server |
//! | `POST …/import` | six refusals | `importFrom=slack`, and a licensed server |
//!
//! The three forwards each happen **before any write**. `inviteUsersToTeam` writes nothing until
//! `InviteNewUsersToTeam`/`…Gracefully`, and the graceful arm additionally creates a
//! `resend_invitation_email` job — so the hand-over sits immediately in front of that branch, past
//! every gate and past nothing else. `importTeam` writes nothing until `SlackImport`. A licensed
//! server is handed the request whole, so its extra behaviour (the cloud refusal on `import`, the
//! guest-account config gates) is Go's answer and not a guess at one.
//!
//! # The two licence gates here are 501, not 403
//!
//! `updateChannelScheme` answers **403** unlicensed (see [`crate::channel_admin`]);
//! `updateTeamScheme` answers **501** with `api.team.update_team_scheme.license.error`, and
//! `inviteGuestsToChannels` answers 501 too. Measured on this stack. The two scheme handlers sit
//! in sibling files, do the same job and disagree on the status code.

use axum::extract::{Path, RawQuery, Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::member_invite::MemberInvite;
use mm_model::permission::{
    PERMISSION_ADD_USER_TO_TEAM, PERMISSION_IMPORT_TEAM, PERMISSION_INVITE_USER,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS, make_permission_error,
};
use mm_model::scheme::SchemeIDPatch;
use mm_model::user::UsersWithGroupsAndCount;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channel_admin::parse_group_ids;
use crate::channels::{LicenceGate, licence_gate, parse_page, parse_per_page, query_first};
use crate::error::ApiError;
use crate::proxy;

/// Port of `teamMembersMinusGroupMembers` (api4/team.go:2222) —
/// `GET /api/v4/teams/{team_id}/members_minus_group_members`.
///
/// The System Console's "who would this group constraint remove?" preview, for a team. The twin of
/// [`crate::channel_admin::channel_members_minus_group_members`], and it shares that handler's
/// `group_ids` parser — Go shares the compiled regex between them, from this file.
///
/// # Three differences from the channel twin, all of them on the wire
///
/// 1. **The permission is `sysconsole_read_user_management_groups`**, not
///    `…_user_management_channels`. A System Console role scoped to channels is refused here.
/// 2. **No space-channel guard**, because there is no channel; `RequireTeamId` is the whole of the
///    id gate.
/// 3. The store adds `TeamMembers.DeleteAt = 0` — see `mm_store::group_store`.
///
/// The gate *order* is identical, and it is the thing a reader gets wrong: the id, then
/// `group_ids`, then the permission. A plain user asking with a malformed `group_ids` is told about
/// `group_ids`, not about their role.
///
/// # The body carries no trailing newline
///
/// `json.Marshal` then `w.Write`, not `json.NewEncoder(w).Encode` — [D-086]'s two conventions,
/// and this route is on the newline-free side.
#[tracing::instrument(skip_all, fields(team_id = %team_id, groups, page, per_page))]
pub async fn team_members_minus_group_members(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Response {
    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
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
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS,
        )
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS],
        ))
        .into_response();
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let (users, count) = match state
        .app
        .team_members_minus_group_members(&team_id, &group_ids, page, per_page)
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
            "Api4.teamMembersMinusGroupMembers",
            "api.marshal_error",
            None,
            err.to_string(),
            500,
        ))
        .into_response(),
    }
}

/// Port of `updateTeamScheme` (api4/team.go:2155) — `PUT /api/v4/teams/{team_id}/scheme`.
///
/// # The body gate accepts the empty string, and the channel twin does not
///
/// Go's condition is `p.SchemeID == nil || (!model.IsValidId(*p.SchemeID) && *p.SchemeID != "")`.
/// The second disjunct's `&& != ""` has no counterpart in `updateChannelScheme`, so
/// `{"scheme_id":""}` is a **400 on a channel and a 501 on a team** — measured on this stack. It is
/// how a client detaches a team from its scheme, and a port that copied the channel predicate would
/// refuse exactly that call.
///
/// # Three gates, in this order
///
/// 1. `RequireTeamId` — a segment outside `[A-Za-z0-9]+` never routes (gorilla's own 404); an
///    id-shaped check failure is `invalid_url_param`.
/// 2. The body, as one 400 naming `scheme_id`: a decode failure, a missing key, an explicit `null`
///    and a malformed non-empty id are indistinguishable on the wire.
/// 3. The licence — **501**, `api.team.update_team_scheme.license.error`.
///
/// Everything past the gate (`GetScheme`, the `SchemeScopeTeam` check, `UpdateTeamScheme`) needs a
/// licensed server, which is forwarded whole.
///
/// # `Decode` stops at the first JSON value
///
/// `json.NewDecoder(r.Body).Decode(&p)` consumes one value and ignores the rest, so
/// `{"scheme_id":""} trailing garbage` passes. Reproduced with a streaming deserializer taking the
/// first item.
#[tracing::instrument(skip_all, fields(team_id = %team_id, licensed))]
pub async fn update_team_scheme(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("scheme_id").into_response();
        }
    };

    if !body_names_a_team_scheme_id(&bytes) {
        return ApiError::invalid_param("scheme_id").into_response();
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            "Api4.UpdateTeamScheme",
            "api.team.update_team_scheme.license.error",
            None,
            String::new(),
            501,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// `updateTeamScheme`'s step 2, as one predicate because Go answers it as one.
///
/// `Some("")` passes — see the handler's doc comment. That single `|| == ""` is the only difference
/// from `channel_admin::body_names_a_valid_scheme_id`, which is why the two are not shared: a
/// shared helper would have to carry the flag, and the flag is the bug someone would flip.
fn body_names_a_team_scheme_id(bytes: &[u8]) -> bool {
    let mut values = serde_json::Deserializer::from_slice(bytes).into_iter::<SchemeIDPatch>();
    match values.next() {
        Some(Ok(patch)) => patch
            .scheme_id
            .as_deref()
            .is_some_and(|id| is_valid_id(id) || id.is_empty()),
        _ => false,
    }
}

/// Port of `inviteUsersToTeam` (api4/team.go:1751) —
/// `POST /api/v4/teams/{team_id}/invite/email`.
///
/// # Six refusals are served; every request that would send mail is handed over
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `team_id` is not an id | 400 `api.context.invalid_url_param.app_error` |
/// | 2 | no `invite_user` **on this team** | 403 |
/// | 3 | no `add_user_to_team` on this team | 403 — **naming `invite_user`** |
/// | 4 | the body does not decode | 400 `api.team.invite_members_to_team_and_channels.invalid_body.app_error` |
/// | 5 | `emails` is empty | 400 `api.context.invalid_body_param.app_error` naming `user_email` |
/// | 6 | `profiles` without `?graceful=` | 400 `api.team.invite_members.profiles_graceful.app_error` |
///
/// **Row 3 is Go's, not a transcription slip.** `SetPermissionError(model.PermissionInviteUser)`
/// sits under the `add_user_to_team` test (api4/team.go:1765), so a caller who may invite but may
/// not add is told they lack `invite_user` — which they have. Reproduced, because the detail string
/// reaches the log and the wire.
///
/// # Why the send forwards
///
/// `InviteNewUsersToTeam` and `InviteNewUsersToTeamGracefully` build and send the invitation mail
/// through the email service and the graceful arm then creates a `resend_invitation_email` job.
/// Neither is ported, and there is no partial answer: every success on this route is an email. So
/// the hand-over sits in front of that branch and behind all six gates, which means **nothing has
/// been written when it happens** — `ValidateUserPermissionsOnChannels`, the last thing before it,
/// is a read whose result only narrows the forwarded call's own copy. See [D-490].
///
/// # `graceful` is presence-and-non-empty
///
/// `r.URL.Query().Get("graceful") != ""`, so `?graceful` (bare) is **false** and `?graceful=0` is
/// true. It only decides which of the two forwarded branches Go takes, except in row 6, where it
/// decides a 400.
///
/// # The body has two shapes
///
/// `MemberInvite` unmarshals a bare `["a@b.c"]` array as well as the object — see
/// `mm_model::member_invite`. `[]` is therefore a well-formed body with no emails, which is row 5
/// and not row 4.
#[tracing::instrument(skip_all, fields(team_id = %team_id, graceful, forwarded))]
pub async fn invite_users_to_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let graceful = query_first(query.as_deref(), "graceful").is_some_and(|v| !v.is_empty());
    tracing::Span::current().record("graceful", graceful);

    if !is_valid_id(&team_id) {
        return ApiError::invalid_url_param("team_id").into_response();
    }

    for permission in [&PERMISSION_INVITE_USER, &PERMISSION_ADD_USER_TO_TEAM] {
        if !state
            .app
            .session_has_permission_to_team(&session.0, &team_id, permission)
            .await
        {
            // Both arms report `invite_user`: Go passes `model.PermissionInviteUser` to
            // `SetPermissionError` under *either* test.
            return ApiError::from(*make_permission_error(
                &session.0,
                &[&PERMISSION_INVITE_USER],
            ))
            .into_response();
        }
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return invalid_invite_body().into_response();
        }
    };

    let mut values = serde_json::Deserializer::from_slice(&bytes).into_iter::<MemberInvite>();
    let invite = match values.next() {
        Some(Ok(invite)) => invite,
        _ => return invalid_invite_body().into_response(),
    };

    if invite.emails.is_empty() {
        return ApiError::invalid_param("user_email").into_response();
    }

    if !graceful && invite.profiles.iter().flatten().next().is_some() {
        return ApiError::from(AppError::new(
            "Api4.inviteUsersToTeam",
            "api.team.invite_members.profiles_graceful.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    tracing::Span::current().record("forwarded", true);
    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    proxy::forward_to_go(State(state), request).await
}

/// Row 4 of [`invite_users_to_team`]'s table. Note the `where` is `Api4.inviteUsersToTeams`,
/// **plural**, and the neighbouring 400 two lines later is singular; both are Go's spellings.
fn invalid_invite_body() -> ApiError {
    ApiError::from(AppError::new(
        "Api4.inviteUsersToTeams",
        "api.team.invite_members_to_team_and_channels.invalid_body.app_error",
        None,
        String::new(),
        400,
    ))
}

/// Port of `inviteGuestsToChannels` (api4/team.go:1878) —
/// `POST /api/v4/teams/{team_id}/invite-guests/email`.
///
/// **The licence check is the handler's first statement**, ahead of `RequireTeamId`, so an
/// unlicensed server answers the same 501 for a team that does not exist, for a malformed body and
/// for a caller with no permission at all. Measured: `/api/v4/teams/zzz/invite-guests/email` with an
/// empty object is the licence 501, not a 404 and not a 400.
///
/// That makes this the whole reachable behaviour of the route on this stack. Everything after it —
/// `GuestAccountsSettings.Enable` (a second 501), `EnableGuestMagicLink` (a 403), `RequireTeamId`,
/// the `invite_guest` permission, `License().Features.GuestAccounts` (a third refusal, this one
/// 403 with the *same* error id as the second), `GuestsInvite.IsValid` and the send — needs a
/// licence, and a licensed server is forwarded whole.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn invite_guests_to_channels(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            "Api4.InviteGuestsToChannels",
            "api.team.invite_guests_to_channels.license.error",
            None,
            String::new(),
            501,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// Port of `importTeam` (api4/team.go:1660) — `POST /api/v4/teams/{team_id}/import`.
///
/// # Eight refusals are served; the one arm that imports anything is handed over
///
/// | # | check | answer |
/// |---|---|---|
/// | 1 | `team_id` is not an id | 400 `api.context.invalid_url_param.app_error` |
/// | 2 | no `import_team` on this team | 403 |
/// | 3 | the body is not `multipart/form-data` | **500** `api.team.import_team.parse.app_error` |
/// | 4 | no `importFrom` part | 400 `…no_import_from.app_error` |
/// | 5 | no `filesize` part | 400 `…unavailable.app_error` |
/// | 6 | `filesize` is not an integer | 400 `…integer.app_error` |
/// | 7 | no `file` part | 400 `…no_file.app_error` |
/// | 8 | `importFrom` is anything but `slack` | 400 `…unknown_import_from.app_error` |
///
/// Row 3 is a **500 for a malformed client body**, which is Go's answer (`StatusInternalServerError`
/// at api4/team.go:1678) and measured here; `createEmoji` answers 400 for the identical failure.
///
/// Go has one more branch between 7 and 8 — `len(fileInfoArray) <= 0`, a different error id
/// (`…array.app_error`) for an empty list under a present key. `ParseMultipartForm` never builds
/// one, so it is unreachable from a client; it is nonetheless reproduced below, because the
/// alternative is a parser difference silently changing the error id.
///
/// # The cloud refusal is not ported, and cannot be reached
///
/// The handler's first statement is `License().IsCloud()` → 403 `api.restricted_system_admin`. A
/// cloud installation is licensed by construction, and a licensed server is forwarded here before
/// anything else runs, so Go answers that 403 itself.
///
/// # Why `slack` forwards
///
/// `App.SlackImport` reads a Slack export zip and creates users, channels and posts from it —
/// several thousand lines, none of it ported. The hand-over is in front of the `switch`, which is
/// the first statement in the handler that writes anything. See [D-491].
#[tracing::instrument(skip_all, fields(team_id = %team_id, import_from, forwarded))]
pub async fn import_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    tracing::Span::current().record("forwarded", false);
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the upload body");
            return import_error("api.team.import_team.parse.app_error", 500).into_response();
        }
    };

    match refuse_import(&state, &team_id, &session, &parts, &bytes).await {
        Some(refusal) => refusal.into_response(),
        None => {
            tracing::Span::current().record("forwarded", true);
            let request = Request::from_parts(parts, axum::body::Body::from(bytes));
            proxy::forward_to_go(State(state), request).await
        }
    }
}

/// Everything `importTeam` answers itself. `None` is the hand-over, and it is reached from exactly
/// two places: a licensed server (whose first statement, the cloud check, only it can evaluate) and
/// `importFrom=slack`.
async fn refuse_import(
    state: &AppState,
    team_id: &str,
    session: &AuthenticatedSession,
    parts: &axum::http::request::Parts,
    bytes: &[u8],
) -> Option<ApiError> {
    // The licence decides the *first* statement of the handler, so it is asked first here too.
    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Licensed) => return None,
        Ok(mm_app::license::LicenseState::Unlicensed) => {}
        Err(err) => return Some(ApiError::from(err)),
    }

    if !is_valid_id(team_id) {
        return Some(ApiError::invalid_url_param("team_id"));
    }

    if !state
        .app
        .session_has_permission_to_team(&session.0, team_id, &PERMISSION_IMPORT_TEAM)
        .await
    {
        return Some(ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_IMPORT_TEAM],
        )));
    }

    let content_type = parts
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let form = match crate::multipart::parse_form(content_type, bytes) {
        Ok(form) => form,
        Err(err) => {
            tracing::debug!(error = %err, "the import body is not multipart/form-data");
            return Some(import_error("api.team.import_team.parse.app_error", 500));
        }
    };

    // **Not `?`.** `None` out of this function is the hand-over to Go, so a missing part has to be
    // spelled out as a refusal; a `?` here would forward every body that names no import source.
    let Some(import_from) = form.first_value("importFrom") else {
        return Some(import_error(
            "api.team.import_team.no_import_from.app_error",
            400,
        ));
    };
    tracing::Span::current().record("import_from", import_from);

    let Some(file_size) = form.first_value("filesize") else {
        return Some(import_error(
            "api.team.import_team.unavailable.app_error",
            400,
        ));
    };
    // `strconv.ParseInt(s, 10, 64)` — base 10, 64 bits, and a leading `+` is accepted. Rust's
    // `i64::from_str` accepts the same set, including the `+`.
    if file_size.parse::<i64>().is_err() {
        return Some(import_error("api.team.import_team.integer.app_error", 400));
    }

    match form.file.get("file") {
        None => return Some(import_error("api.team.import_team.no_file.app_error", 400)),
        Some(files) if files.is_empty() => {
            return Some(import_error("api.team.import_team.array.app_error", 400));
        }
        Some(_) => {}
    }

    if import_from != "slack" {
        return Some(import_error(
            "api.team.import_team.unknown_import_from.app_error",
            400,
        ));
    }

    None
}

/// Every refusal `importTeam` gives shares a `where` and differs only in id and status.
fn import_error(id: &str, status: i32) -> ApiError {
    ApiError::from(AppError::new("importTeam", id, None, String::new(), status))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "abcdefghijklmnopqrstuvwxyz";

    /// The one clause that separates this gate from the channel one: `""` passes here.
    #[test]
    fn the_team_scheme_gate_accepts_the_empty_string() {
        assert!(body_names_a_team_scheme_id(br#"{"scheme_id":""}"#));
        assert!(body_names_a_team_scheme_id(
            br#"{"scheme_id":"abcdefghijklmnopqrstuvwxyz"}"#
        ));
        // `IsValidId` is 26 characters of Unicode letters or numbers, which upper case satisfies.
        assert!(body_names_a_team_scheme_id(
            br#"{"scheme_id":"ABCDEFGHIJKLMNOPQRSTUVWXYZ"}"#
        ));
    }

    /// ...and the four ways to fail it, which Go collapses into one 400.
    #[test]
    fn the_team_scheme_gate_refuses_what_go_refuses() {
        assert!(!body_names_a_team_scheme_id(b"{}"));
        assert!(!body_names_a_team_scheme_id(br#"{"scheme_id":null}"#));
        assert!(!body_names_a_team_scheme_id(br#"{"scheme_id":"nope"}"#));
        assert!(!body_names_a_team_scheme_id(b"notjson"));
        assert!(!body_names_a_team_scheme_id(b""));
        // 25 and 27 characters: `IsValidId` is exactly 26, and neither is empty.
        assert!(!body_names_a_team_scheme_id(
            format!(r#"{{"scheme_id":"{}"}}"#, &VALID[..25]).as_bytes()
        ));
        assert!(!body_names_a_team_scheme_id(
            format!(r#"{{"scheme_id":"{VALID}z"}}"#).as_bytes()
        ));
    }

    /// `Decode` consumes one value and ignores the rest, so trailing garbage is not an error.
    #[test]
    fn the_team_scheme_gate_stops_at_the_first_json_value() {
        assert!(body_names_a_team_scheme_id(
            br#"{"scheme_id":""} and then some nonsense"#
        ));
    }

    /// The bare-array body form, which is what makes `[]` a row-5 refusal rather than a row-4 one.
    #[test]
    fn a_bare_array_body_decodes_as_emails() {
        let parse = |body: &[u8]| -> Option<MemberInvite> {
            serde_json::Deserializer::from_slice(body)
                .into_iter::<MemberInvite>()
                .next()
                .and_then(Result::ok)
        };
        assert_eq!(parse(b"[]").map(|i| i.emails.len()), Some(0));
        assert_eq!(
            parse(br#"["a@b.invalid"]"#).map(|i| i.emails),
            Some(vec!["a@b.invalid".to_owned()])
        );
        assert_eq!(
            parse(br#"{"emails":["a@b.invalid"]}"#).map(|i| i.emails),
            Some(vec!["a@b.invalid".to_owned()])
        );
        // A body that is neither is row 4.
        assert!(parse(b"notjson").is_none());
        assert!(parse(br#"{"emails":5}"#).is_none());
    }
}
