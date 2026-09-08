//! The two bot reads: `getBot` (api4/bot.go:114) and `getBots` (bot.go:155).
//!
//! # The 404 is a security answer, not an error path
//!
//! A caller without `read_others_bots` who asks about someone else's bot gets
//! `store.sql_bot.get.missing.app_error` — **the same 404 an id that does not exist gets**, built
//! by the same `MakeBotNotFoundError`. Go's comment is explicit: "pretend like the bot doesn't
//! exist at all, to avoid revealing that the user is a bot." A 403 here would leak exactly what
//! the 404 exists to hide, so the refusal and the miss must stay byte-identical.
//!
//! The owner branch is the one that reads oddly and is real: a caller who **owns** the bot and
//! lacks `read_bots` also gets the 404, with Go's own comment calling it "kind of silly in this
//! case, since we created the bot".
//!
//! # `getBots` refuses; `getBot` hides
//!
//! The list route has no such concern — it answers a plain 403 naming `read_bots` — because there
//! is no id in the request to confirm or deny. Same file, two routes, opposite answers to "you
//! may not see this".
//!
//! # The permission decides the *filter*, not just admission
//!
//! `read_others_bots` sets `OwnerId = ""` (every bot); plain `read_bots` sets it to the caller's
//! own id. So the list is silently narrowed rather than refused, and a mutation swapping the two
//! branches shows up as a different page, not a different status.

use axum::extract::{Path, State};
use axum::http::header::IF_NONE_MATCH;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_model::bot::{BotGetOptions, make_bot_not_found_error};
use mm_model::permission::{
    PERMISSION_READ_BOTS, PERMISSION_READ_OTHERS_BOTS, make_permission_error,
};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_flag_is_true};
use crate::error::ApiError;

/// Port of `Context.HandleEtag` (web/context.go:230).
///
/// **The `ETag` header is written only on the 304.** Go sets `HeaderEtagServer` inside the
/// `if et == etag` branch and nowhere else, so a 200 from either bot route carries no `ETag` at
/// all — a client can only ever revalidate an etag it was given by some other route. Measured
/// against the running server, because "sets the header on the 200 too" is the intuitive reading
/// and it is wrong.
fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    !etag.is_empty()
        && headers
            .get(IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|sent| sent == etag)
}

fn not_modified(etag: &str) -> Response {
    (
        StatusCode::NOT_MODIFIED,
        [("ETag", etag), ("x-mmrs-served-by", "rust")],
    )
        .into_response()
}

