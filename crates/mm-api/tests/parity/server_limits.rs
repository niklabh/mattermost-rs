//! Cross-server parity for `GET /api/v4/limits/server` — `getServerLimits`.
//!
//! ```sh
//! scripts/parity.sh -p mm-api --test parity server_limits
//! ```
//!
//! # Two answers, no refusal
//!
//! Every session gets a 200. An **admin** — `manage_system` *and*
//! `sysconsole_read_user_management_users`, both system-scoped — gets the seat limits and the
//! active user count; everyone else gets zeros. That is what lets the webapp call this
//! unconditionally on every login, and it is the only decision the route makes on an unlicensed
//! server.
//!
//! # The count moves, so it is bracketed
//!
//! `activeUserCount` is `COUNT(*)` over every non-deleted, non-bot, non-remote user — and this
//! binary creates users throughout a run. The byte comparison goes through
//! [`common::fetch_both_stable`], which accepts our answer if it matches either of Go's two reads
//! around it. See [D-167].

use crate::common;

use common::{
    ACTIVE_LICENCE_ROW, RUST, client, create_plain_user, create_team, fetch_both_raw,
    fetch_both_stable, go_minted_token, set_active_licence_id, stack_enabled,
};

const PATH: &str = "/api/v4/limits/server";

static PLAIN: tokio::sync::OnceCell<String> = tokio::sync::OnceCell::const_new();

async fn plain_token(client: &reqwest::Client, token: &str) -> &'static String {
    PLAIN
        .get_or_init(|| async {
            let team = create_team(client, token, "serverlimits").await;
            create_plain_user(client, token, &team, "serverlimits")
                .await
                .token
        })
        .await
}

/// The admin's answer, byte for byte: the two hard-coded seat limits and a live count.
#[tokio::test]
async fn an_admin_sees_the_seat_limits_and_the_count() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let (go, rs) = fetch_both_stable(&client, &token, PATH).await;
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );
    assert!(
        rs.ends_with(b"\n"),
        "`json.NewEncoder(w).Encode` writes a newline"
    );

    let limits: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    assert_eq!(
        limits["maxUsersLimit"], 200,
        "the unlicensed soft limit is a constant in `app/limits.go`"
    );
    assert_eq!(limits["maxUsersHardLimit"], 250);
    assert!(
        limits["activeUserCount"].as_i64().unwrap_or(0) > 0,
        "the count is live: {limits}"
    );
    assert_eq!(
        limits["singleChannelGuestCount"], 0,
        "`shouldTrackSingleChannelGuests` is false without a licence, so the guest scan never runs"
    );
    assert_eq!(limits["postHistoryLimit"], 0);
    assert_eq!(limits["lastAccessiblePostTime"], 0);

    // Every field is on the wire — `model.ServerLimits` has no `omitempty` — and the keys are
    // **camelCase**, unlike almost everything else in this API.
    assert_eq!(
        limits.as_object().expect("an object").len(),
        7,
        "seven fields, none omitted: {limits}"
    );
    assert!(
        limits.get("maxUsersLimit").is_some() && limits.get("max_users_limit").is_none(),
        "camelCase, not snake: {limits}"
    );
}

/// **A non-admin gets zeros, not a refusal.** Every field, including the seat limits the app layer
/// computed — the handler rebuilds the object and throws them away.
#[tokio::test]
async fn a_plain_user_gets_zeros_and_a_200() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let plain = plain_token(&client, &token).await;

    let ((go_status, go), (rs_status, rs)) = fetch_both_raw(&client, plain, PATH).await;
    assert_eq!(go_status, 200, "there is no refusal on this route");
    assert_eq!(rs_status, go_status, "{PATH}");
    assert_eq!(
        String::from_utf8_lossy(&go),
        String::from_utf8_lossy(&rs),
        "{PATH}"
    );

    let limits: serde_json::Value = serde_json::from_slice(&go).expect("decodes");
    for field in [
        "maxUsersLimit",
        "maxUsersHardLimit",
        "activeUserCount",
        "singleChannelGuestCount",
        "singleChannelGuestLimit",
        "postHistoryLimit",
        "lastAccessiblePostTime",
    ] {
        assert_eq!(
            limits[field], 0,
            "{field} is zeroed for a non-admin: {limits}"
        );
    }
}

/// The two answers are **different**, so the assertions above are about the caller and not about
/// the server having nothing to report.
#[tokio::test]
async fn the_two_answers_differ() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let plain = plain_token(&client, &token).await;

    let (admin, _) = fetch_both_stable(&client, &token, PATH).await;
    let ((_, non_admin), _) = fetch_both_raw(&client, plain, PATH).await;
    assert_ne!(
        String::from_utf8_lossy(&admin),
        String::from_utf8_lossy(&non_admin),
        "an admin and a plain user must not get the same object"
    );
}

/// **The boundary.** Every non-zero field a licence would produce comes from the licence body, so
/// a licensed installation goes back to the proxy. Holds the shared lock exclusively.
#[tokio::test]
async fn a_license_row_hands_the_route_back_to_go() {
    if !stack_enabled() {
        return;
    }
    let _exclusive = ACTIVE_LICENCE_ROW.write().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let served_by = async || {
        client
            .get(format!("{RUST}{PATH}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await
            .expect("we answer")
            .headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };

    set_active_licence_id(None).await;
    assert_eq!(served_by().await.as_deref(), Some("rust"));

    set_active_licence_id(Some("mmrslicence000000000000001")).await;
    let forwarded = served_by().await;

    set_active_licence_id(None).await;
    let cleared = served_by().await;

    assert_eq!(
        forwarded.as_deref(),
        Some("go"),
        "the seat limits would come from the licence body, which we cannot read"
    );
    assert_eq!(cleared.as_deref(), Some("rust"));
}

/// Registering the `GET` must not turn another method on the path into our 405.
#[tokio::test]
async fn other_methods_are_forwarded() {
    if !stack_enabled() {
        return;
    }
    let _unlicensed = ACTIVE_LICENCE_ROW.read().await;
    let client = client();
    let token = go_minted_token(&client).await;

    let ours = client
        .post(format!("{RUST}{PATH}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("we answer");
    assert_eq!(
        ours.headers()
            .get("x-mmrs-served-by")
            .and_then(|v| v.to_str().ok()),
        Some("go"),
        "POST {PATH} must be forwarded"
    );
}
