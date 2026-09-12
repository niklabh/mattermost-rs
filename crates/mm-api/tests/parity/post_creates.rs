//! Cross-server parity for `POST /api/v4/posts` — `createPost` — and `POST /api/v4/posts/ephemeral`.
//!
//! ```sh
//! docker compose up -d
//! scripts/parity.sh --test parity post_creates
//! ```
//!
//! # A create route cannot be compared by asking both servers the same question
//!
//! Every other suite in this binary sends one request to each server and diffs the bytes. That
//! does not work here: the two servers create *different* posts, with different ids and different
//! millisecond timestamps, and both of them stay in the database afterwards. So this suite does
//! three separate things instead, and each answers a different question:
//!
//! - **[`assert_created_posts_agree`]** blanks the four per-request fields and diffs everything
//!   else, which is what catches a wrong `props`, a wrong `hashtags`, a missing `metadata` or a
//!   key one server writes and the other omits.
//! - **[`posts_in_channel_named`]** reads the channel back afterwards, which is what catches a
//!   row that was never written, written twice, or written with the wrong channel counters.
//! - The forward tests send to **Rust only** and then count the rows. One row means Go wrote it
//!   after the proxy handed the request over; two would mean we wrote one *and* forwarded.
//!
//! # The row count is the whole point of the forward tests
//!
//! "Forward before any write" is not observable from a response — a forwarded 201 and a served
//! 201 look the same to a client, and `x-mmrs-served-by` only says which process answered. The
//! failure it guards against is a partial write followed by a forward, and the only thing that
//! sees it is the number of rows in the channel afterwards. Every forward condition in
//! `mm_app::post_create::App::refuse_create_post_shapes` has a row-counting test here for that
//! reason.
//!
//! # Fixture rows all begin `mmrscreatepost`
//!
//! Not the shared `mmrs-parity-` prefix: `common::purge_api_fixtures` runs once per test *binary*
//! and binaries run concurrently, so sharing the prefix lets another suite's start-up delete this
//! suite's channel mid-run. [`purge_create_post_fixtures`] clears these instead.

use crate::common;

use common::{
    GO, RUST, add_user_to_channel, assert_error_bodies_match_except_known_gaps, client,
    create_plain_user, delete_plain_user, go_minted_token, logged_in_user_id, stack_enabled,
};

const PREFIX: &str = "mmrscreatepost";

// ---------------------------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------------------------

async fn fixture_pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(&url)
        .await
        .ok()
}

/// Guards [`purge_create_post_fixtures`] so it runs **once per test binary**.
///
/// Without it every test in this module deletes every other test's team while they run: the tests
/// are concurrent, each one opens with a purge, and the purge is keyed on the shared
/// [`PREFIX`] rather than on the caller's tag. Seven of seventeen failed that way on the first
/// full-workspace run and passed in isolation — the shape the project's own note calls a
/// shared-fixture race, inflicted here by this suite on itself.
static PURGED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

/// Remove every row this suite authors, at the **start** of the run: an assertion panics past
/// trailing cleanup, so the next run's purge is the only one certain to happen.
async fn purge_create_post_fixtures() {
    PURGED.get_or_init(purge_create_post_fixtures_once).await;
}

async fn purge_create_post_fixtures_once() {
    let Some(pool) = fixture_pool().await else {
        return;
    };
    let like = format!("{PREFIX}%");
    for statement in [
        "DELETE FROM posts WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM channelmembers WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM sidebarchannels WHERE channelid IN (SELECT id FROM channels WHERE name LIKE $1)",
        "DELETE FROM channels WHERE name LIKE $1",
        "DELETE FROM teammembers WHERE teamid IN (SELECT id FROM teams WHERE name LIKE $1)",
        "DELETE FROM teams WHERE name LIKE $1",
    ] {
        let _ = sqlx::query(statement).bind(&like).execute(&pool).await;
    }
}

/// A team and an **open** channel of this suite's own, created through Go.
///
/// Its own, not a shared one, for the reason the module docs give and for one more: the channel's
/// `TotalMsgCount` is asserted on, and a channel any other suite posts into moves under the test.
async fn own_channel(client: &reqwest::Client, token: &str, tag: &str) -> (String, String) {
    purge_create_post_fixtures().await;

    let name = format!("{PREFIX}team{tag}");
    let team: serde_json::Value = client
        .post(format!("{GO}/api/v4/teams"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "name": name,
            "display_name": format!("mmrs create post {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the team decodes");
    let team_id = team["id"].as_str().expect("a team id").to_owned();

    let channel_name = format!("{PREFIX}chan{tag}");
    let channel: serde_json::Value = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team_id,
            "name": channel_name,
            "display_name": format!("mmrs create post {tag}"),
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the channel decodes");
    let channel_id = channel["id"].as_str().expect("a channel id").to_owned();

    (team_id, channel_id)
}

/// POST a body to one server, returning `(status, served_by_rust, body)`.
async fn create_on(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, bool, serde_json::Value) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served_by_rust = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    let bytes = response.bytes().await.expect("body reads").to_vec();
    let decoded = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, served_by_rust, decoded)
}

/// The four fields that cannot match between two separate creates, blanked so the rest can be
/// compared by value.
///
/// **`update_at` is in the list and `edit_at` is not**, deliberately: `PreSave` copies `CreateAt`
/// into `UpdateAt`, so the two move together and blanking `update_at` loses nothing the
/// `create_at` assertion below does not already make. `edit_at` is a real value (`0`) and stays.
fn strip_per_request_fields(post: &mut serde_json::Value) {
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at"] {
        obj.insert(key.to_owned(), serde_json::json!(0));
    }
}

