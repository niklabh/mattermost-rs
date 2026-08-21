//! Ported handlers from `channels/api4/channel.go`:
//!
//! - `getChannel` — `GET /api/v4/channels/{channel_id}`
//! - `getChannelMember` — `GET /api/v4/channels/{channel_id}/members/{user_id}`
//! - `getChannelUnread` — `GET /api/v4/users/{user_id}/channels/{channel_id}/unread`
//! - `getChannelStats` — `GET /api/v4/channels/{channel_id}/stats`
//! - `getChannelMembers` — `GET /api/v4/channels/{channel_id}/members`
//! - `getChannelByName` — `GET /api/v4/teams/{team_id}/channels/name/{channel_name}`
//! - `getChannelsForTeamForUser` — `GET /api/v4/users/{user_id}/teams/{team_id}/channels`
//! - `getChannelsForUser` — `GET /api/v4/users/{user_id}/channels` (streamed in Go; see the
//!   handler for the byte layout that implies)
//! - `getPublicChannelsForTeam` — `GET /api/v4/teams/{team_id}/channels`
//! - `getPrivateChannelsForTeam` — `GET /api/v4/teams/{team_id}/channels/private`
//! - `getDeletedChannelsForTeam` — `GET /api/v4/teams/{team_id}/channels/deleted`
//!
//! # The first route migrated *through* a permission check
//!
//! Every route served so far was portable because its permission check could not change the
//! answer: `me`-scoped handlers whose checks short-circuit for self ([D-094]), or a sanitiser
//! that is a no-op on one's own data. This one has no such escape. Its only gate is
//! `SessionHasPermissionToChannel(session, channelID, PermissionReadChannel)`, the check ported
//! in `mm-app/src/authorization.rs` and verified branch-by-branch against the running Go server,
//! and getting it wrong leaks another user's channel membership.
//!
//! # And the first route with path parameters
//!
//! Which brings Go's `RequireChannelId().RequireUserId()` with it — see [`require_id`] and the
//! `me` alias below.
//!
//! # A trap both routes share: the mux regex is narrower than axum's segment
//!
//! Go registers these paths with `{channel_id:[A-Za-z0-9]+}` (api.go:203, :223). A segment
//! containing anything else — a hyphen, a dot, a percent-escape — **does not match the route at
//! all**, so gorilla/mux answers 404 with an empty body before any handler runs. axum's `{name}`
//! matches the whole segment, so the same request reaches our handler and gets the 400 that
//! `IsValidId` produces. Measured, not assumed — see [D-150].

