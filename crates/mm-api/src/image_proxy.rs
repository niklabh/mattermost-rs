//! Port of `getImage` (api4/image.go:17) — `GET /api/v4/image?url=…`, the webapp's image proxy
//! entry point.
//!
//! # With the proxy off, every request is the same 400
//!
//! Go parses `url` (a parse failure or an opaque URL is the 400 `api.image.get.app_error`),
//! fills in the scheme and host from `SiteURL`, and only then looks at
//! `ImageProxySettings.Enable`. Off — the default, and this deployment — it answers the **same**
//! 400 with the same id and an empty `detailed_error` (`Wrap` never reaches the wire), because
//! "we don't support redirecting to external images any longer (MM-54477)". So on this
//! deployment the parse is unobservable: a URL that fails to parse and one that parses both
//! get the identical body, measured on the stack Go for an empty `url`, a remote URL, a
//! `mailto:` and `%zz`. This handler therefore does not port Go's `net/url` parser, which has
//! no Rust equivalent that agrees with it on relative references.
//!
//! With the proxy **on**, Go redirects (302) to a same-host URL and streams a remote one
//! through the local or remote image proxy — both forwarded here, the parser included, since
//! the host comparison would need it.

use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;
use crate::proxy;

/// Port of `getImage` — `GET /api/v4/image`.
#[tracing::instrument(skip_all, fields(forwarded))]
pub async fn get_image(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    if state.app.config().image_proxy_enable {
        tracing::Span::current().record("forwarded", true);
        return proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);
    ApiError::from(AppError::new(
        "getImage",
        "api.image.get.app_error",
        None,
        String::new(),
        400,
    ))
    .into_response()
}
