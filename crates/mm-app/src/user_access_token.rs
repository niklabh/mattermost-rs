//! Port of the four personal-access-token reads in `app/session.go` (:735, :880, :893, :905), plus
//! the lifetime-policy helper they share.
//!
//! # The sanitisation is here, not in the store
//!
//! Three of the four blank `token.Token` after fetching (session.go:887, :899, :918) — the store
//! selects the secret because `GetByToken` authenticates with it. Every route in `mm_api::tokens`
//! goes through these functions for exactly that reason, and the `sanitize` parameter on
//! [`App::get_user_access_token`] is Go's own: `getUserAccessToken` passes `true`, and the
//! token-*revocation* paths pass `false` because they need the secret to find the session.
//!
//! # The count route reads nothing when the policy is off
//!
//! `maxUserAccessTokenExpiry` returns `(0, false)` when
//! `ServiceSettings.MaximumPersonalAccessTokenLifetimeDays <= 0`, and `CountNonCompliantUserAccessTokens`
//! then returns **0 without touching the database**. Zero is the default, so that is the branch a
//! stock server takes — and it means the answer is `{"count":0}` even on an installation with
//! thousands of never-expiring tokens. Reproduced, because it is what a client sees.

use mm_model::user_access_token::UserAccessToken;
use mm_model::utils::{AppError, AppResult};
use mm_store::{StoreError, UserAccessTokenStore};

use crate::App;

/// Milliseconds in a day — Go's `24*60*60*1000`, spelled out at the one place it is used.
const MILLIS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

/// `revokeNonCompliantBatchLimit` (session.go:711) — rows fetched and deleted per statement.
const REVOKE_NON_COMPLIANT_BATCH_LIMIT: i64 = 1000;

/// `revokeNonCompliantMaxBatches` (session.go:712) — the cap on iterations of one revoke call.
///
/// The product is the real bound: a single call revokes at most a million tokens, and hitting the
/// cap is an error rather than a partial success the client can see.
const REVOKE_NON_COMPLIANT_MAX_BATCHES: usize = 1000;

impl App {
    /// Port of `App.maxUserAccessTokenExpiry` (session.go:716).
    ///
    /// Returns `None` when the policy is disabled — which is `<= 0`, **not** `== 0`: a negative
    /// setting disables it too, and reading the condition as an equality would turn a nonsense
    /// configuration into a window ending in the past.
    ///
    /// `now_millis` is a parameter rather than a call to the clock so the arithmetic is testable
    /// without one. Go reads `model.GetMillis()` at this point.
    pub fn max_user_access_token_expiry_at(&self, now_millis: i64) -> Option<i64> {
        max_expiry_at(
            self.config().maximum_personal_access_token_lifetime_days,
            now_millis,
        )
    }

    /// [`App::max_user_access_token_expiry_at`] against the current clock.
    pub fn max_user_access_token_expiry(&self) -> Option<i64> {
        self.max_user_access_token_expiry_at(mm_model::utils::get_millis())
    }

