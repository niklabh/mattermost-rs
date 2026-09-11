//! Port of `channels/api4/view.go` — the whole file, all seven routes.
//!
//! ```text
//! GET    /api/v4/channels/{channel_id}/views                      getViewsForChannel
//! POST   /api/v4/channels/{channel_id}/views                      createView
//! GET    /api/v4/channels/{channel_id}/views/{view_id}            getView
//! PATCH  /api/v4/channels/{channel_id}/views/{view_id}            updateView
//! DELETE /api/v4/channels/{channel_id}/views/{view_id}            deleteView
//! GET    /api/v4/channels/{channel_id}/views/{view_id}/posts      getPostsForView
//! POST   /api/v4/channels/{channel_id}/views/{view_id}/sort_order updateViewSortOrder
//! ```
//!
//! # These routes do not exist on the deployment this server fronts, and that is the first fact
//!
//! `InitView` (view.go:14) is one `if`:
//!
//! ```go
//! if api.srv.Config().FeatureFlags.IntegratedBoards { … seven Handle calls … }
//! ```
//!
//! `IntegratedBoards` is **false** at the pinned SHA (`feature_flags.go:194`, in `SetDefaults`)
//! and nothing in this deployment sets it. So gorilla/mux has never heard of
//! `/channels/{id}/views`, and the answer is not a 501 or a 403 but the mux's own **404
//! `api.context.404.app_error`** — the body whose `detailed_error` interpolates the request URL.
//! Measured against the stack's Go server, not inferred from the source.
//!
//! That shapes the port: [`integrated_boards_enabled`] is consulted as the **first statement of
//! every handler**, and when it is off the request is *forwarded* rather than refused locally.
//! Forwarding is what makes the two servers byte-identical for free — Go writes its own 404, URL
//! interpolation, `request_id` and all, and there is nothing here to keep in step. It is the same
//! reasoning [`crate::mux_segments_or_forward`] gives for a segment outside Go's charset.
//!
//! # Why the flag is read from the environment and not from `Config`
//!
//! `config.Store.Load` **clears `FeatureFlags` before persisting** when `readOnlyFF` is set, which
//! is the default (`config/store.go:306-310`). The live configuration row has no `FeatureFlags`
//! key at all, so the document [`mm_app::config::Config`] reads cannot see this flag — the same
//! wall [D-153] hit for `DiscoverableChannels`. The environment is the only place either server
//! can read it from, which is exactly how `MM_FEATUREFLAGS_ENABLESHIFTESCAPETOMARKALLREAD` is
//! already handled for `readAllMessages`.
//!
//! It is read here rather than added to `Config` because this module is the only consumer. A
//! second consumer should move it; `api4/post.go` has four sites and `api4/properties.go` one, so
//! that day will come.
//!
//! # Verifying the served shape at all required standing up a second Go server
//!
//! With the flag off there is no oracle: every route is a 404 and a comparison proves only that
//! two servers agree about a route neither serves. `scripts/go-boards.sh` starts a second pinned
//! Go process on `MMRS_GO_PORT + 30` with `MM_FEATUREFLAGS_INTEGRATEDBOARDS=true`, sharing the
//! database, the configuration document and the `Sessions` table. Turning the flag on in
//! `scripts/go-server.sh` instead would have been wrong: the same flag changes `api4/post.go` at
//! four sites and registers `api4/properties.go`, all of which the parity suite already asserts
//! against. Every "measured" in this file means measured against that second process.
//!
//! # Four details the wire depends on
//!
//! - **`createView` is a 201**, and the only route in the file that is not a 200.
//! - **Success bodies carry a trailing newline** (`json.NewEncoder(w).Encode`), and `deleteView`
//!   does **not** — it answers `web.ReturnStatusOK`, a bare `w.Write`. [D-086].
//! - **An empty channel lists as `[]`, never `null`.** `GetViewsForChannel` normalises the store's
//!   nil slice before it reaches the encoder (app/view.go:69).
//! - **`updateViewSortOrder`'s request body is a bare JSON integer**, not an object. `2`, not
//!   `{"sort_order": 2}` — which is refused with a 400.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::channel::Channel;
use mm_model::permission::{
    PERMISSION_CREATE_POST, PERMISSION_READ_CHANNEL_CONTENT, make_permission_error,
};
use mm_model::utils::{AppError, decode_one_from_json, is_valid_id};
use mm_model::view::{View, ViewPatch, ViewQueryOpts, ViewsWithCount};
use mm_store::post_store::GetPostsOptions;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_flag_is_true};
use crate::error::ApiError;
use crate::proxy;