/// Compare two created posts field by field, having blanked what cannot match.
///
/// Also asserts each side's `create_at` is a plausible current timestamp rather than zero, since
/// blanking it would otherwise hide a port that forgot to set it at all.
fn assert_created_posts_agree(go: &serde_json::Value, rust: &serde_json::Value, context: &str) {
    for (label, post) in [("go", go), ("rust", rust)] {
        let create_at = post["create_at"].as_i64().expect("a create_at");
        assert!(
            create_at > 1_600_000_000_000,
            "{context}: {label}'s create_at is {create_at}, which is not a current timestamp"
        );
        assert_eq!(
            post["update_at"].as_i64(),
            Some(create_at),
            "{context}: {label}'s PreSave must copy create_at into update_at"
        );
    }

    let mut go = go.clone();
    let mut rust = rust.clone();
    strip_per_request_fields(&mut go);
    strip_per_request_fields(&mut rust);
    assert_eq!(go, rust, "{context}");
}

/// The messages of every **user** post in a channel, read back through Go.
///
/// System posts are dropped: creating a channel writes a `system_join_channel` row before this
/// suite has posted anything at all, so a bare "the channel is empty" assertion is never true and
/// the two tests that need one were written against the wrong baseline until this filtered.
async fn posts_in_channel_named(
    client: &reqwest::Client,
    token: &str,
    channel_id: &str,
) -> Vec<String> {
    let list: serde_json::Value = client
        .get(format!(
            "{GO}/api/v4/channels/{channel_id}/posts?per_page=200"
        ))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the list decodes");
    let mut messages: Vec<String> = list["posts"]
        .as_object()
        .map(|posts| {
            posts
                .values()
                .filter(|post| {
                    !post["type"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("system_")
                })
                .filter_map(|post| post["message"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    messages.sort();
    messages
}

/// The channel's `TotalMsgCount` and `LastPostAt`, read straight from Postgres.
///
/// Through the API rather than the store because the counters are what
/// `SqlPostStore.Save`'s second statement writes, and a port that dropped that statement would
/// still answer the create correctly.
async fn channel_counters(channel_id: &str) -> Option<(i64, i64)> {
    let pool = fixture_pool().await?;
    sqlx::query_as::<_, (i64, i64)>("SELECT totalmsgcount, lastpostat FROM channels WHERE id = $1")
        .bind(channel_id)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten()
}

// ---------------------------------------------------------------------------------------------
// the happy path
// ---------------------------------------------------------------------------------------------

/// The shape the product is built around: a plain message in an open channel the caller is in.
///
/// Both servers create a post, and everything but the id and the two timestamps has to agree —
/// including `props` being `{}` rather than `null`, `file_ids` being `[]` rather than `null`,
/// `metadata` being `{}` rather than absent, and `participants` being `null` rather than `[]`.
/// Four `omitempty`-adjacent decisions that a port gets wrong independently of each other.
#[tokio::test]
async fn a_plain_post_matches_go_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "plain").await;

    let body = |message: &str| serde_json::json!({ "channel_id": channel, "message": message });

    let (go_status, _, go_post) =
        create_on(&client, GO, &token, "/api/v4/posts", &body("mmrs plain go")).await;
    let (rs_status, served_by_rust, rs_post) = create_on(
        &client,
        RUST,
        &token,
        "/api/v4/posts",
        &body("mmrs plain go"),
    )
    .await;

    assert_eq!(go_status, 201, "Go answers 201 Created, not 200");
    assert_eq!(rs_status, 201, "and so must we");
    assert!(
        served_by_rust,
        "a plain post must be served here, not forwarded"
    );
    assert_created_posts_agree(&go_post, &rs_post, "a plain post");

    // Both rows are really there — a response is not a write.
    let messages = posts_in_channel_named(&client, &token, &channel).await;
    assert_eq!(
        messages.iter().filter(|m| *m == "mmrs plain go").count(),
        2,
        "both servers must have written exactly one row each: {messages:?}"
    );
}

/// `SqlPostStore.Save`'s second statement — the one whose failure Go logs and swallows.
///
/// A port that wrote the `Posts` row and skipped this would pass every response comparison in
/// this file and leave every client's unread count wrong.
#[tokio::test]
async fn the_channel_counters_move_by_one_per_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "counters").await;

    let Some((before_count, before_last)) = channel_counters(&channel).await else {
        return;
    };

    let (status, served_by_rust, post) = create_on(
        &client,
        RUST,
        &token,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs counters" }),
    )
    .await;
    assert_eq!(status, 201);
    assert!(served_by_rust);

    let Some((after_count, after_last)) = channel_counters(&channel).await else {
        return;
    };
    assert_eq!(
        after_count,
        before_count + 1,
        "TotalMsgCount must rise by exactly one"
    );
    assert_eq!(
        after_last,
        post["create_at"].as_i64().expect("a create_at"),
        "LastPostAt must become the new post's CreateAt"
    );
    assert!(after_last > before_last, "and it must have moved forward");
}

/// `?set_online` swallows its parse error; `?silent` does not.
///
/// Two `strconv.ParseBool` calls a few lines apart with opposite error handling. A port that
/// shared one helper between them answers 400 for both or 201 for both, and this is the pair that
/// tells them apart.
#[tokio::test]
async fn set_online_tolerates_a_bogus_value_and_silent_refuses_one() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "flags").await;

    for base in [GO, RUST] {
        let (status, _, _) = create_on(
            &client,
            base,
            &token,
            "/api/v4/posts?set_online=bogus",
            &serde_json::json!({ "channel_id": channel, "message": "mmrs set_online" }),
        )
        .await;
        assert_eq!(
            status, 201,
            "{base}: an unparseable set_online is not an error"
        );
    }

    let (go_status, go_body) = raw_create(
        &client,
        GO,
        &token,
        "/api/v4/posts?silent=bogus",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs silent" }),
    )
    .await;
    let (rs_status, rs_body) = raw_create(
        &client,
        RUST,
        &token,
        "/api/v4/posts?silent=bogus",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs silent" }),
    )
    .await;
    assert_eq!(go_status, 400, "an unparseable silent is a 400");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "?silent=bogus");

    // And neither 400 wrote a row.
    let messages = posts_in_channel_named(&client, &token, &channel).await;
    assert_eq!(
        messages.iter().filter(|m| *m == "mmrs silent").count(),
        0,
        "a 400 must not have created a post: {messages:?}"
    );
}

