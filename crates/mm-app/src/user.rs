//! Port of `app.GetUser`, `app.GetUserByUsername`, `app.GetUsersByIds`, `app.GetKnownUsers`,
//! `app.GetTotalUsersStats` and `app.GetViewUsersRestrictions` (channels/app/user.go).

use mm_model::permission::PERMISSION_VIEW_MEMBERS;
use mm_model::stats::UsersStats;
use mm_model::user::User;
use mm_model::user_autocomplete::{UserAutocompleteInChannel, UserAutocompleteInTeam};
use mm_model::utils::{AppError, AppResult};
use mm_store::user_store::UserSearchOptions;
use mm_store::{StoreError, UserStore};

use crate::App;

/// What `GetViewUsersRestrictions` (app/user.go:2756) decided, without the lists.
///
/// Go returns `nil` for a caller holding `view_members` and otherwise a
/// `*model.ViewUsersRestrictions` naming the teams and channels the caller may see members
/// through. This port computes the **decision** and not the lists, because every query that
/// consumes them is unported — see [`App::get_view_users_restrictions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewUsersRestriction {
    /// Go's `nil`: no filter is applied to any user query.
    None,
    /// Go builds a list here. `mm_api::users` forwards the request instead.
    Restricted,
}

impl App {
    /// Port of `app.App.GetViewUsersRestrictions` (app/user.go:2756), narrowed to its verdict.
    ///
    /// # The restricted branch is unreachable on a stock server, and that is a fact about roles
    ///
    /// `system_user` — the role every account carries — grants `view_members` outright
    /// (model/role.go:1179), so `HasPermissionTo` answers `true` and Go returns `nil` before it
    /// touches a store. The only built-in role *without* it is `system_guest`, and guest accounts
    /// are licensed. A deployment that edits `system_user` through the roles API can reach the
    /// other branch; nothing else can.
    ///
    /// # So the lists are not built here
    ///
    /// Go would go on to read the caller's team ids, re-check `view_members` per team, and read
    /// every channel membership — to produce two lists whose only consumers are
    /// `applyViewRestrictionsFilter` and the profile queries, none of which this port has. Naming
    /// the verdict and stopping is the honest shape: it ports the gate, ships no SQL that no test
    /// can reach, and lets `mm_api::users` forward the case Go answers differently.
    ///
    /// Note the permission is checked against the **user's own stored roles**
    /// (`HasPermissionTo`), not the session's — so a session minted with narrower roles than the
    /// account holds still sees the unrestricted answer.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn get_view_users_restrictions(&self, user_id: &str) -> ViewUsersRestriction {
        if self
            .has_permission_to(user_id, &PERMISSION_VIEW_MEMBERS)
            .await
        {
            ViewUsersRestriction::None
        } else {
            ViewUsersRestriction::Restricted
        }
    }

    /// Port of `app.App.GetTotalUsersStats` (app/user.go:2369) for nil view restrictions.
    ///
    /// One store call wrapped in one error id. The `UsersStats` around the number exists because
    /// it is a response body — `{"total_users_count":N}` — not because Go needed a type.
    ///
    /// **Bots are counted.** `IncludeBotAccounts: true` is a literal in Go's options struct, so
    /// this number is larger than any member list on a server with plugins installed. See
    /// [`mm_store::UserStore::count_total_users`].
    #[tracing::instrument(skip(self))]
    pub async fn get_total_users_stats(&self) -> AppResult<UsersStats> {
        let total_users_count = self
            .store()
            .user()
            .count_total_users()
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "user count failed");
                AppError::boxed(
                    "GetTotalUsersStats",
                    "app.user.get_total_users_count.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        Ok(UsersStats { total_users_count })
    }

    /// Port of `app.App.GetKnownUsers` (app/user.go:2932).
    ///
    /// One store call, one error id, and **no permission check anywhere** — the handler has none
    /// either. It is safe because the answer is derived from the caller's own memberships: you
    /// learn only about people you already share a channel with.
    ///
    /// The empty answer is `[]`, not `null`: Go's store initialises `userIds := []string{}`
    /// before the scan, unlike the nil-returning reads in `getReactions` and
    /// `getFileInfosForPost`.
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    pub async fn get_known_users(&self, user_id: &str) -> AppResult<Vec<String>> {
        self.store()
            .user()
            .get_known_users(user_id)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "known users lookup failed");
                AppError::boxed(
                    "GetKnownUsers",
                    "app.user.get_known_users.get_users.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetUser`.
    ///
    /// Go returns `MissingAccountError` — id **`app.user.missing_account.const`**, 404 — for a
    /// miss, and a 500 for anything else. The two are not interchangeable at the API edge.
    ///
    /// Yes, `.const`: the id's last word is the Go keyword, not `error` (app/constants.go:7 —
    /// presumably a long-fossilised typo for a file of constants). This port shipped with
    /// `.error` for three days because `/users/me` can never miss — the session's user always
    /// exists — so no test could reach the branch until `GET /users/{user_id}` landed and its
    /// parity suite compared the 404 against the running server.
    #[tracing::instrument(skip_all, fields(user_id = %id))]
    pub async fn get_user(&self, id: &str) -> AppResult<User> {
        self.store().user().get(id).await.map_err(get_user_error)
    }

    /// Port of `app.App.GetUsersByGroupChannelIds` (app/user.go:909).
    ///
    /// One store call, one error id, and `sanitizeProfiles` over each channel's list. The
    /// sanitisation is the handler's job here — this returns the raw map — because the options
    /// depend on config the api layer already holds; see `mm_api::users`.
    ///
    /// **There is no permission check anywhere above the store.** The access rule lives inside
    /// the query, as an `EXISTS` asserting the caller is a member of each channel it answers
    /// for — see [`mm_store::user_store`]. A port that "tidied" that subquery out of the SQL and
    /// into a forgotten app-layer gate would list every group channel's members to anyone.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, asked = channel_ids.len(), found))]
    pub async fn get_users_by_group_channel_ids(
        &self,
        user_id: &str,
        channel_ids: &[String],
    ) -> AppResult<std::collections::BTreeMap<String, Vec<User>>> {
        let by_channel = self
            .store()
            .user()
            .get_profile_by_group_channel_ids_for_user(user_id, channel_ids)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "group-channel profile lookup failed");
                AppError::boxed(
                    "GetUsersByGroupChannelIds",
                    "app.user.get_profile_by_group_channel_ids_for_user.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        tracing::Span::current().record("found", by_channel.len());
        Ok(by_channel)
    }
}

