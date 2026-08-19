//! Port of `model/permission.go` — the [`Permission`] type, the six scope constants, and the two
//! permission-error constructors.
//!
//! The 311 permission values themselves and the seven tables that group them are **generated**
//! into `permission_generated.rs` and re-exported from here, so `permission::PERMISSION_MANAGE_TEAM`
//! resolves whether a name is generated or hand-written. `MIGRATION.md` has listed permission.go as
//! generate-only from the start; `reference/dump/permission_gen.go` documents why, and the fixture
//! it emits makes the reason concrete — fourteen permissions have an id that does **not** match
//! their Go identifier, six of them with the words in a different order
//! (`PermissionPublicPlaybookCreate` is `playbook_public_create`), and a wrong id fails open.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::session::Session;
use crate::utils::AppError;

pub use crate::permission_generated::*;

/// Port of `model.PermissionScopeSystem` (permission.go:12).
pub const PERMISSION_SCOPE_SYSTEM: &str = "system_scope";
/// Port of `model.PermissionScopeTeam` (permission.go:13).
pub const PERMISSION_SCOPE_TEAM: &str = "team_scope";
/// Port of `model.PermissionScopeChannel` (permission.go:14).
pub const PERMISSION_SCOPE_CHANNEL: &str = "channel_scope";
/// Port of `model.PermissionScopeGroup` (permission.go:15).
pub const PERMISSION_SCOPE_GROUP: &str = "group_scope";
/// Port of `model.PermissionScopePlaybook` (permission.go:16).
pub const PERMISSION_SCOPE_PLAYBOOK: &str = "playbook_scope";
/// Port of `model.PermissionScopeRun` (permission.go:17).
pub const PERMISSION_SCOPE_RUN: &str = "run_scope";

/// Port of `model.Permission` (permission.go:20).
///
/// The fields are `Cow<'static, str>` rather than `String` so the generated table can be built in
/// const position — 311 `Permission`s with no allocation and no lazy initialiser — while a value
/// arriving over the wire still deserialises into owned storage. The JSON is identical either way;
/// serde treats `Cow<'_, str>` exactly as it treats `String`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Permission {
    /// The value stored in `Roles.Permissions` and matched by every permission check.
    #[serde(rename = "id")]
    pub id: Cow<'static, str>,

    /// An i18n key, not a display string.
    #[serde(rename = "name")]
    pub name: Cow<'static, str>,

    /// An i18n key, not a display string.
    #[serde(rename = "description")]
    pub description: Cow<'static, str>,

    /// One of the six `PERMISSION_SCOPE_*` constants.
    #[serde(rename = "scope")]
    pub scope: Cow<'static, str>,
}

/// The `model.ChannelModeratedPermissionsMap` lookup (permission.go:2746).
///
/// Go's value is a map; the generated table is the same pairs sorted by key. Go's map has no order,
/// and its one ranging caller — `Role.GetChannelModeratedPermissions` (role.go:713) — uses the loop
/// to find a key it already holds and writes into a map, so no result depends on the iteration
/// order. Checked at the pinned SHA. Returns the moderation control a permission id
/// belongs to — note that `create_post` and `use_channel_mentions` map to themselves, while
/// `add_reaction`/`remove_reaction` collapse into `create_reactions` and the eight bookmark
/// permissions collapse into `manage_bookmarks`.
#[must_use]
pub fn channel_moderated_permission_for(permission_id: &str) -> Option<&'static str> {
    CHANNEL_MODERATED_PERMISSIONS_MAP
        .binary_search_by_key(&permission_id, |(id, _)| *id)
        .ok()
        .map(|i| CHANNEL_MODERATED_PERMISSIONS_MAP[i].1)
}

/// Port of `model.MakePermissionError` (permission.go:2775).
#[must_use]
pub fn make_permission_error(session: &Session, permissions: &[&Permission]) -> Box<AppError> {
    make_permission_error_for_user(&session.user_id, permissions)
}

