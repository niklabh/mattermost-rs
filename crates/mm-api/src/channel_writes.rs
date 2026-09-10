//! Port of the five channel-lifecycle handlers in `channels/api4/channel.go`:
//! `updateChannel` (:207), `updateChannelPrivacy` (:325), `patchChannel` (:395),
//! `restoreChannel` (:586) and `deleteChannel` (:1717).
//!
//! `createChannel` is deliberately **not** here: it adds the creator as a member, which is a
//! `ChannelMembers` write this file does not own.
//!
//! # `updateChannel` and `patchChannel` are not one route with two verbs
//!
//! They differ in five ways, and each one is a wire difference a happy-path test misses:
//!
//! | | `PUT /channels/{id}` | `PUT /channels/{id}/patch` |
//! |---|---|---|
//! | body | a whole `Channel`, whose `id` must match the path | a `ChannelPatch` |
//! | fields honoured | `header`, `purpose`, `display_name`, `name`, `group_constrained` — **and nothing else**; a submitted `create_at`, `total_msg_count`, `scheme_id`, `discoverable`, `autotranslation` or `default_category_name` is silently dropped | every field `Channel::patch` applies |
//! | empty string | `display_name: ""` and `name: ""` mean "leave alone"; `header: ""` and `purpose: ""` mean "clear" | `null` means leave alone and `""` means clear, for all four |
//! | archived channel | 400 `api.channel.update_channel.deleted.app_error` from the handler | 400 `app.channel.update.bad_id` from the **store** — there is no handler guard |
//! | `props` in the answer | absent | `FillInChannelProps` runs, so a header with a live `~mention` comes back carrying `channel_mentions` |
//!
//! Every row measured against the running Go server, not inferred.
//!
//! # Only `town-square` is special
//!
//! `off-topic` is created by the same team bootstrap and has **no** special case anywhere:
//! renaming it, archiving it and making it private all succeed. `model.DefaultChannelName` is one
//! name, and the four guards that mention it (rename in update, rename in patch, archive, and
//! convert-to-private) all test that one name.
//!
//! # A licensed installation is forwarded
//!
//! Four enterprise gates sit on these paths and none of their inputs is visible from here:
//! `ChannelAccessControlled` (ABAC, `MinimumEnterpriseAdvancedLicense`), the channel banner
//! (same), managed channel categories (`MinimumEnterpriseLicense` **and** a feature flag) and
//! `AutoTranslation().IsFeatureAvailable()`. On an unlicensed installation every one of them is
//! decided — false, false, skipped, unavailable — and all four are reproduced. On a licensed one
//! the whole request goes to Go. Same reading as `getAllChannels`; see
//! [`crate::channels::licence_gate`], which this file does **not** use because it consumes the
//! request that a later branch may still need to forward.
//!
//! `restoreChannel` is the exception: nothing on its path consults the licence, so it is served
//! either way.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::channel_write::{ChannelWrite, default_category_after_patch};
use mm_model::channel::{
    CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE, Channel,
    ChannelPatch, DEFAULT_CHANNEL_NAME,
};
use mm_model::permission::{
    PERMISSION_CONVERT_PRIVATE_CHANNEL_TO_PUBLIC, PERMISSION_CONVERT_PUBLIC_CHANNEL_TO_PRIVATE,
    PERMISSION_DELETE_PRIVATE_CHANNEL, PERMISSION_DELETE_PUBLIC_CHANNEL,
    PERMISSION_MANAGE_PRIVATE_CHANNEL_BANNER, PERMISSION_MANAGE_PRIVATE_CHANNEL_PROPERTIES,
    PERMISSION_MANAGE_PUBLIC_CHANNEL_BANNER, PERMISSION_MANAGE_PUBLIC_CHANNEL_PROPERTIES,
    PERMISSION_MANAGE_TEAM, PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_CHANNELS, Permission,
    make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, decode_one_from_json};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{query_flag_is_true, require_id};
use crate::error::ApiError;
use crate::proxy;

/// `FeatureFlags.DiscoverableChannels` (feature_flags.go:208) — **false** at the pinned SHA, and
/// absent from every configuration document Go writes because it clears `FeatureFlags` before
/// persisting. So there is nothing to read and nothing to configure: the flag is a constant here,
/// exactly as [D-153] already records for `getChannel`'s discoverable branch.
const FEATURE_FLAG_DISCOVERABLE_CHANNELS: bool = false;

/// Whether the auto-translation service is available, i.e. `c.App.AutoTranslation() != nil &&
/// IsFeatureAvailable()` (api4/channel.go:439).
///
/// **False**, and measured rather than assumed: `PUT /channels/{id}/patch` with
/// `{"autotranslation": true}` answers 403
/// `api.channel.patch_update_channel.feature_not_available.app_error` on the stack's Go server.
/// The interface is registered by the enterprise build and gated behind a licence, and a licensed
/// installation is forwarded before this is consulted.
const AUTO_TRANSLATION_AVAILABLE: bool = false;

/// `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing newline**.
/// The one route of the five that answers this rather than a channel.
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

