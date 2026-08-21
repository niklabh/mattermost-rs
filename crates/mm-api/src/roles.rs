//! Port of `getRolesByNames`, `getRoleByName` and `getRole` (channels/api4/role.go:87, :70,
//! :56), reached as `POST /api/v4/roles/names`, `GET /api/v4/roles/name/{role_name}` and
//! `GET /api/v4/roles/{role_id}`.
//!
//! # `APISessionRequiredTrustRequester`, and what it actually changes
//!
//! All three are registered with `APISessionRequiredTrustRequester` (role.go:25-27) rather than
//! `APISessionRequired`. The two differ in exactly one field, `TrustRequester: true`
//! (api4/handlers.go:164), and that field is read in exactly one place:
//!
//! ```text
//! csrfCheckNeeded := session != nil && c.Err == nil &&
//!     tokenLocation == app.TokenLocationCookie && !h.TrustRequester && r.Method != "GET"
//! ```
//! (web/handlers.go:509). So it suppresses the CSRF check, and nothing else — session, MFA and
//! rate limiting are identical. Three of the four conjuncts are already false for these routes:
//! two are `GET`, and a bearer token is not `TokenLocationCookie`. **It is observable on exactly
//! one of the three**: `POST /roles/names` authenticated by the `MMAUTHTOKEN` cookie — a browser
//! — is accepted by Go with no `X-CSRF-Token` header, where the same request to
//! `POST /users/status/ids` is refused.
//!
//! This port performs no CSRF check anywhere, so today it agrees with Go on these routes by
//! having nothing to switch off. That agreement is a coincidence of what is not yet ported, not a
//! decision: **when CSRF lands, these three routes must be exempt**, and this note is where a
//! reader adding it will look.
//!
//! # No permission check
//!
//! Unlike `getAllRoles` (which gates on `manage_system`) and `patchRole`, none of these three
//! asks a permission question. Any authenticated session may read any role by id or by name.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::role::{Role, clean_role_names, is_valid_role_name};
use mm_model::utils::{AppError, is_valid_id, sorted_array_from_json};

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::error::ApiError;

/// `model.PayloadParseError` (model/utils.go:42).
const PAYLOAD_PARSE_ERROR: &str = "api.payload.parse.error";

/// `GetRolesByNamesMax` (api4/role.go:13).
const GET_ROLES_BY_NAMES_MAX: usize = 100;

