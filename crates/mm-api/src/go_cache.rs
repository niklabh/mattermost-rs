//! [`mm_app::peer_cache::PeerCache`] for the Go server beside this one, through Go's own
//! authenticated REST API.
//!
//! # One Go route per Go function, chosen for the cache clear it runs
//!
//! Go exposes no "forget this user's sessions" route, so each method drives a route whose handler
//! runs exactly the clear it needs, with no lasting write:
//!
//! - **`clear_user_sessions`** inserts a throwaway session row for the user and has Go revoke it
//!   (`POST /users/{id}/sessions/revoke`). `PlatformService.RevokeSession` deletes the row and
//!   calls `ClearUserSessionCache(session.UserId)` — the very function being reproduced. If Go
//!   does not answer 200 the row is deleted here instead, so it never outlives the call.
//! - **`invalidate_user`** calls `POST /users/{id}/reset_failed_attempts`, whose
//!   `UpdateFailedPasswordAttempts(id, 0)` goes through the local-cache layer's
//!   `InvalidateProfileCacheForUser`. It **writes the counter**, so it is sent only when the
//!   stored counter is already 0 — which it is after every self-service password change, since the
//!   current-password check zeroes it. When the counter is not 0, or the account is SSO (the route
//!   refuses those), nothing is sent and the stale window remains; logged at debug. A failed login
//!   landing between the read and Go's write is lost from the count — one attempt, once.
//! - **`clear_all_sessions`** is `POST /caches/invalidate`. It also wipes Go's status cache, which
//!   blanks `get_statuses` for Go's web connections until users are active again; accepted only
//!   for the one route it serves, the sysadmin's log-everyone-out.
//!
//! # Authenticated by a session this process mints
//!
//! All three routes need a system administrator. A token in the environment would be a standing
//! secret that the parity suite's own revoke-all tests delete mid-run; this process already writes
//! the shared `Sessions` table, so it inserts a row for the administrator named by
//! `MM_API_GO_CACHE_USER` instead, marked with the [`PEER_CACHE_SESSION_PROP`] prop. A 401 means
//! that row was revoked or expired: it is minted again and the request retried once.
//!
//! # Failure
//!
//! Logged, never propagated — the write that triggered the purge has already happened, and Go's
//! entry ages out on its own (`SessionCacheInMinutes`).

use mm_app::peer_cache::{PeerCache, PeerFuture};
use mm_model::session::Session;
use mm_store::{SessionStore, SqlStore, UserStore};

/// The prop set on the administrator session this process mints.
pub const PEER_CACHE_SESSION_PROP: &str = "mmrs_peer_cache";

/// The prop set on the throwaway session Go is asked to revoke.
pub const PEER_CACHE_PROBE_PROP: &str = "mmrs_peer_cache_probe";

/// How long a minted session lives. Re-minted on a 401 either way; a day keeps the row count at
/// one per day of uptime rather than one per purge.
const SESSION_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;

/// A throwaway row that somehow survives both Go's revoke and our own delete still stops
/// authenticating a minute later. It carries no roles either way.
const PROBE_LIFETIME_MS: i64 = 60 * 1000;

/// A request that hangs must not hold a logout open indefinitely.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
enum MintError {
    #[error("looking up the peer-cache user {username:?}: {source}")]
    User {
        username: String,
        source: mm_store::StoreError,
    },
    #[error("saving the peer-cache session: {0}")]
    Save(mm_store::StoreError),
}

/// Purges the Go server's caches through its authenticated REST API.
#[derive(Debug)]
pub struct GoCacheInvalidator {
    store: SqlStore,
    http: reqwest::Client,
    base: String,
    username: String,
    token: tokio::sync::Mutex<Option<String>>,
}

impl GoCacheInvalidator {
    /// `go_upstream` is the Go server's base URL; `username` a system administrator's.
    pub fn new(store: SqlStore, go_upstream: &str, username: String) -> Self {
        Self {
            store,
            http: reqwest::Client::new(),
            base: go_upstream.trim_end_matches('/').to_owned(),
            username,
            token: tokio::sync::Mutex::new(None),
        }
    }

