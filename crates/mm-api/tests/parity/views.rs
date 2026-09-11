//! Cross-server parity for the seven routes of `api4/view.go` — the integrated-boards surface.
//!
//! ```sh
//! scripts/go-boards.sh start && scripts/parity.sh --test parity views
//! ```
//!
//! # This suite has two halves because the feature has two states, and only one of them ships
//!
//! **Dark.** `FeatureFlags.IntegratedBoards` is `false` at the pinned SHA and unset on this
//! deployment, so gorilla/mux has never registered these paths and every request is the mux's own
//! `api.context.404.app_error`. That is what a client of the real stack sees today, and
//! [`the_dark_feature_is_a_mux_404_on_both_servers`] is the only test here that runs
//! unconditionally. It asserts the port **forwards** rather than answering: a locally-minted 404
//! would have to reproduce a `detailed_error` that interpolates the request URL, and there is no
//! reason to own that string.
//!
//! **Lit.** Everything else needs two extra processes — a second Go server with the flag on
//! (`scripts/go-boards.sh`, which explains why the main one cannot simply have it turned on) and
//! a second `mm-api` with the same flag, pointed at it. Both are started by [`lit`], which returns
//! `None` when the oracle is not up so a plain `scripts/parity.sh` run stays green.
//!
//! # Why the read comparisons are byte-for-byte and the writes are not
//!
//! Both servers share one database, so a list or a get of the *same rows* must produce identical
//! bytes — key order, `omitempty`, the trailing newline and all. That is the strongest assertion
//! available and it costs nothing.
//!
//! A create cannot be compared that way: posting the same body twice makes two rows with different
//! ids and different clocks. So the write tests create on each server into **its own channel** and
//! compare the two documents with the five volatile fields blanked — which still pins the status,
//! the key set, the props ordering, the defaults and the fields the handler overwrites.

use crate::common;

use common::{
    GO, RUST, SecondServer, assert_error_bodies_match_except_known_gaps, client, create_channel,
    go_minted_token, stack_enabled,
};

/// The seven route+method pairs, as a client would spell them.
fn every_route(channel: &str, view: &str) -> Vec<(&'static str, String)> {
    vec![
        ("GET", format!("/api/v4/channels/{channel}/views")),
        ("POST", format!("/api/v4/channels/{channel}/views")),
        ("GET", format!("/api/v4/channels/{channel}/views/{view}")),
        ("PATCH", format!("/api/v4/channels/{channel}/views/{view}")),
        ("DELETE", format!("/api/v4/channels/{channel}/views/{view}")),
        (
            "GET",
            format!("/api/v4/channels/{channel}/views/{view}/posts"),
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{view}/sort_order"),
        ),
    ]
}

fn method(raw: &str) -> reqwest::Method {
    raw.parse().expect("a known method")
}

/// A valid kanban `props`, which is the only shape `IsValid` accepts.
fn kanban_props() -> serde_json::Value {
    serde_json::json!({
        "group_by": {
            "field_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            "columns": [
                {"id": "bbbbbbbbbbbbbbbbbbbbbbbbbb", "name": "Todo", "option_ids": ["todo"]},
                {"id": "cccccccccccccccccccccccccc", "name": "Done", "option_ids": ["done"]}
            ]
        }
    })
}

fn a_view_body(title: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "kanban",
        "title": title,
        "description": "a board",
        "sort_order": 0,
        "props": kanban_props(),
    })
}

// ---------------------------------------------------------------------------------------------
// Dark — the state this deployment is actually in
// ---------------------------------------------------------------------------------------------