/// Go's mux class for `{role_name}`: `[a-z0-9_]+` (role.go:26).
///
/// Strictly narrower than the `[A-Za-z0-9]+` every `*_id` segment uses, so the shared
/// id-charset middleware is the wrong rule here and the parameter is deliberately not
/// id-shaped. `System_User` and `system-user` are both mux 404s from Go, before any handler runs.
fn segment_matches_role_name_mux(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Port of `getRolesByNames` (api4/role.go:87) — `POST /api/v4/roles/names`.
///
/// The webapp posts this on every page load with the current user's role names, so it is the
/// most-called of the three by a wide margin.
///
/// # Order on the wire is the *request's* order, not the table's
///
/// Go's SQL has no `ORDER BY` (role_store.go:271) — but the answer does not come from the SQL.
/// `LocalCacheRoleStore.GetByNames` (localcachelayer/role_layer.go:69) serves cache hits first,
/// appended **in the order of the requested names**, and only queries the misses. Role names are
/// cached for 30 minutes and every permission check on the server populates that cache, so in
/// practice every name in a real request is a hit and the response comes back in request order —
/// which `SortedArrayFromJSON` has already sorted. Measured against the running Go server, not
/// inferred: five built-in names in scrambled order came back alphabetical on both attempts,
/// while the table's own heap order for the same five is `team_user, channel_admin, system_user,
/// system_admin, channel_user`.
///
/// So this sorts by name, which for a sorted, de-duplicated input list is exactly Go's
/// warm-cache order. A **cold** Go cache would append the missed names after the hits instead;
/// that ordering is a cache-warmth artefact of Go's, reachable only in the first seconds after a
/// restart, and there is no version of it a port could match. See the route notes in
/// `MIGRATION.md`.
///
/// # Wire format: `null`, not `[]`, when nothing matches
///
/// `json.Marshal` then `w.Write` — **no trailing newline**, unlike the two single-role routes
/// below, which use `json.NewEncoder(w).Encode`. An unmatched name is simply absent; nothing
/// 404s and nothing is padded with a null.
///
/// But an answer with *no* roles in it is **`null`**, not `[]`, and that is not a quirk of the
/// SQL store — `SqlRoleStore.GetByNames` is careful to return `[]*model.Role{}` (role_store.go:266
/// and :275). It is the cache layer above it: `LocalCacheRoleStore.GetByNames` opens with
/// `var foundRoles []*model.Role` — a **nil** slice — and ends with
/// `append(foundRoles, roles...)` (role_layer.go:70, :104). Appending an empty slice to a nil one
/// yields nil, and `json.Marshal` writes nil as `null`. So the result is nil exactly when it is
/// empty, and empty exactly when it is nil.
///
/// Reachable three ways, all measured against the running server: every requested name is
/// unknown; the body was nothing but blank strings, which `CleanRoleNames` drops to nothing; or
/// some mixture of the two. A partial match is a normal array. Serialising `Vec::new()` as `[]`
/// here would have been the obvious, invisible, wrong answer.
#[tracing::instrument(skip_all, fields(user_id = %session.0.user_id, count))]
pub async fn get_roles_by_names(
    State(state): State<AppState>,
    session: AuthenticatedSession,
    request: Request,
) -> Result<Response, ApiError> {
    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|err| {
            tracing::warn!(error = %err, "could not read the request body");
            payload_parse_error()
        })?;

    let names = parse_role_names(&bytes)?;
    tracing::Span::current().record("count", names.len());

    let mut roles = state.app.get_roles_by_names(&names).await?;
    roles.sort_by(|a, b| a.name.cmp(&b.name));

    let body = if roles.is_empty() {
        // Go's nil slice. See the note above — this is the cache layer's shape, not the SQL
        // store's, and it is what every client actually receives.
        b"null".to_vec()
    } else {
        serde_json::to_vec(&roles).map_err(|err| {
            tracing::error!(error = %err, "failed to serialise roles");
            marshal_error("getRolesByNames")
        })?
    };

    Ok(json_ok(body))
}

/// Port of `getRoleByName` (api4/role.go:70) — `GET /api/v4/roles/name/{role_name}`.
///
/// `RequireRoleName` (web/context.go:722) is `IsValidRoleName`, which is *almost* the mux class:
/// both demand a non-empty `[a-z0-9_]+`, and the validator adds a 64-**byte** cap on top. So the
/// only 400 reachable here is a name longer than 64 bytes — everything else the validator would
/// reject was already a mux 404, which is why the charset check forwards rather than answering.
#[tracing::instrument(skip_all, fields(role_name = %role_name, forwarded))]
pub async fn get_role_by_name(
    State(state): State<AppState>,
    Path(role_name): Path<String>,
    session: AuthenticatedSession,
    request: Request,
) -> Response {
    let _ = session;
    if !segment_matches_role_name_mux(&role_name) {
        tracing::Span::current().record("forwarded", true);
        return crate::proxy::forward_to_go(State(state), request).await;
    }
    tracing::Span::current().record("forwarded", false);

    serve_one_role(async {
        if !is_valid_role_name(&role_name) {
            return Err(ApiError::invalid_url_param("role_name"));
        }
        Ok(state.app.get_role_by_name(&role_name).await?)
    })
    .await
}

/// Port of `getRole` (api4/role.go:56) — `GET /api/v4/roles/{role_id}`.
///
/// The mux class is `[A-Za-z0-9]+`, which the shared id-charset middleware applies for us, so
/// anything outside it has already been forwarded by the time this runs. `RequireRoleId`
/// (web/context.go:656) then applies `IsValidId`: exactly 26 characters, all letters or digits.
/// A 26-character segment that is not a real id therefore reaches the store and 404s, while a
/// 25-character one 400s — two different answers for two kinds of nonsense, both Go's.
#[tracing::instrument(skip_all, fields(role_id = %role_id))]
pub async fn get_role(
    State(state): State<AppState>,
    Path(role_id): Path<String>,
    session: AuthenticatedSession,
) -> Response {
    let _ = session;
    serve_one_role(async {
        if !is_valid_id(&role_id) {
            return Err(ApiError::invalid_url_param("role_id"));
        }
        Ok(state.app.get_role(&role_id).await?)
    })
    .await
}

