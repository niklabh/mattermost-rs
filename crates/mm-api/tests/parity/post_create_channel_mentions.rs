//! Cross-server parity for a `~channel` mention on `POST /api/v4/posts` and `PUT /api/v4/posts/{id}`
//! — `FillInPostProps`'s `channel_mentions` prop, its per-viewer rewrite on a read, and the
//! `channel_mentions` broadcast hook on the `posted` frame.
//!
//! ```sh
//! scripts/parity.sh --test parity post_create_channel_mentions
//! ```
//!
//! The prop takes three shapes on the wire and this suite pins each: the row and the frame a
//! permitted recipient gets carry `{display_name, team_name, id}`; every HTTP body — the create
//! and edit responses included, which are sanitised for the caller after the publish — carries
//! the `{display_name, team_name}` rewrite, minus what the viewer may not resolve; and a
//! recipient who may not resolve a mentioned channel gets a frame with no prop at all.

use std::time::Duration;

use crate::common;

use common::{
    BROADCAST_STREAM, GO, RUST, SocketProbe, add_user_to_channel, client, create_channel_typed,
    create_direct_channel, create_plain_user, create_team, fixture_pool, go_minted_token,
    logged_in_user_id, purge_api_fixtures, stack_enabled,
};

struct Fixture {
    team_id: String,
    team_name: String,
    /// Where the posts go; everyone is a member.
    home_id: String,
    /// A public channel to mention. Nobody but the admin is in it.
    public: (String, String),
    /// A private channel to mention. The admin and `insider` are in it.
    private: (String, String),
    /// In `home` only.
    reader: common::PlainUser,
    /// In `home` and the private channel.
    insider: common::PlainUser,
    /// The admin's DM with the reader.
    dm_id: String,
}

static FIXTURE: tokio::sync::OnceCell<Fixture> = tokio::sync::OnceCell::const_new();

async fn channel_name(client: &reqwest::Client, token: &str, channel_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let channel: serde_json::Value = response.json().await.expect("a channel");
    channel["name"].as_str().expect("a name").to_owned()
}

