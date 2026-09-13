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
//! On a licensed server these routes read the `UserGroups` tables and consult
//! `License().Features.LDAPGroups` — neither is ported — so a licensed installation is forwarded
//! whole.
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
//! What is *not* ported is everything behind the gate, which for the writes is the whole of the
//! group store and the custom-group permission model ([D-360]), and for the three syncable
//! writes the `GroupSyncable` upsert surface, the two permission verifiers and
//! `SyncRolesAndMembership` ([D-390]).
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
use mm_model::group_syncable::{GroupSyncable, GroupSyncablePatch, GroupSyncableType};
use mm_model::license::License;
use mm_model::permission::{
    PERMISSION_INVITE_USER, PERMISSION_MANAGE_CHANNEL_ROLES,
    PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS, PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS,
    PERMISSION_MANAGE_TEAM_ROLES, PERMISSION_SYSCONSOLE_WRITE_USER_MANAGEMENT_GROUPS,
    make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate, require_id};
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

/// Port of `createGroup` (group.go:158) — `POST /api/v4/groups`.
///
/// # Seven writes, and the gate is still the first statement
///
/// `requireLicense` opens all seven of `api4/group.go`'s CRUD and membership writes exactly as it
/// opens the ten reads, so an unlicensed server answers the same 501 to a `POST` with a valid
/// body, a `POST` with malformed JSON and a `POST` with no body at all. **The body is never
/// read.** That is the one thing a port can get wrong here by being helpful: parsing first and
/// returning a 400 for bad JSON would answer a request Go refuses before it looks.
///
/// Behind the gate this handler alone has five more refusals — source must be `custom`,
/// `licensedAndConfiguredForGroupBySource`, `PermissionCreateCustomGroup`, `allow_reference` must
/// be true, and `remote_id` must be empty — then a 201 rather than a 200. None of it is reachable
/// without a licence this stack cannot mint; see [D-360].
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_group(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    answer(&state, request).await
}

/// Port of `getGroupsByNames` (group.go:920) — `POST /api/v4/groups/names`.
///
/// A `POST` that is a read, which is why it is in this family rather than with the other writes.
/// `/names` is a **literal** beside `/{group_id:[A-Za-z0-9]+}`, and `names` would itself match
/// that class — so Go's answer depends on the method: `POST /groups/names` is this handler, while
/// `DELETE /groups/names` is [`delete_group`] with `group_id = "names"`. Both are the same 501
/// unlicensed, from two different handlers.
///
// The literal shadows its parameterised sibling for **every** method, not just this one: axum
/// prefers a static segment over `{group_id}` and does not backtrack across method routers. So
/// registering `/names` for `POST` alone would have handed `GET /groups/names` — which
/// [`get_group`] served until this route existed — to the fallback. [`get_group_named_names`] and
/// [`delete_group_named_names`] re-claim the two methods gorilla *does* route there; `PUT` and the
/// rest stay forwarded, because gorilla routes them nowhere. Measured in `parity::group_writes`.
///
/// Behind the gate the empty-list case short-circuits to a literal `[]` **before** the permission
/// question — `SortedArrayFromJSON` returning nothing writes `[]` and returns — so an empty array
/// is a 200 for a caller with no group permissions at all.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_groups_by_names(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    answer(&state, request).await
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
/// [`get_group_named_names`] gives.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn delete_group_named_names(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    answer(&state, request).await
}

/// Port of `patchGroup` (group.go:220) — `PUT /api/v4/groups/{group_id}/patch`.
///
/// Behind the gate this is the branchiest of the seven: the required permission is chosen by the
/// group's **source** (`PermissionEditCustomGroup` for `custom`, `PermissionSysconsoleWrite…` for
/// everything else), and turning `allow_reference` on without supplying a name derives one by
/// lower-casing the display name and replacing spaces with hyphens — a transform with no other
/// caller in api4. Neither is ported; see [D-360].
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn patch_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `deleteGroup` (group.go:1295) — `DELETE /api/v4/groups/{group_id}`.
///
/// Shares its path with [`get_group`], and the two differ only in method.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `restoreGroup` (group.go:1349) — `POST /api/v4/groups/{group_id}/restore`.
///
/// **Its non-custom refusal is a 501, not a 400.** Every other handler in the family answers
/// `app.group.crud_permission` at 400 when the group is not a custom one; `restoreGroup` answers
/// the same id at `http.StatusNotImplemented`. Recorded because it is behind the licence gate and
/// therefore not something a parity run on this stack can catch.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn restore_group(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `addGroupMembers` (group.go:1405) — `POST /api/v4/groups/{group_id}/members`.
///
/// Three methods now share this path: `GET` is [`get_group_members`], `POST` is this, `DELETE` is
/// [`delete_group_members`]. All three open with the same gate.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn add_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
}

/// Port of `deleteGroupMembers` (group.go:1473) — `DELETE /api/v4/groups/{group_id}/members`.
///
/// A `DELETE` that carries a JSON body (`{"user_ids":[…]}`), which is unusual enough that a proxy
/// dropping the body on `DELETE` would break it once the gate opens. Not reachable here.
///
/// Its marshal-failure branch names **`Api4.addGroupMembers`** (group.go:1516) — copied from its
/// neighbour. Faithful to record, unreachable to test.
#[tracing::instrument(skip_all, fields(group_id = %group_id, licensed))]
pub async fn delete_group_members(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = &group_id;
    answer(&state, request).await
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