use axum::extract::{Path, Request, State};
use axum::http::header::IF_NONE_MATCH;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_model::channel::{CHANNEL_TYPE_OPEN, ChannelSearchOpts, is_valid_channel_identifier};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_LIST_TEAM_CHANNELS, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_MANAGE_TEAM, PERMISSION_READ_CHANNEL, PERMISSION_READ_PUBLIC_CHANNEL,
    PERMISSION_VIEW_TEAM, Permission, make_permission_error,
};
use mm_model::utils::{is_valid_id, parse_go_bool};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.Me` (user.go:26) — the literal a client may send instead of its own id.
pub(crate) const ME: &str = "me";

/// Port of `Context.RequireChannelId` / `RequireUserId` (web/context.go:388, :296).
///
/// Both are the same one-line check against `IsValidId`, differing only in the parameter name
/// they report. Go chains them (`c.RequireChannelId().RequireUserId()`) and each returns early if
/// an error is already set, so **the channel id is validated first** and its error wins when both
/// are malformed. The `?` operator gives the same ordering for free, provided the calls stay in
/// Go's order.
// `ApiError` wraps `AppError`, which is 192 bytes because it *is* the wire format — seven fields
// a client parses. Every handler in this crate already returns `Result<Response, ApiError>` and
// pays the same size; clippy only notices here because the `Ok` side is `()`. Boxing these two
// helpers alone would buy nothing while making them differ from every other signature in the
// crate. Boxing `AppError` inside `ApiError` crate-wide is the real answer — see [D-146].
#[allow(clippy::result_large_err)]
pub(crate) fn require_id(value: &str, parameter: &'static str) -> Result<(), ApiError> {
    if is_valid_id(value) {
        Ok(())
    } else {
        Err(ApiError::invalid_url_param(parameter))
    }
}

/// Go's `c.RequireChannelId().RequireUserId()`, as one call so the **order** is testable.
///
/// The order is not observable from a response body. `AppError` marshals only `id`, `message`,
/// `detailed_error`, `request_id` and `status_code` — `params` is not on the wire — so the
/// parameter name reaches a client solely through the *translated* message, which this server
/// does not produce yet ([D-092]). A mutation swapping the two calls therefore survives every
/// cross-server test. Hence this function and its unit test: the ordering is pinned in-process
/// because it cannot be pinned over HTTP.
#[allow(clippy::result_large_err)]
fn validate_ids(channel_id: &str, user_id: &str) -> Result<(), ApiError> {
    require_id(channel_id, "channel_id")?;
    require_id(user_id, "user_id")?;
    Ok(())
}

/// Port of `getChannelMember` (api4/channel.go).
///
/// # Order of operations, which is the security-relevant part
///
/// 1. **`me` is resolved to the session's user id before validation**, not after — otherwise the
///    literal `"me"` fails `IsValidId` and the route 400s where Go answers.
/// 2. **Both ids are validated**, channel first.
/// 3. **The permission check runs before the member is fetched.** Reversing that would let a
///    caller distinguish "no such member" from "not allowed to look", which is exactly the
///    inference a 403 exists to prevent — and it would put a database read behind an
///    unauthenticated-in-effect path.
/// 4. `SanitizeForCurrentUser` blanks the two timestamps for anyone but the member themselves.
///
/// Note the permission is `read_channel` on the **channel**, not on the target user: any member
/// of a channel may read any other member's row there. That is Go's rule, and it is why the
/// sanitiser exists rather than an ownership check.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(member)` — so this one **has** a trailing newline, unlike
/// `/users/me/sessions` and `/users/me/teams/members` ([D-086]).
///
/// # Not ported
///
/// `c.AppContext.With(app.RequestContextWithMaster)`, which pins the *following* read to the
/// primary rather than a replica. This port has one pool and always reads the primary, so the
/// line is already true here — see [D-140].
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn get_channel_member(
    State(state): State<AppState>,
    Path((channel_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireUserId` substitutes the session's id for `me` **before** the validity check
    // (web/context.go:301), so the alias works and an invalid literal still 400s.
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    validate_ids(&channel_id, &user_id)?;

    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    if !allowed {
        // `c.SetPermissionError(model.PermissionReadChannel)` — 403, with the session's user id
        // and the permission name in the detail. The detail is wiped before the body is written
        // unless developer mode is on, which `ApiError::into_response` already reproduces.
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL],
        )));
    }

    let mut member = state.app.get_channel_member(&channel_id, &user_id).await?;

    member.sanitize_for_current_user(&session.0.user_id);

    let mut body = serde_json::to_vec(&member).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise ChannelMember");
        ApiError(mm_model::utils::AppError::new(
            "getChannelMember",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// Go's two permission gates for [`get_channel_unread`], in order and with the short circuit.
///
/// # Why this is a function
///
/// Neither the order nor the permission each gate reports is observable from a response. Both
/// gates answer 403 with `api.context.permissions.app_error`; the permission name reaches the
/// client only through `detailed_error`, which `ApiError::into_response` wipes because Go wipes it
/// unless `EnableDeveloper` ([D-092] again). So three separate mutations — swapping the gates,
/// naming `read_channel` in the user gate, naming `edit_other_users` in the channel gate — all
/// survive every cross-server test. They are pinned here instead.
///
/// # The short circuit is not a micro-optimisation
///
/// `channel_allowed` is taken as a closure, not a `bool`, because Go never evaluates it when the
/// user gate denies — and evaluating it means `SessionHasPermissionToChannel`, which reads
/// `ChannelMembers` and possibly `Roles`. A port that computed both up front would issue queries
/// on behalf of a caller Go has already refused, which is the wrong direction for a gate to fail
/// in. The test below asserts the closure is never polled.
async fn first_denied_permission<F, Fut>(
    user_allowed: bool,
    channel_allowed: F,
) -> Option<&'static mm_model::permission::Permission>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if !user_allowed {
        return Some(&PERMISSION_EDIT_OTHER_USERS);
    }
    if !channel_allowed().await {
        return Some(&PERMISSION_READ_CHANNEL);
    }
    None
}

/// Port of `getChannelUnread` (api4/channel.go:979), reached as
/// `GET /api/v4/users/{user_id}/channels/{channel_id}/unread`.
///
/// # The path says user-then-channel and the validation says channel-then-user
///
/// The route is registered under `BaseRoutes.ChannelForUser` (api.go:223), so the **user** id is
/// the first path segment — but the handler opens with `c.RequireChannelId().RequireUserId()`,
/// so the **channel** id is validated first and wins when both are malformed. The two orders are
/// genuinely different and both are reproduced: the `Path` tuple is `(user_id, channel_id)`
/// because axum binds by position, and [`validate_ids`] is called `(channel_id, user_id)` because
/// Go's chain is.
///
/// # Two gates, and the first one is about the *user*
///
/// This is the first migrated route with more than one permission check.
/// `SessionHasPermissionToUser` runs first — "may I ask about this person at all" — and only then
/// `SessionHasPermissionToChannel` with `read_channel`. Asking about **oneself** always passes
/// the first gate (authorization.go's self branch), so the common case reaches the second; asking
/// about someone else needs `edit_other_users`, which no ordinary role holds. See
/// [`first_denied_permission`] for why the order is pinned in-process rather than over HTTP.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(channelUnread)` — trailing newline, like `getChannelMember`
/// ([D-086]). `ChannelUnread.NotifyProps` carries `json:"-"`, so the body is exactly seven keys
/// however the member's notify props are set.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, user_id = %user_id))]
pub async fn get_channel_unread(
    State(state): State<AppState>,
    Path((user_id, channel_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `me` again, resolved before the validity check (web/context.go:301).
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    validate_ids(&channel_id, &user_id)?;

    let user_allowed = state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await;

    let denied = first_denied_permission(user_allowed, || async {
        let (allowed, _is_member) = state
            .app
            .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
            .await;
        allowed
    })
    .await;

    if let Some(permission) = denied {
        return Err(ApiError(*make_permission_error(&session.0, &[permission])));
    }

    let unread = state.app.get_channel_unread(&channel_id, &user_id).await?;

    let mut body = serde_json::to_vec(&unread).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise ChannelUnread");
        ApiError(mm_model::utils::AppError::new(
            "getChannelUnread",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// `model.AsContentReviewerParam` (content_flagging.go:19).
const AS_CONTENT_REVIEWER_PARAM: &str = "as_content_reviewer";

/// `getChannelStats`'s query parameter — a literal in the handler, no model constant to cite.
const EXCLUDE_FILES_COUNT_PARAM: &str = "exclude_files_count";

/// Go's boolean-query-flag idiom: `r.URL.Query().Get(key)` — the **first** value when the key
/// repeats — fed to `strconv.ParseBool` with the error discarded. So a bare key, `=yes`, or `=`
/// are all false, while `=1`, `=t` and `=True` are true. `form_urlencoded::parse` decodes
/// percent-escapes and `+`-as-space the way `url.ParseQuery` does, which matters because the
/// *decoded* value is what `ParseBool` sees.
pub(crate) fn query_flag_is_true(query: Option<&str>, flag: &str) -> bool {
    let Some(query) = query else {
        return false;
    };
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == flag)
        .and_then(|(_, value)| parse_go_bool(&value))
        .unwrap_or(false)
}

/// Would Go take the content-reviewer path for this query string? Shared with `getTeam`, which
/// carries the same flag (api4/team.go:316) and forwards it the same way.
pub(crate) fn is_content_reviewer_request(query: Option<&str>) -> bool {
    query_flag_is_true(query, AS_CONTENT_REVIEWER_PARAM)
}

/// `web.PageDefault` (params.go:18).
const PAGE_DEFAULT: i64 = 0;
/// `web.PerPageDefault` (params.go:19).
const PER_PAGE_DEFAULT: i64 = 60;
/// `web.PerPageMaximum` (params.go:20).
const PER_PAGE_MAXIMUM: i64 = 200;

/// `url.Values.Get`: the first value of a repeated key, percent-decoded. `None` when absent.
pub(crate) fn query_first(query: Option<&str>, key: &str) -> Option<String> {
    form_urlencoded::parse(query?.as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, value)| value.into_owned())
}

/// Port of the `page` half of `web.ParamsFromRequest` (params.go:217): `strconv.Atoi` failure
/// **or a negative value** falls to the default — there is no 400 for garbage pagination, ever.
/// (Go's negative branch carries a carve-out for `getChannelMembersForUser`'s streaming mode;
/// no ported route is that one, so the plain rule applies.)
pub(crate) fn parse_page(query: Option<&str>) -> i64 {
    match query_first(query, "page").and_then(|v| v.parse::<i64>().ok()) {
        Some(val) if val >= 0 => val,
        _ => PAGE_DEFAULT,
    }
}

/// The `per_page` half (params.go:234): failure or negative → 60, above 200 → clamped to 200 —
/// and **zero is neither**, so `?per_page=0` survives the parser and reaches the store, whose
/// `Limit > 0` guard turns it into *no limit at all*. A caller can ask for everything by asking
/// for nothing, and both servers oblige.
pub(crate) fn parse_per_page(query: Option<&str>) -> i64 {
    match query_first(query, "per_page").and_then(|v| v.parse::<i64>().ok()) {
        Some(val) if val > PER_PAGE_MAXIMUM => PER_PAGE_MAXIMUM,
        Some(val) if val >= 0 => val,
        _ => PER_PAGE_DEFAULT,
    }
}

/// Go's permission block for [`get_channel`] (api4/channel.go:877-892), with both gates lazy so
/// the evaluation *order* — which is invisible over HTTP — is testable in-process, exactly like
/// [`first_denied_permission`].
///
/// Two properties carry the security, and each is a mutation someone could plausibly ship:
///
/// - **An open channel asks the team first** — `read_public_channel` on `channel.TeamId` — and
///   polls the channel gate only when the team denies. A member of the team can read any public
///   channel in it without a membership row, which is what "public" means.
/// - **A non-open channel never consults the team gate.** `read_channel` via membership is the
///   only way in; handing a private channel the open-channel fallback would leak it to the whole
///   team. (The checker's own internal team fallback still runs, but for `read_channel` — a
///   different permission that plain members do not hold team-wide.)
///
/// Both denials report `read_channel` — Go calls `SetPermissionError(PermissionReadChannel)` in
/// both branches, so `read_public_channel` never appears in an error even when it was the gate
/// that ran first.
///
/// Go's non-open branch then tries `serveDiscoverableNonMember` before the 403. That surface is
/// gated on `FeatureFlags.DiscoverableChannels`, which is **false** at the pinned SHA
/// (feature_flags.go:208) and unset in this deployment, so the whole function is
/// `if !flag { return nil, nil }` — not served, fall through to the permission error. Pinned
/// rather than ported: serving it needs `GetUser`, `IsDiscoverableJoinAllowed` and a feature-flag
/// config surface this server does not have. See TECH_DEBT [D-153].
async fn channel_read_denied<TF, TFut, CF, CFut>(
    channel_is_open: bool,
    team_allowed: TF,
    channel_allowed: CF,
) -> bool
where
    TF: FnOnce() -> TFut,
    TFut: std::future::Future<Output = bool>,
    CF: FnOnce() -> CFut,
    CFut: std::future::Future<Output = bool>,
{
    if channel_is_open && team_allowed().await {
        return false;
    }
    !channel_allowed().await
}

/// [`get_channel`]'s one refusal: `c.SetPermissionError(model.PermissionReadChannel)`, from
/// **both** branches of the permission block — `read_public_channel` never appears in an error
/// even when it was the gate that ran first (api4/channel.go:881, :890).
///
/// A function because the permission's name reaches a client only through `detailed_error`,
/// which is wiped unless developer mode is on ([D-092]) — so naming the wrong permission here
/// survived every cross-server test when it was inline. Same in-process pinning as
/// [`validate_ids`] and [`first_denied_permission`].
fn get_channel_denial(session: &mm_model::session::Session) -> ApiError {
    ApiError(*make_permission_error(session, &[&PERMISSION_READ_CHANNEL]))
}

/// Port of `getChannel` (api4/channel.go:827), reached as `GET /api/v4/channels/{channel_id}`.
///
/// # Order of operations
///
/// 1. **The content-reviewer branch is forwarded, and it is detected first.** Go checks
///    `as_content_reviewer` *after* `RequireChannelId` and `GetChannel`, but forwarding the whole
///    request lets Go re-run those steps itself, so every subcase — bad id, missing channel, no
///    license — is answered by the server that owns the answer. The branch is Enterprise
///    Advanced–licensed and config-gated (`requireContentFlaggingEnabled`), which this deployment
///    fails at the license step; reproducing the resulting 501 would pin a body we cannot oracle.
///    Same Strangler-inside-a-route pattern as the `flagged_post` preferences.
/// 2. `RequireChannelId` — no `me` alias here; the segment is already charset-checked by
///    [`crate::partially_migrated_with_ids`].
/// 3. **`GetChannel` runs before the permission check** (unlike `getChannelMember`, where the
///    permission check's own lookup is the fetch). The handler needs `channel.Type` and
///    `channel.TeamId` to *choose* the gate, so a missing channel is a 404 here, not a 403 —
///    Go's shape, asserted against the running server.
/// 4. The two-gate permission block — see [`channel_read_denied`].
/// 5. `FillInChannelProps` — resolves `~mentions` in the header into a `channel_mentions` prop.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(channel)` — trailing newline ([D-086]). The body is the first full
/// `Channel` this server puts on the wire; its serialisation is fixture-pinned in `mm-model` and
/// byte-compared against Go in `tests/parity_channel_get.rs`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, forwarded))]
pub async fn get_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if is_content_reviewer_request(request.uri().query()) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let mut channel = match state.app.get_channel(&channel_id).await {
        Ok(channel) => channel,
        Err(err) => return ApiError(err).into_response(),
    };

    let denied = channel_read_denied(
        channel.channel_type == CHANNEL_TYPE_OPEN,
        || async {
            state
                .app
                .session_has_permission_to_team(
                    &session.0,
                    &channel.team_id,
                    &PERMISSION_READ_PUBLIC_CHANNEL,
                )
                .await
        },
        || async {
            let (allowed, _is_member) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &channel_id,
                    &PERMISSION_READ_CHANNEL,
                )
                .await;
            allowed
        },
    )
    .await;

    if denied {
        return get_channel_denial(&session.0).into_response();
    }

    if let Err(err) = state.app.fill_in_channel_props(&mut channel).await {
        return ApiError(err).into_response();
    }

    let mut body = match serde_json::to_vec(&channel) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise Channel");
            return ApiError(mm_model::utils::AppError::new(
                "getChannel",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');

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

/// Go's `filesCount := int64(-1)` and the guard around the fourth query
/// (api4/channel.go:1038-1045), as one function so both are testable without a database.
///
/// `-1` is not a placeholder that gets fixed up later — it **is the wire value** when
/// `exclude_files_count` is true, and `ChannelStats.FilesCount` has no `omitempty`, so the client
/// sees the sentinel. The fetch is a closure for the same reason [`first_denied_permission`]'s
/// gate is: when the flag is set Go never runs the `FileInfo` count, and a port that ran it
/// anyway and then overwrote the result would issue a query the flag exists to skip — invisible
/// over HTTP, pinned by the test asserting the closure is never polled.
async fn files_count_unless_excluded<F, Fut>(
    exclude_files_count: bool,
    fetch: F,
) -> Result<i64, mm_model::utils::AppError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<i64, mm_model::utils::AppError>>,
{
    if exclude_files_count {
        return Ok(-1);
    }
    fetch().await
}

/// Port of `getChannelStats` (api4/channel.go:1006), reached as
/// `GET /api/v4/channels/{channel_id}/stats`.
///
/// # Order of operations
///
/// 1. `exclude_files_count` is read first, as Go does — observably irrelevant, since parsing
///    cannot fail, but kept in Go's order so the two read alike.
/// 2. `RequireChannelId` — the segment charset is already checked by
///    [`crate::partially_migrated_with_ids`].
/// 3. One gate: `SessionHasPermissionToChannel(read_channel)`, no open-channel team fallback —
///    unlike `getChannel`, a team member who never joined a **public** channel is refused its
///    stats. That asymmetry is Go's, asserted against the running server.
/// 4. Four counts, in Go's order — member, guest, pinned, files — each returning its own error,
///    so the **first** broken query names the response. The files count is skipped entirely when
///    excluded; see [`files_count_unless_excluded`].
///
/// # The handler never fetches the channel — but the gate does
///
/// No `GetChannel` runs here, so nothing 404s. That does **not** make a missing channel a 200 of
/// zeroes: `SessionHasPermissionToChannel`'s own channel fetch sits above every grant branch,
/// the admin's `manage_system` included, so a well-formed id that matches nothing is a **403**
/// from both servers — measured, after a first draft of the parity suite asserted the
/// 200-of-zeroes and both servers refused. The store's zero counts are reachable only from
/// tests. The `channel_id` in a successful body is the caller's own path segment echoed back,
/// not a row's column.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(stats)` — trailing newline ([D-086]). Five keys, no `omitempty`,
/// `pinnedpost_count` without its middle underscore — the fixture-pinned serialisation in
/// `mm-model/src/channel_stats.rs`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
pub async fn get_channel_stats(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let exclude_files_count = query_flag_is_true(query.as_deref(), EXCLUDE_FILES_COUNT_PARAM);

    require_id(&channel_id, "channel_id")?;

    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    if !allowed {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL],
        )));
    }

    let member_count = state.app.get_channel_member_count(&channel_id).await?;
    let guest_count = state.app.get_channel_guest_count(&channel_id).await?;
    let pinned_post_count = state.app.get_channel_pinned_post_count(&channel_id).await?;
    let files_count = files_count_unless_excluded(exclude_files_count, || async {
        state.app.get_channel_file_count(&channel_id).await
    })
    .await?;

    let stats = mm_model::channel_stats::ChannelStats {
        channel_id,
        member_count,
        guest_count,
        pinned_post_count,
        files_count,
    };

    let mut body = serde_json::to_vec(&stats).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise ChannelStats");
        ApiError(mm_model::utils::AppError::new(
            "getChannelStats",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// Port of `getChannelMembers` (api4/channel.go:1865), reached as
/// `GET /api/v4/channels/{channel_id}/members` — the first paginated route.
///
/// # Order of operations
///
/// 1. `RequireChannelId` — charset already handled by [`crate::partially_migrated_with_ids`].
/// 2. One gate: `SessionHasPermissionToChannel(read_channel)`, the `getChannelStats` shape —
///    which also means a missing channel dies here as a 403 (the gate's own fetch misses), and
///    the empty list a missing channel would produce is unreachable over REST.
/// 3. `GetChannelMembersPage(page, per_page)` — the parser never 400s (garbage falls to
///    defaults) and `per_page=0` means **every member**; see [`parse_per_page`].
/// 4. `SanitizeForCurrentUser` over every element — the two timestamps blank to `-1` on
///    everyone's row but the caller's own, which stays intact *in the middle of the list*.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(members)` — an array plus the trailing newline ([D-086]). An
/// empty page (offset past the end) is `[]`, never `null`. The list order is the store's heap
/// order — Go adds no `ORDER BY` — which both servers share because they share the table; it is
/// not a wire guarantee, and the parity suite treats it as one only because the fixtures are
/// quiescent.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, page, per_page))]
pub async fn get_channel_members(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    require_id(&channel_id, "channel_id")?;

    let (allowed, _is_member) = state
        .app
        .session_has_permission_to_channel(&session.0, &channel_id, &PERMISSION_READ_CHANNEL)
        .await;
    if !allowed {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_READ_CHANNEL],
        )));
    }

    let mut members = state
        .app
        .get_channel_members_page(&channel_id, page, per_page)
        .await?;

    for member in &mut members {
        member.sanitize_for_current_user(&session.0.user_id);
    }

    let mut body = serde_json::to_vec(&members).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the member list");
        ApiError(mm_model::utils::AppError::new(
            "getChannelMembers",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// Go's channel-name path class: `{channel_name:[A-Za-z0-9_-]+}` (api.go:224) — the id class
/// plus `_` and `-`, **without** the `.` the username class allows. A segment outside it never
/// matches Go's route and falls to the mux 404, so it is forwarded rather than answered
/// ([D-150] under a third alphabet).
fn segment_matches_channel_name_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `include_deleted`, read by `web.ParamsFromRequest` (params.go:304) with the same
/// `strconv.ParseBool`-error-is-false idiom as the other flags.
const INCLUDE_DELETED_PARAM: &str = "include_deleted";

/// How [`get_channel_by_name`] refuses — and the two branches refuse **differently**, which is
/// the reason this is an enum rather than a bool like [`channel_read_denied`].
#[derive(Debug, PartialEq, Eq)]
enum ByNameRefusal {
    /// The open-channel branch: `SetPermissionError(PermissionReadPublicChannel)` — note the
    /// permission **named** is `read_public_channel`, where `getChannel`'s same branch names
    /// `read_channel`. Two handlers, same gates, different error detail.
    Forbidden,
    /// The non-open branch: a **404** wearing the store's `get_by_name.missing` id, so a
    /// non-member cannot tell a private channel from one that does not exist. Go builds it
    /// inline in the handler (`where = "getChannelByName"`), not in the app layer.
    NotFound,
}

/// Go's permission block for [`get_channel_by_name`] (api4/channel.go:1792-1811), both gates
/// lazy so the order — invisible over HTTP — is testable in-process.
///
/// Both branches have the same shape, *team gate then channel gate*, and differ in two things a
/// reader could plausibly swap:
///
/// - **Which team permission admits.** An open channel asks `read_public_channel`; a non-open
///   one asks **`manage_team`** — the comment in Go says why: "allows team admins to access
///   private channel". That is wider than `getChannel`, which never consults the team for a
///   private channel at all. The closure receives the permission so a test can see which was
///   asked.
/// - **How a refusal reads.** Open → 403; non-open → 404 (see [`ByNameRefusal`]).
///
/// Go's non-open branch then tries `serveDiscoverableNonMember` before the 404. Pinned off as in
/// `getChannel` — the `DiscoverableChannels` flag is false at the pinned SHA, [D-153].
async fn channel_by_name_refusal<TF, TFut, CF, CFut>(
    channel_is_open: bool,
    team_allowed: TF,
    channel_allowed: CF,
) -> Option<ByNameRefusal>
where
    TF: FnOnce(&'static Permission) -> TFut,
    TFut: std::future::Future<Output = bool>,
    CF: FnOnce() -> CFut,
    CFut: std::future::Future<Output = bool>,
{
    let team_permission = if channel_is_open {
        &PERMISSION_READ_PUBLIC_CHANNEL
    } else {
        &PERMISSION_MANAGE_TEAM
    };
    if team_allowed(team_permission).await {
        return None;
    }
    if channel_allowed().await {
        return None;
    }
    Some(if channel_is_open {
        ByNameRefusal::Forbidden
    } else {
        ByNameRefusal::NotFound
    })
}

/// The response for a [`ByNameRefusal`], split out so the permission and error ids are pinned
/// where a unit test can read them ([D-092]: neither reaches the wire).
fn channel_by_name_denial(
    refusal: ByNameRefusal,
    session: &mm_model::session::Session,
    channel: &mm_model::channel::Channel,
) -> ApiError {
    match refusal {
        ByNameRefusal::Forbidden => ApiError(*make_permission_error(
            session,
            &[&PERMISSION_READ_PUBLIC_CHANNEL],
        )),
        ByNameRefusal::NotFound => ApiError(mm_model::utils::AppError::new(
            "getChannelByName",
            "app.channel.get_by_name.missing.app_error",
            None,
            format!("teamId={}, name={}", channel.team_id, channel.name),
            404,
        )),
    }
}

/// Port of `getChannelByName` (api4/channel.go:1779), reached as
/// `GET /api/v4/teams/{team_id}/channels/name/{channel_name}`.
///
/// # Order of operations
///
/// 1. **The name segment's mux charset**, `[A-Za-z0-9_-]+` — not id-shaped, so
///    [`crate::partially_migrated_with_ids`] does not cover it; a miss is forwarded. The
///    `team_id` segment *is* covered there.
/// 2. **The name is lower-cased before anything looks at it** — `params.ChannelName =
///    strings.ToLower(...)` (params.go:179). So `/channels/name/Town-Square` is `town-square`
///    and answers 200, and the validator below never sees an uppercase letter.
/// 3. `RequireTeamId().RequireChannelName()` — team first. `RequireChannelName` is
///    `IsValidChannelIdentifier`, and like every `Require*` it answers `invalid_url_param`.
/// 4. `?include_deleted` chooses the store variant; it is read **after** validation in Go, but
///    parsing cannot fail so the order is unobservable.
/// 5. The fetch, then the two-branch permission block — see [`channel_by_name_refusal`].
/// 6. `FillInChannelProps`, then `json.NewEncoder(w).Encode` — trailing newline ([D-086]).
///
/// # The team in the path is not necessarily the team in the body
///
/// The store's filter is `TeamId = ? OR TeamId = ''`, so a DM or GM answers under any team's
/// path. The permission block then asks about `channel.TeamId` — the empty string — and
/// `SessionHasPermissionToTeam` on `""` denies for everyone but a system admin, so a DM falls
/// through to its membership gate. Correct, and Go's.
#[tracing::instrument(skip_all, fields(team_id = %team_id, channel_name = %channel_name, forwarded))]
pub async fn get_channel_by_name(
    State(state): State<AppState>,
    Path((team_id, channel_name)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !segment_matches_channel_name_mux(&channel_name) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let channel_name = channel_name.to_lowercase();

    if let Err(err) = require_id(&team_id, "team_id") {
        return err.into_response();
    }
    if !is_valid_channel_identifier(&channel_name) {
        return ApiError::invalid_url_param("channel_name").into_response();
    }

    let include_deleted = query_flag_is_true(query.as_deref(), INCLUDE_DELETED_PARAM);

    let mut channel = match state
        .app
        .get_channel_by_name(&channel_name, &team_id, include_deleted)
        .await
    {
        Ok(channel) => channel,
        Err(err) => return ApiError(err).into_response(),
    };

    let refusal = channel_by_name_refusal(
        channel.channel_type == CHANNEL_TYPE_OPEN,
        |permission| async {
            state
                .app
                .session_has_permission_to_team(&session.0, &channel.team_id, permission)
                .await
        },
        || async {
            let (allowed, _is_member) = state
                .app
                .session_has_permission_to_channel(
                    &session.0,
                    &channel.id,
                    &PERMISSION_READ_CHANNEL,
                )
                .await;
            allowed
        },
    )
    .await;

    if let Some(refusal) = refusal {
        return channel_by_name_denial(refusal, &session.0, &channel).into_response();
    }

    if let Err(err) = state.app.fill_in_channel_props(&mut channel).await {
        return ApiError(err).into_response();
    }

    let mut body = match serde_json::to_vec(&channel) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise Channel");
            return ApiError(mm_model::utils::AppError::new(
                "getChannelByName",
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');

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

/// Go's `c.RequireUserId().RequireTeamId()` for [`get_channels_for_team_for_user`] — **user
/// first**, the opposite of [`validate_ids`]'s channel-first chain, and pinned for the same
/// reason: the parameter name is not on the wire ([D-092]).
#[allow(clippy::result_large_err)]
fn validate_user_then_team(user_id: &str, team_id: &str) -> Result<(), ApiError> {
    require_id(user_id, "user_id")?;
    require_id(team_id, "team_id")?;
    Ok(())
}

/// The two gates of [`get_channels_for_team_for_user`] (api4/channel.go:1390-1398), in order
/// and with the short circuit — [`first_denied_permission`]'s shape with a **team** gate second:
/// `SessionHasPermissionToUser` names `edit_other_users`, then `SessionHasPermissionToTeam`
/// with `view_team` names `view_team`. Both answer the same 403 over HTTP.
async fn channels_for_team_denied<F, Fut>(
    user_allowed: bool,
    team_allowed: F,
) -> Option<&'static Permission>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    if !user_allowed {
        return Some(&PERMISSION_EDIT_OTHER_USERS);
    }
    if !team_allowed().await {
        return Some(&PERMISSION_VIEW_TEAM);
    }
    None
}

/// Go's `last_delete_at` parsing (api4/channel.go:1401-1408): `strconv.Atoi` with the error
/// swallowed to `0` — so absent, empty and garbage are all zero — and then **a negative value is
/// a 400** (`invalid_url_param`, `last_delete_at`). The one pagination-style parameter in the
/// ported routes that can fail a request; `page` and `per_page` never do.
///
/// `Atoi` accepts a leading `+` or `-` and nothing else: no whitespace, no underscores, and an
/// out-of-range value is an error (→ 0), all of which `i64::from_str` matches.
#[allow(clippy::result_large_err)]
fn parse_last_delete_at(query: Option<&str>) -> Result<i64, ApiError> {
    let last_delete_at = query_first(query, "last_delete_at")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    if last_delete_at < 0 {
        return Err(ApiError::invalid_url_param("last_delete_at"));
    }
    Ok(last_delete_at)
}

/// Port of `Context.HandleEtag` (web/context.go:230): an exact string comparison of the raw
/// `If-None-Match` header against the computed etag — no weak comparison, no candidate list —
/// and only when the etag is non-empty, which a list etag always is.
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    !etag.is_empty()
        && headers
            .get(IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|sent| sent == etag)
}

/// Port of `getChannelsForTeamForUser` (api4/channel.go:1384), reached as
/// `GET /api/v4/users/{user_id}/teams/{team_id}/channels` — the route the webapp calls on
/// every team load, so its order is the sidebar's order.
///
/// # Order of operations
///
/// 1. `me` resolves before validation (web/context.go:301).
/// 2. `RequireUserId().RequireTeamId()` — **user first**, see [`validate_user_then_team`].
/// 3. Two gates, user then team — see [`channels_for_team_denied`]. Both run **before** the
///    query string is parsed, so a refused caller sending `last_delete_at=-1` gets the 403, not
///    the 400.
/// 4. `last_delete_at` ([`parse_last_delete_at`]) and `include_deleted` (`ParseBool`, error is
///    false). Together they select the store's three deletion filters.
/// 5. `GetChannelsForTeamForUser` — and **zero channels is a 404**, not `[]`: the store answers
///    `ErrNotFound` for an empty result.
/// 6. **The etag is computed before `FillInChannelsProps`**, from the list as fetched, and a
///    matching `If-None-Match` is a 304 with the `ETag` header and no body. The props fill runs
///    only on a miss. `ChannelList.Etag` is the max over `LastPostAt`/`UpdateAt` plus the length
///    (`mm-model/src/channel_list.rs`), so a new post in any listed channel invalidates it.
/// 7. `ETag` header, then `json.NewEncoder(w).Encode` — trailing newline ([D-086]).
///
/// # Not ported
///
/// `HydrateChannelsPolicyActions` — a no-op for a list with no policy-enforced channel, which
/// is every list this deployment can produce ([D-141]).
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_channels_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    validate_user_then_team(&user_id, &team_id)?;

    let user_allowed = state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await;
    let denied = channels_for_team_denied(user_allowed, || async {
        state
            .app
            .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
            .await
    })
    .await;
    if let Some(permission) = denied {
        return Err(ApiError(*make_permission_error(&session.0, &[permission])));
    }

    let last_delete_at = parse_last_delete_at(query.as_deref())?;
    let include_deleted = query_flag_is_true(query.as_deref(), INCLUDE_DELETED_PARAM);

    let opts = ChannelSearchOpts {
        include_deleted,
        last_delete_at,
        ..Default::default()
    };
    let mut channels = state
        .app
        .get_channels_for_team_for_user(&team_id, &user_id, &opts)
        .await?;
    tracing::Span::current().record("count", channels.0.len());

    let etag = channels.etag();
    if etag_matches(&headers, &etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [("ETag", etag.as_str()), ("x-mmrs-served-by", "rust")],
        )
            .into_response());
    }

    state.app.fill_in_channels_props(&mut channels.0).await?;

    let mut body = serde_json::to_vec(&channels).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the channel list");
        ApiError(mm_model::utils::AppError::new(
            "getChannelsForTeamForUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
            ("ETag", etag.as_str()),
        ],
        body,
    )
        .into_response())
}

/// Go's `pageSize` in `getChannelsForUser` (api4/channel.go:1454): the keyset page the handler
/// walks. Not on the wire directly — the client always gets the whole list — but the **byte**
/// layout is: a `,` lands between pages, and a total that is an exact multiple of this ends the
/// loop on a `not_found` rather than on a short page.
const CHANNELS_FOR_USER_PAGE_SIZE: i64 = 100;

/// The error id on which `getChannelsForUser`'s page loop ends (api4/channel.go:1471): the
/// store's `ErrNotFound` for an empty page, which [`mm_app::App::get_channels_for_user`] maps
/// to this id. Compared by **id**, as Go does, so a 404 from anywhere else would stop the loop
/// too — there is nowhere else for one to come from.
const CHANNELS_NOT_FOUND_ID: &str = "app.channel.get_channels.not_found.app_error";

/// Port of the body `getChannelsForUser` streams (api4/channel.go:1454-1513), byte for byte.
///
/// Go writes `[` before it has fetched anything, then for every page: a `,` if it is not the
/// first page, then each channel through `json.NewEncoder(w).Encode` — which appends a `\n` —
/// with a `,` between channels, and finally `]`. So two channels are
/// `[{…}\n,{…}\n]`, and the page boundary is invisible: `…}\n,{…` either way. **No newline
/// after `]`**, unlike the `Encode`d lists elsewhere ([D-086]).
///
/// The loop ends on a page shorter than [`CHANNELS_FOR_USER_PAGE_SIZE`], or on the store's
/// `not_found` for a page after a full one — which is how an exact multiple of the page size
/// terminates. **A user with no channels is that same `not_found` on the first page**, and
/// since the `[` and the `200` are already out, the wire carries `[` followed by the error
/// body and nothing else: a 200 whose body is not JSON. `mid_stream_error` is that path for
/// every error once the bracket is written — `Encode` failures included, which Go logs and
/// skips, leaving the separators as if the element had been written.
///
/// `fetch_page` is the app call, `(from_channel_id) -> Result<ChannelList, AppError>`, with
/// `FillInChannelsProps` folded in by the caller; the function owns only the byte layout.
async fn stream_channels_for_user<F, Fut>(mut fetch_page: F) -> Vec<u8>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<mm_model::channel_list::ChannelList, ApiError>>,
{
    let mut body = b"[".to_vec();
    let mut from_channel_id = String::new();
    loop {
        let mut channels = match fetch_page(from_channel_id.clone()).await {
            Ok(channels) => channels,
            Err(err) => {
                if !from_channel_id.is_empty() && err.0.id == CHANNELS_NOT_FOUND_ID {
                    break;
                }
                return mid_stream_error(body, err);
            }
        };

        // The intermediary comma between pages.
        if !from_channel_id.is_empty() {
            body.push(b',');
        }

        let count = channels.0.len();
        for (i, channel) in channels.0.iter().enumerate() {
            if let Err(err) = serde_json::to_writer(&mut body, channel) {
                tracing::warn!(error = %err, channel_id = %channel.id, "Error while writing response");
            }
            body.push(b'\n');
            if i + 1 < count {
                body.push(b',');
            }
        }

        if (count as i64) < CHANNELS_FOR_USER_PAGE_SIZE {
            break;
        }
        // `channels[len(channels)-1].Id`: the list is done with, so the id moves out.
        from_channel_id = channels.0.pop().map(|c| c.id).unwrap_or_default();
    }
    body.push(b']');
    body
}

/// What the wire carries when `getChannelsForUser` fails after its `[`: the bytes so far, then
/// Go's error body (`handleContextError`'s `c.Err.ToJSON()`), and no closing bracket — see
/// [`ApiError::into_wire`]. The status line is already `200`.
fn mid_stream_error(mut body: Vec<u8>, err: ApiError) -> Vec<u8> {
    let (_, error_body) = err.into_wire();
    body.extend(error_body.unwrap_or_default());
    body
}

/// Port of `getChannelsForUser` (api4/channel.go:1435), reached as
/// `GET /api/v4/users/{user_id}/channels` — the route the webapp calls on load, as
/// `/users/me/channels?include_deleted=…&last_delete_at=…`.
///
/// # Order of operations
///
/// 1. `me` resolves before validation (web/context.go:301), then `RequireUserId()`.
/// 2. One gate: `SessionHasPermissionToUser`, naming `edit_other_users`. No team gate — the list
///    spans every team — and it runs **before** the query string is parsed, so a refused caller
///    sending `last_delete_at=-1` gets the 403, not the 400.
/// 3. `last_delete_at` ([`parse_last_delete_at`]) and `include_deleted` (`ParseBool`, error is
///    false). Unlike the per-team sibling there is no `ChannelSearchOpts`; the values go to the
///    store as they are.
/// 4. **The response is streamed** — `200` and `[` are committed before the first page is
///    fetched, and every error after that point lands *inside* the body with the status
///    already sent. No etag. See [`stream_channels_for_user`] for the byte layout and the
///    zero-channel case, which is **not a 404** the way the sibling's is.
/// 5. Each page gets `FillInChannelsProps` before it is written, per page, so a header mention
///    resolves against that page's team groups only — the same result, since the lookup is by
///    team and name rather than by list position.
///
/// The body is assembled in memory rather than streamed: the bytes are identical, and a
/// `Content-Length` instead of chunked transfer is not something a client can observe through
/// its JSON parser.
///
/// # Not ported
///
/// `HydrateChannelsPolicyActions`, as in the sibling ([D-141]).
#[tracing::instrument(skip_all, fields(user_id = %user_id))]
pub async fn get_channels_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    require_id(&user_id, "user_id")?;

    if !state
        .app
        .session_has_permission_to_user(&session.0, &user_id)
        .await
    {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let last_delete_at = parse_last_delete_at(query.as_deref())?;
    let include_deleted = query_flag_is_true(query.as_deref(), INCLUDE_DELETED_PARAM);

    let app = &state.app;
    let user_id = &user_id;
    let body = stream_channels_for_user(|from_channel_id| async move {
        let mut channels = app
            .get_channels_for_user(
                user_id,
                include_deleted,
                last_delete_at,
                CHANNELS_FOR_USER_PAGE_SIZE,
                &from_channel_id,
            )
            .await?;
        app.fill_in_channels_props(&mut channels.0).await?;
        Ok(channels)
    })
    .await;

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

/// The two gates of [`get_channel_members_for_team_for_user`] (api4/channel.go:1984-1992),
/// **team first** — the reverse of [`channels_for_team_denied`], its sibling one path segment
/// up, which gates the user first. Then a self-shortcut by string comparison, and only a
/// non-self caller polls the second team check, which asks for `manage_system` *through the
/// team* (`SessionHasPermissionToTeam`): a team admin's `manage_team` does not admit them to
/// another member's memberships, and the error names `manage_system`, not `edit_other_users`.
async fn members_for_team_denied<T, TFut, M, MFut>(
    team_allowed: T,
    is_self: bool,
    manage_system_allowed: M,
) -> Option<&'static Permission>
where
    T: FnOnce() -> TFut,
    TFut: std::future::Future<Output = bool>,
    M: FnOnce() -> MFut,
    MFut: std::future::Future<Output = bool>,
{
    if !team_allowed().await {
        return Some(&PERMISSION_VIEW_TEAM);
    }
    if !is_self && !manage_system_allowed().await {
        return Some(&PERMISSION_MANAGE_SYSTEM);
    }
    None
}

/// Port of `getChannelMembersForTeamForUser` (api4/channel.go:1978), reached as
/// `GET /api/v4/users/{user_id}/teams/{team_id}/channels/members` — the request the webapp
/// sends right after [`get_channels_for_team_for_user`] on every team load.
///
/// # Order of operations
///
/// 1. `me` resolves before validation (web/context.go:301).
/// 2. `RequireUserId().RequireTeamId()` — user first, [`validate_user_then_team`].
/// 3. Two gates, **team then self-or-`manage_system`** — [`members_for_team_denied`].
/// 4. `GetChannelMembersForUser`: every membership in the team's channels plus the teamless
///    ones (DMs, GMs — and the sibling list's "DM in every team" rule holds here too), archived
///    channels **included**, in heap order. **Zero memberships is `[]`**, where the sibling
///    list is a 404 — the two routes disagree on the empty case.
/// 5. `SanitizeForCurrentUser` over every element. Every row is the *target* user's, so for a
///    self read nothing is blanked and for an admin reading someone else **every** row's
///    `last_viewed_at` and `last_update_at` is `-1`.
/// 6. `json.NewEncoder(w).Encode` — trailing newline ([D-086]). No etag on this route.
#[tracing::instrument(skip_all, fields(user_id = %user_id, team_id = %team_id, count))]
pub async fn get_channel_members_for_team_for_user(
    State(state): State<AppState>,
    Path((user_id, team_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = if user_id == ME {
        session.0.user_id.clone()
    } else {
        user_id
    };

    validate_user_then_team(&user_id, &team_id)?;

    let denied = members_for_team_denied(
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_VIEW_TEAM)
                .await
        },
        session.0.user_id == user_id,
        || async {
            state
                .app
                .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_MANAGE_SYSTEM)
                .await
        },
    )
    .await;
    if let Some(permission) = denied {
        return Err(ApiError(*make_permission_error(&session.0, &[permission])));
    }

    let mut members = state
        .app
        .get_channel_members_for_user(&team_id, &user_id)
        .await?;
    tracing::Span::current().record("count", members.len());

    for member in &mut members {
        member.sanitize_for_current_user(&session.0.user_id);
    }

    let mut body = serde_json::to_vec(&members).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the member list");
        ApiError(mm_model::utils::AppError::new(
            "getChannelMembersForTeamForUser",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

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

/// Go's `c.Params.Page * c.Params.PerPage`, computed in `int` — 64 bits on every platform this
/// deployment runs on — and therefore **wrapping** on overflow rather than saturating.
///
/// The distinction is on the wire. `page` and `per_page` are parsed with `strconv.Atoi`, which
/// happily accepts `9223372036854775807`; multiplied by a `per_page` above 1 that wraps to a
/// negative offset, which squirrel renders as `uint64(negative)` — a literal Postgres rejects as
/// out of range for `bigint`. The result is a 500 carrying the store's own error id, measured on
/// the running server. Saturating instead would answer `200 []` for the same request, and
/// clamping the page to something "sensible" would answer a *page of channels*.
///
/// This port binds the offset as a signed parameter, so Postgres refuses it as "OFFSET must not
/// be negative" instead of "out of range" — a different message behind the same 500 and the same
/// id, since `detailed_error` is wiped ([D-092]).
pub(crate) fn page_offset(page: i64, per_page: i64) -> i64 {
    page.wrapping_mul(per_page)
}

/// `json.NewEncoder(w).Encode(channels)` for the three team channel lists: the list, then a
/// newline ([D-086]).
///
/// Go logs an encode failure and leaves whatever it had already written on the wire; a
/// `model.Channel` cannot fail to marshal, so the 500 below is the unreachable branch that keeps
/// this crate free of `unwrap` rather than a behaviour claim.
#[allow(clippy::result_large_err)]
fn encoded_channel_list(
    where_: &'static str,
    channels: &mm_model::channel_list::ChannelList,
) -> Result<Vec<u8>, ApiError> {
    let mut body = serde_json::to_vec(channels).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the channel list");
        ApiError(mm_model::utils::AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');
    Ok(body)
}

/// The 200 the three team channel lists share: JSON, the served-by marker, body, no etag.
fn channel_list_response(body: Vec<u8>) -> Response {
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

/// Port of `getPublicChannelsForTeam` (api4/channel.go:1221), reached as
/// `GET /api/v4/teams/{team_id}/channels` — the "Browse channels" list.
///
/// # Order of operations
///
/// 1. `RequireTeamId()`. No `me` alias and no user parameter at all: this list is the team's, not
///    a member's, and nothing here consults the caller's memberships.
/// 2. **One gate, `list_team_channels`, asked through `SessionHasPermissionToTeam`.** Not
///    `view_team` — the constant its two siblings in this file use — and not `manage_system`,
///    which is what the `/private` route asks for one segment deeper. An ordinary team member
///    holds `list_team_channels`; a non-member does not, and gets a 403 (measured, both ways).
/// 3. `page * per_page` as the offset — see [`page_offset`] for what an overflowing page does.
/// 4. `GetPublicChannelsForTeam`, then `FillInChannelsProps`.
/// 5. `json.NewEncoder(w).Encode` — trailing newline ([D-086]). **No etag on this route**, unlike
///    the per-team-per-user list, so every request pays for the whole page.
///
/// # What "public" means here, and what it does not
///
/// The store joins `PublicChannels`, Go's denormalised shadow table, and filters on **its**
/// `TeamId` and `DeleteAt` — see [`mm_store::channel_store::get_public_channels_for_team`]. An
/// archived public channel is excluded (its shadow row survives with the new `DeleteAt`), and a
/// channel the caller is not a member of is **included**: this is the browse list, not the
/// sidebar.
#[tracing::instrument(skip_all, fields(team_id = %team_id, count))]
pub async fn get_public_channels_for_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&team_id, "team_id")?;

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_LIST_TEAM_CHANNELS)
        .await
    {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_LIST_TEAM_CHANNELS],
        )));
    }

    let per_page = parse_per_page(query.as_deref());
    let offset = page_offset(parse_page(query.as_deref()), per_page);

    let mut channels = state
        .app
        .get_public_channels_for_team(&team_id, offset, per_page)
        .await?;
    tracing::Span::current().record("count", channels.0.len());

    state.app.fill_in_channels_props(&mut channels.0).await?;

    Ok(channel_list_response(encoded_channel_list(
        "getPublicChannelsForTeam",
        &channels,
    )?))
}

/// Port of `getPrivateChannelsForTeam` (api4/channel.go:1301), reached as
/// `GET /api/v4/teams/{team_id}/channels/private`.
///
/// **Its gate is not its sibling's, and the difference is not a scope but a permission.** Go
/// calls `SessionHasPermissionTo(session, PermissionManageSystem)` — the *system* check, with no
/// team argument — so a team admin is refused here while passing
/// [`get_public_channels_for_team`] one segment up. Copying the sibling's
/// `session_has_permission_to_team(team_id, list_team_channels)` would hand every member of the
/// team a list of its private channels, which is the one thing this route exists to withhold.
/// The refusal names `manage_system`; a plain team member gets a 403 (measured).
///
/// Everything after the gate is the sibling's: offset paging, `FillInChannelsProps`, an
/// `Encode`d list with a trailing newline, no etag.
#[tracing::instrument(skip_all, fields(team_id = %team_id, count))]
pub async fn get_private_channels_for_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&team_id, "team_id")?;

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let per_page = parse_per_page(query.as_deref());
    let offset = page_offset(parse_page(query.as_deref()), per_page);

    let mut channels = state
        .app
        .get_private_channels_for_team(&team_id, offset, per_page)
        .await?;
    tracing::Span::current().record("count", channels.0.len());

    state.app.fill_in_channels_props(&mut channels.0).await?;

    Ok(channel_list_response(encoded_channel_list(
        "getPrivateChannelsForTeam",
        &channels,
    )?))
}