async fn fixture(client: &reqwest::Client, token: &str) -> &'static Fixture {
    FIXTURE
        .get_or_init(|| async {
            purge_api_fixtures().await;
            let team_id = create_team(client, token, "cpcm").await;
            let response = client
                .get(format!("{GO}/api/v4/teams/{team_id}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .expect("Go answers");
            let team: serde_json::Value = response.json().await.expect("a team");
            let team_name = team["name"].as_str().expect("a name").to_owned();

            let home_id = create_channel_typed(client, token, &team_id, "cpcmh", "O").await;
            let public_id = create_channel_typed(client, token, &team_id, "cpcmp", "O").await;
            let private_id = create_channel_typed(client, token, &team_id, "cpcmv", "P").await;
            let reader = create_plain_user(client, token, &team_id, "cpcm").await;
            let insider = create_plain_user(client, token, &team_id, "cpcmi").await;
            add_user_to_channel(client, token, &home_id, &reader.id).await;
            add_user_to_channel(client, token, &home_id, &insider.id).await;
            add_user_to_channel(client, token, &private_id, &insider.id).await;
            let dm_id = create_direct_channel(client, token, logged_in_user_id(), &reader.id).await;
            let public = (
                public_id.clone(),
                channel_name(client, token, &public_id).await,
            );
            let private = (
                private_id.clone(),
                channel_name(client, token, &private_id).await,
            );
            Fixture {
                team_id,
                team_name,
                home_id,
                public,
                private,
                reader,
                insider,
                dm_id,
            }
        })
        .await
}

async fn create(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    channel_id: &str,
    message: &str,
    props: serde_json::Value,
) -> (u16, bool, serde_json::Value) {
    let mut body = serde_json::json!({ "channel_id": channel_id, "message": message });
    if !props.is_null() {
        body["props"] = props;
    }
    let response = client
        .post(format!("{base}/api/v4/posts"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&body)
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

async fn update(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
    message: &str,
) -> (u16, bool, serde_json::Value) {
    let response = client
        .put(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .json(&serde_json::json!({ "id": post_id, "message": message }))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

async fn get_post(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    post_id: &str,
) -> (u16, bool, serde_json::Value) {
    let response = client
        .get(format!("{base}/api/v4/posts/{post_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
    let status = response.status().as_u16();
    let served = response
        .headers()
        .get("x-mmrs-served-by")
        .and_then(|v| v.to_str().ok())
        == Some("rust");
    (
        status,
        served,
        response.json().await.unwrap_or(serde_json::Value::Null),
    )
}

fn normalised(post: &serde_json::Value) -> serde_json::Value {
    let mut post = post.clone();
    let obj = post.as_object_mut().expect("a post object");
    for key in ["id", "create_at", "update_at", "edit_at", "pending_post_id"] {
        if obj.contains_key(key) {
            obj.insert(key.to_owned(), serde_json::json!(0));
        }
    }
    post
}

/// The `props` column of the row, as stored.
async fn props_column(post_id: &str) -> serde_json::Value {
    let pool = fixture_pool().await.expect("DATABASE_URL");
    sqlx::query_scalar("SELECT props FROM posts WHERE id = $1")
        .bind(post_id)
        .fetch_one(&pool)
        .await
        .expect("the post row")
}

fn full_entry(f: &Fixture, channel: &(String, String), display_name: &str) -> serde_json::Value {
    serde_json::json!({
        "display_name": display_name,
        "team_name": f.team_name,
        "id": channel.0,
    })
}

fn read_entry(f: &Fixture, display_name: &str) -> serde_json::Value {
    serde_json::json!({
        "display_name": display_name,
        "team_name": f.team_name,
    })
}

async fn display_name_of(client: &reqwest::Client, token: &str, channel_id: &str) -> String {
    let response = client
        .get(format!("{GO}/api/v4/channels/{channel_id}"))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .expect("Go answers");
    let channel: serde_json::Value = response.json().await.expect("a channel");
    channel["display_name"].as_str().expect("a name").to_owned()
}

/// A public channel: the row stores the three-key entry, and every body — the create response
/// and a read by the reader, who is not in that channel but may read public ones — carries the
/// two-key rewrite.
#[tokio::test]
async fn a_public_channel_mention_is_a_prop_with_the_id_and_reads_back_without_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.public.0).await;
    let message = format!("cpcm see ~{}", f.public.1);

    let mut bodies = Vec::new();
    let mut reads = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.home_id,
            &message,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            body["props"]["channel_mentions"],
            serde_json::json!({ f.public.1.clone(): read_entry(f, &display) }),
            "{base}: the create response is sanitised for the poster, so no id"
        );
        assert_eq!(
            props_column(post_id).await["channel_mentions"],
            serde_json::json!({ f.public.1.clone(): full_entry(f, &f.public, &display) }),
            "{base}: stored with the id"
        );
        bodies.push(normalised(&body));

        for reader in [GO, RUST] {
            let (status, served, read) = get_post(&client, reader, &f.reader.token, post_id).await;
            assert_eq!(status, 200, "{reader}: {read}");
            assert_eq!(served, reader == RUST, "{reader}: the read is served too");
            assert_eq!(
                read["props"]["channel_mentions"],
                serde_json::json!({ f.public.1.clone(): read_entry(f, &display) }),
                "{reader}: rewritten for the viewer, no id"
            );
            reads.push(normalised(&read));
        }
    }
    assert_eq!(bodies[0], bodies[1]);
    assert!(reads.iter().all(|r| r == &reads[0]), "{reads:#?}");
}

/// A private channel: the poster (a member) gets the entry; a read by a non-member drops it,
/// and a read by a member keeps the rewrite.
#[tokio::test]
async fn a_private_channel_mention_is_dropped_for_a_viewer_who_may_not_resolve_it() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.private.0).await;
    let message = format!("cpcm secret ~{}", f.private.1);

    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.home_id,
            &message,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            body["props"]["channel_mentions"],
            serde_json::json!({ f.private.1.clone(): read_entry(f, &display) }),
            "{base}: the poster is a member, so the rewrite keeps it"
        );
        assert_eq!(
            props_column(post_id).await["channel_mentions"],
            serde_json::json!({ f.private.1.clone(): full_entry(f, &f.private, &display) }),
            "{base}: stored with the id"
        );

        for reader in [GO, RUST] {
            let (status, _, as_outsider) =
                get_post(&client, reader, &f.reader.token, post_id).await;
            assert_eq!(status, 200, "{reader}: {as_outsider}");
            assert!(
                as_outsider["props"].get("channel_mentions").is_none(),
                "{reader}: the outsider's read has no prop: {as_outsider}"
            );
            assert_eq!(as_outsider["props"], serde_json::json!({}), "{reader}");

            let (status, _, as_insider) =
                get_post(&client, reader, &f.insider.token, post_id).await;
            assert_eq!(status, 200, "{reader}: {as_insider}");
            assert_eq!(
                as_insider["props"]["channel_mentions"],
                serde_json::json!({ f.private.1.clone(): read_entry(f, &display) }),
                "{reader}"
            );
        }
    }
}