/// The deduplication cache, which is the one piece of `CreatePost` with no database behind it.
///
/// Two identical requests carrying the same `pending_post_id` answer with the **same post id**
/// and leave **one** row. A port with no cache answers with two ids and leaves two rows, which is
/// exactly the duplicate-message bug the cache exists to prevent.
#[tokio::test]
async fn a_repeated_pending_post_id_is_answered_with_the_first_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "dedup").await;

    // A pending id unique to this run. Go's cache has a 30-second TTL and **outlives the test**:
    // a constant id makes a re-run within that window find a cached entry pointing at a post the
    // purge has since deleted, which Go answers with a 500
    // (`api.post.deduplicate_create_post.failed_to_get`) rather than a create. Measured, not
    // predicted — this test failed that way on its second run.
    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    for base in [GO, RUST] {
        let pending = format!(
            "{}:mmrsdedup{run}{}",
            logged_in_user_id(),
            if base == GO { "go" } else { "rs" }
        );
        let body = serde_json::json!({
            "channel_id": channel,
            "message": "mmrs dedup",
            "pending_post_id": pending,
        });

        let (first_status, _, first) =
            create_on(&client, base, &token, "/api/v4/posts", &body).await;
        let (second_status, _, second) =
            create_on(&client, base, &token, "/api/v4/posts", &body).await;

        assert_eq!(first_status, 201, "{base}: the first create");
        assert_eq!(
            second_status, 201,
            "{base}: the second is idempotent, not a conflict"
        );
        assert_eq!(
            first["id"], second["id"],
            "{base}: the same pending_post_id must answer with the same post"
        );
    }

    let messages = posts_in_channel_named(&client, &token, &channel).await;
    assert_eq!(
        messages.iter().filter(|m| *m == "mmrs dedup").count(),
        2,
        "one row per server, not two: {messages:?}"
    );
}

/// A *different* pending post id is a different post — the control for the test above.
///
/// Without it, a cache that deduplicated everything, or one keyed on the channel rather than on
/// the pending id, would pass.
#[tokio::test]
async fn a_different_pending_post_id_is_a_different_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "dedup2").await;

    let run = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let make = |suffix: &str| {
        serde_json::json!({
            "channel_id": channel,
            "message": "mmrs dedup control",
            "pending_post_id": format!("{}:mmrsctl{run}{suffix}", logged_in_user_id()),
        })
    };
    let (_, served_a, first) = create_on(&client, RUST, &token, "/api/v4/posts", &make("a")).await;
    let (_, served_b, second) = create_on(&client, RUST, &token, "/api/v4/posts", &make("b")).await;
    assert!(served_a && served_b, "both must be served here");
    assert_ne!(
        first["id"], second["id"],
        "two different pending ids are two different posts"
    );
}

// ---------------------------------------------------------------------------------------------
// the error branches
// ---------------------------------------------------------------------------------------------

async fn raw_create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    path: &str,
    body: &serde_json::Value,
) -> (u16, Vec<u8>) {
    let response = client
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base}{path} is unreachable: {e}"));
    (
        response.status().as_u16(),
        response.bytes().await.expect("body reads").to_vec(),
    )
}

/// Every refusal `createPost` can raise before it writes, asked of both servers.
///
/// Ordering is the wire format here: a body that fails two of these gets whichever answer comes
/// first, so a single test per branch would not catch a reordering. These are all single-fault
/// bodies; [`the_permission_check_answers_before_the_message_length_check`] is the ordering one.
#[tokio::test]
async fn every_refusal_before_the_write_matches_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "errors").await;

    let cases: Vec<(&str, serde_json::Value, u16)> = vec![
        // `CreatePostAsUser`: a `system_*` type is refused before the channel is even looked at
        // for its DeleteAt.
        (
            "a system post type",
            serde_json::json!({ "channel_id": channel, "message": "x", "type": "system_join_channel" }),
            400,
        ),
        // `userCreatePostPermissionCheckWithContext` fires first for a channel that is not there,
        // so this is a 403 and *not* the `post.channel_id` 400 a reader would predict.
        (
            "an unknown channel",
            serde_json::json!({ "channel_id": "zzzzzzzzzzzzzzzzzzzzzzzzzz", "message": "x" }),
            403,
        ),
        // `postPriorityCheck`: a priority on a reply, which is refused before the reply itself is
        // resolved.
        (
            "a priority on a reply",
            serde_json::json!({
                "channel_id": channel,
                "message": "x",
                "root_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
                "metadata": { "priority": { "priority": "urgent" } },
            }),
            400,
        ),
        // `postPriorityCheck`: `requested_ack` needs at least a Professional licence, and the
        // refusal is a **501**, not the 403 the priority setting's own gate raises.
        (
            "an acknowledgement request without a licence",
            serde_json::json!({
                "channel_id": channel,
                "message": "x",
                "metadata": { "priority": { "priority": "", "requested_ack": true } },
            }),
            501,
        ),
        // `CreatePost`'s parent/child verification: a root id that is not a post.
        (
            "a root that does not exist",
            serde_json::json!({
                "channel_id": channel,
                "message": "x",
                "root_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
            }),
            400,
        ),
    ];

    for (label, body, expected) in cases {
        let (go_status, go_body) = raw_create(&client, GO, &token, "/api/v4/posts", &body).await;
        let (rs_status, rs_body) = raw_create(&client, RUST, &token, "/api/v4/posts", &body).await;
        assert_eq!(go_status, expected, "{label}: Go's status moved");
        assert_eq!(rs_status, go_status, "{label}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
    }

    // None of them wrote anything.
    assert!(
        posts_in_channel_named(&client, &token, &channel)
            .await
            .is_empty(),
        "a refusal must not create a post"
    );
}