/// `json.NewEncoder(w).Encode(channel)` — a 200 and a **trailing newline** ([D-086]).
fn channel_response(handler: &'static str, channel: &Channel) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(channel).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise Channel");
        ApiError::from(AppError::new(
            handler,
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

/// `model.NewAppError(handler, id, map[string]any{key: value}, "", status)`.
fn app_error_with(
    handler: &'static str,
    id: &'static str,
    key: &'static str,
    value: &str,
    status: i32,
) -> Box<AppError> {
    AppError::boxed(
        handler,
        id,
        Some(std::collections::HashMap::from([(
            key.to_owned(),
            serde_json::Value::String(value.to_owned()),
        )])),
        String::new(),
        status,
    )
}

/// Which permission gates *property* edits for a channel of this type, or `None` for a type that
/// has no property gate at all (`D`, `G`, and Go's `default:` arm).
///
/// A function so the pairing is testable in-process: the two ids differ by one word, both are
/// 403s, and the wrong one reaches a client only through `detailed_error`, which is wiped unless
/// developer mode is on ([D-092]). Swapping them survives every cross-server test.
fn channel_properties_permission(channel_type: &str) -> Option<&'static Permission> {
    match channel_type {
        CHANNEL_TYPE_OPEN => Some(&PERMISSION_MANAGE_PUBLIC_CHANNEL_PROPERTIES),
        CHANNEL_TYPE_PRIVATE => Some(&PERMISSION_MANAGE_PRIVATE_CHANNEL_PROPERTIES),
        _ => None,
    }
}

/// `403 api.channel.patch_update_channel.forbidden.app_error` — shared by both handlers' DM/GM
/// membership failure *and* their `default:` arm, under two different handler names.
fn patch_update_forbidden(handler: &'static str) -> Box<AppError> {
    AppError::boxed(
        handler,
        "api.channel.patch_update_channel.forbidden.app_error",
        None,
        String::new(),
        403,
    )
}

/// Whether the installation carries a licence, i.e. whether these routes must hand over.
///
/// Not [`crate::channels::licence_gate`], which forwards for you and therefore consumes the
/// request — three of these five handlers have a *second* forwarding branch further down and need
/// to still own it.
async fn licensed(state: &AppState) -> Result<bool, ApiError> {
    let state_of_licence = state.app.license_state().await?;
    let licensed = state_of_licence == mm_app::license::LicenseState::Licensed;
    tracing::Span::current().record("licensed", licensed);
    Ok(licensed)
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/channels/{channel_id}
// ---------------------------------------------------------------------------------------------

/// Port of `updateChannel` (api4/channel.go:207).
///
/// # The submitted channel is a *source of five fields*, not a replacement
///
/// Go copies `Header` and `Purpose` unconditionally, `DisplayName` and `Name` only when non-empty,
/// and `GroupConstrained` only when non-nil — onto the channel it read from the database. Every
/// other field on the request is discarded. That asymmetry is the whole route: `header: ""` clears
/// the header while `display_name: ""` leaves the display name alone, and a client that
/// round-trips a fetched channel through this endpoint cannot corrupt `create_at` or
/// `total_msg_count` no matter what it sends. Measured field by field against the running server.
///
/// # Order of operations
///
/// 1. `RequireChannelId` — 400 `api.context.invalid_url_param.app_error`.
/// 2. decode the body — 400 `api.context.invalid_body_param.app_error` naming **`channel`**.
/// 3. `channel.Id != c.Params.ChannelId` — the same id naming **`channel_id`**. A body that omits
///    `id` lands here, not on step 2.
/// 4. `GetChannel` — 404 `app.channel.get.existing.app_error`. Before every permission check,
///    because the channel's *type* chooses the check.
/// 5. the type switch: `manage_public_channel_properties` / `manage_private_channel_properties` /
///    membership-only for DM and GM / 403 for anything else.
/// 6. archived → 400 `api.channel.update_channel.deleted.app_error`.
/// 7. a different non-empty `type` → 400 `api.channel.update_channel.typechange.app_error`.
///    **A type change is refused here, so `/privacy` is the only way to convert a channel.**
/// 8. renaming `town-square` → 400 `api.channel.update_channel.tried.app_error`.
/// 9. the five-field copy, then `App.UpdateChannel`.
///
/// Step 6 comes *after* the permission switch, so a member of an archived DM sees the archived
/// error and a non-member sees the 403.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, licensed, forwarded = false))]
pub async fn update_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    // Go decodes into `*model.Channel`, so a body of `null` decodes *successfully* into a nil
    // pointer and is caught by the same `err != nil || channel == nil` test as a malformed one.
    //
    // `decode_one_from_json` rather than `serde_json::from_slice`: `json.Decoder.Decode` reads one
    // value and **stops**, so `{"id":"…"} garbage` and two concatenated objects are both a 200 on
    // Go, taking the first. `from_slice` rejects the trailing bytes and would 400. Measured.
    let submitted: Channel = match decode_one_from_json::<Option<Channel>>(&bytes) {
        Ok(Some(channel)) => channel,
        Ok(None) => return ApiError::invalid_param("channel").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "channel body did not decode");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    if submitted.id != channel_id {
        return ApiError::invalid_param("channel_id").into_response();
    }

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    match licensed(&state).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    match serve_update_channel(&state, &session.0, &channel_id, &submitted).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_update_channel(
    state: &AppState,
    session: &Session,
    channel_id: &str,
    submitted: &Channel,
) -> Result<Response, ApiError> {
    let mut channel = state.app.get_channel(channel_id).await?;

    match channel.channel_type.as_str() {
        CHANNEL_TYPE_OPEN | CHANNEL_TYPE_PRIVATE => {
            let permission = channel_properties_permission(&channel.channel_type)
                .ok_or_else(|| patch_update_forbidden("updateChannel"))?;
            let (allowed, _) = state
                .app
                .session_has_permission_to_channel(session, channel_id, permission)
                .await;
            if !allowed {
                return Err(make_permission_error(session, &[permission]).into());
            }
        }
        CHANNEL_TYPE_GROUP | CHANNEL_TYPE_DIRECT => {
            // "Modifying the header is not linked to any specific permission for group/dm
            // channels, so just check for membership." The lookup's own 404 is discarded and
            // replaced with a **403** — a non-member of a DM is told forbidden, not not-found.
            if state
                .app
                .get_channel_member(channel_id, &session.user_id)
                .await
                .is_err()
            {
                return Err(patch_update_forbidden("updateChannel").into());
            }
            // `purpose` is compared **unconditionally** while `name` and `display_name` are
            // compared only when non-empty — so any purpose change to a DM is a 400, including
            // `purpose: ""` against a DM that has one, while a `header`-only edit is allowed.
            if (!submitted.name.is_empty() && submitted.name != channel.name)
                || (!submitted.display_name.is_empty()
                    && submitted.display_name != channel.display_name)
                || submitted.purpose != channel.purpose
            {
                return Err(AppError::boxed(
                    "updateChannel",
                    "api.channel.update_channel.update_direct_or_group_messages_not_allowed.app_error",
                    None,
                    String::new(),
                    400,
                )
                .into());
            }
        }
        // Unreachable through `GetChannel`, which selects only `O`, `P`, `D` and `G`. Ported
        // because it is the arm a widened lookup would start hitting.
        _ => return Err(patch_update_forbidden("updateChannel").into()),
    }

    if channel.delete_at > 0 {
        return Err(AppError::boxed(
            "updateChannel",
            "api.channel.update_channel.deleted.app_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    if !submitted.channel_type.is_empty() && submitted.channel_type != channel.channel_type {
        return Err(AppError::boxed(
            "updateChannel",
            "api.channel.update_channel.typechange.app_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    if channel.name == DEFAULT_CHANNEL_NAME
        && !submitted.name.is_empty()
        && submitted.name != channel.name
    {
        return Err(app_error_with(
            "updateChannel",
            "api.channel.update_channel.tried.app_error",
            "Channel",
            DEFAULT_CHANNEL_NAME,
            400,
        )
        .into());
    }

    apply_update(&mut channel, submitted);

    state.app.update_channel(&mut channel).await?;

    // `PostUpdateChannelDisplayNameMessage` — [D-232]. Note Go's condition compares the *old*
    // display name against the **submitted** one rather than the applied one, so an update that
    // omits `display_name` posts "renamed to <empty>"; there is nothing to reproduce here.
    //
    // **No `FillInChannelProps`.** `patchChannel` calls it and this does not, so the same channel
    // answers with `props` from one route and without from the other.
    channel_response("updateChannel", &channel)
}

/// The five-field copy at api4/channel.go:286-300, as a function so each field's rule is pinned
/// by a unit test rather than only by a cross-server round trip.
///
/// Two unconditional, two guarded on non-empty, one guarded on presence. Making `header`
/// non-empty-guarded would make the header unclearable; making `display_name` unconditional would
/// let a client blank it by omission and then fail `IsValid` on nothing at all.
fn apply_update(channel: &mut Channel, submitted: &Channel) {
    channel.header = submitted.header.clone();
    channel.purpose = submitted.purpose.clone();
    if !submitted.display_name.is_empty() {
        channel.display_name = submitted.display_name.clone();
    }
    if !submitted.name.is_empty() {
        channel.name = submitted.name.clone();
    }
    if submitted.group_constrained.is_some() {
        channel.group_constrained = submitted.group_constrained;
    }
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/channels/{channel_id}/patch
// ---------------------------------------------------------------------------------------------

/// What [`decide_patch`] concluded.
enum PatchDecision {
    /// Every check passed. Carries the channel as read, ready to be patched.
    Serve(Box<Channel>),
    /// Nothing has been written. Hand the request to Go.
    Forward(&'static str),
}

/// Port of `patchChannel` (api4/channel.go:395).
///
/// # An empty patch is a 400
///
/// `{}` answers `api.channel.patch_update_channel.no_changes.app_error`. The test is a disjunction
/// over nine of the ten fields — every one except `banner_info`, which is tested separately in the
/// same condition — so the only body that fails it is one that mentions none of them.
///
/// # Three of the ten fields are refused outright on this deployment
///
/// - `autotranslation` → **403** `api.channel.patch_update_channel.feature_not_available.app_error`
/// - `discoverable` → **400** `api.channel.discoverable_join_request.feature_disabled.app_error`
/// - `banner_info` → **403** `license_error.feature_unavailable.specific`
///
/// All three measured. The first two are refused *before* the permission switch and the third
/// after it, so a caller holding nothing sees the feature error for the first two and a permission
/// error for the third.
///
/// `managed_category_name` is the fourth oddity and the only silent one: it is accepted, counts
/// towards "is this patch empty", and then **does nothing** — the licence gate skips the write and
/// [`Channel::patch`] never applies the field anyway ([D-016]). A patch of nothing but
/// `managed_category_name` is a 200 whose body is unchanged apart from `update_at`. Measured.
///
/// # Two branches are forwarded because they write something this file does not own
///
/// See [`patch_needs_go`]. Both are decided *before* `PatchChannel` runs, so a forwarded request
/// has not been half-applied.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, licensed, forwarded = false))]
pub async fn patch_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    // The invalid-param name is **`channel`**, not `patch` — the handler decodes a `ChannelPatch`
    // and calls it a channel. `decode_one_from_json` for the same reason as `updateChannel`'s body.
    let patch: ChannelPatch = match decode_one_from_json::<Option<ChannelPatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) => return ApiError::invalid_param("channel").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "channel patch body did not decode");
            return ApiError::invalid_param("channel").into_response();
        }
    };

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));

    let mut channel = match decide_patch(&state, &session.0, &channel_id, &patch).await {
        Ok(PatchDecision::Serve(channel)) => channel,
        Ok(PatchDecision::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the channel patch to Go");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(err) => return err.into_response(),
    };

    match state.app.patch_channel(&mut channel, &patch).await {
        Ok(ChannelWrite::Done) => {}
        Ok(ChannelWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the channel patch to Go");
            return proxy::forward_to_go(State(state), request).await;
        }
        Err(err) => return ApiError::from(err).into_response(),
    }

    // **`patchChannel` fills in the props and `updateChannel` does not.** A `~mention` of a live
    // public channel in the header comes back as `props.channel_mentions` from this route only.
    if let Err(err) = state.app.fill_in_channel_props(&mut channel).await {
        return ApiError::from(err).into_response();
    }

    match channel_response("patchChannel", &channel) {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Everything `patchChannel` does before `App.PatchChannel`, in Go's order — so that a decision to
/// forward is reached with nothing written.
async fn decide_patch(
    state: &AppState,
    session: &Session,
    channel_id: &str,
    patch: &ChannelPatch,
) -> Result<PatchDecision, ApiError> {
    if licensed(state).await? {
        return Ok(PatchDecision::Forward(
            "the banner, managed-category and ABAC gates need the licence",
        ));
    }

    let channel = state.app.get_channel(channel_id).await?;

    let updating_properties = patch.display_name.is_some()
        || patch.name.is_some()
        || patch.header.is_some()
        || patch.purpose.is_some()
        || patch.group_constrained.is_some()
        || patch.default_category_name.is_some();
    let updating_auto_translation = patch.auto_translation.is_some();
    let updating_managed_category = patch.managed_category_name.is_some();
    let updating_discoverable = patch.discoverable.is_some();

    if !updating_properties
        && !updating_auto_translation
        && patch.banner_info.is_none()
        && !updating_managed_category
        && !updating_discoverable
    {
        return Err(AppError::boxed(
            "patchChannel",
            "api.channel.patch_update_channel.no_changes.app_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    if updating_discoverable && !FEATURE_FLAG_DISCOVERABLE_CHANNELS {
        // The flag is off, so the three checks Go makes next — private-only, not archived, not
        // shared — and the `manage_private_channel_discoverability` gate are all unreachable.
        return Err(AppError::boxed(
            "patchChannel",
            "api.channel.discoverable_join_request.feature_disabled.app_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    if updating_auto_translation && !AUTO_TRANSLATION_AVAILABLE {
        // **403, and before the permission switch**, so this fires for a caller who holds nothing.
        return Err(AppError::boxed(
            "patchChannel",
            "api.channel.patch_update_channel.feature_not_available.app_error",
            None,
            String::new(),
            403,
        )
        .into());
    }

    // `SupportsGroupSync` is `O || P`, so this is the guard that turns `group_constrained` on a DM
    // or GM into a 400 rather than letting `IsValid` reject it later with a model error.
    if patch.group_constrained.is_some() && !channel.supports_group_sync() {
        return Err(AppError::boxed(
            "patchChannel",
            "api.channel.patch_update_channel.group_constrained_not_allowed.app_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    match channel.channel_type.as_str() {
        CHANNEL_TYPE_OPEN | CHANNEL_TYPE_PRIVATE => {
            // **Gated on `updatingProperties`, unlike `updateChannel`'s unconditional check.** A
            // patch of nothing but `managed_category_name` needs no channel permission at all.
            if updating_properties {
                let permission = channel_properties_permission(&channel.channel_type)
                    .ok_or_else(|| patch_update_forbidden("patchChannel"))?;
                let (allowed, _) = state
                    .app
                    .session_has_permission_to_channel(session, channel_id, permission)
                    .await;
                if !allowed {
                    return Err(make_permission_error(session, &[permission]).into());
                }
            }
            // Go's `updatingAutoTranslation` permission check follows, unreachable: the feature
            // gate above already returned.
        }
        CHANNEL_TYPE_GROUP | CHANNEL_TYPE_DIRECT => {
            if state
                .app
                .get_channel_member(channel_id, &session.user_id)
                .await
                .is_err()
            {
                return Err(patch_update_forbidden("patchChannel").into());
            }
            // Four disjuncts here against `updateChannel`'s three: a `default_category_name` on a
            // DM is refused outright, whatever its value, because the *presence* of the field is
            // the test.
            if patch
                .name
                .as_ref()
                .is_some_and(|name| *name != channel.name)
                || patch
                    .display_name
                    .as_ref()
                    .is_some_and(|name| *name != channel.display_name)
                || patch
                    .purpose
                    .as_ref()
                    .is_some_and(|purpose| *purpose != channel.purpose)
                || patch.default_category_name.is_some()
            {
                return Err(AppError::boxed(
                    "patchChannel",
                    "api.channel.patch_update_channel.update_direct_or_group_messages_not_allowed.app_error",
                    None,
                    String::new(),
                    400,
                )
                .into());
            }
            // `RestrictDMAndGM` gates auto-translation on a DM; unreachable behind the feature
            // gate above.
        }
        _ => return Err(patch_update_forbidden("patchChannel").into()),
    }

    if channel.name == DEFAULT_CHANNEL_NAME
        && patch
            .name
            .as_ref()
            .is_some_and(|name| *name != channel.name)
    {
        return Err(app_error_with(
            "patchChannel",
            "api.channel.update_channel.tried.app_error",
            "Channel",
            DEFAULT_CHANNEL_NAME,
            400,
        )
        .into());
    }

    if patch.banner_info.is_some() {
        return Err(can_edit_channel_banner(state, session, channel_id, &channel).await);
    }

    // `updatingManagedCategory`'s permission check is inside
    // `MinimumEnterpriseLicense && FeatureFlags.ManagedChannelCategories`, and so is the write
    // that would follow it. Unlicensed, both are skipped and Go logs "Managed category update
    // ignored: feature not available" — the field is accepted and does nothing.

    if let Some(why) = patch_needs_go(
        &channel,
        patch,
        state.app.config().enable_channel_category_sorting,
    ) {
        return Ok(PatchDecision::Forward(why));
    }

    Ok(PatchDecision::Serve(Box::new(channel)))
}

/// Port of `canEditChannelBanner` (api4/channel.go:3245).
///
/// **The licence branch does not `return`.** Go sets `c.Err` to the licence error and then falls
/// into the type switch, which can overwrite it — so the answer is the *last* error assigned:
///
/// | channel | holds the banner permission | answer |
/// |---|---|---|
/// | `O`/`P` | yes | 403 `license_error.feature_unavailable.specific` |
/// | `O`/`P` | no | 403 the permission error |
/// | `D`/`G` | — | 400 `api.channel.update_channel.banner_info.channel_type.not_allowed` |
///
/// On an unlicensed installation this function therefore *always* fails, which is why every
/// `banner_info` patch is an error here and the caller returns unconditionally. A port that
/// returned early on the licence failure would answer the licence error where Go answers the
/// permission one — a difference only a caller without the permission can see.
async fn can_edit_channel_banner(
    state: &AppState,
    session: &Session,
    channel_id: &str,
    channel: &Channel,
) -> ApiError {
    let mut error = app_error_with(
        "patchChannel",
        "license_error.feature_unavailable.specific",
        "Feature",
        "Channel Banner",
        403,
    );

    let permission = match channel.channel_type.as_str() {
        CHANNEL_TYPE_PRIVATE => &PERMISSION_MANAGE_PRIVATE_CHANNEL_BANNER,
        CHANNEL_TYPE_OPEN => &PERMISSION_MANAGE_PUBLIC_CHANNEL_BANNER,
        _ => {
            return ApiError::from(AppError::new(
                "patchChannel",
                "api.channel.update_channel.banner_info.channel_type.not_allowed",
                None,
                String::new(),
                400,
            ));
        }
    };

    let (allowed, _) = state
        .app
        .session_has_permission_to_channel(session, channel_id, permission)
        .await;
    if !allowed {
        error = make_permission_error(session, &[permission]);
    }
    ApiError::from(error)
}

/// The two `patchChannel` branches that must go to Go, decided from the patch and the channel
/// **before** anything is written.
///
/// Kept apart from the handler so the decision is testable without a database, and so the reason
/// each branch exists stays attached to the condition:
///
/// - `group_constrained` going from off to **on** kicks non-group members
///   (`DeleteGroupConstrainedChannelMemberships`, in a goroutine). Setting it to `false`, or to
///   `true` on a channel that is already group-constrained, writes no memberships — Go's condition
///   is `*patch.GroupConstrained && (old == nil || !*old)`, and both halves matter.
/// - a non-empty `default_category_name` after the patch reaches `addChannelToDefaultCategory`,
///   which creates a sidebar category and moves the channel into it. Gated on
///   `TeamSettings.EnableChannelCategorySorting`, whose Go default is `true` — so on a stock
///   server the gate is really just "is the name non-empty".
pub(crate) fn patch_needs_go(
    channel: &Channel,
    patch: &ChannelPatch,
    category_sorting_enabled: bool,
) -> Option<&'static str> {
    if patch.group_constrained == Some(true) && !channel.is_group_constrained() {
        return Some("turning group_constrained on removes members outside the channel's groups");
    }
    if category_sorting_enabled && !default_category_after_patch(channel, patch).is_empty() {
        return Some("a default_category_name patch creates or moves a sidebar category");
    }
    None
}

// ---------------------------------------------------------------------------------------------
// PUT /api/v4/channels/{channel_id}/privacy
// ---------------------------------------------------------------------------------------------

/// Port of `updateChannelPrivacy` (api4/channel.go:325) — the only route that changes a channel's
/// type.
///
/// # The body is read with `StringInterfaceFromJSON`, which cannot fail
///
/// Go ignores the decode error and indexes the resulting map, so a body of `garbage`, `{}`,
/// `{"privacy": 7}` and `{"privacy": "X"}` are **one** answer: 400
/// `api.context.invalid_body_param.app_error` naming `privacy`. All four measured. Only the exact
/// strings `"O"` and `"P"` get past it.
///
/// # Both direction gates are evaluated, and each names a different permission
///
/// Go writes two independent `if`s rather than an `if`/`else`: `→ O` needs
/// `convert_private_channel_to_public` and `→ P` needs `convert_public_channel_to_private`. Only
/// one can match, since `privacy` is a single value — but note neither gate looks at the channel's
/// *current* type, so "converting" a public channel to public re-runs the private→public
/// permission check and then rewrites the row. A 200 that changes nothing but `update_at`.
///
/// # `town-square` cannot be made private
///
/// 400 `api.channel.update_channel_privacy.default_channel_error`, checked **after** the
/// permission gates — so a caller without the permission sees the 403 instead.
///
/// # Two websocket events, and the second is the one clients act on
///
/// `channel_updated` from `App.UpdateChannel`, then `channel_converted` addressed to the
/// **team**. See [`mm_app::channel_write`].
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, privacy, licensed, forwarded = false))]
pub async fn update_channel_privacy(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("privacy").into_response();
        }
    };

    let Some(privacy) = requested_privacy(&bytes) else {
        return ApiError::invalid_param("privacy").into_response();
    };
    tracing::Span::current().record("privacy", privacy);

    let request = Request::from_parts(parts, axum::body::Body::from(bytes));
    match licensed(&state).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    let mut channel = match prepare_privacy_change(&state, &session.0, &channel_id, privacy).await {
        Ok(channel) => channel,
        Err(err) => return err.into_response(),
    };

    match state.app.update_channel_privacy(&mut channel).await {
        Ok(ChannelWrite::Done) => match channel_response("updateChannelPrivacy", &channel) {
            Ok(response) => response,
            Err(err) => err.into_response(),
        },
        Ok(ChannelWrite::Forward(why)) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!(reason = why, "handing the privacy change to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `model.StringInterfaceFromJSON(r.Body)` followed by `props["privacy"].(string)` and the
/// two-value check (api4/channel.go:336-341).
///
/// Every failure collapses to `None`, because Go's decode error is discarded and a missing key, a
/// non-string value and an out-of-set string all fail the same `if`. Returns a `&'static str`
/// rather than the parsed value so a caller cannot accidentally widen the accepted set.
fn requested_privacy(body: &[u8]) -> Option<&'static str> {
    // `json.NewDecoder(data).Decode(&objmap)` — one value, trailing bytes unread, and the error
    // discarded. `serde_json::from_slice` differs on the first of those, which is why this goes
    // through the shared helper rather than the obvious call.
    let props: serde_json::Map<String, serde_json::Value> = decode_one_from_json(body).ok()?;
    match props.get("privacy")?.as_str()? {
        CHANNEL_TYPE_OPEN => Some(CHANNEL_TYPE_OPEN),
        CHANNEL_TYPE_PRIVATE => Some(CHANNEL_TYPE_PRIVATE),
        _ => None,
    }
}

/// Everything `updateChannelPrivacy` does before `App.UpdateChannelPrivacy`, with the requested
/// type already written onto the channel it returns.
async fn prepare_privacy_change(
    state: &AppState,
    session: &Session,
    channel_id: &str,
    privacy: &'static str,
) -> Result<Box<Channel>, ApiError> {
    let mut channel = state.app.get_channel(channel_id).await?;

    // Two `if`s, not an `if`/`else` — Go's shape, and the permission each reports differs.
    if privacy == CHANNEL_TYPE_OPEN {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                session,
                channel_id,
                &PERMISSION_CONVERT_PRIVATE_CHANNEL_TO_PUBLIC,
            )
            .await;
        if !allowed {
            return Err(make_permission_error(
                session,
                &[&PERMISSION_CONVERT_PRIVATE_CHANNEL_TO_PUBLIC],
            )
            .into());
        }
    }
    if privacy == CHANNEL_TYPE_PRIVATE {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                session,
                channel_id,
                &PERMISSION_CONVERT_PUBLIC_CHANNEL_TO_PRIVATE,
            )
            .await;
        if !allowed {
            return Err(make_permission_error(
                session,
                &[&PERMISSION_CONVERT_PUBLIC_CHANNEL_TO_PRIVATE],
            )
            .into());
        }
    }

    if channel.name == DEFAULT_CHANNEL_NAME && privacy == CHANNEL_TYPE_PRIVATE {
        return Err(AppError::boxed(
            "updateChannelPrivacy",
            "api.channel.update_channel_privacy.default_channel_error",
            None,
            String::new(),
            400,
        )
        .into());
    }

    // Only the username reaches the (unported) system post, but the error is Go's and it is
    // raised here — before the conversion, unlike `RestoreChannel`'s user lookup.
    state.app.privacy_change_author(&session.user_id).await?;

    channel.channel_type = privacy.to_owned();
    Ok(Box::new(channel))
}

// ---------------------------------------------------------------------------------------------
// DELETE /api/v4/channels/{channel_id}
// ---------------------------------------------------------------------------------------------

/// Port of `deleteChannel` (api4/channel.go:1717) — an **archive**, and `?permanent=true` is a
/// different operation behind a config flag that is off by default.
///
/// # `?permanent=true` on a stock server is a 401, and the id depends on who asked
///
/// `ServiceSettings.EnableAPIChannelDeletion` defaults to **false**, and the refusal is
/// `http.StatusUnauthorized` — not 403 — with
/// `api.user.delete_channel.not_enabled.for_admin.app_error` for a system admin and
/// `api.user.delete_channel.not_enabled.app_error` for everybody else, the admin's carrying a
/// longer message that names the setting. Measured. `strconv.ParseBool` decides what "true" means
/// and its error is discarded, so `?permanent=yes` is *false* and archives the channel.
///
/// When the flag is **on**, `PermanentDeleteChannel` purges posts, members, webhooks and the row.
/// Not ported, so that combination is forwarded.
///
/// # Order
///
/// `RequireChannelId` → `GetChannel` (404) → DM/GM refused with 400
/// `api.channel.delete_channel.type.invalid` → `delete_public_channel` /
/// `delete_private_channel` → the permanent branch → `App.DeleteChannel`. The type check precedes
/// both permission gates, so a DM is a 400 for anyone.
///
/// The success body is `ReturnStatusOK`: `{"status":"OK"}` with **no** trailing newline, unlike
/// the four routes that encode a channel.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, permanent, licensed, forwarded = false))]
pub async fn delete_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    // `params.Permanent, _ = strconv.ParseBool(query.Get("permanent"))` (web/params.go:232) — the
    // error is discarded, so `?permanent=yes` archives rather than 400s.
    let permanent = query_flag_is_true(request.uri().query(), "permanent");
    tracing::Span::current().record("permanent", permanent);

    // `cleanupChannelAccessControlPolicy` runs on this path under an Enterprise Advanced licence.
    match licensed(&state).await {
        Ok(true) => {
            tracing::Span::current().record("forwarded", true);
            return proxy::forward_to_go(State(state), request).await;
        }
        Ok(false) => {}
        Err(err) => return err.into_response(),
    }

    match serve_delete_channel(&state, &session.0, &channel_id, permanent).await {
        Ok(Some(response)) => response,
        Ok(None) => {
            tracing::Span::current().record("forwarded", true);
            tracing::debug!("handing a permanent channel deletion to Go");
            proxy::forward_to_go(State(state), request).await
        }
        Err(err) => err.into_response(),
    }
}

/// `Ok(None)` means "forward" — the permanent-deletion branch with the flag enabled.
async fn serve_delete_channel(
    state: &AppState,
    session: &Session,
    channel_id: &str,
    permanent: bool,
) -> Result<Option<Response>, ApiError> {
    let channel = state.app.get_channel(channel_id).await?;

    if channel.channel_type == CHANNEL_TYPE_DIRECT || channel.channel_type == CHANNEL_TYPE_GROUP {
        return Err(AppError::boxed(
            "deleteChannel",
            "api.channel.delete_channel.type.invalid",
            None,
            String::new(),
            400,
        )
        .into());
    }

    // Two separate `if`s in Go, not a switch — so a channel of neither type passes both, which
    // `GetChannel`'s type filter makes unreachable.
    if channel.channel_type == CHANNEL_TYPE_OPEN {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                session,
                channel_id,
                &PERMISSION_DELETE_PUBLIC_CHANNEL,
            )
            .await;
        if !allowed {
            return Err(
                make_permission_error(session, &[&PERMISSION_DELETE_PUBLIC_CHANNEL]).into(),
            );
        }
    }
    if channel.channel_type == CHANNEL_TYPE_PRIVATE {
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                session,
                channel_id,
                &PERMISSION_DELETE_PRIVATE_CHANNEL,
            )
            .await;
        if !allowed {
            return Err(
                make_permission_error(session, &[&PERMISSION_DELETE_PRIVATE_CHANNEL]).into(),
            );
        }
    }

    if permanent {
        if state.app.config().enable_api_channel_deletion {
            return Ok(None);
        }
        // `usrErr == nil && user != nil && user.IsSystemAdmin()` — a lookup **failure** falls to
        // the non-admin message rather than becoming an error of its own, so a broken user query
        // changes which 401 a system admin sees and nothing else.
        let is_admin = state
            .app
            .get_user(&session.user_id)
            .await
            .map(|user| user.is_system_admin())
            .unwrap_or(false);
        return Err(ApiError::from(AppError::new(
            "deleteChannel",
            not_enabled_error_id(is_admin),
            None,
            format!("channelId={channel_id}"),
            401,
        )));
    }

    state.app.delete_channel(&channel, &session.user_id).await?;

    Ok(Some(status_ok()))
}

/// The two ids `deleteChannel` chooses between when permanent deletion is disabled — "more
/// verbose error message for system admins".
///
/// A function because the two ids differ by the six characters `for_admin.` in the middle and both
/// are 401s: the wrong one is invisible to every test that only asserts the status.
fn not_enabled_error_id(is_system_admin: bool) -> &'static str {
    if is_system_admin {
        "api.user.delete_channel.not_enabled.for_admin.app_error"
    } else {
        "api.user.delete_channel.not_enabled.app_error"
    }
}

// ---------------------------------------------------------------------------------------------
// POST /api/v4/channels/{channel_id}/restore
// ---------------------------------------------------------------------------------------------

/// Port of `restoreChannel` (api4/channel.go:586).
///
/// # The permission is on the **team**, not the channel
///
/// `manage_team` on `channel.TeamId`, *or* the system-console permission
/// `sysconsole_write_user_management_channels`. Neither is a channel permission, so a channel
/// admin cannot unarchive their own channel and a team admin can unarchive one they were never in.
/// The refusal reports `manage_team` even when it was the second check that decided.
///
/// A DM or GM has an empty `TeamId`, and `SessionHasPermissionToTeam` returns false for an empty
/// team — so a DM's restore is a 403 for anyone without the system-console permission, and a 400
/// `api.channel.restore_channel.restored.app_error` for anyone with it, because a DM is never
/// archived.
///
/// # Not licence-gated
///
/// Unlike the other four, nothing on this path consults the licence: `RestoreChannel` has no ABAC
/// block and no enterprise cleanup. So a licensed installation is served here too.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id))]
pub async fn restore_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    if let Err(err) = require_id(&channel_id, "channel_id") {
        return err.into_response();
    }

    match serve_restore_channel(&state, &session.0, &channel_id).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_restore_channel(
    state: &AppState,
    session: &Session,
    channel_id: &str,
) -> Result<Response, ApiError> {
    let mut channel = state.app.get_channel(channel_id).await?;

    if !state
        .app
        .session_has_permission_to_team(session, &channel.team_id, &PERMISSION_MANAGE_TEAM)
        .await
        && !state
            .app
            .session_has_permission_to(
                session,
                &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_CHANNELS,
            )
            .await
    {
        return Err(make_permission_error(session, &[&PERMISSION_MANAGE_TEAM]).into());
    }

    state
        .app
        .restore_channel(&mut channel, &session.user_id)
        .await?;

    channel_response("restoreChannel", &channel)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_property_permissions_are_paired_with_the_right_type() {
        assert_eq!(
            channel_properties_permission(CHANNEL_TYPE_OPEN).map(|p| p.id.as_ref()),
            Some("manage_public_channel_properties")
        );
        assert_eq!(
            channel_properties_permission(CHANNEL_TYPE_PRIVATE).map(|p| p.id.as_ref()),
            Some("manage_private_channel_properties")
        );
        for other in [CHANNEL_TYPE_DIRECT, CHANNEL_TYPE_GROUP, "S", "BO", ""] {
            assert!(
                channel_properties_permission(other).is_none(),
                "{other} has no property permission"
            );
        }
    }

    /// Ten bodies, one answer. The decode error is discarded in Go, so a malformed body is
    /// indistinguishable from a missing key — and the accepted set is two exact bytes, not a
    /// case-insensitive match nor "any channel type".
    #[test]
    fn only_the_two_exact_privacy_strings_are_accepted() {
        assert_eq!(requested_privacy(br#"{"privacy":"O"}"#), Some("O"));
        assert_eq!(requested_privacy(br#"{"privacy":"P"}"#), Some("P"));
        for rejected in [
            &b"garbage"[..],
            br#"{}"#,
            br#"{"privacy":"X"}"#,
            br#"{"privacy":7}"#,
            br#"{"privacy":null}"#,
            br#"{"privacy":"o"}"#,
            br#"{"privacy":"D"}"#,
            br#"{"privacy":"G"}"#,
            br#"[]"#,
            b"",
        ] {
            assert_eq!(
                requested_privacy(rejected),
                None,
                "{} must be refused",
                String::from_utf8_lossy(rejected)
            );
        }
    }

    /// `json.Decoder.Decode` reads one value and stops, so trailing bytes cannot turn a good body
    /// into a 400. Three shapes Go accepts and `serde_json::from_slice` would refuse — all three
    /// measured against the running server on `/privacy`, `PUT` and `/patch`.
    #[test]
    fn trailing_bytes_after_the_first_value_are_ignored_like_go() {
        assert_eq!(
            requested_privacy(br#"{"privacy":"P"}trailing"#),
            Some("P"),
            "Decode stops after the first object"
        );
        assert_eq!(
            requested_privacy(br#"{"privacy":"P"}{"privacy":"O"}"#),
            Some("P"),
            "the second object is never read"
        );

        // A body that is only `null` decodes to a nil map, which Go replaces with an empty one —
        // so it fails the `privacy` lookup rather than the decode, and reaches the same 400.
        assert_eq!(requested_privacy(b"null"), None);
    }

    /// The same laxity on the two typed bodies, and the `null`-is-a-nil-pointer case beside it.
    #[test]
    fn a_channel_body_decodes_its_first_value_and_null_is_a_nil_pointer() {
        let decoded: Option<Channel> =
            decode_one_from_json(br#"{"id":"abc","header":"h"} trailing"#)
                .expect("Go accepts this");
        let decoded = decoded.expect("an object, not null");
        assert_eq!(decoded.id, "abc");
        assert_eq!(decoded.header, "h");

        let null: Option<Channel> = decode_one_from_json(b"null").expect("null is not an error");
        assert!(
            null.is_none(),
            "a nil pointer, which the handler turns into a 400"
        );

        let patch: Option<ChannelPatch> =
            decode_one_from_json(br#"{"header":"h"}{"header":"i"}"#).expect("Go accepts this");
        assert_eq!(
            patch.expect("an object").header.as_deref(),
            Some("h"),
            "the second patch is never read"
        );
    }

    #[test]
    fn the_permanent_refusal_names_the_admin_variant_only_for_an_admin() {
        assert_eq!(
            not_enabled_error_id(true),
            "api.user.delete_channel.not_enabled.for_admin.app_error"
        );
        assert_eq!(
            not_enabled_error_id(false),
            "api.user.delete_channel.not_enabled.app_error"
        );
    }

    fn channel_of(channel_type: &str) -> Channel {
        Channel {
            channel_type: channel_type.to_owned(),
            ..Channel::default()
        }
    }

    /// The five-field copy, field by field. Two unconditional, two non-empty-guarded, one
    /// presence-guarded — and everything else on the submitted channel ignored.
    #[test]
    fn the_update_copy_honours_five_fields_and_ignores_the_rest() {
        let mut channel = Channel {
            id: "c".to_owned(),
            header: "old header".to_owned(),
            purpose: "old purpose".to_owned(),
            display_name: "Old Name".to_owned(),
            name: "old-name".to_owned(),
            create_at: 111,
            total_msg_count: 7,
            last_post_at: 222,
            creator_id: "creator".to_owned(),
            scheme_id: Some("scheme".to_owned()),
            default_category_name: "Cat".to_owned(),
            discoverable: false,
            auto_translation: false,
            ..channel_of(CHANNEL_TYPE_OPEN)
        };
        let submitted = Channel {
            id: "c".to_owned(),
            header: String::new(),
            purpose: String::new(),
            display_name: String::new(),
            name: String::new(),
            create_at: 999,
            total_msg_count: 999,
            last_post_at: 999,
            creator_id: "someone-else".to_owned(),
            scheme_id: Some("other".to_owned()),
            default_category_name: "Other".to_owned(),
            discoverable: true,
            auto_translation: true,
            ..channel_of(CHANNEL_TYPE_OPEN)
        };
        apply_update(&mut channel, &submitted);

        // Unconditional: an empty header or purpose clears the field.
        assert_eq!(channel.header, "");
        assert_eq!(channel.purpose, "");
        // Non-empty-guarded: an empty display name or name leaves the old value.
        assert_eq!(channel.display_name, "Old Name");
        assert_eq!(channel.name, "old-name");
        // Everything else on the submitted channel is discarded.
        assert_eq!(channel.create_at, 111);
        assert_eq!(channel.total_msg_count, 7);
        assert_eq!(channel.last_post_at, 222);
        assert_eq!(channel.creator_id, "creator");
        assert_eq!(channel.scheme_id.as_deref(), Some("scheme"));
        assert_eq!(channel.default_category_name, "Cat");
        assert!(!channel.discoverable);
        assert!(!channel.auto_translation);
    }

    /// `group_constrained` is presence-guarded, so an explicit `false` clears it and an omitted
    /// field leaves it. `Option<bool>` makes the three cases distinguishable; a `bool` would not.
    #[test]
    fn group_constrained_is_copied_on_presence_not_on_truth() {
        let mut on = Channel {
            group_constrained: Some(true),
            ..channel_of(CHANNEL_TYPE_OPEN)
        };
        apply_update(
            &mut on,
            &Channel {
                group_constrained: Some(false),
                ..channel_of(CHANNEL_TYPE_OPEN)
            },
        );
        assert_eq!(on.group_constrained, Some(false), "an explicit false lands");

        let mut kept = Channel {
            group_constrained: Some(true),
            ..channel_of(CHANNEL_TYPE_OPEN)
        };
        apply_update(&mut kept, &channel_of(CHANNEL_TYPE_OPEN));
        assert_eq!(
            kept.group_constrained,
            Some(true),
            "an omitted field leaves the old value"
        );
    }

    /// Both halves of Go's condition. Turning the flag **off** writes no memberships, and neither
    /// does setting it on a channel that already has it — only the off→on edge does.
    #[test]
    fn only_group_constrained_turning_on_needs_go() {
        let plain = channel_of(CHANNEL_TYPE_OPEN);
        let on = ChannelPatch {
            group_constrained: Some(true),
            ..ChannelPatch::default()
        };
        assert!(patch_needs_go(&plain, &on, false).is_some());

        let off = ChannelPatch {
            group_constrained: Some(false),
            ..ChannelPatch::default()
        };
        assert!(patch_needs_go(&plain, &off, false).is_none());

        let already = Channel {
            group_constrained: Some(true),
            ..channel_of(CHANNEL_TYPE_OPEN)
        };
        assert!(
            patch_needs_go(&already, &on, false).is_none(),
            "re-setting the flag on a group-constrained channel kicks nobody"
        );
    }

    /// The sidebar branch and its config gate. With category sorting off, Go never reaches
    /// `addChannelToDefaultCategory` and the patch is serviceable here.
    #[test]
    fn the_default_category_branch_is_gated_on_the_config() {
        let channel = channel_of(CHANNEL_TYPE_OPEN);
        let named = ChannelPatch {
            default_category_name: Some("Zed".to_owned()),
            ..ChannelPatch::default()
        };
        assert!(patch_needs_go(&channel, &named, true).is_some());
        assert!(
            patch_needs_go(&channel, &named, false).is_none(),
            "EnableChannelCategorySorting off means no sidebar write"
        );

        let header_only = ChannelPatch {
            header: Some("hi".to_owned()),
            ..ChannelPatch::default()
        };
        assert!(patch_needs_go(&channel, &header_only, true).is_none());
    }
}
