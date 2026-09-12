//! Cross-server parity for the seven routes of `api4/channel_join_request.go` — the
//! discoverable-private-channel join queue.
//!
//! ```sh
//! scripts/go-discoverable.sh start && scripts/parity.sh --test parity channel_join_requests
//! ```
//!
//! # This suite has two halves because the feature has two states, and only one of them ships
//!
//! **Dark.** `FeatureFlags.DiscoverableChannels` is `false` at the pinned SHA and unset on this
//! deployment, so `initChannelJoinRequestRoutes` registers nothing and every request is the mux's
//! own `api.context.404.app_error`. [`the_dark_feature_is_a_mux_404_on_both_servers`] is the only
//! test here that runs unconditionally, and it asserts the port **forwards** rather than
//! answering: a locally-minted 404 would have to reproduce a `detailed_error` that interpolates
//! the request URL, and there is no reason to own that string. [D-153].
//!
//! **Lit.** Everything else needs two extra processes — a Go server with the flag on
//! (`scripts/go-discoverable.sh`, which explains why the main one cannot simply have it turned on)
//! and a second `mm-api` with the same flag, pointed at it. Both are started by [`lit`].
//!
//! # Why every comparison here is a *shared-row* comparison
//!
//! Both servers share one database and one `ChannelJoinRequests` table, so a read of the same rows
//! must produce identical bytes — key order, the trailing newline and all. The writes are
//! sequenced rather than duplicated: one server creates, the other reads, and the *state machine*
//! is driven across both. A row can only be withdrawn once and reviewed once, so posting the same
//! body to both servers would compare two different lifecycles.
//!
//! # The fixture channel is created on the **discoverable** Go
//!
//! `createChannel` on the stack's Go refuses `"discoverable": true` with
//! `api.channel.discoverable_join_request.feature_disabled.app_error` at 400 — the same flag. So
//! the fixture cannot come from `common::create_channel`, and every setup call here goes to the
//! oracle.

use crate::common::{
    self, GO, RUST, SecondServer, client, create_plain_user, delete_channel, delete_plain_user,
    go_minted_token, stack_enabled,
};

fn method(raw: &str) -> reqwest::Method {
    raw.parse().expect("a known method")
}

/// Every route+method pair the file registers, with `{channel_id}` and `{user_id}` filled in.
fn every_route(channel: &str, request: &str, user: &str) -> Vec<(&'static str, String)> {
    vec![
        ("POST", format!("/api/v4/channels/{channel}/join_request")),
        ("GET", format!("/api/v4/channels/{channel}/join_request")),
        ("DELETE", format!("/api/v4/channels/{channel}/join_request")),
        ("GET", format!("/api/v4/channels/{channel}/join_requests")),
        (
            "GET",
            format!("/api/v4/channels/{channel}/join_requests/count"),
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{channel}/join_requests/{request}"),
        ),
        ("GET", format!("/api/v4/users/{user}/channel_join_requests")),
    ]
}

