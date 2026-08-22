//! Port of `SqlUserStore` (channels/store/sqlstore/user_store.go), `Get`, `GetByUsername` and
//! `GetProfileByIds`.

use mm_model::user::User;
use mm_model::utils::{CURRENT_VERSION, StringArray, StringMap};
use sqlx::PgPool;

use crate::error::StoreError;

/// The subset of Go's `store.UserStore` (store/store.go:448-550) that is ported.
pub trait UserStore {
    /// Port of `SqlUserStore.Get` (user_store.go:609).
    fn get(&self, id: &str) -> impl std::future::Future<Output = Result<User, StoreError>> + Send;

    /// Port of `SqlUserStore.GetByUsername` (user_store.go:1402).
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
}

/// `model.UserSearchDefaultLimit` (model/user_search.go:7).
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
               AND u.deleteat = 0
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
            team_id,
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
               AND u.deleteat = 0
               AND NOT EXISTS (
                     SELECT 1
                       FROM unnest($3::text[]) AS s(term)
                      WHERE NOT (
                                lower(u.username) LIKE lower('%' || s.term || '%') ESCAPE '*'
                             OR ($4 AND lower(u.firstname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR ($4 AND lower(u.lastname) LIKE lower('%' || s.term || '%') ESCAPE '*')
                             OR lower(u.nickname) LIKE lower('%' || s.term || '%') ESCAPE '*'
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
