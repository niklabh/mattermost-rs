//! The two sessions that are not `Sessions` rows: `app.GetRemoteClusterSession` and
//! `app.GetCloudSession` (channels/app/session.go:56,71), plus the `GetRemoteCluster` lookup the
//! first one makes (channels/app/remote_cluster.go:312).
//!
//! Both mint a bare-bones in-memory [`Session`] — no id, no user, no row — carrying only the
//! token and a `type` prop. The web layer reaches them for a request whose token came from
//! `X-Cloud-Token` or `X-RemoteCluster-Token` rather than a cookie or `Authorization`, and a
//! `CloudKeyRequired` / `RemoteClusterTokenRequired` handler then asks only for that prop.
//!
//! Neither needs the `RemoteClusterService`: the session is a table read and a constant-time
//! compare, which is why it is portable while the service (nil on every build this project runs)
//! is not.

use std::collections::HashMap;

use mm_model::remote_cluster::RemoteCluster;
use mm_model::session::{
    SESSION_PROP_TYPE, SESSION_TYPE_CLOUD_KEY, SESSION_TYPE_REMOTECLUSTER_TOKEN, Session,
};
use mm_model::utils::{AppError, AppResult};
use mm_store::RemoteClusterStore;

use crate::App;

/// Go's id for both refusals here, 401 — the same id `GetSession` builds, under a different
/// `where`.
const INVALID_TOKEN: &str = "api.context.invalid_token.error";

/// `subtle.ConstantTimeCompare(a, b) == 1`. Go returns `0` for different lengths without
/// comparing, so the length is not protected — reproduced rather than improved, as in
/// [`crate::file::public_link_hash_matches`].
fn constant_time_equal(a: &str, b: &str) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// The bare-bones session both functions return: `Token`, `IsOAuth: false` and one prop.
fn bare_session(token: &str, session_type: &str) -> Session {
    let mut session = Session {
        token: token.to_owned(),
        is_oauth: false,
        ..Session::default()
    };
    session.add_prop(SESSION_PROP_TYPE, session_type);
    session
}

/// `NewAppError(where, "api.context.invalid_token.error", {"Token": token, "Error": ""},
/// "The provided token is invalid", 401)` (app/session.go:68, :83).
///
/// The token is carried for the same reason `session.rs`'s twin carries it: the sentence is
/// `Invalid session token={{.Token}}, err={{.Error}}` and the client reads it. See [D-079].
fn invalid_token(where_: &str, token: &str) -> Box<AppError> {
    let mut params: HashMap<String, serde_json::Value> = HashMap::new();
    params.insert(
        "Token".to_owned(),
        serde_json::Value::String(token.to_owned()),
    );
    params.insert("Error".to_owned(), serde_json::Value::String(String::new()));
    AppError::boxed(
        where_,
        INVALID_TOKEN,
        Some(params),
        "The provided token is invalid",
        401,
    )
}

impl App {
    /// Port of `app.App.GetRemoteCluster` (remote_cluster.go:312): the store's `Get`, with a
    /// miss as `api.remote_cluster.get.not_found` at 404 and anything else as
    /// `api.remote_cluster.get.app_error` at 500.
    #[tracing::instrument(skip(self), fields(remote_id = %remote_id))]
    pub async fn get_remote_cluster(
        &self,
        remote_id: &str,
        include_deleted: bool,
    ) -> AppResult<RemoteCluster> {
        self.store()
            .remote_cluster()
            .get(remote_id, include_deleted)
            .await
            .map_err(|err| {
                let (id, status) = if err.is_not_found() {
                    ("api.remote_cluster.get.not_found", 404)
                } else {
                    ("api.remote_cluster.get.app_error", 500)
                };
                Box::new(
                    AppError::new("GetRemoteCluster", id, None, String::new(), status).wrap(err),
                )
            })
    }

