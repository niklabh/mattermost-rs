//! Port of `model/team_member.go` (team_member.go:1–155).
//!
//! Translated ahead of its turn in the dependency order because `Session.TeamMembers` is a
//! `[]*TeamMember` on the wire — `session.rs` cannot round-trip its fixture without it.
//!
//! # Deliberately not translated here
//!
//! - `TeamMemberWithError` / `EmailInviteWithError` and their helpers embed `*AppError` as a
//!   wire field. That works, but they are invite-flow plumbing with no consumer yet.
//! - `Auditable` is an audit-log projection; it follows the audit layer.
//! - `PreUpdate` is empty in Go (team_member.go:139). Not reproduced — an empty method that
//!   exists only to satisfy an interface is noise until that interface exists.

use serde::{Deserialize, Serialize};

use crate::utils::{AppError, AppResult, is_valid_id};

/// team_member.go:13
pub const USERNAME: &str = "Username";

/// Port of `model.TeamMember` (team_member.go:20).
///
/// Note `create_at` carries `json:"-"`: it exists in the database but never on the wire.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMember {
    #[serde(rename = "team_id")]
    pub team_id: String,

    #[serde(rename = "user_id")]
    pub user_id: String,

    #[serde(rename = "roles")]
    pub roles: String,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "scheme_guest")]
    pub scheme_guest: bool,

    #[serde(rename = "scheme_user")]
    pub scheme_user: bool,

    #[serde(rename = "scheme_admin")]
    pub scheme_admin: bool,

    #[serde(rename = "explicit_roles")]
    pub explicit_roles: String,

    /// `json:"-"` in Go — persisted, never serialised.
    #[serde(skip)]
    pub create_at: i64,
}

/// Port of `model.TeamUnread` (team_member.go:47).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamUnread {
    #[serde(rename = "team_id")]
    pub team_id: String,
    #[serde(rename = "msg_count")]
    pub msg_count: i64,
    #[serde(rename = "mention_count")]
    pub mention_count: i64,
    #[serde(rename = "mention_count_root")]
    pub mention_count_root: i64,
    #[serde(rename = "msg_count_root")]
    pub msg_count_root: i64,
    #[serde(rename = "thread_count")]
    pub thread_count: i64,
    #[serde(rename = "thread_mention_count")]
    pub thread_mention_count: i64,
    #[serde(rename = "thread_urgent_mention_count")]
    pub thread_urgent_mention_count: i64,
}

/// Port of `model.TeamMemberForExport` (team_member.go:59).
///
/// `TeamName` has no json tag, so Go emits the field name verbatim.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamMemberForExport {
    #[serde(flatten)]
    pub team_member: TeamMember,
    #[serde(rename = "TeamName")]
    pub team_name: String,
}

impl TeamMember {
    /// Port of `(*TeamMember).IsValid` (team_member.go:122).
    ///
    /// All three failures carry empty details — unlike `Team::is_valid`, which stamps
    /// `id=...` on every branch but the first.
    pub fn is_valid(&self) -> AppResult {
        if !is_valid_id(&self.team_id) {
            return Err(Self::error("team_id"));
        }
        if !is_valid_id(&self.user_id) {
            return Err(Self::error("user_id"));
        }
        if self.roles.len() > crate::user::USER_ROLES_MAX_LENGTH {
            return Err(Self::error("roles_limit"));
        }
        Ok(())
    }

    fn error(field: &str) -> Box<AppError> {
        Box::new(AppError::new(
            "TeamMember.IsValid",
            format!("model.team_member.is_valid.{field}.app_error"),
            None,
            "",
            400,
        ))
    }

    /// Port of `(*TeamMember).GetRoles` (team_member.go:142).
    pub fn get_roles(&self) -> Vec<&str> {
        self.roles.split_whitespace().collect()
    }