/// `model.ConnectionId` (model/websocket_client.go) — the header the three write routes read so a
/// client can be left out of the broadcast for its own change.
const CONNECTION_ID_HEADER: &str = "Connection-Id";

/// `FeatureFlags.IntegratedBoards` (feature_flags.go:100), read from the environment for the
/// reason the module docs give.
///
/// `OnceLock` rather than a plain read per request: the value cannot change without a restart
/// (Go reads it from a configuration document that is rebuilt at boot), and a handler that shelled
/// out to `std::env::var` on every call would make the flag look mutable when it is not.
static INTEGRATED_BOARDS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// `MM_FEATUREFLAGS_INTEGRATEDBOARDS`, parsed the way Go's `strconv.ParseBool` does.
///
/// Unset **or unparseable** is `false`, which is both `SetDefaults`' value and viper's fallback
/// direction — `=yes` is not true on either server. The twelve accepted spellings are a copy of
/// `mm_app::config`'s private `parse_bool`; see that function for why widening them would let the
/// two configurations drift.
pub fn integrated_boards_enabled() -> bool {
    *INTEGRATED_BOARDS.get_or_init(|| {
        matches!(
            std::env::var("MM_FEATUREFLAGS_INTEGRATEDBOARDS").as_deref(),
            Ok("1" | "t" | "T" | "TRUE" | "true" | "True")
        )
    })
}

/// What a handler decided to do, before it has been turned into bytes.
enum Outcome {
    Served(Response),
    Failed(ApiError),
    /// Hand the whole request to Go. Either the feature is off here — in which case Go answers its
    /// own mux 404 — or a post-list stage is not reproducible.
    Forward,
}

impl Outcome {
    async fn finish(self, state: AppState, request: Request) -> Response {
        match self {
            Outcome::Served(response) => response,
            Outcome::Failed(err) => err.into_response(),
            Outcome::Forward => proxy::forward_to_go(State(state), request).await,
        }
    }
}

/// The byte `json.NewEncoder(w).Encode` appends after every document, and `w.Write` does not.
///
/// Named rather than inlined because it is a decision, not a detail: six of this file's seven
/// routes have it and [`status_ok`] must not.
const ENCODER_NEWLINE: u8 = b'\n';

/// `json.NewEncoder(w).Encode(value)` — a **trailing newline**, which a plain `to_vec` omits.
fn encoded(handler: &'static str, status: StatusCode, value: &impl serde::Serialize) -> Outcome {
    match serde_json::to_vec(value) {
        Ok(mut body) => {
            body.push(ENCODER_NEWLINE);
            Outcome::Served(
                (
                    status,
                    [
                        ("Content-Type", "application/json"),
                        ("x-mmrs-served-by", "rust"),
                    ],
                    body,
                )
                    .into_response(),
            )
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the view response");
            Outcome::Failed(ApiError::from(AppError::new(
                handler,
                "api.marshal_error",
                None,
                String::new(),
                500,
            )))
        }
    }
}

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing newline**.
fn status_ok() -> Outcome {
    Outcome::Served(
        (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            r#"{"status":"OK"}"#,
        )
            .into_response(),
    )
}

/// The channel lookup every one of the seven routes opens with, plus the deleted-channel refusal
/// that follows it.
///
/// # The id differs per route and the message does not
///
/// Seven handlers, seven ids — `api.view.create.deleted_channel.app_error`,
/// `…list…`, `…get…`, `…update…`, `…delete…`, `…update_sort_order…`, `…get_posts…` — all at
/// **404** with the same `"channel has been deleted"` detail. The detail is wiped before it
/// reaches a client, so `id` is the whole difference and passing the wrong one is invisible in a
/// status-code test.
///
/// Note this is a **404**, not the 400 a deleted channel usually earns: the view surface treats an
/// archived channel as absent.
async fn channel_or_deleted(
    state: &AppState,
    channel_id: &str,
    handler: &'static str,
    deleted_id: &'static str,
) -> Result<Channel, ApiError> {
    let channel = state.app.get_channel(channel_id).await?;
    if channel.delete_at != 0 {
        return Err(ApiError::from(AppError::new(
            handler,
            deleted_id,
            None,
            "channel has been deleted".to_owned(),
            404,
        )));
    }
    Ok(channel)
}