/// A name that is no channel adds nothing — and an unresolvable mention still takes the branch,
/// so `current_team_id` is consumed.
#[tokio::test]
async fn an_unknown_channel_adds_no_prop_and_current_team_id_is_consumed() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.home_id,
            "cpcm see ~no-such-channel-here",
            serde_json::json!({ "current_team_id": f.team_id }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            body["props"],
            serde_json::json!({}),
            "{base}: nothing survives"
        );
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(props_column(post_id).await, serde_json::json!({}), "{base}");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// Without a `~` the branch is skipped and a `current_team_id` prop is **kept** — Go deletes
/// it only after using it.
#[tokio::test]
async fn without_a_mention_current_team_id_survives() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.home_id,
            "cpcm no mention",
            serde_json::json!({ "current_team_id": f.team_id }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            body["props"]["current_team_id"],
            f.team_id.as_str(),
            "{base}"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// In a DM the channel has no team; `current_team_id` names the one to search, and is then
/// deleted from the props.
#[tokio::test]
async fn a_dm_mention_resolves_in_the_current_team_and_consumes_the_hint() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.public.0).await;
    let message = format!("cpcm dm ~{}", f.public.1);

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.dm_id,
            &message,
            serde_json::json!({ "current_team_id": f.team_id }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            body["props"],
            serde_json::json!({ "channel_mentions": { f.public.1.clone(): read_entry(f, &display) } }),
            "{base}: the mention resolved and the hint is gone"
        );
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(
            props_column(post_id).await,
            serde_json::json!({ "channel_mentions": { f.public.1.clone(): full_entry(f, &f.public, &display) } }),
            "{base}: stored with the id and without the hint"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// The `posted` frame: the precomputed frame carries no prop; the hook puts it back for a
/// recipient who may resolve the channel (the insider, the poster) and not for one who may
/// not (the reader).
#[tokio::test]
async fn the_posted_frame_carries_the_mention_only_to_recipients_who_may_resolve_it() {
    if !stack_enabled() {
        return;
    }
    let _broadcast = BROADCAST_STREAM.lock().await;
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.private.0).await;
    let message = format!("cpcm frame ~{}", f.private.1);

    for base in [GO, RUST] {
        let mut reader_socket = SocketProbe::connect(base, &f.reader.token).await;
        let mut insider_socket = SocketProbe::connect(base, &f.insider.token).await;
        let mut poster_socket = SocketProbe::connect(base, &token).await;
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.home_id,
            &message,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        let post_id = body["id"].as_str().expect("an id").to_owned();

        for (who, socket) in [
            ("reader", &mut reader_socket),
            ("insider", &mut insider_socket),
            ("poster", &mut poster_socket),
        ] {
            let wanted = post_id.clone();
            let found = move |frames: &[serde_json::Value]| {
                frames.iter().any(|f| {
                    f["event"] == "posted"
                        && f["data"]["post"]
                            .as_str()
                            .is_some_and(|p| p.contains(&wanted))
                })
            };
            assert!(
                socket
                    .collect_until(Duration::from_millis(2500), found)
                    .await,
                "{base}: {who} got no posted frame: {:?}",
                socket.raw
            );
        }
        let post_in = |socket: &SocketProbe| -> serde_json::Value {
            let frame = socket
                .events_named("posted")
                .into_iter()
                .find(|f| {
                    f["data"]["post"]
                        .as_str()
                        .is_some_and(|p| p.contains(&post_id))
                })
                .expect("the frame");
            serde_json::from_str(frame["data"]["post"].as_str().expect("a string")).expect("a post")
        };
        let expected =
            serde_json::json!({ f.private.1.clone(): full_entry(f, &f.private, &display) });
        assert_eq!(
            post_in(&reader_socket)["props"],
            serde_json::json!({}),
            "{base}: the reader may not resolve the private channel"
        );
        assert_eq!(
            post_in(&insider_socket)["props"]["channel_mentions"],
            expected,
            "{base}: the insider gets the entry verbatim, id included"
        );
        assert_eq!(
            post_in(&poster_socket)["props"]["channel_mentions"],
            expected,
            "{base}: so does the poster"
        );
        // The frames' posts agree with the HTTP body once the prop is accounted for.
        let mut from_body = normalised(&body);
        from_body["props"] = serde_json::json!({});
        assert_eq!(normalised(&post_in(&reader_socket)), from_body, "{base}");
    }
}

/// The **poster's** permission decides what is written: the reader, who is not in the private
/// channel, mentions it and gets no prop — on both, and nothing for any viewer to be shown.
#[tokio::test]
async fn a_mention_the_poster_may_not_resolve_writes_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let message = format!("cpcm outsider ~{}", f.private.1);

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &f.reader.token,
            &f.home_id,
            &message,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(body["props"], serde_json::json!({}), "{base}");
        let post_id = body["id"].as_str().expect("an id");
        assert_eq!(props_column(post_id).await, serde_json::json!({}), "{base}");
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// The hint **narrows** the search: a `current_team_id` naming a team where the channel does
/// not exist resolves nothing, where a global search would have found it.
#[tokio::test]
async fn a_dm_hint_for_the_wrong_team_resolves_nothing() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let other_team = create_team(&client, &token, "cpcmo").await;
    let message = format!("cpcm dm wrong ~{}", f.public.1);

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, served, body) = create(
            &client,
            base,
            &token,
            &f.dm_id,
            &message,
            serde_json::json!({ "current_team_id": other_team }),
        )
        .await;
        assert_eq!(status, 201, "{base}: {body}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            body["props"],
            serde_json::json!({}),
            "{base}: searched the wrong team, and the hint is consumed"
        );
        bodies.push(normalised(&body));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// An edit that adds a `~` fills the prop on the update path, and one that removes it deletes