    /// Port of `App.GetUserAccessTokens` (session.go:880) — every token on the installation.
    ///
    /// Go multiplies `page * perPage` into an offset **here**, not in the store.
    #[tracing::instrument(skip_all, fields(page, per_page, found))]
    pub async fn get_user_access_tokens(
        &self,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<UserAccessToken>> {
        let mut tokens = self
            .store()
            .user_access_token()
            .get_all(page * per_page, per_page)
            .await
            .map_err(|err| {
                token_error(
                    "GetUserAccessTokens",
                    "app.user_access_token.get_all.app_error",
                    err,
                )
            })?;

        sanitize_all(&mut tokens);
        tracing::Span::current().record("found", tokens.len());
        Ok(tokens)
    }

    /// Port of `App.GetUserAccessTokensForUser` (session.go:893).
    ///
    /// **A different error id from its sibling** — `get_by_user`, not `get_all` — for the same
    /// class of failure. Both are 500s and a client can tell them apart.
    #[tracing::instrument(skip_all, fields(user_id = %user_id, page, per_page, found))]
    pub async fn get_user_access_tokens_for_user(
        &self,
        user_id: &str,
        page: i64,
        per_page: i64,
    ) -> AppResult<Vec<UserAccessToken>> {
        let mut tokens = self
            .store()
            .user_access_token()
            .get_by_user(user_id, page * per_page, per_page)
            .await
            .map_err(|err| {
                token_error(
                    "GetUserAccessTokensForUser",
                    "app.user_access_token.get_by_user.app_error",
                    err,
                )
            })?;

        sanitize_all(&mut tokens);
        tracing::Span::current().record("found", tokens.len());
        Ok(tokens)
    }

    /// Port of `App.GetUserAccessToken` (session.go:905).
    ///
    /// **Both branches carry `app.user_access_token.get_by_user.app_error`** — the *list*
    /// method's id on a single-token lookup — and differ only in status: 404 for a miss, 500 for
    /// anything else. Go's copy-paste, and it is on the wire.
    #[tracing::instrument(skip(self), fields(token_id = %token_id, sanitize))]
    pub async fn get_user_access_token(
        &self,
        token_id: &str,
        sanitize: bool,
    ) -> AppResult<UserAccessToken> {
        let mut token = self
            .store()
            .user_access_token()
            .get(token_id)
            .await
            .map_err(|err| {
                let status = if err.is_not_found() { 404 } else { 500 };
                if status == 500 {
                    tracing::error!(error = ?err, "user access token lookup failed");
                }
                AppError::boxed(
                    "GetUserAccessToken",
                    "app.user_access_token.get_by_user.app_error",
                    None,
                    String::new(),
                    status,
                )
            })?;

        if sanitize {
            token.token.clear();
        }
        Ok(token)
    }

    /// Port of `App.CountNonCompliantUserAccessTokens` (session.go:735).
    ///
    /// Returns `0` and reads nothing when the lifetime policy is off — see the module note.
    #[tracing::instrument(skip_all, fields(policy_enabled, count))]
    pub async fn count_non_compliant_user_access_tokens(&self) -> AppResult<i64> {
        let Some(max_expires_at) = self.max_user_access_token_expiry() else {
            tracing::Span::current().record("policy_enabled", false);
            tracing::Span::current().record("count", 0);
            return Ok(0);
        };
        tracing::Span::current().record("policy_enabled", true);

        let count = self
            .store()
            .user_access_token()
            .count_non_compliant_expiry(max_expires_at)
            .await
            .map_err(|err| {
                token_error(
                    "CountNonCompliantUserAccessTokens",
                    "app.user_access_token.count_non_compliant.app_error",
                    err,
                )
            })?;

        tracing::Span::current().record("count", count);
        Ok(count)
    }

    /// Port of `App.validateUserAccessTokenExpiry` (session.go:516).
    ///
    /// Three branches and they are not interchangeable:
    ///
    /// - `expires_at == 0` — never expires. Allowed **only** when no maximum is configured;
    ///   otherwise `expires_at_required`, because a configured lifetime implies tokens expire.
    /// - `expires_at <= now` — `expires_at_in_past`. Note `<=`: an expiry of exactly now is
    ///   rejected, matching `IsExpired`'s own `>=`.
    /// - `expires_at > now + maxDays` — `expires_at_too_far`, and this one carries a **param**
    ///   (`Days`) the other two do not.
    ///
    /// Every `where` is the string `"CreateUserAccessToken"`, including on the rotate path, which
    /// is Go's copy-paste; `where` is not on the wire.
    ///
    /// `now_millis` is a parameter for the same reason as in
    /// [`App::max_user_access_token_expiry_at`] — Go calls `model.GetMillis()` twice here, once
    /// per comparison, and a test that cannot pin the clock cannot pin the boundary.
    pub fn validate_user_access_token_expiry_at(
        &self,
        expires_at: i64,
        now_millis: i64,
    ) -> AppResult {
        validate_expiry(
            self.config().maximum_personal_access_token_lifetime_days,
            expires_at,
            now_millis,
        )
    }
    /// Port of `App.CreateUserAccessToken` (session.go:553).
    ///
    /// # The secret is minted here and **returned to the caller**
    ///
    /// `token.Token = model.NewId()` immediately before the insert, and the saved token is
    /// returned with the secret intact — no `sanitize` on this path. That is deliberate and it is
    /// the whole point of the route: this is the only response that ever carries the value, and
    /// every later read blanks it (see the module note). A port that sanitised here for symmetry
    /// would hand clients a credential they can never learn.
    ///
    /// # Bots bypass two gates, not one
    ///
    /// `!EnableUserAccessTokens && !user.IsBot` and `if !user.IsBot { validate… }` — so a bot
    /// mints a never-expiring token on a server where the feature is off and a lifetime policy is
    /// enforced. Go's comment says why: integrations that provision bots would break the moment
    /// an admin enabled enforcement.
    ///
    /// # The e-mail is not sent
    ///
    /// Go calls `SendUserAccessTokenAddedEmail` for a non-bot owner and only **logs** a failure,
    /// so it changes no response. There is no e-mail service in this port — see D-238 — and this
    /// route is therefore silent where Go writes to an inbox.
    #[tracing::instrument(skip_all, fields(user_id = %token.user_id, token_id))]
    pub async fn create_user_access_token(
        &self,
        mut token: UserAccessToken,
    ) -> AppResult<UserAccessToken> {
        // `userService.GetUser` — `MissingAccountError` at **404** for a miss and
        // `app.user.get.app_error` at 500 for anything else, which is exactly the split
        // [`App::get_user`] already makes. Propagated unchanged rather than rebuilt with this
        // function's name: `where` is `json:"-"` and the handler overwrites it with the request
        // path, so the only thing a rebuild could change is invisible.
        let user = self.get_user(&token.user_id).await?;

        if !self.config().enable_user_access_tokens && !user.is_bot {
            return Err(AppError::boxed(
                "CreateUserAccessToken",
                "app.user_access_token.disabled",
                None,
                String::new(),
                // **501, not 403.** The id also lacks the `.app_error` suffix every other id on
                // this route carries; both are Go's.
                501,
            ));
        }

        if !user.is_bot {
            self.validate_user_access_token_expiry_at(
                token.expires_at,
                mm_model::utils::get_millis(),
            )?;
        }

        token.token = mm_model::utils::new_id();

        let saved = self
            .store()
            .user_access_token()
            .save(token)
            .await
            .map_err(|err| save_error("CreateUserAccessToken", err))?;

        tracing::Span::current().record("token_id", &saved.id);
        Ok(saved)
    }

    /// Port of `App.RevokeUserAccessToken` (session.go:687).
    ///
    /// # The session the token minted dies in the store, not here
    ///
    /// Go reads the session by the token's **secret**, deletes the token, then calls
    /// `RevokeSession` on what it read. The row deletion is already done by then: the store's
    /// transaction joins `Sessions.Token = UserAccessTokens.Token` and deletes both. What
    /// `RevokeSession` adds is cache eviction and a mobile wipe push, neither of which exists in
    /// this port (D-087 for the cache), so the lookup is not reproduced — reproducing it would
    /// read a session in order to delete a row that is already gone.
    ///
    /// This is also why the token must be fetched **unsanitised** before calling this: Go needs
    /// the secret for the session lookup, and the store needs it for the join. `getUserAccessToken`
    /// passes `sanitize: true` and the revoke paths pass `false` for exactly that reason.
    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    pub async fn revoke_user_access_token(&self, token_id: &str) -> AppResult {
        self.store()
            .user_access_token()
            .delete(token_id)
            .await
            .map_err(|err| {
                token_error(
                    "RevokeUserAccessToken",
                    "app.user_access_token.delete.app_error",
                    err,
                )
            })
    }

    /// Port of `App.DisableUserAccessToken` (session.go:798).
    ///
    /// The same session sweep as a revoke, but the row survives with `IsActive = false` — which
    /// is what makes `enable` possible and a revoke final.
    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    pub async fn disable_user_access_token(&self, token_id: &str) -> AppResult {
        self.store()
            .user_access_token()
            .update_token_disable(token_id)
            .await
            .map_err(|err| {
                token_error(
                    "DisableUserAccessToken",
                    "app.user_access_token.update_token_disable.app_error",
                    err,
                )
            })
    }

    /// Port of `App.EnableUserAccessToken` (session.go:813).
    ///
    /// **No session work at all**, and none is needed: disabling deleted them, so re-enabling a
    /// token restores a credential with nothing authenticated by it yet.
    #[tracing::instrument(skip_all, fields(token_id = %token_id))]
    pub async fn enable_user_access_token(&self, token_id: &str) -> AppResult {
        self.store()
            .user_access_token()
            .update_token_enable(token_id)
            .await
            .map_err(|err| {
                token_error(
                    "EnableUserAccessToken",
                    "app.user_access_token.update_token_enable.app_error",
                    err,
                )
            })
    }

    /// Port of `App.RotateUserAccessToken` (session.go:824).
    ///
    /// Returns the token carrying the **new secret** — the second and last response that ever
    /// does, alongside creation. The old secret and every session minted from it are gone by
    /// then; see [`mm_store::UserAccessTokenStore::update_token_rotate`] for why the order of the
    /// two statements is a security property.
    ///
    /// # The expiry is validated against a throwaway copy
    ///
    /// Go builds a `rotated` struct holding only `{Id, UserId, ExpiresAt}` so a rejected rotation
    /// leaves the caller's token untouched. Here the check takes the proposed `expires_at`
    /// directly, which is the same thing without the copy: nothing is mutated before the store
    /// call succeeds.
    ///
    /// As with creation, the non-bot e-mail (`SendUserAccessTokenRotatedEmail`) is not sent.
    #[tracing::instrument(skip_all, fields(token_id = %token.id, expires_at = expires_at))]
    pub async fn rotate_user_access_token(
        &self,
        mut token: UserAccessToken,
        expires_at: i64,
    ) -> AppResult<UserAccessToken> {
        // `userService.GetUser` — `MissingAccountError` at **404** for a miss and
        // `app.user.get.app_error` at 500 for anything else, which is exactly the split
        // [`App::get_user`] already makes. Propagated unchanged rather than rebuilt with this
        // function's name: `where` is `json:"-"` and the handler overwrites it with the request
        // path, so the only thing a rebuild could change is invisible.
        let user = self.get_user(&token.user_id).await?;

        if !self.config().enable_user_access_tokens && !user.is_bot {
            return Err(AppError::boxed(
                "RotateUserAccessToken",
                "app.user_access_token.disabled",
                None,
                String::new(),
                501,
            ));
        }

        if !user.is_bot {
            self.validate_user_access_token_expiry_at(expires_at, mm_model::utils::get_millis())?;
        }

        let new_secret = mm_model::utils::new_id();
        self.store()
            .user_access_token()
            .update_token_rotate(&token.id, &new_secret, expires_at)
            .await
            .map_err(|err| {
                token_error(
                    "RotateUserAccessToken",
                    "app.user_access_token.rotate.app_error",
                    err,
                )
            })?;

        // Go mutates the caller's token *after* the store call and returns it, so the response
        // carries the pre-rotation `description` and `is_active` beside the new secret and expiry.
        token.token = new_secret;
        token.expires_at = expires_at;
        Ok(token)
    }

    /// Port of `App.SearchUserAccessTokens` (session.go:923).
    ///
    /// Sanitised, like the three list reads — the search is an administrative lookup, not a way
    /// to recover a secret.
    #[tracing::instrument(skip_all, fields(found))]
    pub async fn search_user_access_tokens(&self, term: &str) -> AppResult<Vec<UserAccessToken>> {
        let mut tokens = self
            .store()
            .user_access_token()
            .search(term)
            .await
            .map_err(|err| {
                token_error(
                    "SearchUserAccessTokens",
                    "app.user_access_token.search.app_error",
                    err,
                )
            })?;

        sanitize_all(&mut tokens);
        tracing::Span::current().record("found", tokens.len());
        Ok(tokens)
    }

    /// Port of `App.RevokeNonCompliantUserAccessTokens` (session.go:757).
    ///
    /// # Refused when no policy is in effect
    ///
    /// The count route answers `0` when `MaximumPersonalAccessTokenLifetimeDays <= 0`; this one
    /// answers **400** (`…revoke_non_compliant.no_policy.app_error`). Same predicate, opposite
    /// treatment, and Go's comment gives the reason: a caller reaching this path with the policy
    /// off has a stale view of the config, and there is nothing to revoke.
    ///
    /// # The loop, and what it counts
    ///
    /// At most 1,000 batches of at most 1,000 tokens. A batch shorter than the limit ends the
    /// sweep; a full batch goes round again. The count is the **number of deleted tokens**, not
    /// of distinct users — the store returns one user id per token and Go sums `len(userIDs)`
    /// before de-duplicating for the cache eviction it does per user.
    ///
    /// Running out of batches is a **500** with a partial count already in `totalRevoked`, which
    /// the handler then discards: it returns the error, so the client never learns how many were
    /// revoked before the cap.
    #[tracing::instrument(skip_all, fields(batches, revoked))]
    pub async fn revoke_non_compliant_user_access_tokens(&self) -> AppResult<i64> {
        let Some(max_expires_at) = self.max_user_access_token_expiry() else {
            return Err(AppError::boxed(
                "RevokeNonCompliantUserAccessTokens",
                "app.user_access_token.revoke_non_compliant.no_policy.app_error",
                None,
                String::new(),
                400,
            ));
        };

        let mut total_revoked: i64 = 0;
        let mut all_revoked = false;
        let mut batches = 0;

        for _ in 0..REVOKE_NON_COMPLIANT_MAX_BATCHES {
            batches += 1;
            let user_ids = self
                .store()
                .user_access_token()
                .delete_non_compliant_expiry(max_expires_at, REVOKE_NON_COMPLIANT_BATCH_LIMIT)
                .await
                .map_err(|err| {
                    // **`delete.app_error`, shared with a single-token revoke.** Go reuses the id
                    // here rather than minting one for the sweep, so a client cannot tell the two
                    // failures apart.
                    token_error(
                        "RevokeNonCompliantUserAccessTokens",
                        "app.user_access_token.delete.app_error",
                        err,
                    )
                })?;

            if user_ids.is_empty() {
                all_revoked = true;
                break;
            }

            total_revoked += user_ids.len() as i64;

            // Go clears the session cache once per distinct user id here. There is no session
            // cache in this port (D-087), so the de-duplication has nothing to drive and is not
            // reproduced — the sessions themselves are already gone, deleted inside the store's
            // statement.

            if (user_ids.len() as i64) < REVOKE_NON_COMPLIANT_BATCH_LIMIT {
                all_revoked = true;
                break;
            }
        }

        tracing::Span::current().record("batches", batches);
        tracing::Span::current().record("revoked", total_revoked);

        if !all_revoked {
            return Err(AppError::boxed(
                "RevokeNonCompliantUserAccessTokens",
                "app.user_access_token.revoke_non_compliant.partial.app_error",
                None,
                String::new(),
                500,
            ));
        }

        Ok(total_revoked)
    }
}

/// The arithmetic of [`App::max_user_access_token_expiry_at`], as a free function so it is
/// testable without an `App` — and therefore without a database handle, which is what a
/// pure-arithmetic test should need.
fn max_expiry_at(max_days: i64, now_millis: i64) -> Option<i64> {
    if max_days <= 0 {
        return None;
    }
    Some(now_millis + max_days * MILLIS_PER_DAY)
}

/// The branches of [`App::validate_user_access_token_expiry_at`], as a free function so each one
/// is testable without an `App` — and therefore without a database handle, which a pure
/// comparison should not need. Same reason as [`max_expiry_at`] above.
fn validate_expiry(max_days: i64, expires_at: i64, now_millis: i64) -> AppResult {
    if expires_at == 0 {
        if max_days > 0 {
            return Err(AppError::boxed(
                "CreateUserAccessToken",
                "app.user_access_token.expires_at_required.app_error",
                None,
                String::new(),
                400,
            ));
        }
        return Ok(());
    }

    if expires_at <= now_millis {
        return Err(AppError::boxed(
            "CreateUserAccessToken",
            "app.user_access_token.expires_at_in_past.app_error",
            None,
            String::new(),
            400,
        ));
    }

    if max_days > 0 && expires_at > now_millis + max_days * MILLIS_PER_DAY {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert("Days".to_owned(), serde_json::Value::from(max_days));
        return Err(AppError::boxed(
            "CreateUserAccessToken",
            "app.user_access_token.expires_at_too_far.app_error",
            Some(params),
            String::new(),
            400,
        ));
    }

    Ok(())
}

/// `for _, token := range tokens { token.Token = "" }`, at all three call sites.
fn sanitize_all(tokens: &mut [UserAccessToken]) {
    for token in tokens {
        token.token.clear();
    }
}

/// A list failure is always a 500; only the id changes with the caller.
fn token_error(where_: &'static str, id: &'static str, err: StoreError) -> Box<AppError> {
    tracing::error!(error = ?err, "user access token lookup failed");
    AppError::boxed(where_, id, None, String::new(), 500)
}

/// The save path's error mapping, which is **not** a flat 500.
///
/// Go's `CreateUserAccessToken` does `errors.As(nErr, &appErr)` first and returns that error
/// unchanged: `Save` calls `IsValid()` inside the store, so a description over 255 characters
/// reaches the client as a **400** carrying `model.user_access_token.is_valid.description.app_error`
/// — a model id from a route whose every other failure is an app one. Only a driver failure gets
/// `app.user_access_token.save.app_error` at 500.
fn save_error(where_: &'static str, err: StoreError) -> Box<AppError> {
    match err {
        StoreError::Invalid { app_error, .. } => app_error,
        other => {
            tracing::error!(error = ?other, "saving a user access token failed");
            AppError::boxed(
                where_,
                "app.user_access_token.save.app_error",
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

    fn token_with_secret() -> UserAccessToken {
        UserAccessToken {
            id: "j1x3z8ynqjbstd4c4k6qy1p7ph".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            description: "personal".to_owned(),
            is_active: true,
            expires_at: 0,
            last_notified_at: None,
        }
    }

    /// The security property of three of the four routes, asserted where it happens. If the loop
    /// stops clearing, every listed token's secret goes to the client.
    #[test]
    fn sanitize_clears_the_secret_and_nothing_else() {
        let mut tokens = vec![token_with_secret(), token_with_secret()];
        sanitize_all(&mut tokens);

        for token in &tokens {
            assert_eq!(token.token, "", "the secret must never reach a client");
            assert_eq!(token.id, "j1x3z8ynqjbstd4c4k6qy1p7ph");
            assert!(token.is_active, "everything else survives");
        }

        // `token` carries `omitempty`, so a cleared secret is an **absent key** rather than an
        // empty string — which is what makes the sanitised document a different shape.
        let wire = serde_json::to_string(&tokens[0]).expect("serialises");
        assert!(!wire.contains("token"), "no token key at all: {wire}");
        assert!(wire.contains("\"id\""));
    }

    /// `expires_at == 0` means "never expires", and whether that is allowed depends entirely on
    /// whether a lifetime policy is configured. Go's comment says it outright: a configured
    /// maximum *implies* tokens must expire.
    #[test]
    fn a_never_expiring_token_is_allowed_only_without_a_policy() {
        let now = 1_788_600_000_000;

        assert!(validate_expiry(0, 0, now).is_ok(), "no policy, no expiry");
        assert!(
            validate_expiry(-1, 0, now).is_ok(),
            "a negative maximum is no policy either"
        );

        let err = validate_expiry(30, 0, now).expect_err("a policy forbids a never-expiring token");
        assert_eq!(
            err.id,
            "app.user_access_token.expires_at_required.app_error"
        );
        assert_eq!(err.status_code, 400);
        assert!(
            err.params.is_none(),
            "only the `too_far` branch carries params"
        );
    }

    /// The past check is `<=`, so an expiry of **exactly now** is refused. One millisecond later
    /// it is accepted. A port that wrote `<` would accept a token already expired by `IsExpired`'s
    /// own `>=` — the two comparisons have to meet exactly here.
    #[test]
    fn an_expiry_at_or_before_now_is_in_the_past() {
        let now = 1_788_600_000_000;

        let err = validate_expiry(0, now, now).expect_err("now is not the future");
        assert_eq!(err.id, "app.user_access_token.expires_at_in_past.app_error");
        assert_eq!(err.status_code, 400);

        assert!(validate_expiry(0, now - 1, now).is_err(), "a moment ago");
        assert!(validate_expiry(0, now + 1, now).is_ok(), "a moment hence");
    }

    /// The window is inclusive at its far edge: `ExpiresAt > maxExpiry` is the refusal, so an
    /// expiry landing exactly on `now + maxDays` is accepted. And this is the one branch that
    /// carries an i18n param, `Days` — the number the message interpolates.
    #[test]
    fn the_far_edge_of_the_window_is_inclusive_and_names_the_days() {
        let now = 1_788_600_000_000;
        let boundary = now + 30 * MILLIS_PER_DAY;

        assert!(
            validate_expiry(30, boundary, now).is_ok(),
            "exactly at the cap"
        );

        let err = validate_expiry(30, boundary + 1, now).expect_err("one millisecond beyond");
        assert_eq!(err.id, "app.user_access_token.expires_at_too_far.app_error");
        assert_eq!(err.status_code, 400);
        assert_eq!(
            err.params.as_ref().and_then(|p| p.get("Days")),
            Some(&serde_json::Value::from(30_i64)),
            "the cap in days, not milliseconds: {:?}",
            err.params
        );

        // With no policy the same far-future expiry is fine — the cap is the only thing that
        // makes it "too far".
        assert!(validate_expiry(0, boundary + 1, now).is_ok());
    }

    /// A validation failure inside the store is passed through **unchanged**: Go's
    /// `errors.As(nErr, &appErr)` returns the model's own error, so an over-long description is a
    /// 400 naming a `model.` id from a route whose other failures are 500s naming `app.` ones.
    #[test]
    fn a_store_validation_failure_keeps_its_status_and_id() {
        let mut token = token_with_secret();
        token.description = "d".repeat(256);
        let app_error = token
            .is_valid()
            .expect_err("256 characters is over the cap");

        let mapped = save_error(
            "CreateUserAccessToken",
            StoreError::Invalid {
                entity: "UserAccessToken",
                app_error,
            },
        );

        assert_eq!(
            mapped.id,
            "model.user_access_token.is_valid.description.app_error"
        );
        assert_eq!(mapped.status_code, 400, "not the 500 a driver failure gets");
    }

    /// Everything else on the save path is the 500 with the route's own id. Kept beside the test
    /// above because the pair is the whole point of `save_error`: one branch must not swallow the
    /// other.
    #[test]
    fn any_other_save_failure_is_the_routes_own_five_hundred() {
        let mapped = save_error(
            "CreateUserAccessToken",
            StoreError::Argument {
                entity: "UserAccessToken",
                detail: "something the driver refused",
            },
        );

        assert_eq!(mapped.id, "app.user_access_token.save.app_error");
        assert_eq!(mapped.status_code, 500);
    }

    /// `<= 0` disables the policy, and a **negative** value disables it too — reading the
    /// condition as `== 0` would turn a nonsense setting into a window ending in the past, which
    /// makes every token look non-compliant.
    #[test]
    fn a_non_positive_lifetime_disables_the_policy() {
        let now = 1_788_600_000_000;
        for days in [0, -1, -365] {
            assert_eq!(max_expiry_at(days, now), None, "{days} days");
        }

        assert_eq!(
            max_expiry_at(30, now),
            Some(now + 30 * MILLIS_PER_DAY),
            "thirty days in milliseconds, added to the clock"
        );
        // The unit is days, not seconds or milliseconds: one day is 86,400,000 ms, and getting
        // that wrong by a factor of a thousand would still look like a plausible window.
        assert_eq!(max_expiry_at(1, 0), Some(86_400_000));
    }
}
