//! Port of `api4/websocket.go`, the *read* and *write* pumps of `app/platform/web_conn.go`, and
//! the router in `app/platform/websocket_router.go`.
//!
//! Go splits the socket across three goroutines per connection — `readPump`, `writePump` and a
//! plugin consumer. This port runs one task with a `select!` over the sources, because the
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
//! 2. An authenticated connection is registered with the hub, whose first frame is `hello`, and
//!    the user is marked online (`NewWebConn`, web_conn.go:202).
//! 3. A connection that has no token **five seconds** after the upgrade is closed
//!    (`authTicker`, web_conn.go:625). Authenticating over the socket inside that window keeps it.
//! 4. Server pings every 60s; the client's pong resets a 100s read deadline and, on an
//!    authenticated connection, lets the user go `away` if they have been idle.
//!
//! # A connection that is not registered hears nothing back
//!
//! Every response goes through `hub.SendMessage`, which drops a message for a connection the hub
//! does not hold (web_hub.go:701). So an anonymous connection's `ping` is not answered with
//! `not_authenticated` — it is not answered at all, on either server.
//!
//! # The query-string token is a 401, and that is not a websocket rule
//!
//! `handlers.go:281` rejects any **non-OAuth** session presented as `?access_token=`, on every
//! route, before the handler runs. It is reproduced here rather than at the extractor because
//! this is the only route in this server that accepts an optional session, and the check sits
//! between "no token" and "valid token" — the two cases that would otherwise both upgrade.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::response::{IntoResponse, Response};
use mm_app::hub::{
    DeadQueue, DeadQueueEntry, OutgoingFrame, ParkedQueues, Presence, WebConn, new_connection_id,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, is_valid_id};
use mm_model::websocket_message::{
    WEBSOCKET_AUTHENTICATION_CHALLENGE, WEBSOCKET_PRESENCE_INDICATOR, WEBSOCKET_STATUS_FAIL,
    WebSocketEvent, WebSocketResponse,
};
use mm_model::websocket_request::WebSocketRequest;

use crate::AppState;
use crate::auth::{TokenLocation, parse_auth_token};
use crate::error::ApiError;
use crate::wsapi;

/// Port of `platform.pongWaitTime` (web_conn.go:38).
const PONG_WAIT: Duration = Duration::from_secs(100);
/// Port of `platform.pingInterval` (web_conn.go:39) — 60% of the pong wait.
const PING_INTERVAL: Duration = Duration::from_secs(60);
/// Port of `platform.authCheckInterval` (web_conn.go:40).
const AUTH_CHECK_INTERVAL: Duration = Duration::from_secs(5);
/// Port of `model.SocketMaxMessageSizeKb`, used by Go as the read limit and the buffer sizes.
const SOCKET_MAX_MESSAGE_SIZE: usize = 8 * 1024;

/// Port of `platform.websocketMessagePluginPrefix` (web_conn.go:52). An action with this prefix
/// is for plugins only and never reaches the router — so it is neither answered nor refused.
const WEBSOCKET_MESSAGE_PLUGIN_PREFIX: &str = "custom_";

/// Go's `model.StatusOk`. Lives in `client4.go`, which this project never reads, so the value is
/// repeated rather than imported — the same choice `mm-model` made for `StatusFail`.
pub(crate) const STATUS_OK: &str = "OK";

/// `postedAckParam` (api4/websocket.go:21).
const POSTED_ACK_PARAM: &str = "posted_ack";
/// `connectionIDParam` (api4/websocket.go:19).
const CONNECTION_ID_PARAM: &str = "connection_id";
/// `sequenceNumberParam` (api4/websocket.go:20).
const SEQUENCE_NUMBER_PARAM: &str = "sequence_number";

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

    let query = parts.uri.query();
    // `PostedAck: r.URL.Query().Get(postedAckParam) == "true"` (websocket.go:81). The only
    // reader is the `posted_ack` broadcast hook.
    let posted_ack = query_flag_is_true(query, POSTED_ACK_PARAM);
    // Read here, acted on after the upgrade — see `serve_socket`. `disconnect_err_code` is read by
    // Go only to label a reconnect metric, which this server does not keep.
    let requested_id = query_value(query, CONNECTION_ID_PARAM);
    let sequence_value = query_value(query, SEQUENCE_NUMBER_PARAM);

    Ok(upgrade
        .max_message_size(SOCKET_MAX_MESSAGE_SIZE)
        .on_upgrade(move |socket| {
            serve_socket(
                state,
                socket,
                session,
                posted_ack,
                requested_id,
                sequence_value,
            )
        }))
}

