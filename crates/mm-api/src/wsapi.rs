//! Port of `channels/wsapi` — the six actions `wsapi.Init` registers on the websocket router —
//! and of `webSocketHandler.ServeWebSocket` (websocket_handler.go:25), the wrapper each of them
//! runs inside.
//!
//! The router ([`crate::websocket`]) has already refused an empty action, a non-positive `seq`
//! and an unauthenticated connection by the time anything here runs, and has handled the two
//! actions it owns itself (`authentication_challenge`, `presence`).
//!
//! # The wrapper reads the session again
//!
//! Every action re-resolves the connection's token through `GetSession` before the handler runs,
//! and a failure is answered as a `FAIL` response carrying the session error — **not** as the
//! router's `not_authenticated`. A connection whose session was revoked after it authenticated
//! therefore sees `api.context.invalid_token.error`, and the handler never runs. The session the
//! handler acts as is this fresh one, not the one the connection was upgraded with.
//!
//! # What is not here
//!
//! - `user_typing` calls `ExtendSessionExpiryIfNeeded` first. Not ported on any route ([D-214]);
//!   it is a no-op on the parity stack, where `ExtendSessionLengthWithActivity` is off.
//! - `posted_notify_ack` counts notification metrics. This server builds no metrics interface,
//!   which is Go's `notificationMetricsDisabled` arm — the counters are skipped on both.

use std::collections::{BTreeMap, HashMap};
use std::ops::ControlFlow;
use std::sync::Arc;

use mm_app::hub::{OutgoingFrame, WebConn};
use mm_model::permission::PERMISSION_CREATE_POST;
use mm_model::session::Session;
use mm_model::status::status_map_to_interface_map;
use mm_model::utils::{AppError, StringInterface, array_from_interface, get_millis, is_valid_id};
use mm_model::version::CURRENT_VERSION;
use mm_model::websocket_message::{WEBSOCKET_POSTED_NOTIFY_ACK, WebSocketResponse};
use mm_model::websocket_request::WebSocketRequest;

use crate::AppState;
use crate::websocket::STATUS_OK;

/// The six names `wsapi.Init` registers (user.go:12, system.go:12, status.go:12).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Ping,
    PostedNotifyAck,
    UserTyping,
    UserUpdateActiveStatus,
    GetStatuses,
    GetStatusesByIds,
}

impl Action {
    /// `wr.handlers[r.Action]` (websocket_router.go:115). `None` is Go's `bad_action`.
    pub(crate) fn from_name(name: &str) -> Option<Action> {
        Some(match name {
            "ping" => Action::Ping,
            WEBSOCKET_POSTED_NOTIFY_ACK => Action::PostedNotifyAck,
            "user_typing" => Action::UserTyping,
            "user_update_active_status" => Action::UserUpdateActiveStatus,
            "get_statuses" => Action::GetStatuses,
            "get_statuses_by_ids" => Action::GetStatusesByIds,
            _ => return None,
        })
    }
}

/// What a handler asks the wrapper to do.
#[derive(Debug)]
enum Outcome {
    /// `NewWebSocketResponse(StatusOk, seq, data)`. An empty or absent map leaves no `data` key.
    Reply(Option<StringInterface>),
    /// Close the connection without answering — see [`ack_type_assertion_panics`].
    Drop,
}

type HandlerResult = Result<Outcome, Box<AppError>>;

/// Port of `webSocketHandler.ServeWebSocket` (websocket_handler.go:25).
///
/// `Break` closes the connection; every refusal is a `Continue` with a `FAIL` response queued.
pub(crate) async fn serve_web_socket(
    state: &AppState,
    conn: &Arc<WebConn>,
    action: Action,
    request: &WebSocketRequest,
) -> ControlFlow<()> {
    // "Don't log ping requests to reduce log noise".
    if action != Action::Ping {
        tracing::debug!(action = %request.action, "Websocket request");
    }

    let session = match state.app.get_session(&conn.session().token).await {
        Ok(session) => session,
        Err(err) => {
            tracing::error!(
                action = %request.action,
                seq = request.seq,
                user_id = %conn.user_id(),
                error = %err,
                "websocket session error"
            );
            reply_error(state, conn, request.seq, *err);
            return ControlFlow::Continue(());
        }
    };

    match run(state, action, request, &session).await {
        Ok(Outcome::Reply(data)) => send(
            state,
            conn,
            WebSocketResponse::new(STATUS_OK, request.seq, data),
        ),
        Ok(Outcome::Drop) => return ControlFlow::Break(()),
        Err(err) => {
            tracing::error!(
                action = %request.action,
                seq = request.seq,
                user_id = %conn.user_id(),
                error = %err,
                "websocket request handling error"
            );
            reply_error(state, conn, request.seq, *err);
        }
    }
    ControlFlow::Continue(())
}

