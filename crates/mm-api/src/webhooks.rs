//! Port of `getIncomingHooks` and `getOutgoingHooks` (channels/api4/webhook.go:207, :512),
//! reached as `GET /api/v4/hooks/incoming` and `GET /api/v4/hooks/outgoing`.
//!
//! The webapp's *Integrations* pages. The single-hook reads and every write are still forwarded.
//!
//! # Two shapes on one route
//!
//! `include_total_count=true` changes the response from a bare **array** to an **object**
//! (`{"incoming_webhooks": [...], "total_count": n}`), so the parameter changes the JSON *type* a
//! client must parse. It is `strconv.ParseBool` with the error discarded (params.go:303), so
//! `=yes` and a bare `?include_total_count` are both false.
//!
//! # The count and the page can answer different questions
//!
//! `AnalyticsIncomingCount` adds its `TeamId` predicate **only when the team is non-empty**
//! (webhook_store.go:416), while the page query for the team branch always has one. With
//! `team_id` absent, the page is the caller's hooks across every team and so is the count — they
//! agree. With `team_id` present, both are scoped to it — they agree. So the asymmetry is not
//! observable through *this* route, and it is recorded rather than smoothed over because the
//! store function is shared with routes that are not migrated.

use axum::extract::{RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use mm_model::incoming_webhook::IncomingWebhooksWithCount;
use mm_model::permission::{
    PERMISSION_MANAGE_OTHERS_INCOMING_WEBHOOKS, PERMISSION_MANAGE_OTHERS_OUTGOING_WEBHOOKS,
    PERMISSION_MANAGE_OWN_INCOMING_WEBHOOKS, PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
    make_permission_error,
};
use mm_model::utils::AppError;

use crate::AppState;
use crate::auth::AuthenticatedSession;
use crate::channels::{parse_page, parse_per_page, query_first, query_flag_is_true};
use crate::error::ApiError;

const TEAM_ID_PARAM: &str = "team_id";
const INCLUDE_TOTAL_COUNT_PARAM: &str = "include_total_count";
const CHANNEL_ID_PARAM: &str = "channel_id";

/// Port of `getIncomingHooks` (webhook.go:207).
///
/// # `team_id` picks the *scope of the permission check*, not just the filter
///
/// With a `team_id` the two permissions are asked **on that team**
/// (`SessionHasPermissionToTeam`); without one they are asked at **system scope**
/// (`SessionHasPermissionTo`). A user who may manage webhooks on one team and holds no
/// system-wide grant is therefore allowed on `?team_id=<theirs>` and refused on the bare route —
/// which is the branch a port collapsing the two checks would get wrong, and it fails *open*.
///
/// # The second permission silently widens the answer
///
/// `manage_others_incoming_webhooks` does not gate the route; it **clears the user filter**, so
/// the same request returns one user's hooks or everyone's depending on a permission the response
/// says nothing about. Both branches are exercised by the parity suite.
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (webhook.go:260, :267) — **no trailing newline**. An empty list is
/// `[]`, not `null`: both store functions start from `webhooks := []*model.IncomingWebhook{}`
/// (webhook_store.go:179, :199), which is the opposite of `getUserAudits`' nil slice and is why
/// that distinction is asserted here too.
#[tracing::instrument(skip_all, fields(team_id, scoped, include_total_count, count))]
pub async fn get_incoming_hooks(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let team_id = query_first(query.as_deref(), TEAM_ID_PARAM).unwrap_or_default();
    let include_total_count = query_flag_is_true(query.as_deref(), INCLUDE_TOTAL_COUNT_PARAM);
    tracing::Span::current().record("team_id", &team_id);
    tracing::Span::current().record("scoped", !team_id.is_empty());
    tracing::Span::current().record("include_total_count", include_total_count);

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());

    // `userID := c.AppContext.Session().UserId`, cleared below if the caller may manage others'.
    let mut user_id = session.0.user_id.as_str();

    let hooks = if team_id.is_empty() {
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OWN_INCOMING_WEBHOOKS)
            .await
        {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_MANAGE_OWN_INCOMING_WEBHOOKS],
            )));
        }
        if state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OTHERS_INCOMING_WEBHOOKS)
            .await
        {
            user_id = "";
        }
        state
            .app
            .get_incoming_webhooks_page_by_user(user_id, page, per_page)
            .await?
    } else {
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OWN_INCOMING_WEBHOOKS,
            )
            .await
        {
            return Err(ApiError::from(make_permission_error(
                &session.0,
                &[&PERMISSION_MANAGE_OWN_INCOMING_WEBHOOKS],
            )));
        }
        if state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OTHERS_INCOMING_WEBHOOKS,
            )
            .await
        {
            user_id = "";
        }
        state
            .app
            .get_incoming_webhooks_for_team_page_by_user(&team_id, user_id, page, per_page)
            .await?
    };
    tracing::Span::current().record("count", hooks.len());

    let body = if include_total_count {
        // The count is taken with the **same** `user_id` the page used — so clearing it above
        // widens both together. Passing the session's id here while the page used `""` would
        // ship a total smaller than the array beside it.
        let total_count = state
            .app
            .get_incoming_webhooks_count(&team_id, user_id)
            .await?;
        encode(&IncomingWebhooksWithCount {
            // `Some`, never `None`: `model.IncomingWebhooksWithCount.Webhooks` is a Go slice and
            // therefore nullable, but both store functions start from `[]*model.IncomingWebhook{}`
            // so this route cannot produce the nil. The `Option` in the model is for the decoding
            // direction — see the fixture.
            webhooks: Some(hooks),
            total_count,
        })?
    } else {
        encode(&hooks)?
    };

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