/// the prop from a props map the row already has.
#[tokio::test]
async fn an_edit_adds_and_removes_the_prop() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.public.0).await;

    let mut bodies = Vec::new();
    for base in [GO, RUST] {
        let (status, _, body) = create(
            &client,
            GO,
            &token,
            &f.home_id,
            "cpcm edit me",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, 201, "{body}");
        let post_id = body["id"].as_str().expect("an id").to_owned();

        let (status, served, edited) = update(
            &client,
            base,
            &token,
            &post_id,
            &format!("cpcm edited ~{}", f.public.1),
        )
        .await;
        assert_eq!(status, 200, "{base}: {edited}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            edited["props"]["channel_mentions"],
            serde_json::json!({ f.public.1.clone(): read_entry(f, &display) }),
            "{base}: the edit response is sanitised too"
        );
        assert_eq!(
            props_column(&post_id).await["channel_mentions"],
            serde_json::json!({ f.public.1.clone(): full_entry(f, &f.public, &display) }),
            "{base}"
        );

        let (status, served, cleared) =
            update(&client, base, &token, &post_id, "cpcm edited again").await;
        assert_eq!(status, 200, "{base}: {cleared}");
        assert_eq!(served, base == RUST, "{base}");
        assert_eq!(
            cleared["props"],
            serde_json::json!({}),
            "{base}: the prop is deleted"
        );
        assert_eq!(
            props_column(&post_id).await,
            serde_json::json!({}),
            "{base}"
        );
        bodies.push((normalised(&edited), normalised(&cleared)));
    }
    assert_eq!(bodies[0], bodies[1]);
}

/// A **system post** takes the same branch: a channel header naming `~public` gets the
/// `system_header_change` notice a `channel_mentions` prop, read back as the viewer's rewrite.
#[tokio::test]
async fn a_header_notice_that_names_a_channel_carries_the_prop() {
    if !stack_enabled() {
        return;
    }
    let client = client();
    let token = go_minted_token(&client).await;
    let f = fixture(&client, &token).await;
    let display = display_name_of(&client, &token, &f.public.0).await;

    let mut notices = Vec::new();
    for base in [GO, RUST] {
        // A per-base channel (a name is unique per team) and a header with **no URL** in it — a
        // link would earn the notice an embed, which is a different unit's business.
        let tag = if base == RUST { "cpcmnr" } else { "cpcmng" };
        let channel_id = create_channel_typed(&client, &token, &f.team_id, tag, "O").await;
        let header = format!("cpcm header {tag} ~{}", f.public.1);
        let response = client
            .put(format!("{base}/api/v4/channels/{channel_id}/patch"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({ "header": header }))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{base} is unreachable: {e}"));
        assert_eq!(response.status().as_u16(), 200, "{base}");
        assert_eq!(
            response
                .headers()
                .get("x-mmrs-served-by")
                .and_then(|v| v.to_str().ok())
                == Some("rust"),
            base == RUST,
            "{base}: the patch is served"
        );

        for reader in [GO, RUST] {
            let response = client
                .get(format!("{reader}/api/v4/channels/{channel_id}/posts"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{reader} is unreachable: {e}"));
            assert_eq!(response.status().as_u16(), 200, "{reader}");
            let page: serde_json::Value = response.json().await.expect("a post list");
            let notice = page["posts"]
                .as_object()
                .expect("posts")
                .values()
                .find(|p| p["type"] == "system_header_change")
                .cloned()
                .unwrap_or_else(|| panic!("{reader}: no header notice in {page}"));
            assert_eq!(
                notice["props"]["channel_mentions"],
                serde_json::json!({ f.public.1.clone(): read_entry(f, &display) }),
                "{base} wrote, {reader} read: {notice}"
            );
            let mut notice = normalised(&notice);
            notice["message"] = serde_json::json!("");
            notice["props"]["new_header"] = serde_json::json!("");
            notice["channel_id"] = serde_json::json!("");
            notices.push(notice);
        }
    }
    assert!(
        notices.iter().all(|n| n == &notices[0]),
        "every notice agrees once its own text is masked: {notices:#?}"
    );
}