/// Go's `r.URL.Query().Get(key)`: the **first** value under `key`, percent-decoded, and the empty
/// string when the key is absent.
fn query_value(query: Option<&str>, key: &str) -> String {
    query
        .and_then(|query| {
            form_urlencoded::parse(query.as_bytes())
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        })
        .unwrap_or_default()
}

/// Go's `r.URL.Query().Get(key) == "true"`, compared exactly. `1`, `True` and `t` are false here —
/// this is not the `strconv.ParseBool` rule the REST query parsers use.
fn query_flag_is_true(query: Option<&str>, key: &str) -> bool {
    query_value(query, key) == "true"
}

/// `PopulateWebConnConfig`'s refusals (web_conn.go:165-179), in Go's order: the id must be a
/// Mattermost id, and a sequence number must come with it and parse as `strconv.ParseInt(v, 10,
/// 0)` — which, like `i64::from_str`, takes a leading `+` or `-` and nothing else.
fn parse_resumption(connection_id: &str, sequence: &str) -> Result<i64, &'static str> {
    if !is_valid_id(connection_id) {
        return Err("invalid connection id");
    }
    if sequence.is_empty() {
        return Err("sequence number not present in websocket request");
    }
    sequence
        .parse::<i64>()
        .map_err(|_| "invalid sequence number in query param")
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

/// `ps.Go(func() { SetStatusOnline(userID, false); UpdateLastActivityAtIfNeeded(session) })` —
/// written twice in Go, in `NewWebConn` (web_conn.go:203) and after a successful
/// `authentication_challenge` (websocket_router.go:66), and off the socket's own path both times.
fn spawn_mark_online(state: &AppState, session: Session) {
    // A clone of the app handle: the task outlives this borrow, and `App` is a set of shared
    // handles made to be cloned per task.
    let app = state.app.clone();
    tokio::spawn(async move {
        app.set_status_online(&session.user_id, false).await;
        app.update_last_activity_at_if_needed(&session).await;
    });
}