/// The state of the deployment, and the only test that needs no oracle.
///
/// With the flag off gorilla/mux has never heard of any of these paths. Both servers must answer
/// the mux's 404, and ours must do it by **forwarding** — `x-mmrs-served-by` absent — because the
/// body interpolates the request URL.
#[tokio::test]
async fn the_dark_feature_is_a_mux_404_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let channel = common::a_channel_the_user_is_in(&http, &token).await;
    let user = common::logged_in_user_id();

    for (verb, path) in every_route(&channel, "dddddddddddddddddddddddddd", user) {
        let send = async |base: &str| {
            let response = http
                .request(method(verb), format!("{base}{path}"))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body("{}")
                .send()
                .await
                .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
            let status = response.status().as_u16();
            let served_by = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            (
                status,
                served_by,
                response.bytes().await.expect("a body").to_vec(),
            )
        };

        let (go_status, _, go_body) = send(GO).await;
        let (rs_status, rs_served_by, rs_body) = send(RUST).await;

        assert_eq!(
            go_status, 404,
            "{verb} {path}: the flag is off, so Go's mux must not know this route"
        );
        assert_eq!(rs_status, go_status, "{verb} {path}: status");
        assert_eq!(
            rs_served_by.as_deref(),
            Some("go"),
            "{verb} {path}: a dark route must be forwarded, not answered locally"
        );

        let go: serde_json::Value = serde_json::from_slice(&go_body).expect("Go's body is JSON");
        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("our body is JSON");
        assert_eq!(
            go["id"], "api.context.404.app_error",
            "{verb} {path}: this is the mux's refusal, not a handler's"
        );
        assert_eq!(go["id"], rs["id"], "{verb} {path}: id");
        assert_eq!(
            go["detailed_error"], rs["detailed_error"],
            "{verb} {path}: the URL interpolation is Go's own, so it must match exactly"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Lit — a discoverable-on Go beside a discoverable-on mm-api
// ---------------------------------------------------------------------------------------------

/// The two extra processes. No `reqwest::Client` in the `static`, for the runtime-binding reason
/// [`crate::parity::views`] records at length.
struct Discoverable {
    go: String,
    rust: String,
    _child: SecondServer,
}

struct Lit {
    go: String,
    rust: String,
    token: String,
    http: reqwest::Client,
}

/// `MMRS_GO_PORT + 31`, the port `scripts/go-discoverable.sh` binds — derived from [`GO`] so a
/// worktree on any stack finds its own.
fn discoverable_go_base() -> Option<String> {
    let port: u16 = GO.rsplit(':').next()?.parse().ok()?;
    Some(format!("http://localhost:{}", port + 31))
}

static LIT: tokio::sync::OnceCell<Option<Discoverable>> = tokio::sync::OnceCell::const_new();

async fn lit() -> Option<Lit> {
    let disc = LIT.get_or_init(start_discoverable).await.as_ref()?;
    let http = client();
    let token = go_minted_token(&http).await;
    Some(Lit {
        go: disc.go.clone(),
        rust: disc.rust.clone(),
        token,
        http,
    })
}

async fn start_discoverable() -> Option<Discoverable> {
    if !stack_enabled() {
        return None;
    }
    let http = client();
    let go = discoverable_go_base()?;
    if !http
        .get(format!("{go}/api/v4/system/ping"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        // **Panic rather than skip**, for the reason `parity/views.rs` spells out: `stack_enabled`
        // is already true, so every test below would pass while asserting nothing, and cargo hides
        // the output of a passing test.
        panic!(
            "channel_join_requests: no discoverable oracle at {go}. Every test in this file would \
             pass without asserting anything. Run `scripts/go-discoverable.sh start`."
        );
    }

    let rust_server = SecondServer::start(
        8083,
        &[
            ("MM_FEATUREFLAGS_DISCOVERABLECHANNELS", "true"),
            // The *discoverable* Go, not the stack's: a forward from this server must reach a
            // process that knows these routes.
            ("MM_GO_UPSTREAM", &go),
        ],
    )
    .await?;

    let rust = rust_server.base.clone();
    Some(Discoverable {
        go,
        rust,
        _child: rust_server,
    })
}

impl Lit {
    async fn send(
        &self,
        base: &str,
        verb: &str,
        path: &str,
        token: &str,
        body: Option<&serde_json::Value>,
    ) -> (u16, Vec<u8>) {
        let mut request = self
            .http
            .request(method(verb), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {token}"));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        (status, response.bytes().await.expect("a body").to_vec())
    }

    /// Run the same request against both and return `((go_status, go_body), (rs_status, rs_body))`.
    async fn both(
        &self,
        verb: &str,
        path: &str,
        token: &str,
        body: Option<&serde_json::Value>,
    ) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
        let go = self.send(&self.go.clone(), verb, path, token, body).await;
        let rs = self.send(&self.rust.clone(), verb, path, token, body).await;
        (go, rs)
    }

    async fn team(&self) -> String {
        let teams: serde_json::Value = self
            .http
            .get(format!("{}/api/v4/users/me/teams", self.go))
            .header("Authorization", format!("Bearer {}", self.token))
            .send()
            .await
            .expect("the oracle answers")
            .json()
            .await
            .expect("teams decode");
        teams
            .as_array()
            .and_then(|t| t.first())
            .and_then(|t| t["id"].as_str())
            .expect("the fixture user belongs to a team")
            .to_owned()
    }

    /// A **discoverable private** channel, created on the oracle because the stack's Go refuses
    /// the `discoverable` field outright.
    async fn discoverable_channel(&self, tag: &str) -> String {
        let team = self.team().await;
        let created: serde_json::Value = self
            .http
            .post(format!("{}/api/v4/channels", self.go))
            .header("Authorization", format!("Bearer {}", self.token))
            .json(&serde_json::json!({
                "team_id": team,
                "name": format!("mmrs-parity-{tag}"),
                "display_name": format!("mmrs parity {tag}"),
                "type": "P",
                "discoverable": true,
            }))
            .send()
            .await
            .expect("the oracle answers")
            .json()
            .await
            .expect("the channel decodes");
        created["id"]
            .as_str()
            .unwrap_or_else(|| panic!("creating the discoverable channel failed: {created}"))
            .to_owned()
    }
}

/// Decode a body, asserting it parses — every route here answers JSON except the bodiless 404.
fn json(body: &[u8]) -> serde_json::Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(body)))
}

