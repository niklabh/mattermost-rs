//! `channels/api4/bot.go`, all of it except `convertBotToUser`: the two reads — `getBot`
//! (:114) and `getBots` (:155) — and the five writes — `createBot` (:26), `patchBot` (:76),
//! `disableBot` (:190), `enableBot` (:194) and `assignBot` (:232).
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

use axum::extract::{Path, Request, State};
use axum::http::header::IF_NONE_MATCH;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use mm_model::bot::{Bot, BotGetOptions, BotPatch, make_bot_not_found_error};
use mm_model::permission::{
    PERMISSION_ASSIGN_BOT, PERMISSION_CREATE_BOT, PERMISSION_READ_BOTS,
    PERMISSION_READ_OTHERS_BOTS, make_permission_error,
};
use mm_model::session::Session;
use mm_model::utils::{AppError, decode_one_from_json, is_valid_id};

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

/// `json.NewEncoder(w).Encode` — every route in this file, so every body ends in a newline.
///
/// The status is a parameter for `createBot` alone: it is the only one of the seven that writes
/// a header before encoding, and it writes **201**.
fn encoded<T: serde::Serialize>(
    status: StatusCode,
    value: &T,
    where_: &'static str,
) -> Result<Response, ApiError> {
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
        status,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        body,
    )
        .into_response())
}

/// [`encoded`] at 200, which is what the two reads and three of the four writes send.
fn encoded_ok<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Response, ApiError> {
    encoded(StatusCode::OK, value, where_)
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

// =================================================================================================
// The write half of api4/bot.go.
// =================================================================================================

/// Port of `createBot` (api4/bot.go:26) — `POST /api/v4/bots`.
///
/// # Four gates, in an order that is visible from outside
///
/// 1. the body decodes (400 `invalid_body_param`, `Name: "bot"`);
/// 2. `create_bot` (403 naming the permission);
/// 3. the caller is not itself a bot (**the same 403**, naming the same permission — a bot cannot
///    create bots, and it is told so as if it simply lacked the right);
/// 4. `ServiceSettings.EnableBotAccountCreation` (403 `api.bot.create_disabled`).
///
/// The config check is **last**, so a caller without `create_bot` on a server with bot creation
/// disabled gets the permission error and never learns the feature is off. Swapping (2) and (4)
/// passes every single-gate test and changes what an unprivileged caller is told.
///
/// # This deployment refuses every call
///
/// `EnableBotAccountCreation` defaults to `false` (config.go:917) and the stack leaves it there
/// — `scripts/stack.sh` plants its seeded bots straight into the tables for that reason. So gate
/// (4) is the reachable answer here and the success path below is exercised by
/// `mm_store`'s own database tests rather than end to end. See [D-280].
///
/// # `OwnerId` is the session, never the body
///
/// The bot is built with `OwnerId` set from the session and *then* patched, and `BotPatch` has no
/// owner field — so a caller cannot create a bot owned by someone else. `assignBot` is the only
/// way to move one.
#[tracing::instrument(skip_all, fields(bot_user_id))]
pub async fn create_bot(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("bot").into_response();
        }
    };

    // `json.NewDecoder(r.Body).Decode(&botPatch)` into a `*model.BotPatch`: a body of `null`
    // decodes successfully into a nil pointer, which Go's `err != nil || botPatch == nil` then
    // rejects with the same 400 a malformed body gets.
    let patch: BotPatch = match decode_one_from_json::<Option<BotPatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) => return ApiError::invalid_param("bot").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "bot patch body did not decode");
            return ApiError::invalid_param("bot").into_response();
        }
    };

    let mut bot = Bot {
        owner_id: session.0.user_id.clone(),
        ..Bot::default()
    };
    bot.patch(&patch);

    match serve_create_bot(&state, &session.0, &bot).await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

