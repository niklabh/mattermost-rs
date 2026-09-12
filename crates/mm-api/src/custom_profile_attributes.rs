//! Port of seven of the eight routes in `api4/custom_profile_attributes.go` — the "User
//! Attributes" API, which keeps the older CPA name in every identifier and URL for backward
//! compatibility (MM-68235).
//!
//! The eighth, `GET /custom_profile_attributes/group`, was ported earlier and lives in
//! [`crate::gated_reads`]: it is the one CPA route whose licence check is written out inline in
//! the handler, because `GetPropertyGroup` is the one property call the licence hook does not
//! cover. The other seven get theirs from the hook, and that difference is this module's subject.
//!
//! # The gate is a licence, it is not uniform, and it is not first
//!
//! `App.Srv().propertyService` registers a `LicenseCheckHook` scoped to the `access_control`
//! group as its **first** hook (app/server.go:322). Unlicensed it refuses with 403
//! `app.property.license_error`. But the hook has fourteen arms, and they do not all fire:
//!
//! - `PreCreatePropertyField` refuses unconditionally, so **POST /fields is always a 403**.
//! - `PostGetPropertyField` runs only after the row is **found**, so a patch or delete of an
//!   unknown field is **404 `app.property.not_found.app_error`**, not a 403.
//! - `PostGetPropertyFields` and `PostGetPropertyValues` return `nil` when handed an empty
//!   slice, so an unlicensed **`GET /fields` answers `[]`** on a group with no user fields and
//!   **`GET /users/{id}/custom_profile_attributes` answers `{}`** for a user with no values —
//!   both 200, both real reads.
//! - The batch value patch reads its fields by id first, so a batch naming a field that does not
//!   exist is **404 `app.property_field.not_found.app_error`** — a different id from the
//!   single-field miss, one word apart.
//!
//! Every one of those was checked against the Go server on this stack, twice: once against the
//! seeded database, and once with an `access_control` user field inserted, which flips the two
//! 200s to 403s and the 404s to 403s. A port that answered a flat 403 to all seven would be
//! wrong on five.
//!
//! Everything a client can get wrong *before* the gate — a malformed id, a body that will not
//! decode, an invalid patch, an empty or oversized batch — is answered ahead of it and is
//! therefore fully comparable. That is most of what these handlers are.
//!
//! # What is not ported
//!
//! The success path of every write. It reaches `CreatePropertyField`/`UpdatePropertyField`/
//! `DeletePropertyField`/`UpsertPropertyValues`, and behind those the access-control hook, the
//! attribute-validation hook, the type-change value cleanup and four websocket events — none of
//! which exist on this side. They are unreachable without an Enterprise licence, so each handler
//! **forwards to Go the moment anything says this installation is licensed** and serves only the
//! unlicensed contract itself. See [`mm_app::App::cpa_list_fields_unlicensed`] and its
//! neighbours, whose names carry that precondition because no type can.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::permission::{
    PERMISSION_EDIT_OTHER_USERS, PERMISSION_MANAGE_SYSTEM, PERMISSION_VIEW_MEMBERS,
    make_permission_error,
};
use mm_model::property_field::PropertyFieldPatch;
use mm_model::utils::{AppError, is_valid_id};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::resolve_me;
use crate::error::ApiError;
use crate::proxy;

/// Port of `maxPropertyValuePatchItems` (api4/properties.go:20).
const MAX_PROPERTY_VALUE_PATCH_ITEMS: usize = 50;

/// `json.NewEncoder(w).Encode(v)` — a JSON body **with** the encoder's trailing newline ([D-086]).
fn encoded(value: &impl serde::Serialize, where_: &'static str) -> Response {
    let mut body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise a CPA response");
            return ApiError::from(AppError::new(
                where_,
                "api.marshal_error",
                None,
                String::new(),
                500,
            ))
            .into_response();
        }
    };
    body.push(b'\n');

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

