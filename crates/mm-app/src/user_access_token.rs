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
