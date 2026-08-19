//! Port of `model/role.go` — the logic half.
//!
//! The data half (the 24 default roles, the seven id/permission lists and the sysconsole ancillary
//! map) is **generated** into `role_generated.rs` and re-exported from here, on the same reasoning
//! as `permission.go`: 740 of role.go's 1,311 lines are permission ids, and one wrong id is a role
//! that silently denies.
//!
//! Three functions here build their result by ranging a Go **map**, so Go's answer has no order at
//! all — `ChannelModeratedPermissionsChangedByPatch`, `RolePatchFromChannelModerationsPatch` and
//! (harmlessly) `GetChannelModeratedPermissions`. That is measured rather than assumed: the oracle
//! calls each fifty times and records whether the order ever varied, and it does. Ours sort, and
//! the parity tests compare sets. See [D-125].

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::channel::ChannelModerationPatch;
use crate::channel::{CHANNEL_TYPE_OPEN, CHANNEL_TYPE_PRIVATE};
use crate::permission::{
    ALL_PERMISSIONS, DEPRECATED_PERMISSIONS, MODERATED_BOOKMARK_PERMISSIONS,
    PERMISSION_ADD_BOOKMARK_PRIVATE_CHANNEL, PERMISSION_ADD_BOOKMARK_PUBLIC_CHANNEL,
    PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS, PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS,
    PERMISSION_SCOPE_CHANNEL, Permission, channel_moderated_permission_for,
};
use crate::utils::{FAKE_SETTING, StringInterface, go_quote, is_valid_id};

pub use crate::role_generated::*;

/// Port of the role-id constants (role.go:379).
pub const SYSTEM_GUEST_ROLE_ID: &str = "system_guest";
pub const SYSTEM_USER_ROLE_ID: &str = "system_user";
pub const SYSTEM_ADMIN_ROLE_ID: &str = "system_admin";
pub const SYSTEM_POST_ALL_ROLE_ID: &str = "system_post_all";
pub const SYSTEM_POST_ALL_PUBLIC_ROLE_ID: &str = "system_post_all_public";
pub const SYSTEM_USER_ACCESS_TOKEN_ROLE_ID: &str = "system_user_access_token";
pub const SYSTEM_USER_MANAGER_ROLE_ID: &str = "system_user_manager";
pub const SYSTEM_READ_ONLY_ADMIN_ROLE_ID: &str = "system_read_only_admin";
pub const SYSTEM_MANAGER_ROLE_ID: &str = "system_manager";
pub const SYSTEM_CUSTOM_GROUP_ADMIN_ROLE_ID: &str = "system_custom_group_admin";
pub const SHARED_CHANNEL_MANAGER_ROLE_ID: &str = "system_shared_channel_manager";

pub const TEAM_GUEST_ROLE_ID: &str = "team_guest";
pub const TEAM_USER_ROLE_ID: &str = "team_user";
pub const TEAM_ADMIN_ROLE_ID: &str = "team_admin";
pub const TEAM_POST_ALL_ROLE_ID: &str = "team_post_all";
pub const TEAM_POST_ALL_PUBLIC_ROLE_ID: &str = "team_post_all_public";

pub const CHANNEL_GUEST_ROLE_ID: &str = "channel_guest";
pub const CHANNEL_USER_ROLE_ID: &str = "channel_user";
pub const CHANNEL_ADMIN_ROLE_ID: &str = "channel_admin";

pub const CUSTOM_GROUP_USER_ROLE_ID: &str = "custom_group_user";

pub const PLAYBOOK_ADMIN_ROLE_ID: &str = "playbook_admin";
pub const PLAYBOOK_MEMBER_ROLE_ID: &str = "playbook_member";
pub const RUN_ADMIN_ROLE_ID: &str = "run_admin";
pub const RUN_MEMBER_ROLE_ID: &str = "run_member";

/// All three caps count **bytes**, not runes — `len()` in Go, and the corpus pins a multi-byte
/// display name either side of the boundary.
pub const ROLE_NAME_MAX_LENGTH: usize = 64;
pub const ROLE_DISPLAY_NAME_MAX_LENGTH: usize = 128;
pub const ROLE_DESCRIPTION_MAX_LENGTH: usize = 1024;

/// Port of `model.RoleScope` (role.go:377).
pub const ROLE_SCOPE_SYSTEM: &str = "System";
pub const ROLE_SCOPE_TEAM: &str = "Team";
pub const ROLE_SCOPE_CHANNEL: &str = "Channel";
pub const ROLE_SCOPE_GROUP: &str = "Group";

/// Port of `model.RoleType` (role.go:376).
pub const ROLE_TYPE_GUEST: &str = "Guest";
pub const ROLE_TYPE_USER: &str = "User";
pub const ROLE_TYPE_ADMIN: &str = "Admin";

/// The characters `IsValidRoleName` accepts, as Go's `TrimLeft` cutset (role.go:869).
const ROLE_NAME_CUTSET: &str = "abcdefghijklmnopqrstuvwxyz0123456789_";

/// Port of `model.Role` (role.go:423).
///
/// `permissions` is `Option<Vec<String>>` because the Go field has no `omitempty`: a nil slice
/// serialises as `null` and an empty one as `[]`, and role.go distinguishes them — `Clone`
/// preserves nil, `UnknownPermissions` returns nil for a nil list, and `MergeChannelHigherScoped`
/// always produces `[]`. Same modelling as `channel.rs:707`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Role {
    #[serde(rename = "id")]
    pub id: String,

    #[serde(rename = "name")]
    pub name: String,

    #[serde(rename = "display_name")]
    pub display_name: String,

    #[serde(rename = "description")]
    pub description: String,

    #[serde(rename = "create_at")]
    pub create_at: i64,

    #[serde(rename = "update_at")]
    pub update_at: i64,

    #[serde(rename = "delete_at")]
    pub delete_at: i64,

    #[serde(rename = "permissions")]
    pub permissions: Option<Vec<String>>,

    #[serde(rename = "scheme_managed")]
    pub scheme_managed: bool,

    #[serde(rename = "built_in")]
    pub built_in: bool,

    #[serde(rename = "scheme_id")]
    pub scheme_id: Option<String>,
}

/// Port of `model.RolePatch` (role.go:548).
///
/// Go's field is `*[]string`, which has one state this cannot express: a non-nil pointer to a nil
/// slice, which `Patch` would write through as a nil `Permissions`. It is unreachable from the
/// wire — JSON `null` unmarshals to a nil *pointer* — so the gap is Go-side only. [D-126].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RolePatch {
    #[serde(rename = "permissions")]
    pub permissions: Option<Vec<String>>,
}

