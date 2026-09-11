//! The four personal-access-token reads: `getUserAccessTokens` (api4/user.go:3080),
//! `countNonCompliantUserAccessTokens` (:3103), `getUserAccessTokensForUser` (:3155) and
//! `getUserAccessToken` (:3188).
//!
//! # Four routes, four different permission rules
//!
//! Nothing here is shared, and each difference decides who can read someone else's credentials:
//!
//! | route | gate |
//! |---|---|
//! | `GET /users/tokens` | `manage_system` alone — the whole installation's tokens |
//! | `GET /users/tokens/non_compliant/count` | `manage_system` alone |
//! | `GET /users/{user_id}/tokens` | `read_user_access_token`, **then** `SessionHasPermissionToUserOrBot` |
//! | `GET /users/tokens/{token_id}` | `read_user_access_token`, then the same user-or-bot check — **after** the fetch, on the token's owner |
//!
//! The last row is the one a port gets wrong. The id in the URL is the *token's*, so who the
//! caller is being checked against is not known until the row is loaded — which means a caller
//! holding `read_user_access_token` and nothing else learns whether a token id exists (404) before
//! being refused (403). That is Go's ordering and it is reproduced.
//!
//! # The secret is cleared in the app layer, on every one of them
//!
//! The store selects `Token` because `GetByToken` authenticates with it. `mm_app::user_access_token`
//! blanks it, and because the field carries `omitempty` the cleared secret is an **absent key**
//! rather than an empty string.
//!
//! # Three encoders' worth of newline
//!
//! `getUserAccessToken` uses `json.NewEncoder(w).Encode` and ends in a newline; the other three use
//! `json.Marshal` + `w.Write` and do not. Same file, same wire type.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_CREATE_USER_ACCESS_TOKEN, PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM,
    PERMISSION_READ_USER_ACCESS_TOKEN, PERMISSION_REVOKE_USER_ACCESS_TOKEN, Permission,
    make_permission_error,
};
// **`UserAccessTokenSearch` lives in `search_requests`, not next to `UserAccessToken`.** Go
// declares it in its own file (`user_access_token_search.go`) and this port grouped it with
// `EmojiSearch` for that reason; a second copy beside the token type would be a silent fork of a
// wire type.
use mm_model::search_requests::UserAccessTokenSearch;
use mm_model::user_access_token::{NonCompliantUserAccessTokenResult, UserAccessToken};
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page};
use crate::error::ApiError;

/// The 200 the three `json.Marshal` routes return — **no trailing newline**.
fn json_ok(body: Vec<u8>) -> Response {
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

/// `json.Marshal`, with the id Go uses when it fails.
///
/// **`where` is `searchUserAccessTokens` on two of the three list routes** — a copy-paste in Go
/// (user.go:3094, :3179) that names a handler neither of them is. Not on the wire, and reproduced
/// so the source keeps saying what Go says.
fn encode<T: serde::Serialize>(value: &T, where_: &'static str) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise access tokens");
        ApiError::from(AppError::new(
            where_,
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// Port of `getUserAccessTokens` (user.go:3080) — `GET /api/v4/users/tokens`.
///
/// Every token on the installation, gated on `manage_system` alone. The store's query has **no
/// `ORDER BY`**, so the page order is Postgres's and is not a parity property; see
/// [`mm_store::UserAccessTokenStore`].
#[tracing::instrument(skip_all, fields(page, per_page, count))]
pub async fn get_user_access_tokens(
    State(state): State<AppState>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let tokens = state.app.get_user_access_tokens(page, per_page).await?;
    tracing::Span::current().record("count", tokens.len());

    Ok(json_ok(encode(&tokens, "searchUserAccessTokens")?))
}

/// Port of `countNonCompliantUserAccessTokens` (user.go:3103) —
/// `GET /api/v4/users/tokens/non_compliant/count`.
///
/// On a stock server this reads **nothing**: the lifetime policy is off, so the app layer returns
/// zero before the query. The body is `{"count":0}` — one key, and `Count` has no `omitempty`, so
/// zero is present rather than omitted.
#[tracing::instrument(skip_all, fields(count))]
pub async fn count_non_compliant_user_access_tokens(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        )));
    }

    let count = state.app.count_non_compliant_user_access_tokens().await?;
    tracing::Span::current().record("count", count);

    let result = NonCompliantUserAccessTokenResult { count };
    Ok(json_ok(encode(
        &result,
        "countNonCompliantUserAccessTokens",
    )?))
}