impl App {
    /// Port of `app.App.GetFilteredUsersStats` (user.go:2384).
    ///
    /// One store call, one wrapper struct, one error id —
    /// `app.user.get_total_users_count.app_error`, **shared with the unfiltered
    /// `/users/stats`**, so a client cannot tell which count failed from the id alone.
    #[tracing::instrument(skip_all, fields(team_id = %options.team_id, channel_id = %options.channel_id))]
    pub async fn get_filtered_users_stats(
        &self,
        options: &mm_model::user_count::UserCountOptions,
    ) -> AppResult<mm_model::stats::UsersStats> {
        let total_users_count = self.store().user().count(options).await.map_err(|err| {
            tracing::error!(error = %err, "filtered user count failed");
            AppError::boxed(
                "GetFilteredUsersStats",
                "app.user.get_total_users_count.app_error",
                None,
                String::new(),
                500,
            )
        })?;

        Ok(mm_model::stats::UsersStats { total_users_count })
    }

    /// Port of `app.App.GetUsersByUsernames` (user.go:921), **minus the sanitizer**.
    ///
    /// Go's `sanitizeProfiles(users, asAdmin)` reads the privacy settings from config, which in
    /// this deployment are `AppState`'s stand-ins ([D-085]), so the api layer applies
    /// `SanitizeProfile` per user with the map `getUser` builds. Every caller sanitises.
    ///
    /// One error branch and one id — `app.user.get_profiles.app_error`, 500 — shared with
    /// [`Self::get_users_by_ids`]. There is **no not-found**: a username that names nobody is
    /// simply absent from the array, so a request for five names can legitimately answer with
    /// two.
    #[tracing::instrument(skip_all, fields(count = usernames.len()))]
    pub async fn get_users_by_usernames(&self, usernames: &[String]) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profiles_by_usernames(usernames)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "profiles-by-username lookup failed");
                AppError::boxed(
                    "GetUsersByUsernames",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }

    /// Port of `app.App.GetUserByEmail` (user.go:581).
    ///
    /// **The neighbour's shape, not the one three lines above it.** `GetUserByUsername` invents
    /// its own id; this one uses `MissingAccountError` — `app.user.missing_account.const` — the
    /// same id `GetUser` gives an unknown *id*, so a client cannot tell "no such email" from "no
    /// such user id" by the error alone. Both branches share it and only the status differs.
    #[tracing::instrument(skip_all, fields(email = %email))]
    pub async fn get_user_by_email(&self, email: &str) -> AppResult<User> {
        self.store()
            .user()
            .get_by_email(email)
            .await
            .map_err(|err| {
                let status = if matches!(err, StoreError::NotFound { .. }) {
                    404
                } else {
                    tracing::error!(error = %err, "user-by-email lookup failed");
                    500
                };
                AppError::boxed(
                    "GetUserByEmail",
                    "app.user.missing_account.const",
                    None,
                    String::new(),
                    status,
                )
            })
    }

    /// Port of `app.App.GetUserByUsername` (user.go:567).
    ///
    /// **Both branches carry the same id** — `app.user.get_by_username.app_error` — and only the
    /// status separates a miss from a broken query, the `GetChannelUnread` shape rather than
    /// `GetUser`'s two-id shape three lines up in the same Go file. Neither branch matches
    /// `MissingAccountError` either; a client cannot correlate "no such id" with "no such
    /// username" by error id, and that is Go's wire.
    #[tracing::instrument(skip_all, fields(username = %username))]
    pub async fn get_user_by_username(&self, username: &str) -> AppResult<User> {
        self.store()
            .user()
            .get_by_username(username)
            .await
            .map_err(|err| {
                let status = if matches!(err, StoreError::NotFound { .. }) {
                    404
                } else {
                    tracing::error!(error = %err, "user-by-username lookup failed");
                    500
                };
                AppError::boxed(
                    "GetUserByUsername",
                    "app.user.get_by_username.app_error",
                    None,
                    String::new(),
                    status,
                )
            })
    }
}

impl App {
    /// Port of `app.App.GetUsersByIds` (user.go:900) → `UserService.GetUsersByIds`
    /// (app/users/users.go:146), **minus the sanitizer**: Go's `sanitizeProfiles(users,
    /// options.IsAdmin)` reads the privacy settings from config, which in this deployment are
    /// `AppState`'s stand-ins (D-085), so the caller applies `SanitizeProfile` per user with the
    /// same map `getUser` builds. Every caller sanitises — there is no raw consumer.
    ///
    /// `ViewRestrictions` is not a parameter: the api layer forwards any caller whose
    /// restrictions would be non-nil, so this is always the `allowFromCache` path minus the
    /// cache. One error branch, one id — `app.user.get_profiles.app_error`, 500 — for any store
    /// failure; there is no not-found, an unknown id is simply absent from the list.
    #[tracing::instrument(skip_all, fields(count = ids.len(), since))]
    pub async fn get_users_by_ids(&self, ids: &[String], since: i64) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profile_by_ids(ids, since)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "users-by-ids lookup failed");
                AppError::boxed(
                    "GetUsersByIds",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })
    }
}