async fn serve_create_bot(
    state: &AppState,
    session: &Session,
    bot: &Bot,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(session, &PERMISSION_CREATE_BOT)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&PERMISSION_CREATE_BOT],
        )));
    }

    // `if user, err := c.App.GetUser(...); err == nil` — a *failed* lookup is ignored, so a
    // session whose user row has vanished falls through to the create rather than being refused.
    if let Ok(user) = state.app.get_user(&session.user_id).await
        && user.is_bot
    {
        return Err(ApiError::from(make_permission_error(
            session,
            &[&PERMISSION_CREATE_BOT],
        )));
    }

    if !state.app.config().enable_bot_account_creation {
        return Err(ApiError::from(AppError::new(
            "createBot",
            "api.bot.create_disabled",
            None,
            String::new(),
            403,
        )));
    }

    let created = state.app.create_bot(bot).await?;
    tracing::Span::current().record("bot_user_id", &created.user_id);

    // `w.WriteHeader(http.StatusCreated)` before the encoder, so the body still carries the
    // encoder's trailing newline.
    encoded(StatusCode::CREATED, &created, "createBot")
}

/// Port of `patchBot` (api4/bot.go:76) — `PUT /api/v4/bots/{bot_user_id}`.
///
/// The permission check runs **after** the body is decoded, so a malformed body on a bot the
/// caller may not touch is a 400 and not the 404 that hides the bot's existence. Go's ordering,
/// and it is the one place on this family where the hiding rule does not apply.
#[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id))]
pub async fn patch_bot(
    State(state): State<AppState>,
    Path(bot_user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    if !is_valid_id(&bot_user_id) {
        return ApiError::invalid_url_param("bot_user_id").into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("bot").into_response();
        }
    };

    let patch: BotPatch = match decode_one_from_json::<Option<BotPatch>>(&bytes) {
        Ok(Some(patch)) => patch,
        Ok(None) => return ApiError::invalid_param("bot").into_response(),
        Err(err) => {
            tracing::debug!(error = %err, "bot patch body did not decode");
            return ApiError::invalid_param("bot").into_response();
        }
    };

    let served = async {
        state
            .app
            .session_has_permission_to_manage_bot(&session.0, &bot_user_id)
            .await?;
        let patched = state.app.patch_bot(&bot_user_id, &patch).await?;
        encoded(StatusCode::OK, &patched, "patchBot")
    };
    match served.await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `disableBot` (api4/bot.go:190) — `POST /api/v4/bots/{bot_user_id}/disable`.
///
/// Both this and [`enable_bot`] are one line in Go calling `updateBotActive`; the flag is the
/// only difference and it is the one a mutation flips.
#[tracing::instrument(skip_all)]
pub async fn disable_bot(
    state: State<AppState>,
    path: Path<String>,
    session: AuthenticatedSession,
) -> Response {
    update_bot_active(state, path, session, false).await
}

/// Port of `enableBot` (api4/bot.go:194) — `POST /api/v4/bots/{bot_user_id}/enable`.
#[tracing::instrument(skip_all)]
pub async fn enable_bot(
    state: State<AppState>,
    path: Path<String>,
    session: AuthenticatedSession,
) -> Response {
    update_bot_active(state, path, session, true).await
}

/// Port of `updateBotActive` (api4/bot.go:200).
///
/// There is no request body — Go's handler takes `_ *http.Request` — so anything posted is
/// ignored rather than rejected.
#[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, active))]
async fn update_bot_active(
    State(state): State<AppState>,
    Path(bot_user_id): Path<String>,
    session: AuthenticatedSession,
    active: bool,
) -> Response {
    if !is_valid_id(&bot_user_id) {
        return ApiError::invalid_url_param("bot_user_id").into_response();
    }

    let served = async {
        state
            .app
            .session_has_permission_to_manage_bot(&session.0, &bot_user_id)
            .await?;
        let bot = state.app.update_bot_active(&bot_user_id, active).await?;
        encoded(StatusCode::OK, &bot, "updateBotActive")
    };
    match served.await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
}

/// Port of `assignBot` (api4/bot.go:232) — `POST /bots/{bot_user_id}/assign/{user_id}`.
///
/// # `me` resolves, and the bot id does not
///
/// `RequireUserId` substitutes the session's own id for the literal `me` (web/context.go:301);
/// `RequireBotUserId` has no such rule. So `/assign/me` works and `/bots/me/assign/{id}` is a 400.
///
/// # Both id checks run before the permission check, and `user_id` is checked first
///
/// A request with two malformed ids reports `user_id`, because Go's `RequireBotUserId` returns
/// early once `c.Err` is set. Reversing the two is invisible to any test that sends only one bad
/// id.
///
/// # The new owner need not exist
///
/// The only thing asked of `user_id` is that it is not a **bot** — and that check is
/// `if user, err := GetUser(...); err == nil`, so an id matching no user passes it and becomes
/// the owner. Go's own looseness: `Bots.OwnerId` legitimately holds plugin ids too.
#[tracing::instrument(skip_all, fields(bot_user_id = %bot_user_id, user_id))]
pub async fn assign_bot(
    State(state): State<AppState>,
    Path((bot_user_id, user_id)): Path<(String, String)>,
    session: AuthenticatedSession,
) -> Response {
    // `RequireUserId` first, and it rewrites `me` before validating.
    let user_id = if user_id == mm_model::user::ME {
        session.0.user_id.clone()
    } else {
        user_id
    };
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }
    if !is_valid_id(&bot_user_id) {
        return ApiError::invalid_url_param("bot_user_id").into_response();
    }
    tracing::Span::current().record("user_id", &user_id);

    let served = async {
        state
            .app
            .session_has_permission_to_manage_bot(&session.0, &bot_user_id)
            .await?;

        if let Ok(user) = state.app.get_user(&user_id).await
            && user.is_bot
        {
            // `PermissionAssignBot`, which is **not** the permission that got us this far: the
            // gate above is `manage_bots`/`manage_others_bots`. Nothing ever checks `assign_bot`
            // as a permission on this route — it appears only as the name in this refusal.
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_ASSIGN_BOT],
            )));
        }

        let bot = state.app.update_bot_owner(&bot_user_id, &user_id).await?;
        encoded(StatusCode::OK, &bot, "assignBot")
    };
    match served.await {
        Ok(response) => response,
        Err(err) => err.into_response(),
    }
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

    /// **`createBot` is the only 201 in this file**, and the newline is on the 201 too.
    ///
    /// Go writes the status header and *then* runs `json.NewEncoder(w).Encode`, so a created bot
    /// ends in `\n` exactly like the six 200s do. A port that special-cased the create and
    /// returned a bare `Json` would drop the byte.
    #[tokio::test]
    async fn the_create_answers_201_and_still_ends_in_a_newline() {
        let bot = mm_model::bot::Bot {
            user_id: "rcw3d9njxiy6pquw79ux5wqxjw".to_owned(),
            username: "created".to_owned(),
            owner_id: "ad4rbf7zabbt9gpw1186iwm9ir".to_owned(),
            create_at: 1788459398643,
            update_at: 1788459398643,
            ..Default::default()
        };

        let created = encoded(StatusCode::CREATED, &bot, "createBot").expect("encodes");
        assert_eq!(created.status(), StatusCode::CREATED);

        let patched = encoded_ok(&bot, "patchBot").expect("encodes");
        assert_eq!(
            patched.status(),
            StatusCode::OK,
            "every other write is a 200"
        );

        // The body of both, read back.
        let body = axum::body::to_bytes(created.into_body(), usize::MAX)
            .await
            .expect("reads");
        assert!(body.ends_with(b"\n"), "the encoder's newline");
        let decoded: mm_model::bot::Bot = serde_json::from_slice(&body).expect("round-trips");
        assert_eq!(decoded, bot);
        // A zero-valued bot omits the `omitempty` trio, and the create path must not fill them in.
        let value: serde_json::Value = serde_json::from_slice(&body).expect("decodes");
        let object = value.as_object().expect("an object");
        assert!(!object.contains_key("display_name"));
        assert!(!object.contains_key("description"));
        assert!(!object.contains_key("last_icon_update"));
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