/// Port of `getUserAccessTokensForUser` (user.go:3155) — `GET /api/v4/users/{user_id}/tokens`.
///
/// # Two gates, and the second one reports a permission it never checked
///
/// `read_user_access_token` first, then `SessionHasPermissionToUserOrBot` — whose refusal is
/// `SetPermissionError(PermissionEditOtherUsers)`, a permission that check does not itself consult
/// in the branch that usually denies. Reproduced: the error names `edit_other_users`.
///
/// **`me` is not resolved here.** `RequireUserId` does resolve it (web/context.go:301), so
/// `/users/me/tokens` works — the resolution happens before validation, as everywhere else.
#[tracing::instrument(skip_all, fields(user_id = %user_id, page, per_page, count))]
pub async fn get_user_access_tokens_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let user_id = crate::channels::resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return Err(ApiError::invalid_url_param("user_id"));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_USER_ACCESS_TOKEN)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_USER_ACCESS_TOKEN],
        )));
    }

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());
    tracing::Span::current().record("page", page);
    tracing::Span::current().record("per_page", per_page);

    let tokens = state
        .app
        .get_user_access_tokens_for_user(&user_id, page, per_page)
        .await?;
    tracing::Span::current().record("count", tokens.len());

    Ok(json_ok(encode(&tokens, "searchUserAccessTokens")?))
}

/// Port of `getUserAccessToken` (user.go:3188) — `GET /api/v4/users/tokens/{token_id}`.
///
/// **The fetch sits between the two permission checks.** `read_user_access_token` gates the route,
/// the token is loaded, and only then is the caller checked against the token's *owner*. A caller
/// with the first permission and not the second therefore gets a **404** for an id that does not
/// exist and a **403** for one that does — an existence oracle over token ids, and Go's.
///
/// This is the one route of the four whose body ends in a newline.
#[tracing::instrument(skip_all, fields(token_id = %token_id))]
pub async fn get_user_access_token(
    State(state): State<AppState>,
    Path(token_id): Path<String>,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    // `RequireTokenId` (web/context.go) — the URL-param error.
    if !is_valid_id(&token_id) {
        return Err(ApiError::invalid_url_param("token_id"));
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_READ_USER_ACCESS_TOKEN)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_READ_USER_ACCESS_TOKEN],
        )));
    }

    // `sanitize: true` — the handler's own argument, and the reason the secret never reaches a
    // client through this route while the revocation paths still see it.
    let token: UserAccessToken = state.app.get_user_access_token(&token_id, true).await?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &token.user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    let mut body = encode(&token, "getUserAccessToken")?;
    body.push(b'\n');
    Ok(json_ok(body))
}

/// Port of `web.ReturnStatusOK` (web/web.go:127) — `w.Write(MapToJSON(...))`, so **no trailing
/// newline**, unlike the two routes here that answer with `json.NewEncoder`.
fn status_ok() -> Response {
    (
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        r#"{"status":"OK"}"#,
    )
        .into_response()
}

/// `c.SetPermissionError(p); c.Err.DetailedError += ", attempted access by oauth app"`.
///
/// Five of the seven write handlers carry this. `MakePermissionError` has already filled
/// `DetailedError` with `userId=…, permission=…` and this concatenates onto it — which reaches a
/// **client** only in developer mode, because `handleContextError` calls `WipeDetailed()` unless
/// `ServiceSettings.EnableDeveloper` is set (web/handlers.go:436). It always reaches the server
/// log, and developer mode puts it back on the wire, so the bytes are reproduced.
///
/// Which permission is named differs by handler — `revoke_user_access_token` for revoke and
/// disable, `create_user_access_token` for create, enable and rotate — even though the branch has
/// nothing to do with permissions.
fn oauth_refusal(
    session: &mm_model::session::Session,
    permission: &'static Permission,
) -> ApiError {
    let mut err = make_permission_error(session, &[permission]);
    err.detailed_error
        .push_str(", attempted access by oauth app");
    ApiError::from(err)
}