async fn run(
    state: &AppState,
    action: Action,
    request: &WebSocketRequest,
    session: &Session,
) -> HandlerResult {
    match action {
        Action::Ping => Ok(Outcome::Reply(Some(ping_data(get_millis())))),
        Action::PostedNotifyAck => Ok(posted_notify_ack(request, session)),
        Action::UserTyping => user_typing(state, request, session).await,
        Action::UserUpdateActiveStatus => user_update_active_status(state, request, session).await,
        Action::GetStatuses => Ok(Outcome::Reply(Some(get_statuses(state)))),
        Action::GetStatusesByIds => get_statuses_by_ids(state, request).await,
    }
}

/// `req.Data[key]`, which is Go's `nil` both for an absent key and for a JSON `null`.
fn field<'a>(request: &'a WebSocketRequest, key: &str) -> Option<&'a serde_json::Value> {
    request
        .data
        .as_ref()?
        .get(key)
        .filter(|value| !value.is_null())
}

/// Port of `ping` (system.go:16).
///
/// `node_id` is always `""`: Go writes the literal, not the cluster's node id. Keys are inserted
/// in the order `json.Encoder` sorts a map into.
fn ping_data(now: i64) -> StringInterface {
    let mut data = StringInterface::new();
    data.insert(
        "node_id".to_owned(),
        serde_json::Value::String(String::new()),
    );
    data.insert("server_time".to_owned(), serde_json::Value::from(now));
    data.insert(
        "text".to_owned(),
        serde_json::Value::String("pong".to_owned()),
    );
    data.insert(
        "version".to_owned(),
        serde_json::Value::String(CURRENT_VERSION.to_owned()),
    );
    data
}

/// Port of `websocketNotificationAck` (system.go:25).
///
/// Always an `OK` with no data — except in the one case Go does not answer at all.
fn posted_notify_ack(request: &WebSocketRequest, session: &Session) -> Outcome {
    tracing::debug!(
        user_id = %session.user_id,
        user_agent = ?field(request, "user_agent"),
        post_id = ?field(request, "post_id"),
        status = ?field(request, "status"),
        reason = ?field(request, "reason"),
        "Websocket notification acknowledgment"
    );

    if ack_type_assertion_panics(field(request, "status"), field(request, "reason")) {
        tracing::warn!(
            user_id = %session.user_id,
            "posted_notify_ack: a non-string status or reason; closing the connection as Go does"
        );
        return Outcome::Drop;
    }
    Outcome::Reply(None)
}

/// Whether Go's `status.(string)` or `reason.(string)` (system.go:44, 50) would panic.
///
/// Both are **unchecked** type assertions, so a client that sends `"status": 1` panics the Go
/// read pump. `readPump` runs on the handler goroutine, whose deferred `WebSocket.Close` runs as
/// the panic unwinds and whose panic `net/http` recovers — so what the client sees is its
/// connection closing with no response, and that is what [`Outcome::Drop`] reproduces. (Go also
/// never unregisters that connection from its hub; that half is a leak, not a behaviour.)
///
/// `status` is asserted whenever it is non-nil. `reason` is asserted only when it is non-nil too:
/// a nil reason either returns early or skips the assertion, whatever `status` says.
fn ack_type_assertion_panics(
    status: Option<&serde_json::Value>,
    reason: Option<&serde_json::Value>,
) -> bool {
    match status {
        None => false,
        Some(serde_json::Value::String(_)) => {
            reason.is_some_and(|reason| !matches!(reason, serde_json::Value::String(_)))
        }
        Some(_) => true,
    }
}