/// Replying to a **reply** is `api.post.create_post.root_id.app_error` at 400 — the same id an
/// unreadable root gets, so a client cannot tell the two apart.
///
/// This is the branch `mm_app::post_create::App::resolve_root_post` exists for. It sits at Go's
/// position — after the author lookup and after the mention gate — and it is the one check on the
/// reply path that answers here rather than being forwarded.
#[tokio::test]
async fn replying_to_a_reply_is_refused_here_and_not_forwarded() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "rootid").await;

    let (_, _, root) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs root" }),
    )
    .await;
    let (_, _, reply) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts",
        &serde_json::json!({
            "channel_id": channel,
            "message": "mmrs reply",
            "root_id": root["id"],
        }),
    )
    .await;

    let body = serde_json::json!({
        "channel_id": channel,
        "message": "mmrs reply to a reply",
        "root_id": reply["id"],
    });
    let (go_status, go_body) = raw_create(&client, GO, &token, "/api/v4/posts", &body).await;
    let (rs_status, rs_body) = raw_create(&client, RUST, &token, "/api/v4/posts", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, 400);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a reply to a reply");

    // It is refused, not forwarded and not written.
    let messages = posts_in_channel_named(&client, &token, &channel).await;
    assert_eq!(
        messages
            .iter()
            .filter(|m| *m == "mmrs reply to a reply")
            .count(),
        0,
        "neither server may create a reply to a reply: {messages:?}"
    );
}

/// A root in **another channel** is `api.post.create_post.channel_root_id.app_error`, and its
/// status is **400** even though `CreatePost` constructs it at 500.
///
/// `CreatePostAsUserWithFlags` tests `err.Id` against two ids and lowers the status for both. A
/// port that returned the constructed 500 would differ from Go on a body that is otherwise
/// identical, which is the kind of divergence only a cross-server test finds.
#[tokio::test]
async fn a_root_in_another_channel_is_a_400_not_the_500_it_is_built_as() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (team, channel) = own_channel(&client, &token, "crosschan").await;

    let other: serde_json::Value = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({
            "team_id": team,
            "name": format!("{PREFIX}chanother"),
            "display_name": "mmrs create post other",
            "type": "O",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the channel decodes");
    let other_id = other["id"].as_str().expect("an id").to_owned();

    let (_, _, root) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": other_id, "message": "mmrs elsewhere" }),
    )
    .await;

    let body = serde_json::json!({
        "channel_id": channel,
        "message": "mmrs cross-channel reply",
        "root_id": root["id"],
    });
    let (go_status, go_body) = raw_create(&client, GO, &token, "/api/v4/posts", &body).await;
    let (rs_status, rs_body) = raw_create(&client, RUST, &token, "/api/v4/posts", &body).await;
    assert_eq!(go_status, 400, "lowered from the 500 it is constructed as");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "a root in another channel");
}

/// `createPostChecks` runs entirely **before** `rejectOversizedMessage`.
///
/// So an oversized message in a channel the caller cannot post to is a 403, not a 400. Swapping
/// the two — which reads like a harmless optimisation, since the length test is free and the
/// permission test is a query — changes the status code for exactly this request.
#[tokio::test]
async fn the_permission_check_answers_before_the_message_length_check() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, _channel) = own_channel(&client, &admin, "order").await;

    // A **private** channel the plain user is not in: a public one falls back to the team's
    // `create_post_public`, and the refusal never fires.
    let private: serde_json::Value = client
        .post(format!("{GO}/api/v4/channels"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "team_id": team,
            "name": format!("{PREFIX}chanpriv"),
            "display_name": "mmrs create post private",
            "type": "P",
        }))
        .send()
        .await
        .expect("Go answers")
        .json()
        .await
        .expect("the channel decodes");
    let private_id = private["id"].as_str().expect("an id").to_owned();

    let user = create_plain_user(&client, &admin, &team, "createpostorder").await;
    let (user_id, user_token) = (user.id, user.token);

    let body = serde_json::json!({
        "channel_id": private_id,
        // Comfortably past the 16383-rune default `MaxPostSize`.
        "message": "x".repeat(70_000),
    });
    let (go_status, go_body) = raw_create(&client, GO, &user_token, "/api/v4/posts", &body).await;
    let (rs_status, rs_body) = raw_create(&client, RUST, &user_token, "/api/v4/posts", &body).await;
    assert_eq!(
        go_status, 403,
        "the permission refusal must come first, not the 400 for the length"
    );
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "permission before length");

    // And the same user, in a channel they *can* post to, gets the 400 — which is what makes the
    // assertion above about *order* rather than about the length check being absent.
    add_user_to_channel(&client, &admin, &private_id, &user_id).await;
    let (go_status, go_body) = raw_create(&client, GO, &user_token, "/api/v4/posts", &body).await;
    let (rs_status, rs_body) = raw_create(&client, RUST, &user_token, "/api/v4/posts", &body).await;
    assert_eq!(go_status, 400, "now the length check is reachable");
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "the length refusal");

    delete_plain_user(&client, &admin, &user_id).await;
}

