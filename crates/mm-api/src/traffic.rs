//! One log line per request, for measuring how much of a real client session this server answers.
//!
//! Off unless `MM_API_TRAFFIC_LOG` is set, and then applied by `main.rs` around the finished
//! router, so nothing about routing changes. Each line is target `mm_api::traffic` at `info`:
//!
//! ```text
//! method=GET path=/api/v4/users/me route=/api/v4/users/me status=200 served_by=rust forwarded_at=- params=-
//! ```
//!
//! - `route` is axum's matched template, or `-` when the request matched no route and reached
//!   `Router::fallback` — an **unregistered** route. `scripts/demo-traffic.sh` maps those back to
//!   Go's own templates with `scripts/routes.py`.
//! - `served_by` is the `x-mmrs-served-by` header. A response this process built without the
//!   header (the websocket upgrade's `101`) is `rust-unmarked`: nothing forwarded it.
//! - `params` is the query's parameter **names**, comma-separated — never the values, which can
//!   carry an `access_token`. A handler that forwards some branches picks them by parameter.
//! - `forwarded_at` is the source line that handed the request to Go
//!   ([`crate::proxy::ForwardSite`]). For a registered route that line names the forwarded branch.
//!
//! Not a copy of Go's access log and not meant to become one; it exists for
//! `scripts/demo-traffic.sh`.

use axum::Router;
use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;

use crate::error::SERVED_BY;
use crate::proxy::ForwardSite;

/// The matched route template, carried from inside the router to the outer logging layer.
#[derive(Clone, Debug)]
struct RouteTemplate(MatchedPath);

/// Whether `MM_API_TRAFFIC_LOG` asks for the log. Any non-empty value other than `0` or `false`.
pub fn enabled_from_env() -> bool {
    flag_is_on(std::env::var("MM_API_TRAFFIC_LOG").ok().as_deref())
}

fn flag_is_on(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"))
}

/// Wrap a finished router with the traffic log.
///
/// Two layers because axum only knows the matched template *inside* a route: a `route_layer`
/// copies it onto the response, and the outer `layer` — which also sees fallback responses —
/// writes the line.
pub fn wrap(router: Router) -> Router {
    router
        .route_layer(axum::middleware::from_fn(record_template))
        .layer(axum::middleware::from_fn(log_request))
}

async fn record_template(request: Request, next: Next) -> Response {
    let template = request.extensions().get::<MatchedPath>().cloned();
    let mut response = next.run(request).await;
    if let Some(template) = template {
        response.extensions_mut().insert(RouteTemplate(template));
    }
    response
}

async fn log_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    // Owned because the request is moved into the next layer before the line is written.
    let path = request.uri().path().to_owned();
    let params = query_names(request.uri().query());
    let response = next.run(request).await;
    let route = response
        .extensions()
        .get::<RouteTemplate>()
        .map_or("-", |t| t.0.as_str());
    let served_by = response
        .headers()
        .get(SERVED_BY)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("rust-unmarked");
    let forwarded_at = response
        .extensions()
        .get::<ForwardSite>()
        .map(|site| format!("{}:{}", site.0.file(), site.0.line()));
    tracing::info!(
        target: "mm_api::traffic",
        method = %method,
        path = %path,
        route = %route,
        status = response.status().as_u16(),
        served_by = %served_by,
        forwarded_at = %forwarded_at.as_deref().unwrap_or("-"),
        params = %params,
    );
    response
}

/// The query's parameter names in order, comma-separated, or `-` when there are none.
fn query_names(query: Option<&str>) -> String {
    let names: Vec<String> = form_urlencoded::parse(query.unwrap_or_default().as_bytes())
        .map(|(name, _)| name.into_owned())
        .collect();
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join(",")
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;

    /// The template reaches the outer layer only through the response extension, so a route
    /// served inside the router must come back carrying it, and a fallback response must not.
    #[tokio::test]
    async fn the_template_rides_out_on_matched_routes_only() {
        let router = wrap(
            Router::new()
                .route("/a/{id}", get(|| async { StatusCode::OK }))
                .fallback(|| async { StatusCode::IM_A_TEAPOT.into_response() }),
        );
        let matched = router
            .clone()
            .oneshot(Request::get("/a/xyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        // The outer layer does not strip the extension, so it is observable here.
        assert_eq!(
            matched
                .extensions()
                .get::<RouteTemplate>()
                .map(|t| t.0.as_str()),
            Some("/a/{id}")
        );
        let fallback = router
            .oneshot(Request::get("/nowhere").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(fallback.status(), StatusCode::IM_A_TEAPOT);
        assert!(fallback.extensions().get::<RouteTemplate>().is_none());
    }

    #[test]
    fn params_are_names_without_values() {
        assert_eq!(query_names(None), "-");
        assert_eq!(query_names(Some("")), "-");
        assert_eq!(
            query_names(Some("since=1&access_token=secret&unread")),
            "since,access_token,unread"
        );
    }

    #[test]
    fn the_flag_reads_off_values_as_off() {
        // Over the value rather than the process environment, which tests share.
        assert!(flag_is_on(Some("1")));
        assert!(!flag_is_on(Some("0")));
        assert!(!flag_is_on(Some("FALSE")));
        assert!(!flag_is_on(Some("")));
        assert!(!flag_is_on(None));
    }
}
