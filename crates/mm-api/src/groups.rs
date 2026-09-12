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
//! # Seventeen routes, one gate
//!
//! **Extended 2026-09-12 with the seven CRUD and membership writes** — `createGroup`,
//! `getGroupsByNames`, `patchGroup`, `deleteGroup`, `restoreGroup`, `addGroupMembers` and
//! `deleteGroupMembers`. They open with the same `requireLicense` and in the same position: its
//! first statement, ahead of `RequireGroupId` *and ahead of reading the request body*. So on an
//! unlicensed server all seventeen collapse to the same 501, and a write with malformed JSON is
//! refused for the licence rather than for the JSON.
//!
//! What is *not* ported is everything behind the gate, which for the writes is the whole of the
//! group store and the custom-group permission model. See [D-360].
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
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{LicenceGate, licence_gate};

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