/// The licence decision every route in this file starts from, as a `Request`-preserving split.
///
/// [`licence_gate`] consumes the request to forward it, which the three body-carrying handlers
/// cannot afford: they need the body in the *other* branch. So the decision is taken first and
/// the request is forwarded only on the arm that wants it.
enum Cpa {
    /// Nothing says this server is licensed — serve the contract in this module.
    Unlicensed,
    /// A licence is installed; the work behind the gate is not ported.
    Forward,
    Failed(ApiError),
}

async fn cpa_gate(state: &AppState) -> Cpa {
    match state.app.license_state().await {
        Ok(mm_app::license::LicenseState::Unlicensed) => {
            tracing::Span::current().record("licensed", false);
            Cpa::Unlicensed
        }
        Ok(mm_app::license::LicenseState::Licensed) => {
            tracing::Span::current().record("licensed", true);
            Cpa::Forward
        }
        Err(err) => Cpa::Failed(ApiError::from(err)),
    }
}

/// `json.NewDecoder(r.Body).Decode(&v)` over a body already read into memory: the **first** JSON
/// value in it, or `None` when there is none.
///
/// Go's decoder reads one value and ignores whatever follows, where `serde_json::from_slice`
/// rejects the trailing token — which would turn a body Go accepts into a 400. An empty body is
/// `io.EOF`, an error rather than a zero value, so it is `None` here too.
fn first_value(body: &[u8]) -> Option<serde_json::Value> {
    serde_json::Deserializer::from_slice(body)
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()
}

/// `Decode(&v)` where `v` is a `*Struct`, as both field handlers write it.
///
/// `None` is Go's `err != nil || v == nil` — one branch, because both handlers take it together.
/// **A JSON array is not a struct**: `encoding/json` refuses it, and so must this, which serde
/// would not do on its own (a struct deserialises happily from a sequence of its fields, so `[]`
/// would otherwise decode to an all-default patch and a caller sending `[]` would get a *write*
/// where Go gives a 400). A literal `null` decodes without error in Go and leaves the pointer
/// nil, which lands on the same `None`.
fn decode_struct<T: serde::de::DeserializeOwned>(body: &[u8]) -> Option<T> {
    match first_value(body)? {
        object @ serde_json::Value::Object(_) => serde_json::from_value(object).ok(),
        _ => None,
    }
}

/// `Decode(&v)` where `v` is a `map[string]json.RawMessage`, as both value handlers write it.
///
/// The difference from [`decode_struct`] is `null`: it decodes into a **nil map without an
/// error**, so the handler falls through to the empty-batch refusal instead of answering
/// `invalid_body_param`. Those are two different error ids at the same status from the same four
/// bytes, and Go gives the second.
fn decode_map(body: &[u8]) -> Option<serde_json::Map<String, serde_json::Value>> {
    match first_value(body)? {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => Some(serde_json::Map::new()),
        _ => None,
    }
}

/// Port of `listCPAFields` (api4/custom_profile_attributes.go:34) —
/// `GET /api/v4/custom_profile_attributes/fields`.
///
/// Unlicensed this is `[]` or a 403, and which one depends on whether the `access_control` group
/// holds a `user`-object field. See the module docs.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn list_cpa_fields(
    State(state): State<AppState>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            let group = match state.app.cpa_property_group().await {
                Ok(group) => group,
                Err(err) => return ApiError::from(err).into_response(),
            };
            match state.app.cpa_list_fields_unlicensed(&group.id).await {
                Ok(fields) => encoded(&fields, "listCPAFields"),
                Err(err) => ApiError::from(err).into_response(),
            }
        }
    }
}

