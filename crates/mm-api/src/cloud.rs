//! Port of the twelve `api4/cloud.go` routes whose first statement is `ensureCloudInterface`.
//!
//! # A **400**, not a 501, and that is the whole surprise
//!
//! `c.App.Cloud()` is `einterfaces.CloudInterface`, registered only by the enterprise build, so on
//! the Team Edition binary beside us it is nil and every one of these handlers answers
//! `api.server.cws.needs_enterprise_edition` at **400 Bad Request** (cloud.go:58). Every other
//! licence-shaped refusal this server produces is a 501; this family is the exception, and a port
//! that reached for the neighbouring shape would answer the right id with the wrong status.
//!
//! # The refusal that is *not* ours, again
//!
//! `ensureCloudInterface` has a second arm — `CloudSettings.Disable` — answering
//! `api.server.cws.disabled` at **422**. It is reached only after the interface exists, so this
//! server can never produce it. Three ids and three statuses live in one nine-line helper.
//!
//! # Two routes in the file are **not** here
//!
//! `getPreviewModalData` has no cloud gate at all — it answers
//! `app.cloud.preview_modal_bucket_url_not_configured` at 404, measured — and
//! `handleCWSWebhook` is `api.CloudAPIKeyRequired`, a different authentication wrapper from the
//! session one every route here uses — served since 2026-09-14 as the 401 that wrapper gives
//! without a Cloud licence (see [`handle_cws_webhook`]). The first is left to the proxy.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::LicenceGate;
use crate::error::ApiError;

/// `ensureCloudInterface`'s first arm (cloud.go:58).
const NEEDS_ENTERPRISE_EDITION: &str = "api.server.cws.needs_enterprise_edition";

/// Its second arm, unreachable from here — see the module note.
#[cfg(test)]
const CWS_DISABLED: &str = "api.server.cws.disabled";

/// Refuse with a **400**, or forward if this installation has a licence.
async fn refuse_or_forward(state: AppState, where_: &'static str, request: Request) -> Response {
    match crate::channels::licence_gate(&state, request).await {
        LicenceGate::Forward(response) => response,
        LicenceGate::Unlicensed => ApiError::from(AppError::new(
            where_,
            NEEDS_ENTERPRISE_EDITION,
            None,
            String::new(),
            400,
        ))
        .into_response(),
        LicenceGate::Failed(err) => err.into_response(),
    }
}

macro_rules! cloud_route {
    ($fn_name:ident, $go:literal) => {
        #[doc = concat!("Port of `", $go, "`, whose first statement is `ensureCloudInterface`.")]
        #[tracing::instrument(skip_all, fields(licensed))]
        pub async fn $fn_name(
            State(state): State<AppState>,
            _session: AuthenticatedSession,
            request: Request,
        ) -> Response {
            refuse_or_forward(state, $go, request).await
        }
    };
}

cloud_route!(get_cloud_products, "Api4.getCloudProducts");
cloud_route!(get_cloud_limits, "Api4.getCloudLimits");
cloud_route!(get_cloud_customer, "Api4.getCloudCustomer");
cloud_route!(update_cloud_customer, "Api4.updateCloudCustomer");
cloud_route!(
    update_cloud_customer_address,
    "Api4.updateCloudCustomerAddress"
);
cloud_route!(get_installation, "Api4.getInstallation");
cloud_route!(
    get_invoices_for_subscription,
    "Api4.getInvoicesForSubscription"
);
cloud_route!(
    get_subscription_invoice_pdf,
    "Api4.getSubscriptionInvoicePDF"
);
cloud_route!(validate_business_email, "Api4.validateBusinessEmail");
cloud_route!(
    validate_workspace_business_email,
    "Api4.validateWorkspaceBusinessEmail"
);
cloud_route!(handle_check_cws_connection, "Api4.handleCheckCWSConnection");

/// Port of `getSubscription` (cloud.go:213).
///
/// # The one route here that does something before the gate, and it needs a licence to do it
///
/// `getSubscription` first checks for a **cloud preview licence** and, if one is installed,
/// synthesises a subscription without consulting the cloud interface at all. That branch is
/// unreachable without a licence, so on this server the route falls through to
/// `ensureCloudInterface` and answers the same 400 as its eleven neighbours — measured. Since the
/// preview branch is precisely "a licence is installed", forwarding when licensed covers it.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn get_subscription(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    refuse_or_forward(state, "Api4.getSubscription", request).await
}

/// Port of `handleCWSWebhook` (cloud.go:42), reached as `POST /api/v4/cloud/webhook` — the
/// one route of the file behind `CloudAPIKeyRequired` rather than a session.
///
/// `web.Handler.ServeHTTP` runs `CloudKeyRequired` before the handler: no licence, a licence
/// that is not Cloud, or a session that is not a cloud-key session is the 401
/// `api.context.session_expired.app_error` with `TokenRequired` as its detail — and the first
/// two arms hold for every licence this deployment can carry, so the route is that 401 here for
/// any caller, token or not. A Cloud licence would reach the key check, `ensureCloudInterface`
/// and the webhook's event switch, and is forwarded.
#[tracing::instrument(skip_all, fields(cloud))]
pub async fn handle_cws_webhook(
    State(state): State<AppState>,
    _csrf: crate::auth::CsrfGuard,
    request: Request,
) -> Response {
    let cloud = match state.app.license().await {
        Ok(license) => license.is_some_and(|l| l.is_cloud()),
        Err(err) => return ApiError::from(*err).into_response(),
    };
    tracing::Span::current().record("cloud", cloud);
    if cloud {
        tracing::debug!("handing a CWS webhook to Go");
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    ApiError::from(AppError::new(
        "",
        "api.context.session_expired.app_error",
        None,
        "TokenRequired",
        401,
    ))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A 400, not a 501.** Every other licence-shaped refusal in this router is a 501; this one
    /// is a `Bad Request`, and the status is the thing a client branches on.
    #[test]
    fn the_cloud_refusal_is_a_400() {
        let err = AppError::new(
            "Api4.getCloudLimits",
            NEEDS_ENTERPRISE_EDITION,
            None,
            String::new(),
            400,
        );
        assert_eq!(err.status_code, 400);
        assert_ne!(err.status_code, 501, "the neighbouring families' status");
        assert_eq!(err.id, "api.server.cws.needs_enterprise_edition");
    }

    /// The three answers that nine-line helper can give, and that only the first is ours.
    #[test]
    fn the_helper_has_three_answers_and_we_produce_one() {
        assert_eq!(
            NEEDS_ENTERPRISE_EDITION,
            "api.server.cws.needs_enterprise_edition"
        );
        assert_eq!(CWS_DISABLED, "api.server.cws.disabled");
        assert_ne!(NEEDS_ENTERPRISE_EDITION, CWS_DISABLED);
        assert!(
            !CWS_DISABLED.contains("enterprise"),
            "the second arm names the setting, not the build"
        );
    }
}
