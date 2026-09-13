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
use mm_model::channel::CHANNEL_TYPE_PRIVATE;
use mm_model::group::{Group, GroupModifyMembers, GroupPatch, GroupSource, GroupWithUserIds};
use mm_model::group_syncable::{GroupSyncable, GroupSyncablePatch, GroupSyncableType};
use mm_model::license::License;
use mm_model::permission::{
    PERMISSION_CREATE_CUSTOM_GROUP, PERMISSION_DELETE_CUSTOM_GROUP, PERMISSION_EDIT_CUSTOM_GROUP,
    PERMISSION_INVITE_USER, PERMISSION_MANAGE_CHANNEL_ROLES,
    PERMISSION_MANAGE_CUSTOM_GROUP_MEMBERS, PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS,
    PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS, PERMISSION_MANAGE_TEAM_ROLES,
    PERMISSION_RESTORE_CUSTOM_GROUP, PERMISSION_SYSCONSOLE_READ_USER_MANAGEMENT_GROUPS,
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
use crate::error::ApiError;

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

/// The two literal segments gorilla routes, as the `GroupSyncableType` `params.go:269` maps
/// them to. Only called after [`syncable_type_matches_go_mux`] has admitted the segment.
fn syncable_type_of(segment: &str) -> GroupSyncableType {
    if segment == "teams" {
        GroupSyncableType::from(GroupSyncableType::TEAM)
    } else {
        GroupSyncableType::from(GroupSyncableType::CHANNEL)
    }
}

/// The licence, or the answer the request gets without one.
///
/// `requireLicense` is the first statement of all three handlers, above `RequireGroupId`,
/// `RequireSyncableId`, `RequireSyncableType` **and** `io.ReadAll(r.Body)` — so an unlicensed
/// server answers one 501 to every input, and nothing else on the request is consulted.
/// Measured in `parity::group_syncables`.
async fn require_license(state: &AppState) -> Result<std::sync::Arc<License>, ApiError> {
    match state.app.license().await {
        Ok(Some(license)) => {
            tracing::Span::current().record("licensed", true);
            Ok(license)
        }
        Ok(None) => {
            tracing::Span::current().record("licensed", false);
            Err(ApiError::from(AppError::new(
                "",
                "api.license_error",
                None,
                String::new(),
                501,
            )))
        }
        Err(err) => Err(ApiError::from(err)),
    }
}

/// `!*c.App.Channels().License().Features.LDAPGroups` → 403 `api.ldap_groups.license_error`.
///
/// The one licence-*feature* gate in the family, checked after the body is parsed and before
/// any permission. `Features.SetDefaults` has run on a loaded licence, so the flag is never
/// absent; an absent one would be a nil dereference in Go and reads as `false` here.
fn require_ldap_groups(license: &License, where_: &'static str) -> Result<(), ApiError> {
    let enabled = license
        .features
        .as_ref()
        .and_then(|f| f.ldap_groups)
        .unwrap_or(false);
    if enabled {
        Ok(())
    } else {
        Err(ApiError::from(AppError::new(
            where_,
            "api.ldap_groups.license_error",
            None,
            String::new(),
            403,
        )))
    }
}

/// `RequireGroupId().RequireSyncableId()`, in that order — two `SetInvalidURLParam`s whose only
/// wire difference is which of them fires first when both ids are malformed.
fn require_group_and_syncable_ids(group_id: &str, syncable_id: &str) -> Result<(), ApiError> {
    require_id(group_id, "group_id")?;
    require_id(syncable_id, "syncable_id")?;
    Ok(())
}

/// What a verifier decided: proceed, refuse with Go's error, or hand the request to Go.
enum Verdict {
    Proceed,
    Refuse(Box<AppError>),
    /// The `!group.AllowReference` arm asks `SessionHasPermissionToGroup`, which is not ported
    /// here (it belongs to the custom-group permission model, [D-360]); a group that hides its
    /// members is forwarded whole, before any write. See [D-531].
    Forward(&'static str),
}

/// Port of `verifyLinkUnlinkPermission` (api4/group.go:680).
///
/// # The channel arm asks a team question with the channel's id
///
/// `SessionHasPermissionToTeam(session, syncableID, PermissionInviteUser)` is called with
/// **`syncableID`** — the channel id — in the channel arm, where the team's id is one field away
/// on the channel just fetched. That is Go's line, reproduced: the session's team memberships
/// never match a channel id, so the check falls through to the caller's system roles, and a team
/// admin who is not a system admin is refused their first channel link where the source reads as
/// if they should be allowed. `parity::group_syncables` measures it.
///
/// # The parent-team question
///
/// A channel whose team is not yet synced to the group asks for `invite_user` on the team (or
/// the sysconsole write permission); a channel whose team already is asks only the channel's own
/// `manage_{private,public}_channel_members`. "Already synced" means a `GroupTeams` row exists —
/// **deleted or not**, since `GetGroupSyncable` has no `DeleteAt` filter — so an unlinked team
/// still exempts its channels from the team question.
async fn verify_link_unlink_permission(
    state: &AppState,
    session: &Session,
    group_id: &str,
    syncable_type: &GroupSyncableType,
    syncable_id: &str,
) -> Result<Verdict, Box<AppError>> {
    let group = state.app.get_group(group_id).await?;

    if !group.is_syncable() {
        return Ok(Verdict::Refuse(AppError::boxed(
            "Api4.linkGroupSyncable",
            "app.group.crud_permission",
            None,
            String::new(),
            400,
        )));
    }

    // If AllowReference is disabled, limit who can link the group.
    if !group.allow_reference {
        return Ok(Verdict::Forward(
            "the group hides its members and SessionHasPermissionToGroup is not ported",
        ));
    }

    match syncable_type.as_str() {
        GroupSyncableType::TEAM => {
            if !state
                .app
                .session_has_permission_to_team(session, syncable_id, &PERMISSION_INVITE_USER)
                .await
                && !state
                    .app
                    .session_has_permission_to(
                        session,
                        &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS,
                    )
                    .await
            {
                return Ok(Verdict::Refuse(make_permission_error(
                    session,
                    &[&PERMISSION_INVITE_USER],
                )));
            }
        }
        GroupSyncableType::CHANNEL => {
            let channel = state.app.get_channel(syncable_id).await?;

            let team_type = GroupSyncableType::from(GroupSyncableType::TEAM);
            match state
                .app
                .get_group_syncable(group_id, &channel.team_id, &team_type)
                .await
            {
                Ok(_) => {}
                Err(err) if err.status_code == 404 => {
                    // Go's line passes `syncableID` — the channel's id — as the team id.
                    if !state
                        .app
                        .session_has_permission_to_team(
                            session,
                            syncable_id,
                            &PERMISSION_INVITE_USER,
                        )
                        .await
                        && !state
                            .app
                            .session_has_permission_to(
                                session,
                                &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS,
                            )
                            .await
                    {
                        return Ok(Verdict::Refuse(make_permission_error(
                            session,
                            &[&PERMISSION_INVITE_USER],
                        )));
                    }
                }
                Err(err) => return Err(err),
            }

            let permission = if channel.channel_type == CHANNEL_TYPE_PRIVATE {
                &PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS
            } else {
                &PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS
            };
            let (ok, _) = state
                .app
                .session_has_permission_to_channel(session, syncable_id, permission)
                .await;
            if !ok {
                return Ok(Verdict::Refuse(make_permission_error(
                    session,
                    &[permission],
                )));
            }
        }
        _ => {}
    }
    Ok(Verdict::Proceed)
}

/// Port of `verifySchemeAdminAssignmentPermission` (api4/group.go:747): a no-op unless the
/// patch names `scheme_admin`; then the sysconsole write permission, or the syncable's own
/// role-management permission.
async fn verify_scheme_admin_assignment_permission(
    state: &AppState,
    session: &Session,
    syncable_type: &GroupSyncableType,
    syncable_id: &str,
    patch: &GroupSyncablePatch,
) -> Option<Box<AppError>> {
    patch.scheme_admin?;
    if state
        .app
        .session_has_permission_to(session, &PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS)
        .await
    {
        return None;
    }
    match syncable_type.as_str() {
        GroupSyncableType::TEAM => {
            if !state
                .app
                .session_has_permission_to_team(session, syncable_id, &PERMISSION_MANAGE_TEAM_ROLES)
                .await
            {
                return Some(make_permission_error(
                    session,
                    &[&PERMISSION_MANAGE_TEAM_ROLES],
                ));
            }
        }
        GroupSyncableType::CHANNEL => {
            let (ok, _) = state
                .app
                .session_has_permission_to_channel(
                    session,
                    syncable_id,
                    &PERMISSION_MANAGE_CHANNEL_ROLES,
                )
                .await;
            if !ok {
                return Some(make_permission_error(
                    session,
                    &[&PERMISSION_MANAGE_CHANNEL_ROLES],
                ));
            }
        }
        _ => {}
    }
    None
}

/// `appErr.Where = "Api4.linkGroupSyncable"` and its siblings — Go relabels the verifiers'
/// errors with the handler's name. `Where` is not on the wire; it is kept for the trace.
fn relabelled(mut err: Box<AppError>, where_: &str) -> Response {
    err.where_ = where_.to_owned();
    ApiError::from(err).into_response()
}

/// `json.Marshal` + `w.Write` — **no trailing newline** — at the status the handler chose.
fn marshalled(status: StatusCode, where_: &'static str, syncable: &GroupSyncable) -> Response {
    match serde_json::to_vec(syncable) {
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
            tracing::error!(error = %err, "failed to serialise the group syncable");
            ApiError::from(AppError::new(
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

/// `c.App.Srv().Go(func() { SyncRolesAndMembership(...) })` — after the response, on a task of
/// its own. Its failures are logged by the task, never surfaced.
fn spawn_sync(
    state: &AppState,
    syncable_id: String,
    syncable_type: GroupSyncableType,
    group_id: String,
    sync_roles: bool,
) {
    let app = state.app.clone();
    tokio::spawn(async move {
        app.sync_roles_and_membership(&syncable_id, &syncable_type, &group_id, sync_roles)
            .await;
    });
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
/// # A re-link starts from nothing
///
/// The upsert is onto the existing row only when it is live. A fresh link *or a re-link of a
/// soft-deleted row* starts from a zero-value syncable, so `scheme_admin` from before an unlink
/// is not carried over unless the caller sets it again (group.go:385). The store's update then
/// restores the row by writing `DeleteAt = 0`.
///
/// # 201, and two events for a channel
///
/// The only one of the three that answers **201**. Linking a channel also links its team
/// (`App::upsert_group_syncable`), so the socket sees `received_group_associated_to_team` and
/// then `..._to_channel`. The membership sync runs after the response — see
/// [`mm_app::App::sync_roles_and_membership`].
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn link_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.linkGroupSyncable";
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let license = match require_license(&state).await {
        Ok(license) => license,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = require_group_and_syncable_ids(&group_id, &syncable_id) {
        return err.into_response();
    }
    let syncable_type = syncable_type_of(&syncable_type);

    // The forward, if there is one, needs the request back — so the body is read from a clone
    // of it only once the decision to answer here is made... except that the decision depends
    // on the group, which is read after the body in Go. Go's order is kept: the body is parsed
    // first, and a forward re-sends the bytes it read.
    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::from(AppError::new(
                "Api4.createGroupSyncable",
                "api.io_error",
                None,
                String::new(),
                400,
            ))
            .into_response();
        }
    };
    let patch = match serde_json::from_slice::<Option<GroupSyncablePatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) | Err(_) => {
            return ApiError::invalid_param(&format!("Group{syncable_type}")).into_response();
        }
    };

    if let Err(err) = require_ldap_groups(&license, "Api4.createGroupSyncable") {
        return err.into_response();
    }

    match verify_link_unlink_permission(&state, &session.0, &group_id, &syncable_type, &syncable_id)
        .await
    {
        Ok(Verdict::Proceed) => {}
        Ok(Verdict::Refuse(err)) | Err(err) => return relabelled(err, WHERE),
        Ok(Verdict::Forward(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return crate::proxy::forward_to_go(
                State(state),
                Request::from_parts(parts, axum::body::Body::from(bytes)),
            )
            .await;
        }
    }
    if let Some(err) = verify_scheme_admin_assignment_permission(
        &state,
        &session.0,
        &syncable_type,
        &syncable_id,
        &patch,
    )
    .await
    {
        return relabelled(err, WHERE);
    }

    let existing = match state
        .app
        .get_group_syncable(&group_id, &syncable_id, &syncable_type)
        .await
    {
        Ok(existing) => Some(existing),
        Err(err) if err.status_code == 404 => None,
        Err(err) => return relabelled(err, WHERE),
    };
    let mut group_syncable = match existing {
        Some(existing) if existing.delete_at == 0 => existing,
        _ => GroupSyncable {
            group_id: group_id.clone(),
            syncable_id: syncable_id.clone(),
            type_: syncable_type.clone(),
            ..GroupSyncable::default()
        },
    };
    group_syncable.patch(&patch);
    let group_syncable = match state.app.upsert_group_syncable(group_syncable).await {
        Ok(gs) => gs,
        Err(err) => return ApiError::from(err).into_response(),
    };

    spawn_sync(
        &state,
        syncable_id,
        syncable_type,
        group_id,
        patch.scheme_admin.is_some(),
    );
    marshalled(
        StatusCode::CREATED,
        "Api4.createGroupSyncable",
        &group_syncable,
    )
}

/// Port of `unlinkGroupSyncable` (group.go:624) —
/// `DELETE /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/link`.
///
/// Shares its path with [`link_group_syncable`] and differs only in method, exactly as
/// [`get_group`] and [`delete_group`] do. It is the one of the three that **never reads a body**
/// even behind the gate, and the only one whose success is `ReturnStatusOK` rather than a
/// marshalled syncable — so the three routes have three response shapes: a 201 with a body, a 200
/// with a body, and a 200 with `{"status":"OK"}`.
///
/// Only [`verify_link_unlink_permission`] guards it — there is no patch, so no scheme-admin
/// question. Unlinking a team also soft-deletes the group's channel links
/// (`App::delete_group_syncable`), and the membership removal runs after the response.
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn unlink_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.unlinkGroupSyncable";
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let license = match require_license(&state).await {
        Ok(license) => license,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = require_group_and_syncable_ids(&group_id, &syncable_id) {
        return err.into_response();
    }
    let syncable_type = syncable_type_of(&syncable_type);

    if let Err(err) = require_ldap_groups(&license, WHERE) {
        return err.into_response();
    }
    match verify_link_unlink_permission(&state, &session.0, &group_id, &syncable_type, &syncable_id)
        .await
    {
        Ok(Verdict::Proceed) => {}
        Ok(Verdict::Refuse(err)) | Err(err) => return relabelled(err, WHERE),
        Ok(Verdict::Forward(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return crate::proxy::forward_to_go(State(state), request).await;
        }
    }

    if let Err(err) = state
        .app
        .delete_group_syncable(&group_id, &syncable_id, &syncable_type)
        .await
    {
        return ApiError::from(err).into_response();
    }

    let app = state.app.clone();
    tokio::spawn(async move {
        app.remove_memberships_from_unlinked_syncable(&syncable_id, &syncable_type)
            .await;
    });

    // `ReturnStatusOK` — `w.Write(MapToJSON(...))`, no trailing newline.
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

/// Port of `patchGroupSyncable` (group.go:527) —
/// `PUT /api/v4/groups/{group_id}/{syncable_type}/{syncable_id}/patch`.
///
/// The `patch` literal here sits at a **fourth** path segment under two parameters, which is a
/// different position from `/groups/{group_id}/patch` ([`patch_group`]); the two never compete,
/// and a request reaches this one only with a `syncable_type` and a `syncable_id` between them.
///
/// It differs from [`link_group_syncable`] in exactly one way that matters: `GetGroupSyncable`
/// failing is fatal here (there is nothing to patch, **404** `app.group.no_rows`), where the
/// link handler tolerates the miss and creates the row. Both run the same two verifiers and the
/// same sync afterwards; this one answers **200**. Its invalid-body parameter is
/// `Group[Team]Patch` / `Group[Channel]Patch`, with the brackets, where link's is `GroupTeam`.
#[tracing::instrument(skip_all, fields(group_id = %group_id, syncable_type = %syncable_type, licensed, forwarded))]
pub async fn patch_group_syncable(
    State(state): State<AppState>,
    Path((group_id, syncable_type, syncable_id)): Path<(String, String, String)>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    const WHERE: &str = "Api4.patchGroupSyncable";
    if !syncable_type_matches_go_mux(&syncable_type) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    let license = match require_license(&state).await {
        Ok(license) => license,
        Err(err) => return err.into_response(),
    };
    if let Err(err) = require_group_and_syncable_ids(&group_id, &syncable_id) {
        return err.into_response();
    }
    let syncable_type = syncable_type_of(&syncable_type);

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::from(AppError::new(
                WHERE,
                "api.io_error",
                None,
                String::new(),
                400,
            ))
            .into_response();
        }
    };
    let patch = match serde_json::from_slice::<Option<GroupSyncablePatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) | Err(_) => {
            return ApiError::invalid_param(&format!("Group[{syncable_type}]Patch"))
                .into_response();
        }
    };

    if let Err(err) = require_ldap_groups(&license, WHERE) {
        return err.into_response();
    }
    match verify_link_unlink_permission(&state, &session.0, &group_id, &syncable_type, &syncable_id)
        .await
    {
        Ok(Verdict::Proceed) => {}
        Ok(Verdict::Refuse(err)) | Err(err) => return relabelled(err, WHERE),
        Ok(Verdict::Forward(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            return crate::proxy::forward_to_go(
                State(state),
                Request::from_parts(parts, axum::body::Body::from(bytes)),
            )
            .await;
        }
    }
    if let Some(err) = verify_scheme_admin_assignment_permission(
        &state,
        &session.0,
        &syncable_type,
        &syncable_id,
        &patch,
    )
    .await
    {
        return relabelled(err, WHERE);
    }

    let mut group_syncable = match state
        .app
        .get_group_syncable(&group_id, &syncable_id, &syncable_type)
        .await
    {
        Ok(gs) => gs,
        Err(err) => return ApiError::from(err).into_response(),
    };
    group_syncable.patch(&patch);
    let group_syncable = match state.app.update_group_syncable(group_syncable).await {
        Ok(gs) => gs,
        Err(err) => return ApiError::from(err).into_response(),
    };

    spawn_sync(
        &state,
        syncable_id,
        syncable_type,
        group_id,
        patch.scheme_admin.is_some(),
    );
    marshalled(StatusCode::OK, WHERE, &group_syncable)
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