    /// Port of `(*TeamMember).SanitizeRoleData` (team_member.go:146).
    ///
    /// Note `delete_at` is set to **-1**, not 0, when the member is not the current user.
    /// That sentinel reaches the client.
    pub fn sanitize_role_data(&mut self, current_user_id: &str) {
        if self.user_id != current_user_id {
            self.roles.clear();
            self.explicit_roles.clear();
            self.scheme_admin = false;
            self.scheme_guest = false;
            self.scheme_user = false;
            self.delete_at = -1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils;

    fn valid_member() -> TeamMember {
        TeamMember {
            team_id: utils::new_id(),
            user_id: utils::new_id(),
            roles: "team_user".into(),
            ..Default::default()
        }
    }

    #[test]
    fn team_member_matches_go_serialization() {
        let go = include_str!("../../../fixtures/team_member.json");
        let parsed: TeamMember = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
    }

    /// `TeamUnread` reaches a client from two routes with two different framings
    /// (`getTeamUnread` encodes, `getTeamsUnreadForUser` marshals), so its field set is pinned
    /// against a generated fixture rather than against either handler. Eight fields, none
    /// `omitempty`, every one distinct and non-zero in the fixture.
    #[test]
    fn team_unread_matches_go_serialization() {
        let go = include_str!("../../../fixtures/team_unread.json");
        let parsed: TeamUnread = serde_json::from_str(go).unwrap();
        let round_tripped = serde_json::to_value(&parsed).unwrap();
        let expected: serde_json::Value = serde_json::from_str(go).unwrap();
        assert_eq!(round_tripped, expected);
        assert_eq!(expected.as_object().unwrap().len(), 8);
    }

    #[test]
    fn create_at_never_reaches_the_wire() {
        let member = TeamMember {
            create_at: 1_700_000_000_000,
            ..valid_member()
        };
        let value = serde_json::to_value(&member).unwrap();
        assert!(!value.as_object().unwrap().contains_key("create_at"));
        assert_eq!(value.as_object().unwrap().len(), 8);
    }

    #[test]
    fn is_valid_covers_every_branch() {
        assert!(valid_member().is_valid().is_ok());

        let mut member = valid_member();
        member.team_id = "short".into();
        assert_eq!(
            member.is_valid().unwrap_err().id,
            "model.team_member.is_valid.team_id.app_error"
        );

        let mut member = valid_member();
        member.user_id = String::new();
        assert_eq!(
            member.is_valid().unwrap_err().id,
            "model.team_member.is_valid.user_id.app_error"
        );

        let mut member = valid_member();
        member.roles = "a".repeat(crate::user::USER_ROLES_MAX_LENGTH + 1);
        let err = member.is_valid().unwrap_err();
        assert_eq!(err.id, "model.team_member.is_valid.roles_limit.app_error");
        assert_eq!(err.detailed_error, "", "all branches carry empty details");
    }

    #[test]
    fn sanitize_role_data_uses_minus_one_as_the_delete_at_sentinel() {
        let mut member = valid_member();
        member.scheme_admin = true;
        member.explicit_roles = "team_admin".into();
        let other = utils::new_id();

        member.sanitize_role_data(&other);
        assert_eq!(member.roles, "");
        assert_eq!(member.explicit_roles, "");
        assert!(!member.scheme_admin && !member.scheme_guest && !member.scheme_user);
        assert_eq!(member.delete_at, -1, "Go sets -1, not 0");
    }

    #[test]
    fn sanitize_role_data_leaves_the_current_user_alone() {
        let mut member = valid_member();
        member.scheme_admin = true;
        let me = member.user_id.clone();

        member.sanitize_role_data(&me);
        assert_eq!(member.roles, "team_user");
        assert!(member.scheme_admin);
        assert_eq!(member.delete_at, 0);
    }

    #[test]
    fn get_roles_splits_on_whitespace_runs() {
        let member = TeamMember {
            roles: "team_user  team_admin".into(),
            ..Default::default()
        };
        assert_eq!(member.get_roles(), vec!["team_user", "team_admin"]);
        assert!(TeamMember::default().get_roles().is_empty());
    }
}