/// Port of `json.NewDecoder(r.Body).Decode(&v)` **into a struct**, which is not what
/// `serde_json::from_slice` does on three counts. All three are reachable from a client and all
/// three change the 400 a malformed body gets.
///
/// - **A JSON array is an error in Go and is not in serde.** serde's derived `Deserialize`
///   accepts a sequence for a struct (positional fields), so `[]` decodes to a default-filled
///   struct and the handler carries on; Go answers `cannot unmarshal array into Go value`. This
///   was measured, not theorised — the first version of these handlers accepted `[]` on rotate.
/// - **A JSON `null` is *not* an error in Go.** `Decode` leaves the target untouched and returns
///   nil, so `null` reaches the handler as a zero-valued struct — which on rotate means an empty
///   `token_id`, i.e. `Name: "token_id"` rather than `Name: "rotate_user_access_token"`. serde
///   rejects `null` for a struct outright.
/// - **Trailing bytes after the first value are ignored.** `Decode` reads one value off the
///   stream and stops; `from_slice` requires the input to be exactly one value.
///
/// Everything else — a string, a number, a bool, an empty body, a field of the wrong type — is an
/// error on both sides.
fn decode_go_struct<T: serde::de::DeserializeOwned + Default>(bytes: &[u8]) -> Result<T, String> {
    let first = serde_json::Deserializer::from_slice(bytes)
        .into_iter::<serde_json::Value>()
        .next();

    match first {
        // Go's `Decode` on an empty body is `io.EOF`, which is still an error.
        None => Err("EOF".to_owned()),
        Some(Err(err)) => Err(err.to_string()),
        Some(Ok(serde_json::Value::Null)) => Ok(T::default()),
        Some(Ok(value @ serde_json::Value::Object(_))) => {
            serde_json::from_value(value).map_err(|err| err.to_string())
        }
        Some(Ok(other)) => Err(format!(
            "cannot unmarshal {} into a struct",
            match other {
                serde_json::Value::Array(_) => "array",
                serde_json::Value::String(_) => "string",
                serde_json::Value::Bool(_) => "bool",
                _ => "number",
            }
        )),
    }
}

/// Port of `model.MapFromJSON` (utils.go:507) — **every** decode failure is an empty map.
///
/// The three single-token routes read `token_id` out of this, so a body that is not an object, or
/// not JSON at all, is indistinguishable from `{}` and lands on the empty-`token_id` path below.
fn map_from_json(bytes: &[u8]) -> std::collections::HashMap<String, String> {
    serde_json::from_slice(bytes).unwrap_or_default()
}

/// Port of `createUserAccessToken` (user.go:2970) — `POST /api/v4/users/{user_id}/tokens`.
///
/// # This is the one response that carries the secret
///
/// The app layer mints `Token` and does **not** blank it, so this 200 carries a `token` key
/// holding a live credential. Every later read of the same row omits that key (`omitempty` over a
/// cleared string), so a client that does not store the value here can never recover it.
///
/// # Nine gates, in an order that is itself the API
///
/// `RequireUserId`, the user fetch, `IsRemote`, `IsOAuth`, the body decode, the empty
/// `description`, `create_user_access_token`, `SessionHasPermissionToUserOrBot`, and finally
/// "target is a system admin and caller lacks `manage_system`". Two consequences worth naming:
/// the user is fetched **before** the body is read, so a bad body against a nonexistent user is a
/// 404 and not a 400; and the `description` check precedes every permission check, so an
/// unprivileged caller learns their body was malformed before they learn they are refused.
///
/// # `UserId` and `Token` in the body are overwritten
///
/// `accessToken.UserId = c.Params.UserId` and `accessToken.Token = ""` — so a body naming another
/// user, or supplying its own secret, is silently ignored rather than refused. `is_active` and
/// `id` from the body are discarded too, by `PreSave` in the store.
#[tracing::instrument(skip_all, fields(user_id = %user_id, token_id))]
pub async fn create_user_access_token(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let user_id = crate::channels::resolve_me(&user_id, &session).to_owned();
    if !is_valid_id(&user_id) {
        return ApiError::invalid_url_param("user_id").into_response();
    }

    let user = match state.app.get_user(&user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // Remote (synthetic) users are refused with a **permission** error naming a permission the
    // caller may well hold — the branch is about the target, not the caller.
    if user.is_remote() {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_USER_ACCESS_TOKEN],
        ))
        .into_response();
    }

    if session.0.is_oauth {
        return oauth_refusal(&session.0, &PERMISSION_CREATE_USER_ACCESS_TOKEN).into_response();
    }

    let bytes = match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            return ApiError::invalid_param("user_access_token").into_response();
        }
    };
    // `json.NewDecoder(r.Body).Decode(&accessToken)` — a typed decode, so unlike the `MapFromJSON`
    // routes a malformed body **is** a 400 here.
    let mut access_token: UserAccessToken = match decode_go_struct(&bytes) {
        Ok(token) => token,
        Err(err) => {
            tracing::debug!(error = %err, "the user_access_token body did not decode");
            return ApiError::invalid_param("user_access_token").into_response();
        }
    };

    if access_token.description.is_empty() {
        return ApiError::invalid_param("description").into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_CREATE_USER_ACCESS_TOKEN)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_USER_ACCESS_TOKEN],
        ))
        .into_response();
    }

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &user_id)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    // Minting a token for a system admin needs `manage_system` on top of everything above —
    // otherwise a user-manager role could escalate by issuing an admin a credential it then reads.
    if user.is_system_admin()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    access_token.user_id = user_id;
    access_token.token = String::new();

    let token = match state.app.create_user_access_token(access_token).await {
        Ok(token) => token,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("token_id", &token.id);

    encoded_with_newline(&token, "createUserAccessToken")
}

