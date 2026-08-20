//! Ported handlers from `channels/api4/channel.go`:
//!
//! - `getChannel` — `GET /api/v4/channels/{channel_id}`
//! - `getChannelMember` — `GET /api/v4/channels/{channel_id}/members/{user_id}`
//! - `getChannelUnread` — `GET /api/v4/users/{user_id}/channels/{channel_id}/unread`
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
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::channel::CHANNEL_TYPE_OPEN;
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_READ_CHANNEL, PERMISSION_READ_PUBLIC_CHANNEL,
    make_permission_error,
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

/// Would Go's `getChannel` take the content-reviewer path for this query string?
///
/// Go reads `r.URL.Query().Get(model.AsContentReviewerParam)` — the **first** value when the key
/// repeats — and feeds it to `strconv.ParseBool`, discarding the error. So `?as_content_reviewer`
/// bare, `=yes`, or `=` are all false, while `=1`, `=t` and `=True` are true.
/// `form_urlencoded::parse` decodes percent-escapes and `+`-as-space the way `url.ParseQuery`
/// does, which matters because the *decoded* value is what `ParseBool` sees.
fn is_content_reviewer_request(query: Option<&str>) -> bool {
    let Some(query) = query else {
        return false;
    };
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == AS_CONTENT_REVIEWER_PARAM)
        .and_then(|(_, value)| parse_go_bool(&value))
        .unwrap_or(false)
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
}
