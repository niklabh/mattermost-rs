//! Port of `SqlUserStore` (channels/store/sqlstore/user_store.go), `Get`, `GetByUsername` and
//! `GetProfileByIds`.

use mm_model::user::{User, UserUpdate};
use mm_model::utils::{CURRENT_VERSION, StringArray, StringMap};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UserStore` (store/store.go:448-550) that is ported.
pub trait UserStore {
    /// Port of `SqlUserStore.Get` (user_store.go:609).
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.UpdateUpdateAt` (user_store.go) — one column, no read, no
    /// validation.
    ///
    /// **The timestamp is minted before the write and returned even when the write fails**
    /// (`return curTime, errors.Wrapf(...)`), and Go's caller only checks the error. It is also
    /// not an upsert: an unknown id updates zero rows and is reported as success, which is why
    /// `postProcessTeamMemberLeave` cannot notice a user that vanished under it.
    ///
    /// What it is *for* is `GET /users/{id}`'s etag: every client caching a user re-fetches after
    /// this runs. Skipping it leaves the caches stale — which is exactly what [D-242] records for
    /// the join path.
    fn update_update_at(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlUserStore.Count` (user_store.go:1471) for the **one** options shape reachable
    /// today: `UserCountOptions{IncludeBotAccounts: true}` with nil view restrictions, which is
    /// what `App.GetTotalUsersStats` passes.
    ///
    /// Takes no parameters on purpose. Go builds this query from ten option fields and every one
    /// of the other nine is at its zero value here; a parameter with one reachable value is a
    /// field with no reader, and the two branches it would gate — the `Bots` anti-join and the
    /// view-restriction joins — are unported for the reasons `count_total_users` and
    /// `mm_app::App::get_view_users_restrictions` give.
    /// Port of `SqlUserStore.Count` (user_store.go:1471) — the filtered count.
    fn count(
        &self,
        options: &mm_model::user_count::UserCountOptions,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    fn count_total_users(
        &self,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlUserStore.GetKnownUsers` (user_store.go:2357): every *other* user who shares a
    /// channel with this one.
    fn get_known_users(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<Vec<String>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetByUsername` (user_store.go:1402).
    /// Port of `SqlUserStore.GetProfilesByUsernames` (user_store.go:1084).
    fn get_profiles_by_usernames(
        &self,
        usernames: &[String],
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.Update` (user_store.go:255).
    ///
    /// `trusted_update_data` is Go's second parameter. **Every caller reachable from api4 passes
    /// `false`** — `App.UpdateUser` calls `userService.UpdateUser(rctx, user, false)` regardless
    /// of its own `sendNotifications` flag — so the `!trusted` block is the live path and the
    /// `true` path is reached only by the CLI and by tests.
    ///
    /// Returns Go's `UserUpdate{Old, New}`: the caller needs both, because whether to send an
    /// email-change email, a username-change email, and a new default profile picture are all
    /// decided by comparing them.
    fn update(
        &self,
        user: &User,
        trusted_update_data: bool,
    ) -> impl std::future::Future<Output = Result<UserUpdate, StoreError>> + Send;

    /// Port of `SqlPostStore.GetMaxPostSize` (post_store.go:2747), which `SqlUserStore.Update`
    /// consults to bound `auto_responder_message`.
    ///
    /// Lives on this trait rather than the post store because this is its only caller here, and
    /// because Go reaches it through `us.Post()` — a store-to-store call the layering forbids.
    fn max_post_size(&self) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlGroupStore.GetByName` (group_store.go:290), reduced to the question its one
    /// caller asks.
    ///
    /// `App.isUniqueToGroupNames` wants a yes or no, and Go's query carries **no `DeleteAt`
    /// predicate** — a soft-deleted group still reserves its name against a username.
    fn group_name_exists(
        &self,
        name: &str,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlUserStore.GetByEmail` (user_store.go:1282).
    fn get_by_email(
        &self,
        email: &str,
    ) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    fn get_by_username(
        &self,
        username: &str,
    ) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfileByIds` (user_store.go:1172) for nil view restrictions.
    ///
    /// `since` is `UserGetByIdsOpts.Since`: applied as `UpdateAt > since` **only when positive**
    /// (`options.Since > 0`), so `0` and a negative value both mean "no filter". `IsAdmin` is
    /// not a store concern — Go carries it in the same options struct but only the sanitizer
    /// reads it. The restricted variant (`applyViewRestrictionsFilter`'s joins) is not ported;
    /// the api layer forwards those callers.
    fn get_profile_by_ids(
        &self,
        ids: &[String],
        since: i64,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfileByGroupChannelIdsForUser` (user_store.go:1209): the other
    /// members of each named group channel, keyed by channel id.
    fn get_profile_by_group_channel_ids_for_user(
        &self,
        user_id: &str,
        channel_ids: &[String],
    ) -> impl std::future::Future<
        Output = Result<std::collections::BTreeMap<String, Vec<User>>, StoreError>,
    > + Send;

    /// Port of `SqlUserStore.GetAllProfiles` (user_store.go:682) — `GET /users` with no filter
    /// at all — for nil view restrictions, no role filter and the default sort.
    fn get_all_profiles(
        &self,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfiles` (user_store.go:835) — the `in_team` filter.
    fn get_profiles_in_team(
        &self,
        team_id: &str,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfilesInChannel` (user_store.go:869) — the `in_channel` filter.
    fn get_profiles_in_channel(
        &self,
        channel_id: &str,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfilesNotInChannel` (user_store.go:1012) for nil view
    /// restrictions and `groupConstrained = false`.
    ///
    /// **Takes an offset, not a page.** Go's caller multiplies (`app/user.go:803`), unlike the
    /// three above where the store does it — the difference is preserved so an off-by-one lives
    /// where Go put it. There is also **no active/inactive predicate** here at all: this
    /// function takes no options, so `?active=true&not_in_channel=…` lists deactivated users.
    fn get_profiles_not_in_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetProfilesNotInTeam` (user_store.go:1890) for nil view
    /// restrictions and `groupConstrained = false`. Offset-taking and unfiltered by `DeleteAt`,
    /// for the same reasons as [`UserStore::get_profiles_not_in_channel`].
    fn get_profiles_not_in_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetEtagForProfiles` (user_store.go:826).
    ///
    /// Infallible by design: Go discards the query error and falls back to
    /// `CurrentVersion.GetMillis()`, which never matches a second call — so an empty team's
    /// etag is *deliberately* uncacheable on both servers.
    fn get_etag_for_profiles(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = String> + Send;

    /// Port of `SqlUserStore.GetEtagForProfilesNotInTeam` (user_store.go:1919).
    ///
    /// A different shape from its sibling: `CONCAT(MAX(UpdateAt), '.', COUNT(Id))` over an
    /// aggregate that always returns exactly one row, so the millisecond fallback is
    /// unreachable and an empty result is the literal `.0` rather than a fresh timestamp.
    fn get_etag_for_profiles_not_in_team(
        &self,
        team_id: &str,
    ) -> impl std::future::Future<Output = String> + Send;

    /// Port of `SqlUserStore.Search` (user_store.go:1628) → `performSearch`.
    ///
    /// An **empty `team_id` means no team filter at all**, which is Go's `if teamId != ""`
    /// guard around the `TeamMembers` join — and it is a reachable state, not a defensive
    /// check: `autocompleteUsers`' third arm calls `SearchUsersInTeam(rctx, "", …)` and
    /// searches every user in the installation.
    fn search(
        &self,
        team_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.SearchInChannel` (user_store.go:1727) → `performSearch`.
    fn search_in_channel(
        &self,
        channel_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.SearchNotInChannel` (user_store.go:1707) → `performSearch`, for
    /// `GroupConstrained = false`.
    ///
    /// The `team_id` guard is Go's again, but on this route it can never fire: the only caller
    /// is `AutocompleteUsersInChannel`, and `autocompleteUsers` refuses a channel search with
    /// no team before ever reaching it (api4/user.go:1437). Kept anyway so the port matches the
    /// function it is a port of.
    fn search_not_in_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.GetUserReport` (user_store.go:2503) — the System Console's user
    /// report, one row per non-bot user with four aggregates attached.
    ///
    /// # Keyset pagination, and the tiebreaker that does not follow the sort
    ///
    /// Go decides the SQL sort direction *twice*: from `SortDesc`, and then again from
    /// `Direction` when a cursor is present, where `prev`-on-ascending and `next`-on-descending
    /// both flip it to `DESC`. The cursor predicate follows that flip — `<` for `DESC`, `>` for
    /// `ASC` — but the `Users.Id` tiebreaker in the `ORDER BY` is written without a direction and
    /// is therefore **always `ASC`**, even on a descending page whose predicate reads
    /// `Users.Id <`. That asymmetry is Go's and it is reproduced, not corrected.
    ///
    /// # `prev` re-sorts the page it just fetched
    ///
    /// A backwards page comes out of the database in reverse and Go wraps the whole statement in
    /// a `SELECT … FROM (…) data` that sorts it back. The wrapper is applied whenever
    /// `Direction == "prev"` — **including with no cursor at all**, where it simply reverses the
    /// first page.
    fn get_user_report(
        &self,
        options: &mm_model::report::UserReportOptions,
    ) -> impl std::future::Future<
        Output = Result<Vec<mm_model::report::UserReportQuery>, StoreError>,
    > + Send;

    /// Port of `SqlUserStore.GetUserCountForReport` (user_store.go:2483).
    ///
    /// The same `applyUserReportFilter` as [`UserStore::get_user_report`] over a bare
    /// `COUNT(Users.Id)`, so the two agree by construction — **except** that the count ignores
    /// the date range entirely. `StartAt`/`EndAt` reach the report only through the `PostStats`
    /// join condition, which this query does not have, so narrowing the date range changes the
    /// aggregates on a row and never the number of rows.
    fn get_user_count_for_report(
        &self,
        options: &mm_model::report::UserReportOptions,
    ) -> impl std::future::Future<Output = Result<i64, StoreError>> + Send;

    /// Port of `SqlUserStore.GetByAuthData` (user_store.go:1322).
    ///
    /// **Not `GetByAuth`**, which is the neighbouring function and adds an `AuthService`
    /// predicate. This one matches on `AuthData` alone, so a single value can only belong to one
    /// account however that account authenticates.
    ///
    /// An empty `auth_data` is `ErrInvalidInput`, which the app layer answers **400** to rather
    /// than the 404 a miss gets. Unreachable through `getUserByAuthData`, which rejects an empty
    /// `value` two lines earlier — ported because the two errors are different status codes and a
    /// later caller would meet the distinction.
    fn get_by_auth_data(
        &self,
        auth_data: &str,
    ) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.GetUsersWithInvalidEmails` (user_store.go:2404).
    ///
    /// "Invalid" means **outside every allowed domain**: the caller passes
    /// `TeamSettings.RestrictCreationToDomains`, the store splits it on `,` and adds one
    /// `Email NOT LIKE '%domain%'` per non-empty piece. There is no `@` anchoring and no
    /// trimming, so a configured `" example.com"` matches nothing and every account is reported.
    ///
    /// Bots, guests, deactivated accounts and anyone with an `AuthService` are excluded — a user
    /// who signs in through LDAP or SAML did not choose their email, so it is not theirs to be
    /// wrong.
    ///
    /// **No `ORDER BY`.** The page is whatever order Postgres returns, and page 1 is not
    /// guaranteed to be disjoint from page 0. Both servers issue the same statement to the same
    /// database, so they agree; neither is stable across an `UPDATE`.
    fn get_users_with_invalid_emails(
        &self,
        page: i64,
        per_page: i64,
        restricted_domains: &str,
    ) -> impl std::future::Future<Output = Result<Vec<User>, StoreError>> + Send;

    /// Port of `SqlUserStore.UpdatePassword` (user_store.go:410).
    ///
    /// **Six columns, not one.** The statement is
    ///
    /// ```sql
    /// UPDATE Users SET Password = ?, LastPasswordUpdate = ?, UpdateAt = ?,
    ///                  AuthData = NULL, AuthService = '', FailedAttempts = 0
    ///  WHERE Id = ?
    /// ```
    ///
    /// so setting a password *converts the account to email auth* and clears the lockout counter
    /// as a side effect. That is load-bearing rather than incidental: it is how `resetPassword`
    /// unlocks an account that failed its way to the cap, and how an admin moves a SAML user back
    /// to a password. `LastPasswordUpdate` and `UpdateAt` are the **same** millisecond, read once.
    ///
    /// A miss writes nothing and is **not** an error — Go ignores the affected-row count.
    fn update_password(
        &self,
        user_id: &str,
        hashed_password: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserStore.GetForLogin` (user_store.go:1422).
    ///
    /// # The two flags choose the predicate, and neither being set is an error
    ///
    /// `username && email` matches `Username = lower($1) OR Email = lower($1)`; either alone
    /// drops the other side; **neither returns an error before a query is issued**. That last
    /// branch is reachable — an administrator can switch both sign-in methods off — and it is not
    /// the same as "no such user": Go's `GetUserForLogin` only calls this at all when one of the
    /// two is set, so the error arm is dead from `login` and live from nothing else. Reproduced
    /// because the next caller may not have that guard.
    ///
    /// # `lower()` is applied to the parameter, not the column
    ///
    /// So the comparison is case-**sensitive on the stored value**. `PreSave` normalises both
    /// `Username` and `Email` to lower case, so this is equivalent to a case-insensitive lookup
    /// for any row the server itself wrote — but a row inserted by hand with an uppercase
    /// username cannot be logged into by name at all, on either server. A port that lowered the
    /// column instead would let that row in and diverge.
    ///
    /// # Zero rows and two rows are **different** errors in Go, and both are refusals
    ///
    /// With both flags on, one account may hold another's username as its email address, which
    /// is the two-row case. Go distinguishes them in the message only; both reach
    /// `GetUserForLogin`'s single `store.sql_user.get_for_login.app_error`, so both surface as
    /// [`StoreError::NotFound`] here with different criteria.
    fn get_for_login(
        &self,
        login_id: &str,
        allow_sign_in_with_username: bool,
        allow_sign_in_with_email: bool,
    ) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.UpdateLastLogin` (user_store.go:498).
    ///
    /// `SET LastLogin = $1, UpdateAt = GetMillis()` — **two different instants**. `DoLogin` passes
    /// the new session's `CreateAt` as the login time, while `UpdateAt` is taken fresh inside the
    /// store, so the two columns differ by however long the session insert took. Collapsing them
    /// onto one value would be tidier and would not be Go.
    ///
    /// Bumping `UpdateAt` matters beyond bookkeeping: it is the etag input for `GET /users/{id}`,
    /// so logging in invalidates every cached copy of your own profile.
    ///
    /// A miss writes nothing and is not an error — Go discards the row count.
    fn update_last_login(
        &self,
        user_id: &str,
        last_login: i64,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserStore.UpdateFailedPasswordAttempts` (user_store.go:420).
    ///
    /// An unconditional `SET FailedAttempts = ?`. Every caller passes `0`, so in practice this is
    /// "clear the lockout" — but it is a *set*, not a reset, and it does **not** touch `UpdateAt`.
    /// That last part is why a successful login does not bump `Users.UpdateAt` on its own.
    fn update_failed_password_attempts(
        &self,
        user_id: &str,
        attempts: i32,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserStore.TryIncrementFailedPasswordAttempts` (user_store.go:434).
    ///
    /// `UPDATE ... SET FailedAttempts = FailedAttempts + 1 WHERE Id = ? AND FailedAttempts < ?`,
    /// returning whether one row changed. This is a **claim**, not a count: the caller increments
    /// *before* checking the password and refunds with
    /// [`UserStore::decrement_failed_password_attempts`] when the failure turns out not to be a
    /// credential mismatch. The row lock the UPDATE takes is the whole concurrency story — two
    /// simultaneous attempts cannot both read `maxAttempts - 1` and both claim.
    ///
    /// `false` also means "no such user", which is indistinguishable from "already at the cap"
    /// and is exactly what makes the lockout error safe to return for an unknown account.
    ///
    /// The predicate is strictly `<`, so `maxAttempts` is the number of attempts *allowed*: the
    /// last claim moves the counter to `maxAttempts` and the next call fails.
    fn try_increment_failed_password_attempts(
        &self,
        user_id: &str,
        max_attempts: i32,
    ) -> impl std::future::Future<Output = Result<bool, StoreError>> + Send;

    /// Port of `SqlUserStore.DecrementFailedPasswordAttempts` (user_store.go:456).
    ///
    /// The refund half of the claim above, floored at zero by `AND FailedAttempts > 0` rather
    /// than by arithmetic. Go discards the row count here — a refund that finds nothing to refund
    /// is success.
    fn decrement_failed_password_attempts(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserStore.VerifyEmail` (user_store.go:1455).
    ///
    /// `SET Email = lower(?), EmailVerified = true, UpdateAt = ?` — the email is **rewritten**,
    /// not merely flagged, which is how following a verification link is also what commits an
    /// email *change*. Lower-casing happens in SQL, so a token minted with a mixed-case address
    /// still lands as lower case.
    ///
    /// Go returns the user id it was given; there is nothing to return here that the caller did
    /// not pass in.
    fn verify_email(
        &self,
        user_id: &str,
        email: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;

    /// Port of `SqlUserStore.Save` (user_store.go:175), ported for `createBot`.
    ///
    /// # The hasher is a parameter because it cannot be a dependency
    ///
    /// Go calls `hashers.GetLatestHasher()` inside the store. Here the hashers live in `mm-app`
    /// for licensing reasons (see `mm_app::password`), so the caller supplies one. A bot's
    /// password is empty and `PreSave` only hashes a non-empty one, so `createBot` never
    /// actually reaches it — but the parameter keeps the error branches real for the next caller.
    ///
    /// # A non-empty `Id` is refused
    ///
    /// `if user.Id != "" && !user.IsRemote()` → `ErrInvalidInput("User", "id", …)`. `UserFromBot`
    /// copies `Bot.UserId` into it, so a create whose patch somehow carried a user id fails here
    /// rather than silently overwriting a row.
    ///
    /// # Its unique violations are `InvalidInput`, not `Conflict`
    ///
    /// [`UserStore::update`] raises [`StoreError::Conflict`] for the same constraints. `Save`
    /// raises `ErrInvalidInput` with the *field* — and `App.CreateBot` branches on that field to
    /// pick between `email_exists`, `username_exists` and `existing`. Folding the two shapes
    /// together would change which of those three ids a client sees.
    ///
    /// # The database picks the field, not Go's order of checks
    ///
    /// A bot's email is derived from its username, so a duplicate username violates **both**
    /// unique indexes — and Go tests its email list first, which reads as "email wins". It does
    /// not: `IsUniqueConstraintError` is `strings.Contains(err.Error(), …)` over the pq error
    /// text, and Postgres reports exactly one constraint per error, whichever index it checked.
    /// So the answer is `username`, on both servers, and matching on the constraint name here is
    /// the same rule rather than a simplification. Go's capitalised `"Email"`/`"Username"`
    /// entries are dead on Postgres, where every index name is lower case. Measured in
    /// `db_bot_store.rs`, which asserted the intuitive reading and failed.
    fn save(
        &self,
        user: &User,
        hasher: &(dyn mm_model::user::UserPasswordHasher + Sync),
    ) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.PermanentDelete` (user_store.go:1464).
    ///
    /// One `DELETE`, and **no error for a row that was not there** — Go does not look at
    /// `RowsAffected`. `App.CreateBot` calls it to undo its own user insert when the bot insert
    /// fails, and only logs what comes back.
    fn permanent_delete(
        &self,
        user_id: &str,
    ) -> impl std::future::Future<Output = Result<(), StoreError>> + Send;
}

/// `model.UserSearchDefaultLimit` (model/user_search.go:7).
/// Port of `MaxGroupChannelsForProfiles` (user_store.go:29).
///
/// Go **silently truncates** the caller's id list to this many rather than refusing a longer
/// one — `channelIds = channelIds[0:MaxGroupChannelsForProfiles]` — so asking about sixty group
/// channels answers about fifty of them, with no error and nothing in the response saying so.
pub const MAX_GROUP_CHANNELS_FOR_PROFILES: usize = 50;

pub const USER_SEARCH_DEFAULT_LIMIT: i64 = 100;
/// `model.UserSearchMaxLimit` (model/user_search.go:6).
pub const USER_SEARCH_MAX_LIMIT: i64 = 1000;

/// The slice of `model.UserSearchOptions` (model/user_search.go:29) that `autocompleteUsers`
/// actually varies, and therefore the only slice this port implements.
///
/// The five fields left out are left out because that handler pins each of them and the pinned
/// value is what the SQL below hard-codes:
///
/// | field | value on this route | consequence in the SQL |
/// |---|---|---|
/// | `AllowEmails` | **always `false`** — "Never autocomplete on emails" (api4/user.go:1399) | `Email` is not a searchable column here at all |
/// | `AllowInactive` | never set, so `false` | `Users.DeleteAt = 0` is unconditional |
/// | `Role` / `Roles` / `TeamRoles` / `ChannelRoles` | never set | `applyRoleFilter` and `applyMultiRoleFilters` are both no-ops |
/// | `GroupConstrained` | never set | no group-constrained join |
/// | `ViewRestrictions` | non-nil only for a caller without `view_members`, whom the api layer forwards to Go | `applyViewRestrictionsFilter` is a no-op, and so is its `DISTINCT` |
///
/// `IsAdmin` is absent for a different reason: it is carried in the same Go struct but no query
/// reads it — only the sanitizer does, and sanitisation lives in the api layer here (D-085).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UserSearchOptions {
    /// `AllowFullNames`: whether `FirstName` and `LastName` join `Username` and `Nickname` as
    /// searchable columns (`UserSearchTypeNames` vs `UserSearchTypeNamesNoFullName`,
    /// user_store.go:34-35).
    pub allow_full_names: bool,
    /// `AllowEmails`: whether `Email` joins the searchable columns (`UserSearchTypeAll` vs
    /// `UserSearchTypeNames`, user_store.go:36-37).
    ///
    /// `autocompleteUsers` pins this **false** — "Never autocomplete on emails"
    /// (api4/user.go:1399) — and `searchUsers` sets it from `ShowEmailAddress`, or
    /// unconditionally for a system admin. It is the only difference between the two search
    /// column sets that a non-admin can turn on.
    pub allow_emails: bool,
    /// `AllowInactive`: when false, `Users.DeleteAt = 0` is added.
    ///
    /// Comes straight off `searchUsers`' request body; `autocompleteUsers` never sets it.
    pub allow_inactive: bool,
    /// `Limit`, already defaulted and clamped by the caller. Go casts it with `uint64(...)` and
    /// hands it to Postgres unchecked, so a **negative** limit is a failed query and a 500 on
    /// both servers — measured against the running Go server, not inferred.
    pub limit: i64,
}

/// Port of `sanitizeSearchTerm` (sqlstore/utils.go:62).
///
/// Two steps in this order: strip every occurrence of the escape character, *then* prefix `%`
/// and `_` with it. Doing it the other way round would escape the escapes.
pub fn sanitize_search_term(term: &str, escape_char: char) -> String {
    let stripped: String = term.chars().filter(|c| *c != escape_char).collect();
    let mut out = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        if c == '%' || c == '_' {
            out.push(escape_char);
        }
        out.push(c);
    }
    out
}

/// The terms `generateSearchQuery` (user_store.go:1755) would build one `AND` clause each from.
///
/// `performSearch` sanitises, then guards on `strings.TrimSpace(term) != ""`, then splits with
/// `strings.Fields`; the loop trims **all** leading `@` off each field with `strings.TrimLeft`
/// — so `@@bob` searches for `bob`, and a term that is nothing but `@` searches for the empty
/// string, which `LIKE '%%'` matches everything.
///
/// An empty result means no search predicate at all, which is Go's blank-term branch: the whole
/// `generateSearchQuery` call is skipped and every row in the joined set is returned.
pub fn search_terms(term: &str) -> Vec<String> {
    let sanitized = sanitize_search_term(term, '*');
    if sanitized.trim().is_empty() {
        return Vec::new();
    }
    sanitized
        .split_whitespace()
        .map(|field| field.trim_start_matches('@').to_owned())
        .collect()
}

/// Which `DeleteAt` predicate Go's `if options.Inactive { … } else if options.Active { … }`
/// block selects (user_store.go:697, and identically in three siblings).
///
/// `Some(true)` is "deleted rows only", `Some(false)` is "live rows only", `None` is no
/// predicate. **`Inactive` wins when both are set** — the api layer forwards that request to Go
/// anyway (Go sets an error it never returns on), but the precedence is the store's own and is
/// pinned here so a reader cannot flip the arms.
pub fn deleted_filter(inactive: bool, active: bool) -> Option<bool> {
    if inactive {
        Some(true)
    } else if active {
        Some(false)
    } else {
        None
    }
}

/// `Offset(uint64(options.Page * options.PerPage))`. Saturating because Go's `int` multiply
/// wraps silently and no client should be able to choose which of the two nonsense answers it
/// gets; both servers return nothing for an absurd page either way.
fn offset_of(page: i64, per_page: i64) -> i64 {
    page.saturating_mul(per_page)
}

/// Port of Go's `UserWithChannel` (user_store.go:1198): [`UserRow`] with the joined
/// `ChannelMembers.ChannelId` beside it.
///
/// A separate struct rather than an `Option` on `UserRow` because `query_as!` binds positionally
/// — the two queries have genuinely different shapes, and one type would put a channel id on
/// every user lookup in the file.
struct UserWithChannelRow {
    id: String,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    username: Option<String>,
    password: Option<String>,
    authdata: Option<String>,
    authservice: Option<String>,
    email: Option<String>,
    emailverified: Option<bool>,
    nickname: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
    position: Option<String>,
    roles: Option<String>,
    allowmarketing: Option<bool>,
    props: Option<serde_json::Value>,
    notifyprops: Option<serde_json::Value>,
    lastpasswordupdate: Option<i64>,
    lastpictureupdate: Option<i64>,
    failedattempts: Option<i64>,
    locale: Option<String>,
    timezone: Option<serde_json::Value>,
    mfaactive: Option<bool>,
    mfasecret: Option<String>,
    mfausedtimestamps: Option<serde_json::Value>,
    remoteid: Option<String>,
    lastlogin: i64,
    isbot: bool,
    botdescription: String,
    botlasticonupdate: i64,
    channelid: String,
}

impl UserWithChannelRow {
    /// Drop the joined column and hand the rest to [`user_from_row`], which every other user
    /// lookup already shares.
    fn into_user_row(self) -> UserRow {
        UserRow {
            id: self.id,
            createat: self.createat,
            updateat: self.updateat,
            deleteat: self.deleteat,
            username: self.username,
            password: self.password,
            authdata: self.authdata,
            authservice: self.authservice,
            email: self.email,
            emailverified: self.emailverified,
            nickname: self.nickname,
            firstname: self.firstname,
            lastname: self.lastname,
            position: self.position,
            roles: self.roles,
            allowmarketing: self.allowmarketing,
            props: self.props,
            notifyprops: self.notifyprops,
            lastpasswordupdate: self.lastpasswordupdate,
            lastpictureupdate: self.lastpictureupdate,
            failedattempts: self.failedattempts,
            locale: self.locale,
            timezone: self.timezone,
            mfaactive: self.mfaactive,
            mfasecret: self.mfasecret,
            mfausedtimestamps: self.mfausedtimestamps,
            remoteid: self.remoteid,
            lastlogin: self.lastlogin,
            isbot: self.isbot,
            botdescription: self.botdescription,
            botlasticonupdate: self.botlasticonupdate,
        }
    }
}

/// Postgres-backed implementation.
#[derive(Debug, Clone)]
pub struct SqlUserStore {
    pool: PgPool,
}

impl SqlUserStore {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

/// One row of Go's `usersQuery` — `getUsersColumns()` plus `getBotInfoColumns()` over
/// `Users LEFT JOIN Bots` (user_store.go:120-126). Both ported lookups select exactly this
/// shape, so the mapping lives once in [`user_from_row`].
struct UserRow {
    id: String,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    username: Option<String>,
    password: Option<String>,
    authdata: Option<String>,
    authservice: Option<String>,
    email: Option<String>,
    emailverified: Option<bool>,
    nickname: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
    position: Option<String>,
    roles: Option<String>,
    allowmarketing: Option<bool>,
    props: Option<serde_json::Value>,
    notifyprops: Option<serde_json::Value>,
    lastpasswordupdate: Option<i64>,
    lastpictureupdate: Option<i64>,
    failedattempts: Option<i64>,
    locale: Option<String>,
    timezone: Option<serde_json::Value>,
    mfaactive: Option<bool>,
    mfasecret: Option<String>,
    mfausedtimestamps: Option<serde_json::Value>,
    remoteid: Option<String>,
    lastlogin: i64,
    isbot: bool,
    botdescription: String,
    botlasticonupdate: i64,
}

/// The row-to-model mapping both lookups share.
///
/// Go unmarshals the three JSON columns unconditionally and returns the error, so a malformed
/// column is a failed request on both sides rather than a silently empty map.
///
/// **A JSON `null` is not malformed.** These columns are `jsonb`, which can hold the JSON value
/// `null` as distinct from SQL NULL, and the Go server writes exactly that: four of the five
/// users in the development database have `mfausedtimestamps = 'null'::jsonb`. Go's
/// `json.Unmarshal` turns a JSON null into a nil map or slice without complaint, so both null
/// shapes mean "absent" and only a *type* mismatch is an error. Treating JSON null as a decode
/// failure made `GET /users/me` a 500 for every user except the one the parity tests happen to
/// log in as — see [D-135].
fn user_from_row(row: UserRow) -> Result<User, StoreError> {
    let decode_map = |value: Option<serde_json::Value>,
                      column: &'static str|
     -> Result<Option<StringMap>, StoreError> {
        match value {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => Ok(Some(serde_json::from_value::<StringMap>(value).map_err(
                |source| StoreError::Decode {
                    entity: "User",
                    column,
                    source,
                },
            )?)),
        }
    };

    let mfa_used_timestamps = match row.mfausedtimestamps {
        None | Some(serde_json::Value::Null) => None,
        Some(value) => Some(
            serde_json::from_value::<StringArray>(value).map_err(|source| StoreError::Decode {
                entity: "User",
                column: "mfausedtimestamps",
                source,
            })?,
        ),
    };

    Ok(User {
        id: row.id,
        create_at: row.createat.unwrap_or_default(),
        update_at: row.updateat.unwrap_or_default(),
        delete_at: row.deleteat.unwrap_or_default(),
        username: row.username.unwrap_or_default(),
        password: row.password.unwrap_or_default(),
        auth_data: row.authdata,
        auth_service: row.authservice.unwrap_or_default(),
        email: row.email.unwrap_or_default(),
        email_verified: row.emailverified.unwrap_or_default(),
        nickname: row.nickname.unwrap_or_default(),
        first_name: row.firstname.unwrap_or_default(),
        last_name: row.lastname.unwrap_or_default(),
        position: row.position.unwrap_or_default(),
        roles: row.roles.unwrap_or_default(),
        allow_marketing: row.allowmarketing.unwrap_or_default(),
        props: decode_map(row.props, "props")?,
        notify_props: decode_map(row.notifyprops, "notifyprops")?,
        last_password_update: row.lastpasswordupdate.unwrap_or_default(),
        last_picture_update: row.lastpictureupdate.unwrap_or_default(),
        failed_attempts: row.failedattempts.unwrap_or_default(),
        locale: row.locale.unwrap_or_default(),
        timezone: decode_map(row.timezone, "timezone")?,
        mfa_active: row.mfaactive.unwrap_or_default(),
        mfa_secret: row.mfasecret.unwrap_or_default(),
        mfa_used_timestamps,
        remote_id: row.remoteid,
        last_login: row.lastlogin,
        is_bot: row.isbot,
        bot_description: row.botdescription,
        bot_last_icon_update: row.botlasticonupdate,

        // Not columns on `Users`, and Go's lookups do not populate them either. Each is
        // filled by a different store or left zero:
        //   last_activity_at              — the `Status` table, via a separate query
        //   terms_of_service_*            — `UserTermsOfService`, which the api4 handler
        //                                   fetches separately (api4/user.go:329)
        //   disable_welcome_email         — request-scoped, never persisted
        last_activity_at: 0,
        terms_of_service_id: String::new(),
        terms_of_service_create_at: 0,
        disable_welcome_email: false,
    })
}

impl UserStore for SqlUserStore {
    #[tracing::instrument(skip(self), fields(user_id = %user_id))]
    async fn update_update_at(&self, user_id: &str) -> Result<i64, StoreError> {
        let now = mm_model::utils::get_millis();
        sqlx::query!("UPDATE users SET updateat = $1 WHERE id = $2", now, user_id)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to update User with userId={user_id}"),
                source,
            })?;
        Ok(now)
    }

    /// # Two predicates, and one of them is three-valued
    ///
    /// `DeleteAt = 0` excludes deactivated users. `RemoteId = '' OR RemoteId IS NULL` excludes
    /// users synced from another server — Go writes it as an `OR` over both spellings because
    /// the column is nullable *and* the non-shared write path stores the empty string, so a
    /// plain `RemoteId = ''` would silently drop every row written before that column existed
    /// and `RemoteId IS NULL` alone would drop every row written since.
    ///
    /// # The `Bots` anti-join is absent, and that is `IncludeBotAccounts: true`
    ///
    /// With the flag **off** Go adds `LEFT JOIN Bots … WHERE Bots.UserId IS NULL`. The one
    /// caller sets it on, so bots are counted, and `/users/stats` on a server with an installed
    /// plugin reports a larger number than its member lists show. Reproduced by *not* writing
    /// the join, which is the easiest thing in this file to get wrong by adding.
    /// Port of `SqlUserStore.Count` (user_store.go:1471).
    ///
    /// Go builds this one predicate at a time with squirrel; written here as a single statement
    /// whose clauses are gated on the options, because every join it can add is on a unique key
    /// and so cannot fan a row out.
    ///
    /// # `TeamId` wins over `ChannelId`
    ///
    /// Go's `else if` (user_store.go:1497) means a request naming both filters on the **team**
    /// and ignores the channel entirely — measured: `?in_team=X&in_channel=Y` returns the team's
    /// count, not the intersection. The two guards below encode that precedence rather than
    /// intersecting.
    ///
    /// # The team join carries `DeleteAt = 0`; the channel join does not
    ///
    /// A user who left a team is not counted; a user who left a *channel* has no
    /// `ChannelMembers` row at all, because leaving a channel deletes it outright.
    ///
    /// # `ExcludeRegularUsers` is not modelled
    ///
    /// `getFilteredUsersStats` never sets it, and with `IncludeBotAccounts` off Go **returns an
    /// error** rather than a count for that combination (user_store.go:1491). Nothing reachable
    /// from the wire produces either half.
    #[tracing::instrument(skip_all, fields(count))]
    async fn count(
        &self,
        options: &mm_model::user_count::UserCountOptions,
    ) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
              LEFT JOIN teammembers tm
                ON (tm.userid = u.id AND tm.teamid = $4 AND tm.deleteat = 0)
              LEFT JOIN channelmembers cm ON (cm.userid = u.id AND cm.channelid = $5)
             WHERE ($1 OR u.deleteat = 0)
               AND ($2 OR u.remoteid = '' OR u.remoteid IS NULL)
               AND ($3 OR b.userid IS NULL)
               AND ($4 = '' OR tm.userid IS NOT NULL)
               AND ($4 <> '' OR $5 = '' OR cm.userid IS NOT NULL)
            "#,
            options.include_deleted,
            options.include_remote_users,
            options.include_bot_accounts,
            options.team_id,
            options.channel_id,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Users".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip_all, fields(count))]
    async fn count_total_users(&self) -> Result<i64, StoreError> {
        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(*) AS "count!"
              FROM users
             WHERE users.deleteat = 0
               AND (users.remoteid = '' OR users.remoteid IS NULL)
            "#
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Users".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    /// # A self-join on `ChannelMembers`, and every filter it does **not** have
    ///
    /// No `DeleteAt` anywhere: an archived channel still makes its members known to each other,
    /// and so does a deactivated user's membership — the row survives deactivation. No channel
    /// type filter either, so a direct message counts, which is what makes this route useful to
    /// a client at all.
    ///
    /// `DISTINCT` is doing real work: two users sharing three channels would otherwise appear
    /// three times. There is **no `ORDER BY`**, so the row order is whatever the plan yields and
    /// two servers may legitimately disagree about it — the parity suite compares this route as
    /// a set.
    ///
    /// The `NotEq` excludes the caller. Go writes it as a separate `Where`, which squirrel joins
    /// with `AND`; the caller is in every channel it is a member of, so without it the answer
    /// would always contain the asker.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, count))]
    async fn get_known_users(&self, user_id: &str) -> Result<Vec<String>, StoreError> {
        let ids = sqlx::query_scalar!(
            r#"
            SELECT DISTINCT ocm.userid AS "user_id!"
              FROM channelmembers AS cm
              JOIN channelmembers AS ocm ON ocm.channelid = cm.channelid
             WHERE ocm.userid <> $1
               AND cm.userid = $1
            "#,
            user_id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find ChannelMembers".to_owned(),
            source,
        })?;

        tracing::Span::current().record("count", ids.len());
        Ok(ids)
    }

    #[tracing::instrument(skip_all, fields(user_id = %id, found))]
    async fn get(&self, id: &str) -> Result<User, StoreError> {
        // `usersQuery.Where("Id = ?", id)`. The LEFT JOIN is not optional decoration: `is_bot`
        // is `b.UserId IS NOT NULL`, so dropping the join would make every user a non-bot —
        // including the bots. The two COALESCEs are Go's, reproduced rather than replaced with
        // Rust-side defaulting, so the database answers the same question for both servers.
        //
        // `failedattempts` is `integer` in the schema and `int64` on the model, hence the cast.
        let row = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.id = $1
            "#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get User with userId={id}"),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            // Go interpolates the id here and it is not a credential, so this one matches.
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: id.to_owned(),
            });
        };
        tracing::Span::current().record("found", true);

        user_from_row(row)
    }

    #[tracing::instrument(skip_all, fields(username = %username, found))]
    async fn get_by_username(&self, username: &str) -> Result<User, StoreError> {
        // `usersQuery.Where("Users.Username = lower(?)", username)` (user_store.go:1403) — the
        // **parameter** is lowered, not the column. Stored usernames are already lowercase
        // (`PreSave` normalises them), so this makes the lookup case-insensitive on input while
        // never paying a per-row `lower()`: `GET /users/username/SliceUser` finds `sliceuser`.
        let row = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.username = lower($1)
            "#,
            username
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find User with username={username}"),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: format!("username={username}"),
            });
        };
        tracing::Span::current().record("found", true);

        user_from_row(row)
    }
    /// # No deletion filter, and no options
    ///
    /// `GetProfilesByUsernames` takes a `UserGetOptions` carrying only `ViewRestrictions`; it
    /// never reads `Active`, `Inactive` or `Role`. A **deactivated** user whose username is
    /// asked for is returned like any other, which is what lets a client render an old mention.
    ///
    /// `ORDER BY Users.Username ASC` is wire surface: the answer is a JSON array and its order is
    /// the store's, not the request's.
    ///
    /// # The restrictions filter is not here
    ///
    /// `applyViewRestrictionsFilter` joins `TeamMembers`/`ChannelMembers` for a caller whose
    /// `view_members` is granted only through a team or channel scheme. The api layer forwards
    /// every such caller to Go, so this query is always the nil-restrictions branch — the same
    /// arrangement `get_profile_by_ids` has.
    #[tracing::instrument(skip_all, fields(count = usernames.len(), found))]
    async fn get_profiles_by_usernames(
        &self,
        usernames: &[String],
    ) -> Result<Vec<User>, StoreError> {
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.username = ANY($1::text[])
             ORDER BY u.username ASC
            "#,
            usernames
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(email = %email, found))]
    async fn get_by_email(&self, email: &str) -> Result<User, StoreError> {
        // `usersQuery.Where("Email = lower(?)", email)` (user_store.go:1283) — the
        // **parameter** is lowered, not the column, exactly as the username lookup above does it.
        // `SanitizeEmail` has already lowered the path segment by the time the route reaches
        // here, so the `lower()` is Go's belt and braces; it is kept because a *stored* email
        // that is not lowercase would then be unreachable on both servers, and that is the
        // behaviour to match rather than to fix.
        let row = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.email = lower($1)
            "#,
            email
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find User with email={email}"),
            source,
        })?;

        let Some(row) = row else {
            tracing::Span::current().record("found", false);
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: format!("email={email}"),
            });
        };
        tracing::Span::current().record("found", true);

        user_from_row(row)
    }
    #[tracing::instrument(skip_all, fields(count = ids.len(), since, found))]
    async fn get_profile_by_ids(
        &self,
        ids: &[String],
        since: i64,
    ) -> Result<Vec<User>, StoreError> {
        // `usersQuery.Where({"Users.Id": userIds}).OrderBy("Users.Username ASC")`, plus
        // `Where(Gt{"Users.UpdateAt": Since})` when `Since > 0`. The branch is taken here, in
        // Rust, so the SQL has one shape: a NULL parameter is "no filter".
        //
        // **No `DeleteAt` predicate.** A deactivated user is returned like any other — the
        // webapp relies on it to render the authors of old posts. Pinned by the DB test.
        //
        // The order is the column's collation, which both servers share because they share
        // the database. What they do *not* share is Go's `userProfileByIdsCache`: on the
        // nil-restrictions path Go answers cache hits first, in request order, and only the
        // misses come back from this query sorted — so the wire order over there depends on
        // what was asked recently. Ours is always the query's. See `users::get_users_by_ids`.
        let since_filter = (since > 0).then_some(since);
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.id = ANY($1::varchar[])
               AND ($2::bigint IS NULL OR u.updateat > $2)
             ORDER BY u.username ASC
            "#,
            ids,
            since_filter,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    /// Port of `SqlUserStore.GetProfileByGroupChannelIdsForUser` (user_store.go:1209).
    ///
    /// For each of the named **group** channels the caller is in, the other members' profiles —
    /// which is what the webapp draws a GM's avatar row from.
    ///
    /// # Four predicates, and the interesting one is the `EXISTS`
    ///
    /// `c.Type = 'G'`, the channel id list, `Users.Id <> ?` (the caller is not in their own
    /// avatar row), and an `EXISTS` over `ChannelMembers` asserting the **caller** is a member
    /// of the channel. Without that last one, naming any group channel id would list its
    /// members to anyone — it is the whole access check, and there is none at the handler or app
    /// layer above it.
    ///
    /// **Go builds that `EXISTS` with `fmt.Sprintf` and the user id interpolated into the SQL
    /// text** (user_store.go:1214). The value comes from the session so it is a 26-character id
    /// in practice, but it is a string-built predicate all the same; here it is a bind
    /// parameter. Same rows, and the difference is worth naming rather than silently fixing.
    ///
    /// # The cap truncates rather than refuses
    ///
    /// See [`MAX_GROUP_CHANNELS_FOR_PROFILES`]. The truncation happens on the **sorted,
    /// de-duplicated** list `SortedArrayFromJSON` produced, so it is the fifty
    /// lowest-sorting ids that survive — not the first fifty the client wrote.
    ///
    /// `ORDER BY Users.Username ASC` orders within each channel's list; the map is keyed by
    /// channel id and a `BTreeMap` reproduces `encoding/json`'s bytewise key order.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, asked = channel_ids.len(), found))]
    async fn get_profile_by_group_channel_ids_for_user(
        &self,
        user_id: &str,
        channel_ids: &[String],
    ) -> Result<std::collections::BTreeMap<String, Vec<User>>, StoreError> {
        let capped = &channel_ids[..channel_ids.len().min(MAX_GROUP_CHANNELS_FOR_PROFILES)];

        let rows = sqlx::query_as!(
            UserWithChannelRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!",
                   cm.channelid AS "channelid!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
              JOIN channelmembers cm ON u.id = cm.userid
              JOIN channels c ON cm.channelid = c.id
             WHERE c.type = 'G'
               AND cm.channelid = ANY($1::varchar[])
               AND EXISTS (SELECT 1
                             FROM channelmembers caller
                            WHERE caller.userid = $2
                              AND caller.channelid = cm.channelid)
               AND u.id <> $2
             ORDER BY u.username ASC
            "#,
            capped,
            user_id,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());

        let mut by_channel: std::collections::BTreeMap<String, Vec<User>> =
            std::collections::BTreeMap::new();
        for row in rows {
            let channel_id = row.channelid.clone();
            by_channel
                .entry(channel_id)
                .or_default()
                .push(user_from_row(row.into_user_row())?);
        }
        Ok(by_channel)
    }

    #[tracing::instrument(skip_all, fields(page, per_page, found))]
    async fn get_all_profiles(
        &self,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> Result<Vec<User>, StoreError> {
        // `usersQuery.OrderBy("Users.Username ASC").Offset(page*perPage).Limit(perPage)`, with
        // the `Inactive`/`Active` block as the only predicate — nil restrictions add no join and
        // no DISTINCT, and this route can never reach the `update_at_asc` sort or the
        // `UpdatedAfter` filter (neither has a query parameter on `GET /users`).
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE ($3::bool IS NULL
                    OR ($3 AND u.deleteat != 0)
                    OR (NOT $3 AND u.deleteat = 0))
             ORDER BY u.username ASC
             OFFSET $1 LIMIT $2
            "#,
            offset_of(page, per_page),
            per_page,
            deleted,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get User profiles".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    async fn get_profiles_in_team(
        &self,
        team_id: &str,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> Result<Vec<User>, StoreError> {
        // `Join("TeamMembers tm ON ( tm.UserId = Users.Id AND tm.DeleteAt = 0 )")` plus
        // `Where("tm.TeamId = ?")`. The `tm.DeleteAt = 0` lives in the **join condition** and
        // the team id in the WHERE — moving either changes nothing here, but the join is an
        // INNER one, so a user who left the team is excluded by the membership row, not by
        // `Users.DeleteAt`.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              JOIN teammembers tm ON (tm.userid = u.id AND tm.deleteat = 0)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE tm.teamid = $1
               AND ($4::bool IS NULL
                    OR ($4 AND u.deleteat != 0)
                    OR (NOT $4 AND u.deleteat = 0))
             ORDER BY u.username ASC
             OFFSET $2 LIMIT $3
            "#,
            team_id,
            offset_of(page, per_page),
            per_page,
            deleted,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, found))]
    async fn get_profiles_in_channel(
        &self,
        channel_id: &str,
        page: i64,
        per_page: i64,
        deleted: Option<bool>,
    ) -> Result<Vec<User>, StoreError> {
        // `Join("ChannelMembers cm ON ( cm.UserId = Users.Id )")` — and note what is *not* here:
        // `ChannelMembers` has no `DeleteAt` column, so unlike the team query there is no
        // membership-deletion condition to forget. The active/inactive block is the plain
        // `if/else if` (its `&& !Active` variant belongs to the `sort=status`/`sort=admin`
        // siblings, which this port forwards).
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              JOIN channelmembers cm ON (cm.userid = u.id)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE cm.channelid = $1
               AND ($4::bool IS NULL
                    OR ($4 AND u.deleteat != 0)
                    OR (NOT $4 AND u.deleteat = 0))
             ORDER BY u.username ASC
             OFFSET $2 LIMIT $3
            "#,
            channel_id,
            offset_of(page, per_page),
            per_page,
            deleted,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, channel_id = %channel_id, found))]
    async fn get_profiles_not_in_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<User>, StoreError> {
        // An INNER join to the team and a LEFT join to the channel with `cm.UserId IS NULL` —
        // the anti-join. Both the team id and the channel id sit in **join conditions**, not in
        // the WHERE: moving `cm.ChannelId = ?` into the WHERE would turn the outer join into an
        // inner one and return the empty list for every caller.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              JOIN teammembers tm ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $1)
              LEFT JOIN channelmembers cm ON (cm.userid = u.id AND cm.channelid = $2)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE cm.userid IS NULL
             ORDER BY u.username ASC
             OFFSET $3 LIMIT $4
            "#,
            team_id,
            channel_id,
            offset,
            limit,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, found))]
    async fn get_profiles_not_in_team(
        &self,
        team_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<User>, StoreError> {
        // The anti-join again, this time against `TeamMembers`. `tm.DeleteAt = 0` in the join
        // condition means a user whose membership was soft-deleted counts as *not* in the team
        // and is listed — the opposite of what the same clause does in `get_profiles_in_team`,
        // where it excludes them.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN teammembers tm ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $1)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE tm.userid IS NULL
             ORDER BY u.username ASC
             OFFSET $2 LIMIT $3
            "#,
            team_id,
            offset,
            limit,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    async fn get_etag_for_profiles(&self, team_id: &str) -> String {
        // Go's `SELECT UpdateAt FROM Users, TeamMembers WHERE TeamMembers.TeamId = ? AND
        // Users.Id = TeamMembers.UserId ORDER BY UpdateAt DESC LIMIT 1` — an implicit join with
        // **no `TeamMembers.DeleteAt` condition**, so a member who left still moves this etag
        // even though `get_profiles_in_team` no longer lists them.
        let newest = sqlx::query_scalar!(
            r#"
            SELECT u.updateat
              FROM users u, teammembers tm
             WHERE tm.teamid = $1
               AND u.id = tm.userid
             ORDER BY u.updateat DESC
             LIMIT 1
            "#,
            team_id,
        )
        .fetch_optional(&self.pool)
        .await;

        match newest {
            // A missing row and a NULL `UpdateAt` are both errors for Go's `Get` into an
            // `int64`, and both land on the millisecond fallback.
            Ok(Some(Some(update_at))) => format!("{CURRENT_VERSION}.{update_at}"),
            Ok(_) => format!("{CURRENT_VERSION}.{}", mm_model::utils::get_millis()),
            Err(err) => {
                tracing::warn!(error = %err, "profiles etag query failed; using the clock");
                format!("{CURRENT_VERSION}.{}", mm_model::utils::get_millis())
            }
        }
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id))]
    async fn get_etag_for_profiles_not_in_team(&self, team_id: &str) -> String {
        let etag = sqlx::query_scalar!(
            r#"
            SELECT CONCAT(MAX(u.updateat), '.', COUNT(u.id)) AS etag
              FROM users u
              LEFT JOIN teammembers tm
                ON tm.userid = u.id
               AND tm.teamid = $1
               AND tm.deleteat = 0
             WHERE tm.userid IS NULL
            "#,
            team_id,
        )
        .fetch_one(&self.pool)
        .await;

        match etag {
            // `CONCAT` is null-tolerant in Postgres, so the aggregate always produces a string;
            // an empty result set is the literal `.0`, not the clock.
            Ok(Some(etag)) => format!("{CURRENT_VERSION}.{etag}"),
            Ok(None) => format!("{CURRENT_VERSION}.{}", mm_model::utils::get_millis()),
            Err(err) => {
                tracing::warn!(error = %err, "not-in-team etag query failed; using the clock");
                format!("{CURRENT_VERSION}.{}", mm_model::utils::get_millis())
            }
        }
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, terms, found))]
    async fn search(
        &self,
        team_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> Result<Vec<User>, StoreError> {
        let terms = search_terms(term);
        tracing::Span::current().record("terms", terms.len());

        // `usersQuery.OrderBy("Username ASC").Limit(...)`, plus the `TeamMembers` join when a
        // team is given. Written as a LEFT JOIN with `tm.UserId IS NOT NULL` rather than Go's
        // INNER JOIN so that the no-team case is the *same statement* — `teamid = ''` matches
        // nothing, so the guard is what admits everyone, and one statement means one place for
        // the thirty columns to be right.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN teammembers tm
                ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $1)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE ($1 = '' OR tm.userid IS NOT NULL)
               AND ($5 OR u.deleteat = 0)
               AND NOT EXISTS (
                     SELECT 1
                       FROM unnest($2::text[]) AS s(term)
                      WHERE NOT (
                                lower(u.username) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($3 AND lower(u.firstname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR ($3 AND lower(u.lastname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR lower(u.nickname) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($6 AND lower(u.email) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR u.id = s.term
                            )
                   )
             ORDER BY u.username ASC
             LIMIT $4
            "#,
            team_id,
            &terms,
            options.allow_full_names,
            options.limit,
            options.allow_inactive,
            options.allow_emails,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Users with term={term}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(channel_id = %channel_id, terms, found))]
    async fn search_in_channel(
        &self,
        channel_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> Result<Vec<User>, StoreError> {
        let terms = search_terms(term);
        tracing::Span::current().record("terms", terms.len());

        // No team join and no `TeamMembers` at all — `SearchInChannel` takes no team id, so a
        // channel member who has since left the team is still listed. The channel id lives in
        // the join condition exactly as it does in `get_profiles_in_channel`.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              JOIN channelmembers cm ON (cm.userid = u.id AND cm.channelid = $1)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.deleteat = 0
               AND NOT EXISTS (
                     SELECT 1
                       FROM unnest($2::text[]) AS s(term)
                      WHERE NOT (
                                lower(u.username) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($3 AND lower(u.firstname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR ($3 AND lower(u.lastname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR lower(u.nickname) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR u.id = s.term
                            )
                   )
             ORDER BY u.username ASC
             LIMIT $4
            "#,
            channel_id,
            &terms,
            options.allow_full_names,
            options.limit,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Users with term={term}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    #[tracing::instrument(skip_all, fields(team_id = %team_id, channel_id = %channel_id, terms, found))]
    async fn search_not_in_channel(
        &self,
        team_id: &str,
        channel_id: &str,
        term: &str,
        options: &UserSearchOptions,
    ) -> Result<Vec<User>, StoreError> {
        let terms = search_terms(term);
        tracing::Span::current().record("terms", terms.len());

        // The anti-join: `cm.UserId IS NULL` against a **LEFT** join whose channel id sits in
        // the join condition. Moving `cm.channelid = $2` into the WHERE turns it into an inner
        // join and the result is empty for everyone — the same trap `get_profiles_not_in_channel`
        // documents, and the reason both queries spell the condition where they do.
        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN teammembers tm
                ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $1)
              LEFT JOIN channelmembers cm ON (cm.userid = u.id AND cm.channelid = $2)
              LEFT JOIN bots b ON b.userid = u.id
             WHERE cm.userid IS NULL
               AND ($1 = '' OR tm.userid IS NOT NULL)
               AND ($6 OR u.deleteat = 0)
               AND NOT EXISTS (
                     SELECT 1
                       FROM unnest($3::text[]) AS s(term)
                      WHERE NOT (
                                lower(u.username) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($4 AND lower(u.firstname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR ($4 AND lower(u.lastname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR lower(u.nickname) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($7 AND lower(u.email) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR u.id = s.term
                            )
                   )
             ORDER BY u.username ASC
             LIMIT $5
            "#,
            team_id,
            channel_id,
            &terms,
            options.allow_full_names,
            options.limit,
            options.allow_inactive,
            options.allow_emails,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find Users with term={term}"),
            source,
        })?;
        tracing::Span::current().record("found", rows.len());

        rows.into_iter().map(user_from_row).collect()
    }

    /// # The thirteen fields the caller does not get to change
    ///
    /// Go reads the stored row and copies `CreateAt`, `AuthData`, `AuthService`, `Password`,
    /// `LastPasswordUpdate`, `LastPictureUpdate`, `EmailVerified`, `FailedAttempts`, `MfaSecret`,
    /// `MfaActive`, `MfaUsedTimestamps`, `LastLogin` and `RemoteId` **onto the submitted user**
    /// before writing. That is the security boundary of every update route: without it a client
    /// could set its own password hash, mark its own email verified, clear its own failed-login
    /// count, or turn off its own MFA by putting the field in the request body.
    ///
    /// With `trusted_update_data` false — which is every api4 caller — `Roles` and `DeleteAt`
    /// join that list, so no update route can grant itself a role or un-deactivate an account.
    ///
    /// # Three things the untrusted path does that the trusted one does not
    ///
    /// An OAuth user's email is pinned to the stored one. An LDAP user changing username or email
    /// is **refused**, not ignored — two different `ErrInvalidInput`s. And any other email change
    /// clears `EmailVerified`, which is what makes the verification mail meaningful.
    ///
    /// # And one both paths do
    ///
    /// `IsSSOUser` forces `EmailVerified` true — Go calls it "a lazy migration to fix broken
    /// records", and it runs *after* the untrusted path may have cleared it.
    #[tracing::instrument(skip_all, fields(user_id = %user.id, trusted = trusted_update_data))]
    async fn update(
        &self,
        user: &User,
        trusted_update_data: bool,
    ) -> Result<UserUpdate, StoreError> {
        let mut user = user.clone();
        user.pre_update();

        if let Err(app_error) = user.is_valid() {
            return Err(StoreError::Invalid {
                entity: "User",
                app_error,
            });
        }

        if let Some(notify_props) = user.notify_props.as_ref() {
            let max = self.max_post_size().await?;
            let message = notify_props
                .get(mm_model::user::AUTO_RESPONDER_MESSAGE_NOTIFY_PROP)
                .map(String::as_str)
                .unwrap_or_default();
            if message.chars().count() as i64 > max {
                return Err(StoreError::InvalidInput {
                    entity: "User",
                    field: "auto_responder_message",
                    value: "Auto responder message size can't be more than the allowed Post size"
                        .to_owned(),
                });
            }
        }

        // Go's `oldUser.Id == ""` check: `GetBuilder` into a zero struct leaves the id empty when
        // nothing matched. `get` raises `NotFound` for the same case, so the two are folded.
        let old_user = match self.get(&user.id).await {
            Ok(old_user) => old_user,
            Err(err) if err.is_not_found() => {
                return Err(StoreError::InvalidInput {
                    entity: "User",
                    field: "id",
                    value: user.id.clone(),
                });
            }
            Err(err) => return Err(err),
        };

        user.create_at = old_user.create_at;
        user.auth_data = old_user.auth_data.clone();
        user.auth_service = old_user.auth_service.clone();
        user.password = old_user.password.clone();
        user.last_password_update = old_user.last_password_update;
        user.last_picture_update = old_user.last_picture_update;
        user.email_verified = old_user.email_verified;
        user.failed_attempts = old_user.failed_attempts;
        user.mfa_secret = old_user.mfa_secret.clone();
        user.mfa_active = old_user.mfa_active;
        user.mfa_used_timestamps = old_user.mfa_used_timestamps.clone();
        user.last_login = old_user.last_login;
        user.remote_id = old_user.remote_id.clone();

        if !trusted_update_data {
            user.roles = old_user.roles.clone();
            user.delete_at = old_user.delete_at;

            if user.is_oauth_user() {
                user.email = old_user.email.clone();
            }

            if user.is_ldap_user() {
                if user.username != old_user.username {
                    return Err(StoreError::InvalidInput {
                        entity: "User",
                        field: "id",
                        value: user.id.clone(),
                    });
                }
                if user.email != old_user.email {
                    return Err(StoreError::InvalidInput {
                        entity: "User",
                        field: "email",
                        value: user.id.clone(),
                    });
                }
            }

            if user.email != old_user.email {
                user.email_verified = false;
            }
        }

        // "In the past, changing the email of a SSO user would mark the email as unverified.
        // This is a lazy migration to fix broken records." — and it runs after the clear above.
        if user.is_sso_user() {
            user.email_verified = true;
        }

        if user.username != old_user.username {
            user.update_mention_keys_from_username(&old_user.username);
        }

        let props = json_or_null(user.props.as_ref(), "props")?;
        let notify_props = json_or_null(user.notify_props.as_ref(), "notifyprops")?;
        let timezone = json_or_null(user.timezone.as_ref(), "timezone")?;
        let mfa_used_timestamps = match user.mfa_used_timestamps.as_ref() {
            None => None,
            Some(value) => {
                Some(
                    serde_json::to_value(value).map_err(|source| StoreError::Decode {
                        entity: "User",
                        column: "mfausedtimestamps",
                        source,
                    })?,
                )
            }
        };

        // The column list is Go's, verbatim and in its order. `Id` is the only column of the
        // table that is **not** here: it is the key.
        let affected = sqlx::query!(
            r#"
            UPDATE users
               SET createat = $2, updateat = $3, deleteat = $4, username = $5, password = $6,
                   authdata = $7, authservice = $8, email = $9, emailverified = $10,
                   nickname = $11, firstname = $12, lastname = $13, position = $14, roles = $15,
                   allowmarketing = $16, props = $17, notifyprops = $18,
                   lastpasswordupdate = $19, lastpictureupdate = $20,
                   failedattempts = $21, locale = $22, timezone = $23, mfaactive = $24,
                   mfasecret = $25, remoteid = $26, lastlogin = $27, mfausedtimestamps = $28
             WHERE id = $1
            "#,
            user.id,
            user.create_at,
            user.update_at,
            user.delete_at,
            user.username,
            user.password,
            user.auth_data,
            user.auth_service,
            user.email,
            user.email_verified,
            user.nickname,
            user.first_name,
            user.last_name,
            user.position,
            user.roles,
            user.allow_marketing,
            props,
            notify_props,
            user.last_password_update,
            user.last_picture_update,
            user.failed_attempts as i32,
            user.locale,
            timezone,
            user.mfa_active,
            user.mfa_secret,
            user.remote_id,
            user.last_login,
            mfa_used_timestamps,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| match unique_constraint(&source) {
            // Go tests the constraint names in this order, so a violation matching neither is an
            // ordinary wrapped error rather than a conflict.
            Some(resource) => StoreError::Conflict { resource, source },
            None => StoreError::Db {
                context: format!("failed to update User with userId={}", user.id),
                source,
            },
        })?
        .rows_affected();

        if affected > 1 {
            return Err(StoreError::Db {
                context: format!(
                    "multiple users were update: userId={}, count={affected}",
                    user.id
                ),
                source: sqlx::Error::RowNotFound,
            });
        }

        // Both halves are sanitized, and `Old` is what the caller compares against — so a caller
        // that logged the pair cannot leak a password hash through either.
        let mut new_user = user;
        let mut old_user = old_user;
        new_user.sanitize(&std::collections::HashMap::new());
        old_user.sanitize(&std::collections::HashMap::new());

        Ok(UserUpdate {
            old: old_user,
            new: new_user,
        })
    }

    #[tracing::instrument(skip_all, fields(max))]
    async fn max_post_size(&self) -> Result<i64, StoreError> {
        let bytes: i64 = sqlx::query_scalar!(
            r#"
            SELECT COALESCE(character_maximum_length, 0)::bigint AS "length!"
              FROM information_schema.columns
             WHERE table_name = 'posts' AND column_name = 'message'
            "#,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "Unable to determine the maximum supported post size".to_owned(),
            source,
        })?
        .unwrap_or(0);

        // "Assume a worst-case representation of four bytes per rune" — and the floor is
        // `PostMessageMaxRunesV2`, so a failed query does not make every message invalid the way
        // `max_draft_size` does. That asymmetry is Go's.
        let max = (bytes / 4).max(mm_model::post::POST_MESSAGE_MAX_RUNES_V2 as i64);
        tracing::Span::current().record("max", max);
        Ok(max)
    }

    #[tracing::instrument(skip_all, fields(name = %name, exists))]
    async fn group_name_exists(&self, name: &str) -> Result<bool, StoreError> {
        let exists: bool = sqlx::query_scalar!(
            r#"SELECT EXISTS(SELECT 1 FROM usergroups WHERE name = $1) AS "exists!""#,
            name,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to get Group with name={name}"),
            source,
        })?;

        tracing::Span::current().record("exists", exists);
        Ok(exists)
    }

    #[tracing::instrument(skip_all, fields(sort_column = %options.base.sort_column, found))]
    async fn get_user_report(
        &self,
        options: &mm_model::report::UserReportOptions,
    ) -> Result<Vec<mm_model::report::UserReportQuery>, StoreError> {
        let filter = ReportFilter::from_options(options)?;
        let (start_day, end_day) = report_date_bounds(options);

        // Go builds nine independent `squirrel` fragments here; this is the same query with each
        // fragment turned into a guarded predicate, which is what the rest of this store does
        // (see [`UserStore::count`]). Two lateral joins carry the parts that are needed twice:
        // `k` holds the sort key, whose *column* is chosen by a parameter and cannot be one, and
        // `cc` the channel count, which is both a selected column and the guest filter's subject.
        let rows = sqlx::query_as!(
            UserReportRow,
            r#"
            SELECT data.id AS "id!",
                   data.createat,
                   data.updateat,
                   data.deleteat,
                   data.username,
                   data.password,
                   data.authdata,
                   data.authservice,
                   data.email,
                   data.emailverified,
                   data.nickname,
                   data.firstname,
                   data.lastname,
                   data.position,
                   data.roles,
                   data.allowmarketing,
                   data.props,
                   data.notifyprops,
                   data.lastpasswordupdate,
                   data.lastpictureupdate,
                   data.failedattempts,
                   data.locale,
                   data.timezone,
                   data.mfaactive,
                   data.mfasecret,
                   data.mfausedtimestamps,
                   data.remoteid,
                   data.lastlogin AS "lastlogin!",
                   data.laststatusat,
                   data.lastpostdate,
                   data.daysactive,
                   data.totalposts,
                   data.channelcount,
                   data.teams AS "teams!"
              FROM (
                SELECT u.id,
                       u.createat,
                       u.updateat,
                       u.deleteat,
                       u.username,
                       u.password,
                       u.authdata,
                       u.authservice,
                       u.email,
                       u.emailverified,
                       u.nickname,
                       u.firstname,
                       u.lastname,
                       u.position,
                       u.roles,
                       u.allowmarketing,
                       u.props,
                       u.notifyprops,
                       u.lastpasswordupdate,
                       u.lastpictureupdate,
                       u.failedattempts::bigint AS failedattempts,
                       u.locale,
                       u.timezone,
                       u.mfaactive,
                       u.mfasecret,
                       u.mfausedtimestamps,
                       u.remoteid,
                       u.lastlogin,
                       MAX(s.lastactivityat) AS laststatusat,
                       MAX(ps.lastpostdate) AS lastpostdate,
                       COUNT(ps.day) AS daysactive,
                       SUM(ps.numposts)::bigint AS totalposts,
                       cc.n AS channelcount,
                       COALESCE((SELECT string_agg(t.displayname, ', ' ORDER BY t.displayname)
                                   FROM teammembers tmt
                                   INNER JOIN teams t ON t.id = tmt.teamid AND t.deleteat = 0
                                  WHERE tmt.userid = u.id AND tmt.deleteat = 0), '') AS teams,
                       k.k_num,
                       k.k_txt
                  FROM users u
                  CROSS JOIN LATERAL (
                    SELECT (CASE WHEN $1 = 'CreateAt' THEN u.createat END) AS k_num,
                           (CASE $1 WHEN 'Username'  THEN u.username
                                    WHEN 'FirstName' THEN u.firstname
                                    WHEN 'LastName'  THEN u.lastname
                                    WHEN 'Nickname'  THEN u.nickname
                                    WHEN 'Email'     THEN u.email
                                    WHEN 'Roles'     THEN u.roles END) AS k_txt
                  ) k
                  CROSS JOIN LATERAL (
                    SELECT COUNT(*) AS n
                      FROM channelmembers cm
                      INNER JOIN channels c
                        ON c.id = cm.channelid AND c.deleteat = 0 AND c.type IN ('O', 'P')
                     WHERE cm.userid = u.id
                  ) cc
                  LEFT JOIN status s ON s.userid = u.id
                  LEFT JOIN poststats ps
                    ON ps.userid = u.id
                   AND ($2::date IS NULL OR ps.day >= $2)
                   AND ($3::date IS NULL OR ps.day < $3)
                  LEFT JOIN teammembers tm
                    ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $4)
                 WHERE u.id NOT IN (SELECT userid FROM bots)
                   AND ($4 = '' OR tm.userid IS NOT NULL)
                   AND (NOT $5 OR u.id NOT IN (SELECT userid FROM teammembers WHERE deleteat = 0))
                   AND (NOT $6 OR u.deleteat > 0)
                   AND (NOT $7 OR u.deleteat = 0)
                   AND ($8::text IS NULL OR u.roles LIKE LOWER($8))
                   AND ($9 = 0 OR ($9 = 1 AND cc.n = 1) OR ($9 = 2 AND cc.n > 1))
                   AND NOT EXISTS (
                         SELECT 1
                           FROM unnest($10::text[]) AS srch(term)
                          WHERE NOT (
                                    lower(u.username)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                                 OR lower(u.firstname) LIKE lower('%' || srch.term || '%') ESCAPE '*'
                                 OR lower(u.lastname)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                                 OR lower(u.nickname)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                                 OR lower(u.email)     LIKE lower('%' || srch.term || '%') ESCAPE '*'
                                 OR u.id = srch.term
                                )
                       )
                   AND (NOT $11
                        OR ($12 AND (CASE WHEN $13 THEN (k.k_num, u.id) < ($14, $15)
                                          ELSE (k.k_num, u.id) > ($14, $15) END))
                        OR (NOT $12 AND (CASE WHEN $13 THEN (k.k_txt, u.id) < ($16, $15)
                                              ELSE (k.k_txt, u.id) > ($16, $15) END)))
                 GROUP BY u.id, cc.n, k.k_num, k.k_txt
                 ORDER BY (CASE WHEN NOT $13 THEN k.k_num END) ASC,
                          (CASE WHEN NOT $13 THEN k.k_txt END) ASC,
                          (CASE WHEN $13 THEN k.k_num END) DESC,
                          (CASE WHEN $13 THEN k.k_txt END) DESC,
                          u.id ASC
                 LIMIT (CASE WHEN $17::bigint > 0 THEN $17::bigint END)
              ) data
             ORDER BY (CASE WHEN NOT $18 THEN data.k_num END) ASC,
                      (CASE WHEN NOT $18 THEN data.k_txt END) ASC,
                      (CASE WHEN $18 THEN data.k_num END) DESC,
                      (CASE WHEN $18 THEN data.k_txt END) DESC,
                      data.id ASC
            "#,
            options.base.sort_column,
            start_day,
            end_day,
            filter.team_id,
            filter.has_no_team,
            filter.hide_active,
            filter.hide_inactive,
            filter.role_like,
            filter.guest_channel_mode,
            &filter.terms,
            filter.use_cursor,
            filter.sort_is_numeric,
            filter.sort_desc,
            filter.cursor_num,
            options.base.from_id,
            options.base.from_column_value,
            options.base.page_size,
            filter.outer_desc,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to get users for reporting".to_string(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter().map(UserReportRow::into_query).collect()
    }

    #[tracing::instrument(skip_all, fields(count))]
    async fn get_user_count_for_report(
        &self,
        options: &mm_model::report::UserReportOptions,
    ) -> Result<i64, StoreError> {
        let filter = ReportFilter::from_options(options)?;

        let count = sqlx::query_scalar!(
            r#"
            SELECT COUNT(u.id) AS "count!"
              FROM users u
              CROSS JOIN LATERAL (
                SELECT COUNT(*) AS n
                  FROM channelmembers cm
                  INNER JOIN channels c
                    ON c.id = cm.channelid AND c.deleteat = 0 AND c.type IN ('O', 'P')
                 WHERE cm.userid = u.id
              ) cc
              LEFT JOIN bots b ON u.id = b.userid
              LEFT JOIN teammembers tm
                ON (tm.userid = u.id AND tm.deleteat = 0 AND tm.teamid = $1)
             WHERE b.userid IS NULL
               AND ($1 = '' OR tm.userid IS NOT NULL)
               AND (NOT $2 OR u.id NOT IN (SELECT userid FROM teammembers WHERE deleteat = 0))
               AND (NOT $3 OR u.deleteat > 0)
               AND (NOT $4 OR u.deleteat = 0)
               AND ($5::text IS NULL OR u.roles LIKE LOWER($5))
               AND ($6 = 0 OR ($6 = 1 AND cc.n = 1) OR ($6 = 2 AND cc.n > 1))
               AND NOT EXISTS (
                     SELECT 1
                       FROM unnest($7::text[]) AS srch(term)
                      WHERE NOT (
                                lower(u.username)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                             OR lower(u.firstname) LIKE lower('%' || srch.term || '%') ESCAPE '*'
                             OR lower(u.lastname)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                             OR lower(u.nickname)  LIKE lower('%' || srch.term || '%') ESCAPE '*'
                             OR lower(u.email)     LIKE lower('%' || srch.term || '%') ESCAPE '*'
                             OR u.id = srch.term
                            )
                   )
            "#,
            filter.team_id,
            filter.has_no_team,
            filter.hide_active,
            filter.hide_inactive,
            filter.role_like,
            filter.guest_channel_mode,
            &filter.terms,
        )
        .fetch_one(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to count Users for report".to_string(),
            source,
        })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    #[tracing::instrument(skip_all, fields(found))]
    async fn get_by_auth_data(&self, auth_data: &str) -> Result<User, StoreError> {
        if auth_data.is_empty() {
            return Err(StoreError::InvalidInput {
                entity: "User",
                field: "<authData>",
                value: "empty or nil".to_owned(),
            });
        }

        let row = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE u.authdata = $1
            "#,
            auth_data,
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to find User with authData={auth_data}"),
            source,
        })?;

        tracing::Span::current().record("found", row.is_some());
        match row {
            Some(row) => user_from_row(row),
            None => Err(StoreError::NotFound {
                entity: "User",
                criteria: format!("authData={auth_data}"),
            }),
        }
    }

    #[tracing::instrument(skip_all, fields(page, per_page, domains, found))]
    async fn get_users_with_invalid_emails(
        &self,
        page: i64,
        per_page: i64,
        restricted_domains: &str,
    ) -> Result<Vec<User>, StoreError> {
        // `strings.Split(restrictedDomains, ",")`, with the empty pieces dropped by the loop's
        // own `if d != ""`. **Nothing is trimmed**: a configured `"a.com, b.com"` yields
        // `" b.com"`, whose `LIKE '% b.com%'` matches no address.
        let domains: Vec<String> = restricted_domains
            .split(',')
            .filter(|domain| !domain.is_empty())
            .map(str::to_owned)
            .collect();
        tracing::Span::current().record("page", page);
        tracing::Span::current().record("per_page", per_page);
        tracing::Span::current().record("domains", domains.len());

        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE b.userid IS NULL
               AND u.roles <> 'system_guest'
               AND u.deleteat = 0
               AND (u.authservice = '' OR u.authservice IS NULL)
               AND (cardinality($3::text[]) = 0
                    OR (u.email IS NOT NULL
                        AND NOT EXISTS (
                              SELECT 1
                                FROM unnest($3::text[]) AS d(domain)
                               WHERE u.email LIKE LOWER('%' || d.domain || '%')
                            )))
             OFFSET $1
             LIMIT $2
            "#,
            page * per_page,
            per_page,
            &domains,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users with invalid emails".to_owned(),
            source,
        })?;

        tracing::Span::current().record("found", rows.len());
        rows.into_iter()
            .map(|row| {
                let mut user = user_from_row(row)?;
                // `u.Sanitize(map[string]bool{})` — the **empty** options map, which
                // `ClearNonProfileFields` is not: it blanks the password, the MFA secret and
                // `LastLogin`, and then, because the map is empty, leaves the email and the auth
                // fields alone. Blanking the email here would empty the one column the route
                // exists to show.
                user.sanitize(&std::collections::HashMap::new());
                Ok(user)
            })
            .collect()
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, updated))]
    async fn update_password(
        &self,
        user_id: &str,
        hashed_password: &str,
    ) -> Result<(), StoreError> {
        // One `GetMillis()` feeding both columns, as Go reads it once into `updateAt`.
        let update_at = mm_model::utils::get_millis();

        let result = sqlx::query!(
            "UPDATE users
                SET password = $1,
                    lastpasswordupdate = $2,
                    updateat = $2,
                    authdata = NULL,
                    authservice = '',
                    failedattempts = 0
              WHERE id = $3",
            hashed_password,
            update_at,
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update User with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(by_username, by_email, found))]
    async fn get_for_login(
        &self,
        login_id: &str,
        allow_sign_in_with_username: bool,
        allow_sign_in_with_email: bool,
    ) -> Result<User, StoreError> {
        tracing::Span::current().record("by_username", allow_sign_in_with_username);
        tracing::Span::current().record("by_email", allow_sign_in_with_email);

        // Go builds one query with a squirrel `Where` per arm; sqlx's compile-time checking wants
        // three literal statements. The predicate is the only thing that differs, so the three
        // are folded into one statement guarded by the flags themselves: `$2`/`$3` carry them
        // into SQL rather than into Rust. That keeps a single checked query *and* keeps the
        // three-way choice in one place — a reader changing the `OR` cannot forget a copy.
        //
        // `lower($1)` on the **parameter**, as Go writes it.
        if !allow_sign_in_with_username && !allow_sign_in_with_email {
            return Err(StoreError::Argument {
                entity: "User",
                detail: "sign in with username and email are disabled",
            });
        }

        let rows = sqlx::query_as!(
            UserRow,
            r#"
            SELECT u.id,
                   u.createat,
                   u.updateat,
                   u.deleteat,
                   u.username,
                   u.password,
                   u.authdata,
                   u.authservice,
                   u.email,
                   u.emailverified,
                   u.nickname,
                   u.firstname,
                   u.lastname,
                   u.position,
                   u.roles,
                   u.allowmarketing,
                   u.props,
                   u.notifyprops,
                   u.lastpasswordupdate,
                   u.lastpictureupdate,
                   u.failedattempts::bigint AS failedattempts,
                   u.locale,
                   u.timezone,
                   u.mfaactive,
                   u.mfasecret,
                   u.mfausedtimestamps,
                   u.remoteid,
                   u.lastlogin,
                   (b.userid IS NOT NULL) AS "isbot!",
                   COALESCE(b.description, '') AS "botdescription!",
                   COALESCE(b.lasticonupdate, 0) AS "botlasticonupdate!"
              FROM users u
              LEFT JOIN bots b ON b.userid = u.id
             WHERE ($2 AND u.username = lower($1))
                OR ($3 AND u.email = lower($1))
            "#,
            login_id,
            allow_sign_in_with_username,
            allow_sign_in_with_email,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: "failed to find Users".to_owned(),
            source,
        })?;

        // Go's two refusals, kept apart. The criteria never carries `login_id`: it is a
        // credential-adjacent value and this error is logged.
        if rows.is_empty() {
            tracing::Span::current().record("found", 0);
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: "user not found".to_owned(),
            });
        }
        if rows.len() > 1 {
            tracing::Span::current().record("found", rows.len());
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: "multiple users found".to_owned(),
            });
        }
        tracing::Span::current().record("found", 1);

        let Some(row) = rows.into_iter().next() else {
            // Unreachable: `rows` is neither empty nor longer than one here.
            return Err(StoreError::NotFound {
                entity: "User",
                criteria: "user not found".to_owned(),
            });
        };
        user_from_row(row)
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, last_login, updated))]
    async fn update_last_login(&self, user_id: &str, last_login: i64) -> Result<(), StoreError> {
        // `Set("UpdateAt", model.GetMillis())` — read here and **not** from `last_login`, so the
        // two columns legitimately differ. See the trait doc.
        let now = mm_model::utils::get_millis();
        let result = sqlx::query!(
            "UPDATE users SET lastlogin = $1, updateat = $2 WHERE id = $3",
            last_login,
            now,
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update User with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("last_login", last_login);
        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, attempts, updated))]
    async fn update_failed_password_attempts(
        &self,
        user_id: &str,
        attempts: i32,
    ) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE users SET failedattempts = $1 WHERE id = $2",
            attempts,
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update User with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("attempts", attempts);
        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, max_attempts, claimed))]
    async fn try_increment_failed_password_attempts(
        &self,
        user_id: &str,
        max_attempts: i32,
    ) -> Result<bool, StoreError> {
        let result = sqlx::query!(
            "UPDATE users
                SET failedattempts = failedattempts + 1
              WHERE id = $1 AND failedattempts < $2",
            user_id,
            max_attempts,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update User with userId={user_id}"),
            source,
        })?;

        // Go compares to exactly one. The primary key makes any other count impossible, and
        // writing `> 0` would quietly accept a future statement that matched more than one row.
        let claimed = result.rows_affected() == 1;
        tracing::Span::current().record("max_attempts", max_attempts);
        tracing::Span::current().record("claimed", claimed);
        Ok(claimed)
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, refunded))]
    async fn decrement_failed_password_attempts(&self, user_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE users
                SET failedattempts = failedattempts - 1
              WHERE id = $1 AND failedattempts > 0",
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update User with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("refunded", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, updated))]
    async fn verify_email(&self, user_id: &str, email: &str) -> Result<(), StoreError> {
        let result = sqlx::query!(
            "UPDATE users SET email = lower($1), emailverified = true, updateat = $2 WHERE id = $3",
            email,
            mm_model::utils::get_millis(),
            user_id,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| StoreError::Db {
            context: format!("failed to update Users with userId={user_id}"),
            source,
        })?;

        tracing::Span::current().record("updated", result.rows_affected());
        Ok(())
    }

    #[tracing::instrument(skip_all, fields(username = %user.username))]
    async fn save(
        &self,
        user: &User,
        hasher: &(dyn mm_model::user::UserPasswordHasher + Sync),
    ) -> Result<User, StoreError> {
        if !user.id.is_empty() && !user.is_remote() {
            return Err(StoreError::InvalidInput {
                entity: "User",
                field: "id",
                value: user.id.clone(),
            });
        }

        // Go mutates the caller's `*model.User` in place and returns the same pointer; the caller
        // then reads `user.Id` off it. A clone plus returning the clone gives the caller the same
        // value without the shared mutation.
        let mut user = user.clone();
        if let Err(app_error) = user.pre_save(hasher) {
            return Err(StoreError::Invalid {
                entity: "User",
                app_error,
            });
        }
        if let Err(app_error) = user.is_valid() {
            return Err(StoreError::Invalid {
                entity: "User",
                app_error,
            });
        }

        // `validateAutoResponderMessageSize`, the same guard `update` applies, and it runs inside
        // `insert` — i.e. **after** `IsValid`, so an over-long auto-responder on an otherwise
        // invalid user reports the other failure first.
        if let Some(notify_props) = user.notify_props.as_ref() {
            let max = self.max_post_size().await?;
            let message = notify_props
                .get(mm_model::user::AUTO_RESPONDER_MESSAGE_NOTIFY_PROP)
                .map(String::as_str)
                .unwrap_or_default();
            if message.chars().count() as i64 > max {
                return Err(StoreError::InvalidInput {
                    entity: "User",
                    field: "auto_responder_message",
                    value: "Auto responder message size can't be more than the allowed Post size"
                        .to_owned(),
                });
            }
        }

        let props = json_or_null(user.props.as_ref(), "props")?;
        let notify_props = json_or_null(user.notify_props.as_ref(), "notifyprops")?;
        let timezone = json_or_null(user.timezone.as_ref(), "timezone")?;
        let mfa_used_timestamps = match user.mfa_used_timestamps.as_ref() {
            None => None,
            Some(value) => {
                Some(
                    serde_json::to_value(value).map_err(|source| StoreError::Decode {
                        entity: "User",
                        column: "mfausedtimestamps",
                        source,
                    })?,
                )
            }
        };

        // Twenty-seven columns, Go's order. **`LastLogin` is not among them** — `update` writes
        // it and `insert` does not, so a freshly saved user carries the column default rather
        // than the struct's value.
        sqlx::query!(
            r#"
            INSERT INTO users
                (id, createat, updateat, deleteat, username, password, authdata, authservice,
                 email, emailverified, nickname, firstname, lastname, position, roles,
                 allowmarketing, props, notifyprops, lastpasswordupdate, lastpictureupdate,
                 failedattempts, locale, timezone, mfaactive, mfasecret, remoteid,
                 mfausedtimestamps)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17,
                    $18, $19, $20, $21, $22, $23, $24, $25, $26, $27)
            "#,
            user.id,
            user.create_at,
            user.update_at,
            user.delete_at,
            user.username,
            user.password,
            user.auth_data,
            user.auth_service,
            user.email,
            user.email_verified,
            user.nickname,
            user.first_name,
            user.last_name,
            user.position,
            user.roles,
            user.allow_marketing,
            props,
            notify_props,
            user.last_password_update,
            user.last_picture_update,
            user.failed_attempts as i32,
            user.locale,
            timezone,
            user.mfa_active,
            user.mfa_secret,
            user.remote_id,
            mfa_used_timestamps,
        )
        .execute(&self.pool)
        .await
        .map_err(|source| match unique_constraint(&source) {
            // Go's two `IsUniqueConstraintError` calls, in its order: email first, username
            // second. The *field* is what `App.CreateBot` reads, and it is lower case there
            // while `Conflict`'s resource is capitalised — two spellings of the same constraint,
            // because two Go call sites spell it differently.
            Some("Email") => StoreError::InvalidInput {
                entity: "User",
                field: "email",
                value: user.email.clone(),
            },
            Some("Username") => StoreError::InvalidInput {
                entity: "User",
                field: "username",
                value: user.username.clone(),
            },
            _ => StoreError::Db {
                context: format!("failed to save User with userId={}", user.id),
                source,
            },
        })?;

        Ok(user)
    }

    #[tracing::instrument(skip_all, fields(user_id = %user_id, deleted))]
    async fn permanent_delete(&self, user_id: &str) -> Result<(), StoreError> {
        let result = sqlx::query!("DELETE FROM users WHERE id = $1", user_id)
            .execute(&self.pool)
            .await
            .map_err(|source| StoreError::Db {
                context: format!("failed to delete User with userId={user_id}"),
                source,
            })?;

        tracing::Span::current().record("deleted", result.rows_affected());
        Ok(())
    }
}