/// Port of `checkViewWritePermission` (view.go:361).
///
/// **`create_post`** — writing a board is a posting right, not a channel-management one, so an
/// ordinary member can create, rename, reorder and delete views. The refusal names the same
/// permission it checked, which is not true of every handler in api4.
///
/// Note this is `SessionHasPermissionToChannel`, which reads the *membership*, while the three
/// read routes use `SessionHasPermissionToReadChannel`, which has an open-channel fallback. So a
/// non-member of a public channel can **list** its views and cannot **create** one.
async fn check_view_write_permission(
    state: &AppState,
    session: &AuthenticatedSession,
    channel_id: &str,
) -> Result<(), ApiError> {
    let (allowed, _) = state
        .app
        .session_has_permission_to_channel(&session.0, channel_id, &PERMISSION_CREATE_POST)
        .await;
    if allowed {
        return Ok(());
    }
    Err(ApiError::from(make_permission_error(
        &session.0,
        &[&PERMISSION_CREATE_POST],
    )))
}

/// The read gate of `getViewsForChannel`, `getView` and `getPostsForView`.
///
/// Returns the `is_member` half as well, because Go records it on the audit record
/// (`non_channel_member_access`) — audit is not ported, but dropping the value here would hide
/// that the distinction exists.
async fn check_view_read_permission(
    state: &AppState,
    session: &AuthenticatedSession,
    channel: &Channel,
) -> Result<bool, ApiError> {
    let (allowed, is_member) = state
        .app
        .session_has_permission_to_read_channel(&session.0, channel)
        .await;
    if allowed {
        return Ok(is_member);
    }
    Err(ApiError::from(make_permission_error(
        &session.0,
        &[&PERMISSION_READ_CHANNEL_CONTENT],
    )))
}

/// `c.RequireChannelId()` / `c.RequireViewId()` (web/context.go) — a 400 naming the parameter.
///
/// `RequireViewId` exists in Go's context and is called by five of the seven handlers; the
/// parameter name it reports is `view_id`.
fn require_id(value: &str, parameter: &'static str) -> Result<(), ApiError> {
    if is_valid_id(value) {
        return Ok(());
    }
    Err(ApiError::invalid_url_param(parameter))
}

/// `r.Header.Get(model.ConnectionId)` — absent is `""`, which broadcasts to everyone including
/// the caller.
fn connection_id(request: &Request) -> String {
    request
        .headers()
        .get(CONNECTION_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

// ---------------------------------------------------------------------------------------------
// createView
// ---------------------------------------------------------------------------------------------

/// Port of `createView` (api4/view.go:26) — `POST /api/v4/channels/{channel_id}/views`.
///
/// # The order, which decides which error a malformed request gets
///
/// 1. `RequireChannelId` — a 400 before the body is even read.
/// 2. **Decode the body**, before the channel is fetched. So `POST` to a channel that does not
///    exist with a broken body is `invalid_body_param`, not a 404.
/// 3. `GetChannel`, then the deleted-channel 404.
/// 4. **`ChannelId` and `CreatorId` are overwritten from the path and the session**, so whatever
///    a client sends in those two fields is discarded. Measured: a create naming another
///    channel's id comes back with the path's.
/// 5. The `create_post` permission check — *after* the fields are set, because the audit record
///    between them projects the view.
/// 6. `CreateView`, whose store call runs `PreSave` and then `IsValid`.
///
/// # What `PreSave` keeps and what it discards
///
/// A client-supplied `id` and `create_at` **survive** — `PreSave` only fills them when they are
/// empty or zero — while `delete_at` is reset to 0 and `update_at` is forced to equal `create_at`.
/// So `{"create_at": 12345}` really does create a view stamped 1970; measured.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn create_view(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let connection_id = connection_id(&request);
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the create-view body");
            return ApiError::invalid_param("view").into_response();
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(body.clone()));

    serve_create(&state, &channel_id, &session, &body, &connection_id)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_create(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
    body: &[u8],
    connection_id: &str,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }

    // `json.NewDecoder(r.Body).Decode(&view)` into a `*model.View`, then `|| view == nil`. A
    // literal `null` decodes without error into a nil pointer and is refused by that second
    // clause — hence `Option<View>` rather than `View`, which serde would have accepted as a
    // default-filled struct.
    let view: Option<View> = match decode_one_from_json(body) {
        Ok(view) => view,
        Err(err) => {
            tracing::debug!(error = %err, "the create-view body did not decode");
            return Outcome::Failed(ApiError::invalid_param("view"));
        }
    };
    let Some(mut view) = view else {
        return Outcome::Failed(ApiError::invalid_param("view"));
    };

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "createView",
        "api.view.create.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    // Both overwritten, whatever the body said.
    view.channel_id = channel_id.to_owned();
    view.creator_id = session.0.user_id.clone();

    if let Err(err) = check_view_write_permission(state, session, &channel.id).await {
        return Outcome::Failed(err);
    }

    if let Err(err) = state.app.create_view(&mut view, connection_id).await {
        return Outcome::Failed(ApiError::from(err));
    }

    encoded("createView", StatusCode::CREATED, &view)
}

