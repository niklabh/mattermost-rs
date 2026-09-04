//! Port of `getEmojiList`, `getEmoji` and `getEmojiByName` (channels/api4/emoji.go:116, :207,
//! :229), reached as `GET /api/v4/emoji`, `GET /api/v4/emoji/{emoji_id}` and
//! `GET /api/v4/emoji/name/{emoji_name}`.
//!
//! # The two config gates are not the same gate
//!
//! Each handler checks `EnableCustomEmoji` and answers **501** `api.emoji.disabled.app_error`.
//! [`mm_app::App::get_emoji`] then checks it *again* and answers **403** with the same id. The
//! handler runs first, so the 403 is unreachable through these routes — reproduced in the app
//! layer for the reason given there, and the 501 is what a client sees.
//!
//! # `image` is not here
//!
//! `GET /emoji/{emoji_id}/image` is one segment deeper, is registered by
//! `APISessionRequiredTrustRequester` rather than the ordinary wrapper, and serves bytes from
//! the file backend. Unregistered, so it falls to `Router::fallback` and stays Go's.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::emoji::{EMOJI_NAME_MAX_LENGTH, EMOJI_SORT_BY_NAME};
use mm_model::utils::{AppError, is_valid_alpha_num_hyphen_underscore_plus};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_first, require_id};
use crate::error::ApiError;

/// The `/emoji/` literals gorilla registers **before** `{emoji_id}` and answers with a handler
/// of their own on `GET`.
///
/// `api.BaseRoutes.Emojis` (api.go:285) is a `PathPrefix("/emoji")` subrouter added to `APIRoot`
/// *before* the `PathPrefix("/emoji/{emoji_id}")` one (api.go:286), so a request for one of its
/// literals never reaches `getEmoji`. axum has the opposite instinct — it prefers a literal
/// route, but only one that is *registered* — and `/emoji/autocomplete` is not, so it would land
/// on `{emoji_id}` here and 400 where Go returns a list.
///
/// Only the `GET` literals are listed. `names` and `search` are `POST`-only in Go, so a `GET`
/// to either falls past them with `ErrMethodMismatch` and *does* reach `getEmoji` with
/// `emoji_id = "names"` — a 400 both servers produce, pinned by the parity suite rather than
/// papered over here. The bare `/emoji` collection is one segment shorter and is not this
/// route's problem at all.
const EMOJI_SHADOWED_LITERALS: &[&str] = &["autocomplete"];