/// The parameters `applyUserReportFilter` (user_store.go:2448) and the cursor arithmetic above it
/// reduce to, computed once so the report and its count cannot drift apart.
struct ReportFilter {
    /// `applyRoleFilter`'s `LIKE` pattern, already wildcarded and escaped, or `None` for no role
    /// predicate. A guest filter **overrides** `Role` rather than combining with it.
    role_like: Option<String>,
    /// `0` no channel-count predicate, `1` exactly one channel, `2` more than one.
    guest_channel_mode: i32,
    has_no_team: bool,
    /// Empty when there is no team predicate — including when `has_no_team` is set, which Go
    /// treats as mutually exclusive with a team id rather than as an additional filter.
    team_id: String,
    hide_active: bool,
    hide_inactive: bool,
    terms: Vec<String>,
    use_cursor: bool,
    sort_is_numeric: bool,
    /// The direction the *database* sorts in, after Go's second, cursor-dependent assignment.
    sort_desc: bool,
    /// The direction the result is handed back in — reversed from [`Self::sort_desc`] for a
    /// `prev` page.
    outer_desc: bool,
    cursor_num: Option<i64>,
}

impl ReportFilter {
    fn from_options(options: &mm_model::report::UserReportOptions) -> Result<Self, StoreError> {
        use mm_model::report::{
            GUEST_FILTER_ALL, GUEST_FILTER_MULTIPLE_CHANNEL, GUEST_FILTER_SINGLE_CHANNEL,
        };

        let guest = options.guest_filter.as_str();
        // Go's `switch` reaches `applyRoleFilter(query, filter.Role)` only in the `default` arm,
        // so a guest filter silently discards `role_filter`.
        let role = match guest {
            GUEST_FILTER_ALL | GUEST_FILTER_SINGLE_CHANNEL | GUEST_FILTER_MULTIPLE_CHANNEL => {
                "system_guest"
            }
            _ => options.role.as_str(),
        };
        // `fmt.Sprintf("%%%s%%", sanitizeSearchTerm(role, "\\"))` — a **backslash** escape here,
        // not the `*` the search terms use, and no `ESCAPE` clause, so the pattern relies on
        // Postgres' default escape character being a backslash.
        let role_like =
            (!role.is_empty()).then(|| format!("%{}%", sanitize_search_term(role, '\\')));

        let guest_channel_mode = match guest {
            GUEST_FILTER_SINGLE_CHANNEL => 1,
            GUEST_FILTER_MULTIPLE_CHANNEL => 2,
            _ => 0,
        };

        let base = &options.base;
        let use_cursor = !base.from_id.is_empty() && !base.from_column_value.is_empty();
        let sort_desc = if use_cursor {
            (base.direction == "prev" && !base.sort_desc)
                || (base.direction == "next" && base.sort_desc)
        } else {
            base.sort_desc
        };
        let sort_is_numeric = base.sort_column == "CreateAt";

        // Go hands `FromColumnValue` to the driver as a string and lets Postgres coerce it to the
        // column's type, so a non-numeric cursor value on a `CreateAt` sort is a **failed query**
        // and a 500 — not an empty page. Parsing here reproduces the failure rather than
        // silently comparing against NULL, which would answer 200 with no rows.
        let cursor_num = if use_cursor && sort_is_numeric {
            Some(
                base.from_column_value
                    .parse::<i64>()
                    .map_err(|_| StoreError::Argument {
                        entity: "UserReport",
                        detail: "from_column_value is not an integer for a CreateAt sort",
                    })?,
            )
        } else {
            None
        };

        Ok(Self {
            role_like,
            guest_channel_mode,
            has_no_team: options.has_no_team,
            team_id: if options.has_no_team {
                String::new()
            } else {
                options.team.clone()
            },
            hide_active: options.hide_active,
            hide_inactive: options.hide_inactive,
            terms: search_terms(&options.search_term),
            use_cursor,
            sort_is_numeric,
            sort_desc,
            outer_desc: if base.direction == "prev" {
                !sort_desc
            } else {
                sort_desc
            },
            cursor_num,
        })
    }
}