// ---------------------------------------------------------------------------------------------
// getViewsForChannel
// ---------------------------------------------------------------------------------------------

/// Port of `getViewsForChannel` (api4/view.go:78) — `GET /api/v4/channels/{channel_id}/views`.
///
/// # `include_total_count` changes the shape of the response, not just its contents
///
/// Without it the body is a bare **array**; with it, a `ViewsWithCount` object
/// `{"views": [...], "total_count": n}`. The count is taken with the *caller's* options, which
/// the store ignores — so it is the channel's whole live population, not the page's length, and
/// that is what makes it useful.
///
/// `strconv.ParseBool` with the error discarded (params.go:303), so `=yes` is false.
///
/// # Two page sizes are live on this route
///
/// `c.Params.PerPage` has already defaulted to **60** (`web.PerPageDefault`) by the time it
/// reaches `ViewQueryOpts`, and the store's own default is **20**
/// ([`mm_model::view::VIEW_QUERY_DEFAULT_PER_PAGE`]). The store's is reached only by asking for
/// `per_page=0` explicitly, which `parse_per_page` passes through untouched.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %session.0.user_id))]
pub async fn get_views_for_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let query = request.uri().query().map(str::to_owned);
    serve_list(&state, &channel_id, &session, query.as_deref())
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_list(
    state: &AppState,
    channel_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
) -> Outcome {
    if let Err(err) = require_id(channel_id, "channel_id") {
        return Outcome::Failed(err);
    }

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "getViewsForChannel",
        "api.view.list.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_read_permission(state, session, &channel).await {
        return Outcome::Failed(err);
    }

    let opts = ViewQueryOpts {
        page: parse_page(query),
        per_page: parse_per_page(query),
    };

    let views = match state.app.get_views_for_channel(channel_id, &opts).await {
        Ok(views) => views,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    if query_flag_is_true(query, "include_total_count") {
        let total_count = match state
            .app
            .get_views_count_for_channel(channel_id, &opts)
            .await
        {
            Ok(count) => count,
            Err(err) => return Outcome::Failed(ApiError::from(err)),
        };
        // `Views` has no `omitempty`, so the key is always present; an empty channel is `[]`
        // because the app layer replaced the store's nil slice. `Some(vec![])` rather than `None`
        // is what encodes that.
        let with_count = ViewsWithCount {
            views: Some(views.into_iter().map(Some).collect()),
            total_count,
        };
        return encoded("getViewsForChannel", StatusCode::OK, &with_count);
    }

    encoded("getViewsForChannel", StatusCode::OK, &views)
}

// ---------------------------------------------------------------------------------------------
// getView
// ---------------------------------------------------------------------------------------------