/// An archived channel is `api.post.create_post.can_not_post_to_deleted.error` at 400.
#[tokio::test]
async fn an_archived_channel_refuses_the_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "archived").await;

    let response = client
        .delete(format!("{GO}/api/v4/channels/{channel}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "archiving the channel failed"
    );

    let body = serde_json::json!({ "channel_id": channel, "message": "mmrs archived" });
    let (go_status, go_body) = raw_create(&client, GO, &token, "/api/v4/posts", &body).await;
    let (rs_status, rs_body) = raw_create(&client, RUST, &token, "/api/v4/posts", &body).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an archived channel");
}

/// A body that is not a post is `api.context.invalid_body_param.app_error` with `Name: post`.
#[tokio::test]
async fn a_body_that_does_not_decode_is_a_400_naming_post() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    let send = async |base: &str| {
        let response = client
            .post(format!("{base}/api/v4/posts"))
            .header("Authorization", format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .body("{\"channel_id\": 7}")
            .send()
            .await
            .expect("the server answers");
        (
            response.status().as_u16(),
            response.bytes().await.expect("body reads").to_vec(),
        )
    };
    let (go_status, go_body) = send(GO).await;
    let (rs_status, rs_body) = send(RUST).await;
    assert_eq!(go_status, 400);
    assert_eq!(rs_status, go_status);
    let go = assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "an undecodable body");
    assert_eq!(go["id"], "api.context.invalid_body_param.app_error");
}

// ---------------------------------------------------------------------------------------------
// the forwards — and the proof that each happens before any write
// ---------------------------------------------------------------------------------------------

/// Every condition under which `createPost` hands the request to Go, sent to **Rust only**, with
/// the channel's row count checked afterwards.
///
/// One row means the proxy handed the whole request over and Go wrote it. **Two would mean we
/// wrote one and then forwarded**, which is the correctness bug this suite exists to rule out —
/// and it is invisible in the response, because a forwarded 201 and a served 201 look the same.
///
/// The `x-mmrs-served-by` assertion is the other half: without it a condition that stopped being
/// detected would still leave one row and pass.
#[tokio::test]
async fn every_forward_condition_forwards_and_leaves_exactly_one_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "forwards").await;

    // Each case is (label, message, extra fields). The message is the row key, so every one is
    // distinct.
    let cases: Vec<(&str, serde_json::Value)> = vec![
        (
            "an @-mention reaches the notification fan-out",
            serde_json::json!({ "message": "mmrs fwd at @nobody" }),
        ),
        (
            "a ~channel mention resolves into a prop",
            serde_json::json!({ "message": "mmrs fwd tilde ~town-square" }),
        ),
        (
            "a link needs the embed pipeline",
            serde_json::json!({ "message": "mmrs fwd link https://example.com" }),
        ),
        (
            "a markdown image needs getImagesForPost",
            serde_json::json!({ "message": "mmrs fwd image ![alt](x)" }),
        ),
        (
            "a non-default post type",
            serde_json::json!({ "message": "mmrs fwd type", "type": "me" }),
        ),
        (
            "a custom post type is a plugin's",
            serde_json::json!({ "message": "mmrs fwd custom", "type": "custom_thing" }),
        ),
        (
            "from_webhook is settable with hardened mode off",
            serde_json::json!({ "message": "mmrs fwd webhook", "props": { "from_webhook": "true" } }),
        ),
        (
            "override_username changes the sender name",
            serde_json::json!({ "message": "mmrs fwd override", "props": { "override_username": "nobody" } }),
        ),
        (
            "attachments change the embed and emoji scans",
            serde_json::json!({ "message": "mmrs fwd attach", "props": { "attachments": [] } }),
        ),
        (
            "an inbound metadata document",
            serde_json::json!({ "message": "mmrs fwd metadata", "metadata": { "embeds": [] } }),
        ),
    ];

    for (label, extra) in &cases {
        let mut body = serde_json::json!({ "channel_id": channel });
        let obj = body.as_object_mut().expect("an object");
        for (key, value) in extra.as_object().expect("an object") {
            obj.insert(key.clone(), value.clone());
        }

        let (status, served_by_rust, _) =
            create_on(&client, RUST, &token, "/api/v4/posts", &body).await;
        assert_eq!(status, 201, "{label}: Go must still have created it");
        assert!(
            !served_by_rust,
            "{label}: this shape must be forwarded, not answered here"
        );
    }

    let messages = posts_in_channel_named(&client, &token, &channel).await;
    for (label, extra) in &cases {
        let message = extra["message"].as_str().expect("a message");
        assert_eq!(
            messages.iter().filter(|m| m.as_str() == message).count(),
            1,
            "{label}: exactly one row — two means we wrote one and then forwarded. {messages:?}"
        );
    }
}

/// A reply that passes every root check is forwarded, and leaves one row.
///
/// Split from the table above because it needs a real root post, and because it is the one
/// forward that happens *after* several checks have already run and answered — so it is the
/// strongest test that a served refusal and a forwarded success can live in the same function
/// without the refusal path writing anything.
#[tokio::test]
async fn a_valid_reply_is_forwarded_and_leaves_one_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "replyfwd").await;

    let (_, _, root) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs reply root" }),
    )
    .await;

    let (status, served_by_rust, reply) = create_on(
        &client,
        RUST,
        &token,
        "/api/v4/posts",
        &serde_json::json!({
            "channel_id": channel,
            "message": "mmrs forwarded reply",
            "root_id": root["id"],
        }),
    )
    .await;
    assert_eq!(status, 201);
    assert!(!served_by_rust, "a reply must be forwarded");
    assert_eq!(reply["root_id"], root["id"]);

    let messages = posts_in_channel_named(&client, &token, &channel).await;
    assert_eq!(
        messages
            .iter()
            .filter(|m| *m == "mmrs forwarded reply")
            .count(),
        1,
        "exactly one reply row: {messages:?}"
    );
}

