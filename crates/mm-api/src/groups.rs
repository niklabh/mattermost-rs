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