/// The three single-token lifecycle routes — `revoke` (user.go:3215), `disable` (:3265) and
/// `enable` (:3316) — differ in exactly three places, and this is the shape they share.
///
/// # An empty `token_id` does **not** answer 400
///
/// All three do `if tokenId == "" { c.SetInvalidParam("token_id") }` **without returning**, so the
/// 400 they set is overwritten by whatever comes next: the OAuth refusal, the permission refusal,
/// or — for a caller who passes both — the **404** from looking up a token whose id is the empty
/// string. The 400 is therefore unreachable on every one of the three, and a port that returned
/// early there would answer 400 where Go answers 404. Reproduced by not returning.
///
/// # The token is fetched unsanitised
///
/// `GetUserAccessToken(tokenId, false)`, because the secret is what links the row to the session
/// it minted. Contrast `getUserAccessToken`, the read route, which passes `true`.
///
/// # The owner check happens after the fetch
///
/// So a caller holding the gating permission but not `edit_other_users` gets a 404 for an id that
/// does not exist and a 403 for one that does — the same existence oracle the single-token read
/// has.
async fn token_lifecycle(
    state: &AppState,
    session: &AuthenticatedSession,
    bytes: &[u8],
    gate: &'static Permission,
) -> Result<UserAccessToken, ApiError> {
    let props = map_from_json(bytes);
    let token_id = props
        .get("token_id")
        .map(String::as_str)
        .unwrap_or_default();

    if session.0.is_oauth {
        return Err(oauth_refusal(&session.0, gate));
    }

    if !state.app.session_has_permission_to(&session.0, gate).await {
        return Err(ApiError::from(make_permission_error(&session.0, &[gate])));
    }

    let token = state.app.get_user_access_token(token_id, false).await?;

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &token.user_id)
        .await
    {
        return Err(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        )));
    }

    Ok(token)
}

/// Read the whole body, answering Go's invalid-param error if the stream breaks.
///
/// The parameter name is the handler's own: `user_access_token_search` for the search route,
/// `rotate_user_access_token` for rotate, and for the `MapFromJSON` routes there is no such
/// branch in Go at all — an unreadable body there is simply an empty map.
async fn read_body(request: axum::extract::Request, parameter: &str) -> Result<Vec<u8>, ApiError> {
    match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => Ok(bytes.to_vec()),
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            Err(ApiError::invalid_param(parameter))
        }
    }
}