/// Port of `userTyping` (user.go:16).
///
/// Three refusals, all `invalid_param` naming `channel_id` except the busy one — including the
/// permission refusal, which the REST route answers as a 403 and this one does not.
async fn user_typing(
    state: &AppState,
    request: &WebSocketRequest,
    session: &Session,
) -> HandlerResult {
    // `ExtendSessionExpiryIfNeeded` first — not ported ([D-214]).

    // "this is considered a non-critical service and will be disabled when server busy."
    if crate::system::server_is_busy() {
        return Err(server_busy_error(&request.action));
    }

    let channel_id = match field(request, "channel_id").and_then(|v| v.as_str()) {
        Some(channel_id) if is_valid_id(channel_id) => channel_id,
        _ => return Err(invalid_param_error(&request.action, "channel_id")),
    };

    let (has_permission, _) = state
        .app
        .session_has_permission_to_channel(session, channel_id, &PERMISSION_CREATE_POST)
        .await;
    if !has_permission {
        return Err(invalid_param_error(&request.action, "channel_id"));
    }

    // A non-string `parent_id` is not refused; it is the empty string.
    let parent_id = field(request, "parent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    state
        .app
        .publish_user_typing(&session.user_id, channel_id, parent_id)
        .await?;
    Ok(Outcome::Reply(None))
}

/// Port of `userUpdateActiveStatus` (user.go:45).
///
/// `user_is_active` must be a JSON boolean; `manual` defaults to false when it is anything else.
/// Neither setter can fail, so the only refusal is the parameter.
async fn user_update_active_status(
    state: &AppState,
    request: &WebSocketRequest,
    session: &Session,
) -> HandlerResult {
    let Some(user_is_active) = field(request, "user_is_active").and_then(|v| v.as_bool()) else {
        return Err(invalid_param_error(&request.action, "user_is_active"));
    };
    let manual = field(request, "manual")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if user_is_active {
        state.app.set_status_online(&session.user_id, manual).await;
    } else {
        state
            .app
            .set_status_away_if_needed(&session.user_id, manual)
            .await;
    }
    Ok(Outcome::Reply(None))
}

/// Port of `getStatuses` (status.go:17): every cached status that is not `offline`.
fn get_statuses(state: &AppState) -> StringInterface {
    let statuses = state.app.get_all_statuses();
    // `StatusMapToInterfaceMap` returns a Go map, which the encoder sorts; the BTreeMap puts the
    // keys in that order before they reach the (insertion-ordered) response map.
    let sorted: BTreeMap<String, String> =
        status_map_to_interface_map(&statuses).into_iter().collect();
    sorted
        .into_iter()
        .map(|(user_id, status)| (user_id, serde_json::Value::String(status)))
        .collect()
}

/// Port of `getStatusesByIds` (status.go:22).
async fn get_statuses_by_ids(state: &AppState, request: &WebSocketRequest) -> HandlerResult {
    let user_ids = array_from_interface(field(request, "user_ids"));
    if user_ids.is_empty() {
        tracing::debug!("Error while parsing user_ids");
        return Err(invalid_param_error(&request.action, "user_ids"));
    }

    let status_map = state.app.get_statuses_by_ids(&user_ids).await?;
    // Sorted for the same reason as `get_statuses`.
    let sorted: BTreeMap<String, serde_json::Value> = status_map.into_iter().collect();
    Ok(Outcome::Reply(Some(sorted.into_iter().collect())))
}

/// `hub.SendMessage(conn, resp)`.
fn send(state: &AppState, conn: &Arc<WebConn>, response: WebSocketResponse) {
    state
        .app
        .hub()
        .send_message(conn, OutgoingFrame::Response(Box::new(response)));
}

/// `err.WipeDetailed(); NewWebSocketError(r.Seq, err)`.
fn reply_error(state: &AppState, conn: &Arc<WebConn>, seq: i64, mut err: AppError) {
    err.wipe_detailed();
    send(state, conn, WebSocketResponse::new_error(seq, err));
}

/// Port of `NewInvalidWebSocketParamError` (websocket_handler.go:79).
fn invalid_param_error(action: &str, name: &str) -> Box<AppError> {
    AppError::boxed(
        format!("websocket: {action}"),
        "api.websocket_handler.invalid_param.app_error",
        Some(HashMap::from([(
            "Name".to_owned(),
            serde_json::Value::String(name.to_owned()),
        )])),
        String::new(),
        400,
    )
}

/// Port of `NewServerBusyWebSocketError` (websocket_handler.go:83).
fn server_busy_error(action: &str) -> Box<AppError> {
    AppError::boxed(
        format!("websocket: {action}"),
        "api.websocket_handler.server_busy.app_error",
        None,
        String::new(),
        503,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exactly_the_six_registered_names_resolve() {
        for (name, action) in [
            ("ping", Action::Ping),
            ("posted_notify_ack", Action::PostedNotifyAck),
            ("user_typing", Action::UserTyping),
            ("user_update_active_status", Action::UserUpdateActiveStatus),
            ("get_statuses", Action::GetStatuses),
            ("get_statuses_by_ids", Action::GetStatusesByIds),
        ] {
            assert_eq!(Action::from_name(name), Some(action), "{name}");
        }
        // The router's own two are not wsapi's, and names are exact.
        for name in [
            "presence",
            "authentication_challenge",
            "Ping",
            "ping ",
            "",
            "custom_ping",
        ] {
            assert_eq!(Action::from_name(name), None, "{name:?}");
        }
    }

    #[test]
    fn ping_answers_four_keys_with_an_empty_node_id() {
        let data = ping_data(1_700_000_000_123);
        assert_eq!(
            serde_json::Value::Object(data.clone()),
            json!({
                "node_id": "",
                "server_time": 1_700_000_000_123_i64,
                "text": "pong",
                "version": CURRENT_VERSION,
            })
        );
        let keys: Vec<&str> = data.keys().map(String::as_str).collect();
        assert_eq!(keys, ["node_id", "server_time", "text", "version"]);
    }

    #[test]
    fn the_ack_panics_only_on_a_non_string_that_go_asserts() {
        let s = json!("x");
        let n = json!(1);
        let o = json!({});
        // No status: returns before either assertion, whatever the reason is.
        assert!(!ack_type_assertion_panics(None, None));
        assert!(!ack_type_assertion_panics(None, Some(&n)));
        // A non-string status panics, reason or not.
        assert!(ack_type_assertion_panics(Some(&n), None));
        assert!(ack_type_assertion_panics(Some(&o), Some(&s)));
        // A string status: only a present, non-string reason panics.
        assert!(!ack_type_assertion_panics(Some(&s), None));
        assert!(!ack_type_assertion_panics(Some(&s), Some(&s)));
        assert!(ack_type_assertion_panics(Some(&s), Some(&n)));
    }

    #[test]
    fn a_json_null_field_is_go_nil() {
        let request: WebSocketRequest = serde_json::from_value(json!({
            "seq": 1,
            "action": "posted_notify_ack",
            "data": {"status": null, "reason": 5, "channel_id": "abc"},
        }))
        .unwrap();
        assert_eq!(field(&request, "status"), None);
        assert_eq!(field(&request, "absent"), None);
        assert_eq!(field(&request, "channel_id"), Some(&json!("abc")));
        // `status` null means the early return, so the non-string reason is never asserted.
        assert!(!ack_type_assertion_panics(
            field(&request, "status"),
            field(&request, "reason")
        ));
    }

    #[test]
    fn the_two_errors_carry_go_s_where_id_params_and_status() {
        let err = invalid_param_error("user_typing", "channel_id");
        assert_eq!(err.where_, "websocket: user_typing");
        assert_eq!(err.id, "api.websocket_handler.invalid_param.app_error");
        assert_eq!(
            err.params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(|v| v.as_str()),
            Some("channel_id")
        );
        assert_eq!(err.status_code, 400);
        assert!(err.detailed_error.is_empty());

        let err = server_busy_error("user_typing");
        assert_eq!(err.where_, "websocket: user_typing");
        assert_eq!(err.id, "api.websocket_handler.server_busy.app_error");
        assert!(err.params.is_none());
        assert_eq!(err.status_code, 503);
    }
}