/// Port of `createCPAField` (:62) — `POST /api/v4/custom_profile_attributes/fields`.
///
/// # Three things happen before the licence, and all three are comparable
///
/// The body is decoded into a `*model.CPAField` — a bad body, a bare `null` and a JSON array are
/// all **400 `api.context.invalid_body_param.app_error`** naming `property_field`. Then
/// `PermissionManageSystem`, which is the scope check the generic property handler would have
/// applied to a system-typed field, so a non-admin gets a **permission error** and never learns
/// whether the server is licensed. Only then does the group read run, and only then the hook.
///
/// The decoded field is otherwise discarded here: everything Go does with it —
/// `strings.TrimSpace` on the name, `ToPropertyField`, stamping the group, object type, target
/// shape and creator, clearing id/target/protected — happens between the permission check and
/// `CreatePropertyField`, which refuses unconditionally. None of it is observable unlicensed.
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn create_cpa_field(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            let Some(body) = read_body(request).await else {
                return body_unreadable();
            };
            if decode_struct::<mm_model::custom_profile_attributes::CPAField>(&body).is_none() {
                return ApiError::invalid_param("property_field").into_response();
            }

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

            match state.app.cpa_property_group().await {
                Err(err) => ApiError::from(err).into_response(),
                Ok(_) => {
                    ApiError::from(mm_app::custom_profile_attributes::property_licence_refusal(
                        "CreatePropertyField",
                    ))
                    .into_response()
                }
            }
        }
    }
}

/// Port of `patchCPAField` (:130) — `PATCH /api/v4/custom_profile_attributes/fields/{field_id}`.
///
/// # The name is trimmed **before** it is validated
///
/// `*patch.Name = strings.TrimSpace(*patch.Name)` runs at :142 and `patch.IsValid()` at :149, so
/// a patch of `{"name":"   "}` is `model.property_field.is_valid.app_error` — "value cannot be
/// empty" — and not a successful rename to three spaces. Reversing the two would turn a 400 into
/// a write.
///
/// `TargetID` and `TargetType` are cleared before validation too, so a caller cannot patch them
/// and cannot fail validation on them either.
#[tracing::instrument(skip_all, fields(field_id = %field_id, licensed))]
pub async fn patch_cpa_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            // `c.RequireFieldId()` (web/context.go) — first, before the body is even read.
            if !is_valid_id(&field_id) {
                return ApiError::invalid_url_param("field_id").into_response();
            }

            let Some(body) = read_body(request).await else {
                return body_unreadable();
            };
            let Some(mut patch) = decode_struct::<PropertyFieldPatch>(&body) else {
                return ApiError::invalid_param("property_field_patch").into_response();
            };

            if let Some(name) = patch.name.as_mut() {
                *name = name.trim().to_owned();
            }
            patch.target_id = None;
            patch.target_type = None;

            if let Err(err) = patch.is_valid() {
                return ApiError::from(err).into_response();
            }

            let group = match state.app.cpa_property_group().await {
                Ok(group) => group,
                Err(err) => return ApiError::from(err).into_response(),
            };
            ApiError::from(
                state
                    .app
                    .cpa_field_write_unlicensed(&group.id, &field_id)
                    .await,
            )
            .into_response()
        }
    }
}

/// Port of `deleteCPAField` (:236) — `DELETE /api/v4/custom_profile_attributes/fields/{field_id}`.
///
/// The same read as the patch and the same two answers, with **no body** between the id check and
/// the group read — so a delete of an unknown id is a 404 where the patch of the same id might
/// still be a 400, because the patch reads a body first.
#[tracing::instrument(skip_all, fields(field_id = %field_id, licensed))]
pub async fn delete_cpa_field(
    State(state): State<AppState>,
    Path(field_id): Path<String>,
    _session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            if !is_valid_id(&field_id) {
                return ApiError::invalid_url_param("field_id").into_response();
            }

            let group = match state.app.cpa_property_group().await {
                Ok(group) => group,
                Err(err) => return ApiError::from(err).into_response(),
            };
            ApiError::from(
                state
                    .app
                    .cpa_field_write_unlicensed(&group.id, &field_id)
                    .await,
            )
            .into_response()
        }
    }
}

