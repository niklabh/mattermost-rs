//! Port of `getRedirectLocation` (api4/system.go) — `GET /api/v4/redirect_location?url=…`, the
//! webapp asking where a link points before it previews it.
//!
//! # Every outcome is a 200 with a `location`, and most of them are empty
//!
//! `ServiceSettings.EnableLinkPreviews` off is `{"location":""}` before the parameter is read;
//! a missing `url` is the one 400. Otherwise a **cached** answer is returned as it was cached,
//! else one `HEAD` through the outbound-connection guard ([`mm_app::http_guard`]) with
//! redirects not followed: a transport error — the guard's refusal of an internal address
//! included — is cached as `""` for an hour so it is not retried, a `Location` over 2100 bytes is
//! treated the same way, and anything else is the header's value, cached for the same hour.
//! `model.MapToJSON` writes the map, so there is no trailing newline.
//!
//! On a deployment whose allow-list is empty the loopback and private addresses every parity
//! test can reach are all refusals, so the served answers the suite can measure are the empty
//! ones and the 400; the accept path rests on the guard's unit tests.

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_app::http_guard::GuardedClient;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `RedirectLocationCacheSize` (api4/system.go:30).
const CACHE_SIZE: usize = 10_000;
/// `RedirectLocationMaximumLength` (api4/system.go:31).
const MAXIMUM_LENGTH: usize = 2100;
/// `RedirectLocationCacheExpiry` (api4/system.go:32).
const CACHE_EXPIRY: Duration = Duration::from_secs(60 * 60);

/// `redirectLocationDataCache`, an LRU of 10,000 entries with an hour's expiry — kept here as a
/// map plus an insertion queue, evicting the oldest insertion when full. The entries a client
/// can observe are the same either way; only which entry is evicted first differs.
struct LocationCache {
    entries: HashMap<String, (String, Instant)>,
    order: VecDeque<String>,
}

static CACHE: LazyLock<Mutex<LocationCache>> = LazyLock::new(|| {
    Mutex::new(LocationCache {
        entries: HashMap::new(),
        order: VecDeque::new(),
    })
});

fn cache_get(url: &str) -> Option<String> {
    let cache = CACHE.lock().ok()?;
    let (location, stored) = cache.entries.get(url)?;
    (stored.elapsed() < CACHE_EXPIRY).then(|| location.clone())
}

fn cache_set(url: &str, location: &str) {
    let Ok(mut cache) = CACHE.lock() else {
        return;
    };
    if !cache.entries.contains_key(url) {
        while cache.entries.len() >= CACHE_SIZE {
            match cache.order.pop_front() {
                Some(oldest) => {
                    cache.entries.remove(&oldest);
                }
                None => break,
            }
        }
        cache.order.push_back(url.to_owned());
    }
    cache
        .entries
        .insert(url.to_owned(), (location.to_owned(), Instant::now()));
}

fn answer(location: &str) -> Response {
    let body = serde_json::json!({ "location": location }).to_string();
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response()
}

#[derive(serde::Deserialize)]
pub struct RedirectQuery {
    #[serde(default)]
    url: String,
}

/// Port of `getRedirectLocation` — `GET /api/v4/redirect_location`.
#[tracing::instrument(skip_all, fields(outcome))]
pub async fn get_redirect_location(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    Query(query): Query<RedirectQuery>,
) -> Response {
    let config = state.app.config();
    if !config.enable_link_previews {
        tracing::Span::current().record("outcome", "previews_off");
        return answer("");
    }

    if query.url.is_empty() {
        return ApiError::invalid_param("url").into_response();
    }

    if let Some(location) = cache_get(&query.url) {
        tracing::Span::current().record("outcome", "cached");
        return answer(&location);
    }

    let client = GuardedClient::new(
        &config.allowed_untrusted_internal_connections,
        config.enable_insecure_outgoing_connections,
    );
    let location = match client.head_without_redirects(&query.url).await {
        Ok(response) => response
            .headers()
            .get(axum::http::header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned(),
        Err(err) => {
            // Cache failures to prevent retries.
            tracing::debug!(error = %err, "the redirect probe failed");
            cache_set(&query.url, "");
            tracing::Span::current().record("outcome", "failed");
            return answer("");
        }
    };

    // "If the location length is > 2100, we can probably ignore" — treated as a failure.
    if location.len() > MAXIMUM_LENGTH {
        cache_set(&query.url, "");
        tracing::Span::current().record("outcome", "too_long");
        return answer("");
    }

    cache_set(&query.url, &location);
    tracing::Span::current().record("outcome", "probed");
    answer(&location)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cache answers what was stored, and the cap evicts the oldest insertion.
    #[test]
    fn the_cache_caps_and_answers() {
        cache_set("http://a.example/", "/x");
        assert_eq!(cache_get("http://a.example/").as_deref(), Some("/x"));
        assert_eq!(cache_get("http://b.example/"), None);
        cache_set("http://a.example/", "");
        assert_eq!(cache_get("http://a.example/").as_deref(), Some(""));
    }
}
