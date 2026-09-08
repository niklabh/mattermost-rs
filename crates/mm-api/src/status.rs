//! Port of `getUserStatus` and `getUserStatusesByIds` (channels/api4/status.go:27, :50),
//! reached as `GET /api/v4/users/{user_id}/status` and `POST /api/v4/users/status/ids`.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{PERMISSION_EDIT_OTHER_USERS, make_permission_error};
use mm_model::status::{STATUS_OUT_OF_OFFICE, Status};
use mm_model::utils::{AppError, sorted_array_from_json};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{ME, require_id};
use crate::error::ApiError;

/// `model.PayloadParseError` (model/utils.go:42).
const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";

/// Port of `getUserStatus` (api4/status.go:27).
///
/// # It is the *list* lookup, not `GetStatus`
///
/// The handler calls `GetUserStatusesByIds([]string{userId})` and writes element zero — not
/// `App.GetStatus`, whose not-found branch would 404 with `app.status.get.missing.app_error`.
/// The difference is the whole behaviour of the route: a user with no `Status` row, and an id
/// that is no user at all, both answer **200 `{"user_id": …, "status": "offline", …}`**. The
/// `len(statusMap) == 0` 404 that follows (`api.status.user_not_found.app_error`) is reachable
/// only with `EnableUserStatuses` off, when the platform returns an empty list for any input;
/// it is reproduced because the config stand-in may one day be real.
///
/// # No permission check
///
/// Go's comment says so in as many words. Any authenticated session may read anyone's status,
/// including a non-existent anyone — which is why the 200 for an unknown id is not an
/// information leak Go would have closed.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(statusMap[0])` — **trailing newline**, and plain `Encode`, not
/// `Status.ToJSON`: `active_channel` is omitted only because a database-read status has it
/// empty, not because anything blanks it ([D-086]'s rule, third instance).
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn get_user_status(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireUserId` resolves `me` before it validates (web/context.go:301).
    let user_id = if user_id == ME {
        session.0.user_id
    } else {
        user_id
    };
    require_id(&user_id, "user_id")?;

    let mut statuses = state.app.get_user_statuses_by_ids(&[user_id]).await?;

    // `statusMap[0]`: the platform layer answers every id it is asked about, so the list is
    // never empty while statuses are enabled.
    let Some(status) = statuses.drain(..).next() else {
        return Err(ApiError::from(AppError::new(
            "UserStatus",
            "api.status.user_not_found.app_error",
            None,
            String::new(),
            404,
        )));
    };

    let mut body = serde_json::to_vec(&status).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise Status");
        ApiError::from(AppError::new(
            "getUserStatus",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok(json_ok(body))
}

/// Port of `getUserStatusesByIds` (api4/status.go:50).
///
/// The body is a JSON array of user ids — what the webapp sends on every load for every user it
/// is about to render. Validation is [`parse_user_ids`]; there is no permission check; the
/// answer is `json.Marshal` + `w.Write`, so **no trailing newline** — the other side of [D-086]
/// from the single-user route above.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_user_statuses_by_ids(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            payload_parse_error()
        })?;

    let user_ids = parse_user_ids(&bytes)?;
    tracing::Span::current().record("count", user_ids.len());

    let statuses = state.app.get_user_statuses_by_ids(&user_ids).await?;

    let body = serde_json::to_vec(&statuses).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise statuses");
        ApiError::from(AppError::new(
            "getUserStatusesByIds",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;

    Ok(json_ok(body))
}

fn json_ok(body: Vec<u8>) -> Response {
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

/// `model.NewAppError("getUserStatusesByIds", model.PayloadParseError, nil, "", 400)`.
fn payload_parse_error() -> ApiError {
    ApiError::from(AppError::new(
        "getUserStatusesByIds",
        PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
}

/// The first half of `getUserStatusesByIds`: [`sorted_array_from_json`] (model/utils.go:546)
/// followed by the handler's own two checks. Returns the ids **sorted and de-duplicated**, which
/// is the list the app layer receives and whose order reaches the wire.
///
/// The branches, in Go's order, and the error each one answers:
///
/// 1. **The body is not a JSON array of strings** → 400 `api.payload.parse.error`. The
///    decoder's habits (trailing bytes ignored, `null` elements as `""`, a `null` body as an
///    empty list) live in `sorted_array_from_json` and its oracle.
/// 2. **No ids** (`null`, or `[]`) → 400 `invalid_body_param` naming `user_ids`.
/// 3. **Any id whose byte length is not 26** → the same 400. `len(userId)` is a byte count and
///    nothing checks the charset, so `"ZZZZZZZZZZZZZZZZZZZZZZZZZZ"` passes and is answered
///    `offline` — `is_valid_id` is deliberately *not* consulted here.
///
/// De-duplication happens before the length check (it is inside `SortedArrayFromJSON`), so a
/// body of two copies of one bad id fails the same way as one copy — unobservable, but the order
/// is Go's.
#[allow(clippy::result_large_err)]
fn parse_user_ids(body: &[u8]) -> Result<Vec<String>, ApiError> {
    let user_ids = sorted_array_from_json(body).map_err(|err| {
        tracing::debug!(error = %err, "user_ids body did not decode");
        payload_parse_error()
    })?;

    if user_ids.is_empty() {
        return Err(ApiError::invalid_param("user_ids"));
    }

    if user_ids.iter().any(|id| id.len() != 26) {
        return Err(ApiError::invalid_param("user_ids"));
    }

    Ok(user_ids)
}

/// Port of `updateUserStatus` (api4/status.go) — `PUT /api/v4/users/{user_id}/status`.
///
/// # The body's `user_id` must match the path's, and that check is a **400 naming `user_id`**
///
/// It runs before the permission check, so a mismatched body is a 400 even for a caller who could
/// not have updated that user anyway.
///
/// # Four statuses, and `ooo` is not one of them
///
/// The switch accepts `online`, `offline`, `away` and `dnd`; anything else — including the
/// `ooo` that `Status.Status` can hold — is `SetInvalidParam("status")`. So this route can never
/// *set* out-of-office, only move a user out of it.
///
/// # Leaving `ooo` disables the auto-responder, and that part is forwarded
///
/// When the stored status is `ooo` and the new one is not, Go calls `DisableAutoResponder`, which
/// patches the user's notify props. `PatchUser` is unported, and skipping it would leave the
/// auto-responder armed for a user who is no longer away — a persisted inconsistency a reload does
/// not fix. Reachable only from a status row already saying `ooo`, which no route here writes.
///
/// # The answer is `getUserStatus`'s own body
///
/// Go tail-calls the read handler, so the response is a fresh read rather than the struct just
/// written — and it carries the `Status` shape, not `{"status":"OK"}`.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn update_user_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(user_id): Path<String>,
    request: Request,
) -> Response {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("status").into_response();
        }
    };
    let submitted: Status = match serde_json::from_slice(&bytes) {
        Ok(status) => status,
        Err(err) => {
            tracing::debug!(error = %err, "status body did not decode");
            return ApiError::invalid_param("status").into_response();
        }
    };

    // "The user being updated in the payload must be the same one as indicated in the URL."
    if submitted.user_id != user_id {
        return ApiError::invalid_param("user_id").into_response();
    }

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    // Go logs and carries on when the current status cannot be read, so a missing row does not
    // stop the update — it only skips the out-of-office branch.
    if let Ok(current) = state.app.get_status(&user_id).await
        && current.status == STATUS_OUT_OF_OFFICE
        && submitted.status != STATUS_OUT_OF_OFFICE
    {
        tracing::Span::current().record("forwarded", true);
        tracing::debug!("leaving out-of-office needs DisableAutoResponder; handing to Go");
        let request = Request::from_parts(parts, axum::body::Body::from(bytes));
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match submitted.status.as_str() {
        "online" => state.app.set_status_online(&user_id, true).await,
        "offline" => state.app.set_status_offline(&user_id, true, false).await,
        "away" => state.app.set_status_away_if_needed(&user_id, true).await,
        "dnd" => {
            state
                .app
                .set_status_do_not_disturb_timed(&user_id, submitted.dnd_end_time)
                .await
        }
        // **Including `ooo`**, which the model can hold and this route cannot set.
        _ => return ApiError::invalid_param("status").into_response(),
    }

    // Go tail-calls `getUserStatus`, so the answer is a fresh read — the `Status` shape with a
    // trailing newline, not `{"status":"OK"}`.
    match get_user_status(State(state), Path(user_id), session).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `updateUserCustomStatus` (api4/status.go:136), `PUT /users/{user_id}/status/custom`.
///
/// # Three refusals share one line
///
/// `jsonErr != nil || (Emoji == "" && Text == "") || !AreDurationAndExpirationTimeValid()` is a
/// single `if`, so a malformed body, an empty status and an expired one are indistinguishable on
/// the wire: all three are **400 `custom_status`**. Splitting them would be an improvement and a
/// divergence.
///
/// # The gate is `TeamSettings`, and it answers 501
///
/// `EnableCustomUserStatuses` is a *different* setting from `ServiceSettings.EnableUserStatuses`,
/// and off means **501 with a body**, where the plain status writes off means a silent 200.
///
/// # Order
///
/// The decode runs **before** the permission check, so a caller who may not touch this user still
/// gets 400 for a bad body rather than 403.
#[tracing::instrument(skip_all, fields(user_id = %user_id, forwarded))]
pub async fn update_user_custom_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let user_id = resolve_me(&session, user_id);
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    if !state.app.config().enable_custom_user_statuses {
        return custom_status_disabled("updateUserCustomStatus").into_response();
    }

    let decoded: Option<mm_model::custom_status::CustomStatus> = serde_json::from_slice(&body).ok();
    let Some(mut custom_status) = decoded.filter(|cs| {
        !(cs.emoji.is_empty() && cs.text.is_empty()) && cs.are_duration_and_expiration_time_valid()
    }) else {
        return ApiError::invalid_param("custom_status").into_response();
    };

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    custom_status.pre_save();
    match state.app.set_custom_status(&user_id, &custom_status).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(*err).into_response(),
    }
}

/// Port of `removeUserCustomStatus` (api4/status.go:169),
/// `DELETE /users/{user_id}/status/custom`.
///
/// No body at all, so the gate and the permission check are the whole handler.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn remove_user_custom_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(user_id): Path<String>,
) -> Response {
    let user_id = resolve_me(&session, user_id);
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    if !state.app.config().enable_custom_user_statuses {
        return custom_status_disabled("removeUserCustomStatus").into_response();
    }
    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    match state.app.remove_custom_status(&user_id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(*err).into_response(),
    }
}

/// Port of `removeUserRecentCustomStatus` (api4/status.go:193), reached as **both**
/// `DELETE /users/{user_id}/status/custom/recent` and
/// `POST /users/{user_id}/status/custom/recent/delete`.
///
/// The two paths are the same function registered twice — the POST form exists for clients that
/// cannot send a body with DELETE, and both read one.
///
/// Unlike its sibling, the decode failure is its **own** condition, so an undecodable body is
/// `recent_custom_status` and an empty-but-valid one is not refused here at all — it reaches
/// `RemoveRecentCustomStatus`, whose `Contains` answers false for a status with no emoji and no
/// text, and comes back as the recents-delete error instead.
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn remove_user_recent_custom_status(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    Path(user_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let user_id = resolve_me(&session, user_id);
    if let Err(err) = require_id(&user_id, "user_id") {
        return err.into_response();
    }
    if !state.app.config().enable_custom_user_statuses {
        return custom_status_disabled("removeUserRecentCustomStatus").into_response();
    }

    let Ok(status) = serde_json::from_slice::<mm_model::custom_status::CustomStatus>(&body) else {
        return ApiError::invalid_param("recent_custom_status").into_response();
    };

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return ApiError::from(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    match state
        .app
        .remove_recent_custom_status(&user_id, &status)
        .await
    {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(*err).into_response(),
    }
}

/// `c.Params.UserId` with `me` already resolved — the router's own substitution, applied by
/// `RequireUserId` on every route in this file.
fn resolve_me(session: &AuthenticatedSession, user_id: String) -> String {
    if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    }
}

/// The **501** all four custom-status routes answer when `EnableCustomUserStatuses` is off.
///
/// `Where` differs per handler and is on the wire, so it is a parameter rather than a constant.
fn custom_status_disabled(where_: &'static str) -> ApiError {
    ApiError::from(*AppError::boxed(
        where_,
        "api.custom_status.disabled",
        None,
        String::new(),
        501,
    ))
}

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing newline**.
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

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn error_of(body: &str) -> Box<AppError> {
        parse_user_ids(body.as_bytes())
            .expect_err("this body must be rejected")
            .0
    }

    #[test]
    fn a_valid_body_is_sorted_and_deduplicated() {
        let ids = parse_user_ids(format!(r#"["{B}","{A}","{B}"]"#).as_bytes()).expect("valid");
        assert_eq!(ids, vec![A.to_owned(), B.to_owned()]);
    }

    /// Branch 1: anything that is not an array of strings is the payload error, with Go's
    /// `where` and id.
    #[test]
    fn a_body_that_is_not_an_array_of_strings_is_a_payload_parse_error() {
        for body in ["", "{", "{}", "\"abc\"", "[1]", "[{}]", "[[\"a\"]]"] {
            let err = error_of(body);
            assert_eq!(err.id, PAYLOAD_PARSE_ERROR, "body {body:?}");
            assert_eq!(err.status_code, 400, "body {body:?}");
            assert_eq!(err.where_, "getUserStatusesByIds", "body {body:?}");
        }
    }

    /// Branch 2: `null` and `[]` both reach `len(userIds) == 0` — the *parameter* error, not the
    /// parse error, and naming `user_ids`.
    #[test]
    fn null_and_an_empty_array_are_the_invalid_parameter_error() {
        for body in ["null", "[]", " [ ] "] {
            let err = error_of(body);
            assert_eq!(
                err.id, "api.context.invalid_body_param.app_error",
                "body {body:?}"
            );
            assert_eq!(err.status_code, 400);
            assert_eq!(
                err.params.as_ref().and_then(|p| p.get("Name")),
                Some(&serde_json::Value::String("user_ids".to_owned()))
            );
        }
    }

    /// Branch 3: the check is byte length 26 and nothing else. A 25- or 27-byte id fails; a
    /// 26-byte string outside the id alphabet passes, as does a 26-**byte** non-ASCII one.
    #[test]
    fn the_id_check_is_byte_length_26_and_nothing_else() {
        let short = &A[..25];
        let long = format!("{A}a");
        for bad in [
            format!(r#"["{short}"]"#),
            format!(r#"["{long}"]"#),
            format!(r#"["{A}","{short}"]"#),
            // A 26-character string that is 28 bytes: Go's `len` counts bytes.
            format!(r#"["é{}"]"#, &A[..25]),
        ] {
            let err = error_of(&bad);
            assert_eq!(
                err.id, "api.context.invalid_body_param.app_error",
                "body {bad:?}"
            );
        }

        let shouting = "Z".repeat(26);
        let ids = parse_user_ids(format!(r#"["{shouting}"]"#).as_bytes())
            .expect("26 bytes is 26 bytes; IsValidId is not consulted");
        assert_eq!(ids, vec![shouting]);

        // 13 two-byte characters are 26 bytes.
        let multibyte = "é".repeat(13);
        assert_eq!(multibyte.len(), 26);
        let ids = parse_user_ids(format!(r#"["{multibyte}"]"#).as_bytes()).expect("26 bytes");
        assert_eq!(ids, vec![multibyte]);
    }

    /// `json.Decoder.Decode` reads one value and stops. Trailing bytes — even unparseable ones —
    /// are never looked at, so they cannot turn a good body into a 400.
    #[test]
    fn trailing_garbage_after_the_array_is_ignored_like_go() {
        let ids = parse_user_ids(format!(r#"["{A}"] this is not json"#).as_bytes())
            .expect("Decode stops after the first value");
        assert_eq!(ids, vec![A.to_owned()]);

        let ids = parse_user_ids(format!(r#"["{A}"]["{B}"]"#).as_bytes())
            .expect("the second array is never read");
        assert_eq!(ids, vec![A.to_owned()]);
    }

    /// A `null` element decodes to `""` in Go rather than failing the decode, so it reaches the
    /// length check and fails *there* — the parameter error, not the parse error.
    #[test]
    fn a_null_element_is_an_empty_string_and_fails_the_length_check() {
        let err = error_of(&format!(r#"["{A}", null]"#));
        assert_eq!(err.id, "api.context.invalid_body_param.app_error");
    }

    /// `Encode` for the single route, `Marshal` for the list: one newline, the other none.
    #[test]
    fn the_two_routes_differ_by_exactly_the_trailing_newline() {
        let status = mm_model::status::Status {
            user_id: A.to_owned(),
            status: "offline".to_owned(),
            ..Default::default()
        };

        let mut single = serde_json::to_vec(&status).expect("serialises");
        single.push(b'\n');
        assert_eq!(single.last(), Some(&b'\n'));

        let list = serde_json::to_vec(&[status]).expect("serialises");
        assert_ne!(list.last(), Some(&b'\n'));
        assert_eq!(
            std::str::from_utf8(&list).expect("utf8"),
            format!(
                r#"[{{"user_id":"{A}","status":"offline","manual":false,"last_activity_at":0,"dnd_end_time":0}}]"#
            ),
            "a synthesised status has five keys: active_channel is omitted, prev_status is json:\"-\""
        );
    }
}