/// Port of `getDeletedChannelsForTeam` (api4/channel.go:1272), reached as
/// `GET /api/v4/teams/{team_id}/channels/deleted`.
///
/// # Two permissions, and only one of them can refuse
///
/// The gate is `list_team_channels` on the team, exactly as [`get_public_channels_for_team`].
/// `manage_system` is then asked **as a question, not as a gate**: its answer becomes
/// `skipTeamMembershipCheck`, which widens what the store returns rather than deciding whether
/// anything is returned at all. Turning that second check into a second gate would 403 every
/// ordinary member; dropping it would hide a system admin's archived DMs and private channels.
/// Both halves are measured against Go, which needs two actors — the fixture admin can only ever
/// see the wide answer.
///
/// The order matters for a reason the wire cannot show: Go asks `SessionHasPermissionTo` only
/// after the team gate has passed, so a refused caller never pays for the system-role lookup.
/// Pinned in-process, like [`channel_read_denied`] and friends.
#[tracing::instrument(skip_all, fields(team_id = %team_id, count))]
pub async fn get_deleted_channels_for_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    require_id(&team_id, "team_id")?;

    if !state
        .app
        .session_has_permission_to_team(&session.0, &team_id, &PERMISSION_LIST_TEAM_CHANNELS)
        .await
    {
        return Err(ApiError(*make_permission_error(
            &session.0,
            &[&PERMISSION_LIST_TEAM_CHANNELS],
        )));
    }

    let skip_team_membership_check = state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await;

    let per_page = parse_per_page(query.as_deref());
    let offset = page_offset(parse_page(query.as_deref()), per_page);

    let mut channels = state
        .app
        .get_deleted_channels(
            &team_id,
            offset,
            per_page,
            &session.0.user_id,
            skip_team_membership_check,
        )
        .await?;
    tracing::Span::current().record("count", channels.0.len());

    state.app.fill_in_channels_props(&mut channels.0).await?;

    Ok(channel_list_response(encoded_channel_list(
        "getDeletedChannelsForTeam",
        &channels,
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::channel_member::ChannelMember;

    const ME_ID: &str = "y9i4er48tt8bukijy7i3u5y9ar";
    const OTHER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn member(user_id: &str) -> ChannelMember {
        ChannelMember {
            channel_id: "dpn4orkqniyzurpjzw6w6qxg8y".to_owned(),
            user_id: user_id.to_owned(),
            roles: "channel_user".to_owned(),
            last_viewed_at: 1_755_000_000_000,
            last_update_at: 1_755_000_000_001,
            scheme_user: true,
            ..Default::default()
        }
    }

    /// An id is 26 characters of the id alphabet; anything else is a 400 naming the parameter.
    #[test]
    fn require_id_rejects_everything_that_is_not_an_id() {
        assert!(require_id(ME_ID, "channel_id").is_ok());

        for bad in ["", "me", "short", &"x".repeat(27), &"x".repeat(25)] {
            let err = require_id(bad, "channel_id").expect_err("not a valid id");
            assert_eq!(err.0.status_code, 400);
            assert_eq!(err.0.id, "api.context.invalid_url_param.app_error");
            assert_eq!(
                err.0
                    .params
                    .as_ref()
                    .and_then(|p| p.get("Name"))
                    .and_then(serde_json::Value::as_str),
                Some("channel_id"),
                "the parameter name is what the client is told to fix"
            );
        }
    }

    /// **The channel id is validated first**, so a request with both segments malformed reports
    /// `channel_id`. Go gets this from chaining `RequireChannelId().RequireUserId()`, each
    /// returning early once an error is set.
    ///
    /// Asserted here rather than against the running server because the parameter name is not on
    /// the wire: `AppError` does not serialise `params`, and Go only reveals which one failed
    /// through its translated `message` ([D-092]). A cross-server test cannot see the difference.
    #[test]
    fn the_channel_id_is_validated_before_the_user_id() {
        let both_bad = validate_ids("nope", "alsonope").expect_err("both are invalid");
        assert_eq!(
            both_bad
                .0
                .params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(serde_json::Value::as_str),
            Some("channel_id"),
            "the channel is checked first, so it is the one reported"
        );

        // And with only the user id malformed, it is the user id that is reported — otherwise the
        // assertion above would pass for a function that always says `channel_id`.
        let user_bad = validate_ids(ME_ID, "alsonope").expect_err("the user id is invalid");
        assert_eq!(
            user_bad
                .0
                .params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(serde_json::Value::as_str),
            Some("user_id")
        );

        assert!(validate_ids(ME_ID, OTHER).is_ok(), "two valid ids pass");
    }

    /// The url-param id is **not** the body-param id, and they differ by one word. A client
    /// branching on `id` sees a different error for a bad path segment than for a bad body.
    #[test]
    fn the_url_param_error_is_distinct_from_the_body_param_error() {
        let url = require_id("nope", "user_id").expect_err("invalid");
        let body = ApiError::invalid_param("user_id");
        assert_eq!(url.0.id, "api.context.invalid_url_param.app_error");
        assert_eq!(body.0.id, "api.context.invalid_body_param.app_error");
        assert_ne!(url.0.id, body.0.id);
    }

    /// `SanitizeForCurrentUser` uses **-1**, not 0, and only for someone else's row.
    #[test]
    fn the_sanitiser_blanks_only_another_users_timestamps() {
        let mut mine = member(ME_ID);
        let before = mine.clone();
        mine.sanitize_for_current_user(ME_ID);
        assert_eq!(mine, before, "one's own row is untouched");

        let mut theirs = member(OTHER);
        theirs.sanitize_for_current_user(ME_ID);
        assert_eq!(theirs.last_viewed_at, -1);
        assert_eq!(theirs.last_update_at, -1);
        assert_eq!(
            theirs.roles, "channel_user",
            "roles are not sanitised here — only the two timestamps are"
        );
    }

    /// This handler writes with an encoder, so the body ends in a newline. The three routes
    /// already migrated split two ways on this and the difference is one byte ([D-086]).
    #[test]
    fn the_body_ends_in_a_newline() {
        let mut body = serde_json::to_vec(&member(ME_ID)).expect("serialises");
        body.push(b'\n');
        assert_eq!(body.last(), Some(&b'\n'));
    }

    /// The user gate runs first and reports **`edit_other_users`**, and the channel gate is
    /// **not evaluated** when it denies. All three facts are invisible over HTTP — both gates
    /// answer the same 403 body — so this is where they live.
    #[tokio::test]
    async fn the_user_gate_runs_first_and_short_circuits_the_channel_gate() {
        let polled = std::cell::Cell::new(false);

        let denied = first_denied_permission(false, || async {
            polled.set(true);
            true
        })
        .await;

        assert_eq!(
            denied.map(|p| p.id.as_ref()),
            Some("edit_other_users"),
            "the user gate is first and names its own permission"
        );
        assert!(
            !polled.get(),
            "Go returns before SessionHasPermissionToChannel, which is a database read"
        );
    }

    /// Past the user gate, a channel refusal reports `read_channel` — not the permission the
    /// first gate names. A mutation copying `PERMISSION_EDIT_OTHER_USERS` into both branches is
    /// caught only here.
    #[tokio::test]
    async fn the_channel_gate_reports_read_channel() {
        let denied = first_denied_permission(true, || async { false }).await;
        assert_eq!(denied.map(|p| p.id.as_ref()), Some("read_channel"));
    }

    /// Both gates granting is the only path to a body.
    #[tokio::test]
    async fn two_grants_deny_nothing() {
        assert!(
            first_denied_permission(true, || async { true })
                .await
                .is_none(),
            "nothing is denied when both gates pass"
        );
    }

    /// `ChannelUnread` puts `json:"-"` on `NotifyProps`, so the body is exactly seven keys
    /// whatever the store loaded — and the two the app layer may zero are among them, so a client
    /// sees `0` rather than a missing key for a muted channel.
    #[test]
    fn the_unread_body_is_seven_keys_and_never_carries_notify_props() {
        let mut props = mm_model::utils::StringMap::new();
        props.insert("mark_unread".to_owned(), "mention".to_owned());
        let unread = mm_model::channel_member::ChannelUnread {
            team_id: "tttttttttttttttttttttttttt".to_owned(),
            channel_id: "cccccccccccccccccccccccccc".to_owned(),
            msg_count: 0,
            msg_count_root: 0,
            mention_count: 5,
            mention_count_root: 4,
            urgent_mention_count: 3,
            notify_props: Some(props),
        };

        let value: serde_json::Value = serde_json::to_value(&unread).expect("serialises");
        let object = value.as_object().expect("an object");
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "channel_id",
                "team_id",
                "mention_count",
                "mention_count_root",
                "msg_count",
                "msg_count_root",
                "urgent_mention_count",
            ]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
        );
        assert!(
            !object.contains_key("notify_props"),
            "json:\"-\" — the store loads it, the client never sees it"
        );
        assert_eq!(object["msg_count"], serde_json::json!(0));
    }

    /// This handler encodes too, so its body ends in a newline like `getChannelMember`'s.
    #[test]
    fn the_unread_body_ends_in_a_newline() {
        let mut body = serde_json::to_vec(&mm_model::channel_member::ChannelUnread::default())
            .expect("serialises");
        body.push(b'\n');
        assert_eq!(body.last(), Some(&b'\n'));
    }

    /// The permission error is 403 and carries Go's id; the detail names the user and permission.
    #[test]
    fn the_permission_error_is_gos_forbidden() {
        let session = mm_model::session::Session {
            user_id: ME_ID.to_owned(),
            ..Default::default()
        };
        let err = make_permission_error(&session, &[&PERMISSION_READ_CHANNEL]);
        assert_eq!(err.status_code, 403);
        assert_eq!(err.id, "api.context.permissions.app_error");
        assert_eq!(
            err.detailed_error,
            format!("userId={ME_ID}, permission=read_channel")
        );
    }

    /// `strconv.ParseBool`'s accepted set decides the forward, and its error case is `false` —
    /// so `yes`, an empty value, and a bare key all serve locally, while `1`, `t` and `True`
    /// forward. `url.Values.Get` takes the **first** value of a repeated key, and it decodes
    /// percent-escapes before `ParseBool` sees the string.
    #[test]
    fn the_content_reviewer_forward_parses_the_query_like_go() {
        for (query, expected) in [
            (Some("as_content_reviewer=true"), true),
            (Some("as_content_reviewer=1"), true),
            (Some("as_content_reviewer=t"), true),
            (Some("as_content_reviewer=True"), true),
            (Some("as_content_reviewer=TRUE"), true),
            (Some("as_content_reviewer=%74rue"), true), // decoded before ParseBool
            (Some("as_content_reviewer=false"), false),
            (Some("as_content_reviewer=0"), false),
            (Some("as_content_reviewer=yes"), false), // ParseBool error → false
            (Some("as_content_reviewer="), false),
            (Some("as_content_reviewer"), false), // bare key: value is ""
            (
                Some("as_content_reviewer=true&as_content_reviewer=false"),
                true,
            ), // first wins
            (
                Some("as_content_reviewer=false&as_content_reviewer=true"),
                false,
            ),
            (Some("other=true"), false),
            (Some("AS_CONTENT_REVIEWER=true"), false), // keys are case-sensitive
            (Some(""), false),
            (None, false),
        ] {
            assert_eq!(
                is_content_reviewer_request(query),
                expected,
                "query {query:?}"
            );
        }
    }

    /// `exclude_files_count` wears the same `ParseBool` semantics as the reviewer flag — they
    /// share [`query_flag_is_true`] — but under its own key, so a mutation hardcoding either
    /// key into the shared helper dies here or in the reviewer table.
    #[test]
    fn the_exclude_files_count_flag_parses_the_query_like_go() {
        for (query, expected) in [
            (Some("exclude_files_count=true"), true),
            (Some("exclude_files_count=1"), true),
            (Some("exclude_files_count=yes"), false), // ParseBool error → false
            (Some("exclude_files_count="), false),
            (Some("exclude_files_count"), false),
            (Some("as_content_reviewer=true"), false), // the *other* flag must not trip this one
            (Some("EXCLUDE_FILES_COUNT=true"), false),
            (
                Some("exclude_files_count=false&exclude_files_count=true"),
                false, // first value wins
            ),
            (Some(""), false),
            (None, false),
        ] {
            assert_eq!(
                query_flag_is_true(query, EXCLUDE_FILES_COUNT_PARAM),
                expected,
                "query {query:?}"
            );
        }
    }

    /// `page`: Atoi failure or a negative value falls to 0 — never a 400. Positive values pass
    /// through unclamped; there is no page maximum.
    #[test]
    fn page_parses_like_gos_params_middleware() {
        for (query, expected) in [
            (Some("page=0"), 0),
            (Some("page=3"), 3),
            (Some("page=100000"), 100_000),
            (Some("page=-1"), 0),
            (Some("page=-999"), 0),
            (Some("page=abc"), 0),
            (Some("page=1.5"), 0),
            (Some("page="), 0),
            (Some("page=1&page=9"), 1), // first value wins
            (Some("per_page=5"), 0),    // the other key must not bleed in
            (Some(""), 0),
            (None, 0),
        ] {
            assert_eq!(parse_page(query), expected, "query {query:?}");
        }
    }

    /// `per_page`: failure or negative → 60, above 200 → 200 — and **zero is neither**, so it
    /// survives to the store where `Limit > 0` turns it into "no limit". The parser must not
    /// helpfully treat 0 as the default, or `?per_page=0` stops meaning "everything".
    #[test]
    fn per_page_parses_like_gos_params_middleware() {
        for (query, expected) in [
            (Some("per_page=60"), 60),
            (Some("per_page=1"), 1),
            (Some("per_page=200"), 200),
            (Some("per_page=201"), 200),
            (Some("per_page=99999"), 200),
            (Some("per_page=0"), 0), // zero passes through — the store reads it as unlimited
            (Some("per_page=-1"), 60),
            (Some("per_page=abc"), 60),
            (Some("per_page="), 60),
            (Some("page=7"), 60),
            (None, 60),
        ] {
            assert_eq!(parse_per_page(query), expected, "query {query:?}");
        }
    }

    /// Excluded means `-1` **and no query**: Go initialises the sentinel and only overwrites it
    /// inside `if !excludeFilesCountBool`, so the `FileInfo` count never runs. Both halves are
    /// invisible over HTTP against a channel with no files — `-1` versus `0` shows, but "did a
    /// query run" does not — hence the closure and this test.
    #[tokio::test]
    async fn excluding_files_answers_minus_one_and_never_polls_the_count() {
        let count = files_count_unless_excluded(true, || async {
            panic!("the files count must not run when excluded")
        })
        .await
        .expect("the sentinel is not an error");
        assert_eq!(count, -1, "-1 is the wire value, not a placeholder");
    }

    /// Not excluded: the fetch's answer — and its **error** — pass through untouched.
    #[tokio::test]
    async fn not_excluding_files_serves_the_fetched_count_and_its_error() {
        let count = files_count_unless_excluded(false, || async { Ok(7) }).await;
        assert_eq!(count.expect("fetch succeeded"), 7);

        let err = files_count_unless_excluded(false, || async {
            Err(mm_model::utils::AppError::new(
                "SqlChannelStore.GetFileCount",
                "app.channel.get_file_count.app_error",
                None,
                String::new(),
                500,
            ))
        })
        .await
        .expect_err("the fetch failed");
        assert_eq!(err.id, "app.channel.get_file_count.app_error");
    }

    /// The stats body is the fixture-pinned five keys, and this handler encodes, so it ends in a
    /// newline ([D-086]).
    #[test]
    fn the_stats_body_ends_in_a_newline() {
        let mut body = serde_json::to_vec(&mm_model::channel_stats::ChannelStats::default())
            .expect("serialises");
        body.push(b'\n');
        assert_eq!(body.last(), Some(&b'\n'));
    }

    /// An open channel asks the team first, and a team grant means the channel gate — a database
    /// read — is never polled. Like `first_denied_permission`, the order is invisible over HTTP
    /// (both denials answer the same 403), so it is pinned here.
    #[tokio::test]
    async fn an_open_channel_team_grant_never_polls_the_channel_gate() {
        let denied = channel_read_denied(
            true,
            || async { true },
            || async { panic!("the channel gate must not run when the team grants") },
        )
        .await;
        assert!(!denied);
    }

    /// Team denies, channel gate decides — in both directions.
    #[tokio::test]
    async fn an_open_channel_falls_from_team_to_channel_gate() {
        assert!(!channel_read_denied(true, || async { false }, || async { true }).await);
        assert!(channel_read_denied(true, || async { false }, || async { false }).await);
    }

    /// Both denial branches answer with `read_channel` — the detail is wiped on the wire
    /// ([D-092]), so this is only checkable here.
    #[test]
    fn the_get_channel_denial_names_read_channel() {
        let session = mm_model::session::Session {
            user_id: ME_ID.to_owned(),
            ..Default::default()
        };
        let denial = get_channel_denial(&session);
        assert_eq!(denial.0.status_code, 403);
        assert_eq!(denial.0.id, "api.context.permissions.app_error");
        assert_eq!(
            denial.0.detailed_error,
            format!("userId={ME_ID}, permission=read_channel"),
            "read_public_channel must never be the permission an error names"
        );
    }

    /// Go's channel-name class is the id class plus `_` and `-` — and **not** `.`, which the
    /// username class has. A dot, a space, `%` or an empty segment fall to the mux 404 forward.
    #[test]
    fn the_channel_name_charset_is_gos_mux_class() {
        for ok in ["town-square", "off_topic", "ABC123", "a", "-", "_"] {
            assert!(segment_matches_channel_name_mux(ok), "{ok:?} matches");
        }
        for bad in ["", "a.b", "a b", "a%20b", "ä", "a/b", "a~b"] {
            assert!(
                !segment_matches_channel_name_mux(bad),
                "{bad:?} must forward"
            );
        }
    }

    /// `RequireChannelName` is `IsValidChannelIdentifier` — and the handler lower-cases first,
    /// so a mixed-case segment is valid *after* folding even though the validator is
    /// lowercase-only. `_` alone and `-` alone pass the mux class and fail here with
    /// `invalid_url_param`.
    #[test]
    fn channel_name_validation_runs_on_the_lowercased_segment() {
        assert!(is_valid_channel_identifier(&"Town-Square".to_lowercase()));
        assert!(!is_valid_channel_identifier("Town-Square"));
        for bad in ["-", "_", "-leading"] {
            assert!(
                !is_valid_channel_identifier(bad),
                "{bad:?} is not a channel name"
            );
        }
        // The regex's tail is `[a-z0-9]*` — a trailing hyphen is *valid*. Transcribed from Go's
        // `validSimpleAlphaNum` and pinned so a "tidier" regex cannot land.
        assert!(is_valid_channel_identifier("trailing-"));
        let err = ApiError::invalid_url_param("channel_name");
        assert_eq!(err.0.id, "api.context.invalid_url_param.app_error");
        assert_eq!(err.0.status_code, 400);
    }

    /// An open channel asks the team for **`read_public_channel`**; a team grant never polls the
    /// channel gate.
    #[tokio::test]
    async fn by_name_open_channel_asks_read_public_channel_first() {
        let asked = std::cell::RefCell::new(None);
        let refusal = channel_by_name_refusal(
            true,
            |permission| {
                *asked.borrow_mut() = Some(permission.id.to_string());
                async { true }
            },
            || async { panic!("the channel gate must not run when the team grants") },
        )
        .await;
        assert_eq!(refusal, None);
        assert_eq!(asked.borrow().as_deref(), Some("read_public_channel"));
    }

    /// A non-open channel asks the team for **`manage_team`** — "allows team admins to access
    /// private channel" — which `getChannel` never does. Swapping the two permissions, or
    /// skipping the team gate for private channels as `getChannel` does, dies here.
    #[tokio::test]
    async fn by_name_private_channel_asks_manage_team_first() {
        let asked = std::cell::RefCell::new(None);
        let refusal = channel_by_name_refusal(
            false,
            |permission| {
                *asked.borrow_mut() = Some(permission.id.to_string());
                async { true }
            },
            || async { panic!("a team admin is admitted without the channel gate") },
        )
        .await;
        assert_eq!(refusal, None);
        assert_eq!(asked.borrow().as_deref(), Some("manage_team"));
    }

    /// Team denies, channel decides — and the refusal **shape** follows the channel type: 403
    /// for open, 404 for everything else.
    #[tokio::test]
    async fn by_name_refusals_differ_by_channel_type() {
        assert_eq!(
            channel_by_name_refusal(true, |_| async { false }, || async { true }).await,
            None
        );
        assert_eq!(
            channel_by_name_refusal(true, |_| async { false }, || async { false }).await,
            Some(ByNameRefusal::Forbidden)
        );
        assert_eq!(
            channel_by_name_refusal(false, |_| async { false }, || async { true }).await,
            None
        );
        assert_eq!(
            channel_by_name_refusal(false, |_| async { false }, || async { false }).await,
            Some(ByNameRefusal::NotFound)
        );
    }

    /// The 403 names `read_public_channel` (unlike `getChannel`'s `read_channel`), and the 404
    /// wears the store's `missing` id with `where = getChannelByName` and the team/name detail.
    #[test]
    fn by_name_denials_carry_gos_ids() {
        let session = mm_model::session::Session {
            user_id: ME_ID.to_owned(),
            ..Default::default()
        };
        let channel = mm_model::channel::Channel {
            team_id: "tttttttttttttttttttttttttt".to_owned(),
            name: "secret".to_owned(),
            ..Default::default()
        };

        let forbidden = channel_by_name_denial(ByNameRefusal::Forbidden, &session, &channel);
        assert_eq!(forbidden.0.status_code, 403);
        assert_eq!(forbidden.0.id, "api.context.permissions.app_error");
        assert_eq!(
            forbidden.0.detailed_error,
            format!("userId={ME_ID}, permission=read_public_channel")
        );

        let missing = channel_by_name_denial(ByNameRefusal::NotFound, &session, &channel);
        assert_eq!(missing.0.status_code, 404);
        assert_eq!(missing.0.id, "app.channel.get_by_name.missing.app_error");
        assert_eq!(missing.0.where_, "getChannelByName");
        assert_eq!(
            missing.0.detailed_error,
            "teamId=tttttttttttttttttttttttttt, name=secret"
        );
    }

    /// `RequireUserId().RequireTeamId()` — the user is checked first here, the reverse of the
    /// channel routes' chain.
    #[test]
    fn the_user_id_is_validated_before_the_team_id() {
        let name = |err: ApiError| {
            err.0
                .params
                .as_ref()
                .and_then(|p| p.get("Name"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        };
        assert_eq!(
            name(validate_user_then_team("nope", "alsonope").expect_err("both invalid")),
            Some("user_id".to_owned())
        );
        assert_eq!(
            name(validate_user_then_team(ME_ID, "alsonope").expect_err("team invalid")),
            Some("team_id".to_owned())
        );
        assert!(validate_user_then_team(ME_ID, OTHER).is_ok());
    }

    /// The user gate runs first and names `edit_other_users`; the team gate is never polled when
    /// it denies, and names `view_team` when it does.
    #[tokio::test]
    async fn the_channels_for_team_gates_run_user_then_team() {
        let denied = channels_for_team_denied(false, || async {
            panic!("the team gate must not run when the user gate denies")
        })
        .await;
        assert_eq!(denied.map(|p| p.id.as_ref()), Some("edit_other_users"));

        let denied = channels_for_team_denied(true, || async { false }).await;
        assert_eq!(denied.map(|p| p.id.as_ref()), Some("view_team"));

        assert!(
            channels_for_team_denied(true, || async { true })
                .await
                .is_none()
        );
    }

    /// A list of `n` channels with distinct, sortable ids, for the streaming layout tests.
    fn channel_page(ids: std::ops::Range<usize>) -> mm_model::channel_list::ChannelList {
        mm_model::channel_list::ChannelList(
            ids.map(|i| mm_model::channel::Channel {
                id: format!("{i:0>26}"),
                ..Default::default()
            })
            .collect(),
        )
    }

    fn not_found() -> ApiError {
        ApiError(mm_model::utils::AppError::new(
            "GetChannelsForUser",
            CHANNELS_NOT_FOUND_ID,
            None,
            String::new(),
            404,
        ))
    }

    /// The byte layout of a short single page: `[`, each element `Encode`d (trailing newline)
    /// with `,` between, and a bare `]` — no newline after it.
    #[tokio::test]
    async fn a_short_page_streams_as_bracket_elements_newline_comma_bracket() {
        let mut calls = Vec::new();
        let body = stream_channels_for_user(|from| {
            calls.push(from);
            async { Ok(channel_page(0..2)) }
        })
        .await;
        assert_eq!(
            calls,
            vec![String::new()],
            "one page, fetched from the start"
        );

        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("[{\"id\":\"000"), "{text}");
        assert!(
            text.ends_with("}\n]"),
            "no newline after the bracket: {text:?}"
        );
        assert_eq!(
            text.matches("}\n,{").count(),
            1,
            "one separator between two elements"
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
    }

    /// Two full pages and a short third: the loop keys on the last id of each page, and the
    /// page boundary is the same `}\n,{` as an element boundary.
    #[tokio::test]
    async fn full_pages_continue_from_the_last_id_and_join_with_a_comma() {
        let page = CHANNELS_FOR_USER_PAGE_SIZE as usize;
        let mut calls = Vec::new();
        let body = stream_channels_for_user(|from| {
            calls.push(from.clone());
            async move {
                Ok(match from.as_str() {
                    "" => channel_page(0..page),
                    f if f == format!("{:0>26}", page - 1) => channel_page(page..2 * page),
                    f if f == format!("{:0>26}", 2 * page - 1) => {
                        channel_page(2 * page..2 * page + 1)
                    }
                    other => panic!("unexpected keyset {other}"),
                })
            }
        })
        .await;
        assert_eq!(calls.len(), 3);

        let text = String::from_utf8(body).unwrap();
        assert_eq!(
            text.matches("}\n,{").count(),
            2 * page,
            "2*page+1 elements, 2*page commas"
        );
        assert!(!text.contains(",,"), "no doubled comma at a page boundary");
        assert!(
            !text.contains("\n\n"),
            "no doubled newline at a page boundary"
        );
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2 * page + 1);
    }

    /// A total that is an exact multiple of the page size ends on the store's `not_found` for
    /// the page after, and that error is swallowed: the body closes normally.
    #[tokio::test]
    async fn an_exact_multiple_of_the_page_size_ends_on_a_swallowed_not_found() {
        let page = CHANNELS_FOR_USER_PAGE_SIZE as usize;
        let mut calls = 0;
        let body = stream_channels_for_user(|from| {
            calls += 1;
            async move {
                if from.is_empty() {
                    Ok(channel_page(0..page))
                } else {
                    Err(not_found())
                }
            }
        })
        .await;
        assert_eq!(calls, 2, "the full page forces one more fetch");

        let text = String::from_utf8(body).unwrap();
        assert!(text.ends_with("}\n]"), "{}", &text[text.len() - 40..]);
        assert!(!text.contains("not_found"));
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), page);
    }

    /// Zero channels is the same `not_found` on the **first** page, and that one is not
    /// swallowed: the body is `[` followed by the error JSON — request id populated, detail
    /// wiped, status code 404 in the body — and no closing bracket.
    #[tokio::test]
    async fn zero_channels_is_a_bracket_then_the_error_body() {
        let body = stream_channels_for_user(|_| async { Err(not_found()) }).await;
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("[{\"id\":\"app.channel.get_channels.not_found.app_error\""));
        assert!(text.ends_with("\"status_code\":404}"), "{text}");
        assert!(
            serde_json::from_str::<serde_json::Value>(&text).is_err(),
            "not JSON"
        );
        let error: serde_json::Value = serde_json::from_str(&text[1..]).unwrap();
        assert_eq!(error["request_id"].as_str().map(str::len), Some(26));
        assert_eq!(error["detailed_error"], "");
    }

    /// Any other error after a page has been written — a 500 from the store, or a
    /// `not_found` that is not the keyset's — lands after the elements so far.
    #[tokio::test]
    async fn a_mid_stream_error_lands_after_the_elements_written_so_far() {
        let page = CHANNELS_FOR_USER_PAGE_SIZE as usize;
        let body = stream_channels_for_user(|from| async move {
            if from.is_empty() {
                Ok(channel_page(0..page))
            } else {
                Err(ApiError(mm_model::utils::AppError::new(
                    "GetChannelsForUser",
                    "app.channel.get_channels.get.app_error",
                    None,
                    "db went away",
                    500,
                )))
            }
        })
        .await;
        let text = String::from_utf8(body).unwrap();
        assert_eq!(text.matches("}\n,{").count(), page - 1);
        assert!(
            text.contains("}\n{\"id\":\"app.channel.get_channels.get.app_error\""),
            "{}",
            &text[text.len() - 200..]
        );
        assert!(text.ends_with("\"status_code\":500}"));
        assert!(!text.contains("db went away"), "detail is wiped");
    }

    /// The members route gates team first and names `view_team`; a self read never polls the
    /// `manage_system` check; a non-self read does, and a refusal names `manage_system`.
    #[tokio::test]
    async fn the_members_for_team_gates_run_team_then_self_or_manage_system() {
        let denied = members_for_team_denied(
            || async { false },
            false,
            || async { panic!("manage_system must not be polled when the team gate denies") },
        )
        .await;
        assert_eq!(denied.map(|p| p.id.as_ref()), Some("view_team"));

        let denied = members_for_team_denied(
            || async { true },
            true,
            || async { panic!("a self read must not poll manage_system") },
        )
        .await;
        assert!(denied.is_none());

        let denied = members_for_team_denied(|| async { true }, false, || async { false }).await;
        assert_eq!(denied.map(|p| p.id.as_ref()), Some("manage_system"));

        assert!(
            members_for_team_denied(|| async { true }, false, || async { true })
                .await
                .is_none()
        );
    }

    /// `last_delete_at`: `Atoi` failure is `0`, a negative value is the only 400 in the ported
    /// pagination-style parameters, and `+` is the one decoration `Atoi` accepts — but a literal
    /// `+` in a query string is a **space** to `url.ParseQuery` before `Atoi` ever sees it, so
    /// only the escaped `%2B` reaches the parser as a sign.
    #[test]
    fn last_delete_at_parses_like_atoi_and_rejects_negatives() {
        for (query, expected) in [
            (None, 0),
            (Some(""), 0),
            (Some("last_delete_at="), 0),
            (Some("last_delete_at=abc"), 0),
            (Some("last_delete_at=1.5"), 0),
            (Some("last_delete_at= 5"), 0),
            (Some("last_delete_at=99999999999999999999999"), 0), // out of range → Atoi error
            (Some("last_delete_at=0"), 0),
            (Some("last_delete_at=+7"), 0), // `+` decodes to a space → Atoi error
            (Some("last_delete_at=%2B7"), 7),
            (Some("last_delete_at=007"), 7),
            (Some("last_delete_at=1700000000000"), 1_700_000_000_000),
            (Some("last_delete_at=3&last_delete_at=-1"), 3), // first value wins
            (Some("include_deleted=true"), 0),
        ] {
            assert_eq!(
                parse_last_delete_at(query).expect("not negative"),
                expected,
                "query {query:?}"
            );
        }
        for query in ["last_delete_at=-1", "last_delete_at=-9999999"] {
            let err = parse_last_delete_at(Some(query)).expect_err("negative is a 400");
            assert_eq!(err.0.status_code, 400, "{query}");
            assert_eq!(err.0.id, "api.context.invalid_url_param.app_error");
            assert_eq!(
                err.0
                    .params
                    .as_ref()
                    .and_then(|p| p.get("Name"))
                    .and_then(serde_json::Value::as_str),
                Some("last_delete_at")
            );
        }
    }

    /// `HandleEtag` is an exact comparison of the raw header: no `W/`, no quoting, no list.
    #[test]
    fn the_etag_comparison_is_exact() {
        let etag = "10.0.0.zzzz.1700000000000.0.3";
        let with = |value: &str| {
            let mut headers = HeaderMap::new();
            headers.insert(IF_NONE_MATCH, value.parse().unwrap());
            headers
        };
        assert!(etag_matches(&with(etag), etag));
        assert!(!etag_matches(&with(&format!("W/{etag}")), etag));
        assert!(!etag_matches(&with(&format!("\"{etag}\"")), etag));
        assert!(!etag_matches(&with(&format!("{etag}, other")), etag));
        assert!(!etag_matches(&HeaderMap::new(), etag));
        assert!(!etag_matches(&with(""), ""), "an empty etag never matches");
    }

    /// The list body is a bare array (`ChannelList` is `#[serde(transparent)]`) with the encoder
    /// newline, and an empty list would be `[]` — though the route 404s before it could be.
    #[test]
    fn the_channel_list_body_is_a_bare_array_with_a_newline() {
        let list = mm_model::channel_list::ChannelList(vec![mm_model::channel::Channel::default()]);
        let mut body = serde_json::to_vec(&list).expect("serialises");
        body.push(b'\n');
        assert_eq!(body.first(), Some(&b'['));
        assert_eq!(&body[body.len() - 2..], b"]\n");
    }

    /// A non-open channel **never** consults the team gate: `read_public_channel` team-wide must
    /// not open a private channel. This is the security-relevant mutation — swapping the branch
    /// condition would leak every private channel to its team.
    #[tokio::test]
    async fn a_private_channel_never_polls_the_team_gate() {
        let denied = channel_read_denied(
            false,
            || async { panic!("the team gate must not run for a non-open channel") },
            || async { false },
        )
        .await;
        assert!(denied);

        let allowed = channel_read_denied(
            false,
            || async { panic!("still must not run when the channel gate grants") },
            || async { true },
        )
        .await;
        assert!(!allowed);
    }

    /// `page * per_page` in Go's `int` — 64-bit and **wrapping**, not saturating and not clamped.
    ///
    /// Only the ordinary rows are observable over HTTP as a *page*; the overflow rows are
    /// observable only as the 500 the resulting offset provokes, which the parity suite asserts
    /// but which cannot tell a wrap from a saturation (both 500, one by "out of range" and one
    /// by a page number Postgres also refuses). So the arithmetic is pinned here.
    #[test]
    fn the_offset_is_page_times_per_page_and_it_wraps() {
        assert_eq!(page_offset(0, 60), 0);
        assert_eq!(page_offset(3, 60), 180);
        assert_eq!(page_offset(1, 0), 0, "per_page=0 pages nowhere");
        assert_eq!(
            page_offset(i64::MAX, 1),
            i64::MAX,
            "no overflow at per_page=1"
        );
        assert_eq!(
            page_offset(i64::MAX, 200),
            i64::MAX.wrapping_mul(200),
            "wraps like Go's int, rather than saturating to i64::MAX"
        );
        assert!(
            page_offset(i64::MAX, 200) < 0,
            "and the wrap is what makes the store refuse it"
        );
        assert_eq!(
            page_offset(4_611_686_018_427_387_904, 4),
            0,
            "a wrap that lands on zero serves the first page — Go's answer too"
        );
    }

    /// `web.PerPageDefault` / `PerPageMaximum` as these three routes see them: the parser is
    /// shared, but the *clamp* is only observable on a team with more than 200 channels, which
    /// no parity fixture builds. Pinned here instead.
    #[test]
    fn per_page_defaults_to_sixty_and_clamps_at_two_hundred() {
        assert_eq!(parse_per_page(None), 60);
        assert_eq!(parse_per_page(Some("per_page=201")), 200);
        assert_eq!(parse_per_page(Some("per_page=200")), 200);
        assert_eq!(parse_per_page(Some("per_page=0")), 0, "a real LIMIT 0 here");
        assert_eq!(parse_per_page(Some("per_page=-1")), 60);
        assert_eq!(parse_per_page(Some("per_page=abc")), 60);
        assert_eq!(parse_page(Some("page=-1")), 0);
        assert_eq!(parse_page(Some("page=7")), 7);
    }

    /// The three team lists carry **no** `ETag`, unlike `getChannelsForTeamForUser`, and the
    /// body is a bare array with the encoder newline even when empty — which for these routes is
    /// reachable, since a page past the end is `200 []` rather than a 404.
    #[test]
    fn an_empty_team_list_is_an_empty_array_with_a_newline() {
        let body = encoded_channel_list("x", &mm_model::channel_list::ChannelList(Vec::new()))
            .expect("serialises");
        assert_eq!(body, b"[]\n");

        let response = channel_list_response(body);
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response.headers().get("ETag").is_none(),
            "Go's handler never computes one for these three routes"
        );
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust")
        );
    }
}