/// Port of `getEmoji` (api4/emoji.go:207).
///
/// # Order
///
/// `RequireEmojiId` → the `EnableCustomEmoji` 501 → `App.GetEmoji`. A disabled server therefore
/// still 400s a malformed id, rather than reporting the feature off — which is what a client
/// distinguishing "bad request" from "not available here" depends on.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(emoji)` — **trailing newline** ([D-086]).
#[tracing::instrument(skip_all, fields(emoji_id = %emoji_id, forwarded))]
pub async fn get_emoji(
    State(state): State<AppState>,
    Path(emoji_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = session;

    if EMOJI_SHADOWED_LITERALS.contains(&emoji_id.as_str()) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match serve_one_emoji(async {
        require_id(&emoji_id, "emoji_id")?;
        custom_emoji_enabled(&state, "getEmoji")?;
        Ok(state.app.get_emoji(&emoji_id).await?)
    })
    .await
    {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `getEmojiByName` (api4/emoji.go:229).
///
/// # The segment charset and the validator are the same rule twice
///
/// gorilla's class is `[A-Za-z0-9\_\-\+]+` (api.go:287) and `RequireEmojiName`
/// (web/context.go:597) compiles `^[a-zA-Z0-9\-\+_]+$` — the same character set, so the *only*
/// thing the validator adds is the 64-byte length limit. A segment outside the charset was a
/// mux 404 before any handler ran, which is why it is forwarded here rather than answered 400;
/// a segment inside it but too long reaches the handler on both servers and 400s on both.
///
/// The shared validator is reused for the charset half on evidence rather than on inspection:
/// `mm_model::reaction`'s `go_parity::the_two_emoji_name_regexes_agree` runs both spellings
/// against the oracle corpus.
#[tracing::instrument(skip_all, fields(emoji_name = %emoji_name, forwarded))]
pub async fn get_emoji_by_name(
    State(state): State<AppState>,
    Path(emoji_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = session;

    if !segment_matches_emoji_name_mux(&emoji_name) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    match serve_one_emoji(async {
        // `RequireEmojiName` — everything the mux already enforced, plus the length.
        if emoji_name.is_empty()
            || emoji_name.len() > EMOJI_NAME_MAX_LENGTH
            || !is_valid_alpha_num_hyphen_underscore_plus(&emoji_name)
        {
            return Err(ApiError::invalid_url_param("emoji_name"));
        }
        custom_emoji_enabled(&state, "getEmojiByName")?;
        Ok(state.app.get_emoji_by_name(&emoji_name).await?)
    })
    .await
    {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Go's mux class for `{emoji_name}`: `[A-Za-z0-9\_\-\+]+` (api.go:287).
///
/// Identical to the character set `IsValidAlphaNumHyphenUnderscorePlus` matches, so the
/// validator is reused. It carries no length limit — the mux does not have one, and the
/// handler's 64-byte check is what a long name meets.
fn segment_matches_emoji_name_mux(value: &str) -> bool {
    is_valid_alpha_num_hyphen_underscore_plus(value)
}

/// `!*c.App.Config().ServiceSettings.EnableCustomEmoji` → **501**, not the app layer's 403.
fn custom_emoji_enabled(state: &AppState, where_: &'static str) -> Result<(), ApiError> {
    if state.app.config().enable_custom_emoji {
        return Ok(());
    }
    Err(ApiError::from(AppError::new(
        where_,
        "api.emoji.disabled.app_error",
        None,
        String::new(),
        501,
    )))
}

/// The shared tail of both handlers: encode with a trailing newline, or turn the error into one.
async fn serve_one_emoji(
    lookup: impl Future<Output = Result<mm_model::emoji::Emoji, ApiError>>,
) -> Result<Response, ApiError> {
    let emoji = lookup.await?;

    let mut body = serde_json::to_vec(&emoji).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise Emoji");
        ApiError::from(AppError::new(
            "getEmoji",
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

/// `getEmojiList`'s only query parameter beyond pagination (api4/emoji.go:122).
const SORT_PARAM: &str = "sort";

/// Port of `getEmojiList` (api4/emoji.go:116), reached as `GET /api/v4/emoji`.
///
/// # Order, and the gate that is *not* here
///
/// `EnableCustomEmoji` 501 → the `sort` validation → `App.GetEmojiList`. There is no
/// `RequireEmojiId` because there is no path parameter, and — unlike the two single reads —
/// the **app layer has no gates of its own**: `App.GetEmojiList` (app/emoji.go:99) goes straight
/// to the store. The handler's 501 is the only thing guarding this query.
///
/// # `sort` takes exactly two values and one of them is the empty string
///
/// `if sort != "" && sort != model.EmojiSortByName` → `SetInvalidURLParam("sort")`. So `?sort=`
/// and an absent `sort` are the same request, `?sort=name` orders by name, and everything else —
/// `Name`, `NAME`, `created_at` — is a 400. The comparison is case-sensitive.
///
/// Absent ordering is not a stable order: Go emits **no `ORDER BY` at all** for the default
/// case, so two servers reading the same table can legitimately disagree about row order. The
/// parity suite compares the unsorted page as a set and the sorted page byte for byte.
///
/// # Pagination is `web.ParamsFromRequest`'s, and it never 400s
///
/// `page` defaults to 0 and `per_page` to 60, both clamped rather than rejected — garbage falls
/// to the default and `per_page` above 200 is capped. `?per_page=0` survives to the store as
/// `LIMIT 0` and answers `[]`, which is the one place this route differs from the channel and
/// post lists, where a zero limit means *no limit*.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(listEmoji)` — **trailing newline** ([D-086]). The empty answer is
/// `[]` and not `null`: Go's store initialises `emojis := []*model.Emoji{}` before the scan, the
/// opposite of the nil `getReactions` and `getFileInfosForPost` return.
#[tracing::instrument(skip_all, fields(page, per_page, sort))]
pub async fn get_emoji_list(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = session;
    let query = request.uri().query().map(str::to_owned);

    match serve_emoji_list(&state, query.as_deref()).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_emoji_list(state: &AppState, query: Option<&str>) -> Result<Response, ApiError> {
    custom_emoji_enabled(state, "getEmoji")?;

    // `where` is `getEmoji`, not `getEmojiList` — Go's own copy-paste (api4/emoji.go:118), and
    // `AppError.Where` is `json:"-"` so nothing on the wire depends on it. Reproduced anyway;
    // it is what a log reader matches on.
    let sort = query_first(query, SORT_PARAM).unwrap_or_default();
    if !sort.is_empty() && sort != EMOJI_SORT_BY_NAME {
        return Err(ApiError::invalid_url_param(SORT_PARAM));
    }

    let page = parse_page(query);
    let per_page = parse_per_page(query);
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);
    tracing::Span::current().record("sort", sort.as_str());

    let emojis = state
        .app
        .get_emoji_list(page, per_page, sort == EMOJI_SORT_BY_NAME)
        .await?;

    let mut body = serde_json::to_vec(&emojis).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the emoji list");
        ApiError::from(AppError::new(
            "getEmojiList",
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The mux class, which is the validator's character set exactly. A segment outside it is a
    /// Go mux 404, so the handler must forward rather than answer.
    #[test]
    fn the_emoji_name_segment_charset_is_gos_mux_class() {
        assert!(segment_matches_emoji_name_mux("thumbsup"));
        assert!(segment_matches_emoji_name_mux("+1"));
        assert!(segment_matches_emoji_name_mux("a-b_c+d"));
        assert!(segment_matches_emoji_name_mux("MiXeD9"));
        assert!(!segment_matches_emoji_name_mux(""));
        assert!(!segment_matches_emoji_name_mux("has.dot"));
        assert!(!segment_matches_emoji_name_mux("has space"));
        assert!(!segment_matches_emoji_name_mux("caf\u{e9}"));
    }

    /// The mux has no length limit, so a 65-byte name reaches the handler and is a 400 there —
    /// not a forward. The boundary is **bytes**, matching Go's `len()`.
    #[test]
    fn the_length_limit_is_the_handlers_not_the_muxs() {
        let sixty_four = "a".repeat(EMOJI_NAME_MAX_LENGTH);
        let sixty_five = "a".repeat(EMOJI_NAME_MAX_LENGTH + 1);
        assert!(segment_matches_emoji_name_mux(&sixty_four));
        assert!(
            segment_matches_emoji_name_mux(&sixty_five),
            "the mux has no length limit, so this reaches the handler"
        );
        assert!(sixty_four.len() <= EMOJI_NAME_MAX_LENGTH);
        assert!(sixty_five.len() > EMOJI_NAME_MAX_LENGTH);
    }

    /// The literal list is exactly the `GET`-registered siblings of `{emoji_id}`. `names` and
    /// `search` must **not** be in it: they are POST-only in Go, so a GET falls through to
    /// `getEmoji` and 400s, which is what our `{emoji_id}` handler does too.
    #[test]
    fn only_the_get_literals_are_shadowed() {
        assert!(EMOJI_SHADOWED_LITERALS.contains(&"autocomplete"));
        assert!(!EMOJI_SHADOWED_LITERALS.contains(&"names"));
        assert!(!EMOJI_SHADOWED_LITERALS.contains(&"search"));
    }
}