/// Port of `revokeUserAccessToken` (user.go:3215) — `POST /api/v4/users/tokens/revoke`.
///
/// Gated on `revoke_user_access_token`. The row and **every session minted from it** go together;
/// see [`mm_app::App::revoke_user_access_token`] for where that happens and why it is one
/// transaction.
#[tracing::instrument(skip_all)]
pub async fn revoke_user_access_token(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let bytes = match read_body(request, "token_id").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let token = match token_lifecycle(
        &state,
        &session,
        &bytes,
        &PERMISSION_REVOKE_USER_ACCESS_TOKEN,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };

    match state.app.revoke_user_access_token(&token.id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `disableUserAccessToken` (user.go:3265) — `POST /api/v4/users/tokens/disable`.
///
/// Gated on `revoke_user_access_token` — Go's comment says "no separate permission for this action
/// for now". The row survives with `is_active = false`; the sessions do not.
#[tracing::instrument(skip_all)]
pub async fn disable_user_access_token(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let bytes = match read_body(request, "token_id").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let token = match token_lifecycle(
        &state,
        &session,
        &bytes,
        &PERMISSION_REVOKE_USER_ACCESS_TOKEN,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };

    match state.app.disable_user_access_token(&token.id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `enableUserAccessToken` (user.go:3316) — `POST /api/v4/users/tokens/enable`.
///
/// **Gated on `create_user_access_token`, not `revoke_…`** — the mirror of disable is guarded by
/// the mirror of *create*, because re-enabling a token hands back a working credential. A port
/// that used the same permission for both halves of the pair would let a revoker re-arm what they
/// disabled.
#[tracing::instrument(skip_all)]
pub async fn enable_user_access_token(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let bytes = match read_body(request, "token_id").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let token = match token_lifecycle(
        &state,
        &session,
        &bytes,
        &PERMISSION_CREATE_USER_ACCESS_TOKEN,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => return err.into_response(),
    };

    match state.app.enable_user_access_token(&token.id).await {
        Ok(()) => status_ok(),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// The body of `rotateUserAccessToken` — an **anonymous struct declared in the handler**
/// (user.go:3368), not a model type, which is why it has no fixture.
///
/// `expires_at` has no `omitempty` and is absent from most clients' bodies; Go's decoder leaves it
/// zero, and zero means "never expires" — which the expiry validator then accepts or refuses
/// depending on whether a lifetime policy is configured.
#[derive(Debug, Default, serde::Deserialize)]
struct RotateBody {
    #[serde(rename = "token_id", default)]
    token_id: String,
    #[serde(rename = "expires_at", default)]
    expires_at: i64,
}

/// Port of `rotateUserAccessToken` (user.go:3367) — `POST /api/v4/users/tokens/rotate`.
///
/// # This one **does** return early on an empty `token_id`
///
/// Unlike revoke, disable and enable, the empty check here is followed by `return`, so the 400 is
/// real and reachable. Same family, same-looking three lines, opposite outcome.
///
/// # Five checks the other three do not have
///
/// After the owner check it fetches the user and refuses a system-admin target without
/// `manage_system`, refuses a remote user, and refuses an **inactive token** with
/// `api.user.rotate_user_access_token.disabled_token.app_error` at 400 — the only `api.` id in the
/// family, and a 400 rather than the 403s around it. A disabled token must be enabled before it
/// can be rotated.
///
/// The response carries the **new secret**, like creation and like nothing else.
#[tracing::instrument(skip_all, fields(token_id, expires_at))]
pub async fn rotate_user_access_token(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    let bytes = match read_body(request, "rotate_user_access_token").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let props: RotateBody = match decode_go_struct(&bytes) {
        Ok(props) => props,
        Err(err) => {
            tracing::debug!(error = %err, "the rotate_user_access_token body did not decode");
            return ApiError::invalid_param("rotate_user_access_token").into_response();
        }
    };
    tracing::Span::current().record("token_id", &props.token_id);
    tracing::Span::current().record("expires_at", props.expires_at);

    if props.token_id.is_empty() {
        return ApiError::invalid_param("token_id").into_response();
    }

    if session.0.is_oauth {
        return oauth_refusal(&session.0, &PERMISSION_CREATE_USER_ACCESS_TOKEN).into_response();
    }

    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_CREATE_USER_ACCESS_TOKEN)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_USER_ACCESS_TOKEN],
        ))
        .into_response();
    }

    let token = match state
        .app
        .get_user_access_token(&props.token_id, false)
        .await
    {
        Ok(token) => token,
        Err(err) => return ApiError::from(err).into_response(),
    };

    // **The user fetch is not optional here.** Revoke, disable and enable fetch the owner only to
    // decorate an audit record and ignore the error; rotate needs it for the two checks below, so
    // a token whose owner row is gone is a 404 on this route and a success on those three.
    let user = match state.app.get_user(&token.user_id).await {
        Ok(user) => user,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if !state
        .app
        .session_has_permission_to_user_or_bot(&session.0, &token.user_id)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    if user.is_system_admin()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    if user.is_remote() {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_CREATE_USER_ACCESS_TOKEN],
        ))
        .into_response();
    }

    if !token.is_active {
        return ApiError::from(AppError::new(
            "rotateUserAccessToken",
            "api.user.rotate_user_access_token.disabled_token.app_error",
            None,
            String::new(),
            400,
        ))
        .into_response();
    }

    match state
        .app
        .rotate_user_access_token(token, props.expires_at)
        .await
    {
        Ok(rotated) => encoded_with_newline(&rotated, "rotateUserAccessToken"),
        Err(err) => ApiError::from(err).into_response(),
    }
}