/// `time.UnixMilli(filter.StartAt).Format("2006-01-02")`, and the same for `EndAt`.
///
/// **Local dates, not UTC.** `time.UnixMilli` returns a `time.Time` in `time.Local`, so the day
/// a boundary falls on depends on the server's zone — the same reason
/// `model::report::get_report_date_range` does its month arithmetic in `chrono::Local`. A zero
/// bound is Go's `if filter.StartAt > 0` guard: no predicate at all.
fn report_date_bounds(
    options: &mm_model::report::UserReportOptions,
) -> (Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) {
    use chrono::TimeZone;

    let day = |millis: i64| {
        (millis > 0)
            .then(|| chrono::Local.timestamp_millis_opt(millis).single())
            .flatten()
            .map(|t| t.date_naive())
    };
    (day(options.base.start_at), day(options.base.end_at))
}

/// One row of [`UserStore::get_user_report`] — `getUsersColumns()` plus the six report columns.
///
/// It repeats the twenty-eight user columns rather than reusing [`UserRow`] because this query
/// selects **no bot columns**: `getUsersColumns()` is used bare here, where `usersQuery` adds
/// `getBotInfoColumns()`. Bots are excluded by a `NOT IN` predicate instead, so `IsBot`,
/// `BotDescription` and `BotLastIconUpdate` are left at their zero values on every row — which
/// is what Go's `UserReportQuery` scan does too.
struct UserReportRow {
    id: String,
    createat: Option<i64>,
    updateat: Option<i64>,
    deleteat: Option<i64>,
    username: Option<String>,
    password: Option<String>,
    authdata: Option<String>,
    authservice: Option<String>,
    email: Option<String>,
    emailverified: Option<bool>,
    nickname: Option<String>,
    firstname: Option<String>,
    lastname: Option<String>,
    position: Option<String>,
    roles: Option<String>,
    allowmarketing: Option<bool>,
    props: Option<serde_json::Value>,
    notifyprops: Option<serde_json::Value>,
    lastpasswordupdate: Option<i64>,
    lastpictureupdate: Option<i64>,
    failedattempts: Option<i64>,
    locale: Option<String>,
    timezone: Option<serde_json::Value>,
    mfaactive: Option<bool>,
    mfasecret: Option<String>,
    mfausedtimestamps: Option<serde_json::Value>,
    remoteid: Option<String>,
    lastlogin: i64,
    laststatusat: Option<i64>,
    lastpostdate: Option<i64>,
    /// `COUNT(ps.Day)`, which is `0` and never NULL — so Go's `*int` is always non-nil and
    /// `days_active` is on the wire even for a user who has never posted.
    daysactive: Option<i64>,
    /// `SUM(ps.NumPosts)`, which **is** NULL for a user with no matching `PostStats` rows, and
    /// therefore the one aggregate that is omitted from the response.
    totalposts: Option<i64>,
    channelcount: Option<i64>,
    teams: String,
}