/// Port of `model.RolePermissions` (role.go:558) — no `json:` tags in Go, an internal pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RolePermissions {
    pub role_id: String,
    pub permissions: Vec<String>,
}

/// A row of the generated default-role table. `MakeDefaultRoles` builds `Role`s out of these.
#[derive(Debug, Clone, Copy)]
pub struct DefaultRole {
    /// The key Go files this role under. Identical to `name` for all 24 today, and kept separate
    /// so that stops being an assumption.
    pub key: &'static str,
    pub name: &'static str,
    pub display_name: &'static str,
    pub description: &'static str,
    pub permissions: &'static [&'static str],
    pub scheme_managed: bool,
    pub built_in: bool,
}

/// The error `Role::is_valid` returns. Go returns a bare `fmt.Errorf`, not an `AppError`, so there
/// is no id or status code to reproduce — only the message, which `%q`-quotes the offending value.
/// [`go_quote`] rather than `{:?}`: the two differ on control characters, and a role name arrives
/// off the wire.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RoleError {
    #[error("invalid role id {}", go_quote(.0))]
    InvalidId(String),

    #[error("invalid role name {}", go_quote(.0))]
    InvalidName(String),

    #[error("role display name must not be empty")]
    EmptyDisplayName,

    #[error("role display name {} exceeds maximum length of {ROLE_DISPLAY_NAME_MAX_LENGTH}", go_quote(.0))]
    DisplayNameTooLong(String),

    #[error("role description exceeds maximum length of {ROLE_DESCRIPTION_MAX_LENGTH}")]
    DescriptionTooLong,

    #[error("unknown permissions: {}", .0.join(", "))]
    UnknownPermissions(Vec<String>),
}

impl Role {
    /// Port of `(*Role).Auditable` (role.go:450).
    #[must_use]
    pub fn auditable(&self) -> StringInterface {
        let mut out = serde_json::Map::new();
        out.insert("id".into(), self.id.clone().into());
        out.insert("name".into(), self.name.clone().into());
        out.insert("display_name".into(), self.display_name.clone().into());
        out.insert("description".into(), self.description.clone().into());
        out.insert("create_at".into(), self.create_at.into());
        out.insert("update_at".into(), self.update_at.into());
        out.insert("delete_at".into(), self.delete_at.into());
        out.insert(
            "permissions".into(),
            match &self.permissions {
                Some(permissions) => permissions.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        out.insert("scheme_managed".into(), self.scheme_managed.into());
        out.insert("built_in".into(), self.built_in.into());
        out.insert(
            "scheme_id".into(),
            match &self.scheme_id {
                Some(id) => id.clone().into(),
                None => serde_json::Value::Null,
            },
        );
        out
    }

    /// Port of `(*Role).Sanitize` (role.go:466) — both fields become `FakeSetting`, and nothing
    /// else is touched. Note this is not a redaction of anything secret: it is what the config
    /// export writes so a role's display strings do not leak into a support packet.
    pub fn sanitize(&mut self) {
        self.display_name = FAKE_SETTING.to_owned();
        self.description = FAKE_SETTING.to_owned();
    }

    /// Port of `(*Role).Patch` (role.go:563).
    pub fn patch(&mut self, patch: &RolePatch) {
        if let Some(permissions) = &patch.permissions {
            self.permissions = Some(permissions.clone());
        }
    }

    /// Port of `(*Role).CreateAt_` (role.go:569) — the `float64` accessors the reporting layer
    /// uses. Above 2^53 the conversion is lossy in both languages, identically.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn create_at_f64(&self) -> f64 {
        self.create_at as f64
    }

    /// Port of `(*Role).UpdateAt_` (role.go:573).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn update_at_f64(&self) -> f64 {
        self.update_at as f64
    }

    /// Port of `(*Role).DeleteAt_` (role.go:577).
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn delete_at_f64(&self) -> f64 {
        self.delete_at as f64
    }

    /// Port of `(*Role).MergeChannelHigherScopedPermissions` (role.go:583).
    ///
    /// The result **is** ordered, and the order is `ALL_PERMISSIONS` filtered to channel scope —
    /// not the role's own order and not the higher scope's. It is the one function in this file
    /// whose order can be asserted, because it ranges a slice rather than a map.
    ///
    /// Three rules, in the order Go applies them: the channel **admin** role takes the higher
    /// scope's answer outright (it is not part of the moderation UI); a moderated permission needs
    /// to be present on *both* the role and the higher scope; anything else follows the higher
    /// scope alone. The result is always `Some`, never `None`, even when empty — Go initialises
    /// with `[]string{}`.
    pub fn merge_channel_higher_scoped_permissions(&mut self, higher_scoped: &RolePermissions) {
        let higher: BTreeSet<&str> = higher_scoped
            .permissions
            .iter()
            .map(String::as_str)
            .collect();
        let own: BTreeSet<&str> = self
            .permissions
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(String::as_str)
            .collect();

        let mut merged = Vec::new();
        for permission in ALL_PERMISSIONS {
            if permission.scope != PERMISSION_SCOPE_CHANNEL {
                continue;
            }
            let id = permission.id.as_ref();
            let present_on_higher = higher.contains(id);

            if higher_scoped.role_id == CHANNEL_ADMIN_ROLE_ID && present_on_higher {
                merged.push(id.to_owned());
                continue;
            }

            if channel_moderated_permission_for(id).is_some() {
                if own.contains(id) && present_on_higher {
                    merged.push(id.to_owned());
                }
            } else if present_on_higher {
                merged.push(id.to_owned());
            }
        }

        self.permissions = Some(merged);
    }

