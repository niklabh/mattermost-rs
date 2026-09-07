//! Three small families whose **first statement** is a gate, and whose gates are three different
//! kinds: a licence, a configuration setting, and a configuration setting followed by a licence.
//!
//! | Family | Gate | Refusal |
//! |---|---|---|
//! | `ip_filtering.go` | licence — and a **cloud** one, at Enterprise tier | `api.context.ip_filtering.not_available.app_error`, 501 |
//! | `ai_bridge_test_helper.go` | `ServiceSettings.EnableTesting`, default **false** | `api.ai_bridge_test_helper.disabled.app_error`, 501 |
//! | `scheduled_post.go` | `ServiceSettings.ScheduledPosts` (default **true**), *then* a licence | `api.scheduled_posts.license_error`, **400** |
//!
//! They are one module because the mistake they invite is the same one: reaching for a
//! neighbouring family's status or id. Two of the three refuse with 501 and one with 400; the
//! scheduled-post gate has **two arms with different ids at the same status**, and which arm fires
//! depends on a setting an operator can change.
//!
//! # `ip_filtering` needs more than a licence
//!
//! `ensureIPFilteringInterface` (ip_filtering.go:22) wants the interface **and** a licence **and**
//! `license.IsCloud()` **and** `MinimumEnterpriseLicense`. Four conditions; a self-hosted
//! Enterprise installation is refused as firmly as an unlicensed one. This server can only
//! establish "no licence at all", so it answers exactly that case and forwards the rest — where
//! Go applies the other three.
//!
//! # The scheduled-post gate is the only one here whose *reachable* arm is a licence
//!
//! `ScheduledPosts` defaults to **true**, so on a stock server the config arm passes and the
//! licence arm fires. Turn the setting off and the id changes to
//! `api.scheduled_posts.feature_disabled` at the same 400 — which is why the setting is modelled
//! rather than assumed.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::LicenceGate;
use crate::error::ApiError;
use crate::proxy;

/// `ensureIPFilteringInterface` (ip_filtering.go:24).
const IP_FILTERING_NOT_AVAILABLE: &str = "api.context.ip_filtering.not_available.app_error";
/// `requireAIBridgeTestHelperEnabled` (ai_bridge_test_helper.go:22).
const AI_BRIDGE_DISABLED: &str = "api.ai_bridge_test_helper.disabled.app_error";
/// `requireScheduledPostsEnabled`'s licence arm (scheduled_post.go:69).
const SCHEDULED_POSTS_LICENSE_ERROR: &str = "api.scheduled_posts.license_error";
/// Its config arm (scheduled_post.go:64) — **the same status, a different id**.
const SCHEDULED_POSTS_FEATURE_DISABLED: &str = "api.scheduled_posts.feature_disabled";

/// A licence-gated refusal at 501.
async fn licence_refusal(
    state: AppState,
    where_: &'static str,
    id: &'static str,
    request: Request,
) -> Response {
    match crate::channels::licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => {
            ApiError::from(AppError::new(where_, id, None, String::new(), 501)).into_response()
        }
        LicenceGate::Failed(err) => err.into_response(),
    }
}

macro_rules! ip_filtering_route {
    ($fn_name:ident, $go:literal) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is `ensureIPFilteringInterface`.")]
        #[tracing::instrument(skip_all, fields(licensed))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            licence_refusal(state, $go, IP_FILTERING_NOT_AVAILABLE, request).await
        }
    };
}

ip_filtering_route!(get_ip_filters, "getIPFilters");
ip_filtering_route!(apply_ip_filters, "applyIPFilters");
ip_filtering_route!(my_ip, "myIP");

macro_rules! ai_bridge_route {
    ($fn_name:ident, $go:literal) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is `requireAIBridgeTestHelperEnabled`.")]
        ///
        /// Gated on `ServiceSettings.EnableTesting`, **not** a licence — an operator can turn it
        /// on, and then this server must forward: the helper writes into an in-memory AI bridge
        /// that exists only in the Go process.
        #[tracing::instrument(skip_all, fields(testing_enabled))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            let enabled = state.app.config().enable_testing;
            tracing::Span::current().record("testing_enabled", enabled);
            if enabled {
                return proxy::forward_to_go(State(state), request).await;
            }
            ApiError::from(AppError::new(
                $go,
                AI_BRIDGE_DISABLED,
                None,
                String::new(),
                501,
            ))
            .into_response()
        }
    };
}