/// `json.Marshal` — no encoder, so no trailing newline, and its failure is Go's own
/// `api.marshal_error` **500 raised in the handler**, not the app layer.
fn encode<T: serde::Serialize>(value: &T) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value).map_err(|err| {
        tracing::error!(error = %err, "failed to serialise the incoming webhooks");
        ApiError::from(AppError::new(
            "getIncomingHooks",
            "api.marshal_error",
            None,
            String::new(),
            500,
        ))
    })
}

/// Port of `getOutgoingHooks` (webhook.go:512).
///
/// # Three scopes, checked in a fixed order, and `channel_id` wins
///
/// `channel_id` first, then `team_id`, then neither — and the branches are exclusive, so a
/// request carrying **both** is a *channel* request and the team is ignored entirely. Each branch
/// asks the same pair of permissions at a different scope:
/// `SessionHasPermissionToChannel`, `SessionHasPermissionToTeam`, `SessionHasPermissionTo`. A
/// port that collapsed them fails **open**, exactly as on the incoming route.
///
/// # No `include_total_count`
///
/// The incoming route has one; this one does not, so the response is always an array. The two
/// handlers sit forty lines apart in the same Go file and differ in that, in the number of
/// scopes, and in the owner column their store filters on (`CreatorId`, not `UserId`).
///
/// # Wire format
///
/// `json.Marshal` + `w.Write` (webhook.go:566, :572) — no trailing newline, and an empty list is
/// `[]` because all three store functions start from `[]*model.OutgoingWebhook{}`.
#[tracing::instrument(skip_all, fields(channel_id, team_id, scope, count))]
pub async fn get_outgoing_hooks(
    State(state): State<AppState>,
    RawQuery(query): RawQuery,
    session: AuthenticatedSession,
) -> Result<Response, ApiError> {
    let channel_id = query_first(query.as_deref(), CHANNEL_ID_PARAM).unwrap_or_default();
    let team_id = query_first(query.as_deref(), TEAM_ID_PARAM).unwrap_or_default();
    tracing::Span::current().record("channel_id", &channel_id);
    tracing::Span::current().record("team_id", &team_id);

    let page = parse_page(query.as_deref());
    let per_page = parse_per_page(query.as_deref());

    let mut user_id = session.0.user_id.as_str();
    let refused = || {
        ApiError::from(make_permission_error(
            &session.0,
            &[&PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS],
        ))
    };

    let hooks = if !channel_id.is_empty() {
        tracing::Span::current().record("scope", "channel");
        // `SessionHasPermissionToChannel` returns `(allowed, is_member)`; Go discards the second
        // value here with `ok, _ :=`, so membership does not enter the decision.
        let (allowed, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &channel_id,
                &PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
            )
            .await;
        if !allowed {
            return Err(refused());
        }
        let (others, _) = state
            .app
            .session_has_permission_to_channel(
                &session.0,
                &channel_id,
                &PERMISSION_MANAGE_OTHERS_OUTGOING_WEBHOOKS,
            )
            .await;
        if others {
            user_id = "";
        }
        state
            .app
            .get_outgoing_webhooks_for_channel_page_by_user(&channel_id, user_id, page, per_page)
            .await?
    } else if !team_id.is_empty() {
        tracing::Span::current().record("scope", "team");
        if !state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS,
            )
            .await
        {
            return Err(refused());
        }
        if state
            .app
            .session_has_permission_to_team(
                &session.0,
                &team_id,
                &PERMISSION_MANAGE_OTHERS_OUTGOING_WEBHOOKS,
            )
            .await
        {
            user_id = "";
        }
        state
            .app
            .get_outgoing_webhooks_for_team_page_by_user(&team_id, user_id, page, per_page)
            .await?
    } else {
        tracing::Span::current().record("scope", "system");
        if !state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OWN_OUTGOING_WEBHOOKS)
            .await
        {
            return Err(refused());
        }
        if state
            .app
            .session_has_permission_to(&session.0, &PERMISSION_MANAGE_OTHERS_OUTGOING_WEBHOOKS)
            .await
        {
            user_id = "";
        }
        state
            .app
            .get_outgoing_webhooks_page_by_user(user_id, page, per_page)
            .await?
    };
    tracing::Span::current().record("count", hooks.len());

    Ok((
        StatusCode::OK,
        [
            ("Content-Type", "application/json"),
            ("x-mmrs-served-by", "rust"),
        ],
        encode(&hooks)?,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::incoming_webhook::IncomingWebhook;

    fn hook() -> IncomingWebhook {
        IncomingWebhook {
            id: "nnk54zogfbrxxeepga9g6m6c5e".to_owned(),
            create_at: 1_788_636_490_668,
            update_at: 1_788_636_490_668,
            delete_at: 0,
            user_id: "6rtg4qbe5bn55mw5t6gphxyaxa".to_owned(),
            channel_id: "ezytoszqbfg4i8tofm8zis3apc".to_owned(),
            team_id: "tnewcuy4ztgw5doi7j5ytqxg9w".to_owned(),
            display_name: "mmrs hook".to_owned(),
            description: "probe".to_owned(),
            username: String::new(),
            icon_url: String::new(),
            channel_locked: true,
            last_used: 0,
        }
    }

    /// The two shapes are different JSON **types**, and the bare one is an array even when empty.
    #[test]
    fn the_two_shapes_are_an_array_and_an_object() {
        let empty: Vec<IncomingWebhook> = Vec::new();
        assert_eq!(encode(&empty).expect("encodes"), b"[]");

        let wrapped = IncomingWebhooksWithCount {
            webhooks: Some(Vec::new()),
            total_count: 0,
        };
        assert_eq!(
            String::from_utf8(encode(&wrapped).expect("encodes")).expect("utf-8"),
            r#"{"incoming_webhooks":[],"total_count":0}"#
        );
    }

    /// Every key, in Go's field order, none omitted — asserted on the bytes, because a
    /// `serde_json::Value` object is a `BTreeMap` and would sort them.
    #[test]
    fn a_hook_serialises_in_gos_field_order() {
        assert_eq!(
            String::from_utf8(encode(&vec![hook()]).expect("encodes")).expect("utf-8"),
            concat!(
                r#"[{"id":"nnk54zogfbrxxeepga9g6m6c5e","create_at":1788636490668,"#,
                r#""update_at":1788636490668,"delete_at":0,"#,
                r#""user_id":"6rtg4qbe5bn55mw5t6gphxyaxa","#,
                r#""channel_id":"ezytoszqbfg4i8tofm8zis3apc","#,
                r#""team_id":"tnewcuy4ztgw5doi7j5ytqxg9w","display_name":"mmrs hook","#,
                r#""description":"probe","username":"","icon_url":"","#,
                r#""channel_locked":true,"last_used":0}]"#
            )
        );
    }

    /// `strconv.ParseBool` with the error discarded: a bare key and `=yes` are both false, so
    /// they keep the array shape.
    #[test]
    fn include_total_count_follows_gos_boolean_rules() {
        for query in [
            "include_total_count=true",
            "include_total_count=1",
            "include_total_count=T",
        ] {
            assert!(
                query_flag_is_true(Some(query), INCLUDE_TOTAL_COUNT_PARAM),
                "{query}"
            );
        }
        for query in [
            "include_total_count",
            "include_total_count=",
            "include_total_count=yes",
            "include_total_count=TRUEISH",
            "",
        ] {
            assert!(
                !query_flag_is_true(Some(query), INCLUDE_TOTAL_COUNT_PARAM),
                "{query}"
            );
        }
    }
}