impl UserReportRow {
    fn into_query(self) -> Result<mm_model::report::UserReportQuery, StoreError> {
        let stats = mm_model::user::UserPostStats {
            last_status_at: self.laststatusat,
            last_post_date: self.lastpostdate,
            days_active: self.daysactive,
            total_posts: self.totalposts,
        };
        let channel_count = self.channelcount;
        let teams = self.teams;
        let user = user_from_row(UserRow {
            id: self.id,
            createat: self.createat,
            updateat: self.updateat,
            deleteat: self.deleteat,
            username: self.username,
            password: self.password,
            authdata: self.authdata,
            authservice: self.authservice,
            email: self.email,
            emailverified: self.emailverified,
            nickname: self.nickname,
            firstname: self.firstname,
            lastname: self.lastname,
            position: self.position,
            roles: self.roles,
            allowmarketing: self.allowmarketing,
            props: self.props,
            notifyprops: self.notifyprops,
            lastpasswordupdate: self.lastpasswordupdate,
            lastpictureupdate: self.lastpictureupdate,
            failedattempts: self.failedattempts,
            locale: self.locale,
            timezone: self.timezone,
            mfaactive: self.mfaactive,
            mfasecret: self.mfasecret,
            mfausedtimestamps: self.mfausedtimestamps,
            remoteid: self.remoteid,
            lastlogin: self.lastlogin,
            isbot: false,
            botdescription: String::new(),
            botlasticonupdate: 0,
        })?;

        Ok(mm_model::report::UserReportQuery {
            user,
            post_stats: stats,
            channel_count,
            teams,
        })
    }
}

