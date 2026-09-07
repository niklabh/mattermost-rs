//! Port of `api4/websocket.go` and the *read* and *write* pumps of
//! `app/platform/web_conn.go`.
//!
//! Go splits the socket across three goroutines per connection — `readPump`, `writePump` and a
//! plugin consumer. This port runs one task with a `select!` over the three sources, because the
//! plugin consumer has nothing to consume and the remaining two never contend: the read side's
//! only slow operation is an action handler, and the write side's queue absorbs a broadcast while
//! one runs. The observable difference is that a broadcast arriving *during* an action handler
//! waits for it; the frames still leave in order and with the sequence Go would have given them.
//!
//! # What a client sees
//!
//! 1. `GET /api/v4/websocket` upgrades. The session is **optional** — `APIHandlerTrustRequester`
//!    sets `RequireSession: false` (handlers.go:567) — so a connection with no token, or with a
//!    token that no longer resolves, is upgraded and simply receives nothing until it
//!    authenticates over the socket.
//! 2. An authenticated connection is registered with the hub, whose first frame is `hello`.
//! 3. Server pings every 60s; the client's pong resets a 100s read deadline.
//!
//! # The query-string token is a 401, and that is not a websocket rule
//!
//! `handlers.go:281` rejects any **non-OAuth** session presented as `?access_token=`, on every
//! route, before the handler runs. It is reproduced here rather than at the extractor because
//! this is the only route in this server that accepts an optional session, and the check sits
//! between "no token" and "valid token" — the two cases that would otherwise both upgrade.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use mm_app::hub::{OutgoingFrame, Presence, WebConn, new_connection_id};
use mm_model::session::Session;
use mm_model::utils::AppError;
use mm_model::websocket_message::{
    WEBSOCKET_AUTHENTICATION_CHALLENGE, WEBSOCKET_PRESENCE_INDICATOR, WEBSOCKET_STATUS_FAIL,
    WebSocketResponse,
};
use mm_model::websocket_request::WebSocketRequest;

use crate::AppState;
use crate::auth::{TokenLocation, parse_auth_token};
use crate::error::ApiError;

/// Port of `platform.pongWaitTime` (web_conn.go:38).
const PONG_WAIT: Duration = Duration::from_secs(100);
/// Port of `platform.pingInterval` (web_conn.go:39) — 60% of the pong wait.
const PING_INTERVAL: Duration = Duration::from_secs(60);
/// Port of `model.SocketMaxMessageSizeKb`, used by Go as the read limit and the buffer sizes.
const SOCKET_MAX_MESSAGE_SIZE: usize = 8 * 1024;

/// Go's `model.StatusOk`. Lives in `client4.go`, which this project never reads, so the value is
/// repeated rather than imported — the same choice `mm-model` made for `StatusFail`.
const STATUS_OK: &str = "OK";

/// Port of `connectWebSocket` (api4/websocket.go:56).
///
/// # Why this takes the whole request
///
/// `WebSocketUpgrade` as an argument would be an *extractor*, and an extractor that rejects
/// answers before the handler body runs — so a request with no `Upgrade` header would get axum's
/// 400 and never reach the session check. Go checks the session first (`handlers.go:268`, in
/// `ServeHTTP`, ahead of the handler), and the difference is visible: a plain `GET
/// /api/v4/websocket?access_token=…` is **401** on Go and was 400 here until the extractor was
/// moved inside. Caught by the parity suite on its first run.
#[tracing::instrument(skip_all)]
pub async fn connect_websocket(
    State(state): State<AppState>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    let (mut parts, _body) = request.into_parts();
    let session = resolve_optional_session(&state, &parts).await?;

    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) => upgrade,
        // Go's own upgrade failure is `api.web_socket.connect.upgrade.app_error` at 400
        // (websocket.go:69), carrying the blocked origin in `params`. axum's rejection is a
        // plain-text 400 with a different body: the status agrees and the body does not. Reached
        // only by a malformed upgrade — a real client either upgrades or does not speak
        // websocket — so it is stated here rather than opened as owed work.
        Err(rejection) => return Ok(rejection.into_response()),
    };

    // Go reads `connection_id` and `sequence_number` here to resume a dropped connection. Neither
    // is honoured — reconnect replay is not ported ([D-181]) — so every connection gets a fresh
    // id, which is exactly the branch Go takes when the id is absent (websocket.go:99).
    let connection_id = new_connection_id();

    Ok(upgrade
        .max_message_size(SOCKET_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| serve_socket(state, socket, connection_id, session)))
}