// ---------------------------------------------------------------------------------------------
// The lit suite
// ---------------------------------------------------------------------------------------------

/// The whole lifecycle, driven **across** the two servers: Go creates, we read; we withdraw, Go
/// reads. A row can only reach each terminal state once, so duplicating the writes would compare
/// two different lifecycles rather than one.
#[tokio::test]
async fn the_request_lifecycle_agrees_across_both_servers() {
    let Some(lit) = lit().await else { return };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = lit.team().await;
    let channel = lit.discoverable_channel("jrlife").await;
    let user = create_plain_user(&http, &admin, &team, "jrlife").await;

    // --- POST on ours; the body is the created row, at 201 ---
    let (status, body) = lit
        .send(
            &lit.rust.clone(),
            "POST",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            Some(&serde_json::json!({"message": "please"})),
        )
        .await;
    assert_eq!(
        status,
        201,
        "a create is a 201: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        body.last(),
        Some(&b'\n'),
        "json.NewEncoder appends a newline"
    );
    let created = json(&body);
    assert_eq!(created["status"], "pending");
    assert_eq!(created["message"], "please");
    assert_eq!(created["denial_reason"], "");
    assert_eq!(created["reviewed_by"], "");
    assert_eq!(created["reviewed_at"], 0);
    assert_eq!(
        created["create_at"], created["update_at"],
        "PreSave ties them"
    );

    // --- both servers read the same row byte for byte ---
    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            None,
        )
        .await;
    assert_eq!(go, rs, "GET join_request: the same row must encode alike");
    assert_eq!(go.0, 200);

    // --- POSTing again is idempotent and returns the **original** row, still 201 ---
    let (go, rs) = lit
        .both(
            "POST",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            Some(&serde_json::json!({"message": "second"})),
        )
        .await;
    assert_eq!(go.0, 201, "a conflicting create is still a 201");
    assert_eq!(go, rs, "a conflicting create returns the existing row");
    assert_eq!(
        json(&go.1)["id"],
        created["id"],
        "the existing row's id, not a new one"
    );
    assert_eq!(
        json(&go.1)["message"],
        "please",
        "the *original* message survives the second POST"
    );

    // --- the admin queue and the count agree ---
    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_requests"),
            &admin,
            None,
        )
        .await;
    assert_eq!(go, rs, "the admin queue");
    assert_eq!(json(&go.1)["total_count"], 1);

    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_requests/count"),
            &admin,
            None,
        )
        .await;
    assert_eq!(go, rs, "the badge count");
    assert_eq!(json(&go.1), serde_json::json!({"count": 1}));

    // --- the requester's own list ---
    let (go, rs) = lit
        .both(
            "GET",
            "/api/v4/users/me/channel_join_requests",
            &user.token,
            None,
        )
        .await;
    assert_eq!(go, rs, "the requester's list");
    assert_eq!(json(&go.1)["total_count"], 1);

    // --- withdraw on ours: status flips and the message is **dropped** ---
    let (status, body) = lit
        .send(
            &lit.rust.clone(),
            "DELETE",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            None,
        )
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let withdrawn = json(&body);
    assert_eq!(withdrawn["status"], "withdrawn");
    assert_eq!(
        withdrawn["message"], "",
        "withdrawing clears the requester's note"
    );
    assert_eq!(withdrawn["reviewed_by"], "", "a withdrawal has no reviewer");
    assert_eq!(withdrawn["reviewed_at"], 0);
    assert_eq!(withdrawn["id"], created["id"]);

    // --- and the pending lookup is a **bodiless** 404 on both ---
    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            None,
        )
        .await;
    assert_eq!(go.0, 404);
    assert_eq!(go.1, Vec::<u8>::new(), "Go writes the header and no body");
    assert_eq!(go, rs, "a miss is a bodiless 404 on both");

    // --- a second withdrawal is the not-found 404, with a body this time ---
    let (go, rs) = lit
        .both(
            "DELETE",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            None,
        )
        .await;
    assert_eq!(go.0, 404);
    assert_eq!(
        json(&go.1)["id"],
        "app.channel.join_request.not_found.app_error"
    );
    assert_eq!(json(&go.1)["id"], json(&rs.1)["id"]);
    assert_eq!(go.0, rs.0);

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &channel).await;
}