/// The shared tail of the two single-role routes: `json.NewEncoder(w).Encode(role)`, which
/// appends a **trailing newline** the list route above does not have.
///
/// Go writes no marshal error here at all — `Encode` failing is only logged, after a `200` and
/// possibly some bytes have already gone out. Nothing in `model.Role` can fail to marshal, so
/// this returns the 500 that path would deserve rather than reproducing a half-written response.
async fn serve_one_role(fetch: impl Future<Output = Result<Role, ApiError>>) -> Response {
    let role = match fetch.await {
        Ok(role) => role,
        Err(err) => return err.into_response(),
    };

    match serde_json::to_vec(&role) {
        Ok(mut body) => {
            body.push(b'\n');
            json_ok(body)
        }
        Err(err) => {
            tracing::error!(error = %err, "failed to serialise the role");
            marshal_error("getRole").into_response()
        }
    }
}

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

/// `model.NewAppError(where, "api.marshal_error", nil, "", 500)`.
fn marshal_error(where_: &str) -> ApiError {
    ApiError(AppError::new(
        where_,
        "api.marshal_error",
        None,
        String::new(),
        500,
    ))
}

/// `model.NewAppError("getRolesByNames", model.PayloadParseError, nil, "", 400)`.
fn payload_parse_error() -> ApiError {
    ApiError(AppError::new(
        "getRolesByNames",
        PAYLOAD_PARSE_ERROR,
        None,
        String::new(),
        400,
    ))
}

