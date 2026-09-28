//! The plugin API's user, status, preference and team methods (app/plugin_api.go), each a thin
//! wrapper over the app function a REST route already uses — docs/PLUGIN_PLAN.md, Phase 6.
//!
//! The trait methods in `crate::plugin_api` delegate here one line each; what each answers, and
//! why, is on the function below.
//!
//! # Sanitisation is per method, and it is not REST's
//!
//! `plugin_api.go` itself sanitises nothing. What a plugin receives is whatever the app and
//! store functions behind the method leave:
//!
//! - `GetUsers`, `GetUsersInTeam` and `GetUsersInChannel` read through `SqlUserStore` queries
//!   that run `Sanitize(map[string]bool{})` on every row: no password, MFA secret, MFA
//!   timestamps or `LastLogin`, but the email, the full name and the auth fields intact.
//! - `GetUsersByIds` reads through `GetMany`, the one query that sanitises nothing — the plugin
//!   gets the password hash.
//! - `SearchUsers` is `SanitizeProfile(user, isAdmin: true)`, as `GetUsersByUsernames` is.
//! - The writes answer the store's `Update`/`Save` result, which is `Sanitize(map{})`ed there.
//!
//! # Go's user cache decides two answers there, and nothing here
//!
//! `GetMany` and `GetProfileByIds` share one cache in Go's `LocalCacheUserStore`, and the second
//! stores **sanitised** rows. So a user some REST read fetched by id recently can come back to a
//! plugin's `GetUsersByIds` from that cache without its password hash, where this server — which
//! caches nothing — always answers the row. And `GetAllProfiles` with empty options, page 0 and
//! exactly 100 per page (the webapp's call) is answered from `allUserCache` there, as stale as
//! its last invalidation. The users tranche asks neither shape of a warm cache.
//!
//! # What is not implemented, per call
//!
//! A shape this server cannot answer is Go's `API <Name> called but not implemented.`, decided
//! before anything is written: `GetUsers` with a role filter, the `update_at_asc` sort,
//! `UpdatedAfter` or view restrictions ([D-1030]); `SearchUsers` outside a team or a channel
//! ([D-1031]); `GetProfileImage` on a storage driver this port does not implement; and a
//! deactivation of a user who owns bots ([D-461]).

use mm_model::team::Team;
use mm_model::team_search::TeamSearch;
use mm_model::user::User;
use mm_model::utils::AppError;
use mm_plugin::rpc::NotImplemented;
use mm_plugin::wire::plugin as api;
use mm_store::team_store::TeamMembersGetOptions;
use mm_store::user_store::UserSearchOptions;

use super::AppPluginApi;
use crate::plugin_api_wire::{
    custom_status_from_wire, preference_to_wire, preferences_from_wire, status_to_wire,
    team_from_wire, team_stats_to_wire, team_to_wire, team_unread_to_wire,
};
use crate::plugin_hooks::{HookContext, team_member_to_wire, user_to_wire};
use crate::post::PrepareError;
use crate::user::UserPage;

/// A page of `UserGetOptions` with neither activity flag set, as every plugin method but
/// `GetUsers` builds it.
fn plain_page(page: i64, per_page: i64) -> UserPage {
    UserPage {
        page,
        per_page,
        inactive: false,
        active: false,
    }
}

/// Why `GetUsers` cannot answer these options here, or `None` when it can: the store query
/// behind [`crate::App::get_users_from_profiles`] has no role filter, no `update_at_asc` sort,
/// no `UpdatedAfter` and no view restrictions. `InTeamId`, `InChannelId` and the rest are
/// **ignored** by Go's `GetAllProfiles`, so they are not a reason.
pub fn get_users_unanswerable(options: &mm_plugin::wire::model::UserGetOptions) -> Option<&str> {
    if !options.role.is_empty() || !options.roles.is_empty() {
        Some("a role filter on GetAllProfiles (D-1030)")
    } else if options.sort == "update_at_asc" {
        Some("the update_at_asc sort on GetAllProfiles (D-1030)")
    } else if options.updated_after > 0 {
        Some("UpdatedAfter on GetAllProfiles (D-1030)")
    } else if options.view_restrictions.is_some() {
        Some("view restrictions on GetAllProfiles (D-1030)")
    } else {
        None
    }
}