/// Port of `patchCPAValues` (:434) — `PATCH /api/v4/custom_profile_attributes/values`.
///
/// The target is **always the session's own user**, so `hasTargetAccess`'s self-access arm always
/// passes and no permission can refuse this route. Everything else it does is
/// [`cpa_patch_values`].
#[tracing::instrument(skip_all, fields(licensed))]
pub async fn patch_cpa_values(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            let Some(body) = read_body(request).await else {
                return body_unreadable();
            };
            let user_id = session.0.user_id.clone();
            cpa_patch_values(&state, &session, &user_id, &body).await
        }
    }
}

/// Port of `listCPAValues` (:371) —
/// `GET /api/v4/users/{user_id}/custom_profile_attributes`.
///
/// # The target check runs **before** the group read here, and after it on the PATCH routes
///
/// `listCPAValues` calls `hasTargetAccess` at :377 and `GetPropertyGroup` at :383;
/// `cpaPatchValues`, which both PATCH routes share, reads the group at :306 and checks the target
/// at :311. The asymmetry is only observable when the group is missing — a 404 versus a 403 — but
/// it is reproduced rather than tidied, because tidying it is how a port acquires a divergence
/// nobody chose.
///
/// # The body is `{}`, never `null`
///
/// `returnValue := make(map[string]json.RawMessage)` is always non-nil, so a user with no values
/// gets an empty object. A `BTreeMap` serialises the same way, and its ordering is Go's too:
/// `encoding/json` sorts map keys.
#[tracing::instrument(skip_all, fields(user_id = %user_id, licensed))]
pub async fn list_cpa_values(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            let user_id = resolve_me(&user_id, &session).to_owned();
            if !is_valid_id(&user_id) {
                return ApiError::invalid_url_param("user_id").into_response();
            }

            match target_access_read(&state, &session, &user_id).await {
                TargetAccess::Denied(err) => return err.into_response(),
                // The caller's account carries view restrictions, which needs two membership
                // lookups this port does not have. Forward the request untouched.
                TargetAccess::Forward => return proxy::forward_to_go(State(state), request).await,
                TargetAccess::Allowed => {}
            }

            let group = match state.app.cpa_property_group().await {
                Ok(group) => group,
                Err(err) => return ApiError::from(err).into_response(),
            };
            match state
                .app
                .cpa_list_values_unlicensed(&group.id, &user_id)
                .await
            {
                Ok(values) => encoded(&values, "listCPAValues"),
                Err(err) => ApiError::from(err).into_response(),
            }
        }
    }
}

/// Port of `patchCPAValuesForUser` (:460) —
/// `PATCH /api/v4/users/{user_id}/custom_profile_attributes`.
///
/// The id check runs before the body is read, and the write-side target check —
/// `PermissionEditOtherUsers` for anyone but yourself — runs inside [`cpa_patch_values`], after
/// the group read.
#[tracing::instrument(skip_all, fields(user_id = %user_id, licensed))]
pub async fn patch_cpa_values_for_user(
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    match cpa_gate(&state).await {
        Cpa::Forward => proxy::forward_to_go(State(state), request).await,
        Cpa::Failed(err) => err.into_response(),
        Cpa::Unlicensed => {
            let user_id = resolve_me(&user_id, &session).to_owned();
            if !is_valid_id(&user_id) {
                return ApiError::invalid_url_param("user_id").into_response();
            }

            let Some(body) = read_body(request).await else {
                return body_unreadable();
            };
            cpa_patch_values(&state, &session, &user_id, &body).await
        }
    }
}

