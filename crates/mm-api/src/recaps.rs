//! Port of `api4/recap.go` and `api4/scheduled_recap.go` — all fifteen routes.
//!
//! # A **configuration** gate, not a licence one, and that is the whole difference
//!
//! Every one of the fifteen handlers opens with `requireRecapsEnabled(c)` (recap.go:25), which is
//! `Config.AIRecapsEnabled()` — `FeatureFlags.EnableAIRecaps && AIRecapSettings.IsEnabled()`. On a
//! stock server the feature flag is `false`, so all fifteen answer
//! `api.recap.disabled.app_error` at **501** before consulting anything else.
//!
//! That looks like the licence families in `licensed_features.rs` and behaves differently in the
//! one way that matters: **an operator can turn this on**, from the environment, without touching
//! a licence. The moment they do, every one of these routes must stop being answered here and go
//! back to Go — there is no AI recap engine on this side and never will be. So the gate is read
//! from the live configuration on every request rather than decided at startup, and
//! `mm_app::config::Config::ai_recaps_enabled` is where the rule lives.
//!
//! # The corner that inverts the intuition
//!
//! `AIRecapSettings.IsEnabled()` is `s == nil || s.Enable == nil || *s.Enable`, so an **absent**
//! setting means *enabled*. Only the feature flag is off by default. A port that read the setting
//! as `unwrap_or(false)` would refuse recaps on a server whose operator had switched the flag on
//! and left the setting alone — which is the normal way to enable them.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// `requireRecapsEnabled` (api4/recap.go:26).
const RECAPS_DISABLED_ERROR: &str = "api.recap.disabled.app_error";

/// Refuse, or forward if an operator has enabled recaps.
///
/// `where_` is the Go handler's own name — not on the wire, since `AppError.Where` is not
/// serialised, but it keeps each route's identity in the source and in the trace.
async fn refuse_or_forward(state: AppState, where_: &'static str, request: Request) -> Response {
    let enabled = state.app.config().ai_recaps_enabled();
    tracing::Span::current().record("recaps_enabled", enabled);

    if enabled {
        return proxy::forward_to_go(State(state), request).await;
    }

    ApiError::from(AppError::new(
        where_,
        RECAPS_DISABLED_ERROR,
        None,
        String::new(),
        501,
    ))
    .into_response()
}

macro_rules! recap_route {
    ($fn_name:ident, $go:literal) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is `requireRecapsEnabled`.")]
        #[tracing::instrument(skip_all, fields(recaps_enabled))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            refuse_or_forward(state, $go, request).await
        }
    };
}

recap_route!(get_recaps, "getRecaps");
recap_route!(create_recap, "createRecap");
recap_route!(get_recap_limit_status, "getRecapLimitStatus");
recap_route!(mark_recaps_as_viewed, "markRecapsAsViewed");
recap_route!(get_recap, "getRecap");
recap_route!(delete_recap, "deleteRecap");
recap_route!(mark_recap_as_read, "markRecapAsRead");
recap_route!(regenerate_recap, "regenerateRecap");

recap_route!(get_scheduled_recaps, "getScheduledRecaps");
recap_route!(create_scheduled_recap, "createScheduledRecap");
recap_route!(get_scheduled_recap, "getScheduledRecap");
recap_route!(update_scheduled_recap, "updateScheduledRecap");
recap_route!(delete_scheduled_recap, "deleteScheduledRecap");
recap_route!(pause_scheduled_recap, "pauseScheduledRecap");
recap_route!(resume_scheduled_recap, "resumeScheduledRecap");

#[cfg(test)]
mod tests {
    use super::*;

    /// The error id, spelled out. It names the **feature**, not a licence — the two families this
    /// module sits beside both end in `.license.error`, and reaching for that shape here would be
    /// the natural mistake.
    #[test]
    fn the_refusal_names_the_feature_and_not_a_licence() {
        assert_eq!(RECAPS_DISABLED_ERROR, "api.recap.disabled.app_error");
        assert!(!RECAPS_DISABLED_ERROR.contains("license"));
        assert!(RECAPS_DISABLED_ERROR.ends_with(".app_error"));
    }
}