/// The session block of `Handler.ServeHTTP` (handlers.go:268-286) as it applies to a handler with
/// `RequireSession: false`.
///
/// Three outcomes, and the middle one is the surprising one:
///
/// - no token → no session, and the connection is upgraded unauthenticated;
/// - a token that does not resolve → **also** no session, because `RequireSession` is false and Go
///   only raises the error when the lookup fails with a 500;
/// - a resolved, non-OAuth session presented in the query string → 401, before the upgrade.
async fn resolve_optional_session(
    state: &AppState,
    parts: &Parts,
) -> Result<Option<Session>, ApiError> {
    let Some((token, location)) = parse_auth_token(parts) else {
        return Ok(None);
    };

    let session = match state.app.get_session(&token).await {
        Ok(session) => session,
        Err(err) => {
            // Go raises only the 500 case; a 401 from the lookup leaves `c.Err` unset and the
            // connection proceeds without a session.
            if err.status_code == 500 {
                return Err(ApiError::from(err));
            }
            tracing::info!("Invalid session");
            return Ok(None);
        }
    };

    if !session.is_oauth && location == TokenLocation::QueryString {
        return Err(ApiError(Box::new(AppError::new(
            "ServeHTTP",
            "api.context.token_provided.app_error",
            None,
            format!("token={token}"),
            401,
        ))));
    }

    Ok(Some(session))
}

/// Port of `(*WebConn).Pump` (web_conn.go:408): register, run both pumps, unregister.
async fn serve_socket(
    state: AppState,
    mut socket: WebSocket,
    connection_id: String,
    session: Option<Session>,
) {
    let authenticated = session.as_ref().is_some_and(|s| !s.user_id.is_empty());
    let (conn, mut queue) = WebConn::new(connection_id.clone(), session.unwrap_or_default());

    // Go registers only when the session carries a user (websocket.go:113). An unregistered
    // connection still runs both pumps: it can authenticate later over the socket.
    if authenticated {
        let hello = state.app.hello_message(&conn);
        state.app.hub().register(conn.clone(), hello);
    }

    let mut sequence: i64 = 0;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick is immediate; Go's ticker is not

    let mut read_deadline = tokio::time::Instant::now() + PONG_WAIT;

    loop {
        tokio::select! {
            frame = queue.recv() => {
                let Some(frame) = frame else { break };
                // `seq` is assigned here, on the way out, and only to events — Go's writePump
                // (web_conn.go:583) increments the counter in the event arm alone, so a response
                // interleaved between two events does not consume a number.
                //
                // The two encodings are not interchangeable — see `OutgoingFrame` — and the
                // newline is Go's `json.Encoder.Encode`, which every non-precomputed frame goes
                // through.
                let text = match frame {
                    OutgoingFrame::Event { event, precomputed } => {
                        let event = event.set_sequence(sequence);
                        sequence += 1;
                        let encoded = if precomputed {
                            event.to_json_precomputed()
                        } else {
                            event.to_json().map(|json| json + "\n")
                        };
                        match encoded {
                            Ok(text) => text,
                            Err(err) => {
                                tracing::warn!(error = %err, "Error in encoding websocket message");
                                continue;
                            }
                        }
                    }
                    OutgoingFrame::Response(response) => {
                        match mm_model::utils::go_json_marshal(&response) {
                            Ok(text) => text + "\n",
                            Err(err) => {
                                tracing::warn!(error = %err, "Error in encoding websocket message");
                                continue;
                            }
                        }
                    }
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }

            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        read_deadline = tokio::time::Instant::now() + PONG_WAIT;
                        dispatch_text(&state, &conn, text.as_str()).await;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Go decodes msgpack here, and rejects binary frames outright from an
                        // unauthenticated connection (MM-68222, web_conn.go:471). msgpack is not
                        // ported, so every binary frame is refused — a narrowing, recorded as
                        // [D-187], not a silent drop.
                        tracing::debug!("websocket: binary frame refused; msgpack is not ported");
                        break;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        read_deadline = tokio::time::Instant::now() + PONG_WAIT;
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // axum answers pings itself; the deadline still moves, as Go's does.
                        read_deadline = tokio::time::Instant::now() + PONG_WAIT;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Err(err)) => {
                        tracing::debug!(error = %err, "websocket.NextReader");
                        break;
                    }
                }
            }

            _ = ping.tick() => {
                if socket.send(Message::Ping(Vec::new().into())).await.is_err() {
                    break;
                }
            }

            _ = tokio::time::sleep_until(read_deadline) => {
                // Go's `SetReadDeadline`: a connection that has not spoken in 100s is gone.
                tracing::debug!(conn_id = %connection_id, "websocket: read deadline exceeded");
                break;
            }
        }
    }

    state.app.hub().unregister(&connection_id);
}