/// The review: a denial carries its reason, drops the message, stamps the reviewer — and a second
/// review of the same row is a **409**.
#[tokio::test]
async fn a_denial_records_the_reviewer_and_refuses_a_second_review() {
    let Some(lit) = lit().await else { return };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = lit.team().await;
    let channel = lit.discoverable_channel("jrdeny").await;
    let user = create_plain_user(&http, &admin, &team, "jrdeny").await;

    let (status, body) = lit
        .send(
            &lit.go.clone(),
            "POST",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            Some(&serde_json::json!({"message": "let me in"})),
        )
        .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let request_id = json(&body)["id"].as_str().expect("an id").to_owned();

    // Deny on **ours**.
    let (status, body) = lit
        .send(
            &lit.rust.clone(),
            "PATCH",
            &format!("/api/v4/channels/{channel}/join_requests/{request_id}"),
            &admin,
            Some(&serde_json::json!({"status": "denied", "denial_reason": "not now"})),
        )
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let denied = json(&body);
    assert_eq!(denied["status"], "denied");
    assert_eq!(denied["denial_reason"], "not now");
    assert_eq!(denied["message"], "", "the review drops the free text");
    assert_eq!(
        denied["reviewed_by"],
        common::logged_in_user_id(),
        "the session user is the reviewer"
    );
    assert_ne!(denied["reviewed_at"], 0);

    // Go must read back exactly what we wrote.
    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_requests?status=denied"),
            &admin,
            None,
        )
        .await;
    assert_eq!(go, rs, "the denied queue");
    assert_eq!(json(&go.1)["requests"][0]["denial_reason"], "not now");

    // A second review is a 409 on both.
    let (go, rs) = lit
        .both(
            "PATCH",
            &format!("/api/v4/channels/{channel}/join_requests/{request_id}"),
            &admin,
            Some(&serde_json::json!({"status": "approved"})),
        )
        .await;
    assert_eq!(go.0, 409, "a reviewed row is no longer pending");
    assert_eq!(go.0, rs.0);
    assert_eq!(
        json(&go.1)["id"],
        "api.channel.discoverable_join_request.not_pending.app_error"
    );
    assert_eq!(json(&go.1)["id"], json(&rs.1)["id"]);

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &channel).await;
}