/// Port of `(*WebConn).Pump` (web_conn.go:408), with `PopulateWebConnConfig` (web_conn.go:164)
/// and the resumption prelude of `writePump` folded in: find or mint the connection, register,
/// replay what a resumed client missed, run both pumps, then park the queues in the hub.
///
/// # Resumption is decided after the upgrade
///
/// Go runs `PopulateWebConnConfig` on an already-upgraded socket, so a malformed `connection_id`
/// or `sequence_number` is a websocket that opens and is closed at once, never an HTTP error, and
/// the old connection is looked up only once the new socket exists. Both follow from doing it
/// here rather than in [`connect_websocket`].
async fn serve_socket(
    state: AppState,
    mut socket: WebSocket,
    session: Option<Session>,
    posted_ack: bool,
    requested_id: String,
    sequence_value: String,
) {
    let session = session.unwrap_or_default();
    let user_id = session.user_id.clone();

    // `if cfg.ConnectionID == "" || Session().UserId == ""` (websocket.go:97): only an
    // authenticated client naming a connection is considered for resumption, and a malformed
    // request from an anonymous one is not even parsed.
    let resumed = if requested_id.is_empty() || user_id.is_empty() {
        None
    } else {
        let sequence = match parse_resumption(&requested_id, &sequence_value) {
            Ok(sequence) => sequence,
            Err(reason) => {
                tracing::error!(id = %requested_id, reason, "Error while populating webconn config");
                return;
            }
        };
        state
            .app
            .hub()
            .check_conn(&user_id, &requested_id)
            .map(|found| (found, sequence))
    };

    let (conn, mut queues, mut sequence) = match resumed {
        Some((found, sequence)) => {
            let (conn, queues) = WebConn::resume(requested_id, session, posted_ack, found);
            (conn, queues, sequence)
        }
        // "If the connection is not present, then we assume either timeout, or server restart" —
        // a new id and a sequence of zero, whatever the client sent.
        None => {
            let (conn, active) = WebConn::new(new_connection_id(), session, posted_ack);
            let queues = ParkedQueues {
                active,
                dead: DeadQueue::new(),
            };
            (conn, queues, 0)
        }
    };

    // Go registers only when the session carries a user (websocket.go:113). An unregistered
    // connection still runs both pumps: it can authenticate later over the socket.
    if !user_id.is_empty() {
        spawn_mark_online(&state, conn.session());
        let hello = state.app.hello_message(&conn);
        state.app.hub().register(conn.clone(), hello);
    }

    if sequence != 0
        && resume_prelude(&state, &conn, &mut socket, &mut queues.dead, &mut sequence)
            .await
            .is_break()
    {
        state.app.hub_unregister(&conn, queues).await;
        return;
    }

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await; // the first tick is immediate; Go's ticker is not

    let mut read_deadline = tokio::time::Instant::now() + PONG_WAIT;

    // Go's `authTicker` fires every five seconds until it first finds a token, then stops; it
    // closes the connection on the first tick that finds none. So only the first tick can ever
    // decide anything, and a single deadline is the whole of it.
    let auth_deadline = tokio::time::Instant::now() + AUTH_CHECK_INTERVAL;
    let mut auth_pending = true;

    loop {
        tokio::select! {
            frame = queues.active.recv() => {
                let Some(frame) = frame else { break };
                // `seq` is assigned here, on the way out, and only to events — Go's writePump
                // (web_conn.go:583) increments the counter in the event arm alone, so a response
                // interleaved between two events does not consume a number. The counter moves
                // even when the encoding fails, as Go's does.
                //
                // The two encodings are not interchangeable — see `OutgoingFrame` — and the
                // newline is Go's `json.Encoder.Encode`, which every non-precomputed frame goes
                // through.
                let text = match frame {
                    OutgoingFrame::Event { event, precomputed } => {
                        let event = event.set_sequence(sequence);
                        sequence += 1;
                        match encode_event(&event, precomputed) {
                            Ok(text) => {
                                // Events only, and before the write (web_conn.go:625).
                                queues.dead.add(DeadQueueEntry { event, precomputed });
                                text
                            }
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
                        // No deadline reset: Go sets the read deadline once and moves it only in
                        // the pong handler (web_conn.go:443-449), so a chatty client that never
                        // pongs is still dropped at 100s.
                        if dispatch_text(&state, &conn, text.as_str()).await.is_break() {
                            break;
                        }
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
                        // The pong handler (web_conn.go:449).
                        if state.app.conn_is_authenticated(&conn).await {
                            let app = state.app.clone(); // outlives the borrow, as above
                            let user_id = conn.user_id();
                            tokio::spawn(async move {
                                app.set_status_away_if_needed(&user_id, false).await;
                            });
                        }
                    }
                    Some(Ok(Message::Ping(_))) => {
                        // axum answers pings itself. Go's read deadline moves only on a pong or
                        // a data frame; a client ping reaching gorilla's default handler does
                        // not reset it, and neither does it here.
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
                tracing::debug!(conn_id = %conn.connection_id(), "websocket: read deadline exceeded");
                break;
            }

            _ = tokio::time::sleep_until(auth_deadline), if auth_pending => {
                if conn.session().token.is_empty() {
                    tracing::debug!(conn_id = %conn.connection_id(), "websocket.authTicker: did not authenticate");
                    break;
                }
                auth_pending = false;
            }

            _ = conn.closed() => {
                // The hub closed and removed this connection — a full queue. Go's write pump
                // finds its channel closed and sends a close frame.
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
        }
    }

    state.app.hub_unregister(&conn, queues).await;
}

/// The resumption prelude of `writePump` (web_conn.go:524-557), run before the pump loop for a
/// client that came back with a non-zero sequence number. Three outcomes:
///
/// - **Found** in the dead queue: every frame from that sequence to the newest is written again,
///   in the encoding it first left in, and the counter moves past them.
/// - **Loss** — not found, and the newest entry is not the one just before it: the dead queue is
///   cleared, the connection gets a **new id**, the counter restarts at zero, and a `hello` under
///   the new id is written (and queued as dead) — the client's signal to refetch.
/// - **Lossless** — the newest entry is exactly the one before: nothing is written at all.
///
/// A found connection that has never written anything takes the lossless arm, whatever the
/// sequence. `Break` closes the connection.
async fn resume_prelude(
    state: &AppState,
    conn: &Arc<WebConn>,
    socket: &mut WebSocket,
    dead: &mut DeadQueue,
    sequence: &mut i64,
) -> ControlFlow<()> {
    if let Some(index) = dead.position_of(*sequence) {
        for entry in dead.drain_from(index) {
            if write_event(socket, &entry.event, entry.precomputed, sequence)
                .await
                .is_break()
            {
                return ControlFlow::Break(());
            }
        }
        return ControlFlow::Continue(());
    }

    if dead.has_msg_loss(*sequence) {
        dead.clear();
        conn.set_connection_id(new_connection_id());
        *sequence = 0;
        let hello = state.app.hello_message(conn);
        // The queue keeps its copy; the socket gets the frame.
        dead.add(DeadQueueEntry {
            event: hello.clone(),
            precomputed: false,
        });
        return write_event(socket, &hello, false, sequence).await;
    }

    ControlFlow::Continue(())
}

/// Port of `(*WebConn).writeMessage` (web_conn.go:650): an event written outside the queue, with
/// whatever sequence it already carries. An encoding failure is logged and skipped **without**
/// moving the counter; a write failure closes the connection.
async fn write_event(
    socket: &mut WebSocket,
    event: &WebSocketEvent,
    precomputed: bool,
    sequence: &mut i64,
) -> ControlFlow<()> {
    let text = match encode_event(event, precomputed) {
        Ok(text) => text,
        Err(err) => {
            tracing::warn!(error = %err, "Error in encoding websocket message");
            return ControlFlow::Continue(());
        }
    };
    *sequence += 1;
    if socket.send(Message::Text(text.into())).await.is_err() {
        return ControlFlow::Break(());
    }
    ControlFlow::Continue(())
}

/// `(*WebSocketEvent).Encode` (websocket_message.go:384): the precomputed buffer as is, or
/// `json.Encoder` — compact, newline-terminated.
fn encode_event(event: &WebSocketEvent, precomputed: bool) -> Result<String, serde_json::Error> {
    if precomputed {
        event.to_json_precomputed()
    } else {
        event.to_json().map(|json| json + "\n")
    }
}

/// The body of `readPump`'s loop (web_conn.go:463) for one text frame: decode, then route unless
/// the action belongs to plugins. `Break` closes the connection.
async fn dispatch_text(state: &AppState, conn: &Arc<WebConn>, text: &str) -> ControlFlow<()> {
    // `json.NewDecoder(rd).Decode(&req)` reads **one** JSON value and never looks past it, so
    // `{"seq":1,"action":"ping"} trailing` is a ping. `from_str` would refuse the trailing bytes;
    // the stream deserializer stops where the decoder does.
    let request = match serde_json::Deserializer::from_str(text)
        .into_iter::<WebSocketRequest>()
        .next()
    {
        Some(Ok(request)) => request,
        Some(Err(err)) => {
            // `readPump` returns on a decode failure, which closes the socket.
            tracing::debug!(error = %err, "websocket.Decode");
            return ControlFlow::Break(());
        }
        None => {
            // An empty or all-whitespace frame is `io.EOF` from the decoder — the same return.
            tracing::debug!("websocket.Decode: empty frame");
            return ControlFlow::Break(());
        }
    };

    // "Messages which actions are prefixed with the plugin prefix should only be dispatched to
    // the plugins" — and there is no plugin host.
    if request.action.starts_with(WEBSOCKET_MESSAGE_PLUGIN_PREFIX) {
        return ControlFlow::Continue(());
    }

    serve_web_socket(state, conn, &request).await
}

/// Port of `(*WebSocketRouter).ServeWebSocket` (websocket_router.go:26).
///
/// The two actions the router itself owns — `authentication_challenge` and `presence` — are
/// handled before the authentication test, exactly as Go orders them: the first has to work on an
/// unauthenticated connection, and the second is a presence hint that Go accepts from anyone.
/// Everything else is refused when the connection is not authenticated, then looked up in the
/// [`wsapi`] table, and an unknown name is `bad_action` at 500.
async fn serve_web_socket(
    state: &AppState,
    conn: &Arc<WebConn>,
    request: &WebSocketRequest,
) -> ControlFlow<()> {
    if request.action.is_empty() {
        return_error(state, conn, request.seq, no_action_error());
        return ControlFlow::Continue(());
    }
    if request.seq <= 0 {
        return_error(state, conn, request.seq, bad_seq_error());
        return ControlFlow::Continue(());
    }

    match request.action.as_str() {
        WEBSOCKET_AUTHENTICATION_CHALLENGE => authentication_challenge(state, conn, request).await,
        WEBSOCKET_PRESENCE_INDICATOR => {
            presence(state, conn, request);
            ControlFlow::Continue(())
        }
        _ => {
            if !state.app.conn_is_authenticated(conn).await {
                return_error(state, conn, request.seq, not_authenticated_error());
                return ControlFlow::Continue(());
            }
            let Some(action) = wsapi::Action::from_name(&request.action) else {
                return_error(state, conn, request.seq, bad_action_error());
                return ControlFlow::Continue(());
            };
            wsapi::serve_web_socket(state, conn, action, request).await
        }
    }
}

/// The `authentication_challenge` arm (websocket_router.go:39).
///
/// Go closes the socket outright when the token is absent or does not resolve — not an error
/// frame, a disconnect — so both are `Break`. A connection that already has a token ignores the
/// challenge entirely, without an answer.
async fn authentication_challenge(
    state: &AppState,
    conn: &Arc<WebConn>,
    request: &WebSocketRequest,
) -> ControlFlow<()> {
    if !conn.session().token.is_empty() {
        return ControlFlow::Continue(());
    }
    let Some(token) = request
        .data
        .as_ref()
        .and_then(|data| data.get("token"))
        .and_then(|token| token.as_str())
    else {
        return ControlFlow::Break(());
    };

    let session = match state.app.get_session(token).await {
        Ok(session) => session,
        Err(err) => {
            tracing::warn!(error = %err, "Error while getting session token");
            return ControlFlow::Break(());
        }
    };

    // `SetSession`, `SetSessionToken`, `conn.UserId = session.UserId`, then `HubRegister` — the
    // user id first, because the hub indexes the connection by it.
    let user_id = session.user_id.clone();
    conn.set_session(session);
    conn.set_user_id(user_id);
    let hello = state.app.hello_message(conn);
    state.app.hub().register(conn.clone(), hello);
    spawn_mark_online(state, conn.session());

    send_response(
        state,
        conn,
        WebSocketResponse::new(STATUS_OK, request.seq, None),
    );
    ControlFlow::Continue(())
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
    state
        .app
        .hub()
        .send_message(conn, OutgoingFrame::Response(Box::new(response)));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn posted_ack_is_the_exact_string_true_and_the_first_value_wins() {
        assert!(query_flag_is_true(Some("posted_ack=true"), "posted_ack"));
        assert!(query_flag_is_true(
            Some("connection_id=abc&posted_ack=true&sequence_number=0"),
            "posted_ack"
        ));
        // Percent-decoded before the comparison, as `url.Values` is.
        assert!(query_flag_is_true(Some("posted_ack=%74rue"), "posted_ack"));
        // Go's `Get` returns the first value.
        assert!(!query_flag_is_true(
            Some("posted_ack=false&posted_ack=true"),
            "posted_ack"
        ));
        assert!(query_flag_is_true(
            Some("posted_ack=true&posted_ack=false"),
            "posted_ack"
        ));
        // Not ParseBool.
        for not_true in ["1", "True", "TRUE", "t", "", "yes"] {
            assert!(
                !query_flag_is_true(Some(&format!("posted_ack={not_true}")), "posted_ack"),
                "{not_true:?} must not read as true"
            );
        }
        assert!(!query_flag_is_true(None, "posted_ack"));
        assert!(!query_flag_is_true(Some("posted_ack"), "posted_ack"));
        assert!(!query_flag_is_true(Some("other=true"), "posted_ack"));
    }

    #[test]
    fn query_value_is_the_first_decoded_value_or_empty() {
        assert_eq!(
            query_value(Some("connection_id=a%62c&connection_id=x"), "connection_id"),
            "abc"
        );
        assert_eq!(query_value(Some("sequence_number="), "sequence_number"), "");
        assert_eq!(query_value(Some("other=1"), "sequence_number"), "");
        assert_eq!(query_value(None, "sequence_number"), "");
    }

    #[test]
    fn resumption_needs_a_valid_id_then_a_sequence_that_parse_int_accepts() {
        const ID: &str = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(parse_resumption(ID, "7"), Ok(7));
        assert_eq!(
            parse_resumption(ID, "+7"),
            Ok(7),
            "ParseInt takes a plus sign"
        );
        assert_eq!(parse_resumption(ID, "-3"), Ok(-3), "and a minus");
        assert_eq!(parse_resumption(ID, "007"), Ok(7));
        assert_eq!(parse_resumption(ID, "0"), Ok(0));

        // The id is checked first: a bad id with no sequence is the id refusal.
        assert_eq!(parse_resumption("abc", ""), Err("invalid connection id"));
        assert_eq!(
            parse_resumption(ID, ""),
            Err("sequence number not present in websocket request")
        );
        for bad in ["x", "1.5", "1_000", " 1", "+", "9223372036854775808"] {
            assert_eq!(
                parse_resumption(ID, bad),
                Err("invalid sequence number in query param"),
                "{bad:?}"
            );
        }
    }
}