/// Port of `searchUserAccessTokens` (user.go:3046) — `POST /api/v4/users/tokens/search`.
///
/// `manage_system` alone, like the installation-wide list. The results are **sanitised**, so this
/// is a way to find a token id, never a secret.
///
/// # The term is not a pattern
///
/// `sanitizeSearchTerm` escapes the caller's `%` and `_`, and nothing wraps the term, so the
/// three `LIKE`s are equalities against a token id, a user id and a username. `seed` does not
/// find `seed-bot` and `%` finds nothing. See [`mm_store::UserAccessTokenStore::search`] — this
/// is the single most likely thing for a port to "fix" and thereby diverge on.
#[tracing::instrument(skip_all, fields(count))]
pub async fn search_user_access_tokens(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: axum::extract::Request,
) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let bytes = match read_body(request, "user_access_token_search").await {
        Ok(bytes) => bytes,
        Err(err) => return err.into_response(),
    };
    let props: UserAccessTokenSearch = match decode_go_struct(&bytes) {
        Ok(props) => props,
        Err(err) => {
            tracing::debug!(error = %err, "the user_access_token_search body did not decode");
            return ApiError::invalid_param("user_access_token_search").into_response();
        }
    };

    if props.term.is_empty() {
        return ApiError::invalid_param("term").into_response();
    }

    let tokens = match state.app.search_user_access_tokens(&props.term).await {
        Ok(tokens) => tokens,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("count", tokens.len());

    match encode(&tokens, "searchUserAccessTokens") {
        Ok(body) => json_ok(body),
        Err(err) => err.into_response(),
    }
}

/// Port of `revokeNonCompliantUserAccessTokens` (user.go:3126) —
/// `POST /api/v4/users/tokens/non_compliant/revoke`.
///
/// The destructive twin of `…/non_compliant/count`, and the two disagree about the disabled
/// policy: the count answers `{"count":0}` having read nothing, this answers **400**
/// (`app.user_access_token.revoke_non_compliant.no_policy.app_error`). On a stock server, where
/// `MaximumPersonalAccessTokenLifetimeDays` is 0, that 400 is the only answer this route gives.
///
/// The success body is the same `{"count":N}` wrapper, `N` being tokens deleted rather than users
/// affected.
#[tracing::instrument(skip_all, fields(count))]
pub async fn revoke_non_compliant_user_access_tokens(
    State(state): State<AppState>,
    session: AuthenticatedSession,
) -> Response {
    if !state
        .app
        .session_has_permission_to(&session.0, &PERMISSION_MANAGE_SYSTEM)
        .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_SYSTEM],
        ))
        .into_response();
    }

    let count = match state.app.revoke_non_compliant_user_access_tokens().await {
        Ok(count) => count,
        Err(err) => return ApiError::from(err).into_response(),
    };
    tracing::Span::current().record("count", count);

    let result = NonCompliantUserAccessTokenResult { count };
    match encode(&result, "revokeNonCompliantUserAccessTokens") {
        Ok(body) => json_ok(body),
        Err(err) => err.into_response(),
    }
}