ai_bridge_route!(get_ai_bridge_test_helper, "getAIBridgeTestHelper");
ai_bridge_route!(put_ai_bridge_test_helper, "putAIBridgeTestHelper");
ai_bridge_route!(delete_ai_bridge_test_helper, "deleteAIBridgeTestHelper");

macro_rules! scheduled_post_route {
    ($fn_name:ident, $go:literal) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is `requireScheduledPostsEnabled`.")]
        ///
        /// Two arms, one status. The config arm fires first and answers
        /// `api.scheduled_posts.feature_disabled`; the licence arm answers
        /// `api.scheduled_posts.license_error`. Both are **400**, and the setting defaults to
        /// `true`, so the licence arm is the one a stock server reaches.
        #[tracing::instrument(skip_all, fields(scheduled_posts, licensed))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            scheduled_posts_gate(state, $go, request).await
        }
    };
}

/// `requireScheduledPostsEnabled` (scheduled_post.go:62), both arms and in Go's order.
async fn scheduled_posts_gate(state: AppState, where_: &'static str, request: Request) -> Response {
    let enabled = state.app.config().scheduled_posts;
    tracing::Span::current().record("scheduled_posts", enabled);
    if !enabled {
        return ApiError::from(AppError::new(
            where_,
            SCHEDULED_POSTS_FEATURE_DISABLED,
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match crate::channels::licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            where_,
            SCHEDULED_POSTS_LICENSE_ERROR,
            None,
            String::new(),
            400,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

scheduled_post_route!(create_schedule_post, "createSchedulePost");
scheduled_post_route!(update_scheduled_post, "updateScheduledPost");
scheduled_post_route!(delete_scheduled_post, "deleteScheduledPost");
scheduled_post_route!(get_team_scheduled_posts, "getTeamScheduledPosts");

#[cfg(test)]
mod tests {
    use super::*;

    /// Three families, three ids, and **two statuses**. The scheduled-post one is a 400 where its
    /// two neighbours are 501, which is the mistake this module exists to make hard.
    #[test]
    fn the_four_ids_are_distinct_and_the_statuses_are_not_uniform() {
        let ids = [
            IP_FILTERING_NOT_AVAILABLE,
            AI_BRIDGE_DISABLED,
            SCHEDULED_POSTS_LICENSE_ERROR,
            SCHEDULED_POSTS_FEATURE_DISABLED,
        ];
        for (i, a) in ids.iter().enumerate() {
            for b in &ids[i + 1..] {
                assert_ne!(a, b, "every refusal in this module names a different thing");
            }
        }
        assert!(IP_FILTERING_NOT_AVAILABLE.ends_with(".app_error"));
        assert!(AI_BRIDGE_DISABLED.ends_with(".app_error"));
        assert!(
            !SCHEDULED_POSTS_LICENSE_ERROR.ends_with(".app_error"),
            "the scheduled-post ids do not carry the suffix the other two do"
        );
    }

    /// The scheduled-post gate's **two arms**, which share a status and differ in id.
    ///
    /// Which one a server produces depends on `ServiceSettings.ScheduledPosts`, whose default is
    /// `true` — so the licence arm is the reachable one and the config arm is what an operator
    /// turning the feature off would see.
    #[test]
    fn the_scheduled_post_gate_has_two_arms_at_one_status() {
        let config = AppError::new(
            "createSchedulePost",
            SCHEDULED_POSTS_FEATURE_DISABLED,
            None,
            String::new(),
            400,
        );
        let licence = AppError::new(
            "createSchedulePost",
            SCHEDULED_POSTS_LICENSE_ERROR,
            None,
            String::new(),
            400,
        );
        assert_eq!(config.status_code, licence.status_code, "one status");
        assert_ne!(config.id, licence.id, "two ids");
        assert!(mm_app::config::Config::default().scheduled_posts);
        assert!(!mm_app::config::Config::default().enable_testing);
    }
}
