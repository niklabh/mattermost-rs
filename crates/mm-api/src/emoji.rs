//! Port of `getEmojiList`, `getEmoji`, `getEmojiByName`, `autocompleteEmojis`,
//! `getEmojisByNames` and `searchEmojis` (channels/api4/emoji.go:116, :207, :229, :331, :253,
//! :286), reached as `GET /api/v4/emoji`, `GET /api/v4/emoji/{emoji_id}`,
//! `GET /api/v4/emoji/name/{emoji_name}`, `GET /api/v4/emoji/autocomplete`,
//! `POST /api/v4/emoji/names` and `POST /api/v4/emoji/search`.
//!
//! # The two config gates are not the same gate, and `searchEmojis` has only one
//!
//! Five of these handlers check `EnableCustomEmoji` and answer **501**
//! `api.emoji.disabled.app_error`; the app layer then checks it *again* and answers **403** with
//! the same id, which the handler's earlier return shadows. `searchEmojis` has **no handler
//! check**, so it is the one route where the 403 reaches the wire — a client distinguishing
//! "not implemented here" from "forbidden" sees a different answer from the same feature flag
//! depending on which emoji route it asked.
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
use mm_model::utils::{
    AppError, is_valid_alpha_num_hyphen_underscore_plus, sorted_array_from_json,
};

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
/// `emoji_id = "names"` — a 400 both servers produce. `search` still reaches this handler here,
/// because no route claims it; `names` does not, because `POST /emoji/names` is now served and
/// **axum prefers a registered literal for every method, not just the one it was registered
/// with**. That fallthrough is spelled out as [`get_emoji_name_literal`] instead. The bare
/// `/emoji` collection is one segment shorter and is not this route's problem at all.
/// Empty since `/emoji/autocomplete` became a route of its own: axum prefers a registered
/// literal over `{emoji_id}`, so the list that used to sit here is now the router's job. Kept
/// rather than deleted because `getEmoji`'s forwarding branch is the only thing standing between
/// a future `/emoji/<literal>` route of Go's and a 400 from this handler.
const EMOJI_SHADOWED_LITERALS: &[&str] = &[];

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

/// The body of `POST /api/v4/emoji/search` — `model.EmojiSearch` (model/emoji_search.go:6).
///
/// `#[serde(default)]` because Go leaves an absent key at its zero value: `{"term":"x"}` is a
/// legal body and `prefix_only` is then `false`.
#[derive(Debug, Default, serde::Deserialize)]
struct EmojiSearch {
    #[serde(default)]
    term: String,
    #[serde(default)]
    prefix_only: bool,
}

