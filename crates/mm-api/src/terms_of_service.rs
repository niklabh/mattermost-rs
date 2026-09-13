//! Port of `getLatestTermsOfService` and `createTermsOfService`
//! (channels/api4/terms_of_service.go:20, :32), reached as `GET` and `POST /api/v4/terms_of_service`.
//!
//! The webapp asks for this at login when custom terms of service are switched on, and shows the
//! text before letting anyone in.
//!
//! # A session, and nothing else
//!
//! `APISessionRequired`, and then **no permission check at all** — the terms are what a user must
//! read before they can use the server, so gating them behind a permission would be circular. Two
//! lines of handler: fetch, encode.
//!
//! # Publishing is licence-gated; reading is not
//!
//! `createTermsOfService` beside it needs `manage_system` **and** a licence carrying
//! `CustomTermsOfService` (terms_of_service.go:34-40). On an unlicensed installation — the only
//! kind this server can read — the whole route is one 400, which is served here; a licensed one is
//! forwarded, because what a signed licence enables is not visible from this process.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::utils::AppError;

use mm_model::permission::PERMISSION_MANAGE_SYSTEM;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// Port of `getLatestTermsOfService` (terms_of_service.go:20).
///
/// An empty table is a **404** with `app.terms_of_service.get.no_rows.app_error` — not an empty
/// object and not a `null` — which is what a client checks to decide there is nothing to show.
///
/// `json.NewEncoder(w).Encode`, so a trailing newline.
#[tracing::instrument(skip_all, fields(id))]
pub async fn get_latest_terms_of_service(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let terms = state.app.get_latest_terms_of_service().await?;
    tracing::Span::current().record("id", &terms.id);

    let mut body = serde_json::to_vec(&terms).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the terms of service");
        ApiError::from(AppError::new(
            "getLatestTermsOfService",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Port of `model.MapFromJSON` (utils.go:507) — **every** failure is an empty map.
///
/// Same shape and the same one divergence as [`crate::team_member_writes`]'s copy: Go's decoder
/// fills its map before it fails, so `{"text":"hi","n":5}` leaves `text` behind where `serde_json`
/// yields `{}` and this route then answers `empty_text`. Recorded rather than papered over; no
/// client sends a mixed-type props object here.
fn map_from_json(bytes: &[u8]) -> std::collections::HashMap<String, String> {
    serde_json::from_slice(bytes).unwrap_or_default()
}

/// `app.ErrorTermsOfServiceNoRowsFound` (app/config.go:24).
///
/// It is a **string compared against `AppError.Id`**, not an error type — `createTermsOfService`
/// does `err.Id != app.ErrorTermsOfServiceNoRowsFound` — so the empty-table 404 is the one failure
/// of `GetLatestTermsOfService` this route walks past.
const ERROR_TERMS_OF_SERVICE_NO_ROWS_FOUND: &str = "app.terms_of_service.get.no_rows.app_error";

/// Port of `createTermsOfService` (api4/terms_of_service.go:32) — `POST /api/v4/terms_of_service`.
///
/// # The licence check is second, and it is a 400 rather than a 501
///
/// `manage_system` is required **first**, so a non-admin on an unlicensed server is refused for
/// the permission and never learns that the feature is licensed at all. Only then does
/// `license == nil || !*license.Features.CustomTermsOfService` answer **400**
/// `api.create_terms_of_service.custom_terms_of_service_disabled.app_error` — a different status
/// from the 501 the content-flagging and channel-bookmark families give for the same kind of
/// refusal, and it is the one this deployment always reaches.
///
/// A licensed installation is forwarded: whether `Features.CustomTermsOfService` is on inside a
/// signed licence is not something this server can read.
///
/// # `Config.IsValid` is the `where` on the empty-text error
///
/// Not `createTermsOfService`. Pasted from the configuration validator, kept because `where` is on
/// the wire for this project's own diffing even though Go's writer overwrites it with the request
/// path.
///
/// # Re-posting identical text is not an error and does not write
///
/// The latest revision is fetched and compared **by text**. An exact match returns the *existing*
/// row — same id, same `create_at` — so a client cannot tell a no-op from a publish except by the
/// id it gets back. Any other failure of that fetch (a 500) aborts; only the empty-table 404 is
/// walked past, which is how the very first revision gets published.
#[tracing::instrument(skip_all, fields(licensed, published))]
pub async fn create_terms_of_service(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(*mm_model::permission::make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let (parts, body) = request.into_parts();
    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            // `MapFromJSON` cannot fail; a body that cannot be *read* is an empty map, which is
            // the empty-text 400 below.
            axum::body::Bytes::new()
        }
    };
    let request = Request::from_parts(parts, axum::body::Body::from(bytes.clone()));

    // Go: `license := c.App.Channels().License(); license == nil ||
    // !*license.Features.CustomTermsOfService` → **400**. Read as a value rather than as an early
    // return so the shape of the route survives: the feature being *off* is the refusal, and the
    // feature being *on* is the path below.
    //
    // **Unlicensed is the refusal, not the pass.** Writing the gate the other way round publishes
    // a revision on a server that forbids it, which is what the first run of the parity suite
    // caught this handler doing.
    let custom_terms_of_service = match crate::channels::licence_gate(&state, request).await {
        // A licence's feature set lives inside a signed blob this process cannot open, so whether
        // `CustomTermsOfService` is on is unanswerable here and the request is Go's.
        crate::channels::LicenceGate::Forward(response) => return response,
        crate::channels::LicenceGate::Failed(err) => return err.into_response(),
        // Go's `license == nil`.
        crate::channels::LicenceGate::Unlicensed => false,
    };
    if !custom_terms_of_service {
        return ApiError::from(AppError::new(
            "createTermsOfService",
            "api.create_terms_of_service.custom_terms_of_service_disabled.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    // Everything below is ported and unreachable on an unlicensed installation, which is the only
    // kind this server can read — the same arrangement as `App::get_multiple_emoji_by_name`'s
    // shadowed 403. It is ported rather than deferred because the gate above is the *only* thing
    // in the way, and licensing does not gate development here. Its one branching decision is
    // [`should_publish`], which has tests; the rest is two store calls. See [D-382].
    let props = map_from_json(&bytes);
    let text = props.get("text").map(String::as_str).unwrap_or_default();
    if text.is_empty() {
        return ApiError::from(AppError::new(
            // Go's own paste — see the doc comment.
            "Config.IsValid",
            "api.create_terms_of_service.empty_text.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    let existing = match state.app.get_latest_terms_of_service().await {
        Ok(terms) => Some(terms),
        Err(err) if err.id == ERROR_TERMS_OF_SERVICE_NO_ROWS_FOUND => None,
        Err(err) => return ApiError::from(err).into_response(),
    };

    let terms = if should_publish(existing.as_ref(), text) {
        tracing::Span::current().record("published", true);
        match state
            .app
            .create_terms_of_service(text, &session.0.user_id)
            .await
        {
            Ok(terms) => terms,
            Err(err) => return ApiError::from(err).into_response(),
        }
    } else {
        tracing::Span::current().record("published", false);
        // `should_publish` is false only for a `Some` whose text matches, so this is Go's
        // `else { Encode(oldTermsOfService) }` branch and nothing else can reach it.
        match existing {
            Some(existing) => existing,
            None => {
                tracing::error!("should_publish refused with no existing revision");
                return ApiError::from(AppError::new(
                    "createTermsOfService",
                    "app.terms_of_service.create.app_error",
                    None,
                    String::new(),
                    500,
                ))
                .into_response();
            }
        }
    };

    match encode_terms(&terms, "createTermsOfService") {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// `json.NewEncoder(w).Encode(termsOfService)` — 200 and a **trailing newline**.
fn encode_terms(
    terms: &mm_model::terms_of_service::TermsOfService,
    where_: &'static str,
) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(terms).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the terms of service");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })?;
    body.push(b'\n');

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// Go's `oldTermsOfService == nil || oldTermsOfService.Text != text` (terms_of_service.go:59).
///
/// Two things a reader could plausibly get wrong, and both change what a client gets back:
///
/// * **An empty table publishes.** The `nil` case is the *first* disjunct, so the very first
///   revision is written even though there is nothing to compare it with.
/// * **The comparison is on the text alone and it is exact.** Re-posting byte-identical text is a
///   no-op that returns the *existing* row — same id, same `create_at` — so a client cannot tell a
///   publish from a no-op except by the id. A trimmed or case-folded comparison would silently
///   turn a real edit into a no-op.
fn should_publish(
    existing: Option<&mm_model::terms_of_service::TermsOfService>,
    text: &str,
) -> bool {
    match existing {
        None => true,
        Some(existing) => existing.text != text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::terms_of_service::TermsOfService;

    fn revision(text: &str) -> TermsOfService {
        TermsOfService {
            id: "6bdz674pgq767e4jx75w4pf57a".to_owned(),
            create_at: 1_700_000_000_000,
            user_id: "y3zh8mcpr7f3bemk9w4qsn5xdo".to_owned(),
            text: text.to_owned(),
        }
    }

    /// The empty table publishes, identical text does not, and the comparison is exact.
    #[test]
    fn the_first_revision_publishes_and_an_identical_one_does_not() {
        assert!(should_publish(None, "anything"), "an empty table publishes");
        assert!(
            should_publish(None, ""),
            "even an empty text — the text check is the handler's, and it ran first"
        );

        let current = revision("the terms");
        assert!(
            !should_publish(Some(&current), "the terms"),
            "byte-identical text is a no-op"
        );
        assert!(should_publish(Some(&current), "the terms."), "a real edit");
        assert!(
            should_publish(Some(&current), "The Terms"),
            "the comparison is case-sensitive"
        );
        assert!(
            should_publish(Some(&current), " the terms "),
            "and it does not trim"
        );
    }

    /// `MapFromJSON` swallows every failure into an empty map, which is what makes an
    /// unparseable body reach the `empty_text` 400 rather than a parse error of its own.
    #[test]
    fn map_from_json_swallows_everything_and_leaves_an_empty_text() {
        for body in [
            b"".as_slice(),
            b"not json",
            b"[1,2]",
            br#"{"text":5}"#,
            b"null",
        ] {
            assert!(
                !map_from_json(body).contains_key("text"),
                "{} should have left no text",
                String::from_utf8_lossy(body)
            );
        }
        assert_eq!(
            map_from_json(br#"{"text":"hello"}"#)
                .get("text")
                .map(String::as_str),
            Some("hello")
        );
        // Go's decoder fills its map before failing, so `{"text":"hi","n":5}` leaves `text`
        // behind on Go and nothing here. See the function's doc comment.
        assert!(map_from_json(br#"{"text":"hi","n":5}"#).is_empty());
    }

    /// The empty-text refusal carries **`Config.IsValid`** as its `where` — Go's own paste from
    /// the configuration validator, not this handler's name.
    #[test]
    fn the_empty_text_error_is_attributed_to_the_config_validator() {
        let err = AppError::new(
            "Config.IsValid",
            "api.create_terms_of_service.empty_text.app_error",
            None,
            String::new(),
            400,
        );
        assert_eq!(err.where_, "Config.IsValid");
        assert_ne!(err.where_, "createTermsOfService");
    }
}