    /// Port of `app.App.GetRemoteClusterSession` (session.go:71).
    ///
    /// **Every failure is the one 401**, including the lookup's 404 *and* its 500: Go tests only
    /// `appErr == nil && ConstantTimeCompare(...) == 1`, so a database failure, a deleted remote
    /// (`includeDeleted` is `false`), a row with a NULL column (see `mm_store::
    /// remote_cluster_store`) and a wrong token are indistinguishable to the caller.
    ///
    /// The token compared is the row's `Token` — "their token for calling us" is `RemoteToken` in
    /// the model's own comments, but Go compares `rc.Token`, and so does this.
    #[tracing::instrument(skip(self, token), fields(remote_id = %remote_id))]
    pub async fn get_remote_cluster_session(
        &self,
        token: &str,
        remote_id: &str,
    ) -> AppResult<Session> {
        match self.get_remote_cluster(remote_id, false).await {
            Ok(rc) if constant_time_equal(&rc.token, token) => {
                Ok(bare_session(token, SESSION_TYPE_REMOTECLUSTER_TOKEN))
            }
            Ok(_) => Err(invalid_token("GetRemoteClusterSession", token)),
            Err(err) => {
                tracing::debug!(error = %err, "the remote cluster lookup failed");
                Err(invalid_token("GetRemoteClusterSession", token))
            }
        }
    }

    /// Port of `app.App.GetCloudSession` (session.go:56): the token against the **process's**
    /// `MM_CLOUD_API_KEY`, and an empty key accepts nothing.
    ///
    /// Reached only under a cloud licence (`License().IsCloud()`); each server reads its own
    /// environment, as Go's does.
    pub fn get_cloud_session(&self, token: &str) -> AppResult<Session> {
        cloud_session(std::env::var("MM_CLOUD_API_KEY").ok().as_deref(), token)
    }
}

/// [`App::get_cloud_session`] with the key passed in, so both branches are testable without
/// touching the process environment.
fn cloud_session(api_key: Option<&str>, token: &str) -> AppResult<Session> {
    match api_key {
        Some(api_key) if !api_key.is_empty() && constant_time_equal(api_key, token) => {
            Ok(bare_session(token, SESSION_TYPE_CLOUD_KEY))
        }
        _ => Err(invalid_token("GetCloudSession", token)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_equal_matches_only_identical_strings() {
        assert!(constant_time_equal("abc", "abc"));
        assert!(!constant_time_equal("abc", "abd"));
        assert!(!constant_time_equal("abc", "abcd"));
        assert!(!constant_time_equal("", "a"));
        assert!(constant_time_equal("", ""));
    }

    #[test]
    fn the_remote_session_is_bare_and_typed() {
        let session = bare_session("tok", SESSION_TYPE_REMOTECLUSTER_TOKEN);
        assert_eq!(session.token, "tok");
        assert!(!session.is_oauth);
        assert!(session.id.is_empty());
        assert!(session.user_id.is_empty());
        assert_eq!(
            session.prop(SESSION_PROP_TYPE),
            Some(SESSION_TYPE_REMOTECLUSTER_TOKEN)
        );
    }

    #[test]
    fn invalid_token_is_gos_401_and_carries_the_token_its_sentence_names() {
        let err = invalid_token("GetRemoteClusterSession", "tok");
        assert_eq!(err.id, INVALID_TOKEN);
        assert_eq!(err.status_code, 401);
        assert_eq!(err.where_, "GetRemoteClusterSession");
        assert_eq!(err.detailed_error, "The provided token is invalid");
        let params = err.params.expect("params are set");
        assert_eq!(
            params.get("Token"),
            Some(&serde_json::Value::String("tok".to_owned()))
        );
    }

    /// `apiKey != "" && ConstantTimeCompare(...) == 1` — both halves, each branch.
    #[test]
    fn the_cloud_session_needs_a_non_empty_matching_key() {
        let ok = cloud_session(Some("key"), "key").expect("a matching key is accepted");
        assert_eq!(ok.prop(SESSION_PROP_TYPE), Some(SESSION_TYPE_CLOUD_KEY));
        for (key, token) in [(Some("key"), "other"), (Some(""), ""), (None, "key")] {
            let err = cloud_session(key, token).expect_err("refused");
            assert_eq!((err.id.as_str(), err.status_code), (INVALID_TOKEN, 401));
            assert_eq!(err.where_, "GetCloudSession");
        }
    }
}