/// The `json.NewEncoder(w).Encode(token)` ending — a JSON document **and a newline**.
///
/// Go logs an encode failure and leaves the 200 it already committed; `serde_json` cannot fail on
/// these types, so the error arm is a 500 that no input reaches.
fn encoded_with_newline(token: &UserAccessToken, where_: &'static str) -> Response {
    match encode(token, where_) {
        Ok(mut body) => {
            body.push(b'\n');
            json_ok(body)
        }
        Err(err) => err.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sanitised token serialises **without a `token` key at all** — `omitempty` on an empty
    /// string. A port that modelled the field as `Option<String>` and left it `Some("")` would
    /// emit `"token":""`, which is a different document and a hint that a secret exists.
    #[test]
    fn a_sanitised_token_has_no_token_key() {
        let token = UserAccessToken {
            id: "j1x3z8ynqjbstd4c4k6qy1p7ph".to_owned(),
            token: String::new(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            description: "personal".to_owned(),
            is_active: true,
            expires_at: 0,
            last_notified_at: Some(17),
        };

        let wire: serde_json::Value =
            serde_json::from_slice(&encode(&token, "getUserAccessToken").expect("serialises"))
                .expect("json");
        let object = wire.as_object().expect("an object");

        assert!(!object.contains_key("token"), "no secret key: {wire}");
        // `LastNotifiedAt` is `json:"-"`, so it is absent whatever it holds.
        assert!(!object.contains_key("last_notified_at"), "{wire}");
        assert_eq!(object.len(), 5, "five keys survive: {wire}");
        assert_eq!(wire["expires_at"], 0, "zero is present, not omitted");
    }

    /// The mirror of the test above, on the one route that must **not** sanitise: a creation
    /// response carries the secret, and it ends in a newline because Go uses `json.NewEncoder`.
    ///
    /// If this ever stops containing `token`, the create and rotate routes have become useless —
    /// the value they mint is unrecoverable by any later read.
    #[test]
    fn a_created_token_carries_the_secret_and_a_newline() {
        let token = UserAccessToken {
            id: "j1x3z8ynqjbstd4c4k6qy1p7ph".to_owned(),
            token: "cqjc7ec6bpy65jjamstkhpe6fr".to_owned(),
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            description: "personal".to_owned(),
            is_active: true,
            expires_at: 1_788_600_000_000,
            last_notified_at: None,
        };

        let body = encode(&token, "createUserAccessToken").expect("serialises");
        let wire: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(
            wire["token"], "cqjc7ec6bpy65jjamstkhpe6fr",
            "the secret is the point of this response"
        );
        assert_eq!(wire.as_object().expect("an object").len(), 6, "{wire}");

        // And the framing: `Encode` appends `\n`, `Marshal` does not, and both shapes are on
        // these seven routes.
        let mut framed = body.clone();
        framed.push(b'\n');
        assert_eq!(framed.last(), Some(&b'\n'));
        assert_ne!(body.last(), Some(&b'\n'));
    }

    /// `ReturnStatusOK` is `w.Write`, not `Encode` — **no trailing newline**, and three of these
    /// routes answer with it.
    #[tokio::test]
    async fn the_lifecycle_routes_answer_status_ok_without_a_newline() {
        let response = status_ok();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok()),
            Some("rust"),
            "the header the parity harness keys on"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("the body reads");
        assert_eq!(String::from_utf8_lossy(&body), r#"{"status":"OK"}"#);
    }

    /// The OAuth refusal is a permission error **with a clause appended to its detail**, and the
    /// detail is on the wire. The permission named differs per handler, so the test pins both
    /// halves: `revoke_user_access_token` for revoke and disable, `create_user_access_token` for
    /// create, enable and rotate.
    #[test]
    fn an_oauth_session_is_refused_with_an_appended_detail() {
        let mut session = mm_model::session::Session {
            user_id: "y9i4er48tt8bukijy7i3u5y9ar".to_owned(),
            ..Default::default()
        };
        session.is_oauth = true;

        let err = oauth_refusal(&session, &PERMISSION_REVOKE_USER_ACCESS_TOKEN);
        let body = err.0;
        assert_eq!(body.id, "api.context.permissions.app_error");
        assert_eq!(body.status_code, 403);
        assert_eq!(
            body.detailed_error,
            "userId=y9i4er48tt8bukijy7i3u5y9ar, permission=revoke_user_access_token, \
             attempted access by oauth app"
        );

        let other = oauth_refusal(&session, &PERMISSION_CREATE_USER_ACCESS_TOKEN);
        assert!(
            other
                .0
                .detailed_error
                .contains("permission=create_user_access_token"),
            "{}",
            other.0.detailed_error
        );
    }

    /// `MapFromJSON` swallows everything, so on revoke, disable and enable a malformed body is
    /// indistinguishable from `{}` — and both land on the empty-`token_id` path, which is a 404
    /// rather than the 400 the unreachable `SetInvalidParam` would have given.
    #[test]
    fn a_malformed_lifecycle_body_is_an_empty_map() {
        for raw in [&b"[]"[..], b"\"x\"", b"", b"{\"token_id\": 5}", b"not json"] {
            assert!(
                map_from_json(raw).is_empty(),
                "{:?} must decode to an empty map",
                String::from_utf8_lossy(raw)
            );
        }

        let props = map_from_json(br#"{"token_id":"j1x3z8ynqjbstd4c4k6qy1p7ph"}"#);
        assert_eq!(
            props.get("token_id").map(String::as_str),
            Some("j1x3z8ynqjbstd4c4k6qy1p7ph")
        );
    }

    /// Rotate decodes a **typed** body, so it does not share the swallow above — but every field
    /// defaults, and a missing `expires_at` is zero, which means "never expires" downstream
    /// rather than "unchanged".
    #[test]
    fn the_rotate_body_defaults_expires_at_to_zero() {
        let props: RotateBody =
            serde_json::from_slice(br#"{"token_id":"j1x3z8ynqjbstd4c4k6qy1p7ph"}"#)
                .expect("decodes");
        assert_eq!(props.token_id, "j1x3z8ynqjbstd4c4k6qy1p7ph");
        assert_eq!(
            props.expires_at, 0,
            "absent is zero, and zero never expires"
        );

        let empty: RotateBody = serde_json::from_slice(b"{}").expect("decodes");
        assert!(empty.token_id.is_empty(), "and this one really is a 400");

        // A body that is not an object is a decode failure, not an empty struct — unlike the
        // three `MapFromJSON` routes. **`serde_json::from_slice` accepts `[]` here**, which is
        // why the handlers go through `decode_go_struct` instead.
        assert!(
            serde_json::from_slice::<RotateBody>(b"[]").is_ok(),
            "serde's own answer"
        );
        assert!(decode_go_struct::<RotateBody>(b"[]").is_err(), "Go's");
    }

    /// The three ways [`decode_go_struct`] differs from `serde_json::from_slice`, each pinned
    /// against the answer Go gives.
    #[test]
    fn a_struct_body_decodes_the_way_gos_decoder_does() {
        // `null` is not an error: the target keeps its zero value, so rotate answers
        // `Name: "token_id"` rather than `Name: "rotate_user_access_token"`.
        let from_null: RotateBody = decode_go_struct(b"null").expect("null is not an error");
        assert!(from_null.token_id.is_empty());

        // An array is an error, where serde would have filled the struct positionally.
        assert!(decode_go_struct::<RotateBody>(br#"["a", 1]"#).is_err());

        // Only the first value is read; the rest of the stream is never looked at.
        let first: RotateBody =
            decode_go_struct(br#"{"token_id":"a"} {"token_id":"b"} not even json"#)
                .expect("Decode stops after one value");
        assert_eq!(first.token_id, "a");

        // And the cases that are errors on both sides.
        for raw in [
            &b""[..],
            b"\"x\"",
            b"7",
            b"true",
            b"{",
            b"{\"expires_at\":\"soon\"}",
        ] {
            assert!(
                decode_go_struct::<RotateBody>(raw).is_err(),
                "{:?} must not decode",
                String::from_utf8_lossy(raw)
            );
        }
    }

    /// The search body's empty `term` is rejected by the handler, and `{}` reaches that check the
    /// same way an explicit `""` does.
    #[test]
    fn an_absent_search_term_is_the_empty_one() {
        let absent: UserAccessTokenSearch = serde_json::from_slice(b"{}").expect("decodes");
        assert!(absent.term.is_empty());

        let explicit: UserAccessTokenSearch =
            serde_json::from_slice(br#"{"term":""}"#).expect("decodes");
        assert_eq!(explicit, absent, "the handler cannot tell them apart");
    }

    /// `{"count":0}` — `Count` carries no `omitempty`, so the zero this route always answers on a
    /// stock server is on the wire rather than an empty object.
    #[test]
    fn the_count_result_keeps_a_zero() {
        let body = encode(
            &NonCompliantUserAccessTokenResult { count: 0 },
            "countNonCompliantUserAccessTokens",
        )
        .expect("serialises");
        assert_eq!(String::from_utf8_lossy(&body), r#"{"count":0}"#);
    }
}