/// A `StringMap`/`StringArray`/`Timezone` column, or SQL NULL when the model holds `None`.
///
/// Go writes `nil` maps as SQL NULL through `NamedExec`, and `user_from_row` already treats NULL
/// and JSON `null` alike on the way back — so round-tripping a user with no props does not
/// invent an empty object.
fn json_or_null<T: serde::Serialize>(
    value: Option<&T>,
    column: &'static str,
) -> Result<Option<serde_json::Value>, StoreError> {
    match value {
        None => Ok(None),
        Some(value) => Ok(Some(serde_json::to_value(value).map_err(|source| {
            StoreError::Decode {
                entity: "User",
                column,
                source,
            }
        })?)),
    }
}

/// Port of `IsUniqueConstraintError(err, []string{...})` for the two constraints
/// `SqlUserStore.Update` names.
///
/// Go matches on the **constraint name** and checks Email before Username, which matters only if
/// a statement could violate both — it cannot, since Postgres reports the first. Reproduced in
/// Go's order anyway.
fn unique_constraint(err: &sqlx::Error) -> Option<&'static str> {
    let constraint = err.as_database_error()?.constraint()?;
    for (name, resource) in [
        ("users_email_key", "Email"),
        ("idx_users_email_unique", "Email"),
        ("users_username_key", "Username"),
        ("idx_users_username_unique", "Username"),
    ] {
        if constraint == name {
            return Some(resource);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Go's block is `if Inactive { … } else if Active { … }`, so `Inactive` wins outright when
    /// both are asked for. Writing the arms the other way round passes every single-flag test.
    #[test]
    fn inactive_wins_over_active_and_neither_means_no_predicate() {
        assert_eq!(deleted_filter(false, false), None);
        assert_eq!(deleted_filter(true, false), Some(true), "deleted rows only");
        assert_eq!(deleted_filter(false, true), Some(false), "live rows only");
        assert_eq!(
            deleted_filter(true, true),
            Some(true),
            "the else-if never runs once Inactive is set"
        );
    }

    /// `page * per_page`, and an absurd page must not panic in a debug build — the api layer
    /// caps `per_page` at 200 but leaves `page` unbounded, exactly as Go's `Atoi` does.
    #[test]
    fn the_offset_is_the_product_and_saturates_instead_of_overflowing() {
        assert_eq!(offset_of(0, 60), 0);
        assert_eq!(offset_of(3, 60), 180);
        assert_eq!(offset_of(1, 0), 0, "per_page=0 pages nowhere");
        assert_eq!(offset_of(i64::MAX, 200), i64::MAX);
    }

    #[test]
    fn user_not_found_carries_the_id_go_puts_in_its_message() {
        let err = StoreError::NotFound {
            entity: "User",
            criteria: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
        };
        assert!(err.to_string().contains("y9i4er48tt8bukijy7i3u5y9ar"));
    }

    /// The escape character goes away first and the wildcards are escaped second. Reversing the
    /// two turns `%` into `**%`, and a caller could then never search for a literal per cent.
    #[test]
    fn the_search_term_sanitiser_strips_then_escapes() {
        assert_eq!(sanitize_search_term("bob", '*'), "bob");
        assert_eq!(sanitize_search_term("50%", '*'), "50*%");
        assert_eq!(sanitize_search_term("a_b", '*'), "a*_b");
        // Every `*` the caller sent is removed before anything is escaped, so nothing the
        // caller writes can become an escape sequence.
        assert_eq!(sanitize_search_term("a*b", '*'), "ab");
        assert_eq!(sanitize_search_term("*%", '*'), "*%");
        assert_eq!(sanitize_search_term("100%_x*", '*'), "100*%*_x");
    }

    /// `strings.Fields` then `strings.TrimLeft(term, "@")` — **all** leading at-signs, not one.
    #[test]
    fn the_terms_are_whitespace_split_and_lose_every_leading_at() {
        assert_eq!(search_terms("bob"), ["bob"]);
        assert_eq!(search_terms("@bob"), ["bob"]);
        assert_eq!(search_terms("@@bob"), ["bob"]);
        assert_eq!(search_terms("  bob   alice "), ["bob", "alice"]);
        // Trailing and interior at-signs survive: TrimLeft only.
        assert_eq!(search_terms("bob@"), ["bob@"]);
        assert_eq!(search_terms("a@b"), ["a@b"]);
    }

    /// `performSearch`'s `if strings.TrimSpace(term) != ""` guard is applied to the **sanitised**
    /// term, so a term made only of escape characters vanishes and the search predicate is
    /// dropped entirely — the query then returns the whole joined set.
    #[test]
    fn a_blank_or_escape_only_term_produces_no_predicate() {
        assert!(search_terms("").is_empty());
        assert!(search_terms("   ").is_empty());
        assert!(search_terms("*").is_empty(), "sanitising leaves nothing");
        assert!(search_terms("**").is_empty());
    }

    /// A term of nothing but at-signs is *not* blank before the trim, so Go keeps the clause and
    /// searches for the empty string — `LIKE '%%'`, which matches every row. Dropping the clause
    /// instead would give the same answer here and a different one for `@ bob`.
    #[test]
    fn a_term_of_only_at_signs_survives_as_the_empty_string() {
        assert_eq!(search_terms("@"), [""]);
        assert_eq!(search_terms("@ bob"), ["", "bob"]);
    }
}