/// The paging half of `getUsers`, carried together because every branch reads all four values
/// and Go carries them in one `model.UserGetOptions` (user.go:990).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserPage {
    pub page: i64,
    pub per_page: i64,
    /// `?inactive=` after `strconv.ParseBool`.
    pub inactive: bool,
    /// `?active=` after `strconv.ParseBool`.
    pub active: bool,
}

impl UserPage {
    /// `page * perPage`, for the two branches where **Go's app layer** does the multiply
    /// (`GetUsersNotInChannelPage`, `GetUsersNotInTeamPage`) rather than the store.
    fn offset(self) -> i64 {
        self.page.saturating_mul(self.per_page)
    }

    fn deleted(self) -> Option<bool> {
        mm_store::user_store::deleted_filter(self.inactive, self.active)
    }
}

/// The one error every profile-listing branch of `getUsers` produces: a 500 carrying
/// `app.user.get_profiles.app_error`. Only `where` separates them, and `where` is `json:"-"`,
/// so on the wire all five are the same response — the parameter exists to keep the *logs*
/// honest, not the clients.
fn get_profiles_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, where_, "user profile listing failed");
    AppError::boxed(
        where_,
        "app.user.get_profiles.app_error",
        None,
        String::new(),
        500,
    )
}

impl App {
    /// Port of `app.App.GetUsersPage` (user.go:654) — the unfiltered `GET /users`.
    ///
    /// Go's chain is `GetUsersPage` → `GetUsersFromProfiles` → `store.GetAllProfiles`, and the
    /// `AppError` is minted at the top, so `where` is `GetUsersPage`. `RestrictUsersGetByPermissions`
    /// is not represented: it only sets `ViewRestrictions`, and the api layer forwards every
    /// caller whose restrictions would be non-nil.
    ///
    /// Sanitisation is the caller's, as with [`App::get_users_by_ids`] — the privacy settings
    /// live in `AppState` (D-085). Go sanitises twice here, once in the store
    /// (`u.Sanitize(map[string]bool{})`) and once in `sanitizeProfiles`; the first is wholly
    /// subsumed by the second, which clears the same four fields plus more, so it is not ported.
    #[tracing::instrument(skip_all, fields(page = page.page, per_page = page.per_page))]
    pub async fn get_users_page(&self, page: UserPage) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_all_profiles(page.page, page.per_page, page.deleted())
            .await
            .map_err(|err| get_profiles_error("GetUsersPage", err))
    }

    /// Port of `app.App.GetUsersInTeamPage` (user.go:685) → `store.GetProfiles`.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, page = page.page))]
    pub async fn get_users_in_team_page(
        &self,
        team_id: &str,
        page: UserPage,
    ) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profiles_in_team(team_id, page.page, page.per_page, page.deleted())
            .await
            .map_err(|err| get_profiles_error("GetUsersInTeamPage", err))
    }

    /// Port of `app.App.GetUsersInChannelPage` (user.go:754).
    ///
    /// The error is minted one level down in `GetUsersInChannel`, so `where` is **not** the
    /// `…Page` name its siblings use — a difference visible only in Go's logs, kept because
    /// guessing at it is the habit this project exists to break.
    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, page = page.page))]
    pub async fn get_users_in_channel_page(
        &self,
        channel_id: &str,
        page: UserPage,
    ) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profiles_in_channel(channel_id, page.page, page.per_page, page.deleted())
            .await
            .map_err(|err| get_profiles_error("GetUsersInChannel", err))
    }

    /// Port of `app.App.GetUsersNotInChannelPage` (user.go:803) for `groupConstrained = false`.
    ///
    /// **The multiply happens here**, not in the store (`GetUsersNotInChannel(…, page*perPage,
    /// perPage, …)`), and the options struct is left behind entirely — which is why the
    /// active/inactive flags have no effect on this branch.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, channel_id = %channel_id))]
    pub async fn get_users_not_in_channel_page(
        &self,
        team_id: &str,
        channel_id: &str,
        page: UserPage,
    ) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profiles_not_in_channel(team_id, channel_id, page.offset(), page.per_page)
            .await
            .map_err(|err| get_profiles_error("GetUsersNotInChannel", err))
    }

    /// Port of `app.App.GetUsersNotInTeamPage` (user.go:694) for `groupConstrained = false`.
    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    pub async fn get_users_not_in_team_page(
        &self,
        team_id: &str,
        page: UserPage,
    ) -> AppResult<Vec<User>> {
        self.store()
            .user()
            .get_profiles_not_in_team(team_id, page.offset(), page.per_page)
            .await
            .map_err(|err| get_profiles_error("GetUsersNotInTeamPage", err))
    }

    /// Port of `UserService.GetUsersInTeamEtag` (app/users/users.go:183).
    ///
    /// `fmt.Sprintf("%v.%v.%v.%v", storeEtag, ShowFullName, ShowEmailAddress, restrictionsHash)`.
    /// The restrictions hash is **always the empty string** here —
    /// `(*ViewUsersRestrictions).Hash()` returns `""` for nil (model/user.go:281) and the api
    /// layer forwards every caller whose restrictions are not nil — so every etag this server
    /// mints ends in a dot.
    ///
    /// # This etag cannot match Go's, and it is Go that is wrong
    ///
    /// `PrivacySettings.ShowFullName` and `ShowEmailAddress` are `*bool`, and all three
    /// `UserService` etag builders (users.go:143, 184, 188) interpolate them **without
    /// dereferencing**. `%v` on a pointer prints its address, so Go's answer is literally
    /// `11.11.0.1787307018591.0x32494e83e753.0x32494e83e752.` — two heap addresses, measured on
    /// the running server. Every other call site in api4 writes `*c.App.Config()...` and gets a
    /// bool; these three do not.
    ///
    /// So an etag minted here can never equal one minted over there: no process can reproduce
    /// another's addresses, and Go's own change whenever the config is reloaded. This port emits
    /// the value (`true`/`false`), which is what the format string was reaching for. The
    /// consequence is confined: behind the strangler proxy a client only ever sees the etag of
    /// whichever server answered, an unrecognised `If-None-Match` is a 200 rather than an error,
    /// and both servers 304 correctly on their own. Pinned by
    /// `parity_users_list::the_etag_arms_match_go_except_for_gos_two_pointer_components`,
    /// which fails if upstream ever adds the `*`.
    pub async fn get_users_in_team_etag(
        &self,
        team_id: &str,
        show_full_name: bool,
        show_email_address: bool,
    ) -> String {
        let store_etag = self.store().user().get_etag_for_profiles(team_id).await;
        format!("{store_etag}.{show_full_name}.{show_email_address}.")
    }

    /// Port of `UserService.GetUsersNotInTeamEtag` (app/users/users.go:187).
    ///
    /// # The team id the handler passes here is `in_team`, not `not_in_team`
    ///
    /// `api4/user.go:1049` reads `c.App.GetUsersNotInTeamEtag(inTeamId, restrictions.Hash())`
    /// inside the `notInTeamId != ""` branch. `in_team` is almost always empty there, so the
    /// etag is computed over *every user with no team membership at all* while the body lists
    /// the users outside `not_in_team`. Passing the obviously-intended `notInTeamId` would make
    /// this server 304 where Go returns 200 and vice versa. Reproduced, not corrected — the
    /// caller decides what to pass and this port's handler passes what Go passes.
    pub async fn get_users_not_in_team_etag(
        &self,
        team_id: &str,
        show_full_name: bool,
        show_email_address: bool,
    ) -> String {
        let store_etag = self
            .store()
            .user()
            .get_etag_for_profiles_not_in_team(team_id)
            .await;
        format!("{store_etag}.{show_full_name}.{show_email_address}.")
    }
}