/// `json.NewEncoder(w).Encode` — both routes, so both bodies end in a newline.
fn encoded_ok<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Response, ApiError> {
    let mut body = serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise bots");
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

/// Port of `getBot` (api4/bot.go:114) — `GET /api/v4/bots/{bot_user_id}`.
///
/// # The fetch happens before the permission check, and it has to
///
/// The decision depends on `bot.OwnerId`, so there is nothing to check until the row is loaded.
/// Unlike `getJob`, which has the same ordering, this leaks nothing: the refusal is the same 404
/// as the miss.
#[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, include_deleted, not_modified))]
pub async fn get_bot(
    State(state): State<AppState>,
    Path(bot_user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireBotUserId` (web/context.go:778) — the URL-param error, named `bot_user_id`.
    if !is_valid_id(&bot_user_id) {
        return Err(ApiError::invalid_url_param("bot_user_id"));
    }

    // `strconv.ParseBool` with the error discarded, so anything unparseable is false.
    let include_deleted = query_flag_is_true(query.as_deref(), "include_deleted");
    tracing::Span::current().record("include_deleted", include_deleted);

    let bot = state.app.get_bot(&bot_user_id, include_deleted).await?;

    // Go's three-armed `if`. The two refusing arms build the **same** error as a miss.
    if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_OTHERS_BOTS)
        .await
    {
        // Any bot.
    } else if bot.owner_id == session.0.user_id {
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_READ_BOTS)
            .await
        {
            return Err(ApiError::from(make_bot_not_found_error(
                "permissions",
                &bot_user_id,
            )));
        }
    } else {
        return Err(ApiError::from(make_bot_not_found_error(
            "permissions",
            &bot_user_id,
        )));
    }

    let etag = bot.etag();
    if etag_matches(&headers, &etag) {
        tracing::Span::current().record("not_modified", true);
        return Ok(not_modified(&etag));
    }
    tracing::Span::current().record("not_modified", false);

    encoded_ok(&bot, "getBot")
}

/// Port of `getBots` (api4/bot.go:155) — `GET /api/v4/bots`.
///
/// # `include_deleted` widens; `only_orphaned` narrows
///
/// The first drops the `DeleteAt = 0` clause; the second adds a join to the owner's `Users` row
/// and demands that it be **deleted**. They are independent, and `only_orphaned` is the one that
/// can empty a page that `include_deleted` just filled.
#[tracing::instrument(
    skip_all,
    fields(
        owner_id,
        include_deleted,
        only_orphaned,
        page,
        per_page,
        count,
        not_modified
    )
)]
pub async fn get_bots(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    headers: HeaderMap,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let include_deleted = query_flag_is_true(query.as_deref(), "include_deleted");
    let only_orphaned = query_flag_is_true(query.as_deref(), "only_orphaned");
    tracing::Span::current().record("include_deleted", include_deleted);
    tracing::Span::current().record("only_orphaned", only_orphaned);

    // The permission chooses the owner filter. An empty id means "every owner"; the caller's own
    // id means "mine only". A plain 403 — unlike `getBot`, there is no id here to hide.
    let owner_id = if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_OTHERS_BOTS)
        .await
    {
        String::new()
    } else if state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_BOTS)
        .await
    {
        session.0.user_id.clone()
    } else {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_BOTS],
        )));
    };
    tracing::Span::current().record("owner_id", &owner_id);

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    // `BotGetOptions` holds Go's `int`s; `web.ParamsFromRequest` has already floored `page` at 0
    // and clamped `per_page` to 200, so the narrowing cannot lose a reachable value.
    let options = BotGetOptions {
        owner_id,
        include_deleted,
        only_orphaned,
        page: page as i32,
        per_page: per_page as i32,
    };

    let bots = state.app.get_bots(&options).await?;
    tracing::Span::current().record("count", bots.0.len());

    let etag = bots.etag();
    if etag_matches(&headers, &etag) {
        tracing::Span::current().record("not_modified", true);
        return Ok(not_modified(&etag));
    }
    tracing::Span::current().record("not_modified", false);

    encoded_ok(&bots, "getBots")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers_with(etag: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(IF_NONE_MATCH, HeaderValue::from_str(etag).expect("ascii"));
        headers
    }

    /// An exact string comparison — no weak `W/` prefix, no quoting, no candidate list. Go's
    /// `HandleEtag` is `if et == etag`, and a port that used a real HTTP etag comparison would
    /// 304 requests Go answers with a body.
    #[test]
    fn the_etag_comparison_is_exact() {
        let etag = "11.11.0.rcw3d9njxiy6pquw79ux5wqxjw.1788459398643";
        assert!(etag_matches(&headers_with(etag), etag));
        assert!(!etag_matches(&headers_with(&format!("W/{etag}")), etag));
        assert!(!etag_matches(&headers_with(&format!("\"{etag}\"")), etag));
        assert!(!etag_matches(&HeaderMap::new(), etag));
    }

    /// An empty etag never matches, however the client asks. `Etag()` cannot produce one for a
    /// bot, but the guard is Go's and it is what keeps a header of `""` from 304ing.
    #[test]
    fn an_empty_etag_never_matches() {
        assert!(!etag_matches(&headers_with(""), ""));
    }

    /// The refusal and the miss are the **same document**. If these ever diverge, the route has
    /// become an oracle for which users are bots.
    #[test]
    fn the_permission_refusal_is_byte_identical_to_the_miss() {
        let id = "rcw3d9njxiy6pquw79ux5wqxjw";
        let refusal = make_bot_not_found_error("permissions", id);
        let miss = make_bot_not_found_error("SqlBotStore.Get", id);

        assert_eq!(refusal.id, miss.id);
        assert_eq!(refusal.status_code, miss.status_code);
        assert_eq!(refusal.params, miss.params);
        // `where` differs and is **not** on the wire — `AppError` skips it when serialising —
        // so the bodies a client sees are identical.
        let as_wire = |err: &AppError| {
            let mut value = serde_json::to_value(err).expect("serialises");
            value["request_id"] = serde_json::Value::String(String::new());
            value
        };
        assert_eq!(as_wire(&refusal), as_wire(&miss));
    }
}