/// A channel member whose `mention_keys` is non-empty makes every message in that channel a
/// possible mention, so the whole channel is forwarded.
///
/// This is the third of `getExplicitMentions`' three ways to find a mention, and the only one
/// that cannot be read off the message. Without the query behind it, a post with no `@` in it
/// would be served here while Go raised somebody's mention count.
#[tokio::test]
async fn a_member_with_mention_keys_forwards_the_whole_channel() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, channel) = own_channel(&client, &admin, "keywords").await;

    // Before: plain messages in this channel are served here.
    let (_, served_before, _) = create_on(
        &client,
        RUST,
        &admin,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs before keys" }),
    )
    .await;
    assert!(
        served_before,
        "the control: without a keyword recipient this channel is served"
    );

    let user_id = create_plain_user(&client, &admin, &team, "createpostkeys")
        .await
        .id;
    add_user_to_channel(&client, &admin, &channel, &user_id).await;

    let response = client
        .put(format!("{GO}/api/v4/users/{user_id}/patch"))
        .header("Authorization", format!("Bearer {admin}"))
        .json(&serde_json::json!({
            "notify_props": { "mention_keys": "pineapple", "first_name": "false" },
        }))
        .send()
        .await
        .expect("Go answers");
    assert!(
        response.status().is_success(),
        "setting mention_keys failed"
    );

    let (status, served_after, _) = create_on(
        &client,
        RUST,
        &admin,
        "/api/v4/posts",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs after keys" }),
    )
    .await;
    assert_eq!(status, 201);
    assert!(
        !served_after,
        "a member with mention_keys must forward the channel"
    );

    let messages = posts_in_channel_named(&client, &admin, &channel).await;
    assert_eq!(
        messages.iter().filter(|m| *m == "mmrs after keys").count(),
        1,
        "the forward left exactly one row: {messages:?}"
    );

    delete_plain_user(&client, &admin, &user_id).await;
}

// ---------------------------------------------------------------------------------------------
// createEphemeralPost
// ---------------------------------------------------------------------------------------------

/// The ephemeral route writes nothing, so both servers can be asked the same question and their
/// answers compared the same way a read route's are.
#[tokio::test]
async fn an_ephemeral_post_matches_go_and_writes_no_row() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "eph").await;

    let body = serde_json::json!({
        "user_id": logged_in_user_id(),
        "post": { "channel_id": channel, "message": "mmrs ephemeral" },
    });
    let (go_status, _, go_post) =
        create_on(&client, GO, &token, "/api/v4/posts/ephemeral", &body).await;
    let (rs_status, served_by_rust, rs_post) =
        create_on(&client, RUST, &token, "/api/v4/posts/ephemeral", &body).await;

    assert_eq!(go_status, 201);
    assert_eq!(rs_status, 201);
    assert!(served_by_rust, "the ephemeral route must be served here");
    assert_eq!(go_post["type"], "system_ephemeral", "Type is overwritten");
    // `update_at` stays **0** here: `SendEphemeralPost` never calls `PreSave`, so the field the
    // create route copies from `CreateAt` is untouched. That is the difference between the two
    // routes' shapes and it is asserted rather than blanked.
    assert_eq!(go_post["update_at"], 0);
    assert_eq!(rs_post["update_at"], 0);

    let mut go = go_post.clone();
    let mut rs = rs_post.clone();
    for post in [&mut go, &mut rs] {
        let obj = post.as_object_mut().expect("an object");
        obj.insert("id".to_owned(), serde_json::json!(0));
        obj.insert("create_at".to_owned(), serde_json::json!(0));
    }
    assert_eq!(go, rs, "an ephemeral post");

    assert!(
        posts_in_channel_named(&client, &token, &channel)
            .await
            .is_empty(),
        "an ephemeral post is not written to the database by either server"
    );
}

/// The three 400s the ephemeral route raises before its permission check, and the permission
/// check itself.
#[tokio::test]
async fn the_ephemeral_refusals_match_go() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, channel) = own_channel(&client, &admin, "epherr").await;

    for (label, body) in [
        (
            "an empty user_id",
            serde_json::json!({ "user_id": "", "post": { "channel_id": channel, "message": "x" } }),
        ),
        (
            "a null post",
            serde_json::json!({ "user_id": logged_in_user_id(), "post": null }),
        ),
    ] {
        let (go_status, go_body) =
            raw_create(&client, GO, &admin, "/api/v4/posts/ephemeral", &body).await;
        let (rs_status, rs_body) =
            raw_create(&client, RUST, &admin, "/api/v4/posts/ephemeral", &body).await;
        assert_eq!(go_status, 400, "{label}");
        assert_eq!(rs_status, go_status, "{label}");
        assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, label);
    }

    // `create_post_ephemeral` is a **system** permission, so a plain user is refused however the
    // channel is configured.
    let user = create_plain_user(&client, &admin, &team, "createpostephperm").await;
    let (user_id, user_token) = (user.id, user.token);
    add_user_to_channel(&client, &admin, &channel, &user_id).await;
    let body = serde_json::json!({
        "user_id": user_id,
        "post": { "channel_id": channel, "message": "x" },
    });
    let (go_status, go_body) =
        raw_create(&client, GO, &user_token, "/api/v4/posts/ephemeral", &body).await;
    let (rs_status, rs_body) =
        raw_create(&client, RUST, &user_token, "/api/v4/posts/ephemeral", &body).await;
    assert_eq!(go_status, 403);
    assert_eq!(rs_status, go_status);
    assert_error_bodies_match_except_known_gaps(&go_body, &rs_body, "create_post_ephemeral");

    delete_plain_user(&client, &admin, &user_id).await;
}