/// Approving adds the member, and the approved row keeps no denial reason.
#[tokio::test]
async fn an_approval_adds_the_member() {
    let Some(lit) = lit().await else { return };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = lit.team().await;
    let channel = lit.discoverable_channel("jrok").await;
    let user = create_plain_user(&http, &admin, &team, "jrok").await;

    let (status, body) = lit
        .send(
            &lit.go.clone(),
            "POST",
            &format!("/api/v4/channels/{channel}/join_request"),
            &user.token,
            Some(&serde_json::json!({"message": "hello"})),
        )
        .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let request_id = json(&body)["id"].as_str().expect("an id").to_owned();

    let (status, body) = lit
        .send(
            &lit.rust.clone(),
            "PATCH",
            &format!("/api/v4/channels/{channel}/join_requests/{request_id}"),
            &admin,
            // A denial reason on an *approval* is discarded: `IsValid` would refuse the row
            // otherwise, which is why the app layer clears it before re-setting.
            Some(&serde_json::json!({"status": "approved", "denial_reason": "ignored"})),
        )
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let approved = json(&body);
    assert_eq!(approved["status"], "approved");
    assert_eq!(
        approved["denial_reason"], "",
        "an approval never carries a denial reason"
    );

    // The membership is real, and Go agrees.
    let (status, body) = lit
        .send(
            &lit.go.clone(),
            "GET",
            &format!("/api/v4/channels/{channel}/members/{}", user.id),
            &admin,
            None,
        )
        .await;
    assert_eq!(
        status,
        200,
        "approving must have added the member: {}",
        String::from_utf8_lossy(&body)
    );

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &channel).await;
}