/// Port of `(*WebSocketRouter).ServeWebSocket` (websocket_router.go:26).
///
/// The two actions the router itself owns — `authentication_challenge` and `presence` — are
/// handled before the authentication test, exactly as Go orders them: the first has to work on an
/// unauthenticated connection, and the second is a presence hint that Go accepts from anyone.
///
/// The `wsapi` action table (`ping`, `user_typing`, `get_statuses`, `get_statuses_by_ids`,
/// `user_update_active_status`, `posted_notify_ack`) is **not** served here; every one of them
/// answers Go's own unknown-action error. See [D-188].
async fn dispatch_text(state: &AppState, conn: &Arc<WebConn>, text: &str) {
    let request: WebSocketRequest = match serde_json::from_str(text) {
        Ok(request) => request,
        Err(err) => {
            // Go's `readPump` returns — closing the socket — on a decode failure. Reproduced by
            // the caller breaking out of the loop is not possible from here, so the frame is
            // dropped and logged; a malformed frame is not something a real client sends.
            tracing::debug!(error = %err, "websocket.Decode");
            return;
        }
    };

    if request.action.is_empty() {
        return_error(state, conn, request.seq, no_action_error());
        return;
    }
    if request.seq <= 0 {
        return_error(state, conn, request.seq, bad_seq_error());
        return;
    }

    match request.action.as_str() {
        WEBSOCKET_AUTHENTICATION_CHALLENGE => {
            authentication_challenge(state, conn, &request).await;
        }
        WEBSOCKET_PRESENCE_INDICATOR => {
            presence(state, conn, &request);
        }
        _ => {
            if conn.session().user_id.is_empty() {
                return_error(state, conn, request.seq, not_authenticated_error());
                return;
            }
            return_error(state, conn, request.seq, bad_action_error());
        }
    }
}

/// The `authentication_challenge` arm (websocket_router.go:39).
///
/// Go closes the socket outright when the token is absent or does not resolve — not an error
/// frame, a disconnect. That is reproduced by dropping the connection from the hub, which ends
/// the pump.
async fn authentication_challenge(
    state: &AppState,
    conn: &Arc<WebConn>,
    request: &WebSocketRequest,
) {
    if !conn.session().token.is_empty() {
        return;
    }
    let Some(token) = request
        .data
        .as_ref()
        .and_then(|data| data.get("token"))
        .and_then(|token| token.as_str())
    else {
        state.app.hub().unregister(&conn.connection_id);
        return;
    };

    let session = match state.app.get_session(token).await {
        Ok(session) => session,
        Err(err) => {
            tracing::warn!(error = %err, "Error while getting session token");
            state.app.hub().unregister(&conn.connection_id);
            return;
        }
    };

    conn.set_session(session);
    let hello = state.app.hello_message(conn);
    state.app.hub().register(conn.clone(), hello);
    send_response(
        state,
        conn,
        WebSocketResponse::new(STATUS_OK, request.seq, None),
    );
}

/// The `presence` arm (websocket_router.go:82).
///
/// Four fields, and `thread_channel_id` lands in one of two slots depending on `is_thread_view`.
/// Which slot matters: [`mm_app::hub`]'s `not_in_thread` requires **both** to be set before it can
/// exclude a connection, so writing the wrong one silently changes who receives a reaction.
fn presence(state: &AppState, conn: &Arc<WebConn>, request: &WebSocketRequest) {
    let data = request.data.as_ref();
    let string_field = |name: &str| {
        data.and_then(|data| data.get(name))
            .and_then(|value| value.as_str())
    };

    if let Some(channel_id) = string_field("channel_id") {
        conn.set_presence(Presence::Channel, channel_id);
    }
    // `team_id` is stored by Go and read by nothing in `ShouldSendEvent`; it exists for the
    // presence indicator. Accepted and discarded rather than stored, since storing it would imply
    // a reader.
    if let Some(thread_channel_id) = string_field("thread_channel_id") {
        let is_thread_view = data
            .and_then(|data| data.get("is_thread_view"))
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        if is_thread_view {
            conn.set_presence(Presence::ThreadViewThreadChannel, thread_channel_id);
        } else {
            conn.set_presence(Presence::RhsThreadChannel, thread_channel_id);
        }
    }

    send_response(
        state,
        conn,
        WebSocketResponse::new(STATUS_OK, request.seq, None),
    );
}

fn send_response(state: &AppState, conn: &Arc<WebConn>, response: WebSocketResponse) {
    state.app.hub().send_to_connection(
        &conn.connection_id,
        OutgoingFrame::Response(Box::new(response)),
    );
}

/// Port of `returnWebSocketError` (websocket_router.go:124), including `WipeDetailed`.
fn return_error(state: &AppState, conn: &Arc<WebConn>, seq: i64, mut err: AppError) {
    err.wipe_detailed();
    let response = WebSocketResponse {
        status: WEBSOCKET_STATUS_FAIL.to_owned(),
        seq_reply: seq,
        data: None,
        error: Some(Box::new(err)),
    };
    send_response(state, conn, response);
}

fn no_action_error() -> AppError {
    AppError::new(
        "ServeWebSocket",
        "api.web_socket_router.no_action.app_error",
        None,
        String::new(),
        400,
    )
}

fn bad_seq_error() -> AppError {
    AppError::new(
        "ServeWebSocket",
        "api.web_socket_router.bad_seq.app_error",
        None,
        String::new(),
        400,
    )
}

fn not_authenticated_error() -> AppError {
    AppError::new(
        "ServeWebSocket",
        "api.web_socket_router.not_authenticated.app_error",
        None,
        String::new(),
        401,
    )
}

fn bad_action_error() -> AppError {
    AppError::new(
        "ServeWebSocket",
        "api.web_socket_router.bad_action.app_error",
        None,
        String::new(),
        500,
    )
}