/// The live half of the shadowing proof: the literal `/api/v4/posts/ephemeral` must not have
/// taken `GET`, `PUT` or `DELETE` away from `/api/v4/posts/{post_id}`.
///
/// `mm_api::post_writes`'s `registering_the_two_create_routes_un_serves_nothing` measures the same
/// thing in-process against a dead stack, which is faster and needs nothing running. This one
/// measures it against the real Go server and additionally proves the *bodies* still agree — a
/// route that kept answering with the wrong error would pass the in-process test.
#[tokio::test]
async fn the_ephemeral_literal_did_not_un_serve_its_parameterised_sibling() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;

    for method in [
        reqwest::Method::GET,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let send = async |base: &str| {
            let response = client
                .request(method.clone(), format!("{base}/api/v4/posts/ephemeral"))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .body("{}")
                .send()
                .await
                .expect("the server answers");
            let served_by_rust = response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                == Some("rust");
            (
                response.status().as_u16(),
                served_by_rust,
                response.bytes().await.expect("body reads").to_vec(),
            )
        };
        let (go_status, _, go_body) = send(GO).await;
        let (rs_status, served_by_rust, rs_body) = send(RUST).await;
        assert_eq!(go_status, 400, "{method} /posts/ephemeral: Go's status");
        assert_eq!(rs_status, go_status, "{method} /posts/ephemeral");
        assert!(
            served_by_rust,
            "{method} /posts/ephemeral must still be answered here, not forwarded"
        );
        let go = assert_error_bodies_match_except_known_gaps(
            &go_body,
            &rs_body,
            "the ephemeral literal's other methods",
        );
        assert_eq!(go["id"], "api.context.invalid_url_param.app_error");
    }
}

/// `post.UserId = c.AppContext.Session().UserId` — the body's `user_id` is **overwritten**, not
/// validated.
///
/// So a client can ask to post as somebody else and simply gets its own post. Dropping the
/// assignment would let any caller forge authorship, and nothing else in this suite would notice:
/// every other body leaves `user_id` unset, where the assignment and its absence agree.
#[tokio::test]
async fn the_bodys_user_id_is_overwritten_with_the_sessions() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "authorship").await;

    let body = serde_json::json!({
        "channel_id": channel,
        "message": "mmrs authorship",
        // A well-formed id belonging to nobody.
        "user_id": "aaaaaaaaaaaaaaaaaaaaaaaaaa",
    });
    let (go_status, _, go_post) = create_on(&client, GO, &token, "/api/v4/posts", &body).await;
    let (rs_status, served_by_rust, rs_post) =
        create_on(&client, RUST, &token, "/api/v4/posts", &body).await;

    assert_eq!(go_status, 201);
    assert_eq!(rs_status, 201);
    assert!(served_by_rust);
    assert_eq!(
        go_post["user_id"].as_str(),
        Some(logged_in_user_id()),
        "Go writes the session's user id over the body's"
    );
    assert_created_posts_agree(&go_post, &rs_post, "a forged user_id");
}

/// `if post.CreateAt != 0 && !SessionHasPermissionTo(manage_system) { post.CreateAt = 0 }`.
///
/// A **system** permission, and the failure is silent: a caller without it gets a post at the
/// current time rather than a 403. So the gate has two halves and each needs its own caller — the
/// admin, whose timestamp survives, and a plain user, whose does not. Testing only one of them
/// cannot tell "the gate works" from "the gate is inverted".
#[tokio::test]
async fn a_backdated_create_at_survives_for_an_admin_and_is_dropped_for_everybody_else() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let admin = go_minted_token(&client).await;
    let (team, channel) = own_channel(&client, &admin, "backdate").await;

    // A fixed instant well in the past, and not zero.
    const BACKDATED: i64 = 1_700_000_000_000;
    let body = |message: &str| {
        serde_json::json!({
            "channel_id": channel,
            "message": message,
            "create_at": BACKDATED,
        })
    };

    let (_, served, admin_post) = create_on(
        &client,
        RUST,
        &admin,
        "/api/v4/posts",
        &body("mmrs backdated admin"),
    )
    .await;
    assert!(served, "a plain message must be served here");
    assert_eq!(
        admin_post["create_at"].as_i64(),
        Some(BACKDATED),
        "manage_system keeps the client's timestamp"
    );
    assert_eq!(
        admin_post["update_at"].as_i64(),
        Some(BACKDATED),
        "and PreSave copies it into update_at"
    );

    // Go, for the same caller, agrees.
    let (_, _, go_admin_post) = create_on(
        &client,
        GO,
        &admin,
        "/api/v4/posts",
        &body("mmrs backdated admin go"),
    )
    .await;
    assert_eq!(go_admin_post["create_at"].as_i64(), Some(BACKDATED));

    let user = create_plain_user(&client, &admin, &team, "createpostbackdate").await;
    add_user_to_channel(&client, &admin, &channel, &user.id).await;

    for (base, message) in [
        (GO, "mmrs backdated user go"),
        (RUST, "mmrs backdated user"),
    ] {
        let (status, _, post) =
            create_on(&client, base, &user.token, "/api/v4/posts", &body(message)).await;
        assert_eq!(status, 201, "{base}: the timestamp is dropped, not refused");
        let create_at = post["create_at"].as_i64().expect("a create_at");
        assert_ne!(
            create_at, BACKDATED,
            "{base}: without manage_system the client's timestamp must be discarded"
        );
        assert!(
            create_at > 1_700_000_000_000,
            "{base}: and replaced with the current time, not with zero"
        );
    }

    delete_plain_user(&client, &admin, &user.id).await;
}