/// Every one of the seven pairs is a mux 404 on Go, and the port **forwards** so the bytes are
/// Go's own.
///
/// The served-by header is the content: a handler that answered a reproduced 404 locally would
/// pass a status comparison and fail here, which is the mistake worth catching. The one field
/// allowed to differ is `request_id`, and it differs because there were two requests.
#[tokio::test]
async fn the_dark_feature_is_a_mux_404_on_both_servers() {
    if !stack_enabled() {
        return;
    }
    let http = client();
    let token = go_minted_token(&http).await;
    let channel = common::a_channel_the_user_is_in(&http, &token).await;

    for (verb, path) in every_route(&channel, "dddddddddddddddddddddddddd") {
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
            "{verb} {path}: a dark route must be forwarded, not answered locally — \
             reproducing the mux 404 here would mean owning its interpolated detailed_error"
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
// Lit — a boards-on Go beside a boards-on mm-api
// ---------------------------------------------------------------------------------------------

/// The two extra processes, and nothing that belongs to a tokio runtime.
///
/// **No `reqwest::Client` here, deliberately.** Each `#[tokio::test]` builds its own runtime, and
/// a client parked in a `static` is bound to whichever runtime reached it first; when that test
/// finished, its reactor went away and every later test's request failed with a bare "error
/// sending request" while both servers were demonstrably healthy. That is the second false signal
/// this fixture produced — see [`LIT`] for the first — and it is why the shared state is three
/// strings and a process handle.
struct Boards {
    go: String,
    rust: String,
    /// Held so the process outlives the first test that started it.
    ///
    /// A `static` is never dropped, so this `mm-api` survives the test binary. That is tolerable
    /// because `scripts/parity.sh` pkills every `mm-api` built from this checkout *before* each
    /// run, so at most one is ever left behind and the next run reclaims the port.
    _child: SecondServer,
}

/// A boards-on Go server and a boards-on `mm-api` pointed at it, plus a token both accept.
struct Lit {
    go: String,
    rust: String,
    token: String,
    http: reqwest::Client,
}

/// `MMRS_GO_PORT + 30`, the port `scripts/go-boards.sh` binds — derived from [`GO`] so a worktree
/// on any stack finds its own.
fn boards_go_base() -> Option<String> {
    let port: u16 = GO.rsplit(':').next()?.parse().ok()?;
    Some(format!("http://localhost:{}", port + 30))
}

/// **One boards-on `mm-api` per test binary, not one per test.**
///
/// Not an optimisation — a correctness fix, and the first run of this suite found it. Every test
/// called `lit()` and every call spawned a `SecondServer` on the same port; the first bound it and
/// the rest failed to bind, yet their readiness probe succeeded against the *first* process, so
/// each test got a handle wrapping a dead child. Ten tests passed against one server, and then the
/// first fixture to drop killed it out from under the others — which surfaced as a connection
/// refused in whichever test happened to still be running. Sharing the server removes both the
/// duplicate processes and the drop race.
static LIT: tokio::sync::OnceCell<Option<Boards>> = tokio::sync::OnceCell::const_new();

/// The fixture, or `None` when the oracle is not running.
///
/// Skipping rather than failing is the same contract [`SecondServer::start`] already has: a run
/// without the extra process is a run that cannot measure this, not a regression.
///
/// The *processes* are shared and the *client* is not — see [`Boards`] for why each half is the
/// way it is.
async fn lit() -> Option<Lit> {
    let boards = LIT.get_or_init(start_boards).await.as_ref()?;
    let http = client();
    let token = go_minted_token(&http).await;
    Some(Lit {
        go: boards.go.clone(),
        rust: boards.rust.clone(),
        token,
        http,
    })
}

async fn start_boards() -> Option<Boards> {
    if !stack_enabled() {
        return None;
    }
    let http = client();
    let go = boards_go_base()?;
    // One cheap probe before paying for a second `mm-api`.
    if !http
        .get(format!("{go}/api/v4/system/ping"))
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
    {
        eprintln!("views: skipping — no boards oracle at {go}; run `scripts/go-boards.sh start`");
        return None;
    }

    let rust_server = SecondServer::start(
        8082,
        &[
            ("MM_FEATUREFLAGS_INTEGRATEDBOARDS", "true"),
            // The *boards* Go, not the stack's: a forward from this server must reach a process
            // that knows these routes, or an unreproducible post list would become a 404.
            ("MM_GO_UPSTREAM", &go),
        ],
    )
    .await?;

    // Sessions are one table shared by every process here, so the stack's token authenticates
    // against the boards server unchanged — which is the whole reason this comparison is possible.
    let rust = rust_server.base.clone();

    Some(Boards {
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
        body: Option<&serde_json::Value>,
    ) -> (u16, Vec<u8>) {
        let mut request = self
            .http
            .request(method(verb), format!("{base}{path}"))
            .header("Authorization", format!("Bearer {}", self.token));
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
        let status = response.status().as_u16();
        if base == self.rust {
            common::assert_served_by_rust(response.headers(), path);
        }
        (status, response.bytes().await.expect("a body").to_vec())
    }

    /// Run the same request against both and return `((go_status, go_body), (rs_status, rs_body))`.
    async fn both(
        &self,
        verb: &str,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> ((u16, Vec<u8>), (u16, Vec<u8>)) {
        let go = self.send(&self.go.clone(), verb, path, body).await;
        let rs = self.send(&self.rust.clone(), verb, path, body).await;
        (go, rs)
    }

    /// Create a channel and seed it with `titles`, through the **Go** server, and return its id.
    async fn seeded_channel(&self, tag: &str, titles: &[&str]) -> String {
        let channel = create_channel(&self.http, &self.token, &self.team().await, tag).await;
        for title in titles {
            let (status, body) = self
                .send(
                    &self.go.clone(),
                    "POST",
                    &format!("/api/v4/channels/{channel}/views"),
                    Some(&a_view_body(title)),
                )
                .await;
            assert_eq!(
                status,
                201,
                "seeding {title} failed: {}",
                String::from_utf8_lossy(&body)
            );
        }
        channel
    }

    async fn team(&self) -> String {
        common::a_team_and_channel_the_user_is_in(&self.http, &self.token)
            .await
            .0
    }

    /// The ids of a channel's views, in list order.
    async fn view_ids(&self, channel: &str) -> Vec<String> {
        let (_, body) = self
            .send(
                &self.go.clone(),
                "GET",
                &format!("/api/v4/channels/{channel}/views"),
                None,
            )
            .await;
        let views: Vec<serde_json::Value> = serde_json::from_slice(&body).expect("a list");
        views
            .into_iter()
            .map(|v| v["id"].as_str().expect("an id").to_owned())
            .collect()
    }
}

/// Blank the fields a second row cannot share: identity, parentage and the two clocks.
///
/// Everything else — `type`, `title`, `description`, `sort_order`, `props` and `delete_at` — must
/// survive, which is what makes this a real comparison rather than a shape check.
fn without_volatile(mut value: serde_json::Value) -> serde_json::Value {
    for key in ["id", "channel_id", "creator_id", "create_at", "update_at"] {
        if let Some(object) = value.as_object_mut()
            && object.contains_key(key)
        {
            object.insert(key.to_owned(), serde_json::Value::Null);
        }
    }
    value
}

/// Listing the **same rows** must produce the same bytes on both servers.
///
/// This is where `props`' key ordering is pinned. `View.Props` is a `map[string]any`, so Go's
/// encoder sorts its keys — `columns` before `field_id`, though the request sent them the other
/// way round — and `serde_json::Map` is a `BTreeMap` for us. A build with serde_json's
/// `preserve_order` feature would emit insertion order and fail here, which is the only place
/// that could be caught.
#[tokio::test]
async fn the_list_is_byte_identical_including_the_props_key_order() {
    let Some(lit) = lit().await else { return };
    let channel = lit
        .seeded_channel("viewslist", &["Alpha", "Beta", "Gamma"])
        .await;
    let path = format!("/api/v4/channels/{channel}/views");

    let ((go_status, go_body), (rs_status, rs_body)) = lit.both("GET", &path, None).await;
    assert_eq!(go_status, 200);
    assert_eq!(rs_status, 200);
    assert_eq!(
        String::from_utf8_lossy(&go_body),
        String::from_utf8_lossy(&rs_body),
        "the list of the same three rows must be byte-identical, trailing newline included"
    );

    // The request sent `field_id` first; both servers must answer with `columns` first.
    let text = String::from_utf8_lossy(&go_body);
    let columns_at = text.find("\"columns\"").expect("columns appears");
    let field_id_at = text.find("\"field_id\"").expect("field_id appears");
    assert!(
        columns_at < field_id_at,
        "Go sorts map keys, so `columns` precedes `field_id` however the client sent them"
    );
    assert!(
        go_body.ends_with(b"\n"),
        "json.NewEncoder leaves a trailing newline"
    );
}

/// `include_total_count` changes the body from an array to an object, and the count is the
/// channel's whole live population rather than the page's length.
#[tokio::test]
async fn include_total_count_and_pagination_agree() {
    let Some(lit) = lit().await else { return };
    let channel = lit
        .seeded_channel("viewspage", &["One", "Two", "Three", "Four"])
        .await;

    for query in [
        "?include_total_count=true",
        "?per_page=2&include_total_count=true",
        "?per_page=2&page=1",
        "?per_page=1&page=3",
        "?per_page=0",
        "?per_page=99999&include_total_count=true",
        "?page=-1&per_page=-1",
        "?include_total_count=yes",
        "?include_total_count=1",
    ] {
        let path = format!("/api/v4/channels/{channel}/views{query}");
        let ((go_status, go_body), (rs_status, rs_body)) = lit.both("GET", &path, None).await;
        assert_eq!(go_status, rs_status, "{query}: status");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{query}: body"
        );
    }

    // The count is not the page's length: two of four, but `total_count` is four.
    let (_, body) = lit
        .send(
            &lit.rust.clone(),
            "GET",
            &format!("/api/v4/channels/{channel}/views?per_page=2&include_total_count=true"),
            None,
        )
        .await;
    let value: serde_json::Value = serde_json::from_slice(&body).expect("an object");
    assert_eq!(value["views"].as_array().expect("an array").len(), 2);
    assert_eq!(value["total_count"], 4);
}

/// An empty channel lists as `[]` and not `null` — the app layer's nil-slice normalisation.
#[tokio::test]
async fn an_empty_channel_lists_as_an_array() {
    let Some(lit) = lit().await else { return };
    let channel = lit.seeded_channel("viewsempty", &[]).await;
    let path = format!("/api/v4/channels/{channel}/views");

    let ((_, go_body), (_, rs_body)) = lit.both("GET", &path, None).await;
    assert_eq!(go_body, b"[]\n", "Go normalises the store's nil slice");
    assert_eq!(rs_body, go_body);

    let path = format!("/api/v4/channels/{channel}/views?include_total_count=true");
    let ((_, go_body), (_, rs_body)) = lit.both("GET", &path, None).await;
    assert_eq!(go_body, b"{\"views\":[],\"total_count\":0}\n");
    assert_eq!(rs_body, go_body);
}

/// A single view reads identically, and a soft-deleted one is a 404 rather than a row with
/// `delete_at` set.
#[tokio::test]
async fn get_and_delete_agree_including_the_second_delete() {
    let Some(lit) = lit().await else { return };
    let channel = lit.seeded_channel("viewsget", &["Solo", "Duo"]).await;
    let ids = lit.view_ids(&channel).await;
    let (kept, doomed) = (&ids[0], &ids[1]);

    let path = format!("/api/v4/channels/{channel}/views/{kept}");
    let ((go_status, go_body), (rs_status, rs_body)) = lit.both("GET", &path, None).await;
    assert_eq!((go_status, rs_status), (200, 200));
    assert_eq!(go_body, rs_body, "the same row must serialise identically");

    // `ReturnStatusOK` — **no** trailing newline, unlike every other success in this file.
    let path = format!("/api/v4/channels/{channel}/views/{doomed}");
    let (rs_status, rs_body) = lit.send(&lit.rust.clone(), "DELETE", &path, None).await;
    assert_eq!(rs_status, 200);
    assert_eq!(
        rs_body, br#"{"status":"OK"}"#,
        "web.ReturnStatusOK writes bytes, not an encoder"
    );

    // Deleting again, and reading it, are both 404 — and both servers agree on which id.
    let ((go_status, go_body), (rs_status, rs_body)) = lit.both("DELETE", &path, None).await;
    assert_eq!((go_status, rs_status), (404, 404));
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "the second delete");
    assert_eq!(go["id"], "app.view.get.not_found.app_error");

    let ((_, go_body), (_, rs_body)) = lit.both("GET", &path, None).await;
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a deleted view");

    // The survivor is untouched, so the list still matches.
    let path = format!("/api/v4/channels/{channel}/views");
    let ((_, go_body), (_, rs_body)) = lit.both("GET", &path, None).await;
    assert_eq!(go_body, rs_body, "a delete must not disturb its neighbours");

    // And the **count** must forget it too. `CountForChannel` has its own `DeleteAt = 0`
    // predicate, separate from the list query's, so dropping it is invisible until a channel
    // holds a deleted view — which is exactly this one.
    let path = format!("/api/v4/channels/{channel}/views?include_total_count=true");
    let ((_, go_body), (_, rs_body)) = lit.both("GET", &path, None).await;
    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("an object");
    assert_eq!(go["total_count"], 1, "the deleted view is not counted");
    assert_eq!(go_body, rs_body);
}

/// An **archived** channel is a 404 on all seven routes, and each carries its own error id.
///
/// Seven ids differing only in a middle word (`create`, `list`, `get`, `update`, `delete`,
/// `update_sort_order`, `get_posts`), all at the same status with the same detail — and the detail
/// is wiped before it reaches a client. So `id` is the entire observable difference and a
/// copy-paste between handlers is invisible to everything except a table like this one.
///
/// Note it is a **404**: this surface treats an archived channel as absent rather than refusing
/// the write with the 400 most of api4 gives.
#[tokio::test]
async fn an_archived_channel_is_a_404_with_a_different_id_per_route() {
    let Some(lit) = lit().await else { return };
    let channel = lit.seeded_channel("viewsarchived", &["Doomed"]).await;
    let view = lit.view_ids(&channel).await.remove(0);
    // Archived through the **boards** server, not `common::delete_channel`'s hardcoded one.
    // Each Mattermost process caches channels in memory and there is no cluster bus between these
    // two, so a delete issued elsewhere leaves this server answering 200 from a stale entry —
    // which is how this test first failed. `mm-api` reads the row, so it needs no such nudge.
    lit.send(
        &lit.go.clone(),
        "DELETE",
        &format!("/api/v4/channels/{channel}"),
        None,
    )
    .await;

    let expected = [
        "api.view.list.deleted_channel.app_error",
        "api.view.create.deleted_channel.app_error",
        "api.view.get.deleted_channel.app_error",
        "api.view.update.deleted_channel.app_error",
        "api.view.delete.deleted_channel.app_error",
        "api.view.get_posts.deleted_channel.app_error",
        "api.view.update_sort_order.deleted_channel.app_error",
    ];

    for ((verb, path), id) in every_route(&channel, &view).into_iter().zip(expected) {
        // A body that would otherwise decode, so the refusal is the channel and not the payload.
        let body = match (verb, path.ends_with("sort_order")) {
            ("POST", true) => Some(serde_json::json!(0)),
            ("POST", false) => Some(a_view_body("Ghost")),
            ("PATCH", _) => Some(serde_json::json!({"title": "Ghost"})),
            _ => None,
        };
        let ((go_status, go_body), (rs_status, rs_body)) =
            lit.both(verb, &path, body.as_ref()).await;
        let context = format!("{verb} {path}");
        assert_eq!(go_status, 404, "{context}: an archived channel is absent");
        assert_eq!(rs_status, go_status, "{context}: status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
        assert_eq!(go["id"], id, "{context}: each route has its own id");
    }
}

/// A create on each server, into its own channel, with the same body.
///
/// Pins the 201, the trailing newline, the fields the handler **overwrites** (`channel_id` and
/// `creator_id` come from the path and the session, never the body), and what `PreSave` keeps —
/// a client-supplied `create_at` survives while `delete_at` is reset.
#[tokio::test]
async fn a_create_produces_the_same_document_on_both_servers() {
    let Some(lit) = lit().await else { return };
    let go_channel = lit.seeded_channel("viewscreateg", &[]).await;
    let rs_channel = lit.seeded_channel("viewscreater", &[]).await;

    let mut body = a_view_body("Created");
    // Three fields the handler must ignore or reset, all set to something a correct port drops.
    body["channel_id"] = serde_json::json!("zzzzzzzzzzzzzzzzzzzzzzzzzz");
    body["creator_id"] = serde_json::json!("yyyyyyyyyyyyyyyyyyyyyyyyyy");
    body["delete_at"] = serde_json::json!(999);
    body["create_at"] = serde_json::json!(12345);
    body["sort_order"] = serde_json::json!(7);

    let (go_status, go_body) = lit
        .send(
            &lit.go.clone(),
            "POST",
            &format!("/api/v4/channels/{go_channel}/views"),
            Some(&body),
        )
        .await;
    let (rs_status, rs_body) = lit
        .send(
            &lit.rust.clone(),
            "POST",
            &format!("/api/v4/channels/{rs_channel}/views"),
            Some(&body),
        )
        .await;

    assert_eq!(go_status, 201, "createView is the file's only 201");
    assert_eq!(rs_status, 201);
    assert!(go_body.ends_with(b"\n") && rs_body.ends_with(b"\n"));

    let go: serde_json::Value = serde_json::from_slice(&go_body).expect("a view");
    let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("a view");

    assert_eq!(
        without_volatile(go.clone()),
        without_volatile(rs.clone()),
        "everything but identity and the clocks must match"
    );
    for (label, value, channel) in [("go", &go, &go_channel), ("rust", &rs, &rs_channel)] {
        assert_eq!(value["channel_id"], *channel, "{label}: the path wins");
        assert_eq!(
            value["creator_id"],
            common::logged_in_user_id(),
            "{label}: the session wins"
        );
        assert_eq!(value["delete_at"], 0, "{label}: PreSave resets delete_at");
        assert_eq!(
            value["create_at"], 12345,
            "{label}: PreSave keeps a supplied create_at"
        );
        assert_eq!(
            value["update_at"], 12345,
            "{label}: PreSave forces update_at to equal create_at"
        );
        assert_eq!(value["sort_order"], 7);
    }
}

/// A patch is applied in full and then validated, and `update_at` is the only thing an empty
/// patch moves.
#[tokio::test]
async fn a_patch_agrees_field_by_field() {
    let Some(lit) = lit().await else { return };
    let go_channel = lit.seeded_channel("viewspatchg", &["Before"]).await;
    let rs_channel = lit.seeded_channel("viewspatchr", &["Before"]).await;
    let go_id = lit.view_ids(&go_channel).await.remove(0);
    let rs_id = lit.view_ids(&rs_channel).await.remove(0);

    for patch in [
        serde_json::json!({"title": "After"}),
        serde_json::json!({}),
        serde_json::json!({"description": ""}),
        serde_json::json!({"sort_order": 3}),
        serde_json::json!({"title": "Final", "description": "d", "sort_order": 1}),
    ] {
        let (go_status, go_body) = lit
            .send(
                &lit.go.clone(),
                "PATCH",
                &format!("/api/v4/channels/{go_channel}/views/{go_id}"),
                Some(&patch),
            )
            .await;
        let (rs_status, rs_body) = lit
            .send(
                &lit.rust.clone(),
                "PATCH",
                &format!("/api/v4/channels/{rs_channel}/views/{rs_id}"),
                Some(&patch),
            )
            .await;

        assert_eq!(go_status, rs_status, "{patch}: status");
        let go: serde_json::Value = serde_json::from_slice(&go_body).expect("a view");
        let rs: serde_json::Value = serde_json::from_slice(&rs_body).expect("a view");
        assert_eq!(
            without_volatile(go),
            without_volatile(rs),
            "{patch}: the patched document"
        );
    }
}

/// `POST .../sort_order` renumbers the **whole channel** and answers the list, not the view.
///
/// Both channels are seeded with the same three titles in the same order, so the two answers are
/// comparable once identity and clocks are blanked. The content is the permutation.
#[tokio::test]
async fn a_sort_rewrites_the_whole_channel_the_same_way() {
    let Some(lit) = lit().await else { return };
    let go_channel = lit
        .seeded_channel("viewssortg", &["A", "B", "C", "D"])
        .await;
    let rs_channel = lit
        .seeded_channel("viewssortr", &["A", "B", "C", "D"])
        .await;

    // Move the first view to index 2, then the last to index 0 — two moves in opposite
    // directions, because a port that inserted before removing gets one of them right.
    // `(0, 3)` is the boundary case: index 3 of a four-view channel is the last legal one, and
    // the store's guard is `newIndex > len(views)-1`. An off-by-one there refuses it.
    for (which, to) in [(0usize, 2i64), (3, 0), (1, 1), (0, 3)] {
        let go_id = lit.view_ids(&go_channel).await.remove(which);
        let rs_id = lit.view_ids(&rs_channel).await.remove(which);

        let (go_status, go_body) = lit
            .send(
                &lit.go.clone(),
                "POST",
                &format!("/api/v4/channels/{go_channel}/views/{go_id}/sort_order"),
                Some(&serde_json::json!(to)),
            )
            .await;
        let (rs_status, rs_body) = lit
            .send(
                &lit.rust.clone(),
                "POST",
                &format!("/api/v4/channels/{rs_channel}/views/{rs_id}/sort_order"),
                Some(&serde_json::json!(to)),
            )
            .await;

        assert_eq!((go_status, rs_status), (200, 200), "moving {which} to {to}");
        let go: Vec<serde_json::Value> = serde_json::from_slice(&go_body).expect("a list");
        let rs: Vec<serde_json::Value> = serde_json::from_slice(&rs_body).expect("a list");

        let titles = |list: &[serde_json::Value]| {
            list.iter()
                .map(|v| v["title"].as_str().expect("a title").to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            titles(&go),
            titles(&rs),
            "moving {which} to {to}: the resulting order"
        );
        let orders = |list: &[serde_json::Value]| {
            list.iter()
                .map(|v| v["sort_order"].as_i64().expect("an order"))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            orders(&go),
            vec![0, 1, 2, 3],
            "every row is renumbered from zero"
        );
        assert_eq!(orders(&rs), orders(&go));

        // One shared `update_at` across the whole rewrite — a per-row clock would pass the
        // ordering assertions above and be wrong.
        let shared = |list: &[serde_json::Value]| {
            let first = list[0]["update_at"].as_i64().expect("a clock");
            list.iter().all(|v| v["update_at"] == first)
        };
        assert!(
            shared(&go) && shared(&rs),
            "one UpdateAt for the whole batch"
        );
    }
}

/// `getPostsForView` answers the channel's posts, and the view it validated changes nothing.
#[tokio::test]
async fn the_posts_route_matches_and_ignores_the_view() {
    let Some(lit) = lit().await else { return };
    let channel = lit.seeded_channel("viewsposts", &["Board"]).await;
    let id = lit.view_ids(&channel).await.remove(0);
    common::post_message(&lit.http, &lit.token, &channel, "mmrs views post", None).await;

    for query in ["", "?per_page=2", "?page=1&per_page=1"] {
        let path = format!("/api/v4/channels/{channel}/views/{id}/posts{query}");
        let ((go_status, go_body), (rs_status, rs_body)) = lit.both("GET", &path, None).await;
        assert_eq!((go_status, rs_status), (200, 200), "{query}");
        assert_eq!(
            String::from_utf8_lossy(&go_body),
            String::from_utf8_lossy(&rs_body),
            "{query}: the post list"
        );
    }
}

/// Every refusal in the file, compared by status **and** by error id.
///
/// The ids are the whole contract — a client branches on them and the translated `message` is
/// what [D-092] says it is — so a table like this is worth more than any single happy path. Note
/// how close some of the pairs are: a negative sort order is the *handler's* 400 and an
/// out-of-range one is the *store's*, at the same status with different ids.
#[tokio::test]
async fn every_refusal_carries_the_same_id_on_both_servers() {
    let Some(lit) = lit().await else { return };
    let channel = lit.seeded_channel("viewsrefuse", &["Only"]).await;
    let other = lit.seeded_channel("viewsother", &["Elsewhere"]).await;
    let id = lit.view_ids(&channel).await.remove(0);
    let elsewhere = lit.view_ids(&other).await.remove(0);
    let missing = "dddddddddddddddddddddddddd";

    let cases: Vec<(&str, String, Option<serde_json::Value>, u16, &str)> = vec![
        (
            "POST",
            format!("/api/v4/channels/{channel}/views"),
            Some(serde_json::json!({"type": "kanban", "title": "x"})),
            400,
            "model.view.is_valid.props.kanban_required.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views"),
            Some(serde_json::json!({"type": "table", "title": "x", "props": kanban_props()})),
            400,
            "model.view.is_valid.type.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views"),
            Some(serde_json::json!({"type": "kanban", "title": "   ", "props": kanban_props()})),
            400,
            "model.view.is_valid.title.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views"),
            Some(serde_json::Value::Null),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "GET",
            format!("/api/v4/channels/{channel}/views/short"),
            None,
            400,
            "api.context.invalid_url_param.app_error",
        ),
        (
            "GET",
            format!("/api/v4/channels/{channel}/views/{missing}"),
            None,
            404,
            "app.view.get.not_found.app_error",
        ),
        (
            "GET",
            format!("/api/v4/channels/{channel}/views/{elsewhere}"),
            None,
            404,
            "api.view.get.channel_mismatch.app_error",
        ),
        (
            "GET",
            format!("/api/v4/channels/{channel}/views/{elsewhere}/posts"),
            None,
            404,
            "api.view.get_posts.channel_mismatch.app_error",
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{channel}/views/{elsewhere}"),
            Some(serde_json::json!({"title": "x"})),
            404,
            "api.view.update.channel_mismatch.app_error",
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{channel}/views/{id}"),
            Some(serde_json::Value::Null),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{channel}/views/{id}"),
            Some(serde_json::json!({"title": ""})),
            400,
            "model.view.is_valid.title.app_error",
        ),
        (
            "PATCH",
            format!("/api/v4/channels/{channel}/views/{missing}"),
            Some(serde_json::json!({"title": "x"})),
            404,
            "app.view.get.not_found.app_error",
        ),
        (
            "DELETE",
            format!("/api/v4/channels/{channel}/views/{elsewhere}"),
            None,
            404,
            "api.view.delete.channel_mismatch.app_error",
        ),
        // The handler's own bound.
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{id}/sort_order"),
            Some(serde_json::json!(-1)),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        // The *store's*, at the same status with a different id.
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{id}/sort_order"),
            Some(serde_json::json!(99)),
            400,
            "app.view.update_sort_order.invalid_input.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{id}/sort_order"),
            Some(serde_json::json!({"sort_order": 0})),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{id}/sort_order"),
            Some(serde_json::json!(1.5)),
            400,
            "api.context.invalid_body_param.app_error",
        ),
        // No `GetView` on this route, so another channel's view is a not-found rather than a
        // mismatch — the one place the two vocabularies diverge.
        (
            "POST",
            format!("/api/v4/channels/{channel}/views/{elsewhere}/sort_order"),
            Some(serde_json::json!(0)),
            404,
            "app.view.update_sort_order.not_found.app_error",
        ),
    ];

    for (verb, path, body, status, id) in cases {
        let ((go_status, go_body), (rs_status, rs_body)) =
            lit.both(verb, &path, body.as_ref()).await;
        let context = format!("{verb} {path}");
        assert_eq!(go_status, status, "{context}: Go's status");
        assert_eq!(rs_status, go_status, "{context}: our status");
        let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, &context);
        assert_eq!(go["id"], id, "{context}: the error id");
        assert_eq!(
            go["status_code"], status,
            "{context}: the body's copy of the status"
        );
    }
}