/// The one error every user *search* produces: a 500 carrying `app.user.search.app_error`.
///
/// Five app functions mint it and only `where` separates them, which — like
/// [`get_profiles_error`] — is `json:"-"` and therefore invisible to a client. It is reachable
/// from a real client: `?limit=-1` casts to a `uint64` on Go's side and to a negative `LIMIT` on
/// ours, and Postgres refuses both. Measured, not inferred.
fn search_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = %err, where_, "user search failed");
    AppError::boxed(
        where_,
        "app.user.search.app_error",
        None,
        String::new(),
        500,
    )
}

impl App {
    /// Port of `app.App.SearchUsersInTeam` (app/user.go:2477).
    ///
    /// `autocompleteUsers`' third arm — no `in_channel` and no `in_team` — calls this with an
    /// **empty team id**, which the store reads as "no team filter": the search covers every
    /// active user in the installation, gated by nothing but the caller's view restrictions.
    ///
    /// Sanitisation is the caller's here, unlike Go, where this function runs `SanitizeProfile`
    /// itself. The privacy settings it needs live in `AppState` until config is ported (D-085),
    /// which is the same split [`App::get_users_page`] makes.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    pub async fn search_users_in_team(
        &self,
        team_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> AppResult<Vec<User>> {
        let users = self
            .store()
            .user()
            .search(team_id, term.trim(), options)
            .await
            .map_err(|err| search_error("SearchUsersInTeam", err))?;
        tracing::Span::current().record("found", users.len());
        Ok(users)
    }

    /// Port of `app.App.AutocompleteUsersInTeam` (app/user.go:2567).
    ///
    /// The same store call as [`App::search_users_in_team`] wrapped in
    /// `UserAutocompleteInTeam` — and a **different `where`** on the error, which is the only
    /// thing that distinguishes the two on this route.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    pub async fn autocomplete_users_in_team(
        &self,
        team_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> AppResult<UserAutocompleteInTeam> {
        let users = self
            .store()
            .user()
            .search(team_id, term.trim(), options)
            .await
            .map_err(|err| search_error("AutocompleteUsersInTeam", err))?;
        tracing::Span::current().record("found", users.len());

        // `autocomplete.InTeam = users`, where `users` is the store's `[]*model.User{}` — never
        // nil, so the field is `[]` and not `null` even when nothing matched. `Some(vec![])` is
        // what carries that distinction; `None` would emit `"in_team":null`.
        Ok(UserAutocompleteInTeam {
            in_team: Some(users),
        })
    }

    /// Port of `app.App.AutocompleteUsersInChannel` (app/user.go:2548) → the store's own
    /// `AutocompleteUsersInChannel` (user_store.go:2332).
    ///
    /// # Go runs the two halves concurrently and this does not
    ///
    /// The store's `AutocompleteUsersInChannel` puts `SearchInChannel` and `SearchNotInChannel`
    /// in an `errgroup`; these run in sequence, because this crate deliberately carries no
    /// runtime dependency (`tokio` is a dev-dependency only) and a join combinator would add
    /// one. Nothing observable changes: the two queries are independent, and when both fail the
    /// `AppError` is the same id and status either way, so no client can tell which arrived
    /// first. What changes is latency — two round trips instead of one, on a route the webapp
    /// calls per keystroke.
    ///
    /// # The team id feeds only the *out of channel* half
    ///
    /// `SearchInChannel` never sees it. That is the comment at api4/user.go:1434 in code form:
    /// the channel decides who is in, the team decides who is available to invite.
    #[tracing::instrument(skip_all, fields(team_id = %team_id, channel_id = %channel_id))]
    pub async fn autocomplete_users_in_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> AppResult<UserAutocompleteInChannel> {
        let term = term.trim();
        let store = self.store().user();

        let in_channel = store
            .search_in_channel(channel_id, term, options)
            .await
            .map_err(|err| search_error("AutocompleteUsersInChannel", err))?;
        let out_of_channel = store
            .search_not_in_channel(team_id, channel_id, term, options)
            .await
            .map_err(|err| search_error("AutocompleteUsersInChannel", err))?;

        Ok(UserAutocompleteInChannel {
            in_channel: Some(in_channel),
            out_of_channel: Some(out_of_channel),
        })
    }
}

