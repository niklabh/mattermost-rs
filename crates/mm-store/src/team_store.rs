//! Port of `SqlTeamStore` (channels/store/sqlstore/team_store.go), `GetTeamsForUser` only,
//! together with the scheme-roles machinery it exists to drive.
//!
//! # Why this is not just a SELECT
//!
//! A team member's **effective** roles are not what the `Roles` column says. They are computed
//! from three booleans on `TeamMembers`, three nullable role names on the team's `Scheme`, and
//! whatever is in `Roles` — because roles that predate the scheme migration are still sitting in
//! that column and have to be recognised and moved. `getTeamRoles` (team_store.go:100) is that
//! computation, and getting it wrong produces a **silent** permission difference rather than an
//! error: a member who should be a team admin quietly is not, or vice versa.

use mm_model::team_member::TeamMember;
use sqlx::PgPool;

use crate::error::StoreError;

/// `model.TeamGuestRoleId` (role.go:392).
pub const TEAM_GUEST_ROLE_ID: &str = "team_guest";
/// `model.TeamUserRoleId` (role.go:393).
pub const TEAM_USER_ROLE_ID: &str = "team_user";
/// `model.TeamAdminRoleId` (role.go:394).
pub const TEAM_ADMIN_ROLE_ID: &str = "team_admin";

/// Port of the unexported `rolesInfo` (team_store.go:92).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RolesInfo {
    pub roles: Vec<String>,
    pub explicit_roles: Vec<String>,
    pub scheme_guest: bool,
    pub scheme_user: bool,
    pub scheme_admin: bool,
}

/// Port of `getTeamRoles` (team_store.go:100).
///
/// Two passes, and the order of both is on the wire because the result is joined with spaces.
///
/// 1. **Split `Roles`.** A role matching one of the three scheme role ids sets the corresponding
///    scheme flag — *even when the column said false* — and is dropped from both outputs. This is
///    the un-migrated case Go's comment describes. Anything else lands in `explicit_roles` **and**
///    `roles`, in the order it appeared.
/// 2. **Append the implied roles**, guest then user then admin, each being the scheme's default
///    role name when the team has a scheme and the constant otherwise, and each skipped if it is
///    already present in `roles`.
///
/// The dedup check reads `result.roles` as it grows, so two scheme defaults that happen to be the
/// same string collapse to one — that is Go's behaviour, not an accident of this port.
pub fn get_team_roles(
    scheme_guest: bool,
    scheme_user: bool,
    scheme_admin: bool,
    default_team_guest_role: &str,
    default_team_user_role: &str,
    default_team_admin_role: &str,
    roles: &str,
) -> RolesInfo {
    let mut result = RolesInfo {
        roles: Vec::new(),
        explicit_roles: Vec::new(),
        scheme_guest,
        scheme_user,
        scheme_admin,
    };

    // Go's `strings.Fields`: split on runs of whitespace, dropping empties. Rust's
    // `split_whitespace` is the same Unicode White_Space property Go's `unicode.IsSpace`
    // consults, so an empty or all-blank column yields no roles on both sides.
    for role in roles.split_whitespace() {
        match role {
            TEAM_GUEST_ROLE_ID => result.scheme_guest = true,
            TEAM_USER_ROLE_ID => result.scheme_user = true,
            TEAM_ADMIN_ROLE_ID => result.scheme_admin = true,
            other => {
                result.explicit_roles.push(other.to_owned());
                result.roles.push(other.to_owned());
            }
        }
    }

    // Scheme-implied roles, in Go's order: guest, user, admin.
    let mut implied: Vec<&str> = Vec::new();
    if result.scheme_guest {
        implied.push(if default_team_guest_role.is_empty() {
            TEAM_GUEST_ROLE_ID
        } else {
            default_team_guest_role
        });
    }
    if result.scheme_user {
        implied.push(if default_team_user_role.is_empty() {
            TEAM_USER_ROLE_ID
        } else {
            default_team_user_role
        });
    }
    if result.scheme_admin {
        implied.push(if default_team_admin_role.is_empty() {
            TEAM_ADMIN_ROLE_ID
        } else {
            default_team_admin_role
        });
    }

    for implied_role in implied {
        if !result.roles.iter().any(|role| role == implied_role) {
            result.roles.push(implied_role.to_owned());
        }
    }

    result
}