/// Port of `model.MakePermissionErrorForUser` (permission.go:2779).
///
/// The detail is built even when there is nothing to build it from: an empty permission list still
/// produces a trailing `permission=`, because Go writes the prefix before the loop rather than
/// joining. This string reaches the server log, so the eleven bytes are worth reproducing.
#[must_use]
pub fn make_permission_error_for_user(user_id: &str, permissions: &[&Permission]) -> Box<AppError> {
    let mut detail = format!("userId={user_id}, permission=");
    for (i, permission) in permissions.iter().enumerate() {
        detail.push_str(&permission.id);
        if i != permissions.len() - 1 {
            detail.push(',');
        }
    }
    Box::new(AppError::new(
        "Permissions",
        "api.context.permissions.app_error",
        None,
        detail,
        403, // http.StatusForbidden
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn permission_json_round_trip() {
        let raw = include_str!("../../../fixtures/permission.json");
        let parsed: Permission = serde_json::from_str(raw).expect("permission.json parses");
        let round_tripped = serde_json::to_value(&parsed).expect("re-serialises");
        let original: Value = serde_json::from_str(raw).expect("fixture is JSON");
        assert_eq!(original, round_tripped);
    }

    #[test]
    fn generated_values_are_borrowed_not_owned() {
        // The whole reason for `Cow` rather than `String`. If this ever fails, the table stopped
        // being a compile-time constant and started allocating 1,244 strings at first use.
        assert!(matches!(PERMISSION_MANAGE_TEAM.id, Cow::Borrowed(_)));
        assert!(matches!(PERMISSION_MANAGE_TEAM.scope, Cow::Borrowed(_)));
    }

    #[test]
    fn deserialised_permission_owns_its_strings() {
        let parsed: Permission =
            serde_json::from_str(r#"{"id":"x","name":"n","description":"d","scope":"team_scope"}"#)
                .expect("parses");
        assert!(matches!(parsed.id, Cow::Owned(_)));
        assert_eq!(parsed.scope, PERMISSION_SCOPE_TEAM);
    }

    #[test]
    fn channel_moderated_lookup_misses_are_none() {
        assert_eq!(
            channel_moderated_permission_for("create_post"),
            Some("create_post")
        );
        assert_eq!(
            channel_moderated_permission_for("add_reaction"),
            Some("create_reactions")
        );
        assert_eq!(
            channel_moderated_permission_for("add_bookmark_public_channel"),
            Some("manage_bookmarks")
        );
        // A real permission that is not moderated, and a string that is no permission at all.
        assert_eq!(channel_moderated_permission_for("manage_team"), None);
        assert_eq!(channel_moderated_permission_for("manage_bookmarks"), None);
        assert_eq!(channel_moderated_permission_for(""), None);
    }

    /// Everything here is asserted against `fixtures/behaviour_permission.json`, which
    /// `reference/dump/permission_gen.go` writes from the linked Go package.
    mod go_parity {
        use super::*;
        use std::collections::{BTreeMap, BTreeSet};
        use std::sync::OnceLock;

        fn oracle() -> &'static Value {
            static ORACLE: OnceLock<Value> = OnceLock::new();
            ORACLE.get_or_init(|| {
                let raw = include_str!("../../../fixtures/behaviour_permission.json");
                serde_json::from_str(raw).expect("behaviour_permission.json parses")
            })
        }

        fn ids(table: &[&Permission]) -> Vec<String> {
            table.iter().map(|p| p.id.to_string()).collect()
        }

        fn expected_ids(key: &str) -> Vec<String> {
            oracle()[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} is an array"))
                .iter()
                .map(|v| v.as_str().expect("an id string").to_owned())
                .collect()
        }

        /// Go declares 311 `*Permission` vars and puts every one of them in exactly one of
        /// `AllPermissions` or `DeprecatedPermissions`, so the two tables together are the
        /// complete declared set. The partition test below is what licenses this.
        fn declared() -> Vec<&'static Permission> {
            ALL_PERMISSIONS
                .iter()
                .chain(DEPRECATED_PERMISSIONS.iter())
                .copied()
                .collect()
        }

        #[test]
        fn every_declared_permission_matches_go_field_for_field() {
            let expected: BTreeMap<String, &Value> = oracle()["permissions"]
                .as_array()
                .expect("permissions is an array")
                .iter()
                .map(|p| (p["id"].as_str().expect("an id").to_owned(), p))
                .collect();

            let ours = declared();
            assert_eq!(
                ours.len(),
                expected.len(),
                "declared permission count drifted from Go"
            );

            for permission in ours {
                let go = expected
                    .get(permission.id.as_ref())
                    .unwrap_or_else(|| panic!("Go has no permission with id {}", permission.id));
                assert_eq!(go["name"].as_str(), Some(permission.name.as_ref()));
                assert_eq!(
                    go["description"].as_str(),
                    Some(permission.description.as_ref())
                );
                assert_eq!(go["scope"].as_str(), Some(permission.scope.as_ref()));
            }
        }

        #[test]
        fn tables_match_go_in_order() {
            assert_eq!(ids(ALL_PERMISSIONS), expected_ids("all_permissions"));
            assert_eq!(
                ids(DEPRECATED_PERMISSIONS),
                expected_ids("deprecated_permissions")
            );
            assert_eq!(
                ids(SYSCONSOLE_READ_PERMISSIONS),
                expected_ids("sysconsole_read_permissions")
            );
            assert_eq!(
                ids(SYSCONSOLE_WRITE_PERMISSIONS),
                expected_ids("sysconsole_write_permissions")
            );
            assert_eq!(
                ids(MODERATED_BOOKMARK_PERMISSIONS),
                expected_ids("moderated_bookmark_permissions")
            );
            assert_eq!(
                CHANNEL_MODERATED_PERMISSIONS.to_vec(),
                expected_ids("channel_moderated_permissions")
            );
        }

        #[test]
        fn counts_match_go() {
            let counts = &oracle()["counts"];
            assert_eq!(counts["declared"].as_u64(), Some(declared().len() as u64));
            assert_eq!(
                counts["all_permissions"].as_u64(),
                Some(ALL_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["deprecated"].as_u64(),
                Some(DEPRECATED_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["sysconsole_read"].as_u64(),
                Some(SYSCONSOLE_READ_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["sysconsole_write"].as_u64(),
                Some(SYSCONSOLE_WRITE_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["moderated_bookmark"].as_u64(),
                Some(MODERATED_BOOKMARK_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["channel_moderated"].as_u64(),
                Some(CHANNEL_MODERATED_PERMISSIONS.len() as u64)
            );
            assert_eq!(
                counts["channel_moderated_m"].as_u64(),
                Some(CHANNEL_MODERATED_PERMISSIONS_MAP.len() as u64)
            );
        }

        /// The invariant `declared()` rests on, measured rather than assumed: the deprecated set
        /// and `AllPermissions` are disjoint, `AllPermissions` has no repeats, and nothing Go
        /// declares sits outside both. Go's job types are the counter-example that makes this
        /// worth asserting — 42 declared, 24 reachable ([D-120]).
        #[test]
        fn declared_partitions_into_all_and_deprecated() {
            assert!(
                oracle()["deprecated_in_all"]
                    .as_array()
                    .expect("an array")
                    .is_empty(),
                "a deprecated permission is also in AllPermissions"
            );
            assert!(
                oracle()["all_permissions_duplicates"]
                    .as_array()
                    .expect("an array")
                    .is_empty(),
                "AllPermissions repeats a permission"
            );
            assert!(
                oracle()["declared_in_no_table"]
                    .as_array()
                    .expect("an array")
                    .is_empty(),
                "Go declares a permission that appears in no table"
            );

            let not_in_all: BTreeSet<String> =
                expected_ids("declared_not_in_all").into_iter().collect();
            let deprecated: BTreeSet<String> = ids(DEPRECATED_PERMISSIONS).into_iter().collect();
            assert_eq!(not_in_all, deprecated);

            let all: BTreeSet<String> = ids(ALL_PERMISSIONS).into_iter().collect();
            assert!(all.is_disjoint(&deprecated));
            assert_eq!(all.len() + deprecated.len(), declared().len());
        }

        #[test]
        fn scope_constants_and_histogram_match_go() {
            let scopes = &oracle()["scope_constants"];
            assert_eq!(
                scopes["PermissionScopeSystem"].as_str(),
                Some(PERMISSION_SCOPE_SYSTEM)
            );
            assert_eq!(
                scopes["PermissionScopeTeam"].as_str(),
                Some(PERMISSION_SCOPE_TEAM)
            );
            assert_eq!(
                scopes["PermissionScopeChannel"].as_str(),
                Some(PERMISSION_SCOPE_CHANNEL)
            );
            assert_eq!(
                scopes["PermissionScopeGroup"].as_str(),
                Some(PERMISSION_SCOPE_GROUP)
            );
            assert_eq!(
                scopes["PermissionScopePlaybook"].as_str(),
                Some(PERMISSION_SCOPE_PLAYBOOK)
            );
            assert_eq!(
                scopes["PermissionScopeRun"].as_str(),
                Some(PERMISSION_SCOPE_RUN)
            );

            let mut histogram: BTreeMap<&str, u64> = BTreeMap::new();
            for permission in declared() {
                *histogram.entry(permission.scope.as_ref()).or_default() += 1;
            }
            let expected = oracle()["scope_histogram"]
                .as_object()
                .expect("an object")
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_u64().expect("a count")))
                .collect::<BTreeMap<_, _>>();
            assert_eq!(histogram, expected);
        }

        #[test]
        fn channel_moderated_map_matches_go() {
            let expected = oracle()["channel_moderated_permissions_map"]
                .as_object()
                .expect("an object");
            assert_eq!(CHANNEL_MODERATED_PERMISSIONS_MAP.len(), expected.len());
            for (key, value) in expected {
                assert_eq!(
                    channel_moderated_permission_for(key),
                    value.as_str(),
                    "moderation control for {key}"
                );
            }
            // The generated table must stay sorted, or the binary search silently misses.
            assert!(
                CHANNEL_MODERATED_PERMISSIONS_MAP
                    .windows(2)
                    .all(|w| w[0].0 < w[1].0),
                "CHANNEL_MODERATED_PERMISSIONS_MAP is not sorted by key"
            );
        }

        /// Fourteen permissions have an id their Go identifier would not produce, six of them with
        /// the words transposed. This pins both halves: the real id resolves, and the plausible
        /// one does not exist — so "tidying" `playbook_public_create` into `public_playbook_create`
        /// fails here rather than in production, where it would deny every playbook action.
        #[test]
        fn permissions_whose_id_disagrees_with_their_go_identifier() {
            let mismatches = oracle()["ident_id_mismatches"]
                .as_array()
                .expect("an array");
            assert_eq!(mismatches.len(), 14, "the mismatch set changed upstream");

            let declared_ids: BTreeSet<String> = ids(&declared()).into_iter().collect();
            for entry in mismatches {
                let id = entry["id"].as_str().expect("an id");
                let from_ident = entry["ident_snake"].as_str().expect("a snake-cased ident");
                assert!(declared_ids.contains(id), "Go's id {id} is missing");
                assert!(
                    !declared_ids.contains(from_ident),
                    "{from_ident} exists, so the mismatch is not what it claims"
                );
            }
        }

        #[test]
        fn make_permission_error_matches_go() {
            let by_id: BTreeMap<&str, &Permission> =
                declared().into_iter().map(|p| (p.id.as_ref(), p)).collect();

            for case in oracle()["make_permission_error"]
                .as_array()
                .expect("an array")
            {
                let name = case["name"].as_str().expect("a case name");
                let user_id = case["user_id"].as_str().expect("a user id");
                let permissions: Vec<&Permission> = case["permissions"]
                    .as_array()
                    .expect("an array")
                    .iter()
                    .map(|v| by_id[v.as_str().expect("an id")])
                    .collect();

                let ours = if name == "via_session" {
                    let session = Session {
                        user_id: user_id.to_owned(),
                        ..Default::default()
                    };
                    make_permission_error(&session, &permissions)
                } else {
                    make_permission_error_for_user(user_id, &permissions)
                };

                assert_eq!(
                    ours.where_,
                    case["where"].as_str().unwrap_or_default(),
                    "{name}: where"
                );
                assert_eq!(
                    ours.id,
                    case["id"].as_str().unwrap_or_default(),
                    "{name}: id"
                );
                assert_eq!(
                    ours.message,
                    case["message"].as_str().unwrap_or_default(),
                    "{name}: message"
                );
                assert_eq!(
                    ours.detailed_error,
                    case["detailed_error"].as_str().unwrap_or_default(),
                    "{name}: detailed_error"
                );
                assert_eq!(
                    i64::from(ours.status_code),
                    case["status_code"].as_i64().unwrap_or_default(),
                    "{name}: status_code"
                );
                assert_eq!(
                    ours.to_json().expect("serialises"),
                    case["to_json"].as_str().unwrap_or_default(),
                    "{name}: ToJSON is byte-identical"
                );
            }
        }

        /// `http.StatusForbidden`, read from Go rather than assumed.
        #[test]
        fn permission_errors_are_403() {
            let forbidden = oracle()["http_status_forbidden"]
                .as_i64()
                .expect("a status code");
            let err = make_permission_error_for_user("uid", &[&PERMISSION_MANAGE_TEAM]);
            assert_eq!(i64::from(err.status_code), forbidden);
        }
    }
}
