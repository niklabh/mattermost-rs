//! Port of `getGroups` and `getGroupsByUserId` (channels/api4/group.go:1123, :856), reached as
//! `GET /api/v4/groups` and `GET /api/v4/users/{user_id}/groups`.
//!
//! The System Console's *User Management → Groups* page, and the group list shown on a user's
//! profile. Both are enterprise features and both open with `requireLicense`.
//!
//! # A **fourth** licence error, and this one is shared
//!
//! `requireLicense` (api4/handlers.go:237) returns `api.license_error` at **501** — the generic
//! one, with a blank `where`, used by every group route rather than an id of its own. The three
//! channel gates in `crate::channels` each have their own id and two of them answer 403; this pair
//! shares one id and one status. Four gates, four conventions.
//!
//! # Everything else is behind it
//!
//! On a licensed server the reads consult `License().Features.LDAPGroups` and the group store's
//! read surface, which is not ported — so a licensed installation is forwarded whole for the
//! reads. The writes are a different story; see below.
//!
//! # Twenty routes, one gate — the whole of `api4/group.go`
//!
//! **Extended 2026-09-12 with the seven CRUD and membership writes** — `createGroup`,
//! `getGroupsByNames`, `patchGroup`, `deleteGroup`, `restoreGroup`, `addGroupMembers` and
//! `deleteGroupMembers`. They open with the same `requireLicense` and in the same position: its
//! first statement, ahead of `RequireGroupId` *and ahead of reading the request body*. So on an
//! unlicensed server all seventeen collapse to the same 501, and a write with malformed JSON is
//! refused for the licence rather than for the JSON.
//!
//! **Completed 2026-09-12 with the three syncable writes** — `linkGroupSyncable`,
//! `unlinkGroupSyncable` and `patchGroupSyncable`. `InitGroup` registers twenty route+method
//! pairs and every one of them is now answered here; `api4/group.go` has no handler left that
//! this server forwards on its own account.
//!
//! **The seven writes are served on a licensed server too, since 2026-09-13** — the group store's
//! write surface, `licensedAndConfiguredForGroupBySource`, the custom-group permission model and
//! the three websocket events, compared against the licensed pair in
//! `parity::group_writes_licensed` ([D-360] closed). Still forwarded when licensed: the ten reads,
//! and the three syncable writes with their `GroupSyncable` upsert surface, permission verifiers
//! and `SyncRolesAndMembership` ([D-390]).
//!
//! Two facts the licensed comparison established that no reading of the source would have:
//! `system_user` holds every custom-group permission in the stock role set, so a plain user can
//! create, edit, delete and restore any custom group; and the implicit `custom_group_user` role
//! is **empty** by default, so membership grants nothing until an administrator edits that role.
//!
//! # All ten reads are the same gate
//!
//! Every `GET` in `api4/group.go` opens with `requireLicense(c)` as its **first statement** —
//! checked one by one, not assumed — before `RequireGroupId`, before `RequireTeamId`, before every
//! permission question and every query parameter. So on an unlicensed server the whole family
//! collapses to one answer, and the only thing a port can get wrong is which requests reach it.
//! That is a routing question, and it is where the two syncable routes earn their own handling:
//! `{syncable_type:teams|channels}` is an **alternation of literals**, so `/groups/{id}/foo`
//! never matched gorilla at all and must be forwarded rather than answered with the licence
//! error. See [`syncable_type_matches_go_mux`].
//!
//! Two of the eight added here have a *second* licence check behind the first —
//! `getGroupStats` and `getGroupsAssociatedToChannelsByTeam` consult
//! `License().Features.LDAPGroups` and answer `api.ldap_groups.license_error` at **403**. It is
//! unreachable without a licence, so it is named here and not ported: reaching it needs the
//! licence that makes the whole route forward.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::group::{Group, GroupModifyMembers, GroupPatch, GroupSource, GroupWithUserIds};
use mm_model::permission::{
    PERMISSION_CREATE_CUSTOM_GROUP, PERMISSION_DELETE_CUSTOM_GROUP, PERMISSION_EDIT_CUSTOM_GROUP,
    PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS, PERMISSION_RESTORE_CUSTOM_GROUP,
    PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS,
    PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS, Permission, make_permission_error,
};
use mm_model::session::Session;
use mm_model::user::{CHANNEL_MENTIONS_NOTIFY_PROP, USER_NOTIFY_ALL, USER_NOTIFY_HERE};
use mm_model::utils::{
    AppError, PAYLOAD_PARSE_ERROR, decode_one_from_json, is_valid_id, sorted_array_from_json,
};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate, read_body, require_id};

