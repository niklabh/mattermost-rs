//! Port of `app.GetUser`, `app.GetUserByUsername` and `app.GetUsersByIds` (channels/app/user.go).

use mm_model::user::User;
use mm_model::utils::AppError;
use mm_store::{StoreError, UserStore};

use crate::App;

impl App {
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
    pub async fn get_user(&self, id: &str) -> Result<User, AppError> {
        self.store().user().get(id).await.map_err(get_user_error)
    }
}

impl App {
    /// Port of `app.App.GetUserByUsername` (user.go:567).
    ///
    /// **Both branches carry the same id** — `app.user.get_by_username.app_error` — and only the
    /// status separates a miss from a broken query, the `GetChannelUnread` shape rather than
    /// `GetUser`'s two-id shape three lines up in the same Go file. Neither branch matches
    /// `MissingAccountError` either; a client cannot correlate "no such id" with "no such
    /// username" by error id, and that is Go's wire.
    #[tracing::instrument(skip_all, fields(username = %username))]
    pub async fn get_user_by_username(&self, username: &str) -> Result<User, AppError> {
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
                AppError::new(
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
    pub async fn get_users_by_ids(
        &self,
        ids: &[String],
        since: i64,
    ) -> Result<Vec<User>, AppError> {
        self.store()
            .user()
            .get_profile_by_ids(ids, since)
            .await
            .map_err(|err| {
                tracing::error!(error = %err, "users-by-ids lookup failed");
                AppError::new(
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
fn get_profiles_error(where_: &'static str, err: StoreError) -> AppError {
    tracing::error!(error = %err, where_, "user profile listing failed");
    AppError::new(
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
    pub async fn get_users_page(&self, page: UserPage) -> Result<Vec<User>, AppError> {
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
    ) -> Result<Vec<User>, AppError> {
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
    ) -> Result<Vec<User>, AppError> {
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
    ) -> Result<Vec<User>, AppError> {
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
    ) -> Result<Vec<User>, AppError> {
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

/// The store-error-to-`AppError` mapping for `GetUser`, split out so it is reachable from a test
/// without a database. A miss and a broken query are different HTTP statuses, and collapsing them
/// would report a server fault to the client as a missing account.
fn get_user_error(err: StoreError) -> AppError {
    match err {
        StoreError::NotFound { .. } => AppError::new(
            "GetUser",
            "app.user.missing_account.const",
            None,
            String::new(),
            404,
        ),
        other => {
            tracing::error!(error = %other, "user lookup failed");
            AppError::new(
                "GetUser",
                "app.user.get.app_error",
                None,
                String::new(),
                500,
            )
        }
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
}