/// The `posted` websocket event, which is the whole of `handlePostEvents` this port reproduces.
///
/// # Six data fields, and five of them are computed rather than copied
///
/// `channel_type` and `channel_name` are the channel's. `channel_display_name` is
/// `GetChannelName(model.ShowUsername, "")` — the channel's display name for an open channel, and
/// **not** the sorted member list a group message would get. `sender_name` is
/// `GetSenderName(model.ShowUsername, …)`, which is `"@" + username`: the `@` is the prefix
/// argument and the `ShowUsername` is a *literal constant* at the call site, so neither the
/// `TeammateNameDisplay` setting nor the caller's `name_format` preference is read. `team_id` is
/// the channel's team. `set_online` is the query flag, as a **bool** and not a string.
///
/// Nothing else in this suite sees any of that: the HTTP response carries the post and says
/// nothing about the event. Without this test, `?set_online=false` and a `sender_name` missing
/// its `@` are both invisible.
#[tokio::test]
async fn the_posted_event_matches_go_field_for_field() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = common::BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let (_team, channel) = own_channel(&client, &token, "wsposted").await;

    let mut go_socket = common::SocketProbe::connect(GO, &token).await;
    let mut rust_socket = common::SocketProbe::connect(RUST, &token).await;

    // `?set_online=false` rather than the default, so the field is `false` on both sides and a
    // port that hardcoded `true` — as the system-post publisher does — fails here.
    let (go_status, _, go_post) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts?set_online=false",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs ws go" }),
    )
    .await;
    let (rs_status, served_by_rust, rs_post) = create_on(
        &client,
        RUST,
        &token,
        "/api/v4/posts?set_online=false",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs ws rust" }),
    )
    .await;
    assert_eq!(go_status, 201);
    assert_eq!(rs_status, 201);
    assert!(
        served_by_rust,
        "the event only proves something if we served it"
    );

    let go_id = go_post["id"].as_str().expect("an id").to_owned();
    let rs_id = rs_post["id"].as_str().expect("an id").to_owned();

    let carrying = |post_id: String| {
        move |frames: &[serde_json::Value]| {
            frames.iter().any(|frame| {
                frame["event"] == "posted"
                    && frame["data"]["post"]
                        .as_str()
                        .is_some_and(|post| post.contains(&post_id))
            })
        }
    };
    go_socket
        .collect_until(
            std::time::Duration::from_millis(2000),
            carrying(go_id.clone()),
        )
        .await;
    rust_socket
        .collect_until(
            std::time::Duration::from_millis(2000),
            carrying(rs_id.clone()),
        )
        .await;

    let one = |probe: &common::SocketProbe, post_id: &str, label: &str| {
        let events: Vec<_> = probe
            .events_named("posted")
            .into_iter()
            .filter(|frame| {
                frame["data"]["post"]
                    .as_str()
                    .is_some_and(|post| post.contains(post_id))
            })
            .collect();
        assert_eq!(
            events.len(),
            1,
            "{label} published one `posted`: {events:?}"
        );
        events.into_iter().next().expect("one event")
    };
    let go_event = one(&go_socket, &go_id, "go");
    let rs_event = one(&rust_socket, &rs_id, "rust");

    for (key, expected) in [
        ("channel_type", serde_json::json!("O")),
        ("set_online", serde_json::json!(false)),
    ] {
        assert_eq!(go_event["data"][key], expected, "go's {key}");
        assert_eq!(rs_event["data"][key], expected, "our {key}");
    }
    for key in [
        "channel_display_name",
        "channel_name",
        "sender_name",
        "team_id",
    ] {
        assert_eq!(
            rs_event["data"][key], go_event["data"][key],
            "the two servers' {key} must agree"
        );
    }
    // Pinned rather than only compared, so the two agreeing on a *wrong* value still fails.
    assert!(
        go_event["data"]["sender_name"]
            .as_str()
            .is_some_and(|name| name.starts_with('@')),
        "GetSenderName's prefix argument is `@`"
    );
    assert_eq!(
        go_event["data"]["broadcast"], rs_event["data"]["broadcast"],
        "neither side may put the broadcast hooks on the wire"
    );
    // The event is channel-scoped with no user and no omitted connection, so the posting client
    // is told about its own post.
    assert_eq!(rs_event["broadcast"]["channel_id"], channel.as_str());
    assert_eq!(rs_event["broadcast"]["user_id"], "");
    assert_eq!(rs_event["broadcast"]["omit_connection_id"], "");
    assert_eq!(
        go_event["broadcast"]["user_id"], rs_event["broadcast"]["user_id"],
        "and Go agrees about the addressing"
    );

    // The other half of the `?set_online` gate: an **unparseable** value keeps the default
    // `true`. Asserted on the same pair of sockets because it is the only place the swallowed
    // parse error is observable at all — the HTTP response is a 201 either way.
    let (_, _, go_bogus) = create_on(
        &client,
        GO,
        &token,
        "/api/v4/posts?set_online=bogus",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs ws go bogus" }),
    )
    .await;
    let (_, served, rs_bogus) = create_on(
        &client,
        RUST,
        &token,
        "/api/v4/posts?set_online=bogus",
        &serde_json::json!({ "channel_id": channel, "message": "mmrs ws rust bogus" }),
    )
    .await;
    assert!(served, "still ours to answer");
    let go_bogus_id = go_bogus["id"].as_str().expect("an id").to_owned();
    let rs_bogus_id = rs_bogus["id"].as_str().expect("an id").to_owned();
    go_socket
        .collect_until(
            std::time::Duration::from_millis(2000),
            carrying(go_bogus_id.clone()),
        )
        .await;
    rust_socket
        .collect_until(
            std::time::Duration::from_millis(2000),
            carrying(rs_bogus_id.clone()),
        )
        .await;
    assert_eq!(
        one(&go_socket, &go_bogus_id, "go")["data"]["set_online"],
        serde_json::json!(true),
        "Go's unparseable set_online stays true"
    );
    assert_eq!(
        one(&rust_socket, &rs_bogus_id, "rust")["data"]["set_online"],
        serde_json::json!(true),
        "and so must ours"
    );
}