/// The store-error-to-`AppError` mapping for `GetUser`, split out so it is reachable from a test
/// without a database. A miss and a broken query are different HTTP statuses, and collapsing them
/// would report a server fault to the client as a missing account.
impl App {
    /// Port of `App.SanitizeProfile` (app/user.go:1375) over
    /// `UserService.GetSanitizeOptions` (app/users/utils.go:48).
    ///
    /// The base options are `PrivacySettings.ShowFullName` and `ShowEmailAddress`; `as_admin`
    /// forces those two on and adds `authservice` and `authdata`. Note the asymmetry with
    /// `clear_non_profile_fields`, which `as_admin` makes *less* destructive — an admin copy keeps
    /// `notify_props`, `auth_data` and `failed_attempts` that a member copy does not.
    pub fn sanitize_profile(&self, user: &mut mm_model::user::User, as_admin: bool) {
        let config = self.config();
        let mut options = std::collections::HashMap::new();
        options.insert("fullname".to_owned(), config.show_full_name);
        options.insert("email".to_owned(), config.show_email_address);
        if as_admin {
            options.insert("fullname".to_owned(), true);
            options.insert("email".to_owned(), true);
            options.insert("authservice".to_owned(), true);
            options.insert("authdata".to_owned(), true);
        }
        user.sanitize_profile(&options, as_admin);
    }

    /// Port of `App.sendUpdatedUserEvent` (app/user.go:1508).
    ///
    /// **Three events, all `user_updated`, all carrying a differently sanitised copy of the same
    /// user**, and the hub decides which connection gets which:
    ///
    /// 1. the admin copy, `ContainsSensitiveData` — delivered *only* to connections with
    ///    `manage_system`;
    /// 2. the member copy, `ContainsSanitizedData` — delivered to everyone *without* it;
    /// 3. the subject's own copy, addressed to their user id, with `Sanitize(nil)` rather than
    ///    `SanitizeProfile` — so it keeps the profile fields the other two strip.
    ///
    /// All three omit the subject from the broadcast, which is why the third exists at all: the
    /// user who made the change would otherwise learn nothing.
    pub(crate) async fn send_updated_user_event(&self, user: &mm_model::user::User) {
        let omit: std::collections::BTreeMap<String, bool> =
            std::iter::once((user.id.clone(), true)).collect();

        let mut admin_copy = user.clone();
        self.sanitize_profile(&mut admin_copy, true);
        let mut admin_message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_USER_UPDATED,
            "",
            "",
            "",
            Some(omit.clone()),
            "",
        );
        admin_message.add(
            "user",
            serde_json::to_value(&admin_copy).unwrap_or(serde_json::Value::Null),
        );
        let admin_message = {
            let mut broadcast = admin_message.get_broadcast().cloned().unwrap_or_default();
            broadcast.contains_sensitive_data = true;
            admin_message.set_broadcast(broadcast)
        };
        self.publish(admin_message).await;