/// Which arm of `App.SearchUsers` (app/user.go:2412) a search takes, in Go's order.
#[derive(Debug, PartialEq, Eq)]
pub enum SearchArm<'a> {
    /// `SearchUsersInTeam`, the fall-through — an empty team searches everyone.
    InTeam(&'a str),
    /// `SearchUsersInChannel`.
    InChannel(&'a str),
    /// Any other arm, named: not ported ([D-1031]).
    Unported(&'static str),
}

/// Port of `App.SearchUsers`' dispatch (app/user.go:2412). The props' role fields are **not**
/// read: the plugin API builds its options from `IsAdmin`, `AllowInactive` and `Limit` alone, so
/// a role in the search changes nothing on either server.
pub fn search_arm(search: &mm_plugin::wire::model::UserSearch) -> SearchArm<'_> {
    if search.without_team {
        SearchArm::Unported("WithoutTeam")
    } else if !search.in_channel_id.is_empty() {
        SearchArm::InChannel(&search.in_channel_id)
    } else if !search.not_in_channel_id.is_empty() {
        SearchArm::Unported("NotInChannelId")
    } else if !search.not_in_team_id.is_empty() {
        SearchArm::Unported("NotInTeamId")
    } else if !search.in_group_id.is_empty() {
        SearchArm::Unported("InGroupId")
    } else if !search.not_in_group_id.is_empty() {
        SearchArm::Unported("NotInGroupId")
    } else {
        SearchArm::InTeam(&search.team_id)
    }
}

/// `UpdateUserStatus`'s refusal of a status it does not know — `ooo` included, which the model
/// holds and this method cannot set.
fn bad_status() -> Box<AppError> {
    AppError::boxed(
        "UpdateUserStatus",
        "plugin.api.update_user_status.bad_status",
        None,
        "unrecognized status",
        400,
    )
}

/// `GetUsersInChannel`'s refusal of a sort other than `username` or `status`.
fn bad_channel_sort() -> Box<AppError> {
    AppError::boxed(
        "GetUsersInChannel",
        "plugin.api.get_users_in_channel",
        None,
        "invalid sort option",
        400,
    )
}

impl AppPluginApi {
    /// A list read's two returns, each element converted.
    pub(super) fn reply_list<T, W>(
        &self,
        result: Result<Vec<T>, Box<AppError>>,
        convert: impl Fn(&T) -> W,
    ) -> (Vec<W>, Option<Box<mm_plugin::wire::model::AppError>>) {
        match result {
            Ok(items) => (items.iter().map(convert).collect(), None),
            Err(err) => (Vec::new(), self.wire(err)),
        }
    }

    // -- users ----------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetUsers` (app/plugin_api.go:277): `GetUsersFromProfiles`, the whole
    /// installation in username order, store-sanitised. A nil options pointer — Go dereferences
    /// it and panics — is the zero options, whose `PerPage` of 0 is an empty page.
    pub(super) async fn users_get_users(
        &self,
        args: api::Z_GetUsersArgs,
    ) -> Result<api::Z_GetUsersReturns, NotImplemented> {
        let options = args.a.map(|o| *o).unwrap_or_default();
        if let Some(why) = get_users_unanswerable(&options) {
            return Err(self.not_implemented("GetUsers", why));
        }
        let page = UserPage {
            page: options.page,
            per_page: options.per_page,
            inactive: options.inactive,
            active: options.active,
        };
        let (a, b) = self.reply_list(self.app.get_users_from_profiles(page).await, user_to_wire);
        Ok(api::Z_GetUsersReturns { a, b })
    }

    /// Port of `PluginAPI.GetUsersByIds` (app/plugin_api.go:281): `App.GetUsers`, **unsanitised**
    /// and in no promised order.
    pub(super) async fn users_get_users_by_ids(
        &self,
        args: api::Z_GetUsersByIdsArgs,
    ) -> Result<api::Z_GetUsersByIdsReturns, NotImplemented> {
        let (a, b) = self.reply_list(self.app.get_users(&args.a).await, user_to_wire);
        Ok(api::Z_GetUsersByIdsReturns { a, b })
    }

    /// Port of `PluginAPI.GetUsersInChannel` (app/plugin_api.go:426): `username` or `status`
    /// order, and any other sort is Go's 400 before anything is read.
    pub(super) async fn users_get_users_in_channel(
        &self,
        args: api::Z_GetUsersInChannelArgs,
    ) -> Result<api::Z_GetUsersInChannelReturns, NotImplemented> {
        let page = plain_page(args.c, args.d);
        let result = match args.b.as_str() {
            mm_model::channel::CHANNEL_SORT_BY_USERNAME => {
                self.app.get_users_in_channel(&args.a, page).await
            }
            mm_model::channel::CHANNEL_SORT_BY_STATUS => {
                self.app.get_users_in_channel_by_status(&args.a, page).await
            }
            _ => Err(bad_channel_sort()),
        };
        let (a, b) = self.reply_list(result, user_to_wire);
        Ok(api::Z_GetUsersInChannelReturns { a, b })
    }

    /// Port of `PluginAPI.GetUsersInTeam` (app/plugin_api.go:305): the team's current members,
    /// username order, store-sanitised.
    pub(super) async fn users_get_users_in_team(
        &self,
        args: api::Z_GetUsersInTeamArgs,
    ) -> Result<api::Z_GetUsersInTeamReturns, NotImplemented> {
        let result = self
            .app
            .get_users_in_team(&args.a, plain_page(args.b, args.c))
            .await;
        let (a, b) = self.reply_list(result, user_to_wire);
        Ok(api::Z_GetUsersInTeamReturns { a, b })
    }

    /// Port of `PluginAPI.SearchUsers` (app/plugin_api.go:630): the options are `IsAdmin`, the
    /// search's `AllowInactive` and its `Limit` **as sent** — no default, so a zero limit is an
    /// empty answer — and neither `AllowEmails` nor `AllowFullNames`, so a plugin's search
    /// matches usernames, nicknames and ids only. Each hit is `SanitizeProfile`d as an admin.
    pub(super) async fn users_search_users(
        &self,
        args: api::Z_SearchUsersArgs,
    ) -> Result<api::Z_SearchUsersReturns, NotImplemented> {
        let search = args.a.map(|s| *s).unwrap_or_default();
        let options = UserSearchOptions {
            allow_full_names: false,
            allow_emails: false,
            allow_inactive: search.allow_inactive,
            limit: search.limit,
            // `api.app.SearchUsers(search, options)` with no `ViewRestrictions` set.
            view_restrictions: None,
        };
        let result = match search_arm(&search) {
            SearchArm::InTeam(team) => {
                self.app
                    .search_users_in_team(team, &search.term, &options)
                    .await
            }
            SearchArm::InChannel(channel) => {
                self.app
                    .search_users_in_channel(channel, &search.term, &options)
                    .await
            }
            SearchArm::Unported(arm) => {
                return Err(self.not_implemented("SearchUsers", arm));
            }
        };
        let (a, b) = self.reply_list(result, |user| {
            let mut user = user.clone();
            self.app.sanitize_profile(&mut user, true);
            user_to_wire(&user)
        });
        Ok(api::Z_SearchUsersReturns { a, b })
    }

    /// Port of `PluginAPI.GetProfileImage` (app/plugin_api.go:1016): the user, then
    /// [`crate::App::get_profile_image`] — the stored picture, or the generated avatar (written
    /// back when `LastPictureUpdate == 0`). Only an unimplemented storage driver is not
    /// implemented here.
    pub(super) async fn users_get_profile_image(
        &self,
        args: api::Z_GetProfileImageArgs,
    ) -> Result<api::Z_GetProfileImageReturns, NotImplemented> {
        let user = match self.app.get_user(&args.a).await {
            Ok(user) => user,
            Err(err) => {
                return Ok(api::Z_GetProfileImageReturns {
                    a: Vec::new(),
                    b: self.wire(err),
                });
            }
        };
        match self.app.get_profile_image(&user).await {
            Ok((bytes, _)) => Ok(api::Z_GetProfileImageReturns { a: bytes, b: None }),
            Err(PrepareError::App(err)) => Ok(api::Z_GetProfileImageReturns {
                a: Vec::new(),
                b: self.wire(err),
            }),
            Err(PrepareError::Unreproducible(why)) => {
                Err(self.not_implemented("GetProfileImage", why))
            }
        }
    }

    /// Port of `PluginAPI.UpdateUser` (app/plugin_api.go:369): `App.UpdateUser` with
    /// notifications on, the plugin's user taken whole. A nil user is the zero user, which is
    /// no user.
    pub(super) async fn users_update_user(
        &self,
        args: api::Z_UpdateUserArgs,
    ) -> Result<api::Z_UpdateUserReturns, NotImplemented> {
        let user = args
            .a
            .as_deref()
            .map(crate::plugin_hooks::user_from_wire)
            .unwrap_or_default();
        let (a, b) = self.reply(self.app.update_user(&user, true).await, |u| {
            user_to_wire(&u)
        });
        Ok(api::Z_UpdateUserReturns { a, b })
    }

    /// `App.UpdateUserActive` (app/user.go:1693) as the plugin API reaches it: `GetUser`, then
    /// `UpdateActive`. None of `updateUserActive`'s REST gates — no permission, no guest or LDAP
    /// refusal — only the seat limit on activation. A deactivation of a bot owner is not
    /// implemented (its bots and the admins' DM are Go's, [D-461]), decided before the write.
    async fn set_active(
        &self,
        method: &'static str,
        user_id: &str,
        active: bool,
    ) -> Result<Option<Box<mm_plugin::wire::model::AppError>>, NotImplemented> {
        let user = match self.app.get_user(user_id).await {
            Ok(user) => user,
            Err(err) => return Ok(self.wire(err)),
        };
        let result = if active {
            self.app.activate_user(&user).await.map(|_| ())
        } else {
            match self.app.owns_bots(&user.id).await {
                Ok(true) => {
                    return Err(self.not_implemented(
                        method,
                        "a bot owner's deactivation disables the bots and DMs the admins (D-461)",
                    ));
                }
                Ok(false) => self
                    .app
                    .deactivate_user(&HookContext::default(), &user)
                    .await
                    .map(|_| ()),
                Err(err) => Err(err),
            }
        };
        Ok(result.err().and_then(|err| self.wire(err)))
    }

    /// Port of `PluginAPI.UpdateUserActive` (app/plugin_api.go:377); see [`Self::set_active`].
    pub(super) async fn users_update_user_active(
        &self,
        args: api::Z_UpdateUserActiveArgs,
    ) -> Result<api::Z_UpdateUserActiveReturns, NotImplemented> {
        Ok(api::Z_UpdateUserActiveReturns {
            a: self.set_active("UpdateUserActive", &args.a, args.b).await?,
        })
    }

    /// Port of `PluginAPI.DeleteUser` (app/plugin_api.go:268): a **deactivation**, not a delete —
    /// `GetUser` and `UpdateActive(user, false)`; see [`Self::set_active`].
    pub(super) async fn users_delete_user(
        &self,
        args: api::Z_DeleteUserArgs,
    ) -> Result<api::Z_DeleteUserReturns, NotImplemented> {
        Ok(api::Z_DeleteUserReturns {
            a: self.set_active("DeleteUser", &args.a, false).await?,
        })
    }

    /// Port of `PluginAPI.UpdateUserRoles` (app/plugin_api.go:1267): `UpdateUserRoles` with the
    /// websocket event — so a missing user is the **400** that function turns `GetUser`'s 404
    /// into, and an unknown role is refused before anything is written.
    pub(super) async fn users_update_user_roles(
        &self,
        args: api::Z_UpdateUserRolesArgs,
    ) -> Result<api::Z_UpdateUserRolesReturns, NotImplemented> {
        let result = self.app.update_user_roles(&args.a, &args.b, true).await;
        let (a, b) = self.reply(result, |u| user_to_wire(&u));
        Ok(api::Z_UpdateUserRolesReturns { a, b })
    }

    /// Port of `PluginAPI.CreateUser` (app/plugin_api.go:264): `App.CreateUser`, with none of
    /// the REST route's sign-up gates or `SanitizeInput` — the plugin's user is taken whole,
    /// and the roles are still overwritten with `system_user`. A nil user is the zero user,
    /// which the model refuses.
    pub(super) async fn users_create_user(
        &self,
        args: api::Z_CreateUserArgs,
    ) -> Result<api::Z_CreateUserReturns, NotImplemented> {
        let user: User = args
            .a
            .as_deref()
            .map(crate::plugin_hooks::user_from_wire)
            .unwrap_or_default();
        let result = self.app.create_user(&HookContext::default(), &user).await;
        let (a, b) = self.reply(result, |u| user_to_wire(&u));
        Ok(api::Z_CreateUserReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateUserCustomStatus` (app/plugin_api.go:414): `SetCustomStatus`
    /// without the REST route's `EnableCustomUserStatuses` gate or duration check. A nil status
    /// is Go's first refusal, which the empty status also takes.
    pub(super) async fn users_update_user_custom_status(
        &self,
        args: api::Z_UpdateUserCustomStatusArgs,
    ) -> Result<api::Z_UpdateUserCustomStatusReturns, NotImplemented> {
        let status = args
            .b
            .as_deref()
            .map(custom_status_from_wire)
            .unwrap_or_default();
        let result = self
            .app
            .set_custom_status(&HookContext::default(), &args.a, &status)
            .await;
        Ok(api::Z_UpdateUserCustomStatusReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.RemoveUserCustomStatus` (app/plugin_api.go:418).
    pub(super) async fn users_remove_user_custom_status(
        &self,
        args: api::Z_RemoveUserCustomStatusArgs,
    ) -> Result<api::Z_RemoveUserCustomStatusReturns, NotImplemented> {
        let result = self.app.remove_custom_status(&args.a).await;
        Ok(api::Z_RemoveUserCustomStatusReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- status ---------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetUserStatus` (app/plugin_api.go:381): `GetStatus`, whose table miss
    /// is a 404.
    pub(super) async fn users_get_user_status(
        &self,
        args: api::Z_GetUserStatusArgs,
    ) -> Result<api::Z_GetUserStatusReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_status(&args.a).await, |s| status_to_wire(&s));
        Ok(api::Z_GetUserStatusReturns { a, b })
    }

    /// Port of `PluginAPI.GetUserStatusesByIds` (app/plugin_api.go:385): an id with no status,
    /// or no user, is reported offline.
    pub(super) async fn users_get_user_statuses_by_ids(
        &self,
        args: api::Z_GetUserStatusesByIdsArgs,
    ) -> Result<api::Z_GetUserStatusesByIdsReturns, NotImplemented> {
        let result = self.app.get_user_statuses_by_ids(&args.a).await;
        let (a, b) = self.reply_list(result, status_to_wire);
        Ok(api::Z_GetUserStatusesByIdsReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateUserStatus` (app/plugin_api.go:389): every status is set
    /// **manually**, and `dnd` is the untimed one; then a fresh `GetStatus`. Unlike the REST
    /// route there is no out-of-office branch and no user check — a status for an id that names
    /// nobody is written.
    pub(super) async fn users_update_user_status(
        &self,
        args: api::Z_UpdateUserStatusArgs,
    ) -> Result<api::Z_UpdateUserStatusReturns, NotImplemented> {
        use mm_model::status::{STATUS_AWAY, STATUS_DND, STATUS_OFFLINE, STATUS_ONLINE};
        let user_id = args.a.as_str();
        match args.b.as_str() {
            STATUS_ONLINE => self.app.set_status_online(user_id, true).await,
            STATUS_OFFLINE => self.app.set_status_offline(user_id, true, false).await,
            STATUS_AWAY => self.app.set_status_away_if_needed(user_id, true).await,
            STATUS_DND => self.app.set_status_do_not_disturb(user_id).await,
            _ => {
                return Ok(api::Z_UpdateUserStatusReturns {
                    a: None,
                    b: self.wire(bad_status()),
                });
            }
        }
        let (a, b) = self.reply(self.app.get_status(user_id).await, |s| status_to_wire(&s));
        Ok(api::Z_UpdateUserStatusReturns { a, b })
    }

    /// Port of `PluginAPI.SetUserStatusTimedDND` (app/plugin_api.go:406): the end time is Unix
    /// **seconds**, truncated to the minute, and the previous status is kept for the expiry.
    pub(super) async fn users_set_user_status_timed_dnd(
        &self,
        args: api::Z_SetUserStatusTimedDNDArgs,
    ) -> Result<api::Z_SetUserStatusTimedDNDReturns, NotImplemented> {
        self.app
            .set_status_do_not_disturb_timed(&args.a, args.b)
            .await;
        let (a, b) = self.reply(self.app.get_status(&args.a).await, |s| status_to_wire(&s));
        Ok(api::Z_SetUserStatusTimedDNDReturns { a, b })
    }

    // -- preferences ----------------------------------------------------------------------------

    /// Port of `PluginAPI.GetPreferencesForUser` (app/plugin_api.go:319).
    pub(super) async fn users_get_preferences_for_user(
        &self,
        args: api::Z_GetPreferencesForUserArgs,
    ) -> Result<api::Z_GetPreferencesForUserReturns, NotImplemented> {
        let result = self
            .app
            .get_preferences_for_user(&args.a)
            .await
            .map(|p| p.0);
        let (a, b) = self.reply_list(result, preference_to_wire);
        Ok(api::Z_GetPreferencesForUserReturns { a, b })
    }

    /// Port of `PluginAPI.GetPreferenceForUser` (app/plugin_api.go:310): a miss is the store's
    /// **400**, and the preference returned beside an error is the zero one (a value, not a
    /// pointer, so it crosses as an empty struct).
    pub(super) async fn users_get_preference_for_user(
        &self,
        args: api::Z_GetPreferenceForUserArgs,
    ) -> Result<api::Z_GetPreferenceForUserReturns, NotImplemented> {
        let answer = match self
            .app
            .get_preference_by_category_and_name_for_user(&args.a, &args.b, &args.c)
            .await
        {
            Ok(preference) => api::Z_GetPreferenceForUserReturns {
                a: preference_to_wire(&preference),
                b: None,
            },
            Err(err) => api::Z_GetPreferenceForUserReturns {
                a: Default::default(),
                b: self.wire(err),
            },
        };
        Ok(answer)
    }

    /// Port of `PluginAPI.UpdatePreferencesForUser` (app/plugin_api.go:323): every entry must
    /// belong to the user (403 otherwise, before any write), and `PreferencesHaveChanged` fires
    /// for **every** plugin, this one included, with the API's empty context.
    pub(super) async fn users_update_preferences_for_user(
        &self,
        args: api::Z_UpdatePreferencesForUserArgs,
    ) -> Result<api::Z_UpdatePreferencesForUserReturns, NotImplemented> {
        let preferences = preferences_from_wire(&args.b);
        let result = self
            .app
            .update_preferences(&HookContext::default(), &args.a, &preferences)
            .await;
        Ok(api::Z_UpdatePreferencesForUserReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.DeletePreferencesForUser` (app/plugin_api.go:327): the same ownership
    /// check, and **no** hook — Go's `DeletePreferences` runs none.
    pub(super) async fn users_delete_preferences_for_user(
        &self,
        args: api::Z_DeletePreferencesForUserArgs,
    ) -> Result<api::Z_DeletePreferencesForUserReturns, NotImplemented> {
        let preferences = preferences_from_wire(&args.b);
        let result = self.app.delete_preferences(&args.a, &preferences).await;
        Ok(api::Z_DeletePreferencesForUserReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    // -- teams ----------------------------------------------------------------------------------

    /// Port of `PluginAPI.GetTeams` (app/plugin_api.go:180): `GetAllTeams`, every team,
    /// archived ones included, in display-name order, unsanitised.
    pub(super) async fn users_get_teams(&self) -> Result<api::Z_GetTeamsReturns, NotImplemented> {
        let (a, b) = self.reply_list(self.app.get_all_teams().await, team_to_wire);
        Ok(api::Z_GetTeamsReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamsForUser` (app/plugin_api.go:205): the live teams the user is
    /// a current member of.
    pub(super) async fn users_get_teams_for_user(
        &self,
        args: api::Z_GetTeamsForUserArgs,
    ) -> Result<api::Z_GetTeamsForUserReturns, NotImplemented> {
        let (a, b) = self.reply_list(self.app.get_teams_for_user(&args.a).await, team_to_wire);
        Ok(api::Z_GetTeamsForUserReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamsUnreadForUser` (app/plugin_api.go:197): no team excluded and
    /// collapsed threads off, so the thread counts stay zero.
    pub(super) async fn users_get_teams_unread_for_user(
        &self,
        args: api::Z_GetTeamsUnreadForUserArgs,
    ) -> Result<api::Z_GetTeamsUnreadForUserReturns, NotImplemented> {
        let result = self.app.get_teams_unread_for_user("", &args.a, false).await;
        let (a, b) = self.reply_list(result, team_unread_to_wire);
        Ok(api::Z_GetTeamsUnreadForUserReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamMembers` (app/plugin_api.go:244): the offset is
    /// `page * perPage`, the options nil — current members in user-id order.
    pub(super) async fn users_get_team_members(
        &self,
        args: api::Z_GetTeamMembersArgs,
    ) -> Result<api::Z_GetTeamMembersReturns, NotImplemented> {
        let result = self
            .app
            .get_team_members(
                &args.a,
                args.b.saturating_mul(args.c),
                args.c,
                &TeamMembersGetOptions::default(),
            )
            .await;
        let (a, b) = self.reply_list(result, team_member_to_wire);
        Ok(api::Z_GetTeamMembersReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamMembersForUser` (app/plugin_api.go:252): every membership row,
    /// left teams included, in no promised order.
    pub(super) async fn users_get_team_members_for_user(
        &self,
        args: api::Z_GetTeamMembersForUserArgs,
    ) -> Result<api::Z_GetTeamMembersForUserReturns, NotImplemented> {
        let result = self
            .app
            .get_team_members_for_user_with_pagination(&args.a, args.b, args.c)
            .await;
        let (a, b) = self.reply_list(result, team_member_to_wire);
        Ok(api::Z_GetTeamMembersForUserReturns { a, b })
    }

    /// Port of `PluginAPI.CreateTeamMember` (app/plugin_api.go:224): `AddTeamMember` with no
    /// requestor — the join hooks, the default channels, and one `added_to_team`.
    pub(super) async fn users_create_team_member(
        &self,
        args: api::Z_CreateTeamMemberArgs,
    ) -> Result<api::Z_CreateTeamMemberReturns, NotImplemented> {
        let result = self
            .app
            .add_team_member(&args.a, &args.b, &HookContext::default())
            .await;
        let (a, b) = self.reply(result, |m| team_member_to_wire(&m));
        Ok(api::Z_CreateTeamMemberReturns { a, b })
    }

    /// Port of `PluginAPI.CreateTeamMembers` (app/plugin_api.go:228): `AddTeamMembers`, not
    /// graceful, so the first failure is the answer and the users before it stay added.
    pub(super) async fn users_create_team_members(
        &self,
        args: api::Z_CreateTeamMembersArgs,
    ) -> Result<api::Z_CreateTeamMembersReturns, NotImplemented> {
        let result = self
            .app
            .add_team_members(&args.a, &args.b, &args.c, false, &HookContext::default())
            .await
            .map(|entries| {
                mm_model::team_member::team_members_with_error_to_team_members(&entries)
                    .unwrap_or_default()
            });
        let (a, b) = self.reply_list(result, team_member_to_wire);
        Ok(api::Z_CreateTeamMembersReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteTeamMember` (app/plugin_api.go:240): `RemoveUserFromTeam`, with
    /// none of the REST route's permission or group-constraint refusals.
    pub(super) async fn users_delete_team_member(
        &self,
        args: api::Z_DeleteTeamMemberArgs,
    ) -> Result<api::Z_DeleteTeamMemberReturns, NotImplemented> {
        let result = self
            .app
            .remove_user_from_team(&args.a, &args.b, &args.c, &HookContext::default())
            .await;
        Ok(api::Z_DeleteTeamMemberReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }

    /// Port of `PluginAPI.UpdateTeamMemberRoles` (app/plugin_api.go:256).
    pub(super) async fn users_update_team_member_roles(
        &self,
        args: api::Z_UpdateTeamMemberRolesArgs,
    ) -> Result<api::Z_UpdateTeamMemberRolesReturns, NotImplemented> {
        let result = self
            .app
            .update_team_member_roles(&args.a, &args.b, &args.c)
            .await;
        let (a, b) = self.reply(result, |m| team_member_to_wire(&m));
        Ok(api::Z_UpdateTeamMemberRolesReturns { a, b })
    }

    /// Port of `PluginAPI.GetTeamStats` (app/plugin_api.go:260), with no view restrictions.
    pub(super) async fn users_get_team_stats(
        &self,
        args: api::Z_GetTeamStatsArgs,
    ) -> Result<api::Z_GetTeamStatsReturns, NotImplemented> {
        let (a, b) = self.reply(self.app.get_team_stats(&args.a).await, |s| {
            team_stats_to_wire(&s)
        });
        Ok(api::Z_GetTeamStatsReturns { a, b })
    }

    /// Port of `PluginAPI.SearchTeams` (app/plugin_api.go:188): `SearchAllTeams` with the term
    /// alone — unpaged, so the count Go also computes is dropped.
    pub(super) async fn users_search_teams(
        &self,
        args: api::Z_SearchTeamsArgs,
    ) -> Result<api::Z_SearchTeamsReturns, NotImplemented> {
        let search = TeamSearch {
            term: args.a,
            ..TeamSearch::default()
        };
        let result = self
            .app
            .search_all_teams(&search)
            .await
            .map(|(teams, _)| teams);
        let (a, b) = self.reply_list(result, team_to_wire);
        Ok(api::Z_SearchTeamsReturns { a, b })
    }

    /// Port of `PluginAPI.CreateTeam` (app/plugin_api.go:168): with anonymous URLs under an
    /// Enterprise Advanced licence the name is replaced by a fresh id first; then
    /// `App.CreateTeam`, which makes the default channels and adds **nobody**.
    pub(super) async fn users_create_team(
        &self,
        args: api::Z_CreateTeamArgs,
    ) -> Result<api::Z_CreateTeamReturns, NotImplemented> {
        let mut team: Team = args.a.as_deref().map(team_from_wire).unwrap_or_default();
        if self.app.config().use_anonymous_urls {
            let license = match self.app.license().await {
                Ok(license) => license,
                Err(err) => {
                    tracing::error!(plugin_id = %self.id, error = %err.id, "the licence could not be read");
                    None
                }
            };
            if mm_model::license::minimum_enterprise_advanced_license(license.as_deref()) {
                team.name = mm_model::utils::new_id();
            }
        }
        let (a, b) = self.reply(self.app.create_team(&mut team).await, |t| team_to_wire(&t));
        Ok(api::Z_CreateTeamReturns { a, b })
    }

    /// Port of `PluginAPI.UpdateTeam` (app/plugin_api.go:201): `App.UpdateTeam`, which copies
    /// seven fields (and conditionally the name) onto the stored team and publishes
    /// `update_team`.
    pub(super) async fn users_update_team(
        &self,
        args: api::Z_UpdateTeamArgs,
    ) -> Result<api::Z_UpdateTeamReturns, NotImplemented> {
        let team: Team = args.a.as_deref().map(team_from_wire).unwrap_or_default();
        let (a, b) = self.reply(self.app.update_team(&team).await, |t| team_to_wire(&t));
        Ok(api::Z_UpdateTeamReturns { a, b })
    }

    /// Port of `PluginAPI.DeleteTeam` (app/plugin_api.go:176): `SoftDeleteTeam` — archived, not
    /// removed.
    pub(super) async fn users_delete_team(
        &self,
        args: api::Z_DeleteTeamArgs,
    ) -> Result<api::Z_DeleteTeamReturns, NotImplemented> {
        let result = self.app.soft_delete_team(&args.a).await;
        Ok(api::Z_DeleteTeamReturns {
            a: result.err().and_then(|e| self.wire(e)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_plugin::wire::model::{UserGetOptions, UserSearch, ViewUsersRestrictions};

    /// Only the four filters the store query lacks refuse; the fields `GetAllProfiles` ignores
    /// do not, and neither does a sort other than `update_at_asc`.
    #[test]
    fn get_users_refuses_only_what_get_all_profiles_would_filter_on() {
        let plain = UserGetOptions {
            in_team_id: "t".into(),
            in_channel_id: "c".into(),
            not_in_team_id: "n".into(),
            without_team: true,
            group_constrained: true,
            team_roles: vec!["team_admin".into()],
            channel_roles: vec!["channel_admin".into()],
            sort: "last_activity_at".into(),
            inactive: true,
            page: 3,
            per_page: 7,
            ..UserGetOptions::default()
        };
        assert_eq!(get_users_unanswerable(&plain), None);
        let refused = [
            UserGetOptions {
                role: "system_admin".into(),
                ..UserGetOptions::default()
            },
            UserGetOptions {
                roles: vec!["system_user".into()],
                ..UserGetOptions::default()
            },
            UserGetOptions {
                sort: "update_at_asc".into(),
                ..UserGetOptions::default()
            },
            UserGetOptions {
                updated_after: 1,
                ..UserGetOptions::default()
            },
            UserGetOptions {
                view_restrictions: Some(Box::new(ViewUsersRestrictions::default())),
                ..UserGetOptions::default()
            },
        ];
        for options in refused {
            assert!(get_users_unanswerable(&options).is_some(), "{options:?}");
        }
        let never = UserGetOptions {
            updated_after: -5,
            ..UserGetOptions::default()
        };
        assert_eq!(
            get_users_unanswerable(&never),
            None,
            "`UpdatedAfter > 0` — zero and below add no predicate"
        );
    }

    /// Go's order: `WithoutTeam` first, then the channel, the not-in-channel, the not-in-team,
    /// the two group arms, and the team last.
    #[test]
    fn a_search_takes_the_first_arm_go_checks() {
        let everything = UserSearch {
            without_team: true,
            in_channel_id: "c".into(),
            not_in_channel_id: "nc".into(),
            not_in_team_id: "nt".into(),
            in_group_id: "g".into(),
            not_in_group_id: "ng".into(),
            team_id: "t".into(),
            ..UserSearch::default()
        };
        assert_eq!(search_arm(&everything), SearchArm::Unported("WithoutTeam"));
        let channel = UserSearch {
            without_team: false,
            ..everything.clone()
        };
        assert_eq!(search_arm(&channel), SearchArm::InChannel("c"));
        let not_in_channel = UserSearch {
            in_channel_id: String::new(),
            ..channel
        };
        assert_eq!(
            search_arm(&not_in_channel),
            SearchArm::Unported("NotInChannelId")
        );
        let not_in_team = UserSearch {
            not_in_channel_id: String::new(),
            ..not_in_channel
        };
        assert_eq!(search_arm(&not_in_team), SearchArm::Unported("NotInTeamId"));
        let in_group = UserSearch {
            not_in_team_id: String::new(),
            ..not_in_team
        };
        assert_eq!(search_arm(&in_group), SearchArm::Unported("InGroupId"));
        let not_in_group = UserSearch {
            in_group_id: String::new(),
            ..in_group
        };
        assert_eq!(
            search_arm(&not_in_group),
            SearchArm::Unported("NotInGroupId")
        );
        let team = UserSearch {
            not_in_group_id: String::new(),
            role: "system_admin".into(),
            group_constrained: true,
            ..not_in_group
        };
        assert_eq!(
            search_arm(&team),
            SearchArm::InTeam("t"),
            "the role and group-constraint fields pick no arm"
        );
        assert_eq!(search_arm(&UserSearch::default()), SearchArm::InTeam(""));
    }

    #[test]
    fn the_two_refusals_are_gos() {
        let status = bad_status();
        assert_eq!(
            (
                status.where_.as_str(),
                status.id.as_str(),
                status.detailed_error.as_str(),
                status.status_code
            ),
            (
                "UpdateUserStatus",
                "plugin.api.update_user_status.bad_status",
                "unrecognized status",
                400
            )
        );
        let sort = bad_channel_sort();
        assert_eq!(
            (
                sort.where_.as_str(),
                sort.id.as_str(),
                sort.detailed_error.as_str(),
                sort.status_code
            ),
            (
                "GetUsersInChannel",
                "plugin.api.get_users_in_channel",
                "invalid sort option",
                400
            )
        );
    }
}