    /// Port of `(*Role).GetChannelModeratedPermissions` (role.go:706).
    ///
    /// Two controls answer differently by channel type: `manage_members` is true only for the
    /// matching public/private permission, and `manage_bookmarks` likewise — and for bookmarks
    /// **only the `add_*` permission is consulted**, because Go's comment says the four bookmark
    /// permissions are enabled and disabled together. Every other control is simply true.
    ///
    /// Go ranges a map here, but the result is a map too and each iteration writes at most one
    /// key, so no answer depends on the order.
    #[must_use]
    pub fn get_channel_moderated_permissions(&self, channel_type: &str) -> BTreeMap<String, bool> {
        let mut moderated: BTreeMap<String, bool> = BTreeMap::new();

        for permission in self.permissions.as_deref().unwrap_or_default() {
            let Some(control) = channel_moderated_permission_for(permission) else {
                continue;
            };
            // Go's `if moderatedPermissions[value] { continue }` — an already-true control is
            // never revisited, so a later permission cannot turn it back off.
            if moderated.get(control).copied().unwrap_or(false) {
                continue;
            }

            let value = if permission.as_str() == PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS.id
                || permission.as_str() == PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS.id
            {
                (channel_type == CHANNEL_TYPE_OPEN
                    && permission.as_str() == PERMISSION_MANAGE_PUBLIC_CHANNEL_MEMBERS.id)
                    || (channel_type == CHANNEL_TYPE_PRIVATE
                        && permission.as_str() == PERMISSION_MANAGE_PRIVATE_CHANNEL_MEMBERS.id)
            } else if is_moderated_bookmark_permission(permission) {
                (channel_type == CHANNEL_TYPE_OPEN
                    && permission.as_str() == PERMISSION_ADD_BOOKMARK_PUBLIC_CHANNEL.id)
                    || (channel_type == CHANNEL_TYPE_PRIVATE
                        && permission.as_str() == PERMISSION_ADD_BOOKMARK_PRIVATE_CHANNEL.id)
            } else {
                true
            };

            moderated.insert(control.to_owned(), value);
        }

        moderated
    }

    /// Port of `(*Role).RolePatchFromChannelModerationsPatch` (role.go:747).
    ///
    /// `role_name` is `"members"` or `"guests"`; anything else matches neither branch and the
    /// patch simply keeps whatever the role already had.
    ///
    /// **Two divergences, both forced by the signature.** Go dereferences `*patch.Name` and
    /// `patch.Roles.Members` without a nil check, so a patch missing either field panics — the
    /// oracle records both. Here they are `Option`s: a missing `name` matches no control and a
    /// missing `roles` disables nothing, which is the choice that cannot hand out a permission Go
    /// would have withheld. [D-127].
    ///
    /// The returned order is sorted; Go's comes from ranging a map and varies per call.
    #[must_use]
    pub fn role_patch_from_channel_moderations_patch(
        &self,
        channel_moderations_patch: &[ChannelModerationPatch],
        role_name: &str,
    ) -> RolePatch {
        let mut keep: BTreeSet<String> = BTreeSet::new();

        for permission in self.permissions.as_deref().unwrap_or_default() {
            let Some(control) = channel_moderated_permission_for(permission) else {
                continue;
            };

            let mut enabled = true;
            for patch in channel_moderations_patch {
                if patch.name.as_deref() != Some(control) {
                    continue;
                }
                let roles = patch.roles.unwrap_or_default();
                let turned_off = match role_name {
                    "members" => roles.members == Some(false),
                    "guests" => roles.guests == Some(false),
                    _ => false,
                };
                if turned_off {
                    enabled = false;
                }
            }

            if enabled {
                keep.insert(permission.clone());
            }
        }

        for patch in channel_moderations_patch {
            let roles = patch.roles.unwrap_or_default();
            let turned_on = match role_name {
                "members" => roles.members == Some(true),
                "guests" => roles.guests == Some(true),
                _ => false,
            };
            if !turned_on {
                continue;
            }
            for (permission, control) in crate::permission::CHANNEL_MODERATED_PERMISSIONS_MAP {
                if patch.name.as_deref() == Some(*control) {
                    keep.insert((*permission).to_owned());
                }
            }
        }

        RolePatch {
            permissions: Some(keep.into_iter().collect()),
        }
    }

    /// Port of `(*Role).IsValid` (role.go:800).
    ///
    /// # Errors
    /// The id must pass `IsValidId`; everything else is [`Role::is_valid_without_id`].
    pub fn is_valid(&self) -> Result<(), RoleError> {
        if !is_valid_id(&self.id) {
            return Err(RoleError::InvalidId(self.id.clone()));
        }
        self.is_valid_without_id()
    }

    /// Port of `(*Role).IsValidWithoutId` (role.go:808).
    ///
    /// The display name is checked for emptiness **before** its length, and both caps are byte
    /// counts. An unknown permission is reported last, with every offender joined by `", "`.
    ///
    /// # Errors
    /// One variant of [`RoleError`] per failing branch, in Go's order.
    pub fn is_valid_without_id(&self) -> Result<(), RoleError> {
        if !is_valid_role_name(&self.name) {
            return Err(RoleError::InvalidName(self.name.clone()));
        }

        if self.display_name.is_empty() {
            return Err(RoleError::EmptyDisplayName);
        }
        if self.display_name.len() > ROLE_DISPLAY_NAME_MAX_LENGTH {
            return Err(RoleError::DisplayNameTooLong(self.display_name.clone()));
        }

        if self.description.len() > ROLE_DESCRIPTION_MAX_LENGTH {
            return Err(RoleError::DescriptionTooLong);
        }

        let unknown = self.unknown_permissions();
        if !unknown.is_empty() {
            return Err(RoleError::UnknownPermissions(unknown));
        }

        Ok(())
    }

    /// Port of `(*Role).UnknownPermissions` (role.go:833) — the permissions on this role that are
    /// in neither `ALL_PERMISSIONS` nor `DEPRECATED_PERMISSIONS`.
    ///
    /// Deprecated permissions are deliberately accepted; Go's comment cites MM-68830. Duplicates
    /// in the role are reported once per occurrence, because Go appends per iteration.
    #[must_use]
    pub fn unknown_permissions(&self) -> Vec<String> {
        let known = |permission: &str| {
            let matches = |table: &[&Permission]| table.iter().any(|p| p.id == permission);
            matches(ALL_PERMISSIONS) || matches(DEPRECATED_PERMISSIONS)
        };

        self.permissions
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|permission| !known(permission))
            .cloned()
            .collect()
    }
}

/// Port of `isModeratedBookmarkPermission` (role.go:696).
#[must_use]
fn is_moderated_bookmark_permission(permission: &str) -> bool {
    MODERATED_BOOKMARK_PERMISSIONS
        .iter()
        .any(|p| p.id == permission)
}