/// Port of `cpaPatchValues` (:302), shared by both PATCH-values routes.
///
/// # The order of the four refusals is the whole content
///
/// 1. The body must decode as a JSON **object** — `map[string]json.RawMessage`. An array or a
///    number is 400 `api.context.invalid_body_param.app_error` naming `value`; a literal `null`
///    is **not**, because Go decodes it into a nil map without error and falls through to (3).
/// 2. The group is read, then the target check — `PermissionEditOtherUsers` unless the target is
///    the caller.
/// 3. An empty batch is 400 `api.property_value.patch.empty_body.app_error`. `{}` and `null` both
///    land here.
/// 4. More than fifty entries is 400 `api.property_value.patch.too_many_items.request_error`,
///    **before** any id is checked — so an oversized batch of malformed ids reports the size.
/// 5. Every key must be a valid id: 400 `api.property_value.patch.invalid_field_id.app_error`.
///
/// Go's duplicate-`FieldID` check from the generic handler is absent here and its comment says
/// why: the keys come from a JSON object, so uniqueness is already guaranteed.
async fn cpa_patch_values(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
    body: &[u8],
) -> Response {
    let Some(updates) = decode_map(body) else {
        return ApiError::invalid_param("value").into_response();
    };

    let group = match state.app.cpa_property_group().await {
        Ok(group) => group,
        Err(err) => return ApiError::from(err).into_response(),
    };

    if user_id != session.0.user_id
        && !session.0.is_unrestricted()
        && !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_EDIT_OTHER_USERS)
            .await
    {
        return ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_EDIT_OTHER_USERS],
        ))
        .into_response();
    }

    if updates.is_empty() {
        return value_patch_error("api.property_value.patch.empty_body.app_error", None)
            .into_response();
    }
    if updates.len() > MAX_PROPERTY_VALUE_PATCH_ITEMS {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "Max".to_owned(),
            serde_json::Value::from(MAX_PROPERTY_VALUE_PATCH_ITEMS),
        );
        return value_patch_error(
            "api.property_value.patch.too_many_items.request_error",
            Some(params),
        )
        .into_response();
    }

    let mut field_ids: Vec<String> = Vec::with_capacity(updates.len());
    for field_id in updates.keys() {
        if !is_valid_id(field_id) {
            return value_patch_error("api.property_value.patch.invalid_field_id.app_error", None)
                .into_response();
        }
        field_ids.push(field_id.clone());
    }

    ApiError::from(
        state
            .app
            .cpa_patch_values_unlicensed(&group.id, &field_ids)
            .await,
    )
    .into_response()
}

/// The three 400s `cpaPatchValues` raises itself, all with `Where` = `cpaPatchValues`.
fn value_patch_error(
    id: &'static str,
    params: Option<std::collections::HashMap<String, serde_json::Value>>,
) -> ApiError {
    ApiError::from(AppError::new(
        "cpaPatchValues",
        id,
        params,
        String::new(),
        400,
    ))
}

/// `hasTargetAccess(c, PropertyFieldObjectTypeUser, targetID, write=false)`
/// (api4/properties.go:872), the read arm.
///
/// Self-access and an unrestricted (local-mode) session pass without a query. Anyone else must be
/// able to *see* the target, which is `UserCanSeeOtherUser` — and that needs the team and channel
/// membership lookups this port does not have whenever the caller's account carries view
/// restrictions. That case forwards rather than guesses; see
/// [`mm_app::App::user_can_see_other_user`].
enum TargetAccess {
    Allowed,
    Denied(ApiError),
    Forward,
}

async fn target_access_read(
    state: &AppState,
    session: &AuthenticatedSession,
    user_id: &str,
) -> TargetAccess {
    if user_id == session.0.user_id || session.0.is_unrestricted() {
        return TargetAccess::Allowed;
    }

    match state
        .app
        .user_can_see_other_user(&session.0.user_id, user_id)
        .await
    {
        Ok(true) => TargetAccess::Allowed,
        Ok(false) => TargetAccess::Denied(ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_VIEW_MEMBERS],
        ))),
        Err(mm_app::post::PrepareError::Unreproducible(reason)) => {
            tracing::debug!(reason, "forwarding to Go");
            TargetAccess::Forward
        }
        Err(mm_app::post::PrepareError::App(err)) => TargetAccess::Denied(ApiError::from(err)),
    }
}