/// The refusals: every guard branch, every permission gate, and both malformed-body parameters.
#[tokio::test]
async fn the_refusals_match_go() {
    let Some(lit) = lit().await else { return };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = lit.team().await;
    let discoverable = lit.discoverable_channel("jrref").await;
    let user = create_plain_user(&http, &admin, &team, "jrref").await;

    // A plain private channel and a public one, for the two channel-shape refusals.
    let private = common::create_channel_typed(&http, &admin, &team, "jrrefp", "P").await;
    let public = common::create_channel_typed(&http, &admin, &team, "jrrefo", "O").await;

    let cases: Vec<(&str, String, &str, Option<serde_json::Value>)> = vec![
        // --- requestJoinChannel ---
        (
            "POST",
            format!("/api/v4/channels/{public}/join_request"),
            &user.token,
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            format!("/api/v4/channels/{private}/join_request"),
            &user.token,
            Some(serde_json::json!({})),
        ),
        (
            "POST",
            "/api/v4/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/join_request".to_owned(),
            &user.token,
            Some(serde_json::json!({})),
        ),
        // The admin *is* a member of the channel it created.
        (
            "POST",
            format!("/api/v4/channels/{discoverable}/join_request"),
            &admin,
            Some(serde_json::json!({})),
        ),
        // --- the permission gates ---
        (
            "GET",
            format!("/api/v4/channels/{discoverable}/join_requests"),
            &user.token,
            None,
        ),
        (
            "GET",
            format!("/api/v4/channels/{discoverable}/join_requests/count"),
            &user.token,
            None,
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            &user.token,
            Some(serde_json::json!({"status": "approved"})),
        ),
        // --- the url-parameter refusals, which precede the permission check ---
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/short"),
            &user.token,
            Some(serde_json::json!({"status": "approved"})),
        ),
        // A short but *alphanumeric* channel id: Go's mux matches the segment and
        // `RequireChannelId` refuses it, so this is the handler's 400 and not the mux's 404.
        // One per route, because each handler calls `RequireChannelId` for itself.
        (
            "POST",
            "/api/v4/channels/aaaa/join_request".to_owned(),
            &admin,
            Some(serde_json::json!({})),
        ),
        (
            "GET",
            "/api/v4/channels/aaaa/join_request".to_owned(),
            &admin,
            None,
        ),
        (
            "DELETE",
            "/api/v4/channels/aaaa/join_request".to_owned(),
            &admin,
            None,
        ),
        (
            "GET",
            "/api/v4/channels/aaaa/join_requests".to_owned(),
            &admin,
            None,
        ),
        (
            "GET",
            "/api/v4/channels/aaaa/join_requests/count".to_owned(),
            &admin,
            None,
        ),
        (
            "PATCH",
            "/api/v4/channels/aaaa/join_requests/dddddddddddddddddddddddddd".to_owned(),
            &admin,
            Some(serde_json::json!({"status": "denied"})),
        ),
        (
            "GET",
            "/api/v4/users/aaaa/channel_join_requests".to_owned(),
            &admin,
            None,
        ),
        // --- the patch's accepted statuses ---
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            &admin,
            Some(serde_json::json!({"status": "withdrawn"})),
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            &admin,
            Some(serde_json::json!({"status": "pending"})),
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            &admin,
            Some(serde_json::json!({})),
        ),
        // A well-formed review of an id that does not exist — the app layer's 404.
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            &admin,
            Some(serde_json::json!({"status": "denied"})),
        ),
        // --- another user's list ---
        (
            "GET",
            format!("/api/v4/users/{}/channel_join_requests", user.id),
            &admin,
            None,
        ),
    ];

    for (verb, path, token, body) in cases {
        let (go, rs) = lit.both(verb, &path, token, body.as_ref()).await;
        assert_eq!(go.0, rs.0, "{verb} {path}: status");
        assert!(go.0 >= 400, "{verb} {path}: every case here is a refusal");
        // `message` is the raw id here and English on Go until i18n lands — [D-092] — and
        // `request_id` is per request. Every other field must agree exactly.
        common::assert_error_bodies_match_except_known_gaps(
            &go.1,
            &rs.1,
            &format!("{verb} {path}"),
        );
    }

    // An unrecognised status is silently rewritten to `pending`, not refused — a **200**, so
    // these compare byte for byte rather than through the error helper.
    for (path, token) in [
        (
            format!("/api/v4/channels/{discoverable}/join_requests?status=bogus"),
            &admin,
        ),
        (
            "/api/v4/users/me/channel_join_requests?status=bogus&per_page=0&page=-1".to_owned(),
            &user.token,
        ),
    ] {
        let (go, rs) = lit.both("GET", &path, token, None).await;
        assert_eq!(go.0, 200, "{path}: a bogus status is not a refusal");
        assert_eq!(go.0, rs.0, "{path}: status");
        assert_eq!(
            go.1,
            rs.1,
            "{path}: body — go={} rs={}",
            String::from_utf8_lossy(&go.1),
            String::from_utf8_lossy(&rs.1)
        );
    }

    // The malformed-body 400s carry **different parameter names**, and the message interpolates
    // it — so these go through `assert_error_bodies_match_except_known_gaps`' sibling: a direct
    // byte comparison after stripping the per-request id.
    for (verb, path, param) in [
        (
            "POST",
            format!("/api/v4/channels/{discoverable}/join_request"),
            "body",
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{discoverable}/join_requests/dddddddddddddddddddddddddd"),
            "channel_join_request_patch",
        ),
    ] {
        for raw in ["", "{"] {
            let send = async |base: &str| {
                let response = lit
                    .http
                    .request(method(verb), format!("{base}{path}"))
                    .header("Authorization", format!("Bearer {admin}"))
                    .header("Content-Type", "application/json")
                    .body(raw)
                    .send()
                    .await
                    .expect("a response");
                (
                    response.status().as_u16(),
                    response.bytes().await.expect("a body").to_vec(),
                )
            };
            let go = send(&lit.go).await;
            let rs = send(&lit.rust).await;
            assert_eq!(go.0, 400, "{verb} {path} {raw:?}");
            assert_eq!(rs.0, go.0, "{verb} {path} {raw:?}: status");
            let go_body = common::assert_error_bodies_match_except_known_gaps(
                &go.1,
                &rs.1,
                &format!("{verb} {path} {raw:?}"),
            );
            assert_eq!(
                go_body["id"], "api.context.invalid_body_param.app_error",
                "{verb} {path}"
            );
            // Go's message interpolates the parameter name; ours is the raw id ([D-092]), so the
            // name is asserted on **Go's** side — it is the only place the two handlers' different
            // spellings (`body` against `channel_join_request_patch`) are visible on the wire.
            assert!(
                go_body["message"]
                    .as_str()
                    .is_some_and(|m| m.contains(param)),
                "{verb} {path}: Go's message must name {param:?}, got {}",
                go_body["message"]
            );
        }
    }

    delete_plain_user(&http, &admin, &user.id).await;
    delete_channel(&http, &admin, &discoverable).await;
    delete_channel(&http, &admin, &private).await;
    delete_channel(&http, &admin, &public).await;
}

