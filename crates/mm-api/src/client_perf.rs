//! Port of `submitPerformanceReport` (api4/metrics.go:17) — `POST /api/v4/client_perf`, the
//! webapp's performance telemetry.
//!
//! # Nothing is read, on every server this project runs
//!
//! The handler's first line is `if c.App.Metrics() == nil || !*EnableClientMetrics { return }`
//! — a 200 with an **empty body**, not `{"status":"OK"}` — and `Metrics()` is nil for two
//! independent reasons, either of which alone holds it there:
//!
//! - `PlatformService.Metrics()` (platform/metrics.go:353) returns nil until `resetMetrics`
//!   builds one, which it does only with `MetricsSettings.Enable` on (default off); and the
//!   implementation it wraps is `enterprise/metrics`, compiled in only behind the
//!   `enterprise || sourceavailable` build tags (`enterprise/local_imports.go`). The stack's
//!   Go and the licensed oracle both answer 200 and nothing to a valid report, an invalid one
//!   and a body that is not JSON — measured.
//! - This server has no metrics interface at all, so `Metrics() == nil` is structural here.
//!
//! What lies behind the gate on a metrics-enabled enterprise build — `model.PerformanceReport`
//! decoded, `IsValid` (a semver check against `performanceReportVersion`, `start <= end`, an
//! `end` within the TTL) as the 400 `submitPerformanceReport`, then
//! `RegisterPerformanceReport` observing each sample into Prometheus — is not ported: no route
//! this server can answer reaches it, and a report it accepted would be counted nowhere.
//!
//! `APISessionRequiredTrustRequester`: a session is required; `TrustRequester` only waives
//! the cookie-session CSRF check, which this server does not model (see `auth_writes`).

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::auth::AuthenticatedSession;

/// Port of `submitPerformanceReport` — `POST /api/v4/client_perf`.
///
/// The body is never read: Go returns before its decoder is constructed.
#[tracing::instrument(skip_all)]
pub async fn submit_performance_report(_session: AuthenticatedSession) -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
    )
        .into_response()
}