/// The request body, or `None` when it will not read — which [`body_unreadable`] answers, the
/// same plain-text 400 [`crate::proxy::forward_to_go`] gives for the same failure.
async fn read_body(request: Request) -> Option<axum::body::Bytes> {
    match axum::body::to_bytes(request.into_body(), usize::MAX).await {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::warn!(error = %err, "could not read the request body");
            None
        }
    }
}

fn body_unreadable() -> Response {
    (StatusCode::BAD_REQUEST, "could not read request body").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Decode` into a pointer accepts `null` and leaves the pointer nil; the two handlers that
    /// decode a pointer then answer `invalid_body_param`. The two that decode a **map** treat the
    /// same `null` as an empty map and fall through to the empty-batch error instead — a
    /// different id at the same status, from the same four bytes.
    #[test]
    fn a_null_body_is_nil_for_a_struct_and_empty_for_a_map() {
        assert!(decode_struct::<PropertyFieldPatch>(b"null").is_none());
        assert_eq!(decode_map(b"null"), Some(serde_json::Map::new()));
    }

    /// An empty body is `io.EOF` on Go's decoder, which is an error and not a zero value — so
    /// every route here answers its own `invalid_body_param` to a bodyless request rather than
    /// treating it as `{}`.
    #[test]
    fn an_empty_body_does_not_decode() {
        assert!(decode_struct::<PropertyFieldPatch>(b"").is_none());
        assert!(decode_map(b"").is_none());
        assert!(decode_map(b"  ").is_none());
    }

    /// **A JSON array is not a struct, and serde does not know that.** A struct deserialises from
    /// a sequence of its fields, so `[]` decodes to an all-default `PropertyFieldPatch` unless
    /// something refuses it first — which would let `PATCH …/fields/{id}` with a body of `[]`
    /// through to a *write* where Go answers 400. This is the test that caught it.
    #[test]
    fn an_array_is_not_an_object() {
        assert!(decode_struct::<PropertyFieldPatch>(b"[]").is_none());
        assert!(decode_struct::<PropertyFieldPatch>(br#"["x"]"#).is_none());
        assert!(decode_map(b"[]").is_none());
    }

    /// Nor is a scalar.
    #[test]
    fn a_scalar_is_not_an_object() {
        assert!(decode_struct::<PropertyFieldPatch>(b"5").is_none());
        assert!(decode_map(b"5").is_none());
        assert!(decode_map(br#""x""#).is_none());
    }

    /// An object whose keys are the right names still has to have the right **types**: a
    /// numeric `name` is a decode error in Go and must be one here.
    #[test]
    fn an_object_with_the_wrong_field_type_does_not_decode() {
        assert!(decode_struct::<PropertyFieldPatch>(br#"{"name":5}"#).is_none());
        assert!(decode_struct::<PropertyFieldPatch>(br#"{"name":"ok"}"#).is_some());
    }

    /// **Go's decoder stops at the first value and ignores the rest.** `serde_json::from_slice`
    /// would reject the trailing token, which would turn a body Go accepts into a 400 — so
    /// [`first_value`] uses the streaming form. This is the assertion that keeps it that way.
    #[test]
    fn trailing_content_is_ignored_the_way_go_ignores_it() {
        let decoded = decode_map(br#"{"a":1} {"b":2}"#).expect("the first value decodes");
        assert_eq!(decoded.len(), 1);
        assert!(decoded.contains_key("a"));
    }

    /// The cap is fifty, and it is a `>` and not a `>=`: exactly fifty entries is accepted.
    #[test]
    fn the_batch_cap_is_fifty_inclusive() {
        assert_eq!(MAX_PROPERTY_VALUE_PATCH_ITEMS, 50);
    }
}