/// Port of `getGroups` (group.go:1123).
///
/// `requireLicense` is the first statement, ahead of the query parameters and every permission
/// question, so an unlicensed server answers the same 501 to every caller and every query string.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_groups(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    answer(&state, request).await
}

/// Port of `getGroupsByUserId` (group.go:856).
///
/// The licence check precedes `RequireUserId` here too, so `/users/abc/groups` — an id that could
/// never be valid — is the licence error rather than a 400. Behind the gate there are two more
/// refusals (self-or-`manage_system`, then `Features.LDAPGroups`), and neither is reachable
/// without a licence.
#[tracing::instrument(skip_all, fields(user_id = %user_id, licensed))]
pub async fn get_groups_by_user_id(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &user_id;
    answer(&state, request).await
}

/// `requireLicense`'s error (handlers.go:238): the **generic** id, a blank `where`, and a 501.
async fn answer(state: &AppState, request: Request) -> Response {
    match licence_gate(state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => crate::error::ApiError::from(mm_model::utils::AppError::new(
            "",
            "api.license_error",
            None,
            String::new(),
            501,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

/// Go's `{syncable_type:teams|channels}` (api4/group.go:47) — an **alternation of two literals**,
/// not a character class. A third value never matched the route, so gorilla answers its own mux
/// 404 and this server must forward rather than reproduce the licence error.
///
/// The distinction matters because the licence check is the *handler's* first statement and
/// routing happens before any handler: `/groups/{id}/teams` on an unlicensed server is a 501,
/// while `/groups/{id}/foo` is a 404 — two different answers to what looks like one route.
fn syncable_type_matches_go_mux(value: &str) -> bool {
    value == "teams" || value == "channels"
}

/// Port of `getGroup` (group.go:107) — `GET /api/v4/groups/{group_id}`.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn get_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `getGroupMembers` (group.go:770) — `GET /api/v4/groups/{group_id}/members`.
///
/// Behind the gate this route has a permission check of its own whose error `Where` is rewritten
/// to `Api4.getGroupMembers` after the fact (group.go:783). Not reachable unlicensed.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn get_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `getGroupStats` (group.go:814) — `GET /api/v4/groups/{group_id}/stats`.
///
/// The second gate — `Features.LDAPGroups`, a **403** with `api.ldap_groups.license_error` — is
/// behind the first and therefore unreachable here. See the module note.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn get_group_stats(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `getGroupSyncables` (group.go:483) — `GET /api/v4/groups/{group_id}/{syncable_type}`.
///
/// A `syncable_type` outside `teams|channels` is forwarded: gorilla never routed it, so Go's own
/// mux 404 is the correct answer and reproducing the licence error would invent one.
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn get_group_syncables(
    State(state): State<AppState>,
    Path((group_id, syncable_type)): Path<(String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    answer(&state, request).await
}

/// Port of `getGroupSyncable` (group.go:433) —
/// `GET /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}`.
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn get_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = (&group_id, &syncable_id);
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    answer(&state, request).await
}

/// Port of `getGroupsByChannel` (group.go:900) — `GET /api/v4/channels/{channel_id}/groups`.
#[tracing::instrument(skip_all, fields(channel_id = %channel_id, licensed))]
pub async fn get_groups_by_channel(
    State(state): State<AppState>,
    Path(channel_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &channel_id;
    answer(&state, request).await
}

/// Port of `getGroupsByTeam` (group.go:961) — `GET /api/v4/teams/{team_id}/groups`.
#[tracing::instrument(skip_all, fields(team_id = %team_id, licensed))]
pub async fn get_groups_by_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &team_id;
    answer(&state, request).await
}

/// Port of `getGroupsAssociatedToChannelsByTeam` (group.go:1070) —
/// `GET /api/v4/teams/{team_id}/groups_by_channels`.
///
/// Carries the same second `Features.LDAPGroups` gate as [`get_group_stats`], equally unreachable.
#[tracing::instrument(skip_all, fields(team_id = %team_id, licensed))]
pub async fn get_groups_associated_to_channels_by_team(
    State(state): State<AppState>,
    Path(team_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &team_id;
    answer(&state, request).await
}

/// The licence gate as the seven writes take it: **refuse** unlicensed, **continue** licensed.
///
/// [`answer`] forwards a licensed installation because the reads behind it are not ported; the
/// writes below are, so a licensed server goes on to the handler body. The refusal is the same
/// generic 501 with a blank `where`.
/// A refusal built before the handler body runs. Boxed because `Response` is large and clippy
/// objects to it in an `Err`.
type Refusal = Box<Response>;

async fn require_licence(state: &AppState) -> Result<(), Refusal> {
    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Licensed) => {
            tracing::Span::current().record("licensed", true);
            Ok(())
        }
        Ok(mm_app::license::LicenseState::Unlicensed) => {
            tracing::Span::current().record("licensed", false);
            Err(Box::new(
                crate::error::ApiError::from(AppError::new(
                    "",
                    "api.license_error",
                    None,
                    String::new(),
                    501,
                ))
                .into_response(),
            ))
        }
        Err(err) => Err(Box::new(crate::error::ApiError::from(err).into_response())),
    }
}

/// `licensedAndConfiguredForGroupBySource` with the caller's `where` written in, as every
/// caller in `api4/group.go` does before answering.
async fn licensed_and_configured(
    state: &AppState,
    source: &GroupSource,
    where_: &str,
) -> Result<(), Refusal> {
    state
        .app
        .licensed_and_configured_for_group_by_source(source)
        .await
        .map_err(|mut err| {
            err.where_ = where_.to_owned();
            Box::new(crate::error::ApiError::from(err).into_response())
        })
}

/// `c.App.GetGroup(c.Params.GroupId, nil, nil)` as the six id-taking writes call it, with
/// `RequireGroupId` in front: a malformed id is `invalid_url_param`, a missing group a 404.
async fn group_for_write(state: &AppState, group_id: &str) -> Result<Group, Refusal> {
    require_id(group_id, "group_id").map_err(|err| Box::new(err.into_response()))?;
    state
        .app
        .get_group(group_id)
        .await
        .map_err(|err| Box::new(crate::error::ApiError::from(err).into_response()))
}

/// `app.group.crud_permission` — the refusal every write but `createGroup` gives a group whose
/// source is not `custom`. **`restoreGroup` answers it at 501**; everything else at 400.
fn crud_permission_error(where_: &str, status: i32) -> Response {
    crate::error::ApiError::from(AppError::new(
        where_,
        "app.group.crud_permission",
        None,
        String::new(),
        status,
    ))
    .into_response()
}

fn permission_error(session: &Session, permission: &Permission) -> Response {
    crate::error::ApiError::from(*make_permission_error(session, &[permission])).into_response()
}

/// `w.Write(json.Marshal(v))` — a JSON body with **no trailing newline** — at `status`.
fn json_response<T: serde::Serialize>(status: StatusCode, value: &T, where_: &str) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (
            status,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            body,
        )
            .into_response(),
        Err(err) => {
            tracing::error!(error = %err, "response does not serialise");
            crate::error::ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response()
        }
    }
}

/// The `user_ids` of a `GroupModifyMembers` body, validated the way both membership handlers
/// validate it: the body must decode to a non-null object (`SetInvalidParamWithErr`, named for
/// the handler), and every id must pass `IsValidId` (`SetInvalidParamWithDetails("user_id")`).
/// Both are the same `invalid_body_param` id on the wire; the parameter name is not.
fn member_ids_from_body(bytes: &[u8], handler: &str) -> Result<Vec<String>, Refusal> {
    let body: Option<GroupModifyMembers> = decode_one_from_json(bytes)
        .map_err(|_| Box::new(crate::error::ApiError::invalid_param(handler).into_response()))?;
    let Some(body) = body else {
        return Err(Box::new(
            crate::error::ApiError::invalid_param(handler).into_response(),
        ));
    };
    let user_ids = body.user_ids.unwrap_or_default();
    if let Some(bad) = user_ids.iter().find(|id| !is_valid_id(id)) {
        tracing::debug!(user_id = %bad, "UserID is invalid");
        return Err(Box::new(
            crate::error::ApiError::invalid_param("user_id").into_response(),
        ));
    }
    Ok(user_ids)
}

/// Port of `createGroup` (group.go:158) — `POST /api/v4/groups`.
///
/// # The gate is the first statement, and the body is read second
///
/// `requireLicense` opens all seven of `api4/group.go`'s CRUD and membership writes exactly as it
/// opens the ten reads, so an unlicensed server answers the same 501 to a `POST` with a valid
/// body, a `POST` with malformed JSON and a `POST` with no body at all. **The body is never
/// read.** Measured on the unlicensed pair in `parity::group_writes`.
///
/// # Behind the gate, in Go's order
///
/// 1. the body must decode to a non-null `GroupWithUserIds` (`invalid_body_param`, "group");
/// 2. `source` must be `custom` — `app.group.crud_permission` at 400, **before** the licence
///    tier is consulted, so an `ldap` body is refused for its source and not for `LDAPGroups`;
/// 3. `licensedAndConfiguredForGroupBySource` (`Api4.createGroup`);
/// 4. `create_custom_group` — a *system* permission, since there is no group yet;
/// 5. `allow_reference` must be true (`api.custom_groups.must_be_referenceable`);
/// 6. `remote_id` must be empty (`api.custom_groups.no_remote_id`);
///
/// then `CreateGroupWithUserIds`, and a **201** with the created group including its
/// `member_count`. Measured against the licensed pair in `parity::group_writes_licensed`.
///
/// The audit record (`AuditEventCreateGroup`) is not ported — no audit record is, yet.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_group(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "createGroup";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let bytes = match read_body(request, WHERE).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let group: Option<GroupWithUserIds> = match decode_one_from_json(&bytes) {
        Ok(group) => group,
        Err(_) => return crate::error::ApiError::invalid_param("group").into_response(),
    };
    let Some(group) = group else {
        return crate::error::ApiError::invalid_param("group").into_response();
    };

    if group.group.source.as_str() != GroupSource::CUSTOM {
        return crud_permission_error(WHERE, 400);
    }
    if let Err(response) =
        licensed_and_configured(&state, &group.group.source, "Api4.createGroup").await
    {
        return *response;
    }
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_CREATE_CUSTOM_GROUP)
        .await
    {
        return permission_error(&session.0, &PERMISSION_CREATE_CUSTOM_GROUP);
    }
    if !group.group.allow_reference {
        return crate::error::ApiError::from(AppError::new(
            WHERE,
            "api.custom_groups.must_be_referenceable",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }
    if !group.group.get_remote_id().is_empty() {
        return crate::error::ApiError::from(AppError::new(
            WHERE,
            "api.custom_groups.no_remote_id",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match state.app.create_group_with_user_ids(group).await {
        Ok(created) => json_response(StatusCode::CREATED, &created, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// Port of `getGroupsByNames` (group.go:920) — `POST /api/v4/groups/names`.
///
/// A `POST` that is a read, which is why it is in this family rather than with the other writes.
/// `/names` is a **literal** beside `/{group_id:[A-Za-z0-9]+}`, and `names` would itself match
/// that class — so Go's answer depends on the method: `POST /groups/names` is this handler, while
/// `DELETE /groups/names` is [`delete_group`] with `group_id = "names"`.
///
/// The literal shadows its parameterised sibling for **every** method, not just this one: axum
/// prefers a static segment over `{group_id}` and does not backtrack across method routers. So
/// registering `/names` for `POST` alone would have handed `GET /groups/names` — which
/// [`get_group`] served until this route existed — to the fallback. [`get_group_named_names`] and
/// [`delete_group_named_names`] re-claim the two methods gorilla *does* route there; `PUT` and the
/// rest stay forwarded, because gorilla routes them nowhere. Measured in `parity::group_writes`.
///
/// Behind the gate: the body is `SortedArrayFromJSON` (a parse failure is `PayloadParseError` at
/// 400), an **empty** list short-circuits to a literal `[]` **before** the permission question,
/// and otherwise the list is filtered to referenceable groups unless the caller holds
/// `sysconsole_read_user_management_groups`. No `DeleteAt` filter — a deleted group is still
/// found by name.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_groups_by_names(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "getGroupsByNames";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let bytes = match read_body(request, WHERE).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let names = match sorted_array_from_json(&bytes) {
        Ok(names) => names,
        Err(_) => {
            return crate::error::ApiError::from(AppError::new(
                WHERE,
                PAYLOAD_PARSE_ERROR,
                None,
                String::new(),
                400,
            ))
            .into_response();
        }
    };
    if names.is_empty() {
        return (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            "[]",
        )
            .into_response();
    }

    let filter_allow_reference = !state
        .app
        .session_has_permission_to(
            &session.0,
            &PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS,
        )
        .await;
    match state
        .app
        .get_groups_by_names(&names, filter_allow_reference)
        .await
    {
        Ok(groups) => json_response(StatusCode::OK, &groups, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// [`get_group`] at the one path where axum's static-over-parameter preference would otherwise
/// have taken it away: `GET /api/v4/groups/names`.
///
/// `names` matches `{group_id:[A-Za-z0-9]+}`, so gorilla routes this to `getGroup` with
/// `group_id = "names"` — a group id that is legal to *ask for* and can never exist, since ids
/// are 26 characters. Behind the licence gate it would be a 404 from the store; in front of it,
/// it is the same 501 as every other group route.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_group_named_names(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    answer(&state, request).await
}

/// [`delete_group`] at `DELETE /api/v4/groups/names`, for the reason
/// [`get_group_named_names`] gives. Licensed, `RequireGroupId` refuses `names` — five characters
/// — with `invalid_url_param`, before the group is looked up.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn delete_group_named_names(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    delete_group_by_id(&state, "names", &session).await
}

/// Port of `patchGroup` (group.go:220) — `PUT /api/v4/groups/{group_id}/patch`.
///
/// The branchiest of the seven, in Go's order behind the gate:
///
/// 1. `RequireGroupId`, then the group (404 `app.group.no_rows`);
/// 2. `licensedAndConfiguredForGroupBySource` on the group's **own** source;
/// 3. the permission is chosen by source — `edit_custom_group` for `custom`, held through
///    membership (`SessionHasPermissionToGroup`), `sysconsole_write_user_management_groups` for
///    everything else;
/// 4. only now the body: a `GroupPatch` (`invalid_body_param`, "group");
/// 5. a custom group may not turn `allow_reference` **off** (`must_be_referenceable`);
/// 6. turning it **on** without a `name` derives one — `strings.ToLower(DisplayName)` with
///    spaces replaced by hyphens, no other transform, so a display name with a `.` or a `/`
///    yields a name `IsValidForUpdate` then refuses; turning it on *with* a name checks the
///    name against the three reserved mentions, every username, and every referenceable group
///    — **including the group's own current name**, so re-asserting a name is a refusal;
/// 7. `Patch`, then `UpdateGroup`.
///
/// Not ported: the audit record.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn patch_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.patchGroup";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let mut group = match group_for_write(&state, &group_id).await {
        Ok(group) => group,
        Err(response) => return *response,
    };
    if let Err(response) = licensed_and_configured(&state, &group.source, WHERE).await {
        return *response;
    }
    let required = if group.source.as_str() == GroupSource::CUSTOM {
        &PERMISSION_EDIT_CUSTOM_GROUP
    } else {
        &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS
    };
    if !state
        .app
        .session_has_permission_to_group(&session.0, &group_id, required)
        .await
    {
        return permission_error(&session.0, required);
    }

    let bytes = match read_body(request, WHERE).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let mut patch: GroupPatch = match decode_one_from_json(&bytes) {
        Ok(patch) => patch,
        Err(_) => return crate::error::ApiError::invalid_param("group").into_response(),
    };

    if group.source.as_str() == GroupSource::CUSTOM && patch.allow_reference == Some(false) {
        return crate::error::ApiError::from(AppError::new(
            WHERE,
            "api.custom_groups.must_be_referenceable",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    if patch.allow_reference == Some(true) {
        match &patch.name {
            None => {
                patch.name = Some(derive_group_name(&group.display_name));
            }
            Some(name) => {
                if name == USER_NOTIFY_ALL
                    || name == CHANNEL_MENTIONS_NOTIFY_PROP
                    || name == USER_NOTIFY_HERE
                {
                    return patch_name_error(WHERE, "api.ldap_groups.existing_reserved_name_error");
                }
                // `user, _ := c.App.GetUserByUsername(...)` — the error is discarded, so a store
                // fault reads as "no such user" and the check passes.
                if state.app.get_user_by_username(name).await.is_ok() {
                    return patch_name_error(WHERE, "api.ldap_groups.existing_user_name_error");
                }
                if state.app.get_group_by_name(name, true).await.is_ok() {
                    return patch_name_error(WHERE, "api.ldap_groups.existing_group_name_error");
                }
            }
        }
    }

    group.patch(&patch);
    match state.app.update_group(group).await {
        Ok(updated) => json_response(StatusCode::OK, &updated, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// `strings.ReplaceAll(strings.ToLower(group.DisplayName), " ", "-")` (group.go:265): the name
/// `patchGroup` derives when `allow_reference` is turned on without one. Only ASCII spaces are
/// replaced — a tab or a non-breaking space survives into a name `IsValidForUpdate` refuses.
///
/// `go_to_lower`, not `str::to_lowercase`: Go's is the simple one-rune mapping, so `İ` becomes
/// `i` and a final `Σ` becomes `σ`, where Rust's full mapping gives `i̇` and `ς`. Measured against
/// `fixtures/behaviour_group.json`'s `derived_name` corpus.
pub(crate) fn derive_group_name(display_name: &str) -> String {
    mm_model::utils::go_to_lower(display_name).replace(' ', "-")
}

fn patch_name_error(where_: &str, id: &str) -> Response {
    crate::error::ApiError::from(AppError::new(where_, id, None, String::new(), 400))
        .into_response()
}

/// Port of `deleteGroup` (group.go:1295) — `DELETE /api/v4/groups/{group_id}`.
///
/// Shares its path with [`get_group`], and the two differ only in method. Behind the gate:
/// `RequireGroupId`, the group, source must be `custom` (`crud_permission` at **400**),
/// `licensedAndConfiguredForGroupBySource` for `custom`, `delete_custom_group` through
/// membership, then `DeleteGroup` — which is a 404 for a group already deleted, since the
/// store's select carries `DeleteAt = 0` while `GetGroup`'s does not.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    delete_group_by_id(&state, &group_id, &session).await
}

async fn delete_group_by_id(
    state: &AppState,
    group_id: &str,
    session: &AuthenticatedSession,
) -> Response {
    const WHERE: &str = "Api4.deleteGroup";
    if let Err(response) = require_licence(state).await {
        return *response;
    }
    let group = match group_for_write(state, group_id).await {
        Ok(group) => group,
        Err(response) => return *response,
    };
    if group.source.as_str() != GroupSource::CUSTOM {
        return crud_permission_error(WHERE, 400);
    }
    if let Err(response) =
        licensed_and_configured(state, &GroupSource(GroupSource::CUSTOM.to_owned()), WHERE).await
    {
        return *response;
    }
    if !state
        .app
        .session_has_permission_to_group(&session.0, group_id, &PERMISSION_DELETE_CUSTOM_GROUP)
        .await
    {
        return permission_error(&session.0, &PERMISSION_DELETE_CUSTOM_GROUP);
    }
    match state.app.delete_group(group_id).await {
        Ok(deleted) => json_response(StatusCode::OK, &deleted, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// Port of `restoreGroup` (group.go:1349) — `POST /api/v4/groups/{group_id}/restore`.
///
/// **Its non-custom refusal is a 501, not a 400.** Every other handler in the family answers
/// `app.group.crud_permission` at 400 when the group is not a custom one; `restoreGroup` answers
/// the same id at `http.StatusNotImplemented`, which on the wire is indistinguishable in status
/// from the licence error it sits behind. Measured against the licensed pair. A group that is
/// not deleted is a 404 from the store.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn restore_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    const WHERE: &str = "Api4.restoreGroup";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let group = match group_for_write(&state, &group_id).await {
        Ok(group) => group,
        Err(response) => return *response,
    };
    if group.source.as_str() != GroupSource::CUSTOM {
        return crud_permission_error(WHERE, 501);
    }
    if let Err(response) =
        licensed_and_configured(&state, &GroupSource(GroupSource::CUSTOM.to_owned()), WHERE).await
    {
        return *response;
    }
    if !state
        .app
        .session_has_permission_to_group(&session.0, &group_id, &PERMISSION_RESTORE_CUSTOM_GROUP)
        .await
    {
        return permission_error(&session.0, &PERMISSION_RESTORE_CUSTOM_GROUP);
    }
    match state.app.restore_group(&group_id).await {
        Ok(restored) => json_response(StatusCode::OK, &restored, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// Port of `addGroupMembers` (group.go:1405) — `POST /api/v4/groups/{group_id}/members`.
///
/// Three methods share this path: `GET` is [`get_group_members`], `POST` is this, `DELETE` is
/// [`delete_group_members`]. Behind the gate, in order: the group, `custom` only
/// (`crud_permission` at 400), the licence tier, `manage_custom_group_members` through
/// membership, and **only then** the body — so a caller without the permission never learns
/// whether their JSON was well-formed. Every id must pass `IsValidId` (`invalid_body_param`,
/// "user_id"). `UpsertGroupMembers` answers `app.group.user_not_found` at 400 for an id naming
/// no active user, and a 500 for an id listed twice.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn add_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.addGroupMembers";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let group = match group_for_write(&state, &group_id).await {
        Ok(group) => group,
        Err(response) => return *response,
    };
    if group.source.as_str() != GroupSource::CUSTOM {
        return crud_permission_error(WHERE, 400);
    }
    if let Err(response) =
        licensed_and_configured(&state, &GroupSource(GroupSource::CUSTOM.to_owned()), WHERE).await
    {
        return *response;
    }
    if !state
        .app
        .session_has_permission_to_group(
            &session.0,
            &group_id,
            &PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS,
        )
        .await
    {
        return permission_error(&session.0, &PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS);
    }
    let bytes = match read_body(request, WHERE).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let user_ids = match member_ids_from_body(&bytes, "addGroupMembers") {
        Ok(ids) => ids,
        Err(response) => return *response,
    };
    match state.app.upsert_group_members(&group_id, &user_ids).await {
        Ok(members) => json_response(StatusCode::OK, &members, WHERE),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// Port of `deleteGroupMembers` (group.go:1473) — `DELETE /api/v4/groups/{group_id}/members`.
///
/// A `DELETE` that carries a JSON body (`{"user_ids":[…]}`), which is unusual enough that a proxy
/// dropping the body on `DELETE` would break it. The same shape as [`add_group_members`] gate
/// for gate; the store refuses with `app.group.user_not_found` for an id that has no **active**
/// membership, an already-removed member included.
///
/// Its marshal-failure branch names **`Api4.addGroupMembers`** (group.go:1516) — copied from its
/// neighbour. `where` is not on the wire; the name is kept for the trace.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn delete_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.deleteGroupMembers";
    if let Err(response) = require_licence(&state).await {
        return *response;
    }
    let group = match group_for_write(&state, &group_id).await {
        Ok(group) => group,
        Err(response) => return *response,
    };
    if group.source.as_str() != GroupSource::CUSTOM {
        return crud_permission_error(WHERE, 400);
    }
    if let Err(response) =
        licensed_and_configured(&state, &GroupSource(GroupSource::CUSTOM.to_owned()), WHERE).await
    {
        return *response;
    }
    if !state
        .app
        .session_has_permission_to_group(
            &session.0,
            &group_id,
            &PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS,
        )
        .await
    {
        return permission_error(&session.0, &PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS);
    }
    let bytes = match read_body(request, WHERE).await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let user_ids = match member_ids_from_body(&bytes, "deleteGroupMembers") {
        Ok(ids) => ids,
        Err(response) => return *response,
    };
    match state.app.delete_group_members(&group_id, &user_ids).await {
        // `buildDeleteMembersQuery` declares `members` as a named nil slice and `Select` leaves it
        // nil when nothing matched — which, for an empty `user_ids`, is the only way to get here
        // without an error — so Go marshals **`null`**, where `addGroupMembers`' `make([]…, 0)`
        // marshals `[]`. Measured on the licensed pair.
        Ok(members) if members.is_empty() => (
            StatusCode::OK,
            [
                ("Content-Type", "application/json"),
                ("x-mmrs-served-by", "rust"),
            ],
            "null",
        )
            .into_response(),
        Ok(members) => json_response(StatusCode::OK, &members, "Api4.addGroupMembers"),
        Err(err) => crate::error::ApiError::from(err).into_response(),
    }
}

/// Port of `linkGroupSyncable` (group.go:319) —
/// `POST /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link`.
///
/// # The gate is still the first statement, and here it precedes *four* things
///
/// `requireLicense` opens this handler ahead of `RequireGroupId`, `RequireSyncableId`,
/// `RequireSyncableType` **and** `io.ReadAll(r.Body)` — in that order in the source, all of them
/// below the gate. So on an unlicensed server a `POST` with a malformed body, an unroutable
/// `group_id` and an empty `syncable_id` is the same 501 as a well-formed link request, and the
/// body is never read. Measured in `parity::group_syncables`.
///
/// # `RequireSyncableType` can never fail through the mux
///
/// `Params.SyncableType` is not the URL segment: `params.go:269` maps `teams` → `Team` and
/// `channels` → `Channel` and leaves it **empty for anything else**, while the route pattern
/// `{syncable_type:teams|channels}` already refused anything else. So the `syncable_type` arm of
/// `SetInvalidURLParam` is dead code reachable only from a non-mux caller — which is why a third
/// value is forwarded for gorilla's 404 here rather than answered with a 400.
///
/// # What is behind the gate
///
/// This is the deepest unported handler in the family: `verifyLinkUnlinkPermission` (five
/// permission questions whose shape depends on the syncable type and, for a channel, on whether
/// the parent *team* is already synced), `verifySchemeAdminAssignmentPermission`, the
/// read-modify-write over `GetGroupSyncable`/`UpsertGroupSyncable` whose re-link of a
/// soft-deleted row deliberately starts from a zero value, and the asynchronous
/// `SyncRolesAndMembership`. None of it is reachable without a licence; see [D-390].
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn link_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = (&group_id, &syncable_id);
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    answer(&state, request).await
}

/// Port of `unlinkGroupSyncable` (group.go:624) —
/// `DELETE /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link`.
///
/// Shares its path with [`link_group_syncable`] and differs only in method, exactly as
/// [`get_group`] and [`delete_group`] do. It is the one of the three that **never reads a body**
/// even behind the gate, and the only one whose success is `ReturnStatusOK` rather than a
/// marshalled syncable — so the three routes have three response shapes: a 201 with a body, a 200
/// with a body, and a 200 with `{"status":"OK"}`.
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn unlink_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = (&group_id, &syncable_id);
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    answer(&state, request).await
}

/// Port of `patchGroupSyncable` (group.go:527) —
/// `PUT /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/patch`.
///
/// The `patch` literal here sits at a **fourth** path segment under two parameters, which is a
/// different position from `/groups/{group_id}/patch` ([`patch_group`]); the two never compete,
/// and a request reaches this one only with a `syncable_type` and a `syncable_id` between them.
///
/// Behind the gate it differs from [`link_group_syncable`] in exactly one way that matters:
/// `GetGroupSyncable` failing is fatal here (there is nothing to patch), where the link handler
/// tolerates a 404 and creates the row. Both then run the same two permission verifiers and the
/// same asynchronous `SyncRolesAndMembership`. See [D-390].
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn patch_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = (&group_id, &syncable_id);
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    answer(&state, request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The derived name, against Go's own answers — including the two inputs where a full
    /// Unicode case mapping would differ from Go's simple one.
    #[test]
    fn the_derived_name_matches_gos_to_lower() {
        let corpus: serde_json::Value =
            serde_json::from_str(include_str!("../../../fixtures/behaviour_group.json")).unwrap();
        let cases = corpus["derived_name"].as_array().unwrap();
        assert!(cases.len() >= 14);
        for case in cases {
            let input = case["in"].as_str().unwrap();
            assert_eq!(
                derive_group_name(input),
                case["out"].as_str().unwrap(),
                "{input:?}"
            );
        }
        // The two that separate the mappings, named so a corpus regeneration cannot lose them.
        assert_eq!(derive_group_name("İstanbul Group"), "istanbul-group");
        assert_eq!(derive_group_name("ΣΊΣΥΦΟΣ"), "σίσυφοσ");
    }

    /// `restoreGroup` alone answers `crud_permission` at 501; the others at 400. The status is a
    /// parameter so that the helper cannot quietly pin one for everybody.
    #[test]
    fn the_crud_refusal_carries_the_status_it_is_given() {
        for status in [400u16, 501] {
            let response = crud_permission_error("Api4.restoreGroup", i32::from(status));
            assert_eq!(response.status().as_u16(), status);
        }
    }

    /// The membership body: `null` and non-objects are the handler's own parameter name, a bad
    /// id inside the list is `user_id`, an absent or null list is empty rather than an error.
    #[test]
    fn member_ids_are_validated_the_way_both_handlers_validate_them() {
        let ids = member_ids_from_body(
            br#"{"user_ids":["aaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbb"]}"#,
            "addGroupMembers",
        )
        .ok()
        .unwrap();
        assert_eq!(ids.len(), 2);
        assert!(
            member_ids_from_body(b"{}", "addGroupMembers")
                .ok()
                .unwrap()
                .is_empty()
        );
        assert!(
            member_ids_from_body(br#"{"user_ids":null}"#, "addGroupMembers")
                .ok()
                .unwrap()
                .is_empty()
        );
        for bad in [
            &b"null"[..],
            b"{",
            b"",
            b"[1]",
            br#"{"user_ids":["abc"]}"#,
            br#"{"user_ids":[""]}"#,
        ] {
            let refused = member_ids_from_body(bad, "addGroupMembers").expect_err("refused");
            assert_eq!(
                refused.status().as_u16(),
                400,
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    /// `teams|channels` is two literals, and nothing else routes. A port that read the pattern as
    /// a parameter would answer the licence error to `/groups/{id}/foo`, which Go 404s before any
    /// handler runs.
    #[test]
    fn the_syncable_type_is_two_literals_and_nothing_else() {
        assert!(super::syncable_type_matches_go_mux("teams"));
        assert!(super::syncable_type_matches_go_mux("channels"));
        for other in [
            "Teams",
            "team",
            "channel",
            "teams|channels",
            "",
            "members",
            "stats",
        ] {
            assert!(
                !super::syncable_type_matches_go_mux(other),
                "{other:?} never matched gorilla"
            );
        }
    }
}