/// Port of `model.PermissionsChangedByPatch` (role.go:622) — the symmetric difference, **ordered**:
/// everything on the role that the patch drops, in the role's order, then everything the patch adds,
/// in the patch's order.
///
/// A nil patch answers empty rather than "everything removed". Duplicates survive: the function
/// tests membership with a map but appends while ranging the slice, so a permission listed twice on
/// the role appears twice in the result.
#[must_use]
pub fn permissions_changed_by_patch(role: &Role, patch: &RolePatch) -> Vec<String> {
    let Some(patch_permissions) = &patch.permissions else {
        return Vec::new();
    };

    let role_permissions = role.permissions.as_deref().unwrap_or_default();
    let role_set: BTreeSet<&str> = role_permissions.iter().map(String::as_str).collect();
    let patch_set: BTreeSet<&str> = patch_permissions.iter().map(String::as_str).collect();

    let mut result = Vec::new();
    result.extend(
        role_permissions
            .iter()
            .filter(|p| !patch_set.contains(p.as_str()))
            .cloned(),
    );
    result.extend(
        patch_permissions
            .iter()
            .filter(|p| !role_set.contains(p.as_str()))
            .cloned(),
    );
    result
}

/// Port of `model.ChannelModeratedPermissionsChangedByPatch` (role.go:655) — the same symmetric
/// difference, but over the moderation **controls** the permissions map onto, so a role losing
/// `add_reaction` and gaining `remove_reaction` has changed nothing.
///
/// A `None` role answers empty, which is Go's explicit nil guard rather than a crash.
///
/// **Order:** Go builds this by ranging two maps, so its order is randomised per call — measured,
/// not inferred ([D-125]). This returns the controls sorted.
#[must_use]
pub fn channel_moderated_permissions_changed_by_patch(
    role: Option<&Role>,
    patch: &RolePatch,
) -> Vec<String> {
    let (Some(role), Some(patch_permissions)) = (role, &patch.permissions) else {
        return Vec::new();
    };

    let controls = |permissions: &[String]| -> BTreeSet<&'static str> {
        permissions
            .iter()
            .filter_map(|p| channel_moderated_permission_for(p))
            .collect()
    };

    let role_controls = controls(role.permissions.as_deref().unwrap_or_default());
    let patch_controls = controls(patch_permissions);

    role_controls
        .symmetric_difference(&patch_controls)
        .map(|control| (*control).to_owned())
        .collect()
}

/// Port of `model.CleanRoleNames` (role.go:852).
///
/// Two things a reading gets wrong. A blank entry is **dropped, not rejected** — but only if it is
/// blank after trimming, and the trimming is *not* applied to the name that gets kept, so
/// `" system_user "` is a validation failure rather than a name to clean. And on failure Go returns
/// the **original** slice untouched alongside `false`, not the partially cleaned one.
///
/// An input with nothing to keep answers `None`, matching Go's nil rather than an empty slice.
#[must_use]
pub fn clean_role_names(role_names: &[String]) -> (Option<Vec<String>>, bool) {
    let mut cleaned: Option<Vec<String>> = None;
    for role_name in role_names {
        if role_name.trim().is_empty() {
            continue;
        }
        if !is_valid_role_name(role_name) {
            return (Some(role_names.to_vec()), false);
        }
        cleaned.get_or_insert_with(Vec::new).push(role_name.clone());
    }
    (cleaned, true)
}

/// Port of `model.IsValidRoleName` (role.go:869).
///
/// Go's `strings.TrimLeft` takes a **cutset**, so this is "every character is in `[a-z0-9_]`" and
/// not a prefix test. The length cap counts bytes.
#[must_use]
pub fn is_valid_role_name(role_name: &str) -> bool {
    if role_name.is_empty() || role_name.len() > ROLE_NAME_MAX_LENGTH {
        return false;
    }
    role_name.chars().all(|c| ROLE_NAME_CUTSET.contains(c))
}

/// Port of `model.IsBuiltInRole` (role.go:892).
///
/// The source of truth is `BUILT_IN_SCHEME_MANAGED_ROLE_IDS` despite its name — eleven of its
/// twenty-four entries are not scheme-managed, which Go's own comment at role.go:886 says.
#[must_use]
pub fn is_built_in_role(role_name: &str) -> bool {
    BUILT_IN_SCHEME_MANAGED_ROLE_IDS.contains(&role_name)
}

/// Port of `model.IsChannelScopedBuiltInRole` (role.go:898).
#[must_use]
pub fn is_channel_scoped_built_in_role(role_name: &str) -> bool {
    role_name == CHANNEL_GUEST_ROLE_ID
        || role_name == CHANNEL_USER_ROLE_ID
        || role_name == CHANNEL_ADMIN_ROLE_ID
}

/// Port of `model.IsValidChannelMemberRoles` (role.go:905) — format validation, plus a rejection
/// of any built-in role that is not one of the three channel-scoped ones.
#[must_use]
pub fn is_valid_channel_member_roles(channel_member_roles: &str) -> bool {
    if !crate::user::is_valid_user_roles(channel_member_roles) {
        return false;
    }
    !channel_member_roles
        .split_whitespace()
        .any(|role_name| is_built_in_role(role_name) && !is_channel_scoped_built_in_role(role_name))
}

/// Port of `model.MakeDefaultRoles` (role.go:919), rebuilt from the generated table.
///
/// Go returns a fresh `map[string]*Role` per call and this returns a fresh `BTreeMap`, which
/// matters: callers mutate the roles they get back. The generator asserts the Go function is
/// stable across two calls before emitting the table.
#[must_use]
pub fn make_default_roles() -> BTreeMap<String, Role> {
    DEFAULT_ROLES
        .iter()
        .map(|default| {
            (
                default.key.to_owned(),
                Role {
                    name: default.name.to_owned(),
                    display_name: default.display_name.to_owned(),
                    description: default.description.to_owned(),
                    permissions: Some(
                        default
                            .permissions
                            .iter()
                            .map(|p| (*p).to_owned())
                            .collect(),
                    ),
                    scheme_managed: default.scheme_managed,
                    built_in: default.built_in,
                    ..Default::default()
                },
            )
        })
        .collect()
}

/// Port of `model.AddAncillaryPermissions` (role.go:1294).
///
/// Go appends to the slice it was handed and returns it, and it ranges the **original slice
/// header** — so ancillary permissions added by the loop are never themselves expanded, and one
/// pass is one level deep. Taking the vector by value reproduces the result without reproducing
/// the aliasing, which is the part of Go's behaviour that depends on the caller's spare capacity.
#[must_use]
pub fn add_ancillary_permissions(mut permissions: Vec<String>) -> Vec<String> {
    let original_len = permissions.len();
    for i in 0..original_len {
        let permission = permissions[i].clone();
        if let Some(ancillary) = sysconsole_ancillary_permissions_for(&permission) {
            permissions.extend(ancillary.iter().map(|p| p.id.to_string()));
        }
    }
    permissions
}