/// Paging and ordering, on a channel seeded with several rows in a known order.
///
/// The store orders `CreateAt DESC, Id DESC`, and the tie-break matters: several requests created
/// in the same millisecond would otherwise come back in an arbitrary order and this test would
/// flake. Each row here is a distinct user, and the assertion is that **both servers agree**,
/// which holds whatever the order turns out to be.
#[tokio::test]
async fn paging_and_ordering_agree() {
    let Some(lit) = lit().await else { return };
    let http = client();
    let admin = go_minted_token(&http).await;
    let team = lit.team().await;
    let channel = lit.discoverable_channel("jrpage").await;

    let mut users = Vec::new();
    for tag in ["jrpagea", "jrpageb", "jrpagec"] {
        let user = create_plain_user(&http, &admin, &team, tag).await;
        let (status, body) = lit
            .send(
                &lit.go.clone(),
                "POST",
                &format!("/api/v4/channels/{channel}/join_request"),
                &user.token,
                Some(&serde_json::json!({"message": tag})),
            )
            .await;
        assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
        users.push(user);
    }

    for query in [
        "",
        "?per_page=1",
        "?per_page=1&page=1",
        "?per_page=2",
        "?per_page=0",
        "?per_page=500",
        "?page=9",
        "?status=pending&per_page=2&page=1",
    ] {
        let path = format!("/api/v4/channels/{channel}/join_requests{query}");
        let (go, rs) = lit.both("GET", &path, &admin, None).await;
        assert_eq!(go.0, rs.0, "{path}: status");
        assert_eq!(
            go.1,
            rs.1,
            "{path}: body — go={} rs={}",
            String::from_utf8_lossy(&go.1),
            String::from_utf8_lossy(&rs.1)
        );
        assert_eq!(
            json(&go.1)["total_count"],
            3,
            "{path}: the total ignores the page"
        );
    }

    let (go, rs) = lit
        .both(
            "GET",
            &format!("/api/v4/channels/{channel}/join_requests/count"),
            &admin,
            None,
        )
        .await;
    assert_eq!(go, rs);
    assert_eq!(json(&go.1), serde_json::json!({"count": 3}));

    for user in users {
        delete_plain_user(&http, &admin, &user.id).await;
    }
    delete_channel(&http, &admin, &channel).await;
}

/// Both servers must also agree that `/api/v4/channels/{id}/join_requests` is **not** served for a
/// segment outside Go's `[A-Za-z0-9]+` path charset — that is the mux's 404, before any handler.
#[tokio::test]
async fn a_segment_outside_gos_charset_is_the_mux_404() {
    let Some(lit) = lit().await else { return };
    for path in [
        "/api/v4/channels/has-a-dash/join_request",
        "/api/v4/channels/has-a-dash/join_requests",
        "/api/v4/channels/aaaaaaaaaaaaaaaaaaaaaaaaaa/join_requests/has-a-dash",
        "/api/v4/users/has-a-dash/channel_join_requests",
    ] {
        let (go, rs) = lit.both("GET", path, &lit.token.clone(), None).await;
        assert_eq!(go.0, 404, "{path}: the mux refuses before the handler");
        assert_eq!(go.0, rs.0, "{path}: status");
        assert_eq!(
            json(&go.1)["detailed_error"],
            json(&rs.1)["detailed_error"],
            "{path}: the interpolated URL is Go's own"
        );
    }
}