    async fn mint(&self) -> Result<String, MintError> {
        let user = self
            .store
            .user()
            .get_by_username(&self.username)
            .await
            .map_err(|source| MintError::User {
                username: self.username.clone(),
                source,
            })?;
        let mut session = Session {
            user_id: user.id,
            roles: user.roles,
            expires_at: mm_model::utils::get_millis() + SESSION_LIFETIME_MS,
            ..Session::default()
        };
        session.add_prop(PEER_CACHE_SESSION_PROP, "true");
        let saved = self
            .store
            .session()
            .save(session)
            .await
            .map_err(MintError::Save)?;
        Ok(saved.token)
    }

    /// `POST {go}{path}` as the minted administrator; `true` on a 2xx.
    #[tracing::instrument(skip(self, body), fields(go_status))]
    async fn post(&self, path: &str, body: String) -> bool {
        // Held across the request so concurrent purges share one minted session rather than each
        // inserting their own.
        let mut token = self.token.lock().await;
        for attempt in 0..2 {
            let current = match token.as_deref() {
                Some(current) => current.to_owned(),
                None => match self.mint().await {
                    Ok(minted) => token.insert(minted).clone(),
                    Err(err) => {
                        tracing::warn!(error = %err, "could not mint a session to purge the Go server's cache");
                        return false;
                    }
                },
            };
            let status = match self
                .http
                .post(format!("{}{path}", self.base))
                .bearer_auth(&current)
                // A retry needs the body again; it is a few bytes.
                .body(body.clone())
                .timeout(REQUEST_TIMEOUT)
                .send()
                .await
            {
                Ok(response) => response.status(),
                Err(err) => {
                    tracing::warn!(error = %err, "could not reach the Go server to purge its cache");
                    return false;
                }
            };
            tracing::Span::current().record("go_status", status.as_u16());
            if status.is_success() {
                return true;
            }
            if status == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                *token = None;
                continue;
            }
            tracing::warn!(%status, "the Go server refused to purge its cache");
            return false;
        }
        false
    }

    async fn clear_user_sessions_impl(&self, user_id: &str) {
        let mut probe = Session {
            user_id: user_id.to_owned(),
            expires_at: mm_model::utils::get_millis() + PROBE_LIFETIME_MS,
            ..Session::default()
        };
        probe.add_prop(PEER_CACHE_PROBE_PROP, "true");
        let probe = match self.store.session().save(probe).await {
            Ok(probe) => probe,
            Err(err) => {
                tracing::warn!(error = %err, "could not insert the session Go is asked to revoke");
                return;
            }
        };
        let body = serde_json::json!({ "session_id": probe.id }).to_string();
        let revoked = self
            .post(&format!("/api/v4/users/{user_id}/sessions/revoke"), body)
            .await;
        if !revoked {
            if let Err(err) = self.store.session().remove(&probe.id).await {
                tracing::warn!(error = %err, "could not delete the unrevoked probe session");
            }
        }
    }

    async fn invalidate_user_impl(&self, user_id: &str) {
        let user = match self.store.user().get(user_id).await {
            Ok(user) => user,
            Err(err) => {
                tracing::warn!(error = %err, "could not read the user whose Go cache is stale");
                return;
            }
        };
        if user.failed_attempts != 0 || !matches!(user.auth_service.as_str(), "" | "ldap") {
            tracing::debug!(
                failed_attempts = user.failed_attempts,
                auth_service = user.auth_service,
                "not purging Go's cached user: the only route that does would change the row"
            );
            return;
        }
        self.post(
            &format!("/api/v4/users/{user_id}/reset_failed_attempts"),
            String::new(),
        )
        .await;
    }
}

impl PeerCache for GoCacheInvalidator {
    fn clear_user_sessions<'a>(&'a self, user_id: &'a str) -> PeerFuture<'a> {
        Box::pin(self.clear_user_sessions_impl(user_id))
    }

    fn clear_all_sessions(&self) -> PeerFuture<'_> {
        Box::pin(async {
            self.post("/api/v4/caches/invalidate", String::new()).await;
        })
    }

    fn invalidate_user<'a>(&'a self, user_id: &'a str) -> PeerFuture<'a> {
        Box::pin(self.invalidate_user_impl(user_id))
    }
}
