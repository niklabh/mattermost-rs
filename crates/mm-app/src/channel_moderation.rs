//! Port of `App.GetChannelModerationsForChannel` and `buildChannelModerations`
//! (channels/app/channel.go:1169, :1357), `App.GetMemberCountsByGroup` (channel.go:4205) and
//! `App.GetPropertyGroup` (app/property_group.go:25) — the licensed halves of three reads that
//! forwarded on "any licence" until 2026-09-13.
//!
//! None of the three needs anything enterprise: the moderations are a pure function of four
//! roles the scheme helpers already resolve, the counts are one query, and the property group is
//! one row. What kept them forwarded was that the licence gate in front of them could not be
//! answered; it can now, and the licensed Go oracle answers beside them.

use mm_model::channel::{
    Channel, ChannelMemberCountByGroup, ChannelModeratedRole, ChannelModeratedRoles,
    ChannelModeration,
};
use mm_model::property_group::PropertyGroup;
use mm_model::role::Role;
use mm_model::utils::{AppError, AppResult};
use mm_store::{ChannelStore, PropertyStore, StoreError};

use crate::App;

impl App {
    /// Port of `App.GetChannelModerationsForChannel` (channel.go:1169).
    ///
    /// Four roles: the channel's member and guest roles, and the **higher-scoped** pair from the
    /// team's scheme (or the defaults), which decide whether each moderation is `enabled` at all.
    /// A channel with no guest role — the scheme helpers return `""` for it — is `None` here and
    /// nil in Go, and `buildChannelModerations` reads a nil role as an empty permission map.
    #[tracing::instrument(skip_all, fields(channel_id = %channel.id))]
    pub async fn get_channel_moderations_for_channel(
        &self,
        channel: &Channel,
    ) -> AppResult<Vec<ChannelModeration>> {
        let (guest_role_name, member_role_name, _) =
            self.get_scheme_roles_for_channel(&channel.id).await?;
        let member_role = self.get_role_by_name(&member_role_name).await?;
        let guest_role = if guest_role_name.is_empty() {
            None
        } else {
            Some(self.get_role_by_name(&guest_role_name).await?)
        };

        let (higher_guest_name, higher_member_name, _) =
            self.get_team_scheme_channel_roles(&channel.team_id).await?;
        let higher_member_role = self.get_role_by_name(&higher_member_name).await?;
        let higher_guest_role = if higher_guest_name.is_empty() {
            None
        } else {
            Some(self.get_role_by_name(&higher_guest_name).await?)
        };

        Ok(build_channel_moderations(
            &channel.channel_type,
            Some(&member_role),
            guest_role.as_ref(),
            Some(&higher_member_role),
            higher_guest_role.as_ref(),
        ))
    }