/// The validation half of `getRolesByNames` (role.go:88-108), in Go's order. Every branch, and
/// the answer each one gives:
///
/// 1. **Not a JSON array of strings** → 400 `api.payload.parse.error`. `SortedArrayFromJSON`
///    sorts and de-duplicates what it accepts; its decoder's habits (trailing bytes ignored,
///    `null` elements as `""`, a `null` body as an empty list) live in
///    [`sorted_array_from_json`] and its oracle.
/// 2. **No names** (`null`, `[]`) → 400 `invalid_body_param` naming **`rolenames`** — one word,
///    no underscore, unlike almost every other parameter name in api4.
/// 3. **More than 100 names** → 400 `api.roles.get_multiple_by_name_too_many.request_error`,
///    carrying `MaxNames` in the i18n params. Checked **after** de-duplication, so 150 copies of
///    one name is a legal request for one role. `params` is unexported in `model.AppError` and
///    never reaches the wire; it is populated for the day translation does.
/// 4. **Any name outside `[a-z0-9_]{1,64}`** → 400 `invalid_body_param` naming **`rolename`** —
///    *singular*, and a different string from branch 2's. Two adjacent branches, two parameter
///    names differing by one letter, is precisely the sort of thing a port irons out.
///
/// `CleanRoleNames` **drops** entries that are blank after trimming instead of rejecting them,
/// but does not trim the ones it keeps — so `[""]` and `["  "]` are a legal request for *no*
/// roles (answering `[]`), while `[" system_user "]` fails branch 4. An all-blank list reaches
/// the store as Go's nil slice, which short-circuits to an empty result rather than querying.
#[allow(clippy::result_large_err)]
fn parse_role_names(body: &[u8]) -> Result<Vec<String>, ApiError> {
    let names = sorted_array_from_json(body).map_err(|err| {
        tracing::debug!(error = %err, "rolenames body did not decode");
        payload_parse_error()
    })?;

    if names.is_empty() {
        return Err(ApiError::invalid_param("rolenames"));
    }

    if names.len() > GET_ROLES_BY_NAMES_MAX {
        let mut params: std::collections::HashMap<String, serde_json::Value> =
            std::collections::HashMap::new();
        params.insert(
            "MaxNames".to_owned(),
            serde_json::Value::from(GET_ROLES_BY_NAMES_MAX),
        );
        return Err(ApiError(AppError::new(
            "getRolesByNames",
            "api.roles.get_multiple_by_name_too_many.request_error",
            Some(params),
            String::new(),
            400,
        )));
    }

    let (cleaned, valid) = clean_role_names(&names);
    if !valid {
        return Err(ApiError::invalid_param("rolename"));
    }

    // Go's nil slice; the store short-circuits on it (role_store.go:265).
    Ok(cleaned.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_of(body: &str) -> AppError {
        parse_role_names(body.as_bytes())
            .expect_err("this body must be rejected")
            .0
    }

    fn name_param(err: &AppError) -> Option<&serde_json::Value> {
        err.params.as_ref().and_then(|p| p.get("Name"))
    }

    /// The mux class is `[a-z0-9_]+`: lower case, digits and underscore only. Upper case, hyphen
    /// and dot are all mux 404s, which is why the handler forwards rather than 400s on them.
    #[test]
    fn the_role_name_segment_charset_is_gos_mux_class() {
        assert!(segment_matches_role_name_mux("system_user"));
        assert!(segment_matches_role_name_mux("run_admin2"));
        assert!(segment_matches_role_name_mux("_"));
        assert!(!segment_matches_role_name_mux("System_User"));
        assert!(!segment_matches_role_name_mux("system-user"));
        assert!(!segment_matches_role_name_mux("system.user"));
        assert!(!segment_matches_role_name_mux(""));
        assert!(!segment_matches_role_name_mux("caf\u{e9}"));
    }

    /// The validator and the mux agree on the charset and differ only on length, so a 64-byte
    /// name routes and serves while a 65-byte one routes and 400s. Nothing else is reachable.
    #[test]
    fn the_only_reachable_role_name_400_is_the_length_cap() {
        let sixty_four = "a".repeat(64);
        assert!(segment_matches_role_name_mux(&sixty_four));
        assert!(is_valid_role_name(&sixty_four));

        let sixty_five = "a".repeat(65);
        assert!(
            segment_matches_role_name_mux(&sixty_five),
            "the mux has no length limit, so this reaches the handler"
        );
        assert!(
            !is_valid_role_name(&sixty_five),
            "and the handler is what refuses it"
        );
    }

    /// Branch 1.
    #[test]
    fn a_body_that_is_not_an_array_of_strings_is_a_payload_parse_error() {
        for body in ["", "{", "{}", "\"system_user\"", "[1]", "[{}]", "[[\"a\"]]"] {
            let err = error_of(body);
            assert_eq!(err.id, PAYLOAD_PARSE_ERROR, "body {body:?}");
            assert_eq!(err.status_code, 400, "body {body:?}");
            assert_eq!(err.where_, "getRolesByNames", "body {body:?}");
        }
    }

    /// Branch 2 — and the parameter name is `rolenames`, not `role_names`.
    #[test]
    fn null_and_an_empty_array_name_rolenames() {
        for body in ["null", "[]", " [ ] "] {
            let err = error_of(body);
            assert_eq!(err.id, "api.context.invalid_body_param.app_error");
            assert_eq!(err.status_code, 400);
            assert_eq!(
                name_param(&err),
                Some(&serde_json::Value::String("rolenames".to_owned())),
                "body {body:?}"
            );
        }
    }

    /// Branch 3, and the boundary: 100 distinct names pass, 101 do not.
    #[test]
    fn the_cap_is_one_hundred_names_after_deduplication() {
        let names = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("role_{i}")).collect();
            serde_json::to_string(&list).expect("serialises")
        };

        assert_eq!(
            parse_role_names(names(100).as_bytes())
                .expect("100 is allowed")
                .len(),
            100
        );

        let err = error_of(&names(101));
        assert_eq!(
            err.id,
            "api.roles.get_multiple_by_name_too_many.request_error"
        );
        assert_eq!(err.status_code, 400);
        assert_eq!(err.where_, "getRolesByNames");
        assert_eq!(
            err.params.as_ref().and_then(|p| p.get("MaxNames")),
            Some(&serde_json::Value::from(100))
        );

        // De-duplication happens first, so 150 copies of one name is one name.
        let repeated: Vec<String> = std::iter::repeat_n("system_user".to_owned(), 150).collect();
        let body = serde_json::to_string(&repeated).expect("serialises");
        assert_eq!(
            parse_role_names(body.as_bytes()).expect("one distinct name"),
            vec!["system_user".to_owned()]
        );
    }

    /// Branch 4 — `rolename`, **singular**, one letter away from branch 2's parameter name.
    #[test]
    fn an_invalid_name_names_rolename_in_the_singular() {
        for bad in [
            r#"["System_User"]"#,
            r#"["system-user"]"#,
            r#"["system_user", "bad!"]"#,
            r#"[" system_user "]"#,
        ] {
            let err = error_of(bad);
            assert_eq!(err.id, "api.context.invalid_body_param.app_error", "{bad}");
            assert_eq!(
                name_param(&err),
                Some(&serde_json::Value::String("rolename".to_owned())),
                "body {bad}"
            );
        }

        // The 64-byte cap applies to body names too.
        let err = error_of(&format!(r#"["{}"]"#, "a".repeat(65)));
        assert_eq!(
            name_param(&err),
            Some(&serde_json::Value::String("rolename".to_owned()))
        );
    }

    /// A blank entry is dropped rather than rejected — so a body of nothing but blanks is a
    /// *valid* request for no roles, and answers `null`. The order of branches 2 and 4 is what
    /// makes this reachable at all: emptiness is judged before cleaning, so `[""]` is one name
    /// at branch 2 and zero names after it.
    #[test]
    fn blank_names_are_dropped_not_rejected() {
        assert_eq!(
            parse_role_names(br#"[""]"#).expect("blank is dropped"),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_role_names("[\"   \", \"\\t\"]".as_bytes()).expect("all blank"),
            Vec::<String>::new()
        );
        assert_eq!(
            parse_role_names(br#"["", "system_user"]"#).expect("one survives"),
            vec!["system_user".to_owned()]
        );
    }

    /// The happy path: sorted, de-duplicated, and the sort is Go's bytewise `sort.Strings`.
    #[test]
    fn names_arrive_sorted_and_deduplicated() {
        let names =
            parse_role_names(br#"["team_user","channel_user","team_user","channel_admin"]"#)
                .expect("valid");
        assert_eq!(
            names,
            vec![
                "channel_admin".to_owned(),
                "channel_user".to_owned(),
                "team_user".to_owned()
            ]
        );
    }

    /// The two shapes differ by exactly one byte, and getting it backwards is invisible to any
    /// test that compares parsed JSON instead of bytes.
    #[test]
    fn the_list_route_has_no_trailing_newline_and_the_single_route_does() {
        let role = Role {
            id: "3q6gugfd938tmcc7qymz7foanh".to_owned(),
            name: "system_user".to_owned(),
            permissions: Some(vec!["create_post".to_owned()]),
            scheme_id: None,
            ..Default::default()
        };

        let list = serde_json::to_vec(&[&role]).expect("serialises");
        assert_ne!(list.last(), Some(&b'\n'));

        let mut single = serde_json::to_vec(&role).expect("serialises");
        single.push(b'\n');
        assert_eq!(single.last(), Some(&b'\n'));

        // And the key order and shape Go writes, including `scheme_id` as an explicit `null`
        // rather than an omitted field — the column is NULL for every seeded role, measured
        // against the running server, and the Go field has no `omitempty`.
        assert_eq!(
            std::str::from_utf8(&single).expect("utf8"),
            "{\"id\":\"3q6gugfd938tmcc7qymz7foanh\",\"name\":\"system_user\",\
             \"display_name\":\"\",\"description\":\"\",\"create_at\":0,\"update_at\":0,\
             \"delete_at\":0,\"permissions\":[\"create_post\"],\"scheme_managed\":false,\
             \"built_in\":false,\"scheme_id\":null}\n"
        );
    }

    /// An empty result is Go's **nil** slice, which marshals as `null`. A `Vec` would have
    /// serialised as `[]`, which is why the handler special-cases the empty case rather than
    /// letting serde decide — this pins the wrong answer so the special case cannot be deleted
    /// as redundant.
    #[test]
    fn an_empty_vec_would_serialise_as_the_wrong_shape() {
        let roles: Vec<Role> = Vec::new();
        assert_eq!(serde_json::to_vec(&roles).expect("serialises"), b"[]");
    }
}