        let mut member_copy = user.clone();
        self.sanitize_profile(&mut member_copy, false);
        let mut message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_USER_UPDATED,
            "",
            "",
            "",
            Some(omit),
            "",
        );
        message.add(
            "user",
            serde_json::to_value(&member_copy).unwrap_or(serde_json::Value::Null),
        );
        let message = {
            let mut broadcast = message.get_broadcast().cloned().unwrap_or_default();
            broadcast.contains_sanitized_data = true;
            message.set_broadcast(broadcast)
        };
        self.publish(message).await;

        // `Sanitize(nil)` — the *credential* scrub, not the profile one. A nil options map means
        // every `options[...]` lookup is false, so the email and full name survive; only the
        // password, MFA secret and auth data go.
        let mut own_copy = user.clone();
        own_copy.sanitize(&std::collections::HashMap::new());
        let mut own_message = mm_model::websocket_message::WebSocketEvent::new(
            mm_model::websocket_message::WEBSOCKET_EVENT_USER_UPDATED,
            "",
            "",
            &own_copy.id,
            None,
            "",
        );
        own_message.add(
            "user",
            serde_json::to_value(&own_copy).unwrap_or(serde_json::Value::Null),
        );
        self.publish(own_message).await;
    }

    /// Port of `App.isUniqueToGroupNames` (app/user.go:1539).
    ///
    /// A username may not collide with a **group** name. The empty string is exempt, and the
    /// query has no `DeleteAt` predicate — a soft-deleted group keeps its name reserved.
    async fn is_unique_to_group_names(&self, value: &str) -> AppResult<()> {
        if value.is_empty() {
            return Ok(());
        }
        let exists = self
            .store()
            .user()
            .group_name_exists(value)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "group name lookup failed");
                AppError::boxed(
                    "isUniqueToGroupNames",
                    "app.user.save.groupname.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;
        if exists {
            // Go's id is the *username* one, not a group one — a client that picks a taken group
            // name is told the username exists, which is the message it can act on.
            return Err(AppError::boxed(
                "isUniqueToGroupNames",
                "app.user.save.username_exists.app_error",
                None,
                format!("group name {value} exists"),
                400,
            ));
        }
        Ok(())
    }

    /// Port of `App.UpdateUser` (app/user.go:1555).
    ///
    /// # `CreateAt` is restored twice
    ///
    /// Once here and once in the store. Go does both; keeping the redundancy means a future
    /// caller that bypasses one still cannot move a user's creation time.
    ///
    /// # The email branch is three checks and a swap
    ///
    /// A changed email must pass the member domain list *unless* the stored user is a guest, an
    /// LDAP user or a SAML user; and the guest domain list if the stored user **is** a guest and
    /// not LDAP/SAML. The two lists are different config fields and the two errors are different
    /// ids. Then, with `RequireEmailVerification` on, the submitted address is stashed and the
    /// **stored** one is written back — unless the account is a bot, whose `prev.Email` is a
    /// generated fake that a CLI conversion must be able to replace.
    ///
    /// # Not reproduced
    ///
    /// The three background sends (`SendEmailVerification`, `SendEmailChangeEmail`,
    /// `SendChangeUsernameEmail`) — no mail service is ported, and every one of them is
    /// `a.Srv().Go(...)`, so none can affect the response. `UpdateDefaultProfileImage` on a
    /// username change with no custom picture — needs the image pipeline; the consequence is a
    /// stale initials avatar, recorded rather than guessed at. `InvalidateCacheForUser`,
    /// `onUserProfileChange` and the auto-translation locale cache are all in-process caches this
    /// server does not have.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, notify = send_notifications))]
    pub async fn update_user(
        &self,
        user: &mm_model::user::User,
        send_notifications: bool,
    ) -> AppResult<mm_model::user::User> {
        let mut user = user.clone();
        let prev = self.get_user(&user.id).await?;

        if prev.create_at != user.create_at {
            user.create_at = prev.create_at;
        }

        if user.username != prev.username {
            self.is_unique_to_group_names(&user.username).await?;
        }

        let mut new_email = String::new();
        if user.email != prev.email {
            if !check_email_domain(&user.email, &self.config().restrict_creation_to_domains)
                && !prev.is_guest()
                && !prev.is_ldap_user()
                && !prev.is_saml_user()
            {
                return Err(AppError::boxed(
                    "UpdateUser",
                    "api.user.update_user.accepted_domain.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }

            if !check_email_domain(
                &user.email,
                &self.config().guest_restrict_creation_to_domains,
            ) && prev.is_guest()
                && !prev.is_ldap_user()
                && !prev.is_saml_user()
            {
                return Err(AppError::boxed(
                    "UpdateUser",
                    "api.user.update_user.accepted_guest_domain.app_error",
                    None,
                    String::new(),
                    400,
                ));
            }

            if self.config().require_email_verification {
                new_email = user.email.clone();
                // "Don't set new eMail on user account if email verification is required, this
                // will be done as a post-verification action to avoid users being able to set
                // non-controlled eMails as their account email"
                if self.get_user_by_email(&new_email).await.is_ok() {
                    return Err(AppError::boxed(
                        "UpdateUser",
                        "app.user.save.email_exists.app_error",
                        None,
                        format!("user_id={}", user.id),
                        400,
                    ));
                }
                if !user.is_bot {
                    user.email = prev.email.clone();
                }
            }
        }

        let update = self
            .store()
            .user()
            .update(&user, false)
            .await
            .map_err(|err| update_user_error(err, &user.id))?;

        let new_user = update.new;

        if send_notifications {
            // The three mails are not ported; the event is. `newEmail != ""` is Go's own signal
            // that the address changed even though the row did not.
            let _ = (&new_email, &update.old.email);
            self.send_updated_user_event(&new_user).await;
        }

        // `newUser.Sanitize(map[string]bool{})` — an empty map, not nil: every option lookup is
        // false, so this is the credential scrub only. The store already did it; Go does it
        // again and so does this.
        let mut new_user = new_user;
        new_user.sanitize(&std::collections::HashMap::new());
        Ok(new_user)
    }
}

/// Port of `users.CheckEmailDomain` (app/users/utils.go:18).
///
/// An **empty** domain list admits everything, which is the default and so the live path on a
/// stock server. The match is a suffix test against `"@" + domain`, so `example.com` admits
/// `a@example.com` and also `a@evil-example.com` — Go's own looseness, reproduced.
fn check_email_domain(email: &str, domains: &str) -> bool {
    if domains.is_empty() {
        return true;
    }
    let email = mm_model::utils::go_to_lower(email);
    crate::team::normalize_domains(domains)
        .iter()
        .any(|domain| email.ends_with(&format!("@{domain}")))
}

/// The five error shapes `App.UpdateUser` gives the store's failures (app/user.go:1610).
fn update_user_error(err: StoreError, user_id: &str) -> Box<AppError> {
    match err {
        // Go's `errors.As(err, &appErr)` — a validation failure keeps its own id and status.
        StoreError::Invalid { app_error, .. } => app_error,
        StoreError::InvalidInput { .. } => AppError::boxed(
            "UpdateUser",
            "app.user.update.find.app_error",
            None,
            String::new(),
            400,
        ),
        StoreError::Conflict { resource, .. } => AppError::boxed(
            "UpdateUser",
            if resource == "Username" {
                "app.user.save.username_exists.app_error"
            } else {
                "app.user.save.email_exists.app_error"
            },
            None,
            String::new(),
            400,
        ),
        other => {
            tracing::error!(error = %other, user_id, "user update failed");
            AppError::boxed(
                "UpdateUser",
                "app.user.update.finding.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

impl App {
    /// Port of `App.GetUserByAuthData` (app/user.go:627).
    ///
    /// Three store errors, three status codes, and **one id for all of them**:
    /// `app.user.missing_account.const` is 400 for invalid input, 404 for a miss and 500 for a
    /// broken query. A client cannot tell the three apart from the body — only from the status
    /// line — which is Go's choice and is on the wire.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn get_user_by_auth_data(&self, auth_data: &str) -> AppResult<User> {
        let user = self
            .store()
            .user()
            .get_by_auth_data(auth_data)
            .await
            .map_err(|err| {
                let status = if err.is_invalid_input() {
                    400
                } else if err.is_not_found() {
                    404
                } else {
                    tracing::error!(error = %err, "auth-data lookup failed");
                    500
                };
                AppError::boxed(
                    "GetUserByAuthData",
                    "app.user.missing_account.const",
                    None,
                    String::new(),
                    status,
                )
            })?;

        tracing::Span::current().record("found", true);
        Ok(user)
    }

    /// Port of `App.GetUsersWithInvalidEmails` (app/user.go:3293).
    ///
    /// The allowed-domain list is read from configuration here rather than passed in, as Go does
    /// — `*a.Config().TeamSettings.RestrictCreationToDomains` is the store call's third argument.
    ///
    /// Note the `where`: Go names it **`GetUsersPage`**, not `GetUsersWithInvalidEmails`, and
    /// reuses `app.user.get_profiles.app_error`. Both are on the wire.
    #[tracing::instrument(skip_all, fields(page, per_page, found))]
    pub async fn get_users_with_invalid_emails(
        &self,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<User>> {
        tracing::Span::current().record("page", page);
        tracing::Span::current().record("per_page", per_page);

        let users = self
            .store()
            .user()
            .get_users_with_invalid_emails(
                page,
                per_page,
                &self.config().restrict_creation_to_domains,
            )
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "invalid-email lookup failed");
                AppError::boxed(
                    "GetUsersPage",
                    "app.user.get_profiles.app_error",
                    None,
                    String::new(),
                    500,
                )
            })?;

        tracing::Span::current().record("found", users.len());
        Ok(users)
    }
}

fn get_user_error(err: StoreError) -> Box<AppError> {
    match err {
        StoreError::NotFound { .. } => AppError::boxed(
            "GetUser",
            "app.user.missing_account.const",
            None,
            String::new(),
            404,
        ),
        other => {
            tracing::error!(error = %other, "user lookup failed");
            AppError::boxed(
                "GetUser",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        }
    }
}

impl App {
    /// Port of `app.App.UserCanSeeOtherUser` (app/user.go:2710).
    ///
    /// Three of its four branches are here and the fourth is forwarded:
    ///
    /// - **The caller asking about themselves is true**, checked first and without touching a
    ///   store — so a user with no permissions at all can always read their own profile image.
    /// - **No view restrictions is true**, which is every account on a stock server; see
    ///   [`App::get_view_users_restrictions`] for why.
    /// - **Restricted** would go on to ask whether the *other* user shares a team or a channel
    ///   with the caller, through two store methods this port does not have
    ///   (`Team().UserBelongsToTeams`, `Channel().UserBelongsToChannels`). Refused as
    ///   [`crate::post::PrepareError::Unreproducible`] so the handler forwards.
    ///
    /// The restricted branch is reachable only for a guest account or a deployment that has
    /// edited `system_user`'s permissions, which is why forwarding it costs nothing in practice.
    pub async fn user_can_see_other_user(
        &self,
        user_id: &str,
        other_user_id: &str,
    ) -> Result<bool, crate::post::PrepareError> {
        if user_id == other_user_id {
            return Ok(true);
        }

        match self.get_view_users_restrictions(user_id).await {
            crate::user::ViewUsersRestriction::None => Ok(true),
            crate::user::ViewUsersRestriction::Restricted => {
                Err(crate::post::PrepareError::Unreproducible(
                    "view-user restrictions need the team and channel membership lookups",
                ))
            }
        }
    }
}

/// Port of `app.getProfileImagePath` (app/user.go:3302) — `filepath.Join("users", id, "profile.png")`.
fn profile_image_path(user_id: &str) -> String {
    mm_model::go_path::join(&["users", user_id, "profile.png"])
}

impl App {
    /// Port of `Server.GetProfileImage` (app/server.go:1885), **narrowed to its one reproducible
    /// branch**.
    ///
    /// Go's function has three outcomes and only the middle one is ours:
    ///
    /// | condition | Go's answer | here |
    /// |---|---|---|
    /// | `FileSettings.DriverName == ""` | a generated default avatar | forwarded |
    /// | the stored `users/<id>/profile.png` reads | those bytes, `readFailed = false` | **served** |
    /// | the read fails | a generated default avatar, `readFailed = true`, and a *write* when `LastPictureUpdate == 0` | forwarded |
    ///
    /// # Why the default avatar is not ported
    ///
    /// `users.GetDefaultProfileImage` rasterises the user's initials with a TTF font through
    /// `golang/freetype`, and the answer is a PNG whose every pixel depends on that rasteriser's
    /// hinting and anti-aliasing. There is no way to match it byte for byte short of embedding
    /// the same font *and* the same rasteriser, and a near-match is worse than a forward: the
    /// image is cached by the client for a day under an etag we would have minted.
    ///
    /// This is also why `GET /users/{user_id}/image/default`, which is *only* that path, is not
    /// migrated at all. See [D-204].
    ///
    /// In practice the served branch is the common one: every user gets a `profile.png` written
    /// at account creation by `SetDefaultProfileImage`, so the fallback fires for accounts
    /// created before that behaviour or whose file has been removed from under the server.
    pub async fn get_profile_image(
        &self,
        user_id: &str,
    ) -> Result<Vec<u8>, crate::post::PrepareError> {
        use crate::post::PrepareError;

        if self.config().file_driver_name.is_empty() {
            return Err(PrepareError::Unreproducible(
                "a driverless configuration serves a generated default avatar",
            ));
        }

        self.read_file(&profile_image_path(user_id))
            .await
            .map_err(|err| match err {
                PrepareError::App(_) => PrepareError::Unreproducible(
                    "no stored profile image, so Go generates a default avatar and may write it",
                ),
                unreproducible => unreproducible,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_user_is_404_with_gos_error_id() {
        let err = get_user_error(StoreError::NotFound {
            entity: "User",
            criteria: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
        });
        assert_eq!(err.status_code, 404);
        assert_eq!(err.id, "app.user.missing_account.const");
    }

    /// A driver failure must not be reported to the client as a missing account — that would turn
    /// an outage into a plausible-looking 404 and hide it from every dashboard watching 5xx.
    #[test]
    fn a_broken_query_is_500_not_404() {
        let err = get_user_error(StoreError::Db {
            context: "connection pool closed".to_owned(),
            source: sqlx::Error::PoolClosed,
        });
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user.get.app_error");
    }

    /// `GetUsersByIds` has a single error branch with its own id — not `GetUser`'s pair and
    /// not `GetUserByUsername`'s — and no not-found at all.
    #[tokio::test]
    async fn a_broken_by_ids_lookup_is_a_500_with_get_profiles_id() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let app = crate::App::new(mm_store::SqlStore::from_pool(pool));

        let err = app
            .get_users_by_ids(&["y9i4er48tt8bukijy7i3u5y9ar".to_owned()], 0)
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user.get_profiles.app_error");
        assert_eq!(err.where_, "GetUsersByIds");
        assert!(err.params.is_none());
    }

    /// All five listing branches collapse to one wire error, and the offset multiply belongs to
    /// the two branches whose Go caller does it. Both are pinned here because the *only* way to
    /// tell `GetUsersNotInChannel` from `GetUsersInChannel` after `into_wire` is the log line.
    #[tokio::test]
    async fn every_listing_branch_is_the_same_500_and_only_where_differs() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let app = crate::App::new(mm_store::SqlStore::from_pool(pool));
        let page = UserPage {
            page: 2,
            per_page: 60,
            inactive: false,
            active: false,
        };

        let errors = vec![
            app.get_users_page(page).await.expect_err("unreachable"),
            app.get_users_in_team_page("t", page)
                .await
                .expect_err("unreachable"),
            app.get_users_in_channel_page("c", page)
                .await
                .expect_err("unreachable"),
            app.get_users_not_in_channel_page("t", "c", page)
                .await
                .expect_err("unreachable"),
            app.get_users_not_in_team_page("t", page)
                .await
                .expect_err("unreachable"),
        ];
        for err in &errors {
            assert_eq!(err.status_code, 500);
            assert_eq!(err.id, "app.user.get_profiles.app_error");
            assert!(err.params.is_none());
        }
        let wheres: Vec<&str> = errors.iter().map(|e| e.where_.as_str()).collect();
        assert_eq!(
            wheres,
            vec![
                "GetUsersPage",
                "GetUsersInTeamPage",
                // Not `…Page`: the error is minted a level lower for this one.
                "GetUsersInChannel",
                "GetUsersNotInChannel",
                "GetUsersNotInTeamPage",
            ]
        );

        assert_eq!(page.offset(), 120, "page * per_page, at the app layer");
    }

    /// `GetUserByUsername` shares one id across both branches — only the status splits them —
    /// and that id is **not** `MissingAccountError`. The unreachable store can only produce the
    /// 500; the 404's identity is the same literal by construction, pinned by contrast.
    #[tokio::test]
    async fn a_broken_username_lookup_is_a_500_with_the_shared_id() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(250))
            .connect_lazy("postgres://nobody@127.0.0.1:1/nothing")
            .expect("a lazy pool is built without connecting");
        let app = crate::App::new(mm_store::SqlStore::from_pool(pool));

        let err = app
            .get_user_by_username("sliceuser")
            .await
            .expect_err("the store is unreachable");
        assert_eq!(err.status_code, 500);
        assert_eq!(err.id, "app.user.get_by_username.app_error");
        assert_eq!(err.where_, "GetUserByUsername");
        assert_ne!(
            err.id, "app.user.missing_account.const",
            "the by-username miss does not wear MissingAccountError (user.go:573)"
        );
        assert!(err.params.is_none());
    }

    /// The 500 branch of `GetUsersByGroupChannelIds`, which nothing reachable over HTTP can
    /// produce: the store only fails on a driver error, and the route has no input that causes
    /// one. A mutation swapping this error id for a neighbouring one survived a full parity run
    /// for exactly that reason.
    #[tokio::test]
    async fn a_store_failure_carries_the_group_channel_error_id() {
        // A pool pointed at nothing, so the call fails without a 30-second acquire timeout in a
        // unit suite (CLAUDE.md). `connect_lazy` still wants a reactor, hence `#[tokio::test]`.
        let store = mm_store::SqlStore::from_pool(
            sqlx::postgres::PgPoolOptions::new()
                .acquire_timeout(std::time::Duration::from_millis(50))
                .connect_lazy("postgres://unused/unused")
                .expect("a lazy pool needs no server"),
        );
        let app = crate::App::with_config(store, crate::config::Config::default());

        let err = app
            .get_users_by_group_channel_ids("someuserid1jbyqbtxbtqcgy", &["c".to_owned()])
            .await
            .expect_err("the store is not connected");
        assert_eq!(
            err.id,
            "app.user.get_profile_by_group_channel_ids_for_user.app_error"
        );
        assert_eq!(err.status_code, 500);
        assert_eq!(err.where_, "GetUsersByGroupChannelIds");
    }
}