/// The subset of Go's `store.TeamStore` (store/store.go:135-199) that is ported.
pub trait TeamStore {
    /// Port of `SqlTeamStore.GetTeamsForUser` (team_store.go:1181).
    fn get_teams_for_user(
        &self,
        user_id: &str,
        exclude_team_id: &str,
        include_deleted: bool,
    ) -> impl std::future::Future<Output = Result<Vec<TeamMember>, StoreError>> + Send;
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlTeamStore {
    pool: PgPool,
}

impl SqlTeamStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

impl TeamStore for SqlTeamStore {
    #[tracing::instrument(skip_all, fields(user_id = %user_id, found))]
    async fn get_teams_for_user(
        &self,
        user_id: &str,
        exclude_team_id: &str,
        include_deleted: bool,
    ) -> Result<Vec<TeamMember>, StoreError> {
        get_teams_for_user(&self.pool, user_id, exclude_team_id, include_deleted).await
    }
}

/// Free function so `SqlSessionStore` can reach it without owning a `SqlTeamStore`.
///
/// Go writes this as `me.Team().GetTeamsForUser(...)` — one `SqlStore` exposing every store — and
/// the session store genuinely does depend on the team store. Sharing the pool rather than the
/// struct keeps that dependency without making the two types circular.
#[tracing::instrument(skip(pool), fields(user_id = %user_id))]
pub async fn get_teams_for_user(
    pool: &PgPool,
    user_id: &str,
    exclude_team_id: &str,
    include_deleted: bool,
) -> Result<Vec<TeamMember>, StoreError> {
    // Go assembles this with squirrel and adds the two optional predicates conditionally
    // (team_store.go:1181-1192). Expressing them inside one static statement keeps sqlx's
    // compile-time checking, which is worth more here than mirroring the builder: an empty
    // `exclude_team_id` excludes nothing, and `include_deleted` drops the `DeleteAt` filter.
    //
    // Both joins are LEFT: a team with no scheme — every team on Team Edition, where `Schemes` is
    // an enterprise feature — must still return its member, with the three default role names
    // NULL. An INNER join here would return nothing at all for those members.
    let rows = sqlx::query!(
        r#"
        SELECT tm.teamid,
               tm.userid,
               tm.roles,
               tm.deleteat,
               tm.schemeuser,
               tm.schemeadmin,
               tm.schemeguest,
               tm.createat,
               ts.defaultteamguestrole,
               ts.defaultteamuserrole,
               ts.defaultteamadminrole
          FROM teammembers tm
          LEFT JOIN teams t ON tm.teamid = t.id
          LEFT JOIN schemes ts ON t.schemeid = ts.id
         WHERE tm.userid = $1
           AND ($2 = '' OR tm.teamid <> $2)
           AND ($3 OR tm.deleteat = 0)
        "#,
        user_id,
        exclude_team_id,
        include_deleted
    )
    .fetch_all(pool)
    .await
    .map_err(|source| StoreError::Db {
        context: format!("failed to find TeamMembers with userId={user_id}"),
        source,
    })?;

    // Port of `teamMemberWithSchemeRoles.ToModel` (team_store.go:162). Go's `sql.NullBool` and
    // `sql.NullString` both mean "NULL is the zero value" here — `Valid && Bool` for the flags,
    // `""` for the role names — so `unwrap_or_default` is the same rule, not a looser one.
    Ok(rows
        .into_iter()
        .map(|row| {
            let roles_result = get_team_roles(
                row.schemeguest.unwrap_or_default(),
                row.schemeuser.unwrap_or_default(),
                row.schemeadmin.unwrap_or_default(),
                row.defaultteamguestrole.as_deref().unwrap_or_default(),
                row.defaultteamuserrole.as_deref().unwrap_or_default(),
                row.defaultteamadminrole.as_deref().unwrap_or_default(),
                row.roles.as_deref().unwrap_or_default(),
            );

            TeamMember {
                team_id: row.teamid,
                user_id: row.userid,
                roles: roles_result.roles.join(" "),
                delete_at: row.deleteat.unwrap_or_default(),
                scheme_guest: roles_result.scheme_guest,
                scheme_user: roles_result.scheme_user,
                scheme_admin: roles_result.scheme_admin,
                explicit_roles: roles_result.explicit_roles.join(" "),
                create_at: row.createat.unwrap_or_default(),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact row in the development database, and the exact answer the running Go server
    /// gives for it (`GET /api/v4/users/me/teams/members`):
    ///
    /// ```json
    /// {"roles":"team_user team_admin","scheme_user":true,"scheme_admin":true,
    ///  "scheme_guest":false,"explicit_roles":""}
    /// ```
    ///
    /// Measured, not predicted — this is the one case with a live oracle behind it.
    #[test]
    fn matches_the_running_go_server_for_a_scheme_less_team() {
        let info = get_team_roles(false, true, true, "", "", "", "");
        assert_eq!(info.roles.join(" "), "team_user team_admin");
        assert_eq!(info.explicit_roles.join(" "), "");
        assert!(!info.scheme_guest);
        assert!(info.scheme_user);
        assert!(info.scheme_admin);
    }

    /// The un-migrated case Go's comment describes: a scheme role sitting in the `Roles` column
    /// sets the flag even though the column said false, and is kept out of `explicit_roles`.
    #[test]
    fn a_scheme_role_in_the_roles_column_sets_the_flag_and_is_not_explicit() {
        let info = get_team_roles(false, false, false, "", "", "", "team_admin");
        assert!(
            info.scheme_admin,
            "the column said false; the role says true"
        );
        assert_eq!(info.explicit_roles, Vec::<String>::new());
        assert_eq!(info.roles.join(" "), "team_admin");
    }

    /// A role that is not one of the three is explicit, appears in both outputs, and keeps its
    /// position ahead of the implied roles.
    #[test]
    fn custom_roles_are_explicit_and_come_first() {
        let info = get_team_roles(false, true, false, "", "", "", "custom_one custom_two");
        assert_eq!(info.explicit_roles, vec!["custom_one", "custom_two"]);
        assert_eq!(info.roles.join(" "), "custom_one custom_two team_user");
    }

    /// With a scheme, the implied role is the scheme's name rather than the constant.
    #[test]
    fn scheme_defaults_replace_the_constants() {
        let info = get_team_roles(
            true,
            true,
            true,
            "custom_guest",
            "custom_user",
            "custom_admin",
            "",
        );
        assert_eq!(
            info.roles.join(" "),
            "custom_guest custom_user custom_admin"
        );
    }

    /// Only the flags that are set contribute, and the order is always guest, user, admin —
    /// never the order the flags were discovered in.
    #[test]
    fn implied_roles_are_emitted_in_guest_user_admin_order() {
        let info = get_team_roles(true, false, true, "", "", "", "");
        assert_eq!(info.roles.join(" "), "team_guest team_admin");
    }

    /// The dedup check reads `roles` as it grows, so two scheme defaults with the same name
    /// collapse. Reproduced because it is Go's behaviour, not because it is desirable.
    #[test]
    fn identical_scheme_defaults_collapse_to_one_role() {
        let info = get_team_roles(false, true, true, "", "same_role", "same_role", "");
        assert_eq!(info.roles.join(" "), "same_role");
        assert!(info.scheme_user && info.scheme_admin, "both flags stay set");
    }

    /// An implied role already present as an explicit role is not appended twice.
    #[test]
    fn an_implied_role_already_explicit_is_not_duplicated() {
        let info = get_team_roles(false, true, false, "", "custom_user", "", "custom_user");
        assert_eq!(info.roles.join(" "), "custom_user");
        assert_eq!(info.explicit_roles, vec!["custom_user"]);
    }

    /// `strings.Fields` drops empty fields, so runs of whitespace and a blank column both yield
    /// nothing. A NULL column reaches this as `""` via `unwrap_or_default`.
    #[test]
    fn whitespace_only_roles_contribute_nothing() {
        for input in ["", "   ", "\t\n ", "  \t"] {
            let info = get_team_roles(false, false, false, "", "", "", input);
            assert!(
                info.roles.is_empty() && info.explicit_roles.is_empty(),
                "input {input:?} should contribute no roles"
            );
        }

        // And the separators between real roles are runs, not single spaces.
        let info = get_team_roles(false, false, false, "", "", "", "  a \t\n b  ");
        assert_eq!(info.explicit_roles, vec!["a", "b"]);
    }

    /// No flags and no roles is an empty result, not a defaulted one — a member of a team with
    /// no roles at all serialises `"roles": ""`.
    #[test]
    fn nothing_set_yields_nothing() {
        let info = get_team_roles(false, false, false, "", "", "", "");
        assert_eq!(info, RolesInfo::default());
        assert_eq!(info.roles.join(" "), "");
    }
}