    /// Port of `App.GetMemberCountsByGroup` (channel.go:4205) — one query, one error id.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, include_timezones))]
    pub async fn get_member_counts_by_group(
        &self,
        channel_id: &str,
        include_timezones: bool,
    ) -> AppResult<Vec<ChannelMemberCountByGroup>> {
        self.store()
            .channel()
            .get_member_counts_by_group(channel_id, include_timezones)
            .await
            .map_err(|err| {
                tracing::error!(error = ?err, "counting channel members by group failed");
                AppError::boxed(
                    "GetMemberCountsByGroup",
                    "app.channel.get_member_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `App.GetPropertyGroup` (app/property_group.go:25): one id on both branches, a
    /// **404** for a name that is not registered and a 500 for anything else.
    #[tracing::instrument(skip_all, fields(name = %name))]
    pub async fn get_property_group(&self, name: &str) -> AppResult<PropertyGroup> {
        self.store()
            .property()
            .get_group(name)
            .await
            .map_err(|err| {
                let status = match err {
                    StoreError::NotFound { .. } => 404,
                    _ => 500,
                };
                AppError::boxed(
                    "GetPropertyGroup",
                    "app.property_group.get.app_error",
                    None,
                    String::new(),
                    status,
                )
            })
    }
}

/// Port of `buildChannelModerations` (channel.go:1357).
///
/// One entry per `ChannelModeratedPermissions` key, **in that order** — the response is an array
/// and clients index it. `value` is what the role grants; `enabled` is what the higher-scoped
/// role grants, i.e. whether the console may toggle it. `manage_members` and `manage_bookmarks`
/// carry **no guest entry** (`roles.guests` is nil), the two permissions a guest can never hold.
/// A nil role reads as a map with nothing in it, so every lookup is `false`.
pub fn build_channel_moderations(
    channel_type: &str,
    member_role: Option<&Role>,
    guest_role: Option<&Role>,
    higher_scoped_member_role: Option<&Role>,
    higher_scoped_guest_role: Option<&Role>,
) -> Vec<ChannelModeration> {
    let permissions = |role: Option<&Role>| {
        role.map(|r| r.get_channel_moderated_permissions(channel_type))
            .unwrap_or_default()
    };
    let member = permissions(member_role);
    let guest = permissions(guest_role);
    let higher_member = permissions(higher_scoped_member_role);
    let higher_guest = permissions(higher_scoped_guest_role);
    let granted = |map: &std::collections::BTreeMap<String, bool>, key: &str| {
        map.get(key).copied().unwrap_or(false)
    };

    mm_model::permission::CHANNEL_MODERATED_PERMISSIONS
        .iter()
        .map(|key| {
            let guests = if *key == "manage_members" || *key == "manage_bookmarks" {
                None
            } else {
                Some(ChannelModeratedRole {
                    value: granted(&guest, key),
                    enabled: granted(&higher_guest, key),
                })
            };
            ChannelModeration {
                name: (*key).to_owned(),
                roles: Some(ChannelModeratedRoles {
                    members: Some(ChannelModeratedRole {
                        value: granted(&member, key),
                        enabled: granted(&higher_member, key),
                    }),
                    guests,
                }),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn role(permissions: &[&str]) -> Role {
        Role {
            permissions: Some(permissions.iter().map(|p| (*p).to_owned()).collect()),
            ..Role::default()
        }
    }

    /// Five entries in Go's order, two of them with no guest arm, and `value`/`enabled` drawn
    /// from the channel role and the higher-scoped role respectively — not the other way round.
    #[test]
    fn five_moderations_in_order_and_two_without_a_guest_arm() {
        let member = role(&["create_post"]);
        let higher_member = role(&["create_post", "add_reaction", "remove_reaction"]);
        let guest = role(&[]);
        let higher_guest = role(&["create_post"]);
        let moderations = build_channel_moderations(
            "O",
            Some(&member),
            Some(&guest),
            Some(&higher_member),
            Some(&higher_guest),
        );
        let names: Vec<&str> = moderations.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "create_post",
                "create_reactions",
                "manage_members",
                "use_channel_mentions",
                "manage_bookmarks"
            ]
        );
        let create_post = &moderations[0];
        let roles = create_post.roles.as_ref().unwrap();
        assert_eq!(
            roles.members,
            Some(ChannelModeratedRole {
                value: true,
                enabled: true
            })
        );
        assert_eq!(
            roles.guests,
            Some(ChannelModeratedRole {
                value: false,
                enabled: true
            }),
            "the guest arm reads the guest role for value and the higher guest role for enabled"
        );
        let reactions = moderations[1].roles.as_ref().unwrap();
        assert_eq!(
            reactions.members,
            Some(ChannelModeratedRole {
                value: false,
                enabled: true
            }),
            "create_reactions is the pair add/remove on the higher role, absent on the member role"
        );
        assert!(moderations[2].roles.as_ref().unwrap().guests.is_none());
        assert!(moderations[4].roles.as_ref().unwrap().guests.is_none());
        assert!(moderations[3].roles.as_ref().unwrap().guests.is_some());
    }

    /// A nil role is an empty map: every lookup false, no panic, still five entries.
    #[test]
    fn nil_roles_read_as_nothing_granted() {
        let moderations = build_channel_moderations("P", None, None, None, None);
        assert_eq!(moderations.len(), 5);
        for m in &moderations {
            let roles = m.roles.as_ref().unwrap();
            assert_eq!(
                roles.members.as_ref().map(|r| (r.value, r.enabled)),
                Some((false, false))
            );
        }
    }
}