/// A non-member of a public channel may **read** its views and may not **write** one.
///
/// The two gates are genuinely different functions —
/// `SessionHasPermissionToReadChannel` has an open-channel fallback and
/// `SessionHasPermissionToChannel` does not — so a port that used one for both passes every test
/// above and fails exactly here.
#[tokio::test]
async fn a_non_member_can_list_but_cannot_create() {
    let Some(lit) = lit().await else { return };
    let team = lit.team().await;
    let channel = lit.seeded_channel("viewsperm", &["Public"]).await;
    let plain = common::create_plain_user(&lit.http, &lit.token, &team, "viewsperm").await;

    let read = format!("/api/v4/channels/{channel}/views");
    for base in [&lit.go, &lit.rust] {
        let response = lit
            .http
            .get(format!("{base}{read}"))
            .header("Authorization", format!("Bearer {}", plain.token))
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status().as_u16(),
            200,
            "{base}: a team member reads a public channel's views without joining it"
        );

        let response = lit
            .http
            .post(format!("{base}{read}"))
            .header("Authorization", format!("Bearer {}", plain.token))
            .json(&a_view_body("Trespass"))
            .send()
            .await
            .expect("a response");
        assert_eq!(
            response.status().as_u16(),
            403,
            "{base}: creating needs create_post, which needs a membership"
        );
        let body: serde_json::Value = response.json().await.expect("an error");
        assert_eq!(body["id"], "api.context.permissions.app_error");
    }

    common::delete_plain_user(&lit.http, &lit.token, &plain.id).await;
}