/// The `model.SysconsoleAncillaryPermissions` lookup. The generated table is sorted by key.
#[must_use]
pub fn sysconsole_ancillary_permissions_for(
    permission_id: &str,
) -> Option<&'static [&'static Permission]> {
    SYSCONSOLE_ANCILLARY_PERMISSIONS
        .binary_search_by_key(&permission_id, |(id, _)| *id)
        .ok()
        .map(|i| SYSCONSOLE_ANCILLARY_PERMISSIONS[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(raw: &str) {
        let parsed: T = serde_json::from_str(raw).expect("fixture parses");
        let ours = serde_json::to_value(&parsed).expect("re-serialises");
        let theirs: Value = serde_json::from_str(raw).expect("fixture is JSON");
        assert_eq!(theirs, ours);
    }

    #[test]
    fn role_json_round_trip() {
        round_trip::<Role>(include_str!("../../../fixtures/role.json"));
    }

    #[test]
    fn role_patch_json_round_trip() {
        round_trip::<RolePatch>(include_str!("../../../fixtures/role_patch.json"));
    }

    #[test]
    fn nil_permissions_serialise_as_null_not_empty_array() {
        // The whole reason `permissions` is an `Option`. Go's field has no `omitempty`, so the key
        // is always present and its value is `null` for a nil slice and `[]` for an empty one.
        let nil = Role::default();
        let value = serde_json::to_value(&nil).expect("serialises");
        assert_eq!(value["permissions"], Value::Null);
        assert_eq!(value["scheme_id"], Value::Null);

        let empty = Role {
            permissions: Some(Vec::new()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&empty).expect("serialises")["permissions"],
            Value::Array(vec![])
        );
    }

    /// Asserted against `fixtures/behaviour_role.json` and `fixtures/behaviour_role_tables.json`,
    /// both written by `reference/dump` from the linked Go package.
    mod go_parity {
        use super::*;
        use std::sync::OnceLock;

        fn oracle() -> &'static Value {
            static ORACLE: OnceLock<Value> = OnceLock::new();
            ORACLE.get_or_init(|| {
                serde_json::from_str(include_str!("../../../fixtures/behaviour_role.json"))
                    .expect("behaviour_role.json parses")
            })
        }

        fn tables() -> &'static Value {
            static TABLES: OnceLock<Value> = OnceLock::new();
            TABLES.get_or_init(|| {
                serde_json::from_str(include_str!("../../../fixtures/behaviour_role_tables.json"))
                    .expect("behaviour_role_tables.json parses")
            })
        }

        /// Go marshals a nil slice as `null` and an empty one as `[]`, and several corpus fields
        /// are nil when there is nothing to report. Both read as "no strings" here; the nil-ness
        /// itself is asserted separately, by the `*_nil` flags the corpus carries.
        fn strings(value: &Value) -> Vec<String> {
            match value {
                Value::Null => Vec::new(),
                other => other
                    .as_array()
                    .expect("an array or null")
                    .iter()
                    .map(|v| v.as_str().expect("a string").to_owned())
                    .collect(),
            }
        }

        fn cases(key: &str) -> &'static Vec<Value> {
            oracle()[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} is an array"))
        }

        // --- the generated tables -------------------------------------------------

        #[test]
        fn default_roles_match_go() {
            let ours = make_default_roles();
            let theirs = tables()["default_roles"].as_array().expect("an array");
            assert_eq!(ours.len(), theirs.len());
            assert_eq!(
                ours.len() as u64,
                tables()["counts"]["default_roles"]
                    .as_u64()
                    .expect("a count")
            );

            for expected in theirs {
                let key = expected["key"].as_str().expect("a key");
                let role = ours
                    .get(key)
                    .unwrap_or_else(|| panic!("no default role {key}"));
                assert_eq!(
                    role.name,
                    expected["name"].as_str().expect("a name"),
                    "{key}"
                );
                assert_eq!(
                    role.display_name,
                    expected["display_name"].as_str().unwrap()
                );
                assert_eq!(role.description, expected["description"].as_str().unwrap());
                assert_eq!(
                    role.permissions.as_deref().unwrap_or_default(),
                    strings(&expected["permissions"]).as_slice(),
                    "{key} permissions"
                );
                assert_eq!(
                    role.scheme_managed,
                    expected["scheme_managed"].as_bool().unwrap()
                );
                assert_eq!(role.built_in, expected["built_in"].as_bool().unwrap());
                // Go leaves these zero; a default role has never been persisted.
                assert!(role.id.is_empty() && role.create_at == 0 && role.scheme_id.is_none());
            }
        }

        #[test]
        fn make_default_roles_returns_a_fresh_map() {
            // Go allocates a new map and new Roles per call, and callers mutate what they get.
            let mut first = make_default_roles();
            first
                .get_mut(SYSTEM_ADMIN_ROLE_ID)
                .expect("system_admin")
                .permissions = Some(vec!["MUTATED".to_owned()]);
            let second = make_default_roles();
            assert_ne!(first[SYSTEM_ADMIN_ROLE_ID], second[SYSTEM_ADMIN_ROLE_ID]);
            assert_eq!(
                second[SYSTEM_ADMIN_ROLE_ID]
                    .permissions
                    .as_deref()
                    .unwrap_or_default()
                    .len() as u64,
                tables()["counts"]["system_admin_perms"]
                    .as_u64()
                    .expect("a count"),
            );
        }

        #[test]
        fn id_and_permission_lists_match_go() {
            let pairs: [(&[&str], &str); 7] = [
                (
                    BUILT_IN_SCHEME_MANAGED_ROLE_IDS,
                    "built_in_scheme_managed_role_ids",
                ),
                (NEW_SYSTEM_ROLE_IDS, "new_system_role_ids"),
                (
                    SYSTEM_MANAGER_DEFAULT_PERMISSIONS,
                    "system_manager_default_permissions",
                ),
                (
                    SYSTEM_USER_MANAGER_DEFAULT_PERMISSIONS,
                    "system_user_manager_default_permissions",
                ),
                (
                    SYSTEM_READ_ONLY_ADMIN_DEFAULT_PERMISSIONS,
                    "system_read_only_admin_default_permissions",
                ),
                (
                    SYSTEM_CUSTOM_GROUP_ADMIN_DEFAULT_PERMISSIONS,
                    "system_custom_group_admin_default_permissions",
                ),
                (
                    SHARED_CHANNEL_MANAGER_DEFAULT_PERMISSIONS,
                    "shared_channel_manager_default_permissions",
                ),
            ];
            for (ours, key) in pairs {
                assert_eq!(ours.to_vec(), strings(&tables()[key]), "{key}");
            }
        }

        /// Go's own comment says the name is a misnomer; this is the count behind it.
        #[test]
        fn eleven_built_in_roles_are_not_scheme_managed() {
            let expected = strings(&tables()["built_in_not_scheme_managed"]);
            assert_eq!(expected.len(), 11, "the upstream split changed");

            let roles = make_default_roles();
            let ours: Vec<String> = BUILT_IN_SCHEME_MANAGED_ROLE_IDS
                .iter()
                .filter(|id| !roles[**id].scheme_managed)
                .map(|id| (*id).to_owned())
                .collect();
            assert_eq!(ours, expected);

            // And the two sets are in bijection: every built-in id has a default role and every
            // default role is a built-in id.
            assert!(
                tables()["built_in_with_no_default_role"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            assert!(
                tables()["default_roles_not_built_in"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }

        #[test]
        fn sysconsole_ancillary_permissions_match_go() {
            let expected = tables()["sysconsole_ancillary_permissions"]
                .as_object()
                .expect("an object");
            assert_eq!(SYSCONSOLE_ANCILLARY_PERMISSIONS.len(), expected.len());
            for (key, ids) in expected {
                let ours: Vec<String> = sysconsole_ancillary_permissions_for(key)
                    .unwrap_or_else(|| panic!("no ancillary entry for {key}"))
                    .iter()
                    .map(|p| p.id.to_string())
                    .collect();
                assert_eq!(ours, strings(ids), "{key}");
            }
            assert!(
                SYSCONSOLE_ANCILLARY_PERMISSIONS
                    .windows(2)
                    .all(|w| w[0].0 < w[1].0),
                "the table is not sorted, so the binary search would miss"
            );
        }

        #[test]
        fn constants_match_go() {
            let c = &oracle()["constants"];
            assert_eq!(
                c["role_name_max_length"].as_u64(),
                Some(ROLE_NAME_MAX_LENGTH as u64)
            );
            assert_eq!(
                c["role_display_name_max_length"].as_u64(),
                Some(ROLE_DISPLAY_NAME_MAX_LENGTH as u64)
            );
            assert_eq!(
                c["role_description_max_length"].as_u64(),
                Some(ROLE_DESCRIPTION_MAX_LENGTH as u64)
            );
            assert_eq!(c["fake_setting"].as_str(), Some(FAKE_SETTING));
            assert_eq!(c["role_scope_system"].as_str(), Some(ROLE_SCOPE_SYSTEM));
            assert_eq!(c["role_scope_team"].as_str(), Some(ROLE_SCOPE_TEAM));
            assert_eq!(c["role_scope_channel"].as_str(), Some(ROLE_SCOPE_CHANNEL));
            assert_eq!(c["role_scope_group"].as_str(), Some(ROLE_SCOPE_GROUP));
            assert_eq!(c["role_type_guest"].as_str(), Some(ROLE_TYPE_GUEST));
            assert_eq!(c["role_type_user"].as_str(), Some(ROLE_TYPE_USER));
            assert_eq!(c["role_type_admin"].as_str(), Some(ROLE_TYPE_ADMIN));
        }

        // --- name validation ------------------------------------------------------

        #[test]
        fn is_valid_role_name_matches_go() {
            let all = cases("is_valid_role_name");
            assert!(
                all.len() > 140,
                "the enumerated corpus shrank: {}",
                all.len()
            );
            for case in all {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    is_valid_role_name(input),
                    case["valid"].as_bool().expect("a verdict"),
                    "IsValidRoleName({input:?})"
                );
                assert_eq!(
                    input.len() as u64,
                    case["bytes"].as_u64().expect("a length")
                );
            }
        }

        #[test]
        fn is_valid_user_roles_matches_go() {
            for case in cases("is_valid_user_roles") {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    crate::user::is_valid_user_roles(input),
                    case["valid"].as_bool().expect("a verdict"),
                    "IsValidUserRoles({input:?})"
                );
                // `strings.Fields` and `split_whitespace` must agree, or the loop above is
                // validating a different list than Go's.
                assert_eq!(
                    input.split_whitespace().collect::<Vec<_>>(),
                    strings(&case["fields"]),
                    "Fields({input:?})"
                );
            }
        }

        #[test]
        fn is_valid_channel_member_roles_matches_go() {
            for case in cases("is_valid_channel_member_roles") {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    is_valid_channel_member_roles(input),
                    case["valid"].as_bool().expect("a verdict"),
                    "IsValidChannelMemberRoles({input:?})"
                );
            }
        }

        #[test]
        fn built_in_role_predicates_match_go() {
            for case in cases("is_built_in_role") {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    is_built_in_role(input),
                    case["built_in"].as_bool().expect("a verdict"),
                    "IsBuiltInRole({input:?})"
                );
            }
            for case in cases("is_channel_scoped_built_in_role") {
                let input = case["in"].as_str().expect("an input");
                assert_eq!(
                    is_channel_scoped_built_in_role(input),
                    case["channel_scoped"].as_bool().expect("a verdict"),
                    "IsChannelScopedBuiltInRole({input:?})"
                );
            }
        }

        #[test]
        fn clean_role_names_matches_go() {
            for case in cases("clean_role_names") {
                let input: Vec<String> = case["in"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                let (cleaned, ok) = clean_role_names(&input);
                assert_eq!(ok, case["ok"].as_bool().expect("a verdict"), "{input:?}");
                assert_eq!(
                    cleaned.is_none(),
                    case["cleaned_nil"].as_bool().expect("a nil flag"),
                    "nil-ness for {input:?}"
                );
                let expected: Vec<String> = case["cleaned"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                assert_eq!(
                    cleaned.unwrap_or_default(),
                    expected,
                    "cleaned for {input:?}"
                );
            }
        }

        // --- validation -----------------------------------------------------------

        #[test]
        fn unknown_permissions_matches_go() {
            for case in cases("unknown_permissions") {
                let permissions: Option<Vec<String>> = case["permissions"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect());
                let role = Role {
                    permissions,
                    ..Default::default()
                };
                let ours = role.unknown_permissions();
                let theirs: Vec<String> = case["unknown"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                assert_eq!(ours, theirs, "{:?}", case["permissions"]);
            }
        }

        /// The error *text* as well as the verdict, which is what exercises [`go_quote`] — the
        /// corpus includes a name with a `"`, one with U+007F and one with an emoji, and Go's `%q`
        /// renders all three differently from Rust's `{:?}`.
        #[test]
        fn is_valid_matches_go_including_the_error_text() {
            for case in cases("is_valid") {
                let name = case["name"].as_str().expect("a case name");
                let role: Role =
                    serde_json::from_value(case["role"].clone()).expect("the corpus role parses");

                let with_id = role.is_valid();
                assert_eq!(
                    with_id.is_ok(),
                    case["is_valid_ok"].as_bool().expect("a verdict"),
                    "{name}: IsValid verdict"
                );
                assert_eq!(
                    with_id.err().map(|e| e.to_string()).unwrap_or_default(),
                    case["is_valid_err"].as_str().unwrap_or_default(),
                    "{name}: IsValid message"
                );

                let without_id = role.is_valid_without_id();
                assert_eq!(
                    without_id.is_ok(),
                    case["without_id_ok"].as_bool().expect("a verdict"),
                    "{name}: IsValidWithoutId verdict"
                );
                assert_eq!(
                    without_id.err().map(|e| e.to_string()).unwrap_or_default(),
                    case["without_id_err"].as_str().unwrap_or_default(),
                    "{name}: IsValidWithoutId message"
                );
            }
        }

        // --- value semantics ------------------------------------------------------

        #[test]
        fn clone_matches_go() {
            for case in cases("clone") {
                let name = case["name"].as_str().expect("a case name");
                let cloned: Role =
                    serde_json::from_value(case["clone"].clone()).expect("the corpus role parses");
                assert_eq!(
                    cloned.permissions.is_none(),
                    case["permissions_nil"].as_bool().expect("a flag"),
                    "{name}: nil permissions survive the clone"
                );
                assert_eq!(
                    cloned.scheme_id.is_none(),
                    case["scheme_id_nil"].as_bool().expect("a flag"),
                    "{name}: nil scheme_id survives the clone"
                );
                // Go deep-copies both; ours is a derived `Clone`, which cannot alias at all.
                assert!(!case["permissions_aliased"].as_bool().expect("a flag"));
                assert!(!case["scheme_id_aliased"].as_bool().expect("a flag"));
                let ours = cloned.clone();
                assert_eq!(ours, cloned);
            }
        }

        #[test]
        fn auditable_matches_go() {
            for case in cases("auditable") {
                let role: Role =
                    serde_json::from_value(case["role"].clone()).expect("the corpus role parses");
                let ours = role.auditable();
                let theirs = case["auditable"].as_object().expect("an object");
                assert_eq!(ours.len(), theirs.len());
                for (key, value) in theirs {
                    assert_eq!(ours.get(key), Some(value), "auditable[{key}]");
                }
            }
        }

        #[test]
        fn sanitize_matches_go() {
            for case in cases("sanitize") {
                let mut role = Role {
                    name: "custom_role".to_owned(),
                    display_name: case["display_name_before"].as_str().unwrap().to_owned(),
                    description: case["description_before"].as_str().unwrap().to_owned(),
                    ..Default::default()
                };
                role.sanitize();
                assert_eq!(
                    role.display_name,
                    case["display_name_after"].as_str().unwrap()
                );
                assert_eq!(
                    role.description,
                    case["description_after"].as_str().unwrap()
                );
                assert!(!case["other_fields_touched"].as_bool().expect("a flag"));
                assert_eq!(role.name, "custom_role");
            }
        }

        #[test]
        fn patch_matches_go() {
            for case in cases("patch") {
                let name = case["name"].as_str().expect("a case name");
                let patch = RolePatch {
                    permissions: case["auditable"]["permissions"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
                };
                let mut role = Role {
                    name: "custom_role".to_owned(),
                    permissions: Some(vec!["create_post".to_owned()]),
                    ..Default::default()
                };
                role.patch(&patch);
                assert_eq!(
                    role.permissions.clone().unwrap_or_default(),
                    strings(&case["permissions"]),
                    "{name}"
                );
                assert_eq!(
                    role.permissions.is_none(),
                    case["permissions_nil"].as_bool().expect("a flag"),
                    "{name}: nil-ness"
                );
            }
        }

        #[test]
        fn float_accessors_match_go() {
            for case in cases("float_accessors") {
                let millis = case["in"].as_i64().expect("an i64");
                let role = Role {
                    create_at: millis,
                    update_at: millis,
                    delete_at: millis,
                    ..Default::default()
                };
                assert_eq!(role.create_at_f64(), case["create_at"].as_f64().unwrap());
                assert_eq!(role.update_at_f64(), case["update_at"].as_f64().unwrap());
                assert_eq!(role.delete_at_f64(), case["delete_at"].as_f64().unwrap());
            }
        }

        // --- patch diffing --------------------------------------------------------

        /// This one asserts the **order**, because Go ranges slices rather than maps here.
        #[test]
        fn permissions_changed_by_patch_matches_go_in_order() {
            for case in cases("permissions_changed_by_patch") {
                let name = case["name"].as_str().expect("a case name");
                let role = Role {
                    permissions: case["role"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
                    ..Default::default()
                };
                let patch = RolePatch {
                    permissions: case["patch"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
                };
                let ours = permissions_changed_by_patch(&role, &patch);
                let theirs: Vec<String> = case["changed"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                assert_eq!(ours, theirs, "{name}");
            }
        }

        /// Here Go's order is randomised per call — the corpus records that it varied — so this
        /// compares sorted sets. [D-125].
        #[test]
        fn channel_moderated_permissions_changed_matches_go_as_a_set() {
            let mut saw_varying_order = false;
            for case in cases("channel_moderated_permissions_changed") {
                let name = case["name"].as_str().expect("a case name");
                saw_varying_order |= case["order_varied"].as_bool().expect("a flag");

                let role = Role {
                    permissions: case["role"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
                    ..Default::default()
                };
                let patch = RolePatch {
                    permissions: case["patch"]
                        .as_array()
                        .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect()),
                };
                let role_arg = if case["role_nil"].as_bool().expect("a flag") {
                    None
                } else {
                    Some(&role)
                };

                let mut ours = channel_moderated_permissions_changed_by_patch(role_arg, &patch);
                ours.sort();
                assert_eq!(ours, strings(&case["changed"]), "{name}");
            }
            assert!(
                saw_varying_order,
                "no case observed Go's randomised order, so the set comparison is untested"
            );
        }

        // --- moderation -----------------------------------------------------------

        #[test]
        fn get_channel_moderated_permissions_matches_go() {
            let all = cases("get_channel_moderated_permissions");
            assert!(
                all.len() > 50,
                "the enumerated corpus shrank: {}",
                all.len()
            );
            for case in all {
                let permissions = strings(&case["permissions"]);
                let channel_type = case["channel_type"].as_str().expect("a channel type");
                let role = Role {
                    permissions: Some(permissions.clone()),
                    ..Default::default()
                };
                let ours = role.get_channel_moderated_permissions(channel_type);
                let theirs = case["result"].as_object().expect("an object");
                assert_eq!(
                    ours.len(),
                    theirs.len(),
                    "{permissions:?} in a {channel_type} channel"
                );
                for (control, value) in theirs {
                    assert_eq!(
                        ours.get(control).copied(),
                        value.as_bool(),
                        "{permissions:?} in a {channel_type} channel: {control}"
                    );
                }
            }
        }

        /// Ordered, and the order is `ALL_PERMISSIONS` filtered to channel scope — asserting it is
        /// how a port that rebuilt the list from the role's own order gets caught.
        #[test]
        fn merge_channel_higher_scoped_permissions_matches_go_in_order() {
            for case in cases("merge_channel_higher_scoped_permissions") {
                let name = case["name"].as_str().expect("a case name");
                let mut role = Role {
                    permissions: Some(strings(&case["role"])),
                    ..Default::default()
                };
                role.merge_channel_higher_scoped_permissions(&RolePermissions {
                    role_id: case["higher_role_id"]
                        .as_str()
                        .expect("a role id")
                        .to_owned(),
                    permissions: strings(&case["higher"]),
                });
                assert_eq!(
                    role.permissions.clone().unwrap_or_default(),
                    strings(&case["merged"]),
                    "{name}"
                );
                assert!(
                    !case["merged_nil"].as_bool().expect("a flag"),
                    "{name}: Go always produces a non-nil slice here"
                );
                assert!(role.permissions.is_some());
            }
        }

        #[test]
        fn role_patch_from_channel_moderations_matches_go_as_a_set() {
            for case in cases("role_patch_from_channel_moderations_patch") {
                if case.get("panics").is_some() {
                    // Go dereferences Name and Roles unguarded; our Options make those states
                    // unrepresentable. Recorded, not reproduced. [D-127].
                    assert!(case["panics"].as_bool().expect("a flag"));
                    continue;
                }
                let name = case["name"].as_str().expect("a case name");
                let role = Role {
                    permissions: Some(strings(&case["role"])),
                    ..Default::default()
                };
                let patches = moderation_patches_for(name);
                let patch = role.role_patch_from_channel_moderations_patch(
                    &patches,
                    case["role_name"].as_str().expect("a role name"),
                );
                assert_eq!(
                    patches.len() as u64,
                    case["patch_count"].as_u64().expect("a count"),
                    "{name}: the test's patch list drifted from the corpus"
                );
                let mut ours = patch.permissions.clone().unwrap_or_default();
                ours.sort();
                assert_eq!(ours, strings(&case["permissions"]), "{name}");
                assert!(!case["permissions_nil"].as_bool().expect("a flag"));
            }
        }

        /// The corpus records only the *shape* of each moderation patch, so the patches themselves
        /// are rebuilt here by case name; `patch_count` above is the check that the two agree.
        fn moderation_patches_for(case: &str) -> Vec<ChannelModerationPatch> {
            use crate::channel::ChannelModeratedRolesPatch;
            let patch = |name: &str, members: Option<bool>, guests: Option<bool>| {
                vec![ChannelModerationPatch {
                    name: Some(name.to_owned()),
                    roles: Some(ChannelModeratedRolesPatch { members, guests }),
                }]
            };
            match case {
                "empty_patch_keeps_moderated" => Vec::new(),
                "disable_members" => patch("create_post", Some(false), None),
                "disable_guests_leaves_members" => patch("create_post", None, Some(false)),
                "enable_adds_permission" => patch("create_reactions", Some(true), None),
                "enable_for_guests" => patch("create_reactions", None, Some(true)),
                "enable_bookmarks_adds_all_eight" => patch("manage_bookmarks", Some(true), None),
                "unknown_role_name" => patch("create_post", Some(false), None),
                "nil_bool_is_not_false" => patch("create_post", None, None),
                other => panic!("no patch list for corpus case {other}"),
            }
        }

        // --- ancillary permissions ------------------------------------------------

        #[test]
        fn add_ancillary_permissions_matches_go() {
            let all = cases("add_ancillary_permissions");
            assert!(
                all.len() > 28,
                "the enumerated corpus shrank: {}",
                all.len()
            );
            for case in all {
                let input: Vec<String> = case["in"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                let ours = add_ancillary_permissions(input.clone());
                let theirs: Vec<String> = case["out"]
                    .as_array()
                    .map(|a| a.iter().map(|v| v.as_str().unwrap().to_owned()).collect())
                    .unwrap_or_default();
                assert_eq!(ours, theirs, "AddAncillaryPermissions({input:?})");
            }
        }

        /// One pass, one level: an ancillary permission that is itself a sysconsole key is not
        /// expanded, because Go ranges the original slice header while appending to it.
        #[test]
        fn add_ancillary_permissions_does_not_recurse() {
            let two_level = SYSCONSOLE_ANCILLARY_PERMISSIONS
                .iter()
                .find(|(_, ancillary)| {
                    ancillary
                        .iter()
                        .any(|p| sysconsole_ancillary_permissions_for(&p.id).is_some())
                });
            let Some((key, ancillary)) = two_level else {
                // Nothing in the table is two levels deep today; the guard above is what would
                // notice if that changed.
                return;
            };
            let out = add_ancillary_permissions(vec![(*key).to_owned()]);
            let nested: Vec<&str> = ancillary
                .iter()
                .filter_map(|p| sysconsole_ancillary_permissions_for(&p.id))
                .flatten()
                .map(|p| p.id.as_ref())
                .collect();
            for id in nested {
                assert!(
                    !out.contains(&id.to_owned()),
                    "{id} was expanded, but Go stops after one level"
                );
            }
        }
    }
}