/// Port of `getView` (api4/view.go:137) — `GET /api/v4/channels/{channel_id}/views/{view_id}`.
///
/// # The channel-mismatch check is a 404 and it runs *after* the read
///
/// `GetView` is keyed on the view id alone; the channel in the path is compared afterwards. So a
/// caller who can read channel A can discover **whether** a view id exists in channel B — the
/// answer is `api.view.get.channel_mismatch.app_error` for a real one and
/// `app.view.get.not_found.app_error` for a fictional one, both 404 but with different ids. That
/// is Go's, measured, and reproduced rather than hardened.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, view_id = %view_id))]
pub async fn get_view(
    State(state): State<AppState>,
    Path((channel_id, view_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    serve_get(&state, &channel_id, &view_id, &session)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_get(
    state: &AppState,
    channel_id: &str,
    view_id: &str,
    session: &AuthenticatedSession,
) -> Outcome {
    // `c.RequireChannelId().RequireViewId()` — chained, so the channel is checked first and its
    // 400 wins when both are malformed.
    if let Err(err) =
        require_id(channel_id, "channel_id").and_then(|()| require_id(view_id, "view_id"))
    {
        return Outcome::Failed(err);
    }

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "getView",
        "api.view.get.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_read_permission(state, session, &channel).await {
        return Outcome::Failed(err);
    }

    let view = match state.app.get_view(view_id).await {
        Ok(view) => view,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    if view.channel_id != channel_id {
        return Outcome::Failed(ApiError::from(AppError::new(
            "getView",
            "api.view.get.channel_mismatch.app_error",
            None,
            String::new(),
            404,
        )));
    }

    encoded("getView", StatusCode::OK, &view)
}

// ---------------------------------------------------------------------------------------------
// updateView
// ---------------------------------------------------------------------------------------------

/// Port of `updateView` (api4/view.go:186) — `PATCH /api/v4/channels/{channel_id}/views/{view_id}`.
///
/// # The write permission is checked before the view is read
///
/// So a caller without `create_post` gets a **403** for a view that does not exist, where the read
/// routes would have given a 404. The ordering is a small information-disclosure choice and it is
/// Go's.
///
/// # A patch is applied in full and *then* validated
///
/// `{"title": ""}` is applied — `ViewPatch`'s guards test the pointer, not the value — and the
/// store's `IsValid` then refuses it with `model.view.is_valid.title.app_error`. An empty object
/// `{}` is a legal patch that changes nothing except `update_at`. Both measured.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, view_id = %view_id))]
pub async fn update_view(
    State(state): State<AppState>,
    Path((channel_id, view_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let connection_id = connection_id(&request);
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the update-view body");
            return ApiError::invalid_param("viewPatch").into_response();
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(body.clone()));

    serve_update(
        &state,
        &channel_id,
        &view_id,
        &session,
        &body,
        &connection_id,
    )
    .await
    .finish(state.clone(), request)
    .await
}

async fn serve_update(
    state: &AppState,
    channel_id: &str,
    view_id: &str,
    session: &AuthenticatedSession,
    body: &[u8],
    connection_id: &str,
) -> Outcome {
    if let Err(err) =
        require_id(channel_id, "channel_id").and_then(|()| require_id(view_id, "view_id"))
    {
        return Outcome::Failed(err);
    }

    // `*model.ViewPatch`, so a literal `null` is a nil pointer and a 400 — the parameter is named
    // **`viewPatch`**, camel-cased where every other name in this file is snake.
    let patch: Option<ViewPatch> = match decode_one_from_json(body) {
        Ok(patch) => patch,
        Err(err) => {
            tracing::debug!(error = %err, "the update-view body did not decode");
            return Outcome::Failed(ApiError::invalid_param("viewPatch"));
        }
    };
    let Some(patch) = patch else {
        return Outcome::Failed(ApiError::invalid_param("viewPatch"));
    };

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "updateView",
        "api.view.update.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_write_permission(state, session, &channel.id).await {
        return Outcome::Failed(err);
    }

    let view = match state.app.get_view(view_id).await {
        Ok(view) => view,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    if view.channel_id != channel_id {
        return Outcome::Failed(ApiError::from(AppError::new(
            "updateView",
            "api.view.update.channel_mismatch.app_error",
            None,
            String::new(),
            404,
        )));
    }

    match state
        .app
        .update_view(view, Some(&patch), connection_id)
        .await
    {
        Ok(updated) => encoded("updateView", StatusCode::OK, &updated),
        Err(err) => Outcome::Failed(ApiError::from(err)),
    }
}

// ---------------------------------------------------------------------------------------------
// deleteView
// ---------------------------------------------------------------------------------------------

/// Port of `deleteView` (api4/view.go:251) —
/// `DELETE /api/v4/channels/{channel_id}/views/{view_id}`.
///
/// The only route in the file that answers `{"status":"OK"}` rather than a document, and
/// therefore the only one with **no trailing newline**.
///
/// Deleting twice is a **404**, not an idempotent 200: `GetView` filters on `DeleteAt = 0`, so the
/// second call cannot find the row it soft-deleted. Measured.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, view_id = %view_id))]
pub async fn delete_view(
    State(state): State<AppState>,
    Path((channel_id, view_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let connection_id = connection_id(&request);
    serve_delete(&state, &channel_id, &view_id, &session, &connection_id)
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_delete(
    state: &AppState,
    channel_id: &str,
    view_id: &str,
    session: &AuthenticatedSession,
    connection_id: &str,
) -> Outcome {
    if let Err(err) =
        require_id(channel_id, "channel_id").and_then(|()| require_id(view_id, "view_id"))
    {
        return Outcome::Failed(err);
    }

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "deleteView",
        "api.view.delete.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_write_permission(state, session, &channel.id).await {
        return Outcome::Failed(err);
    }

    let view = match state.app.get_view(view_id).await {
        Ok(view) => view,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    if view.channel_id != channel_id {
        return Outcome::Failed(ApiError::from(AppError::new(
            "deleteView",
            "api.view.delete.channel_mismatch.app_error",
            None,
            String::new(),
            404,
        )));
    }

    if let Err(err) = state.app.delete_view(&view, connection_id).await {
        return Outcome::Failed(ApiError::from(err));
    }

    status_ok()
}

// ---------------------------------------------------------------------------------------------
// updateViewSortOrder
// ---------------------------------------------------------------------------------------------

/// Port of `updateViewSortOrder` (api4/view.go:303) —
/// `POST /api/v4/channels/{channel_id}/views/{view_id}/sort_order`.
///
/// # The body is a bare integer
///
/// `json.NewDecoder(r.Body).Decode(&newSortOrder)` into an `int64`. So the request body is `2`,
/// and `{"sort_order": 2}` is a 400 naming **`viewSortOrder`** — as is `1.5`, because Go's decoder
/// refuses a fractional number for an integer target. Both measured.
///
/// # Two different 400s for an out-of-range index
///
/// A **negative** index is refused by the handler (`api.context.invalid_body_param.app_error`,
/// `viewSortOrder`). An index **past the end of the list** reaches the store and comes back as
/// `app.view.update_sort_order.invalid_input.app_error`. Same status, different id, and the
/// boundary between them is the handler's `newSortOrder < 0`.
///
/// # The answer is the whole channel, renumbered
///
/// Not the moved view: every live view in the channel, in its new order, each carrying a fresh
/// `sort_order` (`0..n-1`) and one shared `update_at`. The `view_sorted` websocket event carries
/// the same list.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, view_id = %view_id))]
pub async fn update_view_sort_order(
    State(state): State<AppState>,
    Path((channel_id, view_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let connection_id = connection_id(&request);
    let (parts, body) = request.into_parts();
    let body = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(body) => body,
        Err(err) => {
            tracing::debug!(error = %err, "could not read the sort-order body");
            return ApiError::invalid_param("viewSortOrder").into_response();
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(body.clone()));

    serve_sort_order(
        &state,
        &channel_id,
        &view_id,
        &session,
        &body,
        &connection_id,
    )
    .await
    .finish(state.clone(), request)
    .await
}

async fn serve_sort_order(
    state: &AppState,
    channel_id: &str,
    view_id: &str,
    session: &AuthenticatedSession,
    body: &[u8],
    connection_id: &str,
) -> Outcome {
    if let Err(err) =
        require_id(channel_id, "channel_id").and_then(|()| require_id(view_id, "view_id"))
    {
        return Outcome::Failed(err);
    }

    let new_sort_order: i64 = match decode_one_from_json(body) {
        Ok(value) => value,
        Err(err) => {
            tracing::debug!(error = %err, "the sort-order body did not decode");
            return Outcome::Failed(ApiError::invalid_param("viewSortOrder"));
        }
    };

    // The handler's own bound, ahead of everything else the request touches.
    if new_sort_order < 0 {
        return Outcome::Failed(ApiError::invalid_param("viewSortOrder"));
    }

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "updateViewSortOrder",
        "api.view.update_sort_order.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_write_permission(state, session, &channel.id).await {
        return Outcome::Failed(err);
    }

    // **No `GetView` and no channel-mismatch check here**, unlike its four neighbours: the store
    // reads the channel's list and reports a view that is not in it as a not-found. So a real
    // view id from another channel is `app.view.update_sort_order.not_found.app_error`, where
    // `updateView` would have said `channel_mismatch`.
    match state
        .app
        .update_view_sort_order(view_id, channel_id, new_sort_order, connection_id)
        .await
    {
        Ok(views) => encoded("updateViewSortOrder", StatusCode::OK, &views),
        Err(err) => Outcome::Failed(ApiError::from(err)),
    }
}

// ---------------------------------------------------------------------------------------------
// getPostsForView
// ---------------------------------------------------------------------------------------------

/// Port of `getPostsForView` (api4/view.go:370) —
/// `GET /api/v4/channels/{channel_id}/views/{view_id}/posts`.
///
/// # The view is read, validated, and then ignored
///
/// Go's comment is explicit: "In the future, this will filter posts based on the view's
/// configuration … For now, it returns all posts in the channel." So the answer is identical to
/// `GET /channels/{id}/posts` for the same page — but the 404s on the way there are not, and a
/// client cannot reach this route with a view id belonging to another channel.
///
/// # What it does *not* carry that `getPostsForChannel` does
///
/// No etag, so no `If-None-Match` handling and no 304; no `skipFetchThreads`,
/// `collapsedThreads`, `since`, `before`, `after` or `include_deleted`; and no
/// `AddCursorIdsForPostList`, so `next_post_id` and `prev_post_id` are whatever the store left
/// (empty strings). Only `page` and `per_page` are read. Reproducing the neighbour's cursor work
/// here would add fields Go does not compute.
///
/// # Two post-list stages can refuse to be reproduced
///
/// `PreparePostListForClient` and `SanitizePostListMetadataForUser` are shared with
/// [`crate::posts`] and forward the whole request when a post carries something this port will not
/// guess at. With the feature flag on and the upstream's flag off, that forward reaches a Go that
/// answers a mux 404 — a divergence that cannot arise on a deployment where both flags agree, and
/// one worth naming rather than hiding.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, view_id = %view_id))]
pub async fn get_posts_for_view(
    State(state): State<AppState>,
    Path((channel_id, view_id)): Path<(String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !integrated_boards_enabled() {
        return proxy::forward_to_go(State(state), request).await;
    }
    let query = request.uri().query().map(str::to_owned);
    serve_posts(&state, &channel_id, &view_id, &session, query.as_deref())
        .await
        .finish(state.clone(), request)
        .await
}

async fn serve_posts(
    state: &AppState,
    channel_id: &str,
    view_id: &str,
    session: &AuthenticatedSession,
    query: Option<&str>,
) -> Outcome {
    if let Err(err) =
        require_id(channel_id, "channel_id").and_then(|()| require_id(view_id, "view_id"))
    {
        return Outcome::Failed(err);
    }

    let channel = match channel_or_deleted(
        state,
        channel_id,
        "getPostsForView",
        "api.view.get_posts.deleted_channel.app_error",
    )
    .await
    {
        Ok(channel) => channel,
        Err(err) => return Outcome::Failed(err),
    };

    if let Err(err) = check_view_read_permission(state, session, &channel).await {
        return Outcome::Failed(err);
    }

    let view = match state.app.get_view(view_id).await {
        Ok(view) => view,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    if view.channel_id != channel_id {
        return Outcome::Failed(ApiError::from(AppError::new(
            "getPostsForView",
            "api.view.get_posts.channel_mismatch.app_error",
            None,
            String::new(),
            404,
        )));
    }

    // `model.GetPostsOptions{ChannelId, Page, PerPage, UserId}` — the other four fields are left
    // at their zero values, which is what makes this the plain, uncollapsed, threads-fetched page.
    let opts = GetPostsOptions {
        channel_id,
        user_id: &session.0.user_id,
        page: parse_page(query),
        per_page: parse_per_page(query),
        skip_fetch_threads: false,
        collapsed_threads: false,
        include_deleted: false,
    };

    let list = match state.app.get_posts_for_view(opts).await {
        Ok(list) => list,
        Err(err) => return Outcome::Failed(ApiError::from(err)),
    };

    let prepared = match state.app.prepare_post_list_for_client(&list).await {
        Ok(prepared) => prepared,
        Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding the view's posts to Go");
            return Outcome::Forward;
        }
        Err(mm_app::post::PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    let (mut sanitized, _all_previews_have_membership) = match state
        .app
        .sanitize_post_list_metadata_for_user(prepared, &session.0.user_id)
        .await
    {
        Ok(sanitized) => sanitized,
        Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, channel_id, "forwarding the view's posts to Go");
            return Outcome::Forward;
        }
        Err(mm_app::post::PrepareError::App(err)) => return Outcome::Failed(ApiError::from(err)),
    };

    // `clientPostList.EncodeJSON(w)` — `PostList`'s own encoder, not `json.Marshal`, because it
    // reproduces Go's precomputed-JSON path. See `mm_model::post_list`.
    let mut body = Vec::new();
    if let Err(err) = sanitized.encode_json(&mut body) {
        tracing::error!(error = %err, "failed to serialise the view's PostList");
        return Outcome::Failed(ApiError::from(AppError::new(
            "getPostsForView",
            "api.marshal_error",
            None,
            String::new(),
            500,
        )));
    }

    Outcome::Served(
        (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag's twelve accepted spellings, and the fallback direction for everything else.
    ///
    /// Read through a closure rather than the process environment: `OnceLock` would freeze the
    /// first answer for the whole test binary, and `config`'s own test asserts that no `MM_`
    /// variable is set during a run.
    fn parse(raw: Option<&str>) -> bool {
        matches!(raw, Some("1" | "t" | "T" | "TRUE" | "true" | "True"))
    }

    #[test]
    fn the_feature_flag_accepts_exactly_go_strconv_parse_bools_true_spellings() {
        for raw in ["1", "t", "T", "TRUE", "true", "True"] {
            assert!(parse(Some(raw)), "{raw} should enable the feature");
        }
        for raw in ["0", "f", "F", "FALSE", "false", "False"] {
            assert!(!parse(Some(raw)), "{raw} should not enable the feature");
        }
    }

    /// The fallback direction: an unparseable value leaves the flag at `SetDefaults`' `false`,
    /// exactly as viper does. Treating a typo as `true` would open seven routes on a server the
    /// operator meant to leave dark.
    #[test]
    fn an_unset_or_unparseable_flag_is_false() {
        assert!(!parse(None));
        assert!(!parse(Some("yes")));
        assert!(!parse(Some("tRuE")));
        assert!(!parse(Some("")));
    }

    /// `json.NewEncoder(w).Encode` leaves a trailing newline; `w.Write(MapToJSON(...))` does not.
    /// The two spellings live one function apart in this module, which is how they get swapped.
    #[tokio::test]
    async fn an_encoded_document_ends_in_a_newline_and_status_ok_does_not() {
        let Outcome::Served(response) =
            encoded("test", StatusCode::OK, &serde_json::json!({"a": 1}))
        else {
            panic!("encoding a plain object cannot fail");
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        assert_eq!(&body[..], b"{\"a\":1}\n");

        let Outcome::Served(response) = status_ok() else {
            panic!("status_ok always serves");
        };
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("a body");
        assert_eq!(&body[..], br#"{"status":"OK"}"#);
    }

    /// An empty channel must list as `[]`. `Vec::new()` gives that; a `None` in `ViewsWithCount`
    /// would give `null`, which is the shape Go's *store* produces and its *app layer* removes.
    #[test]
    fn an_empty_list_encodes_as_an_array_in_both_shapes() {
        let views: Vec<View> = Vec::new();
        assert_eq!(serde_json::to_string(&views).expect("encodes"), "[]");

        let with_count = ViewsWithCount {
            views: Some(Vec::new()),
            total_count: 0,
        };
        assert_eq!(
            serde_json::to_string(&with_count).expect("encodes"),
            r#"{"views":[],"total_count":0}"#
        );
    }

    /// A `null` body is a nil pointer in Go and a 400 — not a default-filled struct, which is what
    /// `decode_one_from_json::<View>` would have produced.
    #[test]
    fn a_null_body_decodes_to_none_rather_than_a_default_view() {
        let decoded: Option<View> = decode_one_from_json(b"null").expect("null decodes");
        assert!(decoded.is_none());

        let decoded: Option<ViewPatch> = decode_one_from_json(b"null").expect("null decodes");
        assert!(decoded.is_none());
    }

    /// The sort-order body is a bare integer. An object and a fractional number are both refused,
    /// and a whole number written with a decimal point is refused too — Go's decoder is strict
    /// about the target type, not about the literal's spelling.
    #[test]
    fn the_sort_order_body_accepts_only_a_json_integer() {
        assert_eq!(decode_one_from_json::<i64>(b"2").ok(), Some(2));
        assert_eq!(decode_one_from_json::<i64>(b"0").ok(), Some(0));
        assert_eq!(decode_one_from_json::<i64>(b"-1").ok(), Some(-1));
        assert!(decode_one_from_json::<i64>(br#"{"sort_order":1}"#).is_err());
        assert!(decode_one_from_json::<i64>(b"1.5").is_err());
        assert!(decode_one_from_json::<i64>(b"\"2\"").is_err());
    }
}