/// Port of `searchEmojis` (api4/emoji.go:286) — `POST /api/v4/emoji/search`.
///
/// The emoji picker posts this on every keystroke past the first.
///
/// # The only emoji route whose config refusal is a 403
///
/// There is no `EnableCustomEmoji` check in this handler, so
/// [`mm_app::App::search_emoji`]'s **403** is what a client sees — where the five routes beside
/// it answer 501 from their own handlers. See the module docs.
///
/// # Two 400s that a client cannot tell apart
///
/// A body that does not decode is `SetInvalidParamWithErr("term", …)` and an empty term is
/// `SetInvalidParam("term")` — the **same id and the same parameter name**, so `{"prefix_only":
/// true}`, `null`, `[]` and `not json` are one answer between them.
///
/// # The limit is not a parameter
///
/// `web.PerPageMaximum` — 200 — is passed as a literal. There is no `per_page` on this route, and
/// a client asking for more gets 200 rows regardless.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(emojis)` — a **trailing newline**, and `[]` rather than `null` for
/// no matches, because the store allocates.
#[tracing::instrument(skip_all, fields(prefix_only))]
pub async fn search_emojis(
    State(state): State<AppState>,
    // Extracted for the 401; this handler reads nothing from the session, because Go does not
    // either — there is no permission check on emoji search at all.
    _session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            ApiError::invalid_param("term")
        })?;

    // Decoded to a `Value` first, because **serde builds a struct from a JSON array
    // positionally** where Go's decoder refuses one: `["abc", true]` would otherwise become
    // `EmojiSearch { term: "abc", prefix_only: true }` and *search*, where Go answers 400.
    // Measured against the running server after this route shipped without the check.
    let decoded: serde_json::Value =
        mm_model::utils::decode_one_from_json(&bytes).map_err(|err| {
            tracing::debug!(error = %err, "emoji search body did not decode");
            ApiError::invalid_param("term")
        })?;
    let search: EmojiSearch = match decoded {
        // `Decode` into a non-pointer struct leaves the zero value for a JSON `null`, and the
        // empty-term check below is what refuses it.
        serde_json::Value::Null => EmojiSearch::default(),
        serde_json::Value::Object(map) => serde_json::from_value(serde_json::Value::Object(map))
            .map_err(|err| {
                tracing::debug!(error = %err, "emoji search body has the wrong field types");
                ApiError::invalid_param("term")
            })?,
        _ => return Err(ApiError::invalid_param("term")),
    };
    if search.term.is_empty() {
        return Err(ApiError::invalid_param("term"));
    }
    tracing::Span::current().record("prefix_only", search.prefix_only);

    let emojis = state
        .app
        .search_emoji(&search.term, search.prefix_only, PER_PAGE_MAXIMUM)
        .await?;

    let mut body = serde_json::to_vec(&emojis).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise emojis");
        ApiError::from(AppError::new(
            "searchEmojis",
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

/// `web.PerPageMaximum` (web/params.go:20).
const PER_PAGE_MAXIMUM: i64 = 200;

/// `GET /api/v4/emoji/search` — the same method fallthrough as [`get_emoji_name_literal`].
pub async fn get_emoji_search_literal(_session: AuthenticatedSession) -> Response {
    ApiError::invalid_url_param("emoji_id").into_response()
}

/// `GET /api/v4/emoji/names` — gorilla's method fallthrough, spelled out.
///
/// Go registers `/emoji/names` for `POST` only, so a `GET` fails the method match and gorilla
/// continues to `/emoji/{emoji_id}`, where `RequireEmojiId` rejects the literal `names`. axum
/// does not fall through: once `/api/v4/emoji/names` is a route, it answers every method, and
/// the `GET` would be forwarded rather than served. This restores the 400.
///
/// It is `RequireEmojiId`'s answer and nothing else — `getEmoji`'s `EnableCustomEmoji` 501 comes
/// *after* the id check, so a disabled server gives the same 400 here.
pub async fn get_emoji_name_literal(_session: AuthenticatedSession) -> Response {
    ApiError::invalid_url_param("emoji_id").into_response()
}

/// `GetEmojisByNamesMax` (api4/emoji.go:19).
const GET_EMOJIS_BY_NAMES_MAX: usize = 200;

/// Port of `getEmojisByNames` (api4/emoji.go:253) — `POST /api/v4/emoji/names`.
///
/// The webapp posts the emoji names it found in a page of posts, so this fires once per channel
/// load beside `POST /users/usernames`.
///
/// # Four refusals, and the config gate is *third*
///
/// 1. The body does not decode → 400 `api.payload.parse.error`.
/// 2. Zero names → 400 `invalid_body_param` naming `names`.
/// 3. `EnableCustomEmoji` off → **501** `api.emoji.disabled.app_error`.
/// 4. More than **200** names → 400 `api.emoji.get_multiple_by_name_too_many.request_error`.
///
/// The order matters on the wire: an empty body on a server with custom emoji disabled is the
/// 400, not the 501, and a 201-name body on that same server is the 501, not the too-many 400.
/// Both measured.
///
/// # System emoji are filtered out of the *request*
///
/// [`mm_app::App::get_multiple_emoji_by_name`] drops every name that is a built-in before
/// querying, so `["+1"]` answers `[]` — this route is about custom emoji only, and asking for a
/// built-in is not an error.
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode(emojis)` — a **trailing newline**, unlike the emoji reads beside
/// it, and `[]` rather than `null` for no matches, because both the store and the
/// filtered-to-nothing branch return an allocated empty slice.
#[tracing::instrument(skip_all, fields(asked))]
pub async fn get_emojis_by_names(
    State(state): State<AppState>,
    // Extracted for the 401 it raises; this handler reads nothing from the session, because Go
    // does not either — there is no permission check here at all.
    _session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            payload_parse_error()
        })?;

    let names = sorted_array_from_json(&bytes).map_err(|err| {
        tracing::debug!(error = %err, "emoji name body did not decode");
        payload_parse_error()
    })?;
    if names.is_empty() {
        return Err(ApiError::invalid_param("names"));
    }
    tracing::Span::current().record("asked", names.len());

    custom_emoji_enabled(&state, "getEmojisByNames")?;

    if names.len() > GET_EMOJIS_BY_NAMES_MAX {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "MaxNames".to_owned(),
            serde_json::Value::from(GET_EMOJIS_BY_NAMES_MAX),
        );
        return Err(ApiError::from(AppError::new(
            "getEmojisByNames",
            "api.emoji.get_multiple_by_name_too_many.request_error",
            Some(params),
            String::new(),
            400,
        )));
    }

    let emojis = state.app.get_multiple_emoji_by_name(&names).await?;

    let mut body = serde_json::to_vec(&emojis).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise emojis");
        ApiError::from(AppError::new(
            "getEmojisByNames",
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

/// `model.NewAppError("getEmojisByNames", model.PayloadParseError, nil, "", 400)`.
fn payload_parse_error() -> ApiError {
    ApiError::from(AppError::new(
        "getEmojisByNames",
        mm_model::utils::PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
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

/// `EmojiMaxAutocompleteItems` (api4/emoji.go:18).
const EMOJI_MAX_AUTOCOMPLETE_ITEMS: i64 = 100;

/// Port of `autocompleteEmojis` (api4/emoji.go:331), reached as
/// `GET /api/v4/emoji/autocomplete` — the `:` picker, which fires once per keystroke.
///
/// # It is the shortest handler in the file, and every line of it is a divergence from its
/// siblings
///
/// 1. **No `RequireEmojiId`**, because there is no path parameter.
/// 2. **No `EnableCustomEmoji` gate.** `getEmoji` and `getEmojiList` both open with one and
///    answer 501; this one does not, so the *app layer's* 403 (`api.emoji.disabled.app_error`,
///    same id, different status) is the one a client sees here. Unreachable on this deployment,
///    where the setting is on — see [`mm_app::App::search_emoji`].
/// 3. **An empty or absent `name` is a 400** `api.context.invalid_url_param.app_error` naming
///    `name` — `SetInvalidURLParam`, not the body-param variant, even though the value comes
///    from the query string.
/// 4. `SearchEmoji(name, prefixOnly = true, limit = 100)`.
///
/// # Prefix, case-sensitive, and `\` matches everything
///
/// `prefixOnly` makes the pattern `name%` rather than `%name%`, there is no `LOWER` on either
/// side, and the sanitiser strips backslashes before escaping — so `?name=MMRS` finds nothing
/// while `?name=mmrs` finds the list, and `?name=\` matches every emoji. All three measured; see
/// [`mm_store::emoji_store`].
///
/// # Wire format
///
/// `json.NewEncoder(w).Encode` over a `[]*model.Emoji` the store initialises to `[]`, so the
/// empty answer is `[]` and never `null`, with a trailing newline.
#[tracing::instrument(skip_all, fields(count))]
pub async fn autocomplete_emojis(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let _ = session;

    // `r.URL.Query().Get("name")` — absent and present-but-empty are the same empty string, and
    // Go tests the string rather than the presence.
    let name = query_first(query.as_deref(), "name").unwrap_or_default();
    if name.is_empty() {
        return Err(ApiError::invalid_url_param("name"));
    }

    let emojis = state
        .app
        .search_emoji(&name, true, EMOJI_MAX_AUTOCOMPLETE_ITEMS)
        .await?;
    tracing::Span::current().record("count", emojis.len());

    let mut body = serde_json::to_vec(&emojis).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the emoji completions");
        ApiError::from(AppError::new(
            "autocompleteEmojis",
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

    /// The literal list holds the `GET`-registered siblings of `{emoji_id}` that this router does
    /// **not** register itself. `autocomplete` left it when it became a route here — axum prefers
    /// a registered literal, so the router does the shadowing now — and `names` and `search` were
    /// never in it: they are POST-only in Go, so a GET falls through to `getEmoji` and 400s,
    /// which is what our `{emoji_id}` handler does too.
    ///
    /// Empty today. The list stays because it is the only thing standing between a future
    /// `GET /emoji/<literal>` of Go's and a 400 from a handler that thought it had an id.
    #[test]
    fn no_get_literal_is_both_shadowed_and_registered() {
        assert!(
            !EMOJI_SHADOWED_LITERALS.contains(&"autocomplete"),
            "autocomplete is a route of its own now; shadowing it would forward what we serve"
        );
        assert!(!EMOJI_SHADOWED_LITERALS.contains(&"names"));
        assert!(!EMOJI_SHADOWED_LITERALS.contains(&"search"));
    }
}
